//! Lottie shape layer (`ty: 4`) content.
//!
//! A shape layer holds a tree of *shape items*. The ones implemented in
//! this phase:
//!
//! | Lottie `ty` | Role        | Notes                                       |
//! |-------------|-------------|---------------------------------------------|
//! | `gr`        | Group       | Container for nested items + own transform  |
//! | `tr`        | Transform   | Group transform (anchor / pos / scale / …)  |
//! | `rc`        | Rectangle   | Geometry — center, size, corner radius      |
//! | `el`        | Ellipse     | Geometry — center, size                     |
//! | `sh`        | Path        | Geometry — vertices + per-vertex tangents   |
//! | `fl`        | Fill        | Solid color + opacity                       |
//! | `st`        | Stroke      | Solid color + opacity + width               |
//!
//! Items not in the table parse as no-ops. Most notably **polystar
//! (`sr`)**, **trim path (`tm`)**, and **gradient fill/stroke
//! (`gf` / `gs`)** are deferred — see Phase 2 / 3 in the crate README.
//!
//! # Render model
//!
//! Each group renders as:
//!
//! 1. Push group transform (`tr`) if present.
//! 2. For each geometry item, build a `Path` then:
//!    - paint with the group's fill, if any
//!    - stroke with the group's stroke, if any
//! 3. Recurse into nested groups.
//! 4. Pop the group transform.
//!
//! Lottie strictly speaking lets paint items reference *all geometry
//! items earlier in the same group* and supports complex stacking with
//! merge paths, trim paths, and z-order tricks. We start with the
//! simpler single-fill / single-stroke per group model — covers the
//! vast majority of motion-design exports and stays readable.

use blinc_core::draw::{Path, Stroke};
use blinc_core::layer::{Brush, Color, CornerRadius, Rect};
use blinc_core::DrawContext;
use serde_json::Value;

use crate::layer::{
    eased_u, parse_animated_scalar, parse_animated_vec2, parse_animated_vec4, pop_layer_transform,
    push_layer_transform, tangent_from_key, AnimatedF32, AnimatedVec2, AnimatedVec4, BezierTangent,
    TransformSpec,
};

/// Top-level shape-layer content. Wraps the layer's `shapes` array as a
/// flat list of root groups.
#[derive(Debug, Clone)]
pub(crate) struct ShapeContent {
    groups: Vec<ShapeGroup>,
}

#[derive(Debug, Clone)]
pub(crate) struct ShapeGroup {
    /// Group transform from the `tr` shape item. `None` means identity.
    transform: Option<TransformSpec>,
    geometries: Vec<Geometry>,
    fill: Option<FillSpec>,
    stroke: Option<StrokeSpec>,
    /// Nested `gr` groups — render after this group's own paint pass.
    children: Vec<ShapeGroup>,
}

#[derive(Debug, Clone)]
enum Geometry {
    Rectangle {
        position: AnimatedVec2,
        size: AnimatedVec2,
        corner_radius: AnimatedF32,
    },
    Ellipse {
        position: AnimatedVec2,
        size: AnimatedVec2,
    },
    /// `sh` path: hand-drawn logos, icons, organic forms. The core
    /// shape is a list of vertices plus per-vertex `in`/`out`
    /// tangent offsets — segment N→N+1 renders as a cubic bezier
    /// whose control points are `v[N] + out[N]` and `v[N+1] + in[N+1]`.
    Path(AnimatedPath),
}

/// A single closed-or-open cubic-bezier path sampled at one moment.
/// Lottie's raw shape data: vertex positions + per-vertex tangent
/// offsets stored **relative to the vertex**. The render path
/// absolutifies them when emitting `cubic_to` control points.
///
/// Same `Vec<[f32; 2]>` length for all three arrays is a Lottie
/// invariant — one `in` / `out` per vertex. If the JSON violates
/// this the parser falls back to a single-point dummy to avoid
/// index panics at render time.
#[derive(Debug, Clone)]
pub(crate) struct PathShape {
    pub vertices: Vec<[f32; 2]>,
    pub in_tangents: Vec<[f32; 2]>,
    pub out_tangents: Vec<[f32; 2]>,
    pub closed: bool,
}

impl PathShape {
    fn empty() -> Self {
        Self {
            vertices: Vec::new(),
            in_tangents: Vec::new(),
            out_tangents: Vec::new(),
            closed: false,
        }
    }

    fn vertex_count(&self) -> usize {
        self.vertices.len()
    }

    /// Linear per-vertex morph between `self` and `other`, writing
    /// into a new shape. Caller guarantees `self.vertex_count() ==
    /// other.vertex_count()` and `self.closed == other.closed` —
    /// see [`AnimatedPath::sample`] for the bail-out path when
    /// those don't hold.
    fn lerp(&self, other: &Self, u: f32) -> Self {
        debug_assert_eq!(self.vertex_count(), other.vertex_count());
        let n = self.vertex_count();
        let mix = |a: &[[f32; 2]], b: &[[f32; 2]]| -> Vec<[f32; 2]> {
            (0..n)
                .map(|i| {
                    [
                        a[i][0] + (b[i][0] - a[i][0]) * u,
                        a[i][1] + (b[i][1] - a[i][1]) * u,
                    ]
                })
                .collect()
        };
        Self {
            vertices: mix(&self.vertices, &other.vertices),
            in_tangents: mix(&self.in_tangents, &other.in_tangents),
            out_tangents: mix(&self.out_tangents, &other.out_tangents),
            closed: self.closed,
        }
    }

    fn to_path(&self) -> Path {
        if self.vertices.is_empty() {
            return Path::new();
        }
        let mut path = Path::new();
        let v0 = self.vertices[0];
        path = path.move_to(v0[0], v0[1]);
        for i in 0..self.vertices.len() - 1 {
            let v_a = self.vertices[i];
            let v_b = self.vertices[i + 1];
            let o_a = self.out_tangents[i];
            let i_b = self.in_tangents[i + 1];
            path = path.cubic_to(
                v_a[0] + o_a[0],
                v_a[1] + o_a[1],
                v_b[0] + i_b[0],
                v_b[1] + i_b[1],
                v_b[0],
                v_b[1],
            );
        }
        if self.closed && self.vertices.len() >= 2 {
            // Close with the cubic segment from last vertex back to
            // first, using last's `out` and first's `in` — matches
            // the Lottie rendering model and preserves the author's
            // curvature at the closure seam.
            let last = *self.vertices.last().unwrap();
            let first = self.vertices[0];
            let o_last = *self.out_tangents.last().unwrap();
            let i_first = self.in_tangents[0];
            path = path.cubic_to(
                last[0] + o_last[0],
                last[1] + o_last[1],
                first[0] + i_first[0],
                first[1] + i_first[1],
                first[0],
                first[1],
            );
            path = path.close();
        }
        path
    }
}

/// One keyframe in an animated path. `PathShape` is too heavy to
/// lerp component-wise as a flat `[f32; N]` (vertex count can differ
/// and tangents are per-vertex), so this lives alongside the
/// scalar / vec2 / vec4 keyframe types in `layer.rs` but keeps its
/// own struct.
#[derive(Debug, Clone)]
pub(crate) struct PathKey {
    pub t: f32,
    pub value: PathShape,
    pub hold: bool,
    pub out_tangent: Option<BezierTangent>,
    pub in_tangent: Option<BezierTangent>,
}

#[derive(Debug, Clone)]
pub(crate) enum AnimatedPath {
    Static(PathShape),
    Keyframed(Vec<PathKey>),
}

impl AnimatedPath {
    fn sample(&self, t: f32) -> PathShape {
        match self {
            Self::Static(p) => p.clone(),
            Self::Keyframed(keys) => sample_path(keys, t),
        }
    }
}

/// Sample an animated path. Piece-wise bezier-eased morph when the
/// bracketing keyframes share a vertex count; otherwise snap to
/// `k0.value` — Lottie's "path morph with mismatched vertex counts"
/// is out of scope for this phase (documented in the crate BACKLOG).
fn sample_path(keys: &[PathKey], t: f32) -> PathShape {
    if keys.is_empty() {
        return PathShape::empty();
    }
    if t <= keys[0].t {
        return keys[0].value.clone();
    }
    let last = keys.last().unwrap();
    if t >= last.t {
        return last.value.clone();
    }
    for pair in keys.windows(2) {
        let k0 = &pair[0];
        let k1 = &pair[1];
        if t >= k0.t && t < k1.t {
            if k0.hold || (k1.t - k0.t).abs() < f32::EPSILON {
                return k0.value.clone();
            }
            if k0.value.vertex_count() != k1.value.vertex_count()
                || k0.value.closed != k1.value.closed
            {
                // Vertex-count mismatch means per-vertex lerp has no
                // well-defined pairing. Snap to `k0` until we cross
                // into `k1`; avoids a slerp that would blow up some
                // vertices toward a wrong target.
                return k0.value.clone();
            }
            let linear_u = (t - k0.t) / (k1.t - k0.t);
            let u = eased_u(linear_u, k0.out_tangent, k1.in_tangent);
            return k0.value.lerp(&k1.value, u);
        }
    }
    last.value.clone()
}

#[derive(Debug, Clone)]
struct FillSpec {
    color: AnimatedVec4,
    /// 0–1 multiplier on the color's alpha.
    opacity: AnimatedF32,
}

#[derive(Debug, Clone)]
struct StrokeSpec {
    color: AnimatedVec4,
    opacity: AnimatedF32,
    /// Line width in source-space pixels.
    width: AnimatedF32,
}

impl ShapeContent {
    /// Parse a `ty: 4` layer's `shapes` array. `fr` is the composition
    /// frame rate, used to convert keyframe times to seconds.
    pub fn from_layer(v: &Value, fr: f32) -> Self {
        let shapes = v.get("shapes").and_then(Value::as_array);
        let groups = match shapes {
            Some(arr) => arr
                .iter()
                .filter_map(|s| {
                    let ty = s.get("ty").and_then(Value::as_str)?;
                    if ty == "gr" {
                        Some(parse_group(s, fr))
                    } else {
                        // Bare geometry without an enclosing group is rare
                        // but legal — wrap in a synthetic group so the
                        // render path stays uniform.
                        single_item_group(s, fr)
                    }
                })
                .collect(),
            None => Vec::new(),
        };
        Self { groups }
    }

    pub fn render(&self, dc: &mut dyn DrawContext, t: f32) {
        for group in &self.groups {
            group.render(dc, t);
        }
    }
}

impl ShapeGroup {
    fn render(&self, dc: &mut dyn DrawContext, t: f32) {
        let pushed = self
            .transform
            .as_ref()
            .map(|ts| {
                let xf = ts.sample(t);
                push_layer_transform(dc, &xf);
            })
            .is_some();

        for geo in &self.geometries {
            let path = geo.to_path(t);
            if let Some(fill) = &self.fill {
                dc.fill_path(&path, Brush::Solid(sample_paint_color(&fill.color, &fill.opacity, t)));
            }
            if let Some(stroke) = &self.stroke {
                let color = sample_paint_color(&stroke.color, &stroke.opacity, t);
                let width = stroke.width.sample(t).max(0.0);
                if width > 0.0 {
                    dc.stroke_path(&path, &Stroke::new(width), Brush::Solid(color));
                }
            }
        }

        for child in &self.children {
            child.render(dc, t);
        }

        if pushed {
            pop_layer_transform(dc);
        }
    }
}

impl Geometry {
    fn to_path(&self, t: f32) -> Path {
        match self {
            Geometry::Rectangle {
                position,
                size,
                corner_radius,
            } => {
                let p = position.sample(t);
                let s = size.sample(t);
                let r = corner_radius.sample(t).max(0.0);
                let rect = Rect::new(p[0] - s[0] * 0.5, p[1] - s[1] * 0.5, s[0], s[1]);
                if r > 0.0 {
                    Path::rounded_rect(rect, CornerRadius::uniform(r))
                } else {
                    Path::rect(rect)
                }
            }
            Geometry::Ellipse { position, size } => {
                let p = position.sample(t);
                let s = size.sample(t);
                ellipse_path(p[0], p[1], s[0] * 0.5, s[1] * 0.5)
            }
            Geometry::Path(animated) => animated.sample(t).to_path(),
        }
    }
}

/// Build a closed cubic-bezier approximation of an axis-aligned ellipse.
///
/// Uses the standard 4-arc construction with `k = (4/3)·tan(π/8)` for
/// the control-point distance.
fn ellipse_path(cx: f32, cy: f32, rx: f32, ry: f32) -> Path {
    const K: f32 = 0.552_284_7;
    let kx = rx * K;
    let ky = ry * K;
    Path::new()
        .move_to(cx + rx, cy)
        .cubic_to(cx + rx, cy + ky, cx + kx, cy + ry, cx, cy + ry)
        .cubic_to(cx - kx, cy + ry, cx - rx, cy + ky, cx - rx, cy)
        .cubic_to(cx - rx, cy - ky, cx - kx, cy - ry, cx, cy - ry)
        .cubic_to(cx + kx, cy - ry, cx + rx, cy - ky, cx + rx, cy)
        .close()
}

fn sample_paint_color(color: &AnimatedVec4, opacity: &AnimatedF32, t: f32) -> Color {
    let c = color.sample(t);
    let o = opacity.sample(t).clamp(0.0, 1.0);
    Color::rgba(c[0], c[1], c[2], c[3] * o)
}

// ─────────────────────────────────────────────────────────────────────────────
// Parsing
// ─────────────────────────────────────────────────────────────────────────────

fn parse_group(v: &Value, fr: f32) -> ShapeGroup {
    let mut group = ShapeGroup {
        transform: None,
        geometries: Vec::new(),
        fill: None,
        stroke: None,
        children: Vec::new(),
    };
    let Some(items) = v.get("it").and_then(Value::as_array) else {
        return group;
    };
    for item in items {
        let Some(ty) = item.get("ty").and_then(Value::as_str) else {
            continue;
        };
        match ty {
            "rc" => group.geometries.push(parse_rectangle(item, fr)),
            "el" => group.geometries.push(parse_ellipse(item, fr)),
            "sh" => group.geometries.push(parse_path(item, fr)),
            "fl" => group.fill = Some(parse_fill(item, fr)),
            "st" => group.stroke = Some(parse_stroke(item, fr)),
            "tr" => group.transform = Some(TransformSpec::from_value(Some(item), fr)),
            "gr" => group.children.push(parse_group(item, fr)),
            _ => {} // unimplemented item — skip silently
        }
    }
    group
}

fn single_item_group(v: &Value, fr: f32) -> Option<ShapeGroup> {
    let ty = v.get("ty").and_then(Value::as_str)?;
    let mut group = ShapeGroup {
        transform: None,
        geometries: Vec::new(),
        fill: None,
        stroke: None,
        children: Vec::new(),
    };
    match ty {
        "rc" => group.geometries.push(parse_rectangle(v, fr)),
        "el" => group.geometries.push(parse_ellipse(v, fr)),
        "sh" => group.geometries.push(parse_path(v, fr)),
        _ => return None,
    }
    Some(group)
}

fn parse_rectangle(v: &Value, fr: f32) -> Geometry {
    Geometry::Rectangle {
        position: parse_animated_vec2(v.get("p"), fr).unwrap_or(AnimatedVec2::Static([0.0, 0.0])),
        size: parse_animated_vec2(v.get("s"), fr).unwrap_or(AnimatedVec2::Static([0.0, 0.0])),
        corner_radius: parse_animated_scalar(v.get("r"), fr)
            .unwrap_or(AnimatedF32::Static(0.0)),
    }
}

fn parse_ellipse(v: &Value, fr: f32) -> Geometry {
    Geometry::Ellipse {
        position: parse_animated_vec2(v.get("p"), fr).unwrap_or(AnimatedVec2::Static([0.0, 0.0])),
        size: parse_animated_vec2(v.get("s"), fr).unwrap_or(AnimatedVec2::Static([0.0, 0.0])),
    }
}

/// Parse a path shape item (`ty: "sh"`). The path data lives in
/// `sh.ks` with the standard `{ "a": 0|1, "k": <value-or-keyframes> }`
/// shape — static (`a: 0`) holds a single `PathShape` object, animated
/// (`a: 1`) holds an array of keyframes.
///
/// Malformed input (missing `ks`, missing `k`, vertex-array length
/// mismatch) falls back to an empty path rather than failing the
/// whole layer parse — matches the rest of the shape-layer code's
/// "best-effort" posture.
fn parse_path(v: &Value, fr: f32) -> Geometry {
    let Some(ks) = v.get("ks") else {
        return Geometry::Path(AnimatedPath::Static(PathShape::empty()));
    };
    let Some(k) = ks.get("k") else {
        return Geometry::Path(AnimatedPath::Static(PathShape::empty()));
    };

    // Animated: `k` is an array of keyframes.
    if let Some(arr) = k.as_array() {
        if arr.first().map(|kf| kf.is_object()).unwrap_or(false)
            && arr.first().and_then(|kf| kf.get("t")).is_some()
        {
            let keys = collect_path_keys(arr, fr);
            return Geometry::Path(if keys.is_empty() {
                AnimatedPath::Static(PathShape::empty())
            } else {
                AnimatedPath::Keyframed(keys)
            });
        }
    }

    // Static: `k` is a single `PathShape` object.
    let shape = path_shape_from_value(k).unwrap_or_else(PathShape::empty);
    Geometry::Path(AnimatedPath::Static(shape))
}

/// Parse a `{ v, i, o, c }` path-shape object. Returns `None` when
/// any required field is missing or the three arrays have different
/// lengths — the render path assumes matched lengths and would panic
/// on mismatch otherwise.
fn path_shape_from_value(v: &Value) -> Option<PathShape> {
    let vertices = parse_point_array(v.get("v")?)?;
    let in_tangents = parse_point_array(v.get("i")?)?;
    let out_tangents = parse_point_array(v.get("o")?)?;
    let closed = v
        .get("c")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if vertices.len() != in_tangents.len() || vertices.len() != out_tangents.len() {
        return None;
    }
    Some(PathShape {
        vertices,
        in_tangents,
        out_tangents,
        closed,
    })
}

fn parse_point_array(v: &Value) -> Option<Vec<[f32; 2]>> {
    let arr = v.as_array()?;
    let mut out = Vec::with_capacity(arr.len());
    for pt in arr {
        let coords = pt.as_array()?;
        let x = coords.first().and_then(Value::as_f64)? as f32;
        let y = coords.get(1).and_then(Value::as_f64)? as f32;
        out.push([x, y]);
    }
    Some(out)
}

fn collect_path_keys(arr: &[Value], fr: f32) -> Vec<PathKey> {
    // Lottie wraps the path-shape value inside a single-element `s`
    // array at each keyframe: `"s": [{ "v": [...], "i": [...], ... }]`.
    // Trailing keyframes sometimes omit `s` to mark the animation's
    // end timestamp — reuse the previous shape so the interpolator
    // has a well-defined endpoint, matching the pattern used for
    // scalar / vec2 / vec4 keys.
    let mut last_value = PathShape::empty();
    arr.iter()
        .filter_map(|kf| {
            let t_frames = kf.get("t")?.as_f64()? as f32;
            let value = kf
                .get("s")
                .and_then(|s| s.as_array())
                .and_then(|s_arr| s_arr.first())
                .and_then(path_shape_from_value)
                .unwrap_or_else(|| last_value.clone());
            last_value = value.clone();
            let hold = kf.get("h").and_then(Value::as_u64).unwrap_or(0) == 1;
            Some(PathKey {
                t: t_frames / fr,
                value,
                hold,
                out_tangent: tangent_from_key(kf, "o"),
                in_tangent: tangent_from_key(kf, "i"),
            })
        })
        .collect()
}

fn parse_fill(v: &Value, fr: f32) -> FillSpec {
    FillSpec {
        color: parse_animated_vec4(v.get("c"), fr)
            .unwrap_or(AnimatedVec4::Static([0.0, 0.0, 0.0, 1.0])),
        opacity: parse_animated_scalar(v.get("o"), fr)
            .unwrap_or(AnimatedF32::Static(100.0))
            .map(|o| (o / 100.0).clamp(0.0, 1.0)),
    }
}

fn parse_stroke(v: &Value, fr: f32) -> StrokeSpec {
    StrokeSpec {
        color: parse_animated_vec4(v.get("c"), fr)
            .unwrap_or(AnimatedVec4::Static([0.0, 0.0, 0.0, 1.0])),
        opacity: parse_animated_scalar(v.get("o"), fr)
            .unwrap_or(AnimatedF32::Static(100.0))
            .map(|o| (o / 100.0).clamp(0.0, 1.0)),
        width: parse_animated_scalar(v.get("w"), fr).unwrap_or(AnimatedF32::Static(1.0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn shape_layer_json(shapes: Value) -> Value {
        json!({ "ty": 4, "ip": 0, "op": 60, "shapes": shapes, "ks": {} })
    }

    #[test]
    fn parses_group_with_rect_and_fill() {
        let v = shape_layer_json(json!([
            {
                "ty": "gr",
                "it": [
                    {
                        "ty": "rc",
                        "p": { "k": [10.0, 20.0, 0.0] },
                        "s": { "k": [100.0, 50.0, 0.0] },
                        "r": { "k": 8.0 }
                    },
                    {
                        "ty": "fl",
                        "c": { "k": [1.0, 0.5, 0.0, 1.0] },
                        "o": { "k": 75.0 }
                    },
                    { "ty": "tr", "p": { "k": [5.0, 5.0, 0.0] } }
                ]
            }
        ]));
        let content = ShapeContent::from_layer(&v, 60.0);
        assert_eq!(content.groups.len(), 1);
        let g = &content.groups[0];
        assert_eq!(g.geometries.len(), 1);
        assert!(g.fill.is_some());
        assert!(g.stroke.is_none());
        assert!(g.transform.is_some(), "tr item should populate transform");
        // Fill alpha = 1.0 source * 0.75 opacity = 0.75
        let fill = g.fill.as_ref().unwrap();
        let c = sample_paint_color(&fill.color, &fill.opacity, 0.0);
        assert!((c.r - 1.0).abs() < 1e-5);
        assert!((c.g - 0.5).abs() < 1e-5);
        assert!((c.b - 0.0).abs() < 1e-5);
        assert!((c.a - 0.75).abs() < 1e-5);
    }

    #[test]
    fn parses_ellipse_with_stroke() {
        let v = shape_layer_json(json!([
            {
                "ty": "gr",
                "it": [
                    {
                        "ty": "el",
                        "p": { "k": [0.0, 0.0, 0.0] },
                        "s": { "k": [80.0, 40.0, 0.0] }
                    },
                    {
                        "ty": "st",
                        "c": { "k": [0.0, 1.0, 0.0, 1.0] },
                        "o": { "k": 100.0 },
                        "w": { "k": 4.0 }
                    }
                ]
            }
        ]));
        let content = ShapeContent::from_layer(&v, 60.0);
        let g = &content.groups[0];
        assert_eq!(g.geometries.len(), 1);
        assert!(g.stroke.is_some());
        let s = g.stroke.as_ref().unwrap();
        assert!((s.width.sample(0.0) - 4.0).abs() < 1e-5);
    }

    #[test]
    fn nested_groups_are_collected_as_children() {
        let v = shape_layer_json(json!([
            {
                "ty": "gr",
                "it": [
                    {
                        "ty": "gr",
                        "it": [
                            { "ty": "rc",
                              "p": { "k": [0.0, 0.0, 0.0] },
                              "s": { "k": [1.0, 1.0, 0.0] },
                              "r": { "k": 0.0 } },
                            { "ty": "fl",
                              "c": { "k": [0.0, 0.0, 0.0, 1.0] },
                              "o": { "k": 100.0 } }
                        ]
                    }
                ]
            }
        ]));
        let content = ShapeContent::from_layer(&v, 60.0);
        assert_eq!(content.groups.len(), 1);
        assert_eq!(content.groups[0].children.len(), 1);
        assert_eq!(content.groups[0].children[0].geometries.len(), 1);
    }

    #[test]
    fn unrecognised_items_are_skipped_not_panicked() {
        let v = shape_layer_json(json!([
            {
                "ty": "gr",
                "it": [
                    { "ty": "sr" }, // polystar — not yet implemented
                    { "ty": "tm" }, // trim path — not yet implemented
                    {
                        "ty": "rc",
                        "p": { "k": [0.0, 0.0, 0.0] },
                        "s": { "k": [10.0, 10.0, 0.0] },
                        "r": { "k": 0.0 }
                    }
                ]
            }
        ]));
        let content = ShapeContent::from_layer(&v, 60.0);
        assert_eq!(content.groups[0].geometries.len(), 1, "rect should still parse");
    }

    #[test]
    fn parses_static_path_shape() {
        // A tiny closed triangle with zero tangents (straight-line
        // segments even though the renderer emits cubic_to). Exercises
        // the `sh` parse path end-to-end.
        let v = shape_layer_json(json!([
            {
                "ty": "gr",
                "it": [
                    {
                        "ty": "sh",
                        "ks": {
                            "a": 0,
                            "k": {
                                "v": [[0.0, 0.0], [100.0, 0.0], [50.0, 86.6]],
                                "i": [[0.0, 0.0], [0.0, 0.0], [0.0, 0.0]],
                                "o": [[0.0, 0.0], [0.0, 0.0], [0.0, 0.0]],
                                "c": true
                            }
                        }
                    },
                    {
                        "ty": "fl",
                        "c": { "k": [1.0, 0.5, 0.25, 1.0] },
                        "o": { "k": 100.0 }
                    }
                ]
            }
        ]));
        let content = ShapeContent::from_layer(&v, 60.0);
        assert_eq!(content.groups[0].geometries.len(), 1);
        match &content.groups[0].geometries[0] {
            Geometry::Path(AnimatedPath::Static(shape)) => {
                assert_eq!(shape.vertices.len(), 3);
                assert!(shape.closed);
            }
            other => panic!("expected static path, got {:?}", other),
        }
    }

    #[test]
    fn parses_animated_path_keyframes() {
        let v = shape_layer_json(json!([
            {
                "ty": "gr",
                "it": [
                    {
                        "ty": "sh",
                        "ks": {
                            "a": 1,
                            "k": [
                                {
                                    "t": 0.0,
                                    "s": [{
                                        "v": [[0.0, 0.0], [10.0, 0.0]],
                                        "i": [[0.0, 0.0], [0.0, 0.0]],
                                        "o": [[0.0, 0.0], [0.0, 0.0]],
                                        "c": false
                                    }]
                                },
                                {
                                    "t": 60.0,
                                    "s": [{
                                        "v": [[0.0, 10.0], [10.0, 10.0]],
                                        "i": [[0.0, 0.0], [0.0, 0.0]],
                                        "o": [[0.0, 0.0], [0.0, 0.0]],
                                        "c": false
                                    }]
                                }
                            ]
                        }
                    }
                ]
            }
        ]));
        let content = ShapeContent::from_layer(&v, 60.0);
        match &content.groups[0].geometries[0] {
            Geometry::Path(AnimatedPath::Keyframed(keys)) => {
                assert_eq!(keys.len(), 2);
                assert_eq!(keys[0].value.vertices[0], [0.0, 0.0]);
                assert_eq!(keys[1].value.vertices[0], [0.0, 10.0]);
                // 60 frames at 60 fps = 1 second.
                assert!((keys[1].t - 1.0).abs() < f32::EPSILON);
            }
            other => panic!("expected keyframed path, got {:?}", other),
        }
    }

    #[test]
    fn animated_path_morphs_mid_interval() {
        // Two keyframes at t=0s and t=1s, vertex 0 moves from
        // [0, 0] to [0, 10]. Mid-interval sample at t=0.5 should
        // land halfway (linear since no bezier tangents).
        let v = shape_layer_json(json!([
            {
                "ty": "gr",
                "it": [
                    {
                        "ty": "sh",
                        "ks": {
                            "a": 1,
                            "k": [
                                {
                                    "t": 0.0,
                                    "s": [{
                                        "v": [[0.0, 0.0], [10.0, 0.0]],
                                        "i": [[0.0, 0.0], [0.0, 0.0]],
                                        "o": [[0.0, 0.0], [0.0, 0.0]],
                                        "c": false
                                    }]
                                },
                                {
                                    "t": 60.0,
                                    "s": [{
                                        "v": [[0.0, 10.0], [10.0, 10.0]],
                                        "i": [[0.0, 0.0], [0.0, 0.0]],
                                        "o": [[0.0, 0.0], [0.0, 0.0]],
                                        "c": false
                                    }]
                                }
                            ]
                        }
                    }
                ]
            }
        ]));
        let content = ShapeContent::from_layer(&v, 60.0);
        let Geometry::Path(path) = &content.groups[0].geometries[0] else {
            panic!("expected path");
        };
        let mid = path.sample(0.5);
        assert_eq!(mid.vertices.len(), 2);
        assert!((mid.vertices[0][1] - 5.0).abs() < 1e-4);
        assert!((mid.vertices[1][1] - 5.0).abs() < 1e-4);
    }
}
