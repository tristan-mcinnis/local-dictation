//! Async Gemma 3 cleanup engine via llama-cpp-2 (Metal backend on Apple Silicon).
//!
//! The spec referenced `llama_cpp = "0.3.0"`, but that crate ships a llama.cpp
//! from April 2024 — too old to recognise the Gemma 3 architecture. We use
//! llama-cpp-2 which tracks current llama.cpp.
//!
//! ## Prefix-cached generation
//!
//! A `LlamaContext` borrows its model, so it can't be stored next to the model
//! `Arc` in a struct (self-reference). Instead a single dedicated worker thread
//! owns one persistent context for the daemon's lifetime; the async API hands it
//! work over a channel. Keeping the context alive lets us **reuse the KV cache**:
//! the cleanup system prompt (~400 tokens) is identical on every utterance, so
//! the worker decodes only the part of each new prompt that *differs* from the
//! one before (`clear_kv_cache_seq` wipes the divergent tail). The constant
//! instruction block is decoded once at boot and reused, cutting ~80 ms off the
//! cleanup leg of every dictation. The reuse is generic — it compares token
//! sequences, so it works for any model/template and degrades gracefully (a
//! cleanup→transform switch simply shares fewer tokens).

use crate::llm_contract::{self, CleanupPlan, ModelFamily, ModelIdentity, S1Axes};
use crate::prompts::Prompts;
use eyre::Context;
use llama_cpp_2::{
    context::params::LlamaContextParams,
    llama_backend::LlamaBackend,
    llama_batch::LlamaBatch,
    model::{params::LlamaModelParams, AddBos, LlamaChatMessage, LlamaChatTemplate, LlamaModel},
    sampling::LlamaSampler,
    token::LlamaToken,
};
use std::{
    num::NonZeroU32,
    path::Path,
    sync::{Arc, OnceLock},
};
use tokio::sync::{mpsc, oneshot};

mod slot;
mod worker;
use slot::*;
use worker::*;

// The cleanup + transform system prompts live in `prompts.rs` (built-in
// defaults, overridable via prompts.json / env). The engine loads them once at
// init into `self.prompts`.

// LlamaBackend can be initialised only once per process. Hold it in a OnceLock
// so multiple TextCleanupEngine instances share the same backend handle.
static BACKEND: OnceLock<Arc<LlamaBackend>> = OnceLock::new();

fn backend() -> eyre::Result<Arc<LlamaBackend>> {
    if let Some(b) = BACKEND.get() {
        return Ok(b.clone());
    }
    let b = Arc::new(LlamaBackend::init().wrap_err("LlamaBackend::init failed")?);
    let _ = BACKEND.set(b.clone());
    Ok(BACKEND.get().cloned().unwrap_or(b))
}

/// Upper bound on the cleanup context window. Gemma 3 1B trains to 32768, but
/// the KV cache scales with n_ctx, so we cap at 8192 (~6000 words) — far more
/// than any plausible single push-to-talk dictation. The persistent context is
/// allocated once at this size (one KV cache for the daemon's lifetime), so
/// memory is paid once rather than per utterance.
const MAX_CTX: u32 = 8192;

/// Largest selection (in chars) transform will take. Its output budget is 1536
/// tokens (~6000 chars of English); a longer selection could only come back
/// cut off, and a far longer one overflows the context window mid-decode.
pub const MAX_TRANSFORM_CHARS: usize = 6000;

/// Why `selection` cannot be transformed, or `None` when it can.
pub fn transform_input_problem(selection: &str) -> Option<String> {
    let n = selection.chars().count();
    (n > MAX_TRANSFORM_CHARS).then(|| {
        format!(
            "selection is too long to rewrite safely ({n} characters, limit {MAX_TRANSFORM_CHARS}); it was left unchanged"
        )
    })
}

/// Headroom kept free in the context window for generation.
const MIN_GENERATION_ROOM: usize = 64;

/// Why a prompt of `prompt_len` tokens cannot run, or `None`. Checked BEFORE
/// the KV cache is touched, so an oversize prompt fails cleanly instead of
/// overflowing the batch half-way through a decode.
fn prompt_len_problem(prompt_len: usize) -> Option<String> {
    let limit = (MAX_CTX as usize).saturating_sub(MIN_GENERATION_ROOM);
    (prompt_len > limit)
        .then(|| format!("prompt is {prompt_len} tokens; the cleanup model's limit is {limit}"))
}

/// Generation budget for one cleanup call, sized off the raw transcript with
/// headroom — cleanup tracks input length, and a fixed 256-token cap used to
/// chop dictation past ~190 words mid-sentence.
fn cleanup_budget(slot: &ChatSlot, raw: &str) -> i32 {
    (slot.token_estimate(raw) as f32 * 1.3) as i32 + 64
}

/// True when every word of `raw` is a filler interjection. Such input carries
/// no content to preserve, so an empty model output is valid rather than a
/// runaway that must fall back.
fn raw_is_filler_only(raw: &str) -> bool {
    const FILLERS: &[&str] = &[
        "um", "umm", "uhm", "uh", "uhh", "erm", "hmm", "hm", "mm", "mhm", "mmhmm", "ah",
        "eh", "er",
    ];
    let words: Vec<String> = raw
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .collect();
    !words.is_empty() && words.iter().all(|w| FILLERS.contains(&w.as_str()))
}

/// The actionable message when the primary model is S1-mini and no instruction
/// model is configured. It must name the setting, because the alternative —
/// silently ignoring a custom prompt or format — is the defect this fixes.
fn no_instruction_model_problem() -> String {
    "no instruction model is configured; S1-mini only normalizes, so selection \
     transforms and any custom prompt/format need a general chat model. Set the \
     instruction model (settings `instruction_model`, or env \
     DICTATE_INSTRUCTION_MODEL_PATH) to enable them."
        .to_string()
}

/// True when cleanup output looks like a model runaway and the caller should
/// fall back to the raw transcript: it emptied non-empty input, it ballooned far
/// beyond the input (cleanup never legitimately expands much — even list/email
/// presets restructure, they don't 4×), or it leaked a chat-template marker that
/// `text_polish::polish` failed to strip. Pure + cheap so it's always safe to run.
fn cleanup_ran_away(raw: &str, cleaned: &str) -> bool {
    let raw_chars = raw.chars().count();
    if raw_chars == 0 {
        return false;
    }
    let out_chars = cleaned.chars().count();
    if out_chars == 0 {
        return true; // dropped all content
    }
    if out_chars > raw_chars * 4 + 32 {
        return true; // ballooned — almost certainly hallucinated
    }
    let words: Vec<String> = cleaned
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|c: char| !c.is_alphanumeric())
                .to_ascii_lowercase()
        })
        .filter(|word| !word.is_empty())
        .collect();
    let mut run = 1usize;
    for pair in words.windows(2) {
        if pair[0] == pair[1] {
            run += 1;
            if run >= 4 {
                return true;
            }
        } else {
            run = 1;
        }
    }
    // Belt-and-suspenders past `polish`: any leaked chat-template marker.
    const MARKERS: &[&str] = &[
        "<|",
        "<start_of_turn>",
        "<end_of_turn>",
        "</s>",
        "<s>",
        "<think>",
        "<Think>",
    ];
    MARKERS.iter().any(|m| cleaned.contains(m))
}

pub struct TextCleanupEngine {
    /// The configured cleanup model. Its family decides how the engine routes:
    /// an S1-mini normalizer uses the trained contract, any other model follows
    /// free-form instructions.
    primary: ChatSlot,
    /// Optional general chat model for instructions S1-mini was not trained on
    /// (a custom prompt/format/tone, and selection transforms).
    instruction: Option<ChatSlot>,
    /// Why instructions are unavailable, or `None` when a slot can serve them.
    /// Exposed through [`Self::instruction_problem`] for boot/status.
    instruction_problem: Option<String>,
    /// Cleanup + transform system prompts, resolved once at init from
    /// env / prompts.json / built-in defaults.
    prompts: Prompts,
    /// The user's curated "known vocabulary" (the target spellings in
    /// corrections.json plus dictionary.json). Appended only on the generic
    /// instruction path; S1-mini's published contract forbids extra text.
    static_vocab: Vec<String>,
    /// The active output-format preset name, if any.
    active_format: Option<String>,
    /// True when a DEFINED output-format preset is applied. The daemon reads
    /// this (not the raw setting) to decide whether clean speech may skip the
    /// model, so the two can never disagree about an unknown preset name.
    format_active: bool,
    /// True when the active preset came from the user's `prompts.json` (a
    /// free-form instruction S1-mini cannot follow).
    format_is_custom: bool,
    /// True when the cleanup prompt came from env/file rather than the built-in
    /// default (a free-form instruction S1-mini cannot follow).
    cleanup_is_custom: bool,
    language: String,
}

impl TextCleanupEngine {
    /// Load the configured cleanup model. When that file is S1-mini the engine
    /// uses the publisher's trained normalizer contract; a general chat model
    /// gets free-form cleanup. Existing single-argument callers keep working.
    pub fn initialize<P: AsRef<Path>>(model_path: P) -> eyre::Result<Self> {
        Self::initialize_with_instruction_model(model_path, None)
    }

    /// Load the cleanup model and an optional instruction model.
    ///
    /// The instruction model serves selection transforms and any cleanup
    /// control (a custom prompt/format, an app tone outside S1-mini's styling
    /// registers) the primary model cannot follow. A failure to load it is NOT
    /// fatal: normal S1-mini dictation keeps working, an actionable error is
    /// exposed through [`Self::instruction_problem`], and no unsupported control
    /// is silently dropped.
    pub fn initialize_with_instruction_model(
        model_path: impl AsRef<Path>,
        instruction_model: Option<&Path>,
    ) -> eyre::Result<Self> {
        let backend = backend()?;
        // Prompts are read once at engine start; edits take effect on relaunch.
        let prompts = Prompts::load();

        // Resolve the active formatting preset (env > settings.json > none) and
        // pick its cleanup prompt, falling back to the default. An unknown name
        // just yields the default cleanup — see `Prompts::cleanup_for`.
        let settings = crate::settings::Settings::load();
        let active_format = settings.resolve_format();
        let language = settings.resolve_asr().language;
        let base_cleanup = prompts.cleanup_for(active_format.as_deref()).to_string();
        let format_active = prompts.format_is_defined(active_format.as_deref());
        let format_is_custom = active_format
            .as_deref()
            .is_some_and(|name| prompts.is_custom_format(name));
        if let Some(name) = &active_format {
            eprintln!(
                "[boot] format      {name} ({})",
                if format_active {
                    if format_is_custom {
                        "custom — needs the instruction model"
                    } else {
                        "applied"
                    }
                } else {
                    "unknown — using default cleanup"
                }
            );
        }

        // Curated vocabulary fed to a generic cleanup prompt: the target
        // spellings from corrections.json plus dictionary.json. S1-mini never
        // receives this — its published contract forbids extra instructions;
        // exact corrections still run outside the engine. Capped once at boot so
        // the block is byte-identical on every utterance (KV prefix reuse).
        let correction_targets = crate::corrections::Corrections::load_default()
            .map(|c| c.replacement_values())
            .unwrap_or_default();
        let dictionary = crate::dictionary::load_default();
        let curated_total = dictionary.len() + correction_targets.len();
        let static_vocab = crate::prompts::curated_vocabulary(&dictionary, &correction_targets);
        if curated_total > 0 {
            eprintln!(
                "[boot] vocab       {} of {curated_total} curated term(s) in the prompt + up to {} screen terms per utterance",
                static_vocab.len(),
                crate::prompts::SCREEN_VOCAB_SLOTS
            );
        }
        let cleanup_prompt = base_cleanup;

        // Primary cleanup model. `load` warms the worker from the detected
        // family: S1-mini with its exact trained system prompt, a generic model
        // with `cleanup_prompt` and the `Raw transcript:` framing.
        let primary = ChatSlot::load(
            &backend,
            model_path.as_ref(),
            "cleanup-worker",
            &cleanup_prompt,
        )?;
        eprintln!(
            "[boot] cleanup model {:?} · {}",
            primary.family,
            model_path.as_ref().display()
        );

        // Optional instruction model. When the primary model is already a
        // general instruction model it can serve instructions itself. A load
        // failure only narrows the routes; it never disables S1-mini dictation.
        let primary_is_general = primary.family != ModelFamily::S1Mini;
        let same_model = instruction_model.is_some_and(|path| {
            let first = std::fs::canonicalize(model_path.as_ref()).unwrap_or_else(|_| model_path.as_ref().to_path_buf());
            let second = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            first == second
        });
        let (instruction, instruction_problem) = match instruction_model {
            None if primary_is_general => (None, None),
            None => (None, Some(no_instruction_model_problem())),
            Some(_) if same_model && primary_is_general => (None, None),
            Some(_) if same_model => (None, Some(llm_contract::s1_transform_refusal())),
            Some(path) => match ChatSlot::load(&backend, path, "instruction-worker", &prompts.transform) {
                Ok(slot) if slot.family == ModelFamily::S1Mini => (
                    None, Some("the configured instruction model is a normalizer, not an instruction model".to_string()),
                ),
                Ok(slot) => {
                    eprintln!("[boot] instruction  {:?} · {}", slot.family, path.display());
                    (Some(slot), None)
                },
                Err(error) => {
                    eprintln!("[warn] instruction model failed to load: {error}");
                    (None, Some(format!("the instruction model failed to load: {error}")))
                },
            },
        };
        if let Some(problem) = &instruction_problem {
            eprintln!("[boot] instruction  unavailable — {problem}");
        }

        let cleanup_is_custom = prompts.cleanup_is_custom;
        Ok(Self {
            primary,
            instruction,
            instruction_problem,
            prompts,
            static_vocab,
            active_format,
            format_active,
            format_is_custom,
            cleanup_is_custom,
            language,
        })
    }

    /// True when a DEFINED output-format preset is applied. The daemon reads
    /// this (not the raw setting) to decide whether clean speech may skip the
    /// model, so the two can never disagree about an unknown preset name.
    pub fn format_active(&self) -> bool {
        self.format_active
    }

    /// Whether the engine can serve free-form instructions (selection
    /// transforms, custom prompts/formats, tones with no trained register).
    /// True when the primary model is a general model or an instruction model
    /// was loaded.
    pub fn supports_instructions(&self) -> bool {
        self.instruction_problem.is_none()
    }

    /// Why instructions are unavailable, or `None` when they are supported. For
    /// boot/status: a missing or failed instruction model is a clear, actionable
    /// state, never a silent one.
    pub fn instruction_problem(&self) -> Option<&str> {
        self.instruction_problem.as_deref()
    }

    /// Clean a raw dictation transcript for readability. Runs on the persistent
    /// cleanup worker thread (llama.cpp's decode loop is sync and CPU/GPU bound).
    pub async fn process_transcript(&self, raw_text: &str) -> eyre::Result<String> {
        self.process_transcript_with_vocab(raw_text, &[]).await
    }

    /// Like [`Self::process_transcript`] but with extra per-utterance vocabulary
    /// (proper nouns harvested from the focused window — see
    /// [`crate::screen_context`]). The curated corrections vocabulary and these
    /// screen terms are combined into one "known vocabulary" block appended
    /// after the cached instruction prefix, so the model preserves on-screen
    /// spellings instead of substituting similar-sounding common words. The
    /// instruction prefix is still reused from the KV cache every time; only the
    /// vocabulary block (when it changes) and the transcript are re-decoded.
    pub async fn process_transcript_with_vocab(
        &self,
        raw_text: &str,
        screen_vocab: &[String],
    ) -> eyre::Result<String> {
        self.process_transcript_with_vocab_tone(raw_text, screen_vocab, None)
            .await
    }

    /// Like [`Self::process_transcript_with_vocab`], but with a per-app tone
    /// hint. S1-mini maps the tone onto its trained styling axes; a tone with no
    /// trained register routes to the instruction model (or fails clearly).
    /// General models keep the legacy free-form tone suffix.
    pub async fn process_transcript_with_vocab_tone(
        &self,
        raw_text: &str,
        screen_vocab: &[String],
        tone: Option<&str>,
    ) -> eyre::Result<String> {
        let raw = raw_text.trim();
        if raw.is_empty() {
            return Ok(String::new());
        }
        match self.primary.family {
            ModelFamily::S1Mini => match self.cleanup_plan(tone) {
                CleanupPlan::S1(axes) => self.run_s1_cleanup(raw, axes).await,
                CleanupPlan::Instruction { .. } => {
                    let slot = self.instruction_slot()?;
                    self.run_generic_cleanup(slot, raw, screen_vocab, tone).await
                }
                CleanupPlan::Refuse { reason } => Err(eyre::eyre!(reason)),
            },
            ModelFamily::QwenChat | ModelFamily::GenericInstruct => {
                self.run_generic_cleanup(&self.primary, raw, screen_vocab, tone)
                    .await
            }
        }
    }

    /// The routing decision for one S1-mini cleanup call.
    fn cleanup_plan(&self, tone: Option<&str>) -> CleanupPlan {
        llm_contract::plan_cleanup(
            &llm_contract::CleanupControls {
                format: self.active_format.as_deref(),
                format_is_custom: self.format_is_custom,
                format_is_defined: self.format_active,
                cleanup_prompt_is_custom: self.cleanup_is_custom,
                tone,
                language: &self.language,
            },
            self.supports_instructions(),
        )
    }

    /// The instruction model, or an actionable error explaining why no
    /// configured model can serve a free-form instruction.
    fn instruction_slot(&self) -> eyre::Result<&ChatSlot> {
        if let Some(slot) = self.instruction.as_ref() {
            return Ok(slot);
        }
        if self.instruction_problem.is_none() && self.primary.family != ModelFamily::S1Mini {
            return Ok(&self.primary);
        }
        Err(eyre::eyre!("{}", self.instruction_problem.as_deref().unwrap_or("no instruction model is configured")))
    }

    /// Run S1-mini's exact trained contract for one transcript.
    async fn run_s1_cleanup(&self, raw: &str, axes: S1Axes) -> eyre::Result<String> {
        let user = llm_contract::s1_user_message(axes, raw);
        let budget = cleanup_budget(&self.primary, raw);
        let cleaned = self
            .primary
            .run(
                llm_contract::S1_SYSTEM_PROMPT.to_string(),
                user,
                budget,
                true,
                false,
                false,
                llm_contract::S1_TEMPERATURE,
            )
            .await?;
        Ok(self.finish_cleanup(raw, &cleaned))
    }

    /// Run a free-form cleanup instruction on a general model.
    async fn run_generic_cleanup(
        &self,
        slot: &ChatSlot,
        raw: &str,
        screen_vocab: &[String],
        tone: Option<&str>,
    ) -> eyre::Result<String> {
        let system = self.build_generic_prompt(screen_vocab, tone);
        let cleaned = slot
            .run(
                system,
                format!("Raw transcript:\n{raw}"),
                cleanup_budget(slot, raw),
                true,
                false,
                false,
                llm_contract::GENERIC_TEMPERATURE,
            )
            .await?;
        Ok(self.finish_cleanup(raw, &cleaned))
    }

    /// Build the free-form cleanup system prompt: the active format/cleanup
    /// prompt, the curated + screen vocabulary, and an optional tone directive.
    fn build_generic_prompt(&self, screen_vocab: &[String], tone: Option<&str>) -> String {
        let mut prompt_body = self
            .prompts
            .cleanup_for(self.active_format.as_deref())
            .to_string();
        // Curated corrections first (high-trust, constant → cached prefix), then
        // the screen-harvested terms in their reserved tail slots.
        if !self.static_vocab.is_empty() || !screen_vocab.is_empty() {
            let combined = crate::prompts::combined_vocabulary(&self.static_vocab, screen_vocab);
            if let Some(suffix) = crate::prompts::vocabulary_suffix(&combined) {
                prompt_body.push_str(&suffix);
            }
        }
        // Per-app tone directive: after the vocabulary block so it steers the
        // style of the whole cleanup. `None` leaves the prompt untouched.
        if let Some(tone) = tone {
            prompt_body.push_str(&crate::app_tones::tone_suffix(tone));
        }
        prompt_body
    }

    /// Shared post-pass for a cleanup result: deterministic speech mechanics,
    /// list/prose shaping, the filler-only empty allowance, and the runaway
    /// guard.
    fn finish_cleanup(&self, raw: &str, cleaned: &str) -> String {
        let cleaned = crate::text_polish::fix_speech_mechanics(cleaned);
        let cleaned = if self.format_active || crate::text_polish::looks_like_list(&cleaned) {
            cleaned
        } else {
            crate::text_polish::flatten_to_line(&cleaned)
        };
        // Filler-only speech correctly normalizes to an empty string; that is a
        // valid result, not a runaway. Any other empty output from non-empty
        // input still trips the guard below.
        if cleaned.trim().is_empty() && raw_is_filler_only(raw) {
            return String::new();
        }
        let deterministic = crate::text_polish::fix_speech_mechanics(raw);
        // Runaway guard: a small model can occasionally hallucinate — drop
        // everything, balloon a short utterance into paragraphs, or leak
        // chat-template markers past `polish`. Cleanup never legitimately
        // expands much, so on any of those fall back to the raw transcript
        // rather than inject garbage.
        if cleanup_ran_away(raw, &cleaned) {
            eprintln!(
                "[warn] cleanup runaway (raw {} → {} chars); using raw transcript",
                raw.chars().count(),
                cleaned.chars().count()
            );
            return deterministic;
        }
        if cleaned.trim() == raw && deterministic != raw {
            deterministic
        } else {
            cleaned
        }
    }

    /// Evaluate an arbitrary candidate **cleanup** system prompt against a raw
    /// transcript, using the exact framing + budget the production cleanup path
    /// uses. The model is already loaded, so the prompt-lab harness
    /// (`examples/prompt_lab.rs`) can A/B many candidate prompts over a corpus
    /// without paying the model-load cost per candidate. `preserve_newlines`
    /// lets the lab exercise list-style presets (numbered / bullets), which
    /// must keep their line breaks — see [`Self::process_transcript`], which
    /// flattens for single-utterance dictation.
    pub async fn eval_cleanup(
        &self,
        cleanup_prompt: &str,
        raw: &str,
        preserve_newlines: bool,
    ) -> eyre::Result<String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(String::new());
        }
        let budget = cleanup_budget(&self.primary, raw);
        let cleaned = self
            .primary
            .run(
                cleanup_prompt.to_string(),
                format!("Raw transcript:\n{raw}"),
                budget,
                preserve_newlines,
                false,
                false,
                llm_contract::GENERIC_TEMPERATURE,
            )
            .await?;
        // Mirror production: apply the same deterministic speech-mechanics pass
        // so the lab measures what the daemon would actually inject.
        let cleaned = crate::text_polish::fix_speech_mechanics(&cleaned);
        let deterministic = crate::text_polish::fix_speech_mechanics(raw);
        if cleanup_ran_away(raw, &cleaned) {
            Ok(deterministic)
        } else if cleaned.trim() == raw && deterministic != raw {
            Ok(deterministic)
        } else {
            Ok(cleaned)
        }
    }

    /// Like [`Self::eval_cleanup`] but forces a full prefill (no KV reuse). Used
    /// by the prefix-cache parity test to prove cached output is bit-identical
    /// to a from-scratch decode.
    pub async fn eval_cleanup_uncached(
        &self,
        cleanup_prompt: &str,
        raw: &str,
        preserve_newlines: bool,
    ) -> eyre::Result<String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(String::new());
        }
        let budget = cleanup_budget(&self.primary, raw);
        let cleaned = self
            .primary
            .run(
                cleanup_prompt.to_string(),
                format!("Raw transcript:\n{raw}"),
                budget,
                preserve_newlines,
                true,
                false,
                llm_contract::GENERIC_TEMPERATURE,
            )
            .await?;
        let cleaned = crate::text_polish::fix_speech_mechanics(&cleaned);
        let deterministic = crate::text_polish::fix_speech_mechanics(raw);
        if cleanup_ran_away(raw, &cleaned) {
            Ok(deterministic)
        } else if cleaned.trim() == raw && deterministic != raw {
            Ok(deterministic)
        } else {
            Ok(cleaned)
        }
    }

    /// Evaluate an arbitrary candidate **transform** system prompt against an
    /// instruction + selection, mirroring [`Self::transform`]'s framing and
    /// budget. Companion to [`Self::eval_cleanup`] for the prompt-lab harness.
    pub async fn eval_transform(
        &self,
        transform_prompt: &str,
        instruction: &str,
        selection: &str,
    ) -> eyre::Result<String> {
        let instruction = instruction.trim();
        if instruction.is_empty() || selection.is_empty() {
            return Ok(selection.to_string());
        }
        let user_msg = format!("Instruction: {instruction}\n\nText:\n{selection}");
        let budget = (selection.chars().count() + 256).clamp(256, 1536) as i32;
        self.instruction_slot()?
            .run(
                transform_prompt.to_string(),
                user_msg,
                budget,
                true,
                false,
                false,
                llm_contract::GENERIC_TEMPERATURE,
            )
            .await
    }

    /// Apply a spoken `instruction` to `selection` and return the edited text.
    /// This is the transform mode (Shift + push-to-talk): the user selects
    /// text, speaks an instruction ("make this concise", "translate to
    /// Spanish", "turn into bullet points"), and the model rewrites it in
    /// place.
    ///
    /// An instruction-capable slot serves it: the primary model when it is a
    /// general chat model, otherwise the configured instruction model. With
    /// only an S1-mini normalizer loaded this returns an actionable error — it
    /// never pretends a normalizer can follow a free-form instruction.
    pub async fn transform(&self, instruction: &str, selection: &str) -> eyre::Result<String> {
        let instruction = instruction.trim();
        if instruction.is_empty() || selection.is_empty() {
            return Ok(selection.to_string());
        }
        if let Some(problem) = transform_input_problem(selection) {
            return Err(eyre::eyre!(problem));
        }
        let slot = self.instruction_slot()?;
        let user_msg = format!("Instruction: {instruction}\n\nText:\n{selection}");
        // Give the output room for a rewrite that's somewhat longer than the
        // input (e.g. "expand this"), capped to keep within the context window.
        let budget = (selection.chars().count() + 256).clamp(256, 1536) as i32;
        // Preserve line structure: transforms may legitimately be lists/paras.
        // The result replaces the selection, so a rewrite that stopped at the
        // token budget (not at the model's own end) is an error, not a result.
        slot.run(
            self.prompts.transform.clone(),
            user_msg,
            budget,
            true,
            false,
            true,
            llm_contract::GENERIC_TEMPERATURE,
        )
        .await
    }
}

#[cfg(test)]
mod tests;
