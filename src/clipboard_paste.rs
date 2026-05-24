//! Clipboard save → write → Cmd+V → restore. Used as a fallback when the AX
//! injector can't write directly (Electron apps, some Java apps, sandboxed
//! web views all silently refuse `kAXValueAttribute` writes).
//!
//! Limitation: we only save/restore plain-text clipboard contents. RTF,
//! images, file URLs etc. on the clipboard at injection time will be lost.
//! Worth fixing later with NSPasteboard multi-type round-trip; sufficient
//! for v1.

use arboard::Clipboard;
use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation, CGKeyCode};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use std::time::Duration;

// US ANSI virtual key codes.
const KEY_C: CGKeyCode = 8;
const KEY_V: CGKeyCode = 9;
const KEY_RETURN: CGKeyCode = 36;
const KEY_TAB: CGKeyCode = 48;
// Time the OS needs to actually consume the synthesized Cmd+V and let the
// receiving app pull from the pasteboard before we restore. Bumped from the
// original 180 ms: under GPU load right after Gemma inference the target app's
// paste handler can run late, and restoring the clipboard too early made it
// paste stale/empty content — one cause of "records but doesn't paste".
const PASTE_SETTLE_MS: u64 = 260;

/// Build a CGEvent source that does **not** merge the live hardware modifier
/// state into the events we post.
///
/// This is the crux of the intermittent-paste bug: the push-to-talk hotkey is
/// a *modifier* key (Right Option by default). With `HIDSystemState` the
/// synthesized Cmd+V inherits whatever modifiers are physically down at post
/// time, so if Option hasn't fully registered as released yet the event
/// becomes Cmd+Option+V — which is not the paste shortcut, so nothing pastes.
/// `Private` gives us an isolated state table, so the only modifiers on the
/// event are the ones we set explicitly.
fn clean_event_source() -> eyre::Result<CGEventSource> {
    CGEventSource::new(CGEventSourceStateID::Private)
        .map_err(|_| eyre::eyre!("CGEventSource::new failed"))
}

pub fn paste_via_clipboard(text: &str) -> eyre::Result<()> {
    let mut cb = Clipboard::new()
        .map_err(|e| eyre::eyre!("Clipboard::new failed: {e}"))?;

    // Save current plain text (if any). We deliberately ignore errors here —
    // an empty / non-text clipboard is fine, we just won't restore anything.
    let saved = cb.get_text().ok();

    cb.set_text(text)
        .map_err(|e| eyre::eyre!("Clipboard::set_text failed: {e}"))?;

    synthesize_cmd_v()?;

    std::thread::sleep(Duration::from_millis(PASTE_SETTLE_MS));

    if let Some(orig) = saved {
        let _ = cb.set_text(orig);
    } else {
        // If the clipboard was empty before, leave our injected text on it
        // (better than wiping it — gives the user a manual paste fallback).
    }
    Ok(())
}

/// Time the OS needs to consume a synthesized Cmd+C and populate the
/// pasteboard with the selection before we read it back.
const COPY_SETTLE_MS: u64 = 140;

/// Grab the focused app's current selection via a Cmd+C round-trip, returning
/// `None` when nothing is selected. Used by transform mode to read the text the
/// user wants rewritten — universal across AX-blind apps (Electron, terminals)
/// where reading `kAXSelectedText` is unreliable, since Cmd+C always works.
///
/// The original clipboard is saved and restored. A unique sentinel is placed
/// before the copy so an unchanged pasteboard (nothing was selected) is
/// distinguishable from a real empty selection.
pub fn copy_selection_via_clipboard() -> eyre::Result<Option<String>> {
    const SENTINEL: &str = "\u{2063}__dictate_selection_probe__\u{2063}";
    let mut cb = Clipboard::new().map_err(|e| eyre::eyre!("Clipboard::new failed: {e}"))?;
    let saved = cb.get_text().ok();

    cb.set_text(SENTINEL)
        .map_err(|e| eyre::eyre!("Clipboard::set_text(sentinel) failed: {e}"))?;
    synthesize_cmd_c()?;
    std::thread::sleep(Duration::from_millis(COPY_SETTLE_MS));

    let after = cb.get_text().unwrap_or_default();

    // Restore the user's clipboard regardless of outcome.
    match &saved {
        Some(orig) => {
            let _ = cb.set_text(orig);
        }
        None => {
            let _ = cb.set_text("");
        }
    }

    if after == SENTINEL || after.is_empty() {
        Ok(None)
    } else {
        Ok(Some(after))
    }
}

fn synthesize_cmd_c() -> eyre::Result<()> {
    let source = clean_event_source()?;

    let down = CGEvent::new_keyboard_event(source.clone(), KEY_C, true)
        .map_err(|_| eyre::eyre!("Cmd+C keydown create failed"))?;
    down.set_flags(CGEventFlags::CGEventFlagCommand);
    down.post(CGEventTapLocation::HID);

    let up = CGEvent::new_keyboard_event(source, KEY_C, false)
        .map_err(|_| eyre::eyre!("Cmd+C keyup create failed"))?;
    up.set_flags(CGEventFlags::CGEventFlagNull);
    up.post(CGEventTapLocation::HID);

    Ok(())
}

fn synthesize_cmd_v() -> eyre::Result<()> {
    let source = clean_event_source()?;

    let key_down = CGEvent::new_keyboard_event(source.clone(), KEY_V, true)
        .map_err(|_| eyre::eyre!("CGEvent keydown create failed"))?;
    key_down.set_flags(CGEventFlags::CGEventFlagCommand);
    key_down.post(CGEventTapLocation::HID);

    let key_up = CGEvent::new_keyboard_event(source, KEY_V, false)
        .map_err(|_| eyre::eyre!("CGEvent keyup create failed"))?;
    // Clear the command flag on key-up so we don't leave a dangling modifier.
    key_up.set_flags(CGEventFlags::CGEventFlagNull);
    key_up.post(CGEventTapLocation::HID);

    Ok(())
}

/// Synthesize a bare key press + release with no modifier flags.
///
/// Posting with `CGEventFlagNull` is load-bearing: a held push-to-talk
/// modifier must not turn this into e.g. Option-Return (insert newline / no
/// submit) or Option-Tab (app switch). `clean_event_source` plus an explicit
/// null flag guarantee the event carries only the key itself.
fn synthesize_plain_key(key: CGKeyCode) -> eyre::Result<()> {
    let source = clean_event_source()?;

    let down = CGEvent::new_keyboard_event(source.clone(), key, true)
        .map_err(|_| eyre::eyre!("keydown create failed"))?;
    down.set_flags(CGEventFlags::CGEventFlagNull);
    down.post(CGEventTapLocation::HID);

    let up = CGEvent::new_keyboard_event(source, key, false)
        .map_err(|_| eyre::eyre!("keyup create failed"))?;
    up.set_flags(CGEventFlags::CGEventFlagNull);
    up.post(CGEventTapLocation::HID);

    Ok(())
}

/// Synthesize a Return key press + release. Used for trailing voice commands
/// ("press enter", "new line") after the body text is in the field.
pub fn synthesize_return() -> eyre::Result<()> {
    synthesize_plain_key(KEY_RETURN)
}

/// Synthesize a Tab key press + release ("press tab" voice command) — useful
/// for advancing between form fields by voice.
pub fn synthesize_tab() -> eyre::Result<()> {
    synthesize_plain_key(KEY_TAB)
}
