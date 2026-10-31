//! The recovery writer thread: its command queue, the storage notices, and
//! the begin, append, finish and flush calls the daemon makes.
use super::*;

// The queue is bounded to 64 chunks (about eight seconds at a 120ms cadence).
// A full/failed writer reports visibly, and marks the record incomplete; it
// never blocks ASR or pretends that missing audio was preserved.
#[derive(Debug)]
pub(super) enum Command {
    Begin(u64),
    Resume(u64, i64),
    Audio(u64, Vec<f32>, u32),
    Stage(u64, StageKind, String, String),
    Finish(u64, RecoveryStatus, Option<String>),
    Flush(mpsc::SyncSender<()>),
}

impl Command {
    pub(super) fn generation(&self) -> u64 {
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

pub(super) static WRITER: OnceLock<Option<mpsc::SyncSender<Command>>> = OnceLock::new();

#[derive(Clone)]
pub(super) struct StorageNotice {
    pub(super) message: String,
    pub(super) is_failure: bool,
}

/// The CURRENT notice. A failure here is what the recording pill shows, so it
/// is scoped to the capture it happened in: the next record that opens
/// successfully clears it (`clear_failure_notice`). It used to be set-only, so
/// one full queue replaced the live preview with "Recovery warning" until the
/// app restarted.
pub(super) static NOTICE: Mutex<Option<StorageNotice>> = Mutex::new(None);

/// The most recent failure, kept for Settings → History after the pill's copy
/// has been cleared.
pub(super) static LAST_FAILURE: Mutex<Option<String>> = Mutex::new(None);

pub(super) static LOST: OnceLock<Mutex<HashMap<u64, bool>>> = OnceLock::new();

pub(super) fn set_notice(message: &str) {
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

pub(super) fn set_info_notice(message: &str) {
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
pub(super) fn clear_failure_notice() {
    if let Ok(mut notice) = NOTICE.lock() {
        *notice = without_failure(notice.take());
    }
}

/// Drop a failure notice, keep an informational one.
pub(super) fn without_failure(notice: Option<StorageNotice>) -> Option<StorageNotice> {
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

pub(super) fn writer() -> Option<&'static mpsc::SyncSender<Command>> {
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

pub(super) fn process_command(
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
            super::super::prune_at(&store.path, store.policy.text_window)?;
        }
    }
    Ok(())
}

pub(super) fn run_writer(path: PathBuf, receiver: mpsc::Receiver<Command>, policy: Policy) {
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

pub(super) fn enqueue(command: Command) -> bool {
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

pub(super) fn flush_sender(writer: &mpsc::SyncSender<Command>, timeout: Duration) -> bool {
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
