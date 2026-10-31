//! Release tail-capture: keep the mic open for a brief adaptive grace after
//! the key comes up so a word still being spoken is not clipped, and settle a
//! re-press, a cancel or a quit that lands during it.
use super::*;

/// How a tail-capture grace loop ended, when it was cut short by an event
/// rather than by a natural quiet run or the cap. The release decides whether
/// to restart a fresh recording (the user re-pressed mid-grace), cancel the
/// abandoned utterance, or quit, instead of proceeding to release it.
#[derive(Debug)]
pub(super) enum TailGraceOutcome {
    /// The user pressed again during the grace tail: abandon the previous
    /// utterance and start a new recording cleanly. Carries the new gesture.
    RePress { transform: bool },
    /// An external cancel arrived during the grace tail: discard everything.
    Cancel,
    /// Quit during the tail: abandon this utterance and release the models.
    Shutdown,
}

impl WorkerState {
    /// Open-on-press only; always-on pre-roll already keeps rolling past the
    /// release. On release the mic used to be dropped immediately, which
    /// clipped a word still being spoken. Instead keep the engine open for a
    /// brief adaptive grace: poll the live level, and stop once it stays below
    /// a room-adaptive threshold for a short quiet run (or the cap). The grace
    /// audio accumulates in the ring and is drained by the release pipeline
    /// next. Only engages when the release plausibly happened mid-speech; a
    /// release in a pause costs nothing. `None` means the release goes on.
    pub(super) fn tail_grace(
        &mut self,
        rx: &std::sync::mpsc::Receiver<DaemonEvent>,
        engine_live: bool,
        generation: u64,
    ) -> Option<TailGraceOutcome> {
        let capture = self.config.capture;
        if !capture.tail_capture || !engine_live {
            return None;
        }
        let tail_cap = Duration::from_millis(capture.tail_capture_cap_ms as u64);
        let (threshold, engage) = {
            let frame_len = (crate::audio::DEFAULT_TAIL_FRAME_MS as f64 / 1000.0
                * crate::audio::SAMPLE_RATE as f64)
                as usize;
            // Bounded trailing window: the engage decision never scans or
            // copies the whole utterance on this hot path.
            let tail = crate::audio::recording_tail(
                &self.ring.as_ref().expect("a live engine has its ring").consumer,
                &self.stream.audio,
                crate::audio::DEFAULT_TAIL_ENGAGE_MS,
            );
            // The floor comes from the estimator maintained during capture
            // (streaming) or, in the whole-buffer path, from a bounded pass
            // over the same trailing window; never a full sort of the utterance.
            let floor = if self.noise_floor.is_empty() {
                crate::audio::noise_floor_of_audio(
                    &tail,
                    frame_len,
                    crate::audio::TAIL_NOISE_PERCENTILE,
                )
            } else {
                self.noise_floor.floor(crate::audio::TAIL_NOISE_PERCENTILE)
            };
            crate::audio::tail_capture_engage_with_mode(
                &tail,
                frame_len,
                floor,
                crate::audio::DEFAULT_TAIL_ENGAGE_MS,
                crate::audio::DEFAULT_TAIL_FRAME_MS,
                capture.whisper_mode,
            )
        };
        if !engage {
            return None;
        }
        let grace_start = Instant::now();
        let poll = Duration::from_millis(crate::audio::DEFAULT_TAIL_POLL_MS);
        let quiet_ms = crate::audio::DEFAULT_TAIL_QUIET_MS;
        let mut series: Vec<f32> = Vec::new();
        let mut recovered_ms: u128 = 0;
        let mut outcome = None;
        loop {
            if let Some(level) = ui_channel::recent_levels().last().copied() {
                series.push(level);
                if series.len() > 16 {
                    series.remove(0);
                }
            }
            // A re-press or cancel during the grace tail must not be lost.
            // Consuming it here abandons the grace so we can restart/cancel
            // cleanly instead of waiting out the tail.
            match rx.try_recv() {
                Ok(DaemonEvent::StartRecording { transform }) => {
                    outcome = Some(TailGraceOutcome::RePress { transform });
                    break;
                }
                Ok(DaemonEvent::CancelRecording) => {
                    // A delayed cancel belongs only to the generation that
                    // accepted it, never a fresh tail-capture.
                    if crate::cancel::CANCEL.requested(generation) {
                        outcome = Some(TailGraceOutcome::Cancel);
                        break;
                    }
                }
                // A duplicate release or a stray latch cue during the grace
                // tail is harmless: keep waiting for quiet.
                Ok(DaemonEvent::StopRecording)
                | Ok(DaemonEvent::LatchArmed)
                | Ok(DaemonEvent::Retry) => {}
                Ok(DaemonEvent::Recover { id }) => {
                    if !self.pending_saved_recoveries.contains(&id) {
                        self.pending_saved_recoveries.push_back(id);
                    }
                }
                Ok(DaemonEvent::Shutdown) => {
                    outcome = Some(TailGraceOutcome::Shutdown);
                    break;
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
        if recovered_ms > 0 && outcome.is_none() {
            eprintln!("  tail +{recovered_ms} ms recovered");
        }
        outcome
    }

    /// The user re-pressed during the grace tail: abandon the previous
    /// utterance and start a new recording. The engine is still open and
    /// capturing, so reuse it instead of dropping it for a ~120 ms cold open:
    /// park (stop production), drain and archive the abandoned utterance's
    /// audio, then resume in place on the same stream.
    pub(super) fn repress_after_tail(
        &mut self,
        mut engine: Option<AudioCaptureEngine>,
        transform: bool,
        generation: u64,
        recovery: &mut RecoveryFinish,
    ) -> Flow {
        recovery.status = crate::history::RecoveryStatus::Interrupted;
        let mut reuse_ok = true;
        let mut resumed_flag = None;
        if let Some(e) = engine.as_mut() {
            if let Err(error) = e.park() {
                eprintln!("[audio] tail re-press pause failed: {error}; opening fresh");
                e.stop_capture();
                reuse_ok = false;
            }
            if let Some(ring) = self.ring.as_mut() {
                let tail = crate::audio::drain_session(&mut ring.consumer, &ring.recording_flag);
                crate::history::append_recovery_audio(generation, tail, crate::audio::SAMPLE_RATE);
            }
            if reuse_ok {
                match e.resume() {
                    Ok(flag) => resumed_flag = Some(flag),
                    Err(error) => {
                        eprintln!("[audio] tail re-press resume failed: {error}; opening fresh");
                        reuse_ok = false;
                    }
                }
            }
        }
        let Some(flag) = resumed_flag.filter(|_| reuse_ok) else {
            drop(engine.take());
            self.close_mic();
            self.stream.reset(None);
            self.pending_target = None;
            crate::ipc::DAEMON_RECORDING.store(false, Ordering::SeqCst);
            ui_channel::finish_utterance(generation);
            self.restart_recording = Some(transform);
            return Flow::Now;
        };
        self.engine = engine;
        if let Some(ring) = self.ring.as_mut() {
            ring.recording_flag = flag.clone();
        }
        self.arm_recording(
            Instant::now(),
            transform,
            flag,
            "  ↻    re-press during tail-capture · reused warm stream",
        );
        Flow::Now
    }

    /// An external cancel arrived during the grace tail: stop, retain the
    /// partial recording (the held streaming backup plus the grace-tail
    /// samples still in the ring) for retry, and discard everything else.
    pub(super) fn cancel_after_tail(
        &mut self,
        mut engine: Option<AudioCaptureEngine>,
        generation: u64,
        recovery: &mut RecoveryFinish,
    ) -> Flow {
        recovery.status = crate::history::RecoveryStatus::Cancelled;
        engine.as_mut().expect("tail grace runs only with a live engine").stop_capture();
        if let Some(ring) = self.ring.as_mut() {
            retain_in_flight_audio(&mut self.last_audio, &self.stream.audio, Some(&mut ring.consumer), Some(&ring.recording_flag), Some(generation));
        }
        self.ring = None;
        self.pending_target = None; // discard the orphaned press-time capture
        self.stream.reset(None);
        cues::play_cancel();
        ui_channel::set_state(UiState::Idle);
        let _unmute = crate::audio_duck::RestoreOnDrop;
        crate::cancel::CANCEL.end();
        eprintln!("⌫ cancelled · discarded");
        eprintln!();
        Flow::Now
    }
}
