//! Dictionary (corrections) editor — the house panel.
//!
//! Same anatomy as the history panel: an input row on top, a divider, the body,
//! a divider, and a `CONTROL_FOOTER` well carrying the hint on the left and the
//! primary key on the right. The input row is the quick-add pair rather than a
//! search field, because this window edits a list instead of filtering one.
//!
//! It edits the user's single vocabulary list (corrections.json): a bare word
//! means "keep this spelling", and "from → to" is a mishearing fix. Save writes
//! corrections.json and relaunches the daemon so it takes effect — the same
//! apply-on-relaunch model as the settings.

use super::*;

pub(super) struct DictionaryUi {
    pub(super) window: Retained<NSWindow>,
    /// The painted panel ground — what the offscreen renderer snapshots.
    pub(super) panel: Retained<NSView>,
    pub(super) text_view: Retained<NSTextView>,
    pub(super) save_button: Retained<NSButton>,
    /// "Heard" field of the quick-add row (the mis-heard spelling; blank = keep).
    pub(super) heard_field: Retained<NSTextField>,
    /// "Correct to" field of the quick-add row (the spelling you want).
    pub(super) correct_field: Retained<NSTextField>,
    pub(super) add_button: Retained<NSButton>,
    /// Left-hand footer text: the hint, or the save/add confirmation.
    pub(super) footer_status: Retained<NSTextField>,
}
// Only ever touched on the main thread (menu actions), so parking the AppKit
// pointers in a static is safe.
unsafe impl Send for DictionaryUi {}
unsafe impl Sync for DictionaryUi {}

pub(super) static DICTIONARY_UI: OnceLock<DictionaryUi> = OnceLock::new();

pub(super) fn dictionary_ui(mtm: MainThreadMarker) -> &'static DictionaryUi {
    DICTIONARY_UI.get_or_init(|| build_dictionary_window(mtm))
}

/// Height reserved for the traffic lights on a full-size-content window.
pub(super) const TITLEBAR_H: f64 = 28.0;

const DICT_W: f64 = LAYOUT_PANEL_WIDTH;
const DICT_H: f64 = 520.0;
const DICT_HINT: &str =
    "One per line — a word to keep spelled as-is, or  heard → Word  to fix a mishearing.";

pub(super) fn build_dictionary_window(mtm: MainThreadMarker) -> DictionaryUi {
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(DICT_W, DICT_H));
    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::FullSizeContentView;
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };

    // Panel ground. A titled window already has the system corner radius, so
    // the ground is the opaque `surface` rather than the pill's glass.
    let panel = slate::SlateView::new(mtm, frame, slate::Paint::Surface, 0.0);

    // ── Input row: [Heard] → [Correct to] [Add] ────────────────────────
    let row_y = DICT_H - TITLEBAR_H - CONTROL_INPUT;
    let row_mid = row_y + CONTROL_INPUT / 2.0;

    let add_button: Retained<NSButton> = unsafe {
        msg_send![
            NSButton::alloc(mtm),
            initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0))
        ]
    };
    let add_chip = slate::chip(mtm, "Add", Some("plus"), DICT_W - SPACE_LG, row_mid);
    let add_x = add_chip.frame().origin.x;
    panel.addSubview(&add_chip);
    add_button.setTitle(&NSString::from_str(""));
    add_button.setBordered(false);
    add_button.setToolTip(Some(&NSString::from_str("Add this term to the list")));
    unsafe {
        let _: () = msg_send![&*add_button, setTransparent: true];
        let _: () = msg_send![&*add_button, setAccessibilityLabel: &*NSString::from_str("Add term")];
    }
    add_button.setFrame(add_chip.frame());
    panel.addSubview(&add_button);

    let arrow = slate::label(mtm, "→", TYPE_BODY_SMALL, WEIGHT_REGULAR, theme::text_tertiary());
    let fields_left = SPACE_LG;
    let fields_right = add_x - SPACE_SM;
    let arrow_w = SPACE_MD;
    let field_w = ((fields_right - fields_left - arrow_w - 2.0 * SPACE_XS) / 2.0).max(80.0);

    let heard_field = input_field(mtm, &panel, fields_left, row_mid, field_w, "Heard");
    slate::place_left(&arrow, fields_left + field_w + SPACE_XS, row_mid);
    panel.addSubview(&arrow);
    let correct_field = input_field(
        mtm,
        &panel,
        fields_left + field_w + SPACE_XS + arrow_w + SPACE_XS,
        row_mid,
        field_w,
        "Correct to",
    );

    // ── Dividers + footer well ─────────────────────────────────────────
    let top_divider = slate::SlateView::new(
        mtm,
        NSRect::new(NSPoint::new(0.0, row_y - HAIRLINE), NSSize::new(DICT_W, HAIRLINE)),
        slate::Paint::Divider,
        0.0,
    );
    panel.addSubview(&top_divider);
    let bottom_divider = slate::SlateView::new(
        mtm,
        NSRect::new(NSPoint::new(0.0, CONTROL_FOOTER), NSSize::new(DICT_W, HAIRLINE)),
        slate::Paint::Divider,
        0.0,
    );
    panel.addSubview(&bottom_divider);

    let footer = slate::SlateView::new(
        mtm,
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(DICT_W, CONTROL_FOOTER)),
        slate::Paint::Well,
        0.0,
    );
    let fmid = CONTROL_FOOTER / 2.0;
    let footer_status = slate::label(
        mtm,
        DICT_HINT,
        TYPE_META,
        WEIGHT_REGULAR,
        theme::text_secondary(),
    );
    slate::place_left(&footer_status, SPACE_MD, fmid);
    footer.addSubview(&footer_status);

    let (save_views, save_w) = slate::key_hint(mtm, "Save", &["⌘", "S"], 0.0, fmid);
    let save_x = DICT_W - SPACE_SM - save_w;
    for v in &save_views {
        let f = v.frame();
        v.setFrameOrigin(NSPoint::new(f.origin.x + save_x, f.origin.y));
        footer.addSubview(v);
    }
    let save_button: Retained<NSButton> = unsafe {
        msg_send![
            NSButton::alloc(mtm),
            initWithFrame: NSRect::new(
                NSPoint::new(save_x - SPACE_XS, 0.0),
                NSSize::new(save_w + 2.0 * SPACE_XS, CONTROL_FOOTER),
            )
        ]
    };
    save_button.setTitle(&NSString::from_str(""));
    save_button.setBordered(false);
    unsafe {
        let _: () = msg_send![&*save_button, setTransparent: true];
        let _: () = msg_send![&*save_button, setKeyEquivalent: &*NSString::from_str("s")];
        let _: () = msg_send![&*save_button, setAccessibilityLabel: &*NSString::from_str("Save dictionary")];
    }
    footer.addSubview(&save_button);
    panel.addSubview(&footer);

    // ── The list itself: a raised card holding the editor ──────────────
    let card_y = CONTROL_FOOTER + HAIRLINE + SPACE_MD;
    let card_h = row_y - HAIRLINE - SPACE_MD - card_y;
    let card = slate::SlateView::new(
        mtm,
        NSRect::new(
            NSPoint::new(SPACE_LG, card_y),
            NSSize::new(DICT_W - 2.0 * SPACE_LG, card_h),
        ),
        slate::Paint::Card,
        RADIUS_LG,
    );
    let scroll_inset = SPACE_SM;
    let scroll: Retained<NSScrollView> = unsafe {
        msg_send![
            NSScrollView::alloc(mtm),
            initWithFrame: NSRect::new(
                NSPoint::new(scroll_inset, scroll_inset),
                NSSize::new(
                    card.frame().size.width - 2.0 * scroll_inset,
                    card_h - 2.0 * scroll_inset,
                ),
            )
        ]
    };
    let content = scroll.contentSize();
    let tv: Retained<NSTextView> = unsafe {
        msg_send![
            NSTextView::alloc(mtm),
            initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), content)
        ]
    };
    unsafe {
        scroll.setHasVerticalScroller(true);
        scroll.setAutohidesScrollers(true);
        scroll.setDrawsBackground(false);

        tv.setEditable(true);
        tv.setRichText(false);
        tv.setFont(Some(&slate::font(TYPE_BODY_SMALL, WEIGHT_REGULAR)));
        tv.setDrawsBackground(false);
        let _: () = msg_send![&*tv, setTextColor: &*theme::text_primary()];
        let _: () = msg_send![&*tv, setInsertionPointColor: &*theme::text_primary()];
        let _: () = msg_send![&*tv, setTextContainerInset: NSSize::new(SPACE_XS, SPACE_XS)];
        tv.setMinSize(NSSize::new(0.0, content.height));
        tv.setMaxSize(NSSize::new(f64::MAX, f64::MAX));
        tv.setVerticallyResizable(true);
        tv.setHorizontallyResizable(false);
        scroll.setDocumentView(Some(&*tv));
    }
    card.addSubview(&scroll);
    panel.addSubview(&card);

    unsafe {
        window.setTitle(&NSString::from_str("Dictionary"));
        window.setReleasedWhenClosed(false);
        window.setContentView(Some(&*panel));
        slate::apply_panel_chrome(&window);
        window.center();
    }

    DictionaryUi {
        window,
        panel: Retained::into_super(panel),
        text_view: tv,
        save_button,
        heard_field,
        correct_field,
        add_button,
        footer_status,
    }
}

/// A house text field: a `chip_fill` shell at `Radius.sm` with an unbezeled
/// field inside it, so the input reads as one of the family's controls rather
/// than a system bezel.
fn input_field(
    mtm: MainThreadMarker,
    parent: &slate::SlateView,
    x: f64,
    mid_y: f64,
    width: f64,
    placeholder: &str,
) -> Retained<NSTextField> {
    let shell = slate::SlateView::new(
        mtm,
        NSRect::new(
            NSPoint::new(x, (mid_y - CONTROL_CHIP / 2.0).round()),
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
        field.setFont(Some(&slate::font(TYPE_BODY_SMALL, WEIGHT_REGULAR)));
        let _: () = msg_send![&*field, setTextColor: &*theme::text_primary()];
        let _: () = msg_send![&*field, setFocusRingType: 1usize]; // None
        field.setPlaceholderString(Some(&NSString::from_str(placeholder)));
    }
    shell.addSubview(&field);
    parent.addSubview(&shell);
    field
}

/// Text band inside a 28 px field shell.
const FIELD_TEXT_H: f64 = 18.0;

/// Open the dictionary editor, pre-filled from corrections.json, wired to save
/// back to it. `actions` targets the Save button.
pub(super) fn show_dictionary_window(actions: &MenuActions) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let ui = dictionary_ui(mtm);

    // Wire Save → saveDictionary: and Add → addDictionaryEntry: on the actions
    // instance (idempotent).
    unsafe {
        ui.save_button.setTarget(Some(actions));
        ui.save_button.setAction(Some(sel!(saveDictionary:)));
        ui.add_button.setTarget(Some(actions));
        ui.add_button.setAction(Some(sel!(addDictionaryEntry:)));
    }

    // Load the user's vocabulary from corrections.json, rendered as editor lines
    // (bare word = keep spelling; "from → to" = fix). The footer well carries
    // the hint, so the title bar stays empty.
    let joined = crate::corrections::Corrections::load_default()
        .map(|c| c.to_editor_lines())
        .unwrap_or_default()
        .join("\n");
    {
        ui.text_view.setString(&NSString::from_str(&joined));
        set_footer(ui, DICT_HINT);
        ui.window.makeKeyAndOrderFront(None);
    }
    // Accessory app: explicitly activate so the window comes to the front.
    let app = NSApplication::sharedApplication(mtm);
    #[allow(deprecated)]
    {
        app.activateIgnoringOtherApps(true)
    };
}

fn set_footer(ui: &DictionaryUi, text: &str) {
    ui.footer_status.setStringValue(&NSString::from_str(text));
    slate::place_left(&ui.footer_status, SPACE_MD, CONTROL_FOOTER / 2.0);
}

/// Read the editor, write corrections.json, and relaunch so it takes effect.
pub(super) fn save_dictionary_from_view() {
    let Some(ui) = DICTIONARY_UI.get() else {
        return;
    };
    let text = ui.text_view.string().to_string();
    let corrections = crate::corrections::Corrections::from_editor_text(&text);
    let count = corrections.len();
    match corrections.save() {
        Ok(()) => {
            set_footer(ui, &format!("Saved {count} term(s)"));
            // The refiner/cleaner read corrections once at boot, so relaunch to
            // apply — same as the model / hotkey settings.
            relaunch_daemon();
        }
        Err(e) => {
            eprintln!("[menu] dictionary save failed: {e}");
            set_footer(ui, "Save failed — see log");
        }
    }
}

/// Append a row from the quick-add fields to the list, formatted exactly the way
/// the list round-trips: a bare word when "Heard" is blank (or matches the target
/// case-insensitively — an identity/keep entry), else `heard → Word`. This keeps
/// the fields and the free-text list in lock-step with `Corrections::from_editor_text`.
/// Nothing is saved here — the user still clicks Save to write + relaunch.
pub(super) fn add_dictionary_entry_from_fields() {
    let Some(ui) = DICTIONARY_UI.get() else {
        return;
    };
    let heard = ui.heard_field.stringValue().to_string();
    let correct = ui.correct_field.stringValue().to_string();

    // The target spelling is required; "Heard" is optional (blank = keep).
    let Some(line) = crate::corrections::quick_add_line(&heard, &correct) else {
        set_footer(ui, "Type the spelling you want in “Correct to”.");
        {
            ui.window.makeFirstResponder(Some(&*ui.correct_field));
        }
        return;
    };

    unsafe {
        let current = ui.text_view.string().to_string();
        let base = current.trim_end_matches('\n');
        let joined = if base.is_empty() {
            line
        } else {
            format!("{base}\n{line}")
        };
        ui.text_view.setString(&NSString::from_str(&joined));
        let _: () =
            msg_send![&*ui.text_view, scrollToEndOfDocument: std::ptr::null_mut::<AnyObject>()];

        // Clear the fields and return focus to "Heard" for the next entry.
        ui.heard_field.setStringValue(&NSString::from_str(""));
        ui.correct_field.setStringValue(&NSString::from_str(""));
        set_footer(ui, "Added — press ⌘S to apply.");
        ui.window.makeFirstResponder(Some(&*ui.heard_field));
    }
}
