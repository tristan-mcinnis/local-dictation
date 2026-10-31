//! Dictation history — the launcher panel.
//!
//! The window is the house floating panel: `LAYOUT_PANEL_WIDTH` wide, the same
//! glass as the pill, with a `CONTROL_INPUT` search row on top, day sections in
//! the `section` style, `CONTROL_ROW` rows carrying a 26 px icon tile, and a
//! `CONTROL_FOOTER` well along the bottom. Clicking a row still copies its full
//! text; the confirmation now lands in the footer instead of a title bar,
//! because a borderless panel has none.
//!
//! Escape and ⌘W close it, and it closes when it stops being the key window —
//! the launcher contract, so a panel with no title bar is never stranded.

use super::*;
use objc2_app_kit::NSViewBoundsDidChangeNotification;
use objc2_foundation::{NSNotification, NSNotificationCenter};
use std::sync::atomic::AtomicUsize;

static HISTORY_VISIBLE: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
static HISTORY_SELECTED: AtomicUsize = AtomicUsize::new(0);

// ─── Panel window ────────────────────────────────────────────────────────

define_class!(
    #[unsafe(super(NSWindow))]
    #[name = "LDPanelWindow"]
    pub(super) struct PanelWindow;

    impl PanelWindow {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool {
            true
        }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main(&self) -> bool {
            true
        }

        /// Escape. AppKit routes it here once nothing in the responder chain
        /// consumes it, which is what makes a chromeless panel dismissible.
        #[unsafe(method(cancelOperation:))]
        fn cancel_operation(&self, _sender: *mut AnyObject) {
            self.orderOut(None);
        }

        /// Clicking away dismisses the panel — the launcher contract. A window
        /// with no title bar has no close button, so this is what guarantees it
        /// can never be stranded on screen.
        #[unsafe(method(resignKeyWindow))]
        fn resign_key_window(&self) {
            unsafe {
                let _: () = msg_send![super(self), resignKeyWindow];
            }
            if self.isVisible() {
                self.orderOut(None);
            }
        }

        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: *mut AnyObject) -> objc2::runtime::Bool {
            unsafe {
                let flags: usize = msg_send![event, modifierFlags];
                let chars: Retained<NSString> = msg_send![event, charactersIgnoringModifiers];
                if HISTORY_UI.get().is_some_and(|ui| std::ptr::eq(self, &*ui.window)) {
                    let modifiers = flags & ((1 << 20) | (1 << 19) | (1 << 18) | (1 << 17));
                    if modifiers == (1 << 20) && chars.to_string().eq_ignore_ascii_case("o") {
                        open_selected_history_entry();
                        return objc2::runtime::Bool::YES;
                    }
                    if modifiers == ((1 << 20) | (1 << 17)) && chars.to_string().eq_ignore_ascii_case("c") {
                        copy_history_entry_at(HISTORY_SELECTED.load(Ordering::SeqCst) as isize);
                        return objc2::runtime::Bool::YES;
                    }
                }
                // NSEventModifierFlagCommand
                if flags & (1 << 20) != 0 && chars.to_string() == "w" {
                    self.orderOut(None);
                    return objc2::runtime::Bool::YES;
                }
                msg_send![super(self), performKeyEquivalent: event]
            }
        }
    }
);

/// Build a borderless, rounded, floating panel window sized `size`.
pub(super) fn panel_window(mtm: MainThreadMarker, size: NSSize) -> Retained<PanelWindow> {
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), size);
    let window: Retained<PanelWindow> = unsafe {
        msg_send![
            PanelWindow::alloc(mtm),
            initWithContentRect: frame,
            styleMask: NSWindowStyleMask::Borderless,
            backing: NSBackingStoreType::Buffered,
            defer: false
        ]
    };
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    window.setLevel(NSWindowLevel::from(3_isize));
    // AppKit's own floating-window shadow is the two panel shadows the rule
    // asks for; drawing them again by hand would double them.
    window.setHasShadow(true);
    unsafe {
        window.setReleasedWhenClosed(false);
    }
    window.setMovableByWindowBackground(true);
    window
}

// ─── History window ──────────────────────────────────────────────────────

pub(super) struct HistoryUi {
    pub(super) window: Retained<PanelWindow>,
    /// The glass panel — also the window's content view and what the offscreen
    /// renderer snapshots.
    pub(super) panel: Retained<NSView>,
    /// Search field pinned to the top of the panel; filters the list live.
    pub(super) search: Retained<NSSearchField>,
    pub(super) scroll: Retained<NSScrollView>,
    /// Flipped document view that holds the section labels and rows. Rebuilt
    /// from scratch on every open.
    pub(super) doc: Retained<FlippedView>,
    /// Left-hand footer text: the status line, or the copy confirmation.
    pub(super) footer_status: Retained<NSTextField>,
}
// Only ever touched on the main thread (created from / used by menu actions),
// so the raw AppKit pointers are safe to park in a static.
unsafe impl Send for HistoryUi {}
unsafe impl Sync for HistoryUi {}

pub(super) static HISTORY_UI: OnceLock<HistoryUi> = OnceLock::new();

pub(super) fn history_ui(mtm: MainThreadMarker) -> &'static HistoryUi {
    HISTORY_UI.get_or_init(|| build_history_window(mtm))
}

/// Panel height. The search row, both dividers and the footer well are token
/// heights; what is left is the list.
const PANEL_H: f64 = 480.0;
/// Horizontal padding inside the list, so a row's selection fill sits inboard
/// of the panel edge.
const LIST_PAD: f64 = 10.0;
/// The footer's right-hand key hints, and the status shown before any copy.
const FOOTER_READY: &str = "Audio and text layers";

pub(super) fn build_history_window(mtm: MainThreadMarker) -> HistoryUi {
    let keycode = Settings::load().hotkey_keycode.unwrap_or(crate::hotkeys::DEFAULT_KEYCODE);
    build_history_with_hotkey(mtm, keycode)
}

pub(super) fn build_history_for_render(mtm: MainThreadMarker) -> HistoryUi {
    build_history_with_hotkey(mtm, crate::hotkeys::DEFAULT_KEYCODE)
}

fn build_history_with_hotkey(mtm: MainThreadMarker, keycode: i64) -> HistoryUi {
    let win_w = LAYOUT_PANEL_WIDTH;
    let window = panel_window(mtm, NSSize::new(win_w, PANEL_H));
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(win_w, PANEL_H));
    let (glass, _tint) = slate::glass(mtm, frame, RADIUS_XL);

    // ── Search row ─────────────────────────────────────────────────────
    let search_y = PANEL_H - CONTROL_INPUT;
    let search_mid = search_y + CONTROL_INPUT / 2.0;

    if let Some(img) = slate::template_symbol("magnifyingglass") {
        let iv = NSImageView::new(mtm);
        iv.setImage(Some(&img));
        unsafe {
            let _: () = msg_send![&*iv, setContentTintColor: &*theme::text_tertiary()];
        }
        iv.setFrame(NSRect::new(
            NSPoint::new(SPACE_LG, (search_mid - SEARCH_GLYPH / 2.0).round()),
            NSSize::new(SEARCH_GLYPH, SEARCH_GLYPH),
        ));
        glass.addSubview(&iv);
    }

    // "🎙 Right ⌥ · hold" — the chip that says how to start a dictation.
    let hotkey_label = format!(
        "{} · hold",
        settings::hotkey_name(keycode)
    );
    let chip = slate::chip(
        mtm,
        &hotkey_label,
        Some("mic"),
        win_w - SPACE_LG,
        search_mid,
    );
    let chip_x = chip.frame().origin.x;
    glass.addSubview(&chip);

    let field_x = SPACE_LG + SEARCH_GLYPH + SPACE_SM;
    let search: Retained<NSSearchField> = unsafe {
        msg_send![
            NSSearchField::alloc(mtm),
            initWithFrame: NSRect::new(
                NSPoint::new(field_x, (search_mid - SEARCH_FIELD_H / 2.0).round()),
                NSSize::new(chip_x - SPACE_SM - field_x, SEARCH_FIELD_H),
            )
        ]
    };
    unsafe {
        // Send the action on every keystroke, not just on Return, so the list
        // filters as you type.
        let _: () = msg_send![&*search, setSendsWholeSearchString: false];
        let _: () = msg_send![&*search, setSendsSearchStringImmediately: true];
        let ph = NSString::from_str("Search dictations…");
        let _: () = msg_send![&*search, setPlaceholderString: &*ph];
        // The house search row draws its own glyph, so strip the cell's bezel
        // and its built-in magnifier / cancel buttons.
        search.setBezeled(false);
        search.setDrawsBackground(false);
        let _: () = msg_send![&*search, setFocusRingType: 1usize]; // None
        search.setFont(Some(&slate::font(TYPE_INPUT, WEIGHT_REGULAR)));
        let _: () = msg_send![&*search, setTextColor: &*theme::text_primary()];
        let cell: *mut AnyObject = msg_send![&*search, cell];
        if !cell.is_null() {
            let _: () = msg_send![cell, setSearchButtonCell: std::ptr::null_mut::<AnyObject>()];
            let _: () = msg_send![cell, setCancelButtonCell: std::ptr::null_mut::<AnyObject>()];
        }
        // Wire the search action to the shared menu-action target (set during
        // init_and_run, well before the window is first built).
        if let Some(actions) = GLOBALS.get().map(|g| &g._actions) {
            let _: () = msg_send![&*search, setTarget: &**actions];
            let _: () = msg_send![&*search, setAction: sel!(searchHistory:)];
            let _: () = msg_send![&*search, setDelegate: &**actions];
        }
    }
    glass.addSubview(&search);

    // ── Dividers: search / list / footer ───────────────────────────────
    let top_divider = slate::SlateView::new(
        mtm,
        NSRect::new(
            NSPoint::new(0.0, search_y - HAIRLINE),
            NSSize::new(win_w, HAIRLINE),
        ),
        slate::Paint::Divider,
        0.0,
    );
    glass.addSubview(&top_divider);

    let bottom_divider = slate::SlateView::new(
        mtm,
        NSRect::new(
            NSPoint::new(0.0, CONTROL_FOOTER),
            NSSize::new(win_w, HAIRLINE),
        ),
        slate::Paint::Divider,
        0.0,
    );
    glass.addSubview(&bottom_divider);

    // ── List ───────────────────────────────────────────────────────────
    let scroll_y = CONTROL_FOOTER + HAIRLINE;
    let scroll_h = search_y - HAIRLINE - scroll_y;
    let scroll: Retained<NSScrollView> = unsafe {
        msg_send![
            NSScrollView::alloc(mtm),
            initWithFrame: NSRect::new(NSPoint::new(0.0, scroll_y), NSSize::new(win_w, scroll_h))
        ]
    };
    {
        scroll.setHasVerticalScroller(true);
        scroll.setAutohidesScrollers(true);
        // The panel's own glass is the ground; an opaque scroll background
        // would punch a hole in it.
        scroll.setDrawsBackground(false);
    }
    let content = scroll.contentSize();
    let doc: Retained<FlippedView> = unsafe {
        msg_send![
            FlippedView::alloc(mtm),
            initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), content)
        ]
    };
    scroll.setDocumentView(Some(&*doc));
    // Rows are built a page at a time; scrolling near the end builds the next.
    {
        let clip = scroll.contentView();
        clip.setPostsBoundsChangedNotifications(true);
        let block = block2::RcBlock::new(|_note: std::ptr::NonNull<NSNotification>| {
            load_more_if_near_end();
        });
        let center = NSNotificationCenter::defaultCenter();
        let token = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(NSViewBoundsDidChangeNotification),
                Some(&*clip),
                None,
                &block,
            )
        };
        // The history window lives for the whole process.
        std::mem::forget(token);
    }
    glass.addSubview(&scroll);

    // ── Footer well ────────────────────────────────────────────────────
    let footer = slate::SlateView::new(
        mtm,
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(win_w, CONTROL_FOOTER)),
        slate::Paint::Well,
        0.0,
    );
    let fmid = CONTROL_FOOTER / 2.0;
    let dot = slate::status_dot(mtm, true, SPACE_MD, fmid);
    footer.addSubview(&dot);
    let footer_status = slate::label(
        mtm,
        FOOTER_READY,
        TYPE_META,
        WEIGHT_REGULAR,
        theme::text_secondary(),
    );
    slate::place_left(&footer_status, SPACE_MD + slate::DOT + PILL_GAP, fmid);
    footer.addSubview(&footer_status);

    // Right side: "Copy ↩ · Actions ⌘ K", outlined key caps.
    let mut right = win_w - SPACE_SM;
    for (text, caps) in [("Copy", &["⇧", "⌘", "C"][..]), ("Open", &["↩"][..])] {
        let (views, width) = slate::key_hint(mtm, text, caps, 0.0, fmid);
        let x = right - width;
        for v in &views {
            let f = v.frame();
            v.setFrameOrigin(NSPoint::new(f.origin.x + x, f.origin.y));
            footer.addSubview(v);
        }
        right = x - SPACE_SM;
    }
    glass.addSubview(&footer);

    {
        window.setContentView(Some(&*glass));
        window.center();
    }

    HistoryUi {
        window,
        panel: glass,
        search,
        scroll,
        doc,
        footer_status,
    }
}

/// Leading magnifier glyph in the search row.
const SEARCH_GLYPH: f64 = 18.0;
/// The editable band inside the 58 px search row.
const SEARCH_FIELD_H: f64 = 24.0;

/// Shown in place of the list while the first read is in flight, when the
/// busy store has no cached list to fall back on, or when the store failed.
static HISTORY_EMPTY_NOTE: Mutex<Option<String>> = Mutex::new(None);
/// Bumped on every open; a read that finishes for an older open is dropped.
static HISTORY_LOAD_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

const HISTORY_BUSY_NOTE: &str = "History is busy, try again.";

fn set_empty_note(note: Option<String>) {
    if let Ok(mut n) = HISTORY_EMPTY_NOTE.lock() {
        *n = note;
    }
}

fn set_footer(ui: &HistoryUi, text: &str) {
    ui.footer_status.setStringValue(&NSString::from_str(text));
    slate::place_left(
        &ui.footer_status,
        SPACE_MD + slate::DOT + PILL_GAP,
        CONTROL_FOOTER / 2.0,
    );
}

/// Say something in the History panel's footer (a failed click-through, say).
pub(super) fn history_footer_note(text: &str) {
    if let Some(ui) = HISTORY_UI.get() {
        set_footer(ui, text);
    }
}

/// Open the panel at once and read the history on a background thread. The
/// read waits only briefly for a writer; the main thread, which also carries
/// the hotkey event tap, never waits on SQLite.
pub(super) fn show_history_window() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let ui = history_ui(mtm);

    // Show what the last open cached right away (search cleared), or a
    // loading line when there is nothing cached yet.
    let cached = HISTORY_ALL.lock().map(|g| !g.is_empty()).unwrap_or(false);
    if cached {
        set_empty_note(None);
        apply_history_filter("");
    } else {
        set_empty_note(Some("Loading…".to_string()));
        HISTORY_SELECTED.store(0, Ordering::SeqCst);
        rebuild_history_list(mtm, ui, &[]);
    }
    set_footer(ui, FOOTER_READY);
    ui.search.setStringValue(&NSString::from_str(""));
    ui.window.makeKeyAndOrderFront(None);
    ui.window.makeFirstResponder(Some(&*ui.search));
    // We're an Accessory app (no Dock icon); without an explicit activate the
    // new window opens behind the frontmost app.
    let app = NSApplication::sharedApplication(mtm);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);

    let generation = HISTORY_LOAD_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    std::thread::spawn(move || {
        let read = crate::history::recent_ui(1000);
        // Only an empty answer needs the (second) probe for "why".
        let reason = match &read {
            crate::history::UiRead::Ok(v) if v.is_empty() => crate::history::unreadable_reason(),
            _ => None,
        };
        run_on_main(move || finish_history_load(generation, read, reason));
    });
}

/// Main-thread half of an open: apply the read, or keep the previous list.
fn finish_history_load(
    generation: u64,
    read: crate::history::UiRead<Vec<Entry>>,
    reason: Option<String>,
) {
    use crate::history::UiRead;
    let (Some(mtm), Some(ui)) = (MainThreadMarker::new(), HISTORY_UI.get()) else {
        return;
    };
    if generation != HISTORY_LOAD_GEN.load(Ordering::SeqCst) {
        return;
    }
    let cached = HISTORY_ALL.lock().map(|g| !g.is_empty()).unwrap_or(false);
    match read {
        UiRead::Ok(entries) => {
            set_empty_note(reason.map(|r| crate::history::empty_state_text(Some(&r), false)));
            if let Ok(mut all) = HISTORY_ALL.lock() {
                *all = entries;
            }
            // Honour a query typed while the read was in flight.
            let query = ui.search.stringValue().to_string();
            apply_history_filter(&query);
        }
        UiRead::Busy => {
            set_footer(ui, HISTORY_BUSY_NOTE);
            if !cached {
                set_empty_note(Some(HISTORY_BUSY_NOTE.to_string()));
                rebuild_history_list(mtm, ui, &[]);
            }
        }
        UiRead::Failed(why) => {
            set_footer(ui, "History could not be read.");
            if !cached {
                set_empty_note(Some(crate::history::empty_state_text(Some(&why), false)));
                rebuild_history_list(mtm, ui, &[]);
            }
        }
    }
}

/// Filter the cached history by a case-insensitive substring query and rebuild
/// the list. An empty/whitespace query restores the full list. Keeps
/// HISTORY_ENTRIES in lock-step with the rows shown so click-to-copy tags stay
/// valid against the *filtered* view.
pub(super) fn apply_history_filter(query: &str) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Some(ui) = HISTORY_UI.get() else {
        return;
    };
    let filtered = {
        let all = HISTORY_ALL.lock().map(|g| g.clone()).unwrap_or_default();
        filter_entries(all, query)
    };

    if let Ok(mut store) = HISTORY_ENTRIES.lock() {
        *store = filtered.iter().map(|e| e.text.clone()).collect();
    }
    *HISTORY_VISIBLE.lock().unwrap() = filtered.clone();
    HISTORY_SELECTED.store(0, Ordering::SeqCst);
    rebuild_history_list(mtm, ui, &filtered);
}

/// The entries whose text contains `query`, case-insensitively. A blank query
/// keeps everything.
pub(super) fn filter_entries(all: Vec<Entry>, query: &str) -> Vec<Entry> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return all;
    }
    all.into_iter()
        .filter(|e| e.text.to_lowercase().contains(&needle))
        .collect()
}

pub(super) fn open_history_entry_at(tag: isize) {
    let id = usize::try_from(tag).ok().and_then(|index| HISTORY_VISIBLE.lock().ok()?.get(index).map(|entry| entry.id));
    if let Some(id) = id {
        HISTORY_SELECTED.store(tag as usize, Ordering::SeqCst);
        if let Some(ui) = HISTORY_UI.get() { select_row(ui, tag); }
        show_history_detail(id);
    }
}

pub(super) fn open_selected_history_entry() {
    open_history_entry_at(HISTORY_SELECTED.load(Ordering::SeqCst) as isize);
}

pub(super) fn move_history_selection(delta: isize) {
    let count = HISTORY_VISIBLE.lock().map(|entries| entries.len()).unwrap_or(0);
    if count == 0 { return; }
    let next = (HISTORY_SELECTED.load(Ordering::SeqCst) as isize + delta).clamp(0, count as isize - 1) as usize;
    HISTORY_SELECTED.store(next, Ordering::SeqCst);
    ensure_row_built(next);
    if let Some(ui) = HISTORY_UI.get() { select_row(ui, next as isize); }
}

/// Copy the stored history text at `tag` (its index in HISTORY_ENTRIES) to the
/// clipboard and confirm in the footer well. Out-of-range/empty are no-ops.
pub(super) fn copy_history_entry_at(tag: isize) {
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
    let flat = text.replace('\n', " ");
    let preview: String = flat.chars().take(42).collect();
    let ellipsis = if flat.chars().count() > 42 { "…" } else { "" };
    let message = format!("Copied “{preview}{ellipsis}”");
    copy_to_clipboard(text, move |copied| {
        if let Some(ui) = HISTORY_UI.get() {
            let message = if copied { message.as_str() } else { "Could not copy this dictation." };
            ui.footer_status.setStringValue(&NSString::from_str(message));
            slate::place_left(&ui.footer_status, SPACE_MD + slate::DOT + PILL_GAP, CONTROL_FOOTER / 2.0);
        }
    });
    // Keep the row highlighted so the copied entry is obvious.
    if let Some(ui) = HISTORY_UI.get() {
        select_row(ui, tag);
    }
}

/// Move the selection fill to the row whose tag is `tag`.
fn select_row(ui: &HistoryUi, tag: isize) {
    for view in ui.doc.subviews().iter() {
        let is_slate: bool = unsafe { msg_send![&*view, isKindOfClass: <slate::SlateView as objc2::ClassType>::class()] };
        if !is_slate {
            continue;
        }
        let row: &slate::SlateView =
            unsafe { &*(&*view as *const NSView as *const slate::SlateView) };
        let row_tag: isize = unsafe { msg_send![row, tag] };
        row.set_selected(row_tag == tag);
        if row_tag == tag { ui.doc.scrollRectToVisible(row.frame()); }
    }
}

/// One rendered row: either a day header or a single dictation. `index` is the
/// entry's position in the (newest-first) input — the same index used as the
/// row's `tag` and as the key into HISTORY_ENTRIES.
#[derive(Debug)]
pub(crate) enum HistoryRow {
    Day(String),
    Item {
        time: String,
        text: String,
        index: usize,
    },
}

/// Group entries by local calendar day (newest first) into a flat row list.
/// `NSDateFormatter` renders dates/times in the user's locale + timezone.
pub(crate) fn history_rows(entries: &[Entry]) -> Vec<HistoryRow> {
    history_rows_range(entries, 0, entries.len(), None)
}

/// Rows for `entries[from..to]` only, with `index` still counting from the
/// start of `entries`. `last_day` is the day label of the row before `from`,
/// so a page that continues a day does not repeat its header.
fn history_rows_range(
    entries: &[Entry],
    from: usize,
    to: usize,
    mut last_day: Option<String>,
) -> Vec<HistoryRow> {
    let day_fmt = NSDateFormatter::new();
    day_fmt.setDateFormat(Some(&NSString::from_str("MMMM d, yyyy")));
    let time_fmt = NSDateFormatter::new();
    time_fmt.setDateFormat(Some(&NSString::from_str("HH:mm")));

    let mut rows = Vec::new();
    let to = to.min(entries.len());
    for (i, e) in entries.iter().enumerate().take(to).skip(from) {
        let date = NSDate::dateWithTimeIntervalSince1970(e.created_at as f64);
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

/// Entries built into rows per page. The list is a window onto the history,
/// not a copy of it: a thousand eager rows is six thousand views and a
/// multi-hundred-millisecond stall on the main run loop, which also carries
/// the hotkey event tap.
const HISTORY_PAGE: usize = 50;
/// Load the next page when the visible edge is within this many points of the
/// last built row.
const LOAD_AHEAD: f64 = 240.0;

/// Where the built part of the list ends, so the next page continues it.
struct ListCursor {
    /// Y of the next row in the flipped document view.
    y: f64,
    /// Index of the next entry to build.
    next: usize,
}

static LIST_CURSOR: Mutex<ListCursor> = Mutex::new(ListCursor { y: 0.0, next: 0 });

/// The entries to build next: from `next` up to `upto` (exclusive), clamped to
/// the list length. `None` when there is nothing left to build. Pure.
fn page_range(next: usize, len: usize, upto: usize) -> Option<std::ops::Range<usize>> {
    let end = upto.min(len);
    (next < end).then_some(next..end)
}

/// Rows for one page. The day label before the page comes from the entry
/// just ahead of it, so a page that continues a day does not repeat the
/// header and a page that starts a new day gets one. Pure apart from date
/// formatting.
fn rows_for_page(entries: &[Entry], page: std::ops::Range<usize>) -> Vec<HistoryRow> {
    let carry = page
        .start
        .checked_sub(1)
        .and_then(|i| entries.get(i))
        .map(|e| history_day_label(e.created_at));
    history_rows_range(entries, page.start, page.end, carry)
}

/// Whether the visible edge is close enough to the end of the built rows to
/// build another page. Pure.
fn needs_more(visible_bottom: f64, built_to: f64, next: usize, len: usize) -> bool {
    next < len && visible_bottom + LOAD_AHEAD >= built_to
}

/// Build more rows when the user has scrolled near the end of the built part.
pub(super) fn load_more_if_near_end() {
    let (Some(mtm), Some(ui)) = (MainThreadMarker::new(), HISTORY_UI.get()) else {
        return;
    };
    let visible = ui.scroll.documentVisibleRect();
    let (built_to, next) = LIST_CURSOR.lock().map(|c| (c.y, c.next)).unwrap_or((0.0, 0));
    // Check under the lock and return before touching anything when there is
    // nothing to build: a scroll event fires on every pixel.
    let Ok(entries) = HISTORY_VISIBLE.lock() else { return };
    if needs_more(visible.origin.y + visible.size.height, built_to, next, entries.len()) {
        append_history_rows(mtm, ui, &entries, next + HISTORY_PAGE);
    }
}

/// Make sure the row for entry `index` exists, so keyboard selection can walk
/// past the built part.
fn ensure_row_built(index: usize) {
    let (Some(mtm), Some(ui)) = (MainThreadMarker::new(), HISTORY_UI.get()) else {
        return;
    };
    if LIST_CURSOR.lock().map(|c| index < c.next).unwrap_or(true) {
        return;
    }
    let Ok(entries) = HISTORY_VISIBLE.lock() else { return };
    append_history_rows(mtm, ui, &entries, index + 1);
}

/// Tear down and rebuild the day-grouped list.
pub(super) fn rebuild_history_list(mtm: MainThreadMarker, ui: &HistoryUi, entries: &[Entry]) {

    let doc = &ui.doc;
    let content = ui.scroll.contentSize();
    let width = content.width.max(LAYOUT_PANEL_WIDTH / 2.0);
    let row_w = width - 2.0 * LIST_PAD;

    // Clear previous rows.
    let empty: Retained<NSArray<NSView>> = NSArray::new();
    doc.setSubviews(&empty);

    if entries.is_empty() {
        // Four situations: still loading, the store was busy, the store could
        // not be opened, or simply nothing (yet, or no match). The load result
        // sets the note; this never touches the database, so a rebuild on the
        // main thread cannot wait on a writer.
        let has_history = HISTORY_ALL.lock().map(|g| !g.is_empty()).unwrap_or(false);
        let msg = HISTORY_EMPTY_NOTE
            .lock()
            .ok()
            .and_then(|n| n.clone())
            .unwrap_or_else(|| crate::history::empty_state_text(None, has_history));
        let label = slate::label(
            mtm,
            &msg,
            TYPE_BODY_SMALL,
            WEIGHT_REGULAR,
            theme::text_tertiary(),
        );
        label.setFrame(NSRect::new(
            NSPoint::new(LIST_PAD + SPACE_SM, SPACE_MD),
            NSSize::new(row_w, CONTROL_ROW / 2.0),
        ));
        doc.addSubview(&label);
        doc.setFrame(NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(width, content.height.max(CONTROL_ROW)),
        ));
        if let Ok(mut c) = LIST_CURSOR.lock() {
            *c = ListCursor { y: 0.0, next: 0 };
        }
        return;
    }

    if let Ok(mut c) = LIST_CURSOR.lock() {
        *c = ListCursor { y: SPACE_XS, next: 0 };
    }
    append_history_rows(mtm, ui, entries, HISTORY_PAGE);
    // Flipped view: (0,0) is the top, so this shows the newest entries.
    ui.doc.scrollPoint(NSPoint::new(0.0, 0.0));
}

/// Build rows for `entries` from the cursor up to (not including) `upto`, and
/// grow the document view to fit.
pub(super) fn append_history_rows(mtm: MainThreadMarker, ui: &HistoryUi, entries: &[Entry], upto: usize) {
    /// Section label band: the `section` type plus the mockup's 6 px above and
    /// below it.
    const SECTION_H: f64 = 24.0;

    let doc = &ui.doc;
    let content = ui.scroll.contentSize();
    let width = content.width.max(LAYOUT_PANEL_WIDTH / 2.0);
    let row_w = width - 2.0 * LIST_PAD;
    let (mut y, from) = match LIST_CURSOR.lock() {
        Ok(c) => (c.y, c.next),
        Err(_) => return,
    };
    let Some(page) = page_range(from, entries.len(), upto) else {
        return;
    };
    let upto = page.end;
    // The shared action target whose copyHistoryEntry: handles row clicks.
    let actions = GLOBALS.get().map(|g| &g._actions);
    for row in rows_for_page(entries, page) {
        match row {
            HistoryRow::Day(day) => {
                let label = slate::section_label(mtm, &day);
                label.sizeToFit();
                let h = label.frame().size.height;
                label.setFrame(NSRect::new(
                    NSPoint::new(LIST_PAD + LIST_PAD, (y + (SECTION_H - h) / 2.0).round()),
                    NSSize::new(row_w - 2.0 * LIST_PAD, h),
                ));
                doc.addSubview(&label);
                y += SECTION_H;
            }
            HistoryRow::Item { time, text, index } => {
                let row_view = slate::SlateView::new(
                    mtm,
                    NSRect::new(
                        NSPoint::new(LIST_PAD, y),
                        NSSize::new(row_w, CONTROL_ROW),
                    ),
                    slate::Paint::Row,
                    RADIUS_ROW,
                );
                unsafe {
                    let _: () = msg_send![&*row_view, setTag: index as isize];
                }
                row_view.set_selected(index == 0);
                row_view.enable_hover();
                let mid = CONTROL_ROW / 2.0;

                // 26 px tile carrying the app's glyph. The history store keeps
                // only the text and its timestamp, so every row gets the one
                // glyph this app owns — the waveform.
                let tile = slate::icon_tile(mtm, "waveform", LIST_PAD, mid);
                row_view.addSubview(&tile);

                // Right edge: the time, then the length, both `meta` tertiary.
                let time_label = slate::label(
                    mtm,
                    &time,
                    TYPE_META,
                    WEIGHT_REGULAR,
                    theme::text_tertiary(),
                );
                // Tabular digits, so times line up down the column.
                time_label.setFont(Some(&slate::tabular_font(TYPE_META, WEIGHT_REGULAR)));
                time_label.setAlignment(NSTextAlignment::Right);
                let time_x = row_w - LIST_PAD - TIME_W;
                time_label.setFrame(NSRect::new(
                    NSPoint::new(time_x, (mid - TYPE_META).round()),
                    NSSize::new(TIME_W, 2.0 * TYPE_META),
                ));
                row_view.addSubview(&time_label);

                let words = text.split_whitespace().count();
                let meta = slate::label(
                    mtm,
                    &format!("{words} words"),
                    TYPE_META,
                    WEIGHT_REGULAR,
                    theme::text_tertiary(),
                );
                meta.sizeToFit();
                let meta_w = meta.frame().size.width;
                let meta_x = time_x - SPACE_SM - meta_w;
                meta.setFrame(NSRect::new(
                    NSPoint::new(meta_x, (mid - meta.frame().size.height / 2.0).round()),
                    NSSize::new(meta_w, meta.frame().size.height),
                ));
                row_view.addSubview(&meta);

                // One line of dictation, truncated at the tail.
                let text_x = LIST_PAD + CONTROL_TILE + SPACE_SM;
                let title = slate::label(
                    mtm,
                    &text,
                    TYPE_BODY_SMALL,
                    WEIGHT_MEDIUM,
                    theme::text_primary(),
                );
                title.setFrame(NSRect::new(
                    NSPoint::new(text_x, (mid - TYPE_BODY_SMALL).round()),
                    NSSize::new(
                        (meta_x - SPACE_SM - text_x).max(0.0),
                        2.0 * TYPE_BODY_SMALL,
                    ),
                ));
                row_view.addSubview(&title);

                let full = entries
                    .get(index)
                    .map(|e| e.text.as_str())
                    .unwrap_or(text.as_str());
                if let Some(a) = actions {
                    let btn = slate::click_overlay(
                        mtm,
                        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(row_w, CONTROL_ROW)),
                        a,
                        sel!(openHistoryEntry:),
                        index as isize,
                    );
                    btn.setToolTip(Some(&NSString::from_str(full)));
                    row_view.addSubview(&btn);
                }
                doc.addSubview(&row_view);
                y += CONTROL_ROW;
            }
        }
    }

    if let Ok(mut c) = LIST_CURSOR.lock() {
        *c = ListCursor { y, next: upto };
    }
    let total = y + SPACE_XS;
    doc.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(width, total.max(content.height)),
    ));
}

/// The upper-case day header text for a timestamp, as `history_rows_range`
/// writes it.
fn history_day_label(created_at: i64) -> String {
    let day_fmt = NSDateFormatter::new();
    day_fmt.setDateFormat(Some(&NSString::from_str("MMMM d, yyyy")));
    let date = NSDate::dateWithTimeIntervalSince1970(created_at as f64);
    day_fmt.stringFromDate(&date).to_string().to_uppercase()
}

/// Width of the right-aligned time column, so times line up whatever the
/// locale renders.
const TIME_W: f64 = 52.0;

#[cfg(test)]
mod paging_tests {
    use super::*;

    /// `n` invented entries, 20 minutes apart, newest first: several days.
    fn entries(n: usize) -> Vec<Entry> {
        (0..n)
            .map(|i| Entry {
                id: i as i64 + 1,
                text: format!("invented dictation {i}"),
                created_at: 1_772_000_000 - i as i64 * 1_200,
            })
            .collect()
    }

    /// Build every page the way the panel does and join the rows.
    fn paged(entries: &[Entry]) -> Vec<HistoryRow> {
        let mut next = 0;
        let mut out = Vec::new();
        while let Some(page) = page_range(next, entries.len(), next + HISTORY_PAGE) {
            next = page.end;
            out.extend(rows_for_page(entries, page));
        }
        out
    }

    #[test]
    fn page_range_clamps_and_ends() {
        assert_eq!(page_range(0, 1000, 50), Some(0..50));
        assert_eq!(page_range(50, 1000, 100), Some(50..100));
        assert_eq!(page_range(990, 1000, 1040), Some(990..1000));
        assert_eq!(page_range(1000, 1000, 1050), None);
        assert_eq!(page_range(0, 0, 50), None);
        assert_eq!(page_range(5, 10, 3), None);
    }

    /// Paging must produce exactly the rows one pass produces: no repeated
    /// day header at a seam, none missing where a new day starts.
    #[test]
    fn pages_join_into_the_same_rows_as_one_pass() {
        for n in [1, 49, 50, 51, 100, 237, 1000] {
            let all = entries(n);
            assert_eq!(
                format!("{:?}", paged(&all)),
                format!("{:?}", history_rows(&all)),
                "seam mismatch at n={n}"
            );
        }
    }

    /// A filtered subset is a new list: paging indexes into it, not the full one.
    #[test]
    fn paging_a_filtered_subset_indexes_the_subset() {
        let all = entries(400);
        let subset: Vec<Entry> = all.into_iter().filter(|e| e.id % 7 == 0).collect();
        let rows = paged(&subset);
        assert_eq!(format!("{rows:?}"), format!("{:?}", history_rows(&subset)));
        let last = rows.iter().rev().find_map(|r| match r {
            HistoryRow::Item { index, .. } => Some(*index),
            _ => None,
        });
        assert_eq!(last, Some(subset.len() - 1), "last built index is len-1");
    }

    #[test]
    fn the_last_page_builds_up_to_len_minus_one() {
        let all = entries(123);
        let rows = paged(&all);
        let items = rows.iter().filter(|r| matches!(r, HistoryRow::Item { .. })).count();
        assert_eq!(items, 123);
        assert!(page_range(123, all.len(), 123 + HISTORY_PAGE).is_none());
    }

    #[test]
    fn near_the_end_loads_more_only_while_entries_remain() {
        assert!(needs_more(900.0, 1000.0, 50, 1000));
        assert!(!needs_more(100.0, 1000.0, 50, 1000), "far from the end");
        assert!(!needs_more(990.0, 1000.0, 1000, 1000), "nothing left");
    }
}
