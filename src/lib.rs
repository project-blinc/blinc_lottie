//! Lottie animation player for Blinc sketches.
//!
//! Parses Lottie JSON and implements [`blinc_canvas_kit::Player`] so the
//! parsed scene can be rendered into any [`blinc_canvas_kit::Sketch`] via
//! `ctx.play(&mut lottie, rect, t)`.
//!
//! # Status
//!
//! Skeleton. The parser loads top-level metadata (version, frame rate,
//! in/out points, canvas dimensions, markers) and stashes layers as
//! opaque JSON values. `Player::draw_at` renders a placeholder rect
//! annotated with metadata — full shape-layer interpolation lands in a
//! follow-up.
//!
//! # Non-goals (for now)
//!
//! - Expression layers (`ix`/`p.k` with JS expressions) are unlikely to
//!   be supported — porting the expression runtime doubles the footprint
//!   of the library and isn't needed for most motion-design exports.
//! - Image asset layers need a `blinc_image` bridge; tracked separately.

use blinc_canvas_kit::{Player, SketchContext};
use blinc_core::draw::Transform;
use blinc_core::layer::Rect;
use blinc_core::DrawContext;

mod layer;
mod parser;
mod shape;

use layer::Layer;

/// Callback fired when playback crosses a marker's timestamp.
type MarkerCallback = Box<dyn FnMut(&Marker) + Send + 'static>;

/// Errors surfaced from [`LottiePlayer`] construction.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("lottie json parse failed: {0}")]
    Json(#[from] serde_json::Error),
}

/// A named point (or span) on a Lottie scene's timeline.
///
/// Lottie exports frame-based markers; `LottiePlayer` converts those to
/// seconds at load time so downstream consumers never need the scene's
/// frame rate.
#[derive(Debug, Clone)]
pub struct Marker {
    /// Name as exported from the source tool (empty string if the export
    /// omitted the comment field).
    pub name: String,
    /// Marker start time in seconds from the composition's in-point.
    pub time_seconds: f32,
    /// Marker duration in seconds. `0.0` for a point-in-time marker.
    pub duration_seconds: f32,
}

/// A loaded Lottie scene, ready to be rendered into a `Sketch`.
pub struct LottiePlayer {
    root: parser::LottieRoot,
    /// Layers parsed from `root.layers` in source order. Render uses
    /// reverse iteration so the first array entry composites on top.
    layers: Vec<Layer>,
    markers: Vec<Marker>,
    is_playing: bool,
    /// Internal seek position in seconds. Added to the `t` passed to
    /// `draw_at` to support pause / resume without losing visual state.
    seek_offset: f32,
    /// If paused, freeze at this scene time (seconds).
    paused_at: Option<f32>,
    /// Last scene time that `draw_at` resolved. Used to (a) freeze the
    /// pose at the correct time when `set_playing(false)` is called and
    /// (b) detect interval crossings for marker emission.
    last_scene_t: f32,
    /// Optional listener invoked once per marker boundary crossed
    /// between consecutive `draw_at` calls.
    marker_callback: Option<MarkerCallback>,
}

impl LottiePlayer {
    /// Parse a Lottie scene from a JSON string.
    pub fn from_json(src: &str) -> Result<Self, Error> {
        let root: parser::LottieRoot = serde_json::from_str(src)?;
        Ok(Self::from_root(root))
    }

    /// Parse a Lottie scene from raw JSON bytes.
    pub fn from_bytes(src: &[u8]) -> Result<Self, Error> {
        let root: parser::LottieRoot = serde_json::from_slice(src)?;
        Ok(Self::from_root(root))
    }

    fn from_root(root: parser::LottieRoot) -> Self {
        let fr = root.frame_rate.max(1.0);
        let markers = root
            .markers
            .iter()
            .map(|m| Marker {
                name: m.name.clone().unwrap_or_default(),
                time_seconds: m.time_frames / fr,
                duration_seconds: m.duration_frames / fr,
            })
            .collect();
        let layers = root
            .layers
            .iter()
            .map(|v| Layer::from_value(v, fr))
            .collect();
        Self {
            root,
            layers,
            markers,
            is_playing: true,
            seek_offset: 0.0,
            paused_at: None,
            last_scene_t: 0.0,
            marker_callback: None,
        }
    }

    /// Scene's intrinsic width in pixels (from the Lottie header).
    pub fn source_width(&self) -> u32 {
        self.root.width
    }

    /// Scene's intrinsic height in pixels (from the Lottie header).
    pub fn source_height(&self) -> u32 {
        self.root.height
    }

    /// Number of layers in the scene. Useful for smoke-testing that a
    /// load succeeded; real layer rendering is not yet wired.
    pub fn layer_count(&self) -> usize {
        self.root.layers.len()
    }

    /// All markers parsed from the Lottie file, in source order.
    ///
    /// Times are in seconds; see [`Marker`].
    pub fn markers(&self) -> &[Marker] {
        &self.markers
    }

    /// Register a callback fired once per marker boundary crossed between
    /// consecutive [`Player::draw_at`] calls. Overwrites any previous
    /// callback.
    ///
    /// Markers fire on the half-open interval `(prev_scene_t, current_scene_t]`.
    /// When playback loops (current wraps below prev), the fire check
    /// covers `(prev, duration)` ∪ `[0, current]` so nothing is missed.
    pub fn on_marker<F>(&mut self, callback: F)
    where
        F: FnMut(&Marker) + Send + 'static,
    {
        self.marker_callback = Some(Box::new(callback));
    }

    /// Clear the marker callback, if any.
    pub fn clear_on_marker(&mut self) {
        self.marker_callback = None;
    }

    /// Resolve the effective scene time for a given sketch time `t`:
    /// if paused, returns the frozen pose time; otherwise returns
    /// `t - seek_offset` wrapped into `[0, duration)`.
    fn scene_time(&self, sketch_t: f32) -> f32 {
        if let Some(frozen) = self.paused_at {
            return frozen;
        }
        let dur = self.duration().unwrap_or(f32::INFINITY);
        let raw = sketch_t - self.seek_offset;
        if dur.is_finite() && dur > 0.0 {
            raw.rem_euclid(dur)
        } else {
            raw.max(0.0)
        }
    }

    /// Fire the marker callback for every marker whose timestamp falls in
    /// `(prev, current]`, accounting for playback loop wrap.
    fn fire_markers(&mut self, prev: f32, current: f32) {
        // Disjoint borrow: iterate markers immutably, mutate callback.
        // Rust's split-borrow rules permit this because `markers` and
        // `marker_callback` are distinct fields.
        let Some(cb) = self.marker_callback.as_mut() else {
            return;
        };
        let wrapped = prev > current;
        for m in &self.markers {
            let t = m.time_seconds;
            let fires = if wrapped {
                t > prev || t <= current
            } else {
                t > prev && t <= current
            };
            if fires {
                cb(m);
            }
        }
    }
}

impl Player for LottiePlayer {
    fn duration(&self) -> Option<f32> {
        let frames = (self.root.out_point - self.root.in_point).max(0.0);
        if self.root.frame_rate > 0.0 {
            Some(frames / self.root.frame_rate)
        } else {
            None
        }
    }

    fn draw_at(&mut self, ctx: &mut SketchContext<'_>, rect: Rect, t: f32) {
        let scene_t = self.scene_time(t);

        // Emit marker events for anything we crossed since the last frame.
        // Runs even while paused? No — paused scene_t equals last_scene_t
        // so no marker can be in the (prev, current] interval. Safe.
        let prev = self.last_scene_t;
        self.fire_markers(prev, scene_t);
        self.last_scene_t = scene_t;

        // Map Lottie source-space coordinates onto the destination rect.
        // Stretches the composition to fill — aspect-fit / aspect-fill
        // modes are intentionally deferred (track via README Phase 5
        // perf/format work).
        let src_w = self.root.width.max(1) as f32;
        let src_h = self.root.height.max(1) as f32;
        let sx = rect.width() / src_w;
        let sy = rect.height() / src_h;

        let dc: &mut dyn DrawContext = ctx.draw_context();
        dc.push_transform(Transform::translate(rect.x(), rect.y()));
        dc.push_transform(Transform::scale(sx, sy));

        // Lottie convention: layers earlier in the array composite on
        // top of layers later in the array. Iterate in reverse so we
        // draw back-to-front.
        for layer in self.layers.iter().rev() {
            layer.render(dc, scene_t);
        }

        dc.pop_transform();
        dc.pop_transform();
    }

    fn seek(&mut self, t: f32) {
        // Interpret `t` as an absolute scene time and offset-compensate
        // so the *next* draw_at with a given sketch time resolves to
        // that scene time.
        self.seek_offset = -t;
        self.last_scene_t = t;
        if self.paused_at.is_some() {
            self.paused_at = Some(t);
        }
    }

    fn set_playing(&mut self, playing: bool) {
        if playing == self.is_playing {
            return;
        }
        self.is_playing = playing;
        // Freeze at the last actually-rendered scene time so the pose
        // snaps to exactly what the user last saw on screen.
        self.paused_at = if playing {
            None
        } else {
            Some(self.last_scene_t)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Minimal Lottie JSON with three markers at 0.5s, 1.5s, 2.5s
    /// (assuming 60 fps, duration = 3s at op=180).
    const FIXTURE: &str = r#"{
        "v": "5.7.0",
        "fr": 60,
        "ip": 0,
        "op": 180,
        "w": 512,
        "h": 512,
        "layers": [],
        "markers": [
            { "cm": "a", "tm": 30,  "dr": 0 },
            { "cm": "b", "tm": 90,  "dr": 0 },
            { "cm": "c", "tm": 150, "dr": 0 }
        ]
    }"#;

    fn player_with_recorder() -> (LottiePlayer, Arc<Mutex<Vec<String>>>) {
        let mut p = LottiePlayer::from_json(FIXTURE).expect("parse");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_cb = seen.clone();
        p.on_marker(move |m| {
            seen_cb.lock().unwrap().push(m.name.clone());
        });
        (p, seen)
    }

    #[test]
    fn parses_markers_into_seconds() {
        let p = LottiePlayer::from_json(FIXTURE).unwrap();
        let ms = p.markers();
        assert_eq!(ms.len(), 3);
        assert_eq!(ms[0].name, "a");
        assert!((ms[0].time_seconds - 0.5).abs() < 1e-6);
        assert!((ms[1].time_seconds - 1.5).abs() < 1e-6);
        assert!((ms[2].time_seconds - 2.5).abs() < 1e-6);
        assert_eq!(ms[0].duration_seconds, 0.0);
    }

    #[test]
    fn duration_derived_from_header() {
        let p = LottiePlayer::from_json(FIXTURE).unwrap();
        assert!((p.duration().unwrap() - 3.0).abs() < 1e-6);
    }

    #[test]
    fn fires_markers_in_non_wrapping_interval() {
        let (mut p, seen) = player_with_recorder();
        // Cross 0.5s and 1.5s — "a" and "b" fire; "c" (at 2.5) is past.
        p.fire_markers(0.0, 2.0);
        assert_eq!(*seen.lock().unwrap(), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn exclusive_start_inclusive_end() {
        let (mut p, seen) = player_with_recorder();
        // Interval (0.5, 1.5] includes b's exact time, excludes a's.
        p.fire_markers(0.5, 1.5);
        assert_eq!(*seen.lock().unwrap(), vec!["b".to_string()]);
    }

    #[test]
    fn fires_across_loop_wrap() {
        let (mut p, seen) = player_with_recorder();
        // Playback wrapped: prev = 2.4s (just before c), current = 0.6s
        // (just past a). Traversal crossed c (at 2.5) then looped and
        // crossed a (at 0.5). b (at 1.5) was not crossed.
        p.fire_markers(2.4, 0.6);
        let got = seen.lock().unwrap().clone();
        assert!(got.contains(&"c".to_string()), "expected c in {got:?}");
        assert!(got.contains(&"a".to_string()), "expected a in {got:?}");
        assert!(!got.contains(&"b".to_string()), "b should not fire, got {got:?}");
    }

    #[test]
    fn pause_freezes_at_last_scene_t() {
        let (mut p, _seen) = player_with_recorder();
        p.last_scene_t = 1.2; // simulate a rendered frame
        p.set_playing(false);
        assert_eq!(p.paused_at, Some(1.2));
        // Subsequent scene_time should return the frozen value regardless
        // of the incoming sketch `t`.
        assert!((p.scene_time(99.0) - 1.2).abs() < 1e-6);
    }
}
