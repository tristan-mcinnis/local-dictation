//! Menu-bar status item + floating waveform pill.
//!
//! UI matches Wispr-Flow / Superwhisper conventions: a small rounded pill
//! at the bottom-center of the cursor's screen, containing a live
//! waveform driven by mic RMS. Hidden when idle.
//!
//! Architecture:
//!   * Worker thread broadcasts state changes via SHARED_STATE (atomic).
//!   * cpal audio thread writes RMS samples to `audio::AUDIO_LEVELS`.
//!   * Main thread runs NSApplication.run(); a CFRunLoopTimer fires every
//!     33 ms (~30 FPS) to update bar heights + show/hide the pill on
//!     state transitions.
//!   * Status item icon swaps emoji per state.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, AllocAnyThread, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSEvent,
    NSImage, NSMenu, NSMenuItem, NSScreen, NSStatusBar, NSStatusItem, NSView, NSWindow,
    NSWindowCollectionBehavior, NSWindowLevel, NSWindowStyleMask,
};
use objc2_core_foundation::{
    kCFRunLoopCommonModes, CFAbsoluteTimeGetCurrent, CFRunLoop, CFRunLoopTimer,
    CFRunLoopTimerContext,
};
use objc2_foundation::{MainThreadMarker, NSObject, NSPoint, NSRect, NSSize, NSString};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

// Pill geometry — tuned to match Wispr Flow's compact pill.
const PILL_W: f64 = 120.0;
const PILL_H: f64 = 44.0;
const BAR_COUNT: usize = 14;
const BAR_W: f64 = 3.0;
const BAR_GAP: f64 = 2.0;
const BAR_MAX_H: f64 = 26.0;
const BAR_MIN_H: f64 = 2.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum UiState {
    Idle = 0,
    Recording = 1,
    Processing = 2,
}

static SHARED_STATE: AtomicU8 = AtomicU8::new(0);

pub fn set_state(state: UiState) {
    SHARED_STATE.store(state as u8, Ordering::SeqCst);
}

struct UiGlobals {
    status_item: Retained<NSStatusItem>,
    pill_window: Retained<NSWindow>,
    bars: Vec<Retained<NSView>>,
    last_state: AtomicU8,
    /// Per-bar smoothed height. Rises instantly to a new peak, decays
    /// slowly toward the new RMS sample — gives the snappy bouncing
    /// feel of real audio meters.
    displayed_heights: Mutex<Vec<f64>>,
    /// Pre-built SF Symbol images for the three states. Swapped onto the
    /// status item's button when the state changes.
    icon_idle: Option<Retained<NSImage>>,
    icon_recording: Option<Retained<NSImage>>,
    icon_processing: Option<Retained<NSImage>>,
    /// Held alive so the Open-Log menu item's target stays valid.
    _log_opener: Retained<LogOpener>,
}
unsafe impl Sync for UiGlobals {}
unsafe impl Send for UiGlobals {}

// ─── Custom NSObject subclass that opens the daemon log on menu click ───
//
// NSMenuItem dispatches its action via objc selector — we need a target
// that responds to the selector. The simplest path is a tiny custom
// class with one method.
define_class!(
    #[unsafe(super(NSObject))]
    #[name = "FDLogOpener"]
    pub struct LogOpener;

    impl LogOpener {
        #[unsafe(method(openLog:))]
        fn open_log(&self, _sender: *mut AnyObject) {
            let _ = std::process::Command::new("open")
                .args(["-t", "/tmp/dictate-daemon.log"])
                .status();
        }
    }
);

impl LogOpener {
    fn new() -> Retained<Self> {
        let alloc = Self::alloc();
        unsafe { msg_send![alloc, init] }
    }
}

static GLOBALS: OnceLock<UiGlobals> = OnceLock::new();

pub fn init_and_run() -> eyre::Result<()> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| eyre::eyre!("menubar::init_and_run must be on main thread"))?;

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let log_opener = LogOpener::new();
    let (status_item, icon_idle, icon_recording, icon_processing) =
        build_status_item(mtm, &log_opener)?;
    let (pill_window, bars) = build_pill_window(mtm)?;

    let globals = UiGlobals {
        status_item,
        pill_window,
        bars,
        last_state: AtomicU8::new(255),
        displayed_heights: Mutex::new(vec![BAR_MIN_H; BAR_COUNT]),
        icon_idle,
        icon_recording,
        icon_processing,
        _log_opener: log_opener,
    };
    let _ = GLOBALS.set(globals);

    install_poll_timer();
    app.run();
    Ok(())
}

fn build_status_item(
    mtm: MainThreadMarker,
    log_opener: &Retained<LogOpener>,
) -> eyre::Result<(
    Retained<NSStatusItem>,
    Option<Retained<NSImage>>,
    Option<Retained<NSImage>>,
    Option<Retained<NSImage>>,
)> {
    let bar = unsafe { NSStatusBar::systemStatusBar() };
    let item = unsafe { bar.statusItemWithLength(-1.0) };
    let button = item
        .button(mtm)
        .ok_or_else(|| eyre::eyre!("status item has no button"))?;

    // Pre-build three SF Symbol images. setTemplate(true) makes them
    // monochrome and follow the menu-bar tint (white on dark, black on
    // light), matching every native macOS app.
    let icon_idle = sf_symbol("mic");
    let icon_recording = sf_symbol("mic.fill");
    let icon_processing = sf_symbol("waveform");
    if let Some(img) = &icon_idle {
        unsafe { button.setImage(Some(img)) };
        unsafe { button.setTitle(&NSString::from_str("")) };
    } else {
        // Fall back to text if SF Symbols somehow fail.
        unsafe { button.setTitle(&NSString::from_str("◯")) };
    }

    let menu = NSMenu::new(mtm);

    // Open Log → opens /tmp/dictate-daemon.log in the default text editor.
    let open_log = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Open Log"),
            Some(sel!(openLog:)),
            &NSString::from_str("l"),
        )
    };
    unsafe { open_log.setTarget(Some(log_opener)) };
    menu.addItem(&open_log);

    // Separator.
    let sep = unsafe { NSMenuItem::separatorItem(mtm) };
    menu.addItem(&sep);

    // Quit → standard NSApp terminate: selector (works with nil target).
    let quit_item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Quit local-dictation"),
            Some(sel!(terminate:)),
            &NSString::from_str("q"),
        )
    };
    menu.addItem(&quit_item);

    item.setMenu(Some(&menu));
    Ok((item, icon_idle, icon_recording, icon_processing))
}

/// Build an NSImage from an SF Symbol name. Returns None if the symbol
/// doesn't exist or we're on an older macOS without SF Symbols.
fn sf_symbol(name: &str) -> Option<Retained<NSImage>> {
    let ns_name = NSString::from_str(name);
    let img = unsafe {
        NSImage::imageWithSystemSymbolName_accessibilityDescription(&ns_name, None)
    };
    if let Some(ref img) = img {
        // Template mode: the image adopts the menu-bar tint color
        // (dark or light depending on the user's theme).
        unsafe { img.setTemplate(true) };
    }
    img
}

/// Build the floating pill window and return (window, bar_views).
/// The pill is a small rounded-rect with a subtle border, containing a
/// row of vertical bars that we animate from audio RMS.
fn build_pill_window(
    mtm: MainThreadMarker,
) -> eyre::Result<(Retained<NSWindow>, Vec<Retained<NSView>>)> {
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
    unsafe {
        window.setLevel(NSWindowLevel::from(3_isize)); // floating panel
        window.setOpaque(false);
        let clear = NSColor::clearColor();
        window.setBackgroundColor(Some(&clear));
        window.setIgnoresMouseEvents(true);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        window.setHasShadow(true);
    }

    // Content view: dark rounded pill with a subtle gray border.
    let content_view = unsafe { NSView::initWithFrame(NSView::alloc(mtm), frame) };
    unsafe {
        content_view.setWantsLayer(true);
        let layer: *mut objc2::runtime::AnyObject = msg_send![&*content_view, layer];
        let _: () = msg_send![layer, setCornerRadius: PILL_H / 2.0];
        let _: () = msg_send![layer, setMasksToBounds: true];
        // Background: near-black, slightly translucent
        let bg = NSColor::colorWithCalibratedRed_green_blue_alpha(0.05, 0.05, 0.07, 0.92);
        let bg_cg: *mut objc2::runtime::AnyObject = msg_send![&*bg, CGColor];
        let _: () = msg_send![layer, setBackgroundColor: bg_cg];
        // Border: faint gray ring matching the reference screenshot.
        let border = NSColor::colorWithCalibratedRed_green_blue_alpha(0.45, 0.45, 0.50, 0.65);
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
        let bar = unsafe { NSView::initWithFrame(NSView::alloc(mtm), bar_frame) };
        unsafe {
            bar.setWantsLayer(true);
            let layer: *mut objc2::runtime::AnyObject = msg_send![&*bar, layer];
            let _: () = msg_send![layer, setCornerRadius: (BAR_W / 2.0)];
            let white = NSColor::colorWithCalibratedRed_green_blue_alpha(0.92, 0.92, 0.95, 1.0);
            let white_cg: *mut objc2::runtime::AnyObject = msg_send![&*white, CGColor];
            let _: () = msg_send![layer, setBackgroundColor: white_cg];
        }
        unsafe { content_view.addSubview(&bar) };
        bars.push(bar);
    }

    window.setContentView(Some(&content_view));
    Ok((window, bars))
}

/// CFRunLoopTimer firing ~30 FPS. On every tick: update bar heights from
/// recent audio levels. On state transitions: show/hide pill, swap icon.
fn install_poll_timer() {
    unsafe extern "C-unwind" fn timer_cb(_t: *mut CFRunLoopTimer, _info: *mut c_void) {
        let globals = match GLOBALS.get() {
            Some(g) => g,
            None => return,
        };
        let now = SHARED_STATE.load(Ordering::SeqCst);
        let prev = globals.last_state.swap(now, Ordering::SeqCst);
        if now != prev {
            let state = match now {
                1 => UiState::Recording,
                2 => UiState::Processing,
                _ => UiState::Idle,
            };
            apply_state_transition(globals, state);
        }
        // Always update bar heights when pill is visible — drives the
        // waveform animation.
        if now == 1 || now == 2 {
            update_bars(globals);
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

fn apply_state_transition(globals: &UiGlobals, state: UiState) {
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
    };
    if let Some(button) = globals.status_item.button(mtm) {
        if let Some(img) = icon {
            unsafe { button.setImage(Some(img)) };
        } else {
            // Fallback if SF Symbols weren't available.
            let fallback = match state {
                UiState::Idle => "◯",
                UiState::Recording => "●",
                UiState::Processing => "◌",
            };
            unsafe { button.setTitle(&NSString::from_str(fallback)) };
        }
    }

    match state {
        UiState::Idle => {
            crate::audio::reset_levels();
            collapse_bars(globals);
            unsafe { globals.pill_window.orderOut(None) };
        }
        UiState::Recording | UiState::Processing => {
            position_pill_at_cursor_screen(&globals.pill_window);
            unsafe { globals.pill_window.orderFrontRegardless() };
        }
    }
}

fn update_bars(globals: &UiGlobals) {
    let levels = crate::audio::recent_levels();
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

        let bar_block_w = BAR_COUNT as f64 * BAR_W + (BAR_COUNT - 1) as f64 * BAR_GAP;
        let x = (PILL_W - bar_block_w) / 2.0 + i as f64 * (BAR_W + BAR_GAP);
        let y = (PILL_H - new_h) / 2.0;
        let new_frame = NSRect::new(NSPoint::new(x, y), NSSize::new(BAR_W, new_h));
        unsafe { bar.setFrame(new_frame) };
    }
}

fn collapse_bars(globals: &UiGlobals) {
    if let Ok(mut g) = globals.displayed_heights.lock() {
        for v in g.iter_mut() {
            *v = BAR_MIN_H;
        }
    }
    for (i, bar) in globals.bars.iter().enumerate() {
        let bar_block_w = BAR_COUNT as f64 * BAR_W + (BAR_COUNT - 1) as f64 * BAR_GAP;
        let x = (PILL_W - bar_block_w) / 2.0 + i as f64 * (BAR_W + BAR_GAP);
        let y = (PILL_H - BAR_MIN_H) / 2.0;
        let f = NSRect::new(NSPoint::new(x, y), NSSize::new(BAR_W, BAR_MIN_H));
        unsafe { bar.setFrame(f) };
    }
}

fn position_pill_at_cursor_screen(window: &NSWindow) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    unsafe {
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
        let y = chosen.origin.y + 80.0;
        window.setFrameOrigin(NSPoint::new(x, y));
    }
}
