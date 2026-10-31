//! The Speech engine pane.
use super::*;

/// The Speech engine pane: the decoder language, the cleanup model and the instruction model.
pub(super) struct SpeechPane {
    pub(super) view: Retained<SettingsFlippedView>,
    pub(super) cohere_lang_popup: Retained<NSPopUpButton>,
    pub(super) model_popup: Retained<NSPopUpButton>,
    pub(super) instruction_model_popup: Retained<NSPopUpButton>,
}

pub(super) fn build_speech_pane(cx: &PaneCx) -> SpeechPane {
    let mtm = cx.mtm;
    let controller = cx.controller;
    let loaded = cx.loaded;
    let render_fixture = cx.render_fixture;
    let speech = cx.new_pane();
    let mut card = card_begin(mtm, &speech, FIRST_CARD_Y, "Engine");
    let resolved_asr = loaded.resolve_asr();
    hint_line(
        mtm,
        &mut card,
        "Fixed engine: Cohere Transcribe — native MLX (Q4)",
    );
    let cohere_lang_popup = popup_row(mtm, &mut card, "Cohere language");
    for (code, l) in settings::COHERE_LANGUAGE_CHOICES {
        popup_add_item(&cohere_lang_popup, l, None, Some(code));
    }
    select_popup_rep(&cohere_lang_popup, &resolved_asr.language);
    finish_popup(&cohere_lang_popup);
    wire_popup(
        &cohere_lang_popup,
        controller,
        sel!(changeCohereLang:),
        std::env::var_os("DICTATE_COHERE_LANG").is_some(),
    );

    let model_popup = popup_row(mtm, &mut card, "Cleanup model");
    let model_env = if render_fixture {
        None
    } else {
        settings::llm_model_env()
    };
    let model_locked = model_env.is_some();
    let models = if render_fixture {
        Vec::new()
    } else {
        settings::discover_llm_models()
    };
    if models.is_empty() {
        popup_add_item(
            &model_popup,
            "(no models found under models/llm/)",
            None,
            Some(""),
        );
    } else {
        let default_llm = crate::app_paths::llm_default_path();
        let selected = loaded.resolve_llm_model(&default_llm);
        let effective = std::fs::canonicalize(&selected)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or(selected);
        for choice in &models {
            let title = match &choice.hint {
                Some(h) => format!("{} — {h}", choice.label),
                None => choice.label.clone(),
            };
            popup_add_item(&model_popup, &title, None, Some(&choice.path));
            if choice.path == effective {
                select_popup_rep(&model_popup, &choice.path);
            }
        }
    }
    finish_popup(&model_popup);
    wire_popup(&model_popup, controller, sel!(changeModel:), model_locked);
    if let Some((name, _)) = model_env {
        env_hint(mtm, &mut card, name);
    }

    let instruction_model_popup = popup_row(mtm, &mut card, "Instruction model");
    let instruction_default = if render_fixture { None } else { crate::app_paths::instruction_default_path() };
    let automatic_title = instruction_default.as_deref()
        .and_then(|path| std::path::Path::new(path).file_name())
        .map(|name| format!("Automatic — {}", name.to_string_lossy()))
        .unwrap_or_else(|| "Automatic (no instruction model installed)".to_string());
    popup_add_item(&instruction_model_popup, &automatic_title, None, Some("__auto__"));
    popup_add_item(&instruction_model_popup, "Disabled", None, Some("__disabled__"));
    for choice in &models {
        let title = match &choice.hint {
            Some(hint) => format!("{} — {hint}", choice.label),
            None => choice.label.clone(),
        };
        popup_add_item(&instruction_model_popup, &title, None, Some(&choice.path));
    }
    let instruction_env = if render_fixture { None } else {
        std::env::var(settings::INSTRUCTION_MODEL_PATH_ENV).ok()
    };
    let selected_instruction = instruction_env.as_deref().or(loaded.instruction_model.as_deref());
    match selected_instruction {
        None => select_popup_rep(&instruction_model_popup, "__auto__"),
        Some(value) if value.trim().is_empty() => select_popup_rep(&instruction_model_popup, "__disabled__"),
        Some(path) => {
            let effective = std::fs::canonicalize(path)
                .map(|value| value.to_string_lossy().into_owned()).unwrap_or_else(|_| path.to_string());
            if !models.iter().any(|choice| choice.path == effective) {
                let name = std::path::Path::new(&effective).file_name()
                    .map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| "Configured model".to_string());
                popup_add_item(&instruction_model_popup, &format!("Current: {name}"), None, Some(&effective));
            }
            select_popup_rep(&instruction_model_popup, &effective);
        }
    }
    finish_popup(&instruction_model_popup);
    wire_popup(&instruction_model_popup, controller, sel!(changeInstructionModel:), instruction_env.is_some());
    if instruction_env.is_some() {
        env_hint(mtm, &mut card, settings::INSTRUCTION_MODEL_PATH_ENV);
    }
    hint_line(mtm, &mut card, "For rewrites, translation and custom formats. Cleanup stays separate.");
    let _ = card_end(&card);
    finish_pane(&speech, &card);
    SpeechPane { view: speech, cohere_lang_popup, model_popup, instruction_model_popup }
}
