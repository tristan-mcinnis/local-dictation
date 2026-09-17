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

mod ptt;
mod worker;
mod listen;
use ptt::*;
use worker::*;
pub use listen::run_listen;
use listen::*;

use crate::audio::{AudioCaptureEngine, BUFFER_CAPACITY};
use crate::corrections::Corrections;
use crate::cues;
use crate::injector::{AccessibilityInjector, FocusTarget};
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
use std::os::raw::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, Ordering};
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

extern "C" {
    /// CoreGraphics: enable or disable a previously-created event tap.
    /// `void CGEventTapEnable(CFMachPortRef tap, bool enable);`
    fn CGEventTapEnable(tap: *const c_void, enable: bool);
}

/// Configuration for the daemon. Built from CLI args / env vars in main.rs.
pub struct DaemonConfig {
    /// Resolved fixed Cohere MLX ASR configuration.
    pub asr: crate::transcriber::AsrConfig,
    #[cfg(feature = "cleaner")]
    pub llm_model_path: String,
    /// If true, inject the raw ASR transcript without Gemma cleanup.
    /// Saves ~860 ms per utterance.
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
        let hotkey_keycode =
            crate::settings::Settings::load().resolve_hotkey(crate::hotkeys::DEFAULT_KEYCODE);
        Self {
            asr,
            #[cfg(feature = "cleaner")]
            llm_model_path,
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
    // Surface the AX permission prompt up front — if we're not trusted, the
    // event tap below will silently fail.
    if !AccessibilityInjector::check_permission() {
        return Err(eyre::eyre!(
            "Accessibility permission required. macOS just showed the prompt — \
             enable this binary in System Settings → Privacy & Security → \
             Accessibility, then re-run."
        ));
    }

    let (tx, rx) = mpsc::channel::<DaemonEvent>();

    // Spawn the worker thread that owns the models + capture engine. It
    // outlives the CFRunLoop on the main thread until shutdown.
    let worker_handle = std::thread::spawn(move || {
        if let Err(e) = worker_loop(rx, config) {
            eprintln!("[daemon] worker exited with error: {e:?}");
        }
        // Every model this thread owned is dropped by now, so the process can
        // safely run ggml's static Metal teardown. Unblocks a quit that is
        // waiting on us (see `crate::shutdown`).
        crate::shutdown::mark_released();
    });

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

    // Warn (but do not block) when another app has already registered the same
    // combo system-wide — the hotkey may not fire reliably.
    if let Some(msg) = crate::hotkeys::detect_conflict(&hotkey) {
        eprintln!("[hotkey] ⚠ conflict: {msg}");
    }

    // Hands-free latch state shared with the callback. `state` is the ST_*
    // machine driven by `ptt_transition`; `space_down` lets us swallow the
    // Space key-up that matches a swallowed key-down so no stray space ever
    // lands in the focused field.
    let state = Arc::new(AtomicU8::new(ST_IDLE));
    let space_down = Arc::new(AtomicBool::new(false));
    let state_cb = state.clone();
    let space_down_cb = space_down.clone();
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
                    let _ = tx.send(DaemonEvent::StartRecording { transform });
                }
                // Double-tap from idle: begin recording AND cue latched,
                // mirroring the Space-chord Start + LatchArmed sequence.
                PttDecision::StartLatched { transform } => {
                    let _ = tx.send(DaemonEvent::StartRecording { transform });
                    let _ = tx.send(DaemonEvent::LatchArmed);
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

    // Install CGEventTap manually so we can hand the main thread over to
    // NSApplication.run() (needed for the menu-bar item + floating pill).
    //
    // This is an ACTIVE tap (not ListenOnly) because hands-free mode must
    // *swallow* the Space key while the PTT key is held — otherwise the held
    // modifier + Space would insert a character (e.g. Option+Space → nbsp).
    // The callback stays trivial (atomics + non-blocking channel sends) so it
    // can never stall the input stream; TapDisabled events re-enable the tap.
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
                    // macOS disabled the tap — re-arm it so the hotkey lives on.
                    let p = TAP_PORT.load(Ordering::SeqCst);
                    if !p.is_null() {
                        unsafe { CGEventTapEnable(p, true) };
                    }
                }
                CGEventType::FlagsChanged => {
                    // Only the modifier-only hotkey uses FlagsChanged; a chord
                    // hotkey reads its modifiers off the KeyDown event's flags.
                    if !modifier_only {
                        return CallbackResult::Keep;
                    }
                    let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
                    if keycode != hotkey_keycode {
                        return CallbackResult::Keep;
                    }
                    // Only a physical key controls the session. A FlagsChanged
                    // posted by another process (AppleScript, an automation
                    // tool, a remote's companion app) carries that process's
                    // PID here; a physical keyboard carries 0.
                    // Without this guard, any app posting Right Option ends a
                    // hands-free session and pastes the transcript wherever
                    // focus is.
                    let src_pid = event.get_integer_value_field(EventField::EVENT_SOURCE_UNIX_PROCESS_ID);
                    let flags = event.get_flags();
                    if src_pid != 0 {
                        eprintln!(
                            "[hotkey] {} ignored synthesized hotkey event from pid {src_pid} (flags {:#x})",
                            stamp(),
                            flags.bits()
                        );
                        return CallbackResult::Keep;
                    }
                    let input = {
                        if crate::hotkeys::modifier_key_is_down(hotkey_keycode, flags.bits()) {
                            if !hotkey_edge(&hotkey_down_cb, true) {
                                return CallbackResult::Keep;
                            }
                            // Shift co-held at press → transform mode (rewrite the
                            // selection). The user holds Shift *before* the PTT key.
                            let press = Instant::now();
                            let double_tap = double_tap_latch
                                && detector_cb.lock().unwrap().on_press(press);
                            PttInput::HotkeyPress {
                                shift: flags.contains(CGEventFlags::CGEventFlagShift),
                                double_tap,
                            }
                        } else {
                            if !hotkey_edge(&hotkey_down_cb, false) {
                                return CallbackResult::Keep;
                            }
                            detector_cb.lock().unwrap().on_release(Instant::now());
                            PttInput::HotkeyRelease
                        }
                    };
                    eprintln!(
                        "[hotkey] {} modifier edge · key {hotkey_keycode:#x} · flags {:#x}",
                        stamp(), flags.bits()
                    );
                    drive(input);
                    // FlagsChanged is never swallowed — let the modifier through.
                }
                CGEventType::KeyDown => {
                    let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
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
                        let flags = event.get_flags();
                        if flags.contains(required_mask) {
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
                        let (next, decision) =
                            ptt_transition(state_cb.load(Ordering::SeqCst), PttInput::SpaceDown);
                        state_cb.store(next, Ordering::SeqCst);
                        match decision {
                            PttDecision::ArmLatch => {
                                // Chord recognised: cue + swallow so the
                                // Option+Space character never reaches the field.
                                let _ = tx_for_callback.send(DaemonEvent::LatchArmed);
                                space_down_cb.store(true, Ordering::SeqCst);
                                return CallbackResult::Drop;
                            }
                            PttDecision::SwallowSpace => {
                                space_down_cb.store(true, Ordering::SeqCst);
                                return CallbackResult::Drop;
                            }
                            _ => {}
                        }
                    }
                }
                CGEventType::KeyUp => {
                    let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
                    if !modifier_only && keycode == hotkey_keycode {
                        // Chord hotkey release: swallow the matching key-up.
                        if hotkey_edge(&hotkey_down_cb, false) {
                            detector_cb.lock().unwrap().on_release(Instant::now());
                            drive(PttInput::HotkeyRelease);
                            return CallbackResult::Drop;
                        }
                        return CallbackResult::Keep;
                    }
                    if modifier_only && keycode == SPACE_KEYCODE && space_down_cb.swap(false, Ordering::SeqCst) {
                        // Swallow the key-up matching a swallowed key-down.
                        return CallbackResult::Drop;
                    }
                }
                _ => {}
            }
            CallbackResult::Keep
        },
    )
    .map_err(|_| {
        eyre::eyre!("failed to install CGEventTap — Accessibility permission missing or revoked")
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
        .map_err(|_| eyre::eyre!("create_runloop_source failed"))?;
    CFRunLoop::get_current().add_source(&loop_source, unsafe { kCFRunLoopCommonModes });
    tap.enable();
    // Leak the tap — it must outlive the run loop. Safe to drop only on
    // process exit (which NSApp.terminate handles).
    std::mem::forget(tap);
    std::mem::forget(loop_source);

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
