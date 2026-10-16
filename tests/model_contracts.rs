use fast_dictate_backend::llm_contract::{
    self as contract, CleanupControls, CleanupPlan, ModelFamily, ModelIdentity, S1Axes,
};

fn plain<'a>() -> CleanupControls<'a> {
    CleanupControls {
        format: None,
        format_is_custom: false,
        format_is_defined: false,
        cleanup_prompt_is_custom: false,
        tone: None,
        language: "en",
    }
}

#[test]
fn normalizer_request_matches_its_published_input() {
    assert_eq!(contract::S1_SYSTEM_PROMPT, "You are a text normalizer for speech-to-text transcripts. The input begins with a control line specifying the styling, structure, and context settings; clean the transcript to match those settings and output only the cleaned text.");
    assert_eq!(
        contract::s1_user_message(S1Axes::DEFAULT, "um ship it"),
        "[Styling: semi-formal] [Structure: prose] [Context: general]\num ship it"
    );
    assert_eq!(contract::S1_TEMPERATURE, 0.0);
    assert_eq!(contract::S1_ASSISTANT_PREFILL, "<think>\n\n</think>\n\n");
}

#[test]
fn ordinary_qwen_is_not_mistaken_for_the_normalizer() {
    let qwen = ModelIdentity {
        filename: "model.gguf",
        meta_name: Some("Qwen3.5 2B"),
        meta_basename: None,
        meta_architecture: Some("qwen35"),
    };
    assert_eq!(contract::detect_family(&qwen), ModelFamily::QwenChat);
    let s1 = ModelIdentity {
        filename: "renamed.gguf",
        meta_name: Some("S1-mini"),
        meta_basename: None,
        meta_architecture: Some("qwen3"),
    };
    assert_eq!(contract::detect_family(&s1), ModelFamily::S1Mini);
    let misleading_filename = ModelIdentity {
        filename: "s1-mini.gguf",
        meta_name: Some("Tiny Aya Global"),
        meta_basename: None,
        meta_architecture: Some("cohere2"),
    };
    assert_eq!(
        contract::detect_family(&misleading_filename),
        ModelFamily::GenericInstruct
    );
}

#[test]
fn cohere_template_never_gets_qwen_thinking_tokens() {
    let aya = ModelIdentity {
        filename: "tiny-aya-global-q4_k_m.gguf",
        meta_name: Some("Tiny Aya Global"),
        meta_basename: None,
        meta_architecture: Some("cohere2"),
    };
    let family = contract::detect_family(&aya);
    assert_eq!(family, ModelFamily::GenericInstruct);
    assert!(!contract::needs_non_thinking_prefill(
        family,
        "Cohere template"
    ));
    assert!(contract::needs_non_thinking_prefill(
        ModelFamily::QwenChat,
        "{% if enable_thinking %}<think>{% endif %}"
    ));
}

#[test]
fn explicit_custom_instructions_are_routed_or_refused_never_ignored() {
    let mut controls = plain();
    controls.cleanup_prompt_is_custom = true;
    assert!(matches!(
        contract::plan_cleanup(&controls, true),
        CleanupPlan::Instruction { .. }
    ));
    assert!(matches!(
        contract::plan_cleanup(&controls, false),
        CleanupPlan::Refuse { .. }
    ));
    controls = plain();
    controls.format = Some("numbered");
    controls.format_is_defined = true;
    assert!(matches!(
        contract::plan_cleanup(&controls, true),
        CleanupPlan::Instruction { .. }
    ));
    controls.format = Some("email");
    controls.format_is_custom = true;
    assert!(matches!(
        contract::plan_cleanup(&controls, true),
        CleanupPlan::Instruction { .. }
    ));
}

#[test]
fn unknown_format_falls_back_to_normal_cleanup() {
    // This is the actual routing seam: preserve the stale name and tell it
    // the resolver did not find that preset, rather than pre-filtering the test.
    let mut controls = plain();
    controls.format = Some("removed-preset");
    assert_eq!(
        contract::plan_cleanup(&controls, false),
        CleanupPlan::S1(S1Axes::DEFAULT)
    );
}

#[test]
fn prefilled_qwen35_prefix_is_not_doubled() {
    let already = "<|im_start|>assistant\n<think>\n\n</think>\n\n";
    assert_eq!(contract::non_thinking_prompt(already.into(), true), already);
    assert_eq!(
        contract::non_thinking_prompt("<|im_start|>assistant\n<think>\n".into(), true),
        already
    );
    assert_eq!(
        contract::non_thinking_prompt("<|im_start|>assistant\n".into(), true),
        already
    );
    assert_eq!(
        contract::non_thinking_prompt("cohere assistant".into(), false),
        "cohere assistant"
    );
}

#[test]
fn non_english_dictation_never_reaches_the_english_normalizer() {
    let mut controls = plain();
    controls.language = "zh";
    assert!(matches!(
        contract::plan_cleanup(&controls, true),
        CleanupPlan::Instruction { .. }
    ));
    assert!(matches!(
        contract::plan_cleanup(&controls, false),
        CleanupPlan::Refuse { .. }
    ));
}

#[test]
fn automatic_technical_tone_does_not_trigger_a_slower_general_model() {
    let mut controls = plain();
    controls.tone = Some("technical");
    assert_eq!(
        contract::plan_cleanup(&controls, false),
        CleanupPlan::S1(S1Axes::DEFAULT)
    );
}

#[test]
fn built_in_layouts_only_use_trained_axis_values() {
    for format in ["bullets", "email"] {
        let mut controls = plain();
        controls.format = Some(format);
        controls.format_is_defined = true;
        let CleanupPlan::S1(axes) = contract::plan_cleanup(&controls, false) else {
            panic!("expected a trained request")
        };
        assert!(contract::S1_STYLING_VALUES.contains(&axes.styling));
        assert!(contract::S1_STRUCTURE_VALUES.contains(&axes.structure));
        assert!(contract::S1_CONTEXT_VALUES.contains(&axes.context));
    }
}
