//! Secure Input detection.
//!
//! While any app has Secure Event Input on (a focused password field,
//! Terminal's "Secure Keyboard Entry", a password manager that left it stuck),
//! macOS delivers no keyboard events to event taps. The dictation hotkey and
//! the copy-last hotkey then look dead. Nothing can override that, by design,
//! so the app says so instead: a menu line, a dimmed menu-bar glyph with a
//! tooltip, and a field in the IPC `status` document. The control socket
//! (`toggle` / `start` / `stop`) keeps working, because it needs no key event.
//!
//! The poll is one Carbon call that reads a flag; it is cheap and thread-safe.

use std::sync::atomic::{AtomicBool, Ordering};

/// The menu line and tooltip text shown while Secure Input is on.
pub const PAUSED_TEXT: &str = "Hotkey paused: another app has Secure Input on";

/// Last polled value, so readers off the polling thread do not call Carbon.
static ACTIVE: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "macos")]
#[link(name = "Carbon", kind = "framework")]
extern "C" {
    /// `Boolean IsSecureEventInputEnabled(void)` (HIToolbox, CarbonEventsCore.h).
    fn IsSecureEventInputEnabled() -> u8;
}

/// Ask macOS now.
pub fn query() -> bool {
    #[cfg(target_os = "macos")]
    {
        unsafe { IsSecureEventInputEnabled() != 0 }
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Poll and remember. Returns `Some(new_value)` only when the value changed,
/// so the caller updates the UI and logs once per transition.
pub fn refresh() -> Option<bool> {
    note(query())
}

fn note(now: bool) -> Option<bool> {
    transition(&ACTIVE, now)
}

fn transition(cell: &AtomicBool, now: bool) -> Option<bool> {
    (cell.swap(now, Ordering::SeqCst) != now).then_some(now)
}

/// The last polled value.
pub fn active() -> bool {
    ACTIVE.load(Ordering::SeqCst)
}

/// The status line for the menu and tooltip, when there is something to say.
pub fn notice(active: bool) -> Option<&'static str> {
    active.then_some(PAUSED_TEXT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_change_is_reported() {
        let cell = AtomicBool::new(false);
        assert_eq!(transition(&cell, false), None);
        assert_eq!(transition(&cell, true), Some(true));
        assert_eq!(transition(&cell, true), None);
        assert_eq!(transition(&cell, false), Some(false));
    }

    #[test]
    fn the_notice_names_the_cause_in_plain_words() {
        assert_eq!(notice(false), None);
        let text = notice(true).unwrap();
        assert!(text.contains("Secure Input") && text.contains("paused"));
    }

    #[test]
    fn the_query_links_and_answers() {
        // Whatever the machine's state, the call must link and return.
        let _ = query();
    }
}
