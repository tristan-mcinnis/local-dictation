//! Async Gemma 3 cleanup engine via llama-cpp-2 (Metal backend on Apple Silicon).
//!
//! The spec referenced `llama_cpp = "0.3.0"`, but that crate ships a llama.cpp
//! from April 2024 — too old to recognise the Gemma 3 architecture. We use
//! llama-cpp-2 which tracks current llama.cpp.

use eyre::Context;
use llama_cpp_2::{
    context::params::LlamaContextParams,
    llama_backend::LlamaBackend,
    llama_batch::LlamaBatch,
    model::{params::LlamaModelParams, AddBos, LlamaChatMessage, LlamaChatTemplate, LlamaModel},
    sampling::LlamaSampler,
};
use crate::prompts::Prompts;
use std::{
    num::NonZeroU32,
    path::Path,
    sync::{Arc, OnceLock},
};

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
/// than any plausible single push-to-talk dictation, while keeping memory sane.
const MAX_CTX: u32 = 8192;
/// Floor for the context window / generation budget. Keeps short utterances on
/// the same fast path they had before (small KV cache, quick reserve).
const MIN_CTX: u32 = 2048;

pub struct TextCleanupEngine {
    backend: Arc<LlamaBackend>,
    model: Arc<LlamaModel>,
    chat_template: Arc<LlamaChatTemplate>,
    /// Cleanup + transform system prompts, resolved once at init from
    /// env / prompts.json / built-in defaults.
    prompts: Prompts,
    /// The effective cleanup system prompt: the default (or active formatting
    /// preset) cleanup prompt with the personal "known vocabulary" suffix
    /// already appended. Built once at init so the hot path just formats the
    /// transcript onto it.
    cleanup_prompt: String,
    /// True when an output-format preset (numbered / bullets / email / …) is
    /// active. List-style presets need their line breaks preserved, so the
    /// cleanup path keeps newlines instead of flattening to one line as plain
    /// single-utterance dictation does.
    format_active: bool,
}

impl TextCleanupEngine {
    pub fn initialize<P: AsRef<Path>>(model_path: P) -> eyre::Result<Self> {
        let backend = backend()?;
        // Offload everything to Metal. llama.cpp ignores layers beyond what
        // the model actually has, so a large number is safe.
        let model_params = LlamaModelParams::default().with_n_gpu_layers(1000);
        let model = LlamaModel::load_from_file(&backend, model_path.as_ref(), &model_params)
            .map_err(|e| eyre::eyre!("LlamaModel::load_from_file failed: {e:?}"))?;
        // Use the model's bundled chat template (Gemma / ChatML / Llama 3 /
        // whatever the GGUF declares). Falls back to a no-op format if the
        // model has no template — in practice all instruct GGUFs do.
        let chat_template = model
            .chat_template(None)
            .map_err(|e| eyre::eyre!("chat_template lookup failed: {e:?}"))?;
        let prompts = Prompts::load();

        // Resolve the active formatting preset (env > settings.json > none) and
        // pick its cleanup prompt, falling back to the default. An unknown name
        // just yields the default cleanup — see `Prompts::cleanup_for`.
        let active_format = crate::settings::Settings::load().resolve_format();
        let base_cleanup = prompts.cleanup_for(active_format.as_deref()).to_string();
        let mut format_active = false;
        if let Some(name) = &active_format {
            format_active = prompts.format_names().iter().any(|f| f.eq_ignore_ascii_case(name));
            eprintln!(
                "[boot] format      {name} ({})",
                if format_active { "applied" } else { "unknown — using default cleanup" }
            );
        }

        // Dictionary-aware cleanup: append the user's known vocabulary (the
        // target spellings in corrections.json) so the model stops "correcting"
        // proper nouns and domain casing away. Best-effort — a missing or
        // unreadable corrections file just means no suffix.
        let vocab = crate::corrections::Corrections::load_default()
            .map(|c| c.replacement_values())
            .unwrap_or_default();
        let mut cleanup_prompt = base_cleanup;
        if let Some(suffix) = crate::prompts::vocabulary_suffix(&vocab) {
            eprintln!("[boot] vocab       {} term(s) fed to cleanup prompt", vocab.len().min(crate::prompts::MAX_VOCAB_TERMS));
            cleanup_prompt.push_str(&suffix);
        }

        Ok(Self {
            backend,
            model: Arc::new(model),
            chat_template: Arc::new(chat_template),
            prompts,
            cleanup_prompt,
            format_active,
        })
    }

    /// Clean a raw dictation transcript for readability. Runs on a Tokio
    /// blocking thread (llama.cpp's decode loop is sync and CPU/GPU bound).
    pub async fn process_transcript(&self, raw_text: &str) -> eyre::Result<String> {
        let raw = raw_text.trim();
        if raw.is_empty() {
            return Ok(String::new());
        }
        let user_msg = format!("{}\n\nRaw transcript:\n{raw}", self.cleanup_prompt);
        // Cleanup never summarizes (the prompt forbids it), so the cleaned text
        // tracks the input length — stripping filler makes it a touch shorter,
        // expanding contractions a touch longer. Budget off the raw transcript
        // with headroom instead of a fixed 256-token cap, which used to chop any
        // dictation past ~190 words mid-sentence.
        let budget = (self.token_estimate(raw) as f32 * 1.3) as i32 + 64;
        // Plain dictation flattens to one line; an active format preset
        // (numbered / bullets / email) keeps its line structure.
        let cleaned = self.run_chat(user_msg, budget, self.format_active).await?;
        // Deterministic mechanics the 1B does unreliably (leading "um", lone
        // lowercase "i") — cheap, exact, and always safe on speech.
        Ok(crate::text_polish::fix_speech_mechanics(&cleaned))
    }

    /// Rough token count for sizing generation budgets / the context window.
    /// Uses the model's own tokenizer; falls back to a chars/3 estimate if
    /// tokenization fails (never on the hot path, but keeps this infallible).
    fn token_estimate(&self, text: &str) -> usize {
        self.model
            .str_to_token(text, AddBos::Never)
            .map(|t| t.len())
            .unwrap_or_else(|_| text.chars().count() / 3)
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
        let user_msg = format!("{cleanup_prompt}\n\nRaw transcript:\n{raw}");
        let budget = (self.token_estimate(raw) as f32 * 1.3) as i32 + 64;
        let cleaned = self.run_chat(user_msg, budget, preserve_newlines).await?;
        // Mirror production: apply the same deterministic speech-mechanics pass
        // so the lab measures what the daemon would actually inject.
        Ok(crate::text_polish::fix_speech_mechanics(&cleaned))
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
        let user_msg = format!(
            "{transform_prompt}\n\nInstruction: {instruction}\n\nText:\n{selection}"
        );
        let budget = (selection.chars().count() + 256).clamp(256, 1536) as i32;
        self.run_chat(user_msg, budget, true).await
    }

    /// Apply a spoken `instruction` to `selection` and return the edited text.
    /// This is the transform mode (Shift + push-to-talk): the user selects
    /// text, speaks an instruction ("make this concise", "translate to
    /// Spanish", "turn into bullet points"), and the model rewrites it in
    /// place. Same warm Gemma engine as cleanup, different prompt.
    pub async fn transform(&self, instruction: &str, selection: &str) -> eyre::Result<String> {
        let instruction = instruction.trim();
        if instruction.is_empty() || selection.is_empty() {
            return Ok(selection.to_string());
        }
        let user_msg = format!(
            "{}\n\nInstruction: {instruction}\n\nText:\n{selection}",
            self.prompts.transform
        );
        // Give the output room for a rewrite that's somewhat longer than the
        // input (e.g. "expand this"), capped to keep within the context window.
        let budget = (selection.chars().count() + 256).clamp(256, 1536) as i32;
        // Preserve line structure: transforms may legitimately be lists/paras.
        self.run_chat(user_msg, budget, true).await
    }

    /// Shared generation core: build the prompt with the model's bundled chat
    /// template, decode up to `max_tokens`, and polish the result. Works for
    /// Gemma / Llama 3 / Qwen-ChatML / SmolLM without per-family hardcoding.
    async fn run_chat(
        &self,
        user_msg: String,
        max_tokens: i32,
        preserve_newlines: bool,
    ) -> eyre::Result<String> {
        let messages = vec![LlamaChatMessage::new("user".into(), user_msg)
            .map_err(|e| eyre::eyre!("invalid chat message: {e:?}"))?];
        let prompt = self
            .model
            .apply_chat_template(&self.chat_template, &messages, true)
            .map_err(|e| eyre::eyre!("apply_chat_template failed: {e:?}"))?;

        let backend = self.backend.clone();
        let model = self.model.clone();

        tokio::task::spawn_blocking(move || -> eyre::Result<String> {
            let tokens = model
                .str_to_token(&prompt, AddBos::Always)
                .map_err(|e| eyre::eyre!("tokenize failed: {e:?}"))?;
            if tokens.is_empty() {
                return Ok(String::new());
            }

            // Size the context to hold the full prompt plus the generation
            // budget, clamped to [MIN_CTX, MAX_CTX]. A fixed 2048 used to
            // truncate long dictations: once prompt + output exceeded it, the
            // tail fell out of context. If the prompt alone is near the cap we
            // still leave room to generate by shrinking the effective budget.
            let prompt_len = tokens.len() as u32;
            let n_ctx = (prompt_len + max_tokens.max(0) as u32 + 16).clamp(MIN_CTX, MAX_CTX);
            // Never ask for more output tokens than the window can hold.
            let max_tokens = max_tokens.min(n_ctx.saturating_sub(prompt_len + 16) as i32);
            let ctx_params = LlamaContextParams::default()
                .with_n_ctx(NonZeroU32::new(n_ctx))
                // n_batch must cover the prompt prefill in one decode call.
                .with_n_batch(n_ctx);
            let mut ctx = model
                .new_context(&backend, ctx_params)
                .map_err(|e| eyre::eyre!("new_context failed: {e:?}"))?;

            let mut batch = LlamaBatch::new(n_ctx as usize, 1);
            let last_idx = (tokens.len() - 1) as i32;
            for (i, tok) in tokens.iter().enumerate() {
                batch
                    .add(*tok, i as i32, &[0], i as i32 == last_idx)
                    .map_err(|e| eyre::eyre!("batch.add prompt token failed: {e:?}"))?;
            }
            ctx.decode(&mut batch)
                .map_err(|e| eyre::eyre!("decode prompt failed: {e:?}"))?;

            // Low temperature for deterministic cleanup. greedy fallback at end of chain.
            let mut sampler = LlamaSampler::chain_simple([
                LlamaSampler::temp(0.2),
                LlamaSampler::dist(1234),
            ]);

            let mut decoder = encoding_rs::UTF_8.new_decoder();
            let mut out = String::new();
            let mut n_cur = batch.n_tokens();
            let mut decoded_count = 0i32;

            while decoded_count < max_tokens {
                let tok = sampler.sample(&ctx, batch.n_tokens() - 1);
                sampler.accept(tok);
                if model.is_eog_token(tok) {
                    break;
                }
                let piece = model
                    .token_to_piece(tok, &mut decoder, true, None)
                    .map_err(|e| eyre::eyre!("token_to_piece failed: {e:?}"))?;
                out.push_str(&piece);

                batch.clear();
                batch
                    .add(tok, n_cur, &[0], true)
                    .map_err(|e| eyre::eyre!("batch.add gen token failed: {e:?}"))?;
                n_cur += 1;
                ctx.decode(&mut batch)
                    .map_err(|e| eyre::eyre!("decode gen step failed: {e:?}"))?;
                decoded_count += 1;
            }

            // Defensive polish: strip preamble, peel wrapping quotes,
            // collapse whitespace, drop chat-template artefacts. Transform
            // mode keeps line breaks (lists/paragraphs); cleanup flattens them.
            Ok(if preserve_newlines {
                crate::text_polish::polish_multiline(&out)
            } else {
                crate::text_polish::polish(&out)
            })
        })
        .await
        .map_err(|e| eyre::eyre!("blocking task join failed: {e}"))?
    }
}
