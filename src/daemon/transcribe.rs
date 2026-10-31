//! Transcription at release: the whole-buffer paths (pre-roll, cold) and the
//! streaming path that joins the sentences transcribed during the hold.
use super::*;

/// Say that the transcription failed, and where the words are. Before this the
/// only signal was a cue played while `audio_duck` still held the system
/// muted, so a failed dictation looked exactly like one that did nothing.
fn show_transcribe_failure(generation: u64) {
    ui_channel::show_transient_for(generation, UiState::Error, FAILURE_PILL);
}

/// The transcript of one release and the streaming bookkeeping for its log.
pub(super) struct Transcript {
    pub text: String,
    /// The exact ASR words when the streaming path joined corrected segments;
    /// the History Raw stage keeps these, never the corrected text.
    pub uncorrected: Option<String>,
    pub segs_cached: usize,
    pub final_seg_ms: u128,
}

impl Transcript {
    fn whole(text: String) -> Self {
        Self { text, uncorrected: None, segs_cached: 0, final_seg_ms: 0 }
    }
}

/// The ASR call failed; the user has been told and the audio is retained.
pub(super) struct TranscribeFailed;

fn transcribe_failed(err: eyre::Report, generation: u64, recovery: &mut RecoveryFinish) -> TranscribeFailed {
    recovery.error = Some(format!("Transcription failed: {err}"));
    eprintln!("[err]  transcribe failed: {err:?}");
    cues::play_error();
    show_transcribe_failure(generation);
    eprintln!();
    TranscribeFailed
}

impl WorkerState {
    /// Acquire the transcript. Streaming ASR assembles it from the sentences
    /// transcribed (NOT cleaned) during the hold plus a final open segment;
    /// everything else (pre-roll, a poisoned stream, a transform) takes the
    /// proven whole-buffer path, byte for byte unchanged. An empty text means
    /// nothing above the noise floor was worth transcribing.
    pub(super) fn acquire_transcript(
        &mut self,
        generation: u64,
        always_on_session: bool,
        transform: bool,
        recovery: &mut RecoveryFinish,
    ) -> Result<Transcript, TranscribeFailed> {
        let whisper = self.config.capture.whisper_mode;
        // Always-on pre-roll: drain the warm stream's queued audio (the
        // pre-roll lookback the callback flushed at press + everything since)
        // and transcribe the whole buffer.
        if always_on_session {
            if let Some((ao, cons)) = self.always.as_mut() {
                let recflag = ao.recording_flag();
                let buf = complete_capture(cons, &recflag, &mut self.stream.audio, generation);
                let Some(keep) = guarded_transcribe_len(&buf, &self.noise_floor, whisper) else {
                    eprintln!("  ◌    no speech above noise floor · nothing to transcribe");
                    return Ok(Transcript::whole(String::new()));
                };
                self.last_audio = buf[..keep].to_vec();
                return match self.asr.transcribe_pcm(&buf[..keep]) {
                    Ok(t) => Ok(Transcript::whole(t)),
                    Err(err) => Err(transcribe_failed(err, generation, recovery)),
                };
            }
        }

        // The open-on-press paths own a per-utterance ring; borrow it (rather
        // than taking it) so a warm-window reuse can keep the read handle for
        // the next dictation.
        let ring = self.ring.as_mut().expect("an open-on-press recording has its ring");

        // Incremental streaming ASR: flush the final open segment, then join
        // the cached segments into the transcript. Cleanup is NOT done
        // per-segment; the joined text feeds the whole-utterance release
        // pipeline unchanged. Falls back to a single whole-buffer transcribe of
        // everything captured when the streamed result can't be trusted: a
        // segment failed (poisoned), the utterance is a transform instruction
        // (keep the proven path so commands / transform never see a VAD-split
        // rerender), or nothing was cached (e.g. pure silence). Audio loss is
        // impossible: `stream.audio` holds every drained sample.
        if self.config.capture.streaming_asr && self.stream.segmenter.is_some() {
            let final_seg_ms = stream_process_asr(
                &mut self.asr,
                &self.text.refiner,
                &mut self.stream,
                &mut ring.consumer,
                &mut self.noise_floor,
                true,
                whisper,
                generation,
            );
            let segs_cached = self.stream.segs.len();
            // Boundary-repairing join: a pause before the last word makes the
            // ASR emit "…instant." + "connection."; see `join_stream_segments`
            // for the lowercase-start repair.
            let joined = crate::text_polish::join_stream_segments(&self.stream.segs);
            let fallback = self.stream.poisoned || transform || self.stream.segs.is_empty();
            // Take the backup buffer so the fallback transcribes exactly what
            // was captured (and frees it for the next utterance).
            let whole = std::mem::take(&mut self.stream.audio);
            let mut uncorrected = None;
            let result = if fallback {
                eprintln!(
                    "  ⟫ streaming fallback · whole-buffer transcribe ({} samples)",
                    whole.len()
                );
                match guarded_transcribe_len(&whole, &self.noise_floor, whisper) {
                    Some(keep) => {
                        self.last_audio = whole[..keep].to_vec();
                        self.asr.transcribe_pcm(&whole[..keep])
                    }
                    None => {
                        eprintln!("  ◌    no speech above noise floor · nothing to transcribe");
                        Ok(String::new())
                    }
                }
            } else {
                eprintln!("  ⟫ streamed {} segment(s) during hold", self.stream.segs.len());
                self.last_audio = whole.clone();
                uncorrected = Some(self.stream.raw_segs.join(" "));
                Ok(joined)
            };
            self.stream.segmenter = None;
            return match result {
                Ok(text) => Ok(Transcript { text, uncorrected, segs_cached, final_seg_ms }),
                Err(err) => Err(transcribe_failed(err, generation, recovery)),
            };
        }

        // Non-streaming open-on-press path: drain the ring synchronously then
        // transcribe, mirroring the always-on path.
        let buf = complete_capture(&mut ring.consumer, &ring.recording_flag, &mut self.stream.audio, generation);
        let Some(keep) = guarded_transcribe_len(&buf, &self.noise_floor, whisper) else {
            eprintln!("  ◌    no speech above noise floor · nothing to transcribe");
            return Ok(Transcript::whole(String::new()));
        };
        self.last_audio = buf[..keep].to_vec();
        match self.asr.transcribe_pcm(&buf[..keep]) {
            Ok(t) => Ok(Transcript::whole(t)),
            Err(err) => Err(transcribe_failed(err, generation, recovery)),
        }
    }
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
    stream: &mut Streaming,
    consumer: &mut crate::audio::HeapAudioConsumer,
    noise_floor: &mut crate::audio::NoiseFloorEstimator,
    final_flush: bool,
    whisper: bool,
    generation: u64,
) -> u128 {
    let Streaming { segmenter, segs: segs_acc, raw_segs: raw_segs_acc, audio: audio_backup, poisoned } = stream;
    let Some(st) = segmenter.as_mut() else {
        return 0;
    };
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
}
