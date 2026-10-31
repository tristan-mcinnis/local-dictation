use super::*;
fn record() -> RecoveryRecord {
    RecoveryRecord {
        entry: Entry {
            id: 42,
            text: "Saved text".to_string(),
            created_at: 0,
        },
        status: RecoveryStatus::Cancelled,
        audio: None,
        stages: vec![],
        error: None,
    }
}

#[test]
fn recovery_action_is_by_record_id_and_never_the_inserting_retry() {
    let mut record = record();
    assert_eq!(
        recovery_command(&record, false).unwrap(),
        crate::ipc::Command::Recover(42)
    );
    assert!(recovery_command(&record, true).is_err());
    record.status = RecoveryStatus::Recovering;
    assert!(recovery_command(&record, false).is_err());
    record.status = RecoveryStatus::Failed;
    assert_eq!(
        recovery_command(&record, false).unwrap(),
        crate::ipc::Command::Recover(42)
    );
}

#[test]
fn repeated_raw_and_cleaned_layers_have_distinct_menu_titles() {
    let stage = |id, ordinal| crate::history::TextStage {
        id,
        ordinal,
        kind: StageKind::Raw,
        title: "Raw".to_string(),
        text: "Transcript".to_string(),
        metadata: "{}".to_string(),
        created_at: 0,
    };
    assert_eq!(
        layer_menu_titles(&[stage(1, 1), stage(2, 3)]),
        ["1. Raw", "3. Raw"]
    );
}

#[test]
fn stale_and_deleted_audio_have_recoverable_errors() {
    let missing = audio_for_action(Some(9), |_| Ok(None)).unwrap_err();
    assert!(missing.contains("unavailable"));
    assert!(!missing.contains("text layers are still available"));
    assert_eq!(
        audio_for_action(None, |_| panic!("deleted record must not look up a path"))
            .unwrap_err(),
        "This dictation was deleted."
    );
    let failure =
        audio_for_action(Some(9), |_| Err(eyre::eyre!("private storage failure"))).unwrap_err();
    assert_eq!(failure, "Could not read the original audio.");
    let valid = audio_for_action(Some(42), |id| {
        assert_eq!(id, 42);
        Ok(Some(PathBuf::from("/tmp/fixture.wav")))
    })
    .unwrap();
    assert_eq!(valid, PathBuf::from("/tmp/fixture.wav"));
}

#[test]
fn detail_shortcuts_are_explicit_clipboard_and_inspection_actions() {
    let command = 1 << 20;
    assert_eq!(detail_key_action("w", command), Some(DetailAction::Close));
    assert_eq!(detail_key_action("r", command), Some(DetailAction::Recover));
    assert_eq!(detail_key_action("p", command), Some(DetailAction::Play));
    assert_eq!(detail_key_action("1", command), Some(DetailAction::Raw));
    assert_eq!(detail_key_action("2", command), Some(DetailAction::Cleaned));
    assert_eq!(
        detail_key_action("c", command | (1 << 17)),
        Some(DetailAction::Copy)
    );
    assert_eq!(detail_key_action("v", command), None);
    assert_eq!(detail_key_action("r", 0), None);
}
