//! The row kit every pane builds from: cards, popups, toggles, text rows,
//! buttons, hints, and the readers that pull values back out of them.
use super::*;

// ─── Card builder ────────────────────────────────────────────────────────

/// A group of rows drawn as one raised card. Rows are added top-down inside the
/// card, each with a `divider` above it, and the card is sized to fit at the end.
pub(super) struct Card {
    pub(super) view: Retained<slate::SlateView>,
    /// Next free y inside the card.
    pub(super) y: f64,
    /// Where the card sits in its pane.
    origin_y: f64,
    /// The last thing added is a helper line, not a row: the card's bottom
    /// inset then matches the header's `sm` text inset.
    last_is_helper: bool,
    /// Anything has been added under the header.
    has_rows: bool,
}

pub(super) fn card_begin(mtm: MainThreadMarker, pane: &SettingsFlippedView, y: f64, label_text: &str) -> Card {
    let view = slate::SlateView::flipped(
        mtm,
        NSRect::new(NSPoint::new(PANE_PAD, y), NSSize::new(CARD_W, 0.0)),
        slate::Paint::Card,
        RADIUS_LG,
    );
    let l = slate::section_label(mtm, label_text);
    l.sizeToFit();
    let lh = l.frame().size.height;
    l.setFrame(NSRect::new(
        NSPoint::new(CARD_PAD_X, CARD_LABEL_TOP),
        NSSize::new(CARD_W - 2.0 * CARD_PAD_X, lh),
    ));
    view.addSubview(&l);
    pane.addSubview(&view);
    let header_h = CARD_LABEL_TOP + lh + CARD_LABEL_BELOW;
    view.setFrameSize(NSSize::new(CARD_W, header_h));
    Card {
        view,
        y: header_h,
        origin_y: y,
        last_is_helper: false,
        has_rows: false,
    }
}

/// Reserve a row of `height` inside the card, drawing the `divider` above it.
/// Returns the row's origin y inside the card.
pub(super) fn card_row(card: &mut Card, height: f64) -> f64 {
    let mtm = MainThreadMarker::new().expect("card_row on the main thread");
    // A helper line's text ends `xs` above the next divider, as the header's
    // label does.
    if card.last_is_helper {
        card.y += SPACE_XS;
    }
    let line = slate::SlateView::new(
        mtm,
        NSRect::new(
            NSPoint::new(CARD_PAD_X, card.y),
            NSSize::new(CARD_W - 2.0 * CARD_PAD_X, HAIRLINE),
        ),
        slate::Paint::Divider,
        0.0,
    );
    card.view.addSubview(&line);
    let y = card.y + HAIRLINE;
    card.y = y + height;
    card.last_is_helper = false;
    card.has_rows = true;
    y
}

/// Size the card to its rows and return the pane's next free y.
pub(super) fn card_end(card: &Card) -> f64 {
    let bottom = if card.last_is_helper {
        SPACE_SM
    } else {
        CARD_PAD_Y
    };
    let h = card.y + bottom;
    card.view.setFrameSize(NSSize::new(CARD_W, h));
    card.origin_y + h + SPACE_XL
}

/// Size the pane's document view to the cards it holds, so the scroll view
/// knows how far it can travel.
pub(super) fn finish_pane(pane: &SettingsFlippedView, last: &Card) {
    let h = last.origin_y + last.view.frame().size.height + SPACE_XXXL;
    let current = pane.frame().size.height;
    pane.setFrameSize(NSSize::new(PANE_W, h.max(current)));
}

// ─── Row builders ────────────────────────────────────────────────────────

pub(super) fn row_label(
    mtm: MainThreadMarker,
    card: &Card,
    y: f64,
    height: f64,
    title: &str,
    detail: Option<&str>,
) -> f64 {
    let mid = y + height / 2.0;
    let l = slate::label(mtm, title, TYPE_LABEL, WEIGHT_MEDIUM, theme::text_primary());
    let w = place_left_flipped(&l, CARD_PAD_X, mid);
    card.view.addSubview(&l);
    let mut used = CARD_PAD_X + w;
    if let Some(d) = detail {
        let dl = slate::label(mtm, d, TYPE_CAPTION, WEIGHT_REGULAR, theme::text_tertiary());
        let dw = place_left_flipped(&dl, used + SPACE_XS, mid);
        card.view.addSubview(&dl);
        used += SPACE_XS + dw;
    }
    used
}

/// `slate::place_left` measures against a bottom-up midline; inside a flipped
/// card the same maths applies, because AppKit flips the frame for us.
pub(super) fn place_left_flipped(field: &NSTextField, x: f64, mid_y: f64) -> f64 {
    field.sizeToFit();
    let f = field.frame();
    field.setFrame(NSRect::new(
        NSPoint::new(x, (mid_y - f.size.height / 2.0).round()),
        f.size,
    ));
    f.size.width
}

/// A label on the left and a popup, drawn as a chip, on the right.
pub(super) fn popup_row(mtm: MainThreadMarker, card: &mut Card, title: &str) -> Retained<NSPopUpButton> {
    let y = card_row(card, CONTROL_ROW);
    row_label(mtm, card, y, CONTROL_ROW, title, None);
    let mid = y + CONTROL_ROW / 2.0;
    let shell = slate::SlateView::new(
        mtm,
        NSRect::new(
            NSPoint::new(
                CARD_W - CARD_PAD_X - POPUP_MAX_W,
                (mid - CONTROL_TILE / 2.0).round(),
            ),
            NSSize::new(POPUP_MAX_W, CONTROL_TILE),
        ),
        slate::Paint::Chip,
        RADIUS_TILE,
    );
    let pop: Retained<NSPopUpButton> = NSPopUpButton::initWithFrame_pullsDown(
        NSPopUpButton::alloc(mtm),
        NSRect::new(
            NSPoint::new(SPACE_XXS, 0.0),
            NSSize::new(POPUP_MAX_W - 2.0 * SPACE_XXS, CONTROL_TILE),
        ),
        false,
    );
    {
        pop.setBordered(false);
        pop.setFont(Some(&slate::font(TYPE_META, WEIGHT_REGULAR)));
        unsafe {
            let _: () = msg_send![&*pop, setAccessibilityLabel: &*NSString::from_str(title)];
        }
    }
    shell.addSubview(&pop);
    card.view.addSubview(&shell);
    pop
}

/// Shrink the popup chip to the width its widest title needs, then right-align
/// it. Called once the items are in.
pub(super) fn finish_popup(pop: &NSPopUpButton) {
    pop.sizeToFit();
    let natural = pop.frame().size.width.min(POPUP_MAX_W);
    let w = (natural + 2.0 * SPACE_XXS).min(POPUP_MAX_W);
    pop.setFrame(NSRect::new(
        NSPoint::new(SPACE_XXS, 0.0),
        NSSize::new(w - 2.0 * SPACE_XXS, CONTROL_TILE),
    ));
    if let Some(shell) = unsafe { pop.superview() } {
        let f = shell.frame();
        shell.setFrame(NSRect::new(
            NSPoint::new(CARD_W - CARD_PAD_X - w, f.origin.y),
            NSSize::new(w, CONTROL_TILE),
        ));
    }
}

pub(super) fn popup_add_item(pop: &NSPopUpButton, title: &str, tag: Option<isize>, rep: Option<&str>) {
    pop.addItemWithTitle(&NSString::from_str(title));
    let idx = pop.numberOfItems() - 1;
    if let Some(item) = pop.itemAtIndex(idx) {
        unsafe {
            style_menu_item(&item, title);
            if let Some(t) = tag {
                item.setTag(t);
            }
            if let Some(r) = rep {
                let rep_ns = NSString::from_str(r);
                let rep_obj: &AnyObject = &rep_ns;
                item.setRepresentedObject(Some(rep_obj));
            }
        }
    }
}

/// Render a popup item in the house type scale and ink, so the chip's title is
/// `meta` in `text_primary` rather than the system label colour.
pub(super) unsafe fn style_menu_item(item: &NSMenuItem, title: &str) {
    use objc2_app_kit::{NSFontAttributeName, NSForegroundColorAttributeName};
    use objc2_foundation::{NSMutableAttributedString, NSRange};
    let attrs = NSMutableAttributedString::from_nsstring(&NSString::from_str(title));
    let range = NSRange::new(0, attrs.length());
    attrs.addAttribute_value_range(
        NSFontAttributeName,
        &slate::font(TYPE_META, WEIGHT_REGULAR),
        range,
    );
    attrs.addAttribute_value_range(
        NSForegroundColorAttributeName,
        &theme::text_primary(),
        range,
    );
    item.setAttributedTitle(Some(&attrs));
}

pub(super) fn select_popup_tag(pop: &NSPopUpButton, tag: i64) {
    let _ = pop.selectItemWithTag(tag as isize);
}

pub(super) fn select_popup_rep(pop: &NSPopUpButton, target: &str) {
    if target.is_empty() {
        return;
    }
    let n = pop.numberOfItems();
    for i in 0..n {
        if let Some(item) = pop.itemAtIndex(i) {
            let obj: *mut AnyObject = unsafe { msg_send![&*item, representedObject] };
            let s = if obj.is_null() {
                String::new()
            } else {
                let s: &NSString = unsafe { &*(obj as *const NSString) };
                s.to_string()
            };
            if s == target {
                pop.selectItem(Some(&item));
                return;
            }
        }
    }
}

pub(super) fn wire_popup(
    pop: &NSPopUpButton,
    controller: &Retained<SettingsController>,
    action: objc2::runtime::Sel,
    locked: bool,
) {
    unsafe {
        pop.setTarget(Some(controller));
        pop.setAction(Some(action));
        pop.setEnabled(!locked);
    }
}

/// A label + optional detail on the left, an ink toggle on the right.
pub(super) fn toggle_row(
    mtm: MainThreadMarker,
    card: &mut Card,
    title: &str,
    detail: &str,
) -> Retained<slate::InkToggle> {
    let y = card_row(card, CONTROL_ROW);
    row_label(mtm, card, y, CONTROL_ROW, title, Some(detail));
    let mid = y + CONTROL_ROW / 2.0;
    let sw = slate::InkToggle::new(
        mtm,
        NSRect::new(
            NSPoint::new(
                CARD_W - CARD_PAD_X - slate::TOGGLE_W,
                (mid - slate::TOGGLE_H / 2.0).round(),
            ),
            NSSize::new(slate::TOGGLE_W, slate::TOGGLE_H),
        ),
    );
    card.view.addSubview(&sw);
    unsafe {
        let _: () = msg_send![&*sw, setAccessibilityLabel: &*NSString::from_str(title)];
        let _: () = msg_send![&*sw, setAccessibilityHelp: &*NSString::from_str(detail)];
    }
    sw
}

pub(super) fn wire_switch(
    sw: &slate::InkToggle,
    controller: &Retained<SettingsController>,
    action: objc2::runtime::Sel,
    locked: bool,
) {
    unsafe {
        let _: () = msg_send![sw, setTarget: &**controller];
        let _: () = msg_send![sw, setAction: action];
    }
    sw.set_active(!locked);
}

/// A label on the left and a house text field on the right.
pub(super) fn text_row(
    mtm: MainThreadMarker,
    card: &mut Card,
    title: &str,
    value: &str,
) -> Retained<NSTextField> {
    let y = card_row(card, CONTROL_ROW);
    let used = row_label(mtm, card, y, CONTROL_ROW, title, None);
    let mid = y + CONTROL_ROW / 2.0;
    let left = (used + SPACE_MD).max(CARD_W / 2.0);
    let width = CARD_W - CARD_PAD_X - left;
    let shell = slate::SlateView::new(
        mtm,
        NSRect::new(
            NSPoint::new(left, (mid - CONTROL_CHIP / 2.0).round()),
            NSSize::new(width, CONTROL_CHIP),
        ),
        slate::Paint::Chip,
        RADIUS_SM,
    );
    let field: Retained<NSTextField> = unsafe {
        msg_send![
            NSTextField::alloc(mtm),
            initWithFrame: NSRect::new(
                NSPoint::new(SPACE_XS, ((CONTROL_CHIP - FIELD_TEXT_H) / 2.0).round()),
                NSSize::new(width - 2.0 * SPACE_XS, FIELD_TEXT_H),
            )
        ]
    };
    unsafe {
        field.setBezeled(false);
        field.setDrawsBackground(false);
        field.setAlignment(NSTextAlignment::Right);
        field.setFont(Some(&slate::font(TYPE_META, WEIGHT_REGULAR)));
        let _: () = msg_send![&*field, setTextColor: &*theme::text_primary()];
        let _: () = msg_send![&*field, setFocusRingType: 1usize]; // None
    }
    field.setStringValue(&NSString::from_str(value));
    shell.addSubview(&field);
    card.view.addSubview(&shell);
    field
}

pub(super) const FIELD_TEXT_H: f64 = 18.0;

/// A row whose "control" is an action: a label on the left and an outlined
/// "Open" chip on the right.
pub(super) fn button_row(
    mtm: MainThreadMarker,
    card: &mut Card,
    title: &str,
    controller: &Retained<SettingsController>,
    action: objc2::runtime::Sel,
) {
    action_button_row(
        mtm,
        card,
        title,
        "Open",
        "arrow.up.forward",
        controller,
        action,
    );
}

pub(super) fn action_button_row(
    mtm: MainThreadMarker,
    card: &mut Card,
    title: &str,
    button_label: &str,
    symbol: &str,
    controller: &Retained<SettingsController>,
    action: objc2::runtime::Sel,
) {
    let y = card_row(card, CONTROL_ROW);
    row_label(mtm, card, y, CONTROL_ROW, title, None);
    let mid = y + CONTROL_ROW / 2.0;
    let chip = slate::chip(mtm, button_label, Some(symbol), CARD_W - CARD_PAD_X, mid);
    // A flipped parent flips the chip's y, so re-centre it in card space.
    let f = chip.frame();
    chip.setFrame(NSRect::new(
        NSPoint::new(f.origin.x, (mid - CONTROL_CHIP / 2.0).round()),
        f.size,
    ));
    card.view.addSubview(&chip);
    let btn = slate::click_overlay(mtm, chip.frame(), controller, action, 0);
    unsafe {
        let _: () = msg_send![&*btn, setToolTip: &*NSString::from_str(title)];
        let _: () = msg_send![&*btn, setAccessibilityLabel: &*NSString::from_str(title)];
    }
    card.view.addSubview(&btn);
}

/// The `caption` line under a list's rows: the count, the empty state, or the
/// cap. Shared by the dictionary and history lists.
pub(super) fn list_note(mtm: MainThreadMarker, body: &slate::SlateView, y: f64, text: &str) -> f64 {
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
    let l = slate::label(
        mtm,
        text,
        TYPE_CAPTION,
        WEIGHT_REGULAR,
        theme::text_tertiary(),
    );
    l.setFrame(NSRect::new(
        NSPoint::new(
            CARD_PAD_X,
            (y + (LIST_NOTE_H - TYPE_CAPTION - SPACE_XXS) / 2.0).round(),
        ),
        NSSize::new(CARD_W - 2.0 * CARD_PAD_X, TYPE_CAPTION + SPACE_XXS),
    ));
    body.addSubview(&l);
    y + LIST_NOTE_H
}

/// Size a label to its text but never wider than `max_w` (it truncates), left
/// edge at `x`, centred on `mid_y` inside a flipped parent.
pub(super) fn clamp_label(
    l: Retained<NSTextField>,
    x: f64,
    mid_y: f64,
    max_w: f64,
) -> Retained<NSTextField> {
    l.sizeToFit();
    let f = l.frame();
    l.setFrame(NSRect::new(
        NSPoint::new(x, (mid_y - f.size.height / 2.0).round()),
        NSSize::new(f.size.width.min(max_w), f.size.height),
    ));
    l
}

/// Size a list card to its body, and the pane to the card. A list is always its
/// pane's last card, so growing it moves nothing else. Shared by the dictionary
/// and history lists.
pub(super) fn size_list_card(
    pane: &SettingsFlippedView,
    card: &slate::SlateView,
    card_y: f64,
    body_y: f64,
    body_h: f64,
) {
    let card_h = body_y + body_h + CARD_PAD_Y;
    card.setFrameSize(NSSize::new(CARD_W, card_h));
    let pane_h = (card_y + card_h + SPACE_XXXL).max(PANE_VIEWPORT_H);
    pane.setFrameSize(NSSize::new(PANE_W, pane_h));
}

/// A full-width `caption` line inside a card: hints, env-var locks, and the
/// About facts. A helper sits `xxs` under the row (or helper) above it, with no
/// divider: it belongs to that control. Straight under the card header it is
/// a row of its own and keeps the header's divider.
pub(super) fn hint_line(mtm: MainThreadMarker, card: &mut Card, text: &str) -> Retained<NSTextField> {
    let y = if card.has_rows {
        card.y + SPACE_XXS
    } else {
        card_row(card, 0.0) + SPACE_XS
    };
    card.y = y + HINT_H;
    card.last_is_helper = true;
    let l = slate::label(
        mtm,
        text,
        TYPE_CAPTION,
        WEIGHT_REGULAR,
        theme::text_tertiary(),
    );
    l.setFrame(NSRect::new(
        NSPoint::new(CARD_PAD_X, y),
        NSSize::new(CARD_W - 2.0 * CARD_PAD_X, HINT_H),
    ));
    card.view.addSubview(&l);
    l
}

/// A helper line's frame: one `caption` line.
pub(super) const HINT_H: f64 = TYPE_CAPTION + SPACE_XXS;

/// The count / empty-state line under a list's rows (its own component band).
pub(super) const LIST_NOTE_H: f64 = 22.0;

/// "(locked by DICTATE_…)" footnote.
pub(super) fn env_hint(mtm: MainThreadMarker, card: &mut Card, varname: &str) {
    hint_line(mtm, card, &format!("Locked by the {varname} env var"));
}

// ─── Sender readers (called from the controller's objc methods) ──────────

/// Works for the house ink toggle and for any `NSControl` with a `state`.
pub(super) fn switch_on(sender: *mut AnyObject) -> bool {
    let state: isize = unsafe { msg_send![sender, state] };
    state != 0
}

pub(super) fn field_string(sender: *mut AnyObject) -> String {
    let f: &NSTextField = unsafe { &*(sender as *const NSTextField) };
    f.stringValue().to_string()
}

pub(super) fn popup_selected_tag(sender: *mut AnyObject) -> isize {
    let pop: &NSPopUpButton = unsafe { &*(sender as *const NSPopUpButton) };
    match pop.selectedItem() {
        Some(it) => unsafe { msg_send![&*it, tag] },
        None => 0,
    }
}

pub(super) fn popup_selected_rep(sender: *mut AnyObject) -> String {
    let pop: &NSPopUpButton = unsafe { &*(sender as *const NSPopUpButton) };
    match pop.selectedItem() {
        Some(it) => {
            let obj: *mut AnyObject = unsafe { msg_send![&*it, representedObject] };
            if obj.is_null() {
                String::new()
            } else {
                let s: &NSString = unsafe { &*(obj as *const NSString) };
                s.to_string()
            }
        }
        None => String::new(),
    }
}

/// What every pane builder reads: the main thread, the action target, the
/// settings the window opened with, and whether this is a render fixture.
#[derive(Clone, Copy)]
pub(super) struct PaneCx<'a> {
    pub(super) mtm: MainThreadMarker,
    pub(super) controller: &'a Retained<SettingsController>,
    pub(super) loaded: &'a Settings,
    pub(super) render_fixture: bool,
    pub(super) scroll_h: f64,
}

impl PaneCx<'_> {
    /// One empty document view for the scroll area, as tall as the viewport.
    pub(super) fn new_pane(&self) -> Retained<SettingsFlippedView> {
        unsafe {
            msg_send![
                SettingsFlippedView::alloc(self.mtm),
                initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(PANE_W, self.scroll_h))
            ]
        }
    }
}
