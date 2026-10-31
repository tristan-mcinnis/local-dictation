//! Cleanup policy: one path from a raw transcript to the text that is refined
//! and delivered. Push-to-talk, retry, listen mode and saved-audio recovery
//! all take it, so the same audio gives the same text whichever way it runs.
use super::*;

/// The warm models and the per-boot cleanup facts every path shares.
pub(super) struct Pipeline<'a> {
    #[cfg(feature = "cleaner")]
    pub cleaner: Option<&'a TextCleanupEngine>,
    #[cfg(feature = "cleaner")]
    pub rt: &'a tokio::runtime::Runtime,
    pub refiner: &'a Refiner,
    /// An output-format preset (numbered, bullets, email) is active: every
    /// utterance of three words or more goes through the model to be
    /// reshaped, so the clean-dictation shortcut is off.
    pub format_active: bool,
}

impl Pipeline<'_> {
    pub(super) fn has_cleaner(&self) -> bool {
        #[cfg(feature = "cleaner")]
        {
            self.cleaner.is_some()
        }
        #[cfg(not(feature = "cleaner"))]
        {
            false
        }
    }
}

/// Where the text goes, as far as cleanup cares.
pub(super) struct CleanupTarget {
    /// A terminal draws its own grid and takes shell commands, not prose:
    /// the raw transcript goes in and cleanup is skipped.
    pub terminal: bool,
    /// Proper nouns harvested from the focused field and window title, so
    /// the model spells on-screen names the way they appear.
    pub screen_vocab: Vec<String>,
    /// The per-app style hint (chat, email, code).
    pub tone: Option<&'static str>,
}

impl CleanupTarget {
    /// Saved-audio recovery copies to the clipboard: no app, no screen, no tone.
    pub(super) const NONE: Self = Self { terminal: false, screen_vocab: Vec::new(), tone: None };

    /// The target facts for the app the text will go to and the field captured
    /// there, resolved once; nothing re-reads the frontmost app later.
    pub(super) fn of(app: Option<&crate::injector::TargetApp>, focus: Option<&FocusTarget>) -> Self {
        let pid = app.map(|a| a.pid);
        let terminal = pid.map(crate::injector::is_terminal_pid).unwrap_or(false);
        let bundle = app.and_then(|a| a.bundle_id.as_deref());
        let name = pid.map(app_name).unwrap_or_default();
        let tone = crate::app_tones::tone_for(&name, bundle);
        if let Some(t) = tone {
            eprintln!("  ♪    tone: {t}");
        }
        let screen_vocab = focus.map(|t| t.screen_terms(SCREEN_VOCAB_CAP)).unwrap_or_default();
        Self { terminal, screen_vocab, tone }
    }
}

/// How the delivered text was produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CleanupSource {
    /// The raw transcript: a terminal target, or no cleanup engine.
    Raw,
    /// Corrections plus the deterministic speech-mechanics pass: the text was
    /// short or already clean, or the model's answer was rejected.
    Deterministic,
    /// The model's own output, accepted.
    Model,
}

pub(super) struct Cleaned {
    pub text: String,
    pub source: CleanupSource,
    /// Why the model's text was not used, for the History record.
    pub error: Option<String>,
    /// Model time, zero when it did not run.
    pub llm_ms: u128,
}

/// Cleanup stopped before the model ran: the utterance is no longer the live
/// one (a newer press owns the pill, or a cancel landed).
pub(super) struct Superseded;

/// The pure decision: what one transcript needs.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum CleanupPlan {
    Raw,
    Deterministic { text: String, words: usize },
    /// Run the model on `input`; `fallback` wins if it fails or drops content.
    Model { input: String, fallback: String },
}

/// Known proper nouns are fixed in the raw transcript before cleanup so the
/// small model sees the intended spelling ("to twist" becomes "Todoist")
/// instead of mangling a garbled one; `refine` corrects again afterwards.
/// Below three words there is nothing for the model to clean, tone or
/// format, so tiny utterances always take the deterministic path, as does
/// clean dictation when no preset is active: about 87% of utterances pass
/// through the model unchanged, and some pick up unwanted substitutions.
pub(super) fn plan_cleanup(
    raw: &str,
    refiner: &Refiner,
    has_cleaner: bool,
    terminal: bool,
    format_active: bool,
) -> CleanupPlan {
    if !has_cleaner || terminal {
        return CleanupPlan::Raw;
    }
    let input = refiner.apply_corrections(raw);
    let fallback = crate::text_polish::fix_speech_mechanics(&input);
    let words = input.split_whitespace().count();
    if words < 3 || (!format_active && !crate::text_polish::needs_llm_cleanup(&input)) {
        CleanupPlan::Deterministic { text: fallback, words }
    } else {
        CleanupPlan::Model { input, fallback }
    }
}

/// The model's answer, or the deterministic fallback with the reason: a
/// rewrite that dropped a clause loses to the faithful text, and so does an
/// error. The fallback can never drop or swap a word.
pub(super) fn accept_cleanup(input: &str, fallback: String, result: eyre::Result<String>) -> Cleaned {
    match result {
        Ok(cleaned) if crate::text_polish::cleanup_dropped_content(input, &cleaned) => {
            eprintln!("  ⚠    cleanup dropped content, using faithful deterministic text instead");
            Cleaned {
                text: fallback,
                source: CleanupSource::Deterministic,
                error: Some("Model cleanup rejected because it dropped content; deterministic text used".into()),
                llm_ms: 0,
            }
        }
        Ok(cleaned) => Cleaned { text: cleaned, source: CleanupSource::Model, error: None, llm_ms: 0 },
        Err(err) => {
            eprintln!("[warn] cleanup failed, using raw: {err:?}");
            Cleaned {
                text: fallback,
                source: CleanupSource::Deterministic,
                error: Some(format!("Cleanup failed; deterministic text used: {err}")),
                llm_ms: 0,
            }
        }
    }
}

/// Clean one transcript for `generation`. The phase moves to Cleaning only
/// when the model is about to run; a refused phase means the utterance is no
/// longer live and nothing more is spent on it.
pub(super) fn clean(
    raw: &str,
    models: &Pipeline,
    target: &CleanupTarget,
    generation: u64,
) -> Result<Cleaned, Superseded> {
    let has_cleaner = models.has_cleaner();
    if has_cleaner && target.terminal {
        eprintln!("  ⌁    terminal · raw transcript (cleanup skipped)");
    }
    match plan_cleanup(raw, models.refiner, has_cleaner, target.terminal, models.format_active) {
        CleanupPlan::Raw => Ok(Cleaned {
            text: raw.to_string(),
            source: CleanupSource::Raw,
            error: None,
            llm_ms: 0,
        }),
        CleanupPlan::Deterministic { text, words } => {
            eprintln!("  ⚡    clean ({words} words) · deterministic cleanup, LLM skipped");
            Ok(Cleaned { text, source: CleanupSource::Deterministic, error: None, llm_ms: 0 })
        }
        CleanupPlan::Model { input, fallback } => {
            if !target.screen_vocab.is_empty() {
                eprintln!("  ⌕    screen vocab: {}", crate::daemon_log::terms(&target.screen_vocab));
            }
            if input != raw {
                eprintln!("  ✎    pre-cleanup corrections applied");
            }
            if !ui_channel::set_phase(generation, UiState::Cleaning) {
                return Err(Superseded);
            }
            let t = Instant::now();
            let result = run_model(models, &input, target);
            let mut cleaned = accept_cleanup(&input, fallback, result);
            cleaned.llm_ms = ms(t.elapsed());
            Ok(cleaned)
        }
    }
}

#[cfg(feature = "cleaner")]
fn run_model(models: &Pipeline, input: &str, target: &CleanupTarget) -> eyre::Result<String> {
    let cleaner = models.cleaner.expect("a Model plan needs a cleaner");
    models.rt.block_on(cleaner.process_transcript_with_vocab_tone(
        input,
        &target.screen_vocab,
        target.tone,
    ))
}

#[cfg(not(feature = "cleaner"))]
fn run_model(_models: &Pipeline, _input: &str, _target: &CleanupTarget) -> eyre::Result<String> {
    unreachable!("plan_cleanup never plans a model run without a cleaner")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refiner() -> Refiner {
        Refiner::new(Corrections::from_map(std::collections::HashMap::from([
            ("to twist".into(), "Todoist".into()),
        ])))
    }

    #[test]
    fn a_terminal_or_no_engine_gets_the_raw_transcript() {
        let r = refiner();
        assert_eq!(plan_cleanup("um add it to to twist", &r, false, false, false), CleanupPlan::Raw);
        assert_eq!(plan_cleanup("um add it to to twist", &r, true, true, false), CleanupPlan::Raw);
    }

    #[test]
    fn short_or_clean_text_skips_the_model_after_corrections() {
        let r = refiner();
        match plan_cleanup("Open to twist.", &r, true, false, false) {
            CleanupPlan::Deterministic { text, words } => {
                assert_eq!(text, "Open Todoist.");
                assert_eq!(words, 2);
            }
            other => panic!("expected the deterministic path, got {other:?}"),
        }
        match plan_cleanup("Please send the report to the team today.", &r, true, false, false) {
            CleanupPlan::Deterministic { .. } => {}
            other => panic!("clean dictation must skip the model, got {other:?}"),
        }
    }

    #[test]
    fn messy_text_or_an_active_preset_runs_the_model_on_corrected_input() {
        let r = refiner();
        match plan_cleanup("so we gotta add it to to twist you know like today", &r, true, false, false) {
            CleanupPlan::Model { input, .. } => assert!(input.contains("Todoist")),
            other => panic!("filler must reach the model, got {other:?}"),
        }
        match plan_cleanup("Please send the report to the team today.", &r, true, false, true) {
            CleanupPlan::Model { .. } => {}
            other => panic!("a preset must reach the model, got {other:?}"),
        }
    }

    #[test]
    fn the_model_loses_when_it_drops_content_or_fails() {
        let input = "Please send the quarterly report to the whole team before the meeting on Thursday.";
        let fallback = || input.to_string();
        let dropped = accept_cleanup(input, fallback(), Ok("Please send the report.".into()));
        assert_eq!(dropped.source, CleanupSource::Deterministic);
        assert_eq!(dropped.text, input);
        assert!(dropped.error.is_some());

        let failed = accept_cleanup(input, fallback(), Err(eyre::eyre!("model refused")));
        assert_eq!(failed.source, CleanupSource::Deterministic);
        assert!(failed.error.as_deref().unwrap_or("").contains("model refused"));

        let kept = accept_cleanup(input, fallback(), Ok(format!("{input} Thanks.")));
        assert_eq!(kept.source, CleanupSource::Model);
        assert!(kept.error.is_none());
    }
}
