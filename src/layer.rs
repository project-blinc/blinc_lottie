//! Lottie layer types and rendering.
//!
//! `Layer` is the parsed, render-ready form of a single entry in the
//! Lottie composition's layer stack. The JSON parser hands us each layer
//! as an opaque `serde_json::Value`; [`Layer::from_value`] dispatches on
//! the `ty` field and produces a typed `Layer`.
//!
//! Phase 1.1 implemented **solid layers** (`ty: 1`). Phase 1.2 adds
//! **keyframe interpolation** for transform properties with linear easing
//! between keyframes. Bezier easing (`i.x` / `o.x` tangent handles) is
//! deferred — keyframes sampled through the linear path will look
//! slightly different from the source tool until then, but timing
//! offsets are already exact.
//!
//! Other layer types parse as [`LayerKind::Unknown`] and render as
//! no-ops.

use blinc_core::draw::Transform;
use blinc_core::layer::{Brush, Color, CornerRadius, Rect};
use blinc_core::DrawContext;
use serde_json::Value;

use crate::shape::ShapeContent;

/// Parsed, render-ready Lottie layer.
#[derive(Debug, Clone)]
pub(crate) struct Layer {
    pub kind: LayerKind,
    /// In-point in seconds. Layer is only rendered when
    /// `scene_t >= in_seconds`.
    pub in_seconds: f32,
    /// Out-point in seconds. Layer is only rendered when
    /// `scene_t < out_seconds`.
    pub out_seconds: f32,
    pub transform: TransformSpec,
}

#[derive(Debug, Clone)]
pub(crate) enum LayerKind {
    /// Solid color rectangle. `width` / `height` are in source-space
    /// pixels (the same coordinate system as the composition's `w`/`h`).
    Solid {
        width: f32,
        height: f32,
        color: Color,
    },
    /// Vector shape layer (`ty: 4`). Holds a tree of shape groups, each
    /// composed of geometry items + paint items (fill, stroke).
    Shape(ShapeContent),
    /// Layer types not yet implemented (image, text, precomp, null,
    /// …). Render as a no-op so the rest of the scene still composites
    /// correctly.
    Unknown,
}

// ─────────────────────────────────────────────────────────────────────────────
// Animated values
// ─────────────────────────────────────────────────────────────────────────────

/// Cubic-bezier ease control point stored on a keyframe. Lottie
/// encodes temporal easing as two tangents per keyframe: `out`
/// (this keyframe's contribution to the segment starting here) and
/// `in` (this keyframe's contribution to the segment ending here).
///
/// Interpolating between consecutive keyframes N → N+1 uses the
/// pair `(keys[N].out, keys[N+1].in)` as the middle two control
/// points of a cubic bezier whose endpoints are fixed at (0,0)
/// and (1,1) — a timing curve that maps linear progress
/// `u ∈ [0, 1]` to eased progress. See
/// [`solve_bezier_ease`] for the math.
///
/// Per-component tangents (`x`/`y` stored separately) are a Lottie
/// feature for axis-independent eases. We only read the first
/// component of each array for now — shared easing covers ~99% of
/// authored Lotties and matches After Effects' "Easy Ease"
/// button output.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BezierTangent {
    /// Normalized time component: where on `[0, 1]` this control
    /// point sits horizontally.
    pub x: f32,
    /// Normalized value component: where on `[0, 1]` this control
    /// point sits vertically (past 1 or below 0 = overshoot ease).
    pub y: f32,
}

/// A single keyframe in a scalar property animation.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ScalarKey {
    /// Keyframe time in seconds.
    pub t: f32,
    /// Value reached at `t`.
    pub value: f32,
    /// If `true`, hold this value until the next keyframe (no
    /// interpolation between this keyframe and the next).
    pub hold: bool,
    /// Out tangent — control point for the bezier segment STARTING
    /// at this keyframe. `None` means linear interpolation toward
    /// the next keyframe (also the fallback when JSON omits `o`).
    pub out_tangent: Option<BezierTangent>,
    /// In tangent — control point for the bezier segment ENDING at
    /// this keyframe. Consumed when interpolating from `N-1 → N`.
    pub in_tangent: Option<BezierTangent>,
}

/// A single keyframe in a 2D vector property animation.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Vec2Key {
    pub t: f32,
    pub value: [f32; 2],
    pub hold: bool,
    pub out_tangent: Option<BezierTangent>,
    pub in_tangent: Option<BezierTangent>,
}

/// A scalar property that may be static or keyframed.
#[derive(Debug, Clone)]
pub(crate) enum AnimatedF32 {
    Static(f32),
    Keyframed(Vec<ScalarKey>),
}

impl AnimatedF32 {
    pub(crate) fn sample(&self, t: f32) -> f32 {
        match self {
            Self::Static(v) => *v,
            Self::Keyframed(keys) => sample_scalar(keys, t),
        }
    }

    /// Apply `f` to every value in the animation (static value plus
    /// all keyframe values). Used to normalize Lottie source units
    /// (percent scale, degrees, 0–100 opacity) at parse time.
    pub(crate) fn map(self, f: impl Fn(f32) -> f32) -> Self {
        match self {
            Self::Static(v) => Self::Static(f(v)),
            Self::Keyframed(ks) => Self::Keyframed(
                ks.into_iter()
                    .map(|k| ScalarKey { value: f(k.value), ..k })
                    .collect(),
            ),
        }
    }
}

/// A 2D vector property that may be static or keyframed.
#[derive(Debug, Clone)]
pub(crate) enum AnimatedVec2 {
    Static([f32; 2]),
    Keyframed(Vec<Vec2Key>),
}

impl AnimatedVec2 {
    pub(crate) fn sample(&self, t: f32) -> [f32; 2] {
        match self {
            Self::Static(v) => *v,
            Self::Keyframed(keys) => sample_vec2(keys, t),
        }
    }

    pub(crate) fn map(self, f: impl Fn([f32; 2]) -> [f32; 2]) -> Self {
        match self {
            Self::Static(v) => Self::Static(f(v)),
            Self::Keyframed(ks) => Self::Keyframed(
                ks.into_iter()
                    .map(|k| Vec2Key { value: f(k.value), ..k })
                    .collect(),
            ),
        }
    }
}

/// A single keyframe in a 4D vector property animation (typically RGBA color).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Vec4Key {
    pub t: f32,
    pub value: [f32; 4],
    pub hold: bool,
    pub out_tangent: Option<BezierTangent>,
    pub in_tangent: Option<BezierTangent>,
}

/// A 4D vector property (e.g. RGBA color) that may be static or keyframed.
#[derive(Debug, Clone)]
pub(crate) enum AnimatedVec4 {
    Static([f32; 4]),
    Keyframed(Vec<Vec4Key>),
}

impl AnimatedVec4 {
    pub(crate) fn sample(&self, t: f32) -> [f32; 4] {
        match self {
            Self::Static(v) => *v,
            Self::Keyframed(keys) => sample_vec4(keys, t),
        }
    }
}

/// Evaluate the eased progress for a pair of keyframe tangents.
/// `u` is linear progress `∈ [0, 1]` between the two keyframes.
///
/// The bezier's endpoints are fixed at (0,0) and (1,1); middle
/// control points come from `out_tangent` of the earlier keyframe
/// and `in_tangent` of the later keyframe. Newton-solve
/// `bezier_x(t) = u` for `t`, then return `bezier_y(t)`.
///
/// After Effects' "Easy Ease" preset produces `(0.833, 0.833)` /
/// `(0.167, 0.167)` tangents → resolves to the classic S-curve.
/// Degenerate inputs (both control points on the line, tangents
/// outside `[0, 1]` for the x axis) fall back to linear.
fn solve_bezier_ease(u: f32, out_p: BezierTangent, in_p: BezierTangent) -> f32 {
    // Linear check — common case for tangents authored as "no ease"
    // (both control points lie on `y = x`). Skips eight Newton
    // iterations per keyframe interp on those.
    if (out_p.x - out_p.y).abs() < 1e-4 && (in_p.x - in_p.y).abs() < 1e-4 {
        return u;
    }
    let p1x = out_p.x;
    let p1y = out_p.y;
    let p2x = in_p.x;
    let p2y = in_p.y;
    // Newton's method on `bezier_x(t) - u = 0`, seeded at `t = u`.
    // Converges to 1e-5 in 2-4 iterations for well-shaped tangents;
    // the 8-iter cap defends against pathological control points
    // authored manually in JSON.
    let mut t = u;
    for _ in 0..8 {
        let ut = 1.0 - t;
        let bx = 3.0 * ut * ut * t * p1x + 3.0 * ut * t * t * p2x + t * t * t;
        let err = bx - u;
        if err.abs() < 1e-5 {
            break;
        }
        // dB/dt for the x component.
        let dbx = 3.0 * ut * ut * p1x
            + 6.0 * ut * t * (p2x - p1x)
            + 3.0 * t * t * (1.0 - p2x);
        if dbx.abs() < 1e-6 {
            // Derivative vanishes — accept the current estimate.
            break;
        }
        t -= err / dbx;
    }
    let t = t.clamp(0.0, 1.0);
    let ut = 1.0 - t;
    3.0 * ut * ut * t * p1y + 3.0 * ut * t * t * p2y + t * t * t
}

/// Compute eased progress between consecutive keyframes.
/// `linear_u` is `(t - k0.t) / (k1.t - k0.t)` ∈ `[0, 1]`.
#[inline]
pub(crate) fn eased_u(
    linear_u: f32,
    k0_out: Option<BezierTangent>,
    k1_in: Option<BezierTangent>,
) -> f32 {
    match (k0_out, k1_in) {
        (Some(o), Some(i)) => solve_bezier_ease(linear_u, o, i),
        _ => linear_u,
    }
}

fn sample_scalar(keys: &[ScalarKey], t: f32) -> f32 {
    if keys.is_empty() {
        return 0.0;
    }
    if t <= keys[0].t {
        return keys[0].value;
    }
    let last = keys.last().unwrap();
    if t >= last.t {
        return last.value;
    }
    for pair in keys.windows(2) {
        let k0 = &pair[0];
        let k1 = &pair[1];
        if t >= k0.t && t < k1.t {
            if k0.hold || (k1.t - k0.t).abs() < f32::EPSILON {
                return k0.value;
            }
            let linear_u = (t - k0.t) / (k1.t - k0.t);
            let u = eased_u(linear_u, k0.out_tangent, k1.in_tangent);
            return k0.value + (k1.value - k0.value) * u;
        }
    }
    last.value
}

fn sample_vec2(keys: &[Vec2Key], t: f32) -> [f32; 2] {
    if keys.is_empty() {
        return [0.0, 0.0];
    }
    if t <= keys[0].t {
        return keys[0].value;
    }
    let last = keys.last().unwrap();
    if t >= last.t {
        return last.value;
    }
    for pair in keys.windows(2) {
        let k0 = &pair[0];
        let k1 = &pair[1];
        if t >= k0.t && t < k1.t {
            if k0.hold || (k1.t - k0.t).abs() < f32::EPSILON {
                return k0.value;
            }
            let linear_u = (t - k0.t) / (k1.t - k0.t);
            let u = eased_u(linear_u, k0.out_tangent, k1.in_tangent);
            return [
                k0.value[0] + (k1.value[0] - k0.value[0]) * u,
                k0.value[1] + (k1.value[1] - k0.value[1]) * u,
            ];
        }
    }
    last.value
}

fn sample_vec4(keys: &[Vec4Key], t: f32) -> [f32; 4] {
    if keys.is_empty() {
        return [0.0, 0.0, 0.0, 0.0];
    }
    if t <= keys[0].t {
        return keys[0].value;
    }
    let last = keys.last().unwrap();
    if t >= last.t {
        return last.value;
    }
    for pair in keys.windows(2) {
        let k0 = &pair[0];
        let k1 = &pair[1];
        if t >= k0.t && t < k1.t {
            if k0.hold || (k1.t - k0.t).abs() < f32::EPSILON {
                return k0.value;
            }
            let linear_u = (t - k0.t) / (k1.t - k0.t);
            let u = eased_u(linear_u, k0.out_tangent, k1.in_tangent);
            return [
                k0.value[0] + (k1.value[0] - k0.value[0]) * u,
                k0.value[1] + (k1.value[1] - k0.value[1]) * u,
                k0.value[2] + (k1.value[2] - k0.value[2]) * u,
                k0.value[3] + (k1.value[3] - k0.value[3]) * u,
            ];
        }
    }
    last.value
}

// ─────────────────────────────────────────────────────────────────────────────
// Transform
// ─────────────────────────────────────────────────────────────────────────────

/// Animatable Lottie transform. Each field may be static or keyframed.
#[derive(Debug, Clone)]
pub(crate) struct TransformSpec {
    pub anchor: AnimatedVec2,
    pub position: AnimatedVec2,
    pub scale: AnimatedVec2,
    pub rotation: AnimatedF32,
    pub opacity: AnimatedF32,
}

impl TransformSpec {
    pub fn identity() -> Self {
        Self {
            anchor: AnimatedVec2::Static([0.0, 0.0]),
            position: AnimatedVec2::Static([0.0, 0.0]),
            scale: AnimatedVec2::Static([1.0, 1.0]),
            rotation: AnimatedF32::Static(0.0),
            opacity: AnimatedF32::Static(1.0),
        }
    }

    /// Sample every component at scene time `t`.
    pub fn sample(&self, t: f32) -> SampledTransform {
        SampledTransform {
            anchor: self.anchor.sample(t),
            position: self.position.sample(t),
            scale: self.scale.sample(t),
            rotation: self.rotation.sample(t),
            opacity: self.opacity.sample(t),
        }
    }

    pub(crate) fn from_value(v: Option<&Value>, fr: f32) -> Self {
        let Some(v) = v else { return Self::identity() };
        Self {
            anchor: parse_animated_vec2(v.get("a"), fr).unwrap_or(AnimatedVec2::Static([0.0, 0.0])),
            position: parse_animated_vec2(v.get("p"), fr)
                .unwrap_or(AnimatedVec2::Static([0.0, 0.0])),
            scale: parse_animated_vec2(v.get("s"), fr)
                .unwrap_or(AnimatedVec2::Static([100.0, 100.0]))
                .map(|[x, y]| [x / 100.0, y / 100.0]),
            rotation: parse_animated_scalar(v.get("r"), fr)
                .unwrap_or(AnimatedF32::Static(0.0))
                .map(f32::to_radians),
            opacity: parse_animated_scalar(v.get("o"), fr)
                .unwrap_or(AnimatedF32::Static(100.0))
                .map(|o| (o / 100.0).clamp(0.0, 1.0)),
        }
    }
}

/// Result of sampling a [`TransformSpec`] at a specific scene time.
/// Fields are in render-ready units (radians, multipliers, 0–1 opacity).
#[derive(Debug, Clone, Copy)]
pub(crate) struct SampledTransform {
    pub anchor: [f32; 2],
    pub position: [f32; 2],
    pub scale: [f32; 2],
    pub rotation: f32,
    pub opacity: f32,
}


// ─────────────────────────────────────────────────────────────────────────────
// Layer lifecycle
// ─────────────────────────────────────────────────────────────────────────────

impl Layer {
    /// Build a typed layer from a raw JSON object. `frame_rate` is used
    /// to convert the Lottie frame-based `ip`/`op` and keyframe times
    /// into seconds.
    pub fn from_value(v: &Value, frame_rate: f32) -> Self {
        let fr = frame_rate.max(1.0);
        let ty = v.get("ty").and_then(Value::as_u64).unwrap_or(99);
        let in_frames = v.get("ip").and_then(Value::as_f64).unwrap_or(0.0) as f32;
        let out_frames = v.get("op").and_then(Value::as_f64).unwrap_or(0.0) as f32;
        let transform = TransformSpec::from_value(v.get("ks"), fr);
        let kind = match ty {
            1 => parse_solid(v),
            4 => LayerKind::Shape(ShapeContent::from_layer(v, fr)),
            _ => LayerKind::Unknown,
        };
        Self {
            kind,
            in_seconds: in_frames / fr,
            out_seconds: out_frames / fr,
            transform,
        }
    }

    /// Render this layer into `dc` at scene time `scene_t`.
    ///
    /// Drawing happens in source-space coordinates; the caller is
    /// expected to have already pushed a transform mapping source space
    /// onto the destination rect.
    pub fn render(&self, dc: &mut dyn DrawContext, scene_t: f32) {
        if scene_t < self.in_seconds || scene_t >= self.out_seconds {
            return;
        }
        let xform = self.transform.sample(scene_t);
        push_layer_transform(dc, &xform);

        match &self.kind {
            LayerKind::Solid {
                width,
                height,
                color,
            } => {
                dc.fill_rect(
                    Rect::new(0.0, 0.0, *width, *height),
                    CornerRadius::uniform(0.0),
                    Brush::Solid(*color),
                );
            }
            LayerKind::Shape(content) => {
                content.render(dc, scene_t);
            }
            LayerKind::Unknown => {}
        }

        pop_layer_transform(dc);
    }
}

fn parse_solid(v: &Value) -> LayerKind {
    let width = v.get("sw").and_then(Value::as_f64).unwrap_or(0.0) as f32;
    let height = v.get("sh").and_then(Value::as_f64).unwrap_or(0.0) as f32;
    let color = v
        .get("sc")
        .and_then(Value::as_str)
        .and_then(parse_hex_color)
        .unwrap_or(Color::BLACK);
    LayerKind::Solid {
        width,
        height,
        color,
    }
}

/// Parse `#RRGGBB` or `#RGB`. Returns `None` for any other shape so
/// callers can fall back to a sensible default.
fn parse_hex_color(s: &str) -> Option<Color> {
    let hex = s.strip_prefix('#')?;
    let (r, g, b) = match hex.len() {
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            (r, g, b)
        }
        3 => {
            // Expand #RGB to #RRGGBB (each digit duplicated).
            let r = u8::from_str_radix(&hex[0..1], 16).ok()? * 17;
            let g = u8::from_str_radix(&hex[1..2], 16).ok()? * 17;
            let b = u8::from_str_radix(&hex[2..3], 16).ok()? * 17;
            (r, g, b)
        }
        _ => return None,
    };
    Some(Color::rgba(
        r as f32 / 255.0,
        g as f32 / 255.0,
        b as f32 / 255.0,
        1.0,
    ))
}

// ─────────────────────────────────────────────────────────────────────────────
// Animated-value parsing
// ─────────────────────────────────────────────────────────────────────────────

/// Parse an animatable scalar property — the common Lottie shape is:
///
/// ```json
/// { "a": 0, "k": 100 }                               // static scalar
/// { "a": 0, "k": [100] }                             // static, wrapped
/// { "a": 1, "k": [{ "t": 0, "s": [0] }, ...] }       // keyframed
/// ```
///
/// Returns `None` only if the surrounding object is missing entirely
/// (so the caller can supply a sensible default); otherwise this falls
/// back to `Static(0.0)` for malformed `k` payloads.
pub(crate) fn parse_animated_scalar(v: Option<&Value>, fr: f32) -> Option<AnimatedF32> {
    let v = v?;
    let k = v.get("k")?;

    if let Some(n) = k.as_f64() {
        return Some(AnimatedF32::Static(n as f32));
    }

    if let Some(arr) = k.as_array() {
        if arr.is_empty() {
            return Some(AnimatedF32::Static(0.0));
        }
        if arr[0].is_object() {
            // Keyframe array.
            let keys: Vec<ScalarKey> = collect_scalar_keys(arr, fr);
            if keys.is_empty() {
                return Some(AnimatedF32::Static(0.0));
            }
            return Some(AnimatedF32::Keyframed(keys));
        }
        // Static array — first element.
        return Some(AnimatedF32::Static(
            arr[0].as_f64().unwrap_or(0.0) as f32,
        ));
    }

    Some(AnimatedF32::Static(0.0))
}

/// Parse an animatable 2D vector property. Same payload shape as
/// [`parse_animated_scalar`], but value fields are `[x, y, z?]` and we
/// take the first two components.
pub(crate) fn parse_animated_vec2(v: Option<&Value>, fr: f32) -> Option<AnimatedVec2> {
    let v = v?;
    let k = v.get("k")?;

    if let Some(arr) = k.as_array() {
        if arr.is_empty() {
            return Some(AnimatedVec2::Static([0.0, 0.0]));
        }
        if arr[0].is_object() {
            let keys = collect_vec2_keys(arr, fr);
            if keys.is_empty() {
                return Some(AnimatedVec2::Static([0.0, 0.0]));
            }
            return Some(AnimatedVec2::Keyframed(keys));
        }
        // Static `[x, y, z?]` array.
        let x = arr.first().and_then(Value::as_f64).unwrap_or(0.0) as f32;
        let y = arr.get(1).and_then(Value::as_f64).unwrap_or(0.0) as f32;
        return Some(AnimatedVec2::Static([x, y]));
    }

    Some(AnimatedVec2::Static([0.0, 0.0]))
}

/// Parse an animatable 4D vector property — matches the shape of
/// [`parse_animated_vec2`] but with 4 components. Lottie uses this for
/// RGBA colors (`{ "k": [r, g, b, a] }` or keyframed equivalent).
pub(crate) fn parse_animated_vec4(v: Option<&Value>, fr: f32) -> Option<AnimatedVec4> {
    let v = v?;
    let k = v.get("k")?;

    if let Some(arr) = k.as_array() {
        if arr.is_empty() {
            return Some(AnimatedVec4::Static([0.0, 0.0, 0.0, 0.0]));
        }
        if arr[0].is_object() {
            let keys = collect_vec4_keys(arr, fr);
            if keys.is_empty() {
                return Some(AnimatedVec4::Static([0.0, 0.0, 0.0, 0.0]));
            }
            return Some(AnimatedVec4::Keyframed(keys));
        }
        // Static `[r, g, b, a]`. Missing components default to 0
        // (alpha defaults to 1 if only RGB was supplied).
        let r = arr.first().and_then(Value::as_f64).unwrap_or(0.0) as f32;
        let g = arr.get(1).and_then(Value::as_f64).unwrap_or(0.0) as f32;
        let b = arr.get(2).and_then(Value::as_f64).unwrap_or(0.0) as f32;
        let a = arr.get(3).and_then(Value::as_f64).unwrap_or(1.0) as f32;
        return Some(AnimatedVec4::Static([r, g, b, a]));
    }

    Some(AnimatedVec4::Static([0.0, 0.0, 0.0, 0.0]))
}

fn collect_vec4_keys(arr: &[Value], fr: f32) -> Vec<Vec4Key> {
    let mut last_value = [0.0f32, 0.0, 0.0, 1.0];
    arr.iter()
        .filter_map(|kf| {
            let t_frames = kf.get("t")?.as_f64()? as f32;
            let value = kf
                .get("s")
                .and_then(vec4_from_s)
                .unwrap_or(last_value);
            last_value = value;
            let hold = kf.get("h").and_then(Value::as_u64).unwrap_or(0) == 1;
            Some(Vec4Key {
                t: t_frames / fr,
                value,
                hold,
                out_tangent: tangent_from_key(kf, "o"),
                in_tangent: tangent_from_key(kf, "i"),
            })
        })
        .collect()
}

/// Extract a `BezierTangent` from a keyframe's `i` or `o` block.
///
/// Lottie JSON stores each tangent as either `{ "x": <num>, "y": <num> }`
/// (shared-axis easing) or `{ "x": [<num>, ...], "y": [<num>, ...] }`
/// (per-component easing). We take the scalar form directly and the
/// first array element for the array form — covers every ease
/// After Effects' "Easy Ease" preset emits, plus manually-authored
/// per-axis eases collapse cleanly to a single shared curve.
pub(crate) fn tangent_from_key(kf: &Value, field: &str) -> Option<BezierTangent> {
    let obj = kf.get(field)?;
    let x = scalar_or_first(obj.get("x")?)?;
    let y = scalar_or_first(obj.get("y")?)?;
    Some(BezierTangent { x, y })
}

fn scalar_or_first(v: &Value) -> Option<f32> {
    if let Some(arr) = v.as_array() {
        arr.first().and_then(Value::as_f64).map(|n| n as f32)
    } else {
        v.as_f64().map(|n| n as f32)
    }
}

fn vec4_from_s(v: &Value) -> Option<[f32; 4]> {
    let arr = v.as_array()?;
    let r = arr.first().and_then(Value::as_f64)? as f32;
    let g = arr.get(1).and_then(Value::as_f64)? as f32;
    let b = arr.get(2).and_then(Value::as_f64)? as f32;
    // Alpha is allowed to be missing — default to opaque to match
    // the static-array convention above.
    let a = arr.get(3).and_then(Value::as_f64).unwrap_or(1.0) as f32;
    Some([r, g, b, a])
}

fn collect_scalar_keys(arr: &[Value], fr: f32) -> Vec<ScalarKey> {
    // Exporters sometimes emit a trailing keyframe with no `s` marking
    // the animation's end timestamp. In that case reuse the previous
    // keyframe's value so interpolation has a well-defined endpoint.
    let mut last_value = 0.0f32;
    arr.iter()
        .filter_map(|kf| {
            let t_frames = kf.get("t")?.as_f64()? as f32;
            let value = kf
                .get("s")
                .and_then(scalar_from_s)
                .unwrap_or(last_value);
            last_value = value;
            let hold = kf.get("h").and_then(Value::as_u64).unwrap_or(0) == 1;
            Some(ScalarKey {
                t: t_frames / fr,
                value,
                hold,
                out_tangent: tangent_from_key(kf, "o"),
                in_tangent: tangent_from_key(kf, "i"),
            })
        })
        .collect()
}

fn collect_vec2_keys(arr: &[Value], fr: f32) -> Vec<Vec2Key> {
    let mut last_value = [0.0f32, 0.0f32];
    arr.iter()
        .filter_map(|kf| {
            let t_frames = kf.get("t")?.as_f64()? as f32;
            let value = kf
                .get("s")
                .and_then(vec2_from_s)
                .unwrap_or(last_value);
            last_value = value;
            let hold = kf.get("h").and_then(Value::as_u64).unwrap_or(0) == 1;
            Some(Vec2Key {
                t: t_frames / fr,
                value,
                hold,
                out_tangent: tangent_from_key(kf, "o"),
                in_tangent: tangent_from_key(kf, "i"),
            })
        })
        .collect()
}

fn scalar_from_s(v: &Value) -> Option<f32> {
    if let Some(arr) = v.as_array() {
        arr.first().and_then(Value::as_f64).map(|n| n as f32)
    } else {
        v.as_f64().map(|n| n as f32)
    }
}

fn vec2_from_s(v: &Value) -> Option<[f32; 2]> {
    let arr = v.as_array()?;
    let x = arr.first().and_then(Value::as_f64)? as f32;
    let y = arr.get(1).and_then(Value::as_f64)? as f32;
    Some([x, y])
}

// ─────────────────────────────────────────────────────────────────────────────
// Transform application
// ─────────────────────────────────────────────────────────────────────────────

pub(crate) fn push_layer_transform(dc: &mut dyn DrawContext, xform: &SampledTransform) {
    // Compose: T(p) · R(r) · S(s) · T(-a)
    // Pop order is the reverse, handled by `pop_layer_transform`.
    dc.push_transform(Transform::translate(xform.position[0], xform.position[1]));
    dc.push_transform(Transform::rotate(xform.rotation));
    dc.push_transform(Transform::scale(xform.scale[0], xform.scale[1]));
    dc.push_transform(Transform::translate(-xform.anchor[0], -xform.anchor[1]));
    dc.push_opacity(xform.opacity);
}

pub(crate) fn pop_layer_transform(dc: &mut dyn DrawContext) {
    dc.pop_opacity();
    dc.pop_transform();
    dc.pop_transform();
    dc.pop_transform();
    dc.pop_transform();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_six_digit_hex() {
        let c = parse_hex_color("#ff8000").unwrap();
        assert!((c.r - 1.0).abs() < 1e-6);
        assert!((c.g - (128.0 / 255.0)).abs() < 1e-6);
        assert!((c.b - 0.0).abs() < 1e-6);
        assert!((c.a - 1.0).abs() < 1e-6);
    }

    #[test]
    fn parses_three_digit_hex() {
        let c = parse_hex_color("#f80").unwrap();
        assert!((c.r - 1.0).abs() < 1e-6);
        assert!((c.g - (136.0 / 255.0)).abs() < 1e-6);
        assert!((c.b - 0.0).abs() < 1e-6);
    }

    #[test]
    fn rejects_bad_hex() {
        assert!(parse_hex_color("ff8000").is_none());
        assert!(parse_hex_color("#xyz").is_none());
        assert!(parse_hex_color("#ff80").is_none());
    }

    #[test]
    fn parses_solid_layer_with_static_transform() {
        let v = json!({
            "ty": 1,
            "ip": 0, "op": 60,
            "sw": 100.0, "sh": 200.0, "sc": "#80c0ff",
            "ks": {
                "p": { "k": [50.0, 75.0, 0.0] },
                "a": { "k": [10.0, 20.0, 0.0] },
                "s": { "k": [200.0, 200.0, 100.0] },
                "r": { "k": 90.0 },
                "o": { "k": 50.0 }
            }
        });
        let layer = Layer::from_value(&v, 60.0);
        let xf = layer.transform.sample(0.0);
        assert_eq!(xf.position, [50.0, 75.0]);
        assert_eq!(xf.anchor, [10.0, 20.0]);
        assert_eq!(xf.scale, [2.0, 2.0]);
        assert!((xf.rotation - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
        assert!((xf.opacity - 0.5).abs() < 1e-6);
    }

    #[test]
    fn unknown_type_falls_through() {
        // ty: 99 is not a real Lottie type — picks up the Unknown path.
        // (ty: 4 is now Shape; ty: 1 is Solid.)
        let v = json!({ "ty": 99, "ip": 0, "op": 60 });
        let layer = Layer::from_value(&v, 60.0);
        assert!(matches!(layer.kind, LayerKind::Unknown));
    }

    #[test]
    fn linear_interpolation_midpoint() {
        // Keyframe array: t=0 → opacity 0%, t=60 → opacity 100%, at 60 fps.
        let v = json!({
            "ty": 1,
            "ip": 0, "op": 60,
            "sw": 10.0, "sh": 10.0, "sc": "#000000",
            "ks": {
                "o": {
                    "a": 1,
                    "k": [
                        { "t": 0,  "s": [0.0]   },
                        { "t": 60, "s": [100.0] }
                    ]
                }
            }
        });
        let layer = Layer::from_value(&v, 60.0);
        // At t=0.5s (midpoint between 0s and 1s keyframes) opacity → 0.5
        let xf = layer.transform.sample(0.5);
        assert!((xf.opacity - 0.5).abs() < 1e-5, "expected 0.5, got {}", xf.opacity);
        // Before first keyframe clamps to first.
        assert!((layer.transform.sample(-1.0).opacity - 0.0).abs() < 1e-5);
        // After last keyframe clamps to last.
        assert!((layer.transform.sample(10.0).opacity - 1.0).abs() < 1e-5);
    }

    #[test]
    fn vec2_keyframe_interpolation() {
        let v = json!({
            "ty": 1,
            "ip": 0, "op": 120,
            "sw": 10.0, "sh": 10.0, "sc": "#000000",
            "ks": {
                "p": {
                    "a": 1,
                    "k": [
                        { "t": 0,   "s": [0.0, 0.0, 0.0] },
                        { "t": 120, "s": [200.0, 400.0, 0.0] }
                    ]
                }
            }
        });
        let layer = Layer::from_value(&v, 60.0);
        // t=1.0s = halfway through the 2s interval → (100, 200)
        let xf = layer.transform.sample(1.0);
        assert!((xf.position[0] - 100.0).abs() < 1e-4);
        assert!((xf.position[1] - 200.0).abs() < 1e-4);
    }

    #[test]
    fn hold_keyframe_skips_interpolation() {
        let v = json!({
            "ty": 1,
            "ip": 0, "op": 60,
            "sw": 10.0, "sh": 10.0, "sc": "#000000",
            "ks": {
                "o": {
                    "a": 1,
                    "k": [
                        { "t": 0,  "s": [0.0],   "h": 1 },
                        { "t": 60, "s": [100.0]          }
                    ]
                }
            }
        });
        let layer = Layer::from_value(&v, 60.0);
        // Inside the hold segment the value stays at 0 regardless of t.
        assert!((layer.transform.sample(0.25).opacity - 0.0).abs() < 1e-5);
        assert!((layer.transform.sample(0.75).opacity - 0.0).abs() < 1e-5);
        // Exactly at (or past) the next keyframe, the new value applies.
        assert!((layer.transform.sample(1.0).opacity - 1.0).abs() < 1e-5);
    }
}
