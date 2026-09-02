//! Floating waveform pill window and its state animation.
use super::*;

/// Build an NSImage from an SF Symbol name. Returns None if the symbol
/// doesn't exist or we're on an older macOS without SF Symbols.
pub(super) fn sf_symbol(name: &str) -> Option<Retained<NSImage>> {
    let ns_name = NSString::from_str(name);
    let img = { NSImage::imageWithSystemSymbolName_accessibilityDescription(&ns_name, None) };
    if let Some(ref img) = img {
        // Template mode: the image adopts the menu-bar tint color
        // (dark or light depending on the user's theme).
        img.setTemplate(true);
    }
    img
}

pub(super) struct PillBuild {
    pub(super) window: Retained<NSWindow>,
    pub(super) content: Retained<NSView>,
    pub(super) bars: Vec<Retained<NSView>>,
    pub(super) status: Retained<NSTextField>,
    pub(super) cancel: Retained<NSButton>,
    pub(super) settings: Retained<NSButton>,
}

/// Build the floating pill window. It starts as a compact waveform capsule;
/// the same content view grows horizontally when the pointer rests over it,
/// revealing status text plus two useful, >=32px controls. Keeping one window
/// and one view tree makes hover expansion cheap and interruptible.
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
        // Controls remain outside the compact frame until expanded.
        window.setIgnoresMouseEvents(false);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        window.setHasShadow(true);
    }

    // Content view: dark rounded pill with a subtle gray border.
    let content_view = NSView::initWithFrame(NSView::alloc(mtm), frame);
    unsafe {
        content_view.setWantsLayer(true);
        let layer: *mut objc2::runtime::AnyObject = msg_send![&*content_view, layer];
        let _: () = msg_send![layer, setCornerRadius: PILL_H / 2.0];
        let _: () = msg_send![layer, setMasksToBounds: true];
        // Background: refined charcoal HUD (family palette).
        let bg = theme::pill_bg();
        let bg_cg: *mut objc2::runtime::AnyObject = msg_send![&*bg, CGColor];
        let _: () = msg_send![layer, setBackgroundColor: bg_cg];
        // Border: a hairline light ring, subtler than the old grey.
        let border = theme::pill_border();
        let border_cg: *mut objc2::runtime::AnyObject = msg_send![&*border, CGColor];
        let _: () = msg_send![layer, setBorderColor: border_cg];
        let _: () = msg_send![layer, setBorderWidth: 1.0_f64];
    }

    // Build N bar views centered horizontally + vertically. Each bar is an
    // NSView with a white layer background. We mutate frame on every UI tick.
    let bar_block_w = BAR_COUNT as f64 * BAR_W + (BAR_COUNT - 1) as f64 * BAR_GAP;
    let bar_block_x = (PILL_W - bar_block_w) / 2.0;
    let mut bars = Vec::with_capacity(BAR_COUNT);
    for i in 0..BAR_COUNT {
        let x = bar_block_x + i as f64 * (BAR_W + BAR_GAP);
        let y = (PILL_H - BAR_MIN_H) / 2.0;
        let bar_frame = NSRect::new(NSPoint::new(x, y), NSSize::new(BAR_W, BAR_MIN_H));
        let bar = NSView::initWithFrame(NSView::alloc(mtm), bar_frame);
        unsafe {
            bar.setWantsLayer(true);
            let layer: *mut objc2::runtime::AnyObject = msg_send![&*bar, layer];
            let _: () = msg_send![layer, setCornerRadius: (BAR_W / 2.0)];
            let active = theme::bar_active();
            let active_cg: *mut objc2::runtime::AnyObject = msg_send![&*active, CGColor];
            let _: () = msg_send![layer, setBackgroundColor: active_cg];
        }
        content_view.addSubview(&bar);
        bars.push(bar);
    }

    // Contextual shelf: hidden by clipping while compact, then revealed by
    // the width animation. The labels are redundant status cues (not color
    // alone), and the buttons have persistent tooltips/accessibility names.
    let status = NSTextField::labelWithString(&NSString::from_str("Ready"), mtm);
    status.setFrame(NSRect::new(
        NSPoint::new(PILL_W + 14.0, 8.0),
        NSSize::new(68.0, 18.0),
    ));
    status.setFont(Some(&NSFont::systemFontOfSize(11.0)));
    unsafe {
        let _: () = msg_send![&*status, setTextColor: &*NSColor::whiteColor()];
        let _: () = msg_send![&*status, setAlphaValue: 0.0_f64];
        let _: () =
            msg_send![&*status, setAccessibilityLabel: &*NSString::from_str("Dictation status")];
    }

    let cancel: Retained<NSButton> = unsafe {
        msg_send![
            NSButton::alloc(mtm),
            initWithFrame: NSRect::new(NSPoint::new(PILL_EXPANDED_W - 66.0, 4.0), NSSize::new(26.0, 26.0))
        ]
    };
    cancel.setTitle(&NSString::from_str("×"));
    cancel.setBordered(false);
    cancel.setToolTip(Some(&NSString::from_str("Cancel recording (Escape)")));
    unsafe {
        let _: () = msg_send![&*cancel, setTarget: &**actions];
        let _: () = msg_send![&*cancel, setAction: sel!(cancelPill:)];
        let _: () = msg_send![&*cancel, setKeyEquivalent: &*NSString::from_str("\u{1b}")];
        let _: () = msg_send![&*cancel, setFont: &*NSFont::systemFontOfSize(17.0)];
        let _: () =
            msg_send![&*cancel, setAccessibilityLabel: &*NSString::from_str("Cancel recording")];
        let _: () = msg_send![&*cancel, setAlphaValue: 0.0_f64];
    }

    let settings: Retained<NSButton> = unsafe {
        msg_send![
            NSButton::alloc(mtm),
            initWithFrame: NSRect::new(NSPoint::new(PILL_EXPANDED_W - 36.0, 4.0), NSSize::new(26.0, 26.0))
        ]
    };
    settings.setTitle(&NSString::from_str("⚙"));
    settings.setBordered(false);
    settings.setToolTip(Some(&NSString::from_str("Open Dictation settings")));
    unsafe {
        let _: () = msg_send![&*settings, setTarget: &**actions];
        let _: () = msg_send![&*settings, setAction: sel!(openSettings:)];
        settings.setFont(Some(&NSFont::systemFontOfSize(13.0)));
        let _: () = msg_send![&*settings, setAccessibilityLabel: &*NSString::from_str("Open Dictation settings")];
        let _: () = msg_send![&*settings, setAlphaValue: 0.0_f64];
    }

    content_view.addSubview(&status);
    content_view.addSubview(&cancel);
    content_view.addSubview(&settings);

    window.setContentView(Some(&content_view));
    Ok(PillBuild {
        window,
        content: content_view,
        bars,
        status,
        cancel,
        settings,
    })
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
            UiState::Success | UiState::Error | UiState::Idle => {}
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

    // Pick the SF Symbol for this state. mic / mic.fill / waveform make
    // a clean visual progression: outline → filled → animated wave.
    let icon = match state {
        UiState::Idle => &globals.icon_idle,
        UiState::Recording => &globals.icon_recording,
        UiState::Processing => &globals.icon_processing,
        UiState::Success => &globals.icon_recording,
        UiState::Error => &globals.icon_processing,
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
                UiState::Error => "!",
            };
            button.setTitle(&NSString::from_str(fallback));
        }
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
            resize_pill(globals, 0.0);
            globals.pill_window.orderOut(None);
        }
        UiState::Recording => {
            set_bars_color(globals, &theme::bar_active());
            globals
                .pill_status
                .setStringValue(&NSString::from_str("Recording · release to finish"));
            set_pill_controls_visible(globals, false);
            resize_pill(globals, 0.0);
            position_pill_at_cursor_screen(&globals.pill_window);
            globals.pill_window.orderFrontRegardless();
        }
        UiState::Processing => {
            set_bars_color(globals, &theme::bar_busy());
            globals
                .pill_status
                .setStringValue(&NSString::from_str("Processing…"));
            set_pill_controls_visible(globals, false);
            resize_pill(globals, 0.0);
            position_pill_at_cursor_screen(&globals.pill_window);
            globals.pill_window.orderFrontRegardless();
        }
        UiState::Success => {
            set_bars_color(globals, &theme::bar_success());
            globals
                .pill_status
                .setStringValue(&NSString::from_str("Done"));
            set_static_success_bars(globals);
            set_pill_controls_visible(globals, false);
            resize_pill(globals, 0.0);
            position_pill_at_cursor_screen(&globals.pill_window);
            globals.pill_window.orderFrontRegardless();
        }
        UiState::Error => {
            set_bars_color(globals, &theme::bar_error());
            globals
                .pill_status
                .setStringValue(&NSString::from_str("Couldn’t transcribe"));
            set_static_error_bars(globals);
            set_pill_controls_visible(globals, false);
            resize_pill(globals, 0.0);
            position_pill_at_cursor_screen(&globals.pill_window);
            globals.pill_window.orderFrontRegardless();
        }
    }
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
    let target = if pointer_over_pill(&globals.pill_window) {
        1.0
    } else {
        0.0
    };
    let Ok(mut progress) = globals.pill_expanded.lock() else {
        return;
    };
    let next = if globals.reduced_motion {
        target
    } else {
        *progress + (target - *progress) * 0.24
    };
    let next = if (next - target).abs() < 0.01 {
        target
    } else {
        next
    };
    if (next - *progress).abs() < 0.004 {
        return;
    }
    *progress = next;
    drop(progress);
    resize_pill(globals, next);
    set_pill_controls_visible(globals, next > 0.08);
}

pub(super) fn set_pill_controls_visible(globals: &UiGlobals, visible: bool) {
    let alpha = if visible { 1.0 } else { 0.0 };
    unsafe {
        let _: () = msg_send![&*globals.pill_status, setAlphaValue: alpha];
        let _: () = msg_send![&*globals.pill_cancel, setAlphaValue: alpha];
        let _: () = msg_send![&*globals.pill_settings, setAlphaValue: alpha];
    }
}

/// Resize the transparent borderless window around the same horizontal
/// centre. `setFrame:display:animate:` is deliberately non-animated; the
/// interpolation above is what makes the transition interruptible.
pub(super) fn resize_pill(globals: &UiGlobals, progress: f64) {
    let width = PILL_W + (PILL_EXPANDED_W - PILL_W) * progress.clamp(0.0, 1.0);
    let old = globals.pill_window.frame();
    let cx = old.origin.x + old.size.width / 2.0;
    let frame = NSRect::new(
        NSPoint::new(cx - width / 2.0, old.origin.y),
        NSSize::new(width, PILL_H),
    );
    unsafe {
        let _: () =
            msg_send![&*globals.pill_window, setFrame: frame, display: true, animate: false];
    }
    globals.pill_content.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(width, PILL_H),
    ));
}

pub(super) fn set_static_processing_bars(globals: &UiGlobals) {
    let h = BAR_MIN_H + (BAR_MAX_H - BAR_MIN_H) * 0.42;
    for (i, bar) in globals.bars.iter().enumerate() {
        set_bar_height(bar, i, h);
    }
}

pub(super) fn set_static_success_bars(globals: &UiGlobals) {
    for (i, bar) in globals.bars.iter().enumerate() {
        set_bar_height(bar, i, if i % 2 == 0 { 12.0 } else { 8.0 });
    }
}

pub(super) fn set_static_error_bars(globals: &UiGlobals) {
    for (i, bar) in globals.bars.iter().enumerate() {
        set_bar_height(
            bar,
            i,
            if i == BAR_COUNT / 2 {
                BAR_MAX_H
            } else {
                BAR_MIN_H
            },
        );
    }
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

pub(super) fn update_bars(globals: &UiGlobals) {
    let levels = ui_channel::recent_levels();
    let mut displayed = match globals.displayed_heights.lock() {
        Ok(g) => g,
        Err(_) => return,
    };

    for (i, bar) in globals.bars.iter().enumerate() {
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

        // Noise gate + moderate gain. Floor at RMS 0.003 silences the
        // "hum animation" the user noticed — ambient mic noise sits
        // around 0.001-0.0025, real speech starts around 0.005+. powf
        // shape (0.45) gives quiet speech visible presence without
        // making the bars constantly max out on loud syllables.
        let target = if level < 0.003 {
            BAR_MIN_H
        } else {
            let normalized = (((level - 0.003) * 25.0) as f64).powf(0.45).min(1.0);
            BAR_MIN_H + normalized * (BAR_MAX_H - BAR_MIN_H)
        };

        // Peak-hold with slow decay: rise instantly to new highs, drop
        // gently toward target so the eye can catch the peak.
        let current = displayed[i];
        let new_h = if target >= current {
            target
        } else {
            current * 0.78 + target * 0.22
        };
        displayed[i] = new_h;
        set_bar_height(bar, i, new_h);
    }
}

/// Synthetic "thinking" waveform shown while the pipeline transcribes / cleans
/// up. No mic samples arrive during Processing, so without this the bars decay
/// to a flat line and the pill looks frozen. We drive the same white bars with
/// a sine wave that travels left→right across them — still a waveform, no new
/// colors, but unmistakably animated so the user sees work is happening.
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

    for (i, bar) in globals.bars.iter().enumerate() {
        let wave = (t * SPEED - i as f64 * SPACING).sin();
        let target = mid + amp * wave;
        // Ease toward the target so the hand-off from the live recording
        // waveform glides in rather than snapping.
        let new_h = displayed[i] * 0.6 + target * 0.4;
        displayed[i] = new_h;
        set_bar_height(bar, i, new_h);
    }
}

/// Recolour every waveform bar (called on state change): off-white while
/// recording, muted grey while "thinking", so the two states are distinct.
pub(super) fn set_bars_color(globals: &UiGlobals, color: &NSColor) {
    for bar in globals.bars.iter() {
        unsafe {
            let layer: *mut objc2::runtime::AnyObject = msg_send![&**bar, layer];
            let cg: *mut objc2::runtime::AnyObject = msg_send![color, CGColor];
            let _: () = msg_send![layer, setBackgroundColor: cg];
        }
    }
}

/// Position bar `i` at height `h`, vertically centered within the pill.
pub(super) fn set_bar_height(bar: &NSView, i: usize, h: f64) {
    let bar_block_w = BAR_COUNT as f64 * BAR_W + (BAR_COUNT - 1) as f64 * BAR_GAP;
    let x = (PILL_W - bar_block_w) / 2.0 + i as f64 * (BAR_W + BAR_GAP);
    let y = (PILL_H - h) / 2.0;
    let frame = NSRect::new(NSPoint::new(x, y), NSSize::new(BAR_W, h));
    bar.setFrame(frame);
}

pub(super) fn collapse_bars(globals: &UiGlobals) {
    if let Ok(mut g) = globals.displayed_heights.lock() {
        for v in g.iter_mut() {
            *v = BAR_MIN_H;
        }
    }
    for (i, bar) in globals.bars.iter().enumerate() {
        set_bar_height(bar, i, BAR_MIN_H);
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
        let y = chosen.origin.y + 32.0;
        window.setFrameOrigin(NSPoint::new(x, y));
    }
}

