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
//! had your Mac muted manually, releasing the key leaves it muted. If the prior
//! state can't be read at all, we leave the device untouched rather than risk
//! un-muting audio you had silenced yourself.
//!
//! We also **skip ducking entirely while a video/voice call app is frontmost**
//! (Zoom, Teams, FaceTime, …) — muting system output mid-call would silence the
//! people you're talking to. Detection is a permission-free
//! `NSWorkspace.frontmostApplication` bundle-id check; browser-based calls
//! (e.g. Google Meet in a tab) can't be detected this way.
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
    // Never mute system output while a video/voice call app is frontmost —
    // doing so would silence the people you're on a call with for the whole
    // capture + processing window. Ducking is only a nicety (keep music from
    // bleeding into the room while you dictate); skipping it here avoids the
    // most jarring cross-app interaction at no real cost.
    if frontmost_is_conferencing() {
        return;
    }
    // Claim ownership; if a session is already active, leave it untouched.
    if ACTIVE.swap(true, Ordering::SeqCst) {
        return;
    }
    // If we can't read the device's current state, leave it untouched rather
    // than mute-then-restore — a failed read previously defaulted to "unmuted",
    // which on restore would un-mute audio the user had muted themselves.
    let was_muted = match current_muted() {
        Some(m) => m,
        None => {
            // Release the ownership we just claimed so a later restore() is a
            // no-op (we never actually touched the device).
            ACTIVE.store(false, Ordering::SeqCst);
            return;
        }
    };
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

/// True when the frontmost app is a known video/voice call client. Reads
/// `NSWorkspace.frontmostApplication` (no extra permission, no `osascript`
/// round-trip) and matches its bundle id against [`is_conferencing_bundle_id`].
/// Best-effort: returns `false` (duck normally) when the frontmost app or its
/// bundle id can't be read.
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
fn frontmost_is_conferencing() -> bool {
    use objc2_app_kit::NSWorkspace;
    let bundle_id = NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .and_then(|app| app.bundleIdentifier())
        .map(|s| s.to_string());
    match bundle_id {
        Some(id) => is_conferencing_bundle_id(&id),
        None => false,
    }
}

/// Without the AppKit stack we can't cheaply read the frontmost app, so never
/// suppress ducking (the default build doesn't run the daemon anyway).
#[cfg(not(all(target_os = "macos", feature = "ax-inject")))]
fn frontmost_is_conferencing() -> bool {
    false
}

/// Whether `bundle_id` belongs to a video/voice call app whose call audio we'd
/// cut by muting system output. Kept as a pure predicate over the bundle id so
/// the match list is unit-tested without AppKit. Browser-based calls (Google
/// Meet in a tab) can't be detected this way — a known limitation; the standalone
/// Meet app and the common native clients are covered.
// Only reached through the AppKit `frontmost_is_conferencing` path; the unit
// test exercises it in every build, so keep it compiled but silence dead-code
// where that caller is absent.
#[cfg_attr(not(all(target_os = "macos", feature = "ax-inject")), allow(dead_code))]
fn is_conferencing_bundle_id(bundle_id: &str) -> bool {
    const CALL_APPS: &[&str] = &[
        "us.zoom.xos",                // Zoom
        "com.microsoft.teams",        // Microsoft Teams (classic)
        "com.microsoft.teams2",       // Microsoft Teams (new)
        "com.cisco.webexmeetingsapp", // Webex Meetings
        "Cisco-Systems.Spark",        // Webex / Cisco Spark
        "com.apple.FaceTime",         // FaceTime
        "com.hnc.Discord",            // Discord
        "com.tinyspeck.slackmacgap",  // Slack (huddles)
        "com.google.meet",            // Google Meet (standalone app)
        "com.skype.skype",            // Skype
        "com.ringcentral.glip",       // RingCentral
        "com.bluejeansnet.app",       // BlueJeans
        "com.gotomeeting.GoToMeeting", // GoToMeeting
    ];
    CALL_APPS.iter().any(|a| a.eq_ignore_ascii_case(bundle_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conferencing_bundle_ids_are_recognized() {
        assert!(is_conferencing_bundle_id("us.zoom.xos"));
        assert!(is_conferencing_bundle_id("com.microsoft.teams2"));
        assert!(is_conferencing_bundle_id("com.apple.FaceTime"));
        // Case-insensitive (bundle ids are compared loosely).
        assert!(is_conferencing_bundle_id("US.ZOOM.XOS"));
        // Ordinary apps you'd dictate into must still duck normally.
        assert!(!is_conferencing_bundle_id("com.apple.TextEdit"));
        assert!(!is_conferencing_bundle_id("com.microsoft.VSCode"));
        assert!(!is_conferencing_bundle_id(""));
    }

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
