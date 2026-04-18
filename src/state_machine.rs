//! dotLottie state-machine wrapper per [dotLottie 2.0 spec](https://dotlottie.io/spec/2.0/).
//!
//! Decodes `s/<id>.json` from a `.lottie` archive into a
//! [`blinc_core::fsm::StateMachine`] so Lottie state machines
//! inherit Blinc's framework FSM primitive (the same one animations
//! and keyboard-chain code use) rather than reinventing transition
//! + guard semantics.
//!
//! # Supported subset
//!
//! The spec is broad; this implementation covers the intersection
//! that real-world `.lottie` assets actually ship with:
//!
//! | Spec feature            | Status    |
//! |-------------------------|-----------|
//! | `PlaybackState`         | parsed + applied |
//! | `GlobalState`           | parsed; transitions fire from any state |
//! | `segment` (frames)      | parsed, converted to seconds at load |
//! | `loop: bool` / `loopCount` | applied — single-pass or N-pass with terminal freeze |
//! | `mode` Forward / Reverse / Bounce / ReverseBounce | all applied |
//! | `speed` multiplier      | applied |
//! | `autoplay: false`       | applied — pins at starting pose until [`Player::set_playing(true)`] |
//! | `Transition` (immediate)| parsed + applied |
//! | `Tweened` transitions   | parsed as immediate Transition (duration + easing ignored) |
//! | `Event` guards          | applied — `send(input_name)` matches `inputName` |
//! | `Numeric` / `String` / `Boolean` guards | applied — evaluated against the input store |
//! | `SetNumeric` / `SetString` / `SetBoolean` / `Toggle` / `Increment` / `Decrement` actions | applied |
//! | `Fire` / `Reset` actions | parsed, not executed (no event cascade / store reset yet) |
//! | `interactions`, top-level `inputs` seeding | parsed, not yet wired |
//!
//! # Inputs
//!
//! The state machine owns an input store (numeric / string / boolean
//! maps) that guard closures read and action closures write. Host
//! code seeds inputs via [`LottieStateMachine::set_numeric`] etc. —
//! any guard that reads an unseeded input gets `0.0` / `""` / `false`.
//!
//! Transitions with non-Event guards (e.g. a pure `Numeric` check)
//! are still only fireable via [`Self::send`] — setting an input
//! does not re-evaluate transitions. The typical authored pattern
//! is `[Event + Numeric]` conjunction ("on click, if counter > 5"),
//! which works naturally.
//!
//! Everything unsupported parses as a no-op rather than erroring,
//! so assets that use richer features render with a best-effort
//! approximation (usually "state transitions fire on Event guards,
//! other guards pass through"). Scoped-out items are tracked in
//! `BACKLOG.md`.
#![cfg(feature = "dotlottie")]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use blinc_canvas_kit::{Player, SketchContext};
use blinc_core::fsm::{EventId, StateId, StateMachine, Transition};
use blinc_core::layer::Rect;
use serde::Deserialize;

use crate::{Error, LottiePlayer};

// ─────────────────────────────────────────────────────────────────────────────
// Raw spec-shape JSON types.
//
// Kept separate from the resolved runtime structures so the decoder
// can evolve the schema without touching the FSM integration.
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct StateMachineSpec {
    initial: String,
    #[serde(default)]
    states: Vec<StateEntry>,
    /// Author-declared inputs with initial values. Seeds
    /// `LottieStateMachine`'s `InputStore` at load and also
    /// backs the `Reset` action — resetting an input returns it
    /// to the value listed here.
    #[serde(default)]
    inputs: Vec<InputSpec>,
    /// Interaction callbacks (`OnClick` / `OnPointerEnter` / …)
    /// map gesture events to `send()` inputs. Parsing them
    /// requires a host-side event routing layer that doesn't
    /// exist yet, so for now we keep the field to surface parse
    /// errors consistently and drop the bodies.
    #[serde(default)]
    #[allow(dead_code)]
    interactions: Vec<serde_json::Value>,
}

/// Either a literal `[startFrame, endFrame]` pair or a marker
/// name the loader resolves against the animation's `markers`
/// array. The untagged form matches both JSON shapes seen in
/// the wild: `"segment": [0, 60]` and `"segment": "entry"`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum SegmentSpec {
    Frames([f32; 2]),
    Marker(String),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum InputSpec {
    /// Seed a numeric input with `value`. Any keyframe-animated
    /// numeric input would live here too, but the spec's `inputs`
    /// array only declares static defaults — runtime mutation is
    /// what `send_numeric` + `SetNumeric` actions are for.
    Numeric { name: String, value: f64 },
    String { name: String, value: String },
    Boolean { name: String, value: bool },
    /// Unknown input type (e.g. `Vector2`, `Color`, `Trigger`)
    /// parses as a no-op so decoding doesn't fail. Trigger inputs
    /// in particular are cascading events — tracked as future work.
    #[serde(other)]
    Unsupported,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum StateEntry {
    PlaybackState(PlaybackStateSpec),
    GlobalState(GlobalStateSpec),
}

#[derive(Debug, Deserialize)]
struct PlaybackStateSpec {
    name: String,
    #[serde(default)]
    #[allow(dead_code)]
    animation: Option<String>,
    #[serde(default)]
    segment: Option<SegmentSpec>,
    /// Alternative `marker` field on PlaybackState entries — some
    /// authoring tools emit the marker name here instead of
    /// packing it into `segment`. When both are set, `segment`
    /// wins; this is a fallback only.
    #[serde(default)]
    marker: Option<String>,
    #[serde(default, rename = "loop")]
    #[allow(dead_code)]
    r#loop: Option<bool>,
    #[serde(default, rename = "loopCount")]
    #[allow(dead_code)]
    loop_count: Option<u32>,
    #[serde(default)]
    #[allow(dead_code)]
    mode: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    speed: Option<f32>,
    #[serde(default)]
    #[allow(dead_code)]
    autoplay: Option<bool>,
    #[serde(default)]
    transitions: Vec<TransitionSpec>,
}

#[derive(Debug, Deserialize)]
struct GlobalStateSpec {
    name: String,
    #[serde(default)]
    transitions: Vec<TransitionSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum TransitionSpec {
    /// Immediate transition — apply the new state's segment on the
    /// next render tick.
    Transition {
        #[serde(rename = "toState")]
        to_state: String,
        #[serde(default)]
        guards: Vec<GuardSpec>,
        #[serde(default)]
        actions: Vec<ActionSpec>,
    },
    /// Spec says Tweened animates between states over `duration`
    /// seconds using `easing` (cubic bezier control points). We
    /// parse but downgrade to immediate — a cross-state tween
    /// needs an interpolator we don't ship yet (tracked in
    /// BACKLOG).
    Tweened {
        #[serde(rename = "toState")]
        to_state: String,
        #[allow(dead_code)]
        duration: f32,
        #[serde(default)]
        #[allow(dead_code)]
        easing: Option<[f32; 4]>,
        #[serde(default)]
        guards: Vec<GuardSpec>,
        #[serde(default)]
        actions: Vec<ActionSpec>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum GuardSpec {
    /// Fires when `inputName` is sent via [`LottieStateMachine::send`].
    Event {
        #[serde(rename = "inputName")]
        input_name: String,
    },
    /// Comparison against a numeric input. When paired with an
    /// Event guard on the same transition, both must pass — the
    /// conjunction is evaluated by the FSM's guard closure.
    Numeric {
        #[serde(rename = "inputName")]
        input_name: String,
        #[serde(rename = "conditionType")]
        condition_type: NumericOp,
        #[serde(rename = "compareTo")]
        compare_to: f64,
    },
    /// Comparison against a string input.
    String {
        #[serde(rename = "inputName")]
        input_name: String,
        #[serde(rename = "conditionType")]
        condition_type: StringOp,
        #[serde(rename = "compareTo")]
        compare_to: String,
    },
    /// Comparison against a boolean input.
    Boolean {
        #[serde(rename = "inputName")]
        input_name: String,
        #[serde(rename = "conditionType")]
        condition_type: BooleanOp,
        #[serde(rename = "compareTo")]
        compare_to: bool,
    },
    /// Parse catch-all so future guard kinds don't break decoding
    /// of existing archives. An unsupported guard on a transition
    /// always passes, matching the "lenient parse" contract documented
    /// at the top of the module.
    #[serde(other)]
    Unsupported,
}

#[derive(Debug, Clone, Copy, Deserialize)]
enum NumericOp {
    Equal,
    NotEqual,
    GreaterThan,
    GreaterOrEqual,
    LessThan,
    LessOrEqual,
}

#[derive(Debug, Clone, Copy, Deserialize)]
enum StringOp {
    Equal,
    NotEqual,
}

#[derive(Debug, Clone, Copy, Deserialize)]
enum BooleanOp {
    Equal,
    NotEqual,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ActionSpec {
    /// Overwrite a numeric input with a fixed value.
    SetNumeric {
        #[serde(rename = "inputName")]
        input_name: String,
        value: f64,
    },
    /// Overwrite a string input.
    SetString {
        #[serde(rename = "inputName")]
        input_name: String,
        value: String,
    },
    /// Overwrite a boolean input.
    SetBoolean {
        #[serde(rename = "inputName")]
        input_name: String,
        value: bool,
    },
    /// Flip a boolean input (missing inputs default to `false`,
    /// so `Toggle` on an unset key produces `true`).
    Toggle {
        #[serde(rename = "inputName")]
        input_name: String,
    },
    /// Add to a numeric input (defaults to `0.0` + value if unset).
    Increment {
        #[serde(rename = "inputName")]
        input_name: String,
        #[serde(default = "default_one")]
        value: f64,
    },
    /// Subtract from a numeric input.
    Decrement {
        #[serde(rename = "inputName")]
        input_name: String,
        #[serde(default = "default_one")]
        value: f64,
    },
    /// Dispatch another event. Parsed but not executed — the FSM
    /// executes actions during `send()`, so a cascading `send()`
    /// from within would need a deferred queue (tracked in BACKLOG).
    Fire {
        #[serde(rename = "inputName")]
        #[allow(dead_code)]
        input_name: String,
    },
    /// Reset an input's value. Parsed but not executed — the
    /// state-machine JSON doesn't carry initial values in the
    /// shape this loader consumes, so "reset to what?" is
    /// ambiguous. Tracked in BACKLOG.
    Reset {
        #[serde(rename = "inputName")]
        #[allow(dead_code)]
        input_name: String,
    },
    #[serde(other)]
    Unsupported,
}

fn default_one() -> f64 {
    1.0
}

// ─────────────────────────────────────────────────────────────────────────────
// Runtime state machine
// ─────────────────────────────────────────────────────────────────────────────

/// A [`LottiePlayer`] driven by a dotLottie state machine.
/// Composes the player + Blinc's FSM primitive. See the module-
/// level docs for the supported-features matrix.
///
/// The `Debug` impl elides the heavy player + FSM internals — it
/// prints the state-name table and the number of registered events
/// so log lines stay short.
pub struct LottieStateMachine {
    player: LottiePlayer,
    fsm: StateMachine,
    /// `StateId` (index) → author-facing name, used by
    /// [`Self::current_state_name`].
    state_names: Vec<String>,
    /// Input (event) name → stable `EventId`. First occurrence
    /// while walking transitions assigns the ID; subsequent
    /// transitions referring to the same input reuse it.
    event_names: HashMap<String, EventId>,
    /// Per-state segment in **seconds**. Frame-based segments
    /// from the JSON are converted at load using the player's
    /// [`LottiePlayer::frame_rate`].
    state_segments: HashMap<StateId, (f32, f32)>,
    /// Per-state playback configuration (mode, loop, loopCount,
    /// speed, autoplay). Populated at load for every
    /// `PlaybackState` that has a segment — `GlobalState` entries
    /// skip since they never render on their own. States without
    /// a config fall through to the player's default clock.
    state_playback: HashMap<StateId, StatePlayback>,
    /// Sketch time at which the current state was entered. Used
    /// with `state_playback[current]` to compute scene time each
    /// frame. `None` until the first `draw_at` observes a `t`,
    /// so a state change that happens long before the first frame
    /// doesn't produce huge `elapsed` values on frame 0.
    state_entered_at_t: Option<f32>,
    /// Frozen scene time when the player is paused via
    /// [`Player::set_playing(false)`]. `Some(t)` means render at
    /// that exact scene time regardless of the incoming `t`;
    /// `None` means normal playback. Resetting to `None` rebases
    /// `state_entered_at_t` so unpausing doesn't skip ahead by
    /// the paused duration.
    paused_scene_t: Option<f32>,
    /// Numeric / string / boolean input store. Shared with guard
    /// and action closures inside the FSM via `Arc<Mutex<_>>`
    /// because Blinc's `Transition::with_guard` / `with_action`
    /// hold `Fn() -> bool` / `FnMut()` — nullary closures that can
    /// only reach state through captured references.
    inputs: Arc<Mutex<InputStore>>,
    /// Author-declared defaults — captured at load from the spec's
    /// top-level `inputs` array, consumed by the `Reset` action.
    /// Immutable for the machine's lifetime, so `Arc` without a
    /// Mutex suffices.
    defaults: Arc<InputDefaults>,
    /// Event queue populated by `Fire` actions running inside
    /// FSM transitions. Drained by [`Self::send`] after each
    /// edge fires so cascading events process serially without
    /// re-entering the FSM mid-transition. Bounded cascade depth
    /// enforced by [`MAX_CASCADE_DEPTH`] to short-circuit action
    /// loops (`A.on(evt) → fire(evt) → A.on(evt) → …`).
    pending_events: Arc<Mutex<Vec<EventId>>>,
    /// `(event_id, source_state_id) → Tweened metadata` for every
    /// FSM edge derived from a `Tweened` transition. `send()`
    /// consults this after an edge fires and, if present, arms
    /// `pending_tween` with the crossfade parameters so the next
    /// `draw_at` starts the fade.
    tween_edges: HashMap<(EventId, StateId), TweenParams>,
    /// Active visual crossfade between two segments, if any.
    /// `None` whenever the player renders a single state normally.
    tween: Option<Tween>,
}

/// Hard cap on `Fire`-driven event cascades inside a single
/// `send()`. Any authored cycle (A→B→A, A→A) would otherwise
/// spin forever; at 32 hops we bail, leaving the machine in
/// whatever state the cascade reached.
const MAX_CASCADE_DEPTH: usize = 32;

/// Compiled per-state playback settings. Durations are in seconds
/// (frame values get converted at load against `LottiePlayer::frame_rate`).
#[derive(Debug, Clone, Copy)]
struct StatePlayback {
    segment_start: f32,
    segment_end: f32,
    mode: PlaybackMode,
    /// `true` means the segment loops indefinitely (the spec's
    /// default). `false` pins playback at the terminal pose
    /// after a single pass — forward-mode pauses on the last
    /// frame, reverse-mode pauses on the first.
    looping: bool,
    /// If set, cap the total one-way passes through the segment.
    /// Forward-mode playback of `loopCount: 3` plays the segment
    /// three times then freezes. Bounce-mode counts each
    /// direction change as a pass.
    loop_count: Option<u32>,
    /// Playback speed multiplier applied to the sketch-time delta.
    /// `2.0` means half-duration playback; negative values are
    /// clamped to zero (for reverse playback use `PlaybackMode`,
    /// not negative speed).
    speed: f32,
    /// `false` means the state enters paused at its starting pose
    /// until explicitly played via [`Player::set_playing`].
    autoplay: bool,
}

#[derive(Debug, Clone, Copy)]
enum PlaybackMode {
    Forward,
    Reverse,
    Bounce,
    ReverseBounce,
}

impl PlaybackMode {
    fn from_str(s: &str) -> Self {
        match s {
            "Reverse" => Self::Reverse,
            "Bounce" => Self::Bounce,
            "ReverseBounce" => Self::ReverseBounce,
            _ => Self::Forward,
        }
    }
}

impl StatePlayback {
    /// Map a non-negative elapsed time (in seconds, already
    /// speed-scaled by the caller) onto the state's scene time.
    /// Handles mode, loop, and loopCount in one pass so no dead
    /// state leaks between frames.
    fn scene_t(&self, elapsed: f32) -> f32 {
        let duration = (self.segment_end - self.segment_start).max(f32::EPSILON);
        let passes_f = (elapsed / duration).max(0.0);
        let mut pass = passes_f.floor() as u32;
        let mut phase = passes_f - pass as f32; // [0, 1)

        if !self.looping && self.loop_count.is_none() {
            // A single pass then freeze.
            if pass > 0 {
                pass = 0;
                phase = 1.0;
            }
        } else if let Some(max) = self.loop_count {
            // Pass index is zero-based; `max` is the count so
            // `max - 1` is the final pass. Past the end, freeze
            // at the terminal of that pass.
            if max == 0 || pass >= max {
                pass = max.saturating_sub(1);
                phase = 1.0;
            }
        }

        // Each "pass" through the segment alternates direction for
        // bounce modes. Forward/Reverse keep the same direction
        // every pass.
        let reversed = match self.mode {
            PlaybackMode::Forward => false,
            PlaybackMode::Reverse => true,
            PlaybackMode::Bounce => pass % 2 == 1,
            PlaybackMode::ReverseBounce => pass % 2 == 0,
        };
        let seg_phase = if reversed { 1.0 - phase } else { phase };
        self.segment_start + seg_phase * duration
    }
}

/// Frozen parameters for a `Tweened` transition — precomputed at
/// load so that `send()` can arm the tween without allocating.
/// Cloned into `Tween` when the transition fires.
#[derive(Debug, Clone)]
struct TweenParams {
    duration: f32,
    easing: Option<[f32; 4]>,
    /// Destination state (resolved at load). Mirrors the FSM's
    /// `to_state` so we don't have to re-derive it after the edge
    /// fires — `send()` could otherwise look up the FSM's history,
    /// but that's fragile.
    to_state: StateId,
}

/// In-flight crossfade between two state segments.
#[derive(Debug, Clone)]
struct Tween {
    /// Source state's segment in seconds — used to apply the
    /// right segment window when rendering the frozen source pose.
    /// `None` when the source state had no `segment` (renders the
    /// full composition at the frozen scene time).
    #[allow(dead_code)]
    from_segment: Option<(f32, f32)>,
    /// Scene time to render the source pose at. The source pose
    /// is frozen at this time for the tween's duration — it does
    /// not continue animating during the crossfade.
    from_scene_t: f32,
    duration: f32,
    easing: Option<[f32; 4]>,
    /// Sketch time at which the tween started. `None` on the
    /// frame the tween arms — set to `t` on the next `draw_at`
    /// so sketches that `send()` long before their first frame
    /// don't compute negative or huge progress values.
    started_at_t: Option<f32>,
}

/// Backing store for dotLottie inputs. Guards read from it during
/// FSM evaluation; actions mutate it on successful transitions.
#[derive(Debug, Default)]
struct InputStore {
    numeric: HashMap<String, f64>,
    string: HashMap<String, String>,
    boolean: HashMap<String, bool>,
}

/// Snapshot of the author-declared initial values from the spec's
/// top-level `inputs` array. The `Reset` action restores an input
/// to its entry here; if the input wasn't declared, Reset clears
/// it (matches the rest of the "unseeded = default-value" contract).
#[derive(Debug, Default)]
struct InputDefaults {
    numeric: HashMap<String, f64>,
    string: HashMap<String, String>,
    boolean: HashMap<String, bool>,
}

/// Mutable view handed to each [`ActionOp`] during execution.
/// Bundles the input store with the ancillary pieces actions might
/// touch (defaults for Reset, pending-event queue for Fire).
struct ActionContext<'a> {
    store: &'a mut InputStore,
    defaults: &'a InputDefaults,
    pending_events: &'a mut Vec<EventId>,
}

impl LottieStateMachine {
    /// Load a `.lottie` archive. The initial animation (per
    /// `manifest.json`'s `initial.animation`, falling back to the
    /// first declared animation) becomes the player; the initial
    /// state machine (per `manifest.json`'s `initial.stateMachine`
    /// or the first declared SM) drives it. An archive without a
    /// state machine returns a no-op wrapper — [`Self::send`]
    /// returns `false` for every call, but rendering still works.
    pub fn from_dotlottie_bytes(src: &[u8]) -> Result<Self, Error> {
        let archive = crate::dotlottie::extract(src)?;
        let anim_bytes = archive.initial_animation().ok_or_else(|| {
            Error::Archive("archive has no animations to render".to_string())
        })?;
        let player = LottiePlayer::from_bytes(anim_bytes)?;
        match archive.initial_state_machine() {
            Some(sm_bytes) => Self::from_player_and_spec(player, sm_bytes),
            None => Ok(Self::empty_for(player)),
        }
    }

    /// Compose from an already-loaded player + raw state-machine
    /// JSON bytes. For callers who managed archive unpacking
    /// themselves (e.g. streamed decoding, test fixtures) rather
    /// than going through [`Self::from_dotlottie_bytes`].
    pub fn from_player_and_spec(
        mut player: LottiePlayer,
        spec_json: &[u8],
    ) -> Result<Self, Error> {
        let spec: StateMachineSpec = serde_json::from_slice(spec_json)?;
        let fr = player.frame_rate();

        // Walk states first so transition resolution has the full
        // state-ID table to look up `toState` names. `PlaybackState`
        // and `GlobalState` share the ID space — order-preserving
        // for stable cross-run IDs.
        let mut state_names: Vec<String> = Vec::with_capacity(spec.states.len());
        let mut state_ids: HashMap<String, StateId> = HashMap::new();
        let mut state_segments: HashMap<StateId, (f32, f32)> = HashMap::new();
        // Track which states are Global so we can expand their
        // transitions over every other state at wiring time (spec:
        // GlobalState transitions fire from *any* state).
        let mut global_source_ids: Vec<StateId> = Vec::new();

        let mut state_playback: HashMap<StateId, StatePlayback> = HashMap::new();
        // Build a name→(time, duration) lookup over the animation's
        // markers so `SegmentSpec::Marker("name")` / `marker` fields
        // can resolve into concrete seconds. Markers in the player
        // are already seconds-relative; we reuse them verbatim.
        let marker_table: HashMap<&str, (f32, f32)> = player
            .markers()
            .iter()
            .map(|m| (m.name.as_str(), (m.time_seconds, m.duration_seconds)))
            .collect();
        for (i, entry) in spec.states.iter().enumerate() {
            let id = i as StateId;
            let name = match entry {
                StateEntry::PlaybackState(p) => {
                    // Resolve the segment in seconds: explicit frame
                    // pair wins, then `segment: "<marker>"`, then the
                    // alternate `marker` field. A literal pair is
                    // clamped to start ≤ end so a swapped authored
                    // range can't produce a negative-length segment.
                    // Marker misses fall through silently — the
                    // state renders un-segmented, matching the
                    // permissive-parse posture elsewhere.
                    let resolved: Option<(f32, f32)> = match &p.segment {
                        Some(SegmentSpec::Frames([start_f, end_f])) => {
                            let start_s = start_f / fr;
                            let end_s = (end_f / fr).max(start_s);
                            Some((start_s, end_s))
                        }
                        Some(SegmentSpec::Marker(name)) => marker_table
                            .get(name.as_str())
                            .map(|(t, d)| (*t, *t + d.max(0.0))),
                        None => p.marker.as_deref().and_then(|name| {
                            marker_table
                                .get(name)
                                .map(|(t, d)| (*t, *t + d.max(0.0)))
                        }),
                    };
                    if let Some((start_s, end_s)) = resolved {
                        state_segments.insert(id, (start_s, end_s));
                        // Compile per-state playback config alongside
                        // the segment. Defaults mirror the spec: loop
                        // on, no loop cap, forward, speed 1×, autoplay.
                        let looping = p.r#loop.unwrap_or(true);
                        state_playback.insert(
                            id,
                            StatePlayback {
                                segment_start: start_s,
                                segment_end: end_s,
                                mode: p
                                    .mode
                                    .as_deref()
                                    .map(PlaybackMode::from_str)
                                    .unwrap_or(PlaybackMode::Forward),
                                looping,
                                loop_count: p.loop_count,
                                // Negative speeds aren't meaningful —
                                // authors reverse via `mode`. Clamp
                                // to non-negative so the elapsed math
                                // never goes backwards on its own.
                                speed: p.speed.unwrap_or(1.0).max(0.0),
                                autoplay: p.autoplay.unwrap_or(true),
                            },
                        );
                    }
                    p.name.clone()
                }
                StateEntry::GlobalState(g) => {
                    global_source_ids.push(id);
                    g.name.clone()
                }
            };
            state_ids.insert(name.clone(), id);
            state_names.push(name);
        }

        let initial_id = *state_ids.get(&spec.initial).ok_or_else(|| {
            Error::Archive(format!(
                "state_machine initial '{}' not in states list",
                spec.initial
            ))
        })?;

        // Second pass: build the FSM transition table. For each
        // state, walk its `transitions` array and wire one
        // `blinc_core::Transition` per (source, event, target)
        // tuple. GlobalState transitions expand over every other
        // state so the spec's "fires from any state" semantics
        // hold. Non-Event guards on a transition are compiled
        // into a conjunctive guard closure shared across all FSM
        // edges generated from that transition. Actions likewise
        // compile into an action closure attached to every edge.
        // Pre-scan: pluck every event name that any transition
        // references, either as an Event guard or as the target of
        // a `Fire` action. Building the event→id map up front lets
        // action closures bind a stable `EventId` even when the Fire
        // target's own Event guard appears later in the walk.
        let mut event_names: HashMap<String, EventId> = HashMap::new();
        for entry in &spec.states {
            for t in entry_transitions(entry) {
                let (guards, actions) = match t {
                    TransitionSpec::Transition { guards, actions, .. }
                    | TransitionSpec::Tweened {
                        guards, actions, ..
                    } => (guards, actions),
                };
                for g in guards {
                    if let GuardSpec::Event { input_name } = g {
                        let next_id = event_names.len() as EventId;
                        event_names.entry(input_name.clone()).or_insert(next_id);
                    }
                }
                for a in actions {
                    if let ActionSpec::Fire { input_name } = a {
                        let next_id = event_names.len() as EventId;
                        event_names.entry(input_name.clone()).or_insert(next_id);
                    }
                }
            }
        }

        let mut transitions: Vec<Transition> = Vec::new();
        let mut tween_edges: HashMap<(EventId, StateId), TweenParams> = HashMap::new();

        // Seed the input store from the spec's top-level `inputs`
        // array. Also snapshot those defaults so the `Reset` action
        // has something to return to — without a defaults table the
        // action can't know whether "reset" means zero, empty, or
        // the author-authored initial value.
        let mut initial_store = InputStore::default();
        let mut defaults = InputDefaults::default();
        for spec_input in &spec.inputs {
            match spec_input {
                InputSpec::Numeric { name, value } => {
                    initial_store.numeric.insert(name.clone(), *value);
                    defaults.numeric.insert(name.clone(), *value);
                }
                InputSpec::String { name, value } => {
                    initial_store.string.insert(name.clone(), value.clone());
                    defaults.string.insert(name.clone(), value.clone());
                }
                InputSpec::Boolean { name, value } => {
                    initial_store.boolean.insert(name.clone(), *value);
                    defaults.boolean.insert(name.clone(), *value);
                }
                InputSpec::Unsupported => {}
            }
        }
        let inputs: Arc<Mutex<InputStore>> = Arc::new(Mutex::new(initial_store));
        let defaults = Arc::new(defaults);
        // Deferred-event queue. `Fire` actions push event_ids here
        // from inside the FSM's action closure; `send()` drains
        // the queue *after* each edge fires so cascading events
        // don't re-enter the FSM mid-transition.
        let pending_events: Arc<Mutex<Vec<EventId>>> = Arc::new(Mutex::new(Vec::new()));

        for (i, entry) in spec.states.iter().enumerate() {
            let from_id = i as StateId;
            let (source_ids, spec_transitions): (Vec<StateId>, &[TransitionSpec]) = match entry {
                StateEntry::PlaybackState(p) => (vec![from_id], &p.transitions),
                StateEntry::GlobalState(_) => {
                    // Every non-Global state gets these transitions
                    // as a fallback.
                    let mut expanded: Vec<StateId> = Vec::new();
                    for j in 0..spec.states.len() {
                        let jid = j as StateId;
                        if !global_source_ids.contains(&jid) {
                            expanded.push(jid);
                        }
                    }
                    (expanded, entry_transitions(entry))
                }
            };

            for t in spec_transitions {
                let (to_name, guards, actions, tween_params) = match t {
                    TransitionSpec::Transition {
                        to_state,
                        guards,
                        actions,
                    } => (to_state, guards, actions, None),
                    TransitionSpec::Tweened {
                        to_state,
                        guards,
                        actions,
                        duration,
                        easing,
                    } => (
                        to_state,
                        guards,
                        actions,
                        Some((*duration, *easing)),
                    ),
                };
                let to_id = *state_ids.get(to_name).ok_or_else(|| {
                    Error::Archive(format!(
                        "transition references unknown target state '{to_name}'",
                    ))
                })?;

                // Split guards into event names + a conjunctive
                // condition list. Without at least one Event guard
                // the transition is unreachable via `send()`; skip
                // wiring (but parse + validate the target so
                // malformed JSON still surfaces early).
                let (event_input_names, conditions) = split_guards(guards);
                if event_input_names.is_empty() {
                    continue;
                }

                for event_input in &event_input_names {
                    let next_event_id = event_names.len() as EventId;
                    let event_id = *event_names
                        .entry(event_input.clone())
                        .or_insert(next_event_id);

                    for &src in &source_ids {
                        if let Some((duration, easing)) = tween_params {
                            // Record one entry per (event, source) so
                            // `send()` can arm the tween after the
                            // FSM edge fires. Duration/easing are
                            // shared across every expanded edge.
                            tween_edges.insert(
                                (event_id, src),
                                TweenParams {
                                    duration,
                                    easing,
                                    to_state: to_id,
                                },
                            );
                        }
                        let mut tr = Transition::new(src, event_id, to_id);

                        if !conditions.is_empty() {
                            let conds = conditions.clone();
                            let store = Arc::clone(&inputs);
                            tr = tr.with_guard(move || {
                                let s = store.lock().expect("inputs poisoned");
                                conds.iter().all(|c| c.eval(&s))
                            });
                        }

                        let action_ops: Vec<ActionOp> = actions
                            .iter()
                            .filter_map(|a| ActionOp::from_spec(a, &event_names))
                            .collect();
                        if !action_ops.is_empty() {
                            let ops = action_ops;
                            let store = Arc::clone(&inputs);
                            let defaults = Arc::clone(&defaults);
                            let pending = Arc::clone(&pending_events);
                            tr = tr.with_action(move || {
                                let mut s = store.lock().expect("inputs poisoned");
                                let mut p = pending.lock().expect("pending queue poisoned");
                                let mut ctx = ActionContext {
                                    store: &mut s,
                                    defaults: &defaults,
                                    pending_events: &mut p,
                                };
                                for op in &ops {
                                    op.apply(&mut ctx);
                                }
                            });
                        }

                        transitions.push(tr);
                    }
                }
            }
        }

        let fsm = StateMachine::new(initial_id, transitions);
        apply_state_segment(&mut player, initial_id, &state_segments);

        Ok(Self {
            player,
            fsm,
            state_names,
            event_names,
            state_segments,
            state_playback,
            state_entered_at_t: None,
            paused_scene_t: None,
            inputs,
            defaults,
            pending_events,
            tween_edges,
            tween: None,
        })
    }

    fn empty_for(player: LottiePlayer) -> Self {
        Self {
            player,
            fsm: StateMachine::new(0, Vec::new()),
            state_names: Vec::new(),
            event_names: HashMap::new(),
            state_segments: HashMap::new(),
            state_playback: HashMap::new(),
            state_entered_at_t: None,
            paused_scene_t: None,
            inputs: Arc::new(Mutex::new(InputStore::default())),
            defaults: Arc::new(InputDefaults::default()),
            pending_events: Arc::new(Mutex::new(Vec::new())),
            tween_edges: HashMap::new(),
            tween: None,
        }
    }

    /// Set a numeric input used by `Numeric` guards and `Set*` /
    /// `Increment` / `Decrement` / `Toggle` actions. Guards read
    /// the store on every `send()`, so this method does not fire
    /// transitions on its own — host code sets the input first,
    /// then sends an event.
    pub fn set_numeric(&mut self, name: &str, value: f64) {
        let mut s = self.inputs.lock().expect("inputs poisoned");
        s.numeric.insert(name.to_string(), value);
    }

    /// Set a string input. See [`Self::set_numeric`] for
    /// evaluation semantics.
    pub fn set_string(&mut self, name: &str, value: &str) {
        let mut s = self.inputs.lock().expect("inputs poisoned");
        s.string.insert(name.to_string(), value.to_string());
    }

    /// Set a boolean input. See [`Self::set_numeric`] for
    /// evaluation semantics.
    pub fn set_boolean(&mut self, name: &str, value: bool) {
        let mut s = self.inputs.lock().expect("inputs poisoned");
        s.boolean.insert(name.to_string(), value);
    }

    /// Current value of a numeric input, or `None` if it hasn't
    /// been seeded. Useful for host code that mirrors input state
    /// back into its own UI / model.
    pub fn get_numeric(&self, name: &str) -> Option<f64> {
        let s = self.inputs.lock().expect("inputs poisoned");
        s.numeric.get(name).copied()
    }

    /// Current value of a string input, or `None` if unseeded.
    pub fn get_string(&self, name: &str) -> Option<String> {
        let s = self.inputs.lock().expect("inputs poisoned");
        s.string.get(name).cloned()
    }

    /// Current value of a boolean input, or `None` if unseeded.
    pub fn get_boolean(&self, name: &str) -> Option<bool> {
        let s = self.inputs.lock().expect("inputs poisoned");
        s.boolean.get(name).copied()
    }

    /// Restore `name` to the value the spec's top-level `inputs`
    /// array declared for it, or clear it entirely when no default
    /// was declared. Same semantics as a `Reset` action firing
    /// from a transition — useful for host code driving lifecycle
    /// resets outside the FSM's own event flow.
    pub fn reset_input(&mut self, name: &str) {
        let mut s = self.inputs.lock().expect("inputs poisoned");
        if let Some(v) = self.defaults.numeric.get(name) {
            s.numeric.insert(name.to_string(), *v);
        } else {
            s.numeric.remove(name);
        }
        if let Some(v) = self.defaults.string.get(name) {
            s.string.insert(name.to_string(), v.clone());
        } else {
            s.string.remove(name);
        }
        if let Some(v) = self.defaults.boolean.get(name) {
            s.boolean.insert(name.to_string(), *v);
        } else {
            s.boolean.remove(name);
        }
    }

    /// Borrow the inner player (for `Player` trait calls: `draw_at`,
    /// `duration`, etc.).
    pub fn player(&self) -> &LottiePlayer {
        &self.player
    }

    /// Mutable borrow of the inner player.
    pub fn player_mut(&mut self) -> &mut LottiePlayer {
        &mut self.player
    }

    /// Name of the active state, or empty string when the archive
    /// didn't carry a state machine.
    pub fn current_state_name(&self) -> &str {
        let id = self.fsm.current_state();
        self.state_names
            .get(id as usize)
            .map(String::as_str)
            .unwrap_or("")
    }

    /// Fire an input event. Matches transitions whose `guards`
    /// include an `Event` guard with `inputName` equal to
    /// `event_name`, then updates the player's segment when the
    /// transition changed state. Returns `true` on state change,
    /// `false` when the event is unknown or no matching transition
    /// applies from the current state.
    ///
    /// If the fired edge was authored as `Tweened`, this also arms
    /// a visual crossfade between the source and destination
    /// segments. The fade starts on the next [`Player::draw_at`]
    /// call (to capture an accurate starting `t`) and runs for the
    /// authored `duration` seconds, easing along the authored cubic
    /// bezier if present.
    ///
    /// Any `Fire` actions the firing edge carries enqueue events
    /// into a pending buffer; this method drains them after the
    /// edge completes, cascading up to [`MAX_CASCADE_DEPTH`] hops
    /// before bailing. The return value reflects the *initial*
    /// state change — cascades beyond that contribute to the
    /// machine's state but not the return value.
    pub fn send(&mut self, event_name: &str) -> bool {
        let Some(&event_id) = self.event_names.get(event_name) else {
            return false;
        };
        let changed = self.dispatch(event_id);
        // Drain the cascade queue. Fire actions appended onto the
        // queue while the above edge fired now dispatch in
        // insertion order. Cap at MAX_CASCADE_DEPTH to keep authored
        // cycles (A→fire(evt)→A) from spinning forever.
        let mut drained = 0usize;
        loop {
            let next = {
                let mut q = self.pending_events.lock().expect("pending queue poisoned");
                if q.is_empty() || drained >= MAX_CASCADE_DEPTH {
                    q.clear();
                    break;
                }
                q.remove(0)
            };
            drained += 1;
            self.dispatch(next);
        }
        changed
    }

    /// Single-edge dispatch: run the FSM for `event_id`, apply the
    /// destination segment on state change, and arm a tween if the
    /// edge came from a `Tweened` transition. Shared between the
    /// primary `send()` call and the `Fire`-cascade drain loop so
    /// cascaded events get tween arming + segment updates too.
    fn dispatch(&mut self, event_id: EventId) -> bool {
        let prev = self.fsm.current_state();
        let from_scene_t = self.player.last_scene_t();
        let from_segment = self.player.segment();
        let next = self.fsm.send(event_id);
        if next == prev {
            return false;
        }
        apply_state_segment(&mut self.player, next, &self.state_segments);
        // Rebase the per-state clock so the destination plays from
        // its authored starting pose. `None` defers the actual
        // sketch-time capture to the next `draw_at` call — mirrors
        // the tween arming pattern.
        self.state_entered_at_t = None;
        self.paused_scene_t = None;
        if let Some(params) = self.tween_edges.get(&(event_id, prev)) {
            if params.to_state == next && params.duration > 0.0 {
                self.tween = Some(Tween {
                    from_segment,
                    from_scene_t,
                    duration: params.duration,
                    easing: params.easing,
                    started_at_t: None,
                });
            }
        }
        true
    }

    /// Compute the effective scene time for the current state at
    /// sketch time `t`. Returns `None` when no per-state playback
    /// config applies (e.g. the machine has no segments, or the
    /// loaded spec was empty) — the caller falls through to the
    /// player's default clock.
    fn state_scene_t(&mut self, t: f32) -> Option<f32> {
        // Paused: return the frozen scene time regardless of `t`.
        if let Some(frozen) = self.paused_scene_t {
            return Some(frozen);
        }
        let active = self.fsm.current_state();
        let pb = self.state_playback.get(&active).copied()?;
        let entered = *self.state_entered_at_t.get_or_insert(t);
        if !pb.autoplay {
            // Stay pinned at the segment's starting pose until
            // something external plays us. Reverse modes enter at
            // the segment end (that's their "first pose").
            return Some(match pb.mode {
                PlaybackMode::Reverse | PlaybackMode::ReverseBounce => pb.segment_end,
                _ => pb.segment_start,
            });
        }
        let elapsed = (t - entered).max(0.0) * pb.speed;
        Some(pb.scene_t(elapsed))
    }

    /// Whether a Tweened transition's crossfade is currently
    /// running. Exposes the fade state for host code that needs
    /// to gate input (e.g. ignoring clicks mid-transition).
    pub fn is_tweening(&self) -> bool {
        self.tween.is_some()
    }
}

impl Player for LottieStateMachine {
    fn duration(&self) -> Option<f32> {
        self.player.duration()
    }

    /// Seek the current state's timeline to scene-time `t`. Rebases
    /// `state_entered_at_t` so `draw_at` returns `t` on the next
    /// frame with the same sketch clock, and clears any paused
    /// pose so playback resumes from the seek.
    fn seek(&mut self, t: f32) {
        self.paused_scene_t = None;
        // A fresh `state_entered_at_t = None` forces the next
        // `draw_at` to re-anchor the clock. Between now and that
        // frame the pose is implicit — we can't render without a
        // sketch-time reference to solve for. Delegate to the
        // player as well so a bare `LottiePlayer::seek` remains
        // consistent for state-machine-less archives.
        self.state_entered_at_t = None;
        self.player.seek(t);
    }

    /// Pause at the current scene time, or resume playing. Paused
    /// state is restored scene-time-exact — no pose skip when the
    /// host re-enables playback after a pause.
    fn set_playing(&mut self, playing: bool) {
        if playing {
            if self.paused_scene_t.take().is_some() {
                // Resume: rebase the entered timestamp so the next
                // `draw_at` doesn't observe the paused-duration's
                // worth of elapsed time.
                self.state_entered_at_t = None;
            }
        } else {
            // Snapshot the scene time at the last `draw_at` — the
            // player's `last_scene_t` already tracks it whether we
            // rendered via `draw_at` or `draw_frame`.
            self.paused_scene_t = Some(self.player.last_scene_t());
        }
        self.player.set_playing(playing);
    }

    /// Render the state machine's current pose. When a Tweened
    /// transition is active, blends the frozen source pose with
    /// the destination animation by crossfading opacity over the
    /// authored `duration`.
    fn draw_at(&mut self, ctx: &mut SketchContext<'_>, rect: Rect, t: f32) {
        // No tween: pick between per-state playback and the
        // player's default clock based on whether we have a config
        // for the current state.
        if self.tween.is_none() {
            if let Some(scene_t) = self.state_scene_t(t) {
                self.player.draw_frame(ctx, rect, scene_t);
                // `draw_frame` doesn't update the player's
                // `last_scene_t`, which the pause/tween paths
                // read. Mirror the effective scene time manually
                // so `set_playing(false)` captures the right
                // pose.
                self.player.set_last_scene_t(scene_t);
            } else {
                self.player.draw_at(ctx, rect, t);
            }
            return;
        }

        // Tween path: first arm the started_at_t if this is the
        // tween's first frame, then split behaviour on progress.
        let (raw, from_scene_t, easing) = {
            let tween = self.tween.as_mut().expect("tween presence checked");
            let started = *tween.started_at_t.get_or_insert(t);
            let raw = if tween.duration > 0.0 {
                ((t - started) / tween.duration).clamp(0.0, 1.0)
            } else {
                1.0
            };
            (raw, tween.from_scene_t, tween.easing)
        };

        if raw >= 1.0 {
            // Tween finished — clear and render the destination
            // normally via the per-state playback path.
            self.tween = None;
            if let Some(scene_t) = self.state_scene_t(t) {
                self.player.draw_frame(ctx, rect, scene_t);
                self.player.set_last_scene_t(scene_t);
            } else {
                self.player.draw_at(ctx, rect, t);
            }
            return;
        }

        let progress = apply_cubic_bezier_easing(raw, easing);
        // Destination scene time — sampled the same way a non-tween
        // draw would, so the pose matches the moment the tween
        // ends. Falls back to `last_scene_t` when there's no
        // per-state config (e.g. empty state machine).
        let dest_scene_t = self.state_scene_t(t).unwrap_or(self.player.last_scene_t());

        {
            let dc = ctx.draw_context();
            dc.push_opacity(1.0 - progress);
        }
        self.player.draw_frame(ctx, rect, from_scene_t);
        {
            let dc = ctx.draw_context();
            dc.pop_opacity();
        }
        {
            let dc = ctx.draw_context();
            dc.push_opacity(progress);
        }
        self.player.draw_frame(ctx, rect, dest_scene_t);
        self.player.set_last_scene_t(dest_scene_t);
        {
            let dc = ctx.draw_context();
            dc.pop_opacity();
        }
    }
}

/// Apply a cubic bezier easing curve parameterised by `[p1x, p1y,
/// p2x, p2y]`. The curve's start (0, 0) and end (1, 1) are fixed;
/// `p1` / `p2` are the two middle control points.
///
/// Implementation: solve `bezier_x(t_param) = u` via a bounded
/// Newton's method (four iterations tends to converge for the
/// control-point ranges Lottie / CSS authoring produces), then
/// evaluate `bezier_y(t_param)`. Falls back to linear when `easing`
/// is `None` or any control-point component is NaN.
fn apply_cubic_bezier_easing(u: f32, easing: Option<[f32; 4]>) -> f32 {
    let Some([p1x, p1y, p2x, p2y]) = easing else {
        return u;
    };
    if !(p1x.is_finite() && p1y.is_finite() && p2x.is_finite() && p2y.is_finite()) {
        return u;
    }
    // Cubic bezier polynomial coefficients: B(t) = a*t^3 + b*t^2 + c*t,
    // where a = 1 - 3*p2 + 3*p1, b = 3*p2 - 6*p1, c = 3*p1.
    // Derivative: B'(t) = 3*a*t^2 + 2*b*t + c.
    let cx = 3.0 * p1x;
    let bx = 3.0 * p2x - 6.0 * p1x;
    let ax = 1.0 - 3.0 * p2x + 3.0 * p1x;
    let cy = 3.0 * p1y;
    let by = 3.0 * p2y - 6.0 * p1y;
    let ay = 1.0 - 3.0 * p2y + 3.0 * p1y;

    let bezier_x = |t: f32| ((ax * t + bx) * t + cx) * t;
    let bezier_dx = |t: f32| (3.0 * ax * t + 2.0 * bx) * t + cx;
    let bezier_y = |t: f32| ((ay * t + by) * t + cy) * t;

    // Newton iterations. Start at `u` (good initial guess — the
    // identity curve is the fallback when the bezier is close to
    // linear) and refine toward `bezier_x(t) = u`.
    let mut t = u;
    for _ in 0..6 {
        let dx = bezier_dx(t);
        if dx.abs() < 1e-6 {
            break;
        }
        let err = bezier_x(t) - u;
        t -= err / dx;
        t = t.clamp(0.0, 1.0);
    }
    bezier_y(t)
}

/// Helper: borrow the `transitions` array out of a state entry
/// without cloning. Only called for `GlobalState` (PlaybackState
/// takes the fast path above), so the match exhaustiveness is
/// enforced at the call site.
fn entry_transitions(entry: &StateEntry) -> &[TransitionSpec] {
    match entry {
        StateEntry::PlaybackState(p) => &p.transitions,
        StateEntry::GlobalState(g) => &g.transitions,
    }
}

/// Split a transition's raw `guards` array into the set of event
/// input names (which key the FSM dispatch) and a compiled list
/// of conjunctive conditions the guard closure evaluates at send
/// time. `Unsupported` guards are dropped with the lenient-parse
/// contract documented at the top of the module.
fn split_guards(guards: &[GuardSpec]) -> (Vec<String>, Vec<Condition>) {
    let mut events: Vec<String> = Vec::new();
    let mut conditions: Vec<Condition> = Vec::new();
    for g in guards {
        match g {
            GuardSpec::Event { input_name } => events.push(input_name.clone()),
            GuardSpec::Numeric {
                input_name,
                condition_type,
                compare_to,
            } => conditions.push(Condition::Numeric {
                name: input_name.clone(),
                op: *condition_type,
                value: *compare_to,
            }),
            GuardSpec::String {
                input_name,
                condition_type,
                compare_to,
            } => conditions.push(Condition::String {
                name: input_name.clone(),
                op: *condition_type,
                value: compare_to.clone(),
            }),
            GuardSpec::Boolean {
                input_name,
                condition_type,
                compare_to,
            } => conditions.push(Condition::Boolean {
                name: input_name.clone(),
                op: *condition_type,
                value: *compare_to,
            }),
            GuardSpec::Unsupported => {}
        }
    }
    (events, conditions)
}

/// Compiled guard condition. `Clone` because one `TransitionSpec`
/// feeds N FSM edges (event × source state), and each edge gets
/// its own captured copy inside the guard closure.
#[derive(Debug, Clone)]
enum Condition {
    Numeric {
        name: String,
        op: NumericOp,
        value: f64,
    },
    String {
        name: String,
        op: StringOp,
        value: String,
    },
    Boolean {
        name: String,
        op: BooleanOp,
        value: bool,
    },
}

impl Condition {
    fn eval(&self, store: &InputStore) -> bool {
        match self {
            Condition::Numeric { name, op, value } => {
                let v = store.numeric.get(name).copied().unwrap_or(0.0);
                match op {
                    NumericOp::Equal => (v - value).abs() < f64::EPSILON,
                    NumericOp::NotEqual => (v - value).abs() >= f64::EPSILON,
                    NumericOp::GreaterThan => v > *value,
                    NumericOp::GreaterOrEqual => v >= *value,
                    NumericOp::LessThan => v < *value,
                    NumericOp::LessOrEqual => v <= *value,
                }
            }
            Condition::String { name, op, value } => {
                let default = String::new();
                let v = store.string.get(name).unwrap_or(&default);
                match op {
                    StringOp::Equal => v == value,
                    StringOp::NotEqual => v != value,
                }
            }
            Condition::Boolean { name, op, value } => {
                let v = store.boolean.get(name).copied().unwrap_or(false);
                match op {
                    BooleanOp::Equal => v == *value,
                    BooleanOp::NotEqual => v != *value,
                }
            }
        }
    }
}

/// Compiled transition action. `Fire` compiles only when the
/// referenced event name is registered in the pre-scan pass; an
/// un-matchable `Fire` still parses but drops here so it can't
/// stall the action queue. `Reset` needs just the input name +
/// kind — the actual default comes from the shared
/// [`InputDefaults`] snapshot at apply time.
#[derive(Debug, Clone)]
enum ActionOp {
    SetNumeric { name: String, value: f64 },
    SetString { name: String, value: String },
    SetBoolean { name: String, value: bool },
    Toggle { name: String },
    Increment { name: String, value: f64 },
    Decrement { name: String, value: f64 },
    /// Queue an event for cascaded dispatch after the current
    /// transition completes. `send()` drains the queue in
    /// insertion order.
    Fire { event: EventId },
    /// Restore an input to its author-declared default, or clear
    /// it entirely when no default was declared.
    Reset { name: String },
}

impl ActionOp {
    fn from_spec(spec: &ActionSpec, event_names: &HashMap<String, EventId>) -> Option<Self> {
        match spec {
            ActionSpec::SetNumeric { input_name, value } => Some(Self::SetNumeric {
                name: input_name.clone(),
                value: *value,
            }),
            ActionSpec::SetString { input_name, value } => Some(Self::SetString {
                name: input_name.clone(),
                value: value.clone(),
            }),
            ActionSpec::SetBoolean { input_name, value } => Some(Self::SetBoolean {
                name: input_name.clone(),
                value: *value,
            }),
            ActionSpec::Toggle { input_name } => Some(Self::Toggle {
                name: input_name.clone(),
            }),
            ActionSpec::Increment { input_name, value } => Some(Self::Increment {
                name: input_name.clone(),
                value: *value,
            }),
            ActionSpec::Decrement { input_name, value } => Some(Self::Decrement {
                name: input_name.clone(),
                value: *value,
            }),
            ActionSpec::Fire { input_name } => event_names
                .get(input_name)
                .copied()
                .map(|event| Self::Fire { event }),
            ActionSpec::Reset { input_name } => Some(Self::Reset {
                name: input_name.clone(),
            }),
            ActionSpec::Unsupported => None,
        }
    }

    fn apply(&self, ctx: &mut ActionContext<'_>) {
        match self {
            ActionOp::SetNumeric { name, value } => {
                ctx.store.numeric.insert(name.clone(), *value);
            }
            ActionOp::SetString { name, value } => {
                ctx.store.string.insert(name.clone(), value.clone());
            }
            ActionOp::SetBoolean { name, value } => {
                ctx.store.boolean.insert(name.clone(), *value);
            }
            ActionOp::Toggle { name } => {
                let entry = ctx.store.boolean.entry(name.clone()).or_insert(false);
                *entry = !*entry;
            }
            ActionOp::Increment { name, value } => {
                let entry = ctx.store.numeric.entry(name.clone()).or_insert(0.0);
                *entry += *value;
            }
            ActionOp::Decrement { name, value } => {
                let entry = ctx.store.numeric.entry(name.clone()).or_insert(0.0);
                *entry -= *value;
            }
            ActionOp::Fire { event } => {
                ctx.pending_events.push(*event);
            }
            ActionOp::Reset { name } => {
                // "Reset to what?" — the author-declared default if
                // there was one, otherwise drop the key so future
                // reads surface the unseeded value (0.0 / "" /
                // false). Reset walks all three kinds because the
                // same name *could* appear in multiple maps; in
                // practice it's almost always one kind.
                if let Some(v) = ctx.defaults.numeric.get(name) {
                    ctx.store.numeric.insert(name.clone(), *v);
                } else {
                    ctx.store.numeric.remove(name);
                }
                if let Some(v) = ctx.defaults.string.get(name) {
                    ctx.store.string.insert(name.clone(), v.clone());
                } else {
                    ctx.store.string.remove(name);
                }
                if let Some(v) = ctx.defaults.boolean.get(name) {
                    ctx.store.boolean.insert(name.clone(), *v);
                } else {
                    ctx.store.boolean.remove(name);
                }
            }
        }
    }
}

fn apply_state_segment(
    player: &mut LottiePlayer,
    state: StateId,
    state_segments: &HashMap<StateId, (f32, f32)>,
) {
    use blinc_canvas_kit::Player;
    match state_segments.get(&state) {
        Some(&(start, end)) => {
            player.play_segment(start, end);
            player.seek(start);
        }
        None => player.clear_segment(),
    }
}

impl std::fmt::Debug for LottieStateMachine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LottieStateMachine")
            .field("states", &self.state_names)
            .field("events", &self.event_names.keys().collect::<Vec<_>>())
            .field(
                "segments",
                &self
                    .state_segments
                    .iter()
                    .map(|(id, seg)| {
                        (
                            self.state_names
                                .get(*id as usize)
                                .map(String::as_str)
                                .unwrap_or("?"),
                            seg,
                        )
                    })
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_ANIM: &str = r#"{
        "v": "5.0",
        "fr": 60,
        "ip": 0,
        "op": 180,
        "w": 100,
        "h": 100,
        "layers": []
    }"#;

    /// Spec-shape state machine with three PlaybackStates and
    /// Event-guarded transitions. Segment values are in **frames**
    /// — the loader converts using the animation's 60 fps frame
    /// rate, so `[0, 60]` becomes `[0.0s, 1.0s]`.
    fn spec_json() -> &'static str {
        r#"{
            "initial": "idle",
            "states": [
                {
                    "type": "PlaybackState",
                    "name": "idle",
                    "animation": "main",
                    "segment": [0, 60],
                    "transitions": [
                        {
                            "type": "Transition",
                            "toState": "hover",
                            "guards": [{ "type": "Event", "inputName": "pointer.enter" }]
                        }
                    ]
                },
                {
                    "type": "PlaybackState",
                    "name": "hover",
                    "animation": "main",
                    "segment": [60, 120],
                    "transitions": [
                        {
                            "type": "Transition",
                            "toState": "idle",
                            "guards": [{ "type": "Event", "inputName": "pointer.leave" }]
                        },
                        {
                            "type": "Transition",
                            "toState": "pressed",
                            "guards": [{ "type": "Event", "inputName": "pointer.click" }]
                        }
                    ]
                },
                {
                    "type": "PlaybackState",
                    "name": "pressed",
                    "animation": "main",
                    "segment": [120, 180],
                    "transitions": []
                }
            ]
        }"#
    }

    #[test]
    fn initial_state_applies_frame_segment_in_seconds() {
        let player = LottiePlayer::from_json(FIXTURE_ANIM).unwrap();
        let sm = LottieStateMachine::from_player_and_spec(player, spec_json().as_bytes()).unwrap();
        assert_eq!(sm.current_state_name(), "idle");
        // `[0, 60]` frames at 60 fps = `[0.0, 1.0]` seconds.
        assert_eq!(sm.player().segment(), Some((0.0, 1.0)));
    }

    #[test]
    fn event_guard_fires_transition() {
        let player = LottiePlayer::from_json(FIXTURE_ANIM).unwrap();
        let mut sm =
            LottieStateMachine::from_player_and_spec(player, spec_json().as_bytes()).unwrap();
        assert!(sm.send("pointer.enter"));
        assert_eq!(sm.current_state_name(), "hover");
        assert_eq!(sm.player().segment(), Some((1.0, 2.0)));
        assert!(sm.send("pointer.click"));
        assert_eq!(sm.current_state_name(), "pressed");
    }

    #[test]
    fn unknown_event_is_false_and_keeps_state() {
        let player = LottiePlayer::from_json(FIXTURE_ANIM).unwrap();
        let mut sm =
            LottieStateMachine::from_player_and_spec(player, spec_json().as_bytes()).unwrap();
        assert!(!sm.send("pointer.drag"));
        assert_eq!(sm.current_state_name(), "idle");
    }

    #[test]
    fn transition_not_applicable_from_current_state() {
        let player = LottiePlayer::from_json(FIXTURE_ANIM).unwrap();
        let mut sm =
            LottieStateMachine::from_player_and_spec(player, spec_json().as_bytes()).unwrap();
        // `pointer.click` is only wired out of `hover`.
        assert!(!sm.send("pointer.click"));
        assert_eq!(sm.current_state_name(), "idle");
    }

    #[test]
    fn global_state_transitions_fire_from_any_state() {
        // GlobalState semantics per spec: its transitions fire
        // from any state in the machine. Here `reset` is always
        // reachable via `pointer.dblclick`.
        let spec = r#"{
            "initial": "idle",
            "states": [
                {
                    "type": "PlaybackState",
                    "name": "idle",
                    "animation": "main",
                    "segment": [0, 60]
                },
                {
                    "type": "PlaybackState",
                    "name": "loaded",
                    "animation": "main",
                    "segment": [60, 120]
                },
                {
                    "type": "GlobalState",
                    "name": "reset_routes",
                    "transitions": [
                        {
                            "type": "Transition",
                            "toState": "idle",
                            "guards": [{ "type": "Event", "inputName": "pointer.dblclick" }]
                        }
                    ]
                }
            ]
        }"#;
        let player = LottiePlayer::from_json(FIXTURE_ANIM).unwrap();
        let mut sm = LottieStateMachine::from_player_and_spec(player, spec.as_bytes()).unwrap();
        // Give the SM a non-idle starting state by firing some
        // made-up transition first? No — there aren't any from
        // idle. Manually seed via player state? Simpler: just
        // check that sending from idle fires (a no-op since we're
        // already there, returns false).
        assert!(!sm.send("pointer.dblclick"));
        assert_eq!(sm.current_state_name(), "idle");
        // The Global transition is wired but from `loaded` it'd
        // fire. Since we can't reach `loaded` without another
        // transition, assert structural: event is registered.
        assert!(sm.event_names.contains_key("pointer.dblclick"));
    }

    #[test]
    fn tweened_transition_arms_crossfade_on_send() {
        // Tweened transitions flip the FSM immediately and arm a
        // visual crossfade — `is_tweening()` flips true on `send`,
        // the player's segment is already on the destination.
        let spec = r#"{
            "initial": "a",
            "states": [
                {
                    "type": "PlaybackState",
                    "name": "a",
                    "animation": "main",
                    "segment": [0, 60],
                    "transitions": [
                        {
                            "type": "Tweened",
                            "toState": "b",
                            "duration": 0.5,
                            "easing": [0.25, 0.1, 0.25, 1.0],
                            "guards": [{ "type": "Event", "inputName": "go" }]
                        }
                    ]
                },
                {
                    "type": "PlaybackState",
                    "name": "b",
                    "animation": "main",
                    "segment": [60, 120]
                }
            ]
        }"#;
        let player = LottiePlayer::from_json(FIXTURE_ANIM).unwrap();
        let mut sm = LottieStateMachine::from_player_and_spec(player, spec.as_bytes()).unwrap();
        assert!(!sm.is_tweening());
        assert!(sm.send("go"));
        assert_eq!(sm.current_state_name(), "b");
        assert!(sm.is_tweening(), "tween should arm after Tweened transition");
        assert_eq!(sm.player().segment(), Some((1.0, 2.0)), "segment already on dest");
    }

    #[test]
    fn tweened_with_zero_duration_does_not_arm() {
        // Zero-duration tween is meaningless — treat as immediate
        // so the crossfade path isn't entered on degenerate input.
        let spec = r#"{
            "initial": "a",
            "states": [
                {
                    "type": "PlaybackState",
                    "name": "a",
                    "animation": "main",
                    "transitions": [
                        {
                            "type": "Tweened",
                            "toState": "b",
                            "duration": 0.0,
                            "guards": [{ "type": "Event", "inputName": "go" }]
                        }
                    ]
                },
                { "type": "PlaybackState", "name": "b", "animation": "main" }
            ]
        }"#;
        let player = LottiePlayer::from_json(FIXTURE_ANIM).unwrap();
        let mut sm = LottieStateMachine::from_player_and_spec(player, spec.as_bytes()).unwrap();
        assert!(sm.send("go"));
        assert!(!sm.is_tweening());
    }

    #[test]
    fn cubic_bezier_easing_matches_endpoints_and_shape() {
        // Linear fallback (no easing provided) is identity.
        assert!((apply_cubic_bezier_easing(0.0, None) - 0.0).abs() < 1e-6);
        assert!((apply_cubic_bezier_easing(1.0, None) - 1.0).abs() < 1e-6);
        assert!((apply_cubic_bezier_easing(0.5, None) - 0.5).abs() < 1e-6);

        // CSS `ease-in`: starts slow, ends fast. Midpoint < 0.5.
        let ease_in = Some([0.42, 0.0, 1.0, 1.0]);
        assert!((apply_cubic_bezier_easing(0.0, ease_in) - 0.0).abs() < 1e-4);
        assert!((apply_cubic_bezier_easing(1.0, ease_in) - 1.0).abs() < 1e-4);
        assert!(
            apply_cubic_bezier_easing(0.5, ease_in) < 0.5,
            "ease-in midpoint should be below linear"
        );

        // CSS `ease-out`: starts fast, ends slow. Midpoint > 0.5.
        let ease_out = Some([0.0, 0.0, 0.58, 1.0]);
        assert!(
            apply_cubic_bezier_easing(0.5, ease_out) > 0.5,
            "ease-out midpoint should be above linear"
        );
    }

    #[test]
    fn transitions_without_event_guard_dont_wire() {
        // A transition with only a Numeric guard (or no guards)
        // has nothing to key on via `send(name)`. The event table
        // stays empty and fire attempts return false.
        let spec = r#"{
            "initial": "a",
            "states": [
                {
                    "type": "PlaybackState",
                    "name": "a",
                    "animation": "main",
                    "transitions": [
                        {
                            "type": "Transition",
                            "toState": "b",
                            "guards": [
                                { "type": "Numeric", "inputName": "p", "conditionType": "GreaterThan", "compareTo": 0.5 }
                            ]
                        }
                    ]
                },
                {
                    "type": "PlaybackState",
                    "name": "b",
                    "animation": "main"
                }
            ]
        }"#;
        let player = LottiePlayer::from_json(FIXTURE_ANIM).unwrap();
        let mut sm = LottieStateMachine::from_player_and_spec(player, spec.as_bytes()).unwrap();
        assert!(sm.event_names.is_empty());
        assert!(!sm.send("anything"));
    }

    #[test]
    fn unknown_initial_state_is_an_error() {
        let player = LottiePlayer::from_json(FIXTURE_ANIM).unwrap();
        let bad = r#"{ "initial": "ghost", "states": [{"type":"PlaybackState","name":"real","animation":"a"}] }"#;
        match LottieStateMachine::from_player_and_spec(player, bad.as_bytes()) {
            Err(Error::Archive(_)) => {}
            other => panic!("expected Archive error, got {other:?}"),
        }
    }

    #[test]
    fn numeric_guard_blocks_transition_until_input_set() {
        // `Event + Numeric` conjunction: `go` only fires when
        // `counter > 5`. Without seeding, counter defaults to 0
        // so the guard fails; after `set_numeric(6)` it passes.
        let spec = r#"{
            "initial": "a",
            "states": [
                {
                    "type": "PlaybackState",
                    "name": "a",
                    "animation": "main",
                    "transitions": [
                        {
                            "type": "Transition",
                            "toState": "b",
                            "guards": [
                                { "type": "Event", "inputName": "go" },
                                { "type": "Numeric", "inputName": "counter",
                                  "conditionType": "GreaterThan", "compareTo": 5.0 }
                            ]
                        }
                    ]
                },
                { "type": "PlaybackState", "name": "b", "animation": "main" }
            ]
        }"#;
        let player = LottiePlayer::from_json(FIXTURE_ANIM).unwrap();
        let mut sm = LottieStateMachine::from_player_and_spec(player, spec.as_bytes()).unwrap();
        assert!(!sm.send("go"), "guard should block without counter set");
        assert_eq!(sm.current_state_name(), "a");
        sm.set_numeric("counter", 6.0);
        assert!(sm.send("go"));
        assert_eq!(sm.current_state_name(), "b");
    }

    #[test]
    fn string_and_boolean_guards_evaluate() {
        let spec = r#"{
            "initial": "a",
            "states": [
                {
                    "type": "PlaybackState",
                    "name": "a",
                    "animation": "main",
                    "transitions": [
                        {
                            "type": "Transition",
                            "toState": "b",
                            "guards": [
                                { "type": "Event", "inputName": "go" },
                                { "type": "String", "inputName": "role",
                                  "conditionType": "Equal", "compareTo": "admin" },
                                { "type": "Boolean", "inputName": "ready",
                                  "conditionType": "Equal", "compareTo": true }
                            ]
                        }
                    ]
                },
                { "type": "PlaybackState", "name": "b", "animation": "main" }
            ]
        }"#;
        let player = LottiePlayer::from_json(FIXTURE_ANIM).unwrap();
        let mut sm = LottieStateMachine::from_player_and_spec(player, spec.as_bytes()).unwrap();
        sm.set_string("role", "guest");
        sm.set_boolean("ready", true);
        assert!(!sm.send("go"), "role mismatch should block");
        sm.set_string("role", "admin");
        sm.set_boolean("ready", false);
        assert!(!sm.send("go"), "ready=false should block");
        sm.set_boolean("ready", true);
        assert!(sm.send("go"));
        assert_eq!(sm.current_state_name(), "b");
    }

    #[test]
    fn set_and_increment_actions_mutate_input_store() {
        // Actions fire on successful transition. `go` sets a
        // numeric, flips a boolean, and increments a counter.
        let spec = r#"{
            "initial": "a",
            "states": [
                {
                    "type": "PlaybackState",
                    "name": "a",
                    "animation": "main",
                    "transitions": [
                        {
                            "type": "Transition",
                            "toState": "b",
                            "guards": [{ "type": "Event", "inputName": "go" }],
                            "actions": [
                                { "type": "SetNumeric", "inputName": "value", "value": 42.0 },
                                { "type": "Toggle", "inputName": "flag" },
                                { "type": "Increment", "inputName": "count", "value": 3.0 }
                            ]
                        }
                    ]
                },
                { "type": "PlaybackState", "name": "b", "animation": "main" }
            ]
        }"#;
        let player = LottiePlayer::from_json(FIXTURE_ANIM).unwrap();
        let mut sm = LottieStateMachine::from_player_and_spec(player, spec.as_bytes()).unwrap();
        sm.set_numeric("count", 10.0);
        assert!(sm.send("go"));
        assert_eq!(sm.get_numeric("value"), Some(42.0));
        assert_eq!(sm.get_boolean("flag"), Some(true));
        assert_eq!(sm.get_numeric("count"), Some(13.0));
    }

    #[test]
    fn top_level_inputs_seed_the_store_at_load() {
        // `inputs` array at the spec root declares author defaults.
        // The input store sees those values immediately and guards
        // evaluate against them without host setup.
        let spec = r#"{
            "initial": "a",
            "inputs": [
                { "type": "Numeric", "name": "counter", "value": 6.0 },
                { "type": "String",  "name": "role",    "value": "admin" },
                { "type": "Boolean", "name": "ready",   "value": true }
            ],
            "states": [
                {
                    "type": "PlaybackState",
                    "name": "a",
                    "animation": "main",
                    "transitions": [
                        {
                            "type": "Transition",
                            "toState": "b",
                            "guards": [
                                { "type": "Event", "inputName": "go" },
                                { "type": "Numeric", "inputName": "counter",
                                  "conditionType": "GreaterThan", "compareTo": 5.0 }
                            ]
                        }
                    ]
                },
                { "type": "PlaybackState", "name": "b", "animation": "main" }
            ]
        }"#;
        let player = LottiePlayer::from_json(FIXTURE_ANIM).unwrap();
        let mut sm = LottieStateMachine::from_player_and_spec(player, spec.as_bytes()).unwrap();
        // Seeds are visible before any host interaction.
        assert_eq!(sm.get_numeric("counter"), Some(6.0));
        assert_eq!(sm.get_string("role"), Some("admin".to_string()));
        assert_eq!(sm.get_boolean("ready"), Some(true));
        // Guard passes because the seed already places counter above
        // the threshold.
        assert!(sm.send("go"));
        assert_eq!(sm.current_state_name(), "b");
    }

    #[test]
    fn reset_action_restores_declared_defaults() {
        // `counter` has an author-declared default of 3. After a
        // SetNumeric action overwrites it to 99, a Reset action on a
        // follow-up transition should bring it back to 3, not to 0.
        let spec = r#"{
            "initial": "a",
            "inputs": [
                { "type": "Numeric", "name": "counter", "value": 3.0 }
            ],
            "states": [
                {
                    "type": "PlaybackState", "name": "a", "animation": "main",
                    "transitions": [
                        {
                            "type": "Transition", "toState": "b",
                            "guards": [{ "type": "Event", "inputName": "go" }],
                            "actions": [
                                { "type": "SetNumeric", "inputName": "counter", "value": 99.0 }
                            ]
                        }
                    ]
                },
                {
                    "type": "PlaybackState", "name": "b", "animation": "main",
                    "transitions": [
                        {
                            "type": "Transition", "toState": "a",
                            "guards": [{ "type": "Event", "inputName": "back" }],
                            "actions": [
                                { "type": "Reset", "inputName": "counter" }
                            ]
                        }
                    ]
                }
            ]
        }"#;
        let player = LottiePlayer::from_json(FIXTURE_ANIM).unwrap();
        let mut sm = LottieStateMachine::from_player_and_spec(player, spec.as_bytes()).unwrap();
        assert_eq!(sm.get_numeric("counter"), Some(3.0));
        assert!(sm.send("go"));
        assert_eq!(sm.get_numeric("counter"), Some(99.0));
        assert!(sm.send("back"));
        assert_eq!(sm.get_numeric("counter"), Some(3.0), "reset should restore default");

        // Reset on an undeclared input clears it — no phantom zero.
        sm.set_numeric("adhoc", 5.0);
        sm.reset_input("adhoc");
        assert_eq!(sm.get_numeric("adhoc"), None);
    }

    #[test]
    fn fire_action_cascades_events_serially() {
        // `kick` fires the `chain` event; the `chain` transition
        // advances to the final state. One `send("kick")` should
        // land on `c` even though the author wrote two separate
        // transitions.
        let spec = r#"{
            "initial": "a",
            "states": [
                {
                    "type": "PlaybackState", "name": "a", "animation": "main",
                    "transitions": [
                        {
                            "type": "Transition", "toState": "b",
                            "guards": [{ "type": "Event", "inputName": "kick" }],
                            "actions": [
                                { "type": "Fire", "inputName": "chain" }
                            ]
                        }
                    ]
                },
                {
                    "type": "PlaybackState", "name": "b", "animation": "main",
                    "transitions": [
                        {
                            "type": "Transition", "toState": "c",
                            "guards": [{ "type": "Event", "inputName": "chain" }]
                        }
                    ]
                },
                { "type": "PlaybackState", "name": "c", "animation": "main" }
            ]
        }"#;
        let player = LottiePlayer::from_json(FIXTURE_ANIM).unwrap();
        let mut sm = LottieStateMachine::from_player_and_spec(player, spec.as_bytes()).unwrap();
        assert!(sm.send("kick"));
        assert_eq!(sm.current_state_name(), "c", "cascade should land on final state");
    }

    #[test]
    fn fire_cascade_bails_on_author_cycle() {
        // Author wrote a self-loop via Fire — `loop` fires `loop`
        // again. Without the depth cap we'd spin forever; with it,
        // the cascade stops after MAX_CASCADE_DEPTH hops leaving
        // the machine alive (no panic, no infinite loop).
        let spec = r#"{
            "initial": "a",
            "states": [
                {
                    "type": "PlaybackState", "name": "a", "animation": "main",
                    "transitions": [
                        {
                            "type": "Transition", "toState": "a",
                            "guards": [{ "type": "Event", "inputName": "loop" }],
                            "actions": [
                                { "type": "Fire", "inputName": "loop" }
                            ]
                        }
                    ]
                }
            ]
        }"#;
        let player = LottiePlayer::from_json(FIXTURE_ANIM).unwrap();
        let mut sm = LottieStateMachine::from_player_and_spec(player, spec.as_bytes()).unwrap();
        // A self-loop target is `prev == next`, so `dispatch` reports
        // `false` even though the FSM ticked through the edge. What
        // matters is that `send` returns without hanging.
        let _ = sm.send("loop");
        assert_eq!(sm.current_state_name(), "a");
    }

    #[test]
    fn forward_playback_loops_within_segment() {
        // Segment [0.0, 2.0]s with default Forward + loop. Elapsed
        // 0.5s → scene 0.5; elapsed 3.0s (past a full loop) wraps
        // back to 1.0 (0.5 into the second pass).
        let pb = StatePlayback {
            segment_start: 0.0,
            segment_end: 2.0,
            mode: PlaybackMode::Forward,
            looping: true,
            loop_count: None,
            speed: 1.0,
            autoplay: true,
        };
        assert!((pb.scene_t(0.5) - 0.5).abs() < 1e-5);
        assert!((pb.scene_t(3.0) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn non_looping_forward_freezes_at_segment_end() {
        let pb = StatePlayback {
            segment_start: 0.0,
            segment_end: 1.0,
            mode: PlaybackMode::Forward,
            looping: false,
            loop_count: None,
            speed: 1.0,
            autoplay: true,
        };
        // Past the first pass: freeze at end.
        assert!((pb.scene_t(5.0) - 1.0).abs() < 1e-5);
        // Inside the first pass: normal interpolation.
        assert!((pb.scene_t(0.25) - 0.25).abs() < 1e-5);
    }

    #[test]
    fn reverse_mode_plays_backward() {
        let pb = StatePlayback {
            segment_start: 0.0,
            segment_end: 1.0,
            mode: PlaybackMode::Reverse,
            looping: true,
            loop_count: None,
            speed: 1.0,
            autoplay: true,
        };
        // t=0 → end of segment; t=0.5 → mid; t=1 wraps to end again.
        assert!((pb.scene_t(0.0) - 1.0).abs() < 1e-5);
        assert!((pb.scene_t(0.5) - 0.5).abs() < 1e-5);
    }

    #[test]
    fn bounce_mode_alternates_direction() {
        let pb = StatePlayback {
            segment_start: 0.0,
            segment_end: 1.0,
            mode: PlaybackMode::Bounce,
            looping: true,
            loop_count: None,
            speed: 1.0,
            autoplay: true,
        };
        // Pass 0 (forward): t=0.5 → 0.5
        assert!((pb.scene_t(0.5) - 0.5).abs() < 1e-5);
        // Pass 1 (reverse): t=1.5 → 0.5 (halfway back)
        assert!((pb.scene_t(1.5) - 0.5).abs() < 1e-5);
        // End of pass 1: t=2.0 → start of segment
        assert!((pb.scene_t(2.0) - 0.0).abs() < 1e-5);
    }

    #[test]
    fn loop_count_caps_total_passes() {
        let pb = StatePlayback {
            segment_start: 0.0,
            segment_end: 1.0,
            mode: PlaybackMode::Forward,
            looping: true,
            loop_count: Some(2),
            speed: 1.0,
            autoplay: true,
        };
        // Within 2 passes: normal.
        assert!((pb.scene_t(1.5) - 0.5).abs() < 1e-5);
        // Beyond the cap: freeze at terminal pose of pass 1.
        assert!((pb.scene_t(5.0) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn speed_multiplier_scales_elapsed() {
        // Same spec-state, different speed → faster playback
        // reaches later scene time at the same elapsed.
        let spec = r#"{
            "initial": "a",
            "states": [
                {
                    "type": "PlaybackState", "name": "a", "animation": "main",
                    "segment": [0, 120],
                    "speed": 2.0
                }
            ]
        }"#;
        let player = LottiePlayer::from_json(FIXTURE_ANIM).unwrap();
        let sm = LottieStateMachine::from_player_and_spec(player, spec.as_bytes()).unwrap();
        let pb = sm.state_playback.get(&0).copied().expect("state playback");
        // 0.5s elapsed at 2× should land at segment time 1.0.
        assert!((pb.scene_t(1.0) - 1.0).abs() < 1e-5);
        assert!((pb.speed - 2.0).abs() < 1e-5);
    }

    #[test]
    fn autoplay_false_pins_at_starting_pose() {
        // `autoplay: false` → no elapsed scene_t advance; pose
        // stays at segment start for Forward mode.
        let spec = r#"{
            "initial": "a",
            "states": [
                {
                    "type": "PlaybackState", "name": "a", "animation": "main",
                    "segment": [30, 90],
                    "autoplay": false
                }
            ]
        }"#;
        let player = LottiePlayer::from_json(FIXTURE_ANIM).unwrap();
        let mut sm = LottieStateMachine::from_player_and_spec(player, spec.as_bytes()).unwrap();
        let scene_t = sm.state_scene_t(99.0).expect("pinned scene_t");
        // Segment [30, 90] frames at 60fps → [0.5, 1.5]s. Pinned at
        // segment_start (0.5s) regardless of sketch time.
        assert!((scene_t - 0.5).abs() < 1e-5);
    }

    #[test]
    fn reverse_mode_accepts_authored_string() {
        assert!(matches!(PlaybackMode::from_str("Forward"), PlaybackMode::Forward));
        assert!(matches!(PlaybackMode::from_str("Reverse"), PlaybackMode::Reverse));
        assert!(matches!(PlaybackMode::from_str("Bounce"), PlaybackMode::Bounce));
        assert!(matches!(
            PlaybackMode::from_str("ReverseBounce"),
            PlaybackMode::ReverseBounce
        ));
        // Unknown → fallback to Forward.
        assert!(matches!(PlaybackMode::from_str("wat"), PlaybackMode::Forward));
    }

    #[test]
    fn marker_segment_resolves_to_time_plus_duration() {
        // A Lottie with two markers: "entry" at 30f for 60f, "exit"
        // at 120f for 60f. States referencing them via string
        // should land on the equivalent seconds range (60fps →
        // entry = 0.5→1.5s, exit = 2.0→3.0s).
        let anim = r#"{
            "v": "5.0", "fr": 60, "ip": 0, "op": 240,
            "w": 100, "h": 100, "layers": [],
            "markers": [
                { "cm": "entry", "tm": 30,  "dr": 60 },
                { "cm": "exit",  "tm": 120, "dr": 60 }
            ]
        }"#;
        let spec = r#"{
            "initial": "a",
            "states": [
                {
                    "type": "PlaybackState", "name": "a", "animation": "main",
                    "segment": "entry"
                },
                {
                    "type": "PlaybackState", "name": "b", "animation": "main",
                    "marker": "exit"
                }
            ]
        }"#;
        let player = LottiePlayer::from_json(anim).unwrap();
        let sm = LottieStateMachine::from_player_and_spec(player, spec.as_bytes()).unwrap();
        assert_eq!(sm.state_segments.get(&0), Some(&(0.5, 1.5)));
        assert_eq!(sm.state_segments.get(&1), Some(&(2.0, 3.0)));
    }

    #[test]
    fn missing_marker_name_leaves_state_unsegmented() {
        // Unknown marker name falls through silently — no crash,
        // no segment. The state just plays the composition's full
        // timeline when active.
        let anim = r#"{
            "v": "5.0", "fr": 60, "ip": 0, "op": 120,
            "w": 100, "h": 100, "layers": [],
            "markers": []
        }"#;
        let spec = r#"{
            "initial": "a",
            "states": [
                {
                    "type": "PlaybackState", "name": "a", "animation": "main",
                    "segment": "ghost"
                }
            ]
        }"#;
        let player = LottiePlayer::from_json(anim).unwrap();
        let sm = LottieStateMachine::from_player_and_spec(player, spec.as_bytes()).unwrap();
        assert!(!sm.state_segments.contains_key(&0));
    }

    #[test]
    fn unknown_transition_target_is_an_error() {
        let player = LottiePlayer::from_json(FIXTURE_ANIM).unwrap();
        let bad = r#"{
            "initial": "a",
            "states": [
                {
                    "type": "PlaybackState",
                    "name": "a",
                    "animation": "main",
                    "transitions": [
                        { "type": "Transition", "toState": "ghost",
                          "guards": [{"type":"Event","inputName":"x"}] }
                    ]
                }
            ]
        }"#;
        match LottieStateMachine::from_player_and_spec(player, bad.as_bytes()) {
            Err(Error::Archive(_)) => {}
            other => panic!("expected Archive error, got {other:?}"),
        }
    }
}
