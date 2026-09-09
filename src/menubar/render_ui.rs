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
    let pill = build_pill_window(mtm, &actions)?;
    let history = build_history_window(mtm);
    let dictionary = build_dictionary_window(mtm);
    let settings = crate::settings_window::build_for_render(mtm);

    // Six rows over two days, matching the mockup's sample list. The same
    // rows fill the Settings History pane, so both surfaces are proved on one
    // known list rather than on whatever this Mac dictated today.
    seed_sample_history(mtm, &history);
    let sample_entries = HISTORY_ALL.lock().map(|g| g.clone()).unwrap_or_default();
    crate::settings_window::seed_history_for_render(&settings, &sample_entries);
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
            &*history.window as &NSWindow,
            &*dictionary.window,
            &*settings.window,
        ] {
            w.setAppearance(Some(&appearance));
        }

        // Pill: one file per state, each laid out then drawn.
        for (name, layout, status, hint) in [
            ("pill-recording", PillLayout::Compact, "Release to finish", "Latch"),
            ("pill-processing", PillLayout::Compact, "Working…", "Latch"),
            ("pill-hover", PillLayout::Shelf, "Release to finish", "Latch"),
            ("pill-success", PillLayout::Success, "Landed · 38 words", "Undo"),
            ("pill-error", PillLayout::Error, "Couldn’t transcribe", "Retry"),
        ] {
            pill.status.setStringValue(&NSString::from_str(status));
            pill.hint.setStringValue(&NSString::from_str(hint));
            pill.waveform.set_muted(name == "pill-processing");
            pill.waveform.set_heights(sample_levels(name == "pill-processing"));
            let width = layout_pill(&pill, layout);
            let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width, PILL_H));
            unsafe {
                let _: () = msg_send![&*pill.window, setFrame: frame, display: false, animate: false];
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
        crate::settings_window::show_pane_for_render(&settings, 0);
    }

    slate::set_opaque_materials(false);
    Ok(written)
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
        None => unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) },
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
                rep.drawInRect(NSRect::new(
                    NSPoint::new(margin, margin),
                    bounds.size,
                ));
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
