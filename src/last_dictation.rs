//! Never lose a dictation: the rules around "the last dictation".
//!
//! Every finished transcript becomes the last dictation, whether it was
//! inserted, held (no text field), failed at the write, produced by a spoken
//! command or a transform, stopped by the recording limit, or recovered from
//! saved audio. When the text did not reach the app it is also put on the
//! clipboard (unless the user turned that off) and the pill says so.
//!
//! The storage is `ui_channel`'s process static. It survives a relaunch by
//! being re-read from the history store at boot ([`seed_from_history`]), so
//! there is no second copy on disk: a user who keeps no text history
//! ("Keep text: Never") gets a memory-only last dictation, and a row deleted
//! from History is not brought back.
//!
//! Plain `std` so the decisions are unit-tested in a featureless build; the
//! clipboard write is passed in as a closure.

use crate::ui_channel::{self, UiState};

/// Why a finished transcript was not inserted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeptCause {
    /// No editable field had focus, or the target app was not frontmost.
    NoEditableField,
    /// The write into the app failed.
    PasteFailed,
    /// The recording hit the maximum length and was stopped without a key.
    LimitReached,
    /// The clipboard route was refused because the user's clipboard holds
    /// content that could not be put back (a promised file, an image past the
    /// snapshot budget). The text is never auto-copied either: that would
    /// destroy the same clipboard the refusal protected.
    ClipboardProtected,
}

/// The pill state for a transcript that was kept instead of inserted.
pub fn kept_state(cause: KeptCause, copied: bool) -> UiState {
    match (cause, copied) {
        (KeptCause::NoEditableField, false) => UiState::NoEditableField,
        (KeptCause::NoEditableField, true) => UiState::HeldCopied,
        (KeptCause::PasteFailed, false) => UiState::PasteFailed,
        (KeptCause::PasteFailed, true) => UiState::PasteFailedCopied,
        (KeptCause::LimitReached, false) => UiState::LimitReached,
        (KeptCause::LimitReached, true) => UiState::LimitReachedCopied,
        (KeptCause::ClipboardProtected, _) => UiState::ClipboardProtected,
    }
}

/// The pill's words for a kept-result state: what happened, then where the
/// text is. `None` for every other state.
pub fn kept_pill_text(state: UiState) -> Option<&'static str> {
    Some(match state {
        UiState::NoEditableField => "No text field · Copy from menu",
        UiState::HeldCopied => "No text field · Copied, press ⌘V",
        UiState::PasteFailed => "Couldn’t paste · Copy from menu",
        UiState::PasteFailedCopied => "Couldn’t paste · Copied, press ⌘V",
        UiState::LimitReached => "Recording limit reached · Copy from menu",
        UiState::LimitReachedCopied => "Recording limit reached · Copied, press ⌘V",
        UiState::ClipboardProtected => "Clipboard kept, not pasted · Copy from menu",
        _ => return None,
    })
}

/// The History "Output" note for a kept transcript.
pub fn kept_stage_note(cause: KeptCause, copied: bool) -> &'static str {
    match (cause, copied) {
        (KeptCause::NoEditableField, true) => "Not inserted (no text field); copied to clipboard",
        (KeptCause::NoEditableField, false) => "Not inserted (no text field)",
        (KeptCause::PasteFailed, true) => "Not inserted (paste failed); copied to clipboard",
        (KeptCause::PasteFailed, false) => "Not inserted",
        (KeptCause::LimitReached, true) => "Not inserted (recording limit); copied to clipboard",
        (KeptCause::LimitReached, false) => "Not inserted (recording limit)",
        (KeptCause::ClipboardProtected, _) => "Not inserted (clipboard content could not be restored)",
    }
}

/// Keep a transcript that did not reach the app: make it the last dictation,
/// copy it when `auto_copy` is on, and return the pill state to show. A failed
/// clipboard write degrades to the "Copy from menu" wording; the text is still
/// the last dictation, so nothing is lost.
pub fn keep(
    text: &str,
    cause: KeptCause,
    auto_copy: bool,
    copy: impl FnOnce(&str) -> Result<(), String>,
) -> UiState {
    ui_channel::set_last_dictation(text);
    let copied = auto_copy
        && cause != KeptCause::ClipboardProtected
        && !text.trim().is_empty()
        && match copy(text) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("[keep] clipboard copy failed: {e}");
                false
            }
        };
    kept_state(cause, copied)
}

/// What "Copy last dictation" did. One code path serves the hotkey, the menu
/// item, and the `copy-last` IPC command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyLastOutcome {
    Copied,
    /// Nothing has been dictated (or history is off and the app relaunched).
    NothingYet,
    Failed,
}

impl CopyLastOutcome {
    /// The pill to show, if any.
    pub fn pill(self) -> Option<UiState> {
        match self {
            CopyLastOutcome::Copied => Some(UiState::Copied),
            CopyLastOutcome::NothingYet => Some(UiState::NothingToCopy),
            CopyLastOutcome::Failed => None,
        }
    }
}

/// Copy `last` with `copy`. Pure apart from the closure.
pub fn copy_last(last: &str, copy: impl FnOnce(&str) -> Result<(), String>) -> CopyLastOutcome {
    if last.trim().is_empty() {
        return CopyLastOutcome::NothingYet;
    }
    match copy(last) {
        Ok(()) => CopyLastOutcome::Copied,
        Err(e) => {
            eprintln!("[copy-last] clipboard copy failed: {e}");
            CopyLastOutcome::Failed
        }
    }
}

/// Texts the history store writes itself; never a dictation.
const PLACEHOLDERS: [&str; 2] = ["(Recording…)", "(Audio retained; transcript expired)"];

/// The newest real transcript among history rows given newest-first.
pub fn newest_transcript<'a>(texts_newest_first: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    texts_newest_first
        .into_iter()
        .find(|t| !t.trim().is_empty() && !PLACEHOLDERS.contains(&t.trim()))
}

/// Boot-time seed from the history store, so "Copy last dictation" still works
/// after a relaunch (every settings change relaunches the daemon). Looks at a
/// few rows because the newest may be a placeholder. Returns whether it seeded.
pub fn seed_from_history() -> bool {
    let rows = crate::history::recent(5);
    match newest_transcript(rows.iter().map(|e| e.text.as_str())) {
        Some(text) => ui_channel::seed_last_dictation(text),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `keep` writes the process-wide last dictation; serialize its tests.
    static KEEP: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        KEEP.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn a_kept_transcript_is_copied_and_the_pill_says_so() {
        let _g = lock();
        let mut got = String::new();
        let state = keep("hello there", KeptCause::NoEditableField, true, |t| {
            got = t.to_string();
            Ok(())
        });
        assert_eq!(got, "hello there");
        assert_eq!(state, UiState::HeldCopied);
        assert_eq!(kept_pill_text(state), Some("No text field · Copied, press ⌘V"));
    }

    #[test]
    fn a_failed_write_keeps_the_text_and_copies_it() {
        let _g = lock();
        // Regression: a hard inject failure used to leave the PREVIOUS text as
        // the last dictation and put nothing on the clipboard.
        let state = keep("the failed one", KeptCause::PasteFailed, true, |_| Ok(()));
        assert_eq!(state, UiState::PasteFailedCopied);
        assert_eq!(ui_channel::last_dictation(), "the failed one");
    }

    #[test]
    fn auto_copy_off_never_touches_the_clipboard() {
        let _g = lock();
        let mut called = false;
        let state = keep("text", KeptCause::PasteFailed, false, |_| {
            called = true;
            Ok(())
        });
        assert!(!called);
        assert_eq!(state, UiState::PasteFailed);
        assert_eq!(kept_pill_text(state), Some("Couldn’t paste · Copy from menu"));
    }

    #[test]
    fn a_clipboard_error_falls_back_to_the_menu_wording() {
        let _g = lock();
        let state = keep("text", KeptCause::LimitReached, true, |_| Err("busy".into()));
        assert_eq!(state, UiState::LimitReached);
    }

    #[test]
    fn empty_text_is_never_copied() {
        let _g = lock();
        let mut called = false;
        let state = keep("  ", KeptCause::NoEditableField, true, |_| {
            called = true;
            Ok(())
        });
        assert!(!called);
        assert_eq!(state, UiState::NoEditableField);
    }

    #[test]
    fn every_kept_state_has_words_and_a_history_note() {
        for cause in [KeptCause::NoEditableField, KeptCause::PasteFailed, KeptCause::LimitReached] {
            for copied in [false, true] {
                let state = kept_state(cause, copied);
                assert!(state.is_kept_result());
                let text = kept_pill_text(state).unwrap();
                assert_eq!(text.contains("Copied"), copied, "{text}");
                assert_eq!(kept_stage_note(cause, copied).contains("copied"), copied);
            }
        }
        assert_eq!(kept_pill_text(UiState::Success), None);
    }

    #[test]
    fn a_protected_clipboard_is_never_overwritten_by_the_auto_copy() {
        let _g = lock();
        // Regression: the refusal to overwrite an unrestorable clipboard fell
        // through to copy-on-paste-failure, which overwrote it anyway.
        let state = keep("kept words", KeptCause::ClipboardProtected, true, |_| {
            panic!("the auto-copy must not touch a protected clipboard")
        });
        assert_eq!(state, UiState::ClipboardProtected);
        assert!(state.is_kept_result());
        assert_eq!(ui_channel::last_dictation(), "kept words");
        assert_eq!(kept_pill_text(state), Some("Clipboard kept, not pasted · Copy from menu"));
        assert!(!kept_stage_note(KeptCause::ClipboardProtected, true).contains("copied"));
    }

    #[test]
    fn copy_last_reports_nothing_yet_without_calling_the_clipboard() {
        let mut called = false;
        let outcome = copy_last("", |_| {
            called = true;
            Ok(())
        });
        assert_eq!(outcome, CopyLastOutcome::NothingYet);
        assert!(!called);
        assert_eq!(outcome.pill(), Some(UiState::NothingToCopy));
    }

    #[test]
    fn copy_last_copies_the_exact_text() {
        let mut got = String::new();
        let outcome = copy_last("Exact  text\nline two", |t| {
            got = t.to_string();
            Ok(())
        });
        assert_eq!(outcome, CopyLastOutcome::Copied);
        assert_eq!(got, "Exact  text\nline two");
        assert_eq!(copy_last("x", |_| Err("no".into())), CopyLastOutcome::Failed);
    }

    #[test]
    fn the_seed_skips_placeholders_and_blank_rows() {
        let rows = ["(Recording…)", "  ", "(Audio retained; transcript expired)", "real words", "older"];
        assert_eq!(newest_transcript(rows), Some("real words"));
        assert_eq!(newest_transcript(["(Recording…)"]), None);
        assert_eq!(newest_transcript([]), None);
    }
}
