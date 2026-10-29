//! `render-ui <out_dir>` — snapshot every surface, offscreen, in both
//! appearances.
//!
//! The whole point is to be able to look at the design without putting a
//! window in front of whoever is using the Mac. Nothing here calls
//! `orderFront`, `makeKeyAndOrderFront` or `activate`: each window is built,
//! its content view is drawn straight into a bitmap with
//! `cacheDisplay(in:to:)`, and the process exits.
//!
//! Blur materials render as nothing in a window that was never ordered in, so
//! `slate::set_opaque_materials(true)` makes `Paint::Panel` fill with the
//! panel tint at full alpha instead. Everything else — strokes, highlights,
//! wells, tiles, key caps, rows, toggles — draws exactly as it does on screen.

use super::house_theme::Rgba;
use super::*;
use objc2_foundation::NSDictionary;

use crate::history::Entry;

/// Render every surface into `out_dir` and return the files written.
pub fn render_ui(out_dir: &str) -> eyre::Result<Vec<PathBuf>> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| eyre::eyre!("render_ui must run on the main thread"))?;
    std::fs::create_dir_all(out_dir)?;

    let app = NSApplication::sharedApplication(mtm);
    // Prohibited: no Dock icon, no menu bar, nothing the user can see.
    app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    slate::set_opaque_materials(true);

    let actions = MenuActions::new();
    let pill = build_pill_window(mtm, &actions, PillSize::Compact)?;
    let standard_pill = build_pill_window(mtm, &actions, PillSize::Standard)?;
    let history = build_history_for_render(mtm);
    let detail = build_history_detail(mtm);
    let detail_audio = write_history_audio_fixture(out_dir)?;
    let dictionary = build_dictionary_window(mtm);
    let setup = setup::build_setup_window(mtm);
    let settings = crate::settings_window::build_for_render(mtm);

    // Six rows over two days, matching the mockup's sample list. The same
    // rows fill the Settings History pane, so both surfaces are proved on one
    // known list rather than on whatever this Mac dictated today.
    seed_sample_history(mtm, &history);
    let sample_entries = HISTORY_ALL.lock().map(|g| g.clone()).unwrap_or_default();
    crate::settings_window::seed_history_for_render(&settings, &sample_entries);
    crate::settings_window::verify_history_render_fixture(&settings, sample_entries.len())?;
    verify_history_scale(mtm, &history)?;
    seed_sample_history(mtm, &history);
    dictionary
        .text_view
        .setString(&NSString::from_str(SAMPLE_DICTIONARY));

    let mut written = Vec::new();
    for (suffix, appearance_name) in [
        ("dark", unsafe { NSAppearanceNameDarkAqua }),
        ("light", unsafe { NSAppearanceNameAqua }),
    ] {
        let Some(appearance) = NSAppearance::appearanceNamed(appearance_name) else {
            continue;
        };
        app.setAppearance(Some(&appearance));
        for w in [
            &*pill.window,
            &*standard_pill.window,
            &*history.window as &NSWindow,
            &*detail.window as &NSWindow,
            &*dictionary.window,
            &*setup.window,
            &*settings.window,
        ] {
            w.setAppearance(Some(&appearance));
        }

        // Pure AppKit snapshots exercise the real control hit regions without
        // activating a window, recording audio, or sending a cancel command.
        for (name, layout, state, status, hovered, quiet) in [
            (
                "pill-recording",
                PillLayout::Recording,
                UiState::Recording,
                "",
                false,
                false,
            ),
            (
                "pill-silence",
                PillLayout::Recording,
                UiState::Recording,
                "",
                false,
                true,
            ),
            (
                "pill-hover",
                PillLayout::Recording,
                UiState::Recording,
                "",
                true,
                false,
            ),
            (
                "pill-preview",
                PillLayout::Recording,
                UiState::Recording,
                "Please move the meeting to Thursday afternoon.",
                false,
                false,
            ),
            (
                "pill-transcribing",
                PillLayout::Processing,
                UiState::Transcribing,
                "Transcribing…",
                false,
                false,
            ),
            (
                "pill-cleaning",
                PillLayout::Processing,
                UiState::Cleaning,
                "Cleaning up…",
                false,
                false,
            ),
            (
                "pill-processing",
                PillLayout::Processing,
                UiState::Processing,
                "Working…",
                false,
                false,
            ),
            (
                "pill-processing-hover",
                PillLayout::Processing,
                UiState::Cleaning,
                "Cleaning up…",
                true,
                false,
            ),
            (
                "pill-pasting",
                PillLayout::Processing,
                UiState::Pasting,
                "Pasting…",
                true,
                false,
            ),
            (
                "pill-success",
                PillLayout::Success,
                UiState::Success,
                "Landed · 38 words",
                false,
                false,
            ),
            (
                "pill-copied",
                PillLayout::Copied,
                UiState::Copied,
                "Copied to clipboard",
                false,
                false,
            ),
            (
                "pill-held",
                PillLayout::Error,
                UiState::NoEditableField,
                crate::last_dictation::kept_pill_text(UiState::NoEditableField).unwrap(),
                false,
                false,
            ),
            (
                "pill-held-copied",
                PillLayout::Error,
                UiState::HeldCopied,
                crate::last_dictation::kept_pill_text(UiState::HeldCopied).unwrap(),
                false,
                false,
            ),
            (
                "pill-paste-failed-copied",
                PillLayout::Error,
                UiState::PasteFailedCopied,
                crate::last_dictation::kept_pill_text(UiState::PasteFailedCopied).unwrap(),
                false,
                false,
            ),
            (
                "pill-limit-copied",
                PillLayout::Error,
                UiState::LimitReachedCopied,
                crate::last_dictation::kept_pill_text(UiState::LimitReachedCopied).unwrap(),
                false,
                false,
            ),
            (
                "pill-nothing-to-copy",
                PillLayout::Copied,
                UiState::NothingToCopy,
                super::pill::NOTHING_TO_COPY_TEXT,
                false,
                false,
            ),
            (
                "pill-recording-storage-warning",
                PillLayout::Recording,
                UiState::Recording,
                "Recovery warning",
                false,
                false,
            ),
            (
                "pill-processing-storage-warning",
                PillLayout::Processing,
                UiState::Transcribing,
                "Transcribing… · Recovery warning",
                false,
                false,
            ),
            (
                "pill-paste-failed",
                PillLayout::Error,
                UiState::PasteFailed,
                crate::last_dictation::kept_pill_text(UiState::PasteFailed).unwrap(),
                false,
                false,
            ),
            (
                "pill-error",
                PillLayout::Retryable,
                UiState::Error,
                "Couldn’t transcribe",
                false,
                false,
            ),
            (
                "pill-rewrite-failed",
                PillLayout::Error,
                UiState::RewriteFailed,
                "Couldn’t rewrite · Selection unchanged",
                false,
                false,
            ),
        ] {
            pill.storage_warning.set(name.ends_with("storage-warning"));
            pill.status.setStringValue(&NSString::from_str(status));
            pill.hint
                .setStringValue(&NSString::from_str(if state == UiState::Error || state.is_kept_result() {
                    "History"
                } else {
                    "Undo"
                }));
            pill.retry_hint.setStringValue(&NSString::from_str(if state == UiState::Error { "Retry" } else { "" }));
            pill.waveform.set_heights(if quiet {
                waveform_targets(&[0.006; BAR_COUNT], WaveformSensitivity::Balanced)
            } else {
                sample_levels(false)
            });
            let width = layout_pill(&pill, layout);
            if state.is_active() {
                set_cancel_hover(&pill, state, hovered);
            }
            assert_eq!(pill.cancel.isHidden(), !state.is_cancellable());
            assert!(!pill.window.canBecomeKeyWindow() && !pill.window.canBecomeMainWindow());
            if state.is_cancellable() {
                assert!(pill.cancel.frame().size.width >= 24.0);
                assert!(pill.cancel.frame().size.height >= 24.0);
                assert_eq!(pill.cancel.keyEquivalent().to_string(), "\u{1b}");
                assert!(pill.cancel.keyEquivalentModifierMask().is_empty());
            }
            if matches!(state, UiState::Copied | UiState::NothingToCopy) {
                assert!(pill.undo.isHidden());
            }
            if state == UiState::Error || state.is_kept_result() {
                assert!(!pill.history.isHidden());
                assert!(
                    pill.history.frame().size.width >= 24.0
                        && pill.history.frame().size.height >= 24.0
                );
                let action: Option<objc2::runtime::Sel> =
                    unsafe { msg_send![&*pill.history, action] };
                assert_eq!(action, Some(sel!(openHistory:)));
                assert!(pill.history.keyEquivalent().length() == 0);
                assert!(pill.caps.iter().all(|cap| cap.isHidden()));
            }
            let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, pill.height));
            unsafe {
                let _: () =
                    msg_send![&*pill.window, setFrame: frame, display: false, animate: false];
            }
            pill.glass.setFrame(frame);
            pill.tint.setFrame(frame);
            written.push(snapshot(
                mtm,
                &pill.glass,
                out_dir,
                &format!("{name}-{suffix}"),
                Some(BACKDROP),
                SPACE_XXL,
            )?);
        }
        standard_pill.status.setStringValue(&NSString::from_str(""));
        standard_pill.waveform.set_heights(sample_levels(false));
        let width = layout_pill(&standard_pill, PillLayout::Recording);
        set_cancel_hover(&standard_pill, UiState::Recording, false);
        let frame = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(width, standard_pill.height),
        );
        unsafe {
            let _: () =
                msg_send![&*standard_pill.window, setFrame: frame, display: false, animate: false];
        }
        standard_pill.glass.setFrame(frame);
        standard_pill.tint.setFrame(frame);
        written.push(snapshot(
            mtm,
            &standard_pill.glass,
            out_dir,
            &format!("pill-standard-{suffix}"),
            Some(BACKDROP),
            SPACE_XXL,
        )?);

        crate::settings_window::show_recording_feedback_for_render(&settings);
        written.push(snapshot(
            mtm,
            &settings.window.contentView().expect("settings content"),
            out_dir,
            &format!("settings-feedback-{suffix}"),
            Some(BACKDROP),
            SPACE_XXL,
        )?);
        crate::settings_window::scroll_to_top_for_render(&settings);

        for (name, view) in [
            ("history", &*history.panel),
            ("dictionary", &*dictionary.panel),
        ] {
            written.push(snapshot(
                mtm,
                view,
                out_dir,
                &format!("{name}-{suffix}"),
                Some(BACKDROP),
                SPACE_LG,
            )?);
        }

        for (name, record) in history_detail_fixtures(&detail_audio) {
            populate_detail(&detail, record.clone());
            verify_detail_layers(&detail, &record)?;
            written.push(snapshot(
                mtm,
                &detail.panel,
                out_dir,
                &format!("history-detail-{name}-{suffix}"),
                None,
                0.0,
            )?);
        }

        // Regression: a live detail loses its record while open. No stale
        // transcript, layer picker, or enabled recovery action may remain.
        clear_deleted_detail(&detail);
        verify_deleted_detail(&detail)?;
        written.push(snapshot(mtm, &detail.panel, out_dir,
            &format!("history-detail-deleted-{suffix}"), None, 0.0)?);

        if let Some(content) = settings.window.contentView() {
            written.push(snapshot(
                mtm,
                &content,
                out_dir,
                &format!("settings-{suffix}"),
                None,
                0.0,
            )?);
        }
        // The cleanup card lives in the Dictation pane, below the fold on a
        // short window; scroll it into view so the snapshot shows both cards.
        crate::settings_window::scroll_to_bottom_for_render(&settings);
        if let Some(content) = settings.window.contentView() {
            written.push(snapshot(
                mtm,
                &content,
                out_dir,
                &format!("settings-cleanup-{suffix}"),
                None,
                0.0,
            )?);
        }
        // ── Setup & permissions ─────────────────────────────────────────
        // Both ends of the checklist, on invented rows: the fresh install a
        // stranger meets, and the same window once everything is granted.
        // Nothing here reads this Mac's permissions or model directory.
        for (name, states) in [
            ("setup-fresh", setup_fresh_fixture()),
            ("setup-ready", setup_ready_fixture()),
        ] {
            setup::apply_for_render(&setup, &states);
            written.push(snapshot(
                mtm,
                &setup.panel,
                out_dir,
                &format!("{name}-{suffix}"),
                None,
                0.0,
            )?);
        }

        crate::settings_window::scroll_to_top_for_render(&settings);

        // The speech pane contains both independent model pickers. Prove its
        // bottom as well, where the optional instruction model lives.
        crate::settings_window::show_pane_for_render(&settings, 5);
        for (name, bottom) in [("settings-speech", false), ("settings-speech-end", true)] {
            if bottom { crate::settings_window::scroll_to_bottom_for_render(&settings); }
            if let Some(content) = settings.window.contentView() {
                written.push(snapshot(mtm, &content, out_dir, &format!("{name}-{suffix}"), None, 0.0)?);
            }
        }
        crate::settings_window::scroll_to_top_for_render(&settings);

        // The Dictionary pane carries the term list, so prove that pane too.
        crate::settings_window::show_pane_for_render(&settings, 1);
        if let Some(content) = settings.window.contentView() {
            written.push(snapshot(
                mtm,
                &content,
                out_dir,
                &format!("settings-dictionary-{suffix}"),
                None,
                0.0,
            )?);
        }
        // …and its tail, where the note line under the rows lives.
        crate::settings_window::scroll_to_bottom_for_render(&settings);
        if let Some(content) = settings.window.contentView() {
            written.push(snapshot(
                mtm,
                &content,
                out_dir,
                &format!("settings-dictionary-end-{suffix}"),
                None,
                0.0,
            )?);
        }
        crate::settings_window::scroll_to_top_for_render(&settings);

        // The History pane: the retention card over the day-grouped list.
        crate::settings_window::show_pane_for_render(&settings, 2);
        if let Some(content) = settings.window.contentView() {
            written.push(snapshot(
                mtm,
                &content,
                out_dir,
                &format!("settings-history-{suffix}"),
                None,
                0.0,
            )?);
        }
        // …and its tail, where the older day and the note line sit.
        crate::settings_window::scroll_to_bottom_for_render(&settings);
        if let Some(content) = settings.window.contentView() {
            written.push(snapshot(
                mtm,
                &content,
                out_dir,
                &format!("settings-history-end-{suffix}"),
                None,
                0.0,
            )?);
        }
        crate::settings_window::scroll_to_top_for_render(&settings);

        // The copy confirmation: the clicked row takes the selection paint and
        // its "Copied" chip appears. Proved because a confirmation nobody sees
        // is the bug this replaced.
        crate::settings_window::flash_history_for_render(&settings, 1);
        if let Some(content) = settings.window.contentView() {
            written.push(snapshot(
                mtm,
                &content,
                out_dir,
                &format!("settings-history-copied-{suffix}"),
                None,
                0.0,
            )?);
        }

        // The sidebar shelf closed: the rail is off-screen and the pane keeps
        // its geometry against the narrowed window.
        crate::settings_window::set_sidebar_for_render(&settings, false);
        if let Some(content) = settings.window.contentView() {
            written.push(snapshot(
                mtm,
                &content,
                out_dir,
                &format!("settings-shelf-closed-{suffix}"),
                None,
                0.0,
            )?);
        }
        crate::settings_window::set_sidebar_for_render(&settings, true);
        crate::settings_window::show_pane_for_render(&settings, 0);
    }

    slate::set_opaque_materials(false);
    Ok(written)
}

/// A fresh install: nothing asked for, nothing granted, no model on disk.
fn setup_fresh_fixture() -> Vec<setup::RowState> {
    vec![
        setup::RowState {
            satisfied: false,
            detail: "Not asked yet — nothing is recorded until you allow it.".to_string(),
            action: Some("Allow".to_string()),
        },
        setup::RowState {
            satisfied: false,
            detail: "Off. The hotkey starts working the moment you turn it on — no restart."
                .to_string(),
            action: Some("Open Settings".to_string()),
        },
        setup::RowState {
            satisfied: false,
            detail: "Downloading… 42%".to_string(),
            action: Some("Cancel".to_string()),
        },
    ]
}

/// The same window on a Mac that is ready: three satisfied rows, no actions.
fn setup_ready_fixture() -> Vec<setup::RowState> {
    vec![
        setup::RowState {
            satisfied: true,
            detail: "Allowed".to_string(),
            action: None,
        },
        setup::RowState {
            satisfied: true,
            detail: "Allowed".to_string(),
            action: None,
        },
        setup::RowState {
            satisfied: true,
            detail: "Ready".to_string(),
            action: None,
        },
    ]
}

/// The slate backdrop the mockups sit a floating panel on, so the glass edge
/// and the shadowless PNG still read the way the design does.
const BACKDROP: Rgba = Rgba {
    r: 0.106,
    g: 0.129,
    b: 0.188,
    a: 1.0,
};

/// Draw `view` into a PNG at `out_dir/name.png`. When `backdrop` is set the
/// bitmap is composited onto it with `margin` of padding, so a floating panel
/// is legible instead of sitting on transparency.
fn snapshot(
    mtm: MainThreadMarker,
    view: &NSView,
    out_dir: &str,
    name: &str,
    backdrop: Option<Rgba>,
    margin: f64,
) -> eyre::Result<PathBuf> {
    let bounds = view.bounds();
    view.setNeedsDisplay(true);
    view.displayIfNeeded();
    let rep = view
        .bitmapImageRepForCachingDisplayInRect(bounds)
        .ok_or_else(|| eyre::eyre!("no bitmap rep for {name}"))?;
    view.cacheDisplayInRect_toBitmapImageRep(bounds, &rep);

    let data = match backdrop {
        None => unsafe {
            rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
        },
        Some(bg) => {
            let size = NSSize::new(
                bounds.size.width + 2.0 * margin,
                bounds.size.height + 2.0 * margin,
            );
            let canvas = NSImage::initWithSize(NSImage::alloc(), size);
            {
                #[allow(deprecated)]
                canvas.lockFocus();
                bg.ns_color().setFill();
                NSBezierPath::fillRect(NSRect::new(NSPoint::new(0.0, 0.0), size));
                let destination = NSRect::new(NSPoint::new(margin, margin), bounds.size);
                // cacheDisplay does not reliably apply the glass layer's
                // rounded mask offscreen. Match the native pill mask while
                // compositing its proof; product view geometry is unchanged.
                let context = objc2_app_kit::NSGraphicsContext::currentContext();
                if let Some(context) = &context { context.saveGraphicsState(); }
                if name.starts_with("pill-") {
                    NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(
                        destination, bounds.size.height / 2.0, bounds.size.height / 2.0,
                    ).addClip();
                }
                rep.drawInRect(destination);
                if let Some(context) = &context { context.restoreGraphicsState(); }
                #[allow(deprecated)]
                canvas.unlockFocus();
            }
            let tiff = canvas
                .TIFFRepresentation()
                .ok_or_else(|| eyre::eyre!("no TIFF for {name}"))?;
            let flat = NSBitmapImageRep::imageRepWithData(&tiff)
                .ok_or_else(|| eyre::eyre!("no rep for {name}"))?;
            unsafe {
                flat.representationUsingType_properties(
                    NSBitmapImageFileType::PNG,
                    &NSDictionary::new(),
                )
            }
        }
    }
    .ok_or_else(|| eyre::eyre!("no PNG data for {name}"))?;

    let path = PathBuf::from(out_dir).join(format!("{name}.png"));
    let ns_path = NSString::from_str(&path.to_string_lossy());
    if !{ data.writeToFile_atomically(&ns_path, true) } {
        return Err(eyre::eyre!("could not write {}", path.display()));
    }
    let _ = mtm;
    Ok(path)
}

/// A plausible waveform so the snapshot shows the bars doing something.
fn sample_levels(processing: bool) -> [f64; BAR_COUNT] {
    let mut out = [BAR_MIN_H; BAR_COUNT];
    for (i, slot) in out.iter_mut().enumerate() {
        let phase = i as f64 * 0.55;
        let wave = if processing {
            (phase).sin() * 0.5 + 0.5
        } else {
            ((phase * 1.7).sin() * 0.5 + 0.5) * (0.45 + 0.55 * ((phase * 0.6).cos() * 0.5 + 0.5))
        };
        *slot = BAR_MIN_H + wave * (BAR_MAX_H - BAR_MIN_H);
    }
    out
}

const SAMPLE_DICTIONARY: &str = "Cohere\nMLX\nkuh here → Cohere\nem el ex → MLX\nslack → Slack";

/// Six rows over two days, matching the mockup's list, injected straight into
/// the history window's row builder without touching the real database.
fn seed_sample_history(mtm: MainThreadMarker, ui: &HistoryUi) {
    let now = 1_772_000_000_i64; // a fixed instant, so snapshots are stable
    let day = 86_400_i64;
    let rows: &[(&str, &str, i64)] = &[
        (
            "Can you pull the Q3 numbers into the deck before Thursday and flag anything that moved more than five percent",
            "Slack",
            now,
        ),
        (
            "Summarize this thread and draft a reply that says yes but pushes the date to next week",
            "Claude",
            now - 2_040,
        ),
        (
            "Reminder: renew the domain and move DNS to Cloudflare",
            "Notes",
            now - 11_520,
        ),
        (
            "Make this concise and keep the second paragraph",
            "Mail",
            now - day - 1_500,
        ),
        (
            "Translate to Spanish: we will ship the first build on Monday",
            "iMessage",
            now - day - 7_800,
        ),
        (
            "Add a test for the empty-transcript case and run the full suite",
            "VS Code",
            now - day - 30_000,
        ),
    ];
    let entries: Vec<Entry> = rows
        .iter()
        .enumerate()
        .map(|(i, (text, _app, at))| Entry {
            id: i as i64 + 1,
            text: (*text).to_string(),
            created_at: *at,
        })
        .collect();
    if let Ok(mut store) = HISTORY_ENTRIES.lock() {
        *store = entries.iter().map(|e| e.text.clone()).collect();
    }
    if let Ok(mut all) = HISTORY_ALL.lock() {
        *all = entries.clone();
    }
    rebuild_history_list(mtm, ui, &entries);
}

/// 1000 invented dictations, newest first, spread over ~14 days.
fn synthetic_history(n: usize) -> Vec<Entry> {
    let now = 1_772_000_000_i64;
    (0..n)
        .map(|i| Entry {
            id: i as i64 + 1,
            text: format!(
                "Invented dictation number {i}: please move the review to next week and \
                 keep the agenda short, then ask about the budget for item {}",
                i % 17
            ),
            created_at: now - (i as i64) * 1_200,
        })
        .collect()
}

/// Time the launcher panel at 1000 entries: an open (cache fill plus first
/// list build) and one search keystroke. Prints the numbers and fails when the
/// panel builds an unbounded number of views or blows a generous ceiling, so a
/// return to the eager 1000-row build cannot ship. Uses invented text only.
fn verify_history_scale(mtm: MainThreadMarker, ui: &HistoryUi) -> eyre::Result<()> {
    use std::time::Instant;
    let entries = synthetic_history(1000);

    let t = Instant::now();
    if let Ok(mut store) = HISTORY_ENTRIES.lock() {
        *store = entries.iter().map(|e| e.text.clone()).collect();
    }
    if let Ok(mut all) = HISTORY_ALL.lock() {
        *all = entries.clone();
    }
    rebuild_history_list(mtm, ui, &entries);
    let open = t.elapsed();
    let open_views = ui.doc.subviews().len();

    let t = Instant::now();
    let all = HISTORY_ALL.lock().map(|g| g.clone()).unwrap_or_default();
    let filtered = filter_entries(all, "i");
    rebuild_history_list(mtm, ui, &filtered);
    let search = t.elapsed();
    let search_views = ui.doc.subviews().len();

    // A query that keeps only part of the list (about one entry in 17).
    let t = Instant::now();
    let all = HISTORY_ALL.lock().map(|g| g.clone()).unwrap_or_default();
    let part = filter_entries(all, "item 3");
    rebuild_history_list(mtm, ui, &part);
    let partial = t.elapsed();
    let partial_views = ui.doc.subviews().len();
    if part.is_empty() || part.len() >= 200 {
        eyre::bail!("history partial query kept {} of 1000 -> fix the fixture", part.len());
    }

    println!(
        "[render-ui] history-scale entries=1000 open={:.1}ms search={:.1}ms partial={:.1}ms ({} kept) views_open={open_views} views_search={search_views} views_partial={partial_views}",
        open.as_secs_f64() * 1e3,
        search.as_secs_f64() * 1e3,
        partial.as_secs_f64() * 1e3,
        part.len(),
    );
    if open_views > MAX_HISTORY_VIEWS || search_views > MAX_HISTORY_VIEWS || partial_views > MAX_HISTORY_VIEWS {
        eyre::bail!(
            "history panel built {open_views}/{search_views} views for 1000 entries (limit {MAX_HISTORY_VIEWS}, partial {partial_views}) -> build rows lazily"
        );
    }
    // Paging must reach every entry with one header per day and no gap.
    rebuild_history_list(mtm, ui, &entries);
    // Two pages first, to cross a page seam, then the rest.
    append_history_rows(mtm, ui, &entries, 100);
    append_history_rows(mtm, ui, &entries, 101);
    append_history_rows(mtm, ui, &entries, usize::MAX);
    let full = history_rows(&entries).len();
    let paged = ui.doc.subviews().len();
    if paged != full {
        eyre::bail!("history paging built {paged} list views, one pass builds {full} -> fix the page seam");
    }
    rebuild_history_list(mtm, ui, &entries);
    if open.as_millis() > 60 || search.as_millis() > 50 || partial.as_millis() > 50 {
        eyre::bail!(
            "history panel too slow at 1000 entries: open {open:?}, search {search:?}, partial {partial:?} -> keep list builds bounded"
        );
    }
    Ok(())
}

/// Views the first page of rows may add to the list: a page of 50 entries plus
/// day labels, with headroom (rows are one list view each).
const MAX_HISTORY_VIEWS: usize = 160;

/// One second of silent PCM, created only in the requested render output.
/// The proof never opens a window, plays audio, or reads private history.
fn write_history_audio_fixture(out_dir: &str) -> eyre::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = PathBuf::from(out_dir).join("history-proof.wav");
    let sample_rate = 16_000_u32;
    let data_bytes = sample_rate * 2;
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_bytes.to_le_bytes());
    wav.resize(wav.len() + data_bytes as usize, 0);
    std::fs::write(&path, wav)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(path)
}

fn history_detail_fixtures(
    audio_path: &std::path::Path,
) -> Vec<(&'static str, crate::history::RecoveryRecord)> {
    use crate::history::{AudioRecord, RecoveryRecord, RecoveryStatus, StageKind, TextStage};
    let now = 1_800_000_000;
    // Ordinals are 1-based, exactly as `RecoveryStore::stage` writes them, so
    // the proof shows the same layer numbers a real record does.
    let stage = |id, ordinal, kind, title: &str, text: &str| TextStage {
        id,
        ordinal,
        kind,
        title: title.to_string(),
        text: text.to_string(),
        metadata: "{}".to_string(),
        created_at: now,
    };
    let audio = || {
        Some(AudioRecord {
            path: audio_path.to_path_buf(),
            duration_ms: 1_000,
            expires_at: Some(now + 30 * 86_400),
            byte_count: 32_044,
        })
    };
    vec![
        (
            "completed",
            RecoveryRecord {
                entry: Entry {
                    id: 41,
                    text: "Please send the revised notes tomorrow.".to_string(),
                    created_at: now,
                },
                status: RecoveryStatus::Completed,
                audio: audio(),
                error: None,
                stages: vec![
                    stage(
                        1,
                        1,
                        StageKind::Raw,
                        "Raw",
                        "um please send the revised notes tomorrow",
                    ),
                    stage(
                        2,
                        2,
                        StageKind::Cleaned,
                        "Cleaned",
                        "Please send the revised notes tomorrow.",
                    ),
                ],
            },
        ),
        (
            "failed",
            RecoveryRecord {
                entry: Entry {
                    id: 42,
                    text: "A partial transcript".to_string(),
                    created_at: now,
                },
                status: RecoveryStatus::Failed,
                audio: audio(),
                error: Some("Cleanup failed. Original audio and raw text are kept.".to_string()),
                stages: vec![stage(
                    3,
                    1,
                    StageKind::Raw,
                    "Raw",
                    "A partial transcript remains available even if a later stage fails.",
                )],
            },
        ),
        (
            "cancelled",
            RecoveryRecord {
                entry: Entry {
                    id: 43,
                    text: String::new(),
                    created_at: now,
                },
                status: RecoveryStatus::Cancelled,
                audio: audio(),
                error: None,
                stages: vec![],
            },
        ),
        (
            "expired",
            RecoveryRecord {
                entry: Entry {
                    id: 44,
                    text: "The saved text remains after its audio expires.".to_string(),
                    created_at: now,
                },
                status: RecoveryStatus::Completed,
                audio: None,
                error: None,
                stages: vec![stage(
                    4,
                    1,
                    StageKind::Output,
                    "Saved text",
                    "The saved text remains after its audio expires.",
                )],
            },
        ),
        (
            "recovered-again",
            RecoveryRecord {
                entry: Entry { id: 47, text: "A later cleaned attempt.".to_string(), created_at: now },
                status: RecoveryStatus::Copied, audio: audio(), error: None,
                stages: vec![
                    stage(8, 1, StageKind::Raw, "Raw", "The first raw attempt."),
                    stage(9, 2, StageKind::Cleaned, "Cleaned", "The first cleaned attempt."),
                    stage(10, 3, StageKind::Raw, "Raw", "A later raw attempt."),
                    stage(11, 4, StageKind::Cleaned, "Cleaned", "A later cleaned attempt."),
                ],
            },
        ),
        (
            "cancelled-expired",
            RecoveryRecord {
                entry: Entry {
                    id: 46,
                    text: String::new(),
                    created_at: now,
                },
                status: RecoveryStatus::Cancelled,
                audio: None,
                error: None,
                stages: vec![],
            },
        ),
        (
            "transform",
            RecoveryRecord {
                entry: Entry {
                    id: 45,
                    text: "Send the notes tomorrow.".to_string(),
                    created_at: now,
                },
                status: RecoveryStatus::Completed,
                audio: audio(),
                error: None,
                stages: vec![
                    stage(
                        5,
                        1,
                        StageKind::Raw,
                        "Raw",
                        "um please send the revised notes tomorrow",
                    ),
                    stage(
                        6,
                        2,
                        StageKind::Cleaned,
                        "Cleaned",
                        "Please send the revised notes tomorrow.",
                    ),
                    stage(
                        7,
                        3,
                        StageKind::Transform,
                        "Shorten",
                        "Send the notes tomorrow.",
                    ),
                ],
            },
        ),
    ]
}
