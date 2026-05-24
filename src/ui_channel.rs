//! The worker → UI cross-thread channel.
//!
//! The push-to-talk daemon runs the inference pipeline on a worker thread and
//! the menu-bar UI on the main thread (AppKit's run loop). Three things flow
//! between them, all via process-wide statics so neither side needs a handle
//! to the other:
//!
//!   * **state** — the current `UiState` (idle / recording / processing),
//!     which drives the status-item icon and shows/hides the floating pill.
//!   * **last dictation** — the most recent injected text, for the menu's
//!     "Copy last dictation" item and "Last: …" preview.
//!   * **audio levels** — a short ring of recent mic RMS values, written from
//!     the cpal audio callback and read by the 30 FPS pill animation.
//!
//! Keeping all three behind one module means the worker↔UI contract is
//! defined in a single place: the worker side (`set_state`, `set_last_dictation`,
//! `push_level`) and the reader side (`state`, `last_dictation`,
//! `recent_levels`) are the whole interface. `audio.rs` stays a pure capture
//! module that just calls `push_level`; `menubar.rs` reads through here rather
//! than owning the statics itself.
//!
//! This module is plain `std` (atomics, a mutex, a `VecDeque`) so it compiles
//! in every feature configuration, including the default lib build that has
//! neither the daemon nor the menu bar.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;

/// What the UI should be showing. Broadcast by the worker thread; the menu-bar
/// poll timer reads it and swaps the status icon / pill visibility.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum UiState {
    Idle = 0,
    Recording = 1,
    Processing = 2,
    /// The agent is speaking a response (TTS playback). Drives a distinct
    /// center-out "assistant talking" waveform, separate from the mic-driven
    /// Recording waveform.
    Speaking = 3,
}

impl UiState {
    /// Reconstruct from the raw atomic byte. Unknown values map to `Idle`.
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => UiState::Recording,
            2 => UiState::Processing,
            3 => UiState::Speaking,
            _ => UiState::Idle,
        }
    }
}

/// Visual accent for the pill, orthogonal to [`UiState`]. Agent mode tints the
/// pill blue (recording *and* speaking) so the mode is unmistakable; everything
/// else stays neutral. Read by the pill renderer alongside the state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Accent {
    Normal = 0,
    Agent = 1,
}

impl Accent {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Accent::Agent,
            _ => Accent::Normal,
        }
    }
}

/// How many recent RMS samples we keep for the waveform display. Matches the
/// number of bars in the floating pill.
pub const WAVEFORM_BARS: usize = 14;

static SHARED_STATE: AtomicU8 = AtomicU8::new(0);

/// Current pill accent (Normal / Agent). Set by the worker at capture start.
static SHARED_ACCENT: AtomicU8 = AtomicU8::new(0);

/// The last successfully-injected text, for the "Copy last dictation" item.
static LAST_DICTATION: Mutex<String> = Mutex::new(String::new());

/// Process-wide ring of recent RMS levels (one per cpal callback chunk).
/// Written from the cpal audio thread, read from the menu-bar UI timer.
static AUDIO_LEVELS: Mutex<VecDeque<f32>> = Mutex::new(VecDeque::new());

// ─── Writer side (worker / audio threads) ───────────────────────────────

/// Broadcast a new UI state.
pub fn set_state(state: UiState) {
    SHARED_STATE.store(state as u8, Ordering::SeqCst);
}

/// Set the pill accent (Normal / Agent). The worker calls this at the start of
/// a capture so the pill tints for the whole gesture, and resets it on return
/// to idle.
pub fn set_accent(accent: Accent) {
    SHARED_ACCENT.store(accent as u8, Ordering::SeqCst);
}

/// Current pill accent.
pub fn accent() -> Accent {
    Accent::from_u8(SHARED_ACCENT.load(Ordering::SeqCst))
}

/// Record the most recent injected text.
pub fn set_last_dictation(text: &str) {
    if let Ok(mut g) = LAST_DICTATION.lock() {
        *g = text.to_string();
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
    UiState::from_u8(SHARED_STATE.load(Ordering::SeqCst))
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
