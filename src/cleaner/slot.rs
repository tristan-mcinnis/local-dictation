//! One loaded chat model: the llama.cpp context slot, its prompt template
//! family, and prompt rendering.
use super::*;

/// One resident llama.cpp model plus the worker thread that owns its context.
/// Each slot keeps its own persistent context, so the KV prefix cache lifetime
/// is independent between the cleanup model and the instruction model.
pub(super) struct ChatSlot {
    model: Arc<LlamaModel>,
    chat_template: Arc<LlamaChatTemplate>,
    /// Decided once from verified GGUF metadata + filename.
    pub(super) family: ModelFamily,
    /// True when the assistant turn needs the empty non-thinking prefill
    /// (S1-mini's trained prefix; Qwen 3/3.5 chat templates).
    prefill: bool,
    /// `Option` so `Drop` can close the channel (stopping the worker) before
    /// joining it.
    job_tx: Option<mpsc::UnboundedSender<Job>>,
    /// Joined on drop so its `LlamaContext` (and the Metal buffers it holds) is
    /// freed before the process tears down ggml's global Metal device —
    /// otherwise `ggml_metal_device_free` asserts at exit.
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Drop for ChatSlot {
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

impl ChatSlot {
    /// Load a model and spawn its persistent-context worker. `generic_system`
    /// is the warm prefix for a general instruction model; an S1-mini model is
    /// warmed with its exact trained system prompt instead (decided here, after
    /// the family is known).
    pub(super) fn load(
        backend: &Arc<LlamaBackend>,
        model_path: &Path,
        thread_name: &str,
        generic_system: &str,
    ) -> eyre::Result<Self> {
        // Offload everything to Metal. llama.cpp ignores layers beyond what
        // the model actually has, so a large number is safe.
        let model_params = LlamaModelParams::default().with_n_gpu_layers(1000);
        let model = Arc::new(
            LlamaModel::load_from_file(backend, model_path, &model_params)
                .map_err(|e| eyre::eyre!("LlamaModel::load_from_file failed: {e:?}"))?,
        );
        // Use the model's bundled chat template (Gemma / ChatML / Cohere2 /
        // whatever the GGUF declares). Falls back to a no-op format if the
        // model has no template — in practice all instruct GGUFs do.
        let chat_template = Arc::new(
            model
                .chat_template(None)
                .map_err(|e| eyre::eyre!("chat_template lookup failed: {e:?}"))?,
        );
        let family = detect_family(&model, model_path);
        let template_text = model.meta_val_str("tokenizer.chat_template").unwrap_or_default();
        let prefill = llm_contract::needs_non_thinking_prefill(family, &template_text);
        // Warm the constant prefix the model will see every utterance. S1-mini
        // warms its exact system prompt + default control line; a generic model
        // warms the cleanup prompt + the "Raw transcript:" framing.
        let (warm_system, warm_user) = if family == ModelFamily::S1Mini {
            (
                llm_contract::S1_SYSTEM_PROMPT.to_string(),
                format!("{}\n", S1Axes::DEFAULT.control_line()),
            )
        } else {
            (generic_system.to_string(), "Raw transcript:\n".to_string())
        };
        let warm_prompt =
            render_prompt(&model, &chat_template, prefill, &warm_system, &warm_user).ok();

        let (job_tx, job_rx) = mpsc::unbounded_channel::<Job>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<eyre::Result<()>>();
        let worker = {
            let backend = backend.clone();
            let model = model.clone();
            let thread_name = thread_name.to_string();
            std::thread::Builder::new()
                .name(thread_name.clone())
                .spawn(move || run_worker(backend, model, job_rx, warm_prompt, ready_tx))
                .map_err(|e| eyre::eyre!("spawn {thread_name} worker failed: {e}"))?
        };
        // Block until the worker has built its context (boot-time, so blocking
        // is fine). Propagate a context-creation failure as an init error.
        ready_rx
            .recv()
            .map_err(|_| eyre::eyre!("{thread_name} worker died during init"))??;

        Ok(Self {
            model,
            chat_template,
            family,
            prefill,
            job_tx: Some(job_tx),
            worker: Some(worker),
        })
    }

    /// Rough token count for sizing generation budgets / the context window.
    pub(super) fn token_estimate(&self, text: &str) -> usize {
        self.model
            .str_to_token(text, AddBos::Never)
            .map(|t| t.len())
            .unwrap_or_else(|_| text.chars().count() / 3)
    }

    /// Template `system` + `user` and hand the prompt to this slot's worker.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run(
        &self,
        system: String,
        user: String,
        max_tokens: i32,
        preserve_newlines: bool,
        no_reuse: bool,
        fail_on_truncation: bool,
        temperature: f32,
    ) -> eyre::Result<String> {
        let prompt =
            render_prompt(&self.model, &self.chat_template, self.prefill, &system, &user)?;
        let (reply, rx) = oneshot::channel();
        self.job_tx
            .as_ref()
            .ok_or_else(|| eyre::eyre!("cleanup worker is gone"))?
            .send(Job {
                prompt,
                max_tokens,
                preserve_newlines,
                no_reuse,
                fail_on_truncation,
                temperature,
                reply,
            })
            .map_err(|_| eyre::eyre!("cleanup worker is gone"))?;
        rx.await
            .map_err(|_| eyre::eyre!("cleanup worker dropped the reply"))?
    }
}

/// Deciding authority for the model family: verified GGUF metadata first, with
/// the filename as a secondary signal. Conservative — see
/// [`llm_contract::detect_family`].
pub(super) fn detect_family(model: &LlamaModel, path: &Path) -> ModelFamily {
    let meta = |key: &str| model.meta_val_str(key).ok();
    let name = meta("general.name");
    let basename = meta("general.basename");
    let architecture = meta("general.architecture");
    let file_name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    llm_contract::detect_family(&ModelIdentity {
        filename: file_name,
        meta_name: name.as_deref(),
        meta_basename: basename.as_deref(),
        meta_architecture: architecture.as_deref(),
    })
}

/// Render a full prompt from separate system and user chat turns. S1-mini's
/// published contract needs an exact system prompt and a control line, and its
/// assistant turn must open with an empty think block. A generic model uses its
/// own embedded template; only its own non-thinking prefill is appended when its
/// template has a thinking branch, so Cohere Aya never receives Qwen tokens.
pub(super) fn render_prompt(
    model: &LlamaModel,
    chat_template: &LlamaChatTemplate,
    prefill: bool,
    system_msg: &str,
    user_msg: &str,
) -> eyre::Result<String> {
    let messages = vec![
        LlamaChatMessage::new("system".into(), system_msg.to_string())
            .map_err(|e| eyre::eyre!("invalid system message: {e:?}"))?,
        LlamaChatMessage::new("user".into(), user_msg.to_string())
            .map_err(|e| eyre::eyre!("invalid user message: {e:?}"))?,
    ];
    let prompt = model
        .apply_chat_template(chat_template, &messages, true)
        .map_err(|e| eyre::eyre!("apply_chat_template failed: {e:?}"))?;
    Ok(llm_contract::non_thinking_prompt(prompt, prefill))
}
