//! Lottie layer types and rendering.
//!
//! `Layer` is the parsed, render-ready form of a single entry in the
//! Lottie composition's layer stack. The JSON parser hands us each layer
//! as an opaque `serde_json::Value`; [`Layer::from_value`] dispatches on
//! the `ty` field and produces a typed `Layer`.
//!
//! Phase 1.1 implements **solid layers** (`ty: 1`) only. All transform
//! values are read as static (the first keyframe's value if animated),
//! pending Phase 1.2 keyframe interpolation. Other layer types parse as
//! [`LayerKind::Unknown`] and render as no-ops.

use blinc_core::draw::Transform;
use blinc_core::layer::{Brush, Color, CornerRadius, Rect};
use blinc_core::DrawContext;
use serde_json::Value;

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
    /// Layer types not yet implemented (shape, image, text, precomp,
    /// null, …). Render as a no-op so the rest of the scene still
    /// composites correctly.
    Unknown,
}

/// Lottie transform — positions a layer within its parent composition.
///
/// All fields are stored in render-friendly units:
/// - `anchor` / `position` in source-space pixels
/// - `scale` as a multiplier (`1.0` = no scale; Lottie source uses 0–100)
/// - `rotation` in radians (Lottie source uses degrees)
/// - `opacity` in `[0.0, 1.0]` (Lottie source uses 0–100)
#[derive(Debug, Clone, Copy)]
pub(crate) struct TransformSpec {
    pub anchor: [f32; 2],
    pub position: [f32; 2],
    pub scale: [f32; 2],
    pub rotation: f32,
    pub opacity: f32,
}

impl TransformSpec {
    pub fn identity() -> Self {
        Self {
            anchor: [0.0, 0.0],
            position: [0.0, 0.0],
            scale: [1.0, 1.0],
            rotation: 0.0,
            opacity: 1.0,
        }
    }

    /// Sample the transform at scene time `t`. Phase 1.1 returns the
    /// static value regardless of `t`; Phase 1.2 will interpolate
    /// keyframes.
    pub fn sample(&self, _t: f32) -> Self {
        *self
    }

    fn from_value(v: Option<&Value>) -> Self {
        let Some(v) = v else { return Self::identity() };
        Self {
            anchor: parse_2d_static(v.get("a")).unwrap_or([0.0, 0.0]),
            position: parse_2d_static(v.get("p")).unwrap_or([0.0, 0.0]),
            scale: parse_2d_static(v.get("s"))
                .map(|s| [s[0] / 100.0, s[1] / 100.0])
                .unwrap_or([1.0, 1.0]),
            rotation: parse_scalar_static(v.get("r"))
                .map(f32::to_radians)
                .unwrap_or(0.0),
            opacity: parse_scalar_static(v.get("o"))
                .map(|o| (o / 100.0).clamp(0.0, 1.0))
                .unwrap_or(1.0),
        }
    }
}

impl Layer {
    /// Build a typed layer from a raw JSON object. `frame_rate` is used
    /// to convert the Lottie frame-based `ip`/`op` fields into seconds.
    pub fn from_value(v: &Value, frame_rate: f32) -> Self {
        let fr = frame_rate.max(1.0);
        let ty = v.get("ty").and_then(Value::as_u64).unwrap_or(99);
        let in_frames = v.get("ip").and_then(Value::as_f64).unwrap_or(0.0) as f32;
        let out_frames = v.get("op").and_then(Value::as_f64).unwrap_or(0.0) as f32;
        let transform = TransformSpec::from_value(v.get("ks"));
        let kind = match ty {
            1 => parse_solid(v),
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

/// Read the static value of a Lottie animatable 2D property
/// (`{ "k": [x, y, z] }`). Returns `None` if the property is missing or
/// in a shape we don't yet handle (e.g. keyframed). Phase 1.2 will
/// extend this to sample the first keyframe instead of giving up.
fn parse_2d_static(v: Option<&Value>) -> Option<[f32; 2]> {
    let arr = v?.get("k")?.as_array()?;
    let x = arr.first().and_then(Value::as_f64)? as f32;
    let y = arr.get(1).and_then(Value::as_f64)? as f32;
    Some([x, y])
}

/// Read the static scalar of a Lottie animatable property
/// (`{ "k": value }`). Returns `None` if missing or keyframed.
fn parse_scalar_static(v: Option<&Value>) -> Option<f32> {
    v?.get("k")?.as_f64().map(|n| n as f32)
}

fn push_layer_transform(dc: &mut dyn DrawContext, xform: &TransformSpec) {
    // Compose: T(p) · R(r) · S(s) · T(-a)
    // Pop order is the reverse, handled by `pop_layer_transform`.
    dc.push_transform(Transform::translate(xform.position[0], xform.position[1]));
    dc.push_transform(Transform::rotate(xform.rotation));
    dc.push_transform(Transform::scale(xform.scale[0], xform.scale[1]));
    dc.push_transform(Transform::translate(-xform.anchor[0], -xform.anchor[1]));
    dc.push_opacity(xform.opacity);
}

fn pop_layer_transform(dc: &mut dyn DrawContext) {
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
        // #f80 expands to #ff8800 → rgba(1, 0.533, 0, 1)
        let c = parse_hex_color("#f80").unwrap();
        assert!((c.r - 1.0).abs() < 1e-6);
        assert!((c.g - (136.0 / 255.0)).abs() < 1e-6);
        assert!((c.b - 0.0).abs() < 1e-6);
    }

    #[test]
    fn rejects_bad_hex() {
        assert!(parse_hex_color("ff8000").is_none(), "missing #");
        assert!(parse_hex_color("#xyz").is_none(), "non-hex");
        assert!(parse_hex_color("#ff80").is_none(), "wrong length");
    }

    #[test]
    fn parses_solid_layer() {
        let v = json!({
            "ty": 1,
            "ip": 0,
            "op": 60,
            "sw": 100.0,
            "sh": 200.0,
            "sc": "#80c0ff",
            "ks": {
                "p": { "k": [50.0, 75.0, 0.0] },
                "a": { "k": [10.0, 20.0, 0.0] },
                "s": { "k": [200.0, 200.0, 100.0] },
                "r": { "k": 90.0 },
                "o": { "k": 50.0 }
            }
        });
        let layer = Layer::from_value(&v, 60.0);
        assert!((layer.in_seconds - 0.0).abs() < 1e-6);
        assert!((layer.out_seconds - 1.0).abs() < 1e-6);
        match layer.kind {
            LayerKind::Solid { width, height, .. } => {
                assert_eq!(width, 100.0);
                assert_eq!(height, 200.0);
            }
            _ => panic!("expected Solid"),
        }
        assert_eq!(layer.transform.position, [50.0, 75.0]);
        assert_eq!(layer.transform.anchor, [10.0, 20.0]);
        assert_eq!(layer.transform.scale, [2.0, 2.0]);
        assert!((layer.transform.rotation - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
        assert!((layer.transform.opacity - 0.5).abs() < 1e-6);
    }

    #[test]
    fn unknown_type_falls_through() {
        let v = json!({ "ty": 4, "ip": 0, "op": 60 });
        let layer = Layer::from_value(&v, 60.0);
        assert!(matches!(layer.kind, LayerKind::Unknown));
    }
}
