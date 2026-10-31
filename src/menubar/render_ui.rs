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

mod fixtures;
use fixtures::*;

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
                "pill-clipboard-protected",
                PillLayout::Error,
                UiState::ClipboardProtected,
                crate::last_dictation::kept_pill_text(UiState::ClipboardProtected).unwrap(),
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
