//! Sandy Loading — smallest possible blinc_lottie demo.
//!
//! Loads `assets/Sandy_Loading.json` into a `LottiePlayer` and runs
//! it inside a Blinc `Sketch` at full window. Exercises parse +
//! precomp + shape-layer render end-to-end.
//!
//! Run: `cargo run --example sandy_loading`

use blinc_app::prelude::*;
use blinc_app::windowed::{WindowedApp, WindowedContext};
use blinc_canvas_kit::prelude::*;
use blinc_core::{Color, Rect};
use blinc_lottie::LottiePlayer;

const LOTTIE_JSON: &str = include_str!("assets/Sandy_Loading.json");

struct Loader {
    player: LottiePlayer,
}

impl Sketch for Loader {
    fn draw(&mut self, ctx: &mut SketchContext<'_>, t: f32, _dt: f32) {
        // Square viewport centered in the canvas. The asset is 250x250;
        // fit it into whichever dimension is smaller so the animation
        // stays in-frame through window resizes. 1:1 mapping of the
        // 250×250 source canvas to a 250-pixel square keeps the
        // tessellation edge density matching what an SVG/Canvas2D
        // renderer (lottie-web) would produce at the authored size
        // — scaling up can widen Glass Out's anti-aliased fringe.
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
        title: "blinc_lottie · Sandy Loading".to_string(),
        width: 600,
        height: 600,
        ..Default::default()
    };

    WindowedApp::run(config, build_ui)
}

fn build_ui(ctx: &mut WindowedContext) -> impl ElementBuilder {
    let player =
        LottiePlayer::from_json(LOTTIE_JSON).expect("Sandy_Loading.json should parse cleanly");

    div()
        .w(ctx.width)
        .h(ctx.height)
        .bg(Color::WHITE)
        .child(sketch("sandy_loading", Loader { player }))
}
