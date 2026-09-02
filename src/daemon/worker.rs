//! Worker thread: capture, streaming ASR, cleanup, transform, spoken commands, injection.
use super::*;

pub(super) fn worker_loop(
    rx: std::sync::mpsc::Receiver<DaemonEvent>,
    config: DaemonConfig,
) -> eyre::Result<()> {
    cues::preload();
    let t_load = Instant::now();
    let mut worker = LocalInferenceWorker::initialize(&config.asr)?;
    eprintln!(
        "[boot] asr         loaded in {:>4} ms · {}",
        ms(t_load.elapsed()),
        config.asr.describe()
    );

    #[cfg(feature = "cleaner")]
    let cleaner = if !config.no_cleanup {
        let pretty = std::path::Path::new(&config.llm_model_path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&config.llm_model_path);
        let t = Instant::now();
        let c = TextCleanupEngine::initialize(&config.llm_model_path)?;
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
    eprintln!(
        "[boot] warm-up     done   in {:>4} ms",
        ms(t_warm.elapsed())
    );

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

    // Focus capture at key-press. The currently-focused UI element + caret
    // context are resolved on a background thread when recording starts, so the
    // AX wake / cross-process cost is paid while the user is speaking. At
    // release we join this receiver (usually instant) and reuse the target; a
    // failed or stale press-time capture re-resolves fresh. `None` means no
    // capture is in flight (between utterances / after a cancel).
    let mut pending_target: Option<std::sync::mpsc::Receiver<eyre::Result<FocusTarget>>> = None;

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
    let format_active = crate::settings::Settings::load().resolve_format().is_some();
    // Per-utterance streaming accumulators (only used when `streaming_asr`). These
    // are the whole incremental-ASR state, independent of the cleaner feature:
    // transcribe_pcm + SegmentStream + corrections need no LLM.
    let mut stream: Option<crate::vad::SegmentStream> = None;
    // Cached corrected text of each completed segment (VAD-pause-bounded).
    let mut stream_segs: Vec<String> = Vec::new();
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

    'evloop: loop {
        // While recording with streaming ASR, wake periodically to transcribe any
        // sentence a VAD pause has just closed; otherwise block for the next event.
        // recv_timeout keeps everything on this one thread, which already owns the
        // ASR worker — no shared state.
        let event = {
            // Ticks are needed while an engine is alive AND either streaming ASR
            // is active (to transcribe VAD-closed sentences mid-hold) or the
            // engine is warm-parked (to expire the warm window and release the
            // mic once idle past the deadline). A quiet cold-open recording with
            // no streaming and no warm-window blocks for the next event, exactly
            // as before.
            if engine.is_some() && (streaming_asr || warm_parked) {
                match rx.recv_timeout(std::time::Duration::from_millis(120)) {
                    Ok(ev) => ev,
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
                        } else if streaming_asr {
                            if let (Some(st), Some(c)) = (stream.as_mut(), consumer.as_mut()) {
                                stream_process_asr(
                                    &mut worker,
                                    &refiner,
                                    st,
                                    c,
                                    &mut stream_segs,
                                    &mut stream_audio,
                                    &mut stream_poisoned,
                                    &mut noise_floor,
                                    false,
                                    whisper_mode,
                                );
                            }
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
            DaemonEvent::Retry => {
                // Re-run the pipeline on the last utterance's audio (control
                // socket "retry" / menu-bar item). No-op until the first
                // successful dictation has stashed its audio.
                #[cfg(feature = "cleaner")]
                retry_last(&rt, &mut worker, cleaner.as_ref(), &refiner, &last_audio);
                #[cfg(not(feature = "cleaner"))]
                retry_last(&mut worker, &refiner, &last_audio);
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
                pending_target = None; // discard the orphaned press-time capture
                if let Some(e) = on_demand_engine.as_mut() {
                    if warm_window_ms > 0 && preroll_ms == 0 {
                        // Warm-window cancel: park the mic for reuse, but first
                        // drain + discard the cancelled audio so the next recording
                        // starts from a clean ring.
                        e.park();
                        if let (Some(c), Some(r)) = (consumer.as_mut(), is_recording.as_ref()) {
                            let _ = crate::audio::drain_session(c, r);
                        }
                        engine = on_demand_engine.take();
                        warm_parked = true;
                        warm_deadline = Some(Instant::now()
                            + Duration::from_millis(warm_window_ms as u64));
                    } else {
                        e.stop_capture();
                    }
                }
                if always_on_session {
                    // End the session and drain+discard the buffered audio so the
                    // next press starts from a clean ring.
                    if let Some((ao, cons)) = always.as_mut() {
                        ao.end_session();
                        let recflag = ao.recording_flag();
                        let _ = crate::audio::drain_session(cons, &recflag);
                    }
                } else if !warm_parked {
                    // Drop the per-utterance consumer + recording flag unread (the
                    // warm-park path keeps them for the next reuse).
                    consumer.take();
                    is_recording.take();
                }
                stream = None;
                stream_segs.clear();
                stream_audio.clear();
                stream_poisoned = false;
                cues::play_cancel();
                ui_channel::set_state(UiState::Idle);
                eprintln!("⌫ cancelled · discarded");
                eprintln!();
            }
            DaemonEvent::StartRecording { transform } => {
                if session_active {
                    continue;
                }
                // Warm-window reuse: a parked engine (mic open but idle) is
                // resumed in place, skipping cpal's ~120 ms stream-open cost for
                // a back-to-back dictation. Rebuild instead if the default input
                // changed since the stream was built.
                if engine.is_some() {
                    if warm_parked {
                        let changed = engine
                            .as_mut()
                            .map(|e| e.device_changed())
                            .unwrap_or(false);
                        if changed {
                            eprintln!("[audio] default input changed → rebuilt warm stream");
                            drop(engine.take());
                            consumer.take();
                            is_recording.take();
                            warm_parked = false;
                            warm_deadline = None;
                            // fall through to a fresh open below
                        } else {
                            let r = engine.as_mut().unwrap().resume();
                            is_recording = Some(r);
                            warm_parked = false;
                            warm_deadline = None;
                            transform_mode = transform;
                            t_press = Some(Instant::now());
                            cues::play_start();
                            crate::audio_duck::mute();
                            ui_channel::set_state(UiState::Recording);
                            eprintln!("▶ recording (warm reuse)");
                            crate::ipc::DAEMON_RECORDING.store(true, Ordering::SeqCst);
                            if streaming_asr {
                                stream = Some(crate::vad::SegmentStream::new(
                                    crate::audio::SAMPLE_RATE,
                                    vad_config(whisper_mode),
                                ));
                                // Reset the per-utterance streaming accumulators so
                                // a warm-window reuse never carries stale segments
                                // or a stale poison flag into the new utterance.
                                stream_segs.clear();
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
                    if always
                        .as_ref()
                        .map(|(ao, _)| ao.device_changed())
                        .unwrap_or(false)
                    {
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
                        noise_floor.clear();
                        ao.begin_session();
                        cues::play_start();
                        crate::audio_duck::mute();
                        ui_channel::set_state(UiState::Recording);
                        eprintln!("▶ recording · pre-roll");
                        session_active = true;
                        crate::ipc::DAEMON_RECORDING.store(true, Ordering::SeqCst);
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
                    &mut stream_audio,
                    &mut stream_poisoned,
                    &mut transform_mode,
                    &mut t_press,
                    streaming_asr,
                    transform,
                    whisper_mode,
                ) {
                    cues::play_error();
                } else {
                    noise_floor.clear();
                    pending_target = Some(spawn_focus_capture());
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
                // Capture (and reset) whether this utterance is a transform. Done
                // up-front so the tail-capture re-press restart can re-set it with
                // the new gesture.
                #[allow(unused_variables)]
                let transform = transform_mode;
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
                                    grace_outcome = Some(TailGraceOutcome::Cancel);
                                    break;
                                }
                                // A duplicate release or a stray latch cue during the
                                // grace tail is harmless: keep waiting for quiet.
                                Ok(DaemonEvent::StopRecording)
                                | Ok(DaemonEvent::LatchArmed)
                                | Ok(DaemonEvent::Retry) => {}
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
                        // The engine is still open and capturing during the grace
                        // tail. Reuse it instead of dropping it for a ~120 ms cold
                        // open: park (stop production), drain+discard the abandoned
                        // utterance's audio, then resume in place on the same stream.
                        if let Some(e) = on_demand_engine.as_mut() {
                            e.park();
                            if let (Some(c), Some(r)) = (consumer.as_mut(), is_recording.as_ref()) {
                                let _ = crate::audio::drain_session(c, r);
                            }
                            e.resume();
                        }
                        engine = on_demand_engine;
                        transform_mode = transform;
                        t_press = Some(Instant::now());
                        stream_segs.clear();
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
                        ui_channel::set_state(UiState::Recording);
                        crate::ipc::DAEMON_RECORDING.store(true, Ordering::SeqCst);
                        eprintln!("  ↻    re-press during tail-capture · reused warm stream");
                        pending_target = Some(spawn_focus_capture());
                        continue 'evloop;
                    }
                    Some(TailGraceOutcome::Cancel) => {
                        on_demand_engine.as_mut().unwrap().stop_capture();
                        consumer.take();
                        is_recording.take();
                        pending_target = None; // discard the orphaned press-time capture
                        stream = None;
                        stream_segs.clear();
                        stream_audio.clear();
                        stream_poisoned = false;
                        cues::play_cancel();
                        ui_channel::set_state(UiState::Idle);
                        let _unmute = crate::audio_duck::RestoreOnDrop;
                        eprintln!("⌫ cancelled · discarded");
                        eprintln!();
                        continue 'evloop;
                    }
                    None => {}
                }

                // Un-mute other audio when this arm exits — by any path
                // (transcribe error, empty/discarded utterance, transform,
                // or successful inject). The guard's Drop runs on every
                // `continue` and the normal fall-through, so the restore can't
                // be forgotten on a new code path.
                let _unmute = crate::audio_duck::RestoreOnDrop;
                // End capture for whichever path is live. Open-on-press either parks
                // the stream (warm-window reuse) or drops it (releasing the mic);
                // always-on just ends the session — the warm stream keeps running.
                if let Some(e) = on_demand_engine.as_mut() {
                    if warm_window_ms > 0 && preroll_ms == 0 {
                        e.park();
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
                cues::play_stop();
                ui_channel::set_state(UiState::Processing);
                let press_to_release = t_press.take().map(|t| t.elapsed());
                let held = press_to_release.unwrap_or_default();
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
                let transcript: String = 'got: {
                    // Always-on pre-roll: drain the warm stream's queued audio
                    // (the pre-roll lookback the callback flushed at press +
                    // everything since) and transcribe the whole buffer.
                    if always_on_session {
                        if let Some((ao, cons)) = always.as_mut() {
                            let recflag = ao.recording_flag();
                            let buf = crate::audio::drain_session(cons, &recflag);
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
                                &mut stream_audio,
                                &mut stream_poisoned,
                                &mut noise_floor,
                                true,
                                whisper_mode,
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
                            Ok(joined)
                        };
                        stream = None;
                        match result {
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
                    // Non-streaming open-on-press path: drain the (borrowed)
                    // consumer synchronously then transcribe, mirroring the
                    // always-on path. Borrowing leaves the consumer in `consumer`
                    // so a warm-window reuse can keep it for the next dictation.
                    let buf = crate::audio::drain_session(c, r);
                    let Some(keep) = guarded_transcribe_len(&buf, &noise_floor, whisper_mode) else {
                        eprintln!("  ◌    no speech above noise floor · nothing to transcribe");
                        break 'got String::new();
                    };
                    last_audio = buf[..keep].to_vec();
                    match worker.transcribe_pcm(&buf[..keep]) {
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

                // Warm-window keeps the drained (now-empty) consumer + flag for the
                // next reuse; otherwise drop them exactly as the pre-change code
                // did, so cold mode returns to a fully None capture state.
                if !warm_parked && !always_on_session {
                    consumer = None;
                    is_recording = None;
                }

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

                // Receive the focus target captured at key-press. It ran for
                // the whole hold, so it's almost always ready by here — the join
                // is a no-op. A very short hold just blocks on the join; the
                // cost is the same as the old release-time spawn, and usually
                // strictly less because the AX wake happened during speech, not
                // after. We need the target app *before* cleanup so we can skip
                // Gemma when the destination is a terminal — and we reuse this
                // same target for injection, keeping get_focused_element off the
                // critical path.
                let press_target: Option<FocusTarget> = match pending_target.take() {
                    Some(rx) => rx.recv().ok().and_then(|r| r.ok()),
                    None => None,
                };
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
                let target_pid = captured_target.as_ref().and_then(|t| t.inner_pid());
                if captured_target.is_some() {
                    eprintln!("  ⌖    reuse press-time focus target");
                } else {
                    eprintln!("  ⌖    fresh focus capture (press target failed/stale)");
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

                // Per-app tone: a short style hint for cleanup, derived from the
                // frontmost app (chat vs email vs code …). Feeds the same
                // whole-utterance cleanup prompt as the vocabulary block.
                #[cfg(feature = "cleaner")]
                let tone: Option<&'static str> = {
                    let bundle = crate::audio_duck::frontmost_bundle_id();
                    let name = target_pid.map(app_name).unwrap_or_default();
                    let t = crate::app_tones::tone_for(&name, bundle.as_deref());
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
                                                deterministic
                                            } else {
                                                cleaned
                                            }
                                        }
                                        Err(err) => {
                                            eprintln!("[warn] cleanup failed, using raw: {err:?}");
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
                    eprintln!("  ⟫ raw · \"{raw_transcript}\"");
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
                    ui_channel::show_transient(UiState::Error, Duration::from_millis(650));
                } else {
                    eprintln!("  ✓    \"{final_text}\"");
                    cues::play_success();
                    ui_channel::show_transient(UiState::Success, Duration::from_millis(420));
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
                if ui_channel::state() == UiState::Processing {
                    ui_channel::set_state(UiState::Idle);
                }
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
pub(super) fn handle_transform(rt: &tokio::runtime::Runtime, cleaner: &TextCleanupEngine, instruction: &str) {
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
    eprintln!(
        "  ✎    \"{}\"  ·  {} chars selected",
        instruction.trim(),
        selection.chars().count()
    );

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
        Ok(()) => eprintln!(
            "  ✓    transformed in {} ms → \"{result}\"",
            ms(t.elapsed())
        ),
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
    audio_backup: &mut Vec<f32>,
    poisoned: &mut bool,
    noise_floor: &mut crate::audio::NoiseFloorEstimator,
    final_flush: bool,
    whisper: bool,
) -> u128 {
    let avail = drain_available(consumer);
    if !avail.is_empty() {
        st.push(&avail);
        audio_backup.extend_from_slice(&avail);
        // Feed the incremental noise-floor estimator with this tick's frames so
        // the release engage check reads a bounded floor instead of re-sorting
        // the whole utterance on the hot path.
        let frame_len = (crate::audio::DEFAULT_TAIL_FRAME_MS as f64 / 1000.0
            * crate::audio::SAMPLE_RATE as f64)
            as usize;
        for r in crate::audio::frame_rms(&avail, frame_len) {
            noise_floor.update(r);
        }
    }
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
        let corrected = refiner.apply_corrections(&raw);
        if std::env::var_os("DICTATE_DEBUG_STREAM").is_some() {
            eprintln!("  ⟫ seg · {raw:?}");
        }
        segs_acc.push(corrected);
    }
    last_seg_ms
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
pub(super) fn spawn_focus_capture() -> std::sync::mpsc::Receiver<eyre::Result<FocusTarget>> {
    let (tx, rx) = std::sync::mpsc::channel::<eyre::Result<FocusTarget>>();
    std::thread::spawn(move || {
        let _ = tx.send(FocusTarget::capture());
    });
    rx
}

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
            return false;
        }
    };
    cues::play_start();
    // Mute any other system audio for the duration of capture + processing;
    // restored on every release exit path via the RestoreOnDrop guard.
    crate::audio_duck::mute();
    ui_channel::set_state(UiState::Recording);
    crate::ipc::DAEMON_RECORDING.store(true, Ordering::SeqCst);
    *engine = Some(e);
    *consumer = Some(c);
    *is_recording = Some(r);
    if streaming_asr {
        *stream = Some(crate::vad::SegmentStream::new(
            crate::audio::SAMPLE_RATE,
            vad_config(whisper),
        ));
        stream_segs.clear();
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
