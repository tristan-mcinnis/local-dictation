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
