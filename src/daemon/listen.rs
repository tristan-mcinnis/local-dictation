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

    let wake_word = &config.wake_word;
    let mut worker = load_asr(&config)?;
    let pipeline = Pipeline::load(&config)?;
    warm_up(&mut worker, &pipeline);

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

            let generation = ui_channel::begin_utterance(UiState::Transcribing, false);
            let _utterance = ui_channel::UtteranceScope(generation);
            dictate_transcript(&pipeline, &body, generation);
        }

        // Keep memory bounded over a long listening session.
        if seg_stream.buf().len() > COMPACT_AFTER_SECS * SAMPLE_RATE as usize {
            seg_stream.compact();
        }
        std::thread::sleep(Duration::from_millis(120));
    }
}

/// Re-run the pipeline on the retained "last recording" audio (control-socket
/// "retry" / menu-bar item). From the transcript on, retry is the live path.
pub(super) fn retry_last(
    pipeline: &Pipeline,
    worker: &mut LocalInferenceWorker,
    audio: &[f32],
    retry_gen: u64,
) {
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
    dictate_transcript(pipeline, &transcript, retry_gen);
}

/// Clean, refine and deliver a transcript that did not come from a live
/// release: a retry of the retained audio, or a wake-word segment. The target
/// evidence is the field under the caret now, and from there the path is the
/// live one: the same cleanup policy (corrections, the short and clean
/// shortcut, the dropped-content guard, the terminal skip, tone and screen
/// vocabulary), the same refiner, the same cancel gate and write authorizer.
pub(super) fn dictate_transcript(pipeline: &Pipeline, transcript: &str, generation: u64) {
    ui_channel::publish_preview(generation, transcript);
    if !crate::cancel::CANCEL.allows_progress(generation) {
        return;
    }
    let app = crate::injector::frontmost_target_app();
    let focus = FocusTarget::capture().ok();
    let target_pid = app.as_ref().map(|a| a.pid);
    let target = CleanupTarget::of(app.as_ref(), focus.as_ref());
    let Ok(cleaned) = clean(transcript, pipeline, &target, generation) else {
        return;
    };
    ui_channel::publish_preview(generation, &cleaned.text);
    match pipeline.refiner.refine(&cleaned.text).outcome() {
        DictationOutcome::Discard => eprintln!("  ⌫    scratch that · discarded"),
        DictationOutcome::Skip => eprintln!("  skip · empty after cleanup"),
        DictationOutcome::BareAction(action) => {
            if authorize_paste(generation) {
                execute_trailing_action(&action);
            }
            eprintln!("  ↵    bare command");
        }
        DictationOutcome::Inject { text, action } => {
            if deliver(pipeline.result, generation, None, focus, target_pid, &text) != Delivery::Delivered {
                return;
            }
            eprintln!("  ✓    {}", crate::daemon_log::text(&text));
            ui_channel::set_last_dictation(&text);
            crate::history::record(&text);
            if !matches!(action, TrailingAction::None) {
                // Clipboard insertion returns after queuing Cmd+V. Keep
                // Enter/Tab behind its consume/restore window so a slow app
                // cannot submit the previous field text.
                crate::clipboard_paste::wait_for_pending_transaction();
                std::thread::sleep(Duration::from_millis(40));
                execute_trailing_action(&action);
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
    if matches!(action, TrailingAction::None | TrailingAction::Cancel) { return; }
    // Shared by ordinary dictation, retries, listen mode and bare commands.
    // No trailing key may overtake a pending clipboard consume/restore window.
    let result = crate::clipboard_paste::after_pending_transaction(|| match action {
        TrailingAction::None | TrailingAction::Cancel => Ok(()),
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
    });
    match (action, result) {
        (_, Ok(())) => eprintln!("  ↵    {action:?} sent"),
        (_, Err(e)) => eprintln!("  ✗    {action:?} failed: {e:?}"),
    }
}

