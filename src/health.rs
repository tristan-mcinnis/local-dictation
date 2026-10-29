//! What is wrong with the app right now — the channel beside `ui_channel`.
//!
//! `UiState` is a `Copy` `repr(u8)` enum that describes one utterance's
//! progress. It cannot carry a reason string, and a boot failure is not a
//! stage of an utterance, so health gets its own home here.
//!
//! Three writers raise faults: the daemon's permission checks (Accessibility,
//! Microphone), the worker thread (models missing, engine dead, helper dead,
//! mic stream lost), and the panic hook installed by [`install_panic_hook`].
//! Two readers consume them: the menu bar / pill, and the Setup window.
//!
//! The rule the app must keep is simple: **never look idle while dictation
//! cannot work.** A fault is raised the moment it is known and cleared the
//! moment it is fixed, and [`current`] always returns the one the user should
//! act on first.
//!
//! Plain `std` (a mutex and a small vector) so it compiles in every feature
//! configuration, exactly like `ui_channel`.

use std::sync::Mutex;

/// One thing that is wrong, ordered by what the user must fix first. Derived
/// `Ord` is the priority: the lowest discriminant wins in [`current`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Fault {
    /// Accessibility is not granted, so no event tap and no hotkey.
    AccessibilityMissing = 0,
    /// Accessibility was turned off while running and is back on, but the event
    /// tap made at boot may not recover: only a relaunch restores the hotkey.
    AccessibilityRelaunch = 1,
    /// Microphone access is denied, restricted, or not yet decided.
    MicrophoneBlocked = 2,
    /// A model file the engine needs is not on disk.
    ModelsMissing = 3,
    /// The worker could not start, or it stopped (error or panic).
    EngineFailed = 4,
    /// The speech helper died and could not be restarted.
    HelperFailed = 5,
    /// The microphone stream failed mid-session; it is rebuilt on next press.
    AudioLost = 6,
    /// The cleanup model could not load. Dictation still works: the raw
    /// transcript is used, so this is a degradation, not a blocker.
    CleanupUnavailable = 7,
    /// Models are still loading. Lowest priority: any real fault replaces it
    /// in [`current`], and it clears itself when the worker is ready.
    Booting = 8,
}

impl Fault {
    /// Short line for the menu bar and the pill. Says what is wrong, never how
    /// it is implemented.
    pub fn headline(self) -> &'static str {
        match self {
            Fault::AccessibilityMissing => "Accessibility is off",
            Fault::AccessibilityRelaunch => "Relaunch to restore the hotkey",
            Fault::MicrophoneBlocked => "Microphone access is off",
            Fault::ModelsMissing => "Speech model is missing",
            Fault::EngineFailed => "Dictation engine stopped",
            Fault::HelperFailed => "Speech helper stopped",
            Fault::AudioLost => "Microphone disconnected",
            Fault::CleanupUnavailable => "Cleanup is unavailable",
            Fault::Booting => "Starting up…",
        }
    }

    /// The single next thing the user can do about it.
    pub fn next_step(self) -> &'static str {
        match self {
            Fault::AccessibilityMissing => "Open Setup & permissions to turn it on.",
            Fault::AccessibilityRelaunch => "Accessibility is back on; relaunch Local Dictation.",
            Fault::MicrophoneBlocked => "Open Setup & permissions to turn it on.",
            Fault::ModelsMissing => "Open Setup & permissions to get the model.",
            Fault::EngineFailed => "Open the log, then relaunch from the menu.",
            Fault::HelperFailed => "Open the log, then relaunch from the menu.",
            Fault::AudioLost => "Check the microphone, then press the key again.",
            Fault::CleanupUnavailable => "Dictation works; the raw transcript is used.",
            Fault::Booting => "The hotkey works as soon as the models are loaded.",
        }
    }

    /// True while dictation cannot produce text at all. `AudioLost` is not
    /// blocking: the next press rebuilds the stream.
    pub fn blocks_dictation(self) -> bool {
        !matches!(self, Fault::AudioLost | Fault::CleanupUnavailable)
    }
}

/// A raised fault and the detail that goes with it (a path, an error string).
/// The detail is for the log, the Setup window and a tooltip; the pill shows
/// the headline and the next step.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Problem {
    pub fault: Fault,
    pub detail: String,
}

impl Problem {
    pub fn headline(&self) -> &'static str {
        self.fault.headline()
    }

    pub fn next_step(&self) -> &'static str {
        self.fault.next_step()
    }

    /// One line for the pill: what happened, then what to do.
    pub fn pill_line(&self) -> String {
        format!("{} · {}", self.headline(), self.next_step())
    }
}

/// Every fault currently raised, at most one per [`Fault`] kind.
static FAULTS: Mutex<Vec<Problem>> = Mutex::new(Vec::new());

/// Serializes the tests that mutate the process-wide fault store — here and in
/// `permissions`, which raises `AccessibilityMissing`. Without it those tests
/// race each other on a parallel test runner.
#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

// ─── Writer side ────────────────────────────────────────────────────────

/// Raise a fault, replacing any previous detail for the same kind. Logged the
/// first time it appears, and whenever the detail changes, so the log shows
/// the transition rather than a per-poll flood.
pub fn raise(fault: Fault, detail: impl Into<String>) {
    let detail = detail.into();
    let Ok(mut faults) = FAULTS.lock() else {
        return;
    };
    match faults.iter_mut().find(|p| p.fault == fault) {
        Some(existing) if existing.detail == detail => return,
        Some(existing) => existing.detail = detail.clone(),
        None => faults.push(Problem {
            fault,
            detail: detail.clone(),
        }),
    }
    drop(faults);
    eprintln!("[health] {fault:?} · {detail}");
}

/// Clear one fault. A no-op when it was not raised.
pub fn clear(fault: Fault) {
    let Ok(mut faults) = FAULTS.lock() else {
        return;
    };
    let before = faults.len();
    faults.retain(|p| p.fault != fault);
    let changed = faults.len() != before;
    drop(faults);
    if changed {
        eprintln!("[health] {fault:?} cleared");
    }
}

// ─── Reader side ────────────────────────────────────────────────────────

/// The fault the user should act on first, or `None` when everything is well.
pub fn current() -> Option<Problem> {
    FAULTS
        .lock()
        .ok()
        .and_then(|faults| faults.iter().min_by_key(|p| p.fault).cloned())
}

/// Is one specific fault raised?
pub fn is_raised(fault: Fault) -> bool {
    FAULTS
        .lock()
        .map(|faults| faults.iter().any(|p| p.fault == fault))
        .unwrap_or(false)
}

/// True when nothing at all is raised, including `Booting`.
pub fn ready() -> bool {
    FAULTS.lock().map(|f| f.is_empty()).unwrap_or(true)
}

/// True when a hotkey press right now cannot produce text. The menu bar and
/// the press handler use this to show the reason instead of a dead key.
pub fn blocked() -> bool {
    FAULTS
        .lock()
        .map(|faults| faults.iter().any(|p| p.fault.blocks_dictation()))
        .unwrap_or(false)
}

// ─── Panic hook ─────────────────────────────────────────────────────────

/// Route a panic on any thread into the daemon log and the health channel.
///
/// Without this, a worker-thread panic left the menu bar on Idle with a dead
/// hotkey and nothing in the log but the default panic text. The previous hook
/// still runs, so the standard message and any backtrace are kept.
///
/// The panic payload can hold user text, so only the panic *location* and the
/// payload's first line go into the fault detail; the full message is left to
/// the default hook, which writes to the same private log.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let where_ = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "unknown location".to_string());
        let thread = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_string();
        eprintln!("[panic] on thread `{thread}` at {where_}");
        raise(Fault::EngineFailed, format!("panic at {where_}"));
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The faults store is process-wide, so the scenarios run in one test
    /// rather than racing each other across parallel test threads.
    #[test]
    fn the_user_is_always_pointed_at_the_fault_to_fix_first() {
        let _serial = test_lock();
        for fault in [
            Fault::AccessibilityMissing,
            Fault::AccessibilityRelaunch,
            Fault::MicrophoneBlocked,
            Fault::ModelsMissing,
            Fault::EngineFailed,
            Fault::HelperFailed,
            Fault::AudioLost,
            Fault::CleanupUnavailable,
            Fault::Booting,
        ] {
            clear(fault);
        }
        assert!(ready());
        assert!(!blocked());
        assert_eq!(current(), None);

        // Boot: visible, but not something the user must fix.
        raise(Fault::Booting, "loading models");
        assert_eq!(current().unwrap().fault, Fault::Booting);
        assert!(blocked(), "the hotkey does nothing while the models load");

        // A real fault outranks Booting whatever order they arrive in.
        raise(Fault::EngineFailed, "cleanup model not found");
        assert_eq!(current().unwrap().fault, Fault::EngineFailed);

        // Permission problems come first: they are what the user can act on.
        raise(Fault::MicrophoneBlocked, "denied");
        assert_eq!(current().unwrap().fault, Fault::MicrophoneBlocked);
        raise(Fault::AccessibilityMissing, "not trusted");
        assert_eq!(current().unwrap().fault, Fault::AccessibilityMissing);

        // Granting Accessibility uncovers the next problem, never "all clear".
        clear(Fault::AccessibilityMissing);
        assert_eq!(current().unwrap().fault, Fault::MicrophoneBlocked);
        assert!(is_raised(Fault::EngineFailed));

        // Re-raising the same kind replaces the detail, never duplicates it.
        raise(Fault::MicrophoneBlocked, "still denied");
        assert_eq!(current().unwrap().detail, "still denied");
        clear(Fault::MicrophoneBlocked);
        assert!(!is_raised(Fault::MicrophoneBlocked));

        clear(Fault::EngineFailed);
        clear(Fault::Booting);
        assert!(ready());
        assert!(!blocked());

        // A worker panic used to leave the menu bar idle and the hotkey dead,
        // with nothing but the default panic text in the log. The hook turns
        // it into a visible fault. Same test, because the store is
        // process-wide and a second test thread would race it.
        install_panic_hook();
        std::thread::Builder::new()
            .name("health-panic-probe".into())
            .spawn(|| panic!("probe"))
            .unwrap()
            .join()
            .expect_err("the probe thread must panic");
        let raised = current().expect("a panic must be visible as a fault");
        assert_eq!(raised.fault, Fault::EngineFailed);
        assert!(raised.detail.starts_with("panic at"), "{raised:?}");
        assert!(blocked());
        clear(Fault::EngineFailed);
    }

    #[test]
    fn a_lost_microphone_stream_is_reported_without_declaring_the_app_dead() {
        // Regression: a cpal stream error used to be log-only. It is a fault,
        // but the next key press rebuilds the stream, so it must not read as
        // "dictation is impossible".
        assert!(!Fault::AudioLost.blocks_dictation());
        // Same rule for a missing cleanup model: the raw transcript still
        // lands, so the app must not present itself as unusable (CP-11).
        assert!(!Fault::CleanupUnavailable.blocks_dictation());
        for fault in [
            Fault::AccessibilityMissing,
            Fault::AccessibilityRelaunch,
            Fault::MicrophoneBlocked,
            Fault::ModelsMissing,
            Fault::EngineFailed,
            Fault::HelperFailed,
            Fault::Booting,
        ] {
            assert!(fault.blocks_dictation(), "{fault:?}");
        }
    }

    #[test]
    fn every_fault_says_what_is_wrong_and_what_to_do_next() {
        for fault in [
            Fault::AccessibilityMissing,
            Fault::AccessibilityRelaunch,
            Fault::MicrophoneBlocked,
            Fault::ModelsMissing,
            Fault::EngineFailed,
            Fault::HelperFailed,
            Fault::AudioLost,
            Fault::CleanupUnavailable,
            Fault::Booting,
        ] {
            assert!(!fault.headline().is_empty(), "{fault:?}");
            assert!(!fault.next_step().is_empty(), "{fault:?}");
            let line = Problem {
                fault,
                detail: String::new(),
            }
            .pill_line();
            assert!(line.contains(fault.headline()) && line.contains(fault.next_step()));
        }
    }
}
