//! Menu-bar status item + floating waveform pill.
//!
//! UI matches Wispr-Flow / Superwhisper conventions: a small rounded pill
//! at the bottom-center of the cursor's screen, containing a live
//! waveform driven by mic RMS. Hidden when idle.
//!
//! The status-item menu is recovery-first and minimal at the top level: the
//! last dictation (preview + Copy + an explicit "Paste last dictation" that
//! re-inserts the already-captured text into the app that was frontmost before
//! the menu opened — never a re-transcribe), Settings, a single "More" submenu
//! that groups history / retranscribe / config / diagnostics, and Quit.
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
mod pill;
mod render_ui;
pub(crate) mod slate;
use dictionary::*;
use history::*;
use pill::*;
pub use render_ui::render_ui;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, sel, AllocAnyThread, MainThreadOnly};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSBezierPath,
    NSBitmapImageFileType, NSBitmapImageRep, NSButton, NSColor, NSControl, NSControlStateValueOff,
    NSControlStateValueOn, NSEvent, NSFont, NSFontAttributeName, NSForegroundColorAttributeName,
    NSGraphicsContext, NSImage, NSImageSymbolConfiguration, NSImageView, NSKernAttributeName, NSLineBreakMode, NSMenu,
    NSMenuDelegate, NSMenuItem, NSScreen, NSScrollView, NSSearchField, NSShadow, NSStatusBar, NSStatusItem,
    NSTextAlignment, NSTextField, NSTextView, NSTrackingArea, NSView, NSVisualEffectBlendingMode,
    NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView, NSWindow,
    NSWindowCollectionBehavior, NSWindowLevel, NSWindowStyleMask, NSWindowTitleVisibility,
};
use objc2_core_foundation::{
    kCFRunLoopCommonModes, CFAbsoluteTimeGetCurrent, CFRunLoop, CFRunLoopTimer,
    CFRunLoopTimerContext,
};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSDate, NSDateFormatter, NSMutableAttributedString, NSNumber,
    NSObject, NSPoint, NSRange, NSRect, NSSize, NSString,
};

use crate::history::Entry;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::settings::{self, Settings};
use crate::ui_channel::{self, UiState};

use crate::app_paths::DAEMON_LOG_PATH as LOG_PATH;

// ─── Slate geometry ──────────────────────────────────────────────────────
//
// Every number here is either a house token or a value derived from one. The
// pill is `CONTROL_PILL` high at `RADIUS_PILL`, and its waveform sits in a
// sunken well; the bar block width falls out of the bar count, width and gap.

/// House palette (design-system/tokens.json via the generated `house_theme.rs`).
/// Both appearances live in the file; `slate::palette()` picks one per draw and
/// `theme::*` hands out dynamic colours for the parts AppKit draws itself. No
/// RGBA literal lives outside the generated file.
#[path = "house_theme.rs"]
pub(crate) mod house_theme;

use house_theme::{
    CONTROL_CHIP, CONTROL_FOOTER, CONTROL_INPUT, CONTROL_KEYCAP, CONTROL_PILL, CONTROL_ROW, CONTROL_STATUSGLYPH, CONTROL_TILE, HAIRLINE, LAYOUT_PANEL_WIDTH, MOTION_HOVER, RADIUS_LG, RADIUS_MD, RADIUS_PILL,
    RADIUS_ROW, RADIUS_SM, RADIUS_TILE, RADIUS_XL, RADIUS_XS, SHADOW_SELECTION, SPACE_LG, SPACE_MD,
    SPACE_SM, SPACE_XS, SPACE_XXL, SPACE_XXS, TRACKING_SECTION, TYPE_BODY_SMALL, TYPE_CAPTION, TYPE_INPUT,
    TYPE_KEY_CAP, TYPE_META, TYPE_SECTION, WEIGHT_MEDIUM, WEIGHT_REGULAR,
    WEIGHT_SEMIBOLD,
};

/// The pill is `CONTROL_PILL` tall at `RADIUS_PILL`.
const PILL_H: f64 = CONTROL_PILL;
/// Horizontal padding between the pill's edge and the waveform well.
const PILL_PAD: f64 = SPACE_XXS + 2.0;
/// Padding between the pill edge and a text/dot run (the status states).
const PILL_TEXT_PAD: f64 = SPACE_SM + 2.0;
/// Gap between items on the hover shelf and in the status states.
const PILL_GAP: f64 = SPACE_XS + 2.0;
/// The sunken well the waveform sits in.
const WELL_H: f64 = CONTROL_TILE - 2.0;
const WELL_PAD: f64 = SPACE_XS + 2.0;
const WELL_RADIUS: f64 = RADIUS_LG;

const BAR_COUNT: usize = 14;
const BAR_W: f64 = 3.0;
const BAR_GAP: f64 = 2.0;
/// Tallest bar. The well is `WELL_H` high, so the peak sits just inside it.
const BAR_MAX_H: f64 = 14.0;
const BAR_MIN_H: f64 = 2.0;

/// Compact pill width: two paddings, the well, and the bar block inside it.
const PILL_W: f64 = 2.0 * PILL_PAD + 2.0 * WELL_PAD + BAR_BLOCK_W;
const BAR_BLOCK_W: f64 = BAR_COUNT as f64 * BAR_W + (BAR_COUNT as f64 - 1.0) * BAR_GAP;

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
    /// 0 = compact, 1 = expanded. The timer eases this value so hover can be
    /// interrupted without a second animation system or a bounce.
    pill_expanded: Mutex<f64>,
    reduced_motion: bool,
    last_state: AtomicU8,
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
}
unsafe impl Sync for UiGlobals {}
unsafe impl Send for UiGlobals {}

// ─── Custom NSObject subclass holding all menu actions ──────────────────
//
// NSMenuItem dispatches its action via an objc selector to its target. We
// route every dynamic menu item to one instance of this class.
define_class!(
    #[unsafe(super(NSObject))]
    #[name = "FDMenuActions"]
    pub struct MenuActions;

    impl MenuActions {
        #[unsafe(method(openLog:))]
        fn open_log(&self, _sender: *mut AnyObject) {
            let _ = std::process::Command::new("open")
                .args(["-t", LOG_PATH])
                .status();
        }

        #[unsafe(method(exportLog:))]
        fn export_log(&self, _sender: *mut AnyObject) {
            export_log_to_downloads();
        }

        #[unsafe(method(copyLast:))]
        fn copy_last(&self, _sender: *mut AnyObject) {
            let text = ui_channel::last_dictation();
            if !text.is_empty() {
                copy_to_clipboard(&text);
            }
        }

        #[unsafe(method(openHistory:))]
        fn open_history(&self, _sender: *mut AnyObject) {
            show_history_window();
        }

        // A row in the history window was clicked. Its `tag` indexes into
        // HISTORY_ENTRIES (the texts shown, newest first); copy the full,
        // untruncated text and confirm via the window subtitle.
        #[unsafe(method(copyHistoryEntry:))]
        fn copy_history_entry(&self, sender: *mut AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            copy_history_entry_at(tag);
        }

        // The history search field changed. Re-filter the cached entries by
        // case-insensitive substring and rebuild the list. Fires per keystroke
        // (the field is configured to send its string immediately).
        #[unsafe(method(searchHistory:))]
        fn search_history(&self, sender: *mut AnyObject) {
            let field: &NSSearchField = unsafe { &*(sender as *const NSSearchField) };
            let query = field.stringValue().to_string();
            apply_history_filter(&query);
        }

        #[unsafe(method(openCorrections:))]
        fn open_corrections(&self, _sender: *mut AnyObject) {
            open_corrections_folder();
        }

        #[unsafe(method(openDictionary:))]
        fn open_dictionary(&self, _sender: *mut AnyObject) {
            show_dictionary_window(self);
        }

        #[unsafe(method(saveDictionary:))]
        fn save_dictionary(&self, _sender: *mut AnyObject) {
            save_dictionary_from_view();
        }

        #[unsafe(method(addDictionaryEntry:))]
        fn add_dictionary_entry(&self, _sender: *mut AnyObject) {
            add_dictionary_entry_from_fields();
        }

        #[unsafe(method(editPrompts:))]
        fn edit_prompts(&self, _sender: *mut AnyObject) {
            open_prompts_file();
        }

        #[unsafe(method(selectModel:))]
        fn select_model(&self, sender: *mut AnyObject) {
            let path = unsafe {
                let obj: *mut AnyObject = msg_send![sender, representedObject];
                if obj.is_null() {
                    return;
                }
                let s: &NSString = &*(obj as *const NSString);
                s.to_string()
            };
            write_settings_and_relaunch(move |set| set.llm_model = Some(path));
        }

        #[unsafe(method(selectHotkey:))]
        fn select_hotkey(&self, sender: *mut AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            write_settings_and_relaunch(move |set| set.hotkey_keycode = Some(tag as i64));
        }

        #[unsafe(method(toggleCleanup:))]
        fn toggle_cleanup(&self, _sender: *mut AnyObject) {
            let current = Settings::load().cleanup_enabled.unwrap_or(true);
            write_settings_and_relaunch(move |set| set.cleanup_enabled = Some(!current));
        }

        #[unsafe(method(togglePreroll:))]
        fn toggle_preroll(&self, _sender: *mut AnyObject) {
            // On ⇒ keep the mic warm with DEFAULT_PREROLL_MS of lookback so the
            // first words aren't clipped; off ⇒ open-on-press (0 ms).
            let on = Settings::load().preroll_ms.unwrap_or(0) > 0;
            write_settings_and_relaunch(move |set| {
                set.preroll_ms = Some(if on { 0 } else { crate::audio::DEFAULT_PREROLL_MS });
            });
        }

        #[unsafe(method(selectFormat:))]
        fn select_format(&self, sender: *mut AnyObject) {
            // representedObject holds the preset name; the empty string means
            // "Default (no preset)", which clears the setting.
            let name = unsafe {
                let obj: *mut AnyObject = msg_send![sender, representedObject];
                if obj.is_null() {
                    String::new()
                } else {
                    let s: &NSString = &*(obj as *const NSString);
                    s.to_string()
                }
            };
            write_settings_and_relaunch(move |set| {
                set.active_format = if name.is_empty() { None } else { Some(name.clone()) };
            });
        }

        #[unsafe(method(openCommandsRef:))]
        fn open_commands_ref(&self, _sender: *mut AnyObject) {
            write_and_open_commands_reference();
        }

        #[unsafe(method(openSettings:))]
        fn open_settings(&self, _sender: *mut AnyObject) {
            crate::settings_window::show_settings_window();
        }

        /// The expanded pill's compact, keyboard-reachable escape hatch. It
        /// uses the same local control channel as the CLI/Shortcuts path, so
        /// cancelling never reaches into the audio worker from AppKit.
        #[unsafe(method(cancelPill:))]
        fn cancel_pill(&self, _sender: *mut AnyObject) {
            let _ = crate::ipc::send(crate::ipc::Command::Cancel);
        }

        // Re-run the pipeline on the last utterance's audio at the current
        // cursor. No-op when nothing has been dictated this session.
        #[unsafe(method(retryLast:))]
        fn retry_last(&self, _sender: *mut AnyObject) {
            if let Err(e) = crate::ipc::send(crate::ipc::Command::Retry) {
                eprintln!("[menu] retry last dictation failed: {e}");
            }
        }

        // Recovery paste: re-insert the last dictation text into the app that
        // was frontmost before this menu opened. This is NOT a re-transcribe —
        // it uses the already-captured text, restoring + verifying the target
        // app (see `crate::recovery`). No-op/logs when there is nothing to
        // paste or the target is unsafe (self / unknown / no editable field).
        #[unsafe(method(pasteLast:))]
        fn paste_last(&self, _sender: *mut AnyObject) {
            let recorded = RECOVERY_TARGET_PID.lock().ok().and_then(|g| *g);
            crate::recovery::paste_last_dictation(recorded);
        }

        // Pick the active Translate-mode target. representedObject holds the
        // target name; the translate path reads settings per utterance, so a
        // save (no relaunch) is enough for the change to take effect.
        #[unsafe(method(selectTranslateTarget:))]
        fn select_translate_target(&self, sender: *mut AnyObject) {
            let target = unsafe {
                let obj: *mut AnyObject = msg_send![sender, representedObject];
                if obj.is_null() {
                    return;
                }
                let s: &NSString = &*(obj as *const NSString);
                s.to_string()
            };
            if target.is_empty() {
                return;
            }
            let mut s = Settings::load();
            s.active_translation_target = Some(target);
            if let Err(e) = s.save() {
                eprintln!("[menu] failed to save translate target: {e}");
            }
        }

        // Pick the history-retention window. representedObject holds the
        // canonical string ("forever" / "1year" / …); history reads retention
        // per record, so a save (no relaunch) is enough.
        #[unsafe(method(selectRetention:))]
        fn select_retention(&self, sender: *mut AnyObject) {
            let rep = unsafe {
                let obj: *mut AnyObject = msg_send![sender, representedObject];
                if obj.is_null() {
                    return;
                }
                let s: &NSString = &*(obj as *const NSString);
                s.to_string()
            };
            if rep.is_empty() {
                return;
            }
            let mut s = Settings::load();
            s.history_retention = Some(rep);
            if let Err(e) = s.save() {
                eprintln!("[menu] failed to save history retention: {e}");
            }
        }

        // Import a .csv of vocabulary entries into corrections.json and refresh
        // the dictionary window. Backs the "Import Dictionary CSV…" menu item.
        #[unsafe(method(importCsv:))]
        fn import_csv(&self, _sender: *mut AnyObject) {
            import_dictionary_csv();
        }
    }
);

impl MenuActions {
    fn new() -> Retained<Self> {
        let alloc = Self::alloc();
        unsafe { msg_send![alloc, init] }
    }
}

// ─── Menu delegate: record the pre-menu frontmost app ──────────────────
//
// The recovery paste needs the app that was frontmost BEFORE the user opened
// the menu. This delegate's `menuWillOpen:` fires exactly when the status menu
// is about to show — i.e. right after the click but before any menu row is
// chosen — so it is the earliest reliable place to snapshot the foreign app.
// The value is stored in a process-wide static (the delegate is main-thread
// only and the action dispatch is main-thread too, so a Mutex keeps them in
// sync without plumbing a handle between the two objects).
define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "FDMenuDelegate"]
    struct MenuDelegate;

    impl MenuDelegate {}

    unsafe impl NSObjectProtocol for MenuDelegate {}

    unsafe impl NSMenuDelegate for MenuDelegate {
        #[unsafe(method(menuWillOpen:))]
        fn menu_will_open(&self, _menu: &NSMenu) {
            // Snapshot the app that was frontmost when the menu opened; it is
            // the recovery-paste target (the guard refuses Local Dictation
            // itself / an unknown target).
            let pid = crate::injector::frontmost_pid();
            if let Ok(mut g) = RECOVERY_TARGET_PID.lock() {
                *g = pid;
            }
        }
    }
);

impl MenuDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm);
        unsafe { msg_send![this, init] }
    }
}

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

/// The app (PID) that was frontmost just before the status menu opened. This is
/// the recovery-paste target — the foreign app the user was dictating into.
/// Written by the menu delegate's `menuWillOpen:`, read by the
/// "Paste last dictation" action. `None` = nothing recorded yet (or the recorded
/// app was Local Dictation itself, which the guard refuses).
static RECOVERY_TARGET_PID: Mutex<Option<i32>> = Mutex::new(None);

pub fn init_and_run() -> eyre::Result<()> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| eyre::eyre!("menubar::init_and_run must be on main thread"))?;

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    install_main_menu(mtm, &app);

    // Dark is the default; the setting can pin Light or follow the system.
    apply_app_appearance(mtm, &Settings::load().resolve_appearance());

    let actions = MenuActions::new();
    let built = build_status_item(mtm, &actions)?;
    let pill = build_pill_window(mtm, &actions)?;

    let globals = UiGlobals {
        status_item: built.status_item,
        pill_window: pill.window.clone(),
        pill,
        pill_expanded: Mutex::new(0.0),
        reduced_motion: reduced_motion_enabled(),
        last_state: AtomicU8::new(255),
        displayed_heights: Mutex::new(vec![BAR_MIN_H; BAR_COUNT]),
        icon_idle: built.icon_idle,
        icon_recording: built.icon_recording,
        icon_processing: built.icon_processing,
        last_item: built.last_item,
        _actions: actions,
        _menu_delegate: built.menu_delegate,
    };
    let _ = GLOBALS.set(globals);

    install_poll_timer();
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

struct StatusItemBuild {
    status_item: Retained<NSStatusItem>,
    icon_idle: Option<Retained<NSImage>>,
    icon_recording: Option<Retained<NSImage>>,
    icon_processing: Option<Retained<NSImage>>,
    last_item: Retained<NSMenuItem>,
    menu_delegate: Retained<MenuDelegate>,
}

/// Build a plain menu item with a title, selector, and key-equivalent,
/// targeting `actions`. Always enabled (we disable auto-enabling on the
/// menu so we control state explicitly).
/// Generate the live voice-commands + settings reference and open it in the
/// default text editor. Generated from the actual command tables + settings so
/// it never drifts. Backs the menu's "Voice Commands & Shortcuts…" item.
pub(crate) fn write_and_open_commands_reference() {
    let s = Settings::load();
    let hotkey = settings::hotkey_name(s.hotkey_keycode.unwrap_or(crate::hotkeys::DEFAULT_KEYCODE));

    let mut body = String::new();
    body.push_str("Local Dictation — Voice Commands & Settings\n");
    body.push_str("===========================================\n\n");
    body.push_str(&crate::voice_commands::reference_text());
    body.push_str(&format!(
        "\n\nPUSH-TO-TALK KEY: {hotkey} (hold, speak, release)\n"
    ));
    body.push_str(
        "OUTPUT FORMATS (menu → Output format): default · numbered · bullets · email · code\n",
    );

    let Some(path) = crate::app_paths::config_file("voice-commands.txt") else {
        eprintln!("[menu] no config dir for commands reference");
        return;
    };
    if let Err(e) = std::fs::write(&path, body) {
        eprintln!("[menu] failed to write commands reference: {e}");
        return;
    }
    let _ = std::process::Command::new("open").arg(&path).status();
}

fn action_item(
    mtm: MainThreadMarker,
    title: &str,
    selector: objc2::runtime::Sel,
    key: &str,
    actions: &Retained<MenuActions>,
) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(selector),
            &NSString::from_str(key),
        )
    };
    unsafe {
        item.setTarget(Some(actions));
        item.setEnabled(true);
    }
    item
}

fn build_status_item(
    mtm: MainThreadMarker,
    actions: &Retained<MenuActions>,
) -> eyre::Result<StatusItemBuild> {
    let bar = NSStatusBar::systemStatusBar();
    let item = bar.statusItemWithLength(-1.0);
    let button = item
        .button(mtm)
        .ok_or_else(|| eyre::eyre!("status item has no button"))?;

    // One glyph family — the waveform, matching the app icon, so the icon and
    // the status item read as one app. Idle and processing are the template
    // image (it adopts the menu-bar tint); recording is the same shape drawn
    // once in `danger`, the only chroma the rule allows on chrome.
    let icon_idle = sf_symbol("waveform");
    let icon_recording =
        sf_symbol_tinted("waveform", &theme::danger()).or_else(|| sf_symbol("waveform"));
    let icon_processing = sf_symbol("waveform");
    if let Some(img) = &icon_idle {
        button.setImage(Some(img));
        button.setTitle(&NSString::from_str(""));
    } else {
        button.setTitle(&NSString::from_str("◯"));
    }

    let menu = NSMenu::new(mtm);
    // We manage enabled-state ourselves (lets us grey the preview label and
    // any env-locked items).
    menu.setAutoenablesItems(false);

    // Menu delegate: records the app that was frontmost just before this menu
    // opened — the recovery-paste target. Retained by the caller so it is not
    // freed (NSMenu keeps its delegate weakly).
    let menu_delegate = MenuDelegate::new(mtm);
    let delegate_proto = ProtocolObject::from_retained(menu_delegate.clone());
    menu.setDelegate(Some(&*delegate_proto));

    let settings = Settings::load();
    // Output-format presets: the built-in set (numbered / bullets / email /
    // code) plus any the user added/overrode in prompts.json `formats`. Uses
    // the non-logging loader so building the menu doesn't re-emit the boot
    // summary.
    let format_names = crate::prompts::Prompts::load_quiet().format_names();

    // ── Recovery: disabled preview + copy + explicit paste ──────────────
    let last_item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(&last_dictation_label()),
            None,
            &NSString::from_str(""),
        )
    };
    last_item.setEnabled(false);
    menu.addItem(&last_item);

    let copy_last = action_item(mtm, "Copy last dictation", sel!(copyLast:), "c", actions);
    menu.addItem(&copy_last);

    // Explicit recovery: paste the already-captured last dictation into the
    // app that was frontmost before this menu opened. This is NOT a
    // re-transcribe — it does not touch audio or the pipeline, only re-inserts
    // the text (restoring + verifying the target; see `crate::recovery`). The
    // injector's focus gate still applies — never a blind paste.
    let paste_last = action_item(mtm, "Paste last dictation", sel!(pasteLast:), "", actions);
    menu.addItem(&paste_last);

    menu.addItem(&*NSMenuItem::separatorItem(mtm));

    // ── Settings + More + Quit ─────────────────────────────────────────
    // The top level stays minimal: the recoverable last dictation (preview,
    // copy, paste), Settings, a single "More" that gathers everything else, and
    // Quit. Model choices are intentionally absent (this personal build is one
    // warmed Cohere MLX transcriber followed by S1-mini).
    let settings_item = action_item(mtm, "Settings…", sel!(openSettings:), ",", actions);
    menu.addItem(&settings_item);

    let more_parent = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("More"),
            None,
            &NSString::from_str(""),
        )
    };
    more_parent.setEnabled(true);
    let more_submenu = build_more_submenu(mtm, actions, &settings, &format_names);
    more_parent.setSubmenu(Some(&more_submenu));
    menu.addItem(&more_parent);

    menu.addItem(&*NSMenuItem::separatorItem(mtm));

    // ── Quit (standard NSApp terminate: — works with nil target) ─────────
    let quit_item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Quit local-dictation"),
            Some(sel!(terminate:)),
            &NSString::from_str("q"),
        )
    };
    quit_item.setEnabled(true);
    menu.addItem(&quit_item);

    item.setMenu(Some(&menu));
    Ok(StatusItemBuild {
        status_item: item,
        icon_idle,
        icon_recording,
        icon_processing,
        last_item,
        menu_delegate,
    })
}

/// Build the "More" submenu: history · retranscribe · config · diagnostics.
/// This keeps the previously-flat rows reachable while the top level stays
/// minimal (last dictation, Settings, More, Quit). Pure NSMenu grouping with
/// separators for the four groups — no new visual system.
fn build_more_submenu(
    mtm: MainThreadMarker,
    actions: &Retained<MenuActions>,
    settings: &Settings,
    format_names: &[String],
) -> Retained<NSMenu> {
    let submenu = NSMenu::new(mtm);
    submenu.setAutoenablesItems(false);

    // ── History ─────────────────────────────────────────────────────────
    // Browsable history of past dictations — the friendly counterpart to the
    // verbose daemon log.
    submenu.addItem(&action_item(mtm, "Dictation History…", sel!(openHistory:), "h", actions));

    // How long to keep dictation history (read per record, so no relaunch).
    let retention_parent = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("History retention"),
            None,
            &NSString::from_str(""),
        )
    };
    retention_parent.setEnabled(true);
    let retention_submenu = build_retention_submenu(mtm, actions, settings);
    retention_parent.setSubmenu(Some(&retention_submenu));
    submenu.addItem(&retention_parent);

    submenu.addItem(&*NSMenuItem::separatorItem(mtm));

    // ── Retranscribe ─────────────────────────────────────────────────────
    // Re-run the last recording (including a cancelled one): the pipeline
    // replays the retained in-memory audio and injects the fresh transcript at
    // the current cursor, obeying the same Escape + focus guard.
    submenu.addItem(&action_item(mtm, "Retry last recording", sel!(retryLast:), "r", actions));

    // Pick the active Translate-mode target (only meaningful when targets are
    // configured; reads settings per utterance, so no relaunch).
    let translate_parent = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Translate to"),
            None,
            &NSString::from_str(""),
        )
    };
    translate_parent.setEnabled(true);
    let translate_submenu = build_translate_submenu(mtm, actions, settings);
    translate_parent.setSubmenu(Some(&translate_submenu));
    submenu.addItem(&translate_parent);

    // Output-format preset picker. `format_names` is non-empty out of the box.
    if !format_names.is_empty() {
        let format_parent = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Output format"),
                None,
                &NSString::from_str(""),
            )
        };
        format_parent.setEnabled(true);
        let format_submenu = build_format_submenu(mtm, actions, settings, format_names);
        format_parent.setSubmenu(Some(&format_submenu));
        submenu.addItem(&format_parent);
    }

    submenu.addItem(&*NSMenuItem::separatorItem(mtm));

    // ── Config ───────────────────────────────────────────────────────────
    let hotkey_parent = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Push-to-talk key"),
            None,
            &NSString::from_str(""),
        )
    };
    hotkey_parent.setEnabled(true);
    let hotkey_submenu = build_hotkey_submenu(mtm, actions, settings);
    hotkey_parent.setSubmenu(Some(&hotkey_submenu));
    submenu.addItem(&hotkey_parent);

    let cleanup_item = action_item(mtm, "Cleanup enabled", sel!(toggleCleanup:), "", actions);
    let cleanup_on = settings.cleanup_enabled.unwrap_or(true);
    cleanup_item.setState(if cleanup_on {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    submenu.addItem(&cleanup_item);

    // Keeps the mic stream warm and retains a short lookback so the first
    // words aren't clipped and there's no stream-open latency on key-press.
    let preroll_item = action_item(
        mtm,
        "Always-on mic (pre-roll)",
        sel!(togglePreroll:),
        "",
        actions,
    );
    let preroll_on = settings.preroll_ms.unwrap_or(0) > 0;
    preroll_item.setState(if preroll_on {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    submenu.addItem(&preroll_item);

    // Opens prompts.json in a text editor (seeded with the live prompts the
    // first time, so there's real text to edit).
    submenu.addItem(&action_item(mtm, "Edit cleanup prompts…", sel!(editPrompts:), "", actions));

    // Simple editor for the personal dictionary (names/terms to keep spelled
    // verbatim).
    submenu.addItem(&action_item(mtm, "Dictionary…", sel!(openDictionary:), "d", actions));

    // Import a .csv of vocabulary entries into the personal dictionary.
    submenu.addItem(&action_item(mtm, "Import Dictionary CSV…", sel!(importCsv:), "", actions));

    submenu.addItem(&*NSMenuItem::separatorItem(mtm));

    // ── Diagnostics ──────────────────────────────────────────────────────
    submenu.addItem(&action_item(mtm, "Open Log", sel!(openLog:), "l", actions));
    submenu.addItem(&action_item(mtm, "Export Log to Downloads…", sel!(exportLog:), "", actions));
    submenu.addItem(&action_item(mtm, "Open corrections folder", sel!(openCorrections:), "", actions));
    // A clear, always-current reference of every voice command + the wake-word
    // and shortcut settings (generated from the live tables, opened as text).
    submenu.addItem(&action_item(mtm, "Voice Commands & Shortcuts…", sel!(openCommandsRef:), "", actions));

    submenu
}

/// Build the output-format picker. A "Default (no preset)" row clears the
/// active format; each defined preset gets a checkable row. The active one is
/// checked. `DICTATE_FORMAT` env locks the picker (disabled + note), mirroring
/// the model / hotkey submenus.
fn build_format_submenu(
    mtm: MainThreadMarker,
    actions: &Retained<MenuActions>,
    settings: &Settings,
    names: &[String],
) -> Retained<NSMenu> {
    let submenu = NSMenu::new(mtm);
    submenu.setAutoenablesItems(false);

    let env_locked = std::env::var_os("DICTATE_FORMAT").is_some();
    let active = settings.active_format.clone().unwrap_or_default();

    // "Default (no preset)" — empty representedObject clears the setting.
    let default_item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Default (no preset)"),
            Some(sel!(selectFormat:)),
            &NSString::from_str(""),
        )
    };
    unsafe {
        default_item.setTarget(Some(actions));
        default_item.setEnabled(!env_locked);
        let rep_ns = NSString::from_str("");
        let rep: &AnyObject = &rep_ns;
        default_item.setRepresentedObject(Some(rep));
        default_item.setState(if !env_locked && active.trim().is_empty() {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        });
    }
    submenu.addItem(&default_item);
    submenu.addItem(&*NSMenuItem::separatorItem(mtm));

    for name in names {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(name),
                Some(sel!(selectFormat:)),
                &NSString::from_str(""),
            )
        };
        unsafe {
            item.setTarget(Some(actions));
            item.setEnabled(!env_locked);
            let rep_ns = NSString::from_str(name);
            let rep: &AnyObject = &rep_ns;
            item.setRepresentedObject(Some(rep));
            item.setState(if !env_locked && active.eq_ignore_ascii_case(name) {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
        }
        submenu.addItem(&item);
    }

    if env_locked {
        submenu.addItem(&*NSMenuItem::separatorItem(mtm));
        let note = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("(locked by DICTATE_FORMAT env var)"),
                None,
                &NSString::from_str(""),
            )
        };
        note.setEnabled(false);
        submenu.addItem(&note);
    }

    submenu
}

/// Build the Translate-to picker. Each configured target is a checkable row
/// (the active one checked); clicking saves the active target without a
/// relaunch (the translate path reads settings per utterance). When no targets
/// are configured, the submenu carries a single disabled note pointing at the
/// Settings window.
fn build_translate_submenu(
    mtm: MainThreadMarker,
    actions: &Retained<MenuActions>,
    settings: &Settings,
) -> Retained<NSMenu> {
    let submenu = NSMenu::new(mtm);
    submenu.setAutoenablesItems(false);

    let targets = settings.resolve_translation_targets();
    if targets.is_empty() {
        let note = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("(add targets in Settings → Translate to)"),
                None,
                &NSString::from_str(""),
            )
        };
        note.setEnabled(false);
        submenu.addItem(&note);
        return submenu;
    }

    let active = settings.resolve_active_translation_target(&targets);
    for target in &targets {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(target),
                Some(sel!(selectTranslateTarget:)),
                &NSString::from_str(""),
            )
        };
        unsafe {
            item.setTarget(Some(actions));
            item.setEnabled(true);
            let rep_ns = NSString::from_str(target);
            let rep: &AnyObject = &rep_ns;
            item.setRepresentedObject(Some(rep));
            let is_active = active
                .as_deref()
                .map(|a| a.eq_ignore_ascii_case(target))
                .unwrap_or(false);
            item.setState(if is_active {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
        }
        submenu.addItem(&item);
    }

    submenu
}

/// Build the history-retention picker. Each window is a checkable row (the
/// active one checked); clicking saves the window without a relaunch (history
/// reads retention per record).
fn build_retention_submenu(
    mtm: MainThreadMarker,
    actions: &Retained<MenuActions>,
    settings: &Settings,
) -> Retained<NSMenu> {
    let submenu = NSMenu::new(mtm);
    submenu.setAutoenablesItems(false);

    let active = settings.resolve_history_retention();
    const CHOICES: &[(&str, &str)] = &[
        ("Forever", "forever"),
        ("1 year", "1year"),
        ("1 month", "1month"),
        ("1 week", "1week"),
        ("24 hours", "24hours"),
        ("Never", "never"),
    ];
    for (label, rep) in CHOICES {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(label),
                Some(sel!(selectRetention:)),
                &NSString::from_str(""),
            )
        };
        unsafe {
            item.setTarget(Some(actions));
            item.setEnabled(true);
            let rep_ns = NSString::from_str(rep);
            let rep_obj: &AnyObject = &rep_ns;
            item.setRepresentedObject(Some(rep_obj));
            item.setState(if active.eq_ignore_ascii_case(rep) {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
        }
        submenu.addItem(&item);
    }

    submenu
}

/// Build the push-to-talk key submenu from `settings::HOTKEY_CHOICES`.
fn build_hotkey_submenu(
    mtm: MainThreadMarker,
    actions: &Retained<MenuActions>,
    settings: &Settings,
) -> Retained<NSMenu> {
    let submenu = NSMenu::new(mtm);
    submenu.setAutoenablesItems(false);

    let env_locked = std::env::var_os("DICTATE_HOTKEY_KEYCODE").is_some();
    let active = settings.hotkey_keycode.unwrap_or(crate::hotkeys::DEFAULT_KEYCODE);

    for (label, code) in settings::HOTKEY_CHOICES {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(label),
                Some(sel!(selectHotkey:)),
                &NSString::from_str(""),
            )
        };
        unsafe {
            item.setTarget(Some(actions));
            item.setEnabled(!env_locked);
            item.setTag(*code as isize);
            if !env_locked && *code == active {
                item.setState(NSControlStateValueOn);
            } else {
                item.setState(NSControlStateValueOff);
            }
        }
        submenu.addItem(&item);
    }

    if env_locked {
        submenu.addItem(&*NSMenuItem::separatorItem(mtm));
        let note = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("(locked by DICTATE_HOTKEY_KEYCODE env var)"),
                None,
                &NSString::from_str(""),
            )
        };
        note.setEnabled(false);
        submenu.addItem(&note);
    }

    submenu
}

/// Title for the disabled preview item, truncated for the menu.
fn last_dictation_label() -> String {
    let text = ui_channel::last_dictation();
    if text.is_empty() {
        return "No dictation yet".to_string();
    }
    let one_line = text.replace('\n', " ");
    let truncated: String = one_line.chars().take(48).collect();
    if one_line.chars().count() > 48 {
        format!("Last: \"{truncated}…\"")
    } else {
        format!("Last: \"{truncated}\"")
    }
}

// ─── Settings mutation + daemon relaunch ────────────────────────────────

/// Load settings, apply `mutate`, save, then relaunch the daemon so the
/// change takes effect. Runs on the main thread (inside a menu action).
fn write_settings_and_relaunch(mutate: impl FnOnce(&mut Settings)) {
    let mut s = Settings::load();
    mutate(&mut s);
    if let Err(e) = s.save() {
        eprintln!("[menu] failed to save settings: {e}");
        return;
    }
    relaunch_daemon();
}

/// Spawn a fresh daemon (same binary, `daemon` subcommand) with stdout+stderr
/// pointed at the log file — matching how the daemon is normally launched so
/// Open/Export Log keep working — then terminate ourselves.
pub(crate) fn relaunch_daemon() {
    use std::process::{Command, Stdio};

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[menu] current_exe failed, not relaunching: {e}");
            return;
        }
    };

    let open_log = || {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(LOG_PATH)
    };
    let (stdout, stderr) = match (open_log(), open_log()) {
        (Ok(a), Ok(b)) => (Stdio::from(a), Stdio::from(b)),
        _ => (Stdio::inherit(), Stdio::inherit()),
    };

    eprintln!("[menu] settings changed — relaunching daemon…");
    match Command::new(&exe)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
    {
        Ok(_) => terminate_app(),
        Err(e) => eprintln!("[menu] relaunch spawn failed: {e}"),
    }
}

fn terminate_app() {
    if let Some(mtm) = MainThreadMarker::new() {
        let app = NSApplication::sharedApplication(mtm);
        app.terminate(None);
    }
}

// ─── Menu action helpers (plain Rust, no AppKit) ────────────────────────

fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    if let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}

pub(crate) fn export_log_to_downloads() {
    let Some(home) = crate::app_paths::home_dir() else {
        return;
    };
    if !std::path::Path::new(LOG_PATH).exists() {
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dest = home
        .join("Downloads")
        .join(format!("dictate-log-{ts}.txt"));
    if std::fs::copy(LOG_PATH, &dest).is_ok() {
        // Reveal it in Finder so the user sees where it landed.
        let _ = std::process::Command::new("open")
            .arg("-R")
            .arg(&dest)
            .status();
    }
}

pub(crate) fn open_corrections_folder() {
    let Some(dir) = crate::app_paths::config_dir() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let corrections = dir.join("corrections.json");
    if corrections.exists() {
        let _ = std::process::Command::new("open")
            .arg("-R")
            .arg(&corrections)
            .status();
    } else {
        let _ = std::process::Command::new("open").arg(&dir).status();
    }
}

/// Import entries from a user-picked CSV file into the personal vocabulary
/// (corrections.json), then refresh the dictionary window's editor text and
/// confirm via the window subtitle.
///
/// The native file dialog is driven through AppleScript's `choose file` (which
/// presents the same macOS open panel) because `NSOpenPanel` isn't compiled
/// into this build — its `objc2-app-kit` cargo feature is not enabled, and
/// adding it would mean editing Cargo.toml, which is out of scope for this UI
/// change. A CSV is matched by its standard UTI (`public.comma-separated-
/// values-text`) with a `public.text` fallback for pickers that don't register
/// the specific type.
fn import_dictionary_csv() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };

    let out = std::process::Command::new("osascript")
        .args([
            "-e",
            "POSIX path of (choose file of type {\"public.comma-separated-values-text\", \"public.text\"} with prompt \"Choose a dictionary CSV to import\")",
        ])
        .output();
    let Ok(out) = out else {
        eprintln!("[menu] import CSV: osascript failed to launch");
        return;
    };
    if !out.status.success() {
        // User dismissed the dialog (AppleScript returns -128 on cancel) or
        // the script errored. Nothing was chosen, so there's nothing to do.
        eprintln!("[menu] import CSV cancelled or failed (osascript exit {:?})", out.status.code());
        return;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if path.is_empty() {
        return;
    }
    let path = PathBuf::from(path);

    let mut corrections = match crate::corrections::Corrections::load_default() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[menu] import CSV: failed to load corrections: {e}");
            return;
        }
    };
    let added = match corrections.import_csv_path(&path) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("[menu] import CSV: failed to read {}: {e}", path.display());
            return;
        }
    };
    if let Err(e) = corrections.save() {
        eprintln!("[menu] import CSV: failed to save corrections: {e}");
        return;
    }
    eprintln!("[menu] imported {added} term(s) from {}", path.display());

    // Refresh the dictionary window's editor text from the merged vocabulary so
    // the imported entries appear inline, and bring it forward. (We reuse the
    // build-once window; corrections are read fresh from disk here.
    let ui = dictionary_ui(mtm);
    let joined = crate::corrections::Corrections::load_default()
        .map(|c| c.to_editor_lines())
        .unwrap_or_default()
        .join("\n");
    ui.text_view.setString(&NSString::from_str(&joined));
    unsafe {
        let msg = NSString::from_str(&format!("Imported {added} term(s)"));
        let _: () = msg_send![&*ui.window, setSubtitle: &*msg];
    }
    ui.window.makeKeyAndOrderFront(None);
    let app = NSApplication::sharedApplication(mtm);
    #[allow(deprecated)]
    {
        app.activateIgnoringOtherApps(true)
    };
}

/// Open the user's `prompts.json` (cleanup + transform system prompts) in a
/// text editor. The file is optional — the daemon falls back to built-in
/// defaults when it's missing — so the first time it doesn't exist we seed it
/// with the *currently active* prompts plus inline `_`-comments. That way the
/// user edits real, working text instead of staring at a blank file, and the
/// seeded values round-trip through `Prompts::load()` unchanged.
///
/// Edits take effect on the next daemon launch (prompts are read once at
/// startup), so the comment tells the user to relaunch — e.g. toggle "Cleanup
/// enabled" off and on, which relaunches the daemon.
pub(crate) fn open_prompts_file() {
    let Some(path) = crate::prompts::Prompts::config_path() else {
        eprintln!("[menu] cannot resolve prompts.json path (no $HOME?)");
        return;
    };

    if !path.exists() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let prompts = crate::prompts::Prompts::load();
        let esc = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string());
        let seed = format!(
            "{{\n  \"_comment\": {},\n\n  \"_cleanup\": {},\n  \"cleanup\": {},\n\n  \"_transform\": {},\n  \"transform\": {}\n}}\n",
            esc(
                "Edit the two system prompts below, then save. Changes take effect on the \
                 next daemon launch — toggle \u{201C}Cleanup enabled\u{201D} off and on \
                 (or quit and relaunch the daemon) to reload them. Blank or delete a field \
                 to restore that prompt\u{2019}s built-in default. \u{201C}_\u{201D}-prefixed \
                 keys are comments and are ignored."
            ),
            esc(
                "Always-on cleanup: tidies every normal dictation. Keep it conservative — \
                 its job is to clean up speech-to-text, not to rewrite or summarize."
            ),
            esc(&prompts.cleanup),
            esc(
                "Transform mode (Shift + push-to-talk): you select text, speak an \
                 instruction, and the model rewrites the selection in place."
            ),
            esc(&prompts.transform),
        );
        if let Err(e) = std::fs::write(&path, seed) {
            eprintln!(
                "[menu] failed to seed prompts.json ({}): {e}",
                path.display()
            );
            // Still try to open whatever exists / the folder below.
        }
    }

    // `open -t` opens in the default text editor (TextEdit), matching Open Log.
    let _ = std::process::Command::new("open")
        .arg("-t")
        .arg(&path)
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_empty() {
        assert!(history_rows(&[]).is_empty());
    }

    #[test]
    fn rows_group_by_day_newest_first() {
        // Two entries ~a day apart. We assert structure (a header per day, item
        // ordering and indices) without pinning exact times, since formatting
        // is locale/timezone dependent.
        let entries = vec![
            Entry {
                text: "second day line".into(),
                created_at: 1_747_900_000,
            },
            Entry {
                text: "first day line".into(),
                created_at: 1_747_900_000 - 90_000,
            },
        ];
        let rows = history_rows(&entries);
        eprintln!("\n----- history_rows -----\n{rows:#?}\n------------------------");

        // Two distinct day headers (different calendar days).
        let headers = rows
            .iter()
            .filter(|r| matches!(r, HistoryRow::Day(_)))
            .count();
        assert_eq!(headers, 2, "expected one header per day, got {headers}");

        // Items appear newest-first, carrying their original entry index, and
        // each row's text matches its slot.
        let items: Vec<(usize, &str)> = rows
            .iter()
            .filter_map(|r| match r {
                HistoryRow::Item { index, text, .. } => Some((*index, text.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(items, vec![(0, "second day line"), (1, "first day line")]);
    }
}
