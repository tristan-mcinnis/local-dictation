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
//! never writes them. Env-var locks (`DICTATE_*`) disable the matching control
//! and show a footnote, mirroring the menu's "(locked by … env var)" notes.
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
    RADIUS_SM, RADIUS_TILE, SPACE_LG, SPACE_MD, SPACE_SM, SPACE_XL, SPACE_XS, SPACE_XXS,
    TYPE_BODY_SMALL, TYPE_CAPTION, TYPE_LABEL, TYPE_META, TYPE_TITLE, WEIGHT_MEDIUM,
    WEIGHT_REGULAR, WEIGHT_SEMIBOLD,
};

// ─── Shell geometry, all derived from tokens ─────────────────────────────
const WIN_W: f64 = LAYOUT_SETTINGS_WIDTH;
const WIN_H: f64 = LAYOUT_SETTINGS_HEIGHT;
const RAIL_W: f64 = LAYOUT_SETTINGS_RAIL;
/// Room for the traffic lights on a full-size-content window.
const TITLEBAR_H: f64 = 28.0;
/// The pane's heading band: `title` over a `meta` subtitle.
const HEADER_H: f64 = 60.0;
const PANE_X: f64 = RAIL_W;
const PANE_W: f64 = WIN_W - RAIL_W;
/// Gutter between the pane edge and a card.
const PANE_PAD: f64 = 22.0;
const CARD_W: f64 = PANE_W - 2.0 * PANE_PAD;
/// Horizontal padding inside a card, and the 4 px band above/below its rows.
const CARD_PAD_X: f64 = 14.0;
const CARD_PAD_Y: f64 = 4.0;
/// The uppercase section label band at the top of a card.
const CARD_LABEL_H: f64 = 28.0;
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
/// How much of a dictionary row's text width the heard spelling takes; the
/// replacement gets the rest.
const TERM_HEARD_FRACTION: f64 = 0.42;
/// Rows drawn before the list stops and asks for a search instead. A settings
/// pane is a list you scan, not a database you page through.
const TERM_ROW_CAP: usize = 60;
/// The same cap for the history list, which is a longer read: a month of
/// dictation still scrolls, and past this the search field is the faster path.
const HISTORY_ROW_CAP: usize = 200;
/// How many rows are pulled from the DB per open. Well past the row cap, so
/// the "narrow your search" note reports a real total rather than the fetch.
const HISTORY_FETCH: usize = 2000;
/// The day header band inside the history list: the `section` label plus the
/// air above and below it.
const HISTORY_DAY_H: f64 = SPACE_XL;
/// The fixed time column on the left of a history row, so every row's text
/// starts on one edge whatever the hour reads.
const HISTORY_TIME_W: f64 = 40.0;
/// The History section's index in `PANES`. Its list is re-queried on each
/// visit, so the switch needs to know which pane it is.
const HISTORY_PANE: usize = 2;
const WAVEFORM_SENSITIVITY_NOTE: &str =
    "Visual response only. Microphone gain and recognition stay the same.";

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
    // General
    hotkey_popup: Retained<NSPopUpButton>,
    cleanup_switch: Retained<slate::InkToggle>,
    preroll_switch: Retained<slate::InkToggle>,
    format_popup: Retained<NSPopUpButton>,
    appearance_popup: Retained<NSPopUpButton>,
    // Speech
    cohere_lang_popup: Retained<NSPopUpButton>,
    model_popup: Retained<NSPopUpButton>,
    // Advanced
    streaming_switch: Retained<slate::InkToggle>,
    streaming_hint: Retained<NSTextField>,
    processing_detail_switch: Retained<slate::InkToggle>,
    pill_size_popup: Retained<NSPopUpButton>,
    waveform_sensitivity_popup: Retained<NSPopUpButton>,
    feedback_card_y: f64,
    tail_capture_switch: Retained<slate::InkToggle>,
    listen_switch: Retained<slate::InkToggle>,
    wake_field: Retained<NSTextField>,
    // Push-to-talk chord + behavior
    custom_hotkey_field: Retained<NSTextField>,
    custom_hotkey_hint: Retained<NSTextField>,
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

// ─── Flipped container (top-left origin) for top-down row layout ──────────
define_class!(
    #[unsafe(super(NSView))]
    #[name = "FDSettingsFlippedView"]
    pub struct SettingsFlippedView;

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
// sender and updates `pending` (no restart). `applyAndRestart:` flushes. The
// file/log buttons call straight through to the menubar helpers.
define_class!(
    #[unsafe(super(NSObject))]
    #[name = "FDSettingsController"]
    pub struct SettingsController;

    impl SettingsController {
        #[unsafe(method(selectPane:))]
        fn select_pane(&self, sender: *mut AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            if let Some(ui) = SETTINGS_UI.get() {
                show_pane(ui, tag as usize);
            }
        }

        #[unsafe(method(changeHotkey:))]
        fn change_hotkey(&self, sender: *mut AnyObject) {
            let code = popup_selected_tag(sender);
            // The popup only offers bare modifier keys, so selecting one clears
            // any chord modifiers the custom-hotkey field may have set.
            with_pending(|p| {
                p.hotkey_keycode = Some(code as i64);
                p.hotkey_modifiers = Some(0);
            });
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
            refresh_live_preview_control();
        }

        #[unsafe(method(changeFormat:))]
        fn change_format(&self, sender: *mut AnyObject) {
            let name = popup_selected_rep(sender);
            with_pending(|p| {
                p.active_format = if name.is_empty() { None } else { Some(name) };
            });
        }

        #[unsafe(method(changeAppearance:))]
        fn change_appearance(&self, sender: *mut AnyObject) {
            let v = popup_selected_rep(sender);
            // Appearance is the one setting that also applies immediately —
            // picking a look you cannot see until a restart is no choice at all.
            if let Some(mtm) = MainThreadMarker::new() {
                menubar::apply_app_appearance(mtm, &v);
            }
            with_pending(|p| p.appearance = if v.is_empty() { None } else { Some(v) });
        }

        #[unsafe(method(changeCohereLang:))]
        fn change_cohere_lang(&self, sender: *mut AnyObject) {
            let v = popup_selected_rep(sender);
            with_pending(|p| p.cohere_language = if v.is_empty() { None } else { Some(v) });
        }

        #[unsafe(method(changeModel:))]
        fn change_model(&self, sender: *mut AnyObject) {
            let path = popup_selected_rep(sender);
            with_pending(|p| p.llm_model = if path.is_empty() { None } else { Some(path) });
        }

        #[unsafe(method(changeStreaming:))]
        fn change_streaming(&self, sender: *mut AnyObject) {
            let on = switch_on(sender);
            with_pending(|p| p.streaming_asr = Some(on));
            refresh_live_preview_control();
        }

        #[unsafe(method(changeTailCapture:))]
        fn change_tail_capture(&self, sender: *mut AnyObject) {
            let on = switch_on(sender);
            with_pending(|p| p.tail_capture = Some(on));
        }

        #[unsafe(method(changeProcessingDetail:))]
        fn change_processing_detail(&self, sender: *mut AnyObject) {
            with_pending(|p| p.show_processing_detail = Some(switch_on(sender)));
        }

        #[unsafe(method(changePillSize:))]
        fn change_pill_size(&self, sender: *mut AnyObject) {
            with_pending(|p| p.pill_size = Some(popup_selected_rep(sender)));
        }

        #[unsafe(method(changeWaveformSensitivity:))]
        fn change_waveform_sensitivity(&self, sender: *mut AnyObject) {
            with_pending(|p| p.waveform_sensitivity = Some(popup_selected_rep(sender)));
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

        #[unsafe(method(addDictionaryEntry:))]
        fn add_dictionary_entry(&self, _sender: *mut AnyObject) {
            let Some(ui) = SETTINGS_UI.get() else {
                return;
            };
            let heard = ui.dictionary_heard_field.stringValue().to_string();
            let correct = ui.dictionary_correct_field.stringValue().to_string();
            let Some(line) = crate::corrections::quick_add_line(&heard, &correct) else {
                ui.dictionary_status.setStringValue(&NSString::from_str(
                    "Type the spelling you want in ‘Correct to’.",
                ));
                ui.window.makeFirstResponder(Some(&*ui.dictionary_correct_field));
                return;
            };
            // A row click pulls an entry into the fields; saving it must drop
            // the original key, or renaming the heard side leaves both.
            let editing = ui.dictionary_editing.lock().unwrap().take();
            let count = {
                let mut corrections = ui.pending_corrections.lock().unwrap();
                if let Some(key) = editing {
                    corrections.remove(&key);
                }
                let mut lines = corrections.to_editor_lines();
                lines.push(line);
                *corrections =
                    crate::corrections::Corrections::from_editor_text(&lines.join("\n"));
                corrections.len()
            };
            ui.dictionary_heard_field.setStringValue(&NSString::from_str(""));
            ui.dictionary_correct_field.setStringValue(&NSString::from_str(""));
            ui.dictionary_status.setStringValue(&NSString::from_str(&format!(
                "Saved. {count} terms will be active after Apply & Restart.",
            )));
            refresh_dictionary_list(ui);
            ui.window.makeFirstResponder(Some(&*ui.dictionary_heard_field));
        }

        #[unsafe(method(searchDictionary:))]
        fn search_dictionary(&self, _sender: *mut AnyObject) {
            if let Some(ui) = SETTINGS_UI.get() {
                refresh_dictionary_list(ui);
            }
        }

        #[unsafe(method(editDictionaryEntry:))]
        fn edit_dictionary_entry(&self, sender: *mut AnyObject) {
            let Some(ui) = SETTINGS_UI.get() else {
                return;
            };
            let tag: isize = unsafe { msg_send![sender, tag] };
            let Some((key, value)) = dictionary_entry_at(ui, tag) else {
                return;
            };
            // An identity entry ("keep this spelling") has no heard side.
            let heard = if key.eq_ignore_ascii_case(&value) { "" } else { &key };
            ui.dictionary_heard_field
                .setStringValue(&NSString::from_str(heard));
            ui.dictionary_correct_field
                .setStringValue(&NSString::from_str(&value));
            *ui.dictionary_editing.lock().unwrap() = Some(key);
            ui.dictionary_status.setStringValue(&NSString::from_str(&format!(
                "Editing “{value}”. Change it above, then Add.",
            )));
            ui.window
                .makeFirstResponder(Some(&*ui.dictionary_correct_field));
        }

        #[unsafe(method(removeDictionaryEntry:))]
        fn remove_dictionary_entry(&self, sender: *mut AnyObject) {
            let Some(ui) = SETTINGS_UI.get() else {
                return;
            };
            let tag: isize = unsafe { msg_send![sender, tag] };
            let Some((key, value)) = dictionary_entry_at(ui, tag) else {
                return;
            };
            let count = {
                let mut corrections = ui.pending_corrections.lock().unwrap();
                corrections.remove(&key);
                corrections.len()
            };
            // Removing the entry that is open in the fields ends that edit.
            let mut editing = ui.dictionary_editing.lock().unwrap();
            if editing.as_deref() == Some(key.as_str()) {
                *editing = None;
            }
            drop(editing);
            ui.dictionary_status.setStringValue(&NSString::from_str(&format!(
                "Removed “{value}”. {count} terms after Apply & Restart.",
            )));
            refresh_dictionary_list(ui);
        }

        #[unsafe(method(searchHistory:))]
        fn search_history(&self, _sender: *mut AnyObject) {
            if let Some(ui) = SETTINGS_UI.get() {
                refresh_history_list(ui);
            }
        }

        #[unsafe(method(openHistoryDetail:))]
        fn open_history_detail(&self, sender: *mut AnyObject) {
            let Some(ui) = SETTINGS_UI.get() else { return; };
            let tag: isize = unsafe { msg_send![sender, tag] };
            if let Some((id, _)) = history_entry_at(ui, tag) { menubar::show_history_detail(id); }
        }

        #[unsafe(method(copyHistoryEntry:))]
        fn copy_history_entry(&self, sender: *mut AnyObject) {
            let Some(ui) = SETTINGS_UI.get() else {
                return;
            };
            let tag: isize = unsafe { msg_send![sender, tag] };
            let Some((_, text)) = history_entry_at(ui, tag) else {
                return;
            };
            if text.trim().is_empty() {
                return;
            }
            menubar::copy_to_clipboard(&text);
            set_history_status(ui, &format!("Copied “{}”.", history_preview(&text)));
            flash_history_row(ui, tag as usize);
            // Cancel any un-flash already in flight, so a second copy restarts
            // the full window rather than inheriting the first one's remainder.
            unsafe {
                let obj: &AnyObject = &*(self as *const Self as *const AnyObject);
                let _: () = msg_send![
                    objc2::class!(NSObject),
                    cancelPreviousPerformRequestsWithTarget: obj,
                    selector: sel!(clearHistoryFlash:),
                    object: std::ptr::null::<AnyObject>(),
                ];
                let _: () = msg_send![
                    self,
                    performSelector: sel!(clearHistoryFlash:),
                    withObject: std::ptr::null::<AnyObject>(),
                    afterDelay: HISTORY_COPIED_FLASH_S,
                ];
            }
        }

        #[unsafe(method(toggleSidebar:))]
        fn toggle_sidebar(&self, _sender: *mut AnyObject) {
            if let Some(ui) = SETTINGS_UI.get() {
                set_sidebar_open(ui, !*ui.sidebar_open.lock().unwrap());
            }
        }

        /// Fired by `performSelector:afterDelay:` to end a copy flash.
        #[unsafe(method(clearHistoryFlash:))]
        fn clear_history_flash_action(&self, _sender: *mut AnyObject) {
            if let Some(ui) = SETTINGS_UI.get() {
                clear_history_flash(ui);
            }
        }

        #[unsafe(method(copyAllHistory:))]
        fn copy_all_history(&self, _sender: *mut AnyObject) {
            let Some(ui) = SETTINGS_UI.get() else {
                return;
            };
            // Everything currently shown, in the order it reads, so a search
            // scopes the copy the way it scopes the list.
            let texts = ui.history_texts.lock().unwrap().clone();
            if texts.is_empty() {
                set_history_status(ui, "Nothing to copy.");
                return;
            }
            let n = texts.len();
            menubar::copy_to_clipboard(&texts.join("\n\n"));
            let plural = if n == 1 { "dictation" } else { "dictations" };
            set_history_status(ui, &format!("Copied {n} {plural}."));
        }

        #[unsafe(method(deleteHistoryEntry:))]
        fn delete_history_entry(&self, sender: *mut AnyObject) {
            let Some(ui) = SETTINGS_UI.get() else {
                return;
            };
            let tag: isize = unsafe { msg_send![sender, tag] };
            let Some((id, text)) = history_entry_at(ui, tag) else {
                return;
            };
            // History is a store, not a pending setting: the delete lands now.
            if !crate::history::delete(id) {
                set_history_status(ui, "That dictation was already gone.");
                refresh_history_list(ui);
                return;
            }
            refresh_history_list(ui);
            set_history_status(ui, &format!("Deleted “{}”.", history_preview(&text)));
        }

        #[unsafe(method(clearHistory:))]
        fn clear_history(&self, _sender: *mut AnyObject) {
            let Some(ui) = SETTINGS_UI.get() else {
                return;
            };
            let n = crate::history::clear();
            refresh_history_list(ui);
            if n == 0 {
                set_history_status(ui, "There was nothing to delete.");
            } else {
                let plural = if n == 1 { "dictation" } else { "dictations" };
                set_history_status(ui, &format!("Deleted {n} {plural}."));
            }
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
            menubar::open_log_in_editor();
        }

        #[unsafe(method(exportLog:))]
        fn export_log(&self, _sender: *mut AnyObject) {
            menubar::export_log_to_downloads();
        }

        #[unsafe(method(changeShowResult:))]
        fn change_show_result(&self, sender: *mut AnyObject) {
            let on = switch_on(sender);
            with_pending(|p| p.show_result = Some(on));
        }

        #[unsafe(method(changeWhisper:))]
        fn change_whisper(&self, sender: *mut AnyObject) {
            let on = switch_on(sender);
            with_pending(|p| p.whisper_mode = Some(on));
        }

        #[unsafe(method(changeRetention:))]
        fn change_retention(&self, sender: *mut AnyObject) {
            let rep = popup_selected_rep(sender);
            with_pending(|p| p.history_retention = if rep.is_empty() { None } else { Some(rep) });
        }

        #[unsafe(method(changeAudioHistory:))]
        fn change_audio_history(&self, sender: *mut AnyObject) {
            with_pending(|p| p.audio_history_enabled = Some(switch_on(sender)));
        }

        #[unsafe(method(changeAudioRetention:))]
        fn change_audio_retention(&self, sender: *mut AnyObject) {
            with_pending(|p| p.audio_retention = Some(popup_selected_rep(sender)));
        }

        #[unsafe(method(changeAudioStorageCap:))]
        fn change_audio_storage_cap(&self, sender: *mut AnyObject) {
            with_pending(|p| p.audio_storage_cap_mb = popup_selected_rep(sender).parse().ok());
        }

        #[unsafe(method(changeTranslateTargets:))]
        fn change_translate_targets(&self, sender: *mut AnyObject) {
            let s = field_string(sender);
            let list: Vec<String> = s
                .split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect();
            with_pending(|p| p.translation_targets = if list.is_empty() { None } else { Some(list) });
        }

        #[unsafe(method(changeActiveTranslateTarget:))]
        fn change_active_translate_target(&self, sender: *mut AnyObject) {
            let rep = popup_selected_rep(sender);
            with_pending(|p| {
                p.active_translation_target = if rep.is_empty() { None } else { Some(rep) };
            });
        }

        #[unsafe(method(changeCustomHotkey:))]
        fn change_custom_hotkey(&self, sender: *mut AnyObject) {
            let s = field_string(sender);
            let (hint, applied) = parse_custom_hotkey(&s);
            with_pending(|p| match applied {
                Some((keycode, modifiers)) => {
                    p.hotkey_keycode = Some(keycode);
                    p.hotkey_modifiers = Some(modifiers);
                }
                None => {
                    p.hotkey_keycode = None;
                    p.hotkey_modifiers = None;
                }
            });
            if let Some(ui) = SETTINGS_UI.get() {
                ui.custom_hotkey_hint
                    .setStringValue(&NSString::from_str(&hint));
            }
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
    let ui = SETTINGS_UI.get_or_init(|| build_settings_window(mtm, false));
    ui.window.makeKeyAndOrderFront(None);
    let app = NSApplication::sharedApplication(mtm);
    #[allow(deprecated)]
    {
        app.activateIgnoringOtherApps(true)
    };
}

const RENDER_DICTIONARY: &str = "Cohere\nMLX\nkuh here → Cohere\nem el ex → MLX";

/// Build the window without registering it as the process-wide singleton and
/// without ever ordering it in. Used by `render-ui` to snapshot the shell.
pub fn build_for_render(mtm: MainThreadMarker) -> SettingsUi {
    build_settings_window(mtm, true)
}

/// Scroll the visible pane to its bottom so a snapshot shows the cards that sit
/// below the fold.
pub fn scroll_to_bottom_for_render(ui: &SettingsUi) {
    scroll_render(ui, true);
}

/// Switch the visible pane while snapshotting, so a render proof can capture a
/// pane other than the first. Unlike a click this never re-queries history, so
/// rows seeded by [`seed_history_for_render`] survive the switch.
pub fn show_pane_for_render(ui: &SettingsUi, index: usize) {
    select_pane_view(ui, index);
}

/// Draw `entries` into the History pane instead of the store's real rows, so a
/// render proof shows a known list rather than whatever the Mac dictated today.
pub fn seed_history_for_render(ui: &SettingsUi, entries: &[crate::history::Entry]) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let list = fill_history_body(
        mtm,
        &ui.history_body,
        &ui._controller,
        entries,
        entries.len(),
        "",
    );
    adopt_history_list(ui, list);
    size_list_card(
        &ui.history_pane,
        &ui.history_card,
        ui.history_card_y,
        ui.history_body_y,
        ui.history_body.frame().size.height,
    );
    ui.history_status
        .setStringValue(&NSString::from_str(&history_count_line(entries.len())));
    ui.audio_usage_label
        .setStringValue(&NSString::from_str(&recovery_usage_summary(
            0,
            0,
            entries.len(),
        )));
    ui.recovery_warning_label
        .setStringValue(&NSString::from_str(""));
    ui.recovery_warning_label.setHidden(true);
}

pub fn verify_history_render_fixture(ui: &SettingsUi, entries: usize) -> eyre::Result<()> {
    eyre::ensure!(
        ui.audio_usage_label.stringValue().to_string() == recovery_usage_summary(0, 0, entries),
        "History render read a live audio-usage count"
    );
    eyre::ensure!(
        ui.recovery_warning_label.isHidden(),
        "History render included a live recovery warning"
    );
    eyre::ensure!(
        ui.original == Settings::default(),
        "Settings render included live settings"
    );
    let dictionary = crate::corrections::Corrections::from_editor_text(RENDER_DICTIONARY);
    eyre::ensure!(
        ui.pending_corrections.lock().unwrap().to_editor_lines() == dictionary.to_editor_lines(),
        "Settings render included live dictionary terms"
    );
    Ok(())
}

/// Light row `idx` of the History list as a copy would, so the render proof
/// shows the confirmation state and not just the resting one.
pub fn flash_history_for_render(ui: &SettingsUi, idx: usize) {
    flash_history_row(ui, idx);
}

/// Open or close the sidebar shelf for a render proof.
pub fn set_sidebar_for_render(ui: &SettingsUi, open: bool) {
    set_sidebar_open(ui, open);
}

pub fn scroll_to_top_for_render(ui: &SettingsUi) {
    scroll_render(ui, false);
}

/// The complete Recording feedback card, regardless of the keyboard card's height.
pub fn show_recording_feedback_for_render(ui: &SettingsUi) {
    select_pane_view(ui, 0);
    if let Some(doc) = ui._scroll.documentView() {
        doc.scrollPoint(NSPoint::new(0.0, ui.feedback_card_y));
        ui._scroll.setNeedsDisplay(true);
        ui._scroll.displayIfNeeded();
    }
}

fn scroll_render(ui: &SettingsUi, bottom: bool) {
    let Some(doc) = ui._scroll.documentView() else {
        return;
    };
    let doc_h = doc.frame().size.height;
    let clip_h = ui._scroll.contentSize().height;
    let y = if bottom {
        (doc_h - clip_h).max(0.0)
    } else {
        0.0
    };
    // Flipped document: scrolling "down" means a larger y.
    doc.scrollPoint(NSPoint::new(0.0, y));
    ui._scroll.setNeedsDisplay(true);
    ui._scroll.displayIfNeeded();
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
        NSPoint::new(PANE_X + PANE_PAD, header_y + HEADER_H / 2.0 - 2.0),
        NSSize::new(PANE_W - 2.0 * PANE_PAD, TYPE_TITLE + SPACE_XS),
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
        NSPoint::new(PANE_X + PANE_PAD, header_y + SPACE_XS),
        NSSize::new(PANE_W - 2.0 * PANE_PAD, TYPE_META + SPACE_XXS),
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

    let new_pane = || -> Retained<SettingsFlippedView> {
        unsafe {
            msg_send![
                SettingsFlippedView::alloc(mtm),
                initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(PANE_W, scroll_h))
            ]
        }
    };

    // ═══ Pane 0 — Dictation ═════════════════════════════════════════════
    let dictation = new_pane();
    let mut y = SPACE_MD;

    let mut card = card_begin(mtm, &dictation, y, "Input");

    let hotkey_locked = std::env::var_os("DICTATE_HOTKEY_KEYCODE").is_some();
    let hotkey_active = loaded.resolve_hotkey(0x3D);
    let hotkey_popup = popup_row(mtm, &mut card, "Push-to-talk key");
    for (label_str, code) in settings::HOTKEY_CHOICES {
        popup_add_item(&hotkey_popup, label_str, Some(*code as isize), None);
    }
    select_popup_tag(&hotkey_popup, hotkey_active);
    finish_popup(&hotkey_popup);
    wire_popup(
        &hotkey_popup,
        &controller,
        sel!(changeHotkey:),
        hotkey_locked,
    );
    if hotkey_locked {
        env_hint(mtm, &mut card, "DICTATE_HOTKEY_KEYCODE");
    }

    // Custom hotkey (chord) — an arbitrary modifiers + key combo, e.g.
    // cmd+shift+d. The popup above only offers bare modifier keys; this field
    // is where a chord is declared. A live hint below reports the parsed
    // result plus any system-wide conflict.
    let (initial_custom_hotkey, initial_hotkey_hint) = initial_custom_hotkey();
    let custom_hotkey_field = text_row(mtm, &mut card, "Custom hotkey", &initial_custom_hotkey);
    custom_hotkey_field.setPlaceholderString(Some(&NSString::from_str("e.g. cmd+shift+d or f5")));
    unsafe {
        custom_hotkey_field.setTarget(Some(&controller));
        custom_hotkey_field.setAction(Some(sel!(changeCustomHotkey:)));
        custom_hotkey_field.setEnabled(!hotkey_locked);
    }
    let custom_hotkey_hint = hint_line(mtm, &mut card, &initial_hotkey_hint);

    y = card_end(&card);

    let feedback_card_y = y;
    let mut card = card_begin(mtm, &dictation, y, "Recording feedback");
    let preview = loaded.live_preview_control();
    let streaming_switch = toggle_row(
        mtm,
        &mut card,
        "Live transcript preview",
        "Draft while speaking",
    );
    streaming_switch.set_on(preview.enabled);
    wire_switch(
        &streaming_switch,
        &controller,
        sel!(changeStreaming:),
        !preview.editable,
    );
    let streaming_hint = hint_line(mtm, &mut card, preview.hint);
    unsafe {
        let _: () =
            msg_send![&*streaming_switch, setAccessibilityHelp: &*NSString::from_str(preview.hint)];
    }

    let processing_detail_switch = toggle_row(
        mtm,
        &mut card,
        "Processing detail",
        "Show each step before paste",
    );
    processing_detail_switch.set_on(loaded.resolve_show_processing_detail());
    wire_switch(
        &processing_detail_switch,
        &controller,
        sel!(changeProcessingDetail:),
        std::env::var_os("DICTATE_SHOW_PROCESSING_DETAIL").is_some(),
    );
    if std::env::var_os("DICTATE_SHOW_PROCESSING_DETAIL").is_some() {
        env_hint(mtm, &mut card, "DICTATE_SHOW_PROCESSING_DETAIL");
    }

    let pill_size_popup = popup_row(mtm, &mut card, "Pill size");
    for (title, value) in [("Compact", "compact"), ("Standard", "standard")] {
        popup_add_item(&pill_size_popup, title, None, Some(value));
    }
    select_popup_rep(&pill_size_popup, loaded.resolve_pill_size().as_str());
    finish_popup(&pill_size_popup);
    wire_popup(
        &pill_size_popup,
        &controller,
        sel!(changePillSize:),
        std::env::var_os("DICTATE_PILL_SIZE").is_some(),
    );
    if std::env::var_os("DICTATE_PILL_SIZE").is_some() {
        env_hint(mtm, &mut card, "DICTATE_PILL_SIZE");
    }

    let waveform_sensitivity_popup = popup_row(mtm, &mut card, "Waveform sensitivity");
    for (title, value) in [("Low", "low"), ("Balanced", "balanced"), ("High", "high")] {
        popup_add_item(&waveform_sensitivity_popup, title, None, Some(value));
    }
    select_popup_rep(
        &waveform_sensitivity_popup,
        loaded.resolve_waveform_sensitivity().as_str(),
    );
    finish_popup(&waveform_sensitivity_popup);
    wire_popup(
        &waveform_sensitivity_popup,
        &controller,
        sel!(changeWaveformSensitivity:),
        std::env::var_os("DICTATE_WAVEFORM_SENSITIVITY").is_some(),
    );
    if std::env::var_os("DICTATE_WAVEFORM_SENSITIVITY").is_some() {
        env_hint(mtm, &mut card, "DICTATE_WAVEFORM_SENSITIVITY");
    }
    hint_line(mtm, &mut card, WAVEFORM_SENSITIVITY_NOTE);
    unsafe {
        let note = NSString::from_str(WAVEFORM_SENSITIVITY_NOTE);
        let _: () = msg_send![&*waveform_sensitivity_popup, setToolTip: &*note];
        let _: () = msg_send![&*waveform_sensitivity_popup, setAccessibilityHelp: &*note];
    }
    let cleanup_switch = toggle_row(
        mtm,
        &mut card,
        "Enable cleanup",
        "Only runs when speech is messy",
    );
    cleanup_switch.set_on(loaded.cleanup_enabled.unwrap_or(true));
    wire_switch(&cleanup_switch, &controller, sel!(changeCleanup:), false);

    let whisper_switch = toggle_row(
        mtm,
        &mut card,
        "Whisper mode",
        "Lower gate for quiet speech",
    );
    whisper_switch.set_on(loaded.resolve_whisper_mode());
    wire_switch(
        &whisper_switch,
        &controller,
        sel!(changeWhisper:),
        std::env::var_os("DICTATE_WHISPER_MODE").is_some(),
    );
    if std::env::var_os("DICTATE_WHISPER_MODE").is_some() {
        env_hint(mtm, &mut card, "DICTATE_WHISPER_MODE");
    }

    let result_switch = toggle_row(
        mtm,
        &mut card,
        "Show result",
        "\"Landed · N words\" pill for 2 s after each dictation",
    );
    result_switch.set_on(loaded.resolve_show_result());
    wire_switch(
        &result_switch,
        &controller,
        sel!(changeShowResult:),
        std::env::var_os("DICTATE_SHOW_RESULT").is_some(),
    );
    if std::env::var_os("DICTATE_SHOW_RESULT").is_some() {
        env_hint(mtm, &mut card, "DICTATE_SHOW_RESULT");
    }

    y = card_end(&card);
    let mut card = card_begin(mtm, &dictation, y, "Capture");
    let preroll_switch = toggle_row(
        mtm,
        &mut card,
        "Always-on mic",
        "Pre-roll so the first words aren't clipped",
    );
    preroll_switch.set_on(loaded.resolve_preroll_ms() > 0);
    wire_switch(
        &preroll_switch,
        &controller,
        sel!(changePreroll:),
        std::env::var_os("DICTATE_PREROLL_MS").is_some(),
    );
    if std::env::var_os("DICTATE_PREROLL_MS").is_some() {
        env_hint(mtm, &mut card, "DICTATE_PREROLL_MS");
    }

    let tail_capture_switch = toggle_row(
        mtm,
        &mut card,
        "Tail-capture",
        "Keep listening ~250 ms after release",
    );
    tail_capture_switch.set_on(loaded.resolve_tail_capture());
    wire_switch(
        &tail_capture_switch,
        &controller,
        sel!(changeTailCapture:),
        std::env::var_os("DICTATE_TAIL_CAPTURE").is_some(),
    );
    let _ = card_end(&card);
    finish_pane(&dictation, &card);

    // ═══ Pane 1 — Dictionary ════════════════════════════════════════════
    let dictionary = new_pane();
    let mut card = card_begin(mtm, &dictionary, SPACE_MD, "Add a term");
    let dictionary_heard_field = text_row(mtm, &mut card, "What dictation hears", "");
    dictionary_heard_field.setPlaceholderString(Some(&NSString::from_str("e.g. exampleco")));
    let dictionary_correct_field = text_row(mtm, &mut card, "Correct to", "");
    dictionary_correct_field.setPlaceholderString(Some(&NSString::from_str("e.g. Example Co")));
    action_button_row(
        mtm,
        &mut card,
        "Add to dictionary",
        "Add",
        "plus",
        &controller,
        sel!(addDictionaryEntry:),
    );
    let loaded_corrections = if render_fixture {
        crate::corrections::Corrections::from_editor_text(RENDER_DICTIONARY)
    } else {
        crate::corrections::Corrections::load_default()
            .unwrap_or_else(|_| crate::corrections::Corrections::empty())
    };
    let original_correction_lines = loaded_corrections.to_editor_lines();
    let dictionary_status = hint_line(
        mtm,
        &mut card,
        &format!(
            "{} terms. Replacements run before and after cleanup, including raw terminal dictation.",
            loaded_corrections.len()
        ),
    );
    hint_line(
        mtm,
        &mut card,
        "Leave the first field blank to keep a spelling. Use both to fix a mishearing.",
    );
    let dictionary_card_y = card_end(&card);

    // The terms themselves: one row per entry, what dictation hears on the
    // left and what lands on the right. A row click loads it into the fields
    // above; the trash chip drops it. Both rebuild the rows in place.
    let mut card = card_begin(mtm, &dictionary, dictionary_card_y, "Your dictionary");
    let dictionary_search = dictionary_search_row(mtm, &mut card, &controller);
    let dictionary_body_y = card.y;
    let dictionary_body = slate::SlateView::flipped(
        mtm,
        NSRect::new(
            NSPoint::new(0.0, dictionary_body_y),
            NSSize::new(CARD_W, 0.0),
        ),
        slate::Paint::None,
        0.0,
    );
    card.view.addSubview(&dictionary_body);
    let dictionary_card = card.view.clone();
    let dictionary_keys =
        fill_dictionary_body(mtm, &dictionary_body, &controller, &loaded_corrections, "");
    let dictionary_pane = dictionary.clone();
    size_list_card(
        &dictionary,
        &dictionary_card,
        dictionary_card_y,
        dictionary_body_y,
        dictionary_body.frame().size.height,
    );

    // ═══ Pane 2 — History ═══════════════════════════════════════════════
    //
    // What was dictated, newest first, grouped by day. The retention window
    // lives here too, beside the rows it governs, rather than three panes away
    // among the input settings.
    let history_pane_view = new_pane();
    let mut card = card_begin(mtm, &history_pane_view, SPACE_MD, "Keeping");

    let retention_locked = std::env::var_os("DICTATE_HISTORY_RETENTION").is_some();
    let retention_popup = popup_row(mtm, &mut card, "Keep text for");
    for (label_str, rep) in [
        ("Forever", "forever"),
        ("1 year", "1year"),
        ("1 month", "1month"),
        ("1 week", "1week"),
        ("24 hours", "24hours"),
        ("Never", "never"),
    ] {
        popup_add_item(&retention_popup, label_str, None, Some(rep));
    }
    select_popup_rep(&retention_popup, &loaded.resolve_history_retention());
    finish_popup(&retention_popup);
    wire_popup(
        &retention_popup,
        &controller,
        sel!(changeRetention:),
        retention_locked,
    );
    if retention_locked {
        env_hint(mtm, &mut card, "DICTATE_HISTORY_RETENTION");
    }
    let audio_history_switch = toggle_row(
        mtm,
        &mut card,
        "Save audio for recovery",
        "Future recordings",
    );
    audio_history_switch.set_on(loaded.resolve_audio_history_enabled());
    wire_switch(
        &audio_history_switch,
        &controller,
        sel!(changeAudioHistory:),
        std::env::var_os("DICTATE_AUDIO_HISTORY").is_some(),
    );
    if std::env::var_os("DICTATE_AUDIO_HISTORY").is_some() {
        env_hint(mtm, &mut card, "DICTATE_AUDIO_HISTORY");
    }

    let audio_retention_popup = popup_row(mtm, &mut card, "Keep audio for");
    for (title, value) in [
        ("24 hours", "24hours"),
        ("1 week", "1week"),
        ("30 days", "1month"),
        ("Forever", "forever"),
    ] {
        popup_add_item(&audio_retention_popup, title, None, Some(value));
    }
    let audio_retention = loaded.resolve_audio_retention();
    if audio_retention == "1year" {
        popup_add_item(&audio_retention_popup, "1 year", None, Some("1year"));
    }
    if audio_retention == "never" {
        popup_add_item(&audio_retention_popup, "Never", None, Some("never"));
    }
    select_popup_rep(&audio_retention_popup, &audio_retention);
    finish_popup(&audio_retention_popup);
    wire_popup(
        &audio_retention_popup,
        &controller,
        sel!(changeAudioRetention:),
        std::env::var_os("DICTATE_AUDIO_RETENTION").is_some(),
    );
    if std::env::var_os("DICTATE_AUDIO_RETENTION").is_some() {
        env_hint(mtm, &mut card, "DICTATE_AUDIO_RETENTION");
    }

    let audio_cap_popup = popup_row(mtm, &mut card, "Audio storage limit");
    let cap_choices = [
        ("No size limit", 0_u64),
        ("512 MB", 512),
        ("1 GB", 1024),
        ("4 GB", 4096),
        ("10 GB", 10240),
    ];
    for (title, value) in cap_choices {
        popup_add_item(&audio_cap_popup, title, None, Some(&value.to_string()));
    }
    let cap = loaded.resolve_audio_storage_cap_mb();
    if !cap_choices.iter().any(|(_, value)| *value == cap) {
        popup_add_item(
            &audio_cap_popup,
            &format!("{cap} MB"),
            None,
            Some(&cap.to_string()),
        );
    }
    select_popup_rep(&audio_cap_popup, &cap.to_string());
    finish_popup(&audio_cap_popup);
    wire_popup(
        &audio_cap_popup,
        &controller,
        sel!(changeAudioStorageCap:),
        std::env::var_os("DICTATE_AUDIO_STORAGE_CAP_MB").is_some(),
    );
    if std::env::var_os("DICTATE_AUDIO_STORAGE_CAP_MB").is_some() {
        env_hint(mtm, &mut card, "DICTATE_AUDIO_STORAGE_CAP_MB");
    }
    hint_line(
        mtm,
        &mut card,
        "A size limit may remove older audio before its retention ends.",
    );
    hint_line(
        mtm,
        &mut card,
        "Removing audio keeps its transcript. Delete all removes both.",
    );
    action_button_row(
        mtm,
        &mut card,
        "Delete every dictation",
        "Delete all",
        "trash",
        &controller,
        sel!(clearHistory:),
    );
    let history_count = if render_fixture {
        0
    } else {
        crate::history::count()
    };
    let history_status = hint_line(mtm, &mut card, &history_count_line(history_count));
    let usage = if render_fixture {
        recovery_usage_summary(0, 0, 0)
    } else {
        recovery_usage_line()
    };
    let audio_usage_label = hint_line(mtm, &mut card, &usage);
    let recovery_notice = if render_fixture {
        None
    } else {
        crate::history::recovery_storage_notice()
    };
    let recovery_warning_label =
        hint_line(mtm, &mut card, recovery_notice.as_deref().unwrap_or(""));
    recovery_warning_label.setHidden(recovery_notice.is_none());
    hint_line(
        mtm,
        &mut card,
        "Deletes apply straight away. Retention waits for Apply & Restart.",
    );
    let history_card_y = card_end(&card);

    // The dictations themselves. Same in-place list as the dictionary: a
    // search row on top, rows rebuilt into a flipped body below it, and the
    // card sized to what the body drew.
    let mut card = card_begin(mtm, &history_pane_view, history_card_y, "Your dictations");
    let history_search = history_search_row(mtm, &mut card, &controller);
    let history_body_y = card.y;
    let history_body = slate::SlateView::flipped(
        mtm,
        NSRect::new(NSPoint::new(0.0, history_body_y), NSSize::new(CARD_W, 0.0)),
        slate::Paint::None,
        0.0,
    );
    card.view.addSubview(&history_body);
    let history_card = card.view.clone();
    let (initial_history, initial_history_total) = if render_fixture {
        (Vec::new(), 0)
    } else {
        history_query("")
    };
    let initial_history_list = fill_history_body(
        mtm,
        &history_body,
        &controller,
        &initial_history,
        initial_history_total,
        "",
    );
    size_list_card(
        &history_pane_view,
        &history_card,
        history_card_y,
        history_body_y,
        history_body.frame().size.height,
    );

    // ═══ Pane 2 — Translate ═════════════════════════════════════════════
    let translate = new_pane();
    let mut card = card_begin(mtm, &translate, SPACE_MD, "Targets");
    let loaded_targets = if render_fixture {
        vec!["Spanish".to_string(), "Chinese".to_string()]
    } else {
        loaded.resolve_translation_targets()
    };
    let targets_value = loaded_targets.join(", ");
    let translate_targets_field = text_row(mtm, &mut card, "Translate targets", &targets_value);
    translate_targets_field
        .setPlaceholderString(Some(&NSString::from_str("Spanish, Chinese, French")));
    let targets_locked = std::env::var_os("DICTATE_TRANSLATION_TARGETS").is_some();
    unsafe {
        translate_targets_field.setTarget(Some(&controller));
        translate_targets_field.setAction(Some(sel!(changeTranslateTargets:)));
        translate_targets_field.setEnabled(!targets_locked);
    }
    if targets_locked {
        env_hint(mtm, &mut card, "DICTATE_TRANSLATION_TARGETS");
    }

    let active_target_locked = std::env::var_os("DICTATE_ACTIVE_TRANSLATION_TARGET").is_some();
    let active_translate_popup = popup_row(mtm, &mut card, "Active target");
    if loaded_targets.is_empty() {
        popup_add_item(&active_translate_popup, "(none)", None, Some(""));
    } else {
        for t in &loaded_targets {
            popup_add_item(&active_translate_popup, t, None, Some(t));
        }
        if let Some(active) = loaded.resolve_active_translation_target(&loaded_targets) {
            select_popup_rep(&active_translate_popup, &active);
        }
    }
    finish_popup(&active_translate_popup);
    wire_popup(
        &active_translate_popup,
        &controller,
        sel!(changeActiveTranslateTarget:),
        active_target_locked,
    );
    if active_target_locked {
        env_hint(mtm, &mut card, "DICTATE_ACTIVE_TRANSLATION_TARGET");
    }
    let _ = card_end(&card);
    finish_pane(&translate, &card);

    // ═══ Pane 3 — Output ════════════════════════════════════════════════
    let output = new_pane();
    let mut card = card_begin(mtm, &output, SPACE_MD, "Format");
    let format_popup = popup_row(mtm, &mut card, "Output format");
    let format_names = if render_fixture {
        let mut names: Vec<String> = crate::prompts::default_formats().into_keys().collect();
        names.sort();
        names
    } else {
        crate::prompts::Prompts::load_quiet().format_names()
    };
    let format_locked = std::env::var_os("DICTATE_FORMAT").is_some();
    popup_add_item(&format_popup, "Default (no preset)", None, Some(""));
    for name in &format_names {
        popup_add_item(&format_popup, name, None, Some(name));
    }
    select_popup_rep(&format_popup, &loaded.resolve_format().unwrap_or_default());
    finish_popup(&format_popup);
    wire_popup(
        &format_popup,
        &controller,
        sel!(changeFormat:),
        format_locked,
    );
    if format_locked {
        env_hint(mtm, &mut card, "DICTATE_FORMAT");
    }
    let y = card_end(&card);

    let mut card = card_begin(mtm, &output, y, "Interface");
    let appearance_popup = popup_row(mtm, &mut card, "Appearance");
    for (rep, label_str) in settings::APPEARANCE_CHOICES {
        popup_add_item(&appearance_popup, label_str, None, Some(rep));
    }
    select_popup_rep(&appearance_popup, &loaded.resolve_appearance());
    finish_popup(&appearance_popup);
    wire_popup(
        &appearance_popup,
        &controller,
        sel!(changeAppearance:),
        std::env::var_os("DICTATE_APPEARANCE").is_some(),
    );
    hint_line(
        mtm,
        &mut card,
        "Dark is the default. Every window follows this — nothing is pinned to light.",
    );
    let _ = card_end(&card);
    finish_pane(&output, &card);

    // ═══ Pane 4 — Speech engine ═════════════════════════════════════════
    let speech = new_pane();
    let mut card = card_begin(mtm, &speech, SPACE_MD, "Engine");
    let resolved_asr = loaded.resolve_asr();
    hint_line(
        mtm,
        &mut card,
        "Fixed engine: Cohere Transcribe — native MLX (Q4)",
    );
    let cohere_lang_popup = popup_row(mtm, &mut card, "Cohere language");
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
    select_popup_rep(&cohere_lang_popup, &resolved_asr.language);
    finish_popup(&cohere_lang_popup);
    wire_popup(
        &cohere_lang_popup,
        &controller,
        sel!(changeCohereLang:),
        std::env::var_os("DICTATE_COHERE_LANG").is_some(),
    );

    let model_popup = popup_row(mtm, &mut card, "Cleanup model");
    let model_env = if render_fixture {
        None
    } else {
        settings::llm_model_env()
    };
    let model_locked = model_env.is_some();
    let models = if render_fixture {
        Vec::new()
    } else {
        settings::discover_llm_models()
    };
    if models.is_empty() {
        popup_add_item(
            &model_popup,
            "(no models found under models/llm/)",
            None,
            Some(""),
        );
    } else {
        let default_llm = crate::app_paths::llm_default_path();
        let effective = loaded.llm_model.clone().unwrap_or_else(|| {
            std::fs::canonicalize(&default_llm)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or(default_llm)
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
    finish_popup(&model_popup);
    wire_popup(&model_popup, &controller, sel!(changeModel:), model_locked);
    if let Some((name, _)) = model_env {
        env_hint(mtm, &mut card, name);
    }
    let _ = card_end(&card);
    finish_pane(&speech, &card);

    // ═══ Pane 5 — Experimental ══════════════════════════════════════════
    let experimental = new_pane();
    let mut card = card_begin(mtm, &experimental, SPACE_MD, "Experimental");
    let listen_switch = toggle_row(
        mtm,
        &mut card,
        "Listen mode",
        "Hands-free, triggered by a wake word",
    );
    listen_switch.set_on(loaded.resolve_listen_mode());
    wire_switch(
        &listen_switch,
        &controller,
        sel!(changeListen:),
        std::env::var_os("DICTATE_LISTEN_MODE").is_some(),
    );
    hint_line(mtm, &mut card, "Needs always-on mic.");

    let wake_word = if render_fixture {
        "computer".to_string()
    } else {
        loaded.resolve_wake_word()
    };
    let wake_field = text_row(mtm, &mut card, "Wake word", &wake_word);
    let wake_locked = std::env::var_os("DICTATE_WAKE_WORD").is_some();
    unsafe {
        wake_field.setTarget(Some(&controller));
        wake_field.setAction(Some(sel!(changeWakeWord:)));
        wake_field.setEnabled(!wake_locked);
    }
    let _ = card_end(&card);
    finish_pane(&experimental, &card);

    // ═══ Pane 6 — About ═════════════════════════════════════════════════
    let about = new_pane();
    let mut card = card_begin(mtm, &about, SPACE_MD, "Files & logs");
    for (title, action) in [
        ("Cleanup prompts", sel!(editPrompts:)),
        ("Corrections folder", sel!(openCorrections:)),
        ("Voice commands & shortcuts", sel!(openCommandsRef:)),
        ("Log", sel!(openLog:)),
        ("Export log to Downloads", sel!(exportLog:)),
    ] {
        button_row(mtm, &mut card, title, &controller, action);
    }
    let y = card_end(&card);

    let mut card = card_begin(mtm, &about, y, "About");
    hint_line(
        mtm,
        &mut card,
        &format!("Local Dictation {}", env!("CARGO_PKG_VERSION")),
    );
    if let Some(dir) = crate::app_paths::config_dir() {
        hint_line(mtm, &mut card, &format!("Config: {}", dir.display()));
    }
    hint_line(
        mtm,
        &mut card,
        &format!("Models: {}", crate::app_paths::llm_models_dir().display()),
    );
    let _ = card_end(&card);
    finish_pane(&about, &card);

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
        hotkey_popup,
        cleanup_switch,
        preroll_switch,
        format_popup,
        appearance_popup,
        cohere_lang_popup,
        model_popup,
        streaming_switch,
        streaming_hint,
        processing_detail_switch,
        pill_size_popup,
        waveform_sensitivity_popup,
        feedback_card_y,
        tail_capture_switch,
        listen_switch,
        wake_field,
        custom_hotkey_field,
        custom_hotkey_hint,
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

// ─── Card builder ────────────────────────────────────────────────────────

/// A group of rows drawn as one raised card. Rows are added top-down inside the
/// card, each with a `divider` above it, and the card is sized to fit at the end.
struct Card {
    view: Retained<slate::SlateView>,
    /// Next free y inside the card.
    y: f64,
    /// Where the card sits in its pane.
    origin_y: f64,
}

fn card_begin(mtm: MainThreadMarker, pane: &SettingsFlippedView, y: f64, label_text: &str) -> Card {
    let view = slate::SlateView::flipped(
        mtm,
        NSRect::new(NSPoint::new(PANE_PAD, y), NSSize::new(CARD_W, CARD_LABEL_H)),
        slate::Paint::Card,
        RADIUS_LG,
    );
    let l = slate::section_label(mtm, label_text);
    l.sizeToFit();
    let lh = l.frame().size.height;
    l.setFrame(NSRect::new(
        NSPoint::new(CARD_PAD_X, (CARD_PAD_Y + (CARD_LABEL_H - lh) / 2.0).round()),
        NSSize::new(CARD_W - 2.0 * CARD_PAD_X, lh),
    ));
    view.addSubview(&l);
    pane.addSubview(&view);
    Card {
        view,
        y: CARD_PAD_Y + CARD_LABEL_H,
        origin_y: y,
    }
}

/// Reserve a row of `height` inside the card, drawing the `divider` above it.
/// Returns the row's origin y inside the card.
fn card_row(card: &mut Card, height: f64) -> f64 {
    let mtm = MainThreadMarker::new().expect("card_row on the main thread");
    let line = slate::SlateView::new(
        mtm,
        NSRect::new(
            NSPoint::new(CARD_PAD_X, card.y),
            NSSize::new(CARD_W - 2.0 * CARD_PAD_X, HAIRLINE),
        ),
        slate::Paint::Divider,
        0.0,
    );
    card.view.addSubview(&line);
    let y = card.y + HAIRLINE;
    card.y = y + height;
    y
}

/// Size the card to its rows and return the pane's next free y.
fn card_end(card: &Card) -> f64 {
    let h = card.y + CARD_PAD_Y;
    card.view.setFrameSize(NSSize::new(CARD_W, h));
    card.origin_y + h + SPACE_SM
}

/// Size the pane's document view to the cards it holds, so the scroll view
/// knows how far it can travel.
fn finish_pane(pane: &SettingsFlippedView, last: &Card) {
    let h = last.origin_y + last.view.frame().size.height + SPACE_LG;
    let current = pane.frame().size.height;
    pane.setFrameSize(NSSize::new(PANE_W, h.max(current)));
}

// ─── Row builders ────────────────────────────────────────────────────────

fn row_label(
    mtm: MainThreadMarker,
    card: &Card,
    y: f64,
    height: f64,
    title: &str,
    detail: Option<&str>,
) -> f64 {
    let mid = y + height / 2.0;
    let l = slate::label(mtm, title, TYPE_LABEL, WEIGHT_MEDIUM, theme::text_primary());
    let w = place_left_flipped(&l, CARD_PAD_X, mid);
    card.view.addSubview(&l);
    let mut used = CARD_PAD_X + w;
    if let Some(d) = detail {
        let dl = slate::label(mtm, d, TYPE_CAPTION, WEIGHT_REGULAR, theme::text_tertiary());
        let dw = place_left_flipped(&dl, used + SPACE_XS, mid);
        card.view.addSubview(&dl);
        used += SPACE_XS + dw;
    }
    used
}

/// `slate::place_left` measures against a bottom-up midline; inside a flipped
/// card the same maths applies, because AppKit flips the frame for us.
fn place_left_flipped(field: &NSTextField, x: f64, mid_y: f64) -> f64 {
    field.sizeToFit();
    let f = field.frame();
    field.setFrame(NSRect::new(
        NSPoint::new(x, (mid_y - f.size.height / 2.0).round()),
        f.size,
    ));
    f.size.width
}

/// A label on the left and a popup, drawn as a chip, on the right.
fn popup_row(mtm: MainThreadMarker, card: &mut Card, title: &str) -> Retained<NSPopUpButton> {
    let y = card_row(card, CONTROL_ROW);
    row_label(mtm, card, y, CONTROL_ROW, title, None);
    let mid = y + CONTROL_ROW / 2.0;
    let shell = slate::SlateView::new(
        mtm,
        NSRect::new(
            NSPoint::new(
                CARD_W - CARD_PAD_X - POPUP_MAX_W,
                (mid - CONTROL_TILE / 2.0).round(),
            ),
            NSSize::new(POPUP_MAX_W, CONTROL_TILE),
        ),
        slate::Paint::Chip,
        RADIUS_TILE,
    );
    let pop: Retained<NSPopUpButton> = NSPopUpButton::initWithFrame_pullsDown(
        NSPopUpButton::alloc(mtm),
        NSRect::new(
            NSPoint::new(SPACE_XXS, 0.0),
            NSSize::new(POPUP_MAX_W - 2.0 * SPACE_XXS, CONTROL_TILE),
        ),
        false,
    );
    {
        pop.setBordered(false);
        pop.setFont(Some(&slate::font(TYPE_META, WEIGHT_REGULAR)));
        unsafe {
            let _: () = msg_send![&*pop, setAccessibilityLabel: &*NSString::from_str(title)];
        }
    }
    shell.addSubview(&pop);
    card.view.addSubview(&shell);
    pop
}

/// Shrink the popup chip to the width its widest title needs, then right-align
/// it. Called once the items are in.
fn finish_popup(pop: &NSPopUpButton) {
    pop.sizeToFit();
    let natural = pop.frame().size.width.min(POPUP_MAX_W);
    let w = (natural + 2.0 * SPACE_XXS).min(POPUP_MAX_W);
    pop.setFrame(NSRect::new(
        NSPoint::new(SPACE_XXS, 0.0),
        NSSize::new(w - 2.0 * SPACE_XXS, CONTROL_TILE),
    ));
    if let Some(shell) = unsafe { pop.superview() } {
        let f = shell.frame();
        shell.setFrame(NSRect::new(
            NSPoint::new(CARD_W - CARD_PAD_X - w, f.origin.y),
            NSSize::new(w, CONTROL_TILE),
        ));
    }
}

fn popup_add_item(pop: &NSPopUpButton, title: &str, tag: Option<isize>, rep: Option<&str>) {
    pop.addItemWithTitle(&NSString::from_str(title));
    let idx = pop.numberOfItems() - 1;
    if let Some(item) = pop.itemAtIndex(idx) {
        unsafe {
            style_menu_item(&item, title);
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

/// Render a popup item in the house type scale and ink, so the chip's title is
/// `meta` in `text_primary` rather than the system label colour.
unsafe fn style_menu_item(item: &NSMenuItem, title: &str) {
    use objc2_app_kit::{NSFontAttributeName, NSForegroundColorAttributeName};
    use objc2_foundation::{NSMutableAttributedString, NSRange};
    let attrs = NSMutableAttributedString::from_nsstring(&NSString::from_str(title));
    let range = NSRange::new(0, attrs.length());
    attrs.addAttribute_value_range(
        NSFontAttributeName,
        &*slate::font(TYPE_META, WEIGHT_REGULAR),
        range,
    );
    attrs.addAttribute_value_range(
        NSForegroundColorAttributeName,
        &*theme::text_primary(),
        range,
    );
    item.setAttributedTitle(Some(&attrs));
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

/// A label + optional detail on the left, an ink toggle on the right.
fn toggle_row(
    mtm: MainThreadMarker,
    card: &mut Card,
    title: &str,
    detail: &str,
) -> Retained<slate::InkToggle> {
    let y = card_row(card, CONTROL_ROW);
    row_label(mtm, card, y, CONTROL_ROW, title, Some(detail));
    let mid = y + CONTROL_ROW / 2.0;
    let sw = slate::InkToggle::new(
        mtm,
        NSRect::new(
            NSPoint::new(
                CARD_W - CARD_PAD_X - slate::TOGGLE_W,
                (mid - slate::TOGGLE_H / 2.0).round(),
            ),
            NSSize::new(slate::TOGGLE_W, slate::TOGGLE_H),
        ),
    );
    card.view.addSubview(&sw);
    unsafe {
        let _: () = msg_send![&*sw, setAccessibilityLabel: &*NSString::from_str(title)];
        let _: () = msg_send![&*sw, setAccessibilityHelp: &*NSString::from_str(detail)];
    }
    sw
}

fn wire_switch(
    sw: &slate::InkToggle,
    controller: &Retained<SettingsController>,
    action: objc2::runtime::Sel,
    locked: bool,
) {
    unsafe {
        let _: () = msg_send![sw, setTarget: &**controller];
        let _: () = msg_send![sw, setAction: action];
    }
    sw.set_active(!locked);
}

/// A label on the left and a house text field on the right.
fn text_row(
    mtm: MainThreadMarker,
    card: &mut Card,
    title: &str,
    value: &str,
) -> Retained<NSTextField> {
    let y = card_row(card, CONTROL_ROW);
    let used = row_label(mtm, card, y, CONTROL_ROW, title, None);
    let mid = y + CONTROL_ROW / 2.0;
    let left = (used + SPACE_MD).max(CARD_W / 2.0);
    let width = CARD_W - CARD_PAD_X - left;
    let shell = slate::SlateView::new(
        mtm,
        NSRect::new(
            NSPoint::new(left, (mid - CONTROL_CHIP / 2.0).round()),
            NSSize::new(width, CONTROL_CHIP),
        ),
        slate::Paint::Chip,
        RADIUS_SM,
    );
    let field: Retained<NSTextField> = unsafe {
        msg_send![
            NSTextField::alloc(mtm),
            initWithFrame: NSRect::new(
                NSPoint::new(SPACE_XS, ((CONTROL_CHIP - FIELD_TEXT_H) / 2.0).round()),
                NSSize::new(width - 2.0 * SPACE_XS, FIELD_TEXT_H),
            )
        ]
    };
    unsafe {
        field.setBezeled(false);
        field.setDrawsBackground(false);
        field.setAlignment(NSTextAlignment::Right);
        field.setFont(Some(&slate::font(TYPE_META, WEIGHT_REGULAR)));
        let _: () = msg_send![&*field, setTextColor: &*theme::text_primary()];
        let _: () = msg_send![&*field, setFocusRingType: 1usize]; // None
    }
    field.setStringValue(&NSString::from_str(value));
    shell.addSubview(&field);
    card.view.addSubview(&shell);
    field
}

const FIELD_TEXT_H: f64 = 18.0;

/// A row whose "control" is an action: a label on the left and an outlined
/// "Open" chip on the right.
fn button_row(
    mtm: MainThreadMarker,
    card: &mut Card,
    title: &str,
    controller: &Retained<SettingsController>,
    action: objc2::runtime::Sel,
) {
    action_button_row(
        mtm,
        card,
        title,
        "Open",
        "arrow.up.forward",
        controller,
        action,
    );
}

fn action_button_row(
    mtm: MainThreadMarker,
    card: &mut Card,
    title: &str,
    button_label: &str,
    symbol: &str,
    controller: &Retained<SettingsController>,
    action: objc2::runtime::Sel,
) {
    let y = card_row(card, CONTROL_ROW);
    row_label(mtm, card, y, CONTROL_ROW, title, None);
    let mid = y + CONTROL_ROW / 2.0;
    let chip = slate::chip(mtm, button_label, Some(symbol), CARD_W - CARD_PAD_X, mid);
    // A flipped parent flips the chip's y, so re-centre it in card space.
    let f = chip.frame();
    chip.setFrame(NSRect::new(
        NSPoint::new(f.origin.x, (mid - CONTROL_CHIP / 2.0).round()),
        f.size,
    ));
    card.view.addSubview(&chip);
    let btn = slate::click_overlay(mtm, chip.frame(), controller, action, 0);
    unsafe {
        let _: () = msg_send![&*btn, setToolTip: &*NSString::from_str(title)];
        let _: () = msg_send![&*btn, setAccessibilityLabel: &*NSString::from_str(title)];
    }
    card.view.addSubview(&btn);
}

// ─── Dictionary list ─────────────────────────────────────────────────────
//
// The Dictionary pane shows the terms themselves: one row per entry, the heard
// spelling on the left and what lands on the right. Rows are drawn into a
// flipped body view inside the last card, and rebuilt in place whenever the
// search text or the dictionary changes — so a row's `tag` is its index into
// `dictionary_keys`, refreshed by the same pass that draws it.

/// The list card's header row: a search glyph, a field that filters as you
/// type, and the chip that opens the JSON file behind the list.
fn dictionary_search_row(
    mtm: MainThreadMarker,
    card: &mut Card,
    controller: &Retained<SettingsController>,
) -> Retained<NSSearchField> {
    let y = card_row(card, CONTROL_ROW);
    let mid = y + CONTROL_ROW / 2.0;

    let chip = slate::chip(
        mtm,
        "Open file",
        Some("arrow.up.forward"),
        CARD_W - CARD_PAD_X,
        mid,
    );
    // A flipped card flips the chip's y, so re-centre it in card space.
    let cf = chip.frame();
    chip.setFrame(NSRect::new(
        NSPoint::new(cf.origin.x, (mid - CONTROL_CHIP / 2.0).round()),
        cf.size,
    ));
    card.view.addSubview(&chip);
    let open_btn = slate::click_overlay(mtm, chip.frame(), controller, sel!(openCorrections:), 0);
    unsafe {
        let title = NSString::from_str("Open dictionary file");
        let _: () = msg_send![&*open_btn, setToolTip: &*title];
        let _: () = msg_send![&*open_btn, setAccessibilityLabel: &*title];
    }
    card.view.addSubview(&open_btn);

    if let Some(img) = slate::template_symbol("magnifyingglass") {
        let iv = NSImageView::new(mtm);
        iv.setImage(Some(&img));
        unsafe {
            let _: () = msg_send![&*iv, setContentTintColor: &*theme::text_tertiary()];
        }
        iv.setFrame(NSRect::new(
            NSPoint::new(CARD_PAD_X, (mid - slate::GLYPH / 2.0).round()),
            NSSize::new(slate::GLYPH, slate::GLYPH),
        ));
        card.view.addSubview(&iv);
    }

    let field_x = CARD_PAD_X + slate::GLYPH + SPACE_XS;
    let field_w = (chip.frame().origin.x - SPACE_SM - field_x).max(CARD_PAD_X);
    let search: Retained<NSSearchField> = unsafe {
        msg_send![
            NSSearchField::alloc(mtm),
            initWithFrame: NSRect::new(
                NSPoint::new(field_x, (mid - FIELD_TEXT_H / 2.0).round()),
                NSSize::new(field_w, FIELD_TEXT_H),
            )
        ]
    };
    unsafe {
        // Filter on every keystroke, not just on Return.
        let _: () = msg_send![&*search, setSendsWholeSearchString: false];
        let _: () = msg_send![&*search, setSendsSearchStringImmediately: true];
        let ph = NSString::from_str("Search terms…");
        let _: () = msg_send![&*search, setPlaceholderString: &*ph];
        // The row draws its own glyph, so strip the cell's bezel and its
        // built-in magnifier / cancel buttons.
        search.setBezeled(false);
        search.setDrawsBackground(false);
        let _: () = msg_send![&*search, setFocusRingType: 1usize]; // None
        search.setFont(Some(&slate::font(TYPE_META, WEIGHT_REGULAR)));
        let _: () = msg_send![&*search, setTextColor: &*theme::text_primary()];
        let cell: *mut AnyObject = msg_send![&*search, cell];
        if !cell.is_null() {
            let _: () = msg_send![cell, setSearchButtonCell: std::ptr::null_mut::<AnyObject>()];
            let _: () = msg_send![cell, setCancelButtonCell: std::ptr::null_mut::<AnyObject>()];
        }
        let _: () = msg_send![&*search, setTarget: &**controller];
        let _: () = msg_send![&*search, setAction: sel!(searchDictionary:)];
    }
    card.view.addSubview(&search);
    search
}

/// Draw one row per entry into `body`, filtered by `filter`, then size `body`
/// to what it drew. Returns the key behind each row, in row order.
fn fill_dictionary_body(
    mtm: MainThreadMarker,
    body: &slate::SlateView,
    controller: &Retained<SettingsController>,
    corrections: &crate::corrections::Corrections,
    filter: &str,
) -> Vec<String> {
    for view in body.subviews().iter() {
        view.removeFromSuperview();
    }
    let needle = filter.trim().to_lowercase();
    let matched = corrections.search(&needle);

    let mut keys: Vec<String> = Vec::new();
    let mut y = 0.0;
    for (index, (key, value)) in matched.iter().take(TERM_ROW_CAP).enumerate() {
        y = dictionary_row(mtm, body, controller, index, key, value, y);
        keys.push(key.clone());
    }

    let note = if matched.is_empty() {
        if needle.is_empty() {
            "No terms yet. Add one above.".to_string()
        } else {
            format!("No term matches “{}”.", filter.trim())
        }
    } else if matched.len() > TERM_ROW_CAP {
        format!(
            "Showing {TERM_ROW_CAP} of {} matches. Search to narrow the list.",
            matched.len()
        )
    } else if needle.is_empty() {
        "Click a row to edit it. The trash chip removes it.".to_string()
    } else {
        format!("{} of {} terms match.", matched.len(), corrections.len())
    };
    y = list_note(mtm, body, y, &note);

    body.setFrameSize(NSSize::new(CARD_W, y));
    keys
}

/// One term row: heard spelling, arrow, replacement, and a trash chip. The row
/// click edits, the chip removes; both carry the row index as their tag.
fn dictionary_row(
    mtm: MainThreadMarker,
    body: &slate::SlateView,
    controller: &Retained<SettingsController>,
    index: usize,
    key: &str,
    value: &str,
    y: f64,
) -> f64 {
    let mut y = y;
    if y > 0.0 {
        let line = slate::SlateView::new(
            mtm,
            NSRect::new(
                NSPoint::new(CARD_PAD_X, y),
                NSSize::new(CARD_W - 2.0 * CARD_PAD_X, HAIRLINE),
            ),
            slate::Paint::Divider,
            0.0,
        );
        body.addSubview(&line);
        y += HAIRLINE;
    }

    let row_w = CARD_W - 2.0 * CARD_PAD_X + 2.0 * SPACE_XXS;
    let row = slate::SlateView::flipped(
        mtm,
        NSRect::new(
            NSPoint::new(CARD_PAD_X - SPACE_XXS, y),
            NSSize::new(row_w, CONTROL_ROW),
        ),
        slate::Paint::Row,
        RADIUS_SM,
    );
    row.enable_hover();
    let mid = CONTROL_ROW / 2.0;

    let trash = slate::chip(mtm, "", Some("trash"), row_w - SPACE_XXS, mid);
    let tf = trash.frame();
    trash.setFrame(NSRect::new(
        NSPoint::new(tf.origin.x, (mid - CONTROL_CHIP / 2.0).round()),
        tf.size,
    ));
    row.addSubview(&trash);

    let x0 = SPACE_XXS;
    let text_right = trash.frame().origin.x - SPACE_SM;
    let avail = (text_right - x0).max(0.0);
    if key.eq_ignore_ascii_case(value) {
        // An identity entry: the spelling is kept, nothing is replaced.
        let word = clamped_label(
            mtm,
            value,
            TYPE_LABEL,
            WEIGHT_MEDIUM,
            theme::text_primary(),
            x0,
            mid,
            avail,
        );
        let used = word.frame().size.width;
        row.addSubview(&word);
        let note = clamped_label(
            mtm,
            "kept as spelled",
            TYPE_META,
            WEIGHT_REGULAR,
            theme::text_tertiary(),
            x0 + used + SPACE_XS,
            mid,
            (avail - used - SPACE_XS).max(0.0),
        );
        row.addSubview(&note);
    } else {
        let heard_w = (avail * TERM_HEARD_FRACTION).round();
        let heard = clamped_label(
            mtm,
            key,
            TYPE_LABEL,
            WEIGHT_REGULAR,
            theme::text_secondary(),
            x0,
            mid,
            heard_w,
        );
        row.addSubview(&heard);
        let arrow = clamped_label(
            mtm,
            "→",
            TYPE_META,
            WEIGHT_REGULAR,
            theme::text_tertiary(),
            x0 + heard_w + SPACE_XS,
            mid,
            avail,
        );
        let arrow_w = arrow.frame().size.width;
        row.addSubview(&arrow);
        let value_x = x0 + heard_w + SPACE_XS + arrow_w + SPACE_XS;
        let replacement = clamped_label(
            mtm,
            value,
            TYPE_LABEL,
            WEIGHT_MEDIUM,
            theme::text_primary(),
            value_x,
            mid,
            (text_right - value_x).max(0.0),
        );
        row.addSubview(&replacement);
    }

    // The row click edits. The trash overlay goes on last, so it wins the hit
    // test over the row beneath it.
    let edit = slate::click_overlay(
        mtm,
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(row_w, CONTROL_ROW)),
        controller,
        sel!(editDictionaryEntry:),
        index as isize,
    );
    unsafe {
        let title = NSString::from_str(&format!("Edit {value}"));
        let _: () = msg_send![&*edit, setToolTip: &*title];
        let _: () = msg_send![&*edit, setAccessibilityLabel: &*title];
    }
    row.addSubview(&edit);
    let remove = slate::click_overlay(
        mtm,
        trash.frame(),
        controller,
        sel!(removeDictionaryEntry:),
        index as isize,
    );
    unsafe {
        let title = NSString::from_str(&format!("Remove {value}"));
        let _: () = msg_send![&*remove, setToolTip: &*title];
        let _: () = msg_send![&*remove, setAccessibilityLabel: &*title];
    }
    row.addSubview(&remove);

    body.addSubview(&row);
    y + CONTROL_ROW
}

/// The `caption` line under a list's rows: the count, the empty state, or the
/// cap. Shared by the dictionary and history lists.
fn list_note(mtm: MainThreadMarker, body: &slate::SlateView, y: f64, text: &str) -> f64 {
    let mut y = y;
    if y > 0.0 {
        let line = slate::SlateView::new(
            mtm,
            NSRect::new(
                NSPoint::new(CARD_PAD_X, y),
                NSSize::new(CARD_W - 2.0 * CARD_PAD_X, HAIRLINE),
            ),
            slate::Paint::Divider,
            0.0,
        );
        body.addSubview(&line);
        y += HAIRLINE;
    }
    let l = slate::label(
        mtm,
        text,
        TYPE_CAPTION,
        WEIGHT_REGULAR,
        theme::text_tertiary(),
    );
    l.setFrame(NSRect::new(
        NSPoint::new(
            CARD_PAD_X,
            (y + (HINT_H - TYPE_CAPTION - SPACE_XXS) / 2.0).round(),
        ),
        NSSize::new(CARD_W - 2.0 * CARD_PAD_X, TYPE_CAPTION + SPACE_XXS),
    ));
    body.addSubview(&l);
    y + HINT_H
}

/// A label sized to its text but never wider than `max_w` (it truncates), left
/// edge at `x`, centred on `mid_y` inside a flipped parent.
fn clamped_label(
    mtm: MainThreadMarker,
    text: &str,
    size: f64,
    weight: f64,
    color: objc2::rc::Retained<objc2_app_kit::NSColor>,
    x: f64,
    mid_y: f64,
    max_w: f64,
) -> Retained<NSTextField> {
    let l = slate::label(mtm, text, size, weight, color);
    l.sizeToFit();
    let f = l.frame();
    l.setFrame(NSRect::new(
        NSPoint::new(x, (mid_y - f.size.height / 2.0).round()),
        NSSize::new(f.size.width.min(max_w), f.size.height),
    ));
    l
}

/// Size a list card to its body, and the pane to the card. A list is always its
/// pane's last card, so growing it moves nothing else. Shared by the dictionary
/// and history lists.
fn size_list_card(
    pane: &SettingsFlippedView,
    card: &slate::SlateView,
    card_y: f64,
    body_y: f64,
    body_h: f64,
) {
    let card_h = body_y + body_h + CARD_PAD_Y;
    card.setFrameSize(NSSize::new(CARD_W, card_h));
    let pane_h = (card_y + card_h + SPACE_LG).max(PANE_VIEWPORT_H);
    pane.setFrameSize(NSSize::new(PANE_W, pane_h));
}

/// Redraw the term list from `pending_corrections` and the search field.
fn refresh_dictionary_list(ui: &SettingsUi) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let filter = ui.dictionary_search.stringValue().to_string();
    let keys = {
        let corrections = ui.pending_corrections.lock().unwrap();
        fill_dictionary_body(
            mtm,
            &ui.dictionary_body,
            &ui._controller,
            &corrections,
            &filter,
        )
    };
    *ui.dictionary_keys.lock().unwrap() = keys;
    size_list_card(
        &ui.dictionary_pane,
        &ui.dictionary_card,
        ui.dictionary_card_y,
        ui.dictionary_body_y,
        ui.dictionary_body.frame().size.height,
    );
    ui.dictionary_card.setNeedsDisplay(true);
}

/// The `(key, replacement)` a row's tag addresses, or `None` if the list moved
/// under the click.
fn dictionary_entry_at(ui: &SettingsUi, tag: isize) -> Option<(String, String)> {
    if tag < 0 {
        return None;
    }
    let key = ui
        .dictionary_keys
        .lock()
        .unwrap()
        .get(tag as usize)
        .cloned()?;
    let value = ui
        .pending_corrections
        .lock()
        .unwrap()
        .entries()
        .into_iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v)?;
    Some((key, value))
}

// ─── History list ────────────────────────────────────────────────────────
//
// The History pane shows what was dictated: rows grouped under a day header,
// newest first, the time on the left and the text beside it. A row click
// copies the untruncated dictation; the trash chip deletes it from the store
// immediately (history is data, not a pending setting). Rows are rebuilt in
// place on every search, delete and pane visit — the daemon keeps appending
// while the window is open — so a row's `tag` is its index into the ids and
// texts captured by the same pass that drew it.

/// The count line under the retention row. Also the sentence the pane opens
/// with, so an empty store explains itself instead of showing a blank card.
fn history_count_line(count: usize) -> String {
    match count {
        0 => "Nothing stored yet. Hold your hotkey and speak.".to_string(),
        1 => "1 dictation stored.".to_string(),
        n => format!("{n} dictations stored."),
    }
}

/// The list card's header row: a search glyph, a field that filters as you
/// type, and the chip that copies every row currently shown.
fn history_search_row(
    mtm: MainThreadMarker,
    card: &mut Card,
    controller: &Retained<SettingsController>,
) -> Retained<NSSearchField> {
    let y = card_row(card, CONTROL_ROW);
    let mid = y + CONTROL_ROW / 2.0;

    let chip = slate::chip(
        mtm,
        "Copy all",
        Some("doc.on.doc"),
        CARD_W - CARD_PAD_X,
        mid,
    );
    // A flipped card flips the chip's y, so re-centre it in card space.
    let cf = chip.frame();
    chip.setFrame(NSRect::new(
        NSPoint::new(cf.origin.x, (mid - CONTROL_CHIP / 2.0).round()),
        cf.size,
    ));
    card.view.addSubview(&chip);
    let copy_all = slate::click_overlay(mtm, chip.frame(), controller, sel!(copyAllHistory:), 0);
    unsafe {
        let title = NSString::from_str("Copy every dictation shown");
        let _: () = msg_send![&*copy_all, setToolTip: &*title];
        let _: () = msg_send![&*copy_all, setAccessibilityLabel: &*title];
    }
    card.view.addSubview(&copy_all);

    if let Some(img) = slate::template_symbol("magnifyingglass") {
        let iv = NSImageView::new(mtm);
        iv.setImage(Some(&img));
        unsafe {
            let _: () = msg_send![&*iv, setContentTintColor: &*theme::text_tertiary()];
        }
        iv.setFrame(NSRect::new(
            NSPoint::new(CARD_PAD_X, (mid - slate::GLYPH / 2.0).round()),
            NSSize::new(slate::GLYPH, slate::GLYPH),
        ));
        card.view.addSubview(&iv);
    }

    let field_x = CARD_PAD_X + slate::GLYPH + SPACE_XS;
    let field_w = (chip.frame().origin.x - SPACE_SM - field_x).max(CARD_PAD_X);
    let search: Retained<NSSearchField> = unsafe {
        msg_send![
            NSSearchField::alloc(mtm),
            initWithFrame: NSRect::new(
                NSPoint::new(field_x, (mid - FIELD_TEXT_H / 2.0).round()),
                NSSize::new(field_w, FIELD_TEXT_H),
            )
        ]
    };
    unsafe {
        // Filter on every keystroke, not just on Return.
        let _: () = msg_send![&*search, setSendsWholeSearchString: false];
        let _: () = msg_send![&*search, setSendsSearchStringImmediately: true];
        let ph = NSString::from_str("Search dictations…");
        let _: () = msg_send![&*search, setPlaceholderString: &*ph];
        // The row draws its own glyph, so strip the cell's bezel and its
        // built-in magnifier / cancel buttons.
        search.setBezeled(false);
        search.setDrawsBackground(false);
        let _: () = msg_send![&*search, setFocusRingType: 1usize]; // None
        search.setFont(Some(&slate::font(TYPE_META, WEIGHT_REGULAR)));
        let _: () = msg_send![&*search, setTextColor: &*theme::text_primary()];
        let cell: *mut AnyObject = msg_send![&*search, cell];
        if !cell.is_null() {
            let _: () = msg_send![cell, setSearchButtonCell: std::ptr::null_mut::<AnyObject>()];
            let _: () = msg_send![cell, setCancelButtonCell: std::ptr::null_mut::<AnyObject>()];
        }
        let _: () = msg_send![&*search, setTarget: &**controller];
        let _: () = msg_send![&*search, setAction: sel!(searchHistory:)];
    }
    card.view.addSubview(&search);
    search
}

/// The newest [`HISTORY_FETCH`] dictations matching `filter`, and how many
/// match in total. The one place the History pane reads the store, so the
/// drawing below stays free of IO and a render proof can hand it sample rows
/// instead. The total is counted in SQL rather than taken from the fetch, so
/// the note under a long store reports the real number.
fn history_query(filter: &str) -> (Vec<crate::history::Entry>, usize) {
    let filter = filter.trim();
    (
        crate::history::search(filter, HISTORY_FETCH),
        crate::history::match_count(filter),
    )
}

/// Draw `matched` as a day-grouped list into `body`, then size `body` to what
/// it drew. Only the newest [`HISTORY_ROW_CAP`] are drawn; `total` (which may
/// exceed `matched`, since the fetch is itself capped) is what the note counts.
/// `filter` is what produced `matched`, and only shapes that note. Returns the
/// row id and the full text behind each row, in row order.
fn fill_history_body(
    mtm: MainThreadMarker,
    body: &slate::SlateView,
    controller: &Retained<SettingsController>,
    matched: &[crate::history::Entry],
    total: usize,
    filter: &str,
) -> HistoryList {
    for view in body.subviews().iter() {
        view.removeFromSuperview();
    }
    let needle = filter.trim();
    let shown: Vec<crate::history::Entry> = matched.iter().take(HISTORY_ROW_CAP).cloned().collect();

    let mut ids: Vec<i64> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    let mut rows: Vec<Retained<slate::SlateView>> = Vec::new();
    let mut pills: Vec<Retained<slate::SlateView>> = Vec::new();
    let mut trashes: Vec<Retained<slate::SlateView>> = Vec::new();
    let mut y = 0.0;
    // One day-grouping implementation, shared with the launcher's history
    // panel, so both surfaces read the same dates in the same locale.
    for row in menubar::history_rows(&shown) {
        match row {
            menubar::HistoryRow::Day(day) => {
                y = history_day_header(mtm, body, y, &day);
            }
            menubar::HistoryRow::Item { time, text, index } => {
                let (next_y, row_view, copied_pill, trash_chip) =
                    history_row(mtm, body, controller, index, &time, &text, y);
                y = next_y;
                ids.push(shown[index].id);
                texts.push(shown[index].text.clone());
                rows.push(row_view);
                pills.push(copied_pill);
                trashes.push(trash_chip);
            }
        }
    }

    let note = if matched.is_empty() {
        if needle.is_empty() {
            "No dictations yet. Hold your hotkey and speak.".to_string()
        } else {
            format!("No dictation matches “{needle}”.")
        }
    } else if total > shown.len() {
        format!(
            "Showing the newest {} of {total} matches. Search to narrow the list.",
            shown.len()
        )
    } else {
        "Open a dictation to inspect its audio and text layers. The trash chip deletes it."
            .to_string()
    };
    y = list_note(mtm, body, y, &note);

    body.setFrameSize(NSSize::new(CARD_W, y));
    HistoryList {
        ids,
        texts,
        rows,
        pills,
        trashes,
    }
}

/// What one history-list rebuild produced: the store id and full text behind
/// each row (so a click addresses the right entry), plus the row view and its
/// hidden "Copied" chip (so a copy can flash the row it landed on).
struct HistoryList {
    ids: Vec<i64>,
    texts: Vec<String>,
    rows: Vec<Retained<slate::SlateView>>,
    pills: Vec<Retained<slate::SlateView>>,
    trashes: Vec<Retained<slate::SlateView>>,
}

/// A day band: the date in the uppercase `section` style, the same label the
/// launcher's history panel uses to break its list.
fn history_day_header(mtm: MainThreadMarker, body: &slate::SlateView, y: f64, day: &str) -> f64 {
    let mut y = y;
    if y > 0.0 {
        let line = slate::SlateView::new(
            mtm,
            NSRect::new(
                NSPoint::new(CARD_PAD_X, y),
                NSSize::new(CARD_W - 2.0 * CARD_PAD_X, HAIRLINE),
            ),
            slate::Paint::Divider,
            0.0,
        );
        body.addSubview(&line);
        y += HAIRLINE;
    }
    let l = slate::section_label(mtm, day);
    l.sizeToFit();
    let lh = l.frame().size.height;
    l.setFrame(NSRect::new(
        NSPoint::new(CARD_PAD_X, (y + (HISTORY_DAY_H - lh) / 2.0).round()),
        NSSize::new(CARD_W - 2.0 * CARD_PAD_X, lh),
    ));
    body.addSubview(&l);
    y + HISTORY_DAY_H
}

/// One dictation row: the time in a fixed column, the text beside it, and a
/// trash chip. The row click copies, the chip deletes; both carry the row index
/// as their tag.
#[allow(clippy::too_many_arguments)]
fn history_row(
    mtm: MainThreadMarker,
    body: &slate::SlateView,
    controller: &Retained<SettingsController>,
    index: usize,
    time: &str,
    text: &str,
    y: f64,
) -> (
    f64,
    Retained<slate::SlateView>,
    Retained<slate::SlateView>,
    Retained<slate::SlateView>,
) {
    let row_w = CARD_W - 2.0 * CARD_PAD_X + 2.0 * SPACE_XXS;
    let row = slate::SlateView::flipped(
        mtm,
        NSRect::new(
            NSPoint::new(CARD_PAD_X - SPACE_XXS, y),
            NSSize::new(row_w, CONTROL_ROW),
        ),
        slate::Paint::Row,
        RADIUS_SM,
    );
    row.enable_hover();
    let mid = CONTROL_ROW / 2.0;

    let trash = slate::chip(mtm, "", Some("trash"), row_w - SPACE_XXS, mid);
    let tf = trash.frame();
    trash.setFrame(NSRect::new(
        NSPoint::new(tf.origin.x, (mid - CONTROL_CHIP / 2.0).round()),
        tf.size,
    ));
    row.addSubview(&trash);

    let x0 = SPACE_XXS;
    // Times are monospaced digits so every row's text starts on one edge. The
    // font goes on before the measure: tabular digits are wider than
    // proportional ones, and a label sized to the narrower set truncates.
    let clock = slate::label(mtm, time, TYPE_META, WEIGHT_REGULAR, theme::text_tertiary());
    clock.setFont(Some(&slate::tabular_font(TYPE_META, WEIGHT_REGULAR)));
    clock.sizeToFit();
    let cs = clock.frame().size;
    clock.setFrame(NSRect::new(
        NSPoint::new(x0, (mid - cs.height / 2.0).round()),
        NSSize::new(cs.width.min(HISTORY_TIME_W), cs.height),
    ));
    row.addSubview(&clock);

    let text_x = x0 + HISTORY_TIME_W + SPACE_XS;
    let text_right = trash.frame().origin.x - SPACE_SM;
    let said = clamped_label(
        mtm,
        text,
        TYPE_BODY_SMALL,
        WEIGHT_REGULAR,
        theme::text_primary(),
        text_x,
        mid,
        (text_right - text_x).max(0.0),
    );
    row.addSubview(&said);

    // Copy confirmation. A status line under the retention row is easy to miss
    // when the eye is on the row that was just clicked, so the row itself says
    // it: for [`HISTORY_COPIED_FLASH_S`] the row takes the selection paint and
    // a checkmark chip stands exactly where the trash chip was, which is the
    // one spot on the row that is already reserved. Same frame, so it can
    // never crowd the text; the trash hides underneath it for the flash, so a
    // confirmation is never something you can click to delete by mistake.
    // Built hidden on every row: the flash is a visibility toggle, not a
    // rebuild.
    let copied = slate::chip(mtm, "", Some("checkmark"), row_w - SPACE_XXS, mid);
    copied.setFrame(trash.frame());
    copied.setHidden(true);
    row.addSubview(&copied);

    // The row click copies. The trash overlay goes on last, so it wins the hit
    // test over the row beneath it.
    let copy = slate::click_overlay(
        mtm,
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(row_w, CONTROL_ROW)),
        controller,
        sel!(openHistoryDetail:),
        index as isize,
    );
    unsafe {
        let title = NSString::from_str("Open audio and text layers");
        let _: () = msg_send![&*copy, setToolTip: &*title];
        let _: () = msg_send![&*copy, setAccessibilityLabel: &*title];
    }
    row.addSubview(&copy);
    let remove = slate::click_overlay(
        mtm,
        trash.frame(),
        controller,
        sel!(deleteHistoryEntry:),
        index as isize,
    );
    unsafe {
        let title = NSString::from_str("Delete this dictation");
        let _: () = msg_send![&*remove, setToolTip: &*title];
        let _: () = msg_send![&*remove, setAccessibilityLabel: &*title];
    }
    row.addSubview(&remove);

    body.addSubview(&row);
    (y + CONTROL_ROW, row, copied, trash)
}

/// Re-query the store through the search field and redraw the list.
fn refresh_history_list(ui: &SettingsUi) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let filter = ui.history_search.stringValue().to_string();
    refresh_recovery_usage(ui);
    let (matched, total) = history_query(&filter);
    let list = fill_history_body(
        mtm,
        &ui.history_body,
        &ui._controller,
        &matched,
        total,
        &filter,
    );
    adopt_history_list(ui, list);
    size_list_card(
        &ui.history_pane,
        &ui.history_card,
        ui.history_card_y,
        ui.history_body_y,
        ui.history_body.frame().size.height,
    );
    ui.history_card.setNeedsDisplay(true);
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

/// Take a fresh list rebuild into the UI state. Any flash belongs to rows that
/// no longer exist, so it is dropped rather than reapplied to whatever moved
/// into that index.
fn adopt_history_list(ui: &SettingsUi, list: HistoryList) {
    clear_history_flash(ui);
    *ui.history_ids.lock().unwrap() = list.ids;
    *ui.history_texts.lock().unwrap() = list.texts;
    *ui.history_rows.lock().unwrap() = list.rows;
    *ui.history_pills.lock().unwrap() = list.pills;
    *ui.history_trashes.lock().unwrap() = list.trashes;
}

/// How long a copied row stays lit. Long enough to register after the eye
/// leaves the pointer, short enough that it never reads as selection.
const HISTORY_COPIED_FLASH_S: f64 = 1.1;

/// Light the row a copy landed on: selection paint plus its "Copied" chip.
/// Replaces any previous flash, so a fast run of copies lights one row at a
/// time rather than leaving a trail.
fn flash_history_row(ui: &SettingsUi, idx: usize) {
    clear_history_flash(ui);
    if let Some(row) = ui.history_rows.lock().unwrap().get(idx) {
        row.set_selected(true);
    }
    if let Some(trash) = ui.history_trashes.lock().unwrap().get(idx) {
        trash.setHidden(true);
    }
    if let Some(pill) = ui.history_pills.lock().unwrap().get(idx) {
        pill.setHidden(false);
    }
    *ui.history_flashed.lock().unwrap() = Some(idx);
}

/// Put the flashed row back to rest. Safe to call when nothing is lit.
fn clear_history_flash(ui: &SettingsUi) {
    let Some(idx) = ui.history_flashed.lock().unwrap().take() else {
        return;
    };
    if let Some(row) = ui.history_rows.lock().unwrap().get(idx) {
        row.set_selected(false);
    }
    if let Some(pill) = ui.history_pills.lock().unwrap().get(idx) {
        pill.setHidden(true);
    }
    if let Some(trash) = ui.history_trashes.lock().unwrap().get(idx) {
        trash.setHidden(false);
    }
}

/// Set the status line under the retention row, then re-state the stored count
/// so the pane never leaves a stale total behind an action.
fn set_history_status(ui: &SettingsUi, message: &str) {
    let line = format!("{message} {}", history_count_line(crate::history::count()));
    ui.history_status.setStringValue(&NSString::from_str(&line));
    refresh_recovery_usage(ui);
}

fn recovery_usage_line() -> String {
    match crate::history::recovery_usage() {
        Ok(usage) => {
            recovery_usage_summary(usage.audio_count, usage.audio_bytes, usage.record_count)
        }
        Err(_) => "Audio storage totals are unavailable.".to_string(),
    }
}

fn recovery_usage_summary(audio_count: usize, audio_bytes: u64, records: usize) -> String {
    let (amount, unit) = if audio_bytes >= 1024 * 1024 * 1024 {
        (audio_bytes as f64 / (1024.0 * 1024.0 * 1024.0), "GB")
    } else {
        (audio_bytes as f64 / (1024.0 * 1024.0), "MB")
    };
    format!("{audio_count} audio clips · {amount:.1} {unit} · {records} recovery records")
}

fn refresh_recovery_usage(ui: &SettingsUi) {
    ui.audio_usage_label
        .setStringValue(&NSString::from_str(&recovery_usage_line()));
    let notice = crate::history::recovery_storage_notice();
    ui.recovery_warning_label
        .setStringValue(&NSString::from_str(notice.as_deref().unwrap_or("")));
    ui.recovery_warning_label.setHidden(notice.is_none());
    unsafe {
        let _: () = msg_send![&*ui.recovery_warning_label, setAccessibilityHelp: &*NSString::from_str(notice.as_deref().unwrap_or(""))];
    }
}

/// A one-line preview of `text` for a status message.
fn history_preview(text: &str) -> String {
    const PREVIEW_CHARS: usize = 42;
    let flat = text.replace('\n', " ");
    let short: String = flat.chars().take(PREVIEW_CHARS).collect();
    let ellipsis = if flat.chars().count() > PREVIEW_CHARS {
        "…"
    } else {
        ""
    };
    format!("{short}{ellipsis}")
}

/// The `(id, text)` a row's tag addresses, or `None` if the list moved under
/// the click.
fn history_entry_at(ui: &SettingsUi, tag: isize) -> Option<(i64, String)> {
    if tag < 0 {
        return None;
    }
    let idx = tag as usize;
    let id = *ui.history_ids.lock().unwrap().get(idx)?;
    let text = ui.history_texts.lock().unwrap().get(idx).cloned()?;
    Some((id, text))
}

/// A full-width `caption` line inside a card: hints, env-var locks, and the
/// About facts.
fn hint_line(mtm: MainThreadMarker, card: &mut Card, text: &str) -> Retained<NSTextField> {
    let y = card_row(card, HINT_H);
    let l = slate::label(
        mtm,
        text,
        TYPE_CAPTION,
        WEIGHT_REGULAR,
        theme::text_tertiary(),
    );
    l.setFrame(NSRect::new(
        NSPoint::new(
            CARD_PAD_X,
            (y + (HINT_H - TYPE_CAPTION - SPACE_XXS) / 2.0).round(),
        ),
        NSSize::new(CARD_W - 2.0 * CARD_PAD_X, TYPE_CAPTION + SPACE_XXS),
    ));
    card.view.addSubview(&l);
    l
}

const HINT_H: f64 = 22.0;

/// "(locked by DICTATE_…)" footnote.
fn env_hint(mtm: MainThreadMarker, card: &mut Card, varname: &str) {
    hint_line(mtm, card, &format!("Locked by the {varname} env var"));
}

// ─── Sender readers (called from the controller's objc methods) ──────────

/// Works for the house ink toggle and for any `NSControl` with a `state`.
fn switch_on(sender: *mut AnyObject) -> bool {
    let state: isize = unsafe { msg_send![sender, state] };
    state != 0
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

/// Render a keycode as a parseable key name ("d", "f5", "space", …) so a
/// configured chord can be round-tripped back through `Hotkey::parse`.
fn key_name_from_keycode(code: i64) -> String {
    match code {
        0x31 => "space".into(),
        0x24 => "return".into(),
        0x30 => "tab".into(),
        0x35 => "escape".into(),
        0x7B => "left".into(),
        0x7C => "right".into(),
        0x7D => "down".into(),
        0x7E => "up".into(),
        _ => {
            if (0x7A..=0x8D).contains(&code) {
                format!("f{}", code - 0x7A + 1)
            } else if (0x00..=0x19).contains(&code) {
                ((b'a' + code as u8) as char).to_string()
            } else if (0x1D..=0x26).contains(&code) {
                ((b'0' + (code - 0x1D) as u8) as char).to_string()
            } else {
                format!("key 0x{code:x}")
            }
        }
    }
}

/// Render a `Hotkey` as a parseable shorthand ("cmd+shift+d", "f5", …).
fn hotkey_to_parseable(h: &crate::hotkeys::Hotkey) -> String {
    let mut parts: Vec<String> = Vec::new();
    if h.modifiers & crate::hotkeys::COMMAND != 0 {
        parts.push("cmd".into());
    }
    if h.modifiers & crate::hotkeys::SHIFT != 0 {
        parts.push("shift".into());
    }
    if h.modifiers & crate::hotkeys::CONTROL != 0 {
        parts.push("ctrl".into());
    }
    if h.modifiers & crate::hotkeys::OPTION != 0 {
        parts.push("option".into());
    }
    parts.push(key_name_from_keycode(h.keycode));
    parts.join("+")
}

/// Parse a custom-hotkey field string. Returns the hint line to display and,
/// on a valid parse, the `(keycode, modifiers)` to write into `pending`.
fn parse_custom_hotkey(s: &str) -> (String, Option<(i64, u64)>) {
    match crate::hotkeys::Hotkey::parse(s) {
        Some(hk) => {
            let hint = match crate::hotkeys::detect_conflict(&hk) {
                Some(msg) => format!("{} — {msg}", hk.display()),
                None => format!("{} — no conflict detected", hk.display()),
            };
            (hint, Some((hk.keycode, hk.modifiers)))
        }
        None => ("unrecognized hotkey".to_string(), None),
    }
}

/// The initial (field value, hint) for the custom-hotkey field, derived from
/// the loaded hotkey. A bare modifier (the historical default, Right Option)
/// is owned by the popup above; any real key renders as a parseable chord.
fn initial_custom_hotkey() -> (String, String) {
    let hk = crate::hotkeys::Hotkey::from_settings();
    if hk.is_modifier_only() {
        (
            String::new(),
            "The key is a bare modifier — set it in the popup above, or type a \
             chord like cmd+shift+d here."
                .to_string(),
        )
    } else {
        let text = hotkey_to_parseable(&hk);
        let hint = match crate::hotkeys::detect_conflict(&hk) {
            Some(msg) => format!("{} — {msg}", hk.display()),
            None => format!("{} — no conflict detected", hk.display()),
        };
        (text, hint)
    }
}

fn with_pending<F: FnOnce(&mut Settings)>(f: F) {
    if let Some(ui) = SETTINGS_UI.get() {
        let mut p = ui.pending.lock().unwrap();
        f(&mut p);
    }
}

fn refresh_live_preview_control() {
    if let Some(ui) = SETTINGS_UI.get() {
        let state = ui.pending.lock().unwrap().live_preview_control();
        ui.streaming_switch.set_on(state.enabled);
        ui.streaming_switch.set_active(state.editable);
        ui.streaming_hint
            .setStringValue(&NSString::from_str(state.hint));
        unsafe {
            let _: () = msg_send![&*ui.streaming_switch, setAccessibilityHelp: &*NSString::from_str(state.hint)];
        }
    }
}

fn apply_and_restart(ui: &SettingsUi) {
    let pending = ui.pending.lock().unwrap().clone();
    let corrections = ui.pending_corrections.lock().unwrap();
    let corrections_changed = corrections.to_editor_lines() != ui.original_correction_lines;
    if pending == ui.original && !corrections_changed {
        // Nothing changed — don't restart for nothing.
        ui.window.close();
        return;
    }
    if pending != ui.original {
        if let Err(e) = pending.save() {
            eprintln!("[settings] failed to save settings: {e}");
            return;
        }
    }
    if corrections_changed {
        if let Err(e) = corrections.save() {
            eprintln!("[settings] failed to save dictionary: {e}");
            return;
        }
    }
    menubar::relaunch_daemon();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_totals_keep_counts_separate_from_storage_size() {
        assert_eq!(
            recovery_usage_summary(0, 0, 3),
            "0 audio clips · 0.0 MB · 3 recovery records"
        );
        assert_eq!(
            recovery_usage_summary(2, 1536 * 1024, 4),
            "2 audio clips · 1.5 MB · 4 recovery records"
        );
        assert_eq!(
            recovery_usage_summary(8, 1024 * 1024 * 1024, 10),
            "8 audio clips · 1.0 GB · 10 recovery records"
        );
    }

    #[test]
    fn history_count_line_reads_naturally_at_every_size() {
        assert_eq!(
            history_count_line(0),
            "Nothing stored yet. Hold your hotkey and speak."
        );
        assert_eq!(history_count_line(1), "1 dictation stored.");
        assert_eq!(history_count_line(7), "7 dictations stored.");
    }

    #[test]
    fn history_preview_flattens_and_truncates() {
        assert_eq!(history_preview("one\ntwo"), "one two");
        let long = "a".repeat(60);
        let preview = history_preview(&long);
        assert!(preview.ends_with('…'));
        assert_eq!(preview.chars().count(), 43, "42 chars plus the ellipsis");
    }

    #[test]
    fn history_preview_counts_characters_not_bytes() {
        // A 42-character CJK line is 126 bytes; slicing by byte would panic or
        // cut a character in half.
        let cjk = "字".repeat(42);
        assert_eq!(history_preview(&cjk), cjk);
        let longer = "字".repeat(43);
        assert_eq!(history_preview(&longer).chars().count(), 43);
    }

    #[test]
    fn history_pane_index_points_at_the_history_section() {
        assert_eq!(PANES[HISTORY_PANE].0, "History");
    }
}
