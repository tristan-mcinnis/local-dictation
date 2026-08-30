//! Native AppKit Settings window.
//!
//! A single scrollable window with grouped sections (Dictation, Output, Speech,
//! Experimental, Files & Logs, About) built from the same AppKit primitives the
//! Dictionary/History windows use — `NSScrollView` + a flipped `NSView`
//! document, manual frame layout, system fonts/colours. No SwiftUI, no second
//! binary: this is the "idiomatic AppKit in Rust" route (option C), giving the
//! System-Settings feel using the exact frameworks SwiftUI wraps.
//!
//! Edit model: controls edit an in-memory `pending` copy of `Settings`; nothing
//! is written until the user clicks **Apply & Restart**, which saves once and
//! relaunches the daemon (the model load makes in-process hot-swap not worth
//! the complexity — same apply-on-restart contract as the menu submenus, just
//! batched so a window session with several edits costs one restart, not N).
//! Closing without applying keeps the edits in-memory for the window's life but
//! never writes them. Env-var locks (`DICTATE_*`) disable the matching control
//! and show a footnote, mirroring the menu's "(locked by … env var)" notes.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, AllocAnyThread, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSButton, NSColor, NSControlStateValueOff,
    NSControlStateValueOn, NSFont, NSPopUpButton, NSScrollView, NSSwitch, NSTextField, NSView,
    NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{MainThreadMarker, NSObject, NSPoint, NSRect, NSSize, NSString};

use crate::menubar;
use crate::settings::{self, Settings};
use crate::transcriber::AsrBackend;

use std::sync::{Mutex, OnceLock};

const LOG_PATH: &str = "/tmp/dictate-daemon.log";

// Window + layout geometry. The window is fixed-size (manual layout); the form
// scrolls if a section grows past the visible height.
const WIN_W: f64 = 480.0;
const WIN_H: f64 = 560.0;
const BOTTOM_BAR_H: f64 = 44.0;
const PAD: f64 = 20.0;
const LABEL_W: f64 = 196.0;
const CONTROL_X: f64 = PAD + LABEL_W + 8.0;
const CONTROL_W: f64 = WIN_W - CONTROL_X - PAD;
const ROW_H: f64 = 30.0;

// Control fields are held for ownership/future refresh even when not read after
// build (the view hierarchy already retains them); suppress the dead-code lint.
#[allow(dead_code)]
struct SettingsUi {
    window: Retained<NSWindow>,
    _scroll: Retained<NSScrollView>,
    #[allow(dead_code)]
    container: Retained<SettingsFlippedView>,
    // General
    hotkey_popup: Retained<NSPopUpButton>,
    cleanup_switch: Retained<NSSwitch>,
    preroll_switch: Retained<NSSwitch>,
    format_popup: Retained<NSPopUpButton>,
    // Speech
    backend_popup: Retained<NSPopUpButton>,
    cohere_quant_popup: Retained<NSPopUpButton>,
    cohere_lang_popup: Retained<NSPopUpButton>,
    moonshine_popup: Retained<NSPopUpButton>,
    model_popup: Retained<NSPopUpButton>,
    // Advanced
    streaming_switch: Retained<NSSwitch>,
    listen_switch: Retained<NSSwitch>,
    wake_field: Retained<NSTextField>,
    /// In-memory edits, not yet written. Updated by every control action;
    /// flushed by `Apply & Restart`.
    pending: Mutex<Settings>,
    /// Snapshot of the file at build time, so `Apply` can no-op (just close)
    /// when nothing actually changed — avoids a pointless restart.
    original: Settings,
    /// Held alive so control action targets stay valid for the window's life.
    _controller: Retained<SettingsController>,
}
unsafe impl Send for SettingsUi {}
unsafe impl Sync for SettingsUi {}

static SETTINGS_UI: OnceLock<SettingsUi> = OnceLock::new();

// ─── Flipped container (top-left origin) for top-down row layout ──────────
define_class!(
    #[unsafe(super(NSView))]
    #[name = "FDSettingsFlippedView"]
    struct SettingsFlippedView;

    impl SettingsFlippedView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

// ─── Action target for every control in the window ───────────────────────
//
// One NSObject subclass receives every control's action. Each method reads the
// sender, updates `pending` (no restart), and — for the backend picker —
// refreshes the dependent Cohere/Moonshine field enablement. `applyAndRestart:`
// flushes. The file/log buttons call straight through to the menubar helpers.
define_class!(
    #[unsafe(super(NSObject))]
    #[name = "FDSettingsController"]
    pub struct SettingsController;

    impl SettingsController {
        #[unsafe(method(changeHotkey:))]
        fn change_hotkey(&self, sender: *mut AnyObject) {
            let code = popup_selected_tag(sender);
            with_pending(|p| p.hotkey_keycode = Some(code as i64));
        }

        #[unsafe(method(changeCleanup:))]
        fn change_cleanup(&self, sender: *mut AnyObject) {
            let on = switch_on(sender);
            with_pending(|p| p.cleanup_enabled = Some(on));
        }

        #[unsafe(method(changePreroll:))]
        fn change_preroll(&self, sender: *mut AnyObject) {
            let on = switch_on(sender);
            with_pending(|p| {
                p.preroll_ms = Some(if on { crate::audio::DEFAULT_PREROLL_MS } else { 0 });
            });
        }

        #[unsafe(method(changeFormat:))]
        fn change_format(&self, sender: *mut AnyObject) {
            let name = popup_selected_rep(sender);
            with_pending(|p| {
                p.active_format = if name.is_empty() { None } else { Some(name) };
            });
        }

        #[unsafe(method(changeBackend:))]
        fn change_backend(&self, sender: *mut AnyObject) {
            let name = popup_selected_rep(sender);
            with_pending(|p| p.asr_backend = if name.is_empty() { None } else { Some(name) });
            if let Some(ui) = SETTINGS_UI.get() {
                refresh_backend_dependents(ui);
            }
        }

        #[unsafe(method(changeCohereQuant:))]
        fn change_cohere_quant(&self, sender: *mut AnyObject) {
            let v = popup_selected_rep(sender);
            with_pending(|p| p.cohere_quant = if v.is_empty() { None } else { Some(v) });
        }

        #[unsafe(method(changeCohereLang:))]
        fn change_cohere_lang(&self, sender: *mut AnyObject) {
            let v = popup_selected_rep(sender);
            with_pending(|p| p.cohere_language = if v.is_empty() { None } else { Some(v) });
        }

        #[unsafe(method(changeMoonshine:))]
        fn change_moonshine(&self, sender: *mut AnyObject) {
            let v = popup_selected_rep(sender);
            with_pending(|p| p.moonshine_variant = if v.is_empty() { None } else { Some(v) });
        }

        #[unsafe(method(changeModel:))]
        fn change_model(&self, sender: *mut AnyObject) {
            let path = popup_selected_rep(sender);
            with_pending(|p| p.gemma_model = if path.is_empty() { None } else { Some(path) });
        }

        #[unsafe(method(changeStreaming:))]
        fn change_streaming(&self, sender: *mut AnyObject) {
            let on = switch_on(sender);
            with_pending(|p| p.streaming_cleanup = Some(on));
        }

        #[unsafe(method(changeListen:))]
        fn change_listen(&self, sender: *mut AnyObject) {
            let on = switch_on(sender);
            with_pending(|p| p.listen_mode = Some(on));
        }

        #[unsafe(method(changeWakeWord:))]
        fn change_wake_word(&self, sender: *mut AnyObject) {
            let s = field_string(sender);
            let t = s.trim().to_string();
            with_pending(|p| {
                p.wake_word = if t.is_empty() { None } else { Some(t) };
            });
        }

        #[unsafe(method(applyAndRestart:))]
        fn apply_and_restart(&self, _sender: *mut AnyObject) {
            if let Some(ui) = SETTINGS_UI.get() {
                apply_and_restart(ui);
            }
        }

        #[unsafe(method(editPrompts:))]
        fn edit_prompts(&self, _sender: *mut AnyObject) {
            menubar::open_prompts_file();
        }

        #[unsafe(method(openCorrections:))]
        fn open_corrections(&self, _sender: *mut AnyObject) {
            menubar::open_corrections_folder();
        }

        #[unsafe(method(openCommandsRef:))]
        fn open_commands_ref(&self, _sender: *mut AnyObject) {
            menubar::write_and_open_commands_reference();
        }

        #[unsafe(method(openLog:))]
        fn open_log(&self, _sender: *mut AnyObject) {
            let _ = std::process::Command::new("open").args(["-t", LOG_PATH]).status();
        }

        #[unsafe(method(exportLog:))]
        fn export_log(&self, _sender: *mut AnyObject) {
            menubar::export_log_to_downloads();
        }
    }
);

impl SettingsController {
    fn new() -> Retained<Self> {
        unsafe { msg_send![Self::alloc(), init] }
    }
}

/// Show the settings window, building it on first open. Idempotent across
/// reopens within a process (the window is reused).
pub fn show_settings_window() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let ui = SETTINGS_UI.get_or_init(|| build_settings_window(mtm));
    ui.window.makeKeyAndOrderFront(None);
    let app = NSApplication::sharedApplication(mtm);
    #[allow(deprecated)]
    {
        app.activateIgnoringOtherApps(true)
    };
}

fn build_settings_window(mtm: MainThreadMarker) -> SettingsUi {
    let controller = SettingsController::new();
    let loaded = Settings::load();

    // ── Shell: window + scroll view + bottom bar ────────────────────────
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIN_W, WIN_H));
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Titled
                | NSWindowStyleMask::Closable
                | NSWindowStyleMask::Miniaturizable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe {
        window.setTitle(&NSString::from_str("Local Dictation Settings"));
        window.setReleasedWhenClosed(false);
    }
    menubar::apply_light_chrome(&window);

    let content: Retained<NSView> = NSView::initWithFrame(NSView::alloc(mtm), frame);

    let scroll: Retained<NSScrollView> = unsafe {
        msg_send![
            NSScrollView::alloc(mtm),
            initWithFrame: NSRect::new(
                NSPoint::new(0.0, BOTTOM_BAR_H),
                NSSize::new(WIN_W, WIN_H - BOTTOM_BAR_H),
            )
        ]
    };
    scroll.setHasVerticalScroller(true);
    scroll.setAutohidesScrollers(true);
    scroll.setDrawsBackground(false);

    let container: Retained<SettingsFlippedView> = unsafe {
        msg_send![
            SettingsFlippedView::alloc(mtm),
            initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIN_W, 1000.0))
        ]
    };

    // Bottom bar: hint (left) + Apply & Restart (right).
    let apply_w = 130.0;
    let apply_h = 28.0;
    let apply_btn: Retained<NSButton> = unsafe {
        msg_send![
            NSButton::alloc(mtm),
            initWithFrame: NSRect::new(
                NSPoint::new(WIN_W - PAD - apply_w, (BOTTOM_BAR_H - apply_h) / 2.0),
                NSSize::new(apply_w, apply_h),
            )
        ]
    };
    unsafe {
        apply_btn.setTitle(&NSString::from_str("Apply & Restart"));
        let _: () = msg_send![&*apply_btn, setBezelStyle: 1usize]; // rounded
        apply_btn.setTarget(Some(&controller));
        apply_btn.setAction(Some(sel!(applyAndRestart:)));
    }
    let hint = label(
        mtm,
        "Changes take effect after restart",
        NSRect::new(
            NSPoint::new(PAD, (BOTTOM_BAR_H - 16.0) / 2.0 + 2.0),
            NSSize::new(WIN_W - 2.0 * PAD - apply_w - 12.0, 16.0),
        ),
        &NSFont::systemFontOfSize(11.0),
        &NSColor::secondaryLabelColor(),
    );

    // ── Rows ────────────────────────────────────────────────────────────
    let mut y = 0.0_f64;

    y = section(mtm, &container, y, "Dictation");

    // Push-to-talk key
    let hotkey_popup = popup(mtm, &container, y, "Push-to-talk key");
    let hotkey_locked = std::env::var_os("DICTATE_HOTKEY_KEYCODE").is_some();
    let hotkey_active = loaded.resolve_hotkey(0x3D);
    for (label_str, code) in settings::HOTKEY_CHOICES {
        popup_add_item(&hotkey_popup, label_str, Some(*code as isize), None);
    }
    select_popup_tag(&hotkey_popup, hotkey_active);
    wire_popup(
        &hotkey_popup,
        &controller,
        sel!(changeHotkey:),
        hotkey_locked,
    );
    y += ROW_H;
    if hotkey_locked {
        y = env_hint(mtm, &container, y, "DICTATE_HOTKEY_KEYCODE");
    }

    // Cleanup enabled
    let cleanup_switch = switch_row(mtm, &container, y, "Enable cleanup");
    cleanup_switch.setState(if loaded.cleanup_enabled.unwrap_or(true) {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    wire_switch(&cleanup_switch, &controller, sel!(changeCleanup:), false);
    y += ROW_H;

    // Always-on mic (pre-roll)
    let preroll_switch = switch_row(mtm, &container, y, "Always-on mic (pre-roll)");
    let preroll_on = loaded.resolve_preroll_ms() > 0;
    preroll_switch.setState(if preroll_on {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    wire_switch(
        &preroll_switch,
        &controller,
        sel!(changePreroll:),
        std::env::var_os("DICTATE_PREROLL_MS").is_some(),
    );
    y += ROW_H;
    if std::env::var_os("DICTATE_PREROLL_MS").is_some() {
        y = env_hint(mtm, &container, y, "DICTATE_PREROLL_MS");
    }

    y = section(mtm, &container, y, "Output");

    // Output format
    let format_popup = popup(mtm, &container, y, "Output format");
    let format_names = crate::prompts::Prompts::load_quiet().format_names();
    let format_locked = std::env::var_os("DICTATE_FORMAT").is_some();
    popup_add_item(&format_popup, "Default (no preset)", None, Some(""));
    for name in &format_names {
        popup_add_item(&format_popup, name, None, Some(name));
    }
    select_popup_rep(&format_popup, &loaded.resolve_format().unwrap_or_default());
    wire_popup(
        &format_popup,
        &controller,
        sel!(changeFormat:),
        format_locked,
    );
    y += ROW_H;
    if format_locked {
        y = env_hint(mtm, &container, y, "DICTATE_FORMAT");
    }

    y = section(mtm, &container, y, "Speech engine");

    // ASR backend
    let backend_popup = popup(mtm, &container, y, "Speech engine");
    let backend_locked = std::env::var_os("DICTATE_ASR_BACKEND").is_some();
    for (backend, label_str) in [(AsrBackend::Cohere, "Cohere Transcribe — native MLX")] {
        popup_add_item(&backend_popup, label_str, None, Some(backend.as_str()));
    }
    let resolved_asr = loaded.resolve_asr();
    select_popup_rep(&backend_popup, resolved_asr.backend.as_str());
    wire_popup(&backend_popup, &controller, sel!(changeBackend:), true);
    y += ROW_H;
    if backend_locked {
        y = env_hint(mtm, &container, y, "DICTATE_ASR_BACKEND");
    }

    // Cohere quant
    let cohere_quant_popup = popup(mtm, &container, y, "Cohere quality");
    for (v, l) in [("int4", "Q4 — fixed personal checkpoint")] {
        popup_add_item(&cohere_quant_popup, l, None, Some(v));
    }
    select_popup_rep(&cohere_quant_popup, resolved_asr.cohere_quant.as_str());
    wire_popup(
        &cohere_quant_popup,
        &controller,
        sel!(changeCohereQuant:),
        true,
    );
    y += ROW_H;

    // Cohere language
    let cohere_lang_popup = popup(mtm, &container, y, "Cohere language");
    const LANGS: &[(&str, &str)] = &[
        ("en", "English (en)"),
        ("de", "German (de)"),
        ("es", "Spanish (es)"),
        ("fr", "French (fr)"),
        ("it", "Italian (it)"),
        ("pt", "Portuguese (pt)"),
        ("nl", "Dutch (nl)"),
        ("ru", "Russian (ru)"),
        ("zh", "Chinese (zh)"),
        ("ja", "Japanese (ja)"),
        ("ko", "Korean (ko)"),
        ("ar", "Arabic (ar)"),
        ("hi", "Hindi (hi)"),
        ("tr", "Turkish (tr)"),
        ("pl", "Polish (pl)"),
    ];
    for (code, l) in LANGS {
        popup_add_item(&cohere_lang_popup, l, None, Some(code));
    }
    select_popup_rep(&cohere_lang_popup, &resolved_asr.cohere_language);
    wire_popup(
        &cohere_lang_popup,
        &controller,
        sel!(changeCohereLang:),
        std::env::var_os("DICTATE_COHERE_LANG").is_some(),
    );
    y += ROW_H;

    // Native runtime is fixed; retain the control object only to keep the
    // settings window's stable ownership layout.
    let moonshine_popup = popup(mtm, &container, y, "Inference runtime");
    for (v, l) in [("base", "MLX helper — persistent and warmed")] {
        popup_add_item(&moonshine_popup, l, None, Some(v));
    }
    select_popup_rep(&moonshine_popup, resolved_asr.moonshine_size.as_str());
    wire_popup(&moonshine_popup, &controller, sel!(changeMoonshine:), true);
    y += ROW_H;

    // Cleanup model
    let model_popup = popup(mtm, &container, y, "Cleanup model");
    let model_locked = std::env::var_os("GEMMA_MODEL_PATH").is_some();
    let models = settings::discover_llm_models();
    if models.is_empty() {
        popup_add_item(
            &model_popup,
            "(no models found under models/llm/)",
            None,
            Some(""),
        );
    } else {
        let default_gemma = crate::app_paths::gemma_default_path();
        let effective = loaded.gemma_model.clone().unwrap_or_else(|| {
            std::fs::canonicalize(&default_gemma)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or(default_gemma)
        });
        for choice in &models {
            let title = match &choice.hint {
                Some(h) => format!("{} — {h}", choice.label),
                None => choice.label.clone(),
            };
            popup_add_item(&model_popup, &title, None, Some(&choice.path));
            if choice.path == effective {
                select_popup_rep(&model_popup, &choice.path);
            }
        }
    }
    wire_popup(&model_popup, &controller, sel!(changeModel:), model_locked);
    y += ROW_H;
    if model_locked {
        y = env_hint(mtm, &container, y, "GEMMA_MODEL_PATH");
    }

    y = section(mtm, &container, y, "Experimental");

    // Streaming cleanup
    let streaming_switch = switch_row(mtm, &container, y, "Streaming cleanup");
    streaming_switch.setState(if loaded.resolve_streaming_cleanup() {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    wire_switch(
        &streaming_switch,
        &controller,
        sel!(changeStreaming:),
        std::env::var_os("DICTATE_STREAMING_CLEANUP").is_some(),
    );
    y += ROW_H;
    y = hint_line(
        mtm,
        &container,
        y,
        "Clean each sentence during the hold (VAD-segmented). Off = whole-buffer.",
    );

    // Listen mode
    let listen_switch = switch_row(mtm, &container, y, "Listen mode (wake word)");
    listen_switch.setState(if loaded.resolve_listen_mode() {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    wire_switch(
        &listen_switch,
        &controller,
        sel!(changeListen:),
        std::env::var_os("DICTATE_LISTEN_MODE").is_some(),
    );
    y += ROW_H;
    y = hint_line(
        mtm,
        &container,
        y,
        "Hands-free: say the wake word to dictate. Needs always-on mic.",
    );

    // Wake word
    let wake_field = text_row(mtm, &container, y, "Wake word", &loaded.resolve_wake_word());
    let wake_locked = std::env::var_os("DICTATE_WAKE_WORD").is_some();
    unsafe {
        wake_field.setTarget(Some(&controller));
        wake_field.setAction(Some(sel!(changeWakeWord:)));
        wake_field.setEnabled(!wake_locked);
    }
    y += ROW_H;

    y = section(mtm, &container, y, "Files & Logs");
    y = button_row(
        mtm,
        &container,
        y,
        "Edit cleanup prompts…",
        &controller,
        sel!(editPrompts:),
    );
    y = button_row(
        mtm,
        &container,
        y,
        "Open corrections folder",
        &controller,
        sel!(openCorrections:),
    );
    y = button_row(
        mtm,
        &container,
        y,
        "Voice Commands & Shortcuts…",
        &controller,
        sel!(openCommandsRef:),
    );
    y = button_row(mtm, &container, y, "Open Log", &controller, sel!(openLog:));
    y = button_row(
        mtm,
        &container,
        y,
        "Export Log to Downloads…",
        &controller,
        sel!(exportLog:),
    );

    y = section(mtm, &container, y, "About");
    y = about_line(
        mtm,
        &container,
        y,
        &format!("Local Dictation {}", env!("CARGO_PKG_VERSION")),
    );
    if let Some(dir) = crate::app_paths::config_dir() {
        y = about_line(mtm, &container, y, &format!("Config: {}", dir.display()));
    }
    y = about_line(
        mtm,
        &container,
        y,
        &format!("Models: {}", crate::app_paths::llm_models_dir().display()),
    );

    let total_h = y + PAD;
    container.setFrameSize(NSSize::new(WIN_W, total_h));

    scroll.setDocumentView(Some(&*container));
    content.addSubview(&scroll);
    content.addSubview(&apply_btn);
    content.addSubview(&hint);
    window.setContentView(Some(&*content));
    window.center();

    let ui = SettingsUi {
        window,
        _scroll: scroll,
        container,
        hotkey_popup,
        cleanup_switch,
        preroll_switch,
        format_popup,
        backend_popup,
        cohere_quant_popup,
        cohere_lang_popup,
        moonshine_popup,
        model_popup,
        streaming_switch,
        listen_switch,
        wake_field,
        pending: Mutex::new(loaded.clone()),
        original: loaded,
        _controller: controller,
    };
    refresh_backend_dependents(&ui);
    ui
}

// ─── Layout helpers ──────────────────────────────────────────────────────

fn label(
    mtm: MainThreadMarker,
    text: &str,
    frame: NSRect,
    font: &NSFont,
    color: &NSColor,
) -> Retained<NSTextField> {
    let l: Retained<NSTextField> =
        unsafe { msg_send![NSTextField::alloc(mtm), initWithFrame: frame] };
    unsafe {
        l.setStringValue(&NSString::from_str(text));
        l.setBezeled(false);
        l.setDrawsBackground(false);
        l.setEditable(false);
        l.setSelectable(false);
        l.setFont(Some(font));
        let _: () = msg_send![&*l, setTextColor: &*color];
    }
    l
}

/// Section header (bold, secondary colour) with top spacing. Returns new y.
fn section(mtm: MainThreadMarker, container: &SettingsFlippedView, y: f64, title: &str) -> f64 {
    let top = y + 18.0;
    let h = 18.0;
    let l = label(
        mtm,
        title,
        NSRect::new(NSPoint::new(PAD, top), NSSize::new(WIN_W - 2.0 * PAD, h)),
        &NSFont::boldSystemFontOfSize(12.0),
        &NSColor::secondaryLabelColor(),
    );
    container.addSubview(&l);
    top + h + 2.0
}

/// A small grey hint line under a control. Returns new y.
fn hint_line(mtm: MainThreadMarker, container: &SettingsFlippedView, y: f64, text: &str) -> f64 {
    let l = label(
        mtm,
        text,
        NSRect::new(NSPoint::new(PAD, y), NSSize::new(WIN_W - 2.0 * PAD, 14.0)),
        &NSFont::systemFontOfSize(11.0),
        &NSColor::secondaryLabelColor(),
    );
    container.addSubview(&l);
    y + 14.0
}

/// "(locked by DICTATE_…)" footnote. Returns new y.
fn env_hint(mtm: MainThreadMarker, container: &SettingsFlippedView, y: f64, varname: &str) -> f64 {
    hint_line(
        mtm,
        container,
        y,
        &format!("Locked by the {varname} env var"),
    )
}

/// About-section line. Returns new y.
fn about_line(mtm: MainThreadMarker, container: &SettingsFlippedView, y: f64, text: &str) -> f64 {
    let l = label(
        mtm,
        text,
        NSRect::new(NSPoint::new(PAD, y), NSSize::new(WIN_W - 2.0 * PAD, 16.0)),
        &NSFont::systemFontOfSize(11.0),
        &NSColor::secondaryLabelColor(),
    );
    container.addSubview(&l);
    y + 16.0
}

/// A row label (left column) + a popup control. Returns the popup.
fn popup(
    mtm: MainThreadMarker,
    container: &SettingsFlippedView,
    y: f64,
    title: &str,
) -> Retained<NSPopUpButton> {
    let l = label(
        mtm,
        title,
        NSRect::new(NSPoint::new(PAD, y + 6.0), NSSize::new(LABEL_W, 18.0)),
        &NSFont::systemFontOfSize(13.0),
        &NSColor::labelColor(),
    );
    container.addSubview(&l);
    let pop: Retained<NSPopUpButton> = NSPopUpButton::initWithFrame_pullsDown(
        NSPopUpButton::alloc(mtm),
        NSRect::new(
            NSPoint::new(CONTROL_X, y + 4.0),
            NSSize::new(CONTROL_W, 24.0),
        ),
        false,
    );
    container.addSubview(&pop);
    pop
}

fn popup_add_item(pop: &NSPopUpButton, title: &str, tag: Option<isize>, rep: Option<&str>) {
    pop.addItemWithTitle(&NSString::from_str(title));
    let idx = pop.numberOfItems() - 1;
    if let Some(item) = pop.itemAtIndex(idx) {
        unsafe {
            if let Some(t) = tag {
                item.setTag(t);
            }
            if let Some(r) = rep {
                let rep_ns = NSString::from_str(r);
                let rep_obj: &AnyObject = &rep_ns;
                item.setRepresentedObject(Some(rep_obj));
            }
        }
    }
}

fn select_popup_tag(pop: &NSPopUpButton, tag: i64) {
    let _ = pop.selectItemWithTag(tag as isize);
}

fn select_popup_rep(pop: &NSPopUpButton, target: &str) {
    if target.is_empty() {
        return;
    }
    let n = pop.numberOfItems();
    for i in 0..n {
        if let Some(item) = pop.itemAtIndex(i) {
            let obj: *mut AnyObject = unsafe { msg_send![&*item, representedObject] };
            let s = if obj.is_null() {
                String::new()
            } else {
                let s: &NSString = unsafe { &*(obj as *const NSString) };
                s.to_string()
            };
            if s == target {
                pop.selectItem(Some(&item));
                return;
            }
        }
    }
}

fn wire_popup(
    pop: &NSPopUpButton,
    controller: &Retained<SettingsController>,
    action: objc2::runtime::Sel,
    locked: bool,
) {
    unsafe {
        pop.setTarget(Some(controller));
        pop.setAction(Some(action));
        pop.setEnabled(!locked);
    }
}

/// A row label (left) + a switch (right). Returns the switch.
fn switch_row(
    mtm: MainThreadMarker,
    container: &SettingsFlippedView,
    y: f64,
    title: &str,
) -> Retained<NSSwitch> {
    let l = label(
        mtm,
        title,
        NSRect::new(NSPoint::new(PAD, y + 6.0), NSSize::new(LABEL_W, 18.0)),
        &NSFont::systemFontOfSize(13.0),
        &NSColor::labelColor(),
    );
    container.addSubview(&l);
    let sw = NSSwitch::initWithFrame(
        NSSwitch::alloc(mtm),
        NSRect::new(NSPoint::new(CONTROL_X, y + 4.0), NSSize::new(40.0, 22.0)),
    );
    container.addSubview(&sw);
    sw
}

fn wire_switch(
    sw: &NSSwitch,
    controller: &Retained<SettingsController>,
    action: objc2::runtime::Sel,
    locked: bool,
) {
    unsafe {
        sw.setTarget(Some(controller));
        sw.setAction(Some(action));
        sw.setEnabled(!locked);
    }
}

/// A row label (left) + a text field (right). Returns the field.
fn text_row(
    mtm: MainThreadMarker,
    container: &SettingsFlippedView,
    y: f64,
    title: &str,
    value: &str,
) -> Retained<NSTextField> {
    let l = label(
        mtm,
        title,
        NSRect::new(NSPoint::new(PAD, y + 6.0), NSSize::new(LABEL_W, 18.0)),
        &NSFont::systemFontOfSize(13.0),
        &NSColor::labelColor(),
    );
    container.addSubview(&l);
    let field: Retained<NSTextField> = unsafe {
        msg_send![
            NSTextField::alloc(mtm),
            initWithFrame: NSRect::new(NSPoint::new(CONTROL_X, y + 4.0), NSSize::new(CONTROL_W, 22.0))
        ]
    };
    field.setStringValue(&NSString::from_str(value));
    field.setFont(Some(&NSFont::systemFontOfSize(13.0)));
    field.setPlaceholderString(Some(&NSString::from_str(
        crate::settings::DEFAULT_WAKE_WORD,
    )));
    container.addSubview(&field);
    field
}

/// A full-width push button row. Returns new y.
fn button_row(
    mtm: MainThreadMarker,
    container: &SettingsFlippedView,
    y: f64,
    title: &str,
    controller: &Retained<SettingsController>,
    action: objc2::runtime::Sel,
) -> f64 {
    let btn: Retained<NSButton> = unsafe {
        msg_send![
            NSButton::alloc(mtm),
            initWithFrame: NSRect::new(NSPoint::new(CONTROL_X, y + 2.0), NSSize::new(CONTROL_W, 26.0))
        ]
    };
    unsafe {
        btn.setTitle(&NSString::from_str(title));
        let _: () = msg_send![&*btn, setBezelStyle: 1usize]; // rounded
        btn.setTarget(Some(controller));
        btn.setAction(Some(action));
    }
    container.addSubview(&btn);
    y + ROW_H
}

// ─── Sender readers (called from the controller's objc methods) ──────────

fn switch_on(sender: *mut AnyObject) -> bool {
    let sw: &NSSwitch = unsafe { &*(sender as *const NSSwitch) };
    sw.state() == NSControlStateValueOn
}

fn field_string(sender: *mut AnyObject) -> String {
    let f: &NSTextField = unsafe { &*(sender as *const NSTextField) };
    f.stringValue().to_string()
}

fn popup_selected_tag(sender: *mut AnyObject) -> isize {
    let pop: &NSPopUpButton = unsafe { &*(sender as *const NSPopUpButton) };
    match pop.selectedItem() {
        Some(it) => unsafe { msg_send![&*it, tag] },
        None => 0,
    }
}

fn popup_selected_rep(sender: *mut AnyObject) -> String {
    let pop: &NSPopUpButton = unsafe { &*(sender as *const NSPopUpButton) };
    match pop.selectedItem() {
        Some(it) => {
            let obj: *mut AnyObject = unsafe { msg_send![&*it, representedObject] };
            if obj.is_null() {
                String::new()
            } else {
                let s: &NSString = unsafe { &*(obj as *const NSString) };
                s.to_string()
            }
        }
        None => String::new(),
    }
}

// ─── State plumbing ──────────────────────────────────────────────────────

fn with_pending<F: FnOnce(&mut Settings)>(f: F) {
    if let Some(ui) = SETTINGS_UI.get() {
        let mut p = ui.pending.lock().unwrap();
        f(&mut p);
    }
}

/// Enable Cohere-only / Moonshine-only fields based on the *pending* backend
/// (so toggling the engine in the window live-enables its dependents), while
/// still respecting per-field env locks.
fn refresh_backend_dependents(ui: &SettingsUi) {
    let backend = {
        let p = ui.pending.lock().unwrap();
        p.asr_backend
            .as_deref()
            .and_then(AsrBackend::parse)
            .unwrap_or_default()
    };
    let cohere = backend == AsrBackend::Cohere;
    let moonshine = backend == AsrBackend::Moonshine;
    ui.cohere_quant_popup
        .setEnabled(cohere && std::env::var_os("DICTATE_COHERE_QUANT").is_none());
    ui.cohere_lang_popup
        .setEnabled(cohere && std::env::var_os("DICTATE_COHERE_LANG").is_none());
    ui.moonshine_popup
        .setEnabled(moonshine && std::env::var_os("DICTATE_MOONSHINE_VARIANT").is_none());
}

fn apply_and_restart(ui: &SettingsUi) {
    let pending = ui.pending.lock().unwrap().clone();
    if pending == ui.original {
        // Nothing changed — don't restart for nothing.
        ui.window.close();
        return;
    }
    if let Err(e) = pending.save() {
        eprintln!("[settings] failed to save settings: {e}");
        return;
    }
    menubar::relaunch_daemon();
}
