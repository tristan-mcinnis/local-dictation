//! The release of one utterance: stop capture, transcribe, clean, refine,
//! deliver, and log the per-utterance timings. Each stage lives in its own
//! file; this is the order they run in and the exits between them.
use super::*;

/// The app and field the text goes to, resolved once at release.
struct ResolvedTarget {
    /// The press-time field, kept only while its app is still in front. The
    /// injector still resolves the current field at write time; this is the
    /// same-app fallback if that fresh lookup loses focus.
    captured: Option<FocusTarget>,
    app: Option<crate::injector::TargetApp>,
    pid: Option<i32>,
    focus_wait_ms: u128,
}

impl WorkerState {
    pub(super) fn on_stop(&mut self, rx: &std::sync::mpsc::Receiver<DaemonEvent>) -> Flow {
        let t_release = Instant::now();
        // A parked warm microphone is not a recording. An idle/duplicate Stop
        // must not create an empty recovery row or reuse an ended generation.
        if self.parked_until.is_some() || !crate::cancel::CANCEL.active() {
            return Flow::Now;
        }
        // Either capture path may be live: open-on-press (`engine`) or
        // always-on pre-roll (`session_active`). Nothing live: ignore.
        crate::ipc::DAEMON_RECORDING.store(false, Ordering::SeqCst);
        // A cold stop holds the stopped engine until the release ends; a
        // warm stop parks it back into `self.engine` in `end_capture`.
        let mut on_demand_engine = self.engine.take();
        let always_on_session = std::mem::take(&mut self.session_active);
        if on_demand_engine.is_none() && !always_on_session {
            return Flow::Now;
        }
        let Some(rec) = self.recording.take() else {
            return Flow::Now;
        };
        let generation = rec.generation;
        let _utterance = ui_channel::UtteranceScope(generation);
        let mut recovery = RecoveryFinish::new(generation);
        // A limit stop is never a transform or a spoken command: both write
        // into the front app, and nobody asked for that write.
        let limit_stopped = rec.limit_hit;
        let transform = rec.transform && !limit_stopped;

        let t_tail_grace = Instant::now();
        if let Some(outcome) = self.tail_grace(rx, on_demand_engine.is_some(), generation) {
            return self.settle_tail(outcome, on_demand_engine, generation, &mut recovery);
        }
        let t_tail_ms = ms(t_tail_grace.elapsed());

        // Un-mute other audio when this release exits by any path (transcribe
        // error, empty/discarded utterance, transform, or successful inject).
        // The guard's Drop runs on every return, so the restore can't be
        // forgotten on a new code path.
        let _unmute = crate::audio_duck::RestoreOnDrop;
        let Some(t_capture_stop_ms) =
            self.end_capture(&mut on_demand_engine, always_on_session, generation, &mut recovery)
        else {
            return Flow::Now;
        };
        cues::play_stop();
        if !ui_channel::set_phase(generation, UiState::Transcribing) {
            return Flow::Now;
        }
        crate::daemon_log::rotate_stdio_if_needed();
        eprintln!("⏹ stopped · held {}", secs(rec.pressed_at.elapsed()));

        let t_pipeline = Instant::now();
        let Ok(got) = self.acquire_transcript(generation, always_on_session, transform, &mut recovery) else {
            return Flow::Now;
        };
        let t_transcribe = t_pipeline.elapsed();
        let Transcript { text: transcript, uncorrected, segs_cached, final_seg_ms } = got;
        crate::history::append_recovery_stage(generation, crate::history::StageKind::Raw,
            uncorrected.as_deref().unwrap_or(&transcript), "ASR, complete");
        ui_channel::publish_preview(generation, &transcript);

        // Warm-window keeps the drained (now-empty) ring for the next reuse;
        // otherwise drop it, so cold mode returns to a fully None capture state.
        if self.parked_until.is_none() && !always_on_session {
            self.ring = None;
        }

        if transcript.trim().is_empty() {
            recovery.status = crate::history::RecoveryStatus::Completed;
            eprintln!("  skip · empty transcript");
            eprintln!();
            ui_channel::set_state(UiState::Idle);
            return Flow::Now;
        }

        // Cancel gate, second checkpoint: after ASR but before the (expensive)
        // cleanup pass. An Escape during transcription must stop here rather
        // than burn the cleanup model.
        if crate::cancel::CANCEL.consume_cancel(generation) {
            cues::play_cancel();
            ui_channel::set_state(UiState::Idle);
            eprintln!("⌫ cancelled · discarded");
            eprintln!();
            return Flow::Now;
        }

        // Transform mode (Shift + push-to-talk): the utterance is an
        // *instruction*: rewrite the current selection in place rather than
        // insert text. Handled entirely via a Cmd+C/Cmd+V clipboard round-trip
        // so it works even in AX-blind apps (Electron, terminals).
        #[cfg(feature = "cleaner")]
        if transform {
            recovery.status = handle_transform(&self.text, &transcript, generation);
            eprintln!();
            return settle_instruction();
        }

        let target = self.resolve_target();

        // Spoken prefix command (e.g. "translate to Chinese …"): a recognised
        // lead-in at the START of the utterance turns the rest into a one-shot
        // transform: translate the spoken body and inject the result at the
        // cursor. Detection is deterministic (no model on the decision path);
        // the model only runs once we know it's a command. Falls through to
        // normal dictation when nothing matches, or when running --no-cleanup.
        #[cfg(feature = "cleaner")]
        if let Some(cmd) = crate::spoken_command::parse_spoken_command(&transcript).filter(|_| !limit_stopped) {
            if self.text.has_cleaner() {
                recovery.status = handle_spoken_command(&self.text, cmd, target.captured, generation);
                eprintln!();
                return settle_instruction();
            }
            eprintln!(
                "  ✗    \"{}\" needs the cleanup engine (running with --no-cleanup) — dictating as-is",
                cmd.label
            );
        }

        // Keep the raw ASR text so the summary block can compare it to the
        // cleaned output: the only way to tell, after the fact, whether a short
        // result came from ASR or from the cleanup model dropping content.
        let raw_transcript = transcript;
        let cleanup_target = CleanupTarget::of(target.app.as_ref(), target.captured.as_ref());
        let Ok(cleaned) = clean(&raw_transcript, &self.text, &cleanup_target, generation) else {
            return Flow::Now;
        };
        let t_cleanup_ms = cleaned.llm_ms;
        if cleaned.error.is_some() {
            recovery.error = cleaned.error;
        }
        if cleaned.source == CleanupSource::Model {
            crate::history::append_recovery_stage(generation, crate::history::StageKind::Cleaned, &cleaned.text, "Accepted model cleanup");
        }
        ui_channel::publish_preview(generation, &cleaned.text);
        // The write gate (cancel commit and fresh focus check) is claimed by
        // the injector's write authorizer immediately before the first side
        // effect; see `deliver`.
        let used_fresh_capture = target.captured.is_none();
        let to = Destination {
            prefs: self.text.result,
            generation,
            // A limit stop is held by decision: known "do not write".
            hold: limit_stopped.then_some(crate::last_dictation::KeptCause::LimitReached),
            target: target.captured,
            target_pid: target.pid,
        };
        recovery.status = settle(&self.text.refiner, &cleaned.text, to, |written| {
            crate::latency::record(crate::latency::Snapshot {
                release_ms: ms(t_release.elapsed()) as u64,
                tail_ms: t_tail_ms as u64,
                capture_stop_ms: t_capture_stop_ms as u64,
                asr_ms: ms(t_transcribe) as u64,
                cleanup_ms: t_cleanup_ms as u64,
                focus_ms: target.focus_wait_ms as u64,
                insertion_ms: written.write_ms as u64,
                inserted: written.delivery == Delivery::Delivered,
            });
            self.log_summary(
                self.config.capture.streaming_asr.then_some((segs_cached, final_seg_ms)),
                &raw_transcript,
                written.text,
                target.pid,
                used_fresh_capture,
                ms(t_transcribe),
                t_cleanup_ms,
                written.write_ms,
            );
        });
        eprintln!();
        // The utterance settled (injected, held, cancelled, or failed): close
        // the cancel window so the gate reads idle and an idle Escape is never
        // swallowed.
        crate::cancel::CANCEL.end();
        if ui_channel::state().is_processing() {
            ui_channel::set_state(UiState::Idle);
        }
        Flow::Settle
    }

    /// The grace tail was cut short: restart on a re-press, discard on a
    /// cancel, or leave the loop on a quit.
    fn settle_tail(
        &mut self,
        outcome: TailGraceOutcome,
        mut engine: Option<AudioCaptureEngine>,
        generation: u64,
        recovery: &mut RecoveryFinish,
    ) -> Flow {
        match outcome {
            TailGraceOutcome::RePress { transform } => {
                self.repress_after_tail(engine, transform, generation, recovery)
            }
            TailGraceOutcome::Cancel => self.cancel_after_tail(engine, generation, recovery),
            TailGraceOutcome::Shutdown => {
                recovery.status = crate::history::RecoveryStatus::Interrupted;
                if let Some(e) = engine.as_mut() {
                    e.stop_capture();
                }
                if let Some(ring) = self.ring.as_mut() {
                    retain_in_flight_audio(&mut self.last_audio, &self.stream.audio, Some(&mut ring.consumer), Some(&ring.recording_flag), Some(generation));
                }
                recovery.finish_now();
                crate::history::flush_recovery(Duration::from_secs(2));
                eprintln!("[daemon] shutdown · releasing models before exit");
                Flow::Exit
            }
        }
    }

    /// End capture for whichever path is live. Open-on-press either parks the
    /// stream (warm-window reuse) or drops it (releasing the mic); always-on
    /// just ends the session, the warm stream keeps running. Then the first
    /// out-of-band cancel checkpoint: an Escape / IPC cancel that arrived
    /// during capture or the tail grace discards now, no transcribe, no
    /// inject, but RETAINS the recording's audio for retry (see
    /// `crate::cancel`). `None` means the release ends here. The value is the
    /// capture-stop time in ms for the latency line.
    fn end_capture(
        &mut self,
        engine: &mut Option<AudioCaptureEngine>,
        always_on_session: bool,
        generation: u64,
        recovery: &mut RecoveryFinish,
    ) -> Option<u128> {
        let t_capture_stop = Instant::now();
        if let Some(e) = engine.as_mut() {
            if self.warm_window_active() {
                if let Err(error) = e.park() {
                    eprintln!("[audio] capture pause failed: {error}; resource will be rebuilt");
                    e.stop_capture();
                }
                self.engine = engine.take();
                self.parked_until = Some(
                    Instant::now() + Duration::from_millis(self.config.capture.warm_window_ms as u64),
                );
            } else {
                e.stop_capture();
            }
        }
        if always_on_session {
            if let Some((ao, _)) = self.always.as_ref() {
                ao.end_session();
            }
        }
        let t_capture_stop_ms = ms(t_capture_stop.elapsed());
        // The audio has not been drained into `last_audio` yet at this
        // checkpoint, so drain it from the active capture source first.
        if crate::cancel::CANCEL.consume_cancel(generation) {
            recovery.status = crate::history::RecoveryStatus::Cancelled;
            if always_on_session {
                if let Some((ao, cons)) = self.always.as_mut() {
                    let recflag = ao.recording_flag();
                    retain_in_flight_audio(&mut self.last_audio, &self.stream.audio, Some(cons), Some(&recflag), Some(generation));
                }
            } else if let Some(ring) = self.ring.as_mut() {
                retain_in_flight_audio(&mut self.last_audio, &self.stream.audio, Some(&mut ring.consumer), Some(&ring.recording_flag), Some(generation));
            }
            cues::play_cancel();
            ui_channel::set_state(UiState::Idle);
            crate::cancel::CANCEL.end();
            eprintln!("⌫ cancelled · discarded");
            eprintln!();
            return None;
        }
        Some(t_capture_stop_ms)
    }

    /// Receive the focus target captured at key-press. It ran for the whole
    /// hold, so it's almost always ready by here and the join is a no-op. A
    /// very short hold just blocks on the join; the cost is the same as the
    /// old release-time spawn, and usually strictly less because the AX wake
    /// happened during speech, not after. The target app is needed *before*
    /// cleanup to skip the model when the destination is a terminal.
    fn resolve_target(&mut self) -> ResolvedTarget {
        let t_focus_join = Instant::now();
        let (press_app, press_target): (Option<crate::injector::TargetApp>, Option<FocusTarget>) =
            match self.pending_target.take().and_then(|rx| rx.recv().ok()) {
                Some((app, focus)) => (app, focus.ok()),
                None => (None, None),
            };
        let focus_wait_ms = ms(t_focus_join.elapsed());
        // Stale-target fallback: if the press-time capture failed, or the
        // frontmost app changed since the key was pressed (a long hold or
        // hands-free can out-last the user's focus), the retained element
        // belongs to a background app and writing to it would be wrong. Trust
        // it when AX didn't disclose a PID (nothing to verify) or when the
        // OS-frontmost app still matches. Otherwise re-resolve fresh at inject:
        // correctness over speed.
        let captured = press_target.filter(crate::injector::focus_target_still_current);
        // One app identity for every per-app decision (terminal raw mode,
        // tone, the log line): the press-time app while it is still in front,
        // else the app the text will actually go to. Resolved once here;
        // nothing re-reads the frontmost app later.
        let app = crate::injector::resolve_target_app(
            press_app,
            crate::injector::frontmost_pid(),
            crate::injector::bundle_id_for_pid,
        );
        let pid = app.as_ref().map(|a| a.pid);
        if captured.is_some() {
            eprintln!("  ⌖    press-time focus ready · fresh check at write");
        } else {
            eprintln!("  ⌖    press-time focus unavailable/stale · fresh check at write");
        }
        ResolvedTarget { captured, app, pid, focus_wait_ms }
    }

    /// The per-utterance summary block: timings, the target app, the model,
    /// and the raw transcript whenever cleanup changed it, so ASR-vs-cleanup
    /// is always attributable after the fact.
    #[allow(clippy::too_many_arguments)]
    fn log_summary(
        &self,
        streaming: Option<(usize, u128)>,
        raw_transcript: &str,
        final_text: &str,
        target_pid: Option<i32>,
        used_fresh_capture: bool,
        asr_ms: u128,
        cleanup_ms: u128,
        inject_ms: u128,
    ) {
        eprintln!("  xcr  {asr_ms:>4} ms · cln {cleanup_ms:>4} ms · inj {inject_ms:>4} ms");
        if let Some((segs_cached, final_seg_ms)) = streaming {
            eprintln!("  segs={segs_cached} cached · finalseg={final_seg_ms} ms");
        }
        if let Some(pid) = target_pid {
            eprintln!("  app  {} (pid {})", app_name(pid), pid);
        } else if used_fresh_capture {
            eprintln!("  app  <fresh capture · target unknown>");
        }
        // Which cleanup model produced this, always visible, so the log is
        // self-describing when comparing models.
        #[cfg(feature = "cleaner")]
        if self.text.has_cleaner() {
            let m = std::path::Path::new(&self.config.llm_model_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("?");
            eprintln!("  llm  {m}");
        }
        // Raw-to-cleaned visibility. The whole-buffer path otherwise logs only
        // the final text, so a SAME-LENGTH intent rewrite (raw "did you do
        // this" to "did you actually fix everything") is invisible and the
        // shrink watchdog below never fires on it.
        if self.text.has_cleaner() && raw_transcript.trim() != final_text.trim() {
            eprintln!("  ⟫ raw · {}", crate::daemon_log::text(raw_transcript));
        }
        // Truncation watchdog: if the cleaned text is dramatically shorter
        // than the raw transcript (>40% dropped), the cleanup model likely gave
        // up early or summarized. Filler removal alone rarely exceeds 40%, so
        // this fires on real content loss, not noise.
        let raw_chars = raw_transcript.chars().count();
        let final_chars = final_text.chars().count();
        if raw_chars > 40 && final_chars * 5 < raw_chars * 3 {
            eprintln!(
                "  ⚠    cleanup shrank text {raw_chars} → {final_chars} chars · raw: {}",
                crate::daemon_log::text(raw_transcript)
            );
        }
    }
}

/// A transform or spoken command settled its own result. Leave a kept-result
/// pill on screen; only a still-working state is reset.
#[cfg(feature = "cleaner")]
fn settle_instruction() -> Flow {
    if ui_channel::state().is_processing() {
        ui_channel::set_state(UiState::Idle);
    }
    Flow::Now
}
