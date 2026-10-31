//! Menu-bar status item + floating waveform pill.
//!
//! UI matches Wispr-Flow / Superwhisper conventions: a small rounded pill
//! at the bottom-center of the cursor's screen, containing a live
//! waveform driven by mic RMS. Hidden when idle.
//!
//! The status-item menu leads with the actions that are used: Record/Stop,
//! Cancel (live only while cancellable), Copy last text, Paste last text (the
//! already-captured transcript — never a re-transcribe), Retry last audio
//! (re-transcribe), History and Settings. A single "More" submenu keeps the
//! config and diagnostics, and a read-only latency line sits above Quit.
//! Settings-changing items write `settings.json` and relaunch the daemon
//! so the change takes effect; the model load makes in-process swapping not
//! worth the complexity.
//!
//! Architecture:
//!   * Worker thread broadcasts state changes via SHARED_STATE (atomic) and
//!     the last injected text via LAST_DICTATION (mutex).
//!   * cpal audio thread writes RMS samples to `audio::AUDIO_LEVELS`.
//!   * Main thread runs NSApplication.run(); a CFRunLoopTimer fires every
//!     33 ms (~30 FPS) to update bar heights + show/hide the pill on
//!     state transitions.
//!   * Status item icon swaps SF Symbols per state.

mod dictionary;
mod history;
mod history_detail;
mod pill;
mod render_ui;
pub(crate) mod setup;
pub(crate) mod slate;
use dictionary::*;
use history::*;
use history_detail::*;
pub(crate) use history_detail::show_history_detail;
// The Settings window's History pane groups rows by day with the same
// formatter the launcher panel uses — one implementation, two surfaces.
pub(crate) use history::{history_rows, HistoryRow};
use pill::*;
pub use render_ui::render_ui;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, sel, AllocAnyThread, MainThreadOnly};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSBackingStoreType,
    NSBezierPath,
    NSBitmapImageFileType, NSBitmapImageRep, NSButton, NSColor, NSControl, NSControlStateValueOff,
    NSControlStateValueOn, NSEvent, NSFont, NSFontAttributeName, NSForegroundColorAttributeName,
    NSGraphicsContext, NSImage, NSImageSymbolConfiguration, NSImageView, NSKernAttributeName, NSLineBreakMode, NSMenu,
    NSMenuDelegate, NSMenuItem, NSPanel, NSScreen, NSScrollView, NSSearchField, NSShadow, NSStatusBar, NSStatusItem,
    NSTextAlignment, NSTextField, NSTextView, NSTrackingArea, NSView, NSVisualEffectBlendingMode,
    NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView, NSWindow,
    NSWindowCollectionBehavior, NSWindowLevel, NSWindowStyleMask, NSWindowTitleVisibility,
};
use objc2_core_foundation::{
    kCFRunLoopCommonModes, CFAbsoluteTimeGetCurrent, CFRetained, CFRunLoop, CFRunLoopTimer,
    CFRunLoopTimerContext,
};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSDate, NSDateFormatter, NSMutableAttributedString, NSNotification,
    NSNumber, NSObject, NSPoint, NSRange, NSRect, NSSize, NSString,
};

use crate::history::Entry;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::settings::{self, PillSize, Settings, WaveformSensitivity};
use crate::ui_channel::{self, UiState};

// ─── Slate geometry ──────────────────────────────────────────────────────
//
// Every number here is either a house token or a value derived from one. The
// pill is Compact (32) or Standard (36), with a stable cancel slot; its
// geometry falls out of the control and spacing tokens and ten live bars.

/// House palette (design-system/tokens.json via the generated `house_theme.rs`).
/// Both appearances live in the file; `slate::palette()` picks one per draw and
/// `theme::*` hands out dynamic colours for the parts AppKit draws itself. No
/// RGBA literal lives outside the generated file.
#[path = "house_theme.rs"]
pub(crate) mod house_theme;

use house_theme::{
    CONTROL_CHIP, CONTROL_FOOTER, CONTROL_INPUT, CONTROL_KEYCAP, CONTROL_PILL, CONTROL_ROW, CONTROL_STATUSGLYPH, CONTROL_TILE, HAIRLINE, LAYOUT_PANEL_WIDTH, RADIUS_LG, RADIUS_MD,
    RADIUS_ROW, RADIUS_SM, RADIUS_TILE, RADIUS_XL, RADIUS_XS, SHADOW_SELECTION, SPACE_LG, SPACE_MD,
    SPACE_SM, SPACE_XS, SPACE_XXL, SPACE_XXS, TRACKING_SECTION, TYPE_BODY_SMALL, TYPE_CAPTION, TYPE_INPUT,
    TYPE_KEY_CAP, TYPE_META, TYPE_SECTION, WEIGHT_MEDIUM, WEIGHT_REGULAR,
    WEIGHT_SEMIBOLD,
};

mod actions;
mod status_menu;
mod system;
use actions::*;
pub(crate) use status_menu::write_and_open_commands_reference;
use status_menu::*;
pub(crate) use system::{
    copy_to_clipboard,
    export_log_to_downloads,
    open_corrections_folder,
    open_log_in_editor,
    open_prompts_file,
    relaunch_daemon,
    run_on_main,
};
use system::*;

/// Compact is one House spacing step shorter than Standard.
const PILL_H: f64 = CONTROL_PILL - SPACE_XXS;
const PILL_PAD: f64 = SPACE_XS;
const PILL_TEXT_PAD: f64 = SPACE_SM;
const PILL_GAP: f64 = SPACE_XS;
const CANCEL_HIT: f64 = CONTROL_TILE;
const WELL_H: f64 = CONTROL_TILE - SPACE_XXS;
const WELL_PAD: f64 = 0.0;
const WELL_RADIUS: f64 = RADIUS_LG;

const BAR_COUNT: usize = 10;
const BAR_W: f64 = HAIRLINE * 3.0;
const BAR_GAP: f64 = SPACE_XXS / 2.0;
const BAR_MAX_H: f64 = CONTROL_KEYCAP;
const BAR_MIN_H: f64 = HAIRLINE * 2.0;
const BAR_BLOCK_W: f64 = BAR_COUNT as f64 * BAR_W + (BAR_COUNT as f64 - 1.0) * BAR_GAP;
const PILL_W: f64 = 2.0 * PILL_PAD + CANCEL_HIT + SPACE_XXS + BAR_BLOCK_W;
const PREVIEW_W: f64 = LAYOUT_PANEL_WIDTH / 4.0;
const PROCESSING_TEXT_W: f64 = CONTROL_INPUT * 2.0;

/// Dynamic colours for the parts AppKit paints for us (text fields, popup
/// titles, placeholders). Each one re-resolves per appearance, so a window
/// never has to be rebuilt when the user switches light/dark.
#[allow(dead_code)] // the full palette is named once; not every tier is consumed yet
pub(crate) mod theme {
    use super::house_theme::{DARK, LIGHT};
    use super::slate::dyn_color;
    use objc2::rc::Retained;
    use objc2_app_kit::NSColor;

    macro_rules! token {
        ($($fn_name:ident),* $(,)?) => {
            $(
                pub fn $fn_name() -> Retained<NSColor> {
                    dyn_color(
                        concat!("ld.", stringify!($fn_name)),
                        LIGHT.$fn_name,
                        DARK.$fn_name,
                    )
                }
            )*
        };
    }

    token!(
        surface,
        surface_raised,
        surface_sunken,
        surface_tint,
        well,
        tile_fill,
        tile_stroke,
        chip_fill,
        text_primary,
        text_secondary,
        text_tertiary,
        text_inverse,
        accent,
        accent_soft,
        success,
        warning,
        danger,
        stroke,
        stroke_strong,
        divider,
        highlight_top,
        selection_fill,
        selection_ring,
        hover_fill,
        key_cap_fill,
        key_cap_stroke,
        panel_tint,
    );
}

// ─── Appearance ──────────────────────────────────────────────────────────

/// Apply the user's Appearance setting to the whole application. Every window
/// inherits it, so nothing is pinned to Aqua any more and Dark is the default.
pub(crate) fn apply_app_appearance(mtm: MainThreadMarker, choice: &str) {
    let app = NSApplication::sharedApplication(mtm);
    let name = match choice {
        "light" => Some(unsafe { NSAppearanceNameAqua }),
        "dark" => Some(unsafe { NSAppearanceNameDarkAqua }),
        // "system" — clear the override and follow the system setting.
        _ => None,
    };
    match name {
        Some(n) => {
            if let Some(a) = NSAppearance::appearanceNamed(n) {
                app.setAppearance(Some(&a));
            }
        }
        None => app.setAppearance(None),
    }
}

// The worker→UI state (current `UiState`, last dictation, audio levels) lives
// in `crate::ui_channel`. The menu bar is purely the reader side here.

struct UiGlobals {
    status_item: Retained<NSStatusItem>,
    pill_window: Retained<NSWindow>,
    pill: PillBuild,
    pill_hovered: AtomicBool,
    pill_feedback: Mutex<Option<PillFeedback>>,
    recovery_enabled: bool,
    waveform_sensitivity: WaveformSensitivity,
    show_processing_detail: bool,
    reduced_motion: bool,
    last_state: AtomicU8,
    /// What the status item last showed about health (see
    /// `pill::health_signature`). Most faults arrive and clear without moving
    /// `UiState`, so without this the glyph and tooltip went stale.
    last_health: AtomicU8,
    /// When the poll timer last ran its once-a-second checks
    /// (`CFAbsoluteTime` bits).
    last_slow_check: std::sync::atomic::AtomicU64,
    /// Per-bar smoothed height. Rises instantly to a new peak, decays
    /// slowly toward the new RMS sample — gives the snappy bouncing
    /// feel of real audio meters.
    displayed_heights: Mutex<Vec<f64>>,
    /// The waveform glyph, in the three menu-bar treatments the rule allows:
    /// template ink when idle, `danger`-tinted while recording, and the same
    /// template at 40 % while the pipeline works. One glyph family, so the
    /// status item and the app icon read as one app.
    icon_idle: Option<Retained<NSImage>>,
    icon_recording: Option<Retained<NSImage>>,
    icon_processing: Option<Retained<NSImage>>,
    /// The disabled "Last: …" preview item, refreshed when we return to Idle.
    last_item: Retained<NSMenuItem>,
    /// Held alive so the menu items' action target stays valid.
    _actions: Retained<MenuActions>,
    /// Held alive so the status menu's delegate (which records the pre-menu
    /// frontmost app for recovery paste) is not deallocated. NSMenu keeps its
    /// delegate weakly.
    _menu_delegate: Retained<MenuDelegate>,
    /// Held alive so the application delegate (which releases the models on the
    /// way out — see `AppDelegate`) survives. NSApplication, like NSMenu, keeps
    /// its delegate weakly.
    _app_delegate: Retained<AppDelegate>,
}
unsafe impl Sync for UiGlobals {}
unsafe impl Send for UiGlobals {}

// ─── Flipped container for the history list ─────────────────────────────
//
// NSView's default coordinate origin is bottom-left, which makes a top-down
// list (newest dictation first, at the top, scrolled into view) awkward.
// Overriding `isFlipped` to true puts the origin at the top-left so rows
// stack downward and the scroll view shows the top by default.
define_class!(
    #[unsafe(super(NSView))]
    #[name = "FDFlippedView"]
    struct FlippedView;

    impl FlippedView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

static GLOBALS: OnceLock<UiGlobals> = OnceLock::new();

/// The texts currently shown in the history window, newest first. A clicked
/// row carries its index here as its `tag`, so the click handler can copy the
/// full (untruncated) text even though the row only displays one line.
static HISTORY_ENTRIES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// All entries loaded on the last window open (newest first), unfiltered. The
/// search field filters this master list down to what's shown; kept separate
/// so clearing the query restores the full list without re-querying the DB.
static HISTORY_ALL: Mutex<Vec<Entry>> = Mutex::new(Vec::new());

/// The app and positively-editable field captured just before the status menu
/// opened. The PID remains the fail-closed destination; the retained field is a
/// fallback only if the fresh AX query loses focus while that same app remains
/// frontmost. The action consumes the field, and the next menu open refreshes it.
static RECOVERY_TARGET: Mutex<(Option<i32>, Option<crate::injector::FocusTarget>)> =
    Mutex::new((None, None));

pub fn init_and_run() -> eyre::Result<()> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| eyre::eyre!("menubar::init_and_run must be on main thread"))?;

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    install_main_menu(mtm, &app);

    // Dark is the default; the setting can pin Light or follow the system.
    let settings = Settings::load();
    apply_app_appearance(mtm, &settings.resolve_appearance());

    // Install before anything can quit: the delegate is what releases the
    // models ahead of the process-exit teardown (see `AppDelegate`).
    let app_delegate = AppDelegate::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*app_delegate)));

    let actions = MenuActions::new();
    let built = build_status_item(mtm, &actions)?;
    let pill = build_pill_window(mtm, &actions, settings.resolve_pill_size())?;

    let globals = UiGlobals {
        status_item: built.status_item,
        pill_window: pill.window.clone(),
        pill,
        pill_hovered: AtomicBool::new(false),
        pill_feedback: Mutex::new(None),
        recovery_enabled: settings.resolve_audio_history_enabled(),
        waveform_sensitivity: settings.resolve_waveform_sensitivity(),
        show_processing_detail: settings.resolve_show_processing_detail(),
        reduced_motion: reduced_motion_enabled(),
        last_state: AtomicU8::new(255),
        last_health: AtomicU8::new(255),
        last_slow_check: std::sync::atomic::AtomicU64::new(0),
        displayed_heights: Mutex::new(vec![BAR_MIN_H; BAR_COUNT]),
        icon_idle: built.icon_idle,
        icon_recording: built.icon_recording,
        icon_processing: built.icon_processing,
        last_item: built.last_item,
        _actions: actions,
        _menu_delegate: built.menu_delegate,
        _app_delegate: app_delegate,
    };
    let _ = GLOBALS.set(globals);

    // A launch that cannot dictate opens the checklist by itself. A launch
    // that can never sees it — the item stays in the menu either way.
    if setup::should_open_on_launch(&setup::current_states()) {
        eprintln!("[boot] setup      needed — opening Setup & permissions");
        setup::show_setup_window();
    }

    install_poll_timer();

    // Direct-download builds update themselves through Sparkle 2. This is a
    // no-op outside a bundle that carries an HTTPS feed and the embedded
    // framework, so cask and dev builds are unaffected. Sparkle must be started
    // on the main thread, right before the run loop takes over.
    #[cfg(feature = "sparkle")]
    if crate::updater::start() {
        eprintln!("[boot] updater    Sparkle started");
    }

    app.run();
    Ok(())
}

/// Install a minimal application main menu carrying a standard **Edit** submenu
/// (Undo/Redo/Cut/Copy/Paste/Select All).
///
/// Accessory (`LSUIElement`) apps get no main menu by default. Cocoa delivers
/// the standard text-editing shortcuts — ⌘Z/⌘X/⌘C/⌘V/⌘A — via the Edit menu's
/// *key equivalents*, so with no such menu Copy/Paste/Select-All simply do
/// nothing inside the Dictionary editor's text view (the user's "copy button
/// doesn't work"). Each item leaves its target nil, so the action travels up the
/// responder chain to whatever text view is focused; the menu's default
/// auto-enable then greys items out when they don't apply (e.g. Copy with no
/// selection). The menu is never visible — it exists purely to route the keys.
fn install_main_menu(mtm: MainThreadMarker, app: &NSApplication) {
    let main_menu = NSMenu::new(mtm);

    // Top-level item that owns the Edit submenu.
    let edit_top = NSMenuItem::new(mtm);
    let edit_menu = NSMenu::new(mtm);
    edit_menu.setTitle(&NSString::from_str("Edit"));

    // Standard first-responder editing actions + their conventional shortcuts.
    // Target stays nil → routed up the responder chain to the focused view.
    let edit_actions: &[(&str, objc2::runtime::Sel, &str)] = &[
        ("Undo", sel!(undo:), "z"),
        // Uppercase "Z" implies ⇧ in a key equivalent → ⇧⌘Z (Redo).
        ("Redo", sel!(redo:), "Z"),
        ("Cut", sel!(cut:), "x"),
        ("Copy", sel!(copy:), "c"),
        ("Paste", sel!(paste:), "v"),
        ("Select All", sel!(selectAll:), "a"),
    ];
    for (title, selector, key) in edit_actions {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                Some(*selector),
                &NSString::from_str(key),
            )
        };
        edit_menu.addItem(&item);
    }

    edit_top.setSubmenu(Some(&edit_menu));
    main_menu.addItem(&edit_top);
    app.setMainMenu(Some(&main_menu));
}

#[cfg(test)]
mod tests;
