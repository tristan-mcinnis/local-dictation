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
//!
//! A quit or crash mid-capture must not leave the Mac muted. Quit restores in
//! `applicationWillTerminate:`. For a crash or a plain `kill`, a marker file is
//! written before we mute and removed after we unmute; the next daemon start
//! finds a leftover marker and unmutes ([`restore_after_crash`]).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::thread::JoinHandle;

/// One capture session's hold on the system mute flag.
///
/// The read and the set are each an `osascript` launch (measured 110-400 ms
/// apiece on 2026-09-26). They used to run on the dictation worker between
/// the start cue and the recording pill, so the pill, the Escape cancel gate
/// and the press-time focus capture all waited 0.2-0.8 s after every press.
/// `mute` now hands the read+set to a background thread and returns at once;
/// `restore` joins that thread first, so the unmute can never overtake the
/// mute and the outcome cue that follows it is still audible.
struct Ducker {
    /// True while a PTT capture session owns the mute flag. Guards against
    /// double-mute on a spurious second StartRecording and against a
    /// restore() with no matching mute().
    active: AtomicBool,
    /// Whether output was already muted when we took over — restore() leaves
    /// it muted in that case rather than un-muting audio the user had silenced.
    was_muted: AtomicBool,
    /// The in-flight background read+set, joined by the next restore.
    pending: Mutex<Option<JoinHandle<()>>>,
}

impl Ducker {
    const fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            was_muted: AtomicBool::new(false),
            pending: Mutex::new(None),
        }
    }

    /// Claim the session and mute in the background. `read` and `set` are
    /// the device operations (injected so tests never touch real audio).
    fn mute(&'static self, read: fn() -> Option<bool>, set: fn(bool)) {
        // Claim ownership; if a session is already active, leave it untouched.
        if self.active.swap(true, Ordering::SeqCst) {
            return;
        }
        let handle = std::thread::spawn(move || {
            // If we can't read the device's current state, leave it untouched
            // rather than mute-then-restore — a failed read previously
            // defaulted to "unmuted", which on restore would un-mute audio the
            // user had muted themselves. Releasing ownership makes the
            // matching restore() a no-op (it joins this thread first).
            let Some(was_muted) = read() else {
                self.active.store(false, Ordering::SeqCst);
                return;
            };
            self.was_muted.store(was_muted, Ordering::SeqCst);
            if should_change(was_muted) {
                set(true);
            }
        });
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
    }

    /// Wait for any in-flight mute, then put the device back.
    fn restore(&'static self, set: fn(bool)) {
        let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(handle) = pending {
            let _ = handle.join();
        }
        // Release ownership; if we weren't active, there's nothing to restore.
        if !self.active.swap(false, Ordering::SeqCst) {
            return;
        }
        if should_change(self.was_muted.load(Ordering::SeqCst)) {
            set(false);
        }
    }
}

static DUCKER: Ducker = Ducker::new();

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
/// muted. Returns at once: the `osascript` read and set run on a background
/// thread (see [`Ducker`]), so the caller's recording feedback is not held up.
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
    DUCKER.mute(current_muted, set_muted_and_mark);
}

/// Restore output to its pre-capture state. No-op if we never muted (not
/// active). Safe to call on every pipeline exit path — only the first call per
/// session does anything. Synchronous: it waits for a still-running mute, then
/// unmutes, so a cue played right after it is audible.
pub fn restore() {
    if disabled() {
        return;
    }
    DUCKER.restore(set_muted_and_mark);
}

/// Unmute if a previous run muted output and never got to restore it (crash,
/// `kill`). Call once at daemon start, before any capture.
pub fn restore_after_crash() {
    if let Some(marker) = marker_path() {
        if restore_from_marker(&marker, set_muted) {
            eprintln!("[duck] previous run left output muted; unmuted");
        }
    }
}

fn marker_path() -> Option<PathBuf> {
    Some(crate::app_paths::app_support_dir()?.join("output-muted-by-dictation"))
}

/// The device op the live ducker uses: the marker exists exactly while output
/// may be muted by us. Written before muting, so a crash between the two still
/// leaves it; removed only after the unmute.
fn set_muted_and_mark(muted: bool) {
    let marker = marker_path();
    if muted {
        if let Some(marker) = &marker {
            write_marker(marker);
        }
        set_muted(true);
    } else {
        set_muted(false);
        if let Some(marker) = &marker {
            let _ = std::fs::remove_file(marker);
        }
    }
}

fn write_marker(marker: &Path) {
    if let Some(dir) = marker.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(marker, b"");
}

/// Returns true when a marker was found and the device unmuted.
fn restore_from_marker(marker: &Path, set: fn(bool)) -> bool {
    if !marker.exists() {
        return false;
    }
    set(false);
    let _ = std::fs::remove_file(marker);
    true
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

/// The frontmost app's bundle id, read via `NSWorkspace.frontmostApplication`
/// (no extra permission, no `osascript` round-trip). Best-effort: `None` when
/// it can't be read. Used for per-app tone detection and conferencing checks.
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
pub fn frontmost_bundle_id() -> Option<String> {
    use objc2_app_kit::NSWorkspace;
    NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .and_then(|app| app.bundleIdentifier())
        .map(|s| s.to_string())
}

#[cfg(not(all(target_os = "macos", feature = "ax-inject")))]
pub fn frontmost_bundle_id() -> Option<String> {
    None
}

/// True when the frontmost app is a known video/voice call client. Reads
/// `NSWorkspace.frontmostApplication` (no extra permission, no `osascript`
/// round-trip) and matches its bundle id against [`is_conferencing_bundle_id`].
/// Best-effort: returns `false` (duck normally) when the frontmost app or its
/// bundle id can't be read.
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
fn frontmost_is_conferencing() -> bool {
    match frontmost_bundle_id() {
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
        "us.zoom.xos",                 // Zoom
        "com.microsoft.teams",         // Microsoft Teams (classic)
        "com.microsoft.teams2",        // Microsoft Teams (new)
        "com.cisco.webexmeetingsapp",  // Webex Meetings
        "Cisco-Systems.Spark",         // Webex / Cisco Spark
        "com.apple.FaceTime",          // FaceTime
        "com.hnc.Discord",             // Discord
        "com.tinyspeck.slackmacgap",   // Slack (huddles)
        "com.google.meet",             // Google Meet (standalone app)
        "com.skype.skype",             // Skype
        "com.ringcentral.glip",        // RingCentral
        "com.bluejeansnet.app",        // BlueJeans
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
        // assert via the session flag rather than the audio system: with no
        // prior mute(), `active` is false, so the swap returns false and we
        // bail before any osascript call. (The flag is global to DUCKER.)
        DUCKER.active.store(false, Ordering::SeqCst);
        // Should return immediately without panicking or flipping state.
        restore();
        assert!(!DUCKER.active.load(Ordering::SeqCst));
    }

    // Test-only device ops: a slow read (like one osascript launch) and a set
    // that records what it was asked to do instead of touching real audio.
    static SETS: Mutex<Vec<bool>> = Mutex::new(Vec::new());
    fn slow_unmuted_read() -> Option<bool> {
        std::thread::sleep(std::time::Duration::from_millis(200));
        Some(false)
    }
    fn failed_read() -> Option<bool> {
        std::thread::sleep(std::time::Duration::from_millis(50));
        None
    }
    fn record_set(muted: bool) {
        SETS.lock().unwrap().push(muted);
    }

    // SPEED (2026-09-26): the osascript read+set ran on the dictation worker
    // before the recording pill showed. mute must return without waiting for
    // the read, and restore must still unmute strictly after the mute landed.
    #[test]
    fn mute_does_not_wait_for_the_device_and_restore_keeps_the_order() {
        static D: Ducker = Ducker::new();
        SETS.lock().unwrap().clear();
        let t = std::time::Instant::now();
        D.mute(slow_unmuted_read, record_set);
        if crate::timing_checks() {
            assert!(
                t.elapsed() < std::time::Duration::from_millis(100),
                "mute blocked for {:?}",
                t.elapsed()
            );
        }
        D.restore(record_set);
        assert_eq!(*SETS.lock().unwrap(), vec![true, false], "mute, then unmute");
        assert!(!D.active.load(Ordering::SeqCst));
    }

    #[test]
    fn a_marker_left_by_a_crashed_run_unmutes_once_at_start() {
        static CALLS: Mutex<Vec<bool>> = Mutex::new(Vec::new());
        fn record(muted: bool) {
            CALLS.lock().unwrap().push(muted);
        }
        let dir = std::env::temp_dir().join(format!("ld-duck-{}", std::process::id()));
        let marker = dir.join("nested").join("output-muted-by-dictation");

        assert!(!restore_from_marker(&marker, record), "no marker, no unmute");
        assert!(CALLS.lock().unwrap().is_empty());

        write_marker(&marker);
        assert!(restore_from_marker(&marker, record));
        assert_eq!(*CALLS.lock().unwrap(), vec![false]);
        assert!(!marker.exists(), "marker consumed");
        assert!(!restore_from_marker(&marker, record), "second start does nothing");
        assert_eq!(CALLS.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_device_is_left_alone_on_restore() {
        static D: Ducker = Ducker::new();
        static UNTOUCHED: Mutex<Vec<bool>> = Mutex::new(Vec::new());
        fn record(muted: bool) {
            UNTOUCHED.lock().unwrap().push(muted);
        }
        D.mute(failed_read, record);
        D.restore(record);
        assert!(UNTOUCHED.lock().unwrap().is_empty(), "nothing set when the read failed");
    }
}
