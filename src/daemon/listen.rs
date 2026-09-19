//! `listen` mode: the wake-word / hands-free variant of the daemon loop.
use super::*;

/// EXPERIMENTAL hands-free wake-word listener (the `listen` subcommand).
///
/// Runs the always-on mic continuously, segments it at VAD pauses, transcribes
/// each segment with the already-loaded Cohere model, and watches for the
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
    let mut worker = LocalInferenceWorker::initialize(&config.asr)?;
    eprintln!(
        "[boot] asr         loaded in {:>4} ms · {}",
        ms(t_load.elapsed()),
        config.asr.describe()
    );

    #[cfg(feature = "cleaner")]
    let cleaner = if !config.no_cleanup {
        let t = Instant::now();
        let c = TextCleanupEngine::initialize(&config.llm_model_path)?;
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
                Some(m) => m.body,            // wake word heard → its body
                None if armed => raw.clone(), // armed continuation → whole segment
                None => {
                    eprintln!("  ·    (ignored) {}", crate::daemon_log::text(&raw));
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
pub(super) fn listen_dictate(
    rt: &tokio::runtime::Runtime,
    refiner: &Refiner,
    body: &str,
    cleaner: Option<&TextCleanupEngine>,
) {
    let cleaned = match cleaner {
        Some(c) => rt
            .block_on(c.process_transcript(body))
            .unwrap_or_else(|_| body.to_string()),
        None => body.to_string(),
    };
    inject_refined(refiner, &cleaned);
}

#[cfg(not(feature = "cleaner"))]
pub(super) fn listen_dictate(refiner: &Refiner, body: &str) {
    inject_refined(refiner, body);
}

#[cfg(feature = "cleaner")]
pub(super) fn retry_last(
    rt: &tokio::runtime::Runtime,
    worker: &mut LocalInferenceWorker,
    cleaner: Option<&TextCleanupEngine>,
    refiner: &Refiner,
    audio: &[f32],
    retry_gen: u64,
) {
    if audio.is_empty() {
        eprintln!("  ↻    retry: nothing to retry yet");
        cues::play_error();
        return;
    }
    let transcript = match worker.transcribe_pcm(audio) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("  ↻    retry transcribe failed: {e:?}");
            cues::play_error();
            return;
        }
    };
    if transcript.trim().is_empty() {
        eprintln!("  ↻    retry: empty transcript");
        return;
    }
    ui_channel::publish_preview(retry_gen, &transcript);
    if !crate::cancel::CANCEL.allows_progress(retry_gen) { return; }
    let cleaned = match cleaner {
        Some(c) => {
            if !ui_channel::set_phase(retry_gen, UiState::Cleaning) { return; }
            rt.block_on(c.process_transcript_with_vocab(&transcript, &[]))
                .unwrap_or_else(|_| transcript.clone())
        },
        None => transcript,
    };
    // Same cancel gate + focus guard as a live utterance: claim the write
    // commit at the last moment, immediately before the first side effect.
    ui_channel::publish_preview(retry_gen, &cleaned);
    let authorize = || authorize_paste(retry_gen);
    inject_refined_authorized(refiner, &cleaned, &authorize);
}

/// Non-cleaner build: retry re-transcribes and injects the raw transcript
/// (no cleanup engine).
#[cfg(not(feature = "cleaner"))]
pub(super) fn retry_last(
    worker: &mut LocalInferenceWorker,
    refiner: &Refiner,
    audio: &[f32],
    retry_gen: u64,
) {
    if audio.is_empty() {
        eprintln!("  ↻    retry: nothing to retry yet");
        cues::play_error();
        return;
    }
    let transcript = match worker.transcribe_pcm(audio) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("  ↻    retry transcribe failed: {e:?}");
            cues::play_error();
            return;
        }
    };
    ui_channel::publish_preview(retry_gen, &transcript);
    let authorize = || authorize_paste(retry_gen);
    inject_refined_authorized(refiner, &transcript, &authorize);
}

/// Shared tail of `listen_dictate`: run the refiner's terminal classification
/// and act on it (inject text + trailing key, run a bare command, or discard).
pub(super) fn inject_refined(refiner: &Refiner, text: &str) {
    inject_refined_authorized(refiner, text, &|| true);
}

/// Cancellable form of [`inject_refined`]: `authorize` is polled once by the
/// injector immediately before the first write/clipboard side effect, so retry
/// obeys the same Escape + focus/no-blind-paste guard as a live utterance.
pub(super) fn inject_refined_authorized(
    refiner: &Refiner,
    text: &str,
    authorize: &dyn Fn() -> bool,
) {
    use crate::refiner::DictationOutcome;
    match refiner.refine(text).outcome() {
        DictationOutcome::Discard => eprintln!("  ⌫    scratch that · discarded"),
        DictationOutcome::Skip => eprintln!("  skip · empty after cleanup"),
        DictationOutcome::BareAction(action) => {
            if authorize() { execute_trailing_action(&action); }
            eprintln!("  ↵    bare command");
        }
        DictationOutcome::Inject { text, action } => {
            let target = FocusTarget::capture().ok();
            let res = match target {
                Some(t) => AccessibilityInjector::inject_with_target_cancellable(t, &text, authorize),
                None => AccessibilityInjector::inject_text_cancellable(&text, authorize),
            };
            match res {
                Ok(crate::injector::InjectOutcome::Injected) => {
                    eprintln!("  ✓    {}", crate::daemon_log::text(&text));
                    ui_channel::set_last_dictation(&text);
                    crate::history::record(&text);
                    if !matches!(action, TrailingAction::None) {
                        std::thread::sleep(Duration::from_millis(40));
                        execute_trailing_action(&action);
                    }
                }
                Ok(crate::injector::InjectOutcome::NoEditableField) => {
                    eprintln!(
                        "  ◌    no text field selected — text held (copy from menu)"
                    );
                    ui_channel::set_last_dictation(&text);
                }
                Ok(crate::injector::InjectOutcome::Cancelled) => {
                    // A cancel landed during retry: suppress the write. The
                    // retained audio is kept (not cleared) for another retry.
                    eprintln!("  ⌫    cancelled · discarded");
                    cues::play_cancel();
                }
                Ok(crate::injector::InjectOutcome::RefusedForeground) => {
                    // Fail-closed: the expected app was not frontmost at the
                    // global Cmd+V, so nothing was written. Hold it recoverable.
                    eprintln!("  ◌    target not frontmost at paste — text held (copy from menu)");
                    ui_channel::set_last_dictation(&text);
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
pub(super) fn execute_trailing_action(action: &TrailingAction) {
    use crate::clipboard_paste::{
        synthesize_escape, synthesize_return, synthesize_tab, synthesize_undo,
    };
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

