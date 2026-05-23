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
use crate::cues;
use crate::injector::{AccessibilityInjector, FocusTarget};
use crate::menubar::{self, UiState};
use crate::transcriber::LocalInferenceWorker;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::event::{
    CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType, CallbackResult, EventField,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
        let hotkey_keycode = std::env::var("DICTATE_HOTKEY_KEYCODE")
            .ok()
            .and_then(|s| {
                if let Some(rest) = s.strip_prefix("0x") {
                    i64::from_str_radix(rest, 16).ok()
                } else {
                    s.parse().ok()
                }
            })
            .unwrap_or(DEFAULT_HOTKEY_KEYCODE);
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
                let is_set_now = flags.contains(CGEventFlags::CGEventFlagAlternate);
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
        "[daemon] ready. Hold Right Option (keycode 0x{:x}) to dictate. ⌘Q in the menu-bar item quits.",
        hotkey_keycode
    );
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
    std::env::var("DICTATE_HOTKEY_KEYCODE")
        .ok()
        .and_then(|s| {
            if let Some(rest) = s.strip_prefix("0x") {
                i64::from_str_radix(rest, 16).ok()
            } else {
                s.parse().ok()
            }
        })
        .unwrap_or(DEFAULT_HOTKEY_KEYCODE)
}

fn worker_loop(
    rx: std::sync::mpsc::Receiver<DaemonEvent>,
    config: DaemonConfig,
) -> eyre::Result<()> {
    eprintln!("[daemon] loading Parakeet TDT v3 INT8 from {}", config.parakeet_dir);
    let t_load = Instant::now();
    let mut worker = LocalInferenceWorker::initialize(&config.parakeet_dir)?;
    eprintln!("[daemon] parakeet loaded in {:?}", t_load.elapsed());

    #[cfg(feature = "cleaner")]
    let cleaner = if !config.no_cleanup {
        let pretty = std::path::Path::new(&config.gemma_path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&config.gemma_path);
        eprintln!("[daemon] loading cleanup model: {pretty}");
        let t = Instant::now();
        let c = TextCleanupEngine::initialize(&config.gemma_path)?;
        eprintln!("[daemon] cleanup model loaded in {:?}", t.elapsed());
        Some(c)
    } else {
        None
    };

    // Tokio current-thread runtime for the async cleaner path.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    // Warm-up: first call pays graph-optimization + CoreML compile / Metal
    // shader compile costs. Pay them now so the first real utterance is hot.
    eprintln!("[daemon] warming up models...");
    let t_warm = Instant::now();
    let warm_silence = vec![0.0_f32; 16_000]; // 1 s of silence at 16 kHz
    let _ = worker.transcribe_pcm(&warm_silence);
    #[cfg(feature = "cleaner")]
    if let Some(ref c) = cleaner {
        let _ = rt.block_on(c.process_transcript("warmup")).ok();
    }
    eprintln!("[daemon] warm-up done in {:?}", t_warm.elapsed());

    // Per-utterance state.
    let mut engine: Option<AudioCaptureEngine> = None;
    let mut consumer: Option<crate::audio::HeapAudioConsumer> = None;
    let mut is_recording: Option<std::sync::Arc<std::sync::atomic::AtomicBool>> = None;
    let mut t_press: Option<Instant> = None;

    while let Ok(event) = rx.recv() {
        match event {
            DaemonEvent::StartRecording => {
                if engine.is_some() {
                    continue; // already recording (key bounced?)
                }
                t_press = Some(Instant::now());
                let (mut e, c) = AudioCaptureEngine::new(BUFFER_CAPACITY);
                let r = match e.start_microphone() {
                    Ok(r) => r,
                    Err(err) => {
                        eprintln!("[daemon] mic open failed: {err:?}");
                        cues::play_error();
                        continue;
                    }
                };
                cues::play_start();
                menubar::set_state(UiState::Recording);
                eprintln!("[daemon] ● recording");
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
                eprintln!(
                    "[daemon] ○ stopped after {:?}",
                    press_to_release.unwrap_or_default()
                );

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
                        eprintln!("[daemon] transcribe failed: {err:?}");
                        cues::play_error();
                        menubar::set_state(UiState::Idle);
                        continue;
                    }
                };
                let t_transcribe = t_pipeline.elapsed();

                let final_text = if transcript.trim().is_empty() {
                    eprintln!("[daemon] empty transcript — skipping");
                    menubar::set_state(UiState::Idle);
                    continue;
                } else {
                    #[cfg(feature = "cleaner")]
                    {
                        if let Some(ref c) = cleaner {
                            let t_clean = Instant::now();
                            match rt.block_on(c.process_transcript(&transcript)) {
                                Ok(cleaned) => {
                                    eprintln!(
                                        "[daemon] cleanup: {:?} | transcribe: {:?} | total: {:?}",
                                        t_clean.elapsed(),
                                        t_transcribe,
                                        t_pipeline.elapsed()
                                    );
                                    cleaned
                                }
                                Err(err) => {
                                    eprintln!(
                                        "[daemon] cleanup failed, using raw transcript: {err:?}"
                                    );
                                    transcript
                                }
                            }
                        } else {
                            eprintln!("[daemon] cleanup skipped | transcribe: {t_transcribe:?}");
                            transcript
                        }
                    }
                    #[cfg(not(feature = "cleaner"))]
                    {
                        eprintln!("[daemon] transcribe: {t_transcribe:?}");
                        transcript
                    }
                };

                if final_text.trim().is_empty() {
                    continue;
                }

                // Inject using the pre-captured focus target if it's ready
                // (it almost always is — capture takes 30-150 ms; inference
                // takes 200-500 ms). Falls back to fresh inject path on
                // capture failure (e.g. nothing was focused at key release).
                let t_inject = Instant::now();
                let inject_result = match target_rx.try_recv() {
                    Ok(Ok(target)) => {
                        let ax_wait = t_ax_start.elapsed();
                        eprintln!(
                            "[daemon] focus captured in {ax_wait:?} (parallel with inference)"
                        );
                        AccessibilityInjector::inject_with_target(target, &final_text)
                    }
                    Ok(Err(e)) => {
                        eprintln!(
                            "[daemon] focus capture failed ({e:?}); using fresh inject"
                        );
                        AccessibilityInjector::inject_text(&final_text)
                    }
                    Err(_) => {
                        // Capture not done yet — block briefly for it.
                        match target_rx.recv() {
                            Ok(Ok(t)) => AccessibilityInjector::inject_with_target(t, &final_text),
                            _ => AccessibilityInjector::inject_text(&final_text),
                        }
                    }
                };
                if let Err(err) = inject_result {
                    eprintln!("[daemon] inject failed: {err:?}");
                    cues::play_error();
                } else {
                    eprintln!(
                        "[daemon] injected ({} chars) in {:?} → \"{final_text}\"",
                        final_text.len(),
                        t_inject.elapsed()
                    );
                }
                menubar::set_state(UiState::Idle);
            }
        }

        // Give the audio cue a moment to actually start playing before we
        // potentially re-enter recording on a fast double-tap.
        std::thread::sleep(Duration::from_millis(10));
    }

    Ok(())
}
