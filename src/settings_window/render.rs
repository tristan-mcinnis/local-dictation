//! Offscreen render hooks for `render-ui`: build the window from fixtures,
//! pick a pane, seed and flash history, and scroll.
use super::*;

pub(super) const RENDER_DICTIONARY: &str = "Cohere\nMLX\nkuh here → Cohere\nem el ex → MLX";

/// Build the window without registering it as the process-wide singleton and
/// without ever ordering it in. Used by `render-ui` to snapshot the shell.
pub fn build_for_render(mtm: MainThreadMarker) -> SettingsUi {
    build_settings_window(mtm, true)
}

/// Scroll the visible pane to its bottom so a snapshot shows the cards that sit
/// below the fold.
pub fn scroll_to_bottom_for_render(ui: &SettingsUi) {
    scroll_render(ui, true);
}

/// Switch the visible pane while snapshotting, so a render proof can capture a
/// pane other than the first. Unlike a click this never re-queries history, so
/// rows seeded by [`seed_history_for_render`] survive the switch.
pub fn show_pane_for_render(ui: &SettingsUi, index: usize) {
    select_pane_view(ui, index);
}

/// Draw `entries` into the History pane instead of the store's real rows, so a
/// render proof shows a known list rather than whatever the Mac dictated today.
pub fn seed_history_for_render(ui: &SettingsUi, entries: &[crate::history::Entry]) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let list = fill_history_body(
        mtm,
        &ui.history_body,
        &ui._controller,
        entries,
        entries.len(),
        "",
        None,
    );
    adopt_history_list(ui, list);
    size_list_card(
        &ui.history_pane,
        &ui.history_card,
        ui.history_card_y,
        ui.history_body_y,
        ui.history_body.frame().size.height,
    );
    ui.history_status
        .setStringValue(&NSString::from_str(&history_count_line(entries.len())));
    ui.audio_usage_label
        .setStringValue(&NSString::from_str(&recovery_usage_summary(
            0,
            0,
            entries.len(),
        )));
    ui.recovery_warning_label
        .setStringValue(&NSString::from_str(""));
    ui.recovery_warning_label.setHidden(true);
}

pub fn verify_history_render_fixture(ui: &SettingsUi, entries: usize) -> eyre::Result<()> {
    eyre::ensure!(
        ui.audio_usage_label.stringValue().to_string() == recovery_usage_summary(0, 0, entries),
        "History render read a live audio-usage count"
    );
    eyre::ensure!(
        ui.recovery_warning_label.isHidden(),
        "History render included a live recovery warning"
    );
    eyre::ensure!(
        ui.original == Settings::default(),
        "Settings render included live settings"
    );
    let dictionary = crate::corrections::Corrections::from_editor_text(RENDER_DICTIONARY);
    eyre::ensure!(
        ui.pending_corrections.lock().unwrap().to_editor_lines() == dictionary.to_editor_lines(),
        "Settings render included live dictionary terms"
    );
    Ok(())
}

/// Light row `idx` of the History list as a copy would, so the render proof
/// shows the confirmation state and not just the resting one.
pub fn flash_history_for_render(ui: &SettingsUi, idx: usize) {
    flash_history_row(ui, idx);
}

/// Open or close the sidebar shelf for a render proof.
pub fn set_sidebar_for_render(ui: &SettingsUi, open: bool) {
    set_sidebar_open(ui, open);
}

pub fn scroll_to_top_for_render(ui: &SettingsUi) {
    scroll_render(ui, false);
}

/// The complete Recording feedback card, regardless of the keyboard card's height.
pub fn show_recording_feedback_for_render(ui: &SettingsUi) {
    select_pane_view(ui, 0);
    if let Some(doc) = ui._scroll.documentView() {
        doc.scrollPoint(NSPoint::new(0.0, ui.feedback_card_y));
        ui._scroll.setNeedsDisplay(true);
        ui._scroll.displayIfNeeded();
    }
}

pub(super) fn scroll_render(ui: &SettingsUi, bottom: bool) {
    let Some(doc) = ui._scroll.documentView() else {
        return;
    };
    let doc_h = doc.frame().size.height;
    let clip_h = ui._scroll.contentSize().height;
    let y = if bottom {
        (doc_h - clip_h).max(0.0)
    } else {
        0.0
    };
    // Flipped document: scrolling "down" means a larger y.
    doc.scrollPoint(NSPoint::new(0.0, y));
    ui._scroll.setNeedsDisplay(true);
    ui._scroll.displayIfNeeded();
}
