//! The Experimental pane.
use super::*;

/// The Experimental pane: listen mode and the wake word.
pub(super) struct ExperimentalPane {
    pub(super) view: Retained<SettingsFlippedView>,
    pub(super) listen_switch: Retained<slate::InkToggle>,
    pub(super) wake_field: Retained<NSTextField>,
}

pub(super) fn build_experimental_pane(cx: &PaneCx) -> ExperimentalPane {
    let mtm = cx.mtm;
    let controller = cx.controller;
    let loaded = cx.loaded;
    let render_fixture = cx.render_fixture;
    let experimental = cx.new_pane();
    let mut card = card_begin(mtm, &experimental, FIRST_CARD_Y, "Experimental");
    let listen_switch = toggle_row(
        mtm,
        &mut card,
        "Listen mode",
        "Hands-free, triggered by a wake word",
    );
    listen_switch.set_on(loaded.resolve_listen_mode());
    wire_switch(
        &listen_switch,
        controller,
        sel!(changeListen:),
        std::env::var_os("DICTATE_LISTEN_MODE").is_some(),
    );
    hint_line(mtm, &mut card, "Needs always-on mic.");

    let wake_word = if render_fixture {
        "computer".to_string()
    } else {
        loaded.resolve_wake_word()
    };
    let wake_field = text_row(mtm, &mut card, "Wake word", &wake_word);
    let wake_locked = std::env::var_os("DICTATE_WAKE_WORD").is_some();
    unsafe {
        wake_field.setTarget(Some(controller));
        wake_field.setAction(Some(sel!(changeWakeWord:)));
        wake_field.setEnabled(!wake_locked);
    }
    let _ = card_end(&card);
    finish_pane(&experimental, &card);
    ExperimentalPane { view: experimental, listen_switch, wake_field }
}
