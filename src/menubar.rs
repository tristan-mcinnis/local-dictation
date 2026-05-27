//! Menu-bar status item + floating waveform pill.
//!
//! UI matches Wispr-Flow / Superwhisper conventions: a small rounded pill
//! at the bottom-center of the cursor's screen, containing a live
//! waveform driven by mic RMS. Hidden when idle.
//!
//! The status-item menu exposes the settings a GUI user would expect —
//! cleanup model, push-to-talk key, cleanup on/off, edit cleanup prompts —
//! plus quality-of-life items (copy last dictation, open/export log,
//! corrections folder).
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

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, AllocAnyThread, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSButton, NSColor,
    NSControlStateValueOff, NSControlStateValueOn, NSEvent, NSFont, NSImage, NSLineBreakMode,
    NSMenu, NSMenuItem, NSScreen, NSScrollView, NSStatusBar, NSStatusItem, NSTextAlignment,
    NSTextField, NSTextView, NSView, NSWindow, NSWindowCollectionBehavior, NSWindowLevel,
    NSWindowStyleMask,
};
use objc2_core_foundation::{
    kCFRunLoopCommonModes, CFAbsoluteTimeGetCurrent, CFRunLoop, CFRunLoopTimer,
    CFRunLoopTimerContext,
};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSDate, NSDateFormatter, NSObject, NSPoint, NSRect, NSSize, NSString,
};

use crate::history::Entry;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::settings::{self, Settings};
use crate::ui_channel::{self, UiState};

const LOG_PATH: &str = "/tmp/dictate-daemon.log";

// Pill geometry — tuned to match Wispr Flow's compact pill.
const PILL_W: f64 = 120.0;
const PILL_H: f64 = 44.0;
const BAR_COUNT: usize = 14;
const BAR_W: f64 = 3.0;
const BAR_GAP: f64 = 2.0;
const BAR_MAX_H: f64 = 26.0;
const BAR_MIN_H: f64 = 2.0;

// The worker→UI state (current `UiState`, last dictation, audio levels) lives
// in `crate::ui_channel`. The menu bar is purely the reader side here.

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
    /// The disabled "Last: …" preview item, refreshed when we return to Idle.
    last_item: Retained<NSMenuItem>,
    /// Held alive so the menu items' action target stays valid.
    _actions: Retained<MenuActions>,
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
            write_settings_and_relaunch(move |set| set.gemma_model = Some(path));
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
    }
);

impl MenuActions {
    fn new() -> Retained<Self> {
        let alloc = Self::alloc();
        unsafe { msg_send![alloc, init] }
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

pub fn init_and_run() -> eyre::Result<()> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| eyre::eyre!("menubar::init_and_run must be on main thread"))?;

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    install_main_menu(mtm, &app);

    let actions = MenuActions::new();
    let built = build_status_item(mtm, &actions)?;
    let (pill_window, bars) = build_pill_window(mtm)?;

    let globals = UiGlobals {
        status_item: built.status_item,
        pill_window,
        bars,
        last_state: AtomicU8::new(255),
        displayed_heights: Mutex::new(vec![BAR_MIN_H; BAR_COUNT]),
        icon_idle: built.icon_idle,
        icon_recording: built.icon_recording,
        icon_processing: built.icon_processing,
        last_item: built.last_item,
        _actions: actions,
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
    unsafe { edit_menu.setTitle(&NSString::from_str("Edit")) };

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
}

/// Build a plain menu item with a title, selector, and key-equivalent,
/// targeting `actions`. Always enabled (we disable auto-enabling on the
/// menu so we control state explicitly).
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
        unsafe { button.setTitle(&NSString::from_str("◯")) };
    }

    let menu = NSMenu::new(mtm);
    // We manage enabled-state ourselves (lets us grey the preview label and
    // any env-locked items).
    unsafe { menu.setAutoenablesItems(false) };

    let settings = Settings::load();
    // Output-format presets: the built-in set (numbered / bullets / email /
    // code) plus any the user added/overrode in prompts.json `formats`. Uses
    // the non-logging loader so building the menu doesn't re-emit the boot
    // summary.
    let format_names = crate::prompts::Prompts::load_quiet().format_names();

    // ── Last dictation preview (disabled label) + Copy ──────────────────
    let last_item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(&last_dictation_label()),
            None,
            &NSString::from_str(""),
        )
    };
    unsafe { last_item.setEnabled(false) };
    menu.addItem(&last_item);

    let copy_last = action_item(mtm, "Copy last dictation", sel!(copyLast:), "c", actions);
    menu.addItem(&copy_last);

    // Browsable history of past dictations — the friendly counterpart to the
    // verbose daemon log.
    let history_item = action_item(mtm, "Dictation History…", sel!(openHistory:), "h", actions);
    menu.addItem(&history_item);

    // Simple editor for the personal dictionary (names/terms to keep spelled
    // verbatim). Counterpart to Dictation History — one clean native window.
    let dictionary_item =
        action_item(mtm, "Dictionary…", sel!(openDictionary:), "d", actions);
    menu.addItem(&dictionary_item);

    menu.addItem(&*unsafe { NSMenuItem::separatorItem(mtm) });

    // ── Cleanup model submenu ───────────────────────────────────────────
    let model_parent = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Cleanup model"),
            None,
            &NSString::from_str(""),
        )
    };
    unsafe { model_parent.setEnabled(true) };
    let model_submenu = build_model_submenu(mtm, actions, &settings);
    unsafe { model_parent.setSubmenu(Some(&model_submenu)) };
    menu.addItem(&model_parent);

    // ── Hotkey submenu ──────────────────────────────────────────────────
    let hotkey_parent = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Push-to-talk key"),
            None,
            &NSString::from_str(""),
        )
    };
    unsafe { hotkey_parent.setEnabled(true) };
    let hotkey_submenu = build_hotkey_submenu(mtm, actions, &settings);
    unsafe { hotkey_parent.setSubmenu(Some(&hotkey_submenu)) };
    menu.addItem(&hotkey_parent);

    // ── Cleanup on/off toggle ───────────────────────────────────────────
    let cleanup_item = action_item(
        mtm,
        "Cleanup enabled",
        sel!(toggleCleanup:),
        "",
        actions,
    );
    let cleanup_on = settings.cleanup_enabled.unwrap_or(true);
    unsafe {
        cleanup_item.setState(if cleanup_on {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        });
    }
    menu.addItem(&cleanup_item);

    // ── Edit the cleanup / transform system prompts ─────────────────────
    // Opens prompts.json in a text editor (seeded with the live prompts the
    // first time, so there's real text to edit). Sits right under the cleanup
    // controls because it's the knob that shapes what cleanup/transform do.
    menu.addItem(&action_item(
        mtm,
        "Edit cleanup prompts…",
        sel!(editPrompts:),
        "",
        actions,
    ));

    // ── Output-format preset submenu ────────────────────────────────────
    // Built-in presets (numbered / bullets / email / code) plus any the user
    // added in prompts.json `formats`; this picks which one cleanup uses.
    // `format_names` is non-empty out of the box, so the submenu always shows.
    if !format_names.is_empty() {
        let format_parent = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Output format"),
                None,
                &NSString::from_str(""),
            )
        };
        unsafe { format_parent.setEnabled(true) };
        let format_submenu = build_format_submenu(mtm, actions, &settings, &format_names);
        unsafe { format_parent.setSubmenu(Some(&format_submenu)) };
        menu.addItem(&format_parent);
    }

    menu.addItem(&*unsafe { NSMenuItem::separatorItem(mtm) });

    // ── Logs + corrections ──────────────────────────────────────────────
    menu.addItem(&action_item(mtm, "Open Log", sel!(openLog:), "l", actions));
    menu.addItem(&action_item(mtm, "Export Log to Downloads…", sel!(exportLog:), "", actions));
    menu.addItem(&action_item(
        mtm,
        "Open corrections folder",
        sel!(openCorrections:),
        "",
        actions,
    ));

    menu.addItem(&*unsafe { NSMenuItem::separatorItem(mtm) });

    // ── Quit (standard NSApp terminate: — works with nil target) ─────────
    let quit_item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Quit local-dictation"),
            Some(sel!(terminate:)),
            &NSString::from_str("q"),
        )
    };
    unsafe { quit_item.setEnabled(true) };
    menu.addItem(&quit_item);

    item.setMenu(Some(&menu));
    Ok(StatusItemBuild {
        status_item: item,
        icon_idle,
        icon_recording,
        icon_processing,
        last_item,
    })
}

/// Build the model-picker submenu. Each discovered model gets a checkable
/// item; the active one is checked. If `GEMMA_MODEL_PATH` is set in the
/// environment it overrides everything, so we surface a disabled note and
/// leave the items unchecked.
fn build_model_submenu(
    mtm: MainThreadMarker,
    actions: &Retained<MenuActions>,
    settings: &Settings,
) -> Retained<NSMenu> {
    let submenu = NSMenu::new(mtm);
    unsafe { submenu.setAutoenablesItems(false) };

    let env_locked = std::env::var_os("GEMMA_MODEL_PATH").is_some();
    let models = settings::discover_llm_models();

    if models.is_empty() {
        let none = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("(no models found under models/llm/)"),
                None,
                &NSString::from_str(""),
            )
        };
        unsafe { none.setEnabled(false) };
        submenu.addItem(&none);
        return submenu;
    }

    // Effective active path: settings choice, or the canonicalized default.
    let default_gemma = crate::app_paths::gemma_default_path();
    let effective = settings.gemma_model.clone().unwrap_or_else(|| {
        std::fs::canonicalize(&default_gemma)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or(default_gemma)
    });

    for choice in &models {
        // Annotate with the speed/accuracy hint so the trade-off is one glance,
        // e.g. "gemma-3-1b-it — fast · recommended".
        let title = match &choice.hint {
            Some(h) => format!("{} — {h}", choice.label),
            None => choice.label.clone(),
        };
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(&title),
                Some(sel!(selectModel:)),
                &NSString::from_str(""),
            )
        };
        unsafe {
            item.setTarget(Some(actions));
            item.setEnabled(!env_locked);
            // Stash the absolute path so the action knows what was picked.
            let path_ns = NSString::from_str(&choice.path);
            let rep: &AnyObject = &path_ns;
            item.setRepresentedObject(Some(rep));
            if !env_locked && choice.path == effective {
                item.setState(NSControlStateValueOn);
            } else {
                item.setState(NSControlStateValueOff);
            }
        }
        submenu.addItem(&item);
    }

    if env_locked {
        submenu.addItem(&*unsafe { NSMenuItem::separatorItem(mtm) });
        let note = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("(locked by GEMMA_MODEL_PATH env var)"),
                None,
                &NSString::from_str(""),
            )
        };
        unsafe { note.setEnabled(false) };
        submenu.addItem(&note);
    }

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
    unsafe { submenu.setAutoenablesItems(false) };

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
    submenu.addItem(&*unsafe { NSMenuItem::separatorItem(mtm) });

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
        submenu.addItem(&*unsafe { NSMenuItem::separatorItem(mtm) });
        let note = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("(locked by DICTATE_FORMAT env var)"),
                None,
                &NSString::from_str(""),
            )
        };
        unsafe { note.setEnabled(false) };
        submenu.addItem(&note);
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
    unsafe { submenu.setAutoenablesItems(false) };

    let env_locked = std::env::var_os("DICTATE_HOTKEY_KEYCODE").is_some();
    let active = settings.hotkey_keycode.unwrap_or(0x3D); // default Right Option

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
        submenu.addItem(&*unsafe { NSMenuItem::separatorItem(mtm) });
        let note = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("(locked by DICTATE_HOTKEY_KEYCODE env var)"),
                None,
                &NSString::from_str(""),
            )
        };
        unsafe { note.setEnabled(false) };
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
fn relaunch_daemon() {
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

fn export_log_to_downloads() {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    if !std::path::Path::new(LOG_PATH).exists() {
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dest = PathBuf::from(home)
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

fn open_corrections_folder() {
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
fn open_prompts_file() {
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
            eprintln!("[menu] failed to seed prompts.json ({}): {e}", path.display());
            // Still try to open whatever exists / the folder below.
        }
    }

    // `open -t` opens in the default text editor (TextEdit), matching Open Log.
    let _ = std::process::Command::new("open")
        .arg("-t")
        .arg(&path)
        .status();
}

// ─── Dictionary editor window ───────────────────────────────────────────
//
// A small native window mirroring Dictation History: one editable text view,
// one entry per line, plus a Save button. It edits the user's single vocabulary
// list (corrections.json): a bare word means "keep this spelling", and
// "from → to" is a mishearing fix. Save writes corrections.json and relaunches
// the daemon so it takes effect — same apply-on-relaunch model as the settings.

struct DictionaryUi {
    window: Retained<NSWindow>,
    text_view: Retained<NSTextView>,
    save_button: Retained<NSButton>,
    /// "Heard" field of the quick-add row (the mis-heard spelling; blank = keep).
    heard_field: Retained<NSTextField>,
    /// "Correct to" field of the quick-add row (the spelling you want).
    correct_field: Retained<NSTextField>,
    add_button: Retained<NSButton>,
}
// Only ever touched on the main thread (menu actions), so parking the AppKit
// pointers in a static is safe.
unsafe impl Send for DictionaryUi {}
unsafe impl Sync for DictionaryUi {}

static DICTIONARY_UI: OnceLock<DictionaryUi> = OnceLock::new();

fn dictionary_ui(mtm: MainThreadMarker) -> &'static DictionaryUi {
    DICTIONARY_UI.get_or_init(|| build_dictionary_window(mtm))
}

fn build_dictionary_window(mtm: MainThreadMarker) -> DictionaryUi {
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
    let container: Retained<NSView> =
        unsafe { NSView::initWithFrame(NSView::alloc(mtm), frame) };

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
        let _: () = msg_send![&*label, setTextColor: &*NSColor::secondaryLabelColor()];
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
    let content = unsafe { scroll.contentSize() };
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

    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable;
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
fn show_dictionary_window(actions: &MenuActions) {
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
    unsafe {
        ui.text_view.setString(&NSString::from_str(&joined));
        ui.window.makeKeyAndOrderFront(None);
    }
    // Accessory app: explicitly activate so the window comes to the front.
    let app = NSApplication::sharedApplication(mtm);
    #[allow(deprecated)]
    unsafe {
        app.activateIgnoringOtherApps(true)
    };
}

/// Read the editor, write dictionary.json, and relaunch so it takes effect.
fn save_dictionary_from_view() {
    let Some(ui) = DICTIONARY_UI.get() else {
        return;
    };
    let text = unsafe { ui.text_view.string() }.to_string();
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
fn add_dictionary_entry_from_fields() {
    let Some(ui) = DICTIONARY_UI.get() else {
        return;
    };
    let heard = unsafe { ui.heard_field.stringValue() }.to_string();
    let correct = unsafe { ui.correct_field.stringValue() }.to_string();

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
        let _: () = msg_send![&*ui.text_view, scrollToEndOfDocument: std::ptr::null_mut::<AnyObject>()];

        // Clear the fields and return focus to "Heard" for the next entry.
        ui.heard_field.setStringValue(&NSString::from_str(""));
        ui.correct_field.setStringValue(&NSString::from_str(""));
        let msg = NSString::from_str("Added — click Save to apply.");
        let _: () = msg_send![&*ui.window, setSubtitle: &*msg];
        ui.window.makeFirstResponder(Some(&*ui.heard_field));
    }
}

// ─── Dictation history window ───────────────────────────────────────────
//
// A plain, small native window: a scrollable list of past dictations grouped
// by day. Each dictation is a borderless NSButton — click it to copy the full
// text to the clipboard (the window subtitle confirms what was copied). The
// row shows one truncated line; the full text lives in the button's tooltip
// and is what gets copied. No WebView, no HTML — just AppKit views, built once
// and reused; every open re-queries the DB and rebuilds the rows.

struct HistoryUi {
    window: Retained<NSWindow>,
    scroll: Retained<NSScrollView>,
    /// Flipped document view that holds the day-header labels and per-entry
    /// buttons. Rebuilt from scratch on every open.
    doc: Retained<FlippedView>,
}
// Only ever touched on the main thread (created from / used by menu actions),
// so the raw AppKit pointers are safe to park in a static.
unsafe impl Send for HistoryUi {}
unsafe impl Sync for HistoryUi {}

static HISTORY_UI: OnceLock<HistoryUi> = OnceLock::new();

fn history_ui(mtm: MainThreadMarker) -> &'static HistoryUi {
    HISTORY_UI.get_or_init(|| build_history_window(mtm))
}

fn build_history_window(mtm: MainThreadMarker) -> HistoryUi {
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(460.0, 580.0));

    let scroll: Retained<NSScrollView> =
        unsafe { msg_send![NSScrollView::alloc(mtm), initWithFrame: frame] };
    unsafe {
        scroll.setHasVerticalScroller(true);
        scroll.setAutohidesScrollers(true);
        scroll.setDrawsBackground(true);
        scroll.setBackgroundColor(&NSColor::controlBackgroundColor());
    }

    // Document view starts the size of the clip area; rebuild_history_list
    // resizes its height to fit the rows.
    let content = unsafe { scroll.contentSize() };
    let doc: Retained<FlippedView> = unsafe {
        msg_send![
            FlippedView::alloc(mtm),
            initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), content)
        ]
    };
    unsafe { scroll.setDocumentView(Some(&*doc)) };

    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable;
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
        window.setTitle(&NSString::from_str("Dictation History"));
        // Titlebar hint at what clicking does (refreshed to confirm on copy).
        let hint = NSString::from_str("Click any entry to copy it");
        let _: () = msg_send![&*window, setSubtitle: &*hint];
        // Reused across opens — closing must hide, not deallocate it.
        window.setReleasedWhenClosed(false);
        window.setContentView(Some(&*scroll));
        window.center();
    }

    HistoryUi { window, scroll, doc }
}

/// Re-query the history DB, rebuild the list, and bring the window forward.
fn show_history_window() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let ui = history_ui(mtm);
    let entries = crate::history::recent(1000);

    // Stash the full texts, newest first, so a clicked row (carrying its index
    // as its tag) can copy the untruncated text.
    if let Ok(mut store) = HISTORY_ENTRIES.lock() {
        *store = entries.iter().map(|e| e.text.clone()).collect();
    }
    rebuild_history_list(mtm, ui, &entries);

    // Reset the subtitle hint for the fresh open.
    unsafe {
        let hint = NSString::from_str("Click any entry to copy it");
        let _: () = msg_send![&*ui.window, setSubtitle: &*hint];
        ui.window.makeKeyAndOrderFront(None);
    }
    // We're an Accessory app (no Dock icon); without an explicit activate the
    // new window opens behind the frontmost app.
    let app = NSApplication::sharedApplication(mtm);
    #[allow(deprecated)]
    unsafe {
        app.activateIgnoringOtherApps(true)
    };
}

/// Copy the stored history text at `tag` (its index in HISTORY_ENTRIES) to the
/// clipboard and confirm via the window subtitle. Out-of-range/empty are no-ops.
fn copy_history_entry_at(tag: isize) {
    if tag < 0 {
        return;
    }
    let text = HISTORY_ENTRIES
        .lock()
        .ok()
        .and_then(|g| g.get(tag as usize).cloned());
    let Some(text) = text else { return };
    if text.trim().is_empty() {
        return;
    }
    copy_to_clipboard(&text);

    // Confirm in the titlebar: "Copied “<short preview>…”".
    if let Some(ui) = HISTORY_UI.get() {
        let flat = text.replace('\n', " ");
        let preview: String = flat.chars().take(42).collect();
        let ellipsis = if flat.chars().count() > 42 { "…" } else { "" };
        let msg = format!("Copied “{preview}{ellipsis}”");
        unsafe {
            let s = NSString::from_str(&msg);
            let _: () = msg_send![&*ui.window, setSubtitle: &*s];
        }
    }
}

/// One rendered row: either a day header or a single dictation. `index` is the
/// entry's position in the (newest-first) input — the same index used as the
/// button's `tag` and as the key into HISTORY_ENTRIES.
#[derive(Debug)]
enum HistoryRow {
    Day(String),
    Item {
        time: String,
        text: String,
        index: usize,
    },
}

/// Group entries by local calendar day (newest first) into a flat row list.
/// `NSDateFormatter` renders dates/times in the user's locale + timezone.
fn history_rows(entries: &[Entry]) -> Vec<HistoryRow> {
    let day_fmt = NSDateFormatter::new();
    day_fmt.setDateFormat(Some(&NSString::from_str("MMMM d, yyyy")));
    let time_fmt = NSDateFormatter::new();
    time_fmt.setDateFormat(Some(&NSString::from_str("hh:mm a")));

    let mut rows = Vec::new();
    let mut last_day: Option<String> = None;
    for (i, e) in entries.iter().enumerate() {
        let date = unsafe { NSDate::dateWithTimeIntervalSince1970(e.created_at as f64) };
        let day = day_fmt.stringFromDate(&date).to_string().to_uppercase();
        let time = time_fmt.stringFromDate(&date).to_string();
        if last_day.as_deref() != Some(day.as_str()) {
            rows.push(HistoryRow::Day(day.clone()));
            last_day = Some(day);
        }
        // Collapse internal newlines so each dictation stays on one row.
        let text = e.text.replace('\n', " ");
        rows.push(HistoryRow::Item {
            time,
            text,
            index: i,
        });
    }
    rows
}

/// Tear down and rebuild the day-grouped list of clickable rows.
fn rebuild_history_list(mtm: MainThreadMarker, ui: &HistoryUi, entries: &[Entry]) {
    const MX: f64 = 14.0; // horizontal margin
    const TOP: f64 = 10.0; // top padding
    const ROW_H: f64 = 22.0; // one dictation row
    const HEADER_H: f64 = 28.0; // one day header
    const DAY_GAP: f64 = 8.0; // extra space above each day after the first

    let doc = &ui.doc;
    let content = unsafe { ui.scroll.contentSize() };
    let width = content.width.max(200.0);
    let row_w = width - 2.0 * MX;

    // Clear previous rows.
    let empty: Retained<NSArray<NSView>> = NSArray::new();
    unsafe { doc.setSubviews(&empty) };

    if entries.is_empty() {
        let label = NSTextField::labelWithString(
            &NSString::from_str("No dictations yet — hold your hotkey and speak."),
            mtm,
        );
        label.setFrame(NSRect::new(
            NSPoint::new(MX, TOP + 6.0),
            NSSize::new(row_w, 20.0),
        ));
        unsafe { label.setTextColor(Some(&NSColor::secondaryLabelColor())) };
        unsafe { doc.addSubview(&label) };
        doc.setFrame(NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(width, content.height.max(60.0)),
        ));
        return;
    }

    // The shared action target whose copyHistoryEntry: handles row clicks.
    let actions = GLOBALS.get().map(|g| &g._actions);

    let mut y = TOP;
    for row in history_rows(entries) {
        match row {
            HistoryRow::Day(day) => {
                if y > TOP {
                    y += DAY_GAP;
                }
                let label = NSTextField::labelWithString(&NSString::from_str(&day), mtm);
                label.setFrame(NSRect::new(NSPoint::new(MX, y), NSSize::new(row_w, 18.0)));
                label.setFont(Some(&NSFont::boldSystemFontOfSize(11.0)));
                unsafe { label.setTextColor(Some(&NSColor::secondaryLabelColor())) };
                unsafe { doc.addSubview(&label) };
                y += HEADER_H;
            }
            HistoryRow::Item { time, text, index } => {
                let title = format!("{time}   {text}");
                let btn: Retained<NSButton> = unsafe {
                    msg_send![
                        NSButton::alloc(mtm),
                        initWithFrame: NSRect::new(NSPoint::new(MX, y), NSSize::new(row_w, ROW_H))
                    ]
                };
                btn.setTitle(&NSString::from_str(&title));
                btn.setBordered(false);
                btn.setAlignment(NSTextAlignment::Left);
                if let Some(font) = NSFont::userFixedPitchFontOfSize(12.0) {
                    btn.setFont(Some(&font));
                }
                // Single line, truncate the tail; the full text is the tooltip
                // and the copy payload.
                if let Some(cell) = btn.cell() {
                    cell.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
                }
                let full = entries
                    .get(index)
                    .map(|e| e.text.as_str())
                    .unwrap_or(text.as_str());
                btn.setToolTip(Some(&NSString::from_str(full)));
                unsafe {
                    if let Some(a) = actions {
                        let _: () = msg_send![&*btn, setTarget: &**a];
                        let _: () = msg_send![&*btn, setAction: sel!(copyHistoryEntry:)];
                    }
                    let _: () = msg_send![&*btn, setTag: index as isize];
                    doc.addSubview(&btn);
                }
                y += ROW_H;
            }
        }
    }

    let total = y + TOP;
    doc.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(width, total.max(content.height)),
    ));
    // Flipped view: (0,0) is the top, so this shows the newest entries.
    unsafe { doc.scrollPoint(NSPoint::new(0.0, 0.0)) };
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
            UiState::Recording => update_bars(globals),
            UiState::Processing => animate_processing_bars(globals),
            UiState::Idle => {}
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
            // A dictation just completed (or we were cancelled) — refresh the
            // "Last: …" preview so the menu shows the newest text.
            unsafe {
                globals
                    .last_item
                    .setTitle(&NSString::from_str(&last_dictation_label()));
            }
            ui_channel::reset_levels();
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
fn animate_processing_bars(globals: &UiGlobals) {
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

/// Position bar `i` at height `h`, vertically centered within the pill.
fn set_bar_height(bar: &NSView, i: usize, h: f64) {
    let bar_block_w = BAR_COUNT as f64 * BAR_W + (BAR_COUNT - 1) as f64 * BAR_GAP;
    let x = (PILL_W - bar_block_w) / 2.0 + i as f64 * (BAR_W + BAR_GAP);
    let y = (PILL_H - h) / 2.0;
    let frame = NSRect::new(NSPoint::new(x, y), NSSize::new(BAR_W, h));
    unsafe { bar.setFrame(frame) };
}

fn collapse_bars(globals: &UiGlobals) {
    if let Ok(mut g) = globals.displayed_heights.lock() {
        for v in g.iter_mut() {
            *v = BAR_MIN_H;
        }
    }
    for (i, bar) in globals.bars.iter().enumerate() {
        set_bar_height(bar, i, BAR_MIN_H);
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
            Entry { text: "second day line".into(), created_at: 1_747_900_000 },
            Entry { text: "first day line".into(), created_at: 1_747_900_000 - 90_000 },
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
