//! Dictation history window.
use super::*;

// ─── Dictation history window ───────────────────────────────────────────
//
// A plain, small native window: a scrollable list of past dictations grouped
// by day. Each dictation is a borderless NSButton — click it to copy the full
// text to the clipboard (the window subtitle confirms what was copied). The
// row shows one truncated line; the full text lives in the button's tooltip
// and is what gets copied. No WebView, no HTML — just AppKit views, built once
// and reused; every open re-queries the DB and rebuilds the rows.

pub(super) struct HistoryUi {
    pub(super) window: Retained<NSWindow>,
    /// Search field pinned to the top of the window; filters the list live.
    pub(super) search: Retained<NSSearchField>,
    pub(super) scroll: Retained<NSScrollView>,
    /// Flipped document view that holds the day-header labels and per-entry
    /// buttons. Rebuilt from scratch on every open.
    pub(super) doc: Retained<FlippedView>,
}
// Only ever touched on the main thread (created from / used by menu actions),
// so the raw AppKit pointers are safe to park in a static.
unsafe impl Send for HistoryUi {}
unsafe impl Sync for HistoryUi {}

pub(super) static HISTORY_UI: OnceLock<HistoryUi> = OnceLock::new();

pub(super) fn history_ui(mtm: MainThreadMarker) -> &'static HistoryUi {
    HISTORY_UI.get_or_init(|| build_history_window(mtm))
}

pub(super) fn build_history_window(mtm: MainThreadMarker) -> HistoryUi {
    const WIN_W: f64 = 460.0;
    const WIN_H: f64 = 580.0;
    const PAD: f64 = 10.0;
    const SEARCH_H: f64 = 24.0;
    // Raw NSView autoresizing masks (NSUInteger bit flags), used directly to
    // avoid depending on the typed constant names: WidthSizable=2,
    // MinYMargin=8 (flexible bottom → stick to top), HeightSizable=16.
    const M_WIDTH_STICK_TOP: usize = 2 | 8;
    const M_WIDTH_HEIGHT: usize = 2 | 16;

    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIN_W, WIN_H));

    // Container holds the search field (top) and the scroll view (below).
    let container: Retained<NSView> =
        unsafe { msg_send![NSView::alloc(mtm), initWithFrame: frame] };

    // ── Search field, pinned to the top ────────────────────────────────────
    let search_frame = NSRect::new(
        NSPoint::new(PAD, WIN_H - PAD - SEARCH_H),
        NSSize::new(WIN_W - 2.0 * PAD, SEARCH_H),
    );
    let search: Retained<NSSearchField> =
        unsafe { msg_send![NSSearchField::alloc(mtm), initWithFrame: search_frame] };
    unsafe {
        // Send the action on every keystroke, not just on Return, so the list
        // filters as you type.
        let _: () = msg_send![&*search, setSendsWholeSearchString: false];
        let _: () = msg_send![&*search, setSendsSearchStringImmediately: true];
        let ph = NSString::from_str("Search dictations");
        let _: () = msg_send![&*search, setPlaceholderString: &*ph];
        let _: () = msg_send![&*search, setAutoresizingMask: M_WIDTH_STICK_TOP];
        // Wire the search action to the shared menu-action target (set during
        // init_and_run, well before the window is first built).
        if let Some(actions) = GLOBALS.get().map(|g| &g._actions) {
            let _: () = msg_send![&*search, setTarget: &**actions];
            let _: () = msg_send![&*search, setAction: sel!(searchHistory:)];
        }
    }
    container.addSubview(&search);

    // ── Scroll view, filling the area below the search field ───────────────
    let scroll_h = WIN_H - 2.0 * PAD - SEARCH_H;
    let scroll: Retained<NSScrollView> = unsafe {
        msg_send![
            NSScrollView::alloc(mtm),
            initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIN_W, scroll_h))
        ]
    };
    {
        scroll.setHasVerticalScroller(true);
        scroll.setAutohidesScrollers(true);
        scroll.setDrawsBackground(true);
        scroll.setBackgroundColor(&theme::panel());
        unsafe {
            let _: () = msg_send![&*scroll, setAutoresizingMask: M_WIDTH_HEIGHT];
        }
    }

    // Document view starts the size of the clip area; rebuild_history_list
    // resizes its height to fit the rows.
    let content = scroll.contentSize();
    let doc: Retained<FlippedView> = unsafe {
        msg_send![
            FlippedView::alloc(mtm),
            initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), content)
        ]
    };
    scroll.setDocumentView(Some(&*doc));
    container.addSubview(&scroll);

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
        window.setContentView(Some(&*container));
        apply_light_chrome(&window);
        window.center();
    }

    HistoryUi {
        window,
        search,
        scroll,
        doc,
    }
}

/// Re-query the history DB, rebuild the list, and bring the window forward.
pub(super) fn show_history_window() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let ui = history_ui(mtm);
    let entries = crate::history::recent(1000);

    // Cache the full (unfiltered) entry list so the search field can filter it
    // without re-querying the DB, then show everything.
    if let Ok(mut all) = HISTORY_ALL.lock() {
        *all = entries.clone();
    }
    // Stash the full texts, newest first, so a clicked row (carrying its index
    // as its tag) can copy the untruncated text.
    if let Ok(mut store) = HISTORY_ENTRIES.lock() {
        *store = entries.iter().map(|e| e.text.clone()).collect();
    }
    rebuild_history_list(mtm, ui, &entries);

    // Reset the subtitle hint and clear any prior search query for the fresh open.
    unsafe {
        let hint = NSString::from_str("Click any entry to copy it");
        let _: () = msg_send![&*ui.window, setSubtitle: &*hint];
        ui.search.setStringValue(&NSString::from_str(""));
        ui.window.makeKeyAndOrderFront(None);
    }
    // We're an Accessory app (no Dock icon); without an explicit activate the
    // new window opens behind the frontmost app.
    let app = NSApplication::sharedApplication(mtm);
    #[allow(deprecated)]
    {
        app.activateIgnoringOtherApps(true)
    };
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
    let all = HISTORY_ALL.lock().map(|g| g.clone()).unwrap_or_default();
    let needle = query.trim().to_lowercase();
    let filtered: Vec<Entry> = if needle.is_empty() {
        all
    } else {
        all.into_iter()
            .filter(|e| e.text.to_lowercase().contains(&needle))
            .collect()
    };

    if let Ok(mut store) = HISTORY_ENTRIES.lock() {
        *store = filtered.iter().map(|e| e.text.clone()).collect();
    }
    rebuild_history_list(mtm, ui, &filtered);
}

/// Copy the stored history text at `tag` (its index in HISTORY_ENTRIES) to the
/// clipboard and confirm via the window subtitle. Out-of-range/empty are no-ops.
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
pub(super) enum HistoryRow {
    Day(String),
    Item {
        time: String,
        text: String,
        index: usize,
    },
}

/// Group entries by local calendar day (newest first) into a flat row list.
/// `NSDateFormatter` renders dates/times in the user's locale + timezone.
pub(super) fn history_rows(entries: &[Entry]) -> Vec<HistoryRow> {
    let day_fmt = NSDateFormatter::new();
    day_fmt.setDateFormat(Some(&NSString::from_str("MMMM d, yyyy")));
    let time_fmt = NSDateFormatter::new();
    time_fmt.setDateFormat(Some(&NSString::from_str("hh:mm a")));

    let mut rows = Vec::new();
    let mut last_day: Option<String> = None;
    for (i, e) in entries.iter().enumerate() {
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

/// Tear down and rebuild the day-grouped list of clickable rows.
pub(super) fn rebuild_history_list(mtm: MainThreadMarker, ui: &HistoryUi, entries: &[Entry]) {
    const MX: f64 = 14.0; // horizontal margin
    const TOP: f64 = 10.0; // top padding
    const ROW_H: f64 = 22.0; // one dictation row
    const HEADER_H: f64 = 28.0; // one day header
    const DAY_GAP: f64 = 8.0; // extra space above each day after the first

    let doc = &ui.doc;
    let content = ui.scroll.contentSize();
    let width = content.width.max(200.0);
    let row_w = width - 2.0 * MX;

    // Clear previous rows.
    let empty: Retained<NSArray<NSView>> = NSArray::new();
    doc.setSubviews(&empty);

    if entries.is_empty() {
        // Distinguish "history is empty" from "search matched nothing".
        let has_history = HISTORY_ALL.lock().map(|g| !g.is_empty()).unwrap_or(false);
        let msg = if has_history {
            "No dictations match your search."
        } else {
            "No dictations yet — hold your hotkey and speak."
        };
        let label = NSTextField::labelWithString(&NSString::from_str(msg), mtm);
        label.setFrame(NSRect::new(
            NSPoint::new(MX, TOP + 6.0),
            NSSize::new(row_w, 20.0),
        ));
        label.setTextColor(Some(&NSColor::secondaryLabelColor()));
        doc.addSubview(&label);
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
                label.setTextColor(Some(&theme::text_secondary()));
                doc.addSubview(&label);
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
    doc.scrollPoint(NSPoint::new(0.0, 0.0));
}

