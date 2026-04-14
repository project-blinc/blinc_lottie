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
//! | `fl`        | Fill        | Solid color + opacity                       |
//! | `st`        | Stroke      | Solid color + opacity + width               |
//!
//! Items not in the table parse as no-ops. Most notably **path (`sh`)**,
//! **polystar (`sr`)**, **trim path (`tm`)**, and **gradient
//! fill/stroke (`gf` / `gs`)** are deferred — see Phase 2 / 3 in the
//! crate README.
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
    parse_animated_scalar, parse_animated_vec2, parse_animated_vec4, pop_layer_transform,
    push_layer_transform, AnimatedF32, AnimatedVec2, AnimatedVec4, TransformSpec,
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
        }
    }
}

/// Build a closed cubic-bezier approximation of an axis-aligned ellipse.
///
/// Uses the standard 4-arc construction with `k = (4/3)·tan(π/8)` for
/// the control-point distance.
fn ellipse_path(cx: f32, cy: f32, rx: f32, ry: f32) -> Path {
    const K: f32 = 0.552_284_75;
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
                    { "ty": "sh" }, // path — not yet implemented
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
}
