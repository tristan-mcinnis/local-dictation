use fast_dictate_backend::{
    audio::{AudioCaptureEngine, BUFFER_CAPACITY},
    injector::AccessibilityInjector,
    transcriber::LocalInferenceWorker,
};
#[cfg(feature = "parakeet")]
use fast_dictate_backend::cues;

#[cfg(feature = "parakeet")]
use std::time::Instant;

// cpal::Stream is !Send on macOS — pin the runtime to one thread so the
// engine can stay put.
#[tokio::main(flavor = "current_thread")]
async fn main() -> eyre::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    // A double-clicked `.app` is launched with no arguments — when we detect
    // we're running from inside a bundle, the daemon *is* the app, so default
    // to it. From the repo/CLI the default stays `mock-loop` (harmless probe).
    let default_cmd = if fast_dictate_backend::app_paths::running_in_bundle() {
        "daemon"
    } else {
        "mock-loop"
    };
    let subcommand = args.get(1).map(String::as_str).unwrap_or(default_cmd);

    match subcommand {
        "mock-loop" => run_mock_loop().await,
        "bench" => {
            let wav = args
                .get(2)
                .cloned()
                .unwrap_or_else(|| "testdata/dictation_sample.wav".to_string());
            run_bench(&wav).await
        }
        "dictate" => {
            let duration_ms = args
                .get(2)
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(5_000);
            run_dictate(duration_ms).await
        }
        "ax-check" => run_ax_check(),
        "inject-test" => run_inject_test(args.get(2).cloned()),
        "daemon" => run_daemon(args.iter().any(|a| a == "--no-cleanup")).await,
        "synth-press" => run_synth_press(
            args.get(2).and_then(|s| s.parse().ok()).unwrap_or(2000),
        ),
        "logs" => run_logs(),
        "transform" => {
            run_transform(args.get(2).cloned(), args.get(3).cloned()).await
        }
        other => Err(eyre::eyre!(
            "unknown subcommand `{other}` — use one of: daemon [--no-cleanup], logs, bench [wav], dictate [ms], inject-test [text], transform \"<instruction>\" \"<text>\", ax-check, mock-loop"
        )),
    }
}

async fn run_mock_loop() -> eyre::Result<()> {
    let mut worker = LocalInferenceWorker::initialize_mock();
    let (mut engine, consumer) = AudioCaptureEngine::new(BUFFER_CAPACITY);
    let is_recording = engine.start_mock_loop();
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    engine.stop_capture();
    let transcription = worker.run_inference_pipeline(consumer, is_recording).await?;
    AccessibilityInjector::inject_text(&transcription)?;
    println!("Pipeline finished. Transcript length: {}", transcription.len());
    Ok(())
}

fn run_inject_test(text: Option<String>) -> eyre::Result<()> {
    let text = text.unwrap_or_else(|| "hello from fast-dictate-backend!".to_string());
    inject_with_optional_focus(&text)?;
    println!("[inject-test] injected: {text:?}");
    Ok(())
}

/// If FOCUS_APP is set, ensure that app has a writable doc and inject into
/// its AXUIElement by PID (works even when cargo's terminal is OS-frontmost).
/// Otherwise inject via the system-wide focused element (production path).
fn inject_with_optional_focus(text: &str) -> eyre::Result<()> {
    let Ok(app) = std::env::var("FOCUS_APP") else {
        return AccessibilityInjector::inject_text(text);
    };
    let setup = format!(
        "tell application \"{app}\"\n\
            activate\n\
            if (count of documents) is 0 then make new document\n\
         end tell\n\
         delay 0.5\n\
         tell application \"System Events\"\n\
            tell process \"{app}\" to set frontmost to true\n\
            delay 0.2\n\
            keystroke \" \"\n\
            key code 51\n\
         end tell"
    );
    let _ = std::process::Command::new("osascript")
        .args(["-e", &setup])
        .status();
    // Resolve PID — Unix id of frontmost process named `app`.
    let pid_script = format!(
        "tell application \"System Events\" to get unix id of first process whose name is \"{app}\""
    );
    let out = std::process::Command::new("osascript")
        .args(["-e", &pid_script])
        .output()?;
    let pid: i32 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .map_err(|_| eyre::eyre!("could not resolve PID for {app}: {:?}", out))?;
    println!("[inject] targeting {app} (pid {pid})");
    AccessibilityInjector::inject_into_pid(pid, text)
}

/// Apply the same deterministic refinement the daemon uses — personal
/// corrections dictionary, then trailing voice-command parsing — to a
/// cleaned transcript, returning the body to inject. This keeps the
/// `bench` / `dictate` subcommands faithful to production instead of
/// silently skipping corrections + commands. The CLI paths don't execute
/// the trailing action (no key synthesis); we just note it.
#[cfg(feature = "parakeet")]
fn refine_for_cli(cleaned: &str) -> String {
    use fast_dictate_backend::corrections::Corrections;
    use fast_dictate_backend::refiner::Refiner;
    use fast_dictate_backend::voice_commands::TrailingAction;

    let corrections = Corrections::load_default().unwrap_or_else(|_| Corrections::empty());
    let refined = Refiner::new(corrections).refine(cleaned);
    if refined.action != TrailingAction::None {
        println!("[refine] parsed trailing command {:?} (not executed in CLI mode)", refined.action);
    }
    refined.text
}

/// Open the daemon log in the user's default text editor / Console.app.
/// Same path the menu-bar "Open Log" item uses.
fn run_logs() -> eyre::Result<()> {
    let log_path = "/tmp/dictate-daemon.log";
    if !std::path::Path::new(log_path).exists() {
        eprintln!(
            "[logs] {log_path} doesn't exist yet — start the daemon first with `daemon`."
        );
        return Ok(());
    }
    // `open -t` uses the default .txt editor (Console.app on most setups).
    let status = std::process::Command::new("open")
        .args(["-t", log_path])
        .status()?;
    if !status.success() {
        return Err(eyre::eyre!("`open` exited with {status}"));
    }
    println!("[logs] opened {log_path}");
    Ok(())
}

fn run_ax_check() -> eyre::Result<()> {
    let trusted = AccessibilityInjector::check_permission();
    if trusted {
        println!("[ax-check] this binary IS trusted for Accessibility — text injection will work");
    } else {
        println!("[ax-check] NOT trusted. macOS should have just shown a prompt; if not, open");
        println!("           System Settings → Privacy & Security → Accessibility and toggle");
        println!("           the entry for this binary on, then re-run.");
    }
    Ok(())
}

#[cfg(not(feature = "parakeet"))]
async fn run_bench(_wav: &str) -> eyre::Result<()> {
    Err(eyre::eyre!(
        "`bench` requires the `parakeet` cargo feature (try --features full)"
    ))
}

#[cfg(not(feature = "parakeet"))]
async fn run_dictate(_ms: u64) -> eyre::Result<()> {
    Err(eyre::eyre!(
        "`dictate` requires the `parakeet` cargo feature (try --features full)"
    ))
}

#[cfg(feature = "parakeet")]
async fn run_bench(wav_path: &str) -> eyre::Result<()> {
    use fast_dictate_backend::transcriber::load_wav_mono16k;

    let parakeet_dir = parakeet_dir();
    println!("[bench] parakeet model dir: {parakeet_dir}");
    println!("[bench] input wav:          {wav_path}");

    let t_load_audio = Instant::now();
    let audio = load_wav_mono16k(wav_path)?;
    let audio_secs = audio.len() as f64 / 16_000.0;
    println!(
        "[bench] loaded {} samples ({:.2} s of audio) in {:?}",
        audio.len(),
        audio_secs,
        t_load_audio.elapsed()
    );

    let t_load_model = Instant::now();
    let mut worker = LocalInferenceWorker::initialize(&parakeet_dir)?;
    println!(
        "[bench] parakeet model loaded in {:?} (one-time cost)",
        t_load_model.elapsed()
    );

    let t_warm = Instant::now();
    let _ = worker.transcribe_pcm(&audio)?;
    let warm_dur = t_warm.elapsed();

    let t_hot = Instant::now();
    let transcript = worker.transcribe_pcm(&audio)?;
    let hot_dur = t_hot.elapsed();

    println!();
    println!("[bench] WARM-UP transcribe:  {warm_dur:?}");
    println!(
        "[bench] HOT     transcribe:  {hot_dur:?}   ({:.2}x realtime)",
        audio_secs / hot_dur.as_secs_f64()
    );
    println!("[bench] transcript: {transcript:?}");

    #[cfg(feature = "cleaner")]
    let final_text = {
        use fast_dictate_backend::cleaner::TextCleanupEngine;
        let gemma_path = gemma_path();
        println!();
        println!("[bench] gemma model: {gemma_path}");

        let t_g_load = Instant::now();
        let cleaner = TextCleanupEngine::initialize(&gemma_path)?;
        println!(
            "[bench] gemma loaded in {:?} (one-time cost)",
            t_g_load.elapsed()
        );

        let t_clean_warm = Instant::now();
        let _ = cleaner.process_transcript(&transcript).await?;
        let clean_warm = t_clean_warm.elapsed();

        let t_clean_hot = Instant::now();
        let cleaned = cleaner.process_transcript(&transcript).await?;
        let clean_hot = t_clean_hot.elapsed();

        println!("[bench] cleanup WARM-UP:     {clean_warm:?}");
        println!("[bench] cleanup HOT:         {clean_hot:?}");
        println!("[bench] cleaned: {cleaned:?}");
        println!();
        println!(
            "[bench] END-TO-END HOT (transcribe + cleanup): {:?}",
            hot_dur + clean_hot
        );
        cleaned
    };
    #[cfg(not(feature = "cleaner"))]
    let final_text = transcript;

    let final_text = refine_for_cli(&final_text);
    inject_with_optional_focus(&final_text)?;
    Ok(())
}

#[cfg(feature = "parakeet")]
async fn run_dictate(duration_ms: u64) -> eyre::Result<()> {
    let parakeet_dir = parakeet_dir();
    println!("[dictate] parakeet model dir: {parakeet_dir}");
    println!(
        "[dictate] recording for {} ms — speak now...",
        duration_ms
    );

    let t_load_model = Instant::now();
    let mut worker = LocalInferenceWorker::initialize(&parakeet_dir)?;
    println!(
        "[dictate] parakeet loaded in {:?}",
        t_load_model.elapsed()
    );

    #[cfg(feature = "cleaner")]
    let cleaner = {
        use fast_dictate_backend::cleaner::TextCleanupEngine;
        let gemma_path = gemma_path();
        let t = Instant::now();
        let c = TextCleanupEngine::initialize(&gemma_path)?;
        println!("[dictate] gemma   loaded in {:?}", t.elapsed());
        c
    };

    let (mut engine, consumer) = AudioCaptureEngine::new(BUFFER_CAPACITY);
    let t_capture_start = Instant::now();
    let is_recording = engine.start_microphone()?;
    cues::play_start();
    println!("[dictate] recording... (Ctrl+C to cancel without injecting)");

    tokio::time::sleep(std::time::Duration::from_millis(duration_ms)).await;

    engine.stop_capture();
    cues::play_stop();
    let capture_dur = t_capture_start.elapsed();
    println!("[dictate] captured {capture_dur:?} of audio");

    let t_drain = Instant::now();
    let transcript = worker
        .run_inference_pipeline(consumer, is_recording)
        .await?;
    let drain_and_transcribe = t_drain.elapsed();
    println!(
        "[dictate] drain+transcribe: {:?}",
        drain_and_transcribe
    );
    println!("[dictate] raw transcript: {transcript:?}");

    #[cfg(feature = "cleaner")]
    let final_text = {
        let t = Instant::now();
        let cleaned = cleaner.process_transcript(&transcript).await?;
        println!("[dictate] cleanup:          {:?}", t.elapsed());
        println!("[dictate] cleaned:          {cleaned:?}");
        cleaned
    };
    #[cfg(not(feature = "cleaner"))]
    let final_text = transcript;

    let final_text = refine_for_cli(&final_text);
    if final_text.trim().is_empty() {
        println!("[dictate] empty result — skipping injection");
        return Ok(());
    }

    println!("[dictate] injecting into focused app...");
    if let Err(e) = inject_with_optional_focus(&final_text) {
        cues::play_error();
        return Err(e);
    }
    println!("[dictate] done.");
    Ok(())
}

#[cfg(feature = "parakeet")]
fn parakeet_dir() -> String {
    // Precedence: PARAKEET_MODEL_DIR env > bundle/App-Support/repo default.
    std::env::var("PARAKEET_MODEL_DIR")
        .unwrap_or_else(|_| fast_dictate_backend::app_paths::parakeet_default_dir())
}

#[cfg(all(feature = "parakeet", feature = "cleaner"))]
fn gemma_path() -> String {
    // Precedence: GEMMA_MODEL_PATH env > settings.json > bundle-aware default.
    use fast_dictate_backend::settings::Settings;
    Settings::load().resolve_gemma(&fast_dictate_backend::app_paths::gemma_default_path())
}

/// `transform "<instruction>" "<text>"` — run the warm-Gemma transform path on
/// the command line, no audio/AX. Lets transform-mode prompting be eyeballed
/// (and tuned) without speaking into the daemon.
#[cfg(feature = "cleaner")]
async fn run_transform(instruction: Option<String>, text: Option<String>) -> eyre::Result<()> {
    use fast_dictate_backend::cleaner::TextCleanupEngine;
    use fast_dictate_backend::settings::Settings;

    let (Some(instruction), Some(text)) = (instruction, text) else {
        return Err(eyre::eyre!(
            "usage: transform \"<instruction>\" \"<text to transform>\""
        ));
    };
    let gemma = Settings::load().resolve_gemma(&fast_dictate_backend::app_paths::gemma_default_path());
    println!("[transform] gemma: {gemma}");
    let t = std::time::Instant::now();
    let cleaner = TextCleanupEngine::initialize(&gemma)?;
    println!("[transform] loaded in {:?}", t.elapsed());

    let t = std::time::Instant::now();
    let out = cleaner.transform(&instruction, &text).await?;
    println!("[transform] instruction : {instruction:?}");
    println!("[transform] input       : {text:?}");
    println!("[transform] output ({:?}): {out:?}", t.elapsed());
    println!("\n{out}");
    Ok(())
}

#[cfg(not(feature = "cleaner"))]
async fn run_transform(_i: Option<String>, _t: Option<String>) -> eyre::Result<()> {
    Err(eyre::eyre!("transform requires the `cleaner` feature"))
}

/// When launched from a `.app` (Finder / login item) there's no terminal, so
/// `eprintln!` output would vanish. Point stdout+stderr at the same log file
/// the menu's relaunch path uses, so the structured per-utterance log works
/// from the very first launch. No-op when not bundled (keep terminal output).
#[cfg(all(target_os = "macos", feature = "parakeet", feature = "ax-inject"))]
fn redirect_stdio_to_log_if_bundled() {
    use std::os::unix::io::AsRawFd;
    if !fast_dictate_backend::app_paths::running_in_bundle() {
        return;
    }
    if let Ok(f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/dictate-daemon.log")
    {
        unsafe {
            libc::dup2(f.as_raw_fd(), 1);
            libc::dup2(f.as_raw_fd(), 2);
        }
        // Leak the handle so the fd stays valid for the process's lifetime.
        std::mem::forget(f);
    }
}

#[cfg(all(target_os = "macos", feature = "parakeet", feature = "ax-inject"))]
async fn run_daemon(no_cleanup: bool) -> eyre::Result<()> {
    use fast_dictate_backend::daemon::{run, DaemonConfig};
    redirect_stdio_to_log_if_bundled();
    // IMPORTANT: must call daemon::run on the OS main thread (NSApp.run()
    // refuses to start otherwise). We're already on the main thread thanks
    // to #[tokio::main(flavor = "current_thread")] — DO NOT wrap in
    // spawn_blocking, that moves the call to a worker thread and
    // MainThreadMarker::new() will return None.
    // The menu-bar cleanup toggle lives in settings.json; a CLI --no-cleanup
    // is a hard override that always wins.
    #[cfg(feature = "cleaner")]
    let no_cleanup = !fast_dictate_backend::settings::Settings::load()
        .resolve_cleanup_enabled(no_cleanup);
    let config = DaemonConfig::from_env(
        parakeet_dir(),
        #[cfg(feature = "cleaner")]
        gemma_path(),
        no_cleanup,
    );
    run(config)
}

#[cfg(not(all(target_os = "macos", feature = "parakeet", feature = "ax-inject")))]
async fn run_daemon(_no_cleanup: bool) -> eyre::Result<()> {
    Err(eyre::eyre!(
        "`daemon` requires --features full (parakeet + cleaner + ax-inject) on macOS"
    ))
}

/// Hidden helper: synthesize Right-Option hold for `hold_ms` ms via CGEvent.
/// Lets the running daemon be tested from a second process.
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
fn run_synth_press(hold_ms: u64) -> eyre::Result<()> {
    use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    const RIGHT_OPTION_KEYCODE: u16 = 0x3D;

    let src = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| eyre::eyre!("CGEventSource::new failed"))?;

    let down = CGEvent::new_keyboard_event(src.clone(), RIGHT_OPTION_KEYCODE, true)
        .map_err(|_| eyre::eyre!("down event create failed"))?;
    down.set_flags(CGEventFlags::CGEventFlagAlternate);
    down.post(CGEventTapLocation::HID);
    println!("[synth] Right Option DOWN");

    std::thread::sleep(std::time::Duration::from_millis(hold_ms));

    let up = CGEvent::new_keyboard_event(src, RIGHT_OPTION_KEYCODE, false)
        .map_err(|_| eyre::eyre!("up event create failed"))?;
    up.set_flags(CGEventFlags::CGEventFlagNull);
    up.post(CGEventTapLocation::HID);
    println!("[synth] Right Option UP");
    Ok(())
}

#[cfg(not(all(target_os = "macos", feature = "ax-inject")))]
fn run_synth_press(_hold_ms: u64) -> eyre::Result<()> {
    Err(eyre::eyre!("synth-press requires --features ax-inject on macOS"))
}
