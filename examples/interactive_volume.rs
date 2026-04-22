//! Interactive volume — dotLottie state-machine example.
//!
//! Loads `assets/Interactive_volume.lottie`. The archive ships a
//! `StateMachine3` spec with four `Level_N` playback states, six
//! `Increase_to_N` / `Decrease_to_N` transition states, and two
//! `Click` interactions targeting the `Increase. button` and
//! `Decrease button` layers. No host-side controls: clicking the
//! in-scene speaker icons drives the FSM entirely from the authored
//! spec via [`LottieStateMachine::dispatch_click`] and its authored
//! `OnComplete` interactions.
//!
//! Run: `cargo run --example interactive_volume`

use blinc_app::prelude::*;
use blinc_app::windowed::{WindowedApp, WindowedContext};
use blinc_canvas_kit::prelude::*;
use blinc_core::{Color, Rect, State};
use blinc_layout::stateful::{stateful_with_key, NoState};
use blinc_lottie::state_machine::LottieStateMachine;
use std::sync::{Arc, Mutex};

const LOTTIE_BYTES: &[u8] = include_bytes!("assets/Interactive_volume.lottie");

/// Canvas edge, in sketch-local pixels. The Lottie player aspect-fits
/// its authored composition inside this rect, so the sketch's click
/// coordinates map directly into the player's coordinate space after
/// [`LottieStateMachine::dispatch_click`] applies the same aspect-fit
/// internally.
const CANVAS_SIZE: f32 = 440.0;

/// Shared access to the state machine. `LottieStateMachine` owns
/// GPU-touching state through its inner `LottiePlayer`, so the
/// render sketch and the click handler need a shared authoritative
/// instance rather than independent clones.
type SharedMachine = Arc<Mutex<LottieStateMachine>>;

struct VolumeSketch {
    machine: SharedMachine,
    /// Latest sketch time handed to `draw_at`. Published through a
    /// shared cell so the click handler can pass the same `t` into
    /// `dispatch_click` — layer transforms sample against the same
    /// moment the renderer used, so hit-testing matches what's on
    /// screen rather than a stale previous pose.
    last_t: Arc<Mutex<f32>>,
    /// Published every frame so the status label stays in sync with
    /// the FSM's current state name.
    state_label: State<String>,
    last_label: String,
}

impl Sketch for VolumeSketch {
    fn draw(&mut self, ctx: &mut SketchContext<'_>, t: f32, _dt: f32) {
        if let Ok(mut m) = self.machine.lock() {
            ctx.play(&mut *m, Rect::new(0.0, 0.0, ctx.width, ctx.height), t);
            let name = m.current_state_name().to_string();
            if name != self.last_label {
                self.last_label = name.clone();
                self.state_label.set(name);
            }
        }
        if let Ok(mut slot) = self.last_t.lock() {
            *slot = t;
        }
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let config = WindowConfig {
        title: "blinc_lottie · Interactive Volume".to_string(),
        width: 520,
        height: 620,
        ..Default::default()
    };

    WindowedApp::run(config, build_ui)
}

fn build_ui(ctx: &mut WindowedContext) -> impl ElementBuilder {
    // Build the machine once and stash it in keyed state so hot-reload
    // rebuilds don't reset the FSM position.
    let machine = ctx.use_state_keyed("volume_machine", || -> SharedMachine {
        let sm = LottieStateMachine::from_dotlottie_bytes(LOTTIE_BYTES)
            .expect("Interactive_volume.lottie should parse cleanly");
        Arc::new(Mutex::new(sm))
    });
    // Shared "last sketch-time" cell — sketch writes, click handler
    // reads. Used so `dispatch_click` samples layer bounds at the
    // same frame the click actually landed on.
    let last_t = ctx.use_state_keyed("volume_last_t", || -> Arc<Mutex<f32>> {
        Arc::new(Mutex::new(0.0))
    });

    let initial_state_name = machine
        .try_get()
        .and_then(|arc| arc.lock().ok().map(|m| m.current_state_name().to_string()))
        .unwrap_or_default();
    let state_label = ctx.use_state_keyed("volume_state_label", move || initial_state_name);

    let shared = machine
        .try_get()
        .expect("volume_machine state initialised above");
    let shared_t = last_t
        .try_get()
        .expect("volume_last_t state initialised above");

    // Click handler lives on the canvas wrapper div. `on_click`
    // delivers coordinates local to the div's own rect, which is
    // the same space `dispatch_click` expects (the player's `draw_at`
    // rect is this rect).
    let click_machine = shared.clone();
    let click_t = shared_t.clone();
    let click_label = state_label.clone();
    let canvas = div()
        .w(CANVAS_SIZE)
        .h(CANVAS_SIZE)
        .bg(Color::TRANSPARENT)
        .rounded(12.0)
        .cursor_pointer()
        .on_click(move |ev| {
            let t = click_t.lock().ok().map(|v| *v).unwrap_or(0.0);
            if let Ok(mut m) = click_machine.lock() {
                let rect = Rect::new(0.0, 0.0, CANVAS_SIZE, CANVAS_SIZE);
                if m.dispatch_click((ev.local_x, ev.local_y), rect, t) {
                    click_label.set(m.current_state_name().to_string());
                }
            }
        })
        .child(sketch(
            "volume_sketch",
            VolumeSketch {
                machine: shared,
                last_t: shared_t,
                state_label: state_label.clone(),
                last_label: String::new(),
            },
        ));

    div()
        .w(ctx.width)
        .h(ctx.height)
        .bg(Color::rgba(0.09, 0.09, 0.12, 1.0))
        .flex_col()
        .items_center()
        .justify_center()
        .gap_px(20.0)
        .child(
            text("Interactive Volume — click the speaker icons to change levels")
                .size(13.0)
                .color(Color::rgba(0.70, 0.70, 0.76, 1.0)),
        )
        .child(canvas)
        .child(build_state_label(state_label))
}

/// Label that follows the state machine's active state name.
/// Subscribed to the `state_label` signal so the text rebuilds
/// whenever the FSM advances.
fn build_state_label(state_label: State<String>) -> impl ElementBuilder {
    let dep = state_label.signal_id();
    stateful_with_key::<NoState>("volume_state_label_view")
        .deps([dep])
        .on_state(move |_ctx| {
            let current = state_label.try_get().unwrap_or_default();
            div().child(
                text(format!("State: {}", current))
                    .size(14.0)
                    .color(Color::rgba(0.88, 0.88, 0.92, 1.0)),
            )
        })
}
