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

use eyre::Context;
use llama_cpp_2::{
    context::params::LlamaContextParams,
    llama_backend::LlamaBackend,
    llama_batch::LlamaBatch,
    model::{params::LlamaModelParams, AddBos, LlamaChatMessage, LlamaChatTemplate, LlamaModel},
    sampling::LlamaSampler,
    token::LlamaToken,
};
use crate::prompts::Prompts;
use std::{
    num::NonZeroU32,
    path::Path,
    sync::{Arc, OnceLock},
};
use tokio::sync::{mpsc, oneshot};

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

/// A unit of work for the cleanup worker thread: a fully-templated prompt plus
/// the generation parameters and a channel to return the polished result on.
struct Job {
    /// The prompt already run through the model's chat template.
    prompt: String,
    /// Max tokens to generate.
    max_tokens: i32,
    /// Cleanup flattens to one line; transform / list presets keep newlines.
    preserve_newlines: bool,
    /// Force a full prefill (ignore the resident KV prefix). Used only by the
    /// parity test to prove cached output == uncached output.
    no_reuse: bool,
    reply: oneshot::Sender<eyre::Result<String>>,
}

/// Longest common prefix length of two token slices.
fn common_prefix_len(a: &[LlamaToken], b: &[LlamaToken]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

pub struct TextCleanupEngine {
    model: Arc<LlamaModel>,
    chat_template: Arc<LlamaChatTemplate>,
    /// Cleanup + transform system prompts, resolved once at init from
    /// env / prompts.json / built-in defaults.
    prompts: Prompts,
    /// The cleanup system prompt instructions (default or active formatting
    /// preset), WITHOUT any vocabulary suffix. This is the constant, cacheable
    /// prefix the worker warms once at boot; per-utterance vocabulary is
    /// appended after it (see [`Self::process_transcript_with_vocab`]).
    cleanup_prompt: String,
    /// The user's curated "known vocabulary" (the target spellings in
    /// corrections.json). Combined with any per-utterance screen-harvested
    /// terms into the vocabulary block appended to each cleanup prompt.
    static_vocab: Vec<String>,
    /// True when an output-format preset (numbered / bullets / email / …) is
    /// active. List-style presets need their line breaks preserved, so the
    /// cleanup path keeps newlines instead of flattening to one line as plain
    /// single-utterance dictation does.
    format_active: bool,
    /// Hand work to the persistent-context worker thread. `Option` so `Drop`
    /// can close the channel (stopping the worker) before joining it.
    job_tx: Option<mpsc::UnboundedSender<Job>>,
    /// The worker thread handle, joined on drop so its `LlamaContext` (and the
    /// Metal buffers it holds) is freed before the process tears down ggml's
    /// global Metal device — otherwise `ggml_metal_device_free` asserts at exit.
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Drop for TextCleanupEngine {
    fn drop(&mut self) {
        // Close the channel so the worker's blocking_recv returns None and it
        // drops its context, then wait for it. Ordering matters: the context
        // must die before ggml's metal device global destructor runs at exit.
        self.job_tx.take();
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

impl TextCleanupEngine {
    pub fn initialize<P: AsRef<Path>>(model_path: P) -> eyre::Result<Self> {
        let backend = backend()?;
        // Offload everything to Metal. llama.cpp ignores layers beyond what
        // the model actually has, so a large number is safe.
        let model_params = LlamaModelParams::default().with_n_gpu_layers(1000);
        let model = Arc::new(
            LlamaModel::load_from_file(&backend, model_path.as_ref(), &model_params)
                .map_err(|e| eyre::eyre!("LlamaModel::load_from_file failed: {e:?}"))?,
        );
        // Use the model's bundled chat template (Gemma / ChatML / Llama 3 /
        // whatever the GGUF declares). Falls back to a no-op format if the
        // model has no template — in practice all instruct GGUFs do.
        let chat_template = Arc::new(
            model
                .chat_template(None)
                .map_err(|e| eyre::eyre!("chat_template lookup failed: {e:?}"))?,
        );
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
        // Curated vocabulary fed to the cleanup prompt: the target spellings
        // from corrections.json (from→to fixes) plus the flat dictionary.json
        // list (known words to preserve as-is). Both are soft hints; dedup +
        // cap happen in `vocabulary_suffix`.
        let mut static_vocab = crate::corrections::Corrections::load_default()
            .map(|c| c.replacement_values())
            .unwrap_or_default();
        static_vocab.extend(crate::dictionary::load_default());
        if !static_vocab.is_empty() {
            eprintln!(
                "[boot] vocab       {} curated term(s) + per-utterance screen terms",
                static_vocab.len().min(crate::prompts::MAX_VOCAB_TERMS)
            );
        }
        let cleanup_prompt = base_cleanup;

        // Spawn the worker that owns the persistent context. Warm it with the
        // cleanup *instructions* + framing (no vocabulary, empty transcript) so
        // the constant instruction block is in the KV cache before the first
        // dictation. Vocabulary is appended per-utterance after this prefix, so
        // the worker still reuses the warmed instructions every time.
        let warm_prompt = Self::template_user_msg(
            &model,
            &chat_template,
            &format!("{cleanup_prompt}\n\nRaw transcript:\n"),
        )
        .ok();

        let (job_tx, job_rx) = mpsc::unbounded_channel::<Job>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<eyre::Result<()>>();
        let worker = {
            let backend = backend.clone();
            let model = model.clone();
            std::thread::Builder::new()
                .name("gemma-cleanup".into())
                .spawn(move || run_worker(backend, model, job_rx, warm_prompt, ready_tx))
                .map_err(|e| eyre::eyre!("spawn cleanup worker failed: {e}"))?
        };
        // Block until the worker has built its context (boot-time, so blocking
        // is fine). Propagate a context-creation failure as an init error.
        ready_rx
            .recv()
            .map_err(|_| eyre::eyre!("cleanup worker died during init"))??;

        Ok(Self {
            model,
            chat_template,
            prompts,
            cleanup_prompt,
            static_vocab,
            format_active,
            job_tx: Some(job_tx),
            worker: Some(worker),
        })
    }

    /// Apply the model's chat template to a single user message. Shared by the
    /// hot path and the boot-time warm prefill so they tokenize identically.
    fn template_user_msg(
        model: &LlamaModel,
        chat_template: &LlamaChatTemplate,
        user_msg: &str,
    ) -> eyre::Result<String> {
        let messages = vec![LlamaChatMessage::new("user".into(), user_msg.into())
            .map_err(|e| eyre::eyre!("invalid chat message: {e:?}"))?];
        model
            .apply_chat_template(chat_template, &messages, true)
            .map_err(|e| eyre::eyre!("apply_chat_template failed: {e:?}"))
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
        let raw = raw_text.trim();
        if raw.is_empty() {
            return Ok(String::new());
        }
        let mut prompt_body = self.cleanup_prompt.clone();
        // Curated corrections first (high-trust), then screen-harvested terms.
        // `vocabulary_suffix` dedups case-insensitively (first-seen casing wins)
        // and caps the total, so a noisy screen can't balloon the prompt.
        if !self.static_vocab.is_empty() || !screen_vocab.is_empty() {
            let mut combined =
                Vec::with_capacity(self.static_vocab.len() + screen_vocab.len());
            combined.extend_from_slice(&self.static_vocab);
            combined.extend_from_slice(screen_vocab);
            if let Some(suffix) = crate::prompts::vocabulary_suffix(&combined) {
                prompt_body.push_str(&suffix);
            }
        }
        let user_msg = format!("{prompt_body}\n\nRaw transcript:\n{raw}");
        // Cleanup never summarizes (the prompt forbids it), so the cleaned text
        // tracks the input length — stripping filler makes it a touch shorter,
        // expanding contractions a touch longer. Budget off the raw transcript
        // with headroom instead of a fixed 256-token cap, which used to chop any
        // dictation past ~190 words mid-sentence.
        let budget = (self.token_estimate(raw) as f32 * 1.3) as i32 + 64;
        // Plain dictation flattens to one line; an active format preset
        // (numbered / bullets / email) keeps its line structure.
        let cleaned = self.run_chat(user_msg, budget, self.format_active, false).await?;
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
        let cleaned = self.run_chat(user_msg, budget, preserve_newlines, false).await?;
        // Mirror production: apply the same deterministic speech-mechanics pass
        // so the lab measures what the daemon would actually inject.
        Ok(crate::text_polish::fix_speech_mechanics(&cleaned))
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
        let user_msg = format!("{cleanup_prompt}\n\nRaw transcript:\n{raw}");
        let budget = (self.token_estimate(raw) as f32 * 1.3) as i32 + 64;
        let cleaned = self.run_chat(user_msg, budget, preserve_newlines, true).await?;
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
        self.run_chat(user_msg, budget, true, false).await
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
        self.run_chat(user_msg, budget, true, false).await
    }

    /// Build the templated prompt and hand it to the persistent-context worker,
    /// awaiting the polished result. Works for Gemma / Llama 3 / Qwen-ChatML /
    /// SmolLM without per-family hardcoding (the template comes from the GGUF).
    async fn run_chat(
        &self,
        user_msg: String,
        max_tokens: i32,
        preserve_newlines: bool,
        no_reuse: bool,
    ) -> eyre::Result<String> {
        let prompt = Self::template_user_msg(&self.model, &self.chat_template, &user_msg)?;
        let (reply, rx) = oneshot::channel();
        self.job_tx
            .as_ref()
            .ok_or_else(|| eyre::eyre!("cleanup worker is gone"))?
            .send(Job { prompt, max_tokens, preserve_newlines, no_reuse, reply })
            .map_err(|_| eyre::eyre!("cleanup worker is gone"))?;
        rx.await
            .map_err(|_| eyre::eyre!("cleanup worker dropped the reply"))?
    }
}

/// The persistent-context worker loop. Owns one `LlamaContext` for the daemon's
/// lifetime and serves jobs over `job_rx`, reusing the KV-cache prefix shared
/// with the previous prompt. Signals readiness (or context-creation failure)
/// once over `ready_tx` so `initialize` can fail fast.
fn run_worker(
    backend: Arc<LlamaBackend>,
    model: Arc<LlamaModel>,
    mut job_rx: mpsc::UnboundedReceiver<Job>,
    warm_prompt: Option<String>,
    ready_tx: std::sync::mpsc::Sender<eyre::Result<()>>,
) {
    // One context, sized once. n_batch must cover the largest single prefill in
    // one decode call, so it matches n_ctx.
    let ctx_params = LlamaContextParams::default()
        .with_n_ctx(NonZeroU32::new(MAX_CTX))
        .with_n_batch(MAX_CTX);
    let mut ctx = match model.new_context(&backend, ctx_params) {
        Ok(c) => c,
        Err(e) => {
            let _ = ready_tx.send(Err(eyre::eyre!("new_context failed: {e:?}")));
            return;
        }
    };

    // `resident` is the token sequence currently decoded into the KV cache
    // (positions 0..resident.len()). Each job reuses the prefix it shares with
    // `resident` and re-decodes only the rest.
    let mut resident: Vec<LlamaToken> = Vec::new();

    // Warm the constant cleanup framing so the first real dictation reuses it.
    if let Some(warm) = warm_prompt {
        if let Ok(toks) = model.str_to_token(&warm, AddBos::Always) {
            if !toks.is_empty() {
                let mut batch = LlamaBatch::new(MAX_CTX as usize, 1);
                let last = toks.len() - 1;
                let ok = (|| -> eyre::Result<()> {
                    for (i, tok) in toks.iter().enumerate() {
                        batch
                            .add(*tok, i as i32, &[0], i == last)
                            .map_err(|e| eyre::eyre!("warm batch.add failed: {e:?}"))?;
                    }
                    ctx.decode(&mut batch)
                        .map_err(|e| eyre::eyre!("warm decode failed: {e:?}"))?;
                    Ok(())
                })();
                if ok.is_ok() {
                    resident = toks;
                }
            }
        }
    }

    let _ = ready_tx.send(Ok(()));

    while let Some(job) = job_rx.blocking_recv() {
        let res = run_job(&model, &mut ctx, &mut resident, job.prompt, job.max_tokens, job.preserve_newlines, job.no_reuse);
        let _ = job.reply.send(res);
    }
}

/// Decode one job against the persistent context, reusing the KV-cache prefix.
fn run_job(
    model: &LlamaModel,
    ctx: &mut llama_cpp_2::context::LlamaContext,
    resident: &mut Vec<LlamaToken>,
    prompt: String,
    max_tokens: i32,
    preserve_newlines: bool,
    no_reuse: bool,
) -> eyre::Result<String> {
    let tokens = model
        .str_to_token(&prompt, AddBos::Always)
        .map_err(|e| eyre::eyre!("tokenize failed: {e:?}"))?;
    if tokens.is_empty() {
        return Ok(String::new());
    }
    let prompt_len = tokens.len();
    // Never ask for more output tokens than the fixed window can hold.
    let max_tokens = max_tokens
        .min((MAX_CTX as usize).saturating_sub(prompt_len + 16) as i32)
        .max(0);

    // Reuse the KV prefix shared with the last prompt. Cap at prompt_len-1 so at
    // least one prompt token is (re)decoded — we need fresh logits to sample
    // from even when the prompt is byte-identical to the previous one.
    let reuse = if no_reuse {
        0
    } else {
        common_prefix_len(&tokens, resident).min(prompt_len.saturating_sub(1))
    };

    // Wipe everything from `reuse` onward (the divergent tail of the previous
    // prompt plus whatever it generated). KV [0, reuse) holds identical tokens
    // at identical positions, so it stays valid.
    ctx.clear_kv_cache_seq(Some(0), Some(reuse as u32), None)
        .map_err(|e| eyre::eyre!("clear_kv_cache_seq failed: {e:?}"))?;

    let mut batch = LlamaBatch::new(MAX_CTX as usize, 1);
    let new = &tokens[reuse..];
    let last = new.len() - 1;
    for (i, tok) in new.iter().enumerate() {
        batch
            .add(*tok, (reuse + i) as i32, &[0], i == last)
            .map_err(|e| eyre::eyre!("batch.add prompt token failed: {e:?}"))?;
    }
    ctx.decode(&mut batch)
        .map_err(|e| eyre::eyre!("decode prompt failed: {e:?}"))?;

    // Low temperature for deterministic cleanup; fixed seed → reproducible.
    let mut sampler = LlamaSampler::chain_simple([
        LlamaSampler::temp(0.2),
        LlamaSampler::dist(1234),
    ]);

    let mut decoder = encoding_rs::UTF_8.new_decoder();
    let mut out = String::new();
    // Next position to write is right after the full prompt, regardless of how
    // many prompt tokens we actually re-decoded this turn.
    let mut n_cur = prompt_len as i32;
    let mut decoded_count = 0i32;

    while decoded_count < max_tokens {
        let tok = sampler.sample(ctx, batch.n_tokens() - 1);
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

    // The prompt tokens are now resident at [0, prompt_len); generated tokens
    // sit beyond and will be wiped by the next job's `clear_kv_cache_seq`.
    *resident = tokens;

    // Defensive polish: strip preamble, peel wrapping quotes, collapse
    // whitespace, drop chat-template artefacts. Transform / list presets keep
    // line breaks; cleanup flattens them.
    Ok(if preserve_newlines {
        crate::text_polish::polish_multiline(&out)
    } else {
        crate::text_polish::polish(&out)
    })
}
