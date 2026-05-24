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
use crate::menubar::{self, UiState};
use crate::transcriber::LocalInferenceWorker;
use crate::voice_commands::{parse_trailing_command, TrailingAction};
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::event::{
    CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType, CallbackResult, EventField,
};
use std::sync::atomic::{AtomicBool, Ordering};
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
    StartRecording,
    StopRecording,
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
    let currently_pressed = Arc::new(AtomicBool::new(false));
    let pressed_for_callback = currently_pressed.clone();

    // Install CGEventTap manually so we can hand the main thread over to
    // NSApplication.run() (needed for the menu-bar item + floating pill).
    let tap = CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::ListenOnly,
        vec![CGEventType::FlagsChanged],
        move |_proxy, _etype, event| {
            let keycode = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
            if keycode == hotkey_keycode {
                let flags = event.get_flags();
                let is_set_now = flags.contains(hotkey_flag);
                let was_pressed = pressed_for_callback.load(Ordering::SeqCst);
                if is_set_now && !was_pressed {
                    pressed_for_callback.store(true, Ordering::SeqCst);
                    let _ = tx_for_callback.send(DaemonEvent::StartRecording);
                } else if !is_set_now && was_pressed {
                    pressed_for_callback.store(false, Ordering::SeqCst);
                    let _ = tx_for_callback.send(DaemonEvent::StopRecording);
                }
            }
            CallbackResult::Keep
        },
    )
    .map_err(|_| {
        eyre::eyre!(
            "failed to install CGEventTap — Accessibility permission missing or revoked"
        )
    })?;

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
        "[boot] ready · hold {} (0x{:x}) to dictate · ⌘Q quits",
        crate::settings::hotkey_name(hotkey_keycode),
        hotkey_keycode
    );
    eprintln!();
    menubar::set_state(UiState::Idle);

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

    // Load personal corrections dictionary (no-op if no file).
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

    // Per-utterance state.
    let mut engine: Option<AudioCaptureEngine> = None;
    let mut consumer: Option<crate::audio::HeapAudioConsumer> = None;
    let mut is_recording: Option<std::sync::Arc<std::sync::atomic::AtomicBool>> = None;
    let mut t_press: Option<Instant> = None;

    while let Ok(event) = rx.recv() {
        match event {
            DaemonEvent::StartRecording => {
                if engine.is_some() {
                    continue;
                }
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
                menubar::set_state(UiState::Recording);
                eprintln!("▶ recording");
                engine = Some(e);
                consumer = Some(c);
                is_recording = Some(r);
            }
            DaemonEvent::StopRecording => {
                let Some(mut e) = engine.take() else { continue };
                e.stop_capture();
                cues::play_stop();
                menubar::set_state(UiState::Processing);
                let press_to_release = t_press.take().map(|t| t.elapsed());
                let held = press_to_release.unwrap_or_default();
                eprintln!("⏹ stopped · held {}", secs(held));

                // Spawn AX focus capture in parallel with the inference
                // pipeline. By the time Parakeet+Gemma finish, the focused
                // element is already known — get_focused_element drops off
                // the critical path.
                let t_ax_start = Instant::now();
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
                        menubar::set_state(UiState::Idle);
                        eprintln!();
                        continue;
                    }
                };
                let t_transcribe = t_pipeline.elapsed();

                let mut t_cleanup_ms = 0_u128;
                let final_text = if transcript.trim().is_empty() {
                    eprintln!("  skip · empty transcript");
                    eprintln!();
                    menubar::set_state(UiState::Idle);
                    continue;
                } else {
                    #[cfg(feature = "cleaner")]
                    {
                        if let Some(ref c) = cleaner {
                            let t_clean = Instant::now();
                            match rt.block_on(c.process_transcript(&transcript)) {
                                Ok(cleaned) => {
                                    t_cleanup_ms = ms(t_clean.elapsed());
                                    cleaned
                                }
                                Err(err) => {
                                    eprintln!("[warn] cleanup failed, using raw: {err:?}");
                                    transcript
                                }
                            }
                        } else {
                            transcript
                        }
                    }
                    #[cfg(not(feature = "cleaner"))]
                    {
                        transcript
                    }
                };

                if final_text.trim().is_empty() {
                    eprintln!("  skip · cleaned to empty");
                    eprintln!();
                    menubar::set_state(UiState::Idle);
                    continue;
                }

                // Apply user corrections dictionary (proper nouns, common
                // misspellings, domain casing) BEFORE the voice-command
                // parse so corrections to the trigger phrase don't fire.
                let final_text = corrections.apply(&final_text);

                // Voice command detection: trailing "press enter" is
                // stripped and turned into a Return keystroke after inject.
                let (final_text, action) = parse_trailing_command(&final_text);
                let press_enter = matches!(action, TrailingAction::PressEnter);

                if final_text.trim().is_empty() && press_enter {
                    let _ = crate::clipboard_paste::synthesize_return();
                    eprintln!("  ↵    bare Return (no body)");
                    eprintln!();
                    menubar::set_state(UiState::Idle);
                    continue;
                }

                // Inject using the pre-captured focus target.
                let t_inject = Instant::now();
                let (inject_result, target_pid, used_fresh_capture) = match target_rx.try_recv() {
                    Ok(Ok(target)) => {
                        let pid = target.inner_pid();
                        (
                            AccessibilityInjector::inject_with_target(target, &final_text),
                            pid,
                            false,
                        )
                    }
                    Ok(Err(_)) => (
                        AccessibilityInjector::inject_text(&final_text),
                        None,
                        true,
                    ),
                    Err(_) => match target_rx.recv() {
                        Ok(Ok(t)) => {
                            let pid = t.inner_pid();
                            (
                                AccessibilityInjector::inject_with_target(t, &final_text),
                                pid,
                                false,
                            )
                        }
                        _ => (AccessibilityInjector::inject_text(&final_text), None, true),
                    },
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
                if let Err(err) = &inject_result {
                    eprintln!("  ✗    inject failed: {err:?}");
                    cues::play_error();
                } else {
                    eprintln!("  ✓    \"{final_text}\"");
                    // Remember it for the menu-bar "Copy last dictation" item.
                    menubar::set_last_dictation(&final_text);
                    // Persist to the browsable dictation history (best-effort;
                    // never blocks or fails the hot path).
                    crate::history::record(&final_text);
                    if press_enter {
                        std::thread::sleep(Duration::from_millis(40));
                        match crate::clipboard_paste::synthesize_return() {
                            Ok(_) => eprintln!("  ↵    Return key sent"),
                            Err(e) => eprintln!("  ✗    synth return failed: {e:?}"),
                        }
                    }
                }
                eprintln!();
                menubar::set_state(UiState::Idle);
            }
        }

        // Give the audio cue a moment to actually start playing before we
        // potentially re-enter recording on a fast double-tap.
        std::thread::sleep(Duration::from_millis(10));
    }

    Ok(())
}
