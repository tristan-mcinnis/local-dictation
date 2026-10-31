//! Durable recovery storage. All production writes run on one bounded writer
//! queue. Audio callbacks never touch SQLite, files, or this queue.
use super::{db_path, open_at, unix_now, Entry, Retention};
use eyre::{eyre, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::Duration;

mod writer;
pub use writer::{
    append_recovery_audio,
    append_recovery_stage,
    begin_recovery,
    finish_recovery,
    finish_recovery_with_error,
    flush_recovery,
    initialize_recovery,
    recovery_storage_failure_notice,
    recovery_storage_notice,
    resume_recovery,
};
use writer::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryStatus {
    Recording,
    Recovering,
    Cancelled,
    Failed,
    Completed,
    Held,
    Interrupted,
    Copied,
}
impl RecoveryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Recording => "recording",
            Self::Recovering => "recovering",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
            Self::Completed => "completed",
            Self::Held => "held",
            Self::Interrupted => "interrupted",
            Self::Copied => "copied",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Recording => "Recording",
            Self::Recovering => "Recovering",
            Self::Cancelled => "Cancelled",
            Self::Failed => "Failed",
            Self::Completed => "Completed",
            Self::Held => "Held",
            Self::Interrupted => "Interrupted",
            Self::Copied => "Copied",
        }
    }
    fn parse(value: &str) -> Self {
        match value {
            "recording" => Self::Recording,
            "recovering" => Self::Recovering,
            "cancelled" => Self::Cancelled,
            "failed" => Self::Failed,
            "held" => Self::Held,
            "interrupted" => Self::Interrupted,
            "copied" => Self::Copied,
            _ => Self::Completed,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageKind {
    Raw,
    Cleaned,
    Transform,
    Output,
    Correction,
}
impl StageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Cleaned => "cleaned",
            Self::Transform => "transform",
            Self::Output => "output",
            Self::Correction => "correction",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Raw => "Raw transcript",
            Self::Cleaned => "Cleaned transcript",
            Self::Transform => "Transform",
            Self::Output => "Output",
            Self::Correction => "Correction",
        }
    }
    fn parse(value: &str) -> Self {
        match value {
            "raw" => Self::Raw,
            "cleaned" => Self::Cleaned,
            "transform" => Self::Transform,
            "correction" => Self::Correction,
            _ => Self::Output,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextStage {
    pub id: i64,
    pub ordinal: u32,
    pub kind: StageKind,
    pub title: String,
    pub text: String,
    pub metadata: String,
    pub created_at: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioRecord {
    pub path: PathBuf,
    pub duration_ms: u64,
    pub expires_at: Option<i64>,
    pub byte_count: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryRecord {
    pub entry: Entry,
    pub status: RecoveryStatus,
    pub audio: Option<AudioRecord>,
    pub stages: Vec<TextStage>,
    pub error: Option<String>,
}

pub(super) fn private_directory(path: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(eyre!("recovery directory must not be a symlink"));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}
pub(super) fn private_file(path: &Path) -> Result<()> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}
pub(super) fn migrate(conn: &Connection) -> Result<()> {
    if conn.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))? >= 1 {
        return Ok(());
    }
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<()> {
        let columns = conn
            .prepare("PRAGMA table_info(dictations)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<HashSet<_>, _>>()?;
        for (name, declaration) in [
            ("recovery_status", "TEXT NOT NULL DEFAULT 'completed'"),
            ("is_recovery", "INTEGER NOT NULL DEFAULT 0"),
            ("audio_bytes", "INTEGER NOT NULL DEFAULT 0"),
            ("audio_sample_rate", "INTEGER NOT NULL DEFAULT 16000"),
            ("audio_expires_at", "INTEGER"),
            ("audio_error", "TEXT"),
            ("recovery_error", "TEXT"),
        ] {
            if !columns.contains(name) {
                conn.execute_batch(&format!(
                    "ALTER TABLE dictations ADD COLUMN {name} {declaration}"
                ))?;
            }
        }
        conn.execute_batch("CREATE TABLE IF NOT EXISTS dictation_stages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            dictation_id INTEGER NOT NULL REFERENCES dictations(id) ON DELETE CASCADE,
            ordinal INTEGER NOT NULL, kind TEXT NOT NULL, text TEXT NOT NULL,
            metadata TEXT NOT NULL DEFAULT '', created_at INTEGER NOT NULL,
            UNIQUE(dictation_id, ordinal));
            CREATE INDEX IF NOT EXISTS idx_dictation_stages_record ON dictation_stages(dictation_id, ordinal);
            PRAGMA user_version=1;")?;
        Ok(())
    })();
    if result.is_ok() {
        conn.execute_batch("COMMIT")?;
    } else {
        let _ = conn.execute_batch("ROLLBACK");
    }
    result
}
fn audio_directory(path: &Path) -> Result<PathBuf> {
    Ok(crate::app_paths::recovery_audio_dir_at(
        path.parent()
            .ok_or_else(|| eyre!("history directory missing"))?,
    ))
}
fn audio_filename(path: &Path, id: i64) -> Result<PathBuf> {
    if id <= 0 {
        return Err(eyre!("invalid recovery identifier"));
    }
    let directory = audio_directory(path)?;
    if std::fs::symlink_metadata(&directory).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(eyre!("recovery audio directory must not be a symlink"));
    }
    Ok(directory.join(format!("{id}.wav")))
}
pub(super) fn remove_audio_at(path: &Path, id: i64) -> Result<()> {
    let audio = audio_filename(path, id)?;
    match std::fs::symlink_metadata(&audio) {
        Ok(metadata) => {
            if !metadata.is_file() && !metadata.file_type().is_symlink() {
                return Err(eyre!("invalid recovery audio file"));
            }
            std::fs::remove_file(&audio)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}
pub(super) fn audio_path_at(path: &Path, id: i64) -> Result<Option<PathBuf>> {
    if id <= 0 {
        return Ok(None);
    }
    let conn = open_at(path)?;
    audio_path_on(&conn, path, id, true)
}

/// Find the saved audio for `id`. With `sweep`, expired audio is deleted and
/// its row updated; without it (a window's read-only connection) expired audio
/// is simply reported as absent and left for a writer to sweep.
fn audio_path_on(conn: &Connection, path: &Path, id: i64, sweep: bool) -> Result<Option<PathBuf>> {
    let row: Option<(Option<i64>, String)> = conn
        .query_row(
            "SELECT audio_expires_at,recovery_status FROM dictations WHERE id=?1 AND is_recovery=1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((expires, status)) = row else {
        return Ok(None);
    };
    if expires.is_some_and(|date| date <= unix_now())
        && status != "recording"
        && status != "recovering"
    {
        if sweep {
            remove_audio_at(path, id)?;
            conn.execute("UPDATE dictations SET audio_bytes=0,audio_error='Audio expired under retention setting' WHERE id=?1", [id])?;
        }
        return Ok(None);
    }
    let audio = audio_filename(path, id)?;
    match std::fs::symlink_metadata(&audio) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() && meta.len() > 44 => {
            Ok(Some(audio))
        }
        Ok(_) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryUsage {
    pub audio_count: usize,
    pub audio_bytes: u64,
    pub record_count: usize,
}

/// Audio and record counts for a window: read-only, 150 ms wait.
pub fn recovery_usage_ui() -> super::UiRead<RecoveryUsage> {
    super::ui_result((|| {
        let Some(path) = db_path() else {
            return Ok(RecoveryUsage { audio_count: 0, audio_bytes: 0, record_count: 0 });
        };
        let Some(conn) = super::open_ui_at(&path)? else {
            return Ok(RecoveryUsage { audio_count: 0, audio_bytes: 0, record_count: 0 });
        };
        let (count, bytes, records) = conn.query_row(
            "SELECT COALESCE(SUM(audio_bytes>0),0),COALESCE(SUM(audio_bytes),0),COUNT(*) FROM dictations",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)),
        )?;
        Ok(RecoveryUsage {
            audio_count: usize::try_from(count)?,
            audio_bytes: u64::try_from(bytes)?,
            record_count: usize::try_from(records)?,
        })
    })())
}

pub fn recovery_audio_path(id: i64) -> Result<Option<PathBuf>> {
    audio_path_at(
        &db_path().ok_or_else(|| eyre!("history path unavailable"))?,
        id,
    )
}
#[cfg(test)]
fn detail_at(path: &Path, id: i64) -> Result<Option<RecoveryRecord>> {
    let conn = open_at(path)?;
    detail_on(&conn, path, id, true)
}

/// One record for a window: read-only, short busy timeout, never writes.
/// Call it off the main thread.
pub fn entry_detail_ui(id: i64) -> super::UiRead<Option<RecoveryRecord>> {
    super::ui_result((|| {
        let path = db_path().ok_or_else(|| eyre!("history path unavailable"))?;
        match super::open_ui_at(&path)? {
            Some(conn) => detail_on(&conn, &path, id, false),
            None => Ok(None),
        }
    })())
}

fn detail_on(conn: &Connection, path: &Path, id: i64, sweep: bool) -> Result<Option<RecoveryRecord>> {
    let row = conn
        .query_row(
            "SELECT text,created_at,recovery_status,audio_sample_rate,audio_expires_at,
         COALESCE(recovery_error,audio_error),is_recovery FROM dictations WHERE id=?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, u32>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, bool>(6)?,
                ))
            },
        )
        .optional()?;
    let Some((text, created_at, status, sample_rate, expires_at, error, is_recovery)) = row else {
        return Ok(None);
    };
    let mut stages = conn.prepare("SELECT id,ordinal,kind,text,metadata,created_at FROM dictation_stages WHERE dictation_id=?1 ORDER BY ordinal")?
        .query_map([id], |row| {
            let kind = StageKind::parse(&row.get::<_,String>(2)?);
            Ok(TextStage{id:row.get(0)?,ordinal:row.get(1)?,kind,title:kind.label().into(),text:row.get(3)?,metadata:row.get(4)?,created_at:row.get(5)?})
        })?.collect::<Result<Vec<_>,_>>()?;
    if stages.is_empty() && !is_recovery && !text.is_empty() {
        stages.push(TextStage {
            id: 0,
            ordinal: 1,
            kind: StageKind::Output,
            title: "Original saved output".into(),
            text: text.clone(),
            metadata: "legacy".into(),
            created_at,
        });
    }
    let audio = (if id <= 0 { Ok(None) } else { audio_path_on(conn, path, id, sweep) })?
        .map(|audio| -> Result<AudioRecord> {
            let bytes = std::fs::metadata(&audio)?.len();
            Ok(AudioRecord {
                path: audio,
                duration_ms: bytes.saturating_sub(44) / 4 * 1000 / u64::from(sample_rate.max(1)),
                expires_at,
                byte_count: bytes,
            })
        })
        .transpose()?;
    Ok(Some(RecoveryRecord {
        entry: Entry {
            id,
            text,
            created_at,
        },
        status: RecoveryStatus::parse(&status),
        audio,
        stages,
        error,
    }))
}
pub fn read_recovery_audio(id: i64) -> Result<Option<(Vec<f32>, u32)>> {
    read_audio_at(
        &db_path().ok_or_else(|| eyre!("history path unavailable"))?,
        id,
    )
}
fn read_audio_at(path: &Path, id: i64) -> Result<Option<(Vec<f32>, u32)>> {
    let Some(audio) = audio_path_at(path, id)? else {
        return Ok(None);
    };
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(audio)?;
    let length = file.metadata()?.len();
    if length > u64::from(u32::MAX) {
        return Err(eyre!("recovery audio is too large"));
    }
    let mut header = [0u8; 44];
    file.read_exact(&mut header)?;
    if &header[0..4] != b"RIFF"
        || &header[8..12] != b"WAVE"
        || u16::from_le_bytes([header[20], header[21]]) != 3
        || u16::from_le_bytes([header[22], header[23]]) != 1
        || u16::from_le_bytes([header[34], header[35]]) != 32
    {
        return Err(eyre!("unsupported recovery audio format"));
    }
    let rate = u32::from_le_bytes(header[24..28].try_into()?);
    if rate == 0 {
        return Err(eyre!("invalid recovery audio sample rate"));
    }
    // Actual complete samples win over a stale header after an abrupt exit.
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some((
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| f32::from_le_bytes(*chunk))
            .collect(),
        rate,
    )))
}

#[derive(Clone)]
struct Policy {
    enabled: bool,
    window: Option<i64>,
    cap: u64,
    text_window: Option<i64>,
}
impl Policy {
    fn configured() -> Self {
        let settings = crate::settings::Settings::load();
        Self {
            enabled: settings.resolve_audio_history_enabled(),
            window: Retention::parse(&settings.resolve_audio_retention()).cutoff_secs(),
            cap: settings
                .resolve_audio_storage_cap_mb()
                .saturating_mul(1024 * 1024),
            text_window: Retention::parse(&settings.resolve_history_retention()).cutoff_secs(),
        }
    }
}
struct RecoveryStore {
    path: PathBuf,
    conn: Connection,
    policy: Policy,
}
impl RecoveryStore {
    fn open(path: PathBuf, policy: Policy) -> Result<Self> {
        let conn = open_at(&path)?;
        private_directory(&audio_directory(&path)?)?;
        Ok(Self { path, conn, policy })
    }
    fn recover_interrupted(&self) -> Result<()> {
        self.conn.execute("UPDATE dictations SET recovery_status='interrupted',recovery_error='Recording was interrupted before completion' WHERE is_recovery=1 AND recovery_status IN ('recording','recovering')",[])?;
        // Reconcile complete samples written immediately before a crash with
        // the header/metadata, and remove files no history row owns anymore.
        for entry in std::fs::read_dir(audio_directory(&self.path)?)? {
            let entry = entry?;
            if entry.path().extension().and_then(|v| v.to_str()) != Some("wav") {
                continue;
            }
            let Some(id) = entry
                .path()
                .file_stem()
                .and_then(|v| v.to_str())
                .and_then(|v| v.parse::<i64>().ok())
            else {
                continue;
            };
            if id <= 0 {
                continue;
            }
            let rate: Option<u32> = self
                .conn
                .query_row(
                    "SELECT audio_sample_rate FROM dictations WHERE id=?1",
                    [id],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(rate) = rate else {
                remove_audio_at(&self.path, id)?;
                continue;
            };
            if entry.file_type()?.is_symlink() {
                return Err(eyre!("invalid recovery audio symlink"));
            }
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(entry.path())?;
            let length = file.metadata()?.len();
            if length >= 44 && length <= u64::from(u32::MAX) {
                let bytes = (length - 44) / 4 * 4;
                file.set_len(bytes + 44)?;
                file.seek(SeekFrom::Start(0))?;
                file.write_all(&wav_header(rate, bytes as u32))?;
                file.sync_all()?;
                self.conn.execute(
                    "UPDATE dictations SET audio_bytes=?1 WHERE id=?2",
                    params![i64::try_from(bytes + 44)?, id],
                )?;
            }
        }
        self.expire(unix_now())?;
        Ok(())
    }
    fn begin(&self, now: i64) -> Result<i64> {
        self.conn.execute("INSERT INTO dictations(text,created_at,recovery_status,is_recovery,audio_expires_at) VALUES('(Recording…)',?1,'recording',1,?2)",
            params![now,self.policy.window.map(|window|now.saturating_add(window))])?;
        Ok(self.conn.last_insert_rowid())
    }
    fn stage(&mut self, id: i64, kind: StageKind, text: &str, metadata: &str) -> Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }
        let transaction = self.conn.transaction()?;
        transaction.execute("INSERT INTO dictation_stages(dictation_id,ordinal,kind,text,metadata,created_at)
            SELECT ?1,COALESCE(MAX(ordinal),0)+1,?2,?3,?4,?5 FROM dictation_stages WHERE dictation_id=?1",
            params![id,kind.as_str(),text,metadata,unix_now()])?;
        transaction.execute(
            "UPDATE dictations SET text=?1 WHERE id=?2",
            params![text, id],
        )?;
        transaction.commit()?;
        Ok(())
    }
    fn finish(&self, id: i64, status: RecoveryStatus, error: Option<&str>) -> Result<()> {
        self.conn.execute("UPDATE dictations SET recovery_status=?1,recovery_error=?2,
            text=CASE WHEN is_recovery=1 AND NOT EXISTS(SELECT 1 FROM dictation_stages WHERE dictation_id=?3)
            THEN ?4 ELSE text END WHERE id=?3",params![status.as_str(),error,id,format!("({} before transcription)",status.label())])?;
        Ok(())
    }
    fn audio_error(&self, id: i64, message: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE dictations SET audio_error=?1 WHERE id=?2",
            params![message, id],
        )?;
        Ok(())
    }
    /// Apply the audio-retention window. Runs on every Finish and on the 60 s
    /// idle tick, so it must be a no-op when nothing changed: it used to issue
    /// one journaled UPDATE per history row (and re-delete every expired clip)
    /// on every pass, forever. Now one set-based UPDATE touches only rows
    /// whose expiry is wrong, and only rows that still hold audio are removed,
    /// all in one transaction.
    fn expire(&self, now: i64) -> Result<()> {
        const SETTLED: &str = "is_recovery=1 AND recovery_status NOT IN ('recording','recovering')";
        let Some(window) = self.policy.window else {
            self.conn.execute(
                "UPDATE dictations SET audio_expires_at=NULL
                 WHERE is_recovery=1 AND audio_expires_at IS NOT NULL",
                [],
            )?;
            return Ok(());
        };
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            &format!(
                "UPDATE dictations SET audio_expires_at=created_at+?1
                 WHERE {SETTLED} AND audio_expires_at IS NOT created_at+?1"
            ),
            [window],
        )?;
        let expired = tx
            .prepare(&format!(
                "SELECT id FROM dictations
                 WHERE {SETTLED} AND audio_bytes>0 AND created_at+?1<=?2"
            ))?
            .query_map(params![window, now], |row| row.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for id in expired {
            remove_audio_at(&self.path, id)?;
            tx.execute("UPDATE dictations SET audio_bytes=0,audio_error=COALESCE(audio_error,'Audio expired under retention setting') WHERE id=?1",[id])?;
        }
        tx.commit()?;
        Ok(())
    }
    fn make_room(&self, current: i64, additional: u64) -> Result<()> {
        if self.policy.cap == 0 {
            return Ok(());
        }
        let used = self.conn.query_row(
            "SELECT COALESCE(SUM(audio_bytes),0) FROM dictations",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        let mut used = u64::try_from(used)?;
        if used.saturating_add(additional) <= self.policy.cap {
            return Ok(());
        }
        let rows=self.conn.prepare("SELECT id,audio_bytes FROM dictations WHERE id!=?1 AND audio_bytes>0 AND recovery_status NOT IN ('recording','recovering') ORDER BY created_at,id")?
            .query_map([current],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,i64>(1)?)))?.collect::<Result<Vec<_>,_>>()?;
        for (id, bytes) in rows {
            remove_audio_at(&self.path, id)?;
            self.conn.execute("UPDATE dictations SET audio_bytes=0,audio_error='Audio removed to stay within storage limit' WHERE id=?1",[id])?;
            set_info_notice("Older recovery audio was removed to stay within your storage limit. Text versions were kept.");
            used = used.saturating_sub(u64::try_from(bytes)?);
            if used.saturating_add(additional) <= self.policy.cap {
                return Ok(());
            }
        }
        Err(eyre!("Audio storage limit reached. This recording's audio is only partly saved. Increase the limit in Settings."))
    }
    fn append_audio(&self, id: i64, samples: &[f32], rate: u32) -> Result<()> {
        if samples.is_empty() || !self.policy.enabled || self.policy.window == Some(0) {
            return Ok(());
        }
        if rate == 0 || rate > 384_000 {
            return Err(eyre!("invalid audio sample rate"));
        }
        let state: Option<(i64, u32, Option<String>)> = self
            .conn
            .query_row(
                "SELECT audio_bytes,audio_sample_rate,audio_error FROM dictations WHERE id=?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((stored, previous_rate, error)) = state else {
            return Err(eyre!("recovery record was deleted"));
        };
        if error.is_some() {
            return Ok(());
        } // Never join audio across a missing chunk.
        if stored > 0 && previous_rate != rate {
            return Err(eyre!("audio sample rate changed during recording"));
        }
        let audio = audio_filename(&self.path, id)?;
        let old_size = std::fs::metadata(&audio).map(|m| m.len()).unwrap_or(0);
        let added = (samples.len() as u64)
            .checked_mul(4)
            .ok_or_else(|| eyre!("audio size overflow"))?;
        let final_size = old_size
            .max(44)
            .checked_add(added)
            .ok_or_else(|| eyre!("audio size overflow"))?;
        if final_size > u64::from(u32::MAX) {
            return Err(eyre!(
                "Recording exceeds WAV size limit; remaining audio was not saved"
            ));
        }
        self.make_room(id, added + if old_size == 0 { 44 } else { 0 })?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&audio)?;
        if old_size == 0 {
            file.write_all(&wav_header(rate, 0))?;
        }
        file.seek(SeekFrom::End(0))?;
        let bytes: Vec<u8> = samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        file.write_all(&bytes)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&wav_header(rate, (final_size - 44) as u32))?;
        file.sync_all()?;
        self.conn.execute(
            "UPDATE dictations SET audio_bytes=?1,audio_sample_rate=?2 WHERE id=?3",
            params![i64::try_from(final_size)?, rate, id],
        )?;
        Ok(())
    }
}
fn wav_header(rate: u32, bytes: u32) -> [u8; 44] {
    let mut h = [0; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(bytes + 36).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&3u16.to_le_bytes());
    h[22..24].copy_from_slice(&1u16.to_le_bytes());
    h[24..28].copy_from_slice(&rate.to_le_bytes());
    h[28..32].copy_from_slice(&(rate * 4).to_le_bytes());
    h[32..34].copy_from_slice(&4u16.to_le_bytes());
    h[34..36].copy_from_slice(&32u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&bytes.to_le_bytes());
    h
}

#[cfg(test)]
mod tests;
