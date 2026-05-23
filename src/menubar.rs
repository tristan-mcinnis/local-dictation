//! Menu-bar status item + floating cursor pill.
//!
//! Architecture:
//!
//!   * Worker thread (daemon) sets `SHARED_STATE` atomically when state
//!     changes (Idle → Recording → Processing → Idle).
//!
//!   * Main thread runs `NSApplication.run()` which pumps the CFRunLoop
//!     that CGEventTap is also attached to.
//!
//!   * A CFRunLoopTimer on the main run loop polls `SHARED_STATE` every
//!     80 ms. When the state changes it updates the status-item title and
//!     shows/hides the floating pill. Polling instead of cross-thread
//!     dispatch keeps things simple and the cost is negligible (one atomic
//!     load + a cheap comparison).
//!
//!   * The pill is a borderless transparent NSWindow at floating-panel
//!     level, positioned at the bottom-center of the screen the cursor is
//!     currently on. Hidden when idle.

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::{sel, AllocAnyThread, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSEvent,
    NSMenu, NSMenuItem, NSScreen, NSStatusBar, NSStatusItem, NSTextField, NSView,
    NSWindow, NSWindowCollectionBehavior, NSWindowLevel, NSWindowStyleMask,
};
use objc2_core_foundation::{
    kCFRunLoopCommonModes, CFAbsoluteTimeGetCurrent, CFRetained, CFRunLoop,
    CFRunLoopTimer, CFRunLoopTimerContext,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

/// State the worker thread broadcasts to the UI.
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

/// Globals held alive for the lifetime of the app. Stored in OnceLock
/// because Retained<T> isn't const-constructible.
struct UiGlobals {
    status_item: Retained<NSStatusItem>,
    pill_window: Retained<NSWindow>,
    pill_label: Retained<NSTextField>,
    last_state: AtomicU8,
}
// SAFETY: All AppKit objects we hold are only ever accessed on the main
// thread (the run-loop timer fires on the main thread). The Sync bound is
// only needed for OnceLock; we never actually share these across threads.
unsafe impl Sync for UiGlobals {}
unsafe impl Send for UiGlobals {}

static GLOBALS: OnceLock<UiGlobals> = OnceLock::new();

/// Initialize the menu-bar item + floating pill, then run NSApplication.
/// Blocks forever (NSApp.run never returns until terminate is called).
///
/// IMPORTANT: must be called on the main thread.
pub fn init_and_run() -> eyre::Result<()> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| eyre::eyre!("menubar::init_and_run must be on main thread"))?;

    let app = NSApplication::sharedApplication(mtm);
    // Accessory = no Dock icon, no menu (we have our own menu bar item).
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let status_item = build_status_item(mtm)?;
    let (pill_window, pill_label) = build_pill_window(mtm)?;

    let globals = UiGlobals {
        status_item,
        pill_window,
        pill_label,
        last_state: AtomicU8::new(255), // forces first poll to update UI
    };
    let _ = GLOBALS.set(globals);

    install_poll_timer();

    // Block forever. Quit menu item calls NSApp.terminate.
    app.run();
    Ok(())
}

fn build_status_item(mtm: MainThreadMarker) -> eyre::Result<Retained<NSStatusItem>> {
    let bar = unsafe { NSStatusBar::systemStatusBar() };
    // -1.0 = NSVariableStatusItemLength (auto-size to title).
    let item = unsafe { bar.statusItemWithLength(-1.0) };

    // Title — initial emoji, will be updated by the poll timer.
    let button = item
        .button(mtm)
        .ok_or_else(|| eyre::eyre!("status item has no button"))?;
    let title = NSString::from_str("🎤");
    unsafe { button.setTitle(&title) };

    // Menu with a Quit item.
    let menu = NSMenu::new(mtm);
    let quit_title = NSString::from_str("Quit fast-dictate");
    let quit_key = NSString::from_str("q");
    let quit_item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &quit_title,
            Some(sel!(terminate:)),
            &quit_key,
        )
    };
    menu.addItem(&quit_item);
    item.setMenu(Some(&menu));

    Ok(item)
}

fn build_pill_window(
    mtm: MainThreadMarker,
) -> eyre::Result<(Retained<NSWindow>, Retained<NSTextField>)> {
    // Tiny pill — 180x36 at bottom-center of the cursor's screen.
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(180.0, 36.0));
    let style = NSWindowStyleMask::Borderless;
    let backing = NSBackingStoreType::Buffered;
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            style,
            backing,
            false,
        )
    };
    // Floating panel level (3) so it stays above normal app windows.
    unsafe { window.setLevel(NSWindowLevel::from(3_isize)) };
    // Transparent background — our content view draws the pill itself.
    unsafe { window.setOpaque(false) };
    let clear: Retained<NSColor> = unsafe { NSColor::clearColor() };
    unsafe { window.setBackgroundColor(Some(&clear)) };
    // Don't accept mouse events; don't ever become key window.
    unsafe { window.setIgnoresMouseEvents(true) };
    // Float across all spaces, persist across app switches.
    unsafe {
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        )
    };
    unsafe { window.setHasShadow(true) };

    // Build a content view containing a rounded-rect background + label.
    let content_view = unsafe { NSView::initWithFrame(NSView::alloc(mtm), frame) };
    unsafe { content_view.setWantsLayer(true) };
    // Set layer background via CALayer (rounded corners + dark fill).
    unsafe {
        let layer: *mut objc2::runtime::AnyObject = msg_send![&*content_view, layer];
        let _: () = msg_send![layer, setCornerRadius: 18.0_f64];
        let _: () = msg_send![layer, setMasksToBounds: true];
        // Background color via NSColor → CGColor.
        let bg: Retained<NSColor> = NSColor::colorWithCalibratedRed_green_blue_alpha(
            0.08, 0.08, 0.10, 0.92,
        );
        let cgcolor: *mut objc2::runtime::AnyObject = msg_send![&*bg, CGColor];
        let _: () = msg_send![layer, setBackgroundColor: cgcolor];
    }

    // Centered label, white text, system font.
    let label_frame = NSRect::new(NSPoint::new(0.0, 4.0), NSSize::new(180.0, 28.0));
    let label =
        unsafe { NSTextField::initWithFrame(NSTextField::alloc(mtm), label_frame) };
    unsafe {
        label.setStringValue(&NSString::from_str("🎤 Recording"));
        label.setBezeled(false);
        label.setDrawsBackground(false);
        label.setEditable(false);
        label.setSelectable(false);
        label.setAlignment(objc2_app_kit::NSTextAlignment::Center);
        let white = NSColor::whiteColor();
        label.setTextColor(Some(&white));
    }
    unsafe { content_view.addSubview(&label) };
    window.setContentView(Some(&content_view));

    // Don't show yet — poll timer flips it visible on Recording.
    Ok((window, label))
}

/// Schedule a CFRunLoopTimer on the main run loop. It fires every 80 ms,
/// reads `SHARED_STATE`, and updates the UI if the state changed.
fn install_poll_timer() {
    unsafe extern "C-unwind" fn timer_cb(_t: *mut CFRunLoopTimer, _info: *mut c_void) {
        let globals = match GLOBALS.get() {
            Some(g) => g,
            None => return,
        };
        let now = SHARED_STATE.load(Ordering::SeqCst);
        let prev = globals.last_state.swap(now, Ordering::SeqCst);
        if now == prev {
            return;
        }
        let state = match now {
            1 => UiState::Recording,
            2 => UiState::Processing,
            _ => UiState::Idle,
        };
        apply_state(globals, state);
    }

    unsafe {
        let mut ctx = CFRunLoopTimerContext {
            version: 0,
            info: std::ptr::null_mut(),
            retain: None,
            release: None,
            copyDescription: None,
        };
        let now = CFAbsoluteTimeGetCurrent();
        let timer = CFRunLoopTimer::new(
            None,                       // allocator
            now + 0.08,                 // fire date (first fire 80 ms from now)
            0.08,                       // interval (80 ms)
            0,                          // flags
            0,                          // order
            Some(timer_cb),
            &mut ctx,
        )
        .expect("CFRunLoopTimer::new");
        let run_loop = CFRunLoop::main().expect("main run loop");
        run_loop.add_timer(Some(&timer), kCFRunLoopCommonModes);
        // Leak the timer — it's tied to the run loop for the lifetime of
        // the process.
        std::mem::forget(timer);
        let _ = run_loop;
    }
}

fn apply_state(globals: &UiGlobals, state: UiState) {
    let (title, pill_text, pill_visible) = match state {
        UiState::Idle => ("🎤", "🎤 Idle", false),
        UiState::Recording => ("🔴 REC", "🔴 Recording…", true),
        UiState::Processing => ("⏳", "⏳ Processing…", true),
    };
    let mtm = match MainThreadMarker::new() {
        Some(m) => m,
        None => return, // shouldn't happen — timer fires on main thread
    };
    if let Some(button) = globals.status_item.button(mtm) {
        unsafe { button.setTitle(&NSString::from_str(title)) };
    }
    unsafe {
        globals
            .pill_label
            .setStringValue(&NSString::from_str(pill_text));
    }
    if pill_visible {
        position_pill_at_cursor_screen(&globals.pill_window);
        unsafe {
            globals
                .pill_window
                .orderFrontRegardless();
        }
    } else {
        unsafe { globals.pill_window.orderOut(None) };
    }
}

/// Position the pill at the bottom-center of whichever screen currently
/// contains the mouse cursor. Re-computed on every show in case the user
/// dragged the cursor between screens.
fn position_pill_at_cursor_screen(window: &NSWindow) {
    unsafe {
        let mouse_loc = NSEvent::mouseLocation(); // screen coords, bottom-left origin
        let screens = NSScreen::screens(MainThreadMarker::new().unwrap_unchecked());
        let mut chosen_frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0));
        for screen in screens.iter() {
            let f = screen.frame();
            if mouse_loc.x >= f.origin.x
                && mouse_loc.x <= f.origin.x + f.size.width
                && mouse_loc.y >= f.origin.y
                && mouse_loc.y <= f.origin.y + f.size.height
            {
                chosen_frame = f;
                break;
            }
        }
        if chosen_frame.size.width == 0.0 {
            // Fallback to main screen
            if let Some(main) = NSScreen::mainScreen(MainThreadMarker::new().unwrap_unchecked()) {
                chosen_frame = main.frame();
            }
        }
        let win_frame = window.frame();
        let x = chosen_frame.origin.x
            + (chosen_frame.size.width - win_frame.size.width) / 2.0;
        let y = chosen_frame.origin.y + 80.0; // 80 px above bottom edge
        window.setFrameOrigin(NSPoint::new(x, y));
    }
}
