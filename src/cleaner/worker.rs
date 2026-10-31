//! The inference worker thread: the job queue, KV-cache reuse planning, and
//! running one job to a reply.
use super::*;

/// A unit of work for the cleanup worker thread: a fully-templated prompt plus
/// the generation parameters and a channel to return the polished result on.
pub(super) struct Job {
    /// The prompt already run through the model's chat template.
    pub(super) prompt: String,
    /// Max tokens to generate.
    pub(super) max_tokens: i32,
    /// Cleanup flattens to one line; transform / list presets keep newlines.
    pub(super) preserve_newlines: bool,
    /// Force a full prefill (ignore the resident KV prefix). Used only by the
    /// parity test to prove cached output == uncached output.
    pub(super) no_reuse: bool,
    /// Fail instead of returning text when generation stops at `max_tokens`
    /// rather than at the model's end-of-generation token. Transform sets it:
    /// its output REPLACES the user's selection, so a cut-off rewrite must never
    /// be returned as if it were complete.
    pub(super) fail_on_truncation: bool,
    /// Sampling temperature. S1-mini's normalizer is greedy (`0.0`); general
    /// instruction models use a low temperature. Zero selects the greedy
    /// sampler — no distribution, fully deterministic.
    pub(super) temperature: f32,
    pub(super) reply: oneshot::Sender<eyre::Result<String>>,
}

/// Longest common prefix length of two token slices.
pub(super) fn common_prefix_len<T: PartialEq>(a: &[T], b: &[T]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

/// How many leading tokens of `tokens` may be reused from the KV cache that
/// `resident` describes. Capped at `len - 1` so at least one prompt token is
/// (re)decoded — fresh logits are needed to sample from even when the prompt
/// is identical to the previous one.
pub(super) fn plan_reuse<T: PartialEq>(tokens: &[T], resident: &[T], no_reuse: bool) -> usize {
    if no_reuse {
        0
    } else {
        common_prefix_len(tokens, resident).min(tokens.len().saturating_sub(1))
    }
}

/// How much KV prefix survives a clear attempt. `partial_cleared` is the
/// memory implementation's answer: `Ok(true)` cleared the divergent tail,
/// `Ok(false)`/`Err` refused (hybrid/recurrent memory cannot drop part of a
/// sequence). On refusal, reuse NOTHING and reset the cache fully and re-prefill:
/// reusing state the cache no longer holds produces silently wrong output.
pub(super) fn reuse_after_clear(partial_cleared: bool, reuse: usize) -> (usize, bool) {
    if partial_cleared {
        (reuse, false)
    } else {
        (0, true)
    }
}

/// The persistent-context worker loop. Owns one `LlamaContext` for the daemon's
/// lifetime and serves jobs over `job_rx`, reusing the KV-cache prefix shared
/// with the previous prompt. Signals readiness (or context-creation failure)
/// once over `ready_tx` so `initialize` can fail fast.
pub(super) fn run_worker(
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
        let res = run_job(
            &model,
            &mut ctx,
            &mut resident,
            job.prompt,
            job.max_tokens,
            job.preserve_newlines,
            job.no_reuse,
            job.fail_on_truncation,
            job.temperature,
        );
        let _ = job.reply.send(res);
    }
}

/// Decode one job against the persistent context, reusing the KV-cache prefix.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_job(
    model: &LlamaModel,
    ctx: &mut llama_cpp_2::context::LlamaContext,
    resident: &mut Vec<LlamaToken>,
    prompt: String,
    max_tokens: i32,
    preserve_newlines: bool,
    no_reuse: bool,
    fail_on_truncation: bool,
    temperature: f32,
) -> eyre::Result<String> {
    let tokens = model
        .str_to_token(&prompt, AddBos::Always)
        .map_err(|e| eyre::eyre!("tokenize failed: {e:?}"))?;
    if tokens.is_empty() {
        return Ok(String::new());
    }
    let prompt_len = tokens.len();
    if let Some(problem) = prompt_len_problem(prompt_len) {
        return Err(eyre::eyre!(problem)); // KV cache and `resident` untouched
    }
    // Never ask for more output tokens than the fixed window can hold.
    let max_tokens = max_tokens
        .min((MAX_CTX as usize).saturating_sub(prompt_len + 16) as i32)
        .max(0);

    // Reuse the KV prefix shared with the last prompt. Cap at prompt_len-1 so at
    // least one prompt token is (re)decoded — we need fresh logits to sample
    // from even when the prompt is byte-identical to the previous one.
    let mut reuse = plan_reuse(&tokens, resident, no_reuse);

    // Wipe everything from `reuse` onward (the divergent tail of the previous
    // prompt plus whatever it generated). KV [0, reuse) holds identical tokens
    // at identical positions, so it stays valid. Hybrid/recurrent memory
    // (Mamba, RWKV, linear-attention hybrids) cannot drop PART of a sequence:
    // `clear_kv_cache_seq` reports `Ok(false)` and the state it would leave
    // behind is not the prefix we think it is. In that case reset the whole
    // cache and re-prefill from scratch — never reuse state the cache no longer
    // holds.
    if reuse == 0 {
        ctx.clear_kv_cache();
    } else {
        let partial_cleared = ctx.clear_kv_cache_seq(Some(0), Some(reuse as u32), None).unwrap_or_default();
        let (next_reuse, full_reset) = reuse_after_clear(partial_cleared, reuse);
        reuse = next_reuse;
        if full_reset {
            ctx.clear_kv_cache();
        }
    }
    // Bookkeeping follows the cache at every step. `resident` used to be
    // updated only on success, so a job that failed after this wipe left it
    // describing tokens that were gone, and the NEXT job reused a prefix the
    // cache no longer held (silently wrong output until relaunch).
    resident.truncate(reuse);

    let mut batch = LlamaBatch::new(MAX_CTX as usize, 1);
    let new = &tokens[reuse..];
    let last = new.len() - 1;
    for (i, tok) in new.iter().enumerate() {
        batch
            .add(*tok, (reuse + i) as i32, &[0], i == last)
            .map_err(|e| eyre::eyre!("batch.add prompt token failed: {e:?}"))?;
    }
    ctx.decode(&mut batch)
        .map_err(|e| {
            // A failed decode may have written any part of the new tail.
            // Forget everything; the next job clears the cache from 0.
            resident.clear();
            eyre::eyre!("decode prompt failed: {e:?}")
        })?;
    // The whole prompt is resident from here on, whatever generation does.
    // Generated tokens sit beyond it and are wiped by the next job's clear.
    *resident = tokens;

    // Greedy decoding is deterministic and is what S1-mini was trained for
    // (`temperature == 0.0`). A general model gets a low-temperature fixed-seed
    // sample. Never the model's metadata defaults (S1-mini inherits Qwen3's
    // temp 0.6, which reintroduces variance the publisher explicitly warns
    // against).
    let mut sampler = if temperature <= 0.0 {
        LlamaSampler::chain_simple([LlamaSampler::greedy()])
    } else {
        LlamaSampler::chain_simple([LlamaSampler::temp(temperature), LlamaSampler::dist(1234)])
    };

    let mut decoder = encoding_rs::UTF_8.new_decoder();
    let mut out = String::new();
    // Next position to write is right after the full prompt, regardless of how
    // many prompt tokens we actually re-decoded this turn.
    let mut n_cur = prompt_len as i32;
    let mut decoded_count = 0i32;

    let mut finished = false;
    while decoded_count < max_tokens {
        let tok = sampler.sample(ctx, batch.n_tokens() - 1);
        sampler.accept(tok);
        if model.is_eog_token(tok) {
            finished = true;
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

    if fail_on_truncation && !finished {
        return Err(eyre::eyre!(
            "the rewrite was cut off at the {max_tokens}-token limit, so it was discarded"
        ));
    }

    // Defensive polish: strip preamble, peel wrapping quotes, collapse
    // whitespace, drop chat-template artefacts. Transform / list presets keep
    // line breaks; cleanup flattens them.
    Ok(if preserve_newlines {
        crate::text_polish::polish_multiline(&out)
    } else {
        crate::text_polish::polish(&out)
    })
}
