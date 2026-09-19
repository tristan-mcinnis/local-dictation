//! The worker → UI cross-thread channel.
//!
//! The push-to-talk daemon runs the inference pipeline on a worker thread and
//! the menu-bar UI on the main thread (AppKit's run loop). Three things flow
//! between them, all via process-wide statics so neither side needs a handle
//! to the other:
//!
//!   * **state** — the current `UiState` (idle / recording / processing /
//!     transcribing / cleaning / pasting / success / error),
//!     which drives the status-item icon and shows/hides the floating pill.
//!   * **live preview** — bounded sentence-based ASR text for the current
//!     generation only, cleared on cancellation and never persisted here.
//!   * **last dictation** — the most recent injected text, for the menu's
//!     "Copy last dictation" item and "Last: …" preview.
//!
//! Keeping all three behind one module means the worker↔UI contract is
//! defined in a single place: the worker side (`set_state`, `set_last_dictation`,
//! `push_level`) and the reader side (`state`, `last_dictation`,
//! `recent_levels`) are the whole interface. `audio.rs`
//! stays a pure capture module that just calls `push_level`; `menubar.rs` reads through here rather
//! than owning the statics itself.
//!
//! This module is plain `std` (atomics, a mutex, a `VecDeque`) so it compiles
//! in every feature configuration, including the default lib build that has
//! neither the daemon nor the menu bar.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// What the UI should be showing. Broadcast by the worker thread; the menu-bar
/// poll timer reads it and swaps the status icon / pill visibility.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum UiState {
    Idle = 0,
    Recording = 1,
    Processing = 2,
    /// A short-lived completion cue. The daemon may immediately return to
    /// `Idle`; the pill still knows how to render this state for callers that
    /// want an explicit hand-off.
    Success = 3,
    /// A recoverable failure cue. This is deliberately separate from
    /// `Processing` so the UI can provide a redundant text/icon signal.
    Error = 4,
    /// The transcript finished but there was no editable field focused, so
    /// nothing was written. Distinct from `Error`: it is not a failure — the
    /// text is held recoverable (menu "Copy last dictation") and the pill says
    /// "No text field selected" instead of pasting blindly.
    NoEditableField = 5,
    /// ASR is finishing the captured audio after release (or retry).
    Transcribing = 6,
    /// The cleanup/transform model is actually running.
    Cleaning = 7,
    /// The injector has claimed the write and is delivering the text.
    Pasting = 8,
    /// Saved-audio recovery copied text; no target was activated or pasted into.
    Copied = 9,
    /// Transcription worked but the write into the app failed. Not `Error`
    /// (whose pill says "Couldn’t transcribe"): the words exist, are kept as
    /// the last dictation and in History, and the pill says where to get them.
    PasteFailed = 10,
    /// Held (no text field), and the words were put on the clipboard.
    HeldCopied = 11,
    /// The write failed, and the words were put on the clipboard.
    PasteFailedCopied = 12,
    /// The recording hit the maximum length and was stopped. The text is kept,
    /// never pasted: nobody may still be at the keyboard.
    LimitReached = 13,
    /// As `LimitReached`, with the words on the clipboard.
    LimitReachedCopied = 14,
    /// "Copy last dictation" was asked for and there is nothing to copy.
    NothingToCopy = 15,
}

impl UiState {
    pub fn is_processing(self) -> bool {
        matches!(
            self,
            Self::Processing | Self::Transcribing | Self::Cleaning | Self::Pasting
        )
    }

    pub fn is_cancellable(self) -> bool {
        matches!(
            self,
            Self::Recording | Self::Processing | Self::Transcribing | Self::Cleaning
        )
    }

    /// An utterance owns the pill. The cancel gate, not this display state,
    /// decides whether a cancel can still beat the write commit.
    pub fn is_active(self) -> bool {
        self == Self::Recording || self.is_processing()
    }

    /// The transcript exists but was not inserted: the pill says where the
    /// words are, stays about four seconds, and the next key press dismisses it.
    pub fn is_kept_result(self) -> bool {
        matches!(
            self,
            Self::NoEditableField
                | Self::PasteFailed
                | Self::HeldCopied
                | Self::PasteFailedCopied
                | Self::LimitReached
                | Self::LimitReachedCopied
        )
    }

    /// Reconstruct from the raw atomic byte. Unknown values map to `Idle`.
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => UiState::Recording,
            2 => UiState::Processing,
            3 => UiState::Success,
            4 => UiState::Error,
            5 => UiState::NoEditableField,
            6 => UiState::Transcribing,
            7 => UiState::Cleaning,
            8 => UiState::Pasting,
            9 => UiState::Copied,
            10 => UiState::PasteFailed,
            11 => UiState::HeldCopied,
            12 => UiState::PasteFailedCopied,
            13 => UiState::LimitReached,
            14 => UiState::LimitReachedCopied,
            15 => UiState::NothingToCopy,
            _ => UiState::Idle,
        }
    }
}

/// How many recent RMS samples we keep for the waveform display. Matches the
/// bounded level history; the pill may project only the newest samples.
pub const WAVEFORM_BARS: usize = 14;

/// Limit in-flight text independently of the retained final dictation.
pub const LIVE_PREVIEW_CHARACTERS: usize = 320;

/// One mutex protects the presentation and its generation/revision. Audio
/// levels keep their separate ring: the real-time callback never takes this lock.
static PRESENTATION: Mutex<Presentation> = Mutex::new(Presentation::new());

struct Presentation {
    state: UiState,
    generation: Option<u64>,
    preview_enabled: bool,
    capture: Option<Arc<AtomicBool>>,
    preview: String,
    revision: u64,
    /// When a kept-result pill went up; `None` in every other state.
    kept_since: Option<std::time::Instant>,
}

impl Presentation {
    const fn new() -> Self {
        Self {
            state: UiState::Idle,
            generation: None,
            preview_enabled: false,
            capture: None,
            preview: String::new(),
            revision: 0,
            kept_since: None,
        }
    }

    fn set_state(&mut self, state: UiState) -> u64 {
        self.state = state;
        self.kept_since = state.is_kept_result().then(std::time::Instant::now);
        self.generation = None;
        self.preview.clear();
        self.preview_enabled = false;
        self.capture = None;
        self.revision = self.revision.wrapping_add(1);
        self.revision
    }

    fn set_state_if_unowned(&mut self, state: UiState) -> Option<u64> {
        if self.state.is_active() || self.generation.is_some() {
            return None;
        }
        Some(self.set_state(state))
    }

    fn begin(&mut self, gate: &crate::cancel::CancelGate, state: UiState, preview: bool) -> u64 {
        let generation = gate.begin();
        self.set_state(state);
        self.generation = Some(generation);
        self.preview_enabled = preview;
        generation
    }

    fn accepts(&self, gate: &crate::cancel::CancelGate, generation: u64) -> bool {
        self.generation == Some(generation) && gate.allows_progress(generation)
    }

    fn phase(&mut self, gate: &crate::cancel::CancelGate, generation: u64, next: UiState) -> bool {
        if !self.accepts(gate, generation) {
            return false;
        }
        // Stages only advance. Recording-time sentence ASR updates the preview,
        // not the recording state. Cleanup is optional and may be skipped.
        let rank = |state| match state {
            UiState::Recording => 0,
            UiState::Processing | UiState::Transcribing => 1,
            UiState::Cleaning => 2,
            UiState::Pasting => 3,
            _ => 4,
        };
        if !next.is_active() || rank(next) < rank(self.state) {
            return false;
        }
        self.state = next;
        self.revision = self.revision.wrapping_add(1);
        true
    }

    fn authorize_paste(&mut self, gate: &crate::cancel::CancelGate, generation: u64) -> bool {
        if !self.accepts(gate, generation) || !gate.commit(generation) {
            return false;
        }
        self.phase(gate, generation, UiState::Pasting);
        true
    }

    fn publish(&mut self, gate: &crate::cancel::CancelGate, generation: u64, text: &str) -> bool {
        if !self.accepts(gate, generation) || !self.preview_enabled {
            return false;
        }
        let start = text
            .char_indices()
            .rev()
            .nth(LIVE_PREVIEW_CHARACTERS - 1)
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.preview = text[start..].to_string();
        true
    }

    fn cancel(&mut self, gate: &crate::cancel::CancelGate) -> bool {
        if !gate.request_cancel() {
            return false;
        }
        // The worker may be blocked inside ASR. Stop logical capture now using
        // the flag the audio callback already reads, without touching its locks.
        if let Some(capture) = self.capture.as_ref() {
            capture.store(false, Ordering::SeqCst);
        }
        self.set_state(UiState::Idle);
        true
    }

    fn finish(&mut self, gate: &crate::cancel::CancelGate, generation: u64) {
        // The worker may have already displayed Success/Error; leave that cue
        // intact. A stale drop can never end a more recent utterance.
        gate.end_generation(generation);
        if self.generation == Some(generation) {
            self.set_state(UiState::Idle);
        }
    }

    fn refresh_cancelled(&mut self, gate: &crate::cancel::CancelGate) {
        if let Some(generation) = self.generation {
            if !gate.allows_progress(generation) {
                self.set_state(UiState::Idle);
            }
        }
    }

    /// A key press dismisses a kept-result pill, but not one that only just
    /// appeared: a keystroke already on its way must not hide it unread.
    fn dismiss_kept_on_key(&mut self, now: std::time::Instant) -> bool {
        match self.kept_since {
            Some(since)
                if self.state.is_kept_result()
                    && now.saturating_duration_since(since) >= KEPT_PILL_MIN_VISIBLE =>
            {
                self.set_state(UiState::Idle);
                true
            }
            _ => false,
        }
    }

    fn expire(&mut self, revision: u64) {
        if self.revision == revision {
            self.set_state(UiState::Idle);
        }
    }
}

/// How long a kept-result pill stays up when no key is pressed.
pub const KEPT_PILL: std::time::Duration = std::time::Duration::from_millis(4000);

/// A key press inside this window does not dismiss a kept-result pill.
pub const KEPT_PILL_MIN_VISIBLE: std::time::Duration = std::time::Duration::from_millis(700);

/// True while a kept-result pill is up. Lets the event tap skip the
/// presentation lock on every ordinary key press.
static KEPT_PILL_UP: AtomicBool = AtomicBool::new(false);

/// The most recent finished transcript (inserted, held, failed, transformed or
/// recovered), for "Copy last dictation". Seeded from history at boot.
static LAST_DICTATION: Mutex<String> = Mutex::new(String::new());

/// Process-wide ring of recent RMS levels (one per cpal callback chunk).
/// Written from the cpal audio thread, read from the menu-bar UI timer.
static AUDIO_LEVELS: Mutex<VecDeque<f32>> = Mutex::new(VecDeque::new());

// ─── Writer side (worker / audio threads) ───────────────────────────────

/// Broadcast an unscoped/terminal UI state, clearing all in-flight text.
pub fn set_state(state: UiState) {
    if let Ok(mut presentation) = PRESENTATION.lock() {
        presentation.set_state(state);
    }
    KEPT_PILL_UP.store(state.is_kept_result(), Ordering::SeqCst);
}

/// Called by the event tap on a fresh key-down. Cheap when no kept-result
/// pill is showing (one atomic read); otherwise dismisses it once it has been
/// visible long enough to read.
pub fn key_pressed() {
    if !KEPT_PILL_UP.load(Ordering::SeqCst) {
        return;
    }
    if let Ok(mut p) = PRESENTATION.lock() {
        if p.dismiss_kept_on_key(std::time::Instant::now()) || !p.state.is_kept_result() {
            KEPT_PILL_UP.store(false, Ordering::SeqCst);
        }
    }
}

/// Start one gate generation and its presentation atomically with UI cancellation.
pub fn begin_utterance(initial: UiState, preview_enabled: bool) -> u64 {
    PRESENTATION
        .lock()
        .unwrap()
        .begin(&crate::cancel::CANCEL, initial, preview_enabled)
}

/// Register capture and its generation together, so Cancel stops the producer
/// even while the worker is inside a synchronous model call.
pub fn begin_capturing(preview_enabled: bool, capture: Arc<AtomicBool>) -> u64 {
    let mut p = PRESENTATION.lock().unwrap();
    let generation = p.begin(&crate::cancel::CANCEL, UiState::Recording, preview_enabled);
    p.capture = Some(capture);
    generation
}

/// Publish an actual pipeline boundary for the current, uncancelled generation.
pub fn set_phase(generation: u64, phase: UiState) -> bool {
    PRESENTATION
        .lock()
        .map(|mut p| p.phase(&crate::cancel::CANCEL, generation, phase))
        .unwrap_or(false)
}

/// Claim the write and change the stage in the same critical section as cancel.
pub fn authorize_paste(generation: u64) -> bool {
    PRESENTATION
        .lock()
        .map(|mut p| p.authorize_paste(&crate::cancel::CANCEL, generation))
        .unwrap_or(false)
}

/// Publish the real ASR/cleaned transcript without another inference call.
pub fn publish_preview(generation: u64, text: &str) {
    if let Ok(mut p) = PRESENTATION.lock() {
        p.publish(&crate::cancel::CANCEL, generation, text);
    }
}

/// Clear the pill immediately when Escape/IPC wins the existing cancellation gate.
pub fn request_cancel() -> bool {
    PRESENTATION
        .lock()
        .map(|mut p| p.cancel(&crate::cancel::CANCEL))
        .unwrap_or(false)
}

/// Close all exits from one release pipeline, preserving any terminal cue.
pub fn finish_utterance(generation: u64) {
    if let Ok(mut p) = PRESENTATION.lock() {
        p.finish(&crate::cancel::CANCEL, generation);
    }
}

/// End this generation on every early return/continue in a release or retry.
pub struct UtteranceScope(pub u64);
impl Drop for UtteranceScope {
    fn drop(&mut self) {
        finish_utterance(self.0);
    }
}

/// Show a terminal state long enough for the AppKit poller to render it.
/// Revision checking prevents an older timer hiding a newer identical cue.
pub fn show_transient(state: UiState, duration: std::time::Duration) {
    let revision = match PRESENTATION.lock() {
        Ok(mut p) => p.set_state(state),
        Err(_) => return,
    };
    note_kept_pill(state);
    expire_transient_after(revision, duration);
}

/// Show recovery/menu feedback only when no utterance owns the presentation.
/// This can never clear a live generation or make its write authorization fail.
pub fn show_transient_if_unowned(state: UiState, duration: std::time::Duration) -> bool {
    let revision = match PRESENTATION.lock() {
        Ok(mut p) => match p.set_state_if_unowned(state) {
            Some(revision) => revision,
            None => return false,
        },
        Err(_) => return false,
    };
    note_kept_pill(state);
    expire_transient_after(revision, duration);
    true
}

/// Arm the key-press dismissal. A stale `true` is harmless: `key_pressed`
/// clears it as soon as it finds another state showing.
fn note_kept_pill(state: UiState) {
    if state.is_kept_result() {
        KEPT_PILL_UP.store(true, Ordering::SeqCst);
    }
}

fn expire_transient_after(revision: u64, duration: std::time::Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(duration);
        if let Ok(mut p) = PRESENTATION.lock() {
            p.expire(revision);
        }
    });
}

/// A terminal cue from this exact generation, never a late cancelled result.
pub fn show_transient_for(generation: u64, state: UiState, duration: std::time::Duration) {
    let revision = match PRESENTATION.lock() {
        Ok(mut p) if p.accepts(&crate::cancel::CANCEL, generation) => p.set_state(state),
        _ => return,
    };
    note_kept_pill(state);
    std::thread::spawn(move || {
        std::thread::sleep(duration);
        if let Ok(mut p) = PRESENTATION.lock() {
            p.expire(revision);
        }
    });
}

/// PID of the app the last dictation was written into; 0 = unknown. The
/// success pill's Undo posts a global ⌘Z, so it must only fire while that app
/// is still frontmost.
static LAST_INJECT_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// Remember (or forget, with `None`) which app received the last dictation.
pub fn set_last_inject_pid(pid: Option<i32>) {
    LAST_INJECT_PID.store(pid.unwrap_or(0), std::sync::atomic::Ordering::SeqCst);
}

/// The app that received the last dictation, if known.
pub fn last_inject_pid() -> Option<i32> {
    match LAST_INJECT_PID.load(std::sync::atomic::Ordering::SeqCst) {
        0 => None,
        pid => Some(pid),
    }
}

/// May the pill's Undo send ⌘Z now? Only when the app that received the text
/// is known and is still the frontmost app. If the user switched apps while the
/// result pill was up, ⌘Z would revert an unrelated edit somewhere else.
pub fn undo_target_is_current(injected: Option<i32>, frontmost: Option<i32>) -> bool {
    matches!((injected, frontmost), (Some(a), Some(b)) if a == b)
}

/// Record the most recent finished transcript.
pub fn set_last_dictation(text: &str) {
    if let Ok(mut g) = LAST_DICTATION.lock() {
        *g = text.to_string();
    }
}

/// Boot-time seed: fill the last dictation only while it is still empty, so a
/// dictation finished during startup is never replaced by an older one.
pub fn seed_last_dictation(text: &str) -> bool {
    match LAST_DICTATION.lock() {
        Ok(mut g) if g.is_empty() && !text.trim().is_empty() => {
            *g = text.to_string();
            true
        }
        _ => false,
    }
}

/// Push one mic RMS sample onto the waveform ring, dropping the oldest once
/// the ring is full.
pub fn push_level(rms: f32) {
    if let Ok(mut q) = AUDIO_LEVELS.lock() {
        if q.len() >= WAVEFORM_BARS {
            q.pop_front();
        }
        q.push_back(rms);
    }
}

// ─── Reader side (main / UI thread) ──────────────────────────────────────

/// Current UI state.
pub fn state() -> UiState {
    PRESENTATION
        .lock()
        .map(|mut p| {
            p.refresh_cancelled(&crate::cancel::CANCEL);
            p.state
        })
        .unwrap_or(UiState::Idle)
}

/// The bounded in-flight transcript; empty when off, cancelled, or settled.
pub fn live_preview() -> String {
    PRESENTATION
        .lock()
        .map(|mut p| {
            p.refresh_cancelled(&crate::cancel::CANCEL);
            p.preview.clone()
        })
        .unwrap_or_default()
}

/// The last injected text (empty string if nothing yet).
pub fn last_dictation() -> String {
    LAST_DICTATION.lock().map(|g| g.clone()).unwrap_or_default()
}

/// A snapshot of the recent RMS levels, oldest first.
pub fn recent_levels() -> Vec<f32> {
    AUDIO_LEVELS
        .lock()
        .map(|q| q.iter().copied().collect())
        .unwrap_or_default()
}

/// Clear the waveform ring (called when the pill hides on return to idle).
pub fn reset_levels() {
    if let Ok(mut q) = AUDIO_LEVELS.lock() {
        q.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancel::CancelGate;
    use std::sync::Arc;

    #[test]
    fn every_state_round_trips_through_its_byte() {
        for v in 0..=15u8 {
            assert_eq!(UiState::from_u8(v) as u8, v);
        }
        assert_eq!(UiState::from_u8(200), UiState::Idle);
    }

    #[test]
    fn kept_results_are_terminal_and_let_escape_through() {
        for s in [
            UiState::NoEditableField,
            UiState::PasteFailed,
            UiState::HeldCopied,
            UiState::PasteFailedCopied,
            UiState::LimitReached,
            UiState::LimitReachedCopied,
        ] {
            assert!(s.is_kept_result());
            assert!(!s.is_active());
            assert!(!s.is_cancellable());
        }
        for s in [UiState::Idle, UiState::Success, UiState::Copied, UiState::NothingToCopy, UiState::Error] {
            assert!(!s.is_kept_result());
        }
    }

    #[test]
    fn a_key_press_dismisses_a_kept_pill_only_after_it_was_readable() {
        let mut ui = Presentation::new();
        ui.set_state(UiState::HeldCopied);
        let shown = ui.kept_since.unwrap();
        assert!(!ui.dismiss_kept_on_key(shown + std::time::Duration::from_millis(100)));
        assert_eq!(ui.state, UiState::HeldCopied);
        assert!(ui.dismiss_kept_on_key(shown + KEPT_PILL_MIN_VISIBLE));
        assert_eq!(ui.state, UiState::Idle);
        // Any other pill is left alone by typing.
        ui.set_state(UiState::Success);
        assert!(!ui.dismiss_kept_on_key(std::time::Instant::now() + KEPT_PILL));
        assert_eq!(ui.state, UiState::Success);
    }

    #[test]
    fn a_key_press_never_touches_a_live_recording() {
        let gate = CancelGate::new();
        let mut ui = Presentation::new();
        ui.set_state(UiState::PasteFailedCopied);
        let gen = ui.begin(&gate, UiState::Recording, false);
        assert!(!ui.dismiss_kept_on_key(std::time::Instant::now() + KEPT_PILL));
        assert_eq!(ui.state, UiState::Recording);
        assert!(gate.allows_progress(gen));
    }

    #[test]
    fn paste_failed_is_a_result_state_not_a_working_one() {
        assert_eq!(UiState::from_u8(UiState::PasteFailed as u8), UiState::PasteFailed);
        // Not "processing", so the worker's settle step leaves the pill up…
        assert!(!UiState::PasteFailed.is_processing());
        // …and not cancellable, so an Escape passes through to the app.
        assert!(!UiState::PasteFailed.is_cancellable());
    }

    #[test]
    fn pill_undo_only_fires_into_the_app_that_received_the_text() {
        assert!(undo_target_is_current(Some(412), Some(412)));
        // Regression: Undo used to post ⌘Z blind, into whatever was frontmost.
        assert!(!undo_target_is_current(Some(412), Some(977)));
        assert!(!undo_target_is_current(None, Some(412)));
        assert!(!undo_target_is_current(Some(412), None));
        assert!(!undo_target_is_current(None, None));
    }

    #[test]
    fn pipeline_stages_follow_work_and_cleanup_can_be_skipped() {
        for cleanup in [false, true] {
            let gate = CancelGate::new();
            let mut ui = Presentation::new();
            let gen = ui.begin(&gate, UiState::Recording, true);
            let mut observed = vec![ui.state];
            assert!(ui.phase(&gate, gen, UiState::Transcribing));
            observed.push(ui.state);
            if cleanup {
                assert!(ui.phase(&gate, gen, UiState::Cleaning));
                observed.push(ui.state);
            }
            assert!(ui.authorize_paste(&gate, gen));
            observed.push(ui.state);
            assert_eq!(
                observed,
                if cleanup {
                    vec![
                        UiState::Recording,
                        UiState::Transcribing,
                        UiState::Cleaning,
                        UiState::Pasting,
                    ]
                } else {
                    vec![UiState::Recording, UiState::Transcribing, UiState::Pasting]
                }
            );
            assert!(!ui.state.is_cancellable());
            assert!(
                !ui.cancel(&gate),
                "a write already claimed cannot promise cancellation"
            );
            assert!(
                !ui.phase(&gate, gen, UiState::Transcribing),
                "never regress to late ASR"
            );
            ui.finish(&gate, gen);
            assert_eq!(ui.state, UiState::Idle);
            assert!(!gate.active());
        }
    }

    #[test]
    fn unowned_feedback_never_clears_a_live_generation() {
        let gate = CancelGate::new();
        let mut ui = Presentation::new();
        assert!(ui.set_state_if_unowned(UiState::Success).is_some());
        assert!(ui.set_state_if_unowned(UiState::NoEditableField).is_some());
        ui.set_state(UiState::Idle);

        let generation = ui.begin(&gate, UiState::Recording, true);
        assert!(ui.set_state_if_unowned(UiState::Success).is_none());
        assert_eq!(ui.generation, Some(generation));
        assert!(ui.authorize_paste(&gate, generation));
    }

    #[test]
    fn cancelled_cleanup_cannot_publish_or_authorize_any_late_write() {
        let gate = CancelGate::new();
        let mut ui = Presentation::new();
        let gen = ui.begin(&gate, UiState::Transcribing, true);
        ui.publish(&gate, gen, "Real ASR sentence.");
        ui.phase(&gate, gen, UiState::Cleaning);
        assert!(ui.cancel(&gate));
        assert!(ui.preview.is_empty());
        // Models are not hard interrupted: their eventual result must remain invisible.
        assert!(!ui.publish(&gate, gen, "Late cleaned or transformed sentence."));
        assert!(!ui.phase(&gate, gen, UiState::Cleaning));
        assert!(!ui.authorize_paste(&gate, gen));
        assert_eq!(ui.state, UiState::Idle);
    }

    #[test]
    fn a_new_generation_rejects_old_text_stage_and_scope_completion() {
        let gate = CancelGate::new();
        let mut ui = Presentation::new();
        let old = ui.begin(&gate, UiState::Recording, true);
        ui.publish(&gate, old, "Old sentence.");
        let fresh = ui.begin(&gate, UiState::Recording, true);
        assert!(ui.preview.is_empty());
        assert!(!ui.publish(&gate, old, "Late old sentence."));
        assert!(!ui.phase(&gate, old, UiState::Cleaning));
        assert!(!ui.authorize_paste(&gate, old));
        ui.finish(&gate, old);
        assert!(gate.allows_progress(fresh));
        assert!(ui.publish(&gate, fresh, "Fresh sentence."));
        assert_eq!(ui.state, UiState::Recording);
        assert_eq!(ui.preview, "Fresh sentence.");
    }

    #[test]
    fn old_success_timer_cannot_hide_a_new_recording_or_a_new_success() {
        let gate = CancelGate::new();
        let mut ui = Presentation::new();
        let timer = ui.set_state(UiState::Success);
        ui.begin(&gate, UiState::Recording, false);
        ui.expire(timer);
        assert_eq!(ui.state, UiState::Recording);
        let new_timer = ui.set_state(UiState::Success);
        ui.expire(timer);
        assert_eq!(ui.state, UiState::Success);
        ui.expire(new_timer);
        assert_eq!(ui.state, UiState::Idle);
    }

    #[test]
    fn preview_is_opt_in_bounded_unicode_text_and_never_the_last_dictation() {
        let gate = CancelGate::new();
        let mut ui = Presentation::new();
        let disabled = ui.begin(&gate, UiState::Recording, false);
        assert!(!ui.publish(&gate, disabled, "Do not show this."));
        let enabled = ui.begin(&gate, UiState::Recording, true);
        let source = "中文".repeat(LIVE_PREVIEW_CHARACTERS);
        assert!(ui.publish(&gate, enabled, &source));
        assert_eq!(ui.preview.chars().count(), LIVE_PREVIEW_CHARACTERS);
        ui.finish(&gate, enabled);
        assert!(ui.preview.is_empty());
    }

    #[test]
    fn cancellation_stops_the_capture_flag_before_a_busy_worker_can_resume() {
        let gate = CancelGate::new();
        let mut ui = Presentation::new();
        let old_capture = Arc::new(AtomicBool::new(true));
        let old = ui.begin(&gate, UiState::Recording, true);
        ui.capture = Some(old_capture.clone());
        assert!(ui.cancel(&gate));
        assert!(!old_capture.load(Ordering::SeqCst));
        let fresh_capture = Arc::new(AtomicBool::new(true));
        let fresh = ui.begin(&gate, UiState::Recording, true);
        ui.capture = Some(fresh_capture.clone());
        ui.finish(&gate, old);
        assert!(fresh_capture.load(Ordering::SeqCst));
        assert!(gate.allows_progress(fresh));
    }

    #[test]
    fn cancel_racing_segment_completion_cannot_resurrect_the_pill() {
        for _ in 0..100 {
            let gate = Arc::new(CancelGate::new());
            let ui = Arc::new(Mutex::new(Presentation::new()));
            let gen = ui.lock().unwrap().begin(&gate, UiState::Recording, true);
            let writer = {
                let gate = gate.clone();
                let ui = ui.clone();
                std::thread::spawn(move || {
                    let mut p = ui.lock().unwrap();
                    p.publish(&gate, gen, "A sentence finished during Escape.");
                    p.phase(&gate, gen, UiState::Transcribing);
                })
            };
            assert!(ui.lock().unwrap().cancel(&gate));
            writer.join().unwrap();
            let p = ui.lock().unwrap();
            assert_eq!(p.state, UiState::Idle);
            assert!(p.preview.is_empty());
            assert!(!gate.commit(gen));
        }
    }
}
