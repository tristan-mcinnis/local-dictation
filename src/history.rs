//! Lightweight SQLite-backed dictation history.
//!
//! This is the friendly counterpart to the daemon log at
//! `/tmp/dictate-daemon.log`: the log records every timing, app target and
//! pipeline decision (useful for debugging, far too noisy for browsing),
//! whereas this stores just the *injected text* and *when* — the one thing a
//! user actually wants to skim later.
//!
//! Storage lives next to `settings.json` at
//! `~/.config/local-dictation/history.db`. A single table:
//!
//! ```sql
//! CREATE TABLE dictations (
//!     id         INTEGER PRIMARY KEY AUTOINCREMENT,
//!     text       TEXT    NOT NULL,
//!     created_at INTEGER NOT NULL   -- unix seconds, UTC
//! );
//! ```
//!
//! Writes are best-effort: a failure to persist history must never break the
//! dictation hot path, so `record` logs and swallows errors. Connections are
//! opened per-call — dictation happens at human speed, so the open cost is
//! irrelevant and we sidestep sharing a `Connection` across threads.
//!
//! Every store is keyed by a retention window (see [`Retention`]) resolved from
//! `settings.json` / the `DICTATE_HISTORY_RETENTION` env var. After each insert
//! we prune rows older than the window so the DB doesn't grow without bound.
//! Pruning is also best-effort: any error is logged, never propagated.
//!
//! This module is just the data layer. Presentation (the native history
//! window's date grouping + local-time formatting) lives in `menubar`, where
//! AppKit's `NSDateFormatter` gives correct locale/timezone output for free.

use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// One stored dictation.
#[derive(Debug, Clone)]
pub struct Entry {
    pub text: String,
    /// Creation time in unix seconds (UTC).
    pub created_at: i64,
}

/// How long to keep dictation history. A retention window is resolved from
/// `settings.json` / `DICTATE_HISTORY_RETENTION` at the moment a dictation is
/// recorded, then applied as a prune after the insert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retention {
    /// Keep everything, forever (the safe default).
    Forever,
    /// Keep roughly one year.
    OneYear,
    /// Keep roughly one month.
    OneMonth,
    /// Keep roughly one week.
    OneWeek,
    /// Keep roughly the last 24 hours.
    TwentyFourHours,
    /// Keep nothing — every row is pruned as soon as it's written.
    Never,
}

impl Retention {
    /// Parse a retention string (case-insensitive, whitespace-tolerant).
    /// Unknown/blank input falls back to [`Retention::Forever`] so a bad value
    /// can never silently drop history.
    ///
    /// Accepted forms include "forever", "1year"/"1 year"/"year",
    /// "1month"/"1 month"/"month", "1week"/"1 week"/"week",
    /// "24hours"/"24 hours"/"1day"/"1 day"/"day", and "never".
    pub fn parse(s: &str) -> Retention {
        let normalized = s
            .trim()
            .to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        match normalized.as_str() {
            "never" => Retention::Never,
            "forever" => Retention::Forever,
            "year" | "1year" | "1 year" => Retention::OneYear,
            "month" | "1month" | "1 month" => Retention::OneMonth,
            "week" | "1week" | "1 week" => Retention::OneWeek,
            "24hours" | "24 hours" | "1day" | "1 day" | "day" => Retention::TwentyFourHours,
            _ => Retention::Forever,
        }
    }

    /// The retention window in seconds, or `None` to keep everything.
    ///
    /// - `Forever` → `None` (no pruning).
    /// - `Never` → `Some(0)` (prune everything).
    /// - the rest → the window length (1y=31536000, 1mo=2592000, 1w=604800,
    ///   24h=86400).
    pub fn cutoff_secs(&self) -> Option<i64> {
        match self {
            Retention::Forever => None,
            Retention::Never => Some(0),
            Retention::OneYear => Some(31_536_000),
            Retention::OneMonth => Some(2_592_000),
            Retention::OneWeek => Some(604_800),
            Retention::TwentyFourHours => Some(86_400),
        }
    }
}

/// `~/.config/local-dictation/history.db` (None if `$HOME` is unset).
fn db_path() -> Option<PathBuf> {
    crate::app_paths::config_file("history.db")
}

/// Current unix time in seconds.
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Open the history DB at an explicit path, creating the directory, file and
/// schema as needed. This is the injectable core that [`open`] delegates to.
fn open_at(path: &Path) -> eyre::Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| eyre::eyre!("create {}: {e}", dir.display()))?;
    }
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS dictations (
             id         INTEGER PRIMARY KEY AUTOINCREMENT,
             text       TEXT    NOT NULL,
             created_at INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_dictations_created_at
             ON dictations (created_at DESC);",
    )?;
    Ok(conn)
}

/// Open the default history DB (see [`db_path`]).
fn open() -> eyre::Result<Connection> {
    let path = db_path().ok_or_else(|| eyre::eyre!("cannot resolve history path ($HOME unset)"))?;
    open_at(&path)
}

/// Persist one dictation. Best-effort: errors are logged, never propagated,
/// so a history hiccup can't interrupt the inject hot path.
pub fn record(text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    if let Err(e) = try_record(text) {
        eprintln!("[history] record failed: {e}");
    }
}

fn try_record(text: &str) -> eyre::Result<()> {
    let now = unix_now();
    {
        // Open, insert, and drop the connection so its write lock is released
        // before the prune re-opens the same file below.
        let conn = open()?;
        conn.execute(
            "INSERT INTO dictations (text, created_at) VALUES (?1, ?2)",
            rusqlite::params![text, now],
        )?;
    }

    // Resolve and apply the retention window. `Retention::parse` defaults
    // unknown/blank values to `Forever`, so a misconfiguration keeps history.
    let retention = Retention::parse(&crate::settings::Settings::load().resolve_history_retention());
    prune(retention.cutoff_secs())?;
    Ok(())
}

/// Insert a row at an explicit path with the current timestamp. Internal
/// testable counterpart to [`record`]; uses the config-resolving path never —
/// callers pass an explicit DB path. Only referenced from tests.
#[cfg_attr(not(test), allow(dead_code))]
fn record_at(path: &Path, text: &str) -> eyre::Result<()> {
    insert_at(path, text, unix_now())
}

/// Insert a row at an explicit path with an explicit `created_at` (unix secs).
/// Used by tests to plant old rows so pruning can be exercised.
#[cfg_attr(not(test), allow(dead_code))]
fn insert_at(path: &Path, text: &str, created_at: i64) -> eyre::Result<()> {
    let conn = open_at(path)?;
    conn.execute(
        "INSERT INTO dictations (text, created_at) VALUES (?1, ?2)",
        rusqlite::params![text, created_at],
    )?;
    Ok(())
}

/// Apply a retention window to the default DB. Best-effort — errors are
/// logged and swallowed by the caller; this returns them for testing.
fn prune(window_secs: Option<i64>) -> eyre::Result<()> {
    let path = db_path().ok_or_else(|| eyre::eyre!("cannot resolve history path ($HOME unset)"))?;
    prune_at(&path, window_secs)
}

/// Apply a retention window to the DB at `path`.
///
/// `window_secs` mirrors [`Retention::cutoff_secs`]:
/// - `None` keeps everything (`Forever`).
/// - `Some(0)` deletes every row (`Never`).
/// - `Some(win)` deletes rows older than `now - win`.
fn prune_at(path: &Path, window_secs: Option<i64>) -> eyre::Result<()> {
    let conn = open_at(path)?;
    match window_secs {
        None => Ok(()),
        Some(0) => {
            conn.execute("DELETE FROM dictations", [])?;
            Ok(())
        }
        Some(win) => {
            let now = unix_now();
            conn.execute(
                "DELETE FROM dictations WHERE created_at < ?1",
                rusqlite::params![now - win],
            )?;
            Ok(())
        }
    }
}

/// Most-recent-first dictations, capped at `limit`. Returns an empty vec on
/// any error (a missing DB just means "no history yet").
pub fn recent(limit: usize) -> Vec<Entry> {
    if let Some(path) = db_path() {
        try_recent_at(&path, limit).unwrap_or_default()
    } else {
        Vec::new()
    }
}

fn try_recent_at(path: &Path, limit: usize) -> eyre::Result<Vec<Entry>> {
    let conn = open_at(path)?;
    let mut stmt = conn.prepare(
        "SELECT text, created_at FROM dictations
         ORDER BY created_at DESC, id DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as i64], |row| {
        Ok(Entry {
            text: row.get(0)?,
            created_at: row.get(1)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An isolated DB path per test function. A distinct label keeps parallel
    /// test threads from clobbering each other, and we wipe any leftover state
    /// so each test starts clean.
    fn test_db(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fd-history-retention-test-{}-{}",
            std::process::id(),
            label
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("history.db")
    }

    const DAYS: i64 = 86_400;

    #[test]
    fn retention_parse_accepts_all_forms_and_is_case_insensitive() {
        assert_eq!(Retention::parse("forever"), Retention::Forever);
        assert_eq!(Retention::parse("FOREVER"), Retention::Forever);
        assert_eq!(Retention::parse("  Forever "), Retention::Forever);

        assert_eq!(Retention::parse("year"), Retention::OneYear);
        assert_eq!(Retention::parse("1year"), Retention::OneYear);
        assert_eq!(Retention::parse("1 year"), Retention::OneYear);
        assert_eq!(Retention::parse("1  year"), Retention::OneYear);
        assert_eq!(Retention::parse("YEAR"), Retention::OneYear);

        assert_eq!(Retention::parse("month"), Retention::OneMonth);
        assert_eq!(Retention::parse("1month"), Retention::OneMonth);
        assert_eq!(Retention::parse("1 month"), Retention::OneMonth);

        assert_eq!(Retention::parse("week"), Retention::OneWeek);
        assert_eq!(Retention::parse("1week"), Retention::OneWeek);
        assert_eq!(Retention::parse("1 week"), Retention::OneWeek);

        assert_eq!(Retention::parse("24hours"), Retention::TwentyFourHours);
        assert_eq!(Retention::parse("24 hours"), Retention::TwentyFourHours);
        assert_eq!(Retention::parse("1day"), Retention::TwentyFourHours);
        assert_eq!(Retention::parse("1 day"), Retention::TwentyFourHours);
        assert_eq!(Retention::parse("day"), Retention::TwentyFourHours);

        assert_eq!(Retention::parse("never"), Retention::Never);
        assert_eq!(Retention::parse("NEVER"), Retention::Never);
        assert_eq!(Retention::parse(" never "), Retention::Never);
    }

    #[test]
    fn retention_parse_unknown_defaults_to_forever() {
        assert_eq!(Retention::parse("bogus"), Retention::Forever);
        assert_eq!(Retention::parse("2years"), Retention::Forever);
        assert_eq!(Retention::parse(""), Retention::Forever);
        assert_eq!(Retention::parse("   "), Retention::Forever);
    }

    #[test]
    fn retention_cutoff_secs_values() {
        assert_eq!(Retention::Forever.cutoff_secs(), None);
        assert_eq!(Retention::Never.cutoff_secs(), Some(0));
        assert_eq!(Retention::OneYear.cutoff_secs(), Some(31_536_000));
        assert_eq!(Retention::OneMonth.cutoff_secs(), Some(2_592_000));
        assert_eq!(Retention::OneWeek.cutoff_secs(), Some(604_800));
        assert_eq!(Retention::TwentyFourHours.cutoff_secs(), Some(86_400));
    }

    #[test]
    fn prune_one_week_keeps_fresh_drops_old() {
        let db = test_db("prune_one_week");
        let now = unix_now();
        let two_weeks_ago = now - 2 * 7 * DAYS;

        insert_at(&db, "old", two_weeks_ago).unwrap();
        insert_at(&db, "fresh", now).unwrap();

        // OneWeek window in seconds.
        prune_at(&db, Some(604_800)).unwrap();

        let rows = try_recent_at(&db, 10).unwrap();
        assert_eq!(rows.len(), 1, "only the fresh row should survive");
        assert_eq!(rows[0].text, "fresh");
    }

    #[test]
    fn prune_never_deletes_everything() {
        let db = test_db("prune_never");
        let now = unix_now();

        insert_at(&db, "old", now - 3 * DAYS).unwrap();
        insert_at(&db, "fresh", now).unwrap();

        // Never => `cutoff_secs` is Some(0) => delete all rows.
        prune_at(&db, Retention::Never.cutoff_secs()).unwrap();

        let rows = try_recent_at(&db, 10).unwrap();
        assert!(rows.is_empty(), "Never must prune every row");
    }

    #[test]
    fn record_at_and_prune_at_roundtrip() {
        let db = test_db("record_prune_roundtrip");
        record_at(&db, "hello").unwrap();

        let rows = try_recent_at(&db, 5).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "hello");
    }
}
