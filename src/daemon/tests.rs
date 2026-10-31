use super::*;

#[test]
fn an_intel_mac_is_told_plainly_instead_of_failing_somewhere_deeper() {
    // The bundle is arm64-only and the Swift MLX helper refuses to build
    // anywhere else, so an x86 run used to end as a Finder "can't open
    // this application" or a link error inside a build script.
    assert_eq!(architecture_problem("aarch64"), None);
    for arch in ["x86_64", "x86", "powerpc"] {
        let problem = architecture_problem(arch).expect(arch);
        assert!(problem.contains("Apple Silicon"), "{problem}");
        assert!(problem.contains(arch), "{problem}");
    }
    // This build itself must be on a supported machine.
    assert_eq!(unsupported_architecture(), None);
}

#[test]
fn a_limit_stop_resets_a_stuck_hold_so_the_next_press_starts_cleanly() {
    // Regression (PB-F06): a lost key-up left ST_HOLDING + hotkey_down set;
    // the next real press was then dropped as a duplicate edge.
    let pending = AtomicBool::new(true);
    let state = AtomicU8::new(ST_HOLDING);
    let down = AtomicBool::new(true);
    assert!(apply_ptt_resync(&pending, &state, &down));
    assert_eq!(state.load(Ordering::SeqCst), ST_IDLE);
    assert!(hotkey_edge(&down, true), "the next press is a fresh edge");
    let (next, decision) = ptt_transition(
        state.load(Ordering::SeqCst),
        PttInput::HotkeyPress { shift: false, double_tap: false },
    );
    assert_eq!((next, decision), (ST_HOLDING, PttDecision::Start { transform: false }));
    // Applied once only.
    state.store(ST_LATCHED, Ordering::SeqCst);
    assert!(!apply_ptt_resync(&pending, &state, &down));
    assert_eq!(state.load(Ordering::SeqCst), ST_LATCHED);
}

#[test]
fn a_key_still_held_at_the_limit_releases_as_a_no_op() {
    let pending = AtomicBool::new(true);
    let state = AtomicU8::new(ST_HOLDING);
    let down = AtomicBool::new(true);
    apply_ptt_resync(&pending, &state, &down);
    assert!(!hotkey_edge(&down, false), "release after resync is not an edge");
}

#[test]
fn recording_limit_is_off_at_zero_and_exact_at_the_minute() {
    assert!(!recording_limit_reached(Duration::from_secs(86_400), 0));
    assert!(!recording_limit_reached(Duration::from_secs(599), 10));
    assert!(recording_limit_reached(Duration::from_secs(600), 10));
}

#[test]
fn repeated_second_press_of_double_tap_cannot_stop_hands_free() {
    let down = AtomicBool::new(false);
    let mut state = ST_IDLE;
    let mut decisions = Vec::new();
    // First tap, second press, autorepeats while held, then release.
    for (pressed, double_tap) in [
        (true, false), (false, false), (true, true),
        (true, false), (true, false), (false, false),
    ] {
        if hotkey_edge(&down, pressed) {
            let input = if pressed {
                PttInput::HotkeyPress { shift: false, double_tap }
            } else {
                PttInput::HotkeyRelease
            };
            let (next, decision) = ptt_transition(state, input);
            state = next;
            decisions.push(decision);
        }
    }
    assert_eq!(state, ST_LATCHED);
    assert_eq!(decisions, vec![
        PttDecision::Start { transform: false },
        PttDecision::Stop,
        PttDecision::StartLatched { transform: false },
        PttDecision::Nothing,
    ]);
    // A genuinely new tap still ends capture.
    assert!(hotkey_edge(&down, true));
    assert_eq!(
        ptt_transition(state, PttInput::HotkeyPress { shift: false, double_tap: false }),
        (ST_STOPPING_TAP, PttDecision::Stop)
    );
}

#[test]
fn releasing_right_option_while_left_option_is_held_keeps_hands_free_recording() {
    // The second press of a double-tap has already latched recording.
    // Its release carries aggregate Option because Left Option is still down.
    let flags = CGEventFlags::from_bits_retain(crate::hotkeys::OPTION | 0x20);
    let input = if crate::hotkeys::modifier_key_is_down(0x3D, flags.bits()) {
        PttInput::HotkeyPress { shift: false, double_tap: false }
    } else {
        PttInput::HotkeyRelease
    };
    assert_eq!(ptt_transition(ST_LATCHED, input), (ST_LATCHED, PttDecision::Nothing));
}

#[test]
fn space_landing_before_the_modifier_edge_still_arms_hands_free() {
    // The keyboard put Space on the wire before the Option edge, so the pair
    // arrives in that order. It is still the chord and must arm.
    let (state, decision) = ptt_transition(ST_IDLE, PttInput::HotkeyChordSpace);
    assert_eq!(state, ST_LATCHED);
    assert_eq!(decision, PttDecision::StartLatched { transform: false });
    // Releasing the keys leaves hands-free running, which is exactly what
    // the chord promises and what hold-to-talk would break.
    assert_eq!(
        ptt_transition(state, PttInput::HotkeyRelease),
        (ST_LATCHED, PttDecision::Nothing)
    );
    // A recent Space must never swallow a deliberate stop tap.
    assert_eq!(
        ptt_transition(state, PttInput::HotkeyChordSpace),
        (ST_STOPPING_TAP, PttDecision::Stop)
    );
}

/// Drive the state machine through a sequence of inputs, returning the
/// (state, decision) at each step plus the final state. Mirrors exactly
/// what the event-tap callback does: feed input, store next state.
fn run(inputs: &[PttInput]) -> (Vec<PttDecision>, u8) {
    let mut state = ST_IDLE;
    let mut decisions = Vec::new();
    for &input in inputs {
        let (next, decision) = ptt_transition(state, input);
        state = next;
        decisions.push(decision);
    }
    (decisions, state)
}

#[test]
fn normal_ptt_press_and_release() {
    // Hold then release: start on press, stop on release, back to idle.
    let (decisions, end) = run(&[
        PttInput::HotkeyPress { shift: false, double_tap: false },
        PttInput::HotkeyRelease,
    ]);
    assert_eq!(
        decisions,
        vec![PttDecision::Start { transform: false }, PttDecision::Stop]
    );
    assert_eq!(end, ST_IDLE);
}

#[test]
fn shift_press_starts_transform() {
    let (decisions, _) = run(&[PttInput::HotkeyPress { shift: true, double_tap: false }]);
    assert_eq!(decisions, vec![PttDecision::Start { transform: true }]);
}

#[test]
fn hands_free_full_gesture() {
    // Hold PTT, chord Space, release both, then tap PTT to stop.
    let (decisions, end) = run(&[
        PttInput::HotkeyPress { shift: false, double_tap: false }, // Start
        PttInput::SpaceDown,                    // ArmLatch (cue + swallow)
        PttInput::HotkeyRelease,                // → Latched, keep recording
        PttInput::HotkeyPress { shift: false, double_tap: false }, // tap → Stop
        PttInput::HotkeyRelease,                // → Idle, no-op
    ]);
    assert_eq!(
        decisions,
        vec![
            PttDecision::Start { transform: false },
            PttDecision::ArmLatch,
            PttDecision::Nothing,
            PttDecision::Stop,
            PttDecision::Nothing,
        ]
    );
    assert_eq!(end, ST_IDLE);
}

#[test]
fn release_order_space_before_ptt_still_latches() {
    // Arming only needs the Space chord while holding; releasing the PTT
    // afterwards latches regardless of when Space physically came up.
    let (decisions, end) = run(&[
        PttInput::HotkeyPress { shift: false, double_tap: false },
        PttInput::SpaceDown,
        PttInput::HotkeyRelease,
    ]);
    assert_eq!(decisions.last(), Some(&PttDecision::Nothing));
    assert_eq!(end, ST_LATCHED);
}

#[test]
fn repeated_space_while_armed_is_swallowed_not_re_cued() {
    // A second Space during the same hold must still be swallowed, but
    // emits SwallowSpace (no fresh cue) rather than ArmLatch.
    let (decisions, end) = run(&[
        PttInput::HotkeyPress { shift: false, double_tap: false },
        PttInput::SpaceDown,
        PttInput::SpaceDown,
    ]);
    assert_eq!(
        decisions,
        vec![
            PttDecision::Start { transform: false },
            PttDecision::ArmLatch,
            PttDecision::SwallowSpace,
        ]
    );
    assert_eq!(end, ST_HOLDING_ARMED);
}

#[test]
fn stray_space_when_idle_is_passed_through() {
    // Space with no PTT held must NOT be swallowed (Nothing) so normal
    // typing is unaffected.
    let (decisions, end) = run(&[PttInput::SpaceDown]);
    assert_eq!(decisions, vec![PttDecision::Nothing]);
    assert_eq!(end, ST_IDLE);
}

#[test]
fn space_during_latched_session_passes_through() {
    // Once hands-free, the keys are up; a Space is a real keystroke and
    // must not be swallowed or change state.
    let (decisions, end) = run(&[
        PttInput::HotkeyPress { shift: false, double_tap: false },
        PttInput::SpaceDown,
        PttInput::HotkeyRelease, // Latched
        PttInput::SpaceDown,     // real space
    ]);
    assert_eq!(decisions.last(), Some(&PttDecision::Nothing));
    assert_eq!(end, ST_LATCHED);
}

#[test]
fn re_press_while_holding_is_noop() {
    // A spurious second press event while already holding does nothing
    // (and never starts a second recording).
    let (decisions, end) = run(&[
        PttInput::HotkeyPress { shift: false, double_tap: false },
        PttInput::HotkeyPress { shift: false, double_tap: false },
    ]);
    assert_eq!(
        decisions,
        vec![
            PttDecision::Start { transform: false },
            PttDecision::Nothing
        ]
    );
    assert_eq!(end, ST_HOLDING);
}

#[test]
fn normal_then_handsfree_back_to_back() {
    // A plain hold/release, then a hands-free session — state returns to
    // idle cleanly between them.
    let (_decisions, end) = run(&[
        PttInput::HotkeyPress { shift: false, double_tap: false },
        PttInput::HotkeyRelease, // Stop → Idle
        PttInput::HotkeyPress { shift: false, double_tap: false },
        PttInput::SpaceDown,
        PttInput::HotkeyRelease, // Latched
        PttInput::HotkeyPress { shift: false, double_tap: false },
        PttInput::HotkeyRelease, // Stop → Idle
    ]);
    assert_eq!(end, ST_IDLE);
}

// Double-tap to latch. The first press is a normal quick tap (Start, then
// Stop on release); it's the *second* press (double_tap=true) that latches.

#[test]
fn double_tap_from_idle_latches() {
    let (decisions, end) = run(&[
        PttInput::HotkeyPress { shift: false, double_tap: false }, // tap 1
        PttInput::HotkeyRelease,
        PttInput::HotkeyPress { shift: false, double_tap: true }, // tap 2 → latch
        PttInput::HotkeyRelease, // keys up, stays recording
    ]);
    assert_eq!(
        decisions,
        vec![
            PttDecision::Start { transform: false },
            PttDecision::Stop,
            PttDecision::StartLatched { transform: false },
            PttDecision::Nothing,
        ]
    );
    assert_eq!(end, ST_LATCHED);
}

#[test]
fn double_tap_then_single_tap_stops() {
    let (decisions, end) = run(&[
        PttInput::HotkeyPress { shift: false, double_tap: false },
        PttInput::HotkeyRelease,
        PttInput::HotkeyPress { shift: false, double_tap: true }, // latch
        PttInput::HotkeyRelease,
        PttInput::HotkeyPress { shift: false, double_tap: false }, // stop tap
        PttInput::HotkeyRelease,
    ]);
    assert_eq!(
        decisions,
        vec![
            PttDecision::Start { transform: false },
            PttDecision::Stop,
            PttDecision::StartLatched { transform: false },
            PttDecision::Nothing,
            PttDecision::Stop,
            PttDecision::Nothing,
        ]
    );
    assert_eq!(end, ST_IDLE);
}

#[test]
fn single_quick_tap_does_not_latch() {
    let (decisions, end) = run(&[
        PttInput::HotkeyPress { shift: false, double_tap: false },
        PttInput::HotkeyRelease,
    ]);
    assert_eq!(
        decisions,
        vec![PttDecision::Start { transform: false }, PttDecision::Stop]
    );
    assert_eq!(end, ST_IDLE);
}

#[test]
fn hold_then_release_does_not_latch() {
    // A single long hold (double_tap is never set, since there's no second
    // press) is just a normal PTT utterance — never hands-free.
    let (_decisions, end) = run(&[
        PttInput::HotkeyPress { shift: false, double_tap: false },
        PttInput::HotkeyRelease,
    ]);
    assert_eq!(end, ST_IDLE);
}

#[test]
fn double_tap_while_latched_stops() {
    // A press while already hands-free ends the session; the double-tap flag
    // is ignored once latched (ST_LATCHED → Stop on the first press, the
    // rest is a no-op).
    let (decisions, end) = run(&[
        PttInput::HotkeyPress { shift: false, double_tap: false },
        PttInput::SpaceDown,
        PttInput::HotkeyRelease, // latched via Space chord
        PttInput::HotkeyPress { shift: false, double_tap: true },
        PttInput::HotkeyRelease,
    ]);
    assert!(decisions.contains(&PttDecision::Stop));
    assert_eq!(end, ST_IDLE);
}

#[test]
fn triple_tap_latches_then_stops() {
    // Three quick taps: the first is a normal utterance, the second latches
    // hands-free, the third stops it — net return to idle.
    let (_decisions, end) = run(&[
        PttInput::HotkeyPress { shift: false, double_tap: false },
        PttInput::HotkeyRelease,
        PttInput::HotkeyPress { shift: false, double_tap: true }, // latch
        PttInput::HotkeyRelease,
        PttInput::HotkeyPress { shift: false, double_tap: true }, // stop tap
        PttInput::HotkeyRelease,
    ]);
    assert_eq!(end, ST_IDLE);
}

#[test]
fn every_landed_text_gets_its_feedback_from_one_place() {
    // Regression: retry, listen and spoken commands each copied the settle
    // stage and dropped the success cue, the result pill and the Undo target,
    // so the pill's Undo pointed at the previous dictation's app.
    const LANDED_EFFECTS: [&str; 5] = [
        "cues::play_success",
        "set_last_inject_pid",
        "show_result_pill(",
        "StageKind::Output, text, note",
        "DictationOutcome::Inject",
    ];
    let files = [
        ("capture.rs", include_str!("capture.rs")),
        ("listen.rs", include_str!("listen.rs")),
        ("recover.rs", include_str!("recover.rs")),
        ("release.rs", include_str!("release.rs")),
        ("tail.rs", include_str!("tail.rs")),
        ("transcribe.rs", include_str!("transcribe.rs")),
        ("transform.rs", include_str!("transform.rs")),
        ("worker.rs", include_str!("worker.rs")),
    ];
    let mut bad = Vec::new();
    for (name, src) in files {
        for effect in LANDED_EFFECTS {
            if src.contains(effect) {
                bad.push(format!("{name}: {effect} -> call deliver::settle, write_and_land or land"));
            }
        }
    }
    assert!(bad.is_empty(), "landed-text effects outside deliver.rs: {bad:?}");
    let deliver = include_str!("deliver.rs");
    assert!(LANDED_EFFECTS.iter().all(|e| deliver.contains(e)));
}
