//! dotLottie state-machine wrapper.
//!
//! [dotLottie](https://dotlottie.io/state-machines) extends the plain
//! Lottie JSON format with a sibling `state_machine.json` that defines
//! named states pointing at segments of the master animation plus
//! event-triggered transitions between them. Common use case:
//! interactive icons (hover/press/idle) shipped as one archive with
//! all interaction states baked in, rather than wired in application
//! code frame-by-frame.
//!
//! This wrapper composes:
//! - a [`crate::LottiePlayer`] for the actual rendering + timeline
//!   evaluation,
//! - a [`blinc_core::fsm::StateMachine`] for the graph +
//!   transition semantics (the same primitive Blinc's other
//!   animation / keyboard-chain code uses),
//! - a small intermediate schema that decodes `state_machine.json`
//!   into the numeric `StateId` / `EventId` space Blinc's FSM
//!   consumes.
//!
//! On every [`Self::send`] the wrapper resolves the transition through
//! the inner FSM and — if the state changed — updates the player's
//! segment + seeks to the segment start so the new pose plays from
//! its authored first frame.
//!
//! # Schema
//!
//! The `state_machine.json` schema this loader understands:
//!
//! ```json
//! {
//!   "initial_state": "idle",
//!   "states": [
//!     { "name": "idle",    "segment": { "start": 0.0, "end": 1.0 } },
//!     { "name": "hover",   "segment": { "start": 1.0, "end": 2.0 } },
//!     { "name": "pressed", "segment": { "start": 2.0, "end": 2.5 } }
//!   ],
//!   "transitions": [
//!     { "from": "idle",    "to": "hover",   "event": "pointer.enter" },
//!     { "from": "hover",   "to": "idle",    "event": "pointer.leave" },
//!     { "from": "hover",   "to": "pressed", "event": "pointer.click" }
//!   ]
//! }
//! ```
//!
//! Segment times are in **seconds**, not frames — the decoder
//! converts frame-based schemas at a higher layer if / when needed.
//! `states[].segment` is optional: missing means "unconstrained
//! playback on the full composition" (useful for a single pause
//! state that displays any pose). `states[].loop` isn't modelled
//! yet — all segments loop — but non-looping states are on the
//! follow-up list.
//!
//! This schema is deliberately narrower than the full dotLottie
//! state-machine spec (no guards, no triggered actions, no nested
//! machines). Those layer in cleanly on top of `blinc_core::
//! StateMachine`'s guard + action hooks when real assets need them.
#![cfg(feature = "dotlottie")]

use std::collections::HashMap;

use blinc_core::fsm::{EventId, StateId, StateMachine, Transition};
use serde::Deserialize;

use crate::{Error, LottiePlayer};

/// Raw JSON schema for a dotLottie state-machine file. Private —
/// callers see only the resolved [`LottieStateMachine`].
#[derive(Debug, Deserialize)]
struct StateMachineSpec {
    initial_state: String,
    #[serde(default)]
    states: Vec<StateSpec>,
    #[serde(default)]
    transitions: Vec<TransitionSpec>,
}

#[derive(Debug, Deserialize)]
struct StateSpec {
    name: String,
    #[serde(default)]
    segment: Option<SegmentSpec>,
}

#[derive(Debug, Deserialize, Clone, Copy)]
struct SegmentSpec {
    start: f32,
    end: f32,
}

#[derive(Debug, Deserialize)]
struct TransitionSpec {
    from: String,
    to: String,
    event: String,
}

/// A [`LottiePlayer`] driven by a state machine decoded from
/// `state_machine.json`. Own-the-player composition: the wrapper
/// delegates playback/rendering to the inner player (exposed via
/// [`Self::player`] / [`Self::player_mut`]) and drives state
/// transitions itself.
pub struct LottieStateMachine {
    player: LottiePlayer,
    fsm: StateMachine,
    /// Numeric `StateId` ↔ author-facing name. Positional — index
    /// in this vec equals the state's ID. Used to expose the
    /// current state name and to resolve transitions at parse.
    state_names: Vec<String>,
    /// Event name → `EventId`. Stable across calls; events not in
    /// this map are silently dropped by [`Self::send`].
    event_names: HashMap<String, EventId>,
    /// Per-state segment (seconds) picked up on every transition.
    /// States without an entry here fall through to full-timeline
    /// playback via [`LottiePlayer::clear_segment`].
    state_segments: HashMap<StateId, (f32, f32)>,
}

impl LottieStateMachine {
    /// Load a dotLottie archive (`.lottie` bytes) and wire up the
    /// embedded state machine, if any. Returns the resulting
    /// wrapper; if the archive doesn't include a `state_machine.json`
    /// the result still drives a valid player but has no transitions
    /// — [`Self::send`] becomes a no-op.
    pub fn from_dotlottie_bytes(src: &[u8]) -> Result<Self, Error> {
        let (animation, state_machine) = crate::dotlottie::extract_archive(src)?;
        let player = LottiePlayer::from_bytes(&animation)?;
        match state_machine {
            Some(sm_bytes) => Self::from_player_and_spec(player, &sm_bytes),
            None => Ok(Self::empty_for(player)),
        }
    }

    /// Build a state machine from an already-loaded player plus a
    /// raw `state_machine.json` byte slice. Lets callers shuttle
    /// the archive around in their own format (e.g. pre-extracted
    /// animations over the network) without going through
    /// [`Self::from_dotlottie_bytes`].
    pub fn from_player_and_spec(
        mut player: LottiePlayer,
        spec_json: &[u8],
    ) -> Result<Self, Error> {
        let spec: StateMachineSpec = serde_json::from_slice(spec_json)?;
        // Build the name → StateId map by walking `states`.
        // Order-preserving so a state's StateId is stable across
        // subsequent resolution — the FSM's `current_state()` getter
        // returns these numeric IDs and we round-trip them back
        // through the `state_names` vec for display.
        let mut state_names = Vec::with_capacity(spec.states.len());
        let mut state_ids: HashMap<String, StateId> = HashMap::new();
        let mut state_segments: HashMap<StateId, (f32, f32)> = HashMap::new();
        for (i, s) in spec.states.iter().enumerate() {
            let id = i as StateId;
            state_names.push(s.name.clone());
            state_ids.insert(s.name.clone(), id);
            if let Some(seg) = s.segment {
                state_segments.insert(id, (seg.start, seg.end));
            }
        }

        // Events get IDs assigned on first occurrence, same pattern
        // as `state_ids` — deterministic order over the transition
        // list so re-parses produce stable IDs across runs.
        let mut event_names: HashMap<String, EventId> = HashMap::new();
        let mut transitions: Vec<Transition> = Vec::with_capacity(spec.transitions.len());
        for t in &spec.transitions {
            let from = *state_ids.get(&t.from).ok_or_else(|| {
                Error::Archive(format!(
                    "state_machine transition references unknown source state '{}'",
                    t.from
                ))
            })?;
            let to = *state_ids.get(&t.to).ok_or_else(|| {
                Error::Archive(format!(
                    "state_machine transition references unknown target state '{}'",
                    t.to
                ))
            })?;
            let next_id = event_names.len() as EventId;
            let event_id = *event_names.entry(t.event.clone()).or_insert(next_id);
            transitions.push(Transition::new(from, event_id, to));
        }

        let initial_id = *state_ids.get(&spec.initial_state).ok_or_else(|| {
            Error::Archive(format!(
                "state_machine initial_state '{}' not in states list",
                spec.initial_state
            ))
        })?;
        let fsm = StateMachine::new(initial_id, transitions);

        // Apply the initial state's segment to the player so the
        // first `draw_at` call renders the authored entry pose
        // rather than the whole composition.
        apply_state_segment(&mut player, initial_id, &state_segments);

        Ok(Self {
            player,
            fsm,
            state_names,
            event_names,
            state_segments,
        })
    }

    /// Fallback for archives without a `state_machine.json` — every
    /// state-machine query is a no-op but the inner player still
    /// renders normally. Private because callers who don't need the
    /// FSM behaviour can just use [`LottiePlayer::from_dotlottie_bytes`]
    /// directly.
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

    /// Mutable borrow of the inner player. Useful for subscribing
    /// to markers or pausing playback without routing the call
    /// through the state machine.
    pub fn player_mut(&mut self) -> &mut LottiePlayer {
        &mut self.player
    }

    /// Name of the currently-active state, or empty string if the
    /// archive didn't carry a state machine.
    pub fn current_state_name(&self) -> &str {
        let id = self.fsm.current_state();
        self.state_names
            .get(id as usize)
            .map(String::as_str)
            .unwrap_or("")
    }

    /// Dispatch an event to the state machine. Returns `true` when
    /// the event existed in the transition table *and* changed the
    /// current state; `false` otherwise (unknown event, or event
    /// known but no matching transition from the current state).
    ///
    /// When the state changes the player's segment and playback
    /// head are both reset so the new pose begins at the segment
    /// start rather than whatever phase the sketch clock was in.
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

fn apply_state_segment(
    player: &mut LottiePlayer,
    state: StateId,
    state_segments: &HashMap<StateId, (f32, f32)>,
) {
    use blinc_canvas_kit::Player;
    match state_segments.get(&state) {
        Some(&(start, end)) => {
            player.play_segment(start, end);
            // Seek so the state enters at its segment's own start
            // rather than inheriting whatever sketch-clock phase
            // the previous state happened to be in.
            player.seek(start);
        }
        None => player.clear_segment(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A minimal valid Lottie composition — 60 fps, 3 seconds long,
    // no layers. `LottiePlayer` renders it as a noop but the player
    // math still runs, which is all state-machine tests need.
    const FIXTURE_ANIM: &str = r#"{
        "v": "5.0",
        "fr": 60,
        "ip": 0,
        "op": 180,
        "w": 100,
        "h": 100,
        "layers": []
    }"#;

    fn state_machine_spec() -> &'static str {
        r#"{
            "initial_state": "idle",
            "states": [
                { "name": "idle",    "segment": { "start": 0.0, "end": 1.0 } },
                { "name": "hover",   "segment": { "start": 1.0, "end": 2.0 } },
                { "name": "pressed", "segment": { "start": 2.0, "end": 3.0 } }
            ],
            "transitions": [
                { "from": "idle",    "to": "hover",   "event": "pointer.enter" },
                { "from": "hover",   "to": "idle",    "event": "pointer.leave" },
                { "from": "hover",   "to": "pressed", "event": "pointer.click" }
            ]
        }"#
    }

    #[test]
    fn initial_state_segment_applied_on_construction() {
        let player = LottiePlayer::from_json(FIXTURE_ANIM).unwrap();
        let sm = LottieStateMachine::from_player_and_spec(player, state_machine_spec().as_bytes())
            .unwrap();
        assert_eq!(sm.current_state_name(), "idle");
        assert_eq!(sm.player().segment(), Some((0.0, 1.0)));
    }

    #[test]
    fn send_known_event_transitions_state() {
        let player = LottiePlayer::from_json(FIXTURE_ANIM).unwrap();
        let mut sm =
            LottieStateMachine::from_player_and_spec(player, state_machine_spec().as_bytes())
                .unwrap();
        assert!(sm.send("pointer.enter"));
        assert_eq!(sm.current_state_name(), "hover");
        assert_eq!(sm.player().segment(), Some((1.0, 2.0)));
        assert!(sm.send("pointer.click"));
        assert_eq!(sm.current_state_name(), "pressed");
    }

    #[test]
    fn send_unknown_event_returns_false_and_keeps_state() {
        let player = LottiePlayer::from_json(FIXTURE_ANIM).unwrap();
        let mut sm =
            LottieStateMachine::from_player_and_spec(player, state_machine_spec().as_bytes())
                .unwrap();
        assert!(!sm.send("nonsense"));
        assert_eq!(sm.current_state_name(), "idle");
    }

    #[test]
    fn send_event_with_no_transition_from_current_state_returns_false() {
        let player = LottiePlayer::from_json(FIXTURE_ANIM).unwrap();
        let mut sm =
            LottieStateMachine::from_player_and_spec(player, state_machine_spec().as_bytes())
                .unwrap();
        // `pointer.click` is only valid from `hover`; from `idle`
        // it should be a known event with no matching transition.
        assert!(!sm.send("pointer.click"));
        assert_eq!(sm.current_state_name(), "idle");
    }

    #[test]
    fn initial_state_missing_from_states_list_is_an_error() {
        let player = LottiePlayer::from_json(FIXTURE_ANIM).unwrap();
        let bad = r#"{
            "initial_state": "ghost",
            "states": [ { "name": "idle" } ],
            "transitions": []
        }"#;
        let result = LottieStateMachine::from_player_and_spec(player, bad.as_bytes());
        match result {
            Err(Error::Archive(_)) => {}
            Ok(_) => panic!("expected archive error"),
            Err(e) => panic!("expected archive error, got {e}"),
        }
    }

    #[test]
    fn transition_referencing_unknown_state_is_an_error() {
        let player = LottiePlayer::from_json(FIXTURE_ANIM).unwrap();
        let bad = r#"{
            "initial_state": "idle",
            "states": [ { "name": "idle" } ],
            "transitions": [ { "from": "idle", "to": "ghost", "event": "x" } ]
        }"#;
        let result = LottieStateMachine::from_player_and_spec(player, bad.as_bytes());
        match result {
            Err(Error::Archive(_)) => {}
            Ok(_) => panic!("expected archive error"),
            Err(e) => panic!("expected archive error, got {e}"),
        }
    }
}
