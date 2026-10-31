//! The Output pane.
use super::*;

/// The Output pane: output format and appearance.
pub(super) struct OutputPane {
    pub(super) view: Retained<SettingsFlippedView>,
    pub(super) format_popup: Retained<NSPopUpButton>,
    pub(super) appearance_popup: Retained<NSPopUpButton>,
}

pub(super) fn build_output_pane(cx: &PaneCx) -> OutputPane {
    let mtm = cx.mtm;
    let controller = cx.controller;
    let loaded = cx.loaded;
    let render_fixture = cx.render_fixture;
    let output = cx.new_pane();
    let mut card = card_begin(mtm, &output, FIRST_CARD_Y, "Format");
    let format_popup = popup_row(mtm, &mut card, "Output format");
    let format_names = if render_fixture {
        let mut names: Vec<String> = crate::prompts::default_formats().into_keys().collect();
        names.sort();
        names
    } else {
        crate::prompts::Prompts::load_quiet().format_names()
    };
    let format_locked = std::env::var_os("DICTATE_FORMAT").is_some();
    popup_add_item(&format_popup, "Default (no preset)", None, Some(""));
    for name in &format_names {
        popup_add_item(&format_popup, name, None, Some(name));
    }
    select_popup_rep(&format_popup, &loaded.resolve_format().unwrap_or_default());
    finish_popup(&format_popup);
    wire_popup(
        &format_popup,
        controller,
        sel!(changeFormat:),
        format_locked,
    );
    if format_locked {
        env_hint(mtm, &mut card, "DICTATE_FORMAT");
    }
    let y = card_end(&card);

    let mut card = card_begin(mtm, &output, y, "Interface");
    let appearance_popup = popup_row(mtm, &mut card, "Appearance");
    for (rep, label_str) in settings::APPEARANCE_CHOICES {
        popup_add_item(&appearance_popup, label_str, None, Some(rep));
    }
    select_popup_rep(&appearance_popup, &loaded.resolve_appearance());
    finish_popup(&appearance_popup);
    wire_popup(
        &appearance_popup,
        controller,
        sel!(changeAppearance:),
        std::env::var_os("DICTATE_APPEARANCE").is_some(),
    );
    hint_line(
        mtm,
        &mut card,
        "Dark is the default. Every window follows this — nothing is pinned to light.",
    );
    let _ = card_end(&card);
    finish_pane(&output, &card);
    OutputPane { view: output, format_popup, appearance_popup }
}
