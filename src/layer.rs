//! Lottie layer types and rendering.
//!
//! `Layer` is the parsed, render-ready form of a single entry in the
//! Lottie composition's layer stack. The JSON parser hands us each layer
//! as an opaque `serde_json::Value`; [`Layer::from_value`] dispatches on
//! the `ty` field and produces a typed `Layer`.
//!
//! Solid layers (`ty: 1`) and shape layers (`ty: 4`) render. Every
//! animatable transform property (position, anchor, scale, rotation,
//! opacity, plus per-component shape properties) supports keyframes
//! with hold, linear, and cubic-bezier temporal easing. Bezier
//! tangents are solved via [`solve_bezier_ease`] (Newton's method on
//! `bezier_x(t) = u`); per-component tangents fold into a shared
//! easing by taking the first array element.
//!
//! Other layer types parse as [`LayerKind::Unknown`] and render as
//! no-ops.

use blinc_core::draw::{LayerConfig, LayerEffect, Transform};
use blinc_core::layer::{Affine2D, Brush, ClipShape, Color, CornerRadius, Point, Rect};
use blinc_core::DrawContext;
use serde_json::Value;

use crate::shape::{parse_animated_path, AnimatedPath, ShapeContent};

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
    /// Layer-level `masksProperties` — clips the layer content to
    /// the intersection of all `Add`-mode mask paths. Other modes
    /// (Subtract / Intersect / Lighten / Darken / Difference) parse
    /// but don't apply yet; they're rarer in real-world assets and
    /// would need either inverse-clip support from `DrawContext` or
    /// an offscreen render pass.
    pub masks: Vec<MaskSpec>,
    /// Layer's own identifier (the Lottie `ind` field). Used as the
    /// key another layer's `parent` points at. `None` when the
    /// export omitted `ind` — rare but tolerated; such a layer
    /// can't be targeted as a parent but still renders.
    pub ind: Option<i32>,
    /// Direct parent's `ind` (Lottie `parent`). `None` for
    /// top-level layers. Resolution into the composition's layer
    /// vec happens in [`resolve_parent_chains`]; consumers should
    /// use [`Self::parent_chain`] instead of resolving this again
    /// per frame.
    pub parent_ind: Option<i32>,
    /// Ancestor chain in render order — outermost ancestor first,
    /// direct parent last. Indices refer to slots in the player's
    /// layer vec. Populated once per load by
    /// [`resolve_parent_chains`]; forward-referenced or cyclic
    /// parents produce an empty chain so malformed exports
    /// render the layer un-parented rather than looping forever.
    pub parent_chain: Vec<usize>,
    /// Effect-layer chain parsed from `ef`. Drop shadow / Gaussian
    /// blur are applied; unsupported effect types (Tritone, Fill,
    /// Slider Control …) parse as `None` and get dropped. Rendered
    /// by wrapping the layer's paint in a `push_layer` with the
    /// sampled [`LayerEffect`]s — blur radii and shadow offsets
    /// are still in source-space units because the layer's own
    /// transform is applied *inside* that push.
    pub effects: Vec<EffectSpec>,
}

/// Parsed Lottie layer effect. Currently restricted to the two
/// effect types dotLottie-produced assets actually ship with:
/// Drop Shadow and Gaussian Blur. Other effect types (Glow,
/// Tritone, Color Balance, Fill, Slider Control) parse-and-skip
/// — we document that explicitly so the loader's permissive
/// posture isn't mistaken for "everything is supported."
#[derive(Debug, Clone)]
pub(crate) enum EffectSpec {
    /// Lottie effect type 25. Four animatable properties:
    /// color, opacity (0–255 → normalised to 0–1), direction
    /// (degrees, AE "north = 0°"), distance, softness.
    DropShadow {
        color: AnimatedVec4,
        opacity: AnimatedF32,
        direction: AnimatedF32,
        distance: AnimatedF32,
        softness: AnimatedF32,
    },
    /// Lottie effect type 29. One animatable property: Blurriness.
    /// "Blur Dimensions" (horizontal / vertical / both) parses but
    /// isn't applied — every blur is both-axis Gaussian.
    Blur { radius: AnimatedF32 },
}

impl EffectSpec {
    /// Sample the effect into Blinc's [`LayerEffect`] representation
    /// for a specific scene time.
    pub(crate) fn sample(&self, t: f32) -> LayerEffect {
        match self {
            EffectSpec::DropShadow {
                color,
                opacity,
                direction,
                distance,
                softness,
            } => {
                let c = color.sample(t);
                let o = opacity.sample(t);
                let dir_rad = direction.sample(t).to_radians();
                let dist = distance.sample(t);
                // AE convention: direction 0° points up, 90° right,
                // 180° down, 270° left. Screen-space Y grows
                // downward, so flip the cosine.
                let dx = dist * dir_rad.sin();
                let dy = -dist * dir_rad.cos();
                LayerEffect::DropShadow {
                    offset_x: dx,
                    offset_y: dy,
                    blur: softness.sample(t).max(0.0),
                    spread: 0.0,
                    color: Color::rgba(c[0], c[1], c[2], c[3] * o),
                }
            }
            EffectSpec::Blur { radius } => LayerEffect::blur(radius.sample(t).max(0.0)),
        }
    }
}

/// Parse a single entry from a layer's `ef` array. Returns `None`
/// for unsupported effect types — the caller drops them, matching
/// the shape-item catch-all pattern elsewhere in this module.
pub(crate) fn parse_effect(v: &Value, fr: f32) -> Option<EffectSpec> {
    let ty = v.get("ty").and_then(Value::as_u64)?;
    // Each effect parameter is `{ "ty": <kind>, "nm": "...", "v": { "a": ..., "k": ... } }`.
    // Peel off the `v` wrapper so the shared animated-value
    // parsers work without modification.
    let params = v.get("ef").and_then(Value::as_array)?;
    let prop_v = |idx: usize| params.get(idx).and_then(|p| p.get("v"));
    match ty {
        25 => Some(EffectSpec::DropShadow {
            color: parse_animated_vec4(prop_v(0), fr)
                .unwrap_or(AnimatedVec4::Static([0.0, 0.0, 0.0, 1.0])),
            // Opacity authored as 0–255 in AE's Drop Shadow dialog;
            // normalise once at load so renderers see the Blinc
            // convention (0–1). Any keyframe values outside that
            // range clamp at sample time via `Color::rgba` below.
            opacity: parse_animated_scalar(prop_v(1), fr)
                .unwrap_or(AnimatedF32::Static(255.0))
                .map(|o| (o / 255.0).clamp(0.0, 1.0)),
            direction: parse_animated_scalar(prop_v(2), fr).unwrap_or(AnimatedF32::Static(0.0)),
            distance: parse_animated_scalar(prop_v(3), fr).unwrap_or(AnimatedF32::Static(0.0)),
            softness: parse_animated_scalar(prop_v(4), fr).unwrap_or(AnimatedF32::Static(0.0)),
        }),
        29 => Some(EffectSpec::Blur {
            radius: parse_animated_scalar(prop_v(0), fr).unwrap_or(AnimatedF32::Static(0.0)),
        }),
        _ => None,
    }
}

/// A single mask entry from a layer's `masksProperties` array.
/// Paths reuse the same `AnimatedPath` machinery that `sh` shape
/// items use — Lottie stores both in the identical `{ a, k }`
/// wrapper shape.
#[derive(Debug, Clone)]
pub(crate) struct MaskSpec {
    pub mode: MaskMode,
    pub path: AnimatedPath,
    /// Mask opacity. Stored but not yet consumed in render — Blinc's
    /// `push_clip` is binary (in or out), so a per-mask alpha ramp
    /// needs an offscreen composite pass that's out of scope for
    /// this cut.
    #[allow(dead_code)]
    pub opacity: AnimatedF32,
    /// Invert flag (Lottie `inv`). Parsed for forward-compat; not
    /// applied until `Subtract` mode lands, at which point invert
    /// becomes the same operation with the flag flipped.
    #[allow(dead_code)]
    pub invert: bool,
}

/// Combination mode for a single mask. Parsed from Lottie's
/// single-letter encoding (`"a"`, `"s"`, `"i"`, `"l"`, `"da"`,
/// `"f"`). Only `Add` is rendered in this revision; others snap
/// to `Add` with a trace-level warning so unsupported files still
/// render a best-effort approximation instead of nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MaskMode {
    Add,
    Subtract,
    Intersect,
    Lighten,
    Darken,
    Difference,
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
    /// Null layer (`ty: 3`) — transform-only, renders nothing of
    /// its own. Exists so other layers can parent to it: the null's
    /// transform participates in its children's
    /// [`Layer::parent_chain`], while its own `render` is a no-op.
    /// Distinguished from [`LayerKind::Unknown`] so intent stays
    /// readable in Debug output and asset inspections.
    Null,
    /// Layer types not yet implemented (image, text, precomp, …).
    /// Render as a no-op so the rest of the scene still composites
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
/// Per-component tangents (`x` / `y` stored separately) are a
/// Lottie feature for axis-independent eases. `Vec2Key` and
/// `Vec4Key` keep per-axis tangents so each component can resolve
/// its own curve; `ScalarKey` has only one axis of value and
/// so needs one tangent pair.
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
/// Tangents are stored per-axis so position-style properties can
/// ease X and Y independently when the author wants that. Both
/// slots always hold the same value when the JSON uses the
/// shared-tangent shape — `tangents_from_key_per_axis` folds the
/// `{ x: 0.5, y: 0.0 }` form into `[Some(t), Some(t)]`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Vec2Key {
    pub t: f32,
    pub value: [f32; 2],
    pub hold: bool,
    pub out_tangent: [Option<BezierTangent>; 2],
    pub in_tangent: [Option<BezierTangent>; 2],
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
/// Tangents stored per-component; see [`Vec2Key`] for the shared-vs-per-axis
/// folding convention.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Vec4Key {
    pub t: f32,
    pub value: [f32; 4],
    pub hold: bool,
    pub out_tangent: [Option<BezierTangent>; 4],
    pub in_tangent: [Option<BezierTangent>; 4],
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

/// Binary-search the keyframe index `k0` such that
/// `keys[k0].t <= t < keys[k0+1].t`. Callers handle the before /
/// after edge cases before dispatching here; this helper never
/// returns the last index (no `k1` after it).
///
/// Replaces a linear `.windows(2)` scan — O(n) per property per
/// frame — with `partition_point`, which is O(log n). Big win on
/// hand-authored timelines with dozens of keyframes; no cost on
/// the short ones (2-3 keys) since the log2 is already trivial.
#[inline]
fn find_segment_index<K, F: Fn(&K) -> f32>(keys: &[K], t: f32, time_of: F) -> usize {
    // `partition_point(|k| k.t <= t)` returns the index of the first
    // key with `t > target`, so the previous slot is `k0`. Guarded
    // to `[1, n-1]` so `-1` and `k1 = keys[idx]` stay in bounds.
    let idx = keys.partition_point(|k| time_of(k) <= t);
    idx.clamp(1, keys.len() - 1) - 1
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
    let idx = find_segment_index(keys, t, |k| k.t);
    let k0 = &keys[idx];
    let k1 = &keys[idx + 1];
    if k0.hold || (k1.t - k0.t).abs() < f32::EPSILON {
        return k0.value;
    }
    let linear_u = (t - k0.t) / (k1.t - k0.t);
    let u = eased_u(linear_u, k0.out_tangent, k1.in_tangent);
    k0.value + (k1.value - k0.value) * u
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
    let idx = find_segment_index(keys, t, |k| k.t);
    let k0 = &keys[idx];
    let k1 = &keys[idx + 1];
    if k0.hold || (k1.t - k0.t).abs() < f32::EPSILON {
        return k0.value;
    }
    let linear_u = (t - k0.t) / (k1.t - k0.t);
    // Per-axis ease: each component resolves its own bezier curve
    // from the `(k0.out[i], k1.in[i])` pair. Shared-curve keyframes
    // fold to identical slots at parse time so the per-axis path
    // stays as fast as the shared one on that common input.
    let ux = eased_u(linear_u, k0.out_tangent[0], k1.in_tangent[0]);
    let uy = eased_u(linear_u, k0.out_tangent[1], k1.in_tangent[1]);
    [
        k0.value[0] + (k1.value[0] - k0.value[0]) * ux,
        k0.value[1] + (k1.value[1] - k0.value[1]) * uy,
    ]
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
    let idx = find_segment_index(keys, t, |k| k.t);
    let k0 = &keys[idx];
    let k1 = &keys[idx + 1];
    if k0.hold || (k1.t - k0.t).abs() < f32::EPSILON {
        return k0.value;
    }
    let linear_u = (t - k0.t) / (k1.t - k0.t);
    // Per-component ease — see `sample_vec2` for the
    // shared-vs-per-axis folding convention.
    let u0 = eased_u(linear_u, k0.out_tangent[0], k1.in_tangent[0]);
    let u1 = eased_u(linear_u, k0.out_tangent[1], k1.in_tangent[1]);
    let u2 = eased_u(linear_u, k0.out_tangent[2], k1.in_tangent[2]);
    let u3 = eased_u(linear_u, k0.out_tangent[3], k1.in_tangent[3]);
    [
        k0.value[0] + (k1.value[0] - k0.value[0]) * u0,
        k0.value[1] + (k1.value[1] - k0.value[1]) * u1,
        k0.value[2] + (k1.value[2] - k0.value[2]) * u2,
        k0.value[3] + (k1.value[3] - k0.value[3]) * u3,
    ]
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
            3 => LayerKind::Null,
            4 => LayerKind::Shape(ShapeContent::from_layer(v, fr)),
            _ => LayerKind::Unknown,
        };
        let masks = v
            .get("masksProperties")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().map(|m| parse_mask(m, fr)).collect())
            .unwrap_or_default();
        let ind = v.get("ind").and_then(Value::as_i64).map(|n| n as i32);
        let parent_ind = v.get("parent").and_then(Value::as_i64).map(|n| n as i32);
        let effects = v
            .get("ef")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(|e| parse_effect(e, fr)).collect())
            .unwrap_or_default();
        Self {
            kind,
            in_seconds: in_frames / fr,
            out_seconds: out_frames / fr,
            transform,
            masks,
            ind,
            parent_ind,
            parent_chain: Vec::new(),
            effects,
        }
    }

    /// Bounding box of this layer's renderable content in its own
    /// local coordinate frame (before `transform` is applied). Used
    /// by the player's off-screen cull: the caller transforms these
    /// corners through the layer's composed world affine and
    /// checks the AABB against the destination rect.
    ///
    /// Returns `None` for layers whose extent can't be computed
    /// cheaply (unknown content types, null transforms). The cull
    /// treats `None` as "don't cull" so unfamiliar content still
    /// draws — correctness over throughput.
    ///
    /// Path bounds include tangent control points so strongly-curved
    /// `sh` shapes don't false-cull when a control handle extends
    /// beyond the vertex convex hull.
    pub fn source_bounds(&self, scene_t: f32) -> Option<Rect> {
        match &self.kind {
            LayerKind::Solid { width, height, .. } => {
                Some(Rect::new(0.0, 0.0, *width, *height))
            }
            LayerKind::Shape(content) => content.local_bounds(scene_t),
            // Null renders nothing of its own; Unknown is
            // conservative (can't know the extent of a layer type
            // we don't parse). Both return None so the player
            // doesn't base a cull decision on missing data.
            LayerKind::Null | LayerKind::Unknown => None,
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
        // Opacity gate: sample the transform before any allocation
        // (effect vec, mask clip pushes) so fully-transparent layers
        // — a common "fade-out tail" idiom — skip every downstream
        // step. Opacity is the only transform component cheap
        // enough to early-check before deciding to push an
        // offscreen effects layer; position / scale / rotation
        // would still need to draw even at an extreme value.
        let xform = self.transform.sample(scene_t);
        if xform.opacity <= 0.0 {
            return;
        }

        // Effect layers (`ef`) wrap the whole draw pass — transform
        // + masks + content all land inside the offscreen layer
        // `push_layer` creates, so the blur / shadow operates on the
        // layer's composited output. `pop_layer` is matched 1:1.
        let effects_pushed = if !self.effects.is_empty() {
            let effects = self
                .effects
                .iter()
                .map(|e| e.sample(scene_t))
                .collect();
            dc.push_layer(LayerConfig {
                effects,
                ..LayerConfig::default()
            });
            true
        } else {
            false
        };

        push_layer_transform(dc, &xform);

        // Stack up `Add`-mode masks as `ClipShape::Path` pushes.
        // Sequential `push_clip`s already intersect at the
        // renderer level, which matches Lottie's "all masks
        // combine with AND-like semantics" default for Add-mode.
        // Track the push count so we pop exactly as many as went
        // in — a mid-mask bail-out would desync the clip stack.
        let mut pushed_clips = 0usize;
        for mask in &self.masks {
            // Non-Add modes aren't yet supported. Treating them as
            // Add is the forgiving fallback — assets that mix modes
            // (rare) render over-clipped rather than leaking content
            // past the author-intended bounds. Subtract / intersect
            // done properly need either inverse-clip support from
            // `DrawContext` or an offscreen composite pass; tracked
            // as a Phase 4 item in the BACKLOG.
            let _ = mask.mode;
            let shape = mask.path.sample(scene_t);
            if shape.vertices.is_empty() {
                continue;
            }
            dc.push_clip(ClipShape::Path(shape.to_path()));
            pushed_clips += 1;
        }

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
            // Null layer has no content of its own — its transform
            // is still useful because children reference it as a
            // parent in their `parent_chain`.
            LayerKind::Null | LayerKind::Unknown => {}
        }

        for _ in 0..pushed_clips {
            dc.pop_clip();
        }

        pop_layer_transform(dc);

        if effects_pushed {
            dc.pop_layer();
        }
    }
}

/// Parse one entry from a layer's `masksProperties` array into a
/// [`MaskSpec`]. Shape data is routed through the shared
/// `parse_animated_path` helper that `sh` items use.
fn parse_mask(v: &Value, fr: f32) -> MaskSpec {
    let mode = match v.get("mode").and_then(Value::as_str) {
        Some("s") => MaskMode::Subtract,
        Some("i") => MaskMode::Intersect,
        Some("l") => MaskMode::Lighten,
        Some("da") => MaskMode::Darken,
        Some("f") => MaskMode::Difference,
        // "a" or missing defaults to Add — the common case.
        _ => MaskMode::Add,
    };
    let path = v
        .get("pt")
        .map(|pt| parse_animated_path(pt, fr))
        .unwrap_or(AnimatedPath::Static(crate::shape::PathShape {
            vertices: Vec::new(),
            in_tangents: Vec::new(),
            out_tangents: Vec::new(),
            closed: false,
        }));
    let opacity = parse_animated_scalar(v.get("o"), fr)
        .unwrap_or(AnimatedF32::Static(100.0))
        .map(|o| (o / 100.0).clamp(0.0, 1.0));
    let invert = v
        .get("inv")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    MaskSpec {
        mode,
        path,
        opacity,
        invert,
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
                out_tangent: tangents_from_key_per_axis::<4>(kf, "o"),
                in_tangent: tangents_from_key_per_axis::<4>(kf, "i"),
            })
        })
        .collect()
}

/// Extract a single shared `BezierTangent` from a keyframe's `i`
/// or `o` block. Used by `ScalarKey` (which has one axis of
/// easing — itself). The multi-axis forms use
/// [`tangents_from_key_per_axis`] instead.
///
/// Lottie JSON stores each tangent as either `{ "x": <num>, "y": <num> }`
/// (shared-axis easing) or `{ "x": [<num>, ...], "y": [<num>, ...] }`
/// (per-component easing). The scalar form parses directly; the
/// array form takes the first element — that's the canonical
/// "shared curve" pick that collapses all axes onto a single ease.
pub(crate) fn tangent_from_key(kf: &Value, field: &str) -> Option<BezierTangent> {
    let obj = kf.get(field)?;
    let x = scalar_or_nth(obj.get("x")?, 0)?;
    let y = scalar_or_nth(obj.get("y")?, 0)?;
    Some(BezierTangent { x, y })
}

/// Build an N-axis tangent array from a keyframe's `i` or `o` block.
/// Each slot reads `x[idx]` / `y[idx]` when either is an array, else
/// reuses the scalar form. Missing slots fall back to the axis-0
/// tangent — matches the usual export where only one tangent is
/// authored but the value is a multi-component vector.
fn tangents_from_key_per_axis<const N: usize>(
    kf: &Value,
    field: &str,
) -> [Option<BezierTangent>; N] {
    let mut out: [Option<BezierTangent>; N] = [None; N];
    let Some(obj) = kf.get(field) else {
        return out;
    };
    let x_val = obj.get("x");
    let y_val = obj.get("y");
    let Some(x_val) = x_val else { return out };
    let Some(y_val) = y_val else { return out };
    // Axis-0 always draws from the scalar-or-first path so a
    // keyframe with only `{ "x": 0.5, "y": 0.0 }` still produces
    // a valid tangent.
    let share_x = scalar_or_nth(x_val, 0);
    let share_y = scalar_or_nth(y_val, 0);
    let share = match (share_x, share_y) {
        (Some(x), Some(y)) => Some(BezierTangent { x, y }),
        _ => None,
    };
    for (i, slot) in out.iter_mut().enumerate() {
        let xi = scalar_or_nth(x_val, i).or(share_x);
        let yi = scalar_or_nth(y_val, i).or(share_y);
        *slot = match (xi, yi) {
            (Some(x), Some(y)) => Some(BezierTangent { x, y }),
            _ => share,
        };
    }
    out
}

fn scalar_or_nth(v: &Value, idx: usize) -> Option<f32> {
    if let Some(arr) = v.as_array() {
        arr.get(idx).and_then(Value::as_f64).map(|n| n as f32)
    } else if idx == 0 {
        v.as_f64().map(|n| n as f32)
    } else {
        None
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
                out_tangent: tangents_from_key_per_axis::<2>(kf, "o"),
                in_tangent: tangents_from_key_per_axis::<2>(kf, "i"),
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

/// Push a parent's transform onto the stack without its opacity.
/// Lottie / After Effects parent transforms compose spatially
/// (position · rotation · scale · anchor) but do not propagate
/// opacity — each layer gates its own fade independently. The
/// caller matches each call with exactly one
/// [`pop_parent_transform`].
pub(crate) fn push_parent_transform(dc: &mut dyn DrawContext, xform: &SampledTransform) {
    dc.push_transform(Transform::translate(xform.position[0], xform.position[1]));
    dc.push_transform(Transform::rotate(xform.rotation));
    dc.push_transform(Transform::scale(xform.scale[0], xform.scale[1]));
    dc.push_transform(Transform::translate(-xform.anchor[0], -xform.anchor[1]));
}

pub(crate) fn pop_parent_transform(dc: &mut dyn DrawContext) {
    dc.pop_transform();
    dc.pop_transform();
    dc.pop_transform();
    dc.pop_transform();
}

/// Build the 2×3 affine that [`push_layer_transform`] would apply
/// to the stack, without touching the DrawContext. The stack pushes
/// `T(p) · R(r) · S(s) · T(-a)` in that order, so the composed
/// point-apply is `p + R · S · (x - a)`. Collapsing into the
/// `[a, b, c, d, tx, ty]` form Blinc uses gives the elements
/// below.
///
/// Used by the AABB cull to compose a layer's local-to-parent
/// affine with the parent chain's `current_transform()` so the
/// screen-space bounds can be checked before the expensive
/// `push_layer` / `push_clip` setup.
pub(crate) fn layer_local_affine(xform: &SampledTransform) -> Affine2D {
    let sx = xform.scale[0];
    let sy = xform.scale[1];
    let c = xform.rotation.cos();
    let s = xform.rotation.sin();
    let ax = xform.anchor[0];
    let ay = xform.anchor[1];
    Affine2D {
        elements: [
            c * sx,
            s * sx,
            -s * sy,
            c * sy,
            xform.position[0] - c * sx * ax + s * sy * ay,
            xform.position[1] - s * sx * ax - c * sy * ay,
        ],
    }
}

/// Multiply two affines in the `left · right` order the transform
/// stack uses: applying the composed affine to a point p produces
/// `left · (right · p)`. Mirrors the push-semantics of
/// `DrawContext::push_transform` so `multiply_affines(parent,
/// child)` matches the `current_transform` one would observe after
/// pushing child on top of parent.
pub(crate) fn multiply_affines(left: &Affine2D, right: &Affine2D) -> Affine2D {
    let [a0, a1, a2, a3, a4, a5] = left.elements;
    let [b0, b1, b2, b3, b4, b5] = right.elements;
    Affine2D {
        elements: [
            a0 * b0 + a2 * b1,
            a1 * b0 + a3 * b1,
            a0 * b2 + a2 * b3,
            a1 * b2 + a3 * b3,
            a0 * b4 + a2 * b5 + a4,
            a1 * b4 + a3 * b5 + a5,
        ],
    }
}

/// Transform the 4 corners of `source` through `affine` and return
/// the axis-aligned bounding box of the transformed points.
/// Correct under rotation + non-uniform scale — the transformed
/// shape is a parallelogram, but its surrounding AABB is exactly
/// the min/max of the 4 corner x/y values.
pub(crate) fn transform_rect_through_affine(affine: &Affine2D, source: Rect) -> Rect {
    let [a, b, c, d, tx, ty] = affine.elements;
    let apply = |x: f32, y: f32| Point::new(a * x + c * y + tx, b * x + d * y + ty);
    let x0 = source.x();
    let y0 = source.y();
    let x1 = x0 + source.width();
    let y1 = y0 + source.height();
    let corners = [apply(x0, y0), apply(x1, y0), apply(x1, y1), apply(x0, y1)];
    let (mut min_x, mut max_x) = (f32::INFINITY, f32::NEG_INFINITY);
    let (mut min_y, mut max_y) = (f32::INFINITY, f32::NEG_INFINITY);
    for p in corners {
        min_x = min_x.min(p.x);
        max_x = max_x.max(p.x);
        min_y = min_y.min(p.y);
        max_y = max_y.max(p.y);
    }
    Rect::new(min_x, min_y, (max_x - min_x).max(0.0), (max_y - min_y).max(0.0))
}

/// Transform `source` through `affine` and check whether the
/// resulting AABB intersects `dest`. Thin wrapper over
/// [`transform_rect_through_affine`] + `Rect::intersects` that
/// reads cleanly at the cull site.
pub(crate) fn transformed_aabb_intersects(
    affine: &Affine2D,
    source: Rect,
    dest: &Rect,
) -> bool {
    transform_rect_through_affine(affine, source).intersects(dest)
}

/// Populate every layer's `parent_chain` from its `parent_ind`.
/// Walks the chain one hop at a time, guarded against cycles and
/// forward references to missing `ind` values; in either case the
/// layer ends up with an empty chain so the composition still
/// renders (the bad layer just ignores its missing parent).
/// Output is outermost-ancestor-first so callers can push in order.
pub(crate) fn resolve_parent_chains(layers: &mut [Layer]) {
    use std::collections::HashMap;
    let mut ind_to_index: HashMap<i32, usize> = HashMap::new();
    for (i, layer) in layers.iter().enumerate() {
        if let Some(ind) = layer.ind {
            // Later layers with duplicate `ind` shadow earlier
            // ones — matches the common exporter behaviour and
            // keeps a single Option<usize> lookup per chain step.
            ind_to_index.insert(ind, i);
        }
    }
    for i in 0..layers.len() {
        let mut chain: Vec<usize> = Vec::new();
        let mut current = layers[i].parent_ind;
        // Cycle guard: a parent chain longer than the total layer
        // count would've revisited a node, so bail once we hit that
        // bound. Cheaper than a HashSet for the typical ~3-5 hops
        // dotLottie scenes use.
        let max_depth = layers.len();
        while let Some(p_ind) = current {
            if chain.len() >= max_depth {
                chain.clear();
                break;
            }
            let Some(&p_idx) = ind_to_index.get(&p_ind) else {
                break;
            };
            if p_idx == i || chain.contains(&p_idx) {
                // Direct self-parent or cycle back to a visited
                // ancestor — drop the chain entirely.
                chain.clear();
                break;
            }
            chain.push(p_idx);
            current = layers[p_idx].parent_ind;
        }
        chain.reverse();
        layers[i].parent_chain = chain;
    }
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
    fn bezier_easing_deflects_from_linear() {
        // After Effects' "Easy Ease" preset emits tangents around
        // (0.833, 0) on `o` and (0.167, 1) on `i` — the classic
        // S-curve that slows near both endpoints.
        let out = BezierTangent { x: 0.833, y: 0.0 };
        let in_ = BezierTangent { x: 0.167, y: 1.0 };
        // Midpoint of an ease-in-out curve crosses y ≈ 0.5 by
        // symmetry; the quarter-point should fall well below 0.25
        // (the curve is still flat near the start).
        let midpoint = solve_bezier_ease(0.5, out, in_);
        let quarter = solve_bezier_ease(0.25, out, in_);
        assert!((midpoint - 0.5).abs() < 0.01, "midpoint ≈ 0.5, got {midpoint}");
        assert!(quarter < 0.15, "quarter-point should be pulled flat, got {quarter}");
        // Endpoints must stay fixed — a wandering endpoint would
        // cause visible pose jumps at keyframe boundaries.
        assert!((solve_bezier_ease(0.0, out, in_) - 0.0).abs() < 1e-5);
        assert!((solve_bezier_ease(1.0, out, in_) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn bezier_eased_sample_deviates_from_linear() {
        // Easy Ease tangents spanning a 0→100 opacity segment. Per
        // Lottie convention `o` lives on the earlier keyframe (the
        // handle leaving it) and `i` on the later keyframe (the
        // handle arriving at it). A symmetric S-curve midpoint
        // crosses ~50%, but the quarter-point should be pulled
        // clearly below the linear 25% value because the curve is
        // still flat near the start.
        let v = json!({
            "ty": 1,
            "ip": 0, "op": 60,
            "sw": 10.0, "sh": 10.0, "sc": "#000000",
            "ks": {
                "o": {
                    "a": 1,
                    "k": [
                        { "t": 0,  "s": [0.0],   "o": { "x": 0.833, "y": 0.0 } },
                        { "t": 60, "s": [100.0], "i": { "x": 0.167, "y": 1.0 } }
                    ]
                }
            }
        });
        let layer = Layer::from_value(&v, 60.0);
        let linear_quarter = 0.25f32;
        let eased_quarter = layer.transform.sample(0.25).opacity;
        assert!(
            eased_quarter < linear_quarter - 0.05,
            "eased quarter {eased_quarter} should trail linear {linear_quarter}"
        );
    }

    #[test]
    fn parses_drop_shadow_effect() {
        // Layer with a single Drop Shadow effect — spec order is
        // [Color, Opacity (0-255), Direction (°), Distance, Softness].
        // At t=0 with direction 135° and distance 10, the shadow
        // offset is (10·sin135, -10·cos135) ≈ (7.07, 7.07).
        let v = json!({
            "ty": 1,
            "ip": 0, "op": 60,
            "sw": 100.0, "sh": 100.0, "sc": "#ffffff",
            "ef": [
                {
                    "ty": 25,
                    "nm": "Drop Shadow",
                    "ef": [
                        { "ty": 2, "nm": "Shadow Color", "v": { "k": [0.0, 0.0, 0.0, 1.0] } },
                        { "ty": 7, "nm": "Opacity", "v": { "k": 128.0 } },
                        { "ty": 0, "nm": "Direction", "v": { "k": 135.0 } },
                        { "ty": 0, "nm": "Distance", "v": { "k": 10.0 } },
                        { "ty": 0, "nm": "Softness", "v": { "k": 5.0 } }
                    ]
                }
            ]
        });
        let layer = Layer::from_value(&v, 60.0);
        assert_eq!(layer.effects.len(), 1);
        match layer.effects[0].sample(0.0) {
            LayerEffect::DropShadow { offset_x, offset_y, blur, color, .. } => {
                assert!((offset_x - 7.07).abs() < 0.05, "offset_x ≈ 7.07, got {offset_x}");
                assert!((offset_y - 7.07).abs() < 0.05, "offset_y ≈ 7.07, got {offset_y}");
                assert!((blur - 5.0).abs() < 1e-5);
                assert!((color.a - 128.0 / 255.0).abs() < 1e-3, "alpha = {}", color.a);
            }
            other => panic!("expected DropShadow, got {other:?}"),
        }
    }

    #[test]
    fn parses_gaussian_blur_effect() {
        let v = json!({
            "ty": 1,
            "ip": 0, "op": 60,
            "sw": 100.0, "sh": 100.0, "sc": "#ffffff",
            "ef": [
                {
                    "ty": 29,
                    "nm": "Gaussian Blur",
                    "ef": [
                        { "ty": 0, "nm": "Blurriness", "v": { "k": 12.0 } },
                        { "ty": 7, "nm": "Blur Dimensions", "v": { "k": 1.0 } }
                    ]
                }
            ]
        });
        let layer = Layer::from_value(&v, 60.0);
        assert_eq!(layer.effects.len(), 1);
        match layer.effects[0].sample(0.0) {
            LayerEffect::Blur { radius, .. } => {
                assert!((radius - 12.0).abs() < 1e-5);
            }
            other => panic!("expected Blur, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_effect_types_drop_silently() {
        // Effect type 20 (Tritone) isn't implemented. Loader should
        // drop it rather than erroring — matching the rest of the
        // module's lenient-parse contract.
        let v = json!({
            "ty": 1,
            "ip": 0, "op": 60,
            "sw": 10.0, "sh": 10.0, "sc": "#000000",
            "ef": [
                { "ty": 20, "nm": "Tritone", "ef": [] },
                { "ty": 29, "nm": "Gaussian Blur", "ef": [{ "ty": 0, "nm": "Blurriness", "v": { "k": 3.0 } }] }
            ]
        });
        let layer = Layer::from_value(&v, 60.0);
        assert_eq!(layer.effects.len(), 1, "unsupported effect should drop, blur should stay");
    }

    #[test]
    fn binary_search_finds_correct_segment_in_dense_timeline() {
        // 5 keyframes spanning 0→4 seconds with opacity growing
        // linearly 0→100. Binary search should find the segment
        // (2, 3) at sample time 2.5 and return 62.5 — proves
        // mid-array lookup works without false matches.
        let v = json!({
            "ty": 1,
            "ip": 0, "op": 240,
            "sw": 10.0, "sh": 10.0, "sc": "#000000",
            "ks": {
                "o": {
                    "a": 1,
                    "k": [
                        { "t": 0,   "s": [0.0]   },
                        { "t": 60,  "s": [25.0]  },
                        { "t": 120, "s": [50.0]  },
                        { "t": 180, "s": [75.0]  },
                        { "t": 240, "s": [100.0] }
                    ]
                }
            }
        });
        let layer = Layer::from_value(&v, 60.0);
        // 2.5s lies in segment (120f, 180f) = (2s, 3s), value
        // 0.5 → 0.75, linear midpoint = 0.625.
        let o = layer.transform.sample(2.5).opacity;
        assert!((o - 0.625).abs() < 1e-5, "expected 0.625, got {o}");
        // Exact-match on an internal keyframe time should produce
        // that keyframe's value, not interpolate.
        assert!((layer.transform.sample(3.0).opacity - 0.75).abs() < 1e-5);
    }

    #[test]
    fn layer_local_affine_matches_push_semantics() {
        // Identity transform: affine should be the identity matrix.
        let identity = SampledTransform {
            anchor: [0.0, 0.0],
            position: [0.0, 0.0],
            scale: [1.0, 1.0],
            rotation: 0.0,
            opacity: 1.0,
        };
        let a = layer_local_affine(&identity);
        assert!((a.elements[0] - 1.0).abs() < 1e-6);
        assert!((a.elements[3] - 1.0).abs() < 1e-6);
        assert!(a.elements[1].abs() < 1e-6);
        assert!(a.elements[2].abs() < 1e-6);
        assert!(a.elements[4].abs() < 1e-6);
        assert!(a.elements[5].abs() < 1e-6);

        // Translation: point (0, 0) in source space should land at
        // the translation values after applying the affine.
        let translated = SampledTransform {
            anchor: [0.0, 0.0],
            position: [10.0, 20.0],
            scale: [1.0, 1.0],
            rotation: 0.0,
            opacity: 1.0,
        };
        let a = layer_local_affine(&translated);
        let applied_x = a.elements[0] * 0.0 + a.elements[2] * 0.0 + a.elements[4];
        let applied_y = a.elements[1] * 0.0 + a.elements[3] * 0.0 + a.elements[5];
        assert!((applied_x - 10.0).abs() < 1e-6);
        assert!((applied_y - 20.0).abs() < 1e-6);

        // 90° rotation around origin (anchor at origin): point (1, 0)
        // should land near (0, 1).
        let rotated = SampledTransform {
            anchor: [0.0, 0.0],
            position: [0.0, 0.0],
            scale: [1.0, 1.0],
            rotation: std::f32::consts::FRAC_PI_2,
            opacity: 1.0,
        };
        let a = layer_local_affine(&rotated);
        let rx = a.elements[0] * 1.0 + a.elements[2] * 0.0 + a.elements[4];
        let ry = a.elements[1] * 1.0 + a.elements[3] * 0.0 + a.elements[5];
        assert!(rx.abs() < 1e-5, "rotated x ≈ 0, got {rx}");
        assert!((ry - 1.0).abs() < 1e-5, "rotated y ≈ 1, got {ry}");
    }

    #[test]
    fn multiply_affines_composes_like_stack() {
        // Translate(10) then translate(5): composed is translate(15).
        let a = Affine2D {
            elements: [1.0, 0.0, 0.0, 1.0, 10.0, 0.0],
        };
        let b = Affine2D {
            elements: [1.0, 0.0, 0.0, 1.0, 5.0, 0.0],
        };
        let c = multiply_affines(&a, &b);
        // Apply to (0, 0): should land at 15 — translate-order is
        // parent · child, so result = parent(child(p)).
        let rx = c.elements[0] * 0.0 + c.elements[2] * 0.0 + c.elements[4];
        assert!((rx - 15.0).abs() < 1e-6, "expected 15, got {rx}");
    }

    #[test]
    fn transformed_aabb_rejects_offscreen_rect() {
        // A 10×10 source rect translated 1000px to the right. The
        // composed AABB sits at [1000, 1010] on the x axis and
        // doesn't intersect a destination rect at [0, 100].
        let offscreen = Affine2D {
            elements: [1.0, 0.0, 0.0, 1.0, 1000.0, 0.0],
        };
        let source = Rect::new(0.0, 0.0, 10.0, 10.0);
        let dest = Rect::new(0.0, 0.0, 100.0, 100.0);
        assert!(!transformed_aabb_intersects(&offscreen, source, &dest));

        // Same source translated just inside: (95, 0) with size
        // (10, 10) crosses the dest's right edge — visible.
        let partially = Affine2D {
            elements: [1.0, 0.0, 0.0, 1.0, 95.0, 0.0],
        };
        assert!(transformed_aabb_intersects(&partially, source, &dest));
    }

    #[test]
    fn transformed_aabb_expands_under_rotation() {
        // 10×10 rect rotated 45° has an AABB roughly 14.14 wide
        // because corners stick out. Check that a destination just
        // outside the axis-aligned bound but inside the rotated
        // bound still reports visible.
        let rotation = std::f32::consts::FRAC_PI_4;
        let c = rotation.cos();
        let s = rotation.sin();
        let rotate_45 = Affine2D {
            elements: [c, s, -s, c, 0.0, 0.0],
        };
        let source = Rect::new(-5.0, -5.0, 10.0, 10.0);
        // Destination at (6, 0, 1, 1) is past the un-rotated 10×10
        // but inside the rotated AABB (which reaches ~7.07 from
        // origin). Should be visible.
        let dest = Rect::new(6.0, 0.0, 1.0, 1.0);
        assert!(transformed_aabb_intersects(&rotate_45, source, &dest));
    }

    #[test]
    fn source_bounds_for_solid_is_exact() {
        let v = json!({
            "ty": 1,
            "ip": 0, "op": 60,
            "sw": 120.0, "sh": 80.0, "sc": "#aabbcc"
        });
        let layer = Layer::from_value(&v, 60.0);
        let b = layer.source_bounds(0.0).expect("solid has bounds");
        assert_eq!(b.x(), 0.0);
        assert_eq!(b.y(), 0.0);
        assert_eq!(b.width(), 120.0);
        assert_eq!(b.height(), 80.0);
    }

    #[test]
    fn source_bounds_for_null_and_unknown_is_none() {
        let null = json!({ "ty": 3, "ip": 0, "op": 60 });
        let unknown = json!({ "ty": 99, "ip": 0, "op": 60 });
        assert!(Layer::from_value(&null, 60.0).source_bounds(0.0).is_none());
        assert!(Layer::from_value(&unknown, 60.0).source_bounds(0.0).is_none());
    }

    #[test]
    fn null_layer_parses_as_null_kind_and_still_chains_parenting() {
        // A null (`ty: 3`) has no content but contributes its
        // transform to children that parent to it. `ind` still
        // registers in the parent table.
        let null = json!({ "ty": 3, "ind": 1, "ip": 0, "op": 60 });
        let child = json!({ "ty": 99, "ind": 2, "parent": 1, "ip": 0, "op": 60 });
        let mut layers = vec![
            Layer::from_value(&null, 60.0),
            Layer::from_value(&child, 60.0),
        ];
        assert!(matches!(layers[0].kind, LayerKind::Null));
        resolve_parent_chains(&mut layers);
        assert_eq!(layers[1].parent_chain, vec![0]);
    }

    #[test]
    fn parent_chain_resolves_in_outermost_first_order() {
        // Three-layer chain: A is the root, B is parented to A,
        // C is parented to B. Render order should push A's
        // transform before B's before rendering C.
        let a = json!({ "ty": 99, "ind": 1, "ip": 0, "op": 60 });
        let b = json!({ "ty": 99, "ind": 2, "parent": 1, "ip": 0, "op": 60 });
        let c = json!({ "ty": 99, "ind": 3, "parent": 2, "ip": 0, "op": 60 });
        let mut layers = vec![
            Layer::from_value(&a, 60.0),
            Layer::from_value(&b, 60.0),
            Layer::from_value(&c, 60.0),
        ];
        resolve_parent_chains(&mut layers);
        assert!(layers[0].parent_chain.is_empty());
        assert_eq!(layers[1].parent_chain, vec![0]);
        // C's chain: outermost (A) first, direct parent (B) last.
        assert_eq!(layers[2].parent_chain, vec![0, 1]);
    }

    #[test]
    fn parent_chain_handles_forward_refs_and_cycles() {
        // Forward reference (parent's `ind` comes later in the
        // array) resolves fine because `ind_to_index` is built
        // before walking.
        let a = json!({ "ty": 99, "ind": 1, "parent": 2, "ip": 0, "op": 60 });
        let b = json!({ "ty": 99, "ind": 2, "ip": 0, "op": 60 });
        let mut layers = vec![
            Layer::from_value(&a, 60.0),
            Layer::from_value(&b, 60.0),
        ];
        resolve_parent_chains(&mut layers);
        assert_eq!(layers[0].parent_chain, vec![1]);
        assert!(layers[1].parent_chain.is_empty());

        // Cyclic parents drop the chain rather than looping.
        let a = json!({ "ty": 99, "ind": 1, "parent": 2, "ip": 0, "op": 60 });
        let b = json!({ "ty": 99, "ind": 2, "parent": 1, "ip": 0, "op": 60 });
        let mut layers = vec![
            Layer::from_value(&a, 60.0),
            Layer::from_value(&b, 60.0),
        ];
        resolve_parent_chains(&mut layers);
        assert!(layers[0].parent_chain.is_empty(), "cycle should drop chain");
        assert!(layers[1].parent_chain.is_empty(), "cycle should drop chain");

        // Missing parent (dangling `ind`) drops silently.
        let c = json!({ "ty": 99, "ind": 1, "parent": 42, "ip": 0, "op": 60 });
        let mut layers = vec![Layer::from_value(&c, 60.0)];
        resolve_parent_chains(&mut layers);
        assert!(layers[0].parent_chain.is_empty());
    }

    #[test]
    fn per_axis_bezier_eases_x_and_y_independently() {
        // Vec2 keyframe pair going (0,0) → (100, 100). X axis uses
        // an ease-in tangent (slow start), Y stays linear. At the
        // quarter point, X should trail 25 but Y should land near
        // 25 — proves per-axis tangents aren't being collapsed
        // onto a shared curve.
        let v = json!({
            "ty": 1,
            "ip": 0, "op": 60,
            "sw": 10.0, "sh": 10.0, "sc": "#000000",
            "ks": {
                "p": {
                    "a": 1,
                    "k": [
                        {
                            "t": 0,
                            "s": [0.0, 0.0, 0.0],
                            "o": { "x": [0.833, 0.0], "y": [0.0, 0.0] }
                        },
                        {
                            "t": 60,
                            "s": [100.0, 100.0, 0.0],
                            "i": { "x": [1.0, 1.0], "y": [1.0, 1.0] }
                        }
                    ]
                }
            }
        });
        let layer = Layer::from_value(&v, 60.0);
        let sampled = layer.transform.sample(0.25);
        // X axis: slow start, quarter point trails linear 25.
        assert!(
            sampled.position[0] < 20.0,
            "eased X should lag linear at quarter, got {}",
            sampled.position[0]
        );
        // Y axis: linear tangent, quarter point ≈ 25.
        assert!(
            (sampled.position[1] - 25.0).abs() < 2.0,
            "linear Y should be ≈ 25 at quarter, got {}",
            sampled.position[1]
        );
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

    #[test]
    fn parses_add_mask_with_static_path() {
        let v = json!({
            "ty": 4,
            "ip": 0,
            "op": 60,
            "ks": {},
            "shapes": [],
            "masksProperties": [
                {
                    "mode": "a",
                    "pt": {
                        "a": 0,
                        "k": {
                            "v": [[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]],
                            "i": [[0.0, 0.0], [0.0, 0.0], [0.0, 0.0], [0.0, 0.0]],
                            "o": [[0.0, 0.0], [0.0, 0.0], [0.0, 0.0], [0.0, 0.0]],
                            "c": true
                        }
                    },
                    "o": { "a": 0, "k": 100 },
                    "inv": false
                }
            ]
        });
        let layer = Layer::from_value(&v, 60.0);
        assert_eq!(layer.masks.len(), 1);
        let mask = &layer.masks[0];
        assert_eq!(mask.mode, MaskMode::Add);
        assert!(!mask.invert);
        let shape = mask.path.sample(0.0);
        assert_eq!(shape.vertices.len(), 4);
        assert!(shape.closed);
    }

    #[test]
    fn parses_mask_modes_from_single_letter_encoding() {
        let make = |mode: &str| {
            json!({
                "ty": 4, "ip": 0, "op": 60, "ks": {}, "shapes": [],
                "masksProperties": [{
                    "mode": mode,
                    "pt": { "a": 0, "k": { "v": [], "i": [], "o": [], "c": false } },
                    "o": { "a": 0, "k": 100 }
                }]
            })
        };
        for (tag, expected) in [
            ("a", MaskMode::Add),
            ("s", MaskMode::Subtract),
            ("i", MaskMode::Intersect),
            ("l", MaskMode::Lighten),
            ("da", MaskMode::Darken),
            ("f", MaskMode::Difference),
        ] {
            let layer = Layer::from_value(&make(tag), 60.0);
            assert_eq!(layer.masks[0].mode, expected, "mode {tag}");
        }
    }

    #[test]
    fn layer_without_masks_property_has_empty_masks_vec() {
        let v = json!({
            "ty": 4, "ip": 0, "op": 60, "ks": {}, "shapes": []
        });
        let layer = Layer::from_value(&v, 60.0);
        assert!(layer.masks.is_empty());
    }
}
