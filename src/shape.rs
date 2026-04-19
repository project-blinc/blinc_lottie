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

use blinc_core::draw::{LineCap, LineJoin, Path, PathCommand, Stroke};
use blinc_core::layer::{Brush, Color, CornerRadius, Point, Rect};
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
    /// Trim-path state from `tm`. When present, each geometry is
    /// flattened to a polyline, clipped to the trimmed arc-length
    /// window, and re-emitted before fill / stroke. `None` for
    /// groups without trim data (the common case).
    trim: Option<TrimPathSpec>,
    /// Nested `gr` groups — render after this group's own paint pass.
    children: Vec<ShapeGroup>,
}

/// Parsed state of a `ty: "tm"` trim-path shape item.
///
/// Lottie stores `s` (start) and `e` (end) as animated percentages
/// in `[0, 100]`; `o` (offset) is stored in degrees `[0, 360]` but
/// conceptually represents a fractional cycle offset. At sample
/// time the three scale into `[0, 1]` so the arc-length walker
/// doesn't have to repeat the conversion.
#[derive(Debug, Clone)]
struct TrimPathSpec {
    start: AnimatedF32,
    end: AnimatedF32,
    offset: AnimatedF32,
    /// `m: 1` = "Simultaneously" (all geometry paths concatenated
    /// and trimmed as one arc-length chain). `m: 2` =
    /// "Individually" (each geometry trimmed on its own). Both
    /// modes are applied — the simultaneous path produces a
    /// single `Path` from all geometry commands and runs
    /// `apply_trim` once against the unified arc length.
    mode: TrimMode,
}

#[derive(Debug, Clone, Copy)]
enum TrimMode {
    Simultaneous,
    Individual,
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

    pub(crate) fn to_path(&self) -> Path {
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
    pub(crate) fn sample(&self, t: f32) -> PathShape {
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

/// Paint source for a fill or stroke item. Solid color is the
/// original `fl` / `st` case; gradient variants cover `gf` / `gs`
/// (Lottie gradient fill / stroke).
///
/// Stops are currently parsed static-only — animating the stop
/// array mid-gradient is rare, and storing a full
/// `[t, r, g, b, ...]` keyframe list per gradient doubles the
/// shape-state size for a narrow use case. Endpoints (`start` /
/// `end`) are animatable via standard `AnimatedVec2`, which
/// covers most motion-design gradient animations (sliding the
/// gradient across a shape, radial zoom-in/out, etc.).
#[derive(Debug, Clone)]
enum Paint {
    Solid(AnimatedVec4),
    LinearGradient {
        start: AnimatedVec2,
        end: AnimatedVec2,
        stops: Vec<GradientStopSpec>,
    },
    RadialGradient {
        /// Gradient origin (Lottie `s`).
        start: AnimatedVec2,
        /// A point on the outer radius (Lottie `e`). Distance from
        /// `start` to `end` becomes the gradient radius — this
        /// matches how AE authors drag a radial gradient handle.
        end: AnimatedVec2,
        stops: Vec<GradientStopSpec>,
    },
}

/// A single parsed gradient stop in Blinc RGBA space (`[0, 1]`
/// per channel). Alpha stops from a Lottie gradient — stored in
/// the same flat array after the color stops — are merged into
/// the per-stop alpha at parse time rather than kept as a
/// separate channel, since Blinc's [`blinc_core::GradientStop`]
/// has one color-with-alpha value per offset.
#[derive(Debug, Clone, Copy)]
struct GradientStopSpec {
    offset: f32,
    color: [f32; 4],
}

#[derive(Debug, Clone)]
struct FillSpec {
    paint: Paint,
    /// 0–1 multiplier applied to the paint's alpha channel at
    /// sample time. Stacks on top of whatever alpha the paint
    /// itself carries (solid color alpha, gradient stop alphas).
    opacity: AnimatedF32,
}

#[derive(Debug, Clone)]
struct StrokeSpec {
    paint: Paint,
    opacity: AnimatedF32,
    /// Line width in source-space pixels.
    width: AnimatedF32,
    /// Endpoint cap style (Lottie `lc`: 1 = butt, 2 = round,
    /// 3 = square). Defaults to `Butt` to match the glTF /
    /// SVG/Lottie spec default.
    cap: LineCap,
    /// Corner join style (Lottie `lj`: 1 = miter, 2 = round,
    /// 3 = bevel).
    join: LineJoin,
    /// Miter-limit ratio (Lottie `ml` — default 4.0). Only
    /// meaningful with `LineJoin::Miter`; sharp corners past
    /// this ratio degrade to bevels to avoid spiking.
    miter_limit: f32,
    /// Dash / gap cycle in source-space units. Empty = solid line.
    /// Lottie's `st.d` is an array of `{ n: "d"/"g"/"o", v: {k} }`
    /// entries; alternating `d`/`g` values concatenate into this
    /// flat `[d, g, d, g, ...]` pattern that matches how the
    /// Blinc [`Stroke`] builder consumes it.
    ///
    /// Parsed static — animating individual dash lengths is rare
    /// enough that the data-shape cost (per-segment keyframe
    /// arrays) isn't worth it until a real asset needs it.
    dash_pattern: Vec<f32>,
    /// Dash offset (Lottie `st.d` entry with `n: "o"`). Animatable
    /// because the "marching ants" effect (offset ramped linearly)
    /// is the canonical use case.
    dash_offset: AnimatedF32,
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

    /// Local-frame AABB over every group in the content tree. Each
    /// group contributes its own post-transform bounds (including
    /// the `tr` shape item's displacement) and its children
    /// recursively. Returns `None` when the content has nothing
    /// paintable — the layer then declines the cull decision.
    pub(crate) fn local_bounds(&self, t: f32) -> Option<Rect> {
        let mut out: Option<Rect> = None;
        for group in &self.groups {
            if let Some(b) = group.local_bounds(t) {
                out = Some(merge_rects(out, b));
            }
        }
        out
    }

    /// Union every paintable geometry from every group into a
    /// single `Path`. Used by track mattes: the resulting path is
    /// pushed as a `ClipShape::Path` on the matted layer so the
    /// matte's alpha silhouette clips the matted content. Ignores
    /// fills / strokes / trims — track mattes only care about the
    /// shape's extent.
    ///
    /// For groups with multiple geometries, all of them contribute
    /// to the union path in source-space order (each becomes its
    /// own subpath). Nested `gr` children recurse through the same
    /// accumulator so compound mattes work as long as the clip
    /// implementation honours multi-subpath clips (it does — each
    /// subpath intersects via even-odd fill).
    pub(crate) fn extract_union_path(&self, t: f32) -> Option<Path> {
        let mut commands: Vec<PathCommand> = Vec::new();
        for group in &self.groups {
            group.accumulate_clip_commands(&mut commands, t);
        }
        if commands.is_empty() {
            None
        } else {
            Some(Path::from_commands(commands))
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

        // Sample the trim window once per frame so every geometry
        // in the group sees the same cut.
        let trim_window = self
            .trim
            .as_ref()
            .map(|tm| sample_trim_window(tm, t))
            .filter(|window| window.has_visible_range());
        let trim_mode = self.trim.as_ref().map(|tm| tm.mode);

        // Assemble the paths that this group will paint. For
        // `Simultaneous` mode we concatenate every geometry's
        // commands into a single `Path`, trim that, and paint once
        // — the `s` and `e` percentages span the TOTAL arc length
        // across all geometries, which is what AE's "Simultaneously"
        // option produces. For `Individual` (or no trim) we keep
        // the per-geometry loop so paths that are drawn with
        // different fills / strokes still render separately.
        let paths_to_paint: Vec<Path> = match (trim_window, trim_mode) {
            (Some(window), Some(TrimMode::Simultaneous)) => {
                // Concatenate every geometry's commands into one path.
                let mut combined: Vec<PathCommand> = Vec::new();
                for geo in &self.geometries {
                    for cmd in geo.to_path(t).commands() {
                        combined.push(cmd.clone());
                    }
                }
                if combined.is_empty() {
                    Vec::new()
                } else {
                    let one = Path::from_commands(combined);
                    vec![apply_trim(&one, window)]
                }
            }
            _ => self
                .geometries
                .iter()
                .map(|geo| {
                    let raw = geo.to_path(t);
                    match trim_window {
                        Some(window) => apply_trim(&raw, window),
                        None => raw,
                    }
                })
                .collect(),
        };

        for path in &paths_to_paint {
            if path.is_empty() {
                continue;
            }
            if let Some(fill) = &self.fill {
                let brush = sample_paint_brush(&fill.paint, &fill.opacity, t);
                dc.fill_path(path, brush);
            }
            if let Some(stroke) = &self.stroke {
                let width = stroke.width.sample(t).max(0.0);
                if width > 0.0 {
                    let brush = sample_paint_brush(&stroke.paint, &stroke.opacity, t);
                    let mut builder = Stroke::new(width)
                        .with_cap(stroke.cap)
                        .with_join(stroke.join);
                    builder.miter_limit = stroke.miter_limit;
                    if !stroke.dash_pattern.is_empty() {
                        builder = builder.with_dash(
                            stroke.dash_pattern.clone(),
                            stroke.dash_offset.sample(t),
                        );
                    }
                    dc.stroke_path(path, &builder, brush);
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

    /// Append every geometry's path commands to `out`, transformed
    /// through this group's own `tr` affine (if present) and
    /// recursing into nested children. Used by track mattes — the
    /// accumulated commands flatten the entire shape tree into a
    /// single multi-subpath `Path` that the clip stack can honour.
    fn accumulate_clip_commands(&self, out: &mut Vec<PathCommand>, t: f32) {
        let affine = self.transform.as_ref().map(|ts| {
            let xf = ts.sample(t);
            crate::layer::layer_local_affine(&xf)
        });
        for geo in &self.geometries {
            let path = geo.to_path(t);
            for cmd in path.commands() {
                out.push(transform_path_command(cmd, affine.as_ref()));
            }
        }
        for child in &self.children {
            child.accumulate_clip_commands(out, t);
        }
    }

    /// Union of every geometry and nested-group bound, propagated
    /// through this group's own `tr` transform if it has one.
    /// Geometry contributes its pre-trim bounds — computing the
    /// trimmed arc-length sub-path would be expensive and the
    /// author's trim window never extends past the original
    /// shape, so the pre-trim bound is a correct superset.
    fn local_bounds(&self, t: f32) -> Option<Rect> {
        let mut out: Option<Rect> = None;
        for geo in &self.geometries {
            if let Some(b) = geo.local_bounds(t) {
                out = Some(merge_rects(out, b));
            }
        }
        for child in &self.children {
            if let Some(b) = child.local_bounds(t) {
                out = Some(merge_rects(out, b));
            }
        }
        // Apply group-level `tr` transform to the aggregated
        // bounds. Rotation can expand the AABB; we take the
        // axis-aligned box around the 4 transformed corners.
        if let (Some(rect), Some(ts)) = (out, self.transform.as_ref()) {
            let xf = ts.sample(t);
            out = Some(transform_rect_aabb(&xf, rect));
        }
        out
    }
}

fn merge_rects(existing: Option<Rect>, next: Rect) -> Rect {
    match existing {
        Some(prev) => prev.union(&next),
        None => next,
    }
}

/// Transform every point in a `PathCommand` through `affine` if
/// provided, otherwise return the command unchanged. Arc endpoint
/// transforms work because Lottie exports arcs as cubic beziers
/// almost exclusively; `ArcTo` still passes through correctly for
/// the non-Lottie caller that might sit on top of this helper.
fn transform_path_command(cmd: &PathCommand, affine: Option<&blinc_core::layer::Affine2D>) -> PathCommand {
    let apply = |p: Point| match affine {
        None => p,
        Some(a) => {
            let [m0, m1, m2, m3, tx, ty] = a.elements;
            Point::new(m0 * p.x + m2 * p.y + tx, m1 * p.x + m3 * p.y + ty)
        }
    };
    match cmd {
        PathCommand::MoveTo(p) => PathCommand::MoveTo(apply(*p)),
        PathCommand::LineTo(p) => PathCommand::LineTo(apply(*p)),
        PathCommand::QuadTo { control, end } => PathCommand::QuadTo {
            control: apply(*control),
            end: apply(*end),
        },
        PathCommand::CubicTo {
            control1,
            control2,
            end,
        } => PathCommand::CubicTo {
            control1: apply(*control1),
            control2: apply(*control2),
            end: apply(*end),
        },
        PathCommand::ArcTo {
            radii,
            rotation,
            large_arc,
            sweep,
            end,
        } => PathCommand::ArcTo {
            radii: *radii,
            rotation: *rotation,
            large_arc: *large_arc,
            sweep: *sweep,
            end: apply(*end),
        },
        PathCommand::Close => PathCommand::Close,
    }
}

/// AABB of `rect` after applying group-level transform `xform`.
/// Thin wrapper over the shared helpers in [`crate::layer`] —
/// kept here so shape.rs doesn't have to know about `Affine2D`.
fn transform_rect_aabb(xform: &crate::layer::SampledTransform, rect: Rect) -> Rect {
    let affine = crate::layer::layer_local_affine(xform);
    crate::layer::transform_rect_through_affine(&affine, rect)
}

/// Arc-length window in normalized `[0, 1]` coordinates, ready for
/// the polyline trimmer. Sampled once per frame from a
/// [`TrimPathSpec`] so every geometry in the group sees the same
/// cut even if the sampler incurs measurable work.
#[derive(Debug, Clone, Copy)]
struct TrimWindow {
    start: f32,
    end: f32,
}

impl TrimWindow {
    fn has_visible_range(&self) -> bool {
        // Full-range trim (start ≈ 0, end ≈ 1) is a no-op; a degenerate
        // zero-length trim (start == end) paints nothing. Both get
        // fast-pathed here to save the flatten + walk cost.
        let span = self.end - self.start;
        span > 1e-6 && span < 1.0 - 1e-6
    }

    fn is_identity(&self) -> bool {
        self.start <= 1e-6 && self.end >= 1.0 - 1e-6
    }
}

fn sample_trim_window(spec: &TrimPathSpec, t: f32) -> TrimWindow {
    // Lottie spec: start / end are percentages [0, 100]; offset is
    // a fractional cycle expressed in degrees [0, 360]. Fold the
    // offset into both endpoints so we always emit a canonical
    // `[s, e]` span, wrapping through 1.0 when needed.
    let s = (spec.start.sample(t) / 100.0).clamp(0.0, 1.0);
    let e = (spec.end.sample(t) / 100.0).clamp(0.0, 1.0);
    // Full-range shortcut: `rem_euclid(1.0)` maps 1.0 back to 0.0,
    // which would destroy the identity case (s=0, e=1, offset=0).
    // Detect it before folding so `is_identity()` keeps working.
    if e - s >= 1.0 - 1e-6 {
        return TrimWindow { start: 0.0, end: 1.0 };
    }
    let offset = (spec.offset.sample(t) / 360.0).rem_euclid(1.0);
    let start = (s + offset).rem_euclid(1.0);
    let end = (e + offset).rem_euclid(1.0);
    TrimWindow { start, end }
}

/// Flatten each subpath of `path` into a polyline and emit only
/// the arc-length slice `[window.start, window.end]`. Output is a
/// polyline path (line_to only) — loses the cubic smoothness of
/// the original, but at the default subdivision density the
/// difference isn't visible at typical rendering scales.
fn apply_trim(path: &Path, window: TrimWindow) -> Path {
    if window.is_identity() {
        return path.clone();
    }
    // Flatten every subpath and compute the TOTAL arc length
    // across all of them up front. The window percentages map to
    // absolute arc lengths against that total — so a two-subpath
    // input shares a single arc-length chain rather than each
    // subpath being normalised independently. This matches AE's
    // trim-path-on-concatenation semantics for both the
    // Simultaneous mode (where callers concatenate multiple
    // geometries into one `Path` before calling) and the
    // single-geometry multi-subpath case.
    let subpaths: Vec<Vec<Point>> = flatten_subpaths(path)
        .into_iter()
        .filter(|p| p.len() >= 2)
        .collect();
    if subpaths.is_empty() {
        return Path::new();
    }
    let subpath_lengths: Vec<f32> = subpaths.iter().map(|p| polyline_length(p)).collect();
    let total: f32 = subpath_lengths.iter().sum();
    if total < 1e-6 {
        return Path::new();
    }

    let s_abs = window.start * total;
    let e_abs = window.end * total;
    let mut out = Path::new();
    if window.start <= window.end {
        emit_absolute_slice(&mut out, &subpaths, &subpath_lengths, s_abs, e_abs);
    } else {
        // Offset folded the window across the 0/1 boundary — emit
        // the tail of the chain followed by the head as two
        // separate subpaths.
        emit_absolute_slice(&mut out, &subpaths, &subpath_lengths, s_abs, total);
        emit_absolute_slice(&mut out, &subpaths, &subpath_lengths, 0.0, e_abs);
    }
    out
}

/// Walk `subpaths` with a running cumulative-length counter and
/// emit the portion that falls in `[s_abs, e_abs]` (absolute arc
/// lengths from the first subpath's start). Subpath boundaries
/// within the window translate into `MoveTo` / `LineTo` command
/// pairs; a slice that spans multiple subpaths starts a fresh
/// subpath at each boundary so the output polyline doesn't
/// connect disjoint subpaths with a phantom line.
fn emit_absolute_slice(
    out: &mut Path,
    subpaths: &[Vec<Point>],
    lengths: &[f32],
    s_abs: f32,
    e_abs: f32,
) {
    if e_abs <= s_abs {
        return;
    }
    let mut cumulative = 0.0_f32;
    for (subpath, &subpath_len) in subpaths.iter().zip(lengths.iter()) {
        let sub_start = cumulative;
        let sub_end = cumulative + subpath_len;
        cumulative = sub_end;
        if subpath_len < 1e-6 || sub_end < s_abs || sub_start > e_abs {
            continue;
        }
        // Local slice range inside this subpath's arc-length frame.
        let local_s = (s_abs - sub_start).max(0.0);
        let local_e = (e_abs - sub_start).min(subpath_len);
        emit_trimmed_subpath(
            out,
            subpath,
            TrimWindow {
                start: local_s / subpath_len,
                end: local_e / subpath_len,
            },
        );
    }
}

/// Flatten a path into separate polylines, one per subpath. A new
/// subpath begins on every `MoveTo`; `Close` stitches the last
/// vertex back to the subpath's start so the trim walker sees the
/// full cycle even for closed shapes.
fn flatten_subpaths(path: &Path) -> Vec<Vec<Point>> {
    const SAMPLES_PER_CURVE: usize = 24;
    let mut out: Vec<Vec<Point>> = Vec::new();
    let mut current = Point::new(0.0, 0.0);
    let mut subpath_start = current;
    let mut have_subpath = false;
    let begin_subpath = |pt: Point,
                             out: &mut Vec<Vec<Point>>,
                             have_subpath: &mut bool,
                             subpath_start: &mut Point| {
        out.push(vec![pt]);
        *subpath_start = pt;
        *have_subpath = true;
    };
    for cmd in path.commands() {
        match cmd {
            PathCommand::MoveTo(p) => {
                current = *p;
                begin_subpath(*p, &mut out, &mut have_subpath, &mut subpath_start);
            }
            PathCommand::LineTo(p) => {
                if !have_subpath {
                    begin_subpath(current, &mut out, &mut have_subpath, &mut subpath_start);
                }
                out.last_mut().unwrap().push(*p);
                current = *p;
            }
            PathCommand::QuadTo { control, end } => {
                if !have_subpath {
                    begin_subpath(current, &mut out, &mut have_subpath, &mut subpath_start);
                }
                let buf = out.last_mut().unwrap();
                for i in 1..=SAMPLES_PER_CURVE {
                    let u = i as f32 / SAMPLES_PER_CURVE as f32;
                    let mt = 1.0 - u;
                    buf.push(Point::new(
                        mt * mt * current.x + 2.0 * mt * u * control.x + u * u * end.x,
                        mt * mt * current.y + 2.0 * mt * u * control.y + u * u * end.y,
                    ));
                }
                current = *end;
            }
            PathCommand::CubicTo {
                control1,
                control2,
                end,
            } => {
                if !have_subpath {
                    begin_subpath(current, &mut out, &mut have_subpath, &mut subpath_start);
                }
                let buf = out.last_mut().unwrap();
                for i in 1..=SAMPLES_PER_CURVE {
                    let u = i as f32 / SAMPLES_PER_CURVE as f32;
                    let mt = 1.0 - u;
                    let mt2 = mt * mt;
                    let mt3 = mt2 * mt;
                    let u2 = u * u;
                    let u3 = u2 * u;
                    buf.push(Point::new(
                        mt3 * current.x
                            + 3.0 * mt2 * u * control1.x
                            + 3.0 * mt * u2 * control2.x
                            + u3 * end.x,
                        mt3 * current.y
                            + 3.0 * mt2 * u * control1.y
                            + 3.0 * mt * u2 * control2.y
                            + u3 * end.y,
                    ));
                }
                current = *end;
            }
            PathCommand::Close => {
                if have_subpath {
                    out.last_mut().unwrap().push(subpath_start);
                    current = subpath_start;
                }
            }
            PathCommand::ArcTo { .. } => {
                // ArcTo is rare in Lottie exports (shape items
                // rasterise to cubic beziers upstream); skipping
                // with a line_to to the endpoint keeps the
                // polyline contiguous instead of producing a gap.
                // Proper arc flattening can land when an asset
                // actually exercises this path.
            }
        }
    }
    out
}

/// Walk `points` and append the arc-length slice `[window.start,
/// window.end]` (normalized) to `out`. Handles the wrap case
/// (start > end after offset folding) by emitting two segments.
fn emit_trimmed_subpath(out: &mut Path, points: &[Point], window: TrimWindow) {
    let total = polyline_length(points);
    if total < 1e-6 {
        return;
    }
    let s = window.start * total;
    let e = window.end * total;
    if window.start <= window.end {
        emit_slice(out, points, s, e, total);
    } else {
        // Offset folded the window across the 0/1 boundary — emit
        // the tail of the path followed by the head as two
        // separate line runs.
        emit_slice(out, points, s, total, total);
        emit_slice(out, points, 0.0, e, total);
    }
}

fn emit_slice(out: &mut Path, points: &[Point], s: f32, e: f32, _total: f32) {
    if e <= s {
        return;
    }
    let mut cumulative = 0.0_f32;
    let mut started = false;
    for pair in points.windows(2) {
        let seg_start = cumulative;
        let seg_len = distance(pair[0], pair[1]);
        let seg_end = cumulative + seg_len;
        cumulative = seg_end;

        if seg_end < s || seg_start > e || seg_len < 1e-6 {
            continue;
        }

        let local_start = ((s - seg_start) / seg_len).clamp(0.0, 1.0);
        let local_end = ((e - seg_start) / seg_len).clamp(0.0, 1.0);

        let start_pt = lerp_point(pair[0], pair[1], local_start);
        let end_pt = lerp_point(pair[0], pair[1], local_end);

        let take = std::mem::take(out);
        let mut path = take;
        if !started {
            path = path.move_to(start_pt.x, start_pt.y);
            started = true;
        }
        if (end_pt.x - start_pt.x).abs() > 1e-5 || (end_pt.y - start_pt.y).abs() > 1e-5 {
            path = path.line_to(end_pt.x, end_pt.y);
        }
        *out = path;
    }
}

fn polyline_length(points: &[Point]) -> f32 {
    let mut total = 0.0;
    for pair in points.windows(2) {
        total += distance(pair[0], pair[1]);
    }
    total
}

fn distance(a: Point, b: Point) -> f32 {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    (dx * dx + dy * dy).sqrt()
}

fn lerp_point(a: Point, b: Point, u: f32) -> Point {
    Point::new(a.x + (b.x - a.x) * u, a.y + (b.y - a.y) * u)
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

    /// Axis-aligned bounds in the geometry's local frame (before
    /// the enclosing group's `tr` transform is applied). Returns
    /// `None` for degenerate inputs (empty path, zero-size rect /
    /// ellipse) so the caller can skip a bogus zero-rect union.
    ///
    /// Path bounds include each vertex plus its in/out tangent
    /// offset — cubic handles can extend the curve past the vertex
    /// convex hull, and culling must never false-positive.
    fn local_bounds(&self, t: f32) -> Option<Rect> {
        match self {
            Geometry::Rectangle { position, size, .. } => {
                let p = position.sample(t);
                let s = size.sample(t);
                if s[0] <= 0.0 || s[1] <= 0.0 {
                    return None;
                }
                Some(Rect::new(p[0] - s[0] * 0.5, p[1] - s[1] * 0.5, s[0], s[1]))
            }
            Geometry::Ellipse { position, size } => {
                let p = position.sample(t);
                let s = size.sample(t);
                if s[0] <= 0.0 || s[1] <= 0.0 {
                    return None;
                }
                Some(Rect::new(p[0] - s[0] * 0.5, p[1] - s[1] * 0.5, s[0], s[1]))
            }
            Geometry::Path(animated) => path_bounds(&animated.sample(t)),
        }
    }
}

/// Bounds over a `PathShape`'s vertices + per-vertex tangent
/// handles. Including the handles keeps strongly-curved paths
/// from false-culling when a control point extends past the
/// vertex ring (common on bulbous "puddle" or "speech bubble"
/// shapes). Absolute handle position = vertex + tangent offset.
fn path_bounds(shape: &PathShape) -> Option<Rect> {
    if shape.vertices.is_empty() {
        return None;
    }
    let mut min_x = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    let mut fold = |x: f32, y: f32| {
        min_x = min_x.min(x);
        max_x = max_x.max(x);
        min_y = min_y.min(y);
        max_y = max_y.max(y);
    };
    for (i, v) in shape.vertices.iter().enumerate() {
        fold(v[0], v[1]);
        if let Some(out_t) = shape.out_tangents.get(i) {
            fold(v[0] + out_t[0], v[1] + out_t[1]);
        }
        if let Some(in_t) = shape.in_tangents.get(i) {
            fold(v[0] + in_t[0], v[1] + in_t[1]);
        }
    }
    if !min_x.is_finite() {
        return None;
    }
    Some(Rect::new(min_x, min_y, (max_x - min_x).max(0.0), (max_y - min_y).max(0.0)))
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

/// Sample a [`Paint`] at time `t` into a Blinc [`Brush`]. Opacity
/// stacks on top of whatever alpha the paint already carries —
/// solid color alpha for `Paint::Solid`, per-stop alpha for the
/// gradient variants.
fn sample_paint_brush(paint: &Paint, opacity: &AnimatedF32, t: f32) -> Brush {
    let op = opacity.sample(t).clamp(0.0, 1.0);
    match paint {
        Paint::Solid(color) => Brush::Solid(sample_paint_color(color, opacity, t)),
        Paint::LinearGradient { start, end, stops } => {
            let s = start.sample(t);
            let e = end.sample(t);
            let stops = stops
                .iter()
                .map(|gs| {
                    blinc_core::GradientStop::new(
                        gs.offset,
                        Color::rgba(gs.color[0], gs.color[1], gs.color[2], gs.color[3] * op),
                    )
                })
                .collect();
            Brush::Gradient(blinc_core::Gradient::linear_with_stops(
                blinc_core::layer::Point::new(s[0], s[1]),
                blinc_core::layer::Point::new(e[0], e[1]),
                stops,
            ))
        }
        Paint::RadialGradient { start, end, stops } => {
            let s = start.sample(t);
            let e = end.sample(t);
            // Lottie encodes the radial gradient as two points: the
            // origin (`s`) and a point on the outer edge (`e`). The
            // radius is the Euclidean distance between them — matches
            // how AE's radial gradient handle drags outward from the
            // origin.
            let dx = e[0] - s[0];
            let dy = e[1] - s[1];
            let radius = (dx * dx + dy * dy).sqrt();
            let stops = stops
                .iter()
                .map(|gs| {
                    blinc_core::GradientStop::new(
                        gs.offset,
                        Color::rgba(gs.color[0], gs.color[1], gs.color[2], gs.color[3] * op),
                    )
                })
                .collect();
            Brush::Gradient(blinc_core::Gradient::radial_with_stops(
                blinc_core::layer::Point::new(s[0], s[1]),
                radius,
                stops,
            ))
        }
    }
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
        trim: None,
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
            "gf" => group.fill = Some(parse_gradient_fill(item, fr)),
            "st" => group.stroke = Some(parse_stroke(item, fr)),
            "gs" => group.stroke = Some(parse_gradient_stroke(item, fr)),
            "tr" => group.transform = Some(TransformSpec::from_value(Some(item), fr)),
            "tm" => group.trim = Some(parse_trim_path(item, fr)),
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
        trim: None,
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

fn parse_trim_path(v: &Value, fr: f32) -> TrimPathSpec {
    TrimPathSpec {
        start: parse_animated_scalar(v.get("s"), fr).unwrap_or(AnimatedF32::Static(0.0)),
        end: parse_animated_scalar(v.get("e"), fr).unwrap_or(AnimatedF32::Static(100.0)),
        offset: parse_animated_scalar(v.get("o"), fr).unwrap_or(AnimatedF32::Static(0.0)),
        mode: match v.get("m").and_then(Value::as_u64).unwrap_or(1) {
            2 => TrimMode::Individual,
            _ => TrimMode::Simultaneous,
        },
    }
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

/// Parse a path shape item (`ty: "sh"`). Thin wrapper that extracts
/// the `ks` payload and delegates to [`parse_animated_path`] — the
/// same parser masks on a layer use via `masksProperties[*].pt`.
fn parse_path(v: &Value, fr: f32) -> Geometry {
    let Some(ks) = v.get("ks") else {
        return Geometry::Path(AnimatedPath::Static(PathShape::empty()));
    };
    Geometry::Path(parse_animated_path(ks, fr))
}

/// Shared parser for the `{ "a": 0|1, "k": <value-or-keyframes> }`
/// animated-path payload. Consumed by both `sh` shape items and
/// layer-level `masksProperties[*].pt` entries — the JSON shape is
/// identical so there's no reason to duplicate the walker.
///
/// Static (`a: 0`): `k` holds a single `PathShape` object.
/// Animated (`a: 1`): `k` holds an array of keyframes.
/// Malformed input falls back to an empty path; matches the rest of
/// the shape-layer "best-effort" posture.
pub(crate) fn parse_animated_path(ks: &Value, fr: f32) -> AnimatedPath {
    let Some(k) = ks.get("k") else {
        return AnimatedPath::Static(PathShape::empty());
    };

    // Animated: `k` is an array of keyframes.
    if let Some(arr) = k.as_array() {
        if arr.first().map(|kf| kf.is_object()).unwrap_or(false)
            && arr.first().and_then(|kf| kf.get("t")).is_some()
        {
            let keys = collect_path_keys(arr, fr);
            return if keys.is_empty() {
                AnimatedPath::Static(PathShape::empty())
            } else {
                AnimatedPath::Keyframed(keys)
            };
        }
    }

    // Static: `k` is a single `PathShape` object.
    let shape = path_shape_from_value(k).unwrap_or_else(PathShape::empty);
    AnimatedPath::Static(shape)
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
        paint: Paint::Solid(
            parse_animated_vec4(v.get("c"), fr)
                .unwrap_or(AnimatedVec4::Static([0.0, 0.0, 0.0, 1.0])),
        ),
        opacity: parse_opacity(v, fr),
    }
}

fn parse_stroke(v: &Value, fr: f32) -> StrokeSpec {
    let (dash_pattern, dash_offset) = parse_dash(v, fr);
    StrokeSpec {
        paint: Paint::Solid(
            parse_animated_vec4(v.get("c"), fr)
                .unwrap_or(AnimatedVec4::Static([0.0, 0.0, 0.0, 1.0])),
        ),
        opacity: parse_opacity(v, fr),
        width: parse_animated_scalar(v.get("w"), fr).unwrap_or(AnimatedF32::Static(1.0)),
        cap: parse_line_cap(v.get("lc")),
        join: parse_line_join(v.get("lj")),
        miter_limit: v
            .get("ml")
            .and_then(Value::as_f64)
            .map(|n| n as f32)
            .unwrap_or(4.0),
        dash_pattern,
        dash_offset,
    }
}

/// Gradient fill (`gf`). Parses:
///
/// - `t`: `1` = linear, `2` = radial
/// - `s`: start point (animatable vec2) — gradient origin
/// - `e`: end point (animatable vec2) — tip for linear, outer-edge
///   handle for radial (distance from `s` becomes the radius)
/// - `g.p`: color-stop count
/// - `g.k.k`: flat stop array — first `4 * p` entries are color
///   stops (`[t, r, g, b, ...]`), optional tail is alpha stops
///   (`[t, a, t, a, ...]`) that merge into the per-offset alpha
///
/// Animated stop arrays (`g.k.a == 1`) are documented as out of
/// scope — the crate reads the keyframe array's first `s` value
/// and pins the stops to that snapshot. Animating start/end
/// endpoints works via the standard `AnimatedVec2` path.
fn parse_gradient_fill(v: &Value, fr: f32) -> FillSpec {
    FillSpec {
        paint: parse_gradient_paint(v, fr),
        opacity: parse_opacity(v, fr),
    }
}

fn parse_gradient_stroke(v: &Value, fr: f32) -> StrokeSpec {
    let (dash_pattern, dash_offset) = parse_dash(v, fr);
    StrokeSpec {
        paint: parse_gradient_paint(v, fr),
        opacity: parse_opacity(v, fr),
        width: parse_animated_scalar(v.get("w"), fr).unwrap_or(AnimatedF32::Static(1.0)),
        cap: parse_line_cap(v.get("lc")),
        join: parse_line_join(v.get("lj")),
        miter_limit: v
            .get("ml")
            .and_then(Value::as_f64)
            .map(|n| n as f32)
            .unwrap_or(4.0),
        dash_pattern,
        dash_offset,
    }
}

/// Lottie line-cap enum: 1 = butt, 2 = round, 3 = square. Any
/// unrecognised value (including absent field) maps to `Butt`,
/// matching the spec default.
fn parse_line_cap(v: Option<&Value>) -> LineCap {
    match v.and_then(Value::as_u64) {
        Some(2) => LineCap::Round,
        Some(3) => LineCap::Square,
        _ => LineCap::Butt,
    }
}

/// Lottie line-join enum: 1 = miter, 2 = round, 3 = bevel.
fn parse_line_join(v: Option<&Value>) -> LineJoin {
    match v.and_then(Value::as_u64) {
        Some(2) => LineJoin::Round,
        Some(3) => LineJoin::Bevel,
        _ => LineJoin::Miter,
    }
}

/// Parse a Lottie `st.d` / `gs.d` array into a flat dash pattern
/// (`[d, g, d, g, ...]`) plus a separate offset. Each element is
/// an object `{ n: "d"|"g"|"o", v: { k: <num_or_keyframes> } }`.
///
/// Dash / gap values are sampled at `t = 0` (static). Offset
/// stays animatable for the "marching ants" case. When the array
/// is absent or malformed the returned pattern is empty — Blinc's
/// [`Stroke`] treats empty dash as a solid line.
fn parse_dash(v: &Value, fr: f32) -> (Vec<f32>, AnimatedF32) {
    let Some(arr) = v.get("d").and_then(Value::as_array) else {
        return (Vec::new(), AnimatedF32::Static(0.0));
    };
    let mut pattern = Vec::new();
    let mut offset = AnimatedF32::Static(0.0);
    for entry in arr {
        let name = entry.get("n").and_then(Value::as_str).unwrap_or("");
        let value = parse_animated_scalar(entry.get("v"), fr);
        match name {
            "d" | "g" => {
                // Sample at the animation's start; `AnimatedF32`
                // with no keyframes resolves to a constant anyway.
                // Animating individual dash lengths per-frame
                // would need a re-alloc of the dash `Vec<f32>`
                // every sample tick — not worth it for this cut.
                if let Some(v) = value {
                    pattern.push(v.sample(0.0).max(0.0));
                }
            }
            "o" => {
                if let Some(v) = value {
                    offset = v;
                }
            }
            _ => {}
        }
    }
    (pattern, offset)
}

fn parse_gradient_paint(v: &Value, fr: f32) -> Paint {
    let kind = v.get("t").and_then(Value::as_u64).unwrap_or(1);
    let start = parse_animated_vec2(v.get("s"), fr).unwrap_or(AnimatedVec2::Static([0.0, 0.0]));
    let end = parse_animated_vec2(v.get("e"), fr).unwrap_or(AnimatedVec2::Static([0.0, 0.0]));
    let stop_count = v
        .get("g")
        .and_then(|g| g.get("p"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let stop_array = v
        .get("g")
        .and_then(|g| g.get("k"))
        .and_then(extract_static_stop_array);
    let stops = stop_array
        .map(|arr| parse_gradient_stops(&arr, stop_count))
        .unwrap_or_default();
    match kind {
        2 => Paint::RadialGradient { start, end, stops },
        _ => Paint::LinearGradient { start, end, stops },
    }
}

/// Extract the flat stop array from a gradient's `g.k` block,
/// which is shaped like `{ "a": 0|1, "k": <values> }`. For
/// `a == 0` the nested `k` is the flat array directly. For
/// `a == 1` it's a keyframe list; we take the first keyframe's
/// `s` snapshot as documented static-stop fallback.
fn extract_static_stop_array(gk: &Value) -> Option<Vec<f32>> {
    let k = gk.get("k")?;
    // Keyframed stops: pick the first keyframe's `s` snapshot.
    if let Some(arr) = k.as_array() {
        if arr
            .first()
            .map(|e| e.is_object() && e.get("t").is_some())
            .unwrap_or(false)
        {
            let first = arr.first()?;
            return first
                .get("s")
                .and_then(Value::as_array)
                .map(|s_arr| s_arr.iter().filter_map(|n| n.as_f64().map(|n| n as f32)).collect());
        }
        // Static flat array.
        return Some(arr.iter().filter_map(|n| n.as_f64().map(|n| n as f32)).collect());
    }
    None
}

/// Walk a flat gradient array into `GradientStopSpec`s. Layout:
///
/// ```text
/// [t0, r0, g0, b0, t1, r1, g1, b1, …   // `n_stops` color stops
///  at0, a0, at1, a1, …]                // optional alpha stops
/// ```
///
/// Color-stop count `n_stops` comes from `g.p`. Alpha stops are
/// paired by offset match (common case: same count + same offsets
/// as color stops) — merged into per-stop alpha. When alpha stops
/// are absent or mismatched we default to opaque.
fn parse_gradient_stops(arr: &[f32], n_stops: usize) -> Vec<GradientStopSpec> {
    if n_stops == 0 {
        return Vec::new();
    }
    let color_len = n_stops * 4;
    if arr.len() < color_len {
        return Vec::new();
    }
    let mut stops: Vec<GradientStopSpec> = (0..n_stops)
        .map(|i| {
            let base = i * 4;
            GradientStopSpec {
                offset: arr[base].clamp(0.0, 1.0),
                color: [arr[base + 1], arr[base + 2], arr[base + 3], 1.0],
            }
        })
        .collect();
    // Remaining elements after the color block are pairs of
    // (alpha-stop-offset, alpha-value). Apply each to the nearest
    // color stop by offset — handles the common case where the
    // alpha stops share offsets with color stops, and degrades
    // gracefully when they don't (alpha at the wrong stop is
    // visually subtle enough that the fallback is acceptable until
    // we grow per-offset alpha sampling).
    let alpha_region = &arr[color_len..];
    if alpha_region.len() >= 2 {
        for pair in alpha_region.chunks_exact(2) {
            let a_offset = pair[0].clamp(0.0, 1.0);
            let a_value = pair[1].clamp(0.0, 1.0);
            if let Some(nearest) = stops.iter_mut().min_by(|a, b| {
                (a.offset - a_offset)
                    .abs()
                    .partial_cmp(&(b.offset - a_offset).abs())
                    .unwrap_or(core::cmp::Ordering::Equal)
            }) {
                nearest.color[3] = a_value;
            }
        }
    }
    stops
}

#[inline]
fn parse_opacity(v: &Value, fr: f32) -> AnimatedF32 {
    parse_animated_scalar(v.get("o"), fr)
        .unwrap_or(AnimatedF32::Static(100.0))
        .map(|o| (o / 100.0).clamp(0.0, 1.0))
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
        let Paint::Solid(color) = &fill.paint else {
            panic!("expected solid paint, got {:?}", fill.paint);
        };
        let c = sample_paint_color(color, &fill.opacity, 0.0);
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
    fn parses_trim_path_item() {
        let v = shape_layer_json(json!([
            {
                "ty": "gr",
                "it": [
                    {
                        "ty": "rc",
                        "p": { "k": [0.0, 0.0, 0.0] },
                        "s": { "k": [100.0, 100.0, 0.0] },
                        "r": { "k": 0.0 }
                    },
                    {
                        "ty": "tm",
                        "s": { "k": 25.0 },
                        "e": { "k": 75.0 },
                        "o": { "k": 0.0 },
                        "m": 2
                    },
                    {
                        "ty": "fl",
                        "c": { "k": [1.0, 0.0, 0.0, 1.0] },
                        "o": { "k": 100.0 }
                    }
                ]
            }
        ]));
        let content = ShapeContent::from_layer(&v, 60.0);
        assert!(content.groups[0].trim.is_some(), "trim should parse");
        let trim = content.groups[0].trim.as_ref().unwrap();
        assert!((trim.start.sample(0.0) - 25.0).abs() < 1e-5);
        assert!((trim.end.sample(0.0) - 75.0).abs() < 1e-5);
    }

    #[test]
    fn trim_window_samples_normalize_and_fold_offset() {
        // start=25%, end=75%, offset=45° (= 0.125 cycles) shifts
        // the window forward by 0.125: [0.375, 0.875].
        let spec = TrimPathSpec {
            start: AnimatedF32::Static(25.0),
            end: AnimatedF32::Static(75.0),
            offset: AnimatedF32::Static(45.0),
            mode: TrimMode::Individual,
        };
        let window = sample_trim_window(&spec, 0.0);
        assert!((window.start - 0.375).abs() < 1e-5, "start = {}", window.start);
        assert!((window.end - 0.875).abs() < 1e-5, "end = {}", window.end);
        assert!(window.has_visible_range());

        // offset=270° pushes both endpoints past 1.0 and wraps —
        // start=0.25+0.75 mod 1 = 0.0, end=0.75+0.75 mod 1 = 0.5.
        // That's `start <= end` again; no wrap-rendering needed.
        let wrap_spec = TrimPathSpec {
            start: AnimatedF32::Static(25.0),
            end: AnimatedF32::Static(75.0),
            offset: AnimatedF32::Static(270.0),
            mode: TrimMode::Individual,
        };
        let wrap = sample_trim_window(&wrap_spec, 0.0);
        assert!(wrap.start < wrap.end);

        // Full-range trim is identity (fast-pathed upstream).
        let identity = TrimPathSpec {
            start: AnimatedF32::Static(0.0),
            end: AnimatedF32::Static(100.0),
            offset: AnimatedF32::Static(0.0),
            mode: TrimMode::Individual,
        };
        assert!(sample_trim_window(&identity, 0.0).is_identity());
    }

    #[test]
    fn apply_trim_cuts_polyline_to_half_length() {
        // A 4-unit horizontal line (two segments). Trimming to
        // [0, 0.5] should produce a 2-unit segment from (0,0) to
        // (2,0). Output is a polyline — count commands and check
        // final endpoint.
        let path = Path::new()
            .move_to(0.0, 0.0)
            .line_to(2.0, 0.0)
            .line_to(4.0, 0.0);
        let window = TrimWindow { start: 0.0, end: 0.5 };
        let trimmed = apply_trim(&path, window);
        let commands = trimmed.commands();
        // MoveTo + at least one LineTo; final endpoint ≈ (2, 0).
        assert!(matches!(commands[0], PathCommand::MoveTo(_)));
        let last_pt = match commands.last().unwrap() {
            PathCommand::LineTo(p) => *p,
            other => panic!("expected LineTo, got {other:?}"),
        };
        assert!((last_pt.x - 2.0).abs() < 1e-3, "end x ≈ 2, got {}", last_pt.x);
        assert!(last_pt.y.abs() < 1e-3);
    }

    #[test]
    fn apply_trim_wraps_when_offset_crosses_zero() {
        // Unit square, trim [0, 0.5] with offset 50% → wraps to
        // emit the last half of the rectangle followed by the
        // first half (two separate slice runs).
        let path = Path::new()
            .move_to(0.0, 0.0)
            .line_to(1.0, 0.0)
            .line_to(1.0, 1.0)
            .line_to(0.0, 1.0)
            .line_to(0.0, 0.0);
        // Window [0.75, 0.25] after offset-folding represents a
        // wrap (start > end) — two slice emissions.
        let window = TrimWindow { start: 0.75, end: 0.25 };
        let trimmed = apply_trim(&path, window);
        // Expect two MoveTo commands: one per slice.
        let move_tos = trimmed
            .commands()
            .iter()
            .filter(|c| matches!(c, PathCommand::MoveTo(_)))
            .count();
        assert_eq!(move_tos, 2, "wrap should emit two subpaths");
    }

    #[test]
    fn apply_trim_shares_arc_length_across_subpaths() {
        // Two subpaths of different lengths: 4 units + 8 units
        // (total 12). Trim [0, 0.5] expects the first 6 units,
        // which covers the whole first subpath (4) plus 2 units
        // into the second. Confirms the arc length is shared —
        // the earlier per-subpath implementation would have trimmed
        // each to 0.5 * its own length (2 + 4 = 6 as well, but
        // split as two halves; this test exercises the shared
        // behaviour explicitly).
        let path = Path::new()
            .move_to(0.0, 0.0)
            .line_to(4.0, 0.0)
            .move_to(0.0, 1.0)
            .line_to(8.0, 1.0);
        let window = TrimWindow { start: 0.0, end: 0.5 };
        let trimmed = apply_trim(&path, window);
        // Expect two subpaths in output: one for the full first
        // subpath (4 units) and one for the partial second
        // (2 units). Two `MoveTo` commands.
        let move_tos = trimmed
            .commands()
            .iter()
            .filter(|c| matches!(c, PathCommand::MoveTo(_)))
            .count();
        assert_eq!(move_tos, 2, "shared arc length should span both subpaths");

        // Last emitted line_to should be at x=2 of the second
        // subpath (its own frame starts at x=0, so the cut at
        // 6 - 4 = 2 units in yields point (2, 1)).
        let last = trimmed.commands().last().unwrap();
        match last {
            PathCommand::LineTo(p) => {
                assert!((p.x - 2.0).abs() < 0.01, "end x ≈ 2, got {}", p.x);
            }
            other => panic!("expected LineTo at end, got {other:?}"),
        }
    }

    #[test]
    fn apply_trim_individual_per_subpath_matches_old_behavior() {
        // Sanity check: when the caller wraps a single subpath in a
        // single Path (the `Individual` mode's per-geometry call
        // pattern), arc length is the subpath's own — matches
        // pre-refactor semantics.
        let path = Path::new()
            .move_to(0.0, 0.0)
            .line_to(10.0, 0.0);
        let trimmed = apply_trim(&path, TrimWindow { start: 0.0, end: 0.5 });
        let last = trimmed.commands().last().unwrap();
        if let PathCommand::LineTo(p) = last {
            assert!((p.x - 5.0).abs() < 0.01, "single-subpath trim → half length, got {}", p.x);
        } else {
            panic!("expected LineTo");
        }
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

    #[test]
    fn parses_linear_gradient_fill() {
        // Two color stops, red at 0 → blue at 1, linear gradient
        // running left-to-right across the shape.
        let v = shape_layer_json(json!([
            {
                "ty": "gr",
                "it": [
                    {
                        "ty": "rc",
                        "p": { "k": [0.0, 0.0, 0.0] },
                        "s": { "k": [100.0, 100.0, 0.0] },
                        "r": { "k": 0.0 }
                    },
                    {
                        "ty": "gf",
                        "t": 1,
                        "s": { "k": [0.0, 0.0] },
                        "e": { "k": [100.0, 0.0] },
                        "g": {
                            "p": 2,
                            "k": {
                                "a": 0,
                                "k": [
                                    0.0, 1.0, 0.0, 0.0,
                                    1.0, 0.0, 0.0, 1.0
                                ]
                            }
                        },
                        "o": { "k": 100.0 }
                    }
                ]
            }
        ]));
        let content = ShapeContent::from_layer(&v, 60.0);
        let fill = content.groups[0].fill.as_ref().expect("gradient fill parsed");
        let Paint::LinearGradient { stops, .. } = &fill.paint else {
            panic!("expected linear gradient, got {:?}", fill.paint);
        };
        assert_eq!(stops.len(), 2);
        assert!((stops[0].color[0] - 1.0).abs() < 1e-5, "first stop red");
        assert!((stops[1].color[2] - 1.0).abs() < 1e-5, "last stop blue");
    }

    #[test]
    fn parses_radial_gradient_stroke() {
        let v = shape_layer_json(json!([
            {
                "ty": "gr",
                "it": [
                    {
                        "ty": "el",
                        "p": { "k": [0.0, 0.0, 0.0] },
                        "s": { "k": [100.0, 100.0, 0.0] }
                    },
                    {
                        "ty": "gs",
                        "t": 2,
                        "s": { "k": [0.0, 0.0] },
                        "e": { "k": [50.0, 0.0] },
                        "g": {
                            "p": 2,
                            "k": {
                                "a": 0,
                                "k": [
                                    0.0, 1.0, 1.0, 1.0,
                                    1.0, 0.0, 0.0, 0.0
                                ]
                            }
                        },
                        "o": { "k": 100.0 },
                        "w": { "k": 3.0 }
                    }
                ]
            }
        ]));
        let content = ShapeContent::from_layer(&v, 60.0);
        let stroke = content.groups[0].stroke.as_ref().expect("gradient stroke parsed");
        assert!(matches!(stroke.paint, Paint::RadialGradient { .. }));
        assert!((stroke.width.sample(0.0) - 3.0).abs() < 1e-5);
    }

    #[test]
    fn parses_stroke_cap_join_and_miter_limit() {
        let v = shape_layer_json(json!([
            {
                "ty": "gr",
                "it": [
                    {
                        "ty": "rc",
                        "p": { "k": [0.0, 0.0, 0.0] },
                        "s": { "k": [100.0, 100.0, 0.0] },
                        "r": { "k": 0.0 }
                    },
                    {
                        "ty": "st",
                        "c": { "k": [0.0, 0.0, 0.0, 1.0] },
                        "o": { "k": 100.0 },
                        "w": { "k": 2.0 },
                        "lc": 2,  // round
                        "lj": 3,  // bevel
                        "ml": 10.0
                    }
                ]
            }
        ]));
        let content = ShapeContent::from_layer(&v, 60.0);
        let stroke = content.groups[0].stroke.as_ref().unwrap();
        assert_eq!(stroke.cap, LineCap::Round);
        assert_eq!(stroke.join, LineJoin::Bevel);
        assert!((stroke.miter_limit - 10.0).abs() < 1e-5);
    }

    #[test]
    fn parses_dash_pattern_with_offset() {
        let v = shape_layer_json(json!([
            {
                "ty": "gr",
                "it": [
                    {
                        "ty": "rc",
                        "p": { "k": [0.0, 0.0, 0.0] },
                        "s": { "k": [100.0, 100.0, 0.0] },
                        "r": { "k": 0.0 }
                    },
                    {
                        "ty": "st",
                        "c": { "k": [0.0, 0.0, 0.0, 1.0] },
                        "o": { "k": 100.0 },
                        "w": { "k": 2.0 },
                        "d": [
                            { "n": "d", "v": { "k": 10.0 } },
                            { "n": "g", "v": { "k": 5.0 } },
                            { "n": "o", "v": { "k": 3.0 } }
                        ]
                    }
                ]
            }
        ]));
        let content = ShapeContent::from_layer(&v, 60.0);
        let stroke = content.groups[0].stroke.as_ref().unwrap();
        assert_eq!(stroke.dash_pattern, vec![10.0, 5.0]);
        assert!((stroke.dash_offset.sample(0.0) - 3.0).abs() < 1e-5);
    }

    #[test]
    fn parses_multi_stride_dash_pattern() {
        // Lottie can encode patterns like "dash 10, gap 5, dash 20,
        // gap 3" as four d/g entries — they concatenate into a
        // single `[10, 5, 20, 3]` cycle that Blinc's `Stroke`
        // consumes directly.
        let v = shape_layer_json(json!([
            {
                "ty": "gr",
                "it": [
                    {
                        "ty": "rc",
                        "p": { "k": [0.0, 0.0, 0.0] },
                        "s": { "k": [100.0, 100.0, 0.0] },
                        "r": { "k": 0.0 }
                    },
                    {
                        "ty": "st",
                        "c": { "k": [0.0, 0.0, 0.0, 1.0] },
                        "o": { "k": 100.0 },
                        "w": { "k": 2.0 },
                        "d": [
                            { "n": "d", "v": { "k": 10.0 } },
                            { "n": "g", "v": { "k": 5.0 } },
                            { "n": "d", "v": { "k": 20.0 } },
                            { "n": "g", "v": { "k": 3.0 } }
                        ]
                    }
                ]
            }
        ]));
        let content = ShapeContent::from_layer(&v, 60.0);
        let stroke = content.groups[0].stroke.as_ref().unwrap();
        assert_eq!(stroke.dash_pattern, vec![10.0, 5.0, 20.0, 3.0]);
    }

    #[test]
    fn stroke_without_style_fields_uses_defaults() {
        let v = shape_layer_json(json!([
            {
                "ty": "gr",
                "it": [
                    {
                        "ty": "rc",
                        "p": { "k": [0.0, 0.0, 0.0] },
                        "s": { "k": [100.0, 100.0, 0.0] },
                        "r": { "k": 0.0 }
                    },
                    {
                        "ty": "st",
                        "c": { "k": [0.0, 0.0, 0.0, 1.0] },
                        "o": { "k": 100.0 },
                        "w": { "k": 2.0 }
                    }
                ]
            }
        ]));
        let content = ShapeContent::from_layer(&v, 60.0);
        let stroke = content.groups[0].stroke.as_ref().unwrap();
        assert_eq!(stroke.cap, LineCap::Butt);
        assert_eq!(stroke.join, LineJoin::Miter);
        assert!((stroke.miter_limit - 4.0).abs() < 1e-5);
        assert!(stroke.dash_pattern.is_empty());
    }

    #[test]
    fn gradient_alpha_stops_merge_into_color_alpha() {
        // Two color stops + two alpha stops at matching offsets.
        // Expectation: per-stop alpha from the alpha tail overrides
        // the default 1.0.
        let v = shape_layer_json(json!([
            {
                "ty": "gr",
                "it": [
                    {
                        "ty": "rc",
                        "p": { "k": [0.0, 0.0, 0.0] },
                        "s": { "k": [100.0, 100.0, 0.0] },
                        "r": { "k": 0.0 }
                    },
                    {
                        "ty": "gf",
                        "t": 1,
                        "s": { "k": [0.0, 0.0] },
                        "e": { "k": [100.0, 0.0] },
                        "g": {
                            "p": 2,
                            "k": {
                                "a": 0,
                                "k": [
                                    0.0, 1.0, 0.0, 0.0,
                                    1.0, 0.0, 0.0, 1.0,
                                    0.0, 0.25,
                                    1.0, 0.75
                                ]
                            }
                        },
                        "o": { "k": 100.0 }
                    }
                ]
            }
        ]));
        let content = ShapeContent::from_layer(&v, 60.0);
        let fill = content.groups[0].fill.as_ref().unwrap();
        let Paint::LinearGradient { stops, .. } = &fill.paint else {
            panic!("expected linear gradient");
        };
        assert!((stops[0].color[3] - 0.25).abs() < 1e-5);
        assert!((stops[1].color[3] - 0.75).abs() < 1e-5);
    }
}
