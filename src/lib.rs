//! Lottie animation player for Blinc sketches.
//!
//! Parses Lottie JSON and implements [`blinc_canvas_kit::Player`] so the
//! parsed scene can be rendered into any [`blinc_canvas_kit::Sketch`] via
//! `ctx.play(&mut lottie, rect, t)`.
//!
//! # Status
//!
//! Skeleton. The parser loads top-level metadata (version, frame rate,
//! in/out points, canvas dimensions) and stashes layers as opaque JSON
//! values. `Player::draw_at` renders a placeholder rect annotated with
//! metadata — full shape-layer interpolation lands in a follow-up.
//!
//! # Non-goals (for now)
//!
//! - Expression layers (`ix`/`p.k` with JS expressions) are unlikely to
//!   be supported — porting the expression runtime doubles the footprint
//!   of the library and isn't needed for most motion-design exports.
//! - Image asset layers need a `blinc_image` bridge; tracked separately.

use blinc_canvas_kit::{Player, SketchContext};
use blinc_core::layer::{Brush, Color, CornerRadius, Rect};
use blinc_core::DrawContext;

mod parser;

/// Errors surfaced from [`LottiePlayer`] construction.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("lottie json parse failed: {0}")]
    Json(#[from] serde_json::Error),
}

/// A loaded Lottie scene, ready to be rendered into a `Sketch`.
pub struct LottiePlayer {
    root: parser::LottieRoot,
    is_playing: bool,
    /// Internal seek position in seconds. Added to the `t` passed to
    /// `draw_at` to support pause / resume without losing visual state.
    seek_offset: f32,
    /// If paused, freeze at this scene time (seconds).
    paused_at: Option<f32>,
}

impl LottiePlayer {
    /// Parse a Lottie scene from a JSON string.
    pub fn from_json(src: &str) -> Result<Self, Error> {
        let root: parser::LottieRoot = serde_json::from_str(src)?;
        Ok(Self {
            root,
            is_playing: true,
            seek_offset: 0.0,
            paused_at: None,
        })
    }

    /// Parse a Lottie scene from raw JSON bytes.
    pub fn from_bytes(src: &[u8]) -> Result<Self, Error> {
        let root: parser::LottieRoot = serde_json::from_slice(src)?;
        Ok(Self {
            root,
            is_playing: true,
            seek_offset: 0.0,
            paused_at: None,
        })
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

        // Placeholder render: outline rect with a subtle fill so the
        // user can verify the player is wired up and the layout/time
        // plumbing works end-to-end. Real shape-layer rendering will
        // replace this in the parser module.
        let dc: &mut dyn DrawContext = ctx.draw_context();
        let cr = CornerRadius::uniform(4.0);
        dc.fill_rect(rect, cr, Brush::Solid(Color::rgba(0.05, 0.05, 0.1, 0.6)));

        // Flash a progress tick proportional to scene_t / duration so a
        // caller can visually confirm playback is advancing.
        if let Some(dur) = self.duration() {
            if dur > 0.0 {
                let p = (scene_t / dur).clamp(0.0, 1.0);
                let tick = Rect::new(
                    rect.x(),
                    rect.y() + rect.height() - 2.0,
                    rect.width() * p,
                    2.0,
                );
                dc.fill_rect(
                    tick,
                    CornerRadius::uniform(0.0),
                    Brush::Solid(Color::rgba(0.5, 0.8, 1.0, 1.0)),
                );
            }
        }
    }

    fn seek(&mut self, t: f32) {
        // Interpret `t` as an absolute scene time and offset-compensate
        // so the *next* draw_at with a given sketch time resolves to
        // that scene time.
        self.seek_offset = -t;
        if self.paused_at.is_some() {
            self.paused_at = Some(t);
        }
    }

    fn set_playing(&mut self, playing: bool) {
        if playing == self.is_playing {
            return;
        }
        self.is_playing = playing;
        self.paused_at = if playing {
            None
        } else {
            // Freeze at the last computed scene time — we don't have it
            // directly here, so freeze at 0 and let the next draw_at
            // pick up the correct scene_t. (Followup: track last_t.)
            Some(0.0)
        };
    }
}
