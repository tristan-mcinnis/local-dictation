//! Saved-audio recovery has one destination: the clipboard. It never parses
//! voice commands, invokes an injector, chooses a target, or activates an app.
//! Closures are the same warm ASR/cleanup primitives used by live dictation;
//! the seam lets cancellation and version retention be tested without models.

use crate::cancel::CancelGate;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Transcribing,
    Cleaning,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
    Raw,
    Cleaned,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Copied(String),
    Cancelled,
    Empty,
    Failed(String),
}

/// Keep every model result before checking cancellation, but claim the gate
/// immediately before the clipboard write. Cancelled recovery never erases the
/// saved recording or earlier versions. No action interpretation exists here.
pub fn to_clipboard(
    gate: &CancelGate,
    generation: u64,
    audio: &[f32],
    transcribe: impl FnOnce(&[f32]) -> eyre::Result<String>,
    cleanup: Option<&mut dyn FnMut(&str) -> eyre::Result<String>>,
    mut phase: impl FnMut(Phase),
    mut version: impl FnMut(Version, &str),
    mut copy: impl FnMut(&str) -> eyre::Result<()>,
) -> Outcome {
    if !gate.allows_progress(generation) {
        return Outcome::Cancelled;
    }
    phase(Phase::Transcribing);
    let raw = match transcribe(audio) {
        Ok(raw) => raw,
        Err(error) => {
            return if gate.allows_progress(generation) {
                Outcome::Failed(error.to_string())
            } else {
                Outcome::Cancelled
            }
        }
    };
    version(Version::Raw, &raw);
    if !gate.allows_progress(generation) {
        return Outcome::Cancelled;
    }
    if raw.trim().is_empty() {
        return Outcome::Empty;
    }
    let cleaned = match cleanup {
        Some(clean) => {
            phase(Phase::Cleaning);
            match clean(&raw) {
                Ok(text) => {
                    version(Version::Cleaned, &text);
                    text
                }
                Err(error) => {
                    return if gate.allows_progress(generation) {
                        Outcome::Failed(error.to_string())
                    } else {
                        Outcome::Cancelled
                    }
                }
            }
        }
        None => raw,
    };
    if !gate.allows_progress(generation) {
        return Outcome::Cancelled;
    }
    let text = cleaned.trim();
    if text.is_empty() {
        return Outcome::Empty;
    }
    if !gate.commit(generation) {
        return Outcome::Cancelled;
    }
    match copy(text) {
        Ok(()) => Outcome::Copied(text.to_string()),
        Err(error) => Outcome::Failed(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn spoken_actions_are_only_copied_without_inventing_a_cleanup_version() {
        for words in [
            "undo",
            "press enter",
            "translate to Chinese hello",
            "scratch that",
        ] {
            let gate = CancelGate::new();
            let gen = gate.begin();
            let phases = RefCell::new(Vec::new());
            let versions = RefCell::new(Vec::new());
            let mut copies = Vec::new();
            let result = to_clipboard(
                &gate,
                gen,
                &[0.2],
                |_| Ok(words.into()),
                None,
                |p| phases.borrow_mut().push(p),
                |kind, text| versions.borrow_mut().push((kind, text.to_string())),
                |text| {
                    copies.push(text.to_string());
                    Ok(())
                },
            );
            assert_eq!(result, Outcome::Copied(words.into()));
            assert_eq!(copies, vec![words]);
            assert_eq!(*phases.borrow(), vec![Phase::Transcribing]);
            assert_eq!(*versions.borrow(), vec![(Version::Raw, words.into())]);
        }
    }

    #[test]
    fn cancellation_during_cleanup_preserves_versions_but_never_copies() {
        let gate = CancelGate::new();
        let gen = gate.begin();
        let mut versions = Vec::new();
        let mut phases = Vec::new();
        let mut cleanup = |_: &str| {
            gate.request_cancel();
            Ok("Cleaned sentence.".into())
        };
        let result = to_clipboard(
            &gate,
            gen,
            &[0.1, 0.2],
            |_| Ok("Raw sentence.".into()),
            Some(&mut cleanup),
            |p| phases.push(p),
            |kind, text| versions.push((kind, text.to_string())),
            |_| panic!("cancelled cleanup must not write the clipboard"),
        );
        assert_eq!(result, Outcome::Cancelled);
        assert_eq!(phases, vec![Phase::Transcribing, Phase::Cleaning]);
        assert_eq!(
            versions,
            vec![
                (Version::Raw, "Raw sentence.".into()),
                (Version::Cleaned, "Cleaned sentence.".into())
            ]
        );
    }

    #[test]
    fn cancellation_during_asr_does_not_start_cleanup() {
        let gate = CancelGate::new();
        let gen = gate.begin();
        let mut versions = Vec::new();
        let mut cleanup = |_: &str| -> eyre::Result<String> { panic!("no cleanup after cancel") };
        let result = to_clipboard(
            &gate,
            gen,
            &[0.2],
            |_| {
                gate.request_cancel();
                Ok("Recovered raw words".into())
            },
            Some(&mut cleanup),
            |_| {},
            |kind, text| versions.push((kind, text.to_string())),
            |_| panic!("no write after cancel"),
        );
        assert_eq!(result, Outcome::Cancelled);
        assert_eq!(versions, vec![(Version::Raw, "Recovered raw words".into())]);
    }

    #[test]
    fn stale_recovery_cannot_commit_after_a_new_request_begins() {
        let gate = CancelGate::new();
        let old = gate.begin();
        let fresh = std::cell::Cell::new(0);
        let result = to_clipboard(
            &gate,
            old,
            &[0.2],
            |_| {
                fresh.set(gate.begin());
                Ok("Late words".into())
            },
            None,
            |_| {},
            |_, _| {},
            |_| panic!("stale recovery must not copy"),
        );
        assert_eq!(result, Outcome::Cancelled);
        assert!(gate.commit(fresh.get()));
        gate.end_generation(old);
        assert!(gate.allows_progress(fresh.get()));
    }

    #[test]
    fn failed_asr_never_runs_cleanup_or_copy() {
        let gate = CancelGate::new();
        let gen = gate.begin();
        let mut cleanup =
            |_: &str| -> eyre::Result<String> { panic!("no cleanup after ASR failure") };
        let result = to_clipboard(
            &gate,
            gen,
            &[0.2],
            |_| Err(eyre::eyre!("fixture ASR failure")),
            Some(&mut cleanup),
            |_| {},
            |_, _| panic!("no invented result"),
            |_| panic!("no write after ASR failure"),
        );
        assert_eq!(result, Outcome::Failed("fixture ASR failure".into()));
    }
}
