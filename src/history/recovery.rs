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
        remove_audio_at(path, id)?;
        conn.execute("UPDATE dictations SET audio_bytes=0,audio_error='Audio expired under retention setting' WHERE id=?1", [id])?;
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
pub fn recovery_usage() -> Result<RecoveryUsage> {
    let conn = open_at(&db_path().ok_or_else(|| eyre!("history path unavailable"))?)?;
    let (count,bytes,records)=conn.query_row(
        "SELECT COALESCE(SUM(audio_bytes>0),0),COALESCE(SUM(audio_bytes),0),COUNT(*) FROM dictations",[],
        |row|Ok((row.get::<_,i64>(0)?,row.get::<_,i64>(1)?,row.get::<_,i64>(2)?)))?;
    Ok(RecoveryUsage {
        audio_count: usize::try_from(count)?,
        audio_bytes: u64::try_from(bytes)?,
        record_count: usize::try_from(records)?,
    })
}

pub fn recovery_audio_path(id: i64) -> Result<Option<PathBuf>> {
    audio_path_at(
        &db_path().ok_or_else(|| eyre!("history path unavailable"))?,
        id,
    )
}
pub fn entry_detail(id: i64) -> Result<Option<RecoveryRecord>> {
    detail_at(
        &db_path().ok_or_else(|| eyre!("history path unavailable"))?,
        id,
    )
}
fn detail_at(path: &Path, id: i64) -> Result<Option<RecoveryRecord>> {
    let conn = open_at(path)?;
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
    let audio = audio_path_at(path, id)?
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
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes(chunk.try_into().unwrap()))
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

// The queue is bounded to 64 chunks (about eight seconds at a 120ms cadence).
// A full/failed writer reports visibly, and marks the record incomplete; it
// never blocks ASR or pretends that missing audio was preserved.
#[derive(Debug)]
enum Command {
    Begin(u64),
    Resume(u64, i64),
    Audio(u64, Vec<f32>, u32),
    Stage(u64, StageKind, String, String),
    Finish(u64, RecoveryStatus, Option<String>),
    Flush(mpsc::SyncSender<()>),
}
impl Command {
    fn generation(&self) -> u64 {
        match self {
            Self::Begin(g)
            | Self::Resume(g, _)
            | Self::Audio(g, _, _)
            | Self::Stage(g, _, _, _)
            | Self::Finish(g, _, _) => *g,
            Self::Flush(_) => 0,
        }
    }
}
static WRITER: OnceLock<Option<mpsc::SyncSender<Command>>> = OnceLock::new();
#[derive(Clone)]
struct StorageNotice {
    message: String,
    is_failure: bool,
}
/// The CURRENT notice. A failure here is what the recording pill shows, so it
/// is scoped to the capture it happened in: the next record that opens
/// successfully clears it (`clear_failure_notice`). It used to be set-only, so
/// one full queue replaced the live preview with "Recovery warning" until the
/// app restarted.
static NOTICE: Mutex<Option<StorageNotice>> = Mutex::new(None);
/// The most recent failure, kept for Settings → History after the pill's copy
/// has been cleared.
static LAST_FAILURE: Mutex<Option<String>> = Mutex::new(None);
static LOST: OnceLock<Mutex<HashMap<u64, bool>>> = OnceLock::new();
fn set_notice(message: &str) {
    if let Ok(mut last) = LAST_FAILURE.lock() {
        *last = Some(message.into());
    }
    if let Ok(mut notice) = NOTICE.lock() {
        *notice = Some(StorageNotice {
            message: message.into(),
            is_failure: true,
        });
    }
}
fn set_info_notice(message: &str) {
    if let Ok(mut notice) = NOTICE.lock() {
        if !notice.as_ref().is_some_and(|notice| notice.is_failure) {
            *notice = Some(StorageNotice {
                message: message.into(),
                is_failure: false,
            });
        }
    }
}
/// A new record opened and is being written: storage works again, so the
/// pill stops warning. Informational notices are left alone.
fn clear_failure_notice() {
    if let Ok(mut notice) = NOTICE.lock() {
        *notice = without_failure(notice.take());
    }
}
/// Drop a failure notice, keep an informational one.
fn without_failure(notice: Option<StorageNotice>) -> Option<StorageNotice> {
    notice.filter(|notice| !notice.is_failure)
}
/// For Settings → History: the current notice, else the last failure seen
/// since launch (the pill's copy may already have been cleared).
pub fn recovery_storage_notice() -> Option<String> {
    NOTICE
        .lock()
        .ok()
        .and_then(|notice| notice.as_ref().map(|notice| notice.message.clone()))
        .or_else(|| LAST_FAILURE.lock().ok().and_then(|last| last.clone()))
}
/// Actual write/queue failures for the pill; configured-cap retirement is
/// informational and belongs in History, not an "audio not saved" warning.
pub fn recovery_storage_failure_notice() -> Option<String> {
    NOTICE.lock().ok().and_then(|notice| {
        notice
            .as_ref()
            .filter(|notice| notice.is_failure)
            .map(|notice| notice.message.clone())
    })
}
pub fn initialize_recovery() -> bool {
    writer().is_some()
}
fn writer() -> Option<&'static mpsc::SyncSender<Command>> {
    WRITER
        .get_or_init(|| {
            let Some(path) = db_path() else {
                set_notice(
                    "Recovery storage is unavailable. This dictation may not be recoverable.",
                );
                return None;
            };
            let (sender, receiver) = mpsc::sync_channel::<Command>(64);
            match std::thread::Builder::new()
                .name("dictation-recovery".into())
                .spawn(move || run_writer(path, receiver, Policy::configured()))
            {
                Ok(_) => Some(sender),
                Err(_) => {
                    set_notice("Recovery storage could not start.");
                    None
                }
            }
        })
        .as_ref()
}

fn process_command(
    store: &mut RecoveryStore,
    ids: &mut HashMap<u64, i64>,
    command: Command,
) -> Result<()> {
    if let Command::Flush(done) = command {
        let _ = done.send(());
        return Ok(());
    }
    let generation = command.generation();
    if let Command::Resume(_, id) = command {
        // A missing/deleted resume target must never turn later callbacks
        // into a brand-new capture under the same generation.
        ids.insert(generation, -1);
        let exists: bool = store.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM dictations WHERE id=?1)",
            [id],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(eyre!("recovery record no longer exists"));
        }
        ids.insert(generation, id);
        return store.finish(id, RecoveryStatus::Recovering, None);
    }
    let id = match ids.get(&generation) {
        Some(id) => *id,
        None => {
            let id = store.begin(unix_now())?;
            ids.insert(generation, id);
            clear_failure_notice();
            id
        }
    };
    match command {
        Command::Begin(_) => {}
        Command::Resume(..) | Command::Flush(_) => unreachable!(),
        Command::Audio(_, samples, rate) => {
            if let Err(error) = store.append_audio(id, &samples, rate) {
                let message = error.to_string();
                let _ = store.audio_error(id, &message);
                set_notice(&message);
                return Err(error);
            }
        }
        Command::Stage(_, kind, text, metadata) => store.stage(id, kind, &text, &metadata)?,
        Command::Finish(_, status, error) => {
            store.finish(id, status, error.as_deref())?;
            store.expire(unix_now())?;
            super::prune_at(&store.path, store.policy.text_window)?;
        }
    }
    Ok(())
}

fn run_writer(path: PathBuf, receiver: mpsc::Receiver<Command>, policy: Policy) {
    let mut store = match RecoveryStore::open(path, policy) {
        Ok(store) => store,
        Err(_) => {
            set_notice("Recovery storage could not open. Your dictation can continue, but recovery is unavailable.");
            return;
        }
    };
    if store.recover_interrupted().is_err() {
        set_notice("Some recovery records could not be checked after restart.");
    }
    let mut ids = HashMap::new();
    loop {
        let command = match receiver.recv_timeout(Duration::from_secs(60)) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if store.expire(unix_now()).is_err() {
                    set_notice("Expired recovery audio could not be removed.");
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let generation = command.generation();
        let result = process_command(&mut store, &mut ids, command);
        let lost = LOST
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .map(|mut pending| std::mem::take(&mut *pending))
            .unwrap_or_default();
        for (lost_generation, terminal) in lost {
            let id = match ids.get(&lost_generation).copied() {
                Some(id) => Some(id),
                None => store.begin(unix_now()).ok(),
            };
            if let Some(id) = id {
                ids.insert(lost_generation, id);
                let _=store.audio_error(id,"Some recovery data could not be saved because storage was busy. Audio may be incomplete.");
                if terminal {
                    let _ = store.finish(
                        id,
                        RecoveryStatus::Failed,
                        Some("Recovery storage was busy when this dictation finished."),
                    );
                }
            }
        }
        if result.is_err() {
            if let Some(id) = ids.get(&generation) {
                let _ = store.finish(
                    *id,
                    RecoveryStatus::Failed,
                    Some(
                        "A recovery update could not be saved; some audio or text may be missing.",
                    ),
                );
            }
            if recovery_storage_notice().is_none() {
                set_notice("A recovery update could not be saved. The latest audio or text may be missing.");
            }
            eprintln!("[history] recovery update failed; details available in recovery history");
        }
    }
}

fn enqueue(command: Command) -> bool {
    let generation = command.generation();
    let terminal = matches!(command, Command::Finish(..));
    let accepted = writer().is_some_and(|writer| writer.try_send(command).is_ok());
    if !accepted {
        if let Ok(mut lost) = LOST.get_or_init(|| Mutex::new(HashMap::new())).lock() {
            *lost.entry(generation).or_insert(false) |= terminal;
        }
        set_notice(
            "Recovery storage is busy or unavailable. This dictation may be only partly saved.",
        );
    }
    accepted
}
/// Drain earlier updates before graceful quit/reload. This is a bounded
/// shutdown-only wait: never call it from the audio callback or live ASR path.
pub fn flush_recovery(timeout: Duration) -> bool {
    let Some(writer) = WRITER.get() else {
        return true;
    };
    let Some(writer) = writer else { return false };
    let flushed = flush_sender(writer, timeout);
    if !flushed {
        set_notice("Recovery storage did not finish saving before shutdown. The latest audio may be incomplete.");
    }
    flushed
}
fn flush_sender(writer: &mpsc::SyncSender<Command>, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    let (done, receipt) = mpsc::sync_channel(1);
    let mut command = Command::Flush(done);
    loop {
        match writer.try_send(command) {
            Ok(()) => break,
            Err(mpsc::TrySendError::Disconnected(_)) => return false,
            Err(mpsc::TrySendError::Full(rejected)) => command = rejected,
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    receipt
        .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
        .is_ok()
}

pub fn begin_recovery(generation: u64) -> bool {
    enqueue(Command::Begin(generation))
}
pub fn resume_recovery(generation: u64, id: i64) -> bool {
    enqueue(Command::Resume(generation, id))
}
pub fn append_recovery_audio(generation: u64, samples: Vec<f32>, sample_rate: u32) -> bool {
    enqueue(Command::Audio(generation, samples, sample_rate))
}
pub fn append_recovery_stage(generation: u64, kind: StageKind, text: &str, metadata: &str) -> bool {
    enqueue(Command::Stage(
        generation,
        kind,
        text.into(),
        metadata.into(),
    ))
}
pub fn finish_recovery(generation: u64, status: RecoveryStatus) -> bool {
    finish_recovery_with_error(generation, status, None)
}
pub fn finish_recovery_with_error(
    generation: u64,
    status: RecoveryStatus,
    error: Option<&str>,
) -> bool {
    enqueue(Command::Finish(
        generation,
        status,
        error.map(str::to_owned),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    const DAY: i64 = 86_400;
    struct Fixture {
        directory: PathBuf,
        store: RecoveryStore,
    }
    impl Fixture {
        fn new(label: &str, policy: Policy) -> Self {
            let directory =
                std::env::temp_dir().join(format!("ld-recovery-{}-{label}", std::process::id()));
            let _ = std::fs::remove_dir_all(&directory);
            let store = RecoveryStore::open(directory.join("history.db"), policy).unwrap();
            Self { directory, store }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }
    fn policy() -> Policy {
        Policy {
            enabled: true,
            window: Some(30 * DAY),
            cap: 0,
            text_window: None,
        }
    }

    #[test]
    fn legacy_database_migrates_without_changing_rows_or_ids() {
        let directory =
            std::env::temp_dir().join(format!("ld-recovery-{}-legacy", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("history.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE dictations(id INTEGER PRIMARY KEY AUTOINCREMENT,text TEXT NOT NULL,created_at INTEGER NOT NULL);
            INSERT INTO dictations(id,text,created_at) VALUES(19,'Original legacy text',12345);").unwrap();
        drop(conn);
        let detail = detail_at(&path, 19).unwrap().unwrap();
        assert_eq!(detail.entry.id, 19);
        assert_eq!(detail.entry.text, "Original legacy text");
        assert_eq!(detail.entry.created_at, 12345);
        assert_eq!(detail.status, RecoveryStatus::Completed);
        assert!(detail.audio.is_none());
        assert_eq!(detail.stages[0].text, "Original legacy text");
        assert_eq!(super::super::try_recent_at(&path, 10).unwrap().len(), 1);
        drop(open_at(&path).unwrap()); // Migration remains idempotent.
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn cancelled_record_survives_restart_with_original_audio() {
        let fixture = Fixture::new("cancelled", policy());
        let store = &fixture.store;
        let id = store.begin(unix_now()).unwrap();
        store.append_audio(id, &[0.0, 0.25, -0.5], 16_000).unwrap();
        store.finish(id, RecoveryStatus::Cancelled, None).unwrap();
        let reopened = RecoveryStore::open(store.path.clone(), policy()).unwrap();
        reopened.recover_interrupted().unwrap();
        let detail = detail_at(&store.path, id).unwrap().unwrap();
        assert_eq!(detail.status, RecoveryStatus::Cancelled);
        assert!(detail.stages.is_empty());
        let (samples, rate) = read_audio_at(&store.path, id).unwrap().unwrap();
        assert_eq!(samples, vec![0.0, 0.25, -0.5]);
        assert_eq!(rate, 16_000);
    }
    #[test]
    fn raw_cleanup_transforms_and_outputs_are_append_only_and_ordered() {
        let mut fixture = Fixture::new("layers", policy());
        let store = &mut fixture.store;
        let id = store.begin(unix_now()).unwrap();
        for (kind, text) in [
            (StageKind::Raw, "raw original"),
            (StageKind::Cleaned, "Clean original."),
            (StageKind::Output, "Clean original."),
            (StageKind::Transform, "Translated version"),
            (StageKind::Output, "Translated version"),
        ] {
            store
                .stage(id, kind, text, "{\"model\":\"fixture\"}")
                .unwrap();
        }
        store.finish(id, RecoveryStatus::Copied, None).unwrap();
        let detail = detail_at(&store.path, id).unwrap().unwrap();
        assert_eq!(
            detail
                .stages
                .iter()
                .map(|stage| stage.ordinal)
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );
        assert_eq!(detail.stages[0].text, "raw original");
        assert_eq!(detail.stages[1].text, "Clean original.");
        assert_eq!(detail.entry.text, "Translated version");
        assert_eq!(detail.status, RecoveryStatus::Copied);
        assert_eq!(
            super::super::try_search_at(&store.path, "raw original", 10)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            super::super::try_match_count_at(&store.path, "raw original").unwrap(),
            1
        );
    }
    #[test]
    fn failure_keeps_audio_raw_text_and_failure_reason() {
        let mut fixture = Fixture::new("failure", policy());
        let store = &mut fixture.store;
        let id = store.begin(unix_now()).unwrap();
        store.append_audio(id, &[0.2; 160], 16_000).unwrap();
        store
            .stage(id, StageKind::Raw, "Raw recovery text", "")
            .unwrap();
        store
            .finish(id, RecoveryStatus::Failed, Some("Cleanup did not finish"))
            .unwrap();
        let detail = detail_at(&store.path, id).unwrap().unwrap();
        assert_eq!(detail.status, RecoveryStatus::Failed);
        assert_eq!(detail.error.as_deref(), Some("Cleanup did not finish"));
        assert_eq!(detail.stages[0].text, "Raw recovery text");
        assert_eq!(detail.audio.unwrap().duration_ms, 10);
    }
    #[test]
    fn chunked_wav_roundtrip_preserves_float_samples_and_private_modes() {
        let fixture = Fixture::new("wav-private", policy());
        let store = &fixture.store;
        let id = store.begin(unix_now()).unwrap();
        let samples = [-0.0, 0.125, -0.75, 0.33333334, 1.0];
        store.append_audio(id, &samples[..2], 16_000).unwrap();
        store.append_audio(id, &samples[2..], 16_000).unwrap();
        let actual = read_audio_at(&store.path, id).unwrap().unwrap().0;
        assert_eq!(
            actual.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            samples.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
        );
        assert_eq!(
            std::fs::metadata(&store.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&fixture.directory)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(audio_directory(&store.path).unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(audio_filename(&store.path, id).unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    #[test]
    fn audio_expires_after_thirty_days_but_text_versions_remain() {
        let mut fixture = Fixture::new("audio-expiry", policy());
        let store = &mut fixture.store;
        let now = unix_now();
        let id = store.begin(now - 31 * DAY).unwrap();
        store.append_audio(id, &[0.2; 16], 16_000).unwrap();
        store
            .stage(id, StageKind::Raw, "Keep this transcript", "")
            .unwrap();
        // Active recording remains protected even if a clock changes far ahead.
        store.expire(now).unwrap();
        assert!(audio_filename(&store.path, id).unwrap().exists());
        store.finish(id, RecoveryStatus::Completed, None).unwrap();
        store.expire(now).unwrap();
        assert!(!audio_filename(&store.path, id).unwrap().exists());
        let detail = detail_at(&store.path, id).unwrap().unwrap();
        assert!(detail.audio.is_none());
        assert_eq!(detail.stages[0].text, "Keep this transcript");
        assert_eq!(detail.entry.text, "Keep this transcript");
    }
    fn total_changes(store: &RecoveryStore) -> i64 {
        store
            .conn
            .query_row("SELECT total_changes()", [], |row| row.get(0))
            .unwrap()
    }
    #[test]
    fn a_new_record_clears_a_failure_notice_but_not_an_informational_one() {
        // Regression: the notice was set-only, so one full queue turned the
        // live preview into "Recovery warning" until the app restarted. (Pure
        // helper on purpose: NOTICE is process-global and other tests run the
        // real writer.)
        let failure = StorageNotice { message: "busy".into(), is_failure: true };
        let info = StorageNotice { message: "cap".into(), is_failure: false };
        assert!(without_failure(Some(failure)).is_none());
        assert_eq!(without_failure(Some(info)).map(|n| n.message).as_deref(), Some("cap"));
        assert!(without_failure(None).is_none());
    }
    #[test]
    fn a_second_expiry_pass_writes_nothing() {
        // Regression: every pass rewrote every row (one journaled UPDATE each)
        // and re-expired every already-expired clip, every 60 s, forever.
        let mut fixture = Fixture::new("expiry-idle", policy());
        let store = &mut fixture.store;
        let now = unix_now();
        for age in [40, 35, 3, 2, 1] {
            let id = store.begin(now - age * DAY).unwrap();
            store.append_audio(id, &[0.2; 16], 16_000).unwrap();
            store.stage(id, StageKind::Raw, "text", "").unwrap();
            store.finish(id, RecoveryStatus::Completed, None).unwrap();
        }
        store.expire(now).unwrap();
        let expired: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM dictations WHERE audio_bytes=0", [], |r| r.get(0))
            .unwrap();
        assert_eq!(expired, 2, "the two clips past 30 days");
        let before = total_changes(store);
        store.expire(now).unwrap();
        store.expire(now + 30).unwrap();
        assert_eq!(total_changes(store) - before, 0, "idle passes must not write");
        // A newly settled record is still picked up.
        let id = store.begin(now - 31 * DAY).unwrap();
        store.append_audio(id, &[0.2; 16], 16_000).unwrap();
        store.finish(id, RecoveryStatus::Completed, None).unwrap();
        store.expire(now).unwrap();
        assert!(!audio_filename(&store.path, id).unwrap().exists());
    }
    #[test]
    fn turning_retention_off_clears_expiry_once() {
        let mut fixture = Fixture::new("expiry-off", policy());
        let store = &mut fixture.store;
        let id = store.begin(unix_now() - DAY).unwrap();
        store.finish(id, RecoveryStatus::Completed, None).unwrap();
        store.expire(unix_now()).unwrap();
        store.policy.window = None;
        store.expire(unix_now()).unwrap();
        let before = total_changes(store);
        store.expire(unix_now()).unwrap();
        assert_eq!(total_changes(store) - before, 0);
    }
    #[test]
    fn short_text_retention_and_never_preserve_unexpired_audio() {
        let mut fixture = Fixture::new("text-expiry", policy());
        let store = &mut fixture.store;
        let id = store.begin(unix_now() - 2 * DAY).unwrap();
        store.append_audio(id, &[0.1; 16], 16_000).unwrap();
        store
            .stage(id, StageKind::Raw, "Text should expire", "")
            .unwrap();
        store.finish(id, RecoveryStatus::Completed, None).unwrap();
        super::super::prune_at(&store.path, Some(DAY)).unwrap();
        let detail = detail_at(&store.path, id).unwrap().unwrap();
        assert!(detail.stages.is_empty());
        assert!(detail.audio.is_some());
        assert!(!detail.entry.text.contains("Text should expire"));
        super::super::prune_at(&store.path, Some(0)).unwrap();
        assert!(read_audio_at(&store.path, id).unwrap().is_some());
    }
    #[test]
    fn manual_delete_and_clear_remove_audio_and_stages() {
        let mut fixture = Fixture::new("manual-delete", policy());
        let store = &mut fixture.store;
        let first = store.begin(unix_now()).unwrap();
        let second = store.begin(unix_now()).unwrap();
        for id in [first, second] {
            store.append_audio(id, &[0.1; 4], 16_000).unwrap();
            store.stage(id, StageKind::Raw, "temporary", "").unwrap();
        }
        assert_eq!(super::super::delete_at(&store.path, first).unwrap(), 1);
        assert!(!audio_filename(&store.path, first).unwrap().exists());
        assert!(detail_at(&store.path, first).unwrap().is_none());
        assert_eq!(super::super::clear_at(&store.path).unwrap(), 1);
        assert!(!audio_filename(&store.path, second).unwrap().exists());
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM dictation_stages", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    #[test]
    fn restart_repairs_stale_header_marks_interrupted_and_removes_orphans() {
        let fixture = Fixture::new("restart", policy());
        let store = &fixture.store;
        let id = store.begin(unix_now()).unwrap();
        store.append_audio(id, &[0.1; 4], 16_000).unwrap();
        let audio = audio_filename(&store.path, id).unwrap();
        let mut file = OpenOptions::new().append(true).open(&audio).unwrap();
        file.write_all(&0.5f32.to_le_bytes()).unwrap();
        file.sync_all().unwrap();
        drop(file);
        let orphan = audio_filename(&store.path, 99999).unwrap();
        std::fs::write(&orphan, wav_header(16_000, 0)).unwrap();
        store.recover_interrupted().unwrap();
        assert!(!orphan.exists());
        assert_eq!(
            detail_at(&store.path, id).unwrap().unwrap().status,
            RecoveryStatus::Interrupted
        );
        assert_eq!(
            read_audio_at(&store.path, id).unwrap().unwrap().0,
            vec![0.1, 0.1, 0.1, 0.1, 0.5]
        );
        let bytes = std::fs::read(audio).unwrap();
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 20);
    }
    #[test]
    fn explicit_cap_removes_old_inactive_audio_only_and_preserves_text() {
        let mut limited = policy();
        limited.cap = 60;
        let mut fixture = Fixture::new("cap", limited);
        let store = &mut fixture.store;
        let first = store.begin(unix_now() - 1).unwrap();
        store.append_audio(first, &[0.1; 4], 16_000).unwrap();
        store
            .stage(first, StageKind::Raw, "Keep old words", "")
            .unwrap();
        store
            .finish(first, RecoveryStatus::Completed, None)
            .unwrap();
        let second = store.begin(unix_now()).unwrap();
        store.append_audio(second, &[0.2; 4], 16_000).unwrap();
        let old = detail_at(&store.path, first).unwrap().unwrap();
        assert!(old.audio.is_none());
        assert_eq!(old.stages[0].text, "Keep old words");
        assert!(old.error.unwrap().contains("storage limit"));
        assert!(audio_filename(&store.path, second).unwrap().exists());
        let third = store.begin(unix_now()).unwrap();
        assert!(store.append_audio(third, &[0.3; 4], 16_000).is_err());
        assert!(
            audio_filename(&store.path, second).unwrap().exists(),
            "active recording is not evicted"
        );
    }
    #[test]
    fn symlink_audio_never_reads_an_unrelated_file() {
        let fixture = Fixture::new("symlink", policy());
        let store = &fixture.store;
        let id = store.begin(unix_now()).unwrap();
        let foreign = fixture.directory.join("unrelated.wav");
        std::fs::write(&foreign, b"private unrelated bytes").unwrap();
        symlink(&foreign, audio_filename(&store.path, id).unwrap()).unwrap();
        assert!(read_audio_at(&store.path, id).unwrap().is_none());
        super::super::delete_at(&store.path, id).unwrap();
        assert_eq!(std::fs::read(foreign).unwrap(), b"private unrelated bytes");
    }
    #[test]
    fn writer_flush_persists_queued_tail_and_stages_before_shutdown() {
        let fixture = Fixture::new("writer-flush", policy());
        let path = fixture.store.path.clone();
        let (sender, receiver) = mpsc::sync_channel(64);
        let worker = std::thread::spawn(move || run_writer(path, receiver, policy()));
        sender.send(Command::Begin(77)).unwrap();
        sender
            .send(Command::Audio(77, vec![0.2; 1920], 16_000))
            .unwrap();
        sender
            .send(Command::Stage(
                77,
                StageKind::Raw,
                "Original text".into(),
                "".into(),
            ))
            .unwrap();
        sender
            .send(Command::Stage(
                77,
                StageKind::Cleaned,
                "Cleaned text.".into(),
                "".into(),
            ))
            .unwrap();
        sender
            .send(Command::Finish(77, RecoveryStatus::Cancelled, None))
            .unwrap();
        assert!(flush_sender(&sender, Duration::from_secs(5)));
        let entries = super::super::try_recent_at(&fixture.store.path, 10).unwrap();
        assert_eq!(entries.len(), 1);
        let record = detail_at(&fixture.store.path, entries[0].id)
            .unwrap()
            .unwrap();
        assert_eq!(record.status, RecoveryStatus::Cancelled);
        assert_eq!(record.stages.len(), 2);
        assert_eq!(
            read_audio_at(&fixture.store.path, entries[0].id)
                .unwrap()
                .unwrap()
                .0
                .len(),
            1920
        );
        drop(sender);
        worker.join().unwrap();
    }

    #[test]
    #[ignore = "manual storage performance lab; prints timings, never private content"]
    fn recovery_writer_performance_lab() {
        let fixture = Fixture::new("performance-lab", policy());
        let path = fixture.store.path.clone();
        let (sender, receiver) = mpsc::sync_channel(64);
        let worker = std::thread::spawn(move || run_writer(path, receiver, policy()));
        sender.send(Command::Begin(91)).unwrap();
        assert!(flush_sender(&sender, Duration::from_secs(5)));
        let started = std::time::Instant::now();
        let mut enqueue_time = Duration::ZERO;
        let mut maximum_enqueue = Duration::ZERO;
        let mut backpressure = 0usize;
        // Sixty seconds at16kHz, delivered in the production120ms chunk size.
        // A burst measures sustained storage capacity without a minute of sleep.
        for _ in 0..500 {
            let mut command = Command::Audio(91, vec![0.125; 1920], 16_000);
            loop {
                let attempt = std::time::Instant::now();
                match sender.try_send(command) {
                    Ok(()) => {
                        let elapsed = attempt.elapsed();
                        enqueue_time += elapsed;
                        maximum_enqueue = maximum_enqueue.max(elapsed);
                        break;
                    }
                    Err(mpsc::TrySendError::Full(rejected)) => {
                        command = rejected;
                        backpressure += 1;
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(mpsc::TrySendError::Disconnected(_)) => {
                        panic!("fixture recovery writer exited")
                    }
                }
            }
        }
        sender
            .send(Command::Stage(
                91,
                StageKind::Raw,
                "Synthetic benchmark transcript".into(),
                "".into(),
            ))
            .unwrap();
        sender
            .send(Command::Stage(
                91,
                StageKind::Cleaned,
                "Synthetic benchmark transcript.".into(),
                "".into(),
            ))
            .unwrap();
        sender
            .send(Command::Finish(91, RecoveryStatus::Completed, None))
            .unwrap();
        assert!(flush_sender(&sender, Duration::from_secs(20)));
        let elapsed = started.elapsed();
        let entries = super::super::try_recent_at(&fixture.store.path, 10).unwrap();
        let record = detail_at(&fixture.store.path, entries[0].id)
            .unwrap()
            .unwrap();
        assert_eq!(record.audio.unwrap().byte_count, 3_840_044);
        assert_eq!(record.stages.len(), 2);
        assert!(record.error.is_none());
        eprintln!("[recovery-lab] chunks=500 audio_seconds=60 bytes=3840044 enqueue_mean_us={:.3} enqueue_max_us={:.3} total_ms={:.1} chunks_per_second={:.1} burst_backpressure_retries={}",
            enqueue_time.as_secs_f64()*1e6/500.0,maximum_enqueue.as_secs_f64()*1e6,elapsed.as_secs_f64()*1000.0,500.0/elapsed.as_secs_f64(),backpressure);
        drop(sender);
        worker.join().unwrap();
    }
}
