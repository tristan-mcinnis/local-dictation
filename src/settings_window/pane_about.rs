//! The About pane.
use super::*;

/// The About pane: grants, files and logs.
pub(super) struct AboutPane {
    pub(super) view: Retained<SettingsFlippedView>,
}

pub(super) fn build_about_pane(cx: &PaneCx) -> AboutPane {
    let mtm = cx.mtm;
    let controller = cx.controller;
    let about = cx.new_pane();
    let mut card = card_begin(mtm, &about, FIRST_CARD_Y, "Files & logs");
    for (title, action) in [
        ("Cleanup prompts", sel!(editPrompts:)),
        ("Corrections folder", sel!(openCorrections:)),
        ("Voice commands & shortcuts", sel!(openCommandsRef:)),
        ("Log", sel!(openLog:)),
        ("Export log to Downloads", sel!(exportLog:)),
    ] {
        button_row(mtm, &mut card, title, controller, action);
    }
    let y = card_end(&card);

    let mut card = card_begin(mtm, &about, y, "About");
    hint_line(mtm, &mut card, &version_line());
    // The two grants, stated here as well as in the Setup window: About is
    // where a user looks when something is not working, and "which permission
    // is missing" is the first thing they need.
    hint_line(
        mtm,
        &mut card,
        &format!(
            "Microphone: {}",
            crate::permissions::microphone_status().label()
        ),
    );
    hint_line(
        mtm,
        &mut card,
        &format!(
            "Accessibility: {}",
            if crate::permissions::accessibility_trusted() {
                "Allowed"
            } else {
                "Off — open Setup & permissions"
            }
        ),
    );
    if let Some(dir) = crate::app_paths::config_dir() {
        hint_line(mtm, &mut card, &format!("Config: {}", dir.display()));
    }
    hint_line(
        mtm,
        &mut card,
        &format!("Models: {}", crate::app_paths::llm_models_dir().display()),
    );
    let _ = card_end(&card);
    finish_pane(&about, &card);
    AboutPane { view: about }
}
