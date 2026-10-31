//! The Translate pane.
use super::*;

/// The Translate pane: target languages and the active target.
pub(super) struct TranslatePane {
    pub(super) view: Retained<SettingsFlippedView>,
    pub(super) translate_targets_field: Retained<NSTextField>,
    pub(super) active_translate_popup: Retained<NSPopUpButton>,
}

pub(super) fn build_translate_pane(cx: &PaneCx) -> TranslatePane {
    let mtm = cx.mtm;
    let controller = cx.controller;
    let loaded = cx.loaded;
    let render_fixture = cx.render_fixture;
    let translate = cx.new_pane();
    let mut card = card_begin(mtm, &translate, FIRST_CARD_Y, "Targets");
    let loaded_targets = if render_fixture {
        vec!["Spanish".to_string(), "Chinese".to_string()]
    } else {
        loaded.resolve_translation_targets()
    };
    let targets_value = loaded_targets.join(", ");
    let translate_targets_field = text_row(mtm, &mut card, "Translate targets", &targets_value);
    translate_targets_field
        .setPlaceholderString(Some(&NSString::from_str("Spanish, Chinese, French")));
    let targets_locked = std::env::var_os("DICTATE_TRANSLATION_TARGETS").is_some();
    unsafe {
        translate_targets_field.setTarget(Some(controller));
        translate_targets_field.setAction(Some(sel!(changeTranslateTargets:)));
        translate_targets_field.setEnabled(!targets_locked);
    }
    if targets_locked {
        env_hint(mtm, &mut card, "DICTATE_TRANSLATION_TARGETS");
    }

    let active_target_locked = std::env::var_os("DICTATE_ACTIVE_TRANSLATION_TARGET").is_some();
    let active_translate_popup = popup_row(mtm, &mut card, "Active target");
    if loaded_targets.is_empty() {
        popup_add_item(&active_translate_popup, "(none)", None, Some(""));
    } else {
        for t in &loaded_targets {
            popup_add_item(&active_translate_popup, t, None, Some(t));
        }
        if let Some(active) = loaded.resolve_active_translation_target(&loaded_targets) {
            select_popup_rep(&active_translate_popup, &active);
        }
    }
    finish_popup(&active_translate_popup);
    wire_popup(
        &active_translate_popup,
        controller,
        sel!(changeActiveTranslateTarget:),
        active_target_locked,
    );
    if active_target_locked {
        env_hint(mtm, &mut card, "DICTATE_ACTIVE_TRANSLATION_TARGET");
    }
    let _ = card_end(&card);
    finish_pane(&translate, &card);
    TranslatePane { view: translate, translate_targets_field, active_translate_popup }
}
