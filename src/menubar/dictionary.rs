//! Dictionary (corrections) editor window.
use super::*;

// ─── Dictionary editor window ───────────────────────────────────────────
//
// A small native window mirroring Dictation History: one editable text view,
// one entry per line, plus a Save button. It edits the user's single vocabulary
// list (corrections.json): a bare word means "keep this spelling", and
// "from → to" is a mishearing fix. Save writes corrections.json and relaunches
// the daemon so it takes effect — same apply-on-relaunch model as the settings.

pub(super) struct DictionaryUi {
    pub(super) window: Retained<NSWindow>,
    pub(super) text_view: Retained<NSTextView>,
    pub(super) save_button: Retained<NSButton>,
    /// "Heard" field of the quick-add row (the mis-heard spelling; blank = keep).
    pub(super) heard_field: Retained<NSTextField>,
    /// "Correct to" field of the quick-add row (the spelling you want).
    pub(super) correct_field: Retained<NSTextField>,
    pub(super) add_button: Retained<NSButton>,
}
// Only ever touched on the main thread (menu actions), so parking the AppKit
// pointers in a static is safe.
unsafe impl Send for DictionaryUi {}
unsafe impl Sync for DictionaryUi {}

pub(super) static DICTIONARY_UI: OnceLock<DictionaryUi> = OnceLock::new();

pub(super) fn dictionary_ui(mtm: MainThreadMarker) -> &'static DictionaryUi {
    DICTIONARY_UI.get_or_init(|| build_dictionary_window(mtm))
}

pub(super) fn build_dictionary_window(mtm: MainThreadMarker) -> DictionaryUi {
    let win_w = 440.0;
    let win_h = 440.0;
    let margin = 16.0;
    let btn_w = 88.0;
    let btn_h = 30.0;
    let label_h = 16.0;
    // Quick-add row geometry (two fields + Add button, sitting between the list
    // and the Save button so adding a term never requires typing the arrow).
    let field_h = 24.0;
    let add_btn_w = 64.0;
    let add_hint_h = 14.0;
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(win_w, win_h));

    // Container content view: a hint label on top, the editable text area in the
    // middle, a Save button bottom-right.
    let container: Retained<NSView> = NSView::initWithFrame(NSView::alloc(mtm), frame);

    // Instruction label (in-window, so it isn't truncated like a long subtitle).
    let label_y = win_h - margin - label_h;
    let label: Retained<NSTextField> = unsafe {
        msg_send![
            NSTextField::alloc(mtm),
            initWithFrame: NSRect::new(
                NSPoint::new(margin, label_y),
                NSSize::new(win_w - 2.0 * margin, label_h),
            )
        ]
    };
    unsafe {
        label.setStringValue(&NSString::from_str(
            "One per line — a word to keep spelled as-is, or  heard → Word  to fix a mishearing.",
        ));
        label.setBezeled(false);
        label.setDrawsBackground(false);
        label.setEditable(false);
        label.setSelectable(false);
        label.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        let _: () = msg_send![&*label, setTextColor: &*theme::text_secondary()];
        container.addSubview(&label);
    }

    // Bordered, padded, editable text area — one term per line. It sits above
    // the quick-add row, which sits above the Save button:
    //   Save row:     y = margin                         (h = btn_h)
    //   add hint:     y = margin + btn_h + 12            (h = add_hint_h)
    //   add fields:   y = margin + btn_h + 12 + hint + 8 (h = field_h)
    //   list scroll:  starts above the add fields
    let add_hint_y = margin + btn_h + 12.0;
    let add_row_y = add_hint_y + add_hint_h + 8.0;
    let scroll_y = add_row_y + field_h + 16.0;
    let scroll_frame = NSRect::new(
        NSPoint::new(margin, scroll_y),
        NSSize::new(win_w - 2.0 * margin, label_y - 8.0 - scroll_y),
    );
    let scroll: Retained<NSScrollView> =
        unsafe { msg_send![NSScrollView::alloc(mtm), initWithFrame: scroll_frame] };
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
        scroll.setDrawsBackground(true);
        scroll.setBackgroundColor(&NSColor::textBackgroundColor());
        // Bezel border so the area reads as a defined editable field.
        let _: () = msg_send![&*scroll, setBorderType: 2usize]; // NSBezelBorder

        tv.setEditable(true);
        tv.setRichText(false);
        tv.setFont(Some(&NSFont::systemFontOfSize(13.0)));
        // Padding so the caret/text isn't jammed into the corner.
        let _: () = msg_send![&*tv, setTextContainerInset: NSSize::new(8.0, 8.0)];
        tv.setMinSize(NSSize::new(0.0, content.height));
        tv.setMaxSize(NSSize::new(f64::MAX, f64::MAX));
        tv.setVerticallyResizable(true);
        tv.setHorizontallyResizable(false);
        scroll.setDocumentView(Some(&*tv));
        container.addSubview(&scroll);
    }

    // ── Quick-add row: [Heard…]  →  [Correct to…]  [Add] ──────────────────
    // Type the two parts and click Add; we append a correctly-formatted line to
    // the list above (bare word if Heard is blank, "heard → Word" otherwise), so
    // the user never has to know the arrow syntax. Geometry, left→right:
    //   heard field | arrow label | correct field | Add button (right-aligned)
    let gap = 8.0;
    let arrow_w = 16.0;
    let add_btn_x = win_w - margin - add_btn_w;
    let fields_left = margin;
    let fields_right = add_btn_x - gap;
    let field_w = (fields_right - fields_left - arrow_w - 2.0 * gap) / 2.0;

    let heard_field: Retained<NSTextField> = unsafe {
        msg_send![
            NSTextField::alloc(mtm),
            initWithFrame: NSRect::new(
                NSPoint::new(fields_left, add_row_y),
                NSSize::new(field_w, field_h),
            )
        ]
    };
    let arrow_label: Retained<NSTextField> = unsafe {
        msg_send![
            NSTextField::alloc(mtm),
            initWithFrame: NSRect::new(
                NSPoint::new(fields_left + field_w + gap, add_row_y),
                NSSize::new(arrow_w, field_h),
            )
        ]
    };
    let correct_field: Retained<NSTextField> = unsafe {
        msg_send![
            NSTextField::alloc(mtm),
            initWithFrame: NSRect::new(
                NSPoint::new(fields_left + field_w + gap + arrow_w + gap, add_row_y),
                NSSize::new(field_w, field_h),
            )
        ]
    };
    let add_button: Retained<NSButton> = unsafe {
        msg_send![
            NSButton::alloc(mtm),
            initWithFrame: NSRect::new(
                NSPoint::new(add_btn_x, add_row_y - 2.0),
                NSSize::new(add_btn_w, btn_h),
            )
        ]
    };
    unsafe {
        heard_field.setPlaceholderString(Some(&NSString::from_str("Heard")));
        heard_field.setFont(Some(&NSFont::systemFontOfSize(13.0)));
        correct_field.setPlaceholderString(Some(&NSString::from_str("Correct to")));
        correct_field.setFont(Some(&NSFont::systemFontOfSize(13.0)));

        arrow_label.setStringValue(&NSString::from_str("→"));
        arrow_label.setBezeled(false);
        arrow_label.setDrawsBackground(false);
        arrow_label.setEditable(false);
        arrow_label.setSelectable(false);
        arrow_label.setAlignment(NSTextAlignment::Center);
        arrow_label.setFont(Some(&NSFont::systemFontOfSize(13.0)));
        let _: () = msg_send![&*arrow_label, setTextColor: &*NSColor::secondaryLabelColor()];

        add_button.setTitle(&NSString::from_str("Add"));
        let _: () = msg_send![&*add_button, setBezelStyle: 1usize]; // rounded

        container.addSubview(&heard_field);
        container.addSubview(&arrow_label);
        container.addSubview(&correct_field);
        container.addSubview(&add_button);
    }

    // Hint under the quick-add row.
    let add_hint: Retained<NSTextField> = unsafe {
        msg_send![
            NSTextField::alloc(mtm),
            initWithFrame: NSRect::new(
                NSPoint::new(margin, add_hint_y),
                NSSize::new(win_w - 2.0 * margin, add_hint_h),
            )
        ]
    };
    unsafe {
        add_hint.setStringValue(&NSString::from_str(
            "Leave “Heard” blank to just keep a word spelled the way you type it.",
        ));
        add_hint.setBezeled(false);
        add_hint.setDrawsBackground(false);
        add_hint.setEditable(false);
        add_hint.setSelectable(false);
        add_hint.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        let _: () = msg_send![&*add_hint, setTextColor: &*NSColor::secondaryLabelColor()];
        container.addSubview(&add_hint);
    }

    // Save button, bottom-right.
    let btn_frame = NSRect::new(
        NSPoint::new(win_w - margin - btn_w, margin),
        NSSize::new(btn_w, btn_h),
    );
    let save_button: Retained<NSButton> =
        unsafe { msg_send![NSButton::alloc(mtm), initWithFrame: btn_frame] };
    unsafe {
        save_button.setTitle(&NSString::from_str("Save"));
        let _: () = msg_send![&*save_button, setBezelStyle: 1usize]; // rounded
        container.addSubview(&save_button);
    }

    let style =
        NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Miniaturizable;
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe {
        window.setTitle(&NSString::from_str("Dictionary"));
        window.setReleasedWhenClosed(false);
        window.setContentView(Some(&*container));
        apply_light_chrome(&window);
        window.center();
    }

    DictionaryUi {
        window,
        text_view: tv,
        save_button,
        heard_field,
        correct_field,
        add_button,
    }
}

/// Open the dictionary editor, pre-filled from dictionary.json, wired to save
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
    // (bare word = keep spelling; "from → to" = fix). The in-window label carries
    // the hint, so the title bar stays clean ("Dictionary").
    let joined = crate::corrections::Corrections::load_default()
        .map(|c| c.to_editor_lines())
        .unwrap_or_default()
        .join("\n");
    {
        ui.text_view.setString(&NSString::from_str(&joined));
        ui.window.makeKeyAndOrderFront(None);
    }
    // Accessory app: explicitly activate so the window comes to the front.
    let app = NSApplication::sharedApplication(mtm);
    #[allow(deprecated)]
    {
        app.activateIgnoringOtherApps(true)
    };
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
            unsafe {
                let msg = NSString::from_str(&format!("Saved {count} term(s)"));
                let _: () = msg_send![&*ui.window, setSubtitle: &*msg];
            }
            // The refiner/cleaner read corrections once at boot, so relaunch to
            // apply — same as the model / hotkey settings.
            relaunch_daemon();
        }
        Err(e) => {
            eprintln!("[menu] dictionary save failed: {e}");
            unsafe {
                let msg = NSString::from_str("Save failed — see log");
                let _: () = msg_send![&*ui.window, setSubtitle: &*msg];
            }
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
        unsafe {
            let msg = NSString::from_str("Type the spelling you want in “Correct to”.");
            let _: () = msg_send![&*ui.window, setSubtitle: &*msg];
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
        let msg = NSString::from_str("Added — click Save to apply.");
        let _: () = msg_send![&*ui.window, setSubtitle: &*msg];
        ui.window.makeFirstResponder(Some(&*ui.heard_field));
    }
}

