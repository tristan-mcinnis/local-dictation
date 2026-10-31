//! Worker thread: the event loop over the capture state, the warm models and
//! the in-flight utterance. Each event is one method here; the stages of a
//! release have their own files (tail, transcribe, cleanup, deliver, transform).
use super::*;

/// How long a failure pill stays up.
pub(super) const FAILURE_PILL: Duration = Duration::from_millis(3500);

/// The open-on-press ring: the consumer half and the flag the audio callback
/// checks. Set with the engine on a cold open and kept while it is parked.
pub(super) struct Ring {
    pub consumer: crate::audio::HeapAudioConsumer,
    pub recording_flag: Arc<AtomicBool>,
}

/// The utterance being recorded, from the press to the end of its release.
pub(super) struct Recording {
    /// Generation for the out-of-band cancel gate (`crate::cancel`) and the
    /// pill; read by the release pipeline's cancel checkpoints.
    pub generation: u64,
    pub pressed_at: Instant,
    /// Shift was co-held: the utterance is an instruction to rewrite the
    /// current selection, not text to insert.
    pub transform: bool,
    /// The recording limit stopped it, not a key. Nobody may be at the
    /// keyboard, so the text is kept and copied, never pasted.
    pub limit_hit: bool,
}

/// The incremental-ASR state of one utterance, independent of the cleaner
/// feature: transcribe_pcm + SegmentStream + corrections need no LLM. Reset
/// at every start so a reused stream never carries stale segments or a stale
/// poison flag into the next utterance.
#[derive(Default)]
pub(super) struct Streaming {
    /// The VAD segmenter, armed only when streaming ASR is on.
    pub segmenter: Option<crate::vad::SegmentStream>,
    /// Corrected text of each completed (VAD-pause-bounded) segment.
    pub segs: Vec<String>,
    /// The exact ASR words; personal corrections never rewrite the
    /// recoverable Raw layer.
    pub raw_segs: Vec<String>,
    /// Every drained sample, for the whole-buffer fallback on a poisoned
    /// stream, a transform or an empty result: never lose audio.
    pub audio: Vec<f32>,
    /// A segment transcribe failed: stop segmenting and transcribe the whole
    /// buffer as one batch at release.
    pub poisoned: bool,
}

impl Streaming {
    pub(super) fn reset(&mut self, segmenter: Option<crate::vad::SegmentStream>) {
        self.segmenter = segmenter;
        self.segs.clear();
        self.raw_segs.clear();
        self.audio.clear();
        self.poisoned = false;
    }
}

/// Everything the worker thread owns across events.
pub(super) struct WorkerState {
    pub config: DaemonConfig,
    pub asr: LocalInferenceWorker,
    /// The cleanup models, the refiner and the result preferences.
    pub text: Pipeline,
    /// The always-on (pre-roll) mic, warm for the daemon's life. `None` is the
    /// proven open-on-press path (default).
    pub always: Option<(crate::audio::AlwaysOnCapture, crate::audio::HeapAudioConsumer)>,
    /// True between a pre-roll StartRecording and its StopRecording (the
    /// always-on analogue of `engine.is_some()` for the open-on-press path).
    pub session_active: bool,
    /// The open-on-press engine: recording, or parked for a warm reuse.
    pub engine: Option<AudioCaptureEngine>,
    pub ring: Option<Ring>,
    /// When a parked engine expires (mic released). `Some` only while the
    /// engine is parked: idle but open, awaiting a cheap reuse.
    pub parked_until: Option<Instant>,
    pub recording: Option<Recording>,
    /// Focus capture at key-press, joined at release. `None` between
    /// utterances and after a cancel.
    pub pending_target: Option<std::sync::mpsc::Receiver<PressCapture>>,
    pub stream: Streaming,
    /// Incremental room-noise-floor estimate, fed per-frame RMS during capture
    /// so the tail-capture engage check never re-sorts the whole utterance on
    /// the release-to-inject critical path.
    pub noise_floor: crate::audio::NoiseFloorEstimator,
    /// The raw PCM of the last transcribed utterance, so Retry can re-run the
    /// pipeline on the same audio. Empty until the first dictation.
    pub last_audio: Vec<f32>,
    /// Saved-audio recoveries that arrived while a recording was live.
    pub pending_saved_recoveries: std::collections::VecDeque<i64>,
    /// Retry a failed retained-stream resume through the ordinary cold-open
    /// path, preserving queued stop/cancel events for this same press.
    pub restart_recording: Option<bool>,
}

/// What the loop does after an event handler returns.
pub(super) enum Flow {
    /// Pause 10 ms so an audio cue starts before a fast double-tap re-enters
    /// recording.
    Settle,
    /// Pick up the next event straight away.
    Now,
    /// Quit: drop every model while Metal is still alive.
    Exit,
}

pub(super) fn worker_loop(
    rx: std::sync::mpsc::Receiver<DaemonEvent>,
    config: DaemonConfig,
) -> eyre::Result<()> {
    let mut state = WorkerState::boot(config)?;
    while let Some(event) = state.next_event(&rx) {
        let flow = match event {
            DaemonEvent::LatchArmed => state.on_latch_armed(),
            DaemonEvent::Shutdown => state.on_shutdown(),
            DaemonEvent::Recover { id } => state.on_recover(id),
            DaemonEvent::Retry => state.on_retry(),
            DaemonEvent::CancelRecording => state.on_cancel(),
            DaemonEvent::StartRecording { transform } => state.on_start(transform),
            DaemonEvent::StopRecording => state.on_stop(&rx),
        };
        match flow {
            Flow::Settle => std::thread::sleep(Duration::from_millis(10)),
            Flow::Now => {}
            Flow::Exit => return Ok(()),
        }
    }
    crate::history::flush_recovery(Duration::from_secs(2));
    Ok(())
}

impl WorkerState {
    /// The next event, or `None` when the channel is gone. While recording,
    /// wake every 120 ms to checkpoint captured PCM and, with streaming ASR,
    /// transcribe sentences closed by a VAD pause. recv_timeout keeps
    /// everything on this one thread, which already owns the ASR worker.
    fn next_event(&mut self, rx: &std::sync::mpsc::Receiver<DaemonEvent>) -> Option<DaemonEvent> {
        loop {
            if let Some(transform) = self.restart_recording.take() {
                return Some(DaemonEvent::StartRecording { transform });
            }
            if !self.session_active && (self.engine.is_none() || self.parked_until.is_some()) {
                if let Some(id) = self.pending_saved_recoveries.pop_front() {
                    return Some(DaemonEvent::Recover { id });
                }
            }
            if self.engine.is_none() && !self.session_active {
                return rx.recv().ok();
            }
            match rx.recv_timeout(Duration::from_millis(120)) {
                Ok(event) => return Some(event),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return None,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(event) = self.tick() {
                        return Some(event);
                    }
                }
            }
        }
    }

    /// One 120 ms tick while a mic is open. Ticks checkpoint every capture
    /// mode, including non-streaming and pre-roll. A parked engine only checks
    /// its expiry; its idle microphone audio never reaches the archive.
    fn tick(&mut self) -> Option<DaemonEvent> {
        let limit = self.config.capture.max_recording_minutes;
        if self.parked_until.is_none() && crate::cancel::CANCEL.active() {
            if let Some(rec) = self.recording.as_mut() {
                if recording_limit_reached(rec.pressed_at.elapsed(), limit) {
                    // A lost key-up (Secure Input, a disabled tap) or a
                    // forgotten hands-free session: stop here instead of
                    // recording without end.
                    eprintln!("[limit] recording reached {limit} min · stopping (text kept, not pasted)");
                    rec.limit_hit = true;
                    PTT_RESYNC.store(true, Ordering::SeqCst);
                    return Some(DaemonEvent::StopRecording);
                }
            }
        }
        let whisper = self.config.capture.whisper_mode;
        if let Some(deadline) = self.parked_until {
            // Warm-window expiry: close the parked mic once idle past the
            // window, so we don't hold the input device (and its orange
            // indicator) forever between dictations.
            if Instant::now() >= deadline {
                self.close_mic();
                eprintln!("[audio] warm-window expired · mic closed");
            }
        } else if self.session_active {
            if let (Some((_, cons)), Some(rec)) = (self.always.as_mut(), self.recording.as_ref()) {
                checkpoint_capture(cons, &mut self.stream.audio, rec.generation);
            }
        } else if self.config.capture.streaming_asr {
            if let (Some(ring), Some(rec)) = (self.ring.as_mut(), self.recording.as_ref()) {
                stream_process_asr(
                    &mut self.asr,
                    &self.text.refiner,
                    &mut self.stream,
                    &mut ring.consumer,
                    &mut self.noise_floor,
                    StreamPass { final_flush: false, whisper, generation: rec.generation },
                );
            }
        } else if let (Some(ring), Some(rec)) = (self.ring.as_mut(), self.recording.as_ref()) {
            checkpoint_capture(&mut ring.consumer, &mut self.stream.audio, rec.generation);
        }
        None
    }

    /// Warm-window reuse applies to the open-on-press path only; the
    /// always-on stream is already warm for the daemon's life.
    pub(super) fn warm_window_active(&self) -> bool {
        self.config.capture.warm_window_ms > 0 && self.config.capture.preroll_ms == 0
    }

    /// Release the open-on-press mic, parked or not.
    pub(super) fn close_mic(&mut self) {
        self.engine = None;
        self.ring = None;
        self.parked_until = None;
    }

    /// A fresh VAD segmenter when streaming ASR is on.
    pub(super) fn streaming_segmenter(&self) -> Option<crate::vad::SegmentStream> {
        self.config.capture.streaming_asr.then(|| {
            crate::vad::SegmentStream::new(
                crate::audio::SAMPLE_RATE,
                vad_config(self.config.capture.whisper_mode),
            )
        })
    }

    /// Hands-free engaged mid-hold: the mic keeps running once the keys are
    /// released. Audible feedback and a log line.
    fn on_latch_armed(&mut self) -> Flow {
        cues::play_latch();
        eprintln!("⤓ hands-free · release keys, keep talking · tap PTT to stop");
        Flow::Settle
    }

    /// Quit: return so every local here, the cleanup engine with its
    /// LlamaContext and the ASR worker, is dropped while Metal is still alive.
    /// The spawning thread then calls `shutdown::mark_released`, which
    /// unblocks the delegate that is holding `exit()` open.
    fn on_shutdown(&mut self) -> Flow {
        if let Some(rec) = self.recording.take().filter(|_| crate::cancel::CANCEL.active()) {
            if let Some(e) = self.engine.as_mut() {
                e.stop_capture();
            }
            if self.session_active {
                if let Some((ao, cons)) = self.always.as_mut() {
                    ao.end_session();
                    let flag = ao.recording_flag();
                    retain_in_flight_audio(&mut self.last_audio, &self.stream.audio, Some(cons), Some(&flag), Some(rec.generation));
                }
            } else if let Some(ring) = self.ring.as_mut() {
                retain_in_flight_audio(&mut self.last_audio, &self.stream.audio, Some(&mut ring.consumer), Some(&ring.recording_flag), Some(rec.generation));
            }
            crate::history::finish_recovery(rec.generation, crate::history::RecoveryStatus::Interrupted);
            ui_channel::finish_utterance(rec.generation);
        }
        crate::history::flush_recovery(Duration::from_secs(2));
        eprintln!("[daemon] shutdown · releasing models before exit");
        Flow::Exit
    }

    /// Re-run the pipeline on the retained "last recording" audio
    /// (control-socket "retry" / menu-bar item). Retry obeys the SAME cancel
    /// gate and focus/no-blind-paste guard as a live utterance: it begins a
    /// fresh generation, marks Processing (so Escape is recognised), and
    /// claims the write commit at the last moment via the injector's write
    /// authorizer. The retained audio is NOT cleared here; a cancelled retry
    /// keeps it until the next recording start or app quit.
    fn on_retry(&mut self) -> Flow {
        if self.last_audio.is_empty() {
            eprintln!("  ↻    retry: nothing to retry yet");
            cues::play_error();
            return Flow::Now;
        }
        let retry_gen = ui_channel::begin_utterance(UiState::Transcribing, self.config.capture.streaming_asr);
        let _utterance = ui_channel::UtteranceScope(retry_gen);
        retry_last(&self.text, &mut self.asr, &self.last_audio, retry_gen);
        ui_channel::finish_utterance(retry_gen);
        Flow::Settle
    }
}
