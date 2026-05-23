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
use std::{
    num::NonZeroU32,
    path::Path,
    sync::{Arc, OnceLock},
};

const SYSTEM_INSTRUCTION: &str =
    "You are a real-time dictation cleaner. Edit raw speech-to-text for \
     written-text readability:\n\
     - Remove disfluencies: uh, um, er, ah, hmm; 'like' only when used \
       as filler; 'you know' only when used as filler.\n\
     - Expand colloquial contractions to their written form: \
       wanna → want to, gonna → going to, kinda → kind of, sorta → sort of, \
       gimme → give me, lemme → let me, dunno → don't know, outta → out of, \
       lotta → lot of, shoulda → should have, coulda → could have, \
       woulda → would have, y'all → you all.\n\
     - KEEP standard contractions as-is: don't, it's, won't, can't, I'm, \
       I'll, we're, etc. — do not over-formalize.\n\
     - Fix punctuation and capitalization.\n\
     - Preserve domain-specific casing: Rust, macOS, ONNX, GitHub, Parakeet, \
       iOS, Apple Silicon, etc.\n\
     - Do NOT rephrase, paraphrase, summarize, or add words the speaker \
       didn't say. Preserve the speaker's voice and word choice.\n\
     Output ONLY the cleaned text — no preamble, no commentary, no quotes, \
     no markdown.";

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
        })
    }

    /// Run cleanup on a Tokio blocking thread (llama.cpp's decode loop is sync
    /// and CPU/GPU bound).
    pub async fn process_transcript(&self, raw_text: &str) -> eyre::Result<String> {
        let raw = raw_text.trim();
        if raw.is_empty() {
            return Ok(String::new());
        }

        // Build the chat using the model's own bundled template. Works for
        // Gemma / Llama 3 / Qwen-ChatML / SmolLM without per-family hardcoding.
        let user_msg = format!("{SYSTEM_INSTRUCTION}\n\nRaw transcript:\n{raw}");
        let messages = vec![LlamaChatMessage::new("user".into(), user_msg)
            .map_err(|e| eyre::eyre!("invalid chat message: {e:?}"))?];
        let prompt = self
            .model
            .apply_chat_template(&self.chat_template, &messages, true)
            .map_err(|e| eyre::eyre!("apply_chat_template failed: {e:?}"))?;

        let backend = self.backend.clone();
        let model = self.model.clone();
        let max_tokens = self.max_tokens;

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
            // collapse whitespace, drop chat-template artefacts.
            Ok(crate::text_polish::polish(&out))
        })
        .await
        .map_err(|e| eyre::eyre!("blocking task join failed: {e}"))?
    }
}
