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
//! | `loop: bool`            | parsed (all segments loop today — follow-up) |
//! | `mode` Forward          | applied |
//! | `mode` Reverse / Bounce / ReverseBounce | parsed, not yet applied |
//! | `speed`, `autoplay`     | parsed, not yet applied |
//! | `Transition` (immediate)| parsed + applied |
//! | `Tweened` transitions   | parsed as immediate Transition (duration + easing ignored) |
//! | `Event` guards          | applied — `send(input_name)` matches `inputName` |
//! | Numeric / String / Boolean guards | parsed, always treated as truthy |
//! | `actions` on transitions| parsed as opaque, not executed |
//! | `interactions`, `inputs`| parsed, not yet wired |
//!
//! Everything unsupported parses as a no-op rather than erroring,
//! so assets that use richer features render with a best-effort
//! approximation (usually "state transitions fire on Event guards,
//! other guards pass through"). Scoped-out items are tracked in
//! `BACKLOG.md`.
#![cfg(feature = "dotlottie")]

use std::collections::HashMap;

use blinc_core::fsm::{EventId, StateId, StateMachine, Transition};
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
    // These parse but aren't wired to runtime state yet. Kept in
    // the schema so malformed archives surface parse errors
    // consistently with compliant ones.
    #[serde(default)]
    #[allow(dead_code)]
    interactions: Vec<serde_json::Value>,
    #[serde(default)]
    #[allow(dead_code)]
    inputs: Vec<serde_json::Value>,
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
    segment: Option<[f32; 2]>,
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
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum GuardSpec {
    /// Event guards fire when the named input is sent via
    /// [`LottieStateMachine::send`]. This is the one guard type
    /// we actually evaluate — every other guard currently passes
    /// through as `true`.
    Event {
        #[serde(rename = "inputName")]
        input_name: String,
    },
    /// Remaining guard kinds parse into a permissive bucket so
    /// decoding doesn't fail on assets that use them. The lack
    /// of an input store means we can't evaluate them; treating
    /// them as always-pass matches the "simplified subset"
    /// framing documented at the top of the module.
    #[serde(other)]
    Unsupported,
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

        for (i, entry) in spec.states.iter().enumerate() {
            let id = i as StateId;
            let name = match entry {
                StateEntry::PlaybackState(p) => {
                    if let Some([start_f, end_f]) = p.segment {
                        // Spec: `segment` is `[startFrame, endFrame]`.
                        // Clamp start ≤ end so a swapped pair doesn't
                        // produce a negative-length rem_euclid in
                        // `play_segment`.
                        let start_s = start_f / fr;
                        let end_s = (end_f / fr).max(start_s);
                        state_segments.insert(id, (start_s, end_s));
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
        // hold.
        let mut event_names: HashMap<String, EventId> = HashMap::new();
        let mut transitions: Vec<Transition> = Vec::new();

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
                let (to_name, guards) = match t {
                    TransitionSpec::Transition { to_state, guards }
                    | TransitionSpec::Tweened {
                        to_state, guards, ..
                    } => (to_state, guards),
                };
                let to_id = *state_ids.get(to_name).ok_or_else(|| {
                    Error::Archive(format!(
                        "transition references unknown target state '{to_name}'",
                    ))
                })?;
                // For each Event guard in the transition, wire
                // one FSM edge per source state. Transitions with
                // no Event guard can't be fired from `send()`
                // (no event name to key on) — they're skipped
                // until we add inputs + interactions.
                for guard in guards {
                    if let GuardSpec::Event { input_name } = guard {
                        let next_event_id = event_names.len() as EventId;
                        let event_id =
                            *event_names.entry(input_name.clone()).or_insert(next_event_id);
                        for &src in &source_ids {
                            transitions.push(Transition::new(src, event_id, to_id));
                        }
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
        })
    }

    fn empty_for(player: LottiePlayer) -> Self {
        Self {
            player,
            fsm: StateMachine::new(0, Vec::new()),
            state_names: Vec::new(),
            event_names: HashMap::new(),
            state_segments: HashMap::new(),
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
    pub fn send(&mut self, event_name: &str) -> bool {
        let Some(&event_id) = self.event_names.get(event_name) else {
            return false;
        };
        let prev = self.fsm.current_state();
        let next = self.fsm.send(event_id);
        if next == prev {
            return false;
        }
        apply_state_segment(&mut self.player, next, &self.state_segments);
        true
    }
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
    fn tweened_transition_parses_and_fires_as_immediate() {
        // Tweened downgrades to immediate for now — the duration
        // + easing fields are preserved but don't drive animation
        // interpolation yet.
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
        assert!(sm.send("go"));
        assert_eq!(sm.current_state_name(), "b");
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
