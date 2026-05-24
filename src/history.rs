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
//! This module is just the data layer. Presentation (the native history
//! window's date grouping + local-time formatting) lives in `menubar`, where
//! AppKit's `NSDateFormatter` gives correct locale/timezone output for free.

use rusqlite::Connection;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// One stored dictation.
#[derive(Debug, Clone)]
pub struct Entry {
    pub text: String,
    /// Creation time in unix seconds (UTC).
    pub created_at: i64,
}

/// `~/.config/local-dictation/history.db` (None if `$HOME` is unset).
fn db_path() -> Option<PathBuf> {
    crate::app_paths::config_file("history.db")
}

/// Open the history DB, creating the directory, file and schema as needed.
fn open() -> eyre::Result<Connection> {
    let path = db_path().ok_or_else(|| eyre::eyre!("cannot resolve history path ($HOME unset)"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| eyre::eyre!("create {}: {e}", dir.display()))?;
    }
    let conn = Connection::open(&path)?;
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
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let conn = open()?;
    conn.execute(
        "INSERT INTO dictations (text, created_at) VALUES (?1, ?2)",
        rusqlite::params![text, now],
    )?;
    Ok(())
}

/// Most-recent-first dictations, capped at `limit`. Returns an empty vec on
/// any error (a missing DB just means "no history yet").
pub fn recent(limit: usize) -> Vec<Entry> {
    try_recent(limit).unwrap_or_default()
}

fn try_recent(limit: usize) -> eyre::Result<Vec<Entry>> {
    let conn = open()?;
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
