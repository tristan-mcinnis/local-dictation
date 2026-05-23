use fast_dictate_backend::{
    audio::{AudioCaptureEngine, BUFFER_CAPACITY},
    injector::AccessibilityInjector,
    transcriber::LocalInferenceWorker,
};
#[cfg(feature = "parakeet")]
use fast_dictate_backend::cues;

#[cfg(feature = "parakeet")]
use std::time::Instant;

// Default model locations — kept inside the project tree so they're
// discoverable + cached alongside the code.
#[cfg(feature = "parakeet")]
const PARAKEET_DEFAULT_REL: &str = "models/parakeet-tdt-v3-int8";
#[cfg(all(feature = "parakeet", feature = "cleaner"))]
const GEMMA_DEFAULT_REL: &str = "models/qwen-2.5-0.5b-it/Qwen2.5-0.5B-Instruct-Q4_K_M.gguf";

// cpal::Stream is !Send on macOS — pin the runtime to one thread so the
// engine can stay put.
#[tokio::main(flavor = "current_thread")]
async fn main() -> eyre::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let subcommand = args.get(1).map(String::as_str).unwrap_or("mock-loop");

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
        other => Err(eyre::eyre!(
            "unknown subcommand `{other}` — use one of: mock-loop, bench [wav], dictate [ms], daemon [--no-cleanup], inject-test [text], ax-check"
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
    std::env::var("PARAKEET_MODEL_DIR").unwrap_or_else(|_| PARAKEET_DEFAULT_REL.to_string())
}

#[cfg(all(feature = "parakeet", feature = "cleaner"))]
fn gemma_path() -> String {
    std::env::var("GEMMA_MODEL_PATH").unwrap_or_else(|_| GEMMA_DEFAULT_REL.to_string())
}

#[cfg(all(target_os = "macos", feature = "parakeet", feature = "ax-inject"))]
async fn run_daemon(no_cleanup: bool) -> eyre::Result<()> {
    use fast_dictate_backend::daemon::{run, DaemonConfig};
    // Spawn the daemon on a blocking thread because it calls CFRunLoop::run_current()
    // which blocks indefinitely — we don't want to occupy the tokio runtime.
    let config = DaemonConfig::from_env(
        parakeet_dir(),
        #[cfg(feature = "cleaner")]
        gemma_path(),
        no_cleanup,
    );
    tokio::task::spawn_blocking(move || run(config))
        .await
        .map_err(|e| eyre::eyre!("daemon task join failed: {e}"))?
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
