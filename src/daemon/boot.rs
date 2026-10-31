//! Boot: load and warm the models, open the always-on mic when pre-roll is
//! on, and log the resolved capture mode. Shared with listen mode.
use super::*;

/// The ASR model IS the app: a failure here is fatal to the worker, but it
/// leaves behind a named fault instead of an idle icon over a dead hotkey.
/// `run` catches the error and publishes EngineFailed.
pub(super) fn load_asr(config: &DaemonConfig) -> eyre::Result<LocalInferenceWorker> {
    let t_load = Instant::now();
    let asr = LocalInferenceWorker::initialize(&config.asr).map_err(|e| {
        crate::health::raise(
            if crate::model_fetch::ASR
                .present_at(&std::path::PathBuf::from(&config.asr.model_dir))
            {
                crate::health::Fault::EngineFailed
            } else {
                crate::health::Fault::ModelsMissing
            },
            format!("speech model: {e}"),
        );
        e
    })?;
    eprintln!(
        "[boot] asr         loaded in {:>4} ms · {}",
        ms(t_load.elapsed()),
        config.asr.describe()
    );
    Ok(asr)
}

impl Pipeline {
    /// The cleanup models and the refiner. The cleanup model is OPTIONAL
    /// (CP-01/CP-11): a moved, corrupt or template-less GGUF used to take the
    /// whole worker down, which left the menu bar alive and every key press
    /// going nowhere. Now it is a visible warning and dictation carries on with
    /// the raw transcript.
    pub(super) fn load(config: &DaemonConfig) -> eyre::Result<Self> {
        #[cfg(feature = "cleaner")]
        let cleaner = if !config.no_cleanup {
            let pretty = std::path::Path::new(&config.llm_model_path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(&config.llm_model_path);
            let t = Instant::now();
            match TextCleanupEngine::initialize_with_instruction_model(
                &config.llm_model_path,
                config.instruction_model_path.as_deref().map(std::path::Path::new),
            ) {
                Ok(c) => {
                    eprintln!(
                        "[boot] cleaner     loaded in {:>4} ms · {pretty}",
                        ms(t.elapsed())
                    );
                    crate::health::clear(crate::health::Fault::CleanupUnavailable);
                    if config.instruction_model_path.is_some() {
                        if let Some(problem) = c.instruction_problem() {
                            crate::health::raise(crate::health::Fault::CleanupUnavailable,
                                format!("instruction model unavailable: {problem}; plain dictation still works"));
                        }
                    }
                    Some(c)
                }
                Err(e) => {
                    eprintln!("[warn] cleaner     failed to load ({pretty}): {e}");
                    crate::health::raise(
                        crate::health::Fault::CleanupUnavailable,
                        format!("cleanup model: {e}"),
                    );
                    None
                }
            }
        } else {
            eprintln!("[boot] cleaner     disabled (--no-cleanup)");
            None
        };

        // Tokio current-thread runtime for the async cleaner path.
        #[cfg(feature = "cleaner")]
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;

        // Load personal corrections dictionary (no-op if no file) and wrap it
        // in the refiner that applies corrections + voice-command parsing in
        // the canonical order.
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

        // When an output-format preset (numbered/bullets/email) is active,
        // every dictation must go through the LLM to be reshaped, so the
        // short-utterance deterministic shortcut is disabled. Asked of the
        // engine, which validated the name against prompts.json: an unknown
        // or deleted preset name must not force every utterance through the
        // model with the default prompt.
        #[cfg(feature = "cleaner")]
        let format_active = cleaner.as_ref().is_some_and(|c| c.format_active());
        #[cfg(not(feature = "cleaner"))]
        let format_active = false;

        Ok(Self {
            #[cfg(feature = "cleaner")]
            cleaner,
            #[cfg(feature = "cleaner")]
            rt,
            refiner: Refiner::new(corrections),
            format_active,
            result: config.result,
        })
    }
}

/// The first call pays graph-optimization + CoreML compile / Metal shader
/// compile costs. Pay them now so the first real utterance is hot.
pub(super) fn warm_up(asr: &mut LocalInferenceWorker, text: &Pipeline) {
    let t_warm = Instant::now();
    let warm_silence = vec![0.0_f32; 16_000]; // 1 s of silence at 16 kHz
    let _ = asr.transcribe_pcm(&warm_silence);
    #[cfg(feature = "cleaner")]
    if let Some(ref c) = text.cleaner {
        let _ = text.rt.block_on(c.process_transcript("warmup")).ok();
    }
    eprintln!(
        "[boot] warm-up     done   in {:>4} ms",
        ms(t_warm.elapsed())
    );
}

impl WorkerState {
    pub(super) fn boot(config: DaemonConfig) -> eyre::Result<Self> {
        crate::history::initialize_recovery();
        cues::preload();
        let mut asr = load_asr(&config)?;
        let text = Pipeline::load(&config)?;
        warm_up(&mut asr, &text);
        // The engine is live. Anything still raised (a permission, a missing
        // cleanup model) stays raised and stays visible; only "Starting up…"
        // ends here.
        crate::health::clear(crate::health::Fault::Booting);

        let capture = config.capture;
        if capture.whisper_mode {
            eprintln!("[boot] whisper     mode ON (lower speech threshold)");
        }
        // Always-on capture (pre-roll). When the user enables it, the mic
        // stream is kept warm for the daemon's life and the last few hundred
        // ms before each key press are retained, so the first words are never
        // clipped and there's no stream-open latency on press. Created on the
        // worker thread because the cpal Stream is !Send.
        let always = if capture.preroll_ms > 0 {
            let preroll = crate::audio::preroll_samples(capture.preroll_ms);
            let (mut ao, cons) = crate::audio::AlwaysOnCapture::new(preroll, BUFFER_CAPACITY);
            match ao.start() {
                Ok(()) => {
                    eprintln!("[boot] always-on  mic warm · pre-roll {} ms", capture.preroll_ms);
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

        // One-line boot log of the resolved capture mode. Tail capture keeps
        // the mic open for a brief adaptive grace after release so the last
        // word isn't clipped (open-on-press only: always-on pre-roll already
        // keeps rolling past the release). The warm window keeps the input
        // stream open after a stop so a back-to-back dictation reuses it.
        if capture.preroll_ms > 0 {
            eprintln!("[boot] capture    mode: pre-roll {} ms (always-on mic)", capture.preroll_ms);
        } else if capture.warm_window_ms > 0 {
            eprintln!(
                "[boot] capture    mode: warm-window {} ms (reuse mic within window)",
                capture.warm_window_ms
            );
        } else {
            eprintln!("[boot] capture    mode: cold (press-to-open per utterance)");
        }
        if capture.tail_capture {
            eprintln!(
                "[boot] tail-capture on (release grace, up to {} ms)",
                capture.tail_capture_cap_ms
            );
        } else if capture.preroll_ms > 0 {
            eprintln!(
                "[boot] tail-capture skipped (always-on pre-roll keeps rolling past release)"
            );
        }
        // Streaming ASR and always-on pre-roll are mutually exclusive for now
        // (they drain the capture ring differently); pre-roll wins.
        if capture.streaming_asr {
            eprintln!("[boot] streaming   asr ENABLED (VAD-segmented transcribe during hold)");
        }
        if capture.max_recording_minutes > 0 {
            eprintln!("[boot] recording  limit {} min", capture.max_recording_minutes);
        }

        Ok(Self {
            config,
            asr,
            text,
            always,
            session_active: false,
            engine: None,
            ring: None,
            parked_until: None,
            recording: None,
            pending_target: None,
            stream: Streaming::default(),
            noise_floor: crate::audio::NoiseFloorEstimator::new(crate::audio::TAIL_FLOOR_RING_FRAMES),
            last_audio: Vec::new(),
            pending_saved_recoveries: std::collections::VecDeque::new(),
            restart_recording: None,
        })
    }
}
