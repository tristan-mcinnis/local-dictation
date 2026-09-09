//! The floating dictation pill — Slate.
//!
//! The pill is the same glass as a floating panel: an `NSVisualEffectView`
//! tinted with `panel_tint`, a `stroke` hairline, a 1 px `highlight_top` inside
//! the top edge, and the window's own layered shadow. It is
//! `CONTROL_PILL` (36) high at `RADIUS_PILL` (18).
//!
//! Five layouts share one view tree, so a state change is a re-layout rather
//! than a rebuild:
//!
//! | state      | content                                                     |
//! | ---------- | ----------------------------------------------------------- |
//! | recording  | ink waveform in a sunken `well`                              |
//! | processing | the same bars at 40 % ink                                    |
//! | hover      | well + "Release to finish `esc` · Latch `space`"             |
//! | success    | `success` dot + "Landed · N words" + "Undo" `⌘` `Z`          |
//! | error      | `danger` dot + "Couldn't transcribe" + "Retry" `↩`           |
//!
//! Success and error no longer recolour the bars: the pill collapses to a
//! status line, which is the one place the rule allows chroma (a status dot,
//! always paired with a word).

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
    let cfg = status_glyph_configuration()
        .configurationByApplyingConfiguration(&NSImageSymbolConfiguration::configurationWithHierarchicalColor(color));
    let tinted = base.imageWithSymbolConfiguration(&cfg)?;
    tinted.setTemplate(false);
    Some(tinted)
}

/// Every view the pill can show. All are built once; `layout_pill` positions
/// the ones this state needs and hides the rest.
pub(super) struct PillBuild {
    pub(super) window: Retained<NSWindow>,
    /// The blur material; also the pill's content container.
    pub(super) glass: Retained<NSView>,
    /// The tint / stroke / highlight painted over the material.
    pub(super) tint: Retained<slate::SlateView>,
    /// The sunken track the bars sit in.
    pub(super) well: Retained<slate::SlateView>,
    pub(super) waveform: Retained<slate::WaveformView>,
    /// Status dot for the success / error lines.
    pub(super) dot: Retained<slate::SlateView>,
    /// "Release to finish" / "Landed · 38 words" / "Couldn't transcribe".
    pub(super) status: Retained<NSTextField>,
    /// "Latch" / "Undo" / "Retry" — the secondary word before a key cap.
    pub(super) hint: Retained<NSTextField>,
    /// Up to three key caps: `esc`, `space`; `⌘`, `Z`; `↩`.
    pub(super) caps: Vec<Retained<slate::SlateView>>,
    /// Invisible hit targets over the shelf's caps so Escape-to-cancel and
    /// Undo stay clickable, exactly as the old × / ⚙ buttons were.
    pub(super) cancel: Retained<NSButton>,
    pub(super) undo: Retained<NSButton>,
}
unsafe impl Send for PillBuild {}
unsafe impl Sync for PillBuild {}

/// Which layout the pill is showing. Derived from `UiState` plus hover.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum PillLayout {
    Compact,
    Shelf,
    Success,
    Error,
}

/// Build the floating pill window.
pub(super) fn build_pill_window(
    mtm: MainThreadMarker,
    actions: &Retained<MenuActions>,
) -> eyre::Result<PillBuild> {
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(PILL_W, PILL_H));
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    {
        window.setLevel(NSWindowLevel::from(3_isize)); // floating panel
        window.setOpaque(false);
        let clear = NSColor::clearColor();
        window.setBackgroundColor(Some(&clear));
        // The pill is intentionally mouse-aware: the timer samples the
        // pointer against the window frame and expands the shelf on hover.
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
    let (glass, tint) = slate::glass(mtm, frame, RADIUS_PILL);

    // ── The waveform, in its sunken well ───────────────────────────────
    let well = slate::SlateView::new(
        mtm,
        NSRect::new(
            NSPoint::new(PILL_PAD, ((PILL_H - WELL_H) / 2.0).round()),
            NSSize::new(2.0 * WELL_PAD + BAR_BLOCK_W, WELL_H),
        ),
        slate::Paint::Well,
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
    let dot = slate::status_dot(mtm, true, PILL_TEXT_PAD, PILL_H / 2.0);
    dot.setHidden(true);
    glass.addSubview(&dot);

    let status = slate::label(
        mtm,
        "Release to finish",
        TYPE_META,
        WEIGHT_REGULAR,
        theme::text_secondary(),
    );
    unsafe {
        let _: () =
            msg_send![&*status, setAccessibilityLabel: &*NSString::from_str("Dictation status")];
    }
    status.setHidden(true);
    glass.addSubview(&status);

    let hint = slate::label(
        mtm,
        "Latch",
        TYPE_META,
        WEIGHT_REGULAR,
        theme::text_tertiary(),
    );
    hint.setHidden(true);
    glass.addSubview(&hint);

    // Three caps cover every layout: two on the shelf, two for ⌘Z, one for ↩.
    let mut caps = Vec::new();
    for _ in 0..3 {
        let cap = slate::key_cap(mtm, "esc", 0.0, PILL_H / 2.0);
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
        let _: () = msg_send![&*cancel, setTarget: &**actions];
        let _: () = msg_send![&*cancel, setAction: sel!(cancelPill:)];
        let _: () = msg_send![&*cancel, setKeyEquivalent: &*NSString::from_str("\\u{1b}")];
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
        let _: () = msg_send![&*undo, setAccessibilityLabel: &*NSString::from_str("Undo dictation")];
    }
    undo.setHidden(true);
    glass.addSubview(&undo);

    window.setContentView(Some(&*glass));

    Ok(PillBuild {
        window,
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
    })
}

// ─── Layout ──────────────────────────────────────────────────────────────

/// Lay the pill out for `layout` and return the width the window needs.
/// Everything is measured, so a longer word never clips.
pub(super) fn layout_pill(pill: &PillBuild, layout: PillLayout) -> f64 {
    let mid = PILL_H / 2.0;
    let well_w = 2.0 * WELL_PAD + BAR_BLOCK_W;
    for cap in &pill.caps {
        cap.setHidden(true);
    }
    pill.cancel.setHidden(true);
    pill.undo.setHidden(true);

    let width = match layout {
        PillLayout::Compact => {
            pill.well.setHidden(false);
            pill.dot.setHidden(true);
            pill.status.setHidden(true);
            pill.hint.setHidden(true);
            pill.well.setFrameOrigin(NSPoint::new(PILL_PAD, ((PILL_H - WELL_H) / 2.0).round()));
            2.0 * PILL_PAD + well_w
        }
        PillLayout::Shelf => {
            pill.well.setHidden(false);
            pill.dot.setHidden(true);
            pill.status.setHidden(false);
            pill.hint.setHidden(false);
            pill.well
                .setFrameOrigin(NSPoint::new(PILL_PAD, ((PILL_H - WELL_H) / 2.0).round()));
            let mut x = PILL_PAD + well_w + PILL_GAP;
            x += slate::place_left(&pill.status, x, mid) + PILL_GAP;
            x += place_cap(&pill.caps[0], "esc", x, mid) + PILL_GAP;
            // The visible cap is the affordance; this invisible button is what
            // AppKit clicks and what carries the Escape key equivalent.
            pill.cancel.setFrame(pill.caps[0].frame());
            pill.cancel.setHidden(false);
            x += slate::place_left(&pill.hint, x, mid) + PILL_GAP;
            x += place_cap(&pill.caps[1], "space", x, mid);
            x + PILL_TEXT_PAD
        }
        PillLayout::Success => {
            pill.well.setHidden(true);
            pill.dot.setHidden(false);
            pill.status.setHidden(false);
            pill.hint.setHidden(false);
            pill.dot.set_paint(slate::Paint::DotSuccess);
            pill.dot
                .setFrameOrigin(NSPoint::new(PILL_TEXT_PAD, (mid - slate::DOT / 2.0).round()));
            let mut x = PILL_TEXT_PAD + slate::DOT + PILL_GAP;
            x += slate::place_left(&pill.status, x, mid) + PILL_GAP;
            x += slate::place_left(&pill.hint, x, mid) + PILL_GAP;
            let c0 = place_cap(&pill.caps[0], "⌘", x, mid);
            x += c0 + SPACE_XXS;
            x += place_cap(&pill.caps[1], "Z", x, mid);
            pill.undo.setFrame(NSRect::new(
                NSPoint::new(x - c0 - SPACE_XXS - pill.hint.frame().size.width - PILL_GAP, 0.0),
                NSSize::new(
                    c0 + SPACE_XXS + pill.caps[1].frame().size.width
                        + pill.hint.frame().size.width
                        + PILL_GAP,
                    PILL_H,
                ),
            ));
            pill.undo.setHidden(false);
            x + PILL_GAP + 2.0
        }
        PillLayout::Error => {
            pill.well.setHidden(true);
            pill.dot.setHidden(false);
            pill.status.setHidden(false);
            pill.hint.setHidden(false);
            pill.dot.set_paint(slate::Paint::DotDanger);
            pill.dot
                .setFrameOrigin(NSPoint::new(PILL_TEXT_PAD, (mid - slate::DOT / 2.0).round()));
            let mut x = PILL_TEXT_PAD + slate::DOT + PILL_GAP;
            x += slate::place_left(&pill.status, x, mid) + PILL_GAP;
            x += slate::place_left(&pill.hint, x, mid) + PILL_GAP;
            x += place_cap(&pill.caps[2], "↩", x, mid);
            x + PILL_TEXT_PAD
        }
    };
    // The error pill is the one surface that takes a `danger` hairline, so the
    // failure reads at a glance without recolouring anything else.
    pill.tint.set_danger_stroke(layout == PillLayout::Error);
    width.round()
}

/// Retitle a key cap in place and position it. Returns its width.
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
            NSPoint::new(((w - tw) / 2.0).round(), ((CONTROL_KEYCAP - th) / 2.0).round()),
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
        // Drive the waveform while the pill is visible. Recording feeds the
        // bars from live mic RMS; Processing has no mic input, so we run a
        // synthetic "thinking" wave instead of letting the bars sit frozen.
        match state {
            UiState::Recording => {
                update_bars(globals);
            }
            UiState::Processing => {
                if globals.reduced_motion {
                    set_static_processing_bars(globals);
                } else {
                    animate_processing_bars(globals);
                }
            }
            UiState::Success | UiState::Error | UiState::NoEditableField | UiState::Idle => {}
        }
        if state != UiState::Idle {
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
    let icon = match state {
        UiState::Idle | UiState::Success => &globals.icon_idle,
        UiState::Recording => &globals.icon_recording,
        UiState::Processing | UiState::Error | UiState::NoEditableField => {
            &globals.icon_processing
        }
    };
    if let Some(button) = globals.status_item.button(mtm) {
        if let Some(img) = icon {
            button.setImage(Some(img));
        } else {
            // Fallback if SF Symbols weren't available.
            let fallback = match state {
                UiState::Idle => "◯",
                UiState::Recording => "●",
                UiState::Processing => "◌",
                UiState::Success => "✓",
                UiState::Error | UiState::NoEditableField => "!",
            };
            button.setTitle(&NSString::from_str(fallback));
        }
        button.setAlphaValue(if state == UiState::Processing {
            PROCESSING_ALPHA
        } else {
            1.0
        });
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
            if let Ok(mut p) = globals.pill_expanded.lock() {
                *p = 0.0;
            }
            set_pill_layout(globals, PillLayout::Compact);
            globals.pill_window.orderOut(None);
        }
        UiState::Recording => {
            globals.pill.waveform.set_muted(false);
            globals
                .pill
                .status
                .setStringValue(&NSString::from_str("Release to finish"));
            globals.pill.hint.setStringValue(&NSString::from_str("Latch"));
            set_pill_layout(globals, PillLayout::Compact);
            show_pill(globals);
        }
        UiState::Processing => {
            globals.pill.waveform.set_muted(true);
            globals
                .pill
                .status
                .setStringValue(&NSString::from_str("Working…"));
            set_pill_layout(globals, PillLayout::Compact);
            show_pill(globals);
        }
        UiState::Success => {
            let words = ui_channel::last_dictation().split_whitespace().count();
            let text = if words == 1 {
                "Landed · 1 word".to_string()
            } else {
                format!("Landed · {words} words")
            };
            globals.pill.status.setStringValue(&NSString::from_str(&text));
            globals.pill.hint.setStringValue(&NSString::from_str("Undo"));
            set_pill_layout(globals, PillLayout::Success);
            show_pill(globals);
        }
        UiState::Error => {
            globals
                .pill
                .status
                .setStringValue(&NSString::from_str("Couldn’t transcribe"));
            globals.pill.hint.setStringValue(&NSString::from_str("Retry"));
            set_pill_layout(globals, PillLayout::Error);
            show_pill(globals);
        }
        UiState::NoEditableField => {
            // The transcript was held (not pasted) because no editable field was
            // focused. Say so plainly and point at the one real recovery path:
            // the menu-bar "Copy last dictation" item (there is no copy button
            // on the pill, so we label the menu recovery accurately, not "Copy").
            globals
                .pill
                .status
                .setStringValue(&NSString::from_str("No text field selected"));
            globals.pill.hint.setStringValue(&NSString::from_str("Copy: menu"));
            set_pill_layout(globals, PillLayout::Error);
            show_pill(globals);
        }
    }
}

/// The menu-bar glyph's alpha while the pipeline works.
const PROCESSING_ALPHA: f64 = 0.4;

fn show_pill(globals: &UiGlobals) {
    position_pill_at_cursor_screen(&globals.pill_window);
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

/// Interruptible, non-bouncy hover expansion. The compact capsule remains
/// stable at the same screen centre while the shelf reveals itself to the
/// right. Reduced motion snaps to the target and uses static status cues.
pub(super) fn update_pill_hover(globals: &UiGlobals) {
    // Only the waveform states carry a hover shelf; the success and error
    // lines already say what they are.
    let state = ui_channel::state();
    if !matches!(state, UiState::Recording | UiState::Processing) {
        return;
    }
    // The hover shelf is retired (2026-09-09): the pill reacting to the mouse
    // was jarring, and Superwhisper / Wispr Flow never do it. The keys are
    // documented in Settings and the README instead. `pointer_over_pill`
    // stays for the click targets; the shelf target is pinned closed.
    let _ = pointer_over_pill(&globals.pill_window);
    let target = 0.0;
    let Ok(mut progress) = globals.pill_expanded.lock() else {
        return;
    };
    let next = if globals.reduced_motion {
        target
    } else {
        *progress + (target - *progress) * HOVER_EASE
    };
    let next = if (next - target).abs() < 0.01 {
        target
    } else {
        next
    };
    if (next - *progress).abs() < 0.004 {
        return;
    }
    let crossed = (*progress > 0.5) != (next > 0.5);
    *progress = next;
    drop(progress);
    if crossed || next == 0.0 || next == 1.0 {
        set_pill_layout(
            globals,
            if next > 0.5 {
                PillLayout::Shelf
            } else {
                PillLayout::Compact
            },
        );
    } else {
        resize_pill(globals, next);
    }
}

/// How fast the shelf eases open per 33 ms tick. `MOTION_HOVER` is 0.10 s, so
/// roughly a quarter of the remaining distance each frame.
const HOVER_EASE: f64 = 0.033 / MOTION_HOVER * 0.8;

/// Lay the pill out for `layout` and size the window to the result.
pub(super) fn set_pill_layout(globals: &UiGlobals, layout: PillLayout) {
    let width = layout_pill(&globals.pill, layout);
    set_pill_width(globals, width);
}

/// Interpolate the window width while the shelf eases open.
pub(super) fn resize_pill(globals: &UiGlobals, progress: f64) {
    let compact = 2.0 * PILL_PAD + 2.0 * WELL_PAD + BAR_BLOCK_W;
    let expanded = globals.pill.glass.frame().size.width.max(compact);
    let width = compact + (expanded - compact) * progress.clamp(0.0, 1.0);
    set_pill_width(globals, width);
}

fn set_pill_width(globals: &UiGlobals, width: f64) {
    let old = globals.pill_window.frame();
    let cx = old.origin.x + old.size.width / 2.0;
    let frame = NSRect::new(
        NSPoint::new((cx - width / 2.0).round(), old.origin.y),
        NSSize::new(width, PILL_H),
    );
    unsafe {
        let _: () =
            msg_send![&*globals.pill_window, setFrame: frame, display: true, animate: false];
    }
    globals.pill.glass.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(width, PILL_H),
    ));
    globals.pill.tint.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(width, PILL_H),
    ));
    globals.pill.tint.setNeedsDisplay(true);
}

pub(super) fn set_static_processing_bars(globals: &UiGlobals) {
    let h = BAR_MIN_H + (BAR_MAX_H - BAR_MIN_H) * 0.42;
    globals.pill.waveform.set_heights([h; BAR_COUNT]);
}

pub(super) fn reduced_motion_enabled() -> bool {
    if std::env::var_os("DICTATE_REDUCED_MOTION").is_some() {
        return true;
    }
    // macOS exposes the preference through the global defaults domain. A
    // failed read simply means the normal, restrained interpolation is used.
    std::process::Command::new("defaults")
        .args(["read", "-g", "reduceMotion"])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim() == "1")
        .unwrap_or(false)
}

/// RMS below this is treated as room tone and shows the resting bar.
const BAR_GATE_RMS: f32 = 0.003;
/// dBFS that maps to the resting bar / the full bar.
const BAR_DB_FLOOR: f64 = -48.0;
const BAR_DB_CEIL: f64 = -12.0;
/// Per-bar gain bias (±12 %) so adjacent bars never move as one block.
const BAR_BIAS: [f64; BAR_COUNT] = [
    0.92, 1.06, 0.97, 1.10, 0.90, 1.04, 1.12, 0.95, 1.08, 0.93, 1.02, 0.89, 1.05, 0.98,
];

pub(super) fn update_bars(globals: &UiGlobals) {
    let levels = ui_channel::recent_levels();
    let mut displayed = match globals.displayed_heights.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let mut heights = [BAR_MIN_H; BAR_COUNT];

    for (i, slot) in heights.iter_mut().enumerate() {
        // Right-align: bar[BAR_COUNT-1] = newest level. As new samples
        // arrive the entire history scrolls left by one bar.
        let level = if levels.is_empty() {
            0.0
        } else {
            let offset = BAR_COUNT.saturating_sub(levels.len());
            if i < offset {
                0.0
            } else {
                levels[i - offset]
            }
        };

        // Decibel window, the way Superwhisper / Wispr Flow shape their
        // bars. The old linear-ish map (RMS 0.043 = full height, 0.45 power)
        // pinned every spoken syllable at 50-100 %, so speech read as a solid
        // wall. Speech RMS spans roughly -45 dBFS (soft) to -15 dBFS (loud);
        // mapping that window linearly in dB gives the bars room to breathe.
        // Below the gate the bar rests. A small per-bar bias decorrelates
        // neighbours so the row does not move as one block.
        let target = if level < BAR_GATE_RMS {
            BAR_MIN_H
        } else {
            let db = 20.0 * (level as f64).max(1e-6).log10();
            let normalized = ((db - BAR_DB_FLOOR) / (BAR_DB_CEIL - BAR_DB_FLOOR)).clamp(0.0, 1.0);
            let normalized = (normalized * BAR_BIAS[i]).clamp(0.0, 1.0);
            BAR_MIN_H + normalized * (BAR_MAX_H - BAR_MIN_H)
        };

        // Eased attack and slower release: the eye follows a swell, not a
        // strobe. (Previously rise was instant.)
        let current = displayed[i];
        let new_h = if target >= current {
            current * 0.45 + target * 0.55
        } else {
            current * 0.82 + target * 0.18
        };
        displayed[i] = new_h;
        *slot = new_h;
    }
    globals.pill.waveform.set_heights(heights);
}

/// Synthetic "thinking" waveform shown while the pipeline transcribes / cleans
/// up. No mic samples arrive during Processing, so without this the bars decay
/// to a flat line and the pill looks frozen. Same ink bars at 40 % — no new
/// colour, but unmistakably animated so the user sees work is happening.
pub(super) fn animate_processing_bars(globals: &UiGlobals) {
    let mut displayed = match globals.displayed_heights.lock() {
        Ok(g) => g,
        Err(_) => return,
    };

    // Continuous wall-clock phase so the wave keeps moving every tick,
    // independent of frame timing.
    let t = CFAbsoluteTimeGetCurrent();
    const SPEED: f64 = 5.5; // radians/sec — ~0.9 sweeps per second
    const SPACING: f64 = 0.55; // radians/bar — ~1.2 wavelengths across the row
                               // Calm amplitude (peaks ~60% of full) so it reads as "thinking", not a
                               // loud audio signal.
    let amp = (BAR_MAX_H * 0.6 - BAR_MIN_H) / 2.0;
    let mid = BAR_MIN_H + amp;

    let mut heights = [BAR_MIN_H; BAR_COUNT];
    for (i, slot) in heights.iter_mut().enumerate() {
        let wave = (t * SPEED - i as f64 * SPACING).sin();
        let target = mid + amp * wave;
        // Ease toward the target so the hand-off from the live recording
        // waveform glides in rather than snapping.
        let new_h = displayed[i] * 0.6 + target * 0.4;
        displayed[i] = new_h;
        *slot = new_h;
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
