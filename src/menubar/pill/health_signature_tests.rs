use super::health_signature;
use crate::health::{Fault, Problem};

fn problem(fault: Fault) -> Problem {
    Problem {
        fault,
        detail: String::new(),
    }
}

/// Regression: the status item was repainted only on a `UiState`
/// transition, and most health changes do not move `UiState`. The
/// signature is what makes the once-a-second tick notice one.
#[test]
fn every_blocking_fault_has_its_own_signature_and_nothing_else_moves_it() {
    assert_eq!(health_signature(None), 0);

    let mut seen = vec![0u8];
    for fault in [
        Fault::AccessibilityMissing,
        Fault::AccessibilityRelaunch,
        Fault::MicrophoneBlocked,
        Fault::ModelsMissing,
        Fault::EngineFailed,
        Fault::HelperFailed,
        Fault::Booting,
    ] {
        let signature = health_signature(Some(&problem(fault)));
        assert_ne!(signature, 0, "{fault:?} blocks dictation and must show");
        assert!(!seen.contains(&signature), "{fault:?} collides");
        seen.push(signature);
    }

    // The two non-blocking faults never reach the status item, so they
    // must read the same as "nothing wrong" — otherwise an unplugged mic
    // would repaint the menu bar with a danger glyph it does not deserve.
    for fault in [Fault::AudioLost, Fault::CleanupUnavailable] {
        assert_eq!(health_signature(Some(&problem(fault))), 0, "{fault:?}");
    }

    // The whole point: boot → ready is a change the tick can see.
    assert_ne!(
        health_signature(Some(&problem(Fault::Booting))),
        health_signature(None)
    );
}
