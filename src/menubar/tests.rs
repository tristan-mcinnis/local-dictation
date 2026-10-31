use super::*;

/// The picker's answer is the trimmed POSIX path; a cancel (AppleScript
/// error -128) or an empty answer chooses nothing. No dialog is shown.
#[test]
fn picker_reads_the_chosen_path_and_treats_cancel_as_nothing() {
    assert_eq!(
        run_osascript_picker("return \"/tmp/words.csv\"", "test"),
        Some(PathBuf::from("/tmp/words.csv"))
    );
    assert_eq!(run_osascript_picker("error number -128", "test"), None);
    assert_eq!(run_osascript_picker("return \"\"", "test"), None);
}

/// The picker runs off the main thread, and a second click while it is
/// open does not stack another dialog.
#[test]
fn picker_does_not_block_and_does_not_stack() {
    fn never_on_main(_: PathBuf) {}
    let started = std::time::Instant::now();
    assert!(pick_path_with_osascript(
        "delay 0.4\nreturn \"/tmp/a.csv\"",
        "test",
        never_on_main
    ));
    if crate::timing_checks() {
        assert!(
            started.elapsed() < std::time::Duration::from_millis(200),
            "returns before the dialog closes"
        );
    }
    assert!(PICKER_OPEN.load(Ordering::SeqCst));
    // A second request while one is open is refused, not queued.
    assert!(!pick_path_with_osascript(
        "return \"/tmp/b.csv\"",
        "test",
        never_on_main
    ));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while PICKER_OPEN.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        !PICKER_OPEN.load(Ordering::SeqCst),
        "the flag clears when the picker closes"
    );
}

#[test]
fn rows_empty() {
    assert!(history_rows(&[]).is_empty());
}

#[test]
fn rows_group_by_day_newest_first() {
    // Two entries ~a day apart. We assert structure (a header per day, item
    // ordering and indices) without pinning exact times, since formatting
    // is locale/timezone dependent.
    let entries = vec![
        Entry {
            id: 2,
            text: "second day line".into(),
            created_at: 1_747_900_000,
        },
        Entry {
            id: 1,
            text: "first day line".into(),
            created_at: 1_747_900_000 - 90_000,
        },
    ];
    let rows = history_rows(&entries);
    eprintln!("\n----- history_rows -----\n{rows:#?}\n------------------------");

    // Two distinct day headers (different calendar days).
    let headers = rows
        .iter()
        .filter(|r| matches!(r, HistoryRow::Day(_)))
        .count();
    assert_eq!(headers, 2, "expected one header per day, got {headers}");

    // Items appear newest-first, carrying their original entry index, and
    // each row's text matches its slot.
    let items: Vec<(usize, &str)> = rows
        .iter()
        .filter_map(|r| match r {
            HistoryRow::Item { index, text, .. } => Some((*index, text.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(items, vec![(0, "second day line"), (1, "first day line")]);
}
