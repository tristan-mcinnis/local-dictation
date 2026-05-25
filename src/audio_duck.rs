//! Mute other system audio while push-to-talk is active, then restore it.
//!
//! When you hold the PTT key, anything already playing through the Mac's
//! output (music, a video, a call) is muted so it neither distracts you nor
//! bleeds into the room while you dictate. The moment the utterance is fully
//! handled — dictation injected, transform pasted, or the path bailed out —
//! the previous state is restored.
//!
//! Implementation is deliberately dependency-free: it shells out to
//! `osascript` to read + set the system output mute flag (the same tool the
//! menu bar's volume control drives), so it compiles in the default build with
//! no native-audio crate. Whole-output mute also silences our own start/stop
//! cues during the brief capture+processing window — that's intentional, the
//! point is a quiet output while you're talking.
//!
//! Restore is conservative: we remember whether output was *already* muted
//! when capture began and only unmute if we were the ones who muted it. If you
//! had your Mac muted manually, releasing the key leaves it muted.
//!
//! Set `DICTATE_NO_MUTE=1` to disable this entirely.

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

/// True while a PTT capture session currently owns the mute flag. Guards
/// against double-mute on a spurious second StartRecording and against a
/// restore() with no matching mute().
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether output was already muted when we took over — restore() leaves it
/// muted in that case rather than un-muting audio the user had silenced.
static WAS_MUTED: AtomicBool = AtomicBool::new(false);

/// Honour the opt-out env var.
fn disabled() -> bool {
    std::env::var("DICTATE_NO_MUTE").is_ok()
}

/// Whether to actually flip the system mute, given its prior state. Pure so it
/// can be unit-tested without touching the real audio device.
fn should_change(was_muted: bool) -> bool {
    !was_muted
}

/// Mute system output for the duration of a capture session, remembering the
/// prior state. No-op if disabled, already active, or output was already
/// muted. Blocks briefly (~one `osascript` round-trip); called off the hot
/// event-tap thread, after the mic is already capturing.
pub fn mute() {
    if disabled() {
        return;
    }
    // Claim ownership; if a session is already active, leave it untouched.
    if ACTIVE.swap(true, Ordering::SeqCst) {
        return;
    }
    let was_muted = current_muted().unwrap_or(false);
    WAS_MUTED.store(was_muted, Ordering::SeqCst);
    if should_change(was_muted) {
        set_muted(true);
    }
}

/// Restore output to its pre-capture state. No-op if we never muted (not
/// active). Safe to call on every pipeline exit path — only the first call per
/// session does anything.
pub fn restore() {
    if disabled() {
        return;
    }
    // Release ownership; if we weren't active, there's nothing to restore.
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let was_muted = WAS_MUTED.load(Ordering::SeqCst);
    if should_change(was_muted) {
        set_muted(false);
    }
}

/// RAII guard: restore output mute when dropped. Construct one at the top of a
/// scope that ends a capture session and every exit path (early `continue`,
/// error return, normal fall-through) un-mutes automatically.
pub struct RestoreOnDrop;

impl Drop for RestoreOnDrop {
    fn drop(&mut self) {
        restore();
    }
}

/// Read the current system output mute flag via AppleScript. Returns `None` if
/// `osascript` is unavailable or the output couldn't be parsed.
#[cfg(target_os = "macos")]
fn current_muted() -> Option<bool> {
    let out = Command::new("osascript")
        .args(["-e", "output muted of (get volume settings)"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    match s.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

#[cfg(not(target_os = "macos"))]
fn current_muted() -> Option<bool> {
    None
}

/// Set the system output mute flag via AppleScript. Best-effort: errors are
/// swallowed (muting is a nicety, never worth failing a dictation over).
#[cfg(target_os = "macos")]
fn set_muted(muted: bool) {
    let script = format!("set volume output muted {muted}");
    let _ = Command::new("osascript").args(["-e", &script]).status();
}

#[cfg(not(target_os = "macos"))]
fn set_muted(_muted: bool) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_only_when_not_already_muted() {
        // If output was unmuted, we mute on capture and unmute on restore.
        assert!(should_change(false));
        // If the user already had it muted, we leave it alone both ways.
        assert!(!should_change(true));
    }

    #[test]
    fn restore_without_mute_is_noop() {
        // A restore() with no active session must not touch the device. We
        // assert via the ACTIVE flag rather than the audio system: with no
        // prior mute(), ACTIVE is false, so the swap returns false and we bail
        // before any osascript call. (Run in isolation; ACTIVE is global.)
        ACTIVE.store(false, Ordering::SeqCst);
        // Should return immediately without panicking or flipping state.
        restore();
        assert!(!ACTIVE.load(Ordering::SeqCst));
    }
}
