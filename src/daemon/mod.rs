//! Push-to-talk background daemon.
//!
//! Boots once, loads + warms both models, then installs a CGEventTap that
//! watches modifier-flag changes for the Right Option key. Holding the key
//! starts mic capture; releasing it stops capture, runs the inference
//! pipeline, and injects the cleaned text at the cursor.
//!
//! Per-utterance latency after the daemon is running is just the hot-path
//! inference time — the multi-hundred-millisecond model load cost is paid
//! once at startup.
//!
//! Requires Accessibility permission so the event tap can observe modifier
//! flags system-wide. The first run surfaces the TCC prompt via the
//! injector's `ensure_trusted_or_prompt`.

mod boot;
mod capture;
mod cleanup;
mod deliver;
mod listen;
mod ptt;
mod recover;
mod release;
mod tail;
mod tap;
mod transcribe;
mod transform;
mod worker;
use boot::*;
use capture::*;
use cleanup::*;
use deliver::*;
pub use listen::run_listen;
use listen::*;
use ptt::*;
use recover::*;
use tail::*;
use tap::*;
use transcribe::*;
#[cfg(feature = "cleaner")]
use transform::*;
use worker::*;

use crate::audio::{AudioCaptureEngine, BUFFER_CAPACITY};
use crate::corrections::Corrections;
use crate::cues;
use crate::injector::{AccessibilityInjector, Delivery, FocusTarget, InjectRequest};
use crate::menubar;
use crate::refiner::{DictationOutcome, Refiner};
use crate::transcriber::LocalInferenceWorker;
use crate::ui_channel::{self, UiState};
use crate::voice_commands::TrailingAction;
use core_foundation::base::TCFType;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::event::{
    CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType, CallbackResult, EventField,
};
use core_graphics::event_source::CGEventSourceStateID;
use std::os::raw::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Round a Duration to integer milliseconds for log output.
fn ms(d: Duration) -> u128 {
    d.as_millis()
}

/// Format Duration as seconds with 2 decimal places (used for hold time).
fn secs(d: Duration) -> String {
    format!("{:.2}s", d.as_secs_f64())
}

/// Look up an app name from a PID via `ps`. Falls back to the bare PID
/// if the lookup fails. Used purely for log readability.
fn app_name(pid: i32) -> String {
    crate::injector::process_command_path(pid)
        .map(|s| {
            // `/path/to/Visual Studio Code.app/Contents/MacOS/Code` →
            // `Visual Studio Code`
            let trimmed = s.trim();
            if let Some(idx) = trimmed.find(".app/") {
                let before = &trimmed[..idx];
                if let Some(last_slash) = before.rfind('/') {
                    return before[last_slash + 1..].to_string();
                }
                return before.to_string();
            }
            // Fallback: basename of the binary path
            trimmed.rsplit('/').next().unwrap_or(trimmed).to_string()
        })
        .unwrap_or_else(|| format!("pid {pid}"))
}

#[cfg(feature = "cleaner")]
use crate::cleaner::TextCleanupEngine;

/// How many screen-harvested proper nouns to feed the cleanup prompt. Kept small
/// on purpose: measurement (examples/latency_lab) showed the accuracy win is
/// fully realized by ~20 terms, while larger lists both cost prefill latency and
/// start distracting the 1B model from basic cleanup. Combined with the curated
/// corrections vocabulary, this stays well under that regression threshold.
const SCREEN_VOCAB_CAP: usize = 16;

// Spacebar — kVK_Space. Chorded with the held PTT key to arm hands-free mode.
const SPACE_KEYCODE: i64 = 0x31;

// Escape — kVK_Escape. A plain Escape cancels an in-flight utterance (record
// or pending transcription); it passes through untouched when idle. The pill
// advertises "Cancel recording (Escape)" but no global Escape binding existed
// — this fixes that, via the out-of-band cancel gate in `crate::cancel`.
const ESCAPE_KEYCODE: i64 = 0x35;

/// Unblocks a quit that is waiting on the worker's models, from a `Drop` so a
/// panic cannot skip it. Before this, a panic inside `worker_loop` left Quit
/// waiting out the whole `MODEL_RELEASE_TIMEOUT`.
struct ReleaseOnExit;

impl Drop for ReleaseOnExit {
    fn drop(&mut self) {
        // Every model this thread owned is dropped by now, so the process can
        // safely run ggml's static Metal teardown (see `crate::shutdown`).
        crate::shutdown::mark_released();
    }
}

/// How long the "not ready" pill stays up. Long enough to read a sentence.
const UNAVAILABLE_PILL: Duration = Duration::from_millis(3500);

/// A press that cannot start a dictation says why, instead of going into a
/// channel nobody is reading. Returns true when the press was refused.
///
/// Called from the event-tap thread, so it stays to one uncontended mutex read
/// and, only in the failing case, a pill update.
fn report_if_unavailable() -> bool {
    let Some(problem) = crate::health::current().filter(|p| p.fault.blocks_dictation()) else {
        return false;
    };
    eprintln!(
        "[hotkey] {} press refused · {} · {}",
        stamp(),
        problem.headline(),
        problem.detail
    );
    ui_channel::show_transient_if_unowned(UiState::Unavailable, UNAVAILABLE_PILL);
    true
}

/// This build only runs on Apple Silicon: the Swift MLX helper is arm64-only
/// and the models are Metal. Said plainly at boot, rather than as a Finder
/// "can't open this application" or a link error deep in a build script.
pub fn unsupported_architecture() -> Option<String> {
    architecture_problem(std::env::consts::ARCH)
}

/// Split out so it can be tested without an Intel Mac.
fn architecture_problem(arch: &str) -> Option<String> {
    if arch == "aarch64" {
        return None;
    }
    Some(format!(
        "Local Dictation needs an Apple Silicon Mac (M1 or later). This build is running on \
         {arch}, where the speech model cannot load."
    ))
}

/// Everything the daemon reads from settings, resolved once at boot from one
/// `Settings` load (env > settings.json > default) and passed down. Relaunch
/// to apply. The one per-utterance read is the Translate-to target, which
/// `spoken_command` loads only for an utterance that opens with "translate",
/// so the menu's choice applies without a relaunch.
pub struct DaemonConfig {
    /// Resolved fixed Cohere MLX ASR configuration.
    pub asr: crate::transcriber::AsrConfig,
    #[cfg(feature = "cleaner")]
    pub llm_model_path: String,
    #[cfg(feature = "cleaner")]
    pub instruction_model_path: Option<String>,
    /// If true, inject the raw ASR transcript without model cleanup.
    pub no_cleanup: bool,
    /// The push-to-talk key or chord the event tap watches.
    pub hotkey: crate::hotkeys::Hotkey,
    /// The "Copy last dictation" chord, `None` when off.
    pub copy_last_hotkey: Option<crate::hotkeys::Hotkey>,
    /// Double-tap the hotkey to latch hands-free.
    pub double_tap_latch: bool,
    pub capture: CaptureConfig,
    pub result: ResultPrefs,
    /// The listen-mode wake word.
    pub wake_word: String,
}

/// How the microphone is run around a press.
#[derive(Clone, Copy, Debug)]
pub struct CaptureConfig {
    /// Always-on mic lookback; 0 is the open-on-press path.
    pub preroll_ms: u32,
    /// Lower speech thresholds for quiet dictation.
    pub whisper_mode: bool,
    /// Keep the mic open briefly after release so the last word is not clipped.
    pub tail_capture: bool,
    pub tail_capture_cap_ms: u32,
    /// Keep a stopped mic parked this long for a cheap reuse; 0 opens fresh.
    pub warm_window_ms: u32,
    /// Transcribe VAD-closed sentences during the hold for a live preview.
    pub streaming_asr: bool,
    /// Stop a recording at this length; 0 is no limit.
    pub max_recording_minutes: u32,
}

/// What the user sees and gets when a dictation settles.
#[derive(Clone, Copy, Debug)]
pub struct ResultPrefs {
    /// Flash the "Landed" pill after each dictation.
    pub show_result: bool,
    /// Put a kept transcript on the clipboard.
    pub copy_on_paste_failure: bool,
}

impl DaemonConfig {
    pub fn from_settings(settings: &crate::settings::Settings, no_cleanup: bool) -> Self {
        // Through `Hotkey::from_loaded` so an unusable configured key (a
        // plain typing key with no modifier) falls back the same way here as
        // in the Settings window.
        let hotkey = crate::hotkeys::Hotkey::from_loaded(settings);
        Self {
            asr: settings.resolve_asr(),
            #[cfg(feature = "cleaner")]
            llm_model_path: settings.resolve_llm_model(&crate::app_paths::llm_default_path()),
            #[cfg(feature = "cleaner")]
            instruction_model_path: {
                let default = crate::app_paths::instruction_default_path();
                settings.resolve_instruction_model(default.as_deref())
            },
            no_cleanup,
            hotkey,
            copy_last_hotkey: crate::hotkeys::copy_last_from_loaded(settings),
            double_tap_latch: settings.resolve_double_tap_latch(),
            capture: CaptureConfig {
                preroll_ms: settings.resolve_preroll_ms(),
                whisper_mode: settings.resolve_whisper_mode(),
                tail_capture: settings.resolve_tail_capture(),
                tail_capture_cap_ms: settings.resolve_tail_capture_cap_ms(),
                warm_window_ms: settings.resolve_warm_window_ms(),
                streaming_asr: settings.resolve_streaming_asr(),
                max_recording_minutes: settings.resolve_max_recording_minutes(),
            },
            result: ResultPrefs {
                show_result: settings.resolve_show_result(),
                copy_on_paste_failure: settings.resolve_copy_on_paste_failure(),
            },
            wake_word: settings.resolve_wake_word(),
        }
    }
}

#[derive(Debug)]
enum DaemonEvent {
    /// `transform` is set when Shift was co-held at press time — the utterance
    /// is an instruction to rewrite the current selection, not text to insert.
    StartRecording {
        transform: bool,
    },
    StopRecording,
    /// Cancel an in-flight utterance: stop capture and discard it (no
    /// transcribe, no inject). Reached via the control socket or the Escape
    /// key. A no-op when nothing is recording.
    CancelRecording,
    /// PTT + Space chord recognised mid-hold: the session became hands-free, so
    /// releasing the keys won't stop recording. Worker just plays the cue.
    LatchArmed,
    /// Re-run the pipeline on the last utterance's audio (re-transcribe +
    /// cleanup + inject at the current cursor). Reached only via the control
    /// socket ("retry") or the menu-bar item. A no-op when nothing has been
    /// dictated yet this session.
    Retry,
    /// Reprocess a saved recording and copy only; never activate or paste.
    Recover { id: i64 },
    /// Quit is underway: leave the worker loop so the models — and the Metal
    /// buffers they hold — are dropped while Metal is still alive. Sent by the
    /// AppKit delegate's release hook; see `crate::shutdown`.
    Shutdown,
}

pub fn run(config: DaemonConfig) -> eyre::Result<()> {
    // A panic anywhere — worker thread included — must reach the log and the
    // menu bar. Installed first so it covers everything below.
    crate::health::install_panic_hook();

    // First line of every run: which build this log belongs to.
    eprintln!("[boot] {}", crate::settings_window::version_line());

    if let Some(problem) = unsupported_architecture() {
        return Err(eyre::eyre!("{problem}"));
    }

    // One daemon per user. Two would install two event taps and paste every
    // dictation twice. The guard WAITS rather than exits, because the menu's
    // "Apply and restart" spawns the replacement before this process ends.
    match crate::single_instance::acquire(
        &crate::single_instance::default_path(),
        crate::single_instance::HANDOFF_WAIT,
    ) {
        // Parked in the module, because the hand-over happens at quit, from
        // the AppKit delegate — after the models are gone, not before.
        crate::single_instance::Outcome::Held(lock) => crate::single_instance::hold(lock),
        crate::single_instance::Outcome::Busy { pid } => {
            let who = pid.map(|p| format!(" (pid {p})")).unwrap_or_default();
            eprintln!("[boot] Local Dictation is already running{who} — leaving it alone.");
            return Ok(());
        }
        crate::single_instance::Outcome::Unavailable { reason } => {
            eprintln!("[warn] no single-instance lock ({reason}) — starting unguarded");
        }
    }
    // After the lock: until then a departing instance may still be recording.
    crate::audio_duck::restore_after_crash();

    // Ask for Accessibility ONCE with the prompting check: that is what puts
    // this app in the System Settings list at all. Whatever the answer, the
    // daemon carries on — the menu bar appears, the models load, and the
    // event tap is installed the moment the grant lands. Exiting here is how
    // a first launch used to end with a permission dialog and then nothing.
    let trusted_at_boot = AccessibilityInjector::check_permission();
    eprintln!(
        "[boot] accessibility {}",
        if trusted_at_boot {
            "granted"
        } else {
            "NOT granted — the hotkey starts working as soon as you turn it on"
        }
    );

    // Ask for the microphone now, not in the middle of the user's first
    // dictation. An already-answered status returns without a prompt.
    if crate::permissions::microphone_status() == crate::permissions::Grant::Undecided {
        crate::permissions::request_microphone(|grant| {
            eprintln!("[boot] microphone   {}", grant.label());
            crate::permissions::refresh_microphone_health();
        });
    }
    crate::permissions::refresh_microphone_health();

    // "Copy last dictation" must survive a relaunch (every settings change is
    // one): re-read the newest transcript from the history store. With text
    // history off there is nothing to read and it stays memory-only.
    if crate::last_dictation::seed_from_history() {
        eprintln!("[boot] last       dictation restored from history");
    }

    let (tx, rx) = mpsc::channel::<DaemonEvent>();

    // The tap's settings, before the config moves to the worker thread.
    let hotkey = config.hotkey;
    let double_tap_latch = config.double_tap_latch;
    let copy_last = config.copy_last_hotkey;

    // Only the event tap needs Accessibility, so the models load straight
    // away whatever the grant says. "Starting up…" is a visible state until
    // the worker reports ready.
    crate::health::raise(crate::health::Fault::Booting, "loading models");

    // Spawn the worker thread that owns the models + capture engine. It
    // outlives the CFRunLoop on the main thread until shutdown.
    let worker_handle = std::thread::Builder::new()
        .name("dictation-worker".into())
        .spawn(move || {
            // A drop guard, not a straight call: a panic inside worker_loop
            // used to skip mark_released entirely, so Quit then waited out the
            // whole model-release timeout with the menu bar still on Idle.
            let _released = ReleaseOnExit;
            match worker_loop(rx, config) {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("[daemon] worker exited with error: {e:?}");
                    crate::health::raise(crate::health::Fault::EngineFailed, e.to_string());
                }
            }
        })?;

    eprintln!("[daemon] installing event tap on main run loop...");

    let modifier_only = install_event_tap(&tx, hotkey, double_tap_latch, copy_last);

    let hotkey_label = hotkey.display();
    if modifier_only {
        eprintln!(
            "[boot] ready · hold {hotkey_label} to dictate · \
             {hotkey_label}+Space then release = hands-free (tap {hotkey_label} to stop) · Esc cancels · ⌘Q quits"
        );
    } else {
        eprintln!(
            "[boot] ready · hold {hotkey_label} to dictate · Esc cancels · ⌘Q quits \
             (hands-free Space chord is not available for a chord hotkey)"
        );
    }
    eprintln!();
    ui_channel::set_state(UiState::Idle);

    // Local control socket: let macOS Shortcuts / Raycast / a Stream Deck button
    // / a foot pedal drive the same start/stop/cancel flow as the hotkey, with
    // no second model load. The server resolves `toggle` against the live
    // recording state, then forwards a concrete command onto the SAME channel
    // the event tap uses, so it funnels through the worker's existing guards.
    let tx_for_ctl = tx.clone();
    crate::ipc::serve(move |cmd| {
        use crate::ipc::Command;
        // Every socket command is logged: it is the one path that ends a
        // session with no key involved, so it must never be invisible.
        eprintln!("[ipc] {} control socket · {cmd:?}", stamp());
        let _ = match cmd {
            Command::Start => tx_for_ctl.send(DaemonEvent::StartRecording { transform: false }),
            Command::Stop => tx_for_ctl.send(DaemonEvent::StopRecording),
            Command::Cancel => {
                // Signal the out-of-band cancel gate too, so an IPC cancel lands
                // even if the worker is blocked mid-pipeline (see `crate::cancel`).
                ui_channel::request_cancel();
                tx_for_ctl.send(DaemonEvent::CancelRecording)
            }
            Command::Retry => tx_for_ctl.send(DaemonEvent::Retry),
            // Same code path as the hotkey and the menu item; off the socket
            // thread so a slow pasteboard cannot hold the reply.
            Command::CopyLast => {
                std::thread::spawn(|| {
                    crate::recovery::copy_last_dictation("ipc");
                });
                Ok(())
            }
            Command::Recover(id) => tx_for_ctl.send(DaemonEvent::Recover { id }),
            // The server resolves Toggle → Start/Stop before dispatch, so a
            // Toggle reaching here would be a bug; treat it as a no-op.
            Command::Toggle => Ok(()),
            // Status is answered by the socket thread straight from
            // DAEMON_RECORDING, so it never reaches this closure either.
            Command::Status => Ok(()),
        };
    });

    // Publish what this app can do, so Quick Launch (and anything else reading
    // the house command manifests) can list our commands without knowing us by
    // name. Best-effort by design: a failure logs and is ignored.
    crate::ipc::write_manifest();

    // Hand the main thread to NSApplication. AppKit pumps the same
    // CFRunLoop that our event tap is attached to, so the tap + menu bar +
    // pill all coexist on the same loop.
    // Quit goes through `-[NSApplication terminate:]`, which calls `exit()`
    // from inside `NSApp.run()` — so everything below `init_and_run` is dead
    // code, and `exit()` used to destroy ggml's static Metal device while this
    // worker still held model buffers, aborting the process. Register the
    // ordered release the AppKit delegate performs from
    // `applicationWillTerminate:`, before Metal goes away.
    let tx_for_shutdown = tx.clone();
    crate::shutdown::on_release_requested(move || {
        let _ = tx_for_shutdown.send(DaemonEvent::Shutdown);
    });

    menubar::init_and_run()?;

    // Only reached if NSApp.run() ever returns without terminating (it does not
    // today — terminate: exits from inside it). Harmless either way.
    drop(tx);
    let _ = worker_handle.join();
    Ok(())
}

/// The VAD config for the push-to-talk streaming path: whisper mode uses a
/// lower speech threshold so quiet dictation is captured, and every mode uses
/// the longer streaming pause so hesitations inside a clause do not become
/// separate, separately-punctuated ASR requests.
fn vad_config(whisper: bool) -> crate::vad::VadConfig {
    if whisper {
        crate::vad::VadConfig::whisper().streaming()
    } else {
        crate::vad::VadConfig::default().streaming()
    }
}

/// Wall-clock stamp for the session-control log lines (`HH:MM:SS.mmm`,
/// local time). The daemon log had no timestamps and no stop reasons, which
/// made a "stopped by itself" report impossible to check (2026-09-09).
pub(crate) fn stamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs() as i64;
    let ms = now.subsec_millis();
    // Local offset via libc, no chrono dependency.
    extern "C" {
        fn localtime_r(t: *const i64, tm: *mut Tm) -> *mut Tm;
    }
    #[repr(C)]
    struct Tm {
        tm_sec: i32,
        tm_min: i32,
        tm_hour: i32,
        tm_mday: i32,
        tm_mon: i32,
        tm_year: i32,
        tm_wday: i32,
        tm_yday: i32,
        tm_isdst: i32,
        tm_gmtoff: i64,
        tm_zone: *const std::os::raw::c_char,
    }
    let mut tm = Tm {
        tm_sec: 0, tm_min: 0, tm_hour: 0, tm_mday: 0, tm_mon: 0, tm_year: 0,
        tm_wday: 0, tm_yday: 0, tm_isdst: 0, tm_gmtoff: 0, tm_zone: std::ptr::null(),
    };
    unsafe { localtime_r(&secs, &mut tm) };
    format!("{:02}:{:02}:{:02}.{:03}", tm.tm_hour, tm.tm_min, tm.tm_sec, ms)
}

#[cfg(test)]
mod tests;
