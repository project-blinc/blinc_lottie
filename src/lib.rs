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

#[cfg(feature = "dotlottie")]
mod dotlottie;
mod layer;
mod parser;
mod shape;
#[cfg(feature = "dotlottie")]
pub mod state_machine;

use layer::Layer;

/// Callback fired when playback crosses a marker's timestamp.
type MarkerCallback = Box<dyn FnMut(&Marker) + Send + 'static>;

/// Errors surfaced from [`LottiePlayer`] construction.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("lottie json parse failed: {0}")]
    Json(#[from] serde_json::Error),
    /// Archive read error (malformed .lottie zip or missing
    /// required member). Only constructible when the `dotlottie`
    /// feature is enabled.
    #[cfg(feature = "dotlottie")]
    #[error("dotlottie archive: {0}")]
    Archive(String),
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
    /// Optional playback segment `(start, end)` in seconds.
    ///
    /// When set, [`Self::scene_time`] wraps the sketch clock into
    /// `[start, end)` instead of the composition's full duration.
    /// State-machine transitions flip this field to scope playback
    /// to the state's segment; raw callers can use [`Self::play_segment`]
    /// / [`Self::clear_segment`] directly for one-shot trim behaviour
    /// without pulling in the state-machine wrapper.
    segment: Option<(f32, f32)>,
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

    /// Parse a `.lottie` (dotLottie 2.0) archive into a player.
    ///
    /// The archive layout follows the [spec](https://dotlottie.io/spec/2.0/):
    /// `manifest.json` at the root enumerates animations (under `a/`)
    /// and optional state machines (under `s/`). This loader resolves
    /// the manifest's `initial.animation` (falling back to the first
    /// declared animation) and parses that into a player.
    ///
    /// Callers that also want the state machine should use
    /// [`crate::state_machine::LottieStateMachine::from_dotlottie_bytes`]
    /// instead — it returns the player alongside the FSM.
    ///
    /// Image / font / theme assets (`i/`, `f/`, `t/`) are parsed into
    /// the archive but not yet surfaced. Scenes whose raster layers
    /// reference `i/` will render vector content correctly and skip
    /// the raster layers until the Phase 4 image-layer work lands.
    #[cfg(feature = "dotlottie")]
    pub fn from_dotlottie_bytes(src: &[u8]) -> Result<Self, Error> {
        let archive = crate::dotlottie::extract(src)?;
        let animation_bytes = archive
            .initial_animation()
            .ok_or_else(|| Error::Archive("archive declares no animations".to_string()))?;
        Self::from_bytes(animation_bytes)
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
        // Build the precomp asset lookup — every entry in `assets`
        // that carries a `layers` array is a precomposition
        // referenceable by `ty: 0` layers via `refId`. Image-asset
        // entries (`p` / `u`) parse into the same array but don't
        // match here because they lack `layers`.
        let mut precomp_layers: std::collections::HashMap<String, &[serde_json::Value]> =
            std::collections::HashMap::new();
        for asset in &root.assets {
            let Some(id) = asset.get("id").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let Some(layers_arr) = asset.get("layers").and_then(serde_json::Value::as_array)
            else {
                continue;
            };
            precomp_layers.insert(id.to_string(), layers_arr.as_slice());
        }
        let asset_ctx = layer::AssetContext {
            precomp_layers,
            depth: 0,
        };
        let mut layers: Vec<Layer> = root
            .layers
            .iter()
            .map(|v| Layer::from_value_with_assets(v, fr, Some(&asset_ctx)))
            .collect();
        // Every layer's `parent_chain` is derived from the final
        // `Vec<Layer>`, so resolution has to run after all
        // entries are constructed. Keep this next to `from_value`
        // calls so a future contributor can't forget to re-run it
        // after parse-time mutations.
        layer::resolve_parent_chains(&mut layers);
        Self {
            root,
            layers,
            markers,
            is_playing: true,
            seek_offset: 0.0,
            paused_at: None,
            last_scene_t: 0.0,
            marker_callback: None,
            segment: None,
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

    /// Composition frame rate (from the Lottie `fr` field). Used by
    /// [`crate::state_machine::LottieStateMachine`] to convert
    /// frame-based segments from dotLottie state-machine JSON into
    /// the seconds-based timeline the player consumes.
    pub fn frame_rate(&self) -> f32 {
        self.root.frame_rate.max(1.0)
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
    /// `t - seek_offset` wrapped into `[0, duration)`, or into
    /// `[segment.0, segment.1)` when a segment is active.
    fn scene_time(&self, sketch_t: f32) -> f32 {
        if let Some(frozen) = self.paused_at {
            return frozen;
        }
        let raw = sketch_t - self.seek_offset;
        // Segment mode: wrap into `[start, end)` around the segment's
        // own length. `raw - start` lands in `[0, length)`, then `+
        // start` shifts back into the absolute timeline. Callers who
        // want segment-plays-once (no loop) should issue a `seek +
        // set_playing(false)` at the segment's end via the marker
        // hook or the state-machine wrapper below.
        if let Some((start, end)) = self.segment {
            let length = (end - start).max(f32::EPSILON);
            return start + (raw - start).rem_euclid(length);
        }
        let dur = self.duration().unwrap_or(f32::INFINITY);
        if dur.is_finite() && dur > 0.0 {
            raw.rem_euclid(dur)
        } else {
            raw.max(0.0)
        }
    }

    /// Constrain playback to the `[start, end)` segment in seconds.
    /// The sketch clock wraps within the segment's length instead
    /// of the full composition duration, so a 10-second Lottie
    /// with `play_segment(2.0, 5.0)` loops the same 3-second arc
    /// every 3 seconds.
    ///
    /// `start` is clamped to `[0, duration]` and `end` to
    /// `[start, duration]` so malformed segment data from a
    /// state-machine JSON can't produce a negative-length range
    /// that `rem_euclid` would panic on.
    pub fn play_segment(&mut self, start: f32, end: f32) {
        let dur = self.duration().unwrap_or(f32::INFINITY);
        let s = start.clamp(0.0, dur.min(f32::MAX));
        let e = end.clamp(s, dur.min(f32::MAX));
        self.segment = Some((s, e));
    }

    /// Lift the segment constraint; subsequent `draw_at` calls wrap
    /// on the full composition duration again.
    pub fn clear_segment(&mut self) {
        self.segment = None;
    }

    /// Current playback segment, if any.
    pub fn segment(&self) -> Option<(f32, f32)> {
        self.segment
    }

    /// Last scene time that `draw_at` resolved to. Used by the
    /// state-machine wrapper to freeze the "source pose" at the
    /// moment a Tweened transition fires — during the crossfade
    /// the source layer renders at this time while the
    /// destination plays forward from its segment start.
    pub fn last_scene_t(&self) -> f32 {
        self.last_scene_t
    }

    /// Override the last-rendered scene time. Set by the
    /// state-machine wrapper after it calls `draw_frame` instead
    /// of `draw_at`, so `last_scene_t` stays authoritative for
    /// pause capture and marker fire calculations across both
    /// render paths.
    pub fn set_last_scene_t(&mut self, t: f32) {
        self.last_scene_t = t;
    }

    /// Render one frame at absolute scene time `scene_t`, bypassing
    /// the player's clock / segment / pause / markers entirely. The
    /// player's internal state is not mutated — `draw_frame` is safe
    /// to call while `draw_at` is driving a separate frame on the
    /// same player. Used by [`crate::state_machine::LottieStateMachine`]
    /// to render the source pose during a Tweened crossfade.
    pub fn draw_frame(&self, ctx: &mut SketchContext<'_>, rect: Rect, scene_t: f32) {
        let src_w = self.root.width.max(1) as f32;
        let src_h = self.root.height.max(1) as f32;
        let sx = rect.width() / src_w;
        let sy = rect.height() / src_h;

        let dc: &mut dyn DrawContext = ctx.draw_context();
        dc.push_transform(Transform::translate(rect.x(), rect.y()));
        dc.push_transform(Transform::scale(sx, sy));
        for (i, layer) in self.layers.iter().enumerate().rev() {
            for &anc_idx in &layer.parent_chain {
                let anc_xform = self.layers[anc_idx].transform.sample(scene_t);
                layer::push_parent_transform(dc, &anc_xform);
            }
            if cull_layer(&self.layers, i, dc, rect, scene_t) {
                for _ in 0..layer.parent_chain.len() {
                    layer::pop_parent_transform(dc);
                }
                continue;
            }
            layer.render(dc, scene_t);
            for _ in 0..layer.parent_chain.len() {
                layer::pop_parent_transform(dc);
            }
        }
        dc.pop_transform();
        dc.pop_transform();
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
        // draw back-to-front. For every layer, push each of its
        // ancestors' transforms (outermost first) so the child's
        // own `push_layer_transform` composes on top of the parent
        // chain — this is what Lottie `parent` semantics require
        // per the [spec](https://lottiefiles.github.io/lottie-docs/).
        // After the parent chain is on the stack, `cull_layer`
        // consults `dc.current_transform()` + the layer's own
        // affine to decide whether to skip the expensive content
        // render. Parent-chain push/pop still happens for culled
        // layers — it's cheap compared to `push_layer`'s
        // offscreen setup for shadow / blur effects.
        for (i, layer) in self.layers.iter().enumerate().rev() {
            for &anc_idx in &layer.parent_chain {
                let anc_xform = self.layers[anc_idx].transform.sample(scene_t);
                layer::push_parent_transform(dc, &anc_xform);
            }
            if cull_layer(&self.layers, i, dc, rect, scene_t) {
                for _ in 0..layer.parent_chain.len() {
                    layer::pop_parent_transform(dc);
                }
                continue;
            }
            layer.render(dc, scene_t);
            for _ in 0..layer.parent_chain.len() {
                layer::pop_parent_transform(dc);
            }
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

/// Should `layers[i]` skip its content render because its
/// composed-world AABB lies entirely outside the destination
/// `dest` rect? Returns `true` only when the layer provides a
/// `source_bounds` **and** the parent-chain-composed affine
/// maps that bound out of screen — layers without known bounds
/// (Null / Unknown) fall through to always-render, matching
/// correctness over throughput.
///
/// Caller has already pushed the parent chain on `dc`, so
/// `dc.current_transform()` gives `root · parent_chain`; this
/// helper composes the layer's own `push_layer_transform` matrix
/// on top without touching the stack (pure math over `Affine2D`).
/// 3D parent transforms short-circuit to `false` — we can't cheaply
/// project a 3D affine-plus-perspective onto the 2D screen rect.
fn cull_layer(
    layers: &[Layer],
    idx: usize,
    dc: &mut dyn blinc_core::DrawContext,
    dest: Rect,
    scene_t: f32,
) -> bool {
    let layer = &layers[idx];
    let Some(bounds) = layer.source_bounds(scene_t) else {
        return false;
    };
    let parent = match dc.current_transform() {
        Transform::Affine2D(a) => a,
        Transform::Mat4(_) => return false,
    };
    let local = layer::layer_local_affine(&layer.transform.sample(scene_t));
    let composed = layer::multiply_affines(&parent, &local);
    !layer::transformed_aabb_intersects(&composed, bounds, &dest)
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
