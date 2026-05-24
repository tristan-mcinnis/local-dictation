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

pub struct TextCleanupEngine {
    backend: Arc<LlamaBackend>,
    model: Arc<LlamaModel>,
    chat_template: Arc<LlamaChatTemplate>,
    max_tokens: i32,
    /// Cleanup + transform system prompts, resolved once at init from
    /// env / prompts.json / built-in defaults.
    prompts: Prompts,
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
        Ok(Self {
            backend,
            model: Arc::new(model),
            chat_template: Arc::new(chat_template),
            max_tokens: 256,
            prompts: Prompts::load(),
        })
    }

    /// Clean a raw dictation transcript for readability. Runs on a Tokio
    /// blocking thread (llama.cpp's decode loop is sync and CPU/GPU bound).
    pub async fn process_transcript(&self, raw_text: &str) -> eyre::Result<String> {
        let raw = raw_text.trim();
        if raw.is_empty() {
            return Ok(String::new());
        }
        let user_msg = format!("{}\n\nRaw transcript:\n{raw}", self.prompts.cleanup);
        self.run_chat(user_msg, self.max_tokens, false).await
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
            let ctx_params = LlamaContextParams::default()
                .with_n_ctx(NonZeroU32::new(2048));
            let mut ctx = model
                .new_context(&backend, ctx_params)
                .map_err(|e| eyre::eyre!("new_context failed: {e:?}"))?;

            let tokens = model
                .str_to_token(&prompt, AddBos::Always)
                .map_err(|e| eyre::eyre!("tokenize failed: {e:?}"))?;
            if tokens.is_empty() {
                return Ok(String::new());
            }

            let mut batch = LlamaBatch::new(2048, 1);
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
