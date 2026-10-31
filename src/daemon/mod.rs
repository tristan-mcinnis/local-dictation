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

mod cleanup;
mod deliver;
mod ptt;
mod worker;
mod listen;
use cleanup::*;
use deliver::*;
use ptt::*;
use worker::*;
pub use listen::run_listen;
use listen::*;

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


// Raw CFMachPortRef of the live event tap, stashed after creation so the
// callback can re-enable the tap if macOS ever disables it. Active taps can be
// killed by a slow callback or certain user input; without re-enabling, the
// hotkey would silently die. Null until the tap exists.
static TAP_PORT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// How many times macOS has disabled the event tap. A disabled tap drops every
/// key event until it is re-armed, so this count is the difference between
/// "nothing was pressed" and "we were not listening".
static TAP_DISABLES: AtomicU64 = AtomicU64::new(0);

/// Set by the worker when it ends a recording without a key (the recording
/// limit). The tap owns the hold/latch state, so it folds the reset in on its
/// next event: otherwise a lost key-up leaves `ST_HOLDING` behind and the next
/// real press is ignored as a duplicate.
static PTT_RESYNC: AtomicBool = AtomicBool::new(false);

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

/// Apply a pending [`PTT_RESYNC`]: back to idle, key considered up. If the key
/// is in fact still held, its eventual release is a no-op edge.
fn apply_ptt_resync(pending: &AtomicBool, state: &AtomicU8, hotkey_down: &AtomicBool) -> bool {
    if !pending.swap(false, Ordering::SeqCst) {
        return false;
    }
    state.store(ST_IDLE, Ordering::SeqCst);
    hotkey_down.store(false, Ordering::SeqCst);
    true
}

extern "C" {
    /// CoreGraphics: enable or disable a previously-created event tap.
    /// `void CGEventTapEnable(CFMachPortRef tap, bool enable);`
    fn CGEventTapEnable(tap: *const c_void, enable: bool);

    /// CoreGraphics: the live modifier state for a source.
    /// `CGEventFlags CGEventSourceFlagsState(CGEventSourceStateID stateID);`
    /// Read with `kCGEventSourceStateHIDSystemState` this is what the hardware
    /// reports right now, independent of the flags a single event carries.
    fn CGEventSourceFlagsState(state_id: i32) -> u64;
}

/// The hardware's live modifier flags, bypassing the event under inspection.
/// A physical key press shows up here; a state change another process injected
/// into the event stream does not.
fn hid_modifier_flags() -> u64 {
    unsafe { CGEventSourceFlagsState(CGEventSourceStateID::HIDSystemState as i32) }
}

/// Configuration for the daemon. Built from CLI args / env vars in main.rs.
pub struct DaemonConfig {
    /// Resolved fixed Cohere MLX ASR configuration.
    pub asr: crate::transcriber::AsrConfig,
    #[cfg(feature = "cleaner")]
    pub llm_model_path: String,
    #[cfg(feature = "cleaner")]
    pub instruction_model_path: Option<String>,
    /// If true, inject the raw ASR transcript without model cleanup.
    pub no_cleanup: bool,
    /// Hotkey keycode to watch on FlagsChanged.
    pub hotkey_keycode: i64,
}

impl DaemonConfig {
    pub fn from_env(
        asr: crate::transcriber::AsrConfig,
        #[cfg(feature = "cleaner")] llm_model_path: String,
        no_cleanup: bool,
    ) -> Self {
        // Through `Hotkey::from_settings` so an unusable configured key (a
        // plain typing key with no modifier) falls back the same way here as
        // in the event tap.
        let hotkey_keycode = crate::hotkeys::Hotkey::from_settings().keycode;
        Self {
            asr,
            #[cfg(feature = "cleaner")]
            llm_model_path,
            #[cfg(feature = "cleaner")]
            instruction_model_path: {
                let default = crate::app_paths::instruction_default_path();
                crate::settings::Settings::load().resolve_instruction_model(default.as_deref())
            },
            no_cleanup,
            hotkey_keycode,
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

/// How a tail-capture grace loop ended, when it was cut short by an event rather
/// than by a natural quiet run / cap. The StopRecording arm uses this to decide
/// whether to restart a fresh recording (the user re-pressed mid-grace) or cancel
/// the abandoned utterance, instead of proceeding to release the first one.
#[derive(Debug)]
enum TailGraceOutcome {
    /// The user pressed again during the grace tail: abandon the previous
    /// utterance and start a new recording cleanly. Carries the new gesture.
    RePress {
        transform: bool,
    },
    /// An external cancel arrived during the grace tail: discard everything.
    Cancel,
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

    // The CGEventTap callback runs on the main CFRunLoop thread. Keep it
    // FAST — just send to the worker.
    let tx_for_callback = tx.clone();
    let hotkey = crate::hotkeys::Hotkey::from_settings();
    let hotkey_keycode = hotkey.keycode;
    let modifier_only = hotkey.is_modifier_only();
    // A modifier-only hotkey (Right Option etc.) is detected via FlagsChanged
    // on its own modifier bit; a chord hotkey is detected via KeyDown/KeyUp of
    // the non-modifier key while the required modifiers are held.
    let required_mask = if modifier_only {
        CGEventFlags::empty()
    } else {
        modifiers_from_mask(hotkey.modifiers)
    };

    // Hands-free latch state shared with the callback. `state` is the ST_*
    // machine driven by `ptt_transition`; `space_down` lets us swallow the
    // Space key-up that matches a swallowed key-down so no stray space ever
    // lands in the focused field.
    let state = Arc::new(AtomicU8::new(ST_IDLE));
    let space_down = Arc::new(AtomicBool::new(false));
    let state_cb = state.clone();
    let space_down_cb = space_down.clone();
    // When Space was last pressed. The keyboard scans keys in hardware order,
    // so the Space half of Option+Space can reach the tap before the Option
    // edge; the press handler consults this to still recognise the chord
    // (see `CHORD_WINDOW`).
    let last_space_down = Arc::new(Mutex::new(None::<Instant>));
    let last_space_down_cb = last_space_down.clone();
    // Track physical edges separately from latch state. Repeated key-downs
    // must never be mistaken for a new tap that stops hands-free capture.
    let hotkey_down = Arc::new(AtomicBool::new(false));
    let hotkey_down_cb = hotkey_down.clone();

    // Double-tap-to-latch: the callback records press/release timing to
    // recognise a hotkey double-tap (the no-Spacebar route into hands-free).
    let double_tap_latch = crate::settings::Settings::load().resolve_double_tap_latch();
    let detector_cb = Arc::new(Mutex::new(DoubleTapDetector::default()));

    // Drive the latch state machine once, forwarding any Start/Stop decision to
    // the worker. Shared by the modifier-only and chord hotkey detection paths.
    let drive = {
        let tx = tx_for_callback.clone();
        let scb = state_cb.clone();
        move |input: PttInput| {
            let prev = scb.load(Ordering::SeqCst);
            let (next, decision) = ptt_transition(prev, input);
            scb.store(next, Ordering::SeqCst);
            if !matches!(decision, PttDecision::Nothing | PttDecision::SwallowSpace) || prev != next {
                eprintln!(
                    "[hotkey] {} {input:?} · state {prev}→{next} · {decision:?}",
                    stamp()
                );
            }
            match decision {
                PttDecision::Start { transform } => {
                    if !report_if_unavailable() {
                        let _ = tx.send(DaemonEvent::StartRecording { transform });
                    }
                }
                // Double-tap from idle: begin recording AND cue latched,
                // mirroring the Space-chord Start + LatchArmed sequence.
                PttDecision::StartLatched { transform } => {
                    if !report_if_unavailable() {
                        let _ = tx.send(DaemonEvent::StartRecording { transform });
                        let _ = tx.send(DaemonEvent::LatchArmed);
                    }
                }
                PttDecision::Stop => {
                    let why = if prev == ST_LATCHED { "hotkey tap ended hands-free" } else { "hotkey released" };
                    eprintln!("[hotkey] {} stop · {why}", stamp());
                    let _ = tx.send(DaemonEvent::StopRecording);
                }
                _ => {}
            }
        }
    };

    // The second global hotkey: "Copy last dictation". The callback only sends
    // on this channel; the clipboard write and the pill happen on a small
    // dedicated thread, never behind the worker (which blocks during ASR).
    let copy_last = crate::hotkeys::copy_last_from_settings();
    match copy_last {
        Some(h) => eprintln!("[boot] copy-last  hotkey {}", h.display()),
        None => eprintln!("[boot] copy-last  hotkey off"),
    }
    let (copy_tx, copy_rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        while copy_rx.recv().is_ok() {
            crate::recovery::copy_last_dictation("hotkey");
        }
    });
    let copy_key_down = Arc::new(AtomicBool::new(false));
    let copy_key_down_cb = copy_key_down.clone();
    let resync_state = state.clone();
    let resync_down = hotkey_down.clone();
    let own_pid = i64::from(std::process::id());

    // Install CGEventTap manually so we can hand the main thread over to
    // NSApplication.run() (needed for the menu-bar item + floating pill).
    //
    // This is an ACTIVE tap (not ListenOnly) because hands-free mode must
    // *swallow* the Space key while the PTT key is held — otherwise the held
    // modifier + Space would insert a character (e.g. Option+Space → nbsp).
    // The callback stays trivial (atomics + non-blocking channel sends) so it
    // can never stall the input stream; TapDisabled events re-enable the tap.
    //
    // The whole installation is deferred into a closure because it is the one
    // part of the daemon that needs Accessibility. `run_when_accessibility_
    // granted` runs it now when the grant is already there, and otherwise
    // holds it until the Setup window's polling sees the grant land — no
    // relaunch, and no exit in the meantime.
    let install_tap = move || -> Result<(), String> {
    let tap = CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::Default,
        vec![
            CGEventType::FlagsChanged,
            CGEventType::KeyDown,
            CGEventType::KeyUp,
        ],
        move |_proxy, etype, event| {
            match etype {
                CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                    // macOS disabled the tap. Every key event in the disabled
                    // window is dropped, so this must be visible in the log: a
                    // dropped release leaves the hold state stale and the next
                    // edge is then read against the wrong physical state.
                    // Re-arm immediately; the callback stays short.
                    let n = TAP_DISABLES.fetch_add(1, Ordering::SeqCst) + 1;
                    eprintln!(
                        "[hotkey] {} TAP DISABLED ({etype:?}) · #{n} · re-arming",
                        stamp()
                    );
                    let p = TAP_PORT.load(Ordering::SeqCst);
                    if !p.is_null() {
                        unsafe { CGEventTapEnable(p, true) };
                    }
                }
                CGEventType::FlagsChanged => {
                    apply_ptt_resync(&PTT_RESYNC, &resync_state, &resync_down);
                    // Only the modifier-only hotkey uses FlagsChanged; a chord
                    // hotkey reads its modifiers off the KeyDown event's flags.
                    if !modifier_only {
                        return CallbackResult::Keep;
                    }
                    let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
                    if keycode != hotkey_keycode {
                        return CallbackResult::Keep;
                    }
                    let flags = event.get_flags();
                    let raw = flags.bits();
                    // Provenance probe. Every edge is logged with where it came
                    // from, because a stop with no finger behind it is
                    // otherwise indistinguishable from a real tap:
                    //   pid/state/uid — who posted it (0 = the HID system)
                    //   read          — device bit (side known) vs aggregate
                    //   hw            — the HID system's flags for the same key
                    // `hw` trails the event by a few milliseconds as a rule: the
                    // tap sits at the head of the stream, so it sees each edge
                    // before the HID state commits it. A mismatch is therefore
                    // expected on every edge and is NOT evidence of injection.
                    // What it does prove is that the key really was down for the
                    // interval the tap believed it was.
                    let src_pid = event.get_integer_value_field(EventField::EVENT_SOURCE_UNIX_PROCESS_ID);
                    let src_state = event.get_integer_value_field(EventField::EVENT_SOURCE_STATE_ID);
                    let src_uid = event.get_integer_value_field(EventField::EVENT_SOURCE_USER_ID);
                    let hw = hid_modifier_flags();
                    let down = crate::hotkeys::modifier_key_is_down(hotkey_keycode, raw);
                    eprintln!(
                        "[hotkey] {} edge · key {hotkey_keycode:#x} · {} · flags {raw:#x} · read {} · hw {hw:#x} · src pid {src_pid} state {src_state} uid {src_uid}",
                        stamp(),
                        if down { "DOWN" } else { "UP" },
                        if crate::hotkeys::modifier_key_read_via_aggregate(hotkey_keycode, raw) {
                            "aggregate"
                        } else {
                            "device-bit"
                        },
                    );
                    // Only a physical key controls the session. A FlagsChanged
                    // posted by another process (AppleScript, an automation
                    // tool, a remote's companion app) carries that process's
                    // PID here; a physical keyboard carries 0.
                    // Without this guard, any app posting Right Option ends a
                    // hands-free session and pastes the transcript wherever
                    // focus is. A poster that leaves the PID unset still slips
                    // through, so `src pid 0` is a weak signal, not a strong one.
                    if src_pid != 0 {
                        eprintln!(
                            "[hotkey] {} ignored synthesized hotkey event from pid {src_pid} (flags {raw:#x})",
                            stamp()
                        );
                        return CallbackResult::Keep;
                    }
                    // One physical edge only. Repeated key-downs must never be
                    // mistaken for a fresh tap that stops hands-free capture;
                    // log the repeats so a stuck or bouncing device is visible.
                    if !hotkey_edge(&hotkey_down_cb, down) {
                        eprintln!(
                            "[hotkey] {} duplicate {} edge ignored · flags {raw:#x} · hw {hw:#x}",
                            stamp(),
                            if down { "down" } else { "up" }
                        );
                        return CallbackResult::Keep;
                    }
                    if down {
                        // Shift co-held at press → transform mode (rewrite the
                        // selection). The user holds Shift *before* the PTT key.
                        let press = Instant::now();
                        let double_tap = double_tap_latch
                            && detector_cb.lock().unwrap().on_press(press);
                        // Did a Space just precede this edge? The keyboard scans
                        // keys in hardware order, so the chord can arrive
                        // Space-first; without this the press reads as a bare
                        // hold and the session dies the moment the keys come up.
                        let space_first = last_space_down_cb
                            .lock()
                            .unwrap()
                            .map(|t| t.elapsed() <= CHORD_WINDOW)
                            .unwrap_or(false);
                        if space_first {
                            eprintln!(
                                "[hotkey] {} space landed before the press · reading it as the hands-free chord",
                                stamp()
                            );
                            drive(PttInput::HotkeyChordSpace);
                        } else {
                            drive(PttInput::HotkeyPress {
                                shift: flags.contains(CGEventFlags::CGEventFlagShift),
                                double_tap,
                            });
                        }
                    } else {
                        detector_cb.lock().unwrap().on_release(Instant::now());
                        drive(PttInput::HotkeyRelease);
                    }
                    // FlagsChanged is never swallowed — let the modifier through.
                }
                CGEventType::KeyDown => {
                    apply_ptt_resync(&PTT_RESYNC, &resync_state, &resync_down);
                    let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
                    let autorepeat =
                        event.get_integer_value_field(EventField::KEYBOARD_EVENT_AUTOREPEAT) != 0;
                    // "Copy last dictation". Exact modifiers, so our own
                    // synthesized ⌘V can never match; events we posted are
                    // ignored outright. Autorepeats of the held chord are
                    // swallowed without copying again.
                    if let Some(chord) = copy_last {
                        if crate::hotkeys::copy_last_matches(&chord, keycode, event.get_flags().bits())
                            && event.get_integer_value_field(EventField::EVENT_SOURCE_UNIX_PROCESS_ID) != own_pid
                        {
                            if !autorepeat && !copy_key_down_cb.swap(true, Ordering::SeqCst) {
                                let _ = copy_tx.send(());
                            }
                            return CallbackResult::Drop;
                        }
                    }
                    // A fresh key press dismisses a "Copied, press ⌘V" pill.
                    if !autorepeat {
                        ui_channel::key_pressed();
                    }
                    // Escape cancels an in-flight utterance (recording or pending
                    // transcription) and prevents any late insertion. It swallows
                    // the keystroke while active so it never leaks into the field;
                    // when idle it passes through so normal Escape use (dismiss a
                    // dialog, end a drag) is unaffected.
                    if keycode == ESCAPE_KEYCODE {
                        // An utterance is in flight while the worker is capturing
                        // (`Recording`) or running the release pipeline
                        // (`Processing`). This is the reliable signal — the worker
                        // sets it at begin and reverts to `Idle`/`Success`/
                        // `Error` at settle, so an idle Escape (no utterance) is
                        // never mistaken for a cancel.
                        let in_flight = ui_channel::state().is_cancellable();
                        // Only swallow + wake + unwind when a cancel was actually
                        // ACCEPTED. `request_cancel` returns false when idle or when
                        // the utterance already committed to write — in that case the
                        // Escape must pass through untouched, not be silently eaten
                        // (and the PTT latch must not be reset out from under a live
                        // post-commit settle).
                        if in_flight && ui_channel::request_cancel() {
                            // Wake a worker blocked waiting for an event (recording)
                            // so capture stops now rather than at release. Harmless
                            // if it's already mid-pipeline: the out-of-band gate is
                            // what suppresses the eventual injection.
                            let _ = tx_for_callback.send(DaemonEvent::CancelRecording);
                            // Cancellation ends the session, so unwind the PTT latch
                            // state machine too — otherwise a hands-free session left
                            // a `ST_LATCHED` the next PTT tap would have to clear.
                            state_cb.store(ST_IDLE, Ordering::SeqCst);
                            return CallbackResult::Drop;
                        }
                        return CallbackResult::Keep;
                    }
                    if !modifier_only && keycode == hotkey_keycode {
                        // Chord hotkey: the non-modifier key went down with the
                        // required modifiers held. Swallow it so the keystroke
                        // never reaches the app (no stray character / menu).
                        // The four modifier bits must EQUAL the chord's (an
                        // extra Shift is the transform gesture), so ⌘⇧D does
                        // not also swallow another app's ⌘⇧⌥D.
                        let flags = event.get_flags();
                        if crate::hotkeys::chord_modifiers_match(flags.bits(), hotkey.modifiers) {
                            // Key repeat is still the same physical press. In
                            // double-tap hands-free, treating it as a fresh tap
                            // would immediately stop capture before release.
                            if !hotkey_edge(&hotkey_down_cb, true) {
                                return CallbackResult::Drop;
                            }
                            let shift = flags.contains(CGEventFlags::CGEventFlagShift)
                                && !required_mask.contains(CGEventFlags::CGEventFlagShift);
                            let press = Instant::now();
                            let double_tap = double_tap_latch
                                && detector_cb.lock().unwrap().on_press(press);
                            drive(PttInput::HotkeyPress { shift, double_tap });
                            return CallbackResult::Drop;
                        }
                        return CallbackResult::Keep;
                    }
                    if modifier_only && keycode == SPACE_KEYCODE {
                        // Half of the Option+Space chord. Remember when it landed
                        // so the modifier edge that follows can still recognise
                        // the pair if the hardware sent Space first.
                        *last_space_down_cb.lock().unwrap() = Some(Instant::now());
                        let prev = state_cb.load(Ordering::SeqCst);
                        let (next, decision) = ptt_transition(prev, PttInput::SpaceDown);
                        state_cb.store(next, Ordering::SeqCst);
                        let swallowed = matches!(
                            decision,
                            PttDecision::ArmLatch | PttDecision::SwallowSpace
                        );
                        if swallowed {
                            space_down_cb.store(true, Ordering::SeqCst);
                        }
                        // Space used to be invisible in this log, so a chord that
                        // never armed left no trace of why. Only dictation-relevant
                        // Spaces are logged; ordinary typing would flood the file.
                        if swallowed || prev != ST_IDLE {
                            eprintln!(
                                "[hotkey] {} space DOWN · state {prev}→{next} · {decision:?} · {}",
                                stamp(),
                                if swallowed { "swallowed" } else { "passed through" }
                            );
                        }
                        match decision {
                            PttDecision::ArmLatch => {
                                // Chord recognised: cue + swallow so the
                                // Option+Space character never reaches the field.
                                let _ = tx_for_callback.send(DaemonEvent::LatchArmed);
                                return CallbackResult::Drop;
                            }
                            PttDecision::SwallowSpace => return CallbackResult::Drop,
                            _ => {}
                        }
                    }
                }
                CGEventType::KeyUp => {
                    let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
                    // Swallow the key-up that matches a swallowed copy-last
                    // key-down, whatever modifiers are still held by now.
                    if copy_last.is_some_and(|c| c.keycode == keycode)
                        && copy_key_down_cb.swap(false, Ordering::SeqCst)
                    {
                        return CallbackResult::Drop;
                    }
                    if !modifier_only && keycode == hotkey_keycode {
                        // Chord hotkey release: swallow the matching key-up.
                        if hotkey_edge(&hotkey_down_cb, false) {
                            detector_cb.lock().unwrap().on_release(Instant::now());
                            drive(PttInput::HotkeyRelease);
                            return CallbackResult::Drop;
                        }
                        return CallbackResult::Keep;
                    }
                    if modifier_only && keycode == SPACE_KEYCODE {
                        let was_swallowed = space_down_cb.swap(false, Ordering::SeqCst);
                        if was_swallowed || state_cb.load(Ordering::SeqCst) != ST_IDLE {
                            eprintln!(
                                "[hotkey] {} space UP · {}",
                                stamp(),
                                if was_swallowed { "swallowed" } else { "passed through" }
                            );
                        }
                        // Swallow the key-up matching a swallowed key-down.
                        if was_swallowed {
                            return CallbackResult::Drop;
                        }
                    }
                }
                _ => {}
            }
            CallbackResult::Keep
        },
    )
    .map_err(|_| {
        "could not install the event tap — Accessibility is missing or was revoked".to_string()
    })?;

    // Stash the tap's port so the callback can re-enable it after a
    // system-initiated disable (see TapDisabled handling above).
    TAP_PORT.store(
        tap.mach_port().as_concrete_TypeRef() as *mut c_void,
        Ordering::SeqCst,
    );

    let loop_source = tap
        .mach_port()
        .create_runloop_source(0)
        .map_err(|_| "create_runloop_source failed".to_string())?;
    CFRunLoop::get_current().add_source(&loop_source, unsafe { kCFRunLoopCommonModes });
    tap.enable();
    // Leak the tap — it must outlive the run loop. Safe to drop only on
    // process exit (which NSApp.terminate handles).
    std::mem::forget(tap);
    std::mem::forget(loop_source);
    eprintln!("[boot] event tap  installed");
    Ok(())
    };
    crate::permissions::run_when_accessibility_granted(install_tap);

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

/// Convert a hotkey modifier mask (raw CGEventFlags bits) into a CGEventFlags
/// set. Used to turn the settings-stored `u64` into the flag bits the event tap
/// checks against the live key-event flags.
fn modifiers_from_mask(mask: u64) -> CGEventFlags {
    let mut f = CGEventFlags::empty();
    if mask & crate::hotkeys::SHIFT != 0 {
        f |= CGEventFlags::CGEventFlagShift;
    }
    if mask & crate::hotkeys::CONTROL != 0 {
        f |= CGEventFlags::CGEventFlagControl;
    }
    if mask & crate::hotkeys::OPTION != 0 {
        f |= CGEventFlags::CGEventFlagAlternate;
    }
    if mask & crate::hotkeys::COMMAND != 0 {
        f |= CGEventFlags::CGEventFlagCommand;
    }
    if mask & crate::hotkeys::CAPS_LOCK != 0 {
        f |= CGEventFlags::CGEventFlagAlphaShift;
    }
    f
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
mod tests {
    use super::*;

    #[test]
    fn an_intel_mac_is_told_plainly_instead_of_failing_somewhere_deeper() {
        // The bundle is arm64-only and the Swift MLX helper refuses to build
        // anywhere else, so an x86 run used to end as a Finder "can't open
        // this application" or a link error inside a build script.
        assert_eq!(architecture_problem("aarch64"), None);
        for arch in ["x86_64", "x86", "powerpc"] {
            let problem = architecture_problem(arch).expect(arch);
            assert!(problem.contains("Apple Silicon"), "{problem}");
            assert!(problem.contains(arch), "{problem}");
        }
        // This build itself must be on a supported machine.
        assert_eq!(unsupported_architecture(), None);
    }

    #[test]
    fn a_limit_stop_resets_a_stuck_hold_so_the_next_press_starts_cleanly() {
        // Regression (PB-F06): a lost key-up left ST_HOLDING + hotkey_down set;
        // the next real press was then dropped as a duplicate edge.
        let pending = AtomicBool::new(true);
        let state = AtomicU8::new(ST_HOLDING);
        let down = AtomicBool::new(true);
        assert!(apply_ptt_resync(&pending, &state, &down));
        assert_eq!(state.load(Ordering::SeqCst), ST_IDLE);
        assert!(hotkey_edge(&down, true), "the next press is a fresh edge");
        let (next, decision) = ptt_transition(
            state.load(Ordering::SeqCst),
            PttInput::HotkeyPress { shift: false, double_tap: false },
        );
        assert_eq!((next, decision), (ST_HOLDING, PttDecision::Start { transform: false }));
        // Applied once only.
        state.store(ST_LATCHED, Ordering::SeqCst);
        assert!(!apply_ptt_resync(&pending, &state, &down));
        assert_eq!(state.load(Ordering::SeqCst), ST_LATCHED);
    }

    #[test]
    fn a_key_still_held_at_the_limit_releases_as_a_no_op() {
        let pending = AtomicBool::new(true);
        let state = AtomicU8::new(ST_HOLDING);
        let down = AtomicBool::new(true);
        apply_ptt_resync(&pending, &state, &down);
        assert!(!hotkey_edge(&down, false), "release after resync is not an edge");
    }

    #[test]
    fn recording_limit_is_off_at_zero_and_exact_at_the_minute() {
        assert!(!recording_limit_reached(Duration::from_secs(86_400), 0));
        assert!(!recording_limit_reached(Duration::from_secs(599), 10));
        assert!(recording_limit_reached(Duration::from_secs(600), 10));
    }

    #[test]
    fn repeated_second_press_of_double_tap_cannot_stop_hands_free() {
        let down = AtomicBool::new(false);
        let mut state = ST_IDLE;
        let mut decisions = Vec::new();
        // First tap, second press, autorepeats while held, then release.
        for (pressed, double_tap) in [
            (true, false), (false, false), (true, true),
            (true, false), (true, false), (false, false),
        ] {
            if hotkey_edge(&down, pressed) {
                let input = if pressed {
                    PttInput::HotkeyPress { shift: false, double_tap }
                } else {
                    PttInput::HotkeyRelease
                };
                let (next, decision) = ptt_transition(state, input);
                state = next;
                decisions.push(decision);
            }
        }
        assert_eq!(state, ST_LATCHED);
        assert_eq!(decisions, vec![
            PttDecision::Start { transform: false },
            PttDecision::Stop,
            PttDecision::StartLatched { transform: false },
            PttDecision::Nothing,
        ]);
        // A genuinely new tap still ends capture.
        assert!(hotkey_edge(&down, true));
        assert_eq!(
            ptt_transition(state, PttInput::HotkeyPress { shift: false, double_tap: false }),
            (ST_STOPPING_TAP, PttDecision::Stop)
        );
    }

    #[test]
    fn releasing_right_option_while_left_option_is_held_keeps_hands_free_recording() {
        // The second press of a double-tap has already latched recording.
        // Its release carries aggregate Option because Left Option is still down.
        let flags = CGEventFlags::from_bits_retain(crate::hotkeys::OPTION | 0x20);
        let input = if crate::hotkeys::modifier_key_is_down(0x3D, flags.bits()) {
            PttInput::HotkeyPress { shift: false, double_tap: false }
        } else {
            PttInput::HotkeyRelease
        };
        assert_eq!(ptt_transition(ST_LATCHED, input), (ST_LATCHED, PttDecision::Nothing));
    }

    #[test]
    fn space_landing_before_the_modifier_edge_still_arms_hands_free() {
        // The keyboard put Space on the wire before the Option edge, so the pair
        // arrives in that order. It is still the chord and must arm.
        let (state, decision) = ptt_transition(ST_IDLE, PttInput::HotkeyChordSpace);
        assert_eq!(state, ST_LATCHED);
        assert_eq!(decision, PttDecision::StartLatched { transform: false });
        // Releasing the keys leaves hands-free running, which is exactly what
        // the chord promises and what hold-to-talk would break.
        assert_eq!(
            ptt_transition(state, PttInput::HotkeyRelease),
            (ST_LATCHED, PttDecision::Nothing)
        );
        // A recent Space must never swallow a deliberate stop tap.
        assert_eq!(
            ptt_transition(state, PttInput::HotkeyChordSpace),
            (ST_STOPPING_TAP, PttDecision::Stop)
        );
    }

    /// Drive the state machine through a sequence of inputs, returning the
    /// (state, decision) at each step plus the final state. Mirrors exactly
    /// what the event-tap callback does: feed input, store next state.
    fn run(inputs: &[PttInput]) -> (Vec<PttDecision>, u8) {
        let mut state = ST_IDLE;
        let mut decisions = Vec::new();
        for &input in inputs {
            let (next, decision) = ptt_transition(state, input);
            state = next;
            decisions.push(decision);
        }
        (decisions, state)
    }

    #[test]
    fn normal_ptt_press_and_release() {
        // Hold then release: start on press, stop on release, back to idle.
        let (decisions, end) = run(&[
            PttInput::HotkeyPress { shift: false, double_tap: false },
            PttInput::HotkeyRelease,
        ]);
        assert_eq!(
            decisions,
            vec![PttDecision::Start { transform: false }, PttDecision::Stop]
        );
        assert_eq!(end, ST_IDLE);
    }

    #[test]
    fn shift_press_starts_transform() {
        let (decisions, _) = run(&[PttInput::HotkeyPress { shift: true, double_tap: false }]);
        assert_eq!(decisions, vec![PttDecision::Start { transform: true }]);
    }

    #[test]
    fn hands_free_full_gesture() {
        // Hold PTT, chord Space, release both, then tap PTT to stop.
        let (decisions, end) = run(&[
            PttInput::HotkeyPress { shift: false, double_tap: false }, // Start
            PttInput::SpaceDown,                    // ArmLatch (cue + swallow)
            PttInput::HotkeyRelease,                // → Latched, keep recording
            PttInput::HotkeyPress { shift: false, double_tap: false }, // tap → Stop
            PttInput::HotkeyRelease,                // → Idle, no-op
        ]);
        assert_eq!(
            decisions,
            vec![
                PttDecision::Start { transform: false },
                PttDecision::ArmLatch,
                PttDecision::Nothing,
                PttDecision::Stop,
                PttDecision::Nothing,
            ]
        );
        assert_eq!(end, ST_IDLE);
    }

    #[test]
    fn release_order_space_before_ptt_still_latches() {
        // Arming only needs the Space chord while holding; releasing the PTT
        // afterwards latches regardless of when Space physically came up.
        let (decisions, end) = run(&[
            PttInput::HotkeyPress { shift: false, double_tap: false },
            PttInput::SpaceDown,
            PttInput::HotkeyRelease,
        ]);
        assert_eq!(decisions.last(), Some(&PttDecision::Nothing));
        assert_eq!(end, ST_LATCHED);
    }

    #[test]
    fn repeated_space_while_armed_is_swallowed_not_re_cued() {
        // A second Space during the same hold must still be swallowed, but
        // emits SwallowSpace (no fresh cue) rather than ArmLatch.
        let (decisions, end) = run(&[
            PttInput::HotkeyPress { shift: false, double_tap: false },
            PttInput::SpaceDown,
            PttInput::SpaceDown,
        ]);
        assert_eq!(
            decisions,
            vec![
                PttDecision::Start { transform: false },
                PttDecision::ArmLatch,
                PttDecision::SwallowSpace,
            ]
        );
        assert_eq!(end, ST_HOLDING_ARMED);
    }

    #[test]
    fn stray_space_when_idle_is_passed_through() {
        // Space with no PTT held must NOT be swallowed (Nothing) so normal
        // typing is unaffected.
        let (decisions, end) = run(&[PttInput::SpaceDown]);
        assert_eq!(decisions, vec![PttDecision::Nothing]);
        assert_eq!(end, ST_IDLE);
    }

    #[test]
    fn space_during_latched_session_passes_through() {
        // Once hands-free, the keys are up; a Space is a real keystroke and
        // must not be swallowed or change state.
        let (decisions, end) = run(&[
            PttInput::HotkeyPress { shift: false, double_tap: false },
            PttInput::SpaceDown,
            PttInput::HotkeyRelease, // Latched
            PttInput::SpaceDown,     // real space
        ]);
        assert_eq!(decisions.last(), Some(&PttDecision::Nothing));
        assert_eq!(end, ST_LATCHED);
    }

    #[test]
    fn re_press_while_holding_is_noop() {
        // A spurious second press event while already holding does nothing
        // (and never starts a second recording).
        let (decisions, end) = run(&[
            PttInput::HotkeyPress { shift: false, double_tap: false },
            PttInput::HotkeyPress { shift: false, double_tap: false },
        ]);
        assert_eq!(
            decisions,
            vec![
                PttDecision::Start { transform: false },
                PttDecision::Nothing
            ]
        );
        assert_eq!(end, ST_HOLDING);
    }

    #[test]
    fn normal_then_handsfree_back_to_back() {
        // A plain hold/release, then a hands-free session — state returns to
        // idle cleanly between them.
        let (_decisions, end) = run(&[
            PttInput::HotkeyPress { shift: false, double_tap: false },
            PttInput::HotkeyRelease, // Stop → Idle
            PttInput::HotkeyPress { shift: false, double_tap: false },
            PttInput::SpaceDown,
            PttInput::HotkeyRelease, // Latched
            PttInput::HotkeyPress { shift: false, double_tap: false },
            PttInput::HotkeyRelease, // Stop → Idle
        ]);
        assert_eq!(end, ST_IDLE);
    }

    // Double-tap to latch. The first press is a normal quick tap (Start, then
    // Stop on release); it's the *second* press (double_tap=true) that latches.

    #[test]
    fn double_tap_from_idle_latches() {
        let (decisions, end) = run(&[
            PttInput::HotkeyPress { shift: false, double_tap: false }, // tap 1
            PttInput::HotkeyRelease,
            PttInput::HotkeyPress { shift: false, double_tap: true }, // tap 2 → latch
            PttInput::HotkeyRelease, // keys up, stays recording
        ]);
        assert_eq!(
            decisions,
            vec![
                PttDecision::Start { transform: false },
                PttDecision::Stop,
                PttDecision::StartLatched { transform: false },
                PttDecision::Nothing,
            ]
        );
        assert_eq!(end, ST_LATCHED);
    }

    #[test]
    fn double_tap_then_single_tap_stops() {
        let (decisions, end) = run(&[
            PttInput::HotkeyPress { shift: false, double_tap: false },
            PttInput::HotkeyRelease,
            PttInput::HotkeyPress { shift: false, double_tap: true }, // latch
            PttInput::HotkeyRelease,
            PttInput::HotkeyPress { shift: false, double_tap: false }, // stop tap
            PttInput::HotkeyRelease,
        ]);
        assert_eq!(
            decisions,
            vec![
                PttDecision::Start { transform: false },
                PttDecision::Stop,
                PttDecision::StartLatched { transform: false },
                PttDecision::Nothing,
                PttDecision::Stop,
                PttDecision::Nothing,
            ]
        );
        assert_eq!(end, ST_IDLE);
    }

    #[test]
    fn single_quick_tap_does_not_latch() {
        let (decisions, end) = run(&[
            PttInput::HotkeyPress { shift: false, double_tap: false },
            PttInput::HotkeyRelease,
        ]);
        assert_eq!(
            decisions,
            vec![PttDecision::Start { transform: false }, PttDecision::Stop]
        );
        assert_eq!(end, ST_IDLE);
    }

    #[test]
    fn hold_then_release_does_not_latch() {
        // A single long hold (double_tap is never set, since there's no second
        // press) is just a normal PTT utterance — never hands-free.
        let (_decisions, end) = run(&[
            PttInput::HotkeyPress { shift: false, double_tap: false },
            PttInput::HotkeyRelease,
        ]);
        assert_eq!(end, ST_IDLE);
    }

    #[test]
    fn double_tap_while_latched_stops() {
        // A press while already hands-free ends the session; the double-tap flag
        // is ignored once latched (ST_LATCHED → Stop on the first press, the
        // rest is a no-op).
        let (decisions, end) = run(&[
            PttInput::HotkeyPress { shift: false, double_tap: false },
            PttInput::SpaceDown,
            PttInput::HotkeyRelease, // latched via Space chord
            PttInput::HotkeyPress { shift: false, double_tap: true },
            PttInput::HotkeyRelease,
        ]);
        assert!(decisions.contains(&PttDecision::Stop));
        assert_eq!(end, ST_IDLE);
    }

    #[test]
    fn triple_tap_latches_then_stops() {
        // Three quick taps: the first is a normal utterance, the second latches
        // hands-free, the third stops it — net return to idle.
        let (_decisions, end) = run(&[
            PttInput::HotkeyPress { shift: false, double_tap: false },
            PttInput::HotkeyRelease,
            PttInput::HotkeyPress { shift: false, double_tap: true }, // latch
            PttInput::HotkeyRelease,
            PttInput::HotkeyPress { shift: false, double_tap: true }, // stop tap
            PttInput::HotkeyRelease,
        ]);
        assert_eq!(end, ST_IDLE);
    }
}
