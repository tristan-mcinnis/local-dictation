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

/// Where a finished text goes and how its result is shown.
pub(super) struct Destination {
    pub prefs: ResultPrefs,
    pub generation: u64,
    /// A reason known before the write (the recording limit): keep, never write.
    pub hold: Option<KeptCause>,
    /// The field captured at key-press; the injector still resolves the
    /// current focus and uses this only as the same-app fallback.
    pub target: Option<FocusTarget>,
    pub target_pid: Option<i32>,
}

/// One write, for the caller's timing and summary lines.
pub(super) struct Written<'a> {
    pub delivery: Delivery,
    pub text: &'a str,
    pub write_ms: u128,
}

/// Settle a cleaned transcript, the same way for push-to-talk, retry and
/// listen: refine it (corrections, trailing command), then discard, skip,
/// send a bare key, or write it and land the result. `after_write` runs right
/// after the write, before the landed effects, for the caller's timing lines.
pub(super) fn settle(
    refiner: &Refiner,
    cleaned: &str,
    to: Destination,
    after_write: impl FnOnce(&Written),
) -> crate::history::RecoveryStatus {
    use crate::history::RecoveryStatus;
    let status = match refiner.refine(cleaned).outcome() {
        DictationOutcome::Discard => {
            eprintln!("  ⌫    scratch that · discarded");
            RecoveryStatus::Cancelled
        }
        DictationOutcome::BareAction(action) => {
            eprintln!("  ↵    bare command (no body)");
            if authorize_paste(to.generation) {
                execute_trailing_action(&action);
                RecoveryStatus::Completed
            } else {
                RecoveryStatus::Failed
            }
        }
        DictationOutcome::Skip => {
            eprintln!("  skip · cleaned to empty");
            RecoveryStatus::Completed
        }
        DictationOutcome::Inject { text, action } => return write_and_land(to, &text, action, after_write),
    };
    ui_channel::set_state(UiState::Idle);
    status
}

/// Write `text` and land the result: on success the success cue, the last
/// dictation, the Undo target, the result pill, the History row and the
/// trailing key; a held or cancelled text was already settled by `deliver`.
pub(super) fn write_and_land(
    to: Destination,
    text: &str,
    action: TrailingAction,
    after_write: impl FnOnce(&Written),
) -> crate::history::RecoveryStatus {
    let t_write = Instant::now();
    let delivery = deliver(to.prefs, to.generation, to.hold, to.target, to.target_pid, text);
    after_write(&Written { delivery, text, write_ms: ms(t_write.elapsed()) });
    match delivery {
        Delivery::Delivered => land(to.prefs, to.generation, text, "Inserted", action),
        Delivery::Held(_) => {}
        Delivery::Cancelled => ui_channel::set_state(UiState::Idle),
    }
    recovery_status(delivery)
}

/// What a landed text leaves behind: the success cue, the last dictation for
/// the menu, the Undo target, the result pill, the History row (`note`) and
/// the trailing key.
pub(super) fn land(prefs: ResultPrefs, generation: u64, text: &str, note: &str, action: TrailingAction) {
    eprintln!("  ✓    {}", crate::daemon_log::text(text));
    cues::play_success();
    // Stored BEFORE the pill shows so the word count is this text's, not the
    // previous one's.
    ui_channel::set_last_dictation(text);
    // The write was just verified against the frontmost app; the pill's Undo
    // is only valid while that app stays in front.
    ui_channel::set_last_inject_pid(crate::injector::frontmost_pid());
    show_result_pill(prefs);
    crate::history::append_recovery_stage(generation, crate::history::StageKind::Output, text, note);
    if !matches!(action, TrailingAction::None) {
        // Clipboard insertion returns after queuing Cmd+V. Keep Enter/Tab
        // behind its consume/restore window so a slow app cannot submit the
        // previous field text.
        crate::clipboard_paste::wait_for_pending_transaction();
        std::thread::sleep(Duration::from_millis(40));
        execute_trailing_action(&action);
    }
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

/// Run a trailing voice command's keystroke(s). `None`/`Cancel` are no-ops
/// (the caller handles `Cancel` earlier). Errors are logged, never fatal.
pub(super) fn execute_trailing_action(action: &TrailingAction) {
    use crate::clipboard_paste::{
        synthesize_escape, synthesize_return, synthesize_tab, synthesize_undo,
    };
    if matches!(action, TrailingAction::None | TrailingAction::Cancel) { return; }
    // Shared by ordinary dictation, retries, listen mode and bare commands.
    // No trailing key may overtake a pending clipboard consume/restore window.
    let result = crate::clipboard_paste::after_pending_transaction(|| match action {
        TrailingAction::None | TrailingAction::Cancel => Ok(()),
        TrailingAction::PressEnter => synthesize_return(),
        TrailingAction::NewParagraph => {
            // Two Returns, with a beat between so apps that debounce key
            // repeat still register both.
            let first = synthesize_return();
            std::thread::sleep(Duration::from_millis(15));
            first.and(synthesize_return())
        }
        TrailingAction::PressTab => synthesize_tab(),
        TrailingAction::PressEscape => synthesize_escape(),
        TrailingAction::Undo => synthesize_undo(),
    });
    match (action, result) {
        (_, Ok(())) => eprintln!("  ↵    {action:?} sent"),
        (_, Err(e)) => eprintln!("  ✗    {action:?} failed: {e:?}"),
    }
}

