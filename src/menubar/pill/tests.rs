use super::*;

/// Idle polls once a second; anything on screen keeps 30 FPS. The slow
/// checks still run about once a second in both.
#[test]
fn only_idle_slows_the_poll() {
    assert_eq!(poll_interval(UiState::Idle), SLOW_CHECK_SECONDS);
    for v in 1..=17u8 {
        let state = UiState::from_u8(v);
        assert_eq!(poll_interval(state), POLL_ACTIVE_SECONDS, "{state:?}");
    }
    assert!(POLL_IDLE_SECONDS >= SLOW_CHECK_SECONDS - POLL_ACTIVE_SECONDS / 2.0);
}

#[test]
fn recovery_failure_is_visible_without_exposing_the_transcript_or_blocking_work() {
    let recording = active_feedback(
        UiState::Recording,
        true,
        "private spoken words",
        true,
        Some("Recovery storage is unavailable."),
    );
    assert_eq!(recording.text, "Recovery warning");
    assert!(recording.warning && recording.layout == PillLayout::Recording);
    assert_eq!(
        recording.tooltip.as_deref(),
        Some("Recovery storage is unavailable.")
    );
    assert!(!recording.text.contains("private"));
    let processing = active_feedback(
        UiState::Transcribing,
        true,
        "private spoken words",
        true,
        Some("Recovery storage is unavailable."),
    );
    assert_eq!(processing.text, "Transcribing… · Recovery warning");
    assert_eq!(processing.layout, PillLayout::Processing);
    let disabled = active_feedback(
        UiState::Recording,
        true,
        "Live text",
        false,
        Some("Old notice"),
    );
    assert_eq!(disabled.text, "Live text");
    assert!(!disabled.warning);
    assert!(!active_feedback(UiState::Idle, true, "", true, Some("Old notice")).warning);
}

#[test]
fn compact_dimensions_and_preview_preserve_the_contract() {
    assert_eq!(pill_height(PillSize::Compact), 32.0);
    assert_eq!(pill_height(PillSize::Standard), 36.0);
    assert!(PILL_W <= 100.0);
    assert!(CANCEL_HIT >= 24.0);
    let text = "这是最新的中文。".repeat(30);
    let preview = preview_excerpt(&text);
    assert!(preview.starts_with('…') && preview.ends_with("这是最新的中文。"));
    assert!(preview.chars().count() <= 121);
    assert_eq!(preview_excerpt("Hello\n  world"), "Hello world");
}

#[test]
fn visible_stage_labels_and_cancel_deadline_match_pipeline_states() {
    assert_eq!(pill_status(UiState::Transcribing, true), "Transcribing…");
    assert_eq!(pill_status(UiState::Cleaning, true), "Cleaning up…");
    assert_eq!(pill_status(UiState::Pasting, true), "Pasting…");
    assert_eq!(pill_status(UiState::Cleaning, false), "Working…");
    assert_eq!(pill_status(UiState::RewriteFailed, true), "Couldn’t rewrite");
    assert!(
        UiState::Recording.is_cancellable()
            && UiState::Transcribing.is_cancellable()
            && UiState::Cleaning.is_cancellable()
    );
    assert!(!UiState::Pasting.is_cancellable() && !UiState::Copied.is_cancellable());
    assert_eq!(pill_status(UiState::Copied, true), "Copied to clipboard");
}

/// The working indicator is a marching pulse that never leaves the
/// allowable opacity band, peaks at each dot's phase centre, and goes still
/// under Reduce Motion. The three dots fit the leading control slot.
#[test]
fn working_dots_pulse_indeterminately_and_respect_reduce_motion() {
    let cluster =
        PROGRESS_DOTS as f64 * slate::DOT + (PROGRESS_DOTS as f64 - 1.0) * SPACE_XXS;
    assert!(
        cluster <= CANCEL_HIT,
        "the dots must fit the leading control slot"
    );

    // Reduce Motion: a steady fade-only treatment, no march.
    assert_eq!(stage_pulse_alphas(0.42, true), [0.6; PROGRESS_DOTS]);

    // Each dot reaches full opacity at its own phase centre.
    for i in 0..PROGRESS_DOTS {
        let center = i as f64 / PROGRESS_DOTS as f64;
        assert_eq!(stage_pulse_alphas(center, false)[i], 1.0);
    }

    // At every sample the dots stay in band and are not all the same
    // opacity, which is what makes the motion read as indeterminate.
    for step in 0..30 {
        let alphas = stage_pulse_alphas(step as f64 / 30.0, false);
        for a in alphas {
            assert!(
                (PROGRESS_REST_ALPHA..=1.0).contains(&a),
                "opacity {a} out of band"
            );
        }
        let max = alphas.iter().cloned().fold(f64::MIN, f64::max);
        let min = alphas.iter().cloned().fold(f64::MAX, f64::min);
        assert!(max - min > 0.1, "the dots should march, not sit flat");
    }
}
