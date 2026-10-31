//! Render fixtures: the Setup states, sample levels and dictionary, the
//! seeded and synthetic history, the scale check, and the History detail
//! cases. Invented data only; nothing here reads the user stores.
use super::*;

/// A fresh install: nothing asked for, nothing granted, no model on disk.
pub(super) fn setup_fresh_fixture() -> Vec<setup::RowState> {
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
pub(super) fn setup_ready_fixture() -> Vec<setup::RowState> {
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

/// A plausible waveform so the snapshot shows the bars doing something.
pub(super) fn sample_levels(processing: bool) -> [f64; BAR_COUNT] {
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

pub(super) const SAMPLE_DICTIONARY: &str = "Cohere\nMLX\nkuh here → Cohere\nem el ex → MLX\nslack → Slack";

/// Six rows over two days, matching the mockup's list, injected straight into
/// the history window's row builder without touching the real database.
pub(super) fn seed_sample_history(mtm: MainThreadMarker, ui: &HistoryUi) {
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
pub(super) fn synthetic_history(n: usize) -> Vec<Entry> {
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
pub(super) fn verify_history_scale(mtm: MainThreadMarker, ui: &HistoryUi) -> eyre::Result<()> {
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
pub(super) const MAX_HISTORY_VIEWS: usize = 160;

/// One second of silent PCM, created only in the requested render output.
/// The proof never opens a window, plays audio, or reads private history.
pub(super) fn write_history_audio_fixture(out_dir: &str) -> eyre::Result<PathBuf> {
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

pub(super) fn history_detail_fixtures(
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
