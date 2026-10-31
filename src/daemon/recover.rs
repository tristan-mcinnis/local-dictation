//! Saved-audio recovery: reprocess a History recording and copy the text.
//! Copy-only by design: it never activates an app, replays spoken actions or
//! injects into the current app, and it never interrupts a live capture.
use super::*;

impl WorkerState {
    pub(super) fn on_recover(&mut self, id: i64) -> Flow {
        // Recovery is explicitly copy-only and never interrupts capture.
        if self.session_active || (self.engine.is_some() && self.parked_until.is_none()) {
            if !self.pending_saved_recoveries.contains(&id) {
                self.pending_saved_recoveries.push_back(id);
            }
            eprintln!("[recovery] queued until current recording settles");
            return Flow::Now;
        }
        let generation = ui_channel::begin_utterance(UiState::Transcribing, self.config.capture.streaming_asr);
        let _utterance = ui_channel::UtteranceScope(generation);
        crate::history::resume_recovery(generation, id);
        let mut recovery = RecoveryFinish::new(generation);
        let (audio, rate) = match crate::history::read_recovery_audio(id) {
            Ok(Some(audio)) => audio,
            Ok(None) => {
                recovery.error = Some("Saved audio is no longer available".into());
                eprintln!("[recovery] saved audio is no longer available for record {id}");
                ui_channel::show_transient_for(generation, UiState::Error, Duration::from_millis(900));
                return Flow::Now;
            }
            Err(error) => {
                recovery.error = Some(format!("Could not read saved audio: {error}"));
                eprintln!("[recovery] could not read record {id}: {error}");
                ui_channel::show_transient_for(generation, UiState::Error, Duration::from_millis(900));
                return Flow::Now;
            }
        };
        if rate != crate::audio::SAMPLE_RATE {
            recovery.error = Some(format!("Unsupported saved sample rate {rate}"));
            eprintln!("[recovery] unsupported saved sample rate {rate}");
            return Flow::Now;
        }
        let text = &self.text;
        let mut cleanup = |raw: &str| match clean(raw, text, &CleanupTarget::NONE, generation) {
            Ok(cleaned) => crate::reprocess::Cleanup {
                text: cleaned.text,
                from_model: cleaned.source == CleanupSource::Model,
            },
            // Cancelled while cleaning: reprocess sees the cancel next and
            // never copies, so any faithful text will do here.
            Err(Superseded) => crate::reprocess::Cleanup {
                text: crate::text_polish::fix_speech_mechanics(raw),
                from_model: false,
            },
        };
        let cleanup_fn = text
            .has_cleaner()
            .then_some(&mut cleanup as &mut dyn FnMut(&str) -> crate::reprocess::Cleanup);
        let asr = &mut self.asr;
        let outcome = crate::reprocess::to_clipboard(
            &crate::cancel::CANCEL, generation, &audio,
            |audio| asr.transcribe_pcm(audio), cleanup_fn,
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
        Flow::Settle
    }
}

/// Finishes the History record of one capture when it goes out of scope, so
/// every exit path of a release settles it, with a cancel winning over the
/// status the pipeline reached.
pub(super) struct RecoveryFinish {
    generation: u64,
    pub status: crate::history::RecoveryStatus,
    pub error: Option<String>,
    finished: bool,
}
impl RecoveryFinish {
    pub(super) fn new(generation: u64) -> Self {
        Self { generation, status: crate::history::RecoveryStatus::Failed, error: None, finished: false }
    }

    pub(super) fn finish_now(&mut self) {
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
