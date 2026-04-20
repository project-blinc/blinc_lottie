//! Lottie Gallery — multiple `LottiePlayer` instances in a card grid
//! with per-card play / pause / seek controls.
//!
//! Exists as a debugging surface: renders several assets (or several
//! copies of the same asset) side by side so rendering regressions
//! are visible at a glance, and lets you pause / scrub individual
//! cards to inspect specific frames in isolation.
//!
//! To add an asset: drop a `.json` or `.lottie` file into
//! `examples/assets/` and append an entry to `ASSETS` below.
//!
//! Run: `cargo run --example lottie_gallery`

use blinc_app::prelude::*;
use blinc_app::windowed::{WindowedApp, WindowedContext};
use blinc_canvas_kit::prelude::*;
use blinc_core::{Color, Rect, State};
use blinc_layout::div::Div;
use blinc_layout::stateful::{stateful_with_key, NoState};
use blinc_lottie::LottiePlayer;

// ─── asset registry ────────────────────────────────────────────────────────

/// How an asset's bytes were supplied at compile time. Determines
/// which parser we hand the bytes to — both feed the same
/// `LottiePlayer` in the end.
enum AssetSource {
    /// Raw Bodymovin JSON text.
    Json(&'static str),
    /// Zipped dotLottie archive (1.x `animations/` or 2.0 `a/` layout).
    DotLottie(&'static [u8]),
}

struct Asset {
    name: &'static str,
    source: AssetSource,
}

/// Cards rendered in the gallery, in display order. Add entries here
/// to exercise additional scenes. Copies of the same asset are fine —
/// card state (play / pause / seek) is keyed per slot, not per asset.
const ASSETS: &[Asset] = &[
    Asset {
        name: "Coffee (.lottie)",
        source: AssetSource::DotLottie(include_bytes!("assets/Coffee.lottie")),
    },
    Asset {
        name: "Sandy Loading (JSON)",
        source: AssetSource::Json(include_str!("assets/Sandy_Loading.json")),
    },
];

fn parse_player(source: &AssetSource) -> LottiePlayer {
    match source {
        AssetSource::Json(s) => LottiePlayer::from_json(s).expect("Lottie JSON parse"),
        AssetSource::DotLottie(b) => {
            LottiePlayer::from_dotlottie_bytes(b).expect("dotLottie parse")
        }
    }
}

// ─── sketch ────────────────────────────────────────────────────────────────

/// One sketch per card. Owns the `LottiePlayer` so per-card rendering
/// is independent; pulls playback controls from shared `State` handles
/// the UI controls mutate. The sketch also writes back to `scene_time`
/// each frame while playing so the seek bar's `Stateful` (subscribed
/// via `.deps(…)`) refreshes its progress fill — mirrors how
/// `blinc_media::VideoPlayer` drives `position_signal` from its tick
/// and the seek widget re-renders via its subscribed dep.
struct CardSketch {
    player: LottiePlayer,
    /// Play / pause toggle. Read every frame; when `false` the scene
    /// time is frozen at whatever `current_time` holds.
    is_playing: State<bool>,
    /// Current scene time in seconds, shared with the controls.
    /// Written by the seek-track click handler (user seek) and by the
    /// sketch itself (playback progression). We diff against
    /// `last_seek` each frame to tell the two apart.
    scene_time: State<f32>,
    /// Cached loop length so we can wrap `current_time` without each
    /// tick calling back into `LottiePlayer::duration()`.
    duration: f32,
    /// Internal playback clock. Advanced by `dt` each frame while
    /// playing. Rendered into the canvas; pushed back into
    /// `scene_time` so the seek bar's subscribed `Stateful` rebuilds.
    current_time: f32,
    /// Last value we observed / wrote on `scene_time`. A mismatch on
    /// entry means the user scrubbed while we weren't looking, so we
    /// snap `current_time` to the new target.
    last_seek: f32,
}

impl Sketch for CardSketch {
    fn draw(&mut self, ctx: &mut SketchContext<'_>, _t: f32, dt: f32) {
        // Detect user scrub: if `scene_time` differs from the last
        // value we know about, a slider click set it — snap the
        // internal clock to match.
        let seek = self.scene_time.get();
        if (seek - self.last_seek).abs() > 1e-4 {
            self.current_time = seek.clamp(0.0, self.duration);
            self.last_seek = seek;
        }

        // Advance the clock ourselves. Driving `t` from inside the
        // sketch (rather than using the incoming `_t`) lets one
        // button flip the whole per-card timeline without touching
        // `LottiePlayer` internals.
        if self.is_playing.get() && self.duration > 0.0 {
            self.current_time += dt;
            if self.current_time >= self.duration {
                self.current_time %= self.duration;
            }
            // Publish progress so the seek-bar Stateful (subscribed
            // via deps) rebuilds its fill. `last_seek` tracks our own
            // write so next frame's diff doesn't read as a user seek.
            self.last_seek = self.current_time;
            self.scene_time.set(self.current_time);
        }

        // Hand the whole canvas to the player; `LottiePlayer::draw_at`
        // aspect-fits the comp's declared viewport inside the rect, so
        // portrait comps like `Coffee.lottie` (283×376) keep their
        // native proportions rather than stretching to a square.
        ctx.play(
            &mut self.player,
            Rect::new(0.0, 0.0, ctx.width, ctx.height),
            self.current_time,
        );
    }
}

// ─── card + controls ───────────────────────────────────────────────────────

const CARD_WIDTH: f32 = 340.0;
const CANVAS_SIZE: f32 = 316.0;
const TRACK_HEIGHT: f32 = 8.0;

fn build_card_by_idx(idx: usize, asset: &'static Asset) -> Div {
    // Inside a `Stateful::on_state` closure we don't have access to
    // `WindowedContext`; use the module-level keyed-state helpers
    // instead. Keys are identical to what `ctx.use_state_keyed` would
    // produce, so state persists across the ready-gate transition.
    let is_playing = blinc_core::use_state_keyed(&format!("card_{idx}_playing"), || true);
    let scene_time = blinc_core::use_state_keyed(&format!("card_{idx}_time"), || 0.0_f32);

    // Parse once for duration; parse a fresh copy into the sketch.
    // `LottiePlayer::draw_at` mutates per-frame caches, so sharing one
    // player across cards would glitch on the first paint.
    let duration = parse_player(&asset.source).duration().unwrap_or(5.0);
    let player = parse_player(&asset.source);

    println!("Parsed '{}' with duration {:.2}s", asset.name, duration);

    div()
        .id(format!("card_{idx}"))
        .flex_col()
        .w(CARD_WIDTH)
        .h_fit()
        .gap_px(8.0)
        .p_px(12.0)
        .overflow_clip()
        .bg(Color::rgba(0.13, 0.13, 0.17, 1.0))
        .rounded(12.0)
        .child(
            text(asset.name)
                .size(13.0)
                .color(Color::rgba(0.85, 0.85, 0.9, 1.0)),
        )
        .child(
            div()
                .w(CANVAS_SIZE)
                .h(CANVAS_SIZE)
                .bg(Color::WHITE)
                .rounded(8.0)
                .overflow_clip()
                .child(sketch(
                    &format!("card_{idx}_sketch"),
                    CardSketch {
                        player,
                        is_playing: is_playing.clone(),
                        scene_time: scene_time.clone(),
                        duration,
                        current_time: 0.0,
                        last_seek: 0.0,
                    },
                )),
        )
        .child(build_controls(idx, is_playing, scene_time, duration))
}

/// Per-card control row. Wrapped in `Stateful` so the play-button
/// label and seek-fill width re-render automatically when
/// `is_playing` or `scene_time` change — the canonical pattern from
/// `blinc_media::VideoPlayer`. Bare `Div` + `on_click` wouldn't
/// rebuild: Blinc elements are not reactive by default.
fn build_controls(
    idx: usize,
    is_playing: State<bool>,
    scene_time: State<f32>,
    duration: f32,
) -> impl ElementBuilder {
    let key = format!("card_{idx}_ctrls");
    let is_playing_render = is_playing.clone();
    let scene_time_render = scene_time.clone();
    stateful_with_key::<NoState>(&key)
        .deps([is_playing.signal_id(), scene_time.signal_id()])
        .on_state(move |_ctx| {
            let playing = is_playing_render.get();
            let current = scene_time_render.get();
            let progress = (current / duration.max(0.001)).clamp(0.0, 1.0);

            let play_btn = {
                let s = is_playing_render.clone();
                div()
                    .w(60.0)
                    .h(28.0)
                    .bg(Color::rgba(0.25, 0.5, 0.9, 1.0))
                    .rounded(6.0)
                    .justify_center()
                    .items_center()
                    .on_click(move |_| s.set(!s.get()))
                    .child(
                        text(if playing { "Pause" } else { "Play" })
                            .size(12.0)
                            .color(Color::WHITE)
                            .pointer_events_none(),
                    )
                    .cursor_pointer()
            };

            // Seek track: outer Div carries the click handler (canvas
            // itself doesn't take event callbacks); inner canvas
            // paints the track + fill. Flex-grow so the track
            // stretches across whatever space the card's control row
            // has left over — no hardcoded pixel width to drift.
            let scene_time_click = scene_time_render.clone();
            let seek_track = div()
                .flex_grow()
                .h(TRACK_HEIGHT + 8.0)
                .items_center()
                .on_click(move |ev| {
                    let x = ev.local_x.max(0.0);
                    let w = ev.bounds_width.max(1.0);
                    scene_time_click.set((x / w).clamp(0.0, 1.0) * duration);
                })
                .child(
                    blinc_layout::canvas::canvas(move |ctx, bounds| {
                        let track_y = (bounds.height - TRACK_HEIGHT) / 2.0;
                        ctx.fill_rect(
                            Rect::new(0.0, track_y, bounds.width, TRACK_HEIGHT),
                            (TRACK_HEIGHT / 2.0).into(),
                            blinc_core::Brush::Solid(Color::rgba(0.3, 0.3, 0.35, 1.0)),
                        );
                        if progress > 0.0 {
                            ctx.fill_rect(
                                Rect::new(0.0, track_y, bounds.width * progress, TRACK_HEIGHT),
                                (TRACK_HEIGHT / 2.0).into(),
                                blinc_core::Brush::Solid(Color::rgba(0.25, 0.5, 0.9, 1.0)),
                            );
                        }
                    })
                    .flex_grow()
                    .h(TRACK_HEIGHT + 8.0),
                );

            div()
                .flex_row()
                .gap_px(12.0)
                .items_center()
                .h(32.0)
                
                .child(play_btn)
                .child(seek_track)
        })
}

// ─── entry point ───────────────────────────────────────────────────────────

pub fn build_ui(ctx: &mut WindowedContext) -> impl ElementBuilder {
    let w = ctx.width;
    let h = ctx.height;
    let cards: Vec<Div> = ASSETS
        .iter()
        .enumerate()
        .map(|(idx, asset)| build_card_by_idx(idx, asset))
        .collect();
    let bg = Color::rgba(0.08, 0.08, 0.1, 1.0);

    div().w(w).h(h).overflow_y_scroll().child(
        div()
            .w_full()
            .h_fit()
            .flex_row()
            .justify_center()
            .id("gallery_root_base")
            .bg(bg)
            .p_px(20.0)
            .gap_px(20.0)
            .children(cards),
    )
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let config = WindowConfig {
        title: "blinc_lottie · Gallery".to_string(),
        width: 760,
        height: 500,
        fullscreen: false,
        ..Default::default()
    };

    WindowedApp::run(config, build_ui)
}
