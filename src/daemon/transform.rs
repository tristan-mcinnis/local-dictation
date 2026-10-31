//! Transform mode (Shift + push-to-talk) and spoken prefix commands: the
//! utterance is an instruction, run on the warm instruction model.
use super::*;

#[cfg(feature = "cleaner")]
fn show_rewrite_failure(generation: u64) {
    ui_channel::show_transient_for(generation, UiState::RewriteFailed, FAILURE_PILL);
}

/// Transform mode: read the focused app's current selection, apply the spoken
/// `instruction` to it with the warm Gemma engine, and paste the result over
/// the selection. Read + replace both go through the clipboard so it works in
/// AX-blind apps. All failures are logged + cue'd; never fatal.
#[cfg(feature = "cleaner")]
pub(super) fn handle_transform(pipeline: &Pipeline, instruction: &str, generation: u64) -> crate::history::RecoveryStatus {
    use crate::history::{RecoveryStatus, StageKind};
    let Some(cleaner) = pipeline.cleaner.as_ref() else {
        eprintln!("  ✗    transform needs the cleanup engine (running with --no-cleanup)");
        cues::play_error();
        return RecoveryStatus::Failed;
    };
    if !crate::cancel::CANCEL.allows_progress(generation) { return RecoveryStatus::Cancelled; }
    if let Some(problem) = cleaner.instruction_problem() {
        eprintln!("[rewrite] unavailable: {problem}; selection unchanged");
        show_rewrite_failure(generation);
        cues::play_error();
        return RecoveryStatus::Failed;
    }
    let expected = crate::injector::frontmost_pid();
    let selection = match crate::clipboard_paste::copy_selection_via_clipboard() {
        Ok(Some(s)) => s,
        Ok(None) => {
            eprintln!("  ✗    transform: nothing selected — select text first");
            show_rewrite_failure(generation);
            cues::play_error();
            return RecoveryStatus::Failed;
        }
        Err(e) => {
            eprintln!("  ✗    transform: couldn't read selection: {e:?}");
            show_rewrite_failure(generation);
            cues::play_error();
            return RecoveryStatus::Failed;
        }
    };
    eprintln!(
        "  ✎    {}  ·  {} chars selected",
        crate::daemon_log::text(instruction.trim()),
        selection.chars().count()
    );

    if !ui_channel::set_phase(generation, UiState::Cleaning) { return RecoveryStatus::Cancelled; }
    let t = Instant::now();
    let result = match pipeline.rt.block_on(cleaner.transform(instruction, &selection)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("  ✗    transform failed: {e:?}");
            show_rewrite_failure(generation);
            cues::play_error();
            return RecoveryStatus::Failed;
        }
    };
    let result = result.trim();
    if result.is_empty() {
        eprintln!("  ✗    transform produced no output; selection left unchanged");
        show_rewrite_failure(generation);
        cues::play_error();
        return RecoveryStatus::Failed;
    }

    // The transformed text must land in the app the selection came from. Snapshot
    // the frontmost app and require it at the global Cmd+V (fail-closed).
    crate::history::append_recovery_stage(generation, StageKind::Transform, result, "Selection rewrite");
    ui_channel::publish_preview(generation, result);
    if !authorize_paste(generation) { return RecoveryStatus::Cancelled; }
    match crate::clipboard_paste::paste_via_clipboard(result, expected) {
        Ok(crate::clipboard_paste::PasteOutcome::Delivered) => {
            eprintln!("  ✓    transformed in {} ms", ms(t.elapsed()));
            // A rewrite is a finished transcript too: "Copy last dictation"
            // must return it, not the dictation before it.
            ui_channel::set_last_dictation(result);
            ui_channel::set_last_inject_pid(expected);
            crate::history::append_recovery_stage(generation, StageKind::Output, result, "Selection replaced");
            RecoveryStatus::Completed
        },
        Ok(crate::clipboard_paste::PasteOutcome::RefusedForeground) => {
            eprintln!("  ◌    transform refused — target app not frontmost at paste (text kept) app={}", crate::injector::app_label_for_pid(expected));
            keep_and_show(pipeline.result, generation, result, crate::last_dictation::KeptCause::NoEditableField);
            RecoveryStatus::Held
        }
        Err(e) => {
            eprintln!("  ✗    transform paste failed: {e:?}");
            keep_and_show(pipeline.result, generation, result, crate::last_dictation::KeptCause::PasteFailed);
            RecoveryStatus::Failed
        }
    }
}

/// Spoken prefix command (e.g. "translate to Chinese …"): run the resolved
/// transform on the spoken body with the warm Gemma engine and inject the result
/// at the cursor, like a normal dictation. Unlike `handle_transform` there's no
/// selection and no clipboard round-trip — the body is fresh speech, so we
/// inject straight into the focus target (with the usual AX/clipboard fallback).
/// Cleanup and refinement are deliberately skipped: the output is already the
/// final text (often a non-English translation), and the corrections dictionary
/// + trailing-command parse are English-prose transforms that don't apply.
#[cfg(feature = "cleaner")]
pub(super) fn handle_spoken_command(
    pipeline: &Pipeline,
    cmd: crate::spoken_command::SpokenCommand,
    captured_target: Option<FocusTarget>,
    generation: u64,
) -> crate::history::RecoveryStatus {
    use crate::history::{RecoveryStatus, StageKind};
    if !ui_channel::set_phase(generation, UiState::Cleaning) { return RecoveryStatus::Cancelled; }
    eprintln!("  ⌘    {} · {} chars", cmd.label, cmd.body.chars().count());
    let t = Instant::now();
    let Some(cleaner) = pipeline.cleaner.as_ref() else {
        return RecoveryStatus::Failed;
    };
    let result = match pipeline.rt.block_on(cleaner.transform(&cmd.instruction, &cmd.body)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("  ✗    command failed: {e:?}");
            show_rewrite_failure(generation);
            cues::play_error();
            return RecoveryStatus::Failed;
        }
    };
    let result = result.trim();
    if result.is_empty() {
        eprintln!("  ✗    command produced no output");
        show_rewrite_failure(generation);
        cues::play_error();
        return RecoveryStatus::Failed;
    }

    crate::history::append_recovery_stage(generation, StageKind::Transform, result, &cmd.label);
    ui_channel::publish_preview(generation, result);
    let target_pid = captured_target.as_ref().and_then(|t| t.inner_pid());
    let delivery = deliver(pipeline.result, generation, None, captured_target, target_pid, result);
    if delivery == Delivery::Delivered {
        eprintln!("  ✓    {} ms → {}", ms(t.elapsed()), crate::daemon_log::text(result));
        ui_channel::set_last_dictation(result);
        crate::history::append_recovery_stage(generation, StageKind::Output, result, "Inserted");
    }
    recovery_status(delivery)
}
