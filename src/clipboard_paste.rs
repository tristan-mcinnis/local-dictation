//! Clipboard save → write → Cmd+V → restore. Used as a fallback when the AX
//! injector can't write directly (Electron apps, some Java apps, sandboxed
//! web views all silently refuse `kAXValueAttribute` writes).
//!
//! Our own write is marked transient (nspasteboard.org): every clipboard
//! manager that honours the convention — Quick Launch's Clipboard History
//! included — then skips the dictated text instead of recording every
//! sentence the user speaks. See [`declared_write_types`].
//!
//! The save/restore is a full multi-type, multi-item NSPasteboard round-trip:
//! RTF, images and file URLs on the clipboard at injection time survive a
//! dictation. What is still lost is listed on [`is_restorable_type`] and
//! [`SNAPSHOT_BUDGET_BYTES`]: lazily promised file data (the promising app is
//! the only one that can produce it) and anything past the snapshot byte
//! budget. We also guard the restore with the pasteboard's `changeCount` so a
//! user copying something during the paste-settle window isn't clobbered (see
//! [`paste_via_clipboard`]).

use arboard::Clipboard;
use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation, CGKeyCode};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use std::sync::Mutex;
use std::time::Duration;

/// Outcome of a clipboard paste attempt. The paste is a GLOBAL Cmd+V that lands
/// in the OS-frontmost app, so the delivery guard decides whether it may fire.
pub enum PasteOutcome {
    /// The text was set and Cmd+V was posted to the expected frontmost app.
    Delivered,
    /// The expected target was not frontmost at the moment of delivery, so the
    /// Cmd+V was NOT posted (fail-closed — it would land in the wrong app).
    /// The caller holds the transcript recoverable and never pastes blindly.
    RefusedForeground,
}

// US ANSI virtual key codes.
const KEY_C: CGKeyCode = 8;
const KEY_V: CGKeyCode = 9;
const KEY_Z: CGKeyCode = 6;
const KEY_RETURN: CGKeyCode = 36;
const KEY_TAB: CGKeyCode = 48;
const KEY_ESCAPE: CGKeyCode = 53;
// Time the OS needs to actually consume the synthesized Cmd+V and let the
// receiving app pull from the pasteboard before we restore. Under GPU load
// right after Gemma inference the target app's paste handler can run late, and
// restoring the clipboard too early makes it paste stale/empty/partial content
// — the main cause of "records but doesn't paste everything".
//
// The wait is *adaptive*: a fixed delay that's fine for a short phrase is too
// short for a multi-paragraph dictation (more text → the receiving app's paste
// handler takes longer to pull + render before our restore clobbers the
// pasteboard). Scale a base delay by text length, capped so we never stall the
// hot path unreasonably.
const PASTE_SETTLE_BASE_MS: u64 = 280;
const PASTE_SETTLE_MAX_MS: u64 = 1000;
/// Sanity cap on the `DICTATE_PASTE_SETTLE_MS` override so a bad value can't
/// stall the hot path indefinitely.
const PASTE_SETTLE_ENV_CAP_MS: u64 = 5000;

/// Length-scaled settle time before restoring the clipboard: `+1 ms per ~3
/// chars` on top of the base, clamped to the max. Pure — unit-tested.
fn scaled_paste_settle_ms(text_chars: usize) -> u64 {
    (PASTE_SETTLE_BASE_MS + (text_chars as u64) / 3).min(PASTE_SETTLE_MAX_MS)
}

/// Resolve the effective paste-settle duration. `DICTATE_PASTE_SETTLE_MS`
/// overrides the length-scaled default (set it to `0` if you never touch an
/// Electron/Chromium target and want minimum latency; raise it for paste-heavy
/// apps). Reading env per call is cheap and lets a change apply on the next
/// utterance.
fn paste_settle_ms(text_chars: usize) -> u64 {
    let default = scaled_paste_settle_ms(text_chars);
    match std::env::var("DICTATE_PASTE_SETTLE_MS") {
        Ok(v) => v.trim().parse::<u64>().map_or(default, |ms| ms.min(PASTE_SETTLE_ENV_CAP_MS)),
        Err(_) => default,
    }
}

/// Serialises every exclusive pasteboard mutation by this process. A clipboard
/// restore (run on a detached thread via [`restore_clipboard`]) must never
/// interleave with a fresh paste's `set_text → Cmd+V` critical section — if it
/// did, a restore could clobber the next dictation's text before its Cmd+V
/// consumed it. Two concurrent restores must likewise not interleave (last one
/// wins, never a torn write). The paste critical section and every restore hold
/// this same lock, so the two kinds of write are fully ordered.
static CLIPBOARD_WRITE_MUTEX: Mutex<()> = Mutex::new(());

/// A deliberate recovery copy: serialize with any pending pasteboard restore,
/// write plain text, and never synthesize keys or activate a target app.
pub fn copy_only(text: &str) -> eyre::Result<()> {
    let _guard = CLIPBOARD_WRITE_MUTEX.lock().map_err(|_| eyre::eyre!("clipboard write lock unavailable"))?;
    let mut clipboard = Clipboard::new().map_err(|e| eyre::eyre!("clipboard unavailable: {e}"))?;
    clipboard.set_text(text.to_owned()).map_err(|e| eyre::eyre!("clipboard copy failed: {e}"))
}

/// True when the pasteboard's `changeCount` advanced past our own write — so we
/// must NOT restore our snapshot (it would clobber a user's fresh copy). A
/// `None` on either side keeps the old unconditional-replace behaviour. Pure —
/// unit-tested.
fn user_copied_during_settle(our_count: Option<isize>, now_count: Option<isize>) -> bool {
    matches!((our_count, now_count), (Some(a), Some(b)) if b != a)
}

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

/// The general pasteboard's `changeCount` — bumps whenever anything writes to
/// the clipboard. We read it right after our own write, then again before
/// restoring, to detect a user (or another app) copying something during the
/// paste-settle window. `None` if the read fails (then we fall back to the old
/// unconditional restore).
fn pasteboard_change_count() -> Option<isize> {
    use objc2_app_kit::NSPasteboard;
    // NSPasteboard is thread-safe; changeCount is a plain read.
    let pb = NSPasteboard::generalPasteboard();
    Some(pb.changeCount())
}

/// The markers clipboard managers honour (nspasteboard.org): transient means
/// "do not record", auto-generated means "an app wrote this, not the user".
/// Both go on the same pasteboard item as the text, each with empty data, so a
/// manager sees them whichever way it checks.
const TRANSIENT_MARKER_TYPES: [&str; 2] = [
    "org.nspasteboard.TransientType",
    "org.nspasteboard.AutoGeneratedType",
];

/// Everything our own pasteboard write declares, in order: the text type
/// first, then the transient markers. Pure — unit-tested.
fn declared_write_types(text_type: &str) -> Vec<String> {
    let mut types = Vec::with_capacity(1 + TRANSIENT_MARKER_TYPES.len());
    types.push(text_type.to_string());
    types.extend(TRANSIENT_MARKER_TYPES.iter().map(|t| t.to_string()));
    types
}

/// Cap on the bytes we copy off the pasteboard for the restore snapshot. A
/// dictation must not hold a multi-hundred-MB image in memory (twice: the
/// snapshot plus the restored copy); anything past the cap is dropped from the
/// snapshot and logged, so that flavour is lost on restore.
const SNAPSHOT_BUDGET_BYTES: usize = 16 * 1024 * 1024;

/// Whether a pasteboard flavour is worth carrying through the snapshot.
///
/// Dropped: our own transient markers (they describe *our* write, not the
/// user's clipboard) and promised-file flavours, whose data only the promising
/// app can produce — re-writing the metadata without a live promise would put
/// a dangling file promise on the clipboard. Pure — unit-tested.
fn is_restorable_type(ty: &str) -> bool {
    if TRANSIENT_MARKER_TYPES.contains(&ty) {
        return false;
    }
    !(ty.starts_with("com.apple.pasteboard.promised")
        || ty == "com.apple.NSFilePromiseItemMetaData")
}

/// Whether one more flavour of `len` bytes still fits the snapshot budget.
/// Pure — unit-tested.
fn fits_budget(used: usize, len: usize, budget: usize) -> bool {
    used.saturating_add(len) <= budget
}

/// Every item on the general pasteboard, as plain bytes: one entry per
/// pasteboard item, each a list of `(type, data)` flavours. Plain data (not
/// Objective-C objects) so it can cross to the detached restore thread.
type ClipboardSnapshot = Vec<Vec<(String, Vec<u8>)>>;

/// Copy every restorable flavour of every pasteboard item into plain bytes.
/// An empty result means "nothing we can put back" (empty clipboard, or only
/// promised/oversized content).
fn snapshot_pasteboard() -> ClipboardSnapshot {
    use objc2_app_kit::NSPasteboard;

    let pb = NSPasteboard::generalPasteboard();
    let mut snapshot: ClipboardSnapshot = Vec::new();
    let Some(items) = pb.pasteboardItems() else {
        return snapshot;
    };
    let mut used = 0usize;
    for item in items.iter() {
        let mut flavours: Vec<(String, Vec<u8>)> = Vec::new();
        for ty in item.types().iter() {
            let name = ty.to_string();
            if !is_restorable_type(&name) {
                continue;
            }
            let Some(data) = item.dataForType(&ty) else {
                continue;
            };
            let bytes = data.to_vec();
            if !fits_budget(used, bytes.len(), SNAPSHOT_BUDGET_BYTES) {
                eprintln!(
                    "[clipboard] snapshot over budget: dropping {name} ({} bytes) — that flavour will not come back",
                    bytes.len()
                );
                continue;
            }
            used += bytes.len();
            flavours.push((name, bytes));
        }
        if !flavours.is_empty() {
            snapshot.push(flavours);
        }
    }
    snapshot
}

/// Put a snapshot back on the general pasteboard, all items and flavours in
/// one `clearContents` + `writeObjects` cycle.
fn restore_pasteboard(snapshot: &ClipboardSnapshot) -> bool {
    use objc2::runtime::ProtocolObject;
    use objc2_app_kit::{NSPasteboard, NSPasteboardItem, NSPasteboardWriting};
    use objc2_foundation::{NSArray, NSData, NSString};

    let items: Vec<_> = snapshot
        .iter()
        .map(|flavours| {
            let item = NSPasteboardItem::new();
            for (ty, bytes) in flavours {
                item.setData_forType(&NSData::with_bytes(bytes), &NSString::from_str(ty));
            }
            item
        })
        .collect();
    let writable: Vec<&ProtocolObject<dyn NSPasteboardWriting>> = items
        .iter()
        .map(|i| ProtocolObject::from_ref(&**i))
        .collect();

    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();
    pb.writeObjects(&NSArray::from_slice(&writable))
}

/// Write `text` to the general pasteboard marked transient + auto-generated,
/// so clipboard managers don't record the dictation. One `clearContents` →
/// `declareTypes` → write cycle, exactly as nspasteboard.org specifies.
fn write_text_transient(text: &str) -> eyre::Result<()> {
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
    use objc2_foundation::{NSArray, NSData, NSString};

    let pb = NSPasteboard::generalPasteboard();
    pb.clearContents();

    let text_type = unsafe { NSPasteboardTypeString };
    let types: Vec<_> = declared_write_types(&text_type.to_string())
        .iter()
        .map(|t| NSString::from_str(t))
        .collect();
    // SAFETY: no owner — we set every declared type's data below, so nothing
    // is ever lazily requested from us.
    unsafe { pb.declareTypes_owner(&NSArray::from_retained_slice(&types), None) };

    if !pb.setString_forType(&NSString::from_str(text), text_type) {
        return Err(eyre::eyre!("NSPasteboard setString:forType: failed"));
    }
    for marker in TRANSIENT_MARKER_TYPES {
        // Empty data: the marker's presence is the whole signal.
        pb.setData_forType(Some(&NSData::new()), &NSString::from_str(marker));
    }
    Ok(())
}

pub fn paste_via_clipboard(text: &str, expected_pid: Option<i32>) -> eyre::Result<PasteOutcome> {
    // Snapshot the whole pasteboard and set ours under the write lock. The
    // lock keeps a pending background restore (from a previous dictation) from
    // clobbering the pasteboard between our set and the target consuming it.
    let _guard = CLIPBOARD_WRITE_MUTEX.lock().unwrap_or_else(|e| e.into_inner());

    // Fail-closed foreground guard, immediately before the Cmd+V: a global
    // Cmd+V lands in the OS-frontmost app, so it must not be posted unless the
    // expected target IS frontmost. A mismatch means we'd paste into the wrong
    // app — refuse with nothing modified (checked before any clipboard write).
    if !crate::injector::clipboard_target_is_frontmost(expected_pid, crate::injector::frontmost_pid()) {
        eprintln!(
            "[clipboard] refusing Cmd+V: expected pid {expected_pid:?} is not frontmost — not pasting"
        );
        return Ok(PasteOutcome::RefusedForeground);
    }

    let saved = snapshot_pasteboard();

    write_text_transient(text)?;

    // changeCount right after our write. Cmd+V only reads the pasteboard, so this
    // stays put unless the user copies something during the settle wait below.
    let our_count = pasteboard_change_count();

    // Pasteboard access can itself yield. Check again at the delivery boundary;
    // on refusal leave the dictation available for a deliberate manual paste.
    if !crate::injector::clipboard_target_is_frontmost(expected_pid, crate::injector::frontmost_pid()) {
        return Ok(PasteOutcome::RefusedForeground);
    }
    synthesize_cmd_v()?;

    // Let the OS + target app actually consume the Cmd+V before restoring. This
    // stays on the worker: the consume-window also marshals the next dictation's
    // set_text until this paste is consumed, so a quick follow-up can't overwrite
    // text this paste hasn't pulled yet.
    std::thread::sleep(Duration::from_millis(paste_settle_ms(text.chars().count())));
    drop(_guard);

    if !saved.is_empty() {
        // Detach the restore so the worker returns immediately — a ~1 s restore
        // must not pin the next dictation or the UI clear. Re-reading the
        // changeCount on the restore thread honours the "don't clobber a fresh
        // copy" guard, and the write lock keeps restores/pastes fully ordered.
        std::thread::spawn(move || restore_clipboard(saved, our_count));
    }
    // If the clipboard was empty before, leave our injected text on it (better
    // than wiping it — gives the user a manual paste fallback).
    Ok(PasteOutcome::Delivered)
}

/// Detached clipboard restore. Runs on a background thread so a slow restore
/// can't pin the dictation worker. Takes the write lock so it can't interleave
/// with a concurrent paste (`write → Cmd+V`) or another restore; if the user
/// copied during the paste-settle window, leave their copy intact.
fn restore_clipboard(orig: ClipboardSnapshot, our_count: Option<isize>) {
    let _guard = CLIPBOARD_WRITE_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    if user_copied_during_settle(our_count, pasteboard_change_count()) {
        eprintln!("[clipboard] user copied during paste; leaving their clipboard intact");
        return;
    }
    if !restore_pasteboard(&orig) {
        eprintln!("[clipboard] restore: writeObjects failed; clipboard left as-is");
    }
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
    // Hold the write lock for the whole read round-trip so a pending background
    // restore from a previous dictation can't clobber the sentinel between our
    // set and the Cmd+C that reads it.
    let _guard = CLIPBOARD_WRITE_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let mut cb = Clipboard::new().map_err(|e| eyre::eyre!("Clipboard::new failed: {e}"))?;
    let saved = snapshot_pasteboard();

    // The probe is one of our own writes too: mark it transient so clipboard
    // managers don't record the sentinel (or the selection Cmd+C pulls in).
    write_text_transient(SENTINEL)?;
    synthesize_cmd_c()?;
    std::thread::sleep(Duration::from_millis(COPY_SETTLE_MS));

    let after = cb.get_text().unwrap_or_default();

    // Restore the user's clipboard regardless of outcome. An empty snapshot
    // means there was nothing restorable there: clear rather than leave the
    // sentinel (or the copied selection) behind.
    if saved.is_empty() {
        let _ = write_text_transient("");
    } else {
        restore_pasteboard(&saved);
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

/// Synthesize an Escape key press + release ("press escape" voice command) —
/// dismiss dialogs, clear a field, or leave an editor's insert mode by voice.
pub fn synthesize_escape() -> eyre::Result<()> {
    synthesize_plain_key(KEY_ESCAPE)
}

/// Synthesize Cmd+Z ("undo that" voice command) to revert the previous action.
/// Same flag discipline as Cmd+V: set Command only on key-down, clear on key-up
/// so a held push-to-talk modifier can't turn it into a different chord.
pub fn synthesize_undo() -> eyre::Result<()> {
    let source = clean_event_source()?;

    let down = CGEvent::new_keyboard_event(source.clone(), KEY_Z, true)
        .map_err(|_| eyre::eyre!("Cmd+Z keydown create failed"))?;
    down.set_flags(CGEventFlags::CGEventFlagCommand);
    down.post(CGEventTapLocation::HID);

    let up = CGEvent::new_keyboard_event(source, KEY_Z, false)
        .map_err(|_| eyre::eyre!("Cmd+Z keyup create failed"))?;
    up.set_flags(CGEventFlags::CGEventFlagNull);
    up.post(CGEventTapLocation::HID);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaled_settle_scales_with_length_and_caps() {
        // Base with no extra content.
        assert_eq!(scaled_paste_settle_ms(0), PASTE_SETTLE_BASE_MS);
        // +1 ms per ~3 chars.
        assert_eq!(scaled_paste_settle_ms(3), PASTE_SETTLE_BASE_MS + 1);
        assert_eq!(scaled_paste_settle_ms(300), PASTE_SETTLE_BASE_MS + 100);
        // Clamps to the max so a long dictation can't stall the hot path.
        assert_eq!(scaled_paste_settle_ms(3_000), PASTE_SETTLE_MAX_MS);
        assert!(scaled_paste_settle_ms(3_000) <= PASTE_SETTLE_MAX_MS);
    }

    #[test]
    fn user_copy_during_settle_is_detected_only_on_change_count_advance() {
        // Same count (our own write) → no user copy → we may restore.
        assert!(!user_copied_during_settle(Some(1), Some(1)));
        // Count advanced → someone (user) copied → don't clobber.
        assert!(user_copied_during_settle(Some(1), Some(2)));
        // Unreadable count either side → fall back to restoring (old behaviour).
        assert!(!user_copied_during_settle(None, Some(2)));
        assert!(!user_copied_during_settle(Some(1), None));
        assert!(!user_copied_during_settle(None, None));
    }

    #[test]
    fn our_write_declares_the_text_type_plus_both_transient_markers() {
        let types = declared_write_types("public.utf8-plain-text");
        assert_eq!(
            types,
            vec![
                "public.utf8-plain-text".to_string(),
                "org.nspasteboard.TransientType".to_string(),
                "org.nspasteboard.AutoGeneratedType".to_string(),
            ]
        );
        // The text type leads: managers that read only the first flavour still
        // get the text.
        assert_eq!(types[0], "public.utf8-plain-text");
    }

    #[test]
    fn restore_plan_keeps_real_flavours_and_drops_markers_and_promises() {
        // Real user content of every shape round-trips.
        for ty in [
            "public.utf8-plain-text",
            "public.rtf",
            "public.tiff",
            "public.png",
            "public.file-url",
        ] {
            assert!(is_restorable_type(ty), "{ty} should be restored");
        }
        // Our own markers describe our write, not the user's clipboard.
        for ty in TRANSIENT_MARKER_TYPES {
            assert!(!is_restorable_type(ty), "{ty} should not be restored");
        }
        // Promised file data: only the promising app can produce it.
        assert!(!is_restorable_type(
            "com.apple.pasteboard.promised-file-url"
        ));
        assert!(!is_restorable_type(
            "com.apple.pasteboard.promised-file-content-type"
        ));
        assert!(!is_restorable_type("com.apple.NSFilePromiseItemMetaData"));
    }

    #[test]
    fn snapshot_budget_stops_at_the_cap() {
        assert!(fits_budget(0, 10, 100));
        assert!(fits_budget(90, 10, 100));
        // One byte over the cap is dropped, not truncated.
        assert!(!fits_budget(91, 10, 100));
        // A single oversized flavour can't sneak in on an empty budget.
        assert!(!fits_budget(0, 101, 100));
        // No overflow panic on absurd sizes.
        assert!(!fits_budget(usize::MAX, 1, SNAPSHOT_BUDGET_BYTES));
    }
}
