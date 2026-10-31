//! The Dictionary pane: the add-a-term card and the searchable term list.
use super::*;

/// How much of a dictionary row's text width the heard spelling takes; the
/// replacement gets the rest.
pub(super) const TERM_HEARD_FRACTION: f64 = 0.42;

/// Rows drawn before the list stops and asks for a search instead. A settings
/// pane is a list you scan, not a database you page through.
pub(super) const TERM_ROW_CAP: usize = 60;

// ─── Dictionary list ─────────────────────────────────────────────────────
//
// The Dictionary pane shows the terms themselves: one row per entry, the heard
// spelling on the left and what lands on the right. Rows are drawn into a
// flipped body view inside the last card, and rebuilt in place whenever the
// search text or the dictionary changes — so a row's `tag` is its index into
// `dictionary_keys`, refreshed by the same pass that draws it.

/// The list card's header row: a search glyph, a field that filters as you
/// type, and the chip that opens the JSON file behind the list.
pub(super) fn dictionary_search_row(
    mtm: MainThreadMarker,
    card: &mut Card,
    controller: &Retained<SettingsController>,
) -> Retained<NSSearchField> {
    let y = card_row(card, CONTROL_ROW);
    let mid = y + CONTROL_ROW / 2.0;

    let chip = slate::chip(
        mtm,
        "Open file",
        Some("arrow.up.forward"),
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
    let open_btn = slate::click_overlay(mtm, chip.frame(), controller, sel!(openCorrections:), 0);
    unsafe {
        let title = NSString::from_str("Open dictionary file");
        let _: () = msg_send![&*open_btn, setToolTip: &*title];
        let _: () = msg_send![&*open_btn, setAccessibilityLabel: &*title];
    }
    card.view.addSubview(&open_btn);

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
        let ph = NSString::from_str("Search terms…");
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
        let _: () = msg_send![&*search, setAction: sel!(searchDictionary:)];
    }
    card.view.addSubview(&search);
    search
}

/// Draw one row per entry into `body`, filtered by `filter`, then size `body`
/// to what it drew. Returns the key behind each row, in row order.
pub(super) fn fill_dictionary_body(
    mtm: MainThreadMarker,
    body: &slate::SlateView,
    controller: &Retained<SettingsController>,
    corrections: &crate::corrections::Corrections,
    filter: &str,
) -> Vec<String> {
    for view in body.subviews().iter() {
        view.removeFromSuperview();
    }
    let needle = filter.trim().to_lowercase();
    let matched = corrections.search(&needle);

    let mut keys: Vec<String> = Vec::new();
    let mut y = 0.0;
    for (index, (key, value)) in matched.iter().take(TERM_ROW_CAP).enumerate() {
        y = dictionary_row(mtm, body, controller, index, key, value, y);
        keys.push(key.clone());
    }

    let note = if matched.is_empty() {
        if needle.is_empty() {
            "No terms yet. Add one above.".to_string()
        } else {
            format!("No term matches “{}”.", filter.trim())
        }
    } else if matched.len() > TERM_ROW_CAP {
        format!(
            "Showing {TERM_ROW_CAP} of {} matches. Search to narrow the list.",
            matched.len()
        )
    } else if needle.is_empty() {
        "Click a row to edit it. The trash chip removes it.".to_string()
    } else {
        format!("{} of {} terms match.", matched.len(), corrections.len())
    };
    y = list_note(mtm, body, y, &note);

    body.setFrameSize(NSSize::new(CARD_W, y));
    keys
}

/// One term row: heard spelling, arrow, replacement, and a trash chip. The row
/// click edits, the chip removes; both carry the row index as their tag.
pub(super) fn dictionary_row(
    mtm: MainThreadMarker,
    body: &slate::SlateView,
    controller: &Retained<SettingsController>,
    index: usize,
    key: &str,
    value: &str,
    y: f64,
) -> f64 {
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
    let text_right = trash.frame().origin.x - SPACE_SM;
    let avail = (text_right - x0).max(0.0);
    if key.eq_ignore_ascii_case(value) {
        // An identity entry: the spelling is kept, nothing is replaced.
        let word = clamp_label(
            slate::label(mtm, value, TYPE_LABEL, WEIGHT_MEDIUM, theme::text_primary()),
            x0,
            mid,
            avail,
        );
        let used = word.frame().size.width;
        row.addSubview(&word);
        let note = clamp_label(
            slate::label(
                mtm,
                "kept as spelled",
                TYPE_META,
                WEIGHT_REGULAR,
                theme::text_tertiary(),
            ),
            x0 + used + SPACE_XS,
            mid,
            (avail - used - SPACE_XS).max(0.0),
        );
        row.addSubview(&note);
    } else {
        let heard_w = (avail * TERM_HEARD_FRACTION).round();
        let heard = clamp_label(
            slate::label(mtm, key, TYPE_LABEL, WEIGHT_REGULAR, theme::text_secondary()),
            x0,
            mid,
            heard_w,
        );
        row.addSubview(&heard);
        let arrow = clamp_label(
            slate::label(mtm, "→", TYPE_META, WEIGHT_REGULAR, theme::text_tertiary()),
            x0 + heard_w + SPACE_XS,
            mid,
            avail,
        );
        let arrow_w = arrow.frame().size.width;
        row.addSubview(&arrow);
        let value_x = x0 + heard_w + SPACE_XS + arrow_w + SPACE_XS;
        let replacement = clamp_label(
            slate::label(mtm, value, TYPE_LABEL, WEIGHT_MEDIUM, theme::text_primary()),
            value_x,
            mid,
            (text_right - value_x).max(0.0),
        );
        row.addSubview(&replacement);
    }

    // The row click edits. The trash overlay goes on last, so it wins the hit
    // test over the row beneath it.
    let edit = slate::click_overlay(
        mtm,
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(row_w, CONTROL_ROW)),
        controller,
        sel!(editDictionaryEntry:),
        index as isize,
    );
    unsafe {
        let title = NSString::from_str(&format!("Edit {value}"));
        let _: () = msg_send![&*edit, setToolTip: &*title];
        let _: () = msg_send![&*edit, setAccessibilityLabel: &*title];
    }
    row.addSubview(&edit);
    let remove = slate::click_overlay(
        mtm,
        trash.frame(),
        controller,
        sel!(removeDictionaryEntry:),
        index as isize,
    );
    unsafe {
        let title = NSString::from_str(&format!("Remove {value}"));
        let _: () = msg_send![&*remove, setToolTip: &*title];
        let _: () = msg_send![&*remove, setAccessibilityLabel: &*title];
    }
    row.addSubview(&remove);

    body.addSubview(&row);
    y + CONTROL_ROW
}

/// Redraw the term list from `pending_corrections` and the search field.
pub(super) fn refresh_dictionary_list(ui: &SettingsUi) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let filter = ui.dictionary_search.stringValue().to_string();
    let keys = {
        let corrections = ui.pending_corrections.lock().unwrap();
        fill_dictionary_body(
            mtm,
            &ui.dictionary_body,
            &ui._controller,
            &corrections,
            &filter,
        )
    };
    *ui.dictionary_keys.lock().unwrap() = keys;
    size_list_card(
        &ui.dictionary_pane,
        &ui.dictionary_card,
        ui.dictionary_card_y,
        ui.dictionary_body_y,
        ui.dictionary_body.frame().size.height,
    );
    ui.dictionary_card.setNeedsDisplay(true);
}

/// The `(key, replacement)` a row's tag addresses, or `None` if the list moved
/// under the click.
pub(super) fn dictionary_entry_at(ui: &SettingsUi, tag: isize) -> Option<(String, String)> {
    if tag < 0 {
        return None;
    }
    let key = ui
        .dictionary_keys
        .lock()
        .unwrap()
        .get(tag as usize)
        .cloned()?;
    let value = ui
        .pending_corrections
        .lock()
        .unwrap()
        .entries()
        .into_iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)?;
    Some((key, value))
}

/// Shown in the Dictionary pane when corrections.json cannot be parsed.
pub(super) const DICTIONARY_UNREADABLE: &str = "corrections.json could not be read, so it was left untouched. \
     Open the Corrections folder in About, fix or remove the file, then reopen Settings.";

/// The Dictionary pane: the add-a-term card and the searchable list.
pub(super) struct DictionaryPane {
    pub(super) view: Retained<SettingsFlippedView>,
    pub(super) dictionary_heard_field: Retained<NSTextField>,
    pub(super) dictionary_correct_field: Retained<NSTextField>,
    pub(super) dictionary_status: Retained<NSTextField>,
    pub(super) dictionary_search: Retained<NSSearchField>,
    pub(super) dictionary_pane: Retained<SettingsFlippedView>,
    pub(super) dictionary_card: Retained<slate::SlateView>,
    pub(super) dictionary_body: Retained<slate::SlateView>,
    pub(super) dictionary_card_y: f64,
    pub(super) dictionary_body_y: f64,
    pub(super) dictionary_keys: Vec<String>,
    pub(super) dictionary_load_error: Option<String>,
    pub(super) loaded_corrections: crate::corrections::Corrections,
    pub(super) original_correction_lines: Vec<String>,
}

pub(super) fn build_dictionary_pane(cx: &PaneCx) -> DictionaryPane {
    let mtm = cx.mtm;
    let controller = cx.controller;
    let render_fixture = cx.render_fixture;
    let dictionary = cx.new_pane();
    let mut card = card_begin(mtm, &dictionary, FIRST_CARD_Y, "Add a term");
    let dictionary_heard_field = text_row(mtm, &mut card, "What dictation hears", "");
    dictionary_heard_field.setPlaceholderString(Some(&NSString::from_str("e.g. acmelabs")));
    let dictionary_correct_field = text_row(mtm, &mut card, "Correct to", "");
    dictionary_correct_field.setPlaceholderString(Some(&NSString::from_str("e.g. Acme Labs")));
    action_button_row(
        mtm,
        &mut card,
        "Add to dictionary",
        "Add",
        "plus",
        controller,
        sel!(addDictionaryEntry:),
    );
    let (loaded_corrections, dictionary_load_error) = if render_fixture {
        (crate::corrections::Corrections::from_editor_text(RENDER_DICTIONARY), None)
    } else {
        match crate::corrections::Corrections::load_default() {
            Ok(c) => (c, None),
            Err(e) => {
                eprintln!("[settings] dictionary not loaded: {e}");
                (crate::corrections::Corrections::empty(), Some(DICTIONARY_UNREADABLE.to_string()))
            }
        }
    };
    let original_correction_lines = loaded_corrections.to_editor_lines();
    let dictionary_status = hint_line(
        mtm,
        &mut card,
        &dictionary_load_error.clone().unwrap_or_else(|| {
            format!(
                "{} terms. Replacements run before and after cleanup, including raw terminal dictation.",
                loaded_corrections.len()
            )
        }),
    );
    hint_line(
        mtm,
        &mut card,
        "Leave the first field blank to keep a spelling. Use both to fix a mishearing.",
    );
    let dictionary_card_y = card_end(&card);

    // The terms themselves: one row per entry, what dictation hears on the
    // left and what lands on the right. A row click loads it into the fields
    // above; the trash chip drops it. Both rebuild the rows in place.
    let mut card = card_begin(mtm, &dictionary, dictionary_card_y, "Your dictionary");
    let dictionary_search = dictionary_search_row(mtm, &mut card, controller);
    let dictionary_body_y = card.y;
    let dictionary_body = slate::SlateView::flipped(
        mtm,
        NSRect::new(
            NSPoint::new(0.0, dictionary_body_y),
            NSSize::new(CARD_W, 0.0),
        ),
        slate::Paint::None,
        0.0,
    );
    card.view.addSubview(&dictionary_body);
    let dictionary_card = card.view.clone();
    let dictionary_keys =
        fill_dictionary_body(mtm, &dictionary_body, controller, &loaded_corrections, "");
    let dictionary_pane = dictionary.clone();
    size_list_card(
        &dictionary,
        &dictionary_card,
        dictionary_card_y,
        dictionary_body_y,
        dictionary_body.frame().size.height,
    );
    DictionaryPane {
        view: dictionary,
        dictionary_heard_field,
        dictionary_correct_field,
        dictionary_status,
        dictionary_search,
        dictionary_pane,
        dictionary_card,
        dictionary_body,
        dictionary_card_y,
        dictionary_body_y,
        dictionary_keys,
        dictionary_load_error,
        loaded_corrections,
        original_correction_lines,
    }
}
