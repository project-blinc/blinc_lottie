//! Sandy Loading — dotLottie (`.lottie` archive) variant.
//!
//! Loads `assets/Sandy Loading.lottie` (the zipped dotLottie
//! distribution) into a `LottiePlayer` via `from_dotlottie_bytes`.
//! Used to verify the JSON and archive versions of the same asset
//! render identically — isolates export-pipeline issues from
//! renderer bugs.
//!
//! Run: `cargo run --example sandy_loading_dotlottie`

use blinc_app::prelude::*;
use blinc_app::windowed::{WindowedApp, WindowedContext};
use blinc_canvas_kit::prelude::*;
use blinc_core::{Color, Rect};
use blinc_lottie::LottiePlayer;

const LOTTIE_BYTES: &[u8] = include_bytes!("assets/Sandy Loading.lottie");

struct Loader {
    player: LottiePlayer,
}

impl Sketch for Loader {
    fn draw(&mut self, ctx: &mut SketchContext<'_>, t: f32, _dt: f32) {
        let size = ctx.width.min(ctx.height) * 0.85;
        let x = (ctx.width - size) * 0.5;
        let y = (ctx.height - size) * 0.5;
        ctx.play(&mut self.player, Rect::new(x, y, size, size), t);
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let config = WindowConfig {
        title: "blinc_lottie · Sandy Loading (.lottie)".to_string(),
        width: 600,
        height: 600,
        ..Default::default()
    };

    WindowedApp::run(config, build_ui)
}

fn build_ui(ctx: &mut WindowedContext) -> impl ElementBuilder {
    let player = LottiePlayer::from_dotlottie_bytes(LOTTIE_BYTES)
        .expect("Sandy Loading.lottie should parse cleanly");

    div()
        .w(ctx.width)
        .h(ctx.height)
        .bg(Color::WHITE)
        .child(sketch("sandy_loading_dotlottie", Loader { player }))
}
