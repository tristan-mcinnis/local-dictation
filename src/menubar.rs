//! Menu-bar status item + floating waveform pill.
//!
//! UI matches Wispr-Flow / Superwhisper conventions: a small rounded pill
//! at the bottom-center of the cursor's screen, containing a live
//! waveform driven by mic RMS. Hidden when idle.
//!
//! The status-item menu exposes the settings a GUI user would expect —
//! cleanup model, push-to-talk key, cleanup on/off — plus quality-of-life
//! items (copy last dictation, open/export log, corrections folder).
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
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSColor,
    NSControlStateValueOff, NSControlStateValueOn, NSEvent, NSImage, NSMenu, NSMenuItem,
    NSScreen, NSStatusBar, NSStatusItem, NSView, NSWindow, NSWindowCollectionBehavior,
    NSWindowLevel, NSWindowStyleMask,
};
use objc2_core_foundation::{
    kCFRunLoopCommonModes, CFAbsoluteTimeGetCurrent, CFRunLoop, CFRunLoopTimer,
    CFRunLoopTimerContext,
};
use objc2_foundation::{MainThreadMarker, NSObject, NSPoint, NSRect, NSSize, NSString};
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::settings::{self, Settings};

const LOG_PATH: &str = "/tmp/dictate-daemon.log";

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

/// The last successfully-injected text, for the "Copy last dictation" item.
static LAST_DICTATION: Mutex<String> = Mutex::new(String::new());

pub fn set_state(state: UiState) {
    SHARED_STATE.store(state as u8, Ordering::SeqCst);
}

/// Record the most recent injected text (called by the worker thread).
pub fn set_last_dictation(text: &str) {
    if let Ok(mut g) = LAST_DICTATION.lock() {
        *g = text.to_string();
    }
}

fn last_dictation() -> String {
    LAST_DICTATION.lock().map(|g| g.clone()).unwrap_or_default()
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
            let text = last_dictation();
            if !text.is_empty() {
                copy_to_clipboard(&text);
            }
        }

        #[unsafe(method(openCorrections:))]
        fn open_corrections(&self, _sender: *mut AnyObject) {
            open_corrections_folder();
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
    }
);

impl MenuActions {
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
    let effective = settings.gemma_model.clone().unwrap_or_else(|| {
        std::fs::canonicalize(settings::DEFAULT_GEMMA_REL)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| settings::DEFAULT_GEMMA_REL.to_string())
    });

    for choice in &models {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(&choice.label),
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
    let text = last_dictation();
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
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let dir = PathBuf::from(home).join(".config").join("local-dictation");
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
            // A dictation just completed (or we were cancelled) — refresh the
            // "Last: …" preview so the menu shows the newest text.
            unsafe {
                globals
                    .last_item
                    .setTitle(&NSString::from_str(&last_dictation_label()));
            }
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
