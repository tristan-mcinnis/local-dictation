//! Compact, nonactivating dictation feedback: a recording dot and live levels,
//! a hover cancel target, optional transcript preview, and named work stages.
//! Hover changes only the glyph. It never expands or activates the window.

use super::*;

/// Build an NSImage from an SF Symbol name, as a template image (it adopts the
/// menu-bar tint). Returns None if the symbol doesn't exist.
pub(super) fn sf_symbol(name: &str) -> Option<Retained<NSImage>> {
    let ns_name = NSString::from_str(name);
    let base = NSImage::imageWithSystemSymbolName_accessibilityDescription(&ns_name, None)?;
    // Status item (DESIGN.md): every house glyph at `CONTROL_STATUSGLYPH`,
    // medium weight, so the items share one baseline and one weight.
    let img = base.imageWithSymbolConfiguration(&status_glyph_configuration())?;
    img.setTemplate(true);
    Some(img)
}

/// The one symbol configuration every house status item uses.
fn status_glyph_configuration() -> Retained<NSImageSymbolConfiguration> {
    // SAFETY: NSFontWeightMedium is a plain static CGFloat exported by AppKit.
    let medium = unsafe { objc2_app_kit::NSFontWeightMedium };
    NSImageSymbolConfiguration::configurationWithPointSize_weight(CONTROL_STATUSGLYPH, medium)
}

/// The same glyph, rendered once into `danger` so the recording state reads as
/// recording without relying on animation. Template images adopt the menu-bar
/// tint, so the tinted copy must be a real (non-template) bitmap.
pub(super) fn sf_symbol_tinted(name: &str, color: &NSColor) -> Option<Retained<NSImage>> {
    let ns_name = NSString::from_str(name);
    let base = NSImage::imageWithSystemSymbolName_accessibilityDescription(&ns_name, None)?;
    // A hierarchical symbol configuration is the native way to colour an SF
    // Symbol; a template image would ignore it and adopt the menu-bar tint.
    let cfg = status_glyph_configuration().configurationByApplyingConfiguration(
        &NSImageSymbolConfiguration::configurationWithHierarchicalColor(color),
    );
    let tinted = base.imageWithSymbolConfiguration(&cfg)?;
    tinted.setTemplate(false);
    Some(tinted)
}

/// Every view the pill can show. All are built once; `layout_pill` positions
/// the ones this state needs and hides the rest.
pub(super) struct PillBuild {
    pub(super) window: Retained<NSWindow>,
    pub(super) height: f64,
    pub(super) storage_warning: std::cell::Cell<bool>,
    pub(super) cancel_glyph: Retained<NSImageView>,
    /// The blur material; also the pill's content container.
    pub(super) glass: Retained<NSView>,
    /// The tint / stroke / highlight painted over the material.
    pub(super) tint: Retained<slate::SlateView>,
    /// The clear track holding the live-level bars.
    pub(super) well: Retained<slate::SlateView>,
    pub(super) waveform: Retained<slate::WaveformView>,
    /// Status dot for the success / error lines.
    pub(super) dot: Retained<slate::SlateView>,
    /// Live preview, processing stage, or completion/error outcome.
    pub(super) status: Retained<NSTextField>,
    /// "Undo" or "History", paired with the corresponding click target.
    pub(super) hint: Retained<NSTextField>,
    /// Key caps for the completion and recovery actions.
    pub(super) caps: Vec<Retained<slate::SlateView>>,
    /// Named native hit targets: the leading cancel control and completion Undo.
    pub(super) cancel: Retained<NSButton>,
    pub(super) undo: Retained<NSButton>,
    pub(super) history: Retained<NSButton>,
}
// SAFETY: UiGlobals keeps these AppKit objects alive; every read or mutation
// runs on the main CFRunLoop. Worker threads use only ui_channel.
unsafe impl Send for PillBuild {}
unsafe impl Sync for PillBuild {}

/// State layout. Hover changes the leading glyph without changing this layout.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum PillLayout {
    Recording,
    Processing,
    Success,
    Copied,
    Error,
}

// A pill can be clicked without taking the keyboard or activating this app.
define_class!(
    #[unsafe(super(NSPanel))]
    #[name = "LDDictationPillPanel"]
    struct PillPanel;
    impl PillPanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool { false }
        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main_window(&self) -> bool { false }
    }
);

/// Build the floating pill window.
pub(super) fn build_pill_window(
    mtm: MainThreadMarker,
    actions: &Retained<MenuActions>,
    size: PillSize,
) -> eyre::Result<PillBuild> {
    let height = pill_height(size);
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(PILL_W, height));
    let panel: Retained<PillPanel> = unsafe {
        msg_send![PillPanel::alloc(mtm), initWithContentRect: frame,
            styleMask: NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            backing: NSBackingStoreType::Buffered, defer: false]
    };
    unsafe {
        let _: () = msg_send![&*panel, setBecomesKeyOnlyIfNeeded: true];
        let _: () = msg_send![&*panel, setFloatingPanel: true];
        let _: () = msg_send![&*panel, setHidesOnDeactivate: false];
    }
    let window: Retained<NSWindow> = Retained::into_super(Retained::into_super(panel));
    {
        window.setLevel(NSWindowLevel::from(3_isize)); // floating panel
        window.setOpaque(false);
        let clear = NSColor::clearColor();
        window.setBackgroundColor(Some(&clear));
        // The pill is intentionally mouse-aware: the timer samples the
        // pointer against the window frame; hover changes only its cancel glyph.
        window.setIgnoresMouseEvents(false);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        // The two panel shadows (`SHADOW_PANEL_NEAR` + `SHADOW_PANEL_FAR`) are
        // what AppKit draws for a floating borderless window; asking for both
        // by hand would double them.
        window.setHasShadow(true);
    }

    // ── Glass: material + panel tint / stroke / top highlight ──────────
    let (glass, tint) = slate::glass(mtm, frame, height / 2.0);

    // ── The waveform, in its sunken well ───────────────────────────────
    let well = slate::SlateView::new(
        mtm,
        NSRect::new(
            NSPoint::new(PILL_PAD, ((height - WELL_H) / 2.0).round()),
            NSSize::new(2.0 * WELL_PAD + BAR_BLOCK_W, WELL_H),
        ),
        slate::Paint::None,
        WELL_RADIUS,
    );
    let waveform = slate::WaveformView::new(
        mtm,
        NSRect::new(
            NSPoint::new(WELL_PAD, 0.0),
            NSSize::new(BAR_BLOCK_W, WELL_H),
        ),
    );
    well.addSubview(&waveform);
    glass.addSubview(&well);

    // ── Status line pieces ─────────────────────────────────────────────
    let dot = slate::status_dot(mtm, true, PILL_TEXT_PAD, height / 2.0);
    dot.setHidden(true);
    glass.addSubview(&dot);

    let status = slate::label(mtm, "", TYPE_META, WEIGHT_REGULAR, theme::text_secondary());
    unsafe {
        let _: () =
            msg_send![&*status, setAccessibilityLabel: &*NSString::from_str("Dictation status")];
    }
    status.setHidden(true);
    glass.addSubview(&status);

    let hint = slate::label(mtm, "", TYPE_META, WEIGHT_REGULAR, theme::text_tertiary());
    hint.setHidden(true);
    glass.addSubview(&hint);

    // Only the real completion shortcut (⌘Z) needs key caps.
    let mut caps = Vec::new();
    for _ in 0..2 {
        let cap = slate::key_cap(mtm, "esc", 0.0, height / 2.0);
        cap.setHidden(true);
        glass.addSubview(&cap);
        caps.push(cap);
    }

    // Invisible hit targets: the shelf's `esc` cap cancels, the success line's
    // ⌘Z pair undoes. The caps stay the visual affordance; these carry the
    // click, so the pill keeps every action it had.
    let cancel: Retained<NSButton> = unsafe {
        msg_send![
            NSButton::alloc(mtm),
            initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0))
        ]
    };
    cancel.setTitle(&NSString::from_str(""));
    cancel.setBordered(false);
    cancel.setToolTip(Some(&NSString::from_str("Cancel recording (Escape)")));
    unsafe {
        let _: () = msg_send![&*cancel, setTransparent: true];
        let _: () = msg_send![&*cancel, setRefusesFirstResponder: true];
        let _: () = msg_send![&*cancel, setTarget: &**actions];
        let _: () = msg_send![&*cancel, setAction: sel!(cancelPill:)];
        let _: () = msg_send![&*cancel, setKeyEquivalent: &*NSString::from_str("\u{1b}")];
        let _: () = msg_send![&*cancel, setKeyEquivalentModifierMask: objc2_app_kit::NSEventModifierFlags::empty()];
        let _: () =
            msg_send![&*cancel, setAccessibilityLabel: &*NSString::from_str("Cancel recording")];
    }
    cancel.setHidden(true);
    glass.addSubview(&cancel);

    let undo: Retained<NSButton> = unsafe {
        msg_send![
            NSButton::alloc(mtm),
            initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0))
        ]
    };
    undo.setTitle(&NSString::from_str(""));
    undo.setBordered(false);
    undo.setToolTip(Some(&NSString::from_str("Undo the last dictation (⌘Z)")));
    unsafe {
        let _: () = msg_send![&*undo, setTransparent: true];
        let _: () = msg_send![&*undo, setTarget: &**actions];
        let _: () = msg_send![&*undo, setAction: sel!(undoPill:)];
        let _: () =
            msg_send![&*undo, setAccessibilityLabel: &*NSString::from_str("Undo dictation")];
    }
    undo.setHidden(true);
    glass.addSubview(&undo);

    let history = slate::click_overlay(
        mtm,
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)),
        &**actions,
        sel!(openHistory:),
        0,
    );
    history.setToolTip(Some(&NSString::from_str("Open Dictation History")));
    unsafe {
        let _: () = msg_send![&*history, setAccessibilityLabel: &*NSString::from_str("Open Dictation History")];
        let _: () = msg_send![&*history, setRefusesFirstResponder: true];
    }
    history.setHidden(true);
    glass.addSubview(&history);

    let cancel_glyph = NSImageView::new(mtm);
    cancel_glyph.setImage(slate::template_symbol("xmark").as_deref());
    unsafe {
        let _: () = msg_send![&*cancel_glyph, setContentTintColor: &*theme::text_primary()];
        let _: () = msg_send![&*cancel_glyph, setAccessibilityElement: false];
        let _: () = msg_send![&*dot, setAccessibilityElement: true];
        let _: () = msg_send![&*dot, setAccessibilityRole: &*NSString::from_str("AXImage")];
        let _: () = msg_send![&*dot, setAccessibilityLabel: &*NSString::from_str("Recording")];
        let _: () =
            msg_send![&*window, setAccessibilityLabel: &*NSString::from_str("Local Dictation")];
    }
    cancel_glyph.setHidden(true);
    // The glyph is decorative; keep the transparent button above it.
    glass.addSubview(&cancel_glyph);
    cancel.removeFromSuperview();
    glass.addSubview(&cancel);
    window.setContentView(Some(&*glass));

    Ok(PillBuild {
        window,
        height,
        storage_warning: std::cell::Cell::new(false),
        cancel_glyph,
        glass,
        tint,
        well,
        waveform,
        dot,
        status,
        hint,
        caps,
        cancel,
        undo,
        history,
    })
}

// ─── Layout ──────────────────────────────────────────────────────────────

/// Lay the pill out for `layout` and return the width the window needs.
/// Everything is measured, so a longer word never clips.
pub(super) fn pill_height(size: PillSize) -> f64 {
    match size {
        PillSize::Compact => PILL_H,
        PillSize::Standard => CONTROL_PILL,
    }
}

pub(super) fn layout_pill(pill: &PillBuild, layout: PillLayout) -> f64 {
    let mid = pill.height / 2.0;
    for cap in &pill.caps {
        cap.setHidden(true);
    }
    pill.cancel.setHidden(true);
    pill.cancel_glyph.setHidden(true);
    pill.undo.setHidden(true);
    pill.history.setHidden(true);
    let control_x = PILL_PAD;
    pill.cancel.setFrame(NSRect::new(
        NSPoint::new(control_x, 0.0),
        NSSize::new(CANCEL_HIT, pill.height),
    ));
    let glyph = CONTROL_KEYCAP - SPACE_XXS;
    pill.cancel_glyph.setFrame(NSRect::new(
        NSPoint::new(
            control_x + (CANCEL_HIT - glyph) / 2.0,
            (pill.height - glyph) / 2.0,
        ),
        NSSize::new(glyph, glyph),
    ));
    let width = match layout {
        PillLayout::Recording => {
            pill.well.setHidden(false);
            pill.dot.setHidden(false);
            pill.dot.set_paint(slate::Paint::DotDanger);
            pill.dot.setFrameOrigin(NSPoint::new(
                control_x + (CANCEL_HIT - slate::DOT) / 2.0,
                mid - slate::DOT / 2.0,
            ));
            pill.cancel.setHidden(false);
            pill.hint.setHidden(true);
            pill.well.setFrameOrigin(NSPoint::new(
                control_x + CANCEL_HIT + SPACE_XXS,
                (pill.height - WELL_H) / 2.0,
            ));
            let has_preview = pill.status.stringValue().length() > 0;
            pill.status.setHidden(!has_preview);
            unsafe {
                let _: () =
                    msg_send![&*pill.status, setLineBreakMode: NSLineBreakMode::ByTruncatingHead];
            }
            if has_preview {
                place_bounded_label(&pill.status, PILL_W, mid, PREVIEW_W);
                PILL_W + PREVIEW_W + PILL_PAD
            } else {
                PILL_W
            }
        }
        PillLayout::Processing => {
            pill.well.setHidden(true);
            pill.dot.setHidden(false);
            pill.dot.set_paint(slate::Paint::DotNeutral);
            pill.dot.setFrameOrigin(NSPoint::new(
                control_x + (CANCEL_HIT - slate::DOT) / 2.0,
                mid - slate::DOT / 2.0,
            ));
            pill.status.setHidden(false);
            pill.hint.setHidden(true);
            unsafe {
                let _: () =
                    msg_send![&*pill.status, setLineBreakMode: NSLineBreakMode::ByTruncatingTail];
            }
            let x = control_x + CANCEL_HIT + PILL_GAP;
            let text_width = if pill.storage_warning.get() {
                PROCESSING_TEXT_W * 2.0
            } else {
                PROCESSING_TEXT_W
            };
            place_bounded_label(&pill.status, x, mid, text_width);
            x + text_width + PILL_PAD
        }
        PillLayout::Success => {
            pill.well.setHidden(true);
            pill.dot.setHidden(false);
            pill.status.setHidden(false);
            pill.hint.setHidden(false);
            pill.dot.set_paint(slate::Paint::DotSuccess);
            pill.dot.setFrameOrigin(NSPoint::new(
                PILL_TEXT_PAD,
                (mid - slate::DOT / 2.0).round(),
            ));
            let mut x = PILL_TEXT_PAD + slate::DOT + PILL_GAP;
            x += slate::place_left(&pill.status, x, mid) + PILL_GAP;
            x += slate::place_left(&pill.hint, x, mid) + PILL_GAP;
            let c0 = place_cap(&pill.caps[0], "⌘", x, mid);
            x += c0 + SPACE_XXS;
            x += place_cap(&pill.caps[1], "Z", x, mid);
            pill.undo.setFrame(NSRect::new(
                NSPoint::new(
                    x - c0 - SPACE_XXS - pill.hint.frame().size.width - PILL_GAP,
                    0.0,
                ),
                NSSize::new(
                    c0 + SPACE_XXS
                        + pill.caps[1].frame().size.width
                        + pill.hint.frame().size.width
                        + PILL_GAP,
                    pill.height,
                ),
            ));
            pill.undo.setHidden(false);
            x + PILL_GAP + 2.0
        }
        PillLayout::Copied => {
            pill.well.setHidden(true);
            pill.dot.setHidden(false);
            pill.dot.set_paint(slate::Paint::DotNeutral);
            pill.dot
                .setFrameOrigin(NSPoint::new(PILL_TEXT_PAD, mid - slate::DOT / 2.0));
            pill.status.setHidden(false);
            pill.hint.setHidden(true);
            let x = PILL_TEXT_PAD + slate::DOT + PILL_GAP;
            x + slate::place_left(&pill.status, x, mid) + PILL_TEXT_PAD
        }
        PillLayout::Error => {
            pill.well.setHidden(true);
            pill.dot.setHidden(false);
            pill.status.setHidden(false);
            pill.hint.setHidden(false);
            pill.dot.set_paint(slate::Paint::DotDanger);
            pill.dot.setFrameOrigin(NSPoint::new(
                PILL_TEXT_PAD,
                (mid - slate::DOT / 2.0).round(),
            ));
            let mut x = PILL_TEXT_PAD + slate::DOT + PILL_GAP;
            x += slate::place_left(&pill.status, x, mid) + PILL_GAP;
            let hint_width = slate::place_left(&pill.hint, x + SPACE_XXS, mid);
            let hit_width = (hint_width + 2.0 * SPACE_XXS).max(CANCEL_HIT);
            pill.history.setFrame(NSRect::new(
                NSPoint::new(x, 0.0),
                NSSize::new(hit_width, pill.height),
            ));
            pill.history.setHidden(false);
            x + hit_width + PILL_TEXT_PAD
        }
    };
    // The error pill is the one surface that takes a `danger` hairline, so the
    // failure reads at a glance without recolouring anything else.
    pill.tint.set_danger_stroke(layout == PillLayout::Error);
    width.round()
}

/// Retitle a key cap in place and position it. Returns its width.
fn place_bounded_label(field: &NSTextField, x: f64, mid: f64, width: f64) {
    field.sizeToFit();
    let height = field.frame().size.height;
    field.setFrame(NSRect::new(
        NSPoint::new(x, (mid - height / 2.0).round()),
        NSSize::new(width.round(), height),
    ));
}

fn place_cap(cap: &slate::SlateView, text: &str, x: f64, mid_y: f64) -> f64 {
    let subs = cap.subviews();
    if let Some(first) = subs.iter().next() {
        let field: &NSTextField = unsafe { &*(&*first as *const NSView as *const NSTextField) };
        field.setStringValue(&NSString::from_str(text));
        field.sizeToFit();
        let tw = field.frame().size.width;
        let w = (tw + 2.0 * 5.0).max(CONTROL_KEYCAP).round();
        cap.setFrame(NSRect::new(
            NSPoint::new(x.round(), (mid_y - CONTROL_KEYCAP / 2.0).round()),
            NSSize::new(w, CONTROL_KEYCAP),
        ));
        let th = field.frame().size.height;
        field.setFrame(NSRect::new(
            NSPoint::new(
                ((w - tw) / 2.0).round(),
                ((CONTROL_KEYCAP - th) / 2.0).round(),
            ),
            NSSize::new(tw, th),
        ));
    }
    cap.setHidden(false);
    cap.frame().size.width
}

/// CFRunLoopTimer firing ~30 FPS. On every tick: update bar heights from
/// recent audio levels. On state transitions: show/hide pill, swap icon.
pub(super) fn install_poll_timer() {
    unsafe extern "C-unwind" fn timer_cb(_t: *mut CFRunLoopTimer, _info: *mut c_void) {
        let globals = match GLOBALS.get() {
            Some(g) => g,
            None => return,
        };
        let state = ui_channel::state();
        let now = state as u8;
        let prev = globals.last_state.swap(now, Ordering::SeqCst);
        if now != prev {
            apply_state_transition(globals, state);
        }
        if state == UiState::Recording {
            update_bars(globals);
        }
        if state.is_active() {
            update_live_feedback(globals, state);
            update_pill_hover(globals);
        }
    }

    unsafe {
        let mut ctx = CFRunLoopTimerContext {
            version: 0,
            info: std::ptr::null_mut(),
            retain: None,
            release: None,
            copyDescription: None,
        };
        let timer = CFRunLoopTimer::new(
            None,
            CFAbsoluteTimeGetCurrent() + 0.033,
            0.033, // 30 FPS
            0,
            0,
            Some(timer_cb),
            &mut ctx,
        )
        .expect("CFRunLoopTimer::new");
        let run_loop = CFRunLoop::main().expect("main run loop");
        run_loop.add_timer(Some(&timer), kCFRunLoopCommonModes);
        std::mem::forget(timer);
    }
}

pub(super) fn apply_state_transition(globals: &UiGlobals, state: UiState) {
    let mtm = match MainThreadMarker::new() {
        Some(m) => m,
        None => return,
    };

    // One glyph family — the waveform, matching the app icon. Idle is the
    // template ink; recording is the same shape tinted `danger`; processing is
    // the template at 40 %, which is the rule's "busy" treatment.
    let icon = if state == UiState::Recording {
        &globals.icon_recording
    } else if state.is_processing() || matches!(state, UiState::Error | UiState::NoEditableField) {
        &globals.icon_processing
    } else {
        &globals.icon_idle
    };
    if let Some(button) = globals.status_item.button(mtm) {
        if let Some(img) = icon {
            button.setImage(Some(img));
        } else {
            button.setTitle(&NSString::from_str(if state == UiState::Recording {
                "●"
            } else if state.is_processing() {
                "◌"
            } else if matches!(state, UiState::Error | UiState::NoEditableField) {
                "!"
            } else {
                "◯"
            }));
        }
        button.setAlphaValue(if state.is_processing() {
            PROCESSING_ALPHA
        } else {
            1.0
        });
        button.setToolTip(Some(&NSString::from_str(&format!(
            "Local Dictation · {}",
            pill_status(state, globals.show_processing_detail)
        ))));
    }
    globals.pill_hovered.store(false, Ordering::Relaxed);
    globals.pill.storage_warning.set(false);
    globals.pill.status.setToolTip(None);
    if let Ok(mut feedback) = globals.pill_feedback.lock() {
        *feedback = None;
    }
    unsafe {
        let _: () = msg_send![&*globals.pill.dot, setAccessibilityLabel: &*NSString::from_str(pill_status(state, globals.show_processing_detail))];
    }

    match state {
        UiState::Idle => {
            // A dictation just completed (or we were cancelled) — refresh the
            // "Last: …" preview so the menu shows the newest text.
            {
                globals
                    .last_item
                    .setTitle(&NSString::from_str(&last_dictation_label()));
            }
            ui_channel::reset_levels();
            collapse_bars(globals);
            globals.pill.status.setStringValue(&NSString::from_str(""));
            set_pill_layout(globals, PillLayout::Recording);
            globals.pill_window.orderOut(None);
        }
        UiState::Recording => {
            globals.pill.waveform.set_muted(false);
            globals.pill.status.setStringValue(&NSString::from_str(""));
            set_pill_layout(globals, PillLayout::Recording);
            set_cancel_hover(&globals.pill, state, false);
            show_pill(globals);
        }
        UiState::Processing | UiState::Transcribing | UiState::Cleaning | UiState::Pasting => {
            globals
                .pill
                .status
                .setStringValue(&NSString::from_str(pill_status(
                    state,
                    globals.show_processing_detail,
                )));
            set_pill_layout(globals, PillLayout::Processing);
            set_cancel_hover(&globals.pill, state, false);
            show_pill(globals);
        }
        UiState::Copied => {
            globals
                .pill
                .status
                .setStringValue(&NSString::from_str("Copied to clipboard"));
            set_pill_layout(globals, PillLayout::Copied);
            show_pill(globals);
        }
        UiState::Success => {
            let words = ui_channel::last_dictation().split_whitespace().count();
            let text = if words == 1 {
                "Landed · 1 word".to_string()
            } else {
                format!("Landed · {words} words")
            };
            globals
                .pill
                .status
                .setStringValue(&NSString::from_str(&text));
            globals
                .pill
                .hint
                .setStringValue(&NSString::from_str("Undo"));
            set_pill_layout(globals, PillLayout::Success);
            show_pill(globals);
        }
        UiState::Error => {
            globals
                .pill
                .status
                .setStringValue(&NSString::from_str("Couldn’t transcribe"));
            globals
                .pill
                .hint
                .setStringValue(&NSString::from_str("History"));
            set_pill_layout(globals, PillLayout::Error);
            show_pill(globals);
        }
        UiState::NoEditableField => {
            // The transcript was held (not pasted) because no editable field was
            // focused. Say so plainly and point at the one real recovery path:
            // the menu-bar "Copy last dictation" item (there is no copy button
            // on the pill, so we label the menu recovery accurately, not "Copy").
            globals.pill.status.setStringValue(&NSString::from_str(
                "No text field selected · Copy from menu",
            ));
            set_pill_layout(globals, PillLayout::Copied);
            show_pill(globals);
        }
    }
}

/// The menu-bar glyph's alpha while the pipeline works.
const PROCESSING_ALPHA: f64 = 0.4;

fn show_pill(globals: &UiGlobals) {
    if !globals.pill_window.isVisible() {
        position_pill_at_cursor_screen(&globals.pill_window);
    }
    globals.pill_window.orderFrontRegardless();
}

/// Read the pointer directly instead of installing a global event monitor.
/// This keeps the UI self-contained and avoids stealing events from the app
/// where the user is dictating. The panel is mouse-aware only while visible.
pub(super) fn pointer_over_pill(window: &NSWindow) -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let p = NSEvent::mouseLocation();
    let f = window.frame();
    let _ = mtm; // retain the main-thread proof for AppKit's mouse query
    p.x >= f.origin.x
        && p.x <= f.origin.x + f.size.width
        && p.y >= f.origin.y
        && p.y <= f.origin.y + f.size.height
}

/// Hover changes the leading dot to a cancel glyph, never the capsule size.
pub(super) fn update_pill_hover(globals: &UiGlobals) {
    let state = ui_channel::state();
    let hovered = state.is_cancellable() && pointer_over_pill(&globals.pill_window);
    if globals.pill_hovered.swap(hovered, Ordering::Relaxed) != hovered {
        set_cancel_hover(&globals.pill, state, hovered);
    }
}

pub(super) fn set_cancel_hover(pill: &PillBuild, state: UiState, hovered: bool) {
    let cancellable = state.is_cancellable();
    pill.cancel.setHidden(!cancellable);
    pill.cancel.setEnabled(cancellable);
    pill.cancel_glyph.setHidden(!cancellable || !hovered);
    pill.dot.setHidden(cancellable && hovered);
    let label = if state == UiState::Recording {
        "Cancel recording"
    } else {
        "Cancel dictation"
    };
    pill.cancel
        .setToolTip(Some(&NSString::from_str(&format!("{label} (Escape)"))));
    unsafe {
        let _: () = msg_send![&*pill.cancel, setAccessibilityLabel: &*NSString::from_str(label)];
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct PillFeedback {
    text: String,
    tooltip: Option<String>,
    warning: bool,
    layout: PillLayout,
}

fn active_feedback(
    state: UiState,
    details: bool,
    preview: &str,
    recovery_enabled: bool,
    notice: Option<&str>,
) -> PillFeedback {
    let warning = recovery_enabled && state.is_active() && notice.is_some();
    let text = if state == UiState::Recording {
        if warning {
            "Recovery warning".to_owned()
        } else {
            preview_excerpt(preview)
        }
    } else if warning {
        format!("{} · Recovery warning", pill_status(state, details))
    } else {
        pill_status(state, details).to_owned()
    };
    let tooltip = if warning {
        notice.map(str::to_owned)
    } else if state == UiState::Recording && !preview.is_empty() {
        Some(preview.to_owned())
    } else {
        None
    };
    PillFeedback {
        text,
        tooltip,
        warning,
        layout: if state == UiState::Recording {
            PillLayout::Recording
        } else {
            PillLayout::Processing
        },
    }
}

fn update_live_feedback(globals: &UiGlobals, state: UiState) {
    let preview = if state == UiState::Recording {
        ui_channel::live_preview()
    } else {
        String::new()
    };
    let notice = if globals.recovery_enabled {
        crate::history::recovery_storage_failure_notice()
    } else {
        None
    };
    let feedback = active_feedback(
        state,
        globals.show_processing_detail,
        &preview,
        globals.recovery_enabled,
        notice.as_deref(),
    );
    let Ok(mut previous) = globals.pill_feedback.lock() else {
        return;
    };
    if previous.as_ref() == Some(&feedback) {
        return;
    }
    *previous = Some(feedback.clone());
    drop(previous);
    globals.pill.storage_warning.set(feedback.warning);
    globals
        .pill
        .status
        .setStringValue(&NSString::from_str(&feedback.text));
    let tooltip = feedback.tooltip.as_deref().map(NSString::from_str);
    globals.pill.status.setToolTip(tooltip.as_deref());
    set_pill_layout(globals, feedback.layout);
    set_cancel_hover(
        &globals.pill,
        state,
        globals.pill_hovered.load(Ordering::Relaxed),
    );
}

pub(super) fn preview_excerpt(text: &str) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let tail: String = normalized
        .chars()
        .rev()
        .take(120)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if tail.len() < normalized.len() {
        format!("…{tail}")
    } else {
        tail
    }
}

pub(super) fn pill_status(state: UiState, details: bool) -> &'static str {
    match state {
        UiState::Recording => "Recording",
        UiState::Processing => "Working…",
        UiState::Transcribing if details => "Transcribing…",
        UiState::Cleaning if details => "Cleaning up…",
        UiState::Pasting if details => "Pasting…",
        UiState::Transcribing | UiState::Cleaning | UiState::Pasting => "Working…",
        UiState::Copied => "Copied to clipboard",
        UiState::Success => "Landed",
        UiState::Error => "Couldn’t transcribe",
        UiState::NoEditableField => "No text field selected",
        UiState::Idle => "Ready",
    }
}

pub(super) fn set_pill_layout(globals: &UiGlobals, layout: PillLayout) {
    let width = layout_pill(&globals.pill, layout);
    set_pill_width(globals, width);
}

fn set_pill_width(globals: &UiGlobals, width: f64) {
    let old = globals.pill_window.frame();
    if old.size.width == width && old.size.height == globals.pill.height {
        return;
    }
    let cx = old.origin.x + old.size.width / 2.0;
    let frame = NSRect::new(
        NSPoint::new((cx - width / 2.0).round(), old.origin.y),
        NSSize::new(width, globals.pill.height),
    );
    unsafe {
        let _: () =
            msg_send![&*globals.pill_window, setFrame: frame, display: true, animate: false];
    }
    globals.pill.glass.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(width, globals.pill.height),
    ));
    globals.pill.tint.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(width, globals.pill.height),
    ));
    globals.pill.tint.setNeedsDisplay(true);
}

pub(super) fn reduced_motion_enabled() -> bool {
    if std::env::var_os("DICTATE_REDUCED_MOTION").is_some() {
        return true;
    }
    let workspace = objc2_app_kit::NSWorkspace::sharedWorkspace();
    // This is AppKit's accessibility preference, not an assumed defaults key.
    unsafe { msg_send![&*workspace, accessibilityDisplayShouldReduceMotion] }
}

/// Pure mapping seam: newest samples are right-aligned and never synthesized.
pub(super) fn waveform_targets(
    levels: &[f32],
    sensitivity: WaveformSensitivity,
) -> [f64; BAR_COUNT] {
    crate::waveform::levels::<BAR_COUNT>(levels, sensitivity)
        .map(|level| BAR_MIN_H + level * (BAR_MAX_H - BAR_MIN_H))
}

fn eased_bar(current: f64, target: f64, reduced_motion: bool) -> f64 {
    let range = BAR_MAX_H - BAR_MIN_H;
    let normalized = crate::waveform::ease(
        (current - BAR_MIN_H) / range,
        (target - BAR_MIN_H) / range,
        reduced_motion,
    );
    BAR_MIN_H + normalized * range
}

pub(super) fn update_bars(globals: &UiGlobals) {
    let targets = waveform_targets(&ui_channel::recent_levels(), globals.waveform_sensitivity);
    let Ok(mut displayed) = globals.displayed_heights.lock() else {
        return;
    };
    let mut heights = [BAR_MIN_H; BAR_COUNT];
    for (index, target) in targets.iter().enumerate() {
        let height = eased_bar(displayed[index], *target, globals.reduced_motion);
        displayed[index] = height;
        heights[index] = height;
    }
    globals.pill.waveform.set_heights(heights);
}

pub(super) fn collapse_bars(globals: &UiGlobals) {
    if let Ok(mut g) = globals.displayed_heights.lock() {
        for v in g.iter_mut() {
            *v = BAR_MIN_H;
        }
    }
    globals.pill.waveform.set_heights([BAR_MIN_H; BAR_COUNT]);
}

pub(super) fn position_pill_at_cursor_screen(window: &NSWindow) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    {
        let mouse_loc = NSEvent::mouseLocation();
        let screens = NSScreen::screens(mtm);
        let mut chosen = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0));
        for s in screens.iter() {
            let f = s.frame();
            if mouse_loc.x >= f.origin.x
                && mouse_loc.x <= f.origin.x + f.size.width
                && mouse_loc.y >= f.origin.y
                && mouse_loc.y <= f.origin.y + f.size.height
            {
                chosen = f;
                break;
            }
        }
        if chosen.size.width == 0.0 {
            if let Some(main) = NSScreen::mainScreen(mtm) {
                chosen = main.frame();
            }
        }
        let win_frame = window.frame();
        let x = chosen.origin.x + (chosen.size.width - win_frame.size.width) / 2.0;
        // Sit close to the bottom edge so the pill stays out of the way.
        let y = chosen.origin.y + SPACE_XXL;
        window.setFrameOrigin(NSPoint::new(x, y));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_failure_is_visible_without_exposing_the_transcript_or_blocking_work() {
        let recording = active_feedback(
            UiState::Recording,
            true,
            "private spoken words",
            true,
            Some("Recovery storage is unavailable."),
        );
        assert_eq!(recording.text, "Recovery warning");
        assert!(recording.warning && recording.layout == PillLayout::Recording);
        assert_eq!(
            recording.tooltip.as_deref(),
            Some("Recovery storage is unavailable.")
        );
        assert!(!recording.text.contains("private"));
        let processing = active_feedback(
            UiState::Transcribing,
            true,
            "private spoken words",
            true,
            Some("Recovery storage is unavailable."),
        );
        assert_eq!(processing.text, "Transcribing… · Recovery warning");
        assert_eq!(processing.layout, PillLayout::Processing);
        let disabled = active_feedback(
            UiState::Recording,
            true,
            "Live text",
            false,
            Some("Old notice"),
        );
        assert_eq!(disabled.text, "Live text");
        assert!(!disabled.warning);
        assert!(!active_feedback(UiState::Idle, true, "", true, Some("Old notice")).warning);
    }

    #[test]
    fn compact_dimensions_and_preview_preserve_the_contract() {
        assert_eq!(pill_height(PillSize::Compact), 32.0);
        assert_eq!(pill_height(PillSize::Standard), 36.0);
        assert!(PILL_W <= 100.0);
        assert!(CANCEL_HIT >= 24.0);
        let text = "这是最新的中文。".repeat(30);
        let preview = preview_excerpt(&text);
        assert!(preview.starts_with('…') && preview.ends_with("这是最新的中文。"));
        assert!(preview.chars().count() <= 121);
        assert_eq!(preview_excerpt("Hello\n  world"), "Hello world");
    }

    #[test]
    fn visible_stage_labels_and_cancel_deadline_match_pipeline_states() {
        assert_eq!(pill_status(UiState::Transcribing, true), "Transcribing…");
        assert_eq!(pill_status(UiState::Cleaning, true), "Cleaning up…");
        assert_eq!(pill_status(UiState::Pasting, true), "Pasting…");
        assert_eq!(pill_status(UiState::Cleaning, false), "Working…");
        assert!(
            UiState::Recording.is_cancellable()
                && UiState::Transcribing.is_cancellable()
                && UiState::Cleaning.is_cancellable()
        );
        assert!(!UiState::Pasting.is_cancellable() && !UiState::Copied.is_cancellable());
        assert_eq!(pill_status(UiState::Copied, true), "Copied to clipboard");
    }
}
