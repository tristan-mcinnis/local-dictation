//! Worker thread: capture, streaming ASR, cleanup, transform, spoken commands, injection.
use super::*;

/// How long a failure pill stays up.
const FAILURE_PILL: Duration = Duration::from_millis(3500);

/// Say that the transcription failed, and where the words are. Before this the
/// only signal was a cue played while `audio_duck` still held the system
/// muted, so a failed dictation looked exactly like one that did nothing.
fn show_transcribe_failure(generation: u64) {
    ui_channel::show_transient_for(generation, UiState::Error, FAILURE_PILL);
}

#[cfg(feature = "cleaner")]
fn show_rewrite_failure(generation: u64) {
    ui_channel::show_transient_for(generation, UiState::RewriteFailed, FAILURE_PILL);
}

pub(super) fn worker_loop(
    rx: std::sync::mpsc::Receiver<DaemonEvent>,
    config: DaemonConfig,
) -> eyre::Result<()> {
    crate::history::initialize_recovery();
    cues::preload();
    let t_load = Instant::now();
    // The ASR model IS the app: a failure here is fatal to the worker, but it
    // now leaves behind a named fault instead of an idle icon over a dead
    // hotkey. `?` still returns — `run` catches it and publishes EngineFailed.
    let mut worker = LocalInferenceWorker::initialize(&config.asr).map_err(|e| {
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

    // The cleanup model is OPTIONAL (CP-01/CP-11). A moved, corrupt or
    // template-less GGUF used to take the whole worker down with `?`, which
    // left the menu bar alive and every key press going nowhere. Now it is a
    // visible warning and dictation carries on with the raw transcript.
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
    eprintln!(
        "[boot] warm-up     done   in {:>4} ms",
        ms(t_warm.elapsed())
    );
    // The engine is live. Anything still raised (a permission, a missing
    // cleanup model) stays raised and stays visible; only "Starting up…"
    // ends here.
    crate::health::clear(crate::health::Fault::Booting);

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
    // Whisper mode lowers the speech threshold so quiet dictation is captured.
    // Read once at boot (like every other setting); threaded into the VAD + the
    // tail-capture engage threshold below.
    let whisper_mode = crate::settings::Settings::load().resolve_whisper_mode();
    if whisper_mode {
        eprintln!("[boot] whisper     mode ON (lower speech threshold)");
    }
    let mut always: Option<(
        crate::audio::AlwaysOnCapture,
        crate::audio::HeapAudioConsumer,
    )> = if preroll_ms > 0 {
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
    // Generation of the in-flight utterance, for the out-of-band cancel gate
    // (`crate::cancel`). `u64::MAX` = no utterance in flight. Set whenever a
    // real recording begins; read by the release pipeline's cancel checkpoints.
    let mut cur_gen: u64 = u64::MAX;

    // Focus capture at key-press. The currently-focused UI element + caret
    // context are resolved on a background thread when recording starts, so the
    // AX wake / cross-process cost is paid while the user is speaking. At
    // release we join this receiver (usually instant) and reuse the target; a
    // failed or stale press-time capture re-resolves fresh. `None` means no
    // capture is in flight (between utterances / after a cancel).
    let mut pending_target: Option<std::sync::mpsc::Receiver<PressCapture>> = None;

    // ── Capture mode (release tail-capture + warm-window reuse) ──────────
    // Tail capture: on key release, keep the mic open for a brief adaptive grace
    // loop (draining new audio into the streaming path) so the last word of an
    // utterance isn't clipped. Open-on-press only — always-on pre-roll already
    // keeps rolling past the release, so the two don't compose.
    let tail_capture = crate::settings::Settings::load().resolve_tail_capture();
    let tail_capture_cap_ms = crate::settings::Settings::load().resolve_tail_capture_cap_ms();
    // Warm window: after a press-to-open recording stops, keep the input stream
    // open this long so a back-to-back dictation reuses it (skipping cpal's
    // ~120 ms open cost instead of paying it again). 0 = open fresh each time.
    let warm_window_ms = crate::settings::Settings::load().resolve_warm_window_ms();
    // True while a press-to-open engine is parked (idle but open) awaiting a
    // cheap reuse within the warm window. Distinct from `engine.is_some()`
    // (which also covers an actively-recording engine) so the event loop can
    // run the warm-window expiry tick and the re-press can resume cleanly.
    let mut warm_parked = false;
    // When the parked stream expires (mic released). None while recording.
    let mut warm_deadline: Option<Instant> = None;

    // One-line boot log of the resolved capture mode.
    if preroll_ms > 0 {
        eprintln!("[boot] capture    mode: pre-roll {preroll_ms} ms (always-on mic)");
    } else if warm_window_ms > 0 {
        eprintln!(
            "[boot] capture    mode: warm-window {warm_window_ms} ms (reuse mic within window)"
        );
    } else {
        eprintln!("[boot] capture    mode: cold (press-to-open per utterance)");
    }
    if tail_capture {
        eprintln!(
            "[boot] tail-capture on (release grace, up to {tail_capture_cap_ms} ms)"
        );
    } else if preroll_ms > 0 {
        eprintln!(
            "[boot] tail-capture skipped (always-on pre-roll keeps rolling past release)"
        );
    }

    // Incremental streaming ASR (default ON): transcribe completed sentences at
    // VAD pauses *during* the hold for a live preview, then reassemble the cached
    // segments at release. No per-segment cleanup — whole-utterance cleanup still
    // runs at release (accuracy). Streaming ASR and always-on pre-roll are
    // mutually exclusive for now (they drain the capture ring differently);
    // pre-roll wins and streaming falls back to the proven whole-buffer path.
    let streaming_asr = crate::settings::Settings::load().resolve_streaming_asr();
    if streaming_asr {
        eprintln!("[boot] streaming   asr ENABLED (VAD-segmented transcribe during hold)");
    }
    // When an output-format preset (numbered/bullets/email/…) is active, every
    // dictation must go through the LLM to be reshaped — so the short-utterance
    // deterministic shortcut is disabled. Default cleanup has no such need.
    #[cfg(feature = "cleaner")]
    // Asked of the engine, which validated the name against prompts.json: an
    // unknown or deleted preset name must not force every utterance through
    // the model with the default prompt.
    let format_active = cleaner.as_ref().is_some_and(|c| c.format_active());
    // Per-utterance streaming accumulators (only used when `streaming_asr`). These
    // are the whole incremental-ASR state, independent of the cleaner feature:
    // transcribe_pcm + SegmentStream + corrections need no LLM.
    let mut stream: Option<crate::vad::SegmentStream> = None;
    // Cached corrected text of each completed segment (VAD-pause-bounded).
    let mut stream_segs: Vec<String> = Vec::new();
    // Keep exact ASR words separately; personal corrections must not rewrite
    // the recoverable Raw layer.
    let mut stream_raw_segs: Vec<String> = Vec::new();
    // Backup of every drained sample, for the whole-buffer fallback on a poisoned
    // stream / transform / empty result — never lose audio.
    let mut stream_audio: Vec<f32> = Vec::new();
    // True once any segment transcribe errored: stop segmenting and transcribe the
    // full `stream_audio` buffer as one batch at release (today's behaviour).
    let mut stream_poisoned = false;
    // Incremental room-noise-floor estimate, fed per-frame RMS during capture
    // (streaming) so the tail-capture engage check never re-sorts the whole
    // utterance on the release→inject critical path.
    let mut noise_floor =
        crate::audio::NoiseFloorEstimator::new(crate::audio::TAIL_FLOOR_RING_FRAMES);
    // The raw PCM of the last transcribed utterance, kept so "retry" can re-run
    // the pipeline on the same audio. Empty until the first dictation.
    let mut last_audio: Vec<f32> = Vec::new();
    let mut pending_saved_recoveries = std::collections::VecDeque::<i64>::new();
    // Recording limit (minutes, 0 = none) and whether the in-flight stop came
    // from it rather than from a key.
    let max_recording_minutes = crate::settings::Settings::load().resolve_max_recording_minutes();
    if max_recording_minutes > 0 {
        eprintln!("[boot] recording  limit {max_recording_minutes} min");
    }
    let mut limit_hit = false;
    // Retry a failed retained-stream resume through the ordinary cold-open
    // path, preserving queued stop/cancel events for this same press.
    let mut restart_recording: Option<bool> = None;

    'evloop: loop {
        // While recording, wake periodically to checkpoint captured PCM and
        // (when enabled) transcribe sentences closed by a VAD pause.
        // recv_timeout keeps everything on this one thread, which already owns the
        // ASR worker — no shared state.
        let event = if let Some(transform) = restart_recording.take() {
            DaemonEvent::StartRecording { transform }
        } else if !session_active && (engine.is_none() || warm_parked)
            && !pending_saved_recoveries.is_empty() {
            DaemonEvent::Recover { id: pending_saved_recoveries.pop_front().unwrap() }
        } else {
            // Ticks checkpoint every capture mode, including non-streaming and
            // pre-roll. A warm-parked engine only checks its expiry; its idle
            // microphone audio never reaches the archive.
            if engine.is_some() || session_active {
                match rx.recv_timeout(std::time::Duration::from_millis(120)) {
                    Ok(ev) => ev,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                        if !warm_parked
                            && crate::cancel::CANCEL.active()
                            && t_press.is_some_and(|t| recording_limit_reached(t.elapsed(), max_recording_minutes)) =>
                    {
                        // A lost key-up (Secure Input, a disabled tap) or a
                        // forgotten hands-free session: stop here instead of
                        // recording without end. The text is kept and copied,
                        // never pasted, because nobody may be at the keyboard.
                        eprintln!(
                            "[limit] recording reached {max_recording_minutes} min · stopping (text kept, not pasted)"
                        );
                        limit_hit = true;
                        PTT_RESYNC.store(true, Ordering::SeqCst);
                        DaemonEvent::StopRecording
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if warm_parked {
                            // Warm-window expiry: close the parked mic once idle
                            // past the window, so we don't hold the input device
                            // (and its orange indicator) forever between dictations.
                            if let Some(deadline) = warm_deadline {
                                if Instant::now() >= deadline {
                                    drop(engine.take()); // releases the mic
                                    consumer.take();
                                    is_recording.take();
                                    warm_parked = false;
                                    warm_deadline = None;
                                    eprintln!("[audio] warm-window expired · mic closed");
                                }
                            }
                        } else if session_active {
                            if let Some((_, cons)) = always.as_mut() {
                                checkpoint_capture(cons, &mut stream_audio, cur_gen);
                            }
                        } else if streaming_asr {
                            if let (Some(st), Some(c)) = (stream.as_mut(), consumer.as_mut()) {
                                stream_process_asr(
                                    &mut worker,
                                    &refiner,
                                    st,
                                    c,
                                    &mut stream_segs,
                                    &mut stream_raw_segs,
                                    &mut stream_audio,
                                    &mut stream_poisoned,
                                    &mut noise_floor,
                                    false,
                                    whisper_mode,
                                    cur_gen,
                                );
                            }
                        } else if let Some(c) = consumer.as_mut() {
                            checkpoint_capture(c, &mut stream_audio, cur_gen);
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
        };
        match event {
            DaemonEvent::LatchArmed => {
                // Hands-free engaged mid-hold: the mic keeps running once the
                // keys are released. Just give audible feedback + a log line.
                cues::play_latch();
                eprintln!("⤓ hands-free · release keys, keep talking · tap PTT to stop");
            }
            DaemonEvent::Shutdown => {
                // Quit: return so every local here — the cleanup engine with
                // its LlamaContext, the ASR worker — is dropped while Metal is
                // still alive. The spawning thread then calls
                // `shutdown::mark_released`, which unblocks the delegate that
                // is holding `exit()` open.
                if cur_gen != u64::MAX && crate::cancel::CANCEL.active() {
                    if let Some(e) = engine.as_mut() { e.stop_capture(); }
                    if session_active {
                        if let Some((ao, cons)) = always.as_mut() {
                            ao.end_session();
                            let flag = ao.recording_flag();
                            retain_in_flight_audio(&mut last_audio, &stream_audio, Some(cons), Some(&flag), Some(cur_gen));
                        }
                    } else if let (Some(c), Some(r)) = (consumer.as_mut(), is_recording.as_ref()) {
                        retain_in_flight_audio(&mut last_audio, &stream_audio, Some(c), Some(r), Some(cur_gen));
                    }
                    crate::history::finish_recovery(cur_gen, crate::history::RecoveryStatus::Interrupted);
                    ui_channel::finish_utterance(cur_gen);
                }
                crate::history::flush_recovery(Duration::from_secs(2));
                eprintln!("[daemon] shutdown · releasing models before exit");
                return Ok(());
            }
            DaemonEvent::Recover { id } => {
                // Recovery is explicitly copy-only and never interrupts capture.
                if session_active || (engine.is_some() && !warm_parked) {
                    if !pending_saved_recoveries.contains(&id) { pending_saved_recoveries.push_back(id); }
                    eprintln!("[recovery] queued until current recording settles");
                    continue;
                }
                let generation = ui_channel::begin_utterance(UiState::Transcribing, streaming_asr);
                let _utterance = ui_channel::UtteranceScope(generation);
                crate::history::resume_recovery(generation, id);
                let mut recovery = RecoveryFinish::new(generation);
                let (audio, rate) = match crate::history::read_recovery_audio(id) {
                    Ok(Some(audio)) => audio,
                    Ok(None) => {
                        recovery.error = Some("Saved audio is no longer available".into());
                        eprintln!("[recovery] saved audio is no longer available for record {id}");
                        ui_channel::show_transient_for(generation, UiState::Error, Duration::from_millis(900));
                        continue;
                    }
                    Err(error) => {
                        recovery.error = Some(format!("Could not read saved audio: {error}"));
                        eprintln!("[recovery] could not read record {id}: {error}");
                        ui_channel::show_transient_for(generation, UiState::Error, Duration::from_millis(900));
                        continue;
                    }
                };
                if rate != crate::audio::SAMPLE_RATE {
                    recovery.error = Some(format!("Unsupported saved sample rate {rate}"));
                    eprintln!("[recovery] unsupported saved sample rate {rate}");
                    continue;
                }
                #[cfg(feature = "cleaner")]
                let mut cleanup = |text: &str| {
                    rt.block_on(cleaner.as_ref().unwrap().process_transcript_with_vocab(text, &[]))
                };
                #[cfg(feature = "cleaner")]
                let cleanup_fn = if cleaner.is_some() {
                    Some(&mut cleanup as &mut dyn FnMut(&str) -> eyre::Result<String>)
                } else { None };
                #[cfg(not(feature = "cleaner"))]
                let cleanup_fn = None;
                let outcome = crate::reprocess::to_clipboard(
                    &crate::cancel::CANCEL, generation, &audio,
                    |audio| worker.transcribe_pcm(audio), cleanup_fn,
                    |phase| {
                        ui_channel::set_phase(generation, match phase {
                            crate::reprocess::Phase::Transcribing => UiState::Transcribing,
                            crate::reprocess::Phase::Cleaning => UiState::Cleaning,
                        });
                    },
                    |kind, text| {
                        let kind = match kind {
                            crate::reprocess::Version::Raw => crate::history::StageKind::Raw,
                            crate::reprocess::Version::Cleaned => crate::history::StageKind::Cleaned,
                        };
                        crate::history::append_recovery_stage(generation, kind, text, "Saved-audio recovery");
                        ui_channel::publish_preview(generation, text);
                    },
                    |text| {
                        // The copy claim already won; no cancel affordance while
                        // clipboard delivery completes, and no Pasting fiction.
                        ui_channel::set_state(UiState::Idle);
                        crate::clipboard_paste::copy_only(text)
                    },
                );
                match outcome {
                    crate::reprocess::Outcome::Copied(text) => {
                        recovery.status = crate::history::RecoveryStatus::Copied;
                        crate::history::append_recovery_stage(generation, crate::history::StageKind::Output, &text, "Copied only; not pasted");
                        ui_channel::set_last_dictation(&text);
                        ui_channel::show_transient(UiState::Copied, Duration::from_millis(900));
                    }
                    crate::reprocess::Outcome::Cancelled => {
                        recovery.status = crate::history::RecoveryStatus::Cancelled;
                        cues::play_cancel();
                    }
                    crate::reprocess::Outcome::Empty => {
                        recovery.status = crate::history::RecoveryStatus::Held;
                        eprintln!("[recovery] no text recovered; original recording kept");
                        ui_channel::show_transient_for(generation, UiState::Error, Duration::from_millis(900));
                    }
                    crate::reprocess::Outcome::Failed(error) => {
                        eprintln!("[recovery] processing failed: {error}");
                        recovery.error = Some(error);
                        ui_channel::show_transient_for(generation, UiState::Error, Duration::from_millis(900));
                    }
                }
            }
            DaemonEvent::Retry => {
                // Re-run the pipeline on the retained "last recording" audio
                // (control-socket "retry" / menu-bar item). Retry obeys the
                // SAME cancel gate and focus/no-blind-paste guard as a live
                // utterance: it begins a fresh generation, marks Processing (so
                // Escape is recognised), and claims the write commit at the last
                // moment via the injector's write authorizer. The retained audio
                // is NOT cleared here — a cancelled retry keeps it until the next
                // recording start or app quit.
                if last_audio.is_empty() {
                    eprintln!("  ↻    retry: nothing to retry yet");
                    cues::play_error();
                    continue;
                }
                let retry_gen = ui_channel::begin_utterance(UiState::Transcribing, streaming_asr);
                let _utterance = ui_channel::UtteranceScope(retry_gen);
                #[cfg(feature = "cleaner")]
                retry_last(&rt, &mut worker, cleaner.as_ref(), &refiner, &last_audio, retry_gen);
                #[cfg(not(feature = "cleaner"))]
                retry_last(&mut worker, &refiner, &last_audio, retry_gen);
                ui_channel::finish_utterance(retry_gen);
            }
            DaemonEvent::CancelRecording => {
                // External "cancel" (control socket, or the Escape key): stop
                // capture and throw the utterance away — no transcribe, no
                // inject. Mirrors the StopRecording teardown minus the pipeline.
                //
                // Stale-cancel guard: a `CancelRecording` queued from a PREVIOUS
                // utterance (whose gate was already ended when it settled) must
                // NOT discard a recording that started since. Only honour the
                // cancel when the gate still has it pending for the CURRENT
                // utterance — a fresh `begin()` clears the target, so an idle /
                // stale event is a no-op and leaves the live recording alone.
                let genuine = cur_gen != u64::MAX
                    && crate::cancel::CANCEL.requested(cur_gen);
                if !genuine {
                    continue;
                }
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
                pending_target = None; // discard the orphaned press-time capture
                if let Some(e) = on_demand_engine.as_mut() {
                    if warm_window_ms > 0 && preroll_ms == 0 {
                        // Warm-window cancel: park the mic for reuse, but first
                        // drain the cancelled audio and RETAIN it for retry (the
                        // next recording clears this buffer on start).
                        if let Err(error) = e.park() {
                            eprintln!("[audio] cancelled capture could not be retained: {error}");
                            e.stop_capture();
                        }
                        if let (Some(c), Some(r)) = (consumer.as_mut(), is_recording.as_ref()) {
                            retain_in_flight_audio(&mut last_audio, &stream_audio, Some(c), Some(r.as_ref()), Some(cur_gen));
                        }
                        engine = on_demand_engine.take();
                        warm_parked = true;
                        warm_deadline = Some(Instant::now()
                            + Duration::from_millis(warm_window_ms as u64));
                    } else {
                        e.stop_capture();
                        // Cold cancel: drain + retain the partial recording before
                        // dropping the consumer below.
                        if let (Some(c), Some(r)) = (consumer.as_mut(), is_recording.as_ref()) {
                            retain_in_flight_audio(&mut last_audio, &stream_audio, Some(c), Some(r.as_ref()), Some(cur_gen));
                        }
                    }
                }
                if always_on_session {
                    // End the session, drain the buffered audio, and RETAIN it for
                    // retry (the next recording clears it on start).
                    if let Some((ao, cons)) = always.as_mut() {
                        ao.end_session();
                        let recflag = ao.recording_flag();
                        retain_in_flight_audio(&mut last_audio, &stream_audio, Some(cons), Some(recflag.as_ref()), Some(cur_gen));
                    }
                } else if !warm_parked {
                    // Drop the per-utterance consumer + recording flag unread (the
                    // warm-park path keeps them for the next reuse).
                    consumer.take();
                    is_recording.take();
                }
                stream = None;
                stream_segs.clear();
                stream_raw_segs.clear();
                stream_audio.clear();
                stream_poisoned = false;
                cues::play_cancel();
                crate::history::finish_recovery(cur_gen, crate::history::RecoveryStatus::Cancelled);
                ui_channel::finish_utterance(cur_gen);
                cur_gen = u64::MAX;
                eprintln!("⌫ cancelled · retained for recovery");
                eprintln!();
            }
            DaemonEvent::StartRecording { transform } => {
                limit_hit = false;
                if session_active {
                    continue;
                }
                // A new recording attempt invalidates the previously retained
                // "last recording" buffer, even if this attempt fails to open
                // the mic — the stale audio must never be replayable.
                last_audio.clear();
                // Warm-window reuse: a retained engine with idle IO paused is
                // resumed in place, skipping the stream rebuild cost for
                // a back-to-back dictation. Rebuild instead if the default input
                // changed since the stream was built.
                if engine.is_some() {
                    if warm_parked {
                        // Not only a device change: a stream that cpal has
                        // reported an error on is dead while the object is
                        // alive, and `Instant` does not advance while the Mac
                        // sleeps, so a lid closed overnight can leave a
                        // "fresh" parked stream bound to a device that is gone.
                        let changed = engine.as_mut().and_then(|e| e.should_rebuild());
                        if let Some(why) = changed {
                            eprintln!("[audio] {why} → rebuilt warm stream");
                            drop(engine.take());
                            consumer.take();
                            is_recording.take();
                            warm_parked = false;
                            warm_deadline = None;
                            // fall through to a fresh open below
                        } else {
                            let resumed = engine.as_mut().unwrap().resume();
                            let r = match resumed {
                                Ok(flag) => flag,
                                Err(error) => {
                                    eprintln!("[audio] retained capture failed to resume: {error}; opening fresh");
                                    drop(engine.take());
                                    consumer.take();
                                    is_recording.take();
                                    warm_parked = false;
                                    warm_deadline = None;
                                    restart_recording = Some(transform);
                                    continue;
                                }
                            };
                            is_recording = Some(r);
                            warm_parked = false;
                            warm_deadline = None;
                            transform_mode = transform;
                            t_press = Some(Instant::now());
                            cues::play_start();
                            crate::audio_duck::mute();
                            eprintln!("▶ recording (warm reuse)");
                            crate::ipc::DAEMON_RECORDING.store(true, Ordering::SeqCst);
                            last_audio.clear(); // new recording invalidates the previous buffer
                            cur_gen = begin_recording_feedback(streaming_asr, is_recording.as_ref().unwrap().clone());
                            stream_segs.clear();
                            stream_raw_segs.clear();
                            stream_audio.clear();
                            stream_poisoned = false;
                            if streaming_asr {
                                stream = Some(crate::vad::SegmentStream::new(
                                    crate::audio::SAMPLE_RATE,
                                    vad_config(whisper_mode),
                                ));
                                // Reset the per-utterance streaming accumulators so
                                // a warm-window reuse never carries stale segments
                                // or a stale poison flag into the new utterance.
                                stream_segs.clear();
                                stream_raw_segs.clear();
                                stream_audio.clear();
                                stream_poisoned = false;
                            }
                            noise_floor.clear();
                            pending_target = Some(spawn_focus_capture());
                            continue;
                        }
                    } else {
                        // An engine is already actively recording — swallow the
                        // spurious re-press (same guard as before this change).
                        continue;
                    }
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
                    // Same three tests as the warm stream, through the same
                    // function: the always-on stream is open for hours, so it
                    // is the one most likely to have been slept through, and
                    // an inline copy of two of the three checks left it out.
                    let always_stale = always.as_ref().and_then(|(ao, _)| ao.should_rebuild());
                    if let Some(why) = always_stale {
                        let preroll = crate::audio::preroll_samples(preroll_ms);
                        let (mut ao2, cons2) =
                            crate::audio::AlwaysOnCapture::new(preroll, BUFFER_CAPACITY);
                        match ao2.start() {
                            Ok(()) => {
                                eprintln!(
                                    "[audio] {why} → rebuilt always-on stream on `{}`",
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
                        noise_floor.clear();
                        ao.begin_session();
                        cues::play_start();
                        crate::audio_duck::mute();
                        eprintln!("▶ recording · pre-roll");
                        session_active = true;
                        crate::ipc::DAEMON_RECORDING.store(true, Ordering::SeqCst);
                        last_audio.clear(); // new recording invalidates the previous buffer
                        cur_gen = begin_recording_feedback(streaming_asr, ao.recording_flag());
                        stream_audio.clear();
                        pending_target = Some(spawn_focus_capture());
                    }
                    continue;
                }
                // Fresh cold open (no parked engine to reuse).
                if !begin_open_capture(
                    &mut engine,
                    &mut consumer,
                    &mut is_recording,
                    &mut stream,
                    &mut stream_segs,
                    &mut stream_raw_segs,
                    &mut stream_audio,
                    &mut stream_poisoned,
                    &mut transform_mode,
                    &mut t_press,
                    streaming_asr,
                    transform,
                    whisper_mode,
                ) {
                    cues::play_error();
                    // The cue alone is not enough: it plays while the app's own
                    // system mute is still held, and it says nothing.
                    ui_channel::show_transient_if_unowned(
                        UiState::Unavailable,
                        FAILURE_PILL,
                    );
                } else {
                    noise_floor.clear();
                    last_audio.clear(); // new recording invalidates the previous buffer
                    cur_gen = begin_recording_feedback(streaming_asr, is_recording.as_ref().unwrap().clone());
                    pending_target = Some(spawn_focus_capture());
                }
            }
            DaemonEvent::StopRecording => {
                let t_release = Instant::now();
                // A parked warm microphone is not a recording. An idle/duplicate
                // Stop must not create an empty recovery row or reuse an ended generation.
                if warm_parked || !crate::cancel::CANCEL.active() { continue; }
                // Either capture path may be live: open-on-press (`engine`) or
                // always-on pre-roll (`session_active`). Nothing live ⇒ ignore.
                crate::ipc::DAEMON_RECORDING.store(false, Ordering::SeqCst);
                let mut on_demand_engine = engine.take();
                let always_on_session = session_active;
                session_active = false;
                if on_demand_engine.is_none() && !always_on_session {
                    continue;
                }
                let _utterance = ui_channel::UtteranceScope(cur_gen);
                let mut recovery = RecoveryFinish::new(cur_gen);
                // Capture (and reset) whether this utterance is a transform. Done
                // up-front so the tail-capture re-press restart can re-set it with
                // the new gesture.
                // A limit stop is never a transform or a spoken command: both
                // write into the front app, and nobody asked for that write.
                let limit_stopped = std::mem::take(&mut limit_hit);
                #[allow(unused_variables)]
                let transform = transform_mode && !limit_stopped;
                transform_mode = false;

                // ── Tail-capture grace (open-on-press only) ──────────────
                // On release the mic used to be dropped immediately, which clipped
                // a word still being spoken. Keep the engine open for a brief
                // adaptive grace: poll the live level, and stop once it stays below
                // a room-adaptive threshold for a short quiet run (or the cap). The
                // grace audio accumulates in the ring and is drained by the release
                // pipeline next. Only engages when release plausibly happened
                // mid-speech; a release in a pause costs nothing. Skipped entirely
                // under always-on pre-roll, which already keeps rolling past release.
                let t_tail_grace = Instant::now();
                let mut grace_outcome: Option<TailGraceOutcome> = None;
                let tail_cap = Duration::from_millis(tail_capture_cap_ms as u64);
                if tail_capture && on_demand_engine.is_some() {
                    let (threshold, engage) = {
                        let frame_len = (crate::audio::DEFAULT_TAIL_FRAME_MS as f64 / 1000.0
                            * crate::audio::SAMPLE_RATE as f64)
                            as usize;
                        // Bounded trailing window — the engage decision never scans
                        // or copies the whole utterance on this hot path.
                        let tail = crate::audio::recording_tail(
                            consumer.as_ref().unwrap(),
                            &stream_audio,
                            crate::audio::DEFAULT_TAIL_ENGAGE_MS,
                        );
                        // The floor comes from the estimator maintained during
                        // capture (streaming) or, in the whole-buffer path, from a
                        // bounded pass over the same trailing window — never a full
                        // sort of the utterance.
                        let floor = if noise_floor.is_empty() {
                            crate::audio::noise_floor_of_audio(
                                &tail,
                                frame_len,
                                crate::audio::TAIL_NOISE_PERCENTILE,
                            )
                        } else {
                            noise_floor.floor(crate::audio::TAIL_NOISE_PERCENTILE)
                        };
                        crate::audio::tail_capture_engage_with_mode(
                            &tail,
                            frame_len,
                            floor,
                            crate::audio::DEFAULT_TAIL_ENGAGE_MS,
                            crate::audio::DEFAULT_TAIL_FRAME_MS,
                            whisper_mode,
                        )
                    };
                    if engage {
                        let grace_start = Instant::now();
                        let poll = Duration::from_millis(crate::audio::DEFAULT_TAIL_POLL_MS);
                        let quiet_ms = crate::audio::DEFAULT_TAIL_QUIET_MS;
                        let mut series: Vec<f32> = Vec::new();
                        let mut recovered_ms: u128 = 0;
                        loop {
                            if let Some(level) = ui_channel::recent_levels().last().copied() {
                                series.push(level);
                                if series.len() > 16 {
                                    series.remove(0);
                                }
                            }
                            // A re-press or cancel during the grace tail must not be
                            // lost. Consuming it here abandons the grace so we can
                            // restart/cancel cleanly instead of waiting out the tail.
                            match rx.try_recv() {
                                Ok(DaemonEvent::StartRecording { transform }) => {
                                    grace_outcome = Some(TailGraceOutcome::RePress { transform });
                                    break;
                                }
                                Ok(DaemonEvent::CancelRecording) => {
                                    // A delayed cancel belongs only to the generation
                                    // that accepted it, never a fresh tail-capture.
                                    if crate::cancel::CANCEL.requested(cur_gen) {
                                        grace_outcome = Some(TailGraceOutcome::Cancel);
                                        break;
                                    }
                                }
                                // A duplicate release or a stray latch cue during the
                                // grace tail is harmless: keep waiting for quiet.
                                Ok(DaemonEvent::StopRecording)
                                | Ok(DaemonEvent::LatchArmed)
                                | Ok(DaemonEvent::Retry) => {}
                                Ok(DaemonEvent::Recover { id }) => {
                                    if !pending_saved_recoveries.contains(&id) { pending_saved_recoveries.push_back(id); }
                                }
                                // Quit during the tail: abandon this utterance
                                // and release the models (see `crate::shutdown`).
                                Ok(DaemonEvent::Shutdown) => {
                                    recovery.status = crate::history::RecoveryStatus::Interrupted;
                                    if let Some(e) = on_demand_engine.as_mut() { e.stop_capture(); }
                                    if let (Some(c), Some(r)) = (consumer.as_mut(), is_recording.as_ref()) {
                                        retain_in_flight_audio(&mut last_audio, &stream_audio, Some(c), Some(r), Some(cur_gen));
                                    }
                                    recovery.finish_now();
                                    crate::history::flush_recovery(Duration::from_secs(2));
                                    eprintln!("[daemon] shutdown · releasing models before exit");
                                    return Ok(());
                                }
                                Err(_) => {}
                            }
                            recovered_ms = grace_start.elapsed().as_millis();
                            if recovered_ms >= tail_cap.as_millis() {
                                break;
                            }
                            if crate::audio::quiet_run(
                                &series,
                                crate::audio::DEFAULT_TAIL_POLL_MS,
                                threshold,
                                quiet_ms,
                            ) {
                                break;
                            }
                            std::thread::sleep(poll);
                        }
                        if recovered_ms > 0 && grace_outcome.is_none() {
                            eprintln!("  tail +{recovered_ms} ms recovered");
                        }
                    }
                }
                // React to a grace interrupt: the user re-pressed (restart a fresh
                // recording, abandoning the previous utterance) or cancelled (drop
                // everything). Either way we skip the release pipeline below.
                match grace_outcome {
                    Some(TailGraceOutcome::RePress { transform }) => {
                        recovery.status = crate::history::RecoveryStatus::Interrupted;
                        // The engine is still open and capturing during the grace
                        // tail. Reuse it instead of dropping it for a ~120 ms cold
                        // open: park (stop production), drain+discard the abandoned
                        // utterance's audio, then resume in place on the same stream.
                        let mut reuse_ok = true;
                        if let Some(e) = on_demand_engine.as_mut() {
                            if let Err(error) = e.park() {
                                eprintln!("[audio] tail re-press pause failed: {error}; opening fresh");
                                e.stop_capture();
                                reuse_ok = false;
                            }
                            if let (Some(c), Some(r)) = (consumer.as_mut(), is_recording.as_ref()) {
                                let tail = crate::audio::drain_session(c, r);
                                crate::history::append_recovery_audio(cur_gen, tail, crate::audio::SAMPLE_RATE);

                            }
                            if reuse_ok {
                                match e.resume() {
                                    Ok(flag) => is_recording = Some(flag),
                                    Err(error) => {
                                        eprintln!("[audio] tail re-press resume failed: {error}; opening fresh");
                                        reuse_ok = false;
                                    }
                                }
                            }
                        }
                        if !reuse_ok {
                            drop(on_demand_engine.take());
                            consumer.take();
                            is_recording.take();
                            warm_parked = false;
                            warm_deadline = None;
                            stream = None;
                            stream_segs.clear(); stream_raw_segs.clear(); stream_audio.clear();
                            pending_target = None;
                            crate::ipc::DAEMON_RECORDING.store(false, Ordering::SeqCst);
                            ui_channel::finish_utterance(cur_gen);
                            cur_gen = u64::MAX;
                            restart_recording = Some(transform);
                            continue 'evloop;
                        }
                        engine = on_demand_engine;
                        transform_mode = transform;
                        t_press = Some(Instant::now());
                        stream_segs.clear();
                        stream_raw_segs.clear();
                        stream_audio.clear();
                        stream_poisoned = false;
                        noise_floor.clear();
                        if streaming_asr {
                            stream = Some(crate::vad::SegmentStream::new(
                                crate::audio::SAMPLE_RATE,
                                vad_config(whisper_mode),
                            ));
                        }
                        cues::play_start();
                        crate::audio_duck::mute();
                        crate::ipc::DAEMON_RECORDING.store(true, Ordering::SeqCst);
                        last_audio.clear(); // re-press abandons the old utterance
                        cur_gen = begin_recording_feedback(streaming_asr, is_recording.as_ref().unwrap().clone());
                        eprintln!("  ↻    re-press during tail-capture · reused warm stream");
                        pending_target = Some(spawn_focus_capture());
                        continue 'evloop;
                    }
                    Some(TailGraceOutcome::Cancel) => {
                        recovery.status = crate::history::RecoveryStatus::Cancelled;
                        on_demand_engine.as_mut().unwrap().stop_capture();
                        // Retain the partial recording (held streaming backup +
                        // the grace-tail samples still in the ring) for retry.
                        if let (Some(c), Some(r)) = (consumer.as_mut(), is_recording.as_ref()) {
                            retain_in_flight_audio(&mut last_audio, &stream_audio, Some(c), Some(r.as_ref()), Some(cur_gen));
                        }
                        consumer.take();
                        is_recording.take();
                        pending_target = None; // discard the orphaned press-time capture
                        stream = None;
                        stream_segs.clear();
                        stream_raw_segs.clear();
                        stream_audio.clear();
                        stream_poisoned = false;
                        cues::play_cancel();
                        ui_channel::set_state(UiState::Idle);
                        let _unmute = crate::audio_duck::RestoreOnDrop;
                        crate::cancel::CANCEL.end();
                        cur_gen = u64::MAX;
                        eprintln!("⌫ cancelled · discarded");
                        eprintln!();
                        continue 'evloop;
                    }
                    None => {}
                }

                let t_tail_ms = ms(t_tail_grace.elapsed());
                // Un-mute other audio when this arm exits — by any path
                // (transcribe error, empty/discarded utterance, transform,
                // or successful inject). The guard's Drop runs on every
                // `continue` and the normal fall-through, so the restore can't
                // be forgotten on a new code path.
                let _unmute = crate::audio_duck::RestoreOnDrop;
                let t_capture_stop = Instant::now();
                // End capture for whichever path is live. Open-on-press either parks
                // the stream (warm-window reuse) or drops it (releasing the mic);
                // always-on just ends the session — the warm stream keeps running.
                if let Some(e) = on_demand_engine.as_mut() {
                    if warm_window_ms > 0 && preroll_ms == 0 {
                        if let Err(error) = e.park() {
                            eprintln!("[audio] capture pause failed: {error}; resource will be rebuilt");
                            e.stop_capture();
                        }
                        engine = on_demand_engine.take();
                        warm_parked = true;
                        warm_deadline =
                            Some(Instant::now() + Duration::from_millis(warm_window_ms as u64));
                    } else {
                        e.stop_capture();
                    }
                }
                if always_on_session {
                    if let Some((ao, _)) = always.as_ref() {
                        ao.end_session();
                    }
                }
                let t_capture_stop_ms = ms(t_capture_stop.elapsed());
                // Out-of-band cancel gate: an Escape / IPC cancel may have
                // arrived during capture or the tail grace. Discard now — no
                // transcribe, no inject — but RETAIN the recording's audio for
                // retry (see `crate::cancel`). The audio has not been drained
                // into `last_audio` yet at this checkpoint, so drain it from the
                // active capture source first.
                if cur_gen != u64::MAX && crate::cancel::CANCEL.consume_cancel(cur_gen) {
                    recovery.status = crate::history::RecoveryStatus::Cancelled;
                    if always_on_session {
                        if let Some((ao, cons)) = always.as_mut() {
                            let recflag = ao.recording_flag();
                            retain_in_flight_audio(&mut last_audio, &stream_audio, Some(cons), Some(recflag.as_ref()), Some(cur_gen));
                        }
                    } else if let (Some(c), Some(r)) = (consumer.as_mut(), is_recording.as_ref()) {
                        retain_in_flight_audio(&mut last_audio, &stream_audio, Some(c), Some(r.as_ref()), Some(cur_gen));
                    }
                    cues::play_cancel();
                    ui_channel::set_state(UiState::Idle);
                    crate::cancel::CANCEL.end();
                    cur_gen = u64::MAX;
                    eprintln!("⌫ cancelled · discarded");
                    eprintln!();
                    continue 'evloop;
                }
                cues::play_stop();
                if !ui_channel::set_phase(cur_gen, UiState::Transcribing) { continue; }
                let press_to_release = t_press.take().map(|t| t.elapsed());
                let held = press_to_release.unwrap_or_default();
                crate::daemon_log::rotate_stdio_if_needed();
                eprintln!("⏹ stopped · held {}", secs(held));

                // Focus capturing now starts at key-press (StartRecording), not
                // at release — see `spawn_focus_capture`. The AX wake /
                // cross-process cost is paid while the user is speaking, so by
                // the time the pipelines here finish the focused element is
                // already known. At release we simply join it (below).
                let t_pipeline = Instant::now();
                // Streaming ASR now performs NO per-segment cleanup — the cleanup
                // model runs once on the joined transcript at release. So there's
                // no assembled cleaned text to carry forward anymore.
                // Streaming segment bookkeeping for the per-utterance timing log.
                let mut segs_cached = 0usize;
                let mut timing_final_seg_ms = 0u128;

                // Acquire the transcript. Streaming ASR assembles it from the
                // sentences transcribed (NOT cleaned) during the hold + a final
                // open segment; everything else (pre-roll, poisoned / transform
                // fallback) takes the proven whole-buffer path, byte-for-byte
                // unchanged.
                let mut uncorrected_transcript: Option<String> = None;
                let transcript: String = 'got: {
                    // Always-on pre-roll: drain the warm stream's queued audio
                    // (the pre-roll lookback the callback flushed at press +
                    // everything since) and transcribe the whole buffer.
                    if always_on_session {
                        if let Some((ao, cons)) = always.as_mut() {
                            let recflag = ao.recording_flag();
                            let buf = complete_capture(cons, &recflag, &mut stream_audio, cur_gen);
                            let Some(keep) = guarded_transcribe_len(&buf, &noise_floor, whisper_mode) else {
                                eprintln!(
                                    "  ◌    no speech above noise floor · nothing to transcribe"
                                );
                                break 'got String::new();
                            };
                            last_audio = buf[..keep].to_vec();
                            match worker.transcribe_pcm(&buf[..keep]) {
                                Ok(t) => break 'got t,
                                Err(err) => {
                                    recovery.error = Some(format!("Transcription failed: {err}"));
                                    eprintln!("[err]  transcribe failed: {err:?}");
                                    cues::play_error();
                                    show_transcribe_failure(cur_gen);
                                    eprintln!();
                                    continue 'evloop;
                                }
                            }
                        }
                    }

                    // Open-on-press path (and the streaming variant) own a
                    // per-utterance consumer; borrow it here (rather than taking it)
                    // so a warm-window reuse can keep the ring's read handle for the
                    // next dictation. `c`/`r` are reborrowed each use.
                    let c = consumer.as_mut().unwrap();
                    let r = is_recording.as_ref().unwrap();

                    // Incremental streaming ASR: flush the final open segment, then
                    // join the cached segments into the transcript. Cleanup is NOT
                    // done per-segment — the joined text feeds the existing
                    // whole-utterance release pipeline unchanged. Falls back to a
                    // single whole-buffer transcribe of everything captured when the
                    // streamed result can't be trusted: a segment failed (poisoned),
                    // the utterance is a transform instruction (keep the proven path
                    // so commands / transform never see a VAD-split rerender), or
                    // nothing was cached (e.g. pure silence). Audio loss is
                    // impossible — `stream_audio` holds every drained sample, so the
                    // fallback is byte-for-byte today's behaviour.
                    if streaming_asr && stream.is_some() {
                        let final_seg_ms = if let Some(st) = stream.as_mut() {
                            stream_process_asr(
                                &mut worker,
                                &refiner,
                                st,
                                c,
                                &mut stream_segs,
                                &mut stream_raw_segs,
                                &mut stream_audio,
                                &mut stream_poisoned,
                                &mut noise_floor,
                                true,
                                whisper_mode,
                                cur_gen,
                            )
                        } else {
                            0
                        };
                        segs_cached = stream_segs.len();
                        timing_final_seg_ms = final_seg_ms;
                        // Boundary-repairing join: a pause before the last word
                        // makes the ASR emit "…instant." + "connection." — see
                        // `join_stream_segments` for the lowercase-start repair.
                        let joined = crate::text_polish::join_stream_segments(&stream_segs);
                        let fallback = stream_poisoned || transform || stream_segs.is_empty();
                        // Take the backup buffer so the fallback transcribes exactly
                        // what was captured (and frees it for the next utterance).
                        let whole = std::mem::take(&mut stream_audio);
                        let result = if fallback {
                            eprintln!(
                                "  ⟫ streaming fallback · whole-buffer transcribe ({} samples)",
                                whole.len()
                            );
                            match guarded_transcribe_len(&whole, &noise_floor, whisper_mode) {
                                Some(keep) => {
                                    last_audio = whole[..keep].to_vec();
                                    worker.transcribe_pcm(&whole[..keep])
                                }
                                None => {
                                    eprintln!(
                                        "  ◌    no speech above noise floor · nothing to transcribe"
                                    );
                                    Ok(String::new())
                                }
                            }
                        } else {
                            eprintln!(
                                "  ⟫ streamed {} segment(s) during hold",
                                stream_segs.len()
                            );
                            last_audio = whole.clone();
                            uncorrected_transcript = Some(stream_raw_segs.join(" "));
                            Ok(joined)
                        };
                        stream = None;
                        match result {
                            Ok(t) => break 'got t,
                            Err(err) => {
                                recovery.error = Some(format!("Transcription failed: {err}"));
                                eprintln!("[err]  transcribe failed: {err:?}");
                                cues::play_error();
                                show_transcribe_failure(cur_gen);
                                eprintln!();
                                continue 'evloop;
                            }
                        }
                    }
                    // Non-streaming open-on-press path: drain the (borrowed)
                    // consumer synchronously then transcribe, mirroring the
                    // always-on path. Borrowing leaves the consumer in `consumer`
                    // so a warm-window reuse can keep it for the next dictation.
                    let buf = complete_capture(c, r, &mut stream_audio, cur_gen);
                    let Some(keep) = guarded_transcribe_len(&buf, &noise_floor, whisper_mode) else {
                        eprintln!("  ◌    no speech above noise floor · nothing to transcribe");
                        break 'got String::new();
                    };
                    last_audio = buf[..keep].to_vec();
                    match worker.transcribe_pcm(&buf[..keep]) {
                        Ok(t) => t,
                        Err(err) => {
                            recovery.error = Some(format!("Transcription failed: {err}"));
                            eprintln!("[err]  transcribe failed: {err:?}");
                            cues::play_error();
                            show_transcribe_failure(cur_gen);
                            eprintln!();
                            continue 'evloop;
                        }
                    }
                };
                let t_transcribe = t_pipeline.elapsed();
                crate::history::append_recovery_stage(cur_gen, crate::history::StageKind::Raw,
                    uncorrected_transcript.as_deref().unwrap_or(&transcript), "ASR, complete");
                ui_channel::publish_preview(cur_gen, &transcript);

                // Warm-window keeps the drained (now-empty) consumer + flag for the
                // next reuse; otherwise drop them exactly as the pre-change code
                // did, so cold mode returns to a fully None capture state.
                if !warm_parked && !always_on_session {
                    consumer = None;
                    is_recording = None;
                }

                if transcript.trim().is_empty() {
                    recovery.status = crate::history::RecoveryStatus::Completed;
                    eprintln!("  skip · empty transcript");
                    eprintln!();
                    ui_channel::set_state(UiState::Idle);
                    continue;
                }

                // Cancel gate, second checkpoint — after ASR but before the
                // (expensive) cleanup pass. An Escape during transcription must
                // stop here rather than burn the cleanup model.
                if cur_gen != u64::MAX && crate::cancel::CANCEL.consume_cancel(cur_gen) {
                    cues::play_cancel();
                    ui_channel::set_state(UiState::Idle);
                    eprintln!("⌫ cancelled · discarded");
                    eprintln!();
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
                        Some(ref c) => recovery.status = handle_transform(&rt, c, &transcript, cur_gen),
                        None => {
                            eprintln!(
                                "  ✗    transform needs the cleanup engine (running with --no-cleanup)"
                            );
                            cues::play_error();
                        }
                    }
                    eprintln!();
                    // Leave a kept-result pill on screen; only a still-working
                    // state is reset.
                    if ui_channel::state().is_processing() {
                        ui_channel::set_state(UiState::Idle);
                    }
                    continue;
                }

                // Receive the focus target captured at key-press. It ran for
                // the whole hold, so it's almost always ready by here — the join
                // is a no-op. A very short hold just blocks on the join; the
                // cost is the same as the old release-time spawn, and usually
                // strictly less because the AX wake happened during speech, not
                // after. We need the target app *before* cleanup so we can skip
                // Gemma when the destination is a terminal. The injector still
                // resolves the current field at write time; this capture is the
                // same-app fallback only if that fresh lookup loses focus.
                let t_focus_join = Instant::now();
                let (press_app, press_target): (Option<crate::injector::TargetApp>, Option<FocusTarget>) =
                    match pending_target.take().and_then(|rx| rx.recv().ok()) {
                        Some((app, focus)) => (app, focus.ok()),
                        None => (None, None),
                    };
                let t_focus_wait_ms = ms(t_focus_join.elapsed());
                // Stale-target fallback: if the press-time capture failed, or
                // the frontmost app changed since the key was pressed (a long
                // hold or hands-free can out-last the user's focus), the retained
                // element belongs to a background app and writing to it would be
                // wrong. Trust it when AX didn't disclose a PID (nothing to
                // verify) or when the OS-frontmost app still matches. Otherwise
                // re-resolve fresh at inject exactly as the pre-press-capture
                // code did — correctness over speed.
                let captured_target: Option<FocusTarget> = press_target
                    .filter(crate::injector::focus_target_still_current);
                // One app identity for every per-app decision below (terminal
                // raw mode, tone, the log line): the press-time app while it is
                // still in front, else the app the text will actually go to.
                // Resolved once here; nothing re-reads the frontmost app later.
                let target_app = crate::injector::resolve_target_app(
                    press_app,
                    crate::injector::frontmost_pid(),
                    crate::injector::bundle_id_for_pid,
                );
                let target_pid = target_app.as_ref().map(|a| a.pid);
                if captured_target.is_some() {
                    eprintln!("  ⌖    press-time focus ready · fresh check at write");
                } else {
                    eprintln!("  ⌖    press-time focus unavailable/stale · fresh check at write");
                }

                // ─── Spoken prefix command (e.g. "translate to Chinese …") ──
                // A recognised lead-in at the START of the utterance turns the
                // rest into a one-shot transform: translate the spoken body and
                // inject the result at the cursor. Reuses the warm Gemma
                // transform engine — no selection, no second gesture. Detection
                // is deterministic (no model on the decision path); the model
                // only runs once we know it's a command. Falls through to normal
                // dictation when nothing matches, or when running --no-cleanup.
                #[cfg(feature = "cleaner")]
                if let Some(cmd) = crate::spoken_command::parse_spoken_command(&transcript).filter(|_| !limit_stopped) {
                    match cleaner {
                        Some(ref c) => {
                            recovery.status = handle_spoken_command(&rt, c, cmd, captured_target, cur_gen);
                            eprintln!();
                            // Leave a result pill ("Couldn’t paste") on screen;
                            // only a still-working state is reset.
                            if ui_channel::state().is_processing() {
                                ui_channel::set_state(UiState::Idle);
                            }
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

                // Per-app tone: a short style hint for cleanup, derived from the
                // frontmost app (chat vs email vs code …). Feeds the same
                // whole-utterance cleanup prompt as the vocabulary block.
                #[cfg(feature = "cleaner")]
                let tone: Option<&'static str> = {
                    let bundle = target_app.as_ref().and_then(|a| a.bundle_id.as_deref());
                    let name = target_pid.map(app_name).unwrap_or_default();
                    let t = crate::app_tones::tone_for(&name, bundle);
                    if let Some(t) = t {
                        eprintln!("  ♪    tone: {t}");
                    }
                    t
                };

                let mut t_cleanup_ms = 0_u128;
                let final_text = {
                    #[cfg(feature = "cleaner")]
                    {
                        match cleaner {
                            Some(ref c) if !is_terminal => {
                                if !screen_vocab.is_empty() {
                                    eprintln!("  ⌕    screen vocab: {}", crate::daemon_log::terms(&screen_vocab));
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
                                // Deterministic cleanup (filler/casing/numbers on
                                // top of already-corrected, already-punctuated
                                // Cohere text). Used both as the short-utterance
                                // shortcut and as the safe fallback when the LLM
                                // over-deletes — it can never drop a clause.
                                let deterministic =
                                    crate::text_polish::fix_speech_mechanics(&pre_corrected);
                                let word_count = pre_corrected.split_whitespace().count();

                                // Below 3 words there is nothing for the LLM to
                                // clean, tone, or format — even an active preset
                                // gains nothing reshaping "Yes." — so tiny
                                // utterances always take the deterministic path.
                                if word_count < 3
                                    || (!format_active
                                        && !crate::text_polish::needs_llm_cleanup(&pre_corrected))
                                {
                                    // (1) Clean dictation skips the generative LLM
                                    // entirely. Measured: ~87% of utterances pass
                                    // through it unchanged, and some pick up unwanted
                                    // substitutions ("open pencil" → "open source").
                                    // The deterministic pass can't drop or swap a word,
                                    // and it's faster. The LLM runs only on real mess
                                    // (contractions, stutters, dense filler, run-ons).
                                    eprintln!(
                                        "  ⚡    clean ({word_count} words) · deterministic cleanup, LLM skipped"
                                    );
                                    deterministic
                                } else {
                                    if !ui_channel::set_phase(cur_gen, UiState::Cleaning) { continue 'evloop; }
                                    let t_clean = Instant::now();
                                    match rt.block_on(c.process_transcript_with_vocab_tone(
                                        &pre_corrected,
                                        &screen_vocab,
                                        tone,
                                    )) {
                                        Ok(cleaned) => {
                                            t_cleanup_ms = ms(t_clean.elapsed());
                                            // (2) Diff-guard: if the generative
                                            // cleanup rewrote away real content
                                            // (dropped a whole clause), keep the
                                            // faithful deterministic result instead.
                                            if crate::text_polish::cleanup_dropped_content(
                                                &pre_corrected,
                                                &cleaned,
                                            ) {
                                                eprintln!(
                                                    "  ⚠    cleanup dropped content — using faithful deterministic text instead"
                                                );
                                                recovery.error = Some("Model cleanup rejected because it dropped content; deterministic text used".into());
                                                deterministic
                                            } else {
                                                crate::history::append_recovery_stage(cur_gen, crate::history::StageKind::Cleaned, &cleaned, "Accepted model cleanup");
                                                cleaned
                                            }
                                        }
                                        Err(err) => {
                                            eprintln!("[warn] cleanup failed, using raw: {err:?}");
                                            recovery.error = Some(format!("Cleanup failed; deterministic text used: {err}"));
                                            deterministic
                                        }
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
                ui_channel::publish_preview(cur_gen, &final_text);
                let (final_text, action) = match refiner.refine(&final_text).outcome() {
                    DictationOutcome::Discard => {
                        recovery.status = crate::history::RecoveryStatus::Cancelled;
                        eprintln!("  ⌫    scratch that · discarded");
                        eprintln!();
                        ui_channel::set_state(UiState::Idle);
                        continue;
                    }
                    DictationOutcome::BareAction(action) => {
                        if authorize_paste(cur_gen) {
                            execute_trailing_action(&action);
                            recovery.status = crate::history::RecoveryStatus::Completed;
                        }
                        eprintln!("  ↵    bare command (no body)");
                        eprintln!();
                        ui_channel::set_state(UiState::Idle);
                        continue;
                    }
                    DictationOutcome::Skip => {
                        recovery.status = crate::history::RecoveryStatus::Completed;
                        eprintln!("  skip · cleaned to empty");
                        eprintln!();
                        ui_channel::set_state(UiState::Idle);
                        continue;
                    }
                    DictationOutcome::Inject { text, action } => (text, action),
                };

                // The critical reject gate, evaluated at write time through
                // the injector's write-authorizer:
                //   1. cancel: `commit()` is the linearizable point that flips
                //      this utterance from cancellable → committing. It is
                //      claimed by the injector at the LAST moment — after the
                //      focus/context/padding re-resolve and immediately before
                //      the first write/clipboard side effect — so a cancel that
                //      lands during a slow (up to 1.5s) AX lookup is still
                //      honored. A refusal maps to `FinalizeResult::Cancelled`.
                //   2. focus: the injector re-resolves the CURRENT focused
                //      element and gates on its role/writability, so a stale
                //      key-press target or a same-app focus change never
                //      writes blindly. `finalize_utterance` is the tested seam;
                //      the sink is the actual AX/clipboard call.
                let used_fresh_capture = captured_target.is_none();
                let inject_source = captured_target; // moved into the sink
                let t_inject = Instant::now();
                let finish = crate::injector::finalize_utterance(
                    false,
                    // A limit stop is held by decision: known "do not write".
                    limit_stopped.then_some(crate::injector::FocusEditability::NotEditable),
                    &final_text,
                    |text| {
                        // One authorization claim, exactly once, immediately
                        // before the first side effect (no second claim across
                        // the write ladder's fallbacks).
                        let authorize = || authorize_paste(cur_gen);
                        AccessibilityInjector::inject(
                            InjectRequest { target: inject_source, authorize: Some(&authorize), ..Default::default() },
                            text,
                        )
                    },
                );
                let finish = if crate::cancel::CANCEL.requested(cur_gen) {
                    crate::injector::FinalizeResult::Cancelled
                } else { finish };
                let t_inject_ms = ms(t_inject.elapsed());
                crate::latency::record(crate::latency::Snapshot {
                    release_ms: ms(t_release.elapsed()) as u64,
                    tail_ms: t_tail_ms as u64,
                    capture_stop_ms: t_capture_stop_ms as u64,
                    asr_ms: ms(t_transcribe) as u64,
                    cleanup_ms: t_cleanup_ms as u64,
                    focus_ms: t_focus_wait_ms as u64,
                    insertion_ms: t_inject_ms as u64,
                    inserted: matches!(&finish, crate::injector::FinalizeResult::Injected),
                });

                // ─── per-utterance summary block ───────────────────────
                eprintln!(
                    "  xcr  {:>4} ms · cln {:>4} ms · inj {:>4} ms",
                    ms(t_transcribe),
                    t_cleanup_ms,
                    t_inject_ms
                );
                if streaming_asr {
                    eprintln!("  segs={segs_cached} cached · finalseg={timing_final_seg_ms} ms");
                }
                if let Some(pid) = target_pid {
                    eprintln!("  app  {} (pid {})", app_name(pid), pid);
                } else if used_fresh_capture {
                    eprintln!("  app  <fresh capture · target unknown>");
                }
                // Which cleanup model produced this — always visible, so the log
                // is self-describing when comparing models.
                #[cfg(feature = "cleaner")]
                if cleaner.is_some() {
                    let m = std::path::Path::new(&config.llm_model_path)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("?");
                    eprintln!("  llm  {m}");
                }
                // Raw→cleaned visibility. The whole-buffer path otherwise logs
                // only the final text, so a SAME-LENGTH intent rewrite (raw
                // "did you do this" → "did you actually fix everything") is
                // invisible — the shrink watchdog below never fires on it. Log
                // the raw transcript whenever cleanup changed it (paired with the
                // `✓` final line) so ASR-vs-cleanup is always attributable after
                // the fact. One line to the /tmp log, well off the hot path.
                #[cfg(feature = "cleaner")]
                if cleaner.is_some() && raw_transcript.trim() != final_text.trim() {
                    eprintln!("  ⟫ raw · {}", crate::daemon_log::text(&raw_transcript));
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
                        "  ⚠    cleanup shrank text {raw_chars} → {final_chars} chars · raw: {}",
                        crate::daemon_log::text(&raw_transcript)
                    );
                }
                // Capture is over; un-mute other audio now so the outcome cue
                // below is actually audible. `play_start` runs before `mute()`,
                // but the completion/error/cancel cues otherwise played inside
                // the muted window and were silent (the guarded Drop restores
                // only when this arm ends). Restore is idempotent, so the
                // `RestoreOnDrop` guard later is a no-op.
                crate::audio_duck::restore();
                match finish {
                    crate::injector::FinalizeResult::Injected => {
                        recovery.status = crate::history::RecoveryStatus::Completed;
                        eprintln!("  ✓    {}", crate::daemon_log::text(&final_text));
                        cues::play_success();
                        // Remember it for the menu-bar "Copy last dictation" item.
                        // Stored BEFORE the pill shows so the word count is this
                        // dictation's, not the previous one's.
                        ui_channel::set_last_dictation(&final_text);
                        // The write was just verified against the frontmost app;
                        // the pill's Undo is only valid while that app stays in front.
                        ui_channel::set_last_inject_pid(crate::injector::frontmost_pid());
                        show_result_pill();
                        // Persist to the browsable dictation history (best-effort;
                        // never blocks or fails the hot path).
                        crate::history::append_recovery_stage(cur_gen, crate::history::StageKind::Output, &final_text, "Inserted");
                        if !matches!(action, TrailingAction::None) {
                            // Clipboard insertion returns after queuing Cmd+V.
                            // Keep Enter/Tab behind its consume/restore window so
                            // a slow app cannot submit the previous field text.
                            crate::clipboard_paste::wait_for_pending_transaction();
                            std::thread::sleep(Duration::from_millis(40));
                            execute_trailing_action(&action);
                        }
                    }
                    crate::injector::FinalizeResult::Held => {
                        recovery.status = crate::history::RecoveryStatus::Held;
                        // No editable field focused (or the recording limit
                        // stopped it): never paste blindly. The words become the
                        // last dictation, go to the clipboard, and the pill says so.
                        let cause = if limit_stopped {
                            crate::last_dictation::KeptCause::LimitReached
                        } else {
                            crate::last_dictation::KeptCause::NoEditableField
                        };
                        eprintln!("  ◌    not inserted ({cause:?}) — transcript kept app={}", crate::injector::app_label_for_pid(target_pid));
                        keep_and_show(cur_gen, &final_text, cause);
                    }
                    crate::injector::FinalizeResult::Cancelled => {
                        recovery.status = crate::history::RecoveryStatus::Cancelled;
                        eprintln!("  ⌫    cancelled · discarded");
                        cues::play_cancel();
                        ui_channel::set_state(UiState::Idle);
                    }
                    crate::injector::FinalizeResult::Failed => {
                        eprintln!("  ✗    inject failed — transcript kept app={}", crate::injector::app_label_for_pid(target_pid));
                        // The words exist; only the write failed. Keep them
                        // reachable (they used to be dropped, so "Copy last
                        // dictation" returned the PREVIOUS text) and say what
                        // really happened instead of "Couldn’t transcribe".
                        keep_and_show(cur_gen, &final_text, crate::last_dictation::KeptCause::PasteFailed);
                    }
                }
                eprintln!();
                // The utterance settled (injected, held, cancelled, or failed):
                // close the cancel window so the gate reads idle and an idle
                // Escape is never swallowed.
                crate::cancel::CANCEL.end();
                cur_gen = u64::MAX;
                if ui_channel::state().is_processing() {
                    ui_channel::set_state(UiState::Idle);
                }
            }
        }

        // Give the audio cue a moment to actually start playing before we
        // potentially re-enter recording on a fast double-tap.
        std::thread::sleep(Duration::from_millis(10));
    }

    crate::history::flush_recovery(Duration::from_secs(2));
    Ok(())
}

/// Transform mode: read the focused app's current selection, apply the spoken
/// `instruction` to it with the warm Gemma engine, and paste the result over
/// the selection. Read + replace both go through the clipboard so it works in
/// AX-blind apps. All failures are logged + cue'd; never fatal.
#[cfg(feature = "cleaner")]
pub(super) fn handle_transform(rt: &tokio::runtime::Runtime, cleaner: &TextCleanupEngine, instruction: &str, generation: u64) -> crate::history::RecoveryStatus {
    use crate::history::{RecoveryStatus, StageKind};
    if !crate::cancel::CANCEL.allows_progress(generation) { return RecoveryStatus::Cancelled; }
    if let Some(problem) = cleaner.instruction_problem() {
        eprintln!("[rewrite] unavailable: {problem}; selection unchanged");
        show_rewrite_failure(generation);
        cues::play_error();
        return RecoveryStatus::Failed;
    }
    let expected = crate::injector::frontmost_pid();
    let selection = match crate::clipboard_paste::copy_selection_via_clipboard() {
        Ok(Some(s)) => s,
        Ok(None) => {
            eprintln!("  ✗    transform: nothing selected — select text first");
            show_rewrite_failure(generation);
            cues::play_error();
            return RecoveryStatus::Failed;
        }
        Err(e) => {
            eprintln!("  ✗    transform: couldn't read selection: {e:?}");
            show_rewrite_failure(generation);
            cues::play_error();
            return RecoveryStatus::Failed;
        }
    };
    eprintln!(
        "  ✎    {}  ·  {} chars selected",
        crate::daemon_log::text(instruction.trim()),
        selection.chars().count()
    );

    if !ui_channel::set_phase(generation, UiState::Cleaning) { return RecoveryStatus::Cancelled; }
    let t = Instant::now();
    let result = match rt.block_on(cleaner.transform(instruction, &selection)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("  ✗    transform failed: {e:?}");
            show_rewrite_failure(generation);
            cues::play_error();
            return RecoveryStatus::Failed;
        }
    };
    let result = result.trim();
    if result.is_empty() {
        eprintln!("  ✗    transform produced no output; selection left unchanged");
        show_rewrite_failure(generation);
        cues::play_error();
        return RecoveryStatus::Failed;
    }

    // The transformed text must land in the app the selection came from. Snapshot
    // the frontmost app and require it at the global Cmd+V (fail-closed).
    crate::history::append_recovery_stage(generation, StageKind::Transform, result, "Selection rewrite");
    ui_channel::publish_preview(generation, result);
    if !authorize_paste(generation) { return RecoveryStatus::Cancelled; }
    match crate::clipboard_paste::paste_via_clipboard(result, expected) {
        Ok(crate::clipboard_paste::PasteOutcome::Delivered) => {
            eprintln!("  ✓    transformed in {} ms", ms(t.elapsed()));
            // A rewrite is a finished transcript too: "Copy last dictation"
            // must return it, not the dictation before it.
            ui_channel::set_last_dictation(result);
            ui_channel::set_last_inject_pid(expected);
            crate::history::append_recovery_stage(generation, StageKind::Output, result, "Selection replaced");
            RecoveryStatus::Completed
        },
        Ok(crate::clipboard_paste::PasteOutcome::RefusedForeground) => {
            eprintln!("  ◌    transform refused — target app not frontmost at paste (text kept) app={}", crate::injector::app_label_for_pid(expected));
            keep_and_show(generation, result, crate::last_dictation::KeptCause::NoEditableField);
            RecoveryStatus::Held
        }
        Err(e) => {
            eprintln!("  ✗    transform paste failed: {e:?}");
            keep_and_show(generation, result, crate::last_dictation::KeptCause::PasteFailed);
            RecoveryStatus::Failed
        }
    }
}

/// Spoken prefix command (e.g. "translate to Chinese …"): run the resolved
/// Pop everything currently sitting in the capture ring buffer (non-blocking).
/// Used by the streaming path to pull newly captured audio on each tick + at
/// release without waiting on the recording flag.
pub(super) fn drain_available(consumer: &mut crate::audio::HeapAudioConsumer) -> Vec<f32> {
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

/// Retain the in-flight recording's raw PCM as the "last recording" buffer for
/// retry, replacing whatever was there. Combines the already-drained streaming
/// backup (`stream_audio`) with the still-queued ring samples drained from
/// `consumer` (the caller must have stopped/parked capture first, so the
/// recording flag is false and `drain_session` returns). A cancel with no
/// captured audio yields an empty buffer — retry then no-ops. Memory only;
/// never written to disk.
pub(super) fn retain_in_flight_audio(
    last_audio: &mut Vec<f32>,
    stream_audio: &[f32],
    consumer: Option<&mut crate::audio::HeapAudioConsumer>,
    is_recording: Option<&std::sync::atomic::AtomicBool>,
    generation: Option<u64>,
) {
    let mut retained = Vec::with_capacity(stream_audio.len() + 4096);
    retained.extend_from_slice(stream_audio);
    if let (Some(c), Some(r)) = (consumer, is_recording) {
        let tail = crate::audio::drain_session(c, r);
        retained.extend_from_slice(&tail);
        if let Some(gen) = generation {
            crate::history::append_recovery_audio(gen, tail, crate::audio::SAMPLE_RATE);
        }
    }
    *last_audio = retained;
}

/// One incremental ASR step: pull available audio into the segment stream, then
/// transcribe each completed segment (NO cleanup — whole-utterance cleanup stays
/// at release) and cache the corrected text in `segs_acc`. Every drained sample
/// is also appended to `audio_backup`, so a poisoned stream / transform / empty
/// result can fall back to a single whole-buffer transcribe at release without
/// losing audio. With `final_flush` the remaining open segment is flushed too.
///
/// Transcribe calls run on this (worker) thread, which already owns the ASR
/// worker. Trade-off kept deliberately: a StopRecording event may wait behind one
/// in-flight short-segment transcribe (~50-150 ms) rather than a background
/// thread — the single-threaded ownership keeps the segment model memory-stable.
///
/// Returns the transcribe latency (ms) of the LAST segment — the meaningful
/// "final segment" cost only when `final_flush` is set (used for the
/// per-utterance timing log). Callers ignore it during the hold.
pub(super) fn stream_process_asr(
    worker: &mut LocalInferenceWorker,
    refiner: &Refiner,
    st: &mut crate::vad::SegmentStream,
    consumer: &mut crate::audio::HeapAudioConsumer,
    segs_acc: &mut Vec<String>,
    raw_segs_acc: &mut Vec<String>,
    audio_backup: &mut Vec<f32>,
    poisoned: &mut bool,
    noise_floor: &mut crate::audio::NoiseFloorEstimator,
    final_flush: bool,
    whisper: bool,
    generation: u64,
) -> u128 {
    checkpoint_capture_with(consumer, audio_backup, |avail| {
        st.push(avail);
        let frame_len = (crate::audio::DEFAULT_TAIL_FRAME_MS as f64 / 1000.0
            * crate::audio::SAMPLE_RATE as f64) as usize;
        for rms in crate::audio::frame_rms(avail, frame_len) { noise_floor.update(rms); }
    }, |samples| {
        crate::history::append_recovery_audio(generation, samples, crate::audio::SAMPLE_RATE);
    });
    let segs = if final_flush {
        st.take_final()
    } else {
        st.take_complete()
    };
    let mut last_seg_ms = 0u128;
    let frame_ms = crate::audio::DEFAULT_TAIL_FRAME_MS;
    let frame_len = (frame_ms as f64 / 1000.0 * crate::audio::SAMPLE_RATE as f64) as usize;
    let threshold = crate::audio::speech_threshold_with_mode(noise_floor.floor(0.10), whisper);
    for (s, e) in segs {
        if !crate::cancel::CANCEL.allows_progress(generation) { break; }
        // A prior failure poisons the stream: keep draining audio into the backup
        // so the release fallback transcribes everything, but stop spending
        // transcribe calls on individual segments.
        if *poisoned {
            continue;
        }
        let seg_full = &st.buf()[s..e];
        // Hallucination guard: tail-capture keeps the mic open past release, so
        // the final segment is often mostly room tone — which the ASR renders
        // as a trailing "Thank you." Skip near-silent segments; trim the rest.
        let Some(keep) =
            crate::audio::transcribe_keep_len(seg_full, frame_len, threshold, frame_ms)
        else {
            eprintln!("  ◌    quiet segment below speech floor · skipped (hallucination guard)");
            continue;
        };
        let seg = &seg_full[..keep];
        let t = Instant::now();
        let raw = match worker.transcribe_pcm(seg) {
            Ok(t) => t.trim().to_string(),
            Err(err) => {
                eprintln!(
                    "[warn] stream seg transcribe: {err:?} — falling back to whole-buffer at release"
                );
                *poisoned = true;
                ui_channel::publish_preview(generation, "");
                continue;
            }
        };
        last_seg_ms = ms(t.elapsed());
        if raw.is_empty() {
            continue;
        }
        // Fix known proper nouns in this segment before caching — same as the
        // whole-buffer path — so the assembled text carries the intended spelling
        // (e.g. "to do list" → "Todoist"). No per-segment cleanup: the cleanup
        // model runs once on the whole utterance at release, not on partial
        // segments, so accuracy isn't traded for a marginal latency win.
        if std::env::var_os("DICTATE_DEBUG_STREAM").is_some() {
            eprintln!("  ⟫ seg · {}", crate::daemon_log::text(&raw));
        }
        let (raw_so_far, preview) = cache_stream_segment(refiner, raw, raw_segs_acc, segs_acc);
        crate::history::append_recovery_stage(generation, crate::history::StageKind::Raw, &raw_so_far, "Streaming ASR, partial");
        ui_channel::publish_preview(generation, &preview);
    }
    last_seg_ms
}

/// Raw history is the model's words; preview/delivery may apply personal
/// corrections and segment-boundary punctuation repair without rewriting Raw.
fn cache_stream_segment(refiner: &Refiner, raw: String, raw_segments: &mut Vec<String>,
    corrected_segments: &mut Vec<String>) -> (String, String) {
    corrected_segments.push(refiner.apply_corrections(&raw));
    raw_segments.push(raw);
    (raw_segments.join(" "), crate::text_polish::join_stream_segments(corrected_segments))
}

/// Pre-transcribe hallucination guard for the whole-buffer paths: trim
/// trailing room tone and return the keep-length, or `None` when the buffer
/// holds too little speech to transcribe at all (a misfire tap would only
/// produce an invented "Thank you."). Floor comes from the incremental
/// estimator when it was fed during capture, else a bounded estimate over the
/// buffer itself.
pub(super) fn guarded_transcribe_len(
    audio: &[f32],
    estimator: &crate::audio::NoiseFloorEstimator,
    whisper: bool,
) -> Option<usize> {
    let frame_ms = crate::audio::DEFAULT_TAIL_FRAME_MS;
    let frame_len = (frame_ms as f64 / 1000.0 * crate::audio::SAMPLE_RATE as f64) as usize;
    let floor = if estimator.is_empty() {
        crate::audio::noise_floor_of_audio(audio, frame_len, 0.10)
    } else {
        estimator.floor(0.10)
    };
    let threshold = crate::audio::speech_threshold_with_mode(floor, whisper);
    crate::audio::transcribe_keep_len(audio, frame_len, threshold, frame_ms)
}

/// Spawn a background thread that resolves the currently-focused UI element +
/// caret context and returns a receiver for its result. Called at key-PRESS so
/// the AX wake / cross-process cost is paid while the user is speaking, and the
/// result is joined (usually instantly) at release. A very short hold simply
/// blocks on the join — the same cost the old release-time spawn paid, and
/// usually strictly less because the resolve happened during speech, not after.
///
/// The app identity (PID + bundle id) is read first and travels with the
/// result even when the AX capture fails: terminals and other AX-blind apps
/// expose no focused element, and they are exactly the apps whose identity
/// decides raw-vs-cleaned text.
pub(super) fn spawn_focus_capture() -> std::sync::mpsc::Receiver<PressCapture> {
    let (tx, rx) = std::sync::mpsc::channel::<PressCapture>();
    std::thread::spawn(move || {
        let app = crate::injector::frontmost_target_app();
        let _ = tx.send((app, FocusTarget::capture()));
    });
    rx
}

/// What the press-time capture thread hands back: the app, and its focus.
pub(super) type PressCapture = (Option<crate::injector::TargetApp>, eyre::Result<FocusTarget>);

/// Start a fresh open-on-press recording: build a new engine, open the mic,
/// arm the streaming segment stream, and set the per-utterance state (engine,
/// consumer, flag, segment accumulators). Returns false if the mic failed to
/// open (caller fires the error cue). Shared by the event-tap StartRecording and
/// the tail-capture re-press restart so a fast re-press reuses identical setup
/// instead of duplicating it.
#[allow(clippy::too_many_arguments)]
pub(super) fn begin_open_capture(
    engine: &mut Option<AudioCaptureEngine>,
    consumer: &mut Option<crate::audio::HeapAudioConsumer>,
    is_recording: &mut Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    stream: &mut Option<crate::vad::SegmentStream>,
    stream_segs: &mut Vec<String>,
    stream_raw_segs: &mut Vec<String>,
    stream_audio: &mut Vec<f32>,
    stream_poisoned: &mut bool,
    transform_mode: &mut bool,
    t_press: &mut Option<Instant>,
    streaming_asr: bool,
    transform: bool,
    whisper: bool,
) -> bool {
    *transform_mode = transform;
    *t_press = Some(Instant::now());
    let (mut e, c) = AudioCaptureEngine::new(BUFFER_CAPACITY);
    let r = match e.start_microphone() {
        Ok(r) => r,
        Err(err) => {
            eprintln!("[err]  mic open failed: {err:?}");
            // A denied or missing microphone is the reason, and it is the one
            // the user can act on. Raising it here means the menu bar, the
            // pill and the Setup window all say the same thing.
            crate::permissions::refresh_microphone_health();
            if !crate::health::is_raised(crate::health::Fault::MicrophoneBlocked) {
                crate::health::raise(crate::health::Fault::AudioLost, format!("{err}"));
            }
            return false;
        }
    };
    cues::play_start();
    // Mute any other system audio for the duration of capture + processing;
    // restored on every release exit path via the RestoreOnDrop guard.
    crate::audio_duck::mute();
    crate::ipc::DAEMON_RECORDING.store(true, Ordering::SeqCst);
    *engine = Some(e);
    *consumer = Some(c);
    *is_recording = Some(r);
    stream_segs.clear();
    stream_raw_segs.clear();
    stream_audio.clear();
    *stream_poisoned = false;
    if streaming_asr {
        *stream = Some(crate::vad::SegmentStream::new(
            crate::audio::SAMPLE_RATE,
            vad_config(whisper),
        ));
        stream_segs.clear();
        stream_raw_segs.clear();
        stream_audio.clear();
        *stream_poisoned = false;
    }
    eprintln!("▶ recording");
    true
}

/// transform on the spoken body with the warm Gemma engine and inject the result
/// at the cursor, like a normal dictation. Unlike `handle_transform` there's no
/// selection and no clipboard round-trip — the body is fresh speech, so we
/// inject straight into the focus target (with the usual AX/clipboard fallback).
/// Cleanup and refinement are deliberately skipped: the output is already the
/// final text (often a non-English translation), and the corrections dictionary
/// + trailing-command parse are English-prose transforms that don't apply.
#[cfg(feature = "cleaner")]
pub(super) fn handle_spoken_command(
    rt: &tokio::runtime::Runtime,
    cleaner: &TextCleanupEngine,
    cmd: crate::spoken_command::SpokenCommand,
    captured_target: Option<FocusTarget>,
    generation: u64,
) -> crate::history::RecoveryStatus {
    use crate::history::{RecoveryStatus, StageKind};
    if !ui_channel::set_phase(generation, UiState::Cleaning) { return RecoveryStatus::Cancelled; }
    eprintln!("  ⌘    {} · {} chars", cmd.label, cmd.body.chars().count());
    let t = Instant::now();
    let result = match rt.block_on(cleaner.transform(&cmd.instruction, &cmd.body)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("  ✗    command failed: {e:?}");
            show_rewrite_failure(generation);
            cues::play_error();
            return RecoveryStatus::Failed;
        }
    };
    let result = result.trim();
    if result.is_empty() {
        eprintln!("  ✗    command produced no output");
        show_rewrite_failure(generation);
        cues::play_error();
        return RecoveryStatus::Failed;
    }

    crate::history::append_recovery_stage(generation, StageKind::Transform, result, &cmd.label);
    ui_channel::publish_preview(generation, result);
    let authorize = || authorize_paste(generation);
    let target_pid = captured_target.as_ref().and_then(|t| t.inner_pid());
    let inject_result = AccessibilityInjector::inject(
        InjectRequest { target: captured_target, authorize: Some(&authorize), ..Default::default() },
        result,
    );
    match inject_result {
        Ok(crate::injector::InjectOutcome::Injected) => {
            eprintln!("  ✓    {} ms → {}", ms(t.elapsed()), crate::daemon_log::text(result));
            ui_channel::set_last_dictation(result);
            crate::history::append_recovery_stage(generation, StageKind::Output, result, "Inserted");
            RecoveryStatus::Completed
        }
        Ok(crate::injector::InjectOutcome::NoEditableField) => {
            eprintln!("  ◌    no text field selected — result kept app={}", crate::injector::app_label_for_pid(target_pid));
            keep_and_show(generation, result, crate::last_dictation::KeptCause::NoEditableField);
            RecoveryStatus::Held
        }
        Ok(crate::injector::InjectOutcome::Cancelled) => {
            eprintln!("  ⌫    cancelled · discarded");
            cues::play_cancel();
            RecoveryStatus::Cancelled
        }
        Ok(crate::injector::InjectOutcome::RefusedForeground) => {
            // Fail-closed: the expected app was not frontmost at the global
            // Cmd+V, so nothing was written. Keep it recoverable.
            eprintln!("  ◌    target not frontmost at paste — result kept app={}", crate::injector::app_label_for_pid(target_pid));
            keep_and_show(generation, result, crate::last_dictation::KeptCause::NoEditableField);
            RecoveryStatus::Held
        }
        Err(e) => {
            eprintln!("  ✗    inject failed: {e:?} — result kept app={}", crate::injector::app_label_for_pid(target_pid));
            keep_and_show(generation, result, crate::last_dictation::KeptCause::PasteFailed);
            RecoveryStatus::Failed
        }
    }
}

/// A finished transcript did not reach the app. Make it the last dictation,
/// put it on the clipboard (setting permitting), note it in History, play the
/// error cue and show the kept-result pill for this generation: about four
/// seconds, or until the next key press, with the History button on it.
pub(super) fn keep_and_show(generation: u64, text: &str, cause: crate::last_dictation::KeptCause) {
    let state = crate::recovery::keep_transcript(text, cause);
    let copied = matches!(
        state,
        UiState::HeldCopied | UiState::PasteFailedCopied | UiState::LimitReachedCopied
    );
    crate::history::append_recovery_stage(
        generation,
        crate::history::StageKind::Output,
        text,
        crate::last_dictation::kept_stage_note(cause, copied),
    );
    ui_channel::show_transient_for(generation, state, ui_channel::KEPT_PILL);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_raw_layer_keeps_asr_words_before_personal_corrections() {
        let corrections = Corrections::from_map(std::collections::HashMap::from([
            ("open eye".into(), "OpenAI".into()),
        ]));
        let refiner = Refiner::new(corrections);
        let mut raw = Vec::new();
        let mut corrected = Vec::new();
        let (history, preview) = cache_stream_segment(&refiner, "Ask open eye.".into(), &mut raw, &mut corrected);
        assert_eq!(history, "Ask open eye.");
        assert_eq!(preview, "Ask OpenAI.");
        let (history, _) = cache_stream_segment(&refiner, "Press enter.".into(), &mut raw, &mut corrected);
        assert_eq!(history, "Ask open eye. Press enter.", "Raw retains spoken actions too");
    }

    #[test]
    fn checkpoints_and_final_tail_keep_every_sample_once_in_each_capture_mode() {
        use ringbuf::{HeapRb, traits::{Split, Producer}};
        for streaming in [false, true] {
            for preroll in [false, true] {
                let (mut producer, mut consumer) = HeapRb::<f32>::new(32).split();
                let mut backup = Vec::new();
                let mut archived = Vec::new();
                let mut segmenter = crate::vad::SegmentStream::new(crate::audio::SAMPLE_RATE, vad_config(false));
                let mut input = Vec::new();
                for chunk in [vec![0.1, 0.2], vec![0.3, 0.4]] {
                    input.extend_from_slice(&chunk);
                    producer.push_slice(&chunk);
                    checkpoint_capture_with(&mut consumer, &mut backup, |samples| {
                        if streaming && !preroll { segmenter.push(samples); }
                    }, |samples| archived.extend(samples));
                }
                let tail = [0.5, 0.6];
                input.extend_from_slice(&tail);
                producer.push_slice(&tail);
                let audio = complete_capture_with(&mut consumer, &AtomicBool::new(false), &mut backup,
                    |samples| archived.extend(samples));
                assert_eq!(audio, input, "ASR lost/duplicated samples, streaming={streaming} preroll={preroll}");
                assert_eq!(archived, input, "archive lost/duplicated samples");
                assert!(backup.is_empty(), "next recording must not inherit checkpoint audio");
                assert!(drain_available(&mut consumer).is_empty());
            }
        }
    }

    #[test]
    fn cancel_retains_checkpoint_audio_and_undrained_tail_without_previous_recording() {
        use ringbuf::{HeapRb, traits::{Split, Producer}};
        let (mut producer, mut consumer) = HeapRb::<f32>::new(16).split();
        producer.push_slice(&[0.3, 0.4]);
        let mut last = vec![9.0];
        retain_in_flight_audio(&mut last, &[0.1, 0.2], Some(&mut consumer), Some(&AtomicBool::new(false)), None);
        assert_eq!(last, vec![0.1, 0.2, 0.3, 0.4]);
        assert!(drain_available(&mut consumer).is_empty());
    }

    /// Synthetic-audio fixture: the already-drained streaming backup (what the
    /// hold-time ticks moved into `stream_audio`) plus a drained ring tail.
    /// `retain_in_flight_audio` must concatenate both and REPLACE the previous
    /// buffer — no history/log reads, no disk, memory only.
    #[test]
    fn retain_keeps_stream_backup_and_replaces_previous_buffer() {
        let mut last = vec![9.0, 9.0, 9.0];
        let stream_audio = vec![1.0, 2.0, 3.0];
        // No live consumer: only the already-drained backup is retained.
        retain_in_flight_audio(&mut last, &stream_audio, None, None, None);
        assert_eq!(last, vec![1.0, 2.0, 3.0], "must replace, not append, the old buffer");
    }

    #[test]
    fn retain_with_no_audio_yields_empty_so_retry_noops() {
        let mut last = vec![5.0];
        retain_in_flight_audio(&mut last, &[], None, None, None);
        assert!(last.is_empty(), "a cancel with nothing captured must not leave a stale buffer");
    }

    #[test]
    fn retain_empty_does_not_keep_zero_length_invalid_retry() {
        // A retained buffer is only meaningful when it holds samples; the
        // worker's retry path no-ops on an empty buffer.
        let mut last = Vec::new();
        retain_in_flight_audio(&mut last, &[], None, None, None);
        assert!(last.is_empty());
    }
}

/// Flash the "Landed · N words" pill, or go straight to Idle when the result
/// pill is off (default). The text landing at the cursor is the real feedback.
pub(crate) fn show_result_pill() {
    if crate::settings::Settings::load().resolve_show_result() {
        ui_channel::show_transient(
            UiState::Success,
            Duration::from_millis(crate::settings::RESULT_PILL_MS),
        );
    } else {
        ui_channel::set_state(UiState::Idle);
    }
}

/// Claim the existing write gate, then publish Pasting at the actual boundary.
/// A cancellation that beat this claim never presents a late paste stage.
pub(super) fn authorize_paste(generation: u64) -> bool {
    ui_channel::authorize_paste(generation)
}

/// Queue ownership of captured samples to the archive after retaining the ASR
/// backup. No disk I/O runs here or on the audio callback.
fn checkpoint_capture(consumer: &mut crate::audio::HeapAudioConsumer, backup: &mut Vec<f32>, generation: u64) {
    checkpoint_capture_with(consumer, backup, |_| {}, |samples| {
        crate::history::append_recovery_audio(generation, samples, crate::audio::SAMPLE_RATE);
    });
}

/// Shared by streaming, whole-buffer, and pre-roll capture. The observer runs
/// before ownership of the samples is handed to the archive queue.
fn checkpoint_capture_with(consumer: &mut crate::audio::HeapAudioConsumer, backup: &mut Vec<f32>,
    mut observe: impl FnMut(&[f32]), archive: impl FnOnce(Vec<f32>)) {
    let samples = drain_available(consumer);
    if samples.is_empty() { return; }
    observe(&samples);
    backup.extend_from_slice(&samples);
    archive(samples);
}

fn complete_capture(consumer: &mut crate::audio::HeapAudioConsumer,
    recording: &std::sync::atomic::AtomicBool, backup: &mut Vec<f32>, generation: u64) -> Vec<f32> {
    complete_capture_with(consumer, recording, backup, |tail| {
        crate::history::append_recovery_audio(generation, tail, crate::audio::SAMPLE_RATE);
    })
}

fn complete_capture_with(consumer: &mut crate::audio::HeapAudioConsumer,
    recording: &std::sync::atomic::AtomicBool, backup: &mut Vec<f32>, archive: impl FnOnce(Vec<f32>)) -> Vec<f32> {
    let tail = crate::audio::drain_session(consumer, recording);
    backup.extend_from_slice(&tail);
    if !tail.is_empty() { archive(tail); }
    std::mem::take(backup)
}

fn begin_recording_feedback(preview: bool, capture: Arc<AtomicBool>) -> u64 {
    let generation = ui_channel::begin_capturing(preview, capture);
    crate::history::begin_recovery(generation);
    generation
}

struct RecoveryFinish {
    generation: u64,
    status: crate::history::RecoveryStatus,
    error: Option<String>,
    finished: bool,
}
impl RecoveryFinish {
    fn new(generation: u64) -> Self {
        Self { generation, status: crate::history::RecoveryStatus::Failed, error: None, finished: false }
    }

    fn finish_now(&mut self) {
        if self.finished { return; }
        self.finished = true;
        let status = if crate::cancel::CANCEL.requested(self.generation) {
            crate::history::RecoveryStatus::Cancelled
        } else { self.status };
        crate::history::finish_recovery_with_error(self.generation, status, self.error.as_deref());
    }
}
impl Drop for RecoveryFinish {
    fn drop(&mut self) { self.finish_now(); }
}
