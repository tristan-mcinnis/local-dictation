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

/// Synthesize a Return key press + release. Used when the dictated text
/// ends with "press enter" — the daemon strips the trigger phrase and
/// calls this after the cleaned text is in the field.
pub fn synthesize_return() -> eyre::Result<()> {
    let source = clean_event_source()?;

    let down = CGEvent::new_keyboard_event(source.clone(), KEY_RETURN, true)
        .map_err(|_| eyre::eyre!("Return keydown create failed"))?;
    // Post with no modifier flags — a held push-to-talk modifier must not
    // turn this Return into e.g. Option-Return (insert newline / no submit).
    down.set_flags(CGEventFlags::CGEventFlagNull);
    down.post(CGEventTapLocation::HID);

    let up = CGEvent::new_keyboard_event(source, KEY_RETURN, false)
        .map_err(|_| eyre::eyre!("Return keyup create failed"))?;
    up.set_flags(CGEventFlags::CGEventFlagNull);
    up.post(CGEventTapLocation::HID);

    Ok(())
}
