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
    /// The indeterminate working indicator: three dots that march while the
    /// pipeline runs. Shown only in `PillLayout::Processing`; it reuses the
    /// existing 30 FPS poll and never fakes audio levels (the live waveform is
    /// separate and only visible while recording).
    pub(super) progress: Retained<slate::SlateView>,
    pub(super) progress_dots: Vec<Retained<slate::SlateView>>,
    /// "Retry" quick action on a real ASR failure only (never on a transform
    /// failure or on kept text), with its native hit target. It is a click, not
    /// an automatic retry.
    pub(super) retry_hint: Retained<NSTextField>,
    pub(super) retry: Retained<NSButton>,
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
    /// A real ASR failure: the same error surface, plus the one quick action
    /// that makes sense for lost speech, Retry (re-run the retained audio).
    /// Transform failures use `Error` instead and never offer Retry.
    Retryable,
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

    // ── Indeterminate working dots ─────────────────────────────────────
    // Three dots in the leading control slot, shown while the pipeline works
    // instead of the live waveform. `layout_pill` animates their alpha on the
    // existing 30 FPS poll; nothing here reads audio levels, so the indicator
    // never fakes microphone activity.
    let cluster_w = PROGRESS_DOTS as f64 * slate::DOT + (PROGRESS_DOTS as f64 - 1.0) * SPACE_XXS;
    let progress = slate::SlateView::new(
        mtm,
        NSRect::new(NSPoint::new(PILL_PAD, 0.0), NSSize::new(CANCEL_HIT, height)),
        slate::Paint::None,
        0.0,
    );
    let mut progress_dots = Vec::new();
    for i in 0..PROGRESS_DOTS {
        let x = (CANCEL_HIT - cluster_w) / 2.0 + i as f64 * (slate::DOT + SPACE_XXS);
        let dot = slate::status_dot(mtm, true, x, height / 2.0);
        dot.set_paint(slate::Paint::DotNeutral);
        dot.setAlphaValue(PROGRESS_REST_ALPHA);
        progress.addSubview(&dot);
        progress_dots.push(dot);
    }
    progress.setHidden(true);
    glass.addSubview(&progress);

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

    // The ASR-failure pill's quick Retry label (hidden unless shown).
    let retry_hint = slate::label(mtm, "", TYPE_META, WEIGHT_REGULAR, theme::text_secondary());
    retry_hint.setHidden(true);
    glass.addSubview(&retry_hint);

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

    // The Retry quick action on a real ASR failure. Same guarded `retryLast:`
    // path as the menu item and IPC — it replays the retained audio through
    // the normal pipeline; it is never fired automatically.
    let retry: Retained<NSButton> = unsafe {
        msg_send![
            NSButton::alloc(mtm),
            initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0))
        ]
    };
    retry.setTitle(&NSString::from_str(""));
    retry.setBordered(false);
    retry.setToolTip(Some(&NSString::from_str("Retry the last recording (re-transcribe)")));
    unsafe {
        let _: () = msg_send![&*retry, setTransparent: true];
        let _: () = msg_send![&*retry, setRefusesFirstResponder: true];
        let _: () = msg_send![&*retry, setTarget: &**actions];
        let _: () = msg_send![&*retry, setAction: sel!(retryLast:)];
        let _: () =
            msg_send![&*retry, setAccessibilityLabel: &*NSString::from_str("Retry last recording")];
    }
    retry.setHidden(true);
    glass.addSubview(&retry);

    let history = slate::click_overlay(
        mtm,
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)),
        actions,
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
        progress,
        progress_dots,
        retry_hint,
        retry,
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
    pill.progress.setHidden(true);
    pill.retry_hint.setHidden(true);
    pill.retry.setHidden(true);
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
            // The indeterminate working dots stand in for both the live
            // waveform (hidden above) and the single status dot, so nothing
            // here pretends to be microphone activity.
            pill.dot.setHidden(true);
            pill.progress.setHidden(false);
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
        PillLayout::Retryable => {
            pill.well.setHidden(true);
            pill.dot.setHidden(false);
            pill.status.setHidden(false);
            pill.retry_hint.setHidden(false);
            pill.hint.setHidden(false);
            pill.dot.set_paint(slate::Paint::DotDanger);
            pill.dot.setFrameOrigin(NSPoint::new(
                PILL_TEXT_PAD,
                (mid - slate::DOT / 2.0).round(),
            ));
            let mut x = PILL_TEXT_PAD + slate::DOT + PILL_GAP;
            x += slate::place_left(&pill.status, x, mid) + PILL_GAP;
            // Retry is the first quick action; History follows.
            let retry_text = slate::place_left(&pill.retry_hint, x + SPACE_XXS, mid);
            let retry_hit = (retry_text + 2.0 * SPACE_XXS).max(CANCEL_HIT);
            pill.retry.setFrame(NSRect::new(
                NSPoint::new(x, 0.0),
                NSSize::new(retry_hit, pill.height),
            ));
            pill.retry.setHidden(false);
            x += retry_hit + PILL_GAP;
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
    // The failure pills are the surfaces that take a `danger` hairline, so a
    // failure reads at a glance without recolouring anything else.
    pill.tint
        .set_danger_stroke(matches!(layout, PillLayout::Error | PillLayout::Retryable));
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

/// Pill text when "Copy last dictation" has nothing to copy.
pub(super) const NOTHING_TO_COPY_TEXT: &str = "No dictation to copy yet";

/// The menu-bar glyph's alpha while the pipeline works.
pub(super) const PROCESSING_ALPHA: f64 = 0.4;

pub(super) fn show_pill(globals: &UiGlobals) {
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
    if state.is_processing() {
        // The working dots replace the single status dot; the hover cancel
        // glyph takes the slot while the pointer is over the pill.
        pill.dot.setHidden(true);
        pill.progress.setHidden(hovered);
    } else {
        pill.dot.setHidden(cancellable && hovered);
    }
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

#[cfg(test)]
mod health_signature_tests;

#[cfg(test)]
mod tests;
