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
        if state.is_processing() {
            update_progress_pulse(globals);
        }
        if state == UiState::Success {
            // A settle can finish while the success pill is up; keep its Undo
            // affordance in step with the clipboard transaction gate.
            globals
                .pill
                .undo
                .setEnabled(!crate::clipboard_paste::transaction_pending());
        }
        if state.is_active() {
            update_live_feedback(globals, state);
            update_pill_hover(globals);
        }
        // About once a second, on this main-thread timer: follow a keyboard
        // layout switch (Text Input Source calls are main-thread only) and
        // notice Secure Input, which silences the hotkeys.
        let now_time = CFAbsoluteTimeGetCurrent();
        let last_slow = f64::from_bits(globals.last_slow_check.load(Ordering::Relaxed));
        if now_time - last_slow >= SLOW_CHECK_SECONDS - POLL_ACTIVE_SECONDS / 2.0 {
            globals
                .last_slow_check
                .store(now_time.to_bits(), Ordering::Relaxed);
            // The grant can land at any moment in System Settings. Installing
            // the event tap here is what makes "turn it on and it works"
            // true, with no relaunch.
            let was_waiting = crate::permissions::accessibility_task_pending();
            crate::permissions::poll_pending_accessibility();
            if was_waiting && !crate::permissions::accessibility_task_pending() {
                apply_state_transition(globals, state);
            }
            crate::permissions::refresh_microphone_health();
            super::setup::refresh();
            // The status item is only repainted on a UiState transition, and
            // most health changes do not move UiState: "Starting up…" clears
            // when the worker is ready, a microphone grant lands in System
            // Settings, the helper dies between utterances. Without this the
            // icon and its tooltip kept whatever the last transition painted
            // — an app that had finished booting still wore the danger glyph
            // and said "Starting up…" until the first dictation.
            let health = health_signature(crate::health::current().as_ref());
            if globals.last_health.swap(health, Ordering::SeqCst) != health {
                apply_state_transition(globals, state);
            }
            crate::key_layout::refresh();
            if let Some(on) = crate::secure_input::refresh() {
                eprintln!(
                    "[hotkey] Secure Input {} · hotkeys {}",
                    if on { "ON" } else { "off" },
                    if on { "paused (control socket still works)" } else { "back" }
                );
                apply_state_transition(globals, state);
            }
        }
        schedule_next_poll(state);
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
            CFAbsoluteTimeGetCurrent() + POLL_ACTIVE_SECONDS,
            POLL_ACTIVE_SECONDS,
            0,
            0,
            Some(timer_cb),
            &mut ctx,
        )
        .expect("CFRunLoopTimer::new");
        let run_loop = CFRunLoop::main().expect("main run loop");
        run_loop.add_timer(Some(&timer), kCFRunLoopCommonModes);
        // Kept for the life of the process (the run loop also retains it), so
        // `wake_poll_timer` may use the pointer from any thread.
        POLL_TIMER.store(
            CFRetained::into_raw(timer).as_ptr() as usize,
            Ordering::Release,
        );
    }
    ui_channel::set_wake_hook(wake_poll_timer);
}

/// The pill poll timer, stored once by `install_poll_timer` and never freed.
static POLL_TIMER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Set by `wake_poll_timer`, cleared by `schedule_next_poll`: a state change
/// arrived while the timer callback was deciding its next fire date.
static POLL_WAKE_PENDING: AtomicBool = AtomicBool::new(false);

/// Seconds between polls while an utterance or a result pill is on screen
/// (30 FPS: waveform, hover, live preview).
const POLL_ACTIVE_SECONDS: f64 = 1.0 / 30.0;

/// Seconds between polls while idle. Nothing animates; the poll only runs
/// the once-a-second checks. A state change wakes the timer at once through
/// `wake_poll_timer`, so the pill still appears at the press.
const POLL_IDLE_SECONDS: f64 = SLOW_CHECK_SECONDS;

/// Seconds between the slow checks (permissions, health, key layout,
/// Secure Input).
const SLOW_CHECK_SECONDS: f64 = 1.0;

/// How long until the next poll for a given state. Pure, for tests.
pub(super) fn poll_interval(state: UiState) -> f64 {
    if state == UiState::Idle {
        POLL_IDLE_SECONDS
    } else {
        POLL_ACTIVE_SECONDS
    }
}

/// Called at the end of each poll on the main thread. A repeating CF timer
/// honours a later fire date set during its own callout, so idle polls drop
/// to once a second. The pending check runs after the new date is set: a wake
/// from another thread either lands after it (and wins) or set the flag
/// before it (and is replayed here), so a press is never delayed.
fn schedule_next_poll(state: UiState) {
    let Some(timer) = poll_timer() else { return };
    let now = CFAbsoluteTimeGetCurrent();
    timer.set_next_fire_date(now + poll_interval(state));
    if POLL_WAKE_PENDING.swap(false, Ordering::SeqCst) {
        timer.set_next_fire_date(now);
    }
}

/// Fire the poll timer now. Safe from any thread: `ui_channel` calls it on
/// every state change, from the worker, the event tap and the audio path.
fn wake_poll_timer() {
    let Some(timer) = poll_timer() else { return };
    POLL_WAKE_PENDING.store(true, Ordering::SeqCst);
    timer.set_next_fire_date(CFAbsoluteTimeGetCurrent());
    if let Some(main) = CFRunLoop::main() {
        main.wake_up();
    }
}

fn poll_timer() -> Option<&'static CFRunLoopTimer> {
    let raw = POLL_TIMER.load(Ordering::Acquire);
    // SAFETY: the pointer came from `CFRetained::into_raw` and is never
    // released, so it stays valid for the process. CFRunLoop timer calls are
    // thread-safe in Core Foundation.
    (raw != 0).then(|| unsafe { &*(raw as *const CFRunLoopTimer) })
}

/// One byte standing for what the status item currently says about health:
/// `0` for "nothing wrong", otherwise the fault's discriminant plus one.
///
/// Only a fault that blocks dictation reaches the status item, so only those
/// move the signature — a lost microphone stream or a missing cleanup model
/// must not make the menu bar repaint once a second.
pub(super) fn health_signature(problem: Option<&crate::health::Problem>) -> u8 {
    match problem {
        Some(p) if p.fault.blocks_dictation() => p.fault as u8 + 1,
        _ => 0,
    }
}

pub(super) fn apply_state_transition(globals: &UiGlobals, state: UiState) {
    let mtm = match MainThreadMarker::new() {
        Some(m) => m,
        None => return,
    };

    // A fault outranks the utterance state: an idle-looking icon over a dead
    // hotkey is the one thing the menu bar must never show. The rule allows
    // `danger` on the status item for exactly this and for recording.
    let fault = crate::health::current().filter(|p| p.fault.blocks_dictation());

    // One glyph family — the waveform, matching the app icon. Idle is the
    // template ink; recording is the same shape tinted `danger`; processing is
    // the template at 40 %, which is the rule's "busy" treatment.
    let icon = if state == UiState::Recording || fault.is_some() {
        &globals.icon_recording
    } else if state.is_processing() || (state == UiState::Error || state.is_kept_result()) {
        &globals.icon_processing
    } else {
        &globals.icon_idle
    };
    if let Some(button) = globals.status_item.button(mtm) {
        if let Some(img) = icon {
            button.setImage(Some(img));
        } else {
            button.setTitle(&NSString::from_str(if fault.is_some() {
                "!"
            } else if state == UiState::Recording {
                "●"
            } else if state.is_processing() {
                "◌"
            } else if state == UiState::Error || state.is_kept_result() {
                "!"
            } else {
                "◯"
            }));
        }
        // Secure Input silences the hotkeys; while idle the glyph dims and the
        // tooltip says why (the menu carries the same line).
        let paused = state == UiState::Idle && crate::secure_input::active();
        button.setAlphaValue(if state.is_processing() || paused {
            PROCESSING_ALPHA
        } else {
            1.0
        });
        button.setToolTip(Some(&NSString::from_str(&format!(
            "Local Dictation · {}",
            match (&fault, paused) {
                // A word beside the colour: the rule never lets a state be
                // carried by the tint alone.
                (Some(problem), _) => problem.pill_line(),
                (None, true) => crate::secure_input::PAUSED_TEXT.to_string(),
                (None, false) =>
                    pill_status(state, globals.show_processing_detail).to_string(),
            }
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
            // Undo is only safe once no clipboard transaction is still
            // settling: while one is in flight the visible text may belong to
            // that write, so an early ⌘Z would revert unrelated prior content.
            globals
                .pill
                .undo
                .setEnabled(!crate::clipboard_paste::transaction_pending());
        }
        UiState::Error => {
            globals
                .pill
                .status
                .setStringValue(&NSString::from_str("Couldn’t transcribe"));
            // A real ASR failure: offer the one quick action that helps,
            // Retry. It is a click, never an automatic retry.
            globals
                .pill
                .retry_hint
                .setStringValue(&NSString::from_str("Retry"));
            globals
                .pill
                .hint
                .setStringValue(&NSString::from_str("History"));
            set_hint_action(globals, sel!(openHistory:), "Open Dictation History");
            set_pill_layout(globals, PillLayout::Retryable);
            show_pill(globals);
        }
        UiState::RewriteFailed => {
            // The transform failed; the selection was left untouched. Say that
            // plainly instead of the generic transcription error. No Retry
            // here: re-running ASR would not fix a rewrite failure.
            globals.pill.status.setStringValue(&NSString::from_str(
                "Couldn’t rewrite · Selection unchanged",
            ));
            globals
                .pill
                .hint
                .setStringValue(&NSString::from_str("History"));
            set_hint_action(globals, sel!(openHistory:), "Open Dictation History");
            set_pill_layout(globals, PillLayout::Error);
            show_pill(globals);
        }
        UiState::NothingToCopy => {
            globals
                .pill
                .status
                .setStringValue(&NSString::from_str(NOTHING_TO_COPY_TEXT));
            set_pill_layout(globals, PillLayout::Copied);
            show_pill(globals);
        }
        UiState::Unavailable => {
            // The reason lives in the health channel, not in the state byte.
            let line = crate::health::current()
                .map(|p| p.pill_line())
                .unwrap_or_else(|| "Dictation is not ready".to_string());
            globals.pill.status.setStringValue(&NSString::from_str(&line));
            globals
                .pill
                .hint
                .setStringValue(&NSString::from_str("Setup"));
            // The same hint slot, pointed at the window that fixes it rather
            // than at History, which holds nothing relevant here.
            set_hint_action(globals, sel!(openSetup:), "Open Setup & permissions");
            set_pill_layout(globals, PillLayout::Error);
            show_pill(globals);
        }
        UiState::NoEditableField
        | UiState::PasteFailed
        | UiState::HeldCopied
        | UiState::PasteFailedCopied
        | UiState::LimitReached
        | UiState::LimitReachedCopied => {
            // The words exist but did not reach the app. One sentence: what
            // happened, then where the text is ("Copied, press ⌘V" when it is
            // on the clipboard). The Error layout carries the History button,
            // so the pill itself is a route to the transcript. It stays about
            // four seconds, or until the next key press.
            globals.pill.status.setStringValue(&NSString::from_str(
                crate::last_dictation::kept_pill_text(state).unwrap_or("Text kept · Copy from menu"),
            ));
            globals
                .pill
                .hint
                .setStringValue(&NSString::from_str("History"));
            set_hint_action(globals, sel!(openHistory:), "Open Dictation History");
            set_pill_layout(globals, PillLayout::Error);
            show_pill(globals);
        }
    }
}

/// Point the error pill's hint button at one action. The slot is shared by the
/// kept-result pills (History) and the not-ready pill (Setup), so it is always
/// set explicitly rather than left on whatever the previous state wanted.
fn set_hint_action(globals: &UiGlobals, action: objc2::runtime::Sel, tooltip: &str) {
    unsafe {
        let _: () = msg_send![&*globals.pill.history, setAction: action];
    }
    globals
        .pill
        .history
        .setToolTip(Some(&NSString::from_str(tooltip)));
}

/// Pill text when "Copy last dictation" has nothing to copy.
pub(super) const NOTHING_TO_COPY_TEXT: &str = "No dictation to copy yet";

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
        UiState::RewriteFailed => "Couldn’t rewrite",
        UiState::PasteFailed | UiState::PasteFailedCopied => "Couldn’t paste",
        UiState::NoEditableField | UiState::HeldCopied => "No text field selected",
        UiState::LimitReached | UiState::LimitReachedCopied => "Recording limit reached",
        UiState::NothingToCopy => NOTHING_TO_COPY_TEXT,
        UiState::Unavailable => "Not ready",
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

/// How many dots the indeterminate working indicator draws.
pub(super) const PROGRESS_DOTS: usize = 3;
/// Resting opacity of an unlit working dot (lit reaches 1.0).
const PROGRESS_REST_ALPHA: f64 = 0.35;

/// Opacity of each working dot for a pulse `phase` in `[0, 1)`. The pulse
/// marches across the dots so the indicator reads as indeterminate progress,
/// never as a percentage or as microphone levels. Under Reduce Motion there is
/// no movement: every dot sits at a steady mid opacity (a fade-only treatment
/// the design rule allows). Pure, so it is unit-tested.
pub(super) fn stage_pulse_alphas(phase: f64, reduced_motion: bool) -> [f64; PROGRESS_DOTS] {
    if reduced_motion {
        return [0.6; PROGRESS_DOTS];
    }
    let phase = phase.rem_euclid(1.0);
    std::array::from_fn(|i| {
        let center = i as f64 / PROGRESS_DOTS as f64;
        let d = (phase - center).abs();
        let d = d.min(1.0 - d);
        let bump = (1.0 - 2.0 * d).max(0.0);
        PROGRESS_REST_ALPHA + (1.0 - PROGRESS_REST_ALPHA) * bump
    })
}

/// Advance the working dots on the existing 30 FPS poll. Only called while a
/// processing state is on screen, so it adds no timer work when idle.
fn update_progress_pulse(globals: &UiGlobals) {
    let phase = CFAbsoluteTimeGetCurrent().rem_euclid(1.0);
    let alphas = stage_pulse_alphas(phase, globals.reduced_motion);
    for (dot, alpha) in globals.pill.progress_dots.iter().zip(alphas) {
        dot.setAlphaValue(alpha);
    }
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
mod health_signature_tests {
    use super::health_signature;
    use crate::health::{Fault, Problem};

    fn problem(fault: Fault) -> Problem {
        Problem {
            fault,
            detail: String::new(),
        }
    }

    /// Regression: the status item was repainted only on a `UiState`
    /// transition, and most health changes do not move `UiState`. The
    /// signature is what makes the once-a-second tick notice one.
    #[test]
    fn every_blocking_fault_has_its_own_signature_and_nothing_else_moves_it() {
        assert_eq!(health_signature(None), 0);

        let mut seen = vec![0u8];
        for fault in [
            Fault::AccessibilityMissing,
            Fault::AccessibilityRelaunch,
            Fault::MicrophoneBlocked,
            Fault::ModelsMissing,
            Fault::EngineFailed,
            Fault::HelperFailed,
            Fault::Booting,
        ] {
            let signature = health_signature(Some(&problem(fault)));
            assert_ne!(signature, 0, "{fault:?} blocks dictation and must show");
            assert!(!seen.contains(&signature), "{fault:?} collides");
            seen.push(signature);
        }

        // The two non-blocking faults never reach the status item, so they
        // must read the same as "nothing wrong" — otherwise an unplugged mic
        // would repaint the menu bar with a danger glyph it does not deserve.
        for fault in [Fault::AudioLost, Fault::CleanupUnavailable] {
            assert_eq!(health_signature(Some(&problem(fault))), 0, "{fault:?}");
        }

        // The whole point: boot → ready is a change the tick can see.
        assert_ne!(
            health_signature(Some(&problem(Fault::Booting))),
            health_signature(None)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Idle polls once a second; anything on screen keeps 30 FPS. The slow
    /// checks still run about once a second in both.
    #[test]
    fn only_idle_slows_the_poll() {
        assert_eq!(poll_interval(UiState::Idle), SLOW_CHECK_SECONDS);
        for v in 1..=17u8 {
            let state = UiState::from_u8(v);
            assert_eq!(poll_interval(state), POLL_ACTIVE_SECONDS, "{state:?}");
        }
        assert!(POLL_IDLE_SECONDS >= SLOW_CHECK_SECONDS - POLL_ACTIVE_SECONDS / 2.0);
    }

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
        assert_eq!(pill_status(UiState::RewriteFailed, true), "Couldn’t rewrite");
        assert!(
            UiState::Recording.is_cancellable()
                && UiState::Transcribing.is_cancellable()
                && UiState::Cleaning.is_cancellable()
        );
        assert!(!UiState::Pasting.is_cancellable() && !UiState::Copied.is_cancellable());
        assert_eq!(pill_status(UiState::Copied, true), "Copied to clipboard");
    }

    /// The working indicator is a marching pulse that never leaves the
    /// allowable opacity band, peaks at each dot's phase centre, and goes still
    /// under Reduce Motion. The three dots fit the leading control slot.
    #[test]
    fn working_dots_pulse_indeterminately_and_respect_reduce_motion() {
        let cluster =
            PROGRESS_DOTS as f64 * slate::DOT + (PROGRESS_DOTS as f64 - 1.0) * SPACE_XXS;
        assert!(
            cluster <= CANCEL_HIT,
            "the dots must fit the leading control slot"
        );

        // Reduce Motion: a steady fade-only treatment, no march.
        assert_eq!(stage_pulse_alphas(0.42, true), [0.6; PROGRESS_DOTS]);

        // Each dot reaches full opacity at its own phase centre.
        for i in 0..PROGRESS_DOTS {
            let center = i as f64 / PROGRESS_DOTS as f64;
            assert_eq!(stage_pulse_alphas(center, false)[i], 1.0);
        }

        // At every sample the dots stay in band and are not all the same
        // opacity, which is what makes the motion read as indeterminate.
        for step in 0..30 {
            let alphas = stage_pulse_alphas(step as f64 / 30.0, false);
            for a in alphas {
                assert!(
                    (PROGRESS_REST_ALPHA..=1.0).contains(&a),
                    "opacity {a} out of band"
                );
            }
            let max = alphas.iter().cloned().fold(f64::MIN, f64::max);
            let min = alphas.iter().cloned().fold(f64::MAX, f64::min);
            assert!(max - min > 0.1, "the dots should march, not sit flat");
        }
    }
}
