//! Pure model contracts: what each supported GGUF expects on the wire, and
//! which requests the installed model cannot serve at all.
//!
//! This module has no model or I/O dependency on purpose. It is the ONE place
//! that turns *what the user asked for* (an output format, an app tone, a
//! custom prompt) into *the exact prompt a model was trained on*, and decides
//! when that model must hand a request to an instruction model instead. The
//! cleaner wires it to llama.cpp; `tests/model_contracts.rs` pins the literals
//! to the publisher contract so a refactor cannot quietly drift off it.
//!
//! ## S1-mini
//!
//! S1-mini is a text normalizer, not a chat model. Its published contract is
//! exact: a fixed system prompt, then a user message that starts with a control
//! line `[Styling: …] [Structure: …] [Context: …]`, a newline, and the raw
//! transcript; greedy decoding, thinking off. No axis value may change, and no
//! extra instruction (vocabulary, tone, custom prompt) may be appended to it.
//!
//! ## Generic instruct models
//!
//! Any other model is a general chat model and uses its own embedded chat
//! template untouched. Qwen 3 / 3.5 templates default to thinking mode, so they
//! receive the model's own non-thinking assistant prefill; Cohere Aya and
//! everything else never receive Qwen think tokens.


/// The exact S1-mini system prompt. Copied verbatim from the publisher; do not
/// paraphrase, reformat, or append to it.
pub const S1_SYSTEM_PROMPT: &str = "You are a text normalizer for speech-to-text transcripts. The input begins with a control line specifying the styling, structure, and context settings; clean the transcript to match those settings and output only the cleaned text.";

/// The assistant-turn prefill that disables thinking and starts generation at
/// the answer. Matches the publisher's raw `/completion` prefix
/// (`<|im_start|>assistant\n<think>\n\n</think>\n\n`).
pub const QWEN_NONTHINKING_PREFILL: &str = "<think>\n\n</think>\n\n";

/// S1-mini's assistant prefill: the same non-thinking block.
pub const S1_ASSISTANT_PREFILL: &str = QWEN_NONTHINKING_PREFILL;

/// Greedy decoding for S1-mini's deterministic normalizer.
pub const S1_TEMPERATURE: f32 = 0.0;
/// Low-temperature sampling for general instruction models.
pub const GENERIC_TEMPERATURE: f32 = 0.2;

/// The trained `Styling` axis values.
pub const S1_STYLING_VALUES: &[&str] = &["casual", "semi-casual", "semi-formal", "formal"];
/// The trained `Structure` axis values.
pub const S1_STRUCTURE_VALUES: &[&str] = &["prose", "lists"];
/// The trained `Context` axis values.
pub const S1_CONTEXT_VALUES: &[&str] = &["general", "email"];

/// Which prompt/decoding contract a model file follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelFamily {
    /// The installed S1-mini normalizer.
    S1Mini,
    /// A Qwen 3 / 3.5 chat model (thinking mode disabled).
    QwenChat,
    /// Any other instruction model (e.g. Cohere Aya). Uses its own template
    /// exactly as embedded, never Qwen think tokens.
    GenericInstruct,
}

/// The facts family detection is allowed to use: the filename plus selected
/// verified GGUF metadata. Deliberately not the architecture alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelIdentity<'a> {
    pub filename: &'a str,
    /// `general.name`
    pub meta_name: Option<&'a str>,
    /// `general.basename`
    pub meta_basename: Option<&'a str>,
    /// `general.architecture`
    pub meta_architecture: Option<&'a str>,
}

/// Conservative family detection. Only the exact S1-mini lineage is
/// [`ModelFamily::S1Mini`]; a plain Qwen 3 model is not S1, and only the
/// `qwen3` / `qwen35` architectures are [`ModelFamily::QwenChat`]. Everything
/// else is a generic instruction model.
pub fn detect_family(id: &ModelIdentity) -> ModelFamily {
    let contains = |s: Option<&str>, needles: &[&str]| {
        s.is_some_and(|s| {
            let s = s.to_lowercase();
            needles.iter().any(|n| s.contains(n))
        })
    };
    let filename = id.filename.to_lowercase();
    let s1_mini = filename.contains("s1-mini")
        || filename.contains("s1_mini")
        || filename.contains("supertransform")
        || contains(id.meta_name, &["supertransform", "s1-mini", "s1 mini"])
        || contains(id.meta_basename, &["supertransform", "s1-mini", "s1 mini"]);
    // A renamed general model must not inherit the normalizer contract merely
    // from its filename when the actual architecture contradicts that lineage.
    let compatible = matches!(id.meta_architecture, None | Some("qwen3"));
    if s1_mini && compatible {
        return ModelFamily::S1Mini;
    }
    match id.meta_architecture {
        Some("qwen3") | Some("qwen35") => ModelFamily::QwenChat,
        _ => ModelFamily::GenericInstruct,
    }
}

/// Whether a model contract needs the empty non-thinking prefill appended to
/// the assistant turn. S1-mini always does (it is part of the trained prefix).
/// A Qwen chat model only when its own template actually has a thinking branch
/// — never blindly appended to a template that has no such concept.
pub fn needs_non_thinking_prefill(family: ModelFamily, chat_template: &str) -> bool {
    match family {
        ModelFamily::S1Mini => true,
        ModelFamily::QwenChat => {
            let t = chat_template.to_lowercase();
            t.contains("<think>") || t.contains("enable_thinking")
        }
        ModelFamily::GenericInstruct => false,
    }
}

/// Complete a model-specific non-thinking assistant prefix without doubling
/// an empty block already emitted by an embedded template (notably Qwen3.5).
pub fn non_thinking_prompt(mut prompt: String, enabled: bool) -> String {
    if !enabled || prompt.ends_with(QWEN_NONTHINKING_PREFILL) {
        return prompt;
    }
    if prompt.ends_with("<think>\n") {
        prompt.push_str("\n</think>\n\n");
    } else {
        prompt.push_str(QWEN_NONTHINKING_PREFILL);
    }
    prompt
}

/// S1-mini's three trained axes for one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct S1Axes {
    pub styling: &'static str,
    pub structure: &'static str,
    pub context: &'static str,
}

impl S1Axes {
    /// Plain dictation: standard written English, prose, general context.
    pub const DEFAULT: Self = Self {
        styling: "semi-formal",
        structure: "prose",
        context: "general",
    };

    /// The trained control line, exactly:
    /// `[Styling: X] [Structure: Y] [Context: Z]`.
    pub fn control_line(self) -> String {
        format!(
            "[Styling: {}] [Structure: {}] [Context: {}]",
            self.styling, self.structure, self.context
        )
    }
}

/// The exact S1 user message: control line, newline, raw transcript.
pub fn s1_user_message(axes: S1Axes, raw: &str) -> String {
    format!("{}\n{}", axes.control_line(), raw)
}

/// The trained axes for a built-in output-format preset, or `None` when the
/// preset is not one S1-mini was trained on (`numbered`, `code`, anything
/// custom).
pub fn s1_axes_for_format(format: &str) -> Option<S1Axes> {
    match format.trim().to_lowercase().as_str() {
        "bullets" => Some(S1Axes {
            styling: "semi-formal",
            structure: "lists",
            context: "general",
        }),
        "email" => Some(S1Axes {
            styling: "semi-formal",
            structure: "prose",
            context: "email",
        }),
        _ => None,
    }
}

/// The trained axes for an app tone, or `None` when the tone has no trained
/// register (`technical`, `social`). A tone is an automatic soft hint, so a
/// miss here routes to the instruction model when one is configured rather
/// than rejecting dictation.
pub fn s1_axes_for_tone(tone: &str) -> Option<S1Axes> {
    match tone.trim().to_lowercase().as_str() {
        "casual chat" | "social" => Some(S1Axes {
            styling: "semi-casual",
            structure: "prose",
            context: "general",
        }),
        "neutral prose" | "technical" => Some(S1Axes {
            styling: "semi-formal",
            structure: "prose",
            context: "general",
        }),
        "email" => Some(S1Axes {
            styling: "semi-formal",
            structure: "prose",
            context: "email",
        }),
        _ => None,
    }
}

/// The controls a single cleanup call carries, resolved by the engine from the
/// active format, the user's prompts, and the target app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupControls<'a> {
    /// The active output-format preset name, if any.
    pub format: Option<&'a str>,
    /// True when that preset came from the user's `prompts.json` (free-form).
    pub format_is_custom: bool,
    /// False for a stale or removed preset; matches Prompts::format_is_defined.
    pub format_is_defined: bool,
    /// True when the cleanup prompt came from env/file, not the built-in default.
    pub cleanup_prompt_is_custom: bool,
    /// The per-app tone keyword, if the target app is known.
    pub tone: Option<&'a str>,
    /// Configured speech language; S1-mini's trained normalizer is English-only.
    pub language: &'a str,
}

/// What the engine should do for one cleanup call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupPlan {
    /// Run S1-mini with these trained axes.
    S1(S1Axes),
    /// Hand the free-form request to the configured instruction model.
    Instruction { reason: String },
    /// No configured model can serve this control; surface `reason`.
    Refuse { reason: String },
}

/// Decide how S1-mini should serve one cleanup call, or whether an instruction
/// model is required. `instruction_available` is whether a general chat model
/// is configured (and loaded). A control that is not a trained S1 axis is never
/// silently ignored: it becomes [`CleanupPlan::Instruction`] when a model can
/// serve it, and [`CleanupPlan::Refuse`] with an actionable reason otherwise.
pub fn plan_cleanup(controls: &CleanupControls, instruction_available: bool) -> CleanupPlan {
    if !controls.language.eq_ignore_ascii_case("en") {
        let reason = "S1-mini normalization is English-only; choose a multilingual instruction model for this speech language".to_string();
        return if instruction_available {
            CleanupPlan::Instruction { reason }
        } else {
            CleanupPlan::Refuse { reason }
        };
    }
    let mut axes = S1Axes::DEFAULT;
    let mut reason: Option<String> = None;

    if let Some(name) = controls.format.filter(|_| controls.format_is_defined) {
        let name = name.trim().to_lowercase();
        if controls.format_is_custom {
            reason = Some(format!(
                "the custom output format `{name}` is a free-form instruction S1-mini was not trained to follow"
            ));
        } else {
            match s1_axes_for_format(&name) {
                Some(found) => axes = found,
                None => {
                    reason = Some(format!(
                        "the `{name}` output format is not one of S1-mini's trained controls"
                    ))
                }
            }
        }
    } else if controls.cleanup_prompt_is_custom {
        reason = Some(
            "a custom cleanup prompt is configured, which is a free-form instruction S1-mini was not trained to follow"
                .to_string(),
        );
    } else if let Some(tone) = controls.tone {
        match s1_axes_for_tone(tone) {
            Some(found) => axes = found,
            None => {
                reason = Some(format!(
                    "the `{tone}` app tone has no S1-mini styling register"
                ))
            }
        }
    }

    match reason {
        None => CleanupPlan::S1(axes),
        Some(reason) => {
            if instruction_available {
                CleanupPlan::Instruction { reason }
            } else {
                CleanupPlan::Refuse {
                    reason: format!(
                        "{reason}. Configure an instruction model (a general chat GGUF) to use it, \
                         or remove it; S1-mini still handles plain dictation cleanup."
                    ),
                }
            }
        }
    }
}

/// The actionable refusal for a selection transform when only an S1-mini
/// normalizer is loaded.
pub fn s1_transform_refusal() -> String {
    "S1-mini is a text normalizer, not a general instruction model, so it cannot rewrite a \
     selection. Configure an instruction model (a general chat GGUF) for transforms, or use \
     push-to-talk without a selection."
        .to_string()
}
