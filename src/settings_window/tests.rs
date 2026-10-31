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
