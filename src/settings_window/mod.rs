//! Native AppKit Settings window — the house settings shell.
//!
//! `LAYOUT_SETTINGS_WIDTH` × `LAYOUT_SETTINGS_HEIGHT` (760 × 540): a
//! `LAYOUT_SETTINGS_RAIL` (220) rail on `surface_sunken` carrying the app mark
//! and seven `CONTROL_RAILROW` rows, each with a 26 px icon tile; the selected
//! row is a raised tile. The pane on the right has a `title` heading with a
//! `meta` subtitle, groups as cards at `Radius.lg` with an uppercase `section`
//! label and `CONTROL_ROW` rows, and a `CONTROL_FOOTER` well carrying the
//! restart hint on the left and the primary key on the right.
//!
//! Controls follow the rule rather than the system: popups are chips, toggles
//! are ink (`NSSwitch` paints itself in the system accent and offers no tint,
//! so the house toggle is `slate::InkToggle`), and nothing is blue.
//!
//! Edit model: controls edit an in-memory `pending` copy of `Settings`; nothing
//! is written until the user clicks **Apply & Restart**, which saves once and
//! relaunches the daemon (the model load makes in-process hot-swap not worth
//! the complexity — same apply-on-restart contract as the menu submenus, just
//! batched so a window session with several edits costs one restart, not N).
//! Closing without applying keeps the edits in-memory for the window's life but
//! never writes them. The Dictionary pane is the one exception: Add and Remove
//! write `corrections.json` immediately (so a term is never lost by closing the
//! window); the daemon still only reads corrections at boot, so Apply & Restart
//! is what makes a saved term active. Env-var locks (`DICTATE_*`) disable the
//! matching control and show a footnote, mirroring the menu's "(locked by … env
//! var)" notes.
//!
//! Appearance is the one setting that also applies live, because a preview you
//! cannot see is not a preview.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, AllocAnyThread, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSImageView, NSMenuItem, NSPopUpButton, NSScrollView,
    NSSearchField, NSTextAlignment, NSTextField, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{MainThreadMarker, NSObject, NSPoint, NSRect, NSSize, NSString};

use crate::menubar::{self, slate, theme};
use crate::settings::{self, Settings};

use std::sync::{Mutex, OnceLock};

use crate::menubar::house_theme::{
    CONTROL_CHIP, CONTROL_FOOTER, CONTROL_RAILROW, CONTROL_ROW, CONTROL_TILE, HAIRLINE,
    LAYOUT_SETTINGS_HEIGHT, LAYOUT_SETTINGS_RAIL, LAYOUT_SETTINGS_WIDTH, RADIUS_LG, RADIUS_MD,
    RADIUS_SM, RADIUS_TILE, SPACE_MD, SPACE_SM, SPACE_XL, SPACE_XS, SPACE_XXL, SPACE_XXS,
    SPACE_XXXL, TYPE_BODY_SMALL, TYPE_CAPTION, TYPE_LABEL, TYPE_META, TYPE_TITLE, WEIGHT_MEDIUM,
    WEIGHT_REGULAR, WEIGHT_SEMIBOLD,
};

mod controller;
mod render;
mod widgets;
mod pane_dictation;
mod pane_dictionary;
mod pane_history;
mod pane_translate;
mod pane_output;
mod pane_speech;
mod pane_experimental;
mod pane_about;
use controller::*;
pub use render::{
    build_for_render,
    flash_history_for_render,
    scroll_to_bottom_for_render,
    scroll_to_top_for_render,
    seed_history_for_render,
    set_sidebar_for_render,
    show_pane_for_render,
    show_recording_feedback_for_render,
    verify_history_render_fixture,
};
use render::*;
use widgets::*;
use pane_dictation::*;
use pane_dictionary::*;
use pane_history::*;
use pane_translate::*;
use pane_output::*;
use pane_speech::*;
use pane_experimental::*;
use pane_about::*;

/// The app's own name, version and the commit it was built from. The commit
/// comes from the bundle's `Info.plist`, so a bug report can name the exact
/// build; a `cargo run` has no bundle and simply omits it.
pub(crate) fn version_line() -> String {
    match crate::app_paths::build_commit() {
        Some(commit) => format!(
            "Local Dictation {} ({commit})",
            env!("CARGO_PKG_VERSION")
        ),
        None => format!("Local Dictation {}", env!("CARGO_PKG_VERSION")),
    }
}

// ─── Shell geometry, all derived from tokens ─────────────────────────────
const WIN_W: f64 = LAYOUT_SETTINGS_WIDTH;
const WIN_H: f64 = LAYOUT_SETTINGS_HEIGHT;
const RAIL_W: f64 = LAYOUT_SETTINGS_RAIL;
/// Room for the traffic lights on a full-size-content window.
const TITLEBAR_H: f64 = 28.0;
/// The pane title's frame height, and the `meta` subtitle's under it.
const PANE_TITLE_H: f64 = TYPE_TITLE + SPACE_XS;
const PANE_SUBTITLE_H: f64 = TYPE_META + SPACE_XXS;
/// Clear band between the subtitle and the scroll view's top edge, so content
/// scrolled up clips before it reaches the subtitle.
const HEADER_CLIP: f64 = SPACE_XS;
/// The pane's heading band below the titlebar: the title sits `xxxl` below the
/// window top (the pane's top padding; the titlebar lies inside it), the
/// subtitle `xs` below the title frame, then the clip band.
const HEADER_H: f64 =
    SPACE_XXXL - TITLEBAR_H + PANE_TITLE_H + SPACE_XS + PANE_SUBTITLE_H + HEADER_CLIP;
const PANE_X: f64 = RAIL_W;
const PANE_W: f64 = WIN_W - RAIL_W;
/// Gutter between the pane edge and a card: the pane's side padding.
const PANE_PAD: f64 = SPACE_XXL;
const CARD_W: f64 = PANE_W - 2.0 * PANE_PAD;
/// Horizontal padding inside a card, and the 4 px band above/below its rows.
const CARD_PAD_X: f64 = 14.0;
const CARD_PAD_Y: f64 = 4.0;
/// The section label's header row at the top of a card: `sm` above the label,
/// `xs` below it, then the divider. Measured from the card's top edge.
const CARD_LABEL_TOP: f64 = SPACE_SM;
const CARD_LABEL_BELOW: f64 = SPACE_XS;
/// Header (pane heading) to the first card in a pane is `xl`, group to group;
/// the header band already holds `HEADER_CLIP` of it.
const FIRST_CARD_Y: f64 = SPACE_XL - HEADER_CLIP;
/// Widest a popup chip may grow before its title truncates.
const POPUP_MAX_W: f64 = 220.0;
/// The shelf toggle's two glyphs: the standard macOS sidebar pair, so the
/// control reads the same here as in Finder or Mail.
const SIDEBAR_GLYPH_OPEN: &str = "sidebar.left";
const SIDEBAR_GLYPH_CLOSED: &str = "sidebar.leading";
/// The pane viewport: what is left of the window once the titlebar, the pane
/// heading and the footer well have taken their bands. A pane document view is
/// never shorter than this, so a short pane still fills the scroll view.
const PANE_VIEWPORT_H: f64 = WIN_H - TITLEBAR_H - HEADER_H - CONTROL_FOOTER - HAIRLINE;
/// The History section's index in `PANES`. Its list is re-queried on each
/// visit, so the switch needs to know which pane it is.
const HISTORY_PANE: usize = 2;

/// The rail's sections, in order: label, SF Symbol, subtitle.
const PANES: &[(&str, &str, &str)] = &[
    (
        "Dictation",
        "mic",
        "Key, cleanup, and what happens after release.",
    ),
    (
        "Dictionary",
        "text.book.closed",
        "Names and phrases that must always come out right.",
    ),
    (
        "History",
        "clock.arrow.circlepath",
        "Every dictation, newest first. Open one to inspect its audio and text.",
    ),
    (
        "Translate",
        "character.book.closed",
        "Speak in one language, land in another.",
    ),
    ("Output", "text.alignleft", "Formatting and appearance."),
    (
        "Speech engine",
        "cpu",
        "The transcriber and the cleanup model.",
    ),
    (
        "Experimental",
        "flask",
        "Rough edges. Off unless you want them.",
    ),
    (
        "About",
        "info.circle",
        "Files, logs, and where things live.",
    ),
];

// Control fields are held for ownership/future refresh even when not read after
// build (the view hierarchy already retains them); suppress the dead-code lint.
#[allow(dead_code)]
pub struct SettingsUi {
    pub(crate) window: Retained<NSWindow>,
    /// The fixed 760-wide layout. Slides left by `RAIL_W` when the shelf closes.
    panel: Retained<slate::SlateView>,
    /// The toggle in the titlebar band, and the click target over it. Both are
    /// re-placed when the window width changes.
    sidebar_btn: Retained<slate::SlateView>,
    sidebar_click: Retained<objc2_app_kit::NSButton>,
    /// Shelf state. Open on every launch; not persisted, because a settings
    /// window that opens missing its navigation is a worse default than one
    /// extra click.
    sidebar_open: Mutex<bool>,
    _scroll: Retained<NSScrollView>,
    /// One flipped document view per rail section; the scroll view swaps
    /// between them so a pane change is one assignment, not a rebuild.
    panes: Vec<Retained<SettingsFlippedView>>,
    rail_rows: Vec<Retained<slate::SlateView>>,
    pane_title: Retained<NSTextField>,
    pane_subtitle: Retained<NSTextField>,
    /// The footer's left-hand hint. On a failed Apply & Restart it carries the
    /// reason (in warning ink) instead of the resting restart note, so a save
    /// failure is not just a log line the user never sees.
    footer_hint: Retained<NSTextField>,
    // General
    hotkey_popup: Retained<NSPopUpButton>,
    cleanup_switch: Retained<slate::InkToggle>,
    preroll_switch: Retained<slate::InkToggle>,
    format_popup: Retained<NSPopUpButton>,
    appearance_popup: Retained<NSPopUpButton>,
    // Speech
    cohere_lang_popup: Retained<NSPopUpButton>,
    model_popup: Retained<NSPopUpButton>,
    instruction_model_popup: Retained<NSPopUpButton>,
    // Advanced
    streaming_switch: Retained<slate::InkToggle>,
    streaming_hint: Retained<NSTextField>,
    processing_detail_switch: Retained<slate::InkToggle>,
    pill_size_popup: Retained<NSPopUpButton>,
    waveform_sensitivity_popup: Retained<NSPopUpButton>,
    completion_sound_popup: Retained<NSPopUpButton>,
    start_sound_popup: Retained<NSPopUpButton>,
    feedback_card_y: f64,
    tail_capture_switch: Retained<slate::InkToggle>,
    listen_switch: Retained<slate::InkToggle>,
    wake_field: Retained<NSTextField>,
    // Push-to-talk chord + behavior
    custom_hotkey_field: Retained<NSTextField>,
    custom_hotkey_hint: Retained<NSTextField>,
    copy_last_hint: Retained<NSTextField>,
    whisper_switch: Retained<slate::InkToggle>,
    // Dictionary
    dictionary_heard_field: Retained<NSTextField>,
    dictionary_correct_field: Retained<NSTextField>,
    dictionary_status: Retained<NSTextField>,
    /// Filters the term list live (the field sends its action per keystroke).
    dictionary_search: Retained<NSSearchField>,
    /// The pane, the term-list card, and the flipped body inside that card.
    /// Rows are rebuilt in place on every search, add, edit and delete, so the
    /// body's height changes and the card and pane are resized around it. The
    /// list is the pane's last card, so nothing below it can shift.
    dictionary_pane: Retained<SettingsFlippedView>,
    dictionary_card: Retained<slate::SlateView>,
    dictionary_body: Retained<slate::SlateView>,
    /// Where the list card sits in the pane, and the body inside the card.
    dictionary_card_y: f64,
    dictionary_body_y: f64,
    /// Row index → dictionary key, refreshed on every rebuild: a row's tag
    /// addresses its entry through this.
    dictionary_keys: Mutex<Vec<String>>,
    /// The key a row click pulled into the fields, so Add replaces that entry
    /// instead of leaving the old spelling behind beside the new one.
    dictionary_editing: Mutex<Option<String>>,
    /// Set when corrections.json exists but could not be parsed. The pane then
    /// refuses edits: saving would replace the user's whole dictionary.
    dictionary_load_error: Option<String>,
    pending_corrections: Mutex<crate::corrections::Corrections>,
    original_correction_lines: Vec<String>,
    // Translate
    translate_targets_field: Retained<NSTextField>,
    active_translate_popup: Retained<NSPopUpButton>,
    // History
    retention_popup: Retained<NSPopUpButton>,
    /// The line under the retention row: the stored count, and the last thing
    /// a click did (copied, deleted, cleared).
    history_status: Retained<NSTextField>,
    audio_usage_label: Retained<NSTextField>,
    recovery_warning_label: Retained<NSTextField>,
    /// Filters the dictation list live (the field sends its action per
    /// keystroke); an empty query is "everything, newest first".
    history_search: Retained<NSSearchField>,
    /// The pane, the list card, and the flipped body inside that card — the
    /// same in-place rebuild the dictionary list uses. History rows are rebuilt
    /// on every search, delete and pane visit, because the daemon keeps
    /// appending while the window is open.
    history_pane: Retained<SettingsFlippedView>,
    history_card: Retained<slate::SlateView>,
    history_body: Retained<slate::SlateView>,
    history_card_y: f64,
    history_body_y: f64,
    /// Row index → dictation row id, refreshed on every rebuild: a delete
    /// addresses the row the user clicked, not the position it sat in.
    history_ids: Mutex<Vec<i64>>,
    /// Row index → full text, so a click copies the untruncated dictation.
    history_texts: Mutex<Vec<String>>,
    /// Row index → the row view and its hidden "Copied" chip. A copy flashes
    /// the row it landed on, so the confirmation is where the eye already is.
    history_rows: Mutex<Vec<Retained<slate::SlateView>>>,
    history_pills: Mutex<Vec<Retained<slate::SlateView>>>,
    history_trashes: Mutex<Vec<Retained<slate::SlateView>>>,
    /// The row currently flashing, so the next copy (or the timer) can clear it
    /// without walking the whole list.
    history_flashed: Mutex<Option<usize>>,
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

/// Show the settings window, building it on first open. Idempotent across
/// reopens within a process (the window is reused).
pub fn show_settings_window() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let ui = SETTINGS_UI.get_or_init(|| build_settings_window(mtm, false));
    ui.window.makeKeyAndOrderFront(None);
    let app = NSApplication::sharedApplication(mtm);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
}

fn build_settings_window(mtm: MainThreadMarker, render_fixture: bool) -> SettingsUi {
    let controller = SettingsController::new();
    // Rendering is a data source, not merely a hidden production window.
    // None of the fixture branches open settings, dictionary, prompt, model,
    // or history stores; in particular they cannot migrate the user's DB.
    let loaded = if render_fixture {
        Settings::default()
    } else {
        Settings::load()
    };

    // ── Shell: window + rail + pane + footer well ───────────────────────
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIN_W, WIN_H));
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Titled
                | NSWindowStyleMask::Closable
                | NSWindowStyleMask::Miniaturizable
                | NSWindowStyleMask::FullSizeContentView,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe {
        window.setTitle(&NSString::from_str("Local Dictation Settings"));
        window.setReleasedWhenClosed(false);
    }

    let panel = slate::SlateView::new(mtm, frame, slate::Paint::Surface, 0.0);

    // ── Rail ────────────────────────────────────────────────────────────
    let rail = slate::SlateView::new(
        mtm,
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(RAIL_W, WIN_H)),
        slate::Paint::Sunken,
        0.0,
    );
    panel.addSubview(&rail);
    let rail_divider = slate::SlateView::new(
        mtm,
        NSRect::new(
            NSPoint::new(RAIL_W - HAIRLINE, 0.0),
            NSSize::new(HAIRLINE, WIN_H),
        ),
        slate::Paint::Divider,
        0.0,
    );
    panel.addSubview(&rail_divider);

    // App mark: a 28 px tile, the product name, and the SETTINGS section label.
    let mark_y = WIN_H - TITLEBAR_H - SPACE_SM - CONTROL_CHIP;
    let mark_tile = slate::SlateView::new(
        mtm,
        NSRect::new(
            NSPoint::new(SPACE_MD, mark_y),
            NSSize::new(CONTROL_CHIP, CONTROL_CHIP),
        ),
        slate::Paint::Tile,
        RADIUS_MD,
    );
    if let Some(img) = slate::template_symbol("waveform") {
        let iv = NSImageView::new(mtm);
        iv.setImage(Some(&img));
        unsafe {
            let _: () = msg_send![&*iv, setContentTintColor: &*theme::text_secondary()];
        }
        iv.setFrame(NSRect::new(
            NSPoint::new(
                ((CONTROL_CHIP - slate::GLYPH) / 2.0).round(),
                ((CONTROL_CHIP - slate::GLYPH) / 2.0).round(),
            ),
            NSSize::new(slate::GLYPH, slate::GLYPH),
        ));
        mark_tile.addSubview(&iv);
    }
    rail.addSubview(&mark_tile);

    let mark_name = slate::label(
        mtm,
        "Local Dictation",
        TYPE_BODY_SMALL,
        WEIGHT_SEMIBOLD,
        theme::text_primary(),
    );
    slate::place_left(
        &mark_name,
        SPACE_MD + CONTROL_CHIP + SPACE_SM,
        mark_y + CONTROL_CHIP - 8.0,
    );
    rail.addSubview(&mark_name);
    let mark_kind = slate::section_label(mtm, "Settings");
    slate::place_left(&mark_kind, SPACE_MD + CONTROL_CHIP + SPACE_SM, mark_y + 6.0);
    rail.addSubview(&mark_kind);

    let mut rail_rows = Vec::new();
    let mut row_y = mark_y - SPACE_MD - CONTROL_RAILROW;
    for (i, (title, symbol, _)) in PANES.iter().enumerate() {
        let row = slate::SlateView::new(
            mtm,
            NSRect::new(
                NSPoint::new(SPACE_SM, row_y),
                NSSize::new(RAIL_W - 2.0 * SPACE_SM, CONTROL_RAILROW),
            ),
            slate::Paint::Row,
            RADIUS_MD,
        );
        row.set_selected(i == 0);
        row.enable_hover();
        let mid = CONTROL_RAILROW / 2.0;
        let tile = slate::icon_tile(mtm, symbol, SPACE_XS, mid);
        row.addSubview(&tile);
        let l = slate::label(
            mtm,
            title,
            TYPE_BODY_SMALL,
            WEIGHT_MEDIUM,
            theme::text_primary(),
        );
        slate::place_left(&l, SPACE_XS + CONTROL_TILE + SPACE_SM, mid);
        row.addSubview(&l);
        let btn = slate::click_overlay(
            mtm,
            NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(RAIL_W - 2.0 * SPACE_SM, CONTROL_RAILROW),
            ),
            &controller,
            sel!(selectPane:),
            i as isize,
        );
        row.addSubview(&btn);
        rail.addSubview(&row);
        rail_rows.push(row);
        row_y -= CONTROL_RAILROW + SPACE_XXS;
    }

    // ── Pane heading ────────────────────────────────────────────────────
    let header_y = WIN_H - TITLEBAR_H - HEADER_H;
    let pane_title = slate::label(
        mtm,
        PANES[0].0,
        TYPE_TITLE,
        WEIGHT_SEMIBOLD,
        theme::text_primary(),
    );
    pane_title.setFrame(NSRect::new(
        NSPoint::new(PANE_X + PANE_PAD, WIN_H - SPACE_XXXL - PANE_TITLE_H),
        NSSize::new(PANE_W - 2.0 * PANE_PAD, PANE_TITLE_H),
    ));
    panel.addSubview(&pane_title);
    let pane_subtitle = slate::label(
        mtm,
        PANES[0].2,
        TYPE_META,
        WEIGHT_REGULAR,
        theme::text_secondary(),
    );
    pane_subtitle.setFrame(NSRect::new(
        NSPoint::new(PANE_X + PANE_PAD, header_y + HEADER_CLIP),
        NSSize::new(PANE_W - 2.0 * PANE_PAD, PANE_SUBTITLE_H),
    ));
    panel.addSubview(&pane_subtitle);

    // ── Footer well ─────────────────────────────────────────────────────
    let footer = slate::SlateView::new(
        mtm,
        NSRect::new(
            NSPoint::new(PANE_X, 0.0),
            NSSize::new(PANE_W, CONTROL_FOOTER),
        ),
        slate::Paint::Well,
        0.0,
    );
    let fmid = CONTROL_FOOTER / 2.0;
    let footer_hint = slate::label(
        mtm,
        "Restart needed for engine changes",
        TYPE_META,
        WEIGHT_REGULAR,
        theme::text_secondary(),
    );
    slate::place_left(&footer_hint, SPACE_MD, fmid);
    footer.addSubview(&footer_hint);
    let (apply_views, apply_w) = slate::key_hint(mtm, "Apply & Restart", &["⌘", "↩"], 0.0, fmid);
    let apply_x = PANE_W - SPACE_SM - apply_w;
    for v in &apply_views {
        let f = v.frame();
        v.setFrameOrigin(NSPoint::new(f.origin.x + apply_x, f.origin.y));
        footer.addSubview(v);
    }
    let apply_btn = slate::click_overlay(
        mtm,
        NSRect::new(
            NSPoint::new(apply_x - SPACE_XS, 0.0),
            NSSize::new(apply_w + 2.0 * SPACE_XS, CONTROL_FOOTER),
        ),
        &controller,
        sel!(applyAndRestart:),
        0,
    );
    unsafe {
        let _: () = msg_send![&*apply_btn, setKeyEquivalent: &*NSString::from_str("\r")];
        let _: () = msg_send![&*apply_btn, setAccessibilityLabel: &*NSString::from_str("Apply and restart")];
    }
    footer.addSubview(&apply_btn);
    panel.addSubview(&footer);
    let footer_divider = slate::SlateView::new(
        mtm,
        NSRect::new(
            NSPoint::new(PANE_X, CONTROL_FOOTER),
            NSSize::new(PANE_W, HAIRLINE),
        ),
        slate::Paint::Divider,
        0.0,
    );
    panel.addSubview(&footer_divider);

    // ── Scroll area holding one document view per pane ──────────────────
    let scroll_y = CONTROL_FOOTER + HAIRLINE;
    let scroll_h = PANE_VIEWPORT_H;
    debug_assert_eq!(scroll_h, header_y - scroll_y);
    let scroll: Retained<NSScrollView> = unsafe {
        msg_send![
            NSScrollView::alloc(mtm),
            initWithFrame: NSRect::new(
                NSPoint::new(PANE_X, scroll_y),
                NSSize::new(PANE_W, scroll_h),
            )
        ]
    };
    scroll.setHasVerticalScroller(true);
    scroll.setAutohidesScrollers(true);
    scroll.setDrawsBackground(false);
    panel.addSubview(&scroll);

    let cx = PaneCx {
        mtm,
        controller: &controller,
        loaded: &loaded,
        render_fixture,
        scroll_h,
    };
    let DictationPane {
        view: dictation,
        hotkey_popup,
        cleanup_switch,
        preroll_switch,
        streaming_switch,
        streaming_hint,
        processing_detail_switch,
        pill_size_popup,
        waveform_sensitivity_popup,
        completion_sound_popup,
        start_sound_popup,
        feedback_card_y,
        tail_capture_switch,
        custom_hotkey_field,
        custom_hotkey_hint,
        copy_last_hint,
        whisper_switch,
    } = build_dictation_pane(&cx);
    let DictionaryPane {
        view: dictionary,
        dictionary_heard_field,
        dictionary_correct_field,
        dictionary_status,
        dictionary_search,
        dictionary_pane,
        dictionary_card,
        dictionary_body,
        dictionary_card_y,
        dictionary_body_y,
        dictionary_keys,
        dictionary_load_error,
        loaded_corrections,
        original_correction_lines,
    } = build_dictionary_pane(&cx);
    let HistoryPane {
        view: history_pane_view,
        retention_popup,
        history_status,
        audio_usage_label,
        recovery_warning_label,
        history_search,
        history_card,
        history_body,
        history_card_y,
        history_body_y,
        initial_history_list,
    } = build_history_pane(&cx);
    let TranslatePane {
        view: translate,
        translate_targets_field,
        active_translate_popup,
    } = build_translate_pane(&cx);
    let OutputPane { view: output, format_popup, appearance_popup } = build_output_pane(&cx);
    let SpeechPane {
        view: speech,
        cohere_lang_popup,
        model_popup,
        instruction_model_popup,
    } = build_speech_pane(&cx);
    let ExperimentalPane {
        view: experimental,
        listen_switch,
        wake_field,
    } = build_experimental_pane(&cx);
    let AboutPane { view: about } = build_about_pane(&cx);

    let panes = vec![
        dictation,
        dictionary,
        history_pane_view.clone(),
        translate,
        output,
        speech,
        experimental,
        about,
    ];
    scroll.setDocumentView(Some(&*panes[0]));

    // ── Sidebar shelf ───────────────────────────────────────────────────
    // The whole 760-wide layout lives in `panel`, which sits inside a `shell`
    // that is the content view. Closing the shelf slides `panel` left by
    // exactly the rail width and narrows the window by the same amount, so the
    // pane keeps its geometry — no relayout, no reflow, and the rail is simply
    // off-screen. Opening reverses it. Open is the default on every launch.
    let shell = slate::SlateView::new(mtm, frame, slate::Paint::Surface, 0.0);
    shell.addSubview(&panel);
    let sidebar_btn = slate::chip(
        mtm,
        "",
        Some(SIDEBAR_GLYPH_OPEN),
        WIN_W - SPACE_SM,
        WIN_H - TITLEBAR_H / 2.0 - SPACE_XXS,
    );
    shell.addSubview(&sidebar_btn);
    let sidebar_click = slate::click_overlay(
        mtm,
        sidebar_btn.frame(),
        &controller,
        sel!(toggleSidebar:),
        0,
    );
    unsafe {
        let title = NSString::from_str("Hide the sidebar");
        let _: () = msg_send![&*sidebar_click, setToolTip: &*title];
        let _: () = msg_send![&*sidebar_click, setAccessibilityLabel: &*title];
        let _: () = msg_send![&*sidebar_click, setKeyEquivalent: &*NSString::from_str("s")];
        let _: () =
            msg_send![&*sidebar_click, setKeyEquivalentModifierMask: 1_u64 << 20 | 1_u64 << 19];
    }
    shell.addSubview(&sidebar_click);

    window.setContentView(Some(&*shell));
    slate::apply_panel_chrome(&window);
    window.center();

    SettingsUi {
        window,
        panel,
        sidebar_btn,
        sidebar_click,
        sidebar_open: Mutex::new(true),
        _scroll: scroll,
        panes,
        rail_rows,
        pane_title,
        pane_subtitle,
        footer_hint,
        hotkey_popup,
        cleanup_switch,
        preroll_switch,
        format_popup,
        appearance_popup,
        cohere_lang_popup,
        model_popup,
        instruction_model_popup,
        streaming_switch,
        streaming_hint,
        processing_detail_switch,
        pill_size_popup,
        waveform_sensitivity_popup,
        completion_sound_popup,
        start_sound_popup,
        feedback_card_y,
        tail_capture_switch,
        listen_switch,
        wake_field,
        custom_hotkey_field,
        custom_hotkey_hint,
        copy_last_hint,
        whisper_switch,
        dictionary_heard_field,
        dictionary_correct_field,
        dictionary_status,
        dictionary_search,
        dictionary_pane,
        dictionary_card,
        dictionary_body,
        dictionary_card_y,
        dictionary_body_y,
        dictionary_keys: Mutex::new(dictionary_keys),
        dictionary_editing: Mutex::new(None),
        dictionary_load_error,
        pending_corrections: Mutex::new(loaded_corrections),
        original_correction_lines,
        translate_targets_field,
        active_translate_popup,
        retention_popup,
        history_status,
        audio_usage_label,
        recovery_warning_label,
        history_search,
        history_pane: history_pane_view,
        history_card,
        history_body,
        history_card_y,
        history_body_y,
        history_ids: Mutex::new(initial_history_list.ids),
        history_texts: Mutex::new(initial_history_list.texts),
        history_rows: Mutex::new(initial_history_list.rows),
        history_pills: Mutex::new(initial_history_list.pills),
        history_trashes: Mutex::new(initial_history_list.trashes),
        history_flashed: Mutex::new(None),
        pending: Mutex::new(loaded.clone()),
        original: loaded,
        _controller: controller,
    }
}

/// Swap the visible pane and move the rail's selection fill.
fn show_pane(ui: &SettingsUi, index: usize) {
    // The daemon keeps appending while the window sits open, so the history
    // list is re-queried on every visit rather than frozen at build time.
    if index == HISTORY_PANE {
        refresh_history_list(ui);
    }
    select_pane_view(ui, index);
}

/// Put pane `index` in the scroll view and move the rail's selection fill to
/// it. The half of [`show_pane`] that only touches views, so a render proof can
/// switch panes without re-reading any store.
fn select_pane_view(ui: &SettingsUi, index: usize) {
    let Some(pane) = ui.panes.get(index) else {
        return;
    };
    ui._scroll.setDocumentView(Some(&**pane));
    pane.scrollPoint(NSPoint::new(0.0, 0.0));
    for (i, row) in ui.rail_rows.iter().enumerate() {
        row.set_selected(i == index);
    }
    if let Some((title, _, subtitle)) = PANES.get(index) {
        ui.pane_title.setStringValue(&NSString::from_str(title));
        ui.pane_subtitle
            .setStringValue(&NSString::from_str(subtitle));
    }
}

/// Open or close the sidebar shelf.
///
/// The pane never moves relative to the panel: the panel slides by `RAIL_W`
/// and the window narrows by the same amount, so the rail leaves the visible
/// area intact rather than reflowing every card. The window's top-left corner
/// is held, which is what an Apple sidebar toggle looks like from the user's
/// side even though the mechanism differs.
fn set_sidebar_open(ui: &SettingsUi, open: bool) {
    let width = if open { WIN_W } else { PANE_W };
    ui.panel
        .setFrameOrigin(NSPoint::new(if open { 0.0 } else { -RAIL_W }, 0.0));

    let frame = ui.window.frame();
    let content = ui.window.contentRectForFrameRect(frame);
    let chrome_h = frame.size.height - content.size.height;
    // Hold the top-left corner: only the right edge moves.
    let new_frame = NSRect::new(
        NSPoint::new(
            frame.origin.x,
            frame.origin.y + frame.size.height - (WIN_H + chrome_h),
        ),
        NSSize::new(width, WIN_H + chrome_h),
    );
    ui.window.setFrame_display_animate(new_frame, true, true);

    // Re-place the toggle against the new right edge, and re-point its glyph.
    let btn = ui.sidebar_btn.frame();
    let x = width - SPACE_SM - btn.size.width;
    ui.sidebar_btn.setFrameOrigin(NSPoint::new(x, btn.origin.y));
    ui.sidebar_click
        .setFrameOrigin(NSPoint::new(x, btn.origin.y));
    let glyph = if open {
        SIDEBAR_GLYPH_OPEN
    } else {
        SIDEBAR_GLYPH_CLOSED
    };
    if let Some(img) = slate::template_symbol(glyph) {
        for sub in ui.sidebar_btn.subviews().iter() {
            if let Ok(iv) = sub.downcast::<NSImageView>() {
                iv.setImage(Some(&img));
            }
        }
    }
    unsafe {
        let title = NSString::from_str(if open {
            "Hide the sidebar"
        } else {
            "Show the sidebar"
        });
        let _: () = msg_send![&*ui.sidebar_click, setToolTip: &*title];
        let _: () = msg_send![&*ui.sidebar_click, setAccessibilityLabel: &*title];
    }
    *ui.sidebar_open.lock().unwrap() = open;
}

fn with_pending<F: FnOnce(&mut Settings)>(f: F) {
    if let Some(ui) = SETTINGS_UI.get() {
        let mut p = ui.pending.lock().unwrap();
        f(&mut p);
    }
}

/// Show a failed Apply & Restart where the restart hint lives, instead of only
/// a log line. The window stays open, so the in-memory edits are not lost.
fn show_apply_error(ui: &SettingsUi, message: &str) {
    ui.footer_hint.setStringValue(&NSString::from_str(message));
    unsafe {
        let _: () = msg_send![&*ui.footer_hint, setTextColor: &*theme::warning()];
        // Bound the label so a long reason truncates before it runs under the
        // Apply button; the full text stays on the tooltip.
        let f = ui.footer_hint.frame();
        ui.footer_hint.setFrame(NSRect::new(
            f.origin,
            NSSize::new(PANE_W - SPACE_MD - 190.0, f.size.height),
        ));
        let _: () = msg_send![&*ui.footer_hint, setToolTip: &*NSString::from_str(message)];
    }
}

fn apply_and_restart(ui: &SettingsUi) {
    let pending = ui.pending.lock().unwrap().clone();
    let corrections = ui.pending_corrections.lock().unwrap().clone();
    let corrections_changed = corrections.to_editor_lines() != ui.original_correction_lines;
    if pending == ui.original && !corrections_changed {
        // Nothing changed — don't restart for nothing.
        ui.window.close();
        return;
    }
    if pending != ui.original {
        if let Err(e) = pending.save() {
            eprintln!("[settings] failed to save settings: {e}");
            show_apply_error(ui, &format!("Settings not saved. {e}"));
            return;
        }
    }
    if corrections_changed {
        if let Err(e) = corrections.save() {
            eprintln!("[settings] failed to save dictionary: {e}");
            show_apply_error(ui, &format!("Dictionary not saved. {e}"));
            ui.dictionary_status
                .setStringValue(&NSString::from_str(&format!("Not saved. {e}")));
            return;
        }
    }
    menubar::relaunch_daemon();
}

#[cfg(test)]
mod tests;
