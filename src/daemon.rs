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
use std::sync::Arc;
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
    std::process::Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
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
            trimmed
                .rsplit('/')
                .next()
                .unwrap_or(trimmed)
                .to_string()
        })
        .unwrap_or_else(|| format!("pid {pid}"))
}

#[cfg(feature = "cleaner")]
use crate::cleaner::TextCleanupEngine;

// Right Option key — kVK_RightOption from Carbon Events.h.
// Default. Override at startup by setting DICTATE_HOTKEY_KEYCODE.
const DEFAULT_HOTKEY_KEYCODE: i64 = 0x3D;

/// How many screen-harvested proper nouns to feed the cleanup prompt. Kept small
/// on purpose: measurement (examples/latency_lab) showed the accuracy win is
/// fully realized by ~20 terms, while larger lists both cost prefill latency and
/// start distracting the 1B model from basic cleanup. Combined with the curated
/// corrections vocabulary, this stays well under that regression threshold.
const SCREEN_VOCAB_CAP: usize = 16;

// Spacebar — kVK_Space. Chorded with the held PTT key to arm hands-free mode.
const SPACE_KEYCODE: i64 = 0x31;

// Hands-free latch state machine, shared with the event-tap callback:
//
//   ST_IDLE ─PTT press─▶ ST_HOLDING ─release─▶ ST_IDLE              (normal PTT)
//                            │ +Space chord
//                            ▼
//                     ST_HOLDING_ARMED ─release─▶ ST_LATCHED
//                                                     │ PTT tap
//                                                     ▼
//                                               ST_STOPPING_TAP ─release─▶ ST_IDLE
//
// In ST_LATCHED the keys are physically up but the mic keeps recording; the
// next PTT tap stops + processes the utterance. The transition logic lives in
// the pure `ptt_transition` fn below so it can be unit-tested without a tap.
const ST_IDLE: u8 = 0;
const ST_HOLDING: u8 = 1;
const ST_HOLDING_ARMED: u8 = 2;
const ST_LATCHED: u8 = 3;
const ST_STOPPING_TAP: u8 = 4;

/// A normalised hotkey/space event fed to the latch state machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PttInput {
    /// The PTT modifier transitioned to held. `shift` = Shift co-held (the
    /// transform-selection gesture), read only when starting a fresh utterance.
    HotkeyPress { shift: bool },
    /// The PTT modifier transitioned to released.
    HotkeyRelease,
    /// Space went down (only meaningful while the PTT key is held).
    SpaceDown,
}

/// What the state machine wants the caller to do, alongside the new state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PttDecision {
    Nothing,
    /// Begin capture. `transform` carries the Shift-at-press gesture.
    Start { transform: bool },
    /// End capture and run the pipeline.
    Stop,
    /// First Space of a hold: arm hands-free (cue) and swallow the Space.
    ArmLatch,
    /// Subsequent Space while already armed: just swallow it.
    SwallowSpace,
}

/// Pure transition table for the hands-free latch. Given the current state and
/// an input, returns the next state and the side effect to perform. Keeping
/// this free of any CG / channel types makes the whole gesture unit-testable.
fn ptt_transition(state: u8, input: PttInput) -> (u8, PttDecision) {
    match (state, input) {
        (ST_IDLE, PttInput::HotkeyPress { shift }) => {
            (ST_HOLDING, PttDecision::Start { transform: shift })
        }
        (ST_HOLDING, PttInput::SpaceDown) => (ST_HOLDING_ARMED, PttDecision::ArmLatch),
        (ST_HOLDING_ARMED, PttInput::SpaceDown) => (ST_HOLDING_ARMED, PttDecision::SwallowSpace),
        (ST_HOLDING, PttInput::HotkeyRelease) => (ST_IDLE, PttDecision::Stop),
        // Armed → keys released: stay recording hands-free.
        (ST_HOLDING_ARMED, PttInput::HotkeyRelease) => (ST_LATCHED, PttDecision::Nothing),
        // A PTT tap ends a hands-free session.
        (ST_LATCHED, PttInput::HotkeyPress { .. }) => (ST_STOPPING_TAP, PttDecision::Stop),
        (ST_STOPPING_TAP, PttInput::HotkeyRelease) => (ST_IDLE, PttDecision::Nothing),
        // Any other combination is a no-op (e.g. stray Space when not holding,
        // re-press while already holding, release in IDLE).
        (s, _) => (s, PttDecision::Nothing),
    }
}

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
    pub parakeet_dir: String,
    #[cfg(feature = "cleaner")]
    pub gemma_path: String,
    /// If true, inject the raw ASR transcript without Gemma cleanup.
    /// Saves ~860 ms per utterance.
    pub no_cleanup: bool,
    /// Hotkey keycode to watch on FlagsChanged.
    pub hotkey_keycode: i64,
}

impl DaemonConfig {
    pub fn from_env(
        parakeet_dir: String,
        #[cfg(feature = "cleaner")] gemma_path: String,
        no_cleanup: bool,
    ) -> Self {
        let hotkey_keycode =
            crate::settings::Settings::load().resolve_hotkey(DEFAULT_HOTKEY_KEYCODE);
        Self {
            parakeet_dir,
            #[cfg(feature = "cleaner")]
            gemma_path,
            no_cleanup,
            hotkey_keycode,
        }
    }
}

#[derive(Debug)]
enum DaemonEvent {
    /// `transform` is set when Shift was co-held at press time — the utterance
    /// is an instruction to rewrite the current selection, not text to insert.
    StartRecording { transform: bool },
    StopRecording,
    /// Cancel an in-flight utterance: stop capture and discard it (no
    /// transcribe, no inject). Reached only via the control socket — there's no
    /// hotkey gesture for it. A no-op when nothing is recording.
    CancelRecording,
    /// PTT + Space chord recognised mid-hold: the session became hands-free, so
    /// releasing the keys won't stop recording. Worker just plays the cue.
    LatchArmed,
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
    });

    eprintln!("[daemon] installing event tap on main run loop...");

    // The CGEventTap callback runs on the main CFRunLoop thread. Keep it
    // FAST — just send to the worker.
    let tx_for_callback = tx.clone();
    let hotkey_keycode = read_hotkey_keycode();
    // The hold is detected by watching the modifier-flag bit that THIS key
    // toggles — Option vs Command vs Control vs Shift. Without this, any
    // non-Option hotkey would never register a press (the old code checked
    // the Alternate bit unconditionally).
    let hotkey_flag = keycode_to_modifier_flag(hotkey_keycode);

    // Hands-free latch state shared with the callback. `state` is the ST_*
    // machine driven by `ptt_transition`; `space_down` lets us swallow the
    // Space key-up that matches a swallowed key-down so no stray space ever
    // lands in the focused field.
    let state = Arc::new(AtomicU8::new(ST_IDLE));
    let space_down = Arc::new(AtomicBool::new(false));
    let state_cb = state.clone();
    let space_down_cb = space_down.clone();

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
                    let keycode =
                        event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
                    if keycode != hotkey_keycode {
                        return CallbackResult::Keep;
                    }
                    let flags = event.get_flags();
                    let input = if flags.contains(hotkey_flag) {
                        // Shift co-held at press → transform mode (rewrite the
                        // selection). The user holds Shift *before* the PTT key.
                        PttInput::HotkeyPress {
                            shift: flags.contains(CGEventFlags::CGEventFlagShift),
                        }
                    } else {
                        PttInput::HotkeyRelease
                    };
                    let (next, decision) = ptt_transition(state_cb.load(Ordering::SeqCst), input);
                    state_cb.store(next, Ordering::SeqCst);
                    match decision {
                        PttDecision::Start { transform } => {
                            let _ =
                                tx_for_callback.send(DaemonEvent::StartRecording { transform });
                        }
                        PttDecision::Stop => {
                            let _ = tx_for_callback.send(DaemonEvent::StopRecording);
                        }
                        _ => {}
                    }
                    // FlagsChanged is never swallowed — let the modifier through.
                }
                CGEventType::KeyDown => {
                    let keycode =
                        event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
                    if keycode == SPACE_KEYCODE {
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
                    let keycode =
                        event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
                    if keycode == SPACE_KEYCODE && space_down_cb.swap(false, Ordering::SeqCst) {
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
        eyre::eyre!(
            "failed to install CGEventTap — Accessibility permission missing or revoked"
        )
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

    eprintln!(
        "[boot] ready · hold {key} (0x{hotkey_keycode:x}) to dictate · \
         {key}+Space then release = hands-free (tap {key} to stop) · ⌘Q quits",
        key = crate::settings::hotkey_name(hotkey_keycode),
    );
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
        let _ = match cmd {
            Command::Start => tx_for_ctl.send(DaemonEvent::StartRecording { transform: false }),
            Command::Stop => tx_for_ctl.send(DaemonEvent::StopRecording),
            Command::Cancel => tx_for_ctl.send(DaemonEvent::CancelRecording),
            // The server resolves Toggle → Start/Stop before dispatch, so a
            // Toggle reaching here would be a bug; treat it as a no-op.
            Command::Toggle => Ok(()),
        };
    });

    // Hand the main thread to NSApplication. AppKit pumps the same
    // CFRunLoop that our event tap is attached to, so the tap + menu bar +
    // pill all coexist on the same loop.
    menubar::init_and_run()?;

    // Reachable only after NSApp.terminate.
    drop(tx);
    let _ = worker_handle.join();
    Ok(())
}

fn read_hotkey_keycode() -> i64 {
    // Precedence: DICTATE_HOTKEY_KEYCODE env > settings.json > default.
    crate::settings::Settings::load().resolve_hotkey(DEFAULT_HOTKEY_KEYCODE)
}

/// Map a modifier-key keycode to the CGEvent flag bit it toggles. On a
/// FlagsChanged event the keycode tells us which physical key moved; this
/// tells us which flag bit to read to know if it's now held or released.
/// Unknown keycodes fall back to Option (the historical default).
fn keycode_to_modifier_flag(keycode: i64) -> CGEventFlags {
    match keycode {
        0x3A | 0x3D => CGEventFlags::CGEventFlagAlternate, // Left / Right Option
        0x37 | 0x36 => CGEventFlags::CGEventFlagCommand,   // Left / Right Command
        0x3B | 0x3E => CGEventFlags::CGEventFlagControl,   // Left / Right Control
        0x38 | 0x3C => CGEventFlags::CGEventFlagShift,     // Left / Right Shift
        _ => CGEventFlags::CGEventFlagAlternate,
    }
}

fn worker_loop(
    rx: std::sync::mpsc::Receiver<DaemonEvent>,
    config: DaemonConfig,
) -> eyre::Result<()> {
    let t_load = Instant::now();
    let mut worker = LocalInferenceWorker::initialize(&config.parakeet_dir)?;
    eprintln!("[boot] parakeet    loaded in {:>4} ms", ms(t_load.elapsed()));

    #[cfg(feature = "cleaner")]
    let cleaner = if !config.no_cleanup {
        let pretty = std::path::Path::new(&config.gemma_path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&config.gemma_path);
        let t = Instant::now();
        let c = TextCleanupEngine::initialize(&config.gemma_path)?;
        eprintln!(
            "[boot] cleaner     loaded in {:>4} ms · {pretty}",
            ms(t.elapsed())
        );
        Some(c)
    } else {
        eprintln!("[boot] cleaner     disabled (--no-cleanup)");
        None
    };

    // Tokio current-thread runtime for the async cleaner path.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    // Warm-up: first call pays graph-optimization + CoreML compile / Metal
    // shader compile costs. Pay them now so the first real utterance is hot.
    let t_warm = Instant::now();
    let warm_silence = vec![0.0_f32; 16_000]; // 1 s of silence at 16 kHz
    let _ = worker.transcribe_pcm(&warm_silence);
    #[cfg(feature = "cleaner")]
    if let Some(ref c) = cleaner {
        let _ = rt.block_on(c.process_transcript("warmup")).ok();
    }
    eprintln!("[boot] warm-up     done   in {:>4} ms", ms(t_warm.elapsed()));

    // Load personal corrections dictionary (no-op if no file) and wrap it in
    // the refiner that applies corrections + voice-command parsing in the
    // canonical order.
    let corrections = match Corrections::load_default() {
        Ok(c) => {
            if c.is_empty() {
                eprintln!("[boot] corrections none loaded (no dictionary file)");
            } else {
                eprintln!("[boot] corrections {:>4} entries loaded", c.len());
            }
            c
        }
        Err(e) => {
            eprintln!("[warn] corrections failed to load: {e}");
            Corrections::empty()
        }
    };
    let refiner = Refiner::new(corrections);

    // Always-on capture (pre-roll). When the user enables it, the mic stream is
    // kept warm for the daemon's life and the last few hundred ms before each
    // key press are retained, so the first words are never clipped and there's
    // no stream-open latency on press. `None` ⇒ the proven open-on-press path
    // (default). Created on the worker thread because the cpal Stream is !Send.
    let preroll_ms = crate::settings::Settings::load().resolve_preroll_ms();
    let mut always: Option<(crate::audio::AlwaysOnCapture, crate::audio::HeapAudioConsumer)> =
        if preroll_ms > 0 {
            let preroll = crate::audio::preroll_samples(preroll_ms);
            let (mut ao, cons) = crate::audio::AlwaysOnCapture::new(preroll, BUFFER_CAPACITY);
            match ao.start() {
                Ok(()) => {
                    eprintln!("[boot] always-on  mic warm · pre-roll {preroll_ms} ms");
                    Some((ao, cons))
                }
                Err(e) => {
                    eprintln!("[warn] always-on capture failed ({e}); using press-to-open");
                    None
                }
            }
        } else {
            None
        };
    // True between a pre-roll StartRecording and its StopRecording (the
    // always-on analogue of `engine.is_some()` for the open-on-press path).
    let mut session_active = false;

    // Per-utterance state (open-on-press path).
    let mut engine: Option<AudioCaptureEngine> = None;
    let mut consumer: Option<crate::audio::HeapAudioConsumer> = None;
    let mut is_recording: Option<std::sync::Arc<std::sync::atomic::AtomicBool>> = None;
    let mut t_press: Option<Instant> = None;
    // Whether the in-flight utterance is a transform instruction (Shift held).
    let mut transform_mode = false;

    // EXPERIMENTAL streaming cleanup (off unless the setting/env enables it AND
    // the cleaner is loaded). When on, sentences are transcribed + cleaned at VAD
    // pauses *during* the hold, so only the final segment is outstanding at
    // release. The flag-off path below is byte-identical to before.
    // Streaming cleanup and always-on pre-roll are mutually exclusive for now
    // (both are experimental and drain the capture ring differently). Pre-roll
    // wins when both are set — keep the always-on path on the simple
    // whole-buffer transcribe.
    #[cfg(feature = "cleaner")]
    let streaming = cleaner.is_some()
        && always.is_none()
        && crate::settings::Settings::load().resolve_streaming_cleanup();
    #[cfg(not(feature = "cleaner"))]
    let streaming = false;
    if streaming {
        eprintln!("[boot] streaming   cleanup ENABLED (experimental · VAD-segmented during hold)");
    }
    // Per-utterance streaming accumulators (only used when `streaming`).
    #[cfg(feature = "cleaner")]
    let mut stream: Option<crate::vad::SegmentStream> = None;
    #[cfg(feature = "cleaner")]
    let mut stream_raw: Vec<String> = Vec::new();
    #[cfg(feature = "cleaner")]
    let mut stream_clean: Vec<String> = Vec::new();

    'evloop: loop {
        // While recording in streaming mode, wake periodically to process any
        // sentence that a VAD pause has just closed; otherwise block for the next
        // event. recv_timeout keeps everything on this one thread, which already
        // owns the Parakeet worker, the cleaner, and the runtime — no shared state.
        let event = {
            #[cfg(feature = "cleaner")]
            if streaming && engine.is_some() {
                match rx.recv_timeout(std::time::Duration::from_millis(120)) {
                    Ok(ev) => ev,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if let (Some(st), Some(c), Some(cl)) =
                            (stream.as_mut(), consumer.as_mut(), cleaner.as_ref())
                        {
                            stream_process(
                                &mut worker, cl, &refiner, &rt, st, c,
                                &mut stream_raw, &mut stream_clean, false,
                            );
                        }
                        continue;
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            } else {
                match rx.recv() {
                    Ok(ev) => ev,
                    Err(_) => break,
                }
            }
            #[cfg(not(feature = "cleaner"))]
            match rx.recv() {
                Ok(ev) => ev,
                Err(_) => break,
            }
        };
        match event {
            DaemonEvent::LatchArmed => {
                // Hands-free engaged mid-hold: the mic keeps running once the
                // keys are released. Just give audible feedback + a log line.
                cues::play_latch();
                eprintln!("⤓ hands-free · release keys, keep talking · tap PTT to stop");
            }
            DaemonEvent::CancelRecording => {
                // External "cancel" (control socket only): stop capture and throw
                // the utterance away — no transcribe, no inject. Mirrors the
                // StopRecording teardown minus the pipeline. Safe to fire when
                // idle (nothing live ⇒ no-op).
                crate::ipc::DAEMON_RECORDING.store(false, Ordering::SeqCst);
                let mut on_demand_engine = engine.take();
                let always_on_session = session_active;
                session_active = false;
                if on_demand_engine.is_none() && !always_on_session {
                    continue;
                }
                // Un-mute other audio on every exit, exactly like StopRecording.
                let _unmute = crate::audio_duck::RestoreOnDrop;
                transform_mode = false;
                t_press.take();
                if let Some(e) = on_demand_engine.as_mut() {
                    e.stop_capture();
                }
                if always_on_session {
                    // End the session and drain+discard the buffered audio so the
                    // next press starts from a clean ring.
                    if let Some((ao, cons)) = always.as_mut() {
                        ao.end_session();
                        let recflag = ao.recording_flag();
                        let _ = crate::audio::drain_session(cons, &recflag);
                    }
                } else {
                    // Drop the per-utterance consumer + recording flag unread.
                    consumer.take();
                    is_recording.take();
                }
                #[cfg(feature = "cleaner")]
                {
                    stream = None;
                    stream_raw.clear();
                    stream_clean.clear();
                }
                cues::play_cancel();
                ui_channel::set_state(UiState::Idle);
                eprintln!("⌫ cancelled · discarded");
                eprintln!();
            }
            DaemonEvent::StartRecording { transform } => {
                if engine.is_some() || session_active {
                    continue;
                }
                // Always-on (pre-roll) path: the stream is already warm — just
                // open a session. The next audio callback flushes the pre-roll
                // lookback (audio from *before* this press) into the consumer,
                // then live audio, so the first words can't be clipped.
                if always.is_some() {
                    // The warm stream is bound to whatever input was default at
                    // boot. If the user has since switched inputs (AirPods
                    // connected, a call app grabbed the mic), rebuild it now so
                    // we capture from the *current* device instead of silently
                    // recording the old one. Cheap host query; only the rare
                    // device-change case pays the rebuild.
                    if always.as_ref().map(|(ao, _)| ao.device_changed()).unwrap_or(false) {
                        let preroll = crate::audio::preroll_samples(preroll_ms);
                        let (mut ao2, cons2) =
                            crate::audio::AlwaysOnCapture::new(preroll, BUFFER_CAPACITY);
                        match ao2.start() {
                            Ok(()) => {
                                eprintln!(
                                    "[audio] default input changed → rebuilt always-on stream on `{}`",
                                    ao2.device_name()
                                );
                                always = Some((ao2, cons2));
                            }
                            Err(e) => eprintln!(
                                "[warn] always-on rebuild failed ({e}); keeping previous stream"
                            ),
                        }
                    }
                    if let Some((ao, _)) = always.as_ref() {
                        transform_mode = transform;
                        t_press = Some(Instant::now());
                        ao.begin_session();
                        cues::play_start();
                        crate::audio_duck::mute();
                        ui_channel::set_state(UiState::Recording);
                        eprintln!("▶ recording · pre-roll");
                        session_active = true;
                        crate::ipc::DAEMON_RECORDING.store(true, Ordering::SeqCst);
                    }
                    continue;
                }
                transform_mode = transform;
                t_press = Some(Instant::now());
                let (mut e, c) = AudioCaptureEngine::new(BUFFER_CAPACITY);
                let r = match e.start_microphone() {
                    Ok(r) => r,
                    Err(err) => {
                        eprintln!("[err]  mic open failed: {err:?}");
                        cues::play_error();
                        continue;
                    }
                };
                cues::play_start();
                // Mute any other system audio for the duration of capture +
                // processing; restored on every StopRecording exit path below
                // via the RestoreOnDrop guard.
                crate::audio_duck::mute();
                ui_channel::set_state(UiState::Recording);
                eprintln!("▶ recording");
                crate::ipc::DAEMON_RECORDING.store(true, Ordering::SeqCst);
                engine = Some(e);
                consumer = Some(c);
                is_recording = Some(r);
                #[cfg(feature = "cleaner")]
                if streaming {
                    stream = Some(crate::vad::SegmentStream::new(
                        crate::audio::SAMPLE_RATE,
                        crate::vad::VadConfig::default(),
                    ));
                    stream_raw.clear();
                    stream_clean.clear();
                }
            }
            DaemonEvent::StopRecording => {
                // Either capture path may be live: open-on-press (`engine`) or
                // always-on pre-roll (`session_active`). Nothing live ⇒ ignore.
                crate::ipc::DAEMON_RECORDING.store(false, Ordering::SeqCst);
                let mut on_demand_engine = engine.take();
                let always_on_session = session_active;
                session_active = false;
                if on_demand_engine.is_none() && !always_on_session {
                    continue;
                }
                // Un-mute other audio when this arm exits — by any path
                // (transcribe error, empty/discarded utterance, transform,
                // or successful inject). The guard's Drop runs on every
                // `continue` and the normal fall-through, so the restore can't
                // be forgotten on a new code path.
                let _unmute = crate::audio_duck::RestoreOnDrop;
                // Capture (and reset) whether this utterance is a transform.
                #[allow(unused_variables)]
                let transform = transform_mode;
                transform_mode = false;
                // Stop capture for whichever path is live. Open-on-press drops
                // its engine (releasing the mic); always-on just ends the
                // session — the warm stream keeps running for the next press.
                if let Some(e) = on_demand_engine.as_mut() {
                    e.stop_capture();
                }
                if always_on_session {
                    if let Some((ao, _)) = always.as_ref() {
                        ao.end_session();
                    }
                }
                cues::play_stop();
                ui_channel::set_state(UiState::Processing);
                let press_to_release = t_press.take().map(|t| t.elapsed());
                let held = press_to_release.unwrap_or_default();
                eprintln!("⏹ stopped · held {}", secs(held));

                // Spawn AX focus capture in parallel with the inference
                // pipeline. By the time Parakeet+Gemma finish, the focused
                // element is already known — get_focused_element drops off
                // the critical path.
                let (target_tx, target_rx) =
                    std::sync::mpsc::channel::<eyre::Result<FocusTarget>>();
                std::thread::spawn(move || {
                    let _ = target_tx.send(FocusTarget::capture());
                });

                let t_pipeline = Instant::now();
                // When cleanup already happened per-segment during the hold,
                // carry the assembled cleaned text forward so the cleanup step
                // below reuses it instead of re-running Gemma on the whole thing.
                #[cfg(feature = "cleaner")]
                let mut precleaned_override: Option<String> = None;

                // Acquire the transcript. Streaming mode (experimental) assembles
                // it from the sentences cleaned during the hold + a final tail
                // segment; everything else takes the proven whole-buffer path,
                // byte-for-byte unchanged.
                let transcript: String = 'got: {
                    // Always-on pre-roll: drain the warm stream's queued audio
                    // (the pre-roll lookback the callback flushed at press +
                    // everything since) and transcribe the whole buffer.
                    if always_on_session {
                        if let Some((ao, cons)) = always.as_mut() {
                            let recflag = ao.recording_flag();
                            let buf = crate::audio::drain_session(cons, &recflag);
                            match worker.transcribe_pcm(&buf) {
                                Ok(t) => break 'got t,
                                Err(err) => {
                                    eprintln!("[err]  transcribe failed: {err:?}");
                                    cues::play_error();
                                    ui_channel::set_state(UiState::Idle);
                                    eprintln!();
                                    continue 'evloop;
                                }
                            }
                        }
                    }

                    // Open-on-press path (and the streaming variant) own a
                    // per-utterance consumer; take it only here.
                    #[allow(unused_mut)]
                    let mut c = consumer.take().unwrap();
                    let r = is_recording.take().unwrap();

                    #[cfg(feature = "cleaner")]
                    if streaming && stream.is_some() {
                        // Clean the final outstanding segment(s) now (this is the
                        // only cleanup left post-release — the win).
                        if let (Some(st), Some(cl)) = (stream.as_mut(), cleaner.as_ref()) {
                            stream_process(
                                &mut worker, cl, &refiner, &rt, st, &mut c,
                                &mut stream_raw, &mut stream_clean, true,
                            );
                        }
                        let whole = stream.as_ref().map(|s| s.buf().to_vec()).unwrap_or_default();
                        // Use the streamed result only for genuinely multi-sentence
                        // dictations; short utterances / transforms fall back to the
                        // proven whole-buffer path so commands & transform still work.
                        let result = if !transform && stream_clean.len() >= 2 {
                            // Strip a trailing "yeah/yep/…" on the ASSEMBLED text: in
                            // streaming, a standalone trailing "Yeah." is its own
                            // segment and the per-segment pass keeps it (looks like a
                            // whole-utterance ack); only the join reveals it's trailing.
                            let joined = crate::text_polish::strip_trailing_filler(&stream_clean.join(" "));
                            precleaned_override = Some(joined);
                            eprintln!("  ⟫ streamed {} sentence segment(s) during hold", stream_clean.len());
                            stream_raw.join(" ")
                        } else {
                            match worker.transcribe_pcm(&whole) {
                                Ok(t) => t,
                                Err(err) => {
                                    eprintln!("[err]  transcribe failed: {err:?}");
                                    cues::play_error();
                                    ui_channel::set_state(UiState::Idle);
                                    eprintln!();
                                    stream = None;
                                    continue 'evloop;
                                }
                            }
                        };
                        stream = None;
                        break 'got result;
                    }
                    match rt.block_on(worker.run_inference_pipeline(c, r)) {
                        Ok(t) => t,
                        Err(err) => {
                            eprintln!("[err]  transcribe failed: {err:?}");
                            cues::play_error();
                            ui_channel::set_state(UiState::Idle);
                            eprintln!();
                            continue 'evloop;
                        }
                    }
                };
                let t_transcribe = t_pipeline.elapsed();

                if transcript.trim().is_empty() {
                    eprintln!("  skip · empty transcript");
                    eprintln!();
                    ui_channel::set_state(UiState::Idle);
                    continue;
                }

                // ─── Transform mode (Shift + push-to-talk) ───────────────
                // The utterance is an *instruction*: rewrite the current
                // selection in place rather than insert text. Handled entirely
                // via a Cmd+C/Cmd+V clipboard round-trip so it works even in
                // AX-blind apps (Electron, terminals).
                #[cfg(feature = "cleaner")]
                if transform {
                    match cleaner {
                        Some(ref c) => handle_transform(&rt, c, &transcript),
                        None => {
                            eprintln!(
                                "  ✗    transform needs the cleanup engine (running with --no-cleanup)"
                            );
                            cues::play_error();
                        }
                    }
                    eprintln!();
                    ui_channel::set_state(UiState::Idle);
                    continue;
                }

                // Receive the focus target now (capture ran in parallel with
                // inference and is almost always ready by here). We need the
                // target app *before* cleanup so we can skip Gemma when the
                // destination is a terminal — and we reuse this same captured
                // target for injection, keeping get_focused_element off the
                // critical path.
                let captured_target: Option<FocusTarget> = match target_rx.try_recv() {
                    Ok(Ok(t)) => Some(t),
                    Ok(Err(_)) => None,
                    Err(_) => target_rx.recv().ok().and_then(|r| r.ok()),
                };
                let target_pid = captured_target.as_ref().and_then(|t| t.inner_pid());

                // ─── Spoken prefix command (e.g. "translate to Chinese …") ──
                // A recognised lead-in at the START of the utterance turns the
                // rest into a one-shot transform: translate the spoken body and
                // inject the result at the cursor. Reuses the warm Gemma
                // transform engine — no selection, no second gesture. Detection
                // is deterministic (no model on the decision path); the model
                // only runs once we know it's a command. Falls through to normal
                // dictation when nothing matches, or when running --no-cleanup.
                #[cfg(feature = "cleaner")]
                if let Some(cmd) = crate::spoken_command::parse_spoken_command(&transcript) {
                    match cleaner {
                        Some(ref c) => {
                            handle_spoken_command(&rt, c, cmd, captured_target);
                            eprintln!();
                            ui_channel::set_state(UiState::Idle);
                            continue;
                        }
                        None => {
                            eprintln!(
                                "  ✗    \"{}\" needs the cleanup engine (running with --no-cleanup) — dictating as-is",
                                cmd.label
                            );
                        }
                    }
                }

                // Terminals draw their own text grid and almost always receive
                // shell commands, not prose — Gemma cleanup corrupts more than
                // it helps there, so inject the raw transcript instead. Every
                // other app keeps the normal formal-cleanup path.
                #[cfg(feature = "cleaner")]
                let is_terminal = target_pid
                    .map(crate::injector::is_terminal_pid)
                    .unwrap_or(false);

                // Keep the raw ASR text so the summary block can compare it to
                // the cleaned output — the only way to tell, after the fact,
                // whether a short result came from ASR or from the cleanup model
                // dropping content. Cheap: dictations are short strings.
                let raw_transcript = transcript.clone();

                // Screen-context vocabulary: proper nouns harvested (during the
                // parallel focus capture, so off the critical path) from the
                // focused field + window title. Fed to cleanup so Gemma spells
                // on-screen names the way they appear instead of as similar-
                // sounding common words. Empty in AX-blind apps.
                #[cfg(feature = "cleaner")]
                let screen_vocab: Vec<String> = captured_target
                    .as_ref()
                    .map(|t| t.screen_terms(SCREEN_VOCAB_CAP))
                    .unwrap_or_default();

                let mut t_cleanup_ms = 0_u128;
                let final_text = {
                    #[cfg(feature = "cleaner")]
                    {
                        match cleaner {
                            // Already cleaned per-sentence during the hold — reuse
                            // it (the whole point of streaming). Skips a redundant
                            // whole-buffer Gemma pass. Terminals still take raw.
                            Some(_) if !is_terminal && precleaned_override.is_some() => {
                                let pre = precleaned_override.take().unwrap();
                                eprintln!("  ⟫    streamed cleanup ({} chars)", pre.chars().count());
                                pre
                            }
                            Some(ref c) if !is_terminal => {
                                if !screen_vocab.is_empty() {
                                    eprintln!("  ⌕    screen vocab: {}", screen_vocab.join(", "));
                                }
                                // Fix known proper nouns in the *raw* transcript
                                // before cleanup, so the small model sees the
                                // intended spelling (e.g. "to twist" → "Todoist")
                                // instead of mangling a garbled one out of reach
                                // of the post-cleanup pass. refine() corrects
                                // again afterwards (idempotent for these fixes).
                                let pre_corrected = refiner.apply_corrections(&transcript);
                                if pre_corrected != transcript {
                                    eprintln!("  ✎    pre-cleanup corrections applied");
                                }
                                let t_clean = Instant::now();
                                match rt.block_on(
                                    c.process_transcript_with_vocab(&pre_corrected, &screen_vocab),
                                ) {
                                    Ok(cleaned) => {
                                        t_cleanup_ms = ms(t_clean.elapsed());
                                        cleaned
                                    }
                                    Err(err) => {
                                        eprintln!("[warn] cleanup failed, using raw: {err:?}");
                                        transcript
                                    }
                                }
                            }
                            Some(_) => {
                                eprintln!("  ⌁    terminal · raw transcript (cleanup skipped)");
                                transcript
                            }
                            None => transcript,
                        }
                    }
                    #[cfg(not(feature = "cleaner"))]
                    {
                        transcript
                    }
                };

                // Corrections dictionary + trailing voice-command parsing, in
                // the canonical order (corrections first). Same transform the
                // CLI `dictate`/`bench` paths now use, so behaviour matches.
                // Classify the refined utterance into one terminal decision
                // (discard / bare-action / skip / inject). The branching lives
                // in `RefinedDictation::outcome`, unit-tested without audio; the
                // three non-inject arms log + return to idle, leaving the hot
                // path below to handle only the inject case.
                let (final_text, action) = match refiner.refine(&final_text).outcome() {
                    DictationOutcome::Discard => {
                        eprintln!("  ⌫    scratch that · discarded");
                        eprintln!();
                        ui_channel::set_state(UiState::Idle);
                        continue;
                    }
                    DictationOutcome::BareAction(action) => {
                        execute_trailing_action(&action);
                        eprintln!("  ↵    bare command (no body)");
                        eprintln!();
                        ui_channel::set_state(UiState::Idle);
                        continue;
                    }
                    DictationOutcome::Skip => {
                        eprintln!("  skip · cleaned to empty");
                        eprintln!();
                        ui_channel::set_state(UiState::Idle);
                        continue;
                    }
                    DictationOutcome::Inject { text, action } => (text, action),
                };

                // Inject using the focus target captured before cleanup.
                let t_inject = Instant::now();
                let (inject_result, used_fresh_capture) = match captured_target {
                    Some(target) => (
                        AccessibilityInjector::inject_with_target(target, &final_text),
                        false,
                    ),
                    None => (AccessibilityInjector::inject_text(&final_text), true),
                };
                let t_inject_ms = ms(t_inject.elapsed());

                // ─── per-utterance summary block ───────────────────────
                eprintln!(
                    "  xcr  {:>4} ms · cln {:>4} ms · inj {:>4} ms",
                    ms(t_transcribe),
                    t_cleanup_ms,
                    t_inject_ms
                );
                if let Some(pid) = target_pid {
                    eprintln!("  app  {} (pid {})", app_name(pid), pid);
                } else if used_fresh_capture {
                    eprintln!("  app  <fresh capture · target unknown>");
                }
                // Which cleanup model produced this — always visible, so the log
                // is self-describing when comparing models.
                #[cfg(feature = "cleaner")]
                if cleaner.is_some() {
                    let m = std::path::Path::new(&config.gemma_path)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("?");
                    eprintln!("  llm  {m}");
                }
                // Truncation watchdog: if the cleaned text is dramatically
                // shorter than the raw transcript (>40% dropped), the cleanup
                // model likely gave up early or summarized — log the raw text so
                // we can see exactly what was lost. Filler removal alone rarely
                // exceeds 40%, so this fires on real content loss, not noise.
                let raw_chars = raw_transcript.chars().count();
                let final_chars = final_text.chars().count();
                if raw_chars > 40 && final_chars * 5 < raw_chars * 3 {
                    eprintln!(
                        "  ⚠    cleanup shrank text {raw_chars} → {final_chars} chars · raw: \"{raw_transcript}\""
                    );
                }
                if let Err(err) = &inject_result {
                    eprintln!("  ✗    inject failed: {err:?}");
                    cues::play_error();
                } else {
                    eprintln!("  ✓    \"{final_text}\"");
                    // Remember it for the menu-bar "Copy last dictation" item.
                    ui_channel::set_last_dictation(&final_text);
                    // Persist to the browsable dictation history (best-effort;
                    // never blocks or fails the hot path).
                    crate::history::record(&final_text);
                    if !matches!(action, TrailingAction::None) {
                        // Let the field commit the injected text before the key.
                        std::thread::sleep(Duration::from_millis(40));
                        execute_trailing_action(&action);
                    }
                }
                eprintln!();
                ui_channel::set_state(UiState::Idle);
            }
        }

        // Give the audio cue a moment to actually start playing before we
        // potentially re-enter recording on a fast double-tap.
        std::thread::sleep(Duration::from_millis(10));
    }

    Ok(())
}

/// Transform mode: read the focused app's current selection, apply the spoken
/// `instruction` to it with the warm Gemma engine, and paste the result over
/// the selection. Read + replace both go through the clipboard so it works in
/// AX-blind apps. All failures are logged + cue'd; never fatal.
#[cfg(feature = "cleaner")]
fn handle_transform(
    rt: &tokio::runtime::Runtime,
    cleaner: &TextCleanupEngine,
    instruction: &str,
) {
    let selection = match crate::clipboard_paste::copy_selection_via_clipboard() {
        Ok(Some(s)) => s,
        Ok(None) => {
            eprintln!("  ✗    transform: nothing selected — select text first");
            cues::play_error();
            return;
        }
        Err(e) => {
            eprintln!("  ✗    transform: couldn't read selection: {e:?}");
            cues::play_error();
            return;
        }
    };
    eprintln!("  ✎    \"{}\"  ·  {} chars selected", instruction.trim(), selection.chars().count());

    let t = Instant::now();
    let result = match rt.block_on(cleaner.transform(instruction, &selection)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("  ✗    transform failed: {e:?}");
            cues::play_error();
            return;
        }
    };
    let result = result.trim();
    if result.is_empty() {
        eprintln!("  ✗    transform produced no output; selection left unchanged");
        cues::play_error();
        return;
    }

    match crate::clipboard_paste::paste_via_clipboard(result) {
        Ok(()) => eprintln!("  ✓    transformed in {} ms → \"{result}\"", ms(t.elapsed())),
        Err(e) => {
            eprintln!("  ✗    transform paste failed: {e:?}");
            cues::play_error();
        }
    }
}

/// Spoken prefix command (e.g. "translate to Chinese …"): run the resolved
/// Pop everything currently sitting in the capture ring buffer (non-blocking).
/// Used by the streaming path to pull newly captured audio on each tick + at
/// release without waiting on the recording flag.
#[cfg(feature = "cleaner")]
fn drain_available(consumer: &mut crate::audio::HeapAudioConsumer) -> Vec<f32> {
    use ringbuf::traits::{Consumer, Observer};
    let n = consumer.occupied_len();
    if n == 0 {
        return Vec::new();
    }
    let mut buf = vec![0.0_f32; n];
    let got = consumer.pop_slice(&mut buf);
    buf.truncate(got);
    buf
}

/// One streaming step: pull available audio into the segment stream, then
/// transcribe + clean each segment the VAD reports (finished-only on a tick,
/// everything remaining when `final_flush`). Cleaned pieces carry the prior
/// cleaned text as left-context. Results append to `raw_acc` / `clean_acc`.
#[cfg(feature = "cleaner")]
#[allow(clippy::too_many_arguments)]
fn stream_process(
    worker: &mut LocalInferenceWorker,
    cleaner: &TextCleanupEngine,
    refiner: &Refiner,
    rt: &tokio::runtime::Runtime,
    st: &mut crate::vad::SegmentStream,
    consumer: &mut crate::audio::HeapAudioConsumer,
    raw_acc: &mut Vec<String>,
    clean_acc: &mut Vec<String>,
    final_flush: bool,
) {
    let avail = drain_available(consumer);
    if !avail.is_empty() {
        st.push(&avail);
    }
    let segs = if final_flush { st.take_final() } else { st.take_complete() };
    for (s, e) in segs {
        let seg = st.buf()[s..e].to_vec();
        let raw = match worker.transcribe_pcm(&seg) {
            Ok(t) => t.trim().to_string(),
            Err(err) => {
                eprintln!("[warn] stream seg transcribe: {err:?}");
                continue;
            }
        };
        if raw.is_empty() {
            continue;
        }
        // Fix known proper nouns in this segment *before* cleanup — same as the
        // whole-buffer path — so the small model sees the intended spelling
        // (e.g. "to do list" → "Todoist") instead of mangling a garbled one.
        // The final assembled text is still refined again at release.
        // Screen-context vocab isn't known until release (focus capture), so
        // streamed segments use the static corrections vocab only.
        let corrected = refiner.apply_corrections(&raw);
        let prior = clean_acc.join(" ");
        let cleaned = match rt.block_on(cleaner.process_segment_with_context(&corrected, &[], &prior)) {
            Ok(c) => c,
            Err(err) => {
                eprintln!("[warn] stream seg cleanup: {err:?}; using raw");
                corrected.clone()
            }
        };
        eprintln!("  ⟫ seg · {raw:?} → {cleaned:?}");
        raw_acc.push(raw);
        clean_acc.push(cleaned);
    }
}

/// transform on the spoken body with the warm Gemma engine and inject the result
/// at the cursor, like a normal dictation. Unlike `handle_transform` there's no
/// selection and no clipboard round-trip — the body is fresh speech, so we
/// inject straight into the focus target (with the usual AX/clipboard fallback).
/// Cleanup and refinement are deliberately skipped: the output is already the
/// final text (often a non-English translation), and the corrections dictionary
/// + trailing-command parse are English-prose transforms that don't apply.
#[cfg(feature = "cleaner")]
fn handle_spoken_command(
    rt: &tokio::runtime::Runtime,
    cleaner: &TextCleanupEngine,
    cmd: crate::spoken_command::SpokenCommand,
    captured_target: Option<FocusTarget>,
) {
    eprintln!("  ⌘    {} · {} chars", cmd.label, cmd.body.chars().count());
    let t = Instant::now();
    let result = match rt.block_on(cleaner.transform(&cmd.instruction, &cmd.body)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("  ✗    command failed: {e:?}");
            cues::play_error();
            return;
        }
    };
    let result = result.trim();
    if result.is_empty() {
        eprintln!("  ✗    command produced no output");
        cues::play_error();
        return;
    }

    let inject_result = match captured_target {
        Some(target) => AccessibilityInjector::inject_with_target(target, result),
        None => AccessibilityInjector::inject_text(result),
    };
    match inject_result {
        Ok(()) => {
            eprintln!("  ✓    {} ms → \"{result}\"", ms(t.elapsed()));
            ui_channel::set_last_dictation(result);
            crate::history::record(result);
        }
        Err(e) => {
            eprintln!("  ✗    inject failed: {e:?}");
            cues::play_error();
        }
    }
}

/// EXPERIMENTAL hands-free wake-word listener (the `listen` subcommand).
///
/// Runs the always-on mic continuously, segments it at VAD pauses, transcribes
/// each segment with the already-loaded Parakeet model, and watches for the
/// configured wake word. Hearing the wake word "arms" dictation: that segment's
/// body (and every following segment) is cleaned, refined, and injected into the
/// focused field — exactly like push-to-talk, but triggered by voice. The arm
/// auto-expires after `ARM_TIMEOUT` of silence, so ambient conversation after a
/// pause stops flowing into your text.
///
/// This is deliberately a **separate mode** from the push-to-talk daemon, not a
/// background watcher bolted onto it: continuous ASR has a real power/CPU cost
/// (see the assessment in `docs/`), so it should be opt-in and run on its own.
/// The wake-word *decision* is the unit-tested [`crate::wake_word`] primitive;
/// this function is the I/O loop around it. No CGEventTap, no menu bar.
pub fn run_listen(config: DaemonConfig) -> eyre::Result<()> {
    use crate::audio::{AlwaysOnCapture, BUFFER_CAPACITY, SAMPLE_RATE};
    use ringbuf::traits::{Consumer, Observer};

    if !AccessibilityInjector::check_permission() {
        return Err(eyre::eyre!(
            "Accessibility permission required for injection — grant it in System \
             Settings → Privacy & Security → Accessibility, then re-run."
        ));
    }

    let settings = crate::settings::Settings::load();
    let wake_word = settings.resolve_wake_word();

    let t_load = Instant::now();
    let mut worker = LocalInferenceWorker::initialize(&config.parakeet_dir)?;
    eprintln!("[boot] parakeet    loaded in {:>4} ms", ms(t_load.elapsed()));

    #[cfg(feature = "cleaner")]
    let cleaner = if !config.no_cleanup {
        let t = Instant::now();
        let c = TextCleanupEngine::initialize(&config.gemma_path)?;
        eprintln!("[boot] cleaner     loaded in {:>4} ms", ms(t.elapsed()));
        Some(c)
    } else {
        eprintln!("[boot] cleaner     disabled (--no-cleanup)");
        None
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    // Warm both models so the first heard utterance is hot.
    let _ = worker.transcribe_pcm(&vec![0.0_f32; SAMPLE_RATE as usize]);
    #[cfg(feature = "cleaner")]
    if let Some(ref c) = cleaner {
        let _ = rt.block_on(c.process_transcript("warmup")).ok();
    }

    let corrections = Corrections::load_default().unwrap_or_else(|_| Corrections::empty());
    let refiner = Refiner::new(corrections);

    // Continuous capture: hold the recording flag true so every callback pushes
    // into the ring (no pre-roll needed — we're always listening).
    let (mut ao, mut cons) = AlwaysOnCapture::new(0, BUFFER_CAPACITY);
    ao.start()?;
    ao.begin_session();

    let mut seg_stream =
        crate::vad::SegmentStream::new(SAMPLE_RATE, crate::vad::VadConfig::default());

    /// How long after the last dictated segment the listener stays armed before
    /// it requires the wake word again.
    const ARM_TIMEOUT: Duration = Duration::from_secs(8);
    /// Compact the growing VAD buffer once it exceeds this many seconds.
    const COMPACT_AFTER_SECS: usize = 30;

    let mut armed_until: Option<Instant> = None;
    eprintln!();
    eprintln!(
        "👂 listening · say \u{201C}{wake_word}\u{201D} then dictate · trailing commands like \
         \u{201C}press enter\u{201D} still apply · ⌃C to quit"
    );
    eprintln!();

    loop {
        // Non-blocking drain of whatever the mic callback has queued.
        let pending = cons.occupied_len();
        if pending > 0 {
            let mut chunk = vec![0.0_f32; pending];
            let got = cons.pop_slice(&mut chunk);
            seg_stream.push(&chunk[..got]);
        }

        for (s, e) in seg_stream.take_complete() {
            let seg = seg_stream.buf()[s..e].to_vec();
            let raw = match worker.transcribe_pcm(&seg) {
                Ok(t) => t.trim().to_string(),
                Err(err) => {
                    eprintln!("[warn] transcribe: {err:?}");
                    continue;
                }
            };
            if raw.is_empty() {
                continue;
            }

            // Decide whether this segment is dictation: either it opens with the
            // wake word, or we're still armed from a recent trigger.
            let armed = armed_until.map(|t| Instant::now() < t).unwrap_or(false);
            let body = match crate::wake_word::detect(&raw, &wake_word) {
                Some(m) => m.body,                 // wake word heard → its body
                None if armed => raw.clone(),      // armed continuation → whole segment
                None => {
                    eprintln!("  ·    (ignored) {raw:?}");
                    continue;
                }
            };

            // Re-arm the dictation window on every accepted segment.
            armed_until = Some(Instant::now() + ARM_TIMEOUT);

            if body.trim().is_empty() {
                eprintln!("  ⤓    armed · say your text");
                continue;
            }

            #[cfg(feature = "cleaner")]
            listen_dictate(&rt, &refiner, &body, cleaner.as_ref());
            #[cfg(not(feature = "cleaner"))]
            listen_dictate(&refiner, &body);
        }

        // Keep memory bounded over a long listening session.
        if seg_stream.buf().len() > COMPACT_AFTER_SECS * SAMPLE_RATE as usize {
            seg_stream.compact();
        }
        std::thread::sleep(Duration::from_millis(120));
    }
}

/// Clean (if a cleaner is present), refine, and inject one wake-word-triggered
/// dictation body into the focused field, honouring trailing voice commands
/// ("press enter", "scratch that", …) just like push-to-talk.
#[cfg(feature = "cleaner")]
fn listen_dictate(
    rt: &tokio::runtime::Runtime,
    refiner: &Refiner,
    body: &str,
    cleaner: Option<&TextCleanupEngine>,
) {
    let cleaned = match cleaner {
        Some(c) => rt.block_on(c.process_transcript(body)).unwrap_or_else(|_| body.to_string()),
        None => body.to_string(),
    };
    inject_refined(refiner, &cleaned);
}

#[cfg(not(feature = "cleaner"))]
fn listen_dictate(refiner: &Refiner, body: &str) {
    inject_refined(refiner, body);
}

/// Shared tail of `listen_dictate`: run the refiner's terminal classification
/// and act on it (inject text + trailing key, run a bare command, or discard).
fn inject_refined(refiner: &Refiner, text: &str) {
    use crate::refiner::DictationOutcome;
    match refiner.refine(text).outcome() {
        DictationOutcome::Discard => eprintln!("  ⌫    scratch that · discarded"),
        DictationOutcome::Skip => eprintln!("  skip · empty after cleanup"),
        DictationOutcome::BareAction(action) => {
            execute_trailing_action(&action);
            eprintln!("  ↵    bare command");
        }
        DictationOutcome::Inject { text, action } => {
            let target = FocusTarget::capture().ok();
            let res = match target {
                Some(t) => AccessibilityInjector::inject_with_target(t, &text),
                None => AccessibilityInjector::inject_text(&text),
            };
            match res {
                Ok(()) => {
                    eprintln!("  ✓    \"{text}\"");
                    ui_channel::set_last_dictation(&text);
                    crate::history::record(&text);
                    if !matches!(action, TrailingAction::None) {
                        std::thread::sleep(Duration::from_millis(40));
                        execute_trailing_action(&action);
                    }
                }
                Err(e) => {
                    eprintln!("  ✗    inject failed: {e:?}");
                    cues::play_error();
                }
            }
        }
    }
}

/// Run a trailing voice command's keystroke(s). `None`/`Cancel` are no-ops
/// (the caller handles `Cancel` earlier). Errors are logged, never fatal.
fn execute_trailing_action(action: &TrailingAction) {
    use crate::clipboard_paste::{synthesize_escape, synthesize_return, synthesize_tab, synthesize_undo};
    let result = match action {
        TrailingAction::None | TrailingAction::Cancel => return,
        TrailingAction::PressEnter => synthesize_return(),
        TrailingAction::NewParagraph => {
            // Two Returns, with a beat between so apps that debounce key
            // repeat still register both.
            let first = synthesize_return();
            std::thread::sleep(Duration::from_millis(15));
            first.and(synthesize_return())
        }
        TrailingAction::PressTab => synthesize_tab(),
        TrailingAction::PressEscape => synthesize_escape(),
        TrailingAction::Undo => synthesize_undo(),
    };
    match (action, result) {
        (_, Ok(())) => eprintln!("  ↵    {action:?} sent"),
        (_, Err(e)) => eprintln!("  ✗    {action:?} failed: {e:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            PttInput::HotkeyPress { shift: false },
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
        let (decisions, _) = run(&[PttInput::HotkeyPress { shift: true }]);
        assert_eq!(decisions, vec![PttDecision::Start { transform: true }]);
    }

    #[test]
    fn hands_free_full_gesture() {
        // Hold PTT, chord Space, release both, then tap PTT to stop.
        let (decisions, end) = run(&[
            PttInput::HotkeyPress { shift: false }, // Start
            PttInput::SpaceDown,                    // ArmLatch (cue + swallow)
            PttInput::HotkeyRelease,                // → Latched, keep recording
            PttInput::HotkeyPress { shift: false }, // tap → Stop
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
            PttInput::HotkeyPress { shift: false },
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
            PttInput::HotkeyPress { shift: false },
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
            PttInput::HotkeyPress { shift: false },
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
            PttInput::HotkeyPress { shift: false },
            PttInput::HotkeyPress { shift: false },
        ]);
        assert_eq!(
            decisions,
            vec![PttDecision::Start { transform: false }, PttDecision::Nothing]
        );
        assert_eq!(end, ST_HOLDING);
    }

    #[test]
    fn normal_then_handsfree_back_to_back() {
        // A plain hold/release, then a hands-free session — state returns to
        // idle cleanly between them.
        let (_decisions, end) = run(&[
            PttInput::HotkeyPress { shift: false },
            PttInput::HotkeyRelease, // Stop → Idle
            PttInput::HotkeyPress { shift: false },
            PttInput::SpaceDown,
            PttInput::HotkeyRelease, // Latched
            PttInput::HotkeyPress { shift: false },
            PttInput::HotkeyRelease, // Stop → Idle
        ]);
        assert_eq!(end, ST_IDLE);
    }
}
