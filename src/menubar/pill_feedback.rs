//! Live pill feedback: the preview text and status, the waveform bars, the
//! stage pulse, and placing the pill at the cursor screen.
use super::*;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct PillFeedback {
    pub(super) text: String,
    pub(super) tooltip: Option<String>,
    pub(super) warning: bool,
    pub(super) layout: PillLayout,
}

pub(super) fn active_feedback(
    state: UiState,
    details: bool,
    preview: &str,
    recovery_enabled: bool,
    notice: Option<&str>,
) -> PillFeedback {
    let warning = recovery_enabled && state.is_active() && notice.is_some();
    let text = if state == UiState::Recording {
        if warning {
            "Recovery warning".to_owned()
        } else {
            preview_excerpt(preview)
        }
    } else if warning {
        format!("{} · Recovery warning", pill_status(state, details))
    } else {
        pill_status(state, details).to_owned()
    };
    let tooltip = if warning {
        notice.map(str::to_owned)
    } else if state == UiState::Recording && !preview.is_empty() {
        Some(preview.to_owned())
    } else {
        None
    };
    PillFeedback {
        text,
        tooltip,
        warning,
        layout: if state == UiState::Recording {
            PillLayout::Recording
        } else {
            PillLayout::Processing
        },
    }
}

pub(super) fn update_live_feedback(globals: &UiGlobals, state: UiState) {
    let preview = if state == UiState::Recording {
        ui_channel::live_preview()
    } else {
        String::new()
    };
    let notice = if globals.recovery_enabled {
        crate::history::recovery_storage_failure_notice()
    } else {
        None
    };
    let feedback = active_feedback(
        state,
        globals.show_processing_detail,
        &preview,
        globals.recovery_enabled,
        notice.as_deref(),
    );
    let Ok(mut previous) = globals.pill_feedback.lock() else {
        return;
    };
    if previous.as_ref() == Some(&feedback) {
        return;
    }
    *previous = Some(feedback.clone());
    drop(previous);
    globals.pill.storage_warning.set(feedback.warning);
    globals
        .pill
        .status
        .setStringValue(&NSString::from_str(&feedback.text));
    let tooltip = feedback.tooltip.as_deref().map(NSString::from_str);
    globals.pill.status.setToolTip(tooltip.as_deref());
    set_pill_layout(globals, feedback.layout);
    set_cancel_hover(
        &globals.pill,
        state,
        globals.pill_hovered.load(Ordering::Relaxed),
    );
}

pub(super) fn preview_excerpt(text: &str) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let tail: String = normalized
        .chars()
        .rev()
        .take(120)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if tail.len() < normalized.len() {
        format!("…{tail}")
    } else {
        tail
    }
}

pub(super) fn pill_status(state: UiState, details: bool) -> &'static str {
    match state {
        UiState::Recording => "Recording",
        UiState::Processing => "Working…",
        UiState::Transcribing if details => "Transcribing…",
        UiState::Cleaning if details => "Cleaning up…",
        UiState::Pasting if details => "Pasting…",
        UiState::Transcribing | UiState::Cleaning | UiState::Pasting => "Working…",
        UiState::Copied => "Copied to clipboard",
        UiState::Success => "Landed",
        UiState::Error => "Couldn’t transcribe",
        UiState::RewriteFailed => "Couldn’t rewrite",
        UiState::PasteFailed | UiState::PasteFailedCopied => "Couldn’t paste",
        UiState::NoEditableField | UiState::HeldCopied => "No text field selected",
        UiState::LimitReached | UiState::LimitReachedCopied => "Recording limit reached",
        UiState::NothingToCopy => NOTHING_TO_COPY_TEXT,
        UiState::Unavailable => "Not ready",
        UiState::Idle => "Ready",
    }
}

pub(super) fn set_pill_layout(globals: &UiGlobals, layout: PillLayout) {
    let width = layout_pill(&globals.pill, layout);
    set_pill_width(globals, width);
}

pub(super) fn set_pill_width(globals: &UiGlobals, width: f64) {
    let old = globals.pill_window.frame();
    if old.size.width == width && old.size.height == globals.pill.height {
        return;
    }
    let cx = old.origin.x + old.size.width / 2.0;
    let frame = NSRect::new(
        NSPoint::new((cx - width / 2.0).round(), old.origin.y),
        NSSize::new(width, globals.pill.height),
    );
    unsafe {
        let _: () =
            msg_send![&*globals.pill_window, setFrame: frame, display: true, animate: false];
    }
    globals.pill.glass.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(width, globals.pill.height),
    ));
    globals.pill.tint.setFrame(NSRect::new(
        NSPoint::new(0.0, 0.0),
        NSSize::new(width, globals.pill.height),
    ));
    globals.pill.tint.setNeedsDisplay(true);
}

pub(super) fn reduced_motion_enabled() -> bool {
    if std::env::var_os("DICTATE_REDUCED_MOTION").is_some() {
        return true;
    }
    let workspace = objc2_app_kit::NSWorkspace::sharedWorkspace();
    // This is AppKit's accessibility preference, not an assumed defaults key.
    unsafe { msg_send![&*workspace, accessibilityDisplayShouldReduceMotion] }
}

/// Pure mapping seam: newest samples are right-aligned and never synthesized.
pub(super) fn waveform_targets(
    levels: &[f32],
    sensitivity: WaveformSensitivity,
) -> [f64; BAR_COUNT] {
    crate::waveform::levels::<BAR_COUNT>(levels, sensitivity)
        .map(|level| BAR_MIN_H + level * (BAR_MAX_H - BAR_MIN_H))
}

pub(super) fn eased_bar(current: f64, target: f64, reduced_motion: bool) -> f64 {
    let range = BAR_MAX_H - BAR_MIN_H;
    let normalized = crate::waveform::ease(
        (current - BAR_MIN_H) / range,
        (target - BAR_MIN_H) / range,
        reduced_motion,
    );
    BAR_MIN_H + normalized * range
}

pub(super) fn update_bars(globals: &UiGlobals) {
    let targets = waveform_targets(&ui_channel::recent_levels(), globals.waveform_sensitivity);
    let Ok(mut displayed) = globals.displayed_heights.lock() else {
        return;
    };
    let mut heights = [BAR_MIN_H; BAR_COUNT];
    for (index, target) in targets.iter().enumerate() {
        let height = eased_bar(displayed[index], *target, globals.reduced_motion);
        displayed[index] = height;
        heights[index] = height;
    }
    globals.pill.waveform.set_heights(heights);
}

pub(super) fn collapse_bars(globals: &UiGlobals) {
    if let Ok(mut g) = globals.displayed_heights.lock() {
        for v in g.iter_mut() {
            *v = BAR_MIN_H;
        }
    }
    globals.pill.waveform.set_heights([BAR_MIN_H; BAR_COUNT]);
}

/// How many dots the indeterminate working indicator draws.
pub(super) const PROGRESS_DOTS: usize = 3;

/// Resting opacity of an unlit working dot (lit reaches 1.0).
pub(super) const PROGRESS_REST_ALPHA: f64 = 0.35;

/// Opacity of each working dot for a pulse `phase` in `[0, 1)`. The pulse
/// marches across the dots so the indicator reads as indeterminate progress,
/// never as a percentage or as microphone levels. Under Reduce Motion there is
/// no movement: every dot sits at a steady mid opacity (a fade-only treatment
/// the design rule allows). Pure, so it is unit-tested.
pub(super) fn stage_pulse_alphas(phase: f64, reduced_motion: bool) -> [f64; PROGRESS_DOTS] {
    if reduced_motion {
        return [0.6; PROGRESS_DOTS];
    }
    let phase = phase.rem_euclid(1.0);
    std::array::from_fn(|i| {
        let center = i as f64 / PROGRESS_DOTS as f64;
        let d = (phase - center).abs();
        let d = d.min(1.0 - d);
        let bump = (1.0 - 2.0 * d).max(0.0);
        PROGRESS_REST_ALPHA + (1.0 - PROGRESS_REST_ALPHA) * bump
    })
}

/// Advance the working dots on the existing 30 FPS poll. Only called while a
/// processing state is on screen, so it adds no timer work when idle.
pub(super) fn update_progress_pulse(globals: &UiGlobals) {
    let phase = CFAbsoluteTimeGetCurrent().rem_euclid(1.0);
    let alphas = stage_pulse_alphas(phase, globals.reduced_motion);
    for (dot, alpha) in globals.pill.progress_dots.iter().zip(alphas) {
        dot.setAlphaValue(alpha);
    }
}

pub(super) fn position_pill_at_cursor_screen(window: &NSWindow) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    {
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
        // Sit close to the bottom edge so the pill stays out of the way.
        let y = chosen.origin.y + SPACE_XXL;
        window.setFrameOrigin(NSPoint::new(x, y));
    }
}
