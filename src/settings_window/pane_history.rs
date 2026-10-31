//! The History pane: retention and audio controls, and the searchable,
//! day-grouped history list.
use super::*;

/// The same cap for the history list, which is a longer read: a month of
/// dictation still scrolls, and past this the search field is the faster path.
pub(super) const HISTORY_ROW_CAP: usize = 200;

/// How many rows are pulled from the DB per open. Well past the row cap, so
/// the "narrow your search" note reports a real total rather than the fetch.
pub(super) const HISTORY_FETCH: usize = 2000;

/// The day header band inside the history list: the `section` label plus the
/// air above and below it.
pub(super) const HISTORY_DAY_H: f64 = SPACE_XL;

/// The fixed time column on the left of a history row, so every row's text
/// starts on one edge whatever the hour reads.
pub(super) const HISTORY_TIME_W: f64 = 40.0;

// ─── History list ────────────────────────────────────────────────────────
//
// The History pane shows what was dictated: rows grouped under a day header,
// newest first, the time on the left and the text beside it. A row click
// copies the untruncated dictation; the trash chip deletes it from the store
// immediately (history is data, not a pending setting). Rows are rebuilt in
// place on every search, delete and pane visit — the daemon keeps appending
// while the window is open — so a row's `tag` is its index into the ids and
// texts captured by the same pass that drew it.

/// The count line under the retention row. Also the sentence the pane opens
/// with, so an empty store explains itself instead of showing a blank card.
pub(super) fn history_count_line(count: usize) -> String {
    match count {
        0 => "Nothing stored yet. Hold your hotkey and speak.".to_string(),
        1 => "1 dictation stored.".to_string(),
        n => format!("{n} dictations stored."),
    }
}

/// The list card's header row: a search glyph, a field that filters as you
/// type, and the chip that copies every row currently shown.
pub(super) fn history_search_row(
    mtm: MainThreadMarker,
    card: &mut Card,
    controller: &Retained<SettingsController>,
) -> Retained<NSSearchField> {
    let y = card_row(card, CONTROL_ROW);
    let mid = y + CONTROL_ROW / 2.0;

    let chip = slate::chip(
        mtm,
        "Copy all",
        Some("doc.on.doc"),
        CARD_W - CARD_PAD_X,
        mid,
    );
    // A flipped card flips the chip's y, so re-centre it in card space.
    let cf = chip.frame();
    chip.setFrame(NSRect::new(
        NSPoint::new(cf.origin.x, (mid - CONTROL_CHIP / 2.0).round()),
        cf.size,
    ));
    card.view.addSubview(&chip);
    let copy_all = slate::click_overlay(mtm, chip.frame(), controller, sel!(copyAllHistory:), 0);
    unsafe {
        let title = NSString::from_str("Copy every dictation shown");
        let _: () = msg_send![&*copy_all, setToolTip: &*title];
        let _: () = msg_send![&*copy_all, setAccessibilityLabel: &*title];
    }
    card.view.addSubview(&copy_all);

    if let Some(img) = slate::template_symbol("magnifyingglass") {
        let iv = NSImageView::new(mtm);
        iv.setImage(Some(&img));
        unsafe {
            let _: () = msg_send![&*iv, setContentTintColor: &*theme::text_tertiary()];
        }
        iv.setFrame(NSRect::new(
            NSPoint::new(CARD_PAD_X, (mid - slate::GLYPH / 2.0).round()),
            NSSize::new(slate::GLYPH, slate::GLYPH),
        ));
        card.view.addSubview(&iv);
    }

    let field_x = CARD_PAD_X + slate::GLYPH + SPACE_XS;
    let field_w = (chip.frame().origin.x - SPACE_SM - field_x).max(CARD_PAD_X);
    let search: Retained<NSSearchField> = unsafe {
        msg_send![
            NSSearchField::alloc(mtm),
            initWithFrame: NSRect::new(
                NSPoint::new(field_x, (mid - FIELD_TEXT_H / 2.0).round()),
                NSSize::new(field_w, FIELD_TEXT_H),
            )
        ]
    };
    unsafe {
        // Filter on every keystroke, not just on Return.
        let _: () = msg_send![&*search, setSendsWholeSearchString: false];
        let _: () = msg_send![&*search, setSendsSearchStringImmediately: true];
        let ph = NSString::from_str("Search dictations…");
        let _: () = msg_send![&*search, setPlaceholderString: &*ph];
        // The row draws its own glyph, so strip the cell's bezel and its
        // built-in magnifier / cancel buttons.
        search.setBezeled(false);
        search.setDrawsBackground(false);
        let _: () = msg_send![&*search, setFocusRingType: 1usize]; // None
        search.setFont(Some(&slate::font(TYPE_META, WEIGHT_REGULAR)));
        let _: () = msg_send![&*search, setTextColor: &*theme::text_primary()];
        let cell: *mut AnyObject = msg_send![&*search, cell];
        if !cell.is_null() {
            let _: () = msg_send![cell, setSearchButtonCell: std::ptr::null_mut::<AnyObject>()];
            let _: () = msg_send![cell, setCancelButtonCell: std::ptr::null_mut::<AnyObject>()];
        }
        let _: () = msg_send![&*search, setTarget: &**controller];
        let _: () = msg_send![&*search, setAction: sel!(searchHistory:)];
    }
    card.view.addSubview(&search);
    search
}

/// The newest [`HISTORY_FETCH`] dictations matching `filter`, and how many
/// match in total. The one place the History pane reads the store, so the
/// drawing below stays free of IO and a render proof can hand it sample rows
/// instead. The total is counted in SQL rather than taken from the fetch, so
/// the note under a long store reports the real number.
pub(super) fn history_query(filter: &str) -> crate::history::UiRead<(Vec<crate::history::Entry>, usize)> {
    crate::history::query_ui(filter.trim(), HISTORY_FETCH)
}

/// Draw `matched` as a day-grouped list into `body`, then size `body` to what
/// it drew. Only the newest [`HISTORY_ROW_CAP`] are drawn; `total` (which may
/// exceed `matched`, since the fetch is itself capped) is what the note counts.
/// `filter` is what produced `matched`, and only shapes that note. Returns the
/// row id and the full text behind each row, in row order.
pub(super) fn fill_history_body(
    mtm: MainThreadMarker,
    body: &slate::SlateView,
    controller: &Retained<SettingsController>,
    matched: &[crate::history::Entry],
    total: usize,
    filter: &str,
    read_failure: Option<&str>,
) -> HistoryList {
    for view in body.subviews().iter() {
        view.removeFromSuperview();
    }
    let needle = filter.trim();
    let shown: Vec<crate::history::Entry> = matched.iter().take(HISTORY_ROW_CAP).cloned().collect();

    let mut ids: Vec<i64> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    let mut rows: Vec<Retained<slate::SlateView>> = Vec::new();
    let mut pills: Vec<Retained<slate::SlateView>> = Vec::new();
    let mut trashes: Vec<Retained<slate::SlateView>> = Vec::new();
    let mut y = 0.0;
    // One day-grouping implementation, shared with the launcher's history
    // panel, so both surfaces read the same dates in the same locale.
    for row in menubar::history_rows(&shown) {
        match row {
            menubar::HistoryRow::Day(day) => {
                y = history_day_header(mtm, body, y, &day);
            }
            menubar::HistoryRow::Item { time, text, index } => {
                let (next_y, row_view, copied_pill, trash_chip) =
                    history_row(mtm, body, controller, index, &time, &text, y);
                y = next_y;
                ids.push(shown[index].id);
                texts.push(shown[index].text.clone());
                rows.push(row_view);
                pills.push(copied_pill);
                trashes.push(trash_chip);
            }
        }
    }

    let note = if let Some(why) = read_failure {
        // The read itself failed: say so, whatever the query was.
        if why == HISTORY_BUSY {
            why.to_string()
        } else {
            crate::history::empty_state_text(Some(why), false)
        }
    } else if matched.is_empty() {
        if needle.is_empty() {
            crate::history::empty_state_text(
                crate::history::unreadable_reason().as_deref(),
                false,
            )
        } else {
            format!("No dictation matches “{needle}”.")
        }
    } else if total > shown.len() {
        format!(
            "Showing the newest {} of {total} matches. Search to narrow the list.",
            shown.len()
        )
    } else {
        "Open a dictation to inspect its audio and text layers. The trash chip deletes it."
            .to_string()
    };
    y = list_note(mtm, body, y, &note);

    body.setFrameSize(NSSize::new(CARD_W, y));
    HistoryList {
        ids,
        texts,
        rows,
        pills,
        trashes,
    }
}

/// What one history-list rebuild produced: the store id and full text behind
/// each row (so a click addresses the right entry), plus the row view and its
/// hidden "Copied" chip (so a copy can flash the row it landed on).
pub(super) struct HistoryList {
    pub(super) ids: Vec<i64>,
    pub(super) texts: Vec<String>,
    pub(super) rows: Vec<Retained<slate::SlateView>>,
    pub(super) pills: Vec<Retained<slate::SlateView>>,
    pub(super) trashes: Vec<Retained<slate::SlateView>>,
}

/// A day band: the date in the uppercase `section` style, the same label the
/// launcher's history panel uses to break its list.
pub(super) fn history_day_header(mtm: MainThreadMarker, body: &slate::SlateView, y: f64, day: &str) -> f64 {
    let mut y = y;
    if y > 0.0 {
        let line = slate::SlateView::new(
            mtm,
            NSRect::new(
                NSPoint::new(CARD_PAD_X, y),
                NSSize::new(CARD_W - 2.0 * CARD_PAD_X, HAIRLINE),
            ),
            slate::Paint::Divider,
            0.0,
        );
        body.addSubview(&line);
        y += HAIRLINE;
    }
    let l = slate::section_label(mtm, day);
    l.sizeToFit();
    let lh = l.frame().size.height;
    l.setFrame(NSRect::new(
        NSPoint::new(CARD_PAD_X, (y + (HISTORY_DAY_H - lh) / 2.0).round()),
        NSSize::new(CARD_W - 2.0 * CARD_PAD_X, lh),
    ));
    body.addSubview(&l);
    y + HISTORY_DAY_H
}

/// One dictation row: the time in a fixed column, the text beside it, and a
/// trash chip. The row click copies, the chip deletes; both carry the row index
/// as their tag.
#[allow(clippy::too_many_arguments)]
pub(super) fn history_row(
    mtm: MainThreadMarker,
    body: &slate::SlateView,
    controller: &Retained<SettingsController>,
    index: usize,
    time: &str,
    text: &str,
    y: f64,
) -> (
    f64,
    Retained<slate::SlateView>,
    Retained<slate::SlateView>,
    Retained<slate::SlateView>,
) {
    let row_w = CARD_W - 2.0 * CARD_PAD_X + 2.0 * SPACE_XXS;
    let row = slate::SlateView::flipped(
        mtm,
        NSRect::new(
            NSPoint::new(CARD_PAD_X - SPACE_XXS, y),
            NSSize::new(row_w, CONTROL_ROW),
        ),
        slate::Paint::Row,
        RADIUS_SM,
    );
    row.enable_hover();
    let mid = CONTROL_ROW / 2.0;

    let trash = slate::chip(mtm, "", Some("trash"), row_w - SPACE_XXS, mid);
    let tf = trash.frame();
    trash.setFrame(NSRect::new(
        NSPoint::new(tf.origin.x, (mid - CONTROL_CHIP / 2.0).round()),
        tf.size,
    ));
    row.addSubview(&trash);

    let x0 = SPACE_XXS;
    // Times are monospaced digits so every row's text starts on one edge. The
    // font goes on before the measure: tabular digits are wider than
    // proportional ones, and a label sized to the narrower set truncates.
    let clock = slate::label(mtm, time, TYPE_META, WEIGHT_REGULAR, theme::text_tertiary());
    clock.setFont(Some(&slate::tabular_font(TYPE_META, WEIGHT_REGULAR)));
    clock.sizeToFit();
    let cs = clock.frame().size;
    clock.setFrame(NSRect::new(
        NSPoint::new(x0, (mid - cs.height / 2.0).round()),
        NSSize::new(cs.width.min(HISTORY_TIME_W), cs.height),
    ));
    row.addSubview(&clock);

    let text_x = x0 + HISTORY_TIME_W + SPACE_XS;
    let text_right = trash.frame().origin.x - SPACE_SM;
    let said = clamped_label(
        mtm,
        text,
        TYPE_BODY_SMALL,
        WEIGHT_REGULAR,
        theme::text_primary(),
        text_x,
        mid,
        (text_right - text_x).max(0.0),
    );
    row.addSubview(&said);

    // Copy confirmation. A status line under the retention row is easy to miss
    // when the eye is on the row that was just clicked, so the row itself says
    // it: for [`HISTORY_COPIED_FLASH_S`] the row takes the selection paint and
    // a checkmark chip stands exactly where the trash chip was, which is the
    // one spot on the row that is already reserved. Same frame, so it can
    // never crowd the text; the trash hides underneath it for the flash, so a
    // confirmation is never something you can click to delete by mistake.
    // Built hidden on every row: the flash is a visibility toggle, not a
    // rebuild.
    let copied = slate::chip(mtm, "", Some("checkmark"), row_w - SPACE_XXS, mid);
    copied.setFrame(trash.frame());
    copied.setHidden(true);
    row.addSubview(&copied);

    // The row click copies. The trash overlay goes on last, so it wins the hit
    // test over the row beneath it.
    let copy = slate::click_overlay(
        mtm,
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(row_w, CONTROL_ROW)),
        controller,
        sel!(openHistoryDetail:),
        index as isize,
    );
    unsafe {
        let title = NSString::from_str("Open audio and text layers");
        let _: () = msg_send![&*copy, setToolTip: &*title];
        let _: () = msg_send![&*copy, setAccessibilityLabel: &*title];
    }
    row.addSubview(&copy);
    let remove = slate::click_overlay(
        mtm,
        trash.frame(),
        controller,
        sel!(deleteHistoryEntry:),
        index as isize,
    );
    unsafe {
        let title = NSString::from_str("Delete this dictation");
        let _: () = msg_send![&*remove, setToolTip: &*title];
        let _: () = msg_send![&*remove, setAccessibilityLabel: &*title];
    }
    row.addSubview(&remove);

    body.addSubview(&row);
    (y + CONTROL_ROW, row, copied, trash)
}

/// Re-query the store through the search field and redraw the list.
pub(super) fn refresh_history_list(ui: &SettingsUi) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let filter = ui.history_search.stringValue().to_string();
    refresh_recovery_usage(ui);
    let mut failure: Option<String> = None;
    let (matched, total) = match history_query(&filter) {
        crate::history::UiRead::Ok(found) => found,
        // A writer holds the store: keep the list on screen and say so
        // instead of waiting on it or showing an empty pane.
        crate::history::UiRead::Busy => {
            ui.history_status
                .setStringValue(&NSString::from_str(HISTORY_BUSY));
            return;
        }
        crate::history::UiRead::Failed(why) => {
            failure = Some(why);
            (Vec::new(), 0)
        }
    };
    let list = fill_history_body(
        mtm,
        &ui.history_body,
        &ui._controller,
        &matched,
        total,
        &filter,
        failure.as_deref(),
    );
    adopt_history_list(ui, list);
    size_list_card(
        &ui.history_pane,
        &ui.history_card,
        ui.history_card_y,
        ui.history_body_y,
        ui.history_body.frame().size.height,
    );
    ui.history_card.setNeedsDisplay(true);
}

/// Take a fresh list rebuild into the UI state. Any flash belongs to rows that
/// no longer exist, so it is dropped rather than reapplied to whatever moved
/// into that index.
pub(super) fn adopt_history_list(ui: &SettingsUi, list: HistoryList) {
    clear_history_flash(ui);
    *ui.history_ids.lock().unwrap() = list.ids;
    *ui.history_texts.lock().unwrap() = list.texts;
    *ui.history_rows.lock().unwrap() = list.rows;
    *ui.history_pills.lock().unwrap() = list.pills;
    *ui.history_trashes.lock().unwrap() = list.trashes;
}

/// How long a copied row stays lit. Long enough to register after the eye
/// leaves the pointer, short enough that it never reads as selection.
pub(super) const HISTORY_COPIED_FLASH_S: f64 = 1.1;

/// Light the row a copy landed on: selection paint plus its "Copied" chip.
/// Replaces any previous flash, so a fast run of copies lights one row at a
/// time rather than leaving a trail.
pub(super) fn flash_history_row(ui: &SettingsUi, idx: usize) {
    clear_history_flash(ui);
    if let Some(row) = ui.history_rows.lock().unwrap().get(idx) {
        row.set_selected(true);
    }
    if let Some(trash) = ui.history_trashes.lock().unwrap().get(idx) {
        trash.setHidden(true);
    }
    if let Some(pill) = ui.history_pills.lock().unwrap().get(idx) {
        pill.setHidden(false);
    }
    *ui.history_flashed.lock().unwrap() = Some(idx);
}

/// Put the flashed row back to rest. Safe to call when nothing is lit.
pub(super) fn clear_history_flash(ui: &SettingsUi) {
    let Some(idx) = ui.history_flashed.lock().unwrap().take() else {
        return;
    };
    if let Some(row) = ui.history_rows.lock().unwrap().get(idx) {
        row.set_selected(false);
    }
    if let Some(pill) = ui.history_pills.lock().unwrap().get(idx) {
        pill.setHidden(true);
    }
    if let Some(trash) = ui.history_trashes.lock().unwrap().get(idx) {
        trash.setHidden(false);
    }
}

/// Set the status line under the retention row, then re-state the stored count
/// so the pane never leaves a stale total behind an action.
pub(super) fn set_history_status(ui: &SettingsUi, message: &str) {
    let line = format!("{message} {}", history_count_text());
    ui.history_status.setStringValue(&NSString::from_str(&line));
    refresh_recovery_usage(ui);
}

pub(super) const HISTORY_BUSY: &str = "History is busy, try again.";

pub(super) const HISTORY_DELETE_FAILED: &str = "Could not delete, try again.";

/// The stored-count line, read without waiting on a writer.
pub(super) fn history_count_text() -> String {
    match crate::history::count_ui() {
        crate::history::UiRead::Ok(n) => history_count_line(n),
        crate::history::UiRead::Busy => HISTORY_BUSY.to_string(),
        crate::history::UiRead::Failed(_) => history_count_line(0),
    }
}

pub(super) fn recovery_usage_line() -> String {
    match crate::history::recovery_usage_ui() {
        crate::history::UiRead::Ok(usage) => {
            recovery_usage_summary(usage.audio_count, usage.audio_bytes, usage.record_count)
        }
        crate::history::UiRead::Busy => "Audio storage totals are busy.".to_string(),
        crate::history::UiRead::Failed(_) => "Audio storage totals are unavailable.".to_string(),
    }
}

pub(super) fn recovery_usage_summary(audio_count: usize, audio_bytes: u64, records: usize) -> String {
    let (amount, unit) = if audio_bytes >= 1024 * 1024 * 1024 {
        (audio_bytes as f64 / (1024.0 * 1024.0 * 1024.0), "GB")
    } else {
        (audio_bytes as f64 / (1024.0 * 1024.0), "MB")
    };
    format!("{audio_count} audio clips · {amount:.1} {unit} · {records} recovery records")
}

pub(super) fn refresh_recovery_usage(ui: &SettingsUi) {
    ui.audio_usage_label
        .setStringValue(&NSString::from_str(&recovery_usage_line()));
    let notice = crate::history::recovery_storage_notice();
    ui.recovery_warning_label
        .setStringValue(&NSString::from_str(notice.as_deref().unwrap_or("")));
    ui.recovery_warning_label.setHidden(notice.is_none());
    unsafe {
        let _: () = msg_send![&*ui.recovery_warning_label, setAccessibilityHelp: &*NSString::from_str(notice.as_deref().unwrap_or(""))];
    }
}

/// A one-line preview of `text` for a status message.
pub(super) fn history_preview(text: &str) -> String {
    const PREVIEW_CHARS: usize = 42;
    let flat = text.replace('\n', " ");
    let short: String = flat.chars().take(PREVIEW_CHARS).collect();
    let ellipsis = if flat.chars().count() > PREVIEW_CHARS {
        "…"
    } else {
        ""
    };
    format!("{short}{ellipsis}")
}

/// The `(id, text)` a row's tag addresses, or `None` if the list moved under
/// the click.
pub(super) fn history_entry_at(ui: &SettingsUi, tag: isize) -> Option<(i64, String)> {
    if tag < 0 {
        return None;
    }
    let idx = tag as usize;
    let id = *ui.history_ids.lock().unwrap().get(idx)?;
    let text = ui.history_texts.lock().unwrap().get(idx).cloned()?;
    Some((id, text))
}

/// The History pane: retention, audio recovery and the searchable list.
pub(super) struct HistoryPane {
    pub(super) view: Retained<SettingsFlippedView>,
    pub(super) retention_popup: Retained<NSPopUpButton>,
    pub(super) history_status: Retained<NSTextField>,
    pub(super) audio_usage_label: Retained<NSTextField>,
    pub(super) recovery_warning_label: Retained<NSTextField>,
    pub(super) history_search: Retained<NSSearchField>,
    pub(super) history_card: Retained<slate::SlateView>,
    pub(super) history_body: Retained<slate::SlateView>,
    pub(super) history_card_y: f64,
    pub(super) history_body_y: f64,
    pub(super) initial_history_list: HistoryList,
}

pub(super) fn build_history_pane(cx: &PaneCx) -> HistoryPane {
    let mtm = cx.mtm;
    let controller = cx.controller;
    let loaded = cx.loaded;
    let render_fixture = cx.render_fixture;
    //
    // What was dictated, newest first, grouped by day. The retention window
    // lives here too, beside the rows it governs, rather than three panes away
    // among the input settings.
    let history_pane_view = cx.new_pane();
    let mut card = card_begin(mtm, &history_pane_view, FIRST_CARD_Y, "Keeping");

    let retention_locked = std::env::var_os("DICTATE_HISTORY_RETENTION").is_some();
    let retention_popup = popup_row(mtm, &mut card, "Keep text for");
    for (label_str, rep) in [
        ("Forever", "forever"),
        ("1 year", "1year"),
        ("1 month", "1month"),
        ("1 week", "1week"),
        ("24 hours", "24hours"),
        ("Never", "never"),
    ] {
        popup_add_item(&retention_popup, label_str, None, Some(rep));
    }
    select_popup_rep(&retention_popup, &loaded.resolve_history_retention());
    finish_popup(&retention_popup);
    wire_popup(
        &retention_popup,
        controller,
        sel!(changeRetention:),
        retention_locked,
    );
    if retention_locked {
        env_hint(mtm, &mut card, "DICTATE_HISTORY_RETENTION");
    }
    let audio_history_switch = toggle_row(
        mtm,
        &mut card,
        "Save audio for recovery",
        "Future recordings",
    );
    audio_history_switch.set_on(loaded.resolve_audio_history_enabled());
    wire_switch(
        &audio_history_switch,
        controller,
        sel!(changeAudioHistory:),
        std::env::var_os("DICTATE_AUDIO_HISTORY").is_some(),
    );
    if std::env::var_os("DICTATE_AUDIO_HISTORY").is_some() {
        env_hint(mtm, &mut card, "DICTATE_AUDIO_HISTORY");
    }

    let audio_retention_popup = popup_row(mtm, &mut card, "Keep audio for");
    for (title, value) in [
        ("24 hours", "24hours"),
        ("1 week", "1week"),
        ("30 days", "1month"),
        ("Forever", "forever"),
    ] {
        popup_add_item(&audio_retention_popup, title, None, Some(value));
    }
    let audio_retention = loaded.resolve_audio_retention();
    if audio_retention == "1year" {
        popup_add_item(&audio_retention_popup, "1 year", None, Some("1year"));
    }
    if audio_retention == "never" {
        popup_add_item(&audio_retention_popup, "Never", None, Some("never"));
    }
    select_popup_rep(&audio_retention_popup, &audio_retention);
    finish_popup(&audio_retention_popup);
    wire_popup(
        &audio_retention_popup,
        controller,
        sel!(changeAudioRetention:),
        std::env::var_os("DICTATE_AUDIO_RETENTION").is_some(),
    );
    if std::env::var_os("DICTATE_AUDIO_RETENTION").is_some() {
        env_hint(mtm, &mut card, "DICTATE_AUDIO_RETENTION");
    }

    let audio_cap_popup = popup_row(mtm, &mut card, "Audio storage limit");
    let cap_choices = [
        ("No size limit", 0_u64),
        ("512 MB", 512),
        ("1 GB", 1024),
        ("4 GB", 4096),
        ("10 GB", 10240),
    ];
    for (title, value) in cap_choices {
        popup_add_item(&audio_cap_popup, title, None, Some(&value.to_string()));
    }
    let cap = loaded.resolve_audio_storage_cap_mb();
    if !cap_choices.iter().any(|(_, value)| *value == cap) {
        popup_add_item(
            &audio_cap_popup,
            &format!("{cap} MB"),
            None,
            Some(&cap.to_string()),
        );
    }
    select_popup_rep(&audio_cap_popup, &cap.to_string());
    finish_popup(&audio_cap_popup);
    wire_popup(
        &audio_cap_popup,
        controller,
        sel!(changeAudioStorageCap:),
        std::env::var_os("DICTATE_AUDIO_STORAGE_CAP_MB").is_some(),
    );
    if std::env::var_os("DICTATE_AUDIO_STORAGE_CAP_MB").is_some() {
        env_hint(mtm, &mut card, "DICTATE_AUDIO_STORAGE_CAP_MB");
    }
    hint_line(
        mtm,
        &mut card,
        "A size limit may remove older audio before its retention ends.",
    );
    hint_line(
        mtm,
        &mut card,
        "Removing audio keeps its transcript. Delete all removes both.",
    );
    action_button_row(
        mtm,
        &mut card,
        "Delete every dictation",
        "Delete all",
        "trash",
        controller,
        sel!(clearHistory:),
    );
    let history_line = if render_fixture {
        history_count_line(0)
    } else {
        history_count_text()
    };
    let history_status = hint_line(mtm, &mut card, &history_line);
    let usage = if render_fixture {
        recovery_usage_summary(0, 0, 0)
    } else {
        recovery_usage_line()
    };
    let audio_usage_label = hint_line(mtm, &mut card, &usage);
    let recovery_notice = if render_fixture {
        None
    } else {
        crate::history::recovery_storage_notice()
    };
    let recovery_warning_label =
        hint_line(mtm, &mut card, recovery_notice.as_deref().unwrap_or(""));
    recovery_warning_label.setHidden(recovery_notice.is_none());
    hint_line(
        mtm,
        &mut card,
        "Deletes apply straight away. Retention waits for Apply & Restart.",
    );
    let history_card_y = card_end(&card);

    // The dictations themselves. Same in-place list as the dictionary: a
    // search row on top, rows rebuilt into a flipped body below it, and the
    // card sized to what the body drew.
    let mut card = card_begin(mtm, &history_pane_view, history_card_y, "Your dictations");
    let history_search = history_search_row(mtm, &mut card, controller);
    let history_body_y = card.y;
    let history_body = slate::SlateView::flipped(
        mtm,
        NSRect::new(NSPoint::new(0.0, history_body_y), NSSize::new(CARD_W, 0.0)),
        slate::Paint::None,
        0.0,
    );
    card.view.addSubview(&history_body);
    let history_card = card.view.clone();
    let mut initial_failure: Option<String> = None;
    let (initial_history, initial_history_total) = if render_fixture {
        (Vec::new(), 0)
    } else {
        match history_query("") {
            crate::history::UiRead::Ok(found) => found,
            crate::history::UiRead::Busy => {
                initial_failure = Some(HISTORY_BUSY.to_string());
                (Vec::new(), 0)
            }
            crate::history::UiRead::Failed(why) => {
                initial_failure = Some(why);
                (Vec::new(), 0)
            }
        }
    };
    let initial_history_list = fill_history_body(
        mtm,
        &history_body,
        controller,
        &initial_history,
        initial_history_total,
        "",
        initial_failure.as_deref(),
    );
    size_list_card(
        &history_pane_view,
        &history_card,
        history_card_y,
        history_body_y,
        history_body.frame().size.height,
    );
    HistoryPane {
        view: history_pane_view,
        retention_popup,
        history_status,
        audio_usage_label,
        recovery_warning_label,
        history_search,
        history_card,
        history_body,
        history_card_y,
        history_body_y,
        initial_history_list,
    }
}
