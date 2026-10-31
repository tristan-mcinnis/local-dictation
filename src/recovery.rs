//! Recovery paste: re-insert the last dictation into the app that was
//! frontmost *before* the menu opened.
//!
//! A live hold-to-dictate re-resolves the *current* systemwide focus at write
//! time — correct while the user is still holding the key, because the live
//! field is the focus. But a menu-initiated recovery has no such "live" field:
//! clicking the status item and then a row happens over the menu bar, so the
//! frontmost app at inject time is NOT the field the user wants (it is the menu
//! app, a settings window, or whatever happens to be frontmost). The reliable
//! recovery therefore must (a) remember the foreign app that was frontmost
//! before the menu opened, (b) restore (activate) and verify it, and (c) inject
//! into that *specific* app — never into Local Dictation itself (settings / a
//! panel / the menu) and never blindly into whatever is frontmost.
//!
//! The decision logic below is pure `std` so the safety contract is locked down
//! by a deterministic test in a featureless run; the AppKit/AX orchestration
//! (`paste_last_dictation`) lives behind the `ax-inject` feature.

/// Where the recovery paste should go, decided from the recorded pre-menu app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryTarget {
    /// A recovered foreign app. Restore (activate) it, verify it is frontmost,
    /// then inject into its focused field.
    Inject { pid: i32 },
    /// The only app recorded before the menu opened was Local Dictation itself.
    /// Refuse — never paste into our own settings window / a panel / the menu.
    RefuseSelf,
    /// No frontmost app was recorded before the menu opened. Refuse — there is
    /// nothing to restore, and pasting into whatever is frontmost is exactly
    /// the "wrong target" case the recovery is meant to avoid.
    NoTarget,
}

/// Decide the recovery-paste target from the app that was frontmost before the
/// menu opened, and our own process PID.
///
/// `recorded_pid = Some(our_pid)` is the "settings/self" hazard — the user had
/// a Local Dictation window up (or the menu itself became frontmost), so there
/// is no foreign app to recover; the paste must be refused rather than landing
/// in the menu app. `None` is the "no evidence to restore" case — also refused.
pub fn decide_recovery_target(recorded_pid: Option<i32>, self_pid: i32) -> RecoveryTarget {
    match recorded_pid {
        Some(pid) if pid == self_pid => RecoveryTarget::RefuseSelf,
        Some(pid) => RecoveryTarget::Inject { pid },
        None => RecoveryTarget::NoTarget,
    }
}

/// Recovery may write only while no live utterance owns the pipeline. Checked
/// on entry, after app activation, and again by the injector at write commit.
pub fn recovery_may_write(state: crate::ui_channel::UiState, cancel_active: bool) -> bool {
    !state.is_active() && !cancel_active
}

/// "Copy last dictation": the one code path behind the global hotkey, the menu
/// item and the `copy-last` IPC command. A plain clipboard write (clipboard
/// managers keep it), never a paste, never an app activation. The pill confirms
/// it unless a live dictation owns the pill; the copy itself always happens.
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
pub fn copy_last_dictation(source: &str) -> crate::last_dictation::CopyLastOutcome {
    use crate::last_dictation::CopyLastOutcome;
    let outcome = crate::last_dictation::copy_last(&crate::ui_channel::last_dictation(), |text| {
        crate::clipboard_paste::copy_only(text).map_err(|e| e.to_string())
    });
    eprintln!("[copy-last] {source} · {outcome:?}");
    if outcome == CopyLastOutcome::Failed {
        crate::cues::play_error();
    }
    if let Some(state) = outcome.pill() {
        crate::ui_channel::show_transient_if_unowned(state, COPY_LAST_PILL);
    }
    outcome
}

/// Keep a finished transcript that did not reach the app: last dictation,
/// clipboard (when the setting allows), error cue. Returns the pill state for
/// the caller to show under its own generation rules.
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
pub fn keep_transcript(text: &str, cause: crate::last_dictation::KeptCause) -> crate::ui_channel::UiState {
    let auto_copy = crate::settings::Settings::load().resolve_copy_on_paste_failure();
    let state = crate::last_dictation::keep(text, cause, auto_copy, |t| {
        crate::clipboard_paste::copy_only(t).map_err(|e| e.to_string())
    });
    crate::cues::play_error();
    state
}

/// How long the "Copied to clipboard" / "No dictation yet" pill stays up.
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
const COPY_LAST_PILL: std::time::Duration = std::time::Duration::from_millis(1400);

/// Serializes recovery pastes so two "Paste last dictation" clicks can't run
/// concurrently and trip over the same AX / clipboard path. Set true before the
/// worker thread is spawned; the worker clears it on drop.
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
static RECOVERY_BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(all(target_os = "macos", feature = "ax-inject"))]
struct RecoveryBusy;

#[cfg(all(target_os = "macos", feature = "ax-inject"))]
impl Drop for RecoveryBusy {
    fn drop(&mut self) {
        RECOVERY_BUSY.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Restore a foreign app to frontmost and inject the last dictation into it.
///
/// Called from the menu action on the AppKit (main) thread. All the slow work
/// — the activate-app verification, the focused-element AX lookup, and the
/// clipboard Cmd+V + settle — is moved to a background thread so the menu is not
/// frozen. Outcomes are mirrored onto the UI through the existing transients.
///
/// Refuses (never pastes) when: there is nothing to paste, the recorded app is
/// missing / is our own process ([`RecoveryTarget::RefuseSelf`] /
/// [`RecoveryTarget::NoTarget`]), a dictation is in flight, another recovery
/// paste is already running, the app will not come forward (restore+verify
/// fails), or the app has no editable field focused (the injector's focus gate
/// holds it). A global Cmd+V is additionally fail-closed on the explicit target
/// PID (see `clipboard_target_is_frontmost`).
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
pub fn paste_last_dictation(
    recorded_pid: Option<i32>,
    retained_focus: Option<crate::injector::FocusTarget>,
) {
    use crate::injector::{self, AccessibilityInjector, Delivery, InjectRequest};
    use crate::ui_channel::{self, UiState};
    use std::time::Duration;

    let self_pid = std::process::id() as i32;
    let target = match decide_recovery_target(recorded_pid, self_pid) {
        RecoveryTarget::Inject { pid } => pid,
        RecoveryTarget::RefuseSelf => {
            eprintln!("[recovery] refusing to paste into Local Dictation itself");
            return;
        }
        RecoveryTarget::NoTarget => {
            eprintln!("[recovery] no app recorded before the menu opened — refusing");
            return;
        }
    };

    let text = ui_channel::last_dictation();
    if text.trim().is_empty() {
        eprintln!("[recovery] nothing to paste (no last dictation)");
        return;
    }

    // Never run alongside a live dictation — the paste would race the pipeline's
    // own injection / clipboard write.
    if !recovery_may_write(ui_channel::state(), crate::cancel::CANCEL.active()) {
        eprintln!("[recovery] a dictation is in flight — refusing to paste now");
        return;
    }

    // Serialize against a concurrent recovery paste; refuse (don't queue) so we
    // never stack background threads. The busy flag moves into the worker thread
    // and is cleared on drop.
    if RECOVERY_BUSY.swap(true, std::sync::atomic::Ordering::SeqCst) {
        eprintln!("[recovery] another recovery paste is running — refusing");
        return;
    }

    // All slow work off the AppKit loop. The busy guard moves into the
    // worker thread and is held for the duration of the recovery work.
    std::thread::spawn(move || {
        let _busy = RecoveryBusy;
        // Re-check: a dictation may have started between the menu click and now.
        if !recovery_may_write(ui_channel::state(), crate::cancel::CANCEL.active()) {
            eprintln!("[recovery] dictation started mid-recovery — abandoning paste");
            return;
        }

        // Restore the pre-menu app and *verify* it actually comes forward. If it
        // will not (quit, cannot activate, not frontmost), never paste into the
        // wrong target.
        if !injector::activate_app(target) {
            eprintln!(
                "[recovery] could not restore app pid {target} to frontmost — refusing to paste"
            );
            let state = keep_transcript(&text, crate::last_dictation::KeptCause::NoEditableField);
            ui_channel::show_transient_if_unowned(state, ui_channel::KEPT_PILL);
            return;
        }
        if !recovery_may_write(ui_channel::state(), crate::cancel::CANCEL.active()) {
            eprintln!("[recovery] dictation started while restoring target — abandoning paste");
            return;
        }

        // Final race gate: a live utterance that starts after activation but
        // before the first AX/clipboard write wins. Recovery then performs no
        // side effect and publishes no unscoped UI transient.
        let authorize = || recovery_may_write(ui_channel::state(), crate::cancel::CANCEL.active());
        let injection = AccessibilityInjector::inject(
            InjectRequest { pid: Some(target), target: retained_focus, authorize: Some(&authorize) },
            &text,
        );
        match injector::delivery_of(injection, Some(target)) {
            Delivery::Delivered => {
                eprintln!("[recovery] pasted last dictation into pid {target}");
                // A menu action needs explicit feedback even when the normal
                // post-dictation result pill is disabled (the default).
                ui_channel::set_last_inject_pid(Some(target));
                ui_channel::show_transient_if_unowned(UiState::Success, Duration::from_millis(900));
            }
            // Never paste blindly: a missing field, a target that left the
            // front, or a failed write all keep the text.
            Delivery::Held(cause) => {
                let state = keep_transcript(&text, cause);
                ui_channel::show_transient_if_unowned(state, ui_channel::KEPT_PILL);
            }
            Delivery::Cancelled => {
                eprintln!("[recovery] live dictation won the write boundary — paste abandoned");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // The whole point of the recovery guard: a menu-initiated paste must target
    // the app the user was in BEFORE opening the menu, and must refuse to paste
    // into Local Dictation itself or into an unknown target.

    #[test]
    fn recovery_targets_the_recorded_foreign_app() {
        let recorded = Some(4242);
        assert_eq!(
            decide_recovery_target(recorded, 100),
            RecoveryTarget::Inject { pid: 4242 }
        );
    }

    #[test]
    fn recovery_refuses_self() {
        // A dictation followed immediately by opening the menu must NOT make the
        // menu app (ourselves) the paste target. Recorded self → refuse.
        let self_pid = std::process::id() as i32;
        assert_eq!(
            decide_recovery_target(Some(self_pid), self_pid),
            RecoveryTarget::RefuseSelf
        );
        // An explicit self PID (any value equal to self_pid) refuses too.
        assert_eq!(
            decide_recovery_target(Some(7), 7),
            RecoveryTarget::RefuseSelf
        );
    }

    #[test]
    fn recovery_refuses_no_recorded_app() {
        // No pre-menu app recorded → there is nothing to restore; pasting into
        // whatever happens to be frontmost is the "wrong target" case.
        assert_eq!(decide_recovery_target(None, 100), RecoveryTarget::NoTarget);
    }

    #[test]
    fn recovery_write_gate_refuses_every_live_phase() {
        use crate::ui_channel::UiState;
        for state in [
            UiState::Recording,
            UiState::Processing,
            UiState::Transcribing,
            UiState::Cleaning,
            UiState::Pasting,
        ] {
            assert!(!recovery_may_write(state, false), "{state:?}");
        }
        assert!(!recovery_may_write(UiState::Idle, true));
        assert!(recovery_may_write(UiState::Idle, false));
    }
}
