//! Local dictation history and restart-safe recovery. Legacy text rows remain
//! readable; new records preserve original audio and ordered transcript versions.
//! The recovery writer owns disk work off the audio callback and ASR pipeline.

use rusqlite::Connection;

mod recovery;
pub use recovery::*;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// One stored dictation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    /// The row's primary key. Stable across list rebuilds, so the settings
    /// window can delete the row a click meant even after a re-query moved it.
    pub id: i64,
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
        recovery::private_directory(dir)?;
    }
    recovery::private_file(path)?;
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA secure_delete=ON;")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS dictations (
             id         INTEGER PRIMARY KEY AUTOINCREMENT,
             text       TEXT    NOT NULL,
             created_at INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_dictations_created_at
             ON dictations (created_at DESC);",
    )?;
    recovery::migrate(&conn)?;
    Ok(conn)
}

/// How long a UI read waits for a writer before it reports "busy". A
/// dictation that is finishing holds the write lock for a moment; the History
/// panel must not wait out the 2 s write timeout behind it.
const UI_BUSY_MS: u64 = 150;

/// The result of a read made for a window. Never blocks for long.
#[derive(Debug)]
pub enum UiRead<T> {
    Ok(T),
    /// A writer held the lock past the short UI timeout. Nothing is wrong
    /// with the store; the caller should keep what it shows and say so.
    Busy,
    Failed(String),
}

fn is_busy(e: &eyre::Report) -> bool {
    matches!(
        e.downcast_ref::<rusqlite::Error>(),
        Some(rusqlite::Error::SqliteFailure(f, _))
            if matches!(f.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

fn ui_result<T>(r: eyre::Result<T>) -> UiRead<T> {
    match r {
        Ok(v) => UiRead::Ok(v),
        Err(e) if is_busy(&e) => UiRead::Busy,
        Err(e) => UiRead::Failed(e.to_string()),
    }
}

/// A read-only connection for windows: no chmod, no schema create, no
/// migration, and a short busy timeout. `None` when the file does not exist
/// yet (nothing dictated). On-disk format and journal mode are untouched.
fn open_ui_at(path: &Path) -> eyre::Result<Option<Connection>> {
    if !path.exists() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(std::time::Duration::from_millis(UI_BUSY_MS))?;
    Ok(Some(conn))
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
    let retention =
        Retention::parse(&crate::settings::Settings::load().resolve_history_retention());
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
    let Some(window) = window_secs else {
        return Ok(());
    };
    let conn = open_at(path)?;
    let cutoff = if window == 0 {
        i64::MAX
    } else {
        unix_now() - window
    };
    // Rows already reduced to the placeholder whose clip is still inside its
    // own retention need no work; without this filter every Finish re-opened a
    // connection and rewrote each of them again.
    let ids = conn
        .prepare(
            "SELECT id FROM dictations WHERE created_at < ?1
             AND NOT (text='(Audio retained; transcript expired)' AND audio_bytes>0
                      AND (audio_expires_at IS NULL OR audio_expires_at > ?2))",
        )?
        .query_map([cutoff, unix_now()], |row| row.get::<_, i64>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for id in ids {
        // Text and audio have independent retention. Keep a recoverable clip
        // when the user elects to expire transcripts sooner than audio.
        if recovery::audio_path_at(path, id)?.is_some() {
            conn.execute("DELETE FROM dictation_stages WHERE dictation_id=?1", [id])?;
            conn.execute(
                "UPDATE dictations SET text='(Audio retained; transcript expired)' WHERE id=?1",
                [id],
            )?;
        } else {
            delete_at(path, id)?;
        }
    }
    Ok(())
}

/// Most-recent-first dictations, capped at `limit`. Returns an empty vec on
/// any error; call [`unreadable_reason`] to tell "nothing yet" from "the
/// store would not open".
pub fn recent(limit: usize) -> Vec<Entry> {
    if let Some(path) = db_path() {
        try_recent_at(&path, limit).unwrap_or_default()
    } else {
        Vec::new()
    }
}

/// [`recent`] for a window: bounded wait, and "busy" is told apart from
/// "unreadable". Call it off the main thread anyway.
pub fn recent_ui(limit: usize) -> UiRead<Vec<Entry>> {
    match db_path() {
        Some(path) => recent_ui_at(&path, limit),
        None => UiRead::Ok(Vec::new()),
    }
}

pub fn recent_ui_at(path: &Path, limit: usize) -> UiRead<Vec<Entry>> {
    ui_result((|| {
        let Some(conn) = open_ui_at(path)? else {
            return Ok(Vec::new());
        };
        let mut stmt = conn.prepare(
            "SELECT id, text, created_at FROM dictations
             ORDER BY created_at DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |row| {
            Ok(Entry { id: row.get(0)?, text: row.get(1)?, created_at: row.get(2)? })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    })())
}

/// Why the history store cannot be read, or `None` when it is readable (which
/// includes "it does not exist yet" — nothing has been dictated).
///
/// An unreadable store used to look exactly like an empty one: a corrupt file,
/// a locked database or a config directory managed by a dotfile tool all
/// showed "No dictations yet", so a user whose history was there but
/// unreachable was told it was not there at all.
pub fn unreadable_reason() -> Option<String> {
    unreadable_reason_at(&db_path()?)
}

/// Injectable counterpart of [`unreadable_reason`], so tests can point at an
/// invented fixture instead of the user's own store.
pub fn unreadable_reason_at(path: &Path) -> Option<String> {
    if !path.exists() {
        // Nothing has been dictated yet. That is not a fault, and a new user
        // must not be shown one.
        return None;
    }
    // Opening is lazy in SQLite, so a corrupt file only fails on a real
    // query. One cheap count is the honest check. A busy store is a writer at
    // work, not a fault.
    let probe = (|| -> eyre::Result<()> {
        let Some(conn) = open_ui_at(path)? else { return Ok(()) };
        conn.query_row("SELECT count(*) FROM dictations", [], |row| row.get::<_, i64>(0))?;
        Ok(())
    })();
    match probe {
        Err(e) if !is_busy(&e) => Some(e.to_string()),
        _ => None,
    }
}

/// The empty-list line for a history surface. `searching` is true when a query
/// is narrowing the list.
pub fn empty_state_text(unreadable: Option<&str>, searching: bool) -> String {
    if let Some(reason) = unreadable {
        return format!("History could not be opened. It was left untouched. {reason}");
    }
    if searching {
        return "No dictations match your search.".to_string();
    }
    "No dictations yet — hold your hotkey and speak.".to_string()
}

fn try_recent_at(path: &Path, limit: usize) -> eyre::Result<Vec<Entry>> {
    let conn = open_at(path)?;
    let mut stmt = conn.prepare(
        "SELECT id, text, created_at FROM dictations
         ORDER BY created_at DESC, id DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as i64], |row| {
        Ok(Entry {
            id: row.get(0)?,
            text: row.get(1)?,
            created_at: row.get(2)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Most-recent-first dictations whose text contains `query`, capped at
/// `limit`. A blank query is the same as [`recent`]. Returns an empty vec on
/// any error, so a search can never fail loudly in the UI.
///
/// Matching is SQL `LIKE`, which is case-insensitive for ASCII and exact for
/// everything else — the right behaviour for a store that holds both English
/// and Chinese dictations. `%`, `_` and `\` in the query are escaped so a
/// literal wildcard searches for itself.
pub fn search(query: &str, limit: usize) -> Vec<Entry> {
    let needle = query.trim();
    if needle.is_empty() {
        return recent(limit);
    }
    if let Some(path) = db_path() {
        try_search_at(&path, needle, limit).unwrap_or_default()
    } else {
        Vec::new()
    }
}

/// Escape the `LIKE` metacharacters so a query matches itself literally.
fn like_escape(needle: &str) -> String {
    let mut out = String::with_capacity(needle.len() + 2);
    for ch in needle.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn try_search_at(path: &Path, needle: &str, limit: usize) -> eyre::Result<Vec<Entry>> {
    let conn = open_at(path)?;
    let pattern = format!("%{}%", like_escape(needle));
    let mut stmt = conn.prepare(
        r"SELECT id, text, created_at FROM dictations
          WHERE text LIKE ?1 ESCAPE '\' OR EXISTS(SELECT 1 FROM dictation_stages WHERE dictation_id=dictations.id AND text LIKE ?1 ESCAPE '\')
          ORDER BY created_at DESC, id DESC
          LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![pattern, limit as i64], |row| {
        Ok(Entry {
            id: row.get(0)?,
            text: row.get(1)?,
            created_at: row.get(2)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// How many dictations are stored. `0` on any error (a missing DB is "none").
pub fn count() -> usize {
    match_count("")
}

/// How many dictations match `query` — a blank query counts them all. Counted
/// in SQL rather than by measuring a fetch, so a caller that pages a long
/// store can still report the true total.
pub fn match_count(query: &str) -> usize {
    let Some(path) = db_path() else {
        return 0;
    };
    try_match_count_at(&path, query.trim()).unwrap_or(0)
}

fn try_match_count_at(path: &Path, needle: &str) -> eyre::Result<usize> {
    let conn = open_at(path)?;
    let n: i64 = if needle.is_empty() {
        conn.query_row("SELECT COUNT(*) FROM dictations", [], |row| row.get(0))?
    } else {
        let pattern = format!("%{}%", like_escape(needle));
        conn.query_row(
            r"SELECT COUNT(*) FROM dictations WHERE text LIKE ?1 ESCAPE '\' OR EXISTS(SELECT 1 FROM dictation_stages WHERE dictation_id=dictations.id AND text LIKE ?1 ESCAPE '\')",
            rusqlite::params![pattern],
            |row| row.get(0),
        )?
    };
    Ok(n.max(0) as usize)
}

/// The newest `limit` dictations matching `query` and how many match in all,
/// for a window: one read-only connection, 150 ms wait, "busy" kept apart from
/// "failed". A blank query matches everything.
pub fn query_ui(query: &str, limit: usize) -> UiRead<(Vec<Entry>, usize)> {
    match db_path() {
        Some(path) => query_ui_at(&path, query, limit),
        None => UiRead::Ok((Vec::new(), 0)),
    }
}

pub fn query_ui_at(path: &Path, query: &str, limit: usize) -> UiRead<(Vec<Entry>, usize)> {
    ui_result((|| {
        let Some(conn) = open_ui_at(path)? else {
            return Ok((Vec::new(), 0));
        };
        let needle = query.trim();
        let pattern = format!("%{}%", like_escape(needle));
        let filter = if needle.is_empty() {
            "1=1".to_string()
        } else {
            r"(text LIKE ?1 ESCAPE '\' OR EXISTS(SELECT 1 FROM dictation_stages WHERE dictation_id=dictations.id AND text LIKE ?1 ESCAPE '\'))".to_string()
        };
        let total: i64 = {
            let sql = format!("SELECT COUNT(*) FROM dictations WHERE {filter}");
            if needle.is_empty() {
                conn.query_row(&sql, [], |row| row.get(0))?
            } else {
                conn.query_row(&sql, rusqlite::params![pattern], |row| row.get(0))?
            }
        };
        let sql = format!(
            "SELECT id, text, created_at FROM dictations WHERE {filter}
             ORDER BY created_at DESC, id DESC LIMIT {}",
            if needle.is_empty() { "?1" } else { "?2" }
        );
        let mut stmt = conn.prepare(&sql)?;
        let map = |row: &rusqlite::Row| {
            Ok(Entry { id: row.get(0)?, text: row.get(1)?, created_at: row.get(2)? })
        };
        let rows: Vec<Entry> = if needle.is_empty() {
            stmt.query_map(rusqlite::params![limit as i64], map)?.collect::<Result<_, _>>()?
        } else {
            stmt.query_map(rusqlite::params![pattern, limit as i64], map)?.collect::<Result<_, _>>()?
        };
        Ok((rows, total.max(0) as usize))
    })())
}

/// [`count`] for a window.
pub fn count_ui() -> UiRead<usize> {
    match db_path() {
        Some(path) => count_ui_at(&path),
        None => UiRead::Ok(0),
    }
}

pub fn count_ui_at(path: &Path) -> UiRead<usize> {
    ui_result((|| {
        let Some(conn) = open_ui_at(path)? else { return Ok(0) };
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM dictations", [], |row| row.get(0))?;
        Ok(n.max(0) as usize)
    })())
}

/// What a delete did. A stale click ("Gone") is not an error.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum DeleteOutcome {
    Deleted,
    Gone,
    Failed,
}

/// What a clear did. `Cleared(0)` is an empty store, not a failure.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ClearOutcome {
    Cleared(usize),
    Failed,
}

/// Delete one dictation by id, telling a stale click from a real failure.
pub fn delete_outcome(id: i64) -> DeleteOutcome {
    match db_path() {
        Some(path) => delete_outcome_at(&path, id),
        None => DeleteOutcome::Failed,
    }
}

fn delete_outcome_at(path: &Path, id: i64) -> DeleteOutcome {
    match delete_at(path, id) {
        Ok(0) => DeleteOutcome::Gone,
        Ok(_) => DeleteOutcome::Deleted,
        Err(e) => {
            eprintln!("[history] delete failed: {e}");
            DeleteOutcome::Failed
        }
    }
}

/// Delete one dictation by id. Returns whether a row actually went away.
pub fn delete(id: i64) -> bool {
    delete_outcome(id) == DeleteOutcome::Deleted
}

fn delete_at(path: &Path, id: i64) -> eyre::Result<usize> {
    let conn = open_at(path)?;
    recovery::remove_audio_at(path, id)?;
    Ok(conn.execute(
        "DELETE FROM dictations WHERE id = ?1",
        rusqlite::params![id],
    )?)
}

/// Delete every dictation, telling an empty store from a failure.
pub fn clear_outcome() -> ClearOutcome {
    match db_path() {
        Some(path) => clear_outcome_at(&path),
        None => ClearOutcome::Failed,
    }
}

fn clear_outcome_at(path: &Path) -> ClearOutcome {
    match clear_at(path) {
        Ok(n) => ClearOutcome::Cleared(n),
        Err(e) => {
            eprintln!("[history] clear failed: {e}");
            ClearOutcome::Failed
        }
    }
}

/// Delete every dictation. Returns how many rows went away (`0` on error).
pub fn clear() -> usize {
    match clear_outcome() {
        ClearOutcome::Cleared(n) => n,
        ClearOutcome::Failed => 0,
    }
}

fn clear_at(path: &Path) -> eyre::Result<usize> {
    let conn = open_at(path)?;
    let ids = conn
        .prepare("SELECT id FROM dictations")?
        .query_map([], |row| row.get::<_, i64>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for id in ids {
        recovery::remove_audio_at(path, id)?;
    }
    Ok(conn.execute("DELETE FROM dictations", [])?)
}

#[cfg(test)]
mod tests {
    /// Regression (UI-F25): a store that cannot be opened used to show the
    /// same line as a store with nothing in it, so a user whose history was
    /// unreachable was told they had never dictated anything.
    #[test]
    fn an_unreadable_history_says_so_instead_of_reading_as_empty() {
        let empty = super::empty_state_text(None, false);
        assert!(empty.contains("No dictations yet"), "{empty}");
        let searched = super::empty_state_text(None, true);
        assert!(searched.contains("match your search"), "{searched}");

        let broken = super::empty_state_text(Some("file is not a database"), false);
        assert!(broken.contains("could not be opened"), "{broken}");
        assert!(broken.contains("left untouched"), "{broken}");
        assert!(broken.contains("file is not a database"), "{broken}");
        assert!(!broken.contains("No dictations yet"), "{broken}");
        // The reason wins over the search wording too: a search that cannot
        // reach the store has not "matched nothing".
        let broken_search = super::empty_state_text(Some("database is locked"), true);
        assert!(broken_search.contains("could not be opened"), "{broken_search}");
    }

    #[test]
    fn a_missing_store_is_not_a_fault_but_a_corrupt_one_is() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ld-history-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.join("history.db");

        // A fresh install has no history.db. "Nothing yet", not a fault.
        assert_eq!(super::unreadable_reason_at(&path), None);

        // A real store opens cleanly.
        super::record_at(&path, "hello").unwrap();
        assert_eq!(super::unreadable_reason_at(&path), None);

        // Bytes that are not a database must be reported, not read as empty.
        std::fs::write(&path, b"this is not a SQLite file, it is a text file").unwrap();
        let reason = super::unreadable_reason_at(&path).expect("a corrupt store is a fault");
        let line = super::empty_state_text(Some(&reason), false);
        assert!(line.contains("could not be opened"), "{line}");
        assert!(!line.contains("No dictations yet"), "{line}");

        let _ = std::fs::remove_dir_all(&dir);
    }

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
    fn search_matches_case_insensitively_and_newest_first() {
        let db = test_db("search_basic");
        let now = unix_now();
        insert_at(&db, "Call Sam about Acme", now - 60).unwrap();
        insert_at(&db, "acme rocket skates", now).unwrap();
        insert_at(&db, "unrelated note", now - 30).unwrap();

        let hits = try_search_at(&db, "acme", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].text, "acme rocket skates", "newest first");
        assert_eq!(hits[1].text, "Call Sam about Acme");
    }

    #[test]
    fn search_treats_wildcards_as_literals() {
        let db = test_db("search_wildcards");
        let now = unix_now();
        insert_at(&db, "100% done", now).unwrap();
        insert_at(&db, "nothing to see", now - 10).unwrap();

        // Unescaped, "%" would match every row.
        let hits = try_search_at(&db, "%", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "100% done");

        let underscore = try_search_at(&db, "_", 10).unwrap();
        assert!(underscore.is_empty(), "_ must not match any character");
    }

    #[test]
    fn search_honours_the_limit() {
        let db = test_db("search_limit");
        let now = unix_now();
        for i in 0..5 {
            insert_at(&db, &format!("note {i}"), now - i).unwrap();
        }
        assert_eq!(try_search_at(&db, "note", 3).unwrap().len(), 3);
    }

    #[test]
    fn delete_removes_only_the_named_row() {
        let db = test_db("delete_one");
        let now = unix_now();
        insert_at(&db, "keep me", now - 10).unwrap();
        insert_at(&db, "drop me", now).unwrap();

        let rows = try_recent_at(&db, 10).unwrap();
        let doomed = rows.iter().find(|e| e.text == "drop me").unwrap().id;
        assert_eq!(delete_at(&db, doomed).unwrap(), 1);

        let left = try_recent_at(&db, 10).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].text, "keep me");

        // A stale id deletes nothing rather than erroring.
        assert_eq!(delete_at(&db, doomed).unwrap(), 0);
    }

    #[test]
    fn clear_empties_the_store_and_reports_the_count() {
        let db = test_db("clear_all");
        let now = unix_now();
        insert_at(&db, "one", now - 2).unwrap();
        insert_at(&db, "two", now - 1).unwrap();
        insert_at(&db, "three", now).unwrap();

        assert_eq!(try_match_count_at(&db, "").unwrap(), 3);
        assert_eq!(clear_at(&db).unwrap(), 3);
        assert_eq!(try_match_count_at(&db, "").unwrap(), 0);
        assert!(try_recent_at(&db, 10).unwrap().is_empty());
    }

    #[test]
    fn match_count_counts_the_whole_store_and_a_query() {
        let db = test_db("match_count");
        let now = unix_now();
        insert_at(&db, "acme one", now).unwrap();
        insert_at(&db, "acme two", now - 1).unwrap();
        insert_at(&db, "something else", now - 2).unwrap();

        assert_eq!(try_match_count_at(&db, "").unwrap(), 3);
        assert_eq!(try_match_count_at(&db, "acme").unwrap(), 2);
        assert_eq!(try_match_count_at(&db, "absent").unwrap(), 0);
        // Wildcards are literal here too, exactly as in `search`.
        assert_eq!(try_match_count_at(&db, "%").unwrap(), 0);
    }

    #[test]
    fn entries_carry_their_row_id() {
        let db = test_db("entry_ids");
        record_at(&db, "first").unwrap();
        record_at(&db, "second").unwrap();
        let rows = try_recent_at(&db, 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|e| e.id > 0));
        assert_ne!(rows[0].id, rows[1].id);
    }

    #[test]
    fn record_at_and_prune_at_roundtrip() {
        let db = test_db("record_prune_roundtrip");
        record_at(&db, "hello").unwrap();

        let rows = try_recent_at(&db, 5).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "hello");
    }

    /// The panel reads 1000 rows on open. Reading them from SQLite must stay
    /// cheap; the freeze lives in building views, not here. Invented text only.
    #[test]
    fn recent_1000_is_fast() {
        let db = test_db("recent-1000");
        {
            let mut conn = open_at(&db).unwrap();
            let tx = conn.transaction().unwrap();
            for i in 0..1000_i64 {
                tx.execute(
                    "INSERT INTO dictations (text, created_at) VALUES (?1, ?2)",
                    (format!("Invented dictation {i}: move the review and keep the agenda short"), 1_772_000_000 - i * 1_200),
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }
        let t = std::time::Instant::now();
        let rows = try_recent_at(&db, 1000).unwrap();
        let took = t.elapsed();
        eprintln!("[history-bench] recent(1000) = {took:?}");
        assert_eq!(rows.len(), 1000);
        assert!(took < std::time::Duration::from_millis(250), "recent(1000) took {took:?}");
    }

    /// Hold the write lock the way a finishing dictation does, then read the
    /// way the History panel reads. The read must give up within a fraction of
    /// a second so the panel can say "busy" instead of freezing.
    #[test]
    fn ui_read_returns_fast_while_a_writer_holds_the_lock() {
        let db = test_db("ui-read-busy");
        {
            let conn = open_at(&db).unwrap();
            conn.execute("INSERT INTO dictations (text, created_at) VALUES ('x', 1)", []).unwrap();
        }
        let writer = Connection::open(&db).unwrap();
        writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let t = std::time::Instant::now();
        let got = recent_ui_at(&db, 1000);
        let took = t.elapsed();
        eprintln!("[history-bench] read under writer lock = {took:?}");
        // The old read-write open blocked 2 s. Hosted CI runners sleep coarsely
        // (a 150 ms busy timeout measured 555 ms there), so the bound only has to
        // separate "gave up" from "waited the full 2 s".
        assert!(took < std::time::Duration::from_millis(1000), "UI read blocked {took:?}");
        assert!(matches!(got, UiRead::Busy), "a busy store is not an unreadable one");
        assert_eq!(unreadable_reason_at(&db), None, "busy is not a fault");
        writer.execute_batch("ROLLBACK").unwrap();
    }

    /// Every read the Settings pane makes must give up fast behind a writer,
    /// and must still answer correctly when nobody holds the lock.
    #[test]
    fn settings_reads_are_bounded_behind_a_writer_and_correct_without_one() {
        let db = test_db("settings-reads");
        {
            let mut conn = open_at(&db).unwrap();
            let tx = conn.transaction().unwrap();
            for i in 0..30_i64 {
                tx.execute(
                    "INSERT INTO dictations (text, created_at) VALUES (?1, ?2)",
                    (format!("invented {i} {}", if i % 3 == 0 { "alpha" } else { "beta" }), 1_000 + i),
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }
        match query_ui_at(&db, "alpha", 5) {
            UiRead::Ok((rows, total)) => {
                assert_eq!((rows.len(), total), (5, 10));
                assert!(rows[0].created_at > rows[1].created_at, "newest first");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(query_ui_at(&db, "", 1000), UiRead::Ok((r, 30)) if r.len() == 30));
        assert!(matches!(count_ui_at(&db), UiRead::Ok(30)));

        let writer = Connection::open(&db).unwrap();
        writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let t = std::time::Instant::now();
        let (q, c) = (query_ui_at(&db, "alpha", 5), count_ui_at(&db));
        let took = t.elapsed();
        assert!(took < std::time::Duration::from_millis(1500), "two reads took {took:?}");
        assert!(matches!(q, UiRead::Busy) && matches!(c, UiRead::Busy));
        writer.execute_batch("ROLLBACK").unwrap();
    }

    /// A failure is not "already gone" and not "nothing to delete".
    #[test]
    fn delete_and_clear_tell_failure_from_gone() {
        let db = test_db("delete-outcome");
        insert_at(&db, "one", 1).unwrap();
        insert_at(&db, "two", 2).unwrap();
        let id = try_recent_at(&db, 1).unwrap()[0].id;
        assert_eq!(delete_outcome_at(&db, id), DeleteOutcome::Deleted);
        assert_eq!(delete_outcome_at(&db, id), DeleteOutcome::Gone);
        // A path that cannot be opened as a database is a failure.
        let bad = db.parent().unwrap().join("not-a-db");
        std::fs::create_dir_all(&bad).unwrap();
        assert_eq!(delete_outcome_at(&bad, 1), DeleteOutcome::Failed);
        assert_eq!(clear_outcome_at(&bad), ClearOutcome::Failed);
        assert_eq!(clear_outcome_at(&db), ClearOutcome::Cleared(1));
        assert_eq!(clear_outcome_at(&db), ClearOutcome::Cleared(0));
    }
}
