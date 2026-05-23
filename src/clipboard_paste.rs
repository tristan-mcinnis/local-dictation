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
const KEY_V: CGKeyCode = 9;
const KEY_RETURN: CGKeyCode = 36;
// Time the OS needs to actually consume the synthesized Cmd+V and let the
// receiving app pull from the pasteboard before we restore.
const PASTE_SETTLE_MS: u64 = 180;

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

fn synthesize_cmd_v() -> eyre::Result<()> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| eyre::eyre!("CGEventSource::new failed"))?;

    let key_down = CGEvent::new_keyboard_event(source.clone(), KEY_V, true)
        .map_err(|_| eyre::eyre!("CGEvent keydown create failed"))?;
    key_down.set_flags(CGEventFlags::CGEventFlagCommand);
    key_down.post(CGEventTapLocation::HID);

    let key_up = CGEvent::new_keyboard_event(source, KEY_V, false)
        .map_err(|_| eyre::eyre!("CGEvent keyup create failed"))?;
    key_up.set_flags(CGEventFlags::CGEventFlagCommand);
    key_up.post(CGEventTapLocation::HID);

    Ok(())
}

/// Synthesize a Return key press + release. Used when the dictated text
/// ends with "press enter" — the daemon strips the trigger phrase and
/// calls this after the cleaned text is in the field.
pub fn synthesize_return() -> eyre::Result<()> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| eyre::eyre!("CGEventSource::new failed"))?;

    let down = CGEvent::new_keyboard_event(source.clone(), KEY_RETURN, true)
        .map_err(|_| eyre::eyre!("Return keydown create failed"))?;
    down.post(CGEventTapLocation::HID);

    let up = CGEvent::new_keyboard_event(source, KEY_RETURN, false)
        .map_err(|_| eyre::eyre!("Return keyup create failed"))?;
    up.post(CGEventTapLocation::HID);

    Ok(())
}
