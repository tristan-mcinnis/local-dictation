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

    // Per-utterance state.
    let mut engine: Option<AudioCaptureEngine> = None;
    let mut consumer: Option<crate::audio::HeapAudioConsumer> = None;
    let mut is_recording: Option<std::sync::Arc<std::sync::atomic::AtomicBool>> = None;
    let mut t_press: Option<Instant> = None;
    // Whether the in-flight utterance is a transform instruction (Shift held).
    let mut transform_mode = false;

    while let Ok(event) = rx.recv() {
        match event {
            DaemonEvent::LatchArmed => {
                // Hands-free engaged mid-hold: the mic keeps running once the
                // keys are released. Just give audible feedback + a log line.
                cues::play_latch();
                eprintln!("⤓ hands-free · release keys, keep talking · tap PTT to stop");
            }
            DaemonEvent::StartRecording { transform } => {
                if engine.is_some() {
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
                engine = Some(e);
                consumer = Some(c);
                is_recording = Some(r);
            }
            DaemonEvent::StopRecording => {
                let Some(mut e) = engine.take() else { continue };
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
                e.stop_capture();
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

                let c = consumer.take().unwrap();
                let r = is_recording.take().unwrap();

                let t_pipeline = Instant::now();
                let transcript = match rt.block_on(worker.run_inference_pipeline(c, r)) {
                    Ok(t) => t,
                    Err(err) => {
                        eprintln!("[err]  transcribe failed: {err:?}");
                        cues::play_error();
                        ui_channel::set_state(UiState::Idle);
                        eprintln!();
                        continue;
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
                            Some(ref c) if !is_terminal => {
                                if !screen_vocab.is_empty() {
                                    eprintln!("  ⌕    screen vocab: {}", screen_vocab.join(", "));
                                }
                                let t_clean = Instant::now();
                                match rt.block_on(c.process_transcript_with_vocab(&transcript, &screen_vocab)) {
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

/// Run a trailing voice command's keystroke(s). `None`/`Cancel` are no-ops
/// (the caller handles `Cancel` earlier). Errors are logged, never fatal.
fn execute_trailing_action(action: &TrailingAction) {
    use crate::clipboard_paste::{synthesize_return, synthesize_tab};
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
