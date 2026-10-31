//! Delivery: one write for every daemon surface, and what happens to text
//! that did not land.
use super::*;

use crate::injector::Delivery;
use crate::last_dictation::KeptCause;

/// Write `text` for `generation` and settle the outcomes every surface
/// shares. A held text is kept (last dictation, clipboard when the setting
/// allows, History note, kept-result pill); a cancelled one plays the cancel
/// cue. The caller handles `Delivered`, which differs per surface.
///
/// `hold` is a reason known before the write (the recording limit): the text
/// is kept without touching the injector. `target` is the field captured at
/// key-press; the injector still resolves the current focus and uses it only
/// as the same-app fallback. The write authorizer claims the cancel commit
/// immediately before the first side effect, and a cancel that landed on a
/// path that never claimed it (an empty write, a hold) still wins here.
pub(super) fn deliver(
    prefs: ResultPrefs,
    generation: u64,
    hold: Option<KeptCause>,
    target: Option<FocusTarget>,
    target_pid: Option<i32>,
    text: &str,
) -> Delivery {
    let authorize = || authorize_paste(generation);
    let delivery = crate::injector::finalize_utterance(false, hold, text, target_pid, |text| {
        AccessibilityInjector::inject(
            InjectRequest { target, authorize: Some(&authorize), ..Default::default() },
            text,
        )
    });
    let delivery = if crate::cancel::CANCEL.requested(generation) {
        Delivery::Cancelled
    } else {
        delivery
    };
    // Capture is over. Un-mute other audio before any cue so the cue is
    // audible; `play_start` ran before `mute()`, and the restore is
    // idempotent, so the arm's `RestoreOnDrop` guard is then a no-op.
    crate::audio_duck::restore();
    match delivery {
        Delivery::Delivered => {}
        Delivery::Held(cause) => keep_and_show(prefs, generation, text, cause),
        Delivery::Cancelled => {
            eprintln!("  ⌫    cancelled · discarded");
            cues::play_cancel();
        }
    }
    delivery
}

/// The History status a delivery settles the capture with.
pub(super) fn recovery_status(delivery: Delivery) -> crate::history::RecoveryStatus {
    use crate::history::RecoveryStatus;
    match delivery {
        Delivery::Delivered => RecoveryStatus::Completed,
        Delivery::Held(KeptCause::PasteFailed) => RecoveryStatus::Failed,
        Delivery::Held(_) => RecoveryStatus::Held,
        Delivery::Cancelled => RecoveryStatus::Cancelled,
    }
}

/// A finished transcript did not reach the app. Make it the last dictation,
/// put it on the clipboard (setting permitting), note it in History, play the
/// error cue and show the kept-result pill for this generation: about four
/// seconds, or until the next key press, with the History button on it.
pub(super) fn keep_and_show(prefs: ResultPrefs, generation: u64, text: &str, cause: KeptCause) {
    let state = crate::recovery::keep_transcript(text, cause, prefs.copy_on_paste_failure);
    let copied = matches!(
        state,
        UiState::HeldCopied | UiState::PasteFailedCopied | UiState::LimitReachedCopied
    );
    crate::history::append_recovery_stage(
        generation,
        crate::history::StageKind::Output,
        text,
        crate::last_dictation::kept_stage_note(cause, copied),
    );
    ui_channel::show_transient_for(generation, state, ui_channel::KEPT_PILL);
}

/// Flash the "Landed · N words" pill, or go straight to Idle when the result
/// pill is off (default). The text landing at the cursor is the real feedback.
pub(super) fn show_result_pill(prefs: ResultPrefs) {
    if prefs.show_result {
        ui_channel::show_transient(
            UiState::Success,
            Duration::from_millis(crate::settings::RESULT_PILL_MS),
        );
    } else {
        ui_channel::set_state(UiState::Idle);
    }
}

/// Claim the existing write gate, then publish Pasting at the actual boundary.
/// A cancellation that beat this claim never presents a late paste stage.
pub(super) fn authorize_paste(generation: u64) -> bool {
    ui_channel::authorize_paste(generation)
}
