//! The out-of-band utterance cancellation gate.
//!
//! The push-to-talk daemon's worker thread drives the whole capture →
//! ASR → cleanup → inject pipeline. While it is inside the release pipeline
//! (transcribing / cleaning) it is **blocked** on that work, so a cancel
//! placed on the normal event channel (`DaemonEvent::CancelRecording`) sits
//! queued and is only processed *after* the utterance has already been
//! injected — too late. The event tap and the control socket therefore signal
//! cancellation *out-of-band* through a lock-free atomic gate that the worker
//! polls at the pipeline's decision points (before ASR, before cleanup, and
//! crucially before injection).
//!
//! The gate is scoped to a **utterance generation** so a cancel can never leak
//! into a later utterance: every new recording starts a fresh generation and
//! clears any pending cancel, and a cancel targets exactly the in-flight
//! generation. This is the "stale cancel must not affect the next dictation"
//! guarantee.
//!
//! Generation and phase are packed into **one** atomic word. The previous
//! design split them across three atomics (`current`, `cancel_target`,
//! `active`), which raced: `request_cancel` could snapshot generation N while
//! the worker rolled over to N+1, then land its compare-exchange on a freshly
//! cleared `cancel_target`, leaving `cancel_target == N` while `current == N+1`
//! — poisoning the next utterance's commit. A generation-specific
//! compare-exchange on the packed word makes that impossible: an operation
//! that observed generation N can only ever flip generation N's state, never
//! a newer generation's.
//!
//! All state is `std` atomics, so the module compiles in every feature
//! configuration and can live in the featureless lib build. The daemon uses
//! the process-global [`CANCEL`] below; unit tests drive a private [`CancelGate`]
//! directly so they never race the live daemon's statics.
//!
//! Escape semantics (user-approved contract):
//!   * Escape during a recording or a pending transcription cancels the
//!     utterance and prevents **any** late insertion.
//!   * Escape while idle is a normal Escape and passes through untouched.

use std::sync::atomic::{AtomicU64, Ordering};

/// Packed gate state: `(generation << PHASE_BITS) | phase`.
///
/// Phases (2 bits):
///   * `IDLE`      — no utterance in flight (also the initial state, gen 0).
///   * `ACTIVE`    — an utterance is in flight and still cancellable.
///   * `CANCELLED` — a cancel was accepted for this generation. Sticky until
///                   the worker calls `end()` or a fresh `begin()`.
///   * `COMMITTED` — the worker authorized the write; a late cancel is ignored.
const PHASE_BITS: u64 = 2;
const PHASE_MASK: u64 = (1 << PHASE_BITS) - 1;
const PHASE_IDLE: u64 = 0;
const PHASE_ACTIVE: u64 = 1;
const PHASE_CANCELLED: u64 = 2;
const PHASE_COMMITTED: u64 = 3;

#[inline]
const fn pack(gen: u64, phase: u64) -> u64 {
    (gen << PHASE_BITS) | phase
}

/// A lock-free cancellation gate scoped to an utterance generation.
#[derive(Debug, Default)]
pub struct CancelGate {
    /// Monotonic utterance sequence, allocated by [`CancelGate::begin`]. Kept
    /// separate from `state` only because `begin` needs the *next* fresh
    /// number; the gate's truth lives entirely in `state` below.
    gen_counter: AtomicU64,
    /// `(generation << 2) | phase` — the single source of truth.
    state: AtomicU64,
}

impl CancelGate {
    /// A fresh gate (no generation ever used, nothing in flight).
    pub const fn new() -> Self {
        Self {
            gen_counter: AtomicU64::new(0),
            state: AtomicU64::new(pack(0, PHASE_IDLE)),
        }
    }

    /// Begin a new utterance and return its generation. Overwrites any prior
    /// state — a stale cancel from a previous utterance can never affect a
    /// fresh dictation — and marks the gate `ACTIVE`.
    pub fn begin(&self) -> u64 {
        let gen = self.gen_counter.fetch_add(1, Ordering::SeqCst) + 1;
        self.state.store(pack(gen, PHASE_ACTIVE), Ordering::SeqCst);
        gen
    }

    /// End the current utterance (after injection or discard). The gate
    /// returns to idle; a cancel arriving now is a no-op.
    pub fn end(&self) {
        self.state.store(pack(0, PHASE_IDLE), Ordering::SeqCst);
    }

    /// True while an utterance is in flight (begin called, end not yet).
    pub fn active(&self) -> bool {
        (self.state.load(Ordering::SeqCst) & PHASE_MASK) != PHASE_IDLE
    }

    /// Request cancellation of the in-flight utterance. Returns `true` when a
    /// cancel was accepted (or had already been accepted for the same
    /// generation), `false` when idle or already committed — so the caller can
    /// let an idle / late Escape pass through untouched.
    ///
    /// The compare-exchange is **generation-specific**: it only flips the
    /// generation encoded in the word it read. If the CAS loses, it re-reads to
    /// report the outcome **without adopting a newer generation** — a cancel
    /// that observed generation N must never cancel N+1. Linearizable with
    /// [`CancelGate::commit`] on the same word: whoever CASes first wins.
    pub fn request_cancel(&self) -> bool {
        let s = self.state.load(Ordering::SeqCst);
        let gen = s >> PHASE_BITS;
        match s & PHASE_MASK {
            PHASE_ACTIVE => {
                match self.state.compare_exchange(
                    s,
                    pack(gen, PHASE_CANCELLED),
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => true,
                    Err(_) => {
                        let now = self.state.load(Ordering::SeqCst);
                        (now >> PHASE_BITS) == gen && (now & PHASE_MASK) == PHASE_CANCELLED
                    }
                }
            }
            PHASE_CANCELLED => true, // sticky: already cancelled for this gen
            _ => false,              // committed or idle: nothing cancellable
        }
    }

    /// The linearizable write-commit: the single point at which an utterance
    /// transitions from *cancellable* to *committed*. Returns `true` exactly
    /// once per generation, and only when that generation is still `ACTIVE`
    /// (no cancel was accepted before this point). A re-commit, a cancelled
    /// generation, an ended generation, or a stale commit for a superseded
    /// generation all return `false`.
    pub fn commit(&self, gen: u64) -> bool {
        self.state
            .compare_exchange(
                pack(gen, PHASE_ACTIVE),
                pack(gen, PHASE_COMMITTED),
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// True when a cancel has been accepted for exactly this generation.
    /// `false` once the utterance has committed, ended, or been superseded.
    pub fn requested(&self, gen: u64) -> bool {
        self.state.load(Ordering::SeqCst) == pack(gen, PHASE_CANCELLED)
    }

    /// Sticky read for the worker's early-discard checkpoints (before
    /// transcribe / before cleanup). Named "consume" for the caller's intent
    /// (discard at this checkpoint), but it does **not** clear the marker:
    /// clearing it would reopen a window where a later `commit` could authorize
    /// the cancelled write. Cancellation is one-shot in effect because the
    /// caller immediately `end()`s the generation after observing it.
    pub fn consume_cancel(&self, gen: u64) -> bool {
        self.requested(gen)
    }
}

/// The daemon's process-global cancellation gate. The worker owns the truth;
/// the event tap and control socket read/write it. There is exactly one daemon
/// per process, so a `static` mirrors the `ipc::DAEMON_RECORDING` pattern.
pub static CANCEL: CancelGate = CancelGate::new();

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn idle_cannot_be_cancelled() {
        let g = CancelGate::new();
        assert!(!g.active());
        assert!(!g.request_cancel());
        assert!(!g.requested(1));
        assert!(!g.consume_cancel(1));
        // A never-begun generation cannot be committed either.
        assert!(!g.commit(1));
    }

    #[test]
    fn begin_is_active_with_no_pending_cancel() {
        let g = CancelGate::new();
        let gen = g.begin();
        assert!(g.active());
        assert!(gen > 0);
        assert!(!g.requested(gen));
    }

    #[test]
    fn cancel_targets_the_in_flight_generation() {
        let g = CancelGate::new();
        let gen = g.begin();
        assert!(g.request_cancel());
        assert!(g.requested(gen));
        // A different (future) generation must not read the cancel.
        assert!(!g.requested(gen + 1));
    }

    #[test]
    fn cancel_is_sticky_and_idempotent() {
        let g = CancelGate::new();
        let gen = g.begin();
        assert!(g.request_cancel());
        assert!(g.request_cancel()); // idempotent: still accepted
        assert!(g.requested(gen));
        // Sticky: the early-checkpoint read observes the same cancel and does
        // not clear it…
        assert!(g.consume_cancel(gen));
        assert!(g.requested(gen));
        // …so a commit can never resurrect a cancelled write.
        assert!(!g.commit(gen));
        assert!(g.requested(gen));
    }

    #[test]
    fn fresh_begin_discards_a_stale_cancel() {
        // The headlining guarantee: a cancel for utterance N must not affect
        // utterance N+1. After a cancel, beginning a new generation clears it.
        let g = CancelGate::new();
        let n = g.begin();
        assert!(g.request_cancel());
        assert!(g.requested(n));

        let n_plus_1 = g.begin();
        assert_ne!(n_plus_1, n);
        assert!(!g.requested(n_plus_1));
        assert!(g.commit(n_plus_1)); // the new utterance commits cleanly
    }

    #[test]
    fn end_returns_to_idle_and_clears_cancel() {
        let g = CancelGate::new();
        let gen = g.begin();
        assert!(g.request_cancel());
        g.end();
        assert!(!g.active());
        assert!(!g.requested(gen));
        assert!(!g.request_cancel());
    }

    #[test]
    fn generations_are_strictly_ordered() {
        let g = CancelGate::new();
        let a = g.begin();
        let b = g.begin();
        let c = g.begin();
        assert!(a < b && b < c);
    }

    #[test]
    fn global_gate_is_usable_as_a_static() {
        let gen = CANCEL.begin();
        assert!(CANCEL.active());
        assert!(CANCEL.request_cancel());
        assert!(CANCEL.requested(gen));
        CANCEL.end();
        assert!(!CANCEL.active());
    }

    // ─── write-commit authorization ─────────────────────────────────────

    #[test]
    fn commit_with_no_cancel_proceeds_and_ignores_late_cancel() {
        let g = CancelGate::new();
        let gen = g.begin();
        assert!(g.commit(gen));
        // A late cancel (after commit) is rejected, not reported as success.
        assert!(!g.request_cancel());
        assert!(!g.requested(gen)); // committed reads "not cancelled"
    }

    #[test]
    fn commit_is_exactly_once_authorization() {
        let g = CancelGate::new();
        let gen = g.begin();
        assert!(g.commit(gen));
        assert!(!g.commit(gen)); // a re-commit is NOT authorized
    }

    #[test]
    fn cancel_before_commit_suppresses_commit() {
        let g = CancelGate::new();
        let gen = g.begin();
        assert!(g.request_cancel());
        assert!(!g.commit(gen));
    }

    #[test]
    fn stale_commit_for_superseded_generation_is_not_authorized() {
        // DETERMINISTIC regression for "commit(gen) doesn't validate
        // current/active": the old split-atomic design authorized commit(N)
        // even after the worker had moved to N+1. The generation-specific CAS
        // refuses it. This is the pinned, sequential proof; the rollover race
        // itself is exercised (non-pinned) by the stress test below.
        let g = CancelGate::new();
        let n = g.begin();
        g.end();
        let n1 = g.begin();
        assert!(!g.commit(n)); // superseded generation: refused
        assert!(g.commit(n1)); // live generation: authorized exactly once
    }

    #[test]
    fn stale_cancel_never_suppresses_fresh_commit() {
        // A cancel accepted for N is confined to N's lifecycle: the next
        // begin() is clean and its commit proceeds.
        let g = CancelGate::new();
        let n = g.begin();
        assert!(g.request_cancel());
        assert!(!g.commit(n)); // N suppressed
        g.end();
        let n1 = g.begin();
        assert!(!g.requested(n1));
        assert!(g.commit(n1));
    }

    // ─── deterministic concurrency / interleaving ───────────────────────

    #[test]
    fn cancel_racing_generation_rollover_never_poisons_next_utterance() {
        // STRESS test (not a pinned interleaving — the tap/worker schedule
        // varies per iteration): the parent-reported race, a tap observes
        // generation N, the worker ends N and begins N+1, then the tap's cancel
        // lands. The split-atomic design left `cancel_target == N` while
        // `current == N+1`, so commit(N+1) returned false — a silently poisoned
        // fresh utterance. The loop widens the race window so that interleaving
        // is actually exercised.
        //
        // The packed design's generation-specific CAS can only flip the
        // generation it observed. The invariant below is schedule-independent,
        // so the assertion holds for every interleaving: commit(N+1) must be
        // refused iff N+1 itself was cancelled — never because a stale cancel
        // leaked across the rollover.
        for _ in 0..1000 {
            let g = Arc::new(CancelGate::new());
            g.begin(); // N active

            let g_tap = Arc::clone(&g);
            let tap = thread::spawn(move || g_tap.request_cancel());

            g.end();          // worker settles N
            let n1 = g.begin(); // worker begins N+1 (fresh begin clears any stale cancel)
            let _ = tap.join().unwrap();

            let committed = g.commit(n1);
            let requested = g.requested(n1);
            assert_eq!(
                committed,
                !requested,
                "commit authorization must mirror N+1's own cancel state (no stale poison)"
            );
        }
    }
}
