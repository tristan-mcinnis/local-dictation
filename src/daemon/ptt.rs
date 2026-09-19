//! Push-to-talk state machine and double-tap detector (pure logic; unit-tested from `daemon::tests`).
use super::*;

/// Accept only a changed physical key state, independently of hold/latch mode.
/// In particular an autorepeated KeyDown is not a tap to stop hands-free.
pub(super) fn hotkey_edge(down: &AtomicBool, is_down: bool) -> bool {
    down.swap(is_down, Ordering::SeqCst) != is_down
}

// Double-tap-to-latch gesture timing. A quick tap (press+release within
// TAP_MAX_HOLD) followed by a second press within DOUBLE_TAP_WINDOW of that
// release arms hands-free — the no-Spacebar route into latch. Detected in the
// event-tap callback (it needs real press/release timing), not the machine.
pub(super) const TAP_MAX_HOLD: Duration = Duration::from_millis(300);
pub(super) const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(350);

/// How long a Space key-down stays eligible to form the hands-free chord across
/// a following PTT edge. The keyboard scans keys in hardware order, not finger
/// order, so Option+Space can arrive Space-first; inside this window the pair is
/// still the chord, and outside it that Space was ordinary typing.
pub(super) const CHORD_WINDOW: Duration = Duration::from_millis(150);

// Hands-free latch state machine, shared with the event-tap callback:
//
//   ST_IDLE ─PTT press─▶ ST_HOLDING ─release─▶ ST_IDLE              (normal PTT)
//                            │ +Space chord
//                            ▼
//                     ST_HOLDING_ARMED ─release─▶ ST_LATCHED
//                                                     │ PTT tap
//                                                     ▼
//                                               ST_STOPPING_TAP ─release─▶ ST_IDLE
//
// In ST_LATCHED the keys are physically up but the mic keeps recording; the
// next PTT tap stops + processes the utterance. The transition logic lives in
// the pure `ptt_transition` fn below so it can be unit-tested without a tap.
pub(super) const ST_IDLE: u8 = 0;
pub(super) const ST_HOLDING: u8 = 1;
pub(super) const ST_HOLDING_ARMED: u8 = 2;
pub(super) const ST_LATCHED: u8 = 3;
pub(super) const ST_STOPPING_TAP: u8 = 4;

/// A normalised hotkey/space event fed to the latch state machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PttInput {
    /// The PTT modifier transitioned to held. `shift` = Shift co-held (the
    /// transform-selection gesture), read only when starting a fresh utterance.
    /// `double_tap` = this is the second press of a quick double-tap (set by the
    /// event-tap callback; only meaningful from IDLE, where it latches).
    HotkeyPress { shift: bool, double_tap: bool },
    /// The PTT modifier transitioned to held with a Space key-down immediately
    /// before it — the same chord, in the order the hardware happened to send
    /// it. From idle this arms hands-free straight away; from latched it is
    /// still a stop tap, so a real tap is never swallowed by a recent Space.
    HotkeyChordSpace,
    /// The PTT modifier transitioned to released.
    HotkeyRelease,
    /// Space went down (only meaningful while the PTT key is held).
    SpaceDown,
}

/// What the state machine wants the caller to do, alongside the new state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PttDecision {
    Nothing,
    /// Begin capture. `transform` carries the Shift-at-press gesture.
    Start {
        transform: bool,
    },
    /// Double-tap from idle: begin hands-free capture AND cue latched (Start +
    /// ArmLatch shrunk into one decision; the no-Spacebar route into ST_LATCHED).
    StartLatched {
        transform: bool,
    },
    /// End capture and run the pipeline.
    Stop,
    /// First Space of a hold: arm hands-free (cue) and swallow the Space.
    ArmLatch,
    /// Subsequent Space while already armed: just swallow it.
    SwallowSpace,
}

/// Pure transition table for the hands-free latch. Given the current state and
/// an input, returns the next state and the side effect to perform. Keeping
/// this free of any CG / channel types makes the whole gesture unit-testable.
pub(super) fn ptt_transition(state: u8, input: PttInput) -> (u8, PttDecision) {
    match (state, input) {
        // Double-tap from idle: go straight to latched so the release of the
        // second press can't stop the session (see ST_LATCHED release arm).
        (ST_IDLE, PttInput::HotkeyPress { double_tap: true, .. }) => {
            (ST_LATCHED, PttDecision::StartLatched { transform: false })
        }
        // Space landed marginally before the modifier edge: the chord, not a
        // bare hold. Arming here is what keeps Option+Space from silently
        // degrading into hold-to-talk and ending the moment the keys come up.
        (ST_IDLE, PttInput::HotkeyChordSpace) => {
            (ST_LATCHED, PttDecision::StartLatched { transform: false })
        }
        (ST_IDLE, PttInput::HotkeyPress { shift, double_tap: false }) => {
            (ST_HOLDING, PttDecision::Start { transform: shift })
        }
        (ST_HOLDING, PttInput::SpaceDown) => (ST_HOLDING_ARMED, PttDecision::ArmLatch),
        (ST_HOLDING_ARMED, PttInput::SpaceDown) => (ST_HOLDING_ARMED, PttDecision::SwallowSpace),
        (ST_HOLDING, PttInput::HotkeyRelease) => (ST_IDLE, PttDecision::Stop),
        // Armed → keys released: stay recording hands-free.
        (ST_HOLDING_ARMED, PttInput::HotkeyRelease) => (ST_LATCHED, PttDecision::Nothing),
        // A PTT tap ends a hands-free session.
        (ST_LATCHED, PttInput::HotkeyPress { .. }) => (ST_STOPPING_TAP, PttDecision::Stop),
        // A recent Space must not swallow a deliberate stop tap.
        (ST_LATCHED, PttInput::HotkeyChordSpace) => (ST_STOPPING_TAP, PttDecision::Stop),
        (ST_STOPPING_TAP, PttInput::HotkeyRelease) => (ST_IDLE, PttDecision::Nothing),
        // Any other combination is a no-op (e.g. stray Space when not holding,
        // re-press while already holding, release in IDLE).
        (s, _) => (s, PttDecision::Nothing),
    }
}

/// Detects a quick double-tap of the PTT key from its press/release spacing,
/// so a remote with no Spacebar can reach hands-free mode. Lives in the
/// event-tap callback (needs real timing); the state machine stays pure.
#[derive(Default)]
pub(super) struct DoubleTapDetector {
    pub(super) last_press: Option<Instant>,
    pub(super) last_release: Option<Instant>,
}

impl DoubleTapDetector {
    /// Record a press; true if this is the *second* press of a double-tap (the
    /// first tap was held < TAP_MAX_HOLD and released < DOUBLE_TAP_WINDOW ago).
    /// Retains `last_press` so a third tap can chain a detection.
    pub(super) fn on_press(&mut self, now: Instant) -> bool {
        let is_double = matches!(
            (self.last_press, self.last_release),
            (Some(p), Some(r))
                if now.saturating_duration_since(r) <= DOUBLE_TAP_WINDOW
                    && r.saturating_duration_since(p) <= TAP_MAX_HOLD
        );
        self.last_press = Some(now);
        is_double
    }

    /// Record a release.
    pub(super) fn on_release(&mut self, now: Instant) {
        self.last_release = Some(now);
    }
}
