use super::*;

#[test]
fn instruction_model_precedence_and_explicit_disable() {
    assert_eq!(resolve_instruction_model_from(None, None, Some("default.gguf")).as_deref(), Some("default.gguf"));
    assert_eq!(resolve_instruction_model_from(None, Some("saved.gguf"), Some("default.gguf")).as_deref(), Some("saved.gguf"));
    assert_eq!(resolve_instruction_model_from(Some(" env.gguf "), Some("saved.gguf"), None).as_deref(), Some("env.gguf"));
    assert_eq!(resolve_instruction_model_from(Some(""), Some("saved.gguf"), Some("default.gguf")), None);
    assert_eq!(resolve_instruction_model_from(None, Some("  "), Some("default.gguf")), None);
    assert_eq!(resolve_instruction_model_from(None, None, None), None);
}

#[test]
fn instruction_model_survives_settings_round_trip() {
    let settings: Settings = serde_json::from_str(r#"{"instruction_model":"local.gguf","llm_model":"normalize.gguf"}"#).unwrap();
    assert_eq!(settings.instruction_model.as_deref(), Some("local.gguf"));
    assert_eq!(serde_json::from_str::<Settings>(&serde_json::to_string(&settings).unwrap()).unwrap(), settings);
    assert_eq!(serde_json::from_str::<Settings>("{}").unwrap().instruction_model, None);
}

#[test]
fn language_menu_matches_the_checkpoint_supported_set() {
    let mut codes: Vec<_> = COHERE_LANGUAGE_CHOICES.iter().map(|(code, _)| *code).collect();
    codes.sort_unstable();
    assert_eq!(codes, vec!["ar", "de", "el", "en", "es", "fr", "it", "ja", "ko", "nl", "pl", "pt", "vi", "zh"]);
}

#[test]
fn param_billions_parsing() {
    assert_eq!(parse_param_billions("gemma-3-1b-it"), Some(1.0));
    assert_eq!(parse_param_billions("gemma-3-4b-it"), Some(4.0));
    assert_eq!(parse_param_billions("qwen-2.5-0.5b-it"), Some(0.5));
    assert_eq!(parse_param_billions("smollm2-360m-it"), Some(0.36));
    assert_eq!(parse_param_billions("llama-3.2-1b-it"), Some(1.0));
    // No size token → None (the "3" in "gemma-3" must not be misread).
    assert_eq!(parse_param_billions("some-model-it"), None);
}

#[test]
fn model_hints_describe_capability_without_claiming_untested_accuracy() {
    assert_eq!(
        model_hint("smollm2-360m-it").as_deref(),
        Some("0.36B parameters")
    );
    assert_eq!(
        model_hint("gemma-3-1b-it").as_deref(),
        Some("1B parameters")
    );
    assert_eq!(
        model_hint("gemma-3-4b-it").as_deref(),
        Some("4B parameters")
    );
    assert_eq!(model_hint("mystery-model").as_deref(), None);
    assert_eq!(model_hint("s1-mini").as_deref(), Some("English normalization"));
    assert_eq!(model_hint("tiny-aya-global").as_deref(), Some("NonCommercial licence · personal use only"));
}

#[test]
fn parse_keycode_decimal_and_hex() {
    assert_eq!(parse_keycode("61"), Some(61));
    assert_eq!(parse_keycode("0x3d"), Some(0x3d));
    assert_eq!(parse_keycode("0X3D"), Some(0x3d));
    assert_eq!(parse_keycode(" 0x36 "), Some(0x36));
    assert_eq!(parse_keycode("nonsense"), None);
}

#[test]
fn parse_u64_decimal_and_hex() {
    assert_eq!(parse_u64("0"), Some(0));
    assert_eq!(parse_u64("1048576"), Some(1 << 20));
    assert_eq!(parse_u64("0x00100000"), Some(1 << 20));
    assert_eq!(parse_u64("0X20000"), Some(1 << 17));
    assert_eq!(parse_u64("junk"), None);
}

#[test]
fn hotkey_modifiers_default_zero_and_from_settings() {
    if std::env::var_os("DICTATE_HOTKEY_MODIFIERS").is_none() {
        assert_eq!(Settings::default().resolve_hotkey_modifiers(), 0);
        let mut s = Settings::default();
        s.hotkey_modifiers = Some(1 << 20); // Command
        assert_eq!(s.resolve_hotkey_modifiers(), 1 << 20);
    }
}

#[test]
fn translation_targets_parse_trim_and_drop_blanks() {
    if std::env::var_os("DICTATE_TRANSLATION_TARGETS").is_none() {
        assert!(Settings::default().resolve_translation_targets().is_empty());
        let mut s = Settings::default();
        s.translation_targets = Some(vec![
            " Spanish ".into(),
            "".into(),
            "Chinese".into(),
        ]);
        assert_eq!(s.resolve_translation_targets(), vec!["Spanish", "Chinese"]);
    }
}

#[test]
fn active_translation_target_falls_back_to_first() {
    if std::env::var_os("DICTATE_ACTIVE_TRANSLATION_TARGET").is_none() {
        let targets = vec!["Spanish".to_string(), "Chinese".to_string()];
        assert_eq!(
            Settings::default().resolve_active_translation_target(&targets),
            Some("Spanish".to_string())
        );
        let mut s = Settings::default();
        s.active_translation_target = Some("chinese".into());
        assert_eq!(
            s.resolve_active_translation_target(&targets),
            Some("Chinese".to_string())
        );
        // A configured target not in the list falls back to the first.
        let mut s2 = Settings::default();
        s2.active_translation_target = Some("Klingon".into());
        assert_eq!(
            s2.resolve_active_translation_target(&targets),
            Some("Spanish".to_string())
        );
        // No targets at all ⇒ None.
        assert_eq!(
            Settings::default().resolve_active_translation_target(&[]),
            None
        );
    }
}

#[test]
fn history_retention_defaults_forever_and_normalizes() {
    if std::env::var_os("DICTATE_HISTORY_RETENTION").is_none() {
        assert_eq!(Settings::default().resolve_history_retention(), "forever");
        let mut s = Settings::default();
        s.history_retention = Some(" 1Month ".into());
        assert_eq!(s.resolve_history_retention(), "1month");
    }
}

#[test]
fn whisper_mode_off_by_default() {
    if std::env::var_os("DICTATE_WHISPER_MODE").is_none() {
        assert!(!Settings::default().resolve_whisper_mode());
        let mut s = Settings::default();
        s.whisper_mode = Some(true);
        assert!(s.resolve_whisper_mode());
    }
}

#[test]
fn copy_on_paste_failure_is_on_by_default() {
    if std::env::var_os("DICTATE_COPY_ON_PASTE_FAILURE").is_none() {
        assert!(Settings::default().resolve_copy_on_paste_failure());
        let mut s = Settings::default();
        s.copy_on_paste_failure = Some(false);
        assert!(!s.resolve_copy_on_paste_failure());
    }
}

#[test]
fn copy_last_hotkey_distinguishes_unset_from_off() {
    if std::env::var_os("DICTATE_COPY_LAST_HOTKEY").is_none() {
        assert_eq!(Settings::default().resolve_copy_last_hotkey(), None);
        let mut s = Settings::default();
        s.copy_last_hotkey = Some("".into());
        assert_eq!(s.resolve_copy_last_hotkey().as_deref(), Some(""));
        s.copy_last_hotkey = Some(" ctrl+cmd+v ".into());
        assert_eq!(s.resolve_copy_last_hotkey().as_deref(), Some("ctrl+cmd+v"));
    }
}

#[test]
fn max_recording_defaults_to_ten_minutes_and_zero_means_no_limit() {
    if std::env::var_os("DICTATE_MAX_RECORDING_MINUTES").is_none() {
        assert_eq!(Settings::default().resolve_max_recording_minutes(), 10);
        let mut s = Settings::default();
        s.max_recording_minutes = Some(0);
        assert_eq!(s.resolve_max_recording_minutes(), 0);
        assert!(MAX_RECORDING_CHOICES.iter().any(|(m, _)| *m == DEFAULT_MAX_RECORDING_MINUTES));
    }
}

#[test]
fn an_old_settings_file_without_the_new_keys_still_loads() {
    let s = Settings::parse_lenient(r#"{"hotkey_keycode": 61, "show_result": true}"#, "test");
    assert_eq!(s.hotkey_keycode, Some(61));
    assert_eq!(s.copy_last_hotkey, None);
    assert_eq!(s.copy_on_paste_failure, None);
    assert_eq!(s.max_recording_minutes, None);
    assert_eq!(s.completion_sound, None);
    assert_eq!(s.start_sound, None);
    // An old file with no completion_sound key still resolves the default.
    assert_eq!(
        s.resolve_completion_sound(),
        crate::cues::DEFAULT_COMPLETION_SOUND
    );
    // The same for the start sound key.
    assert_eq!(
        s.resolve_start_sound(),
        crate::cues::DEFAULT_START_SOUND
    );
    // A bad value in a new key costs only that key.
    let s = Settings::parse_lenient(r#"{"max_recording_minutes": "ten", "show_result": true}"#, "test");
    assert_eq!(s.max_recording_minutes, None);
    assert_eq!(s.show_result, Some(true));
}

#[test]
fn completion_sound_defaults_and_unknown_falls_back() {
    if std::env::var_os(COMPLETION_SOUND_ENV).is_none() {
        // Unset ⇒ the currently installed tight thud, not an audition cue.
        assert_eq!(
            Settings::default().resolve_completion_sound(),
            crate::cues::DEFAULT_COMPLETION_SOUND
        );
        // A saved audition cue is honored.
        let mut s = Settings::default();
        s.completion_sound = Some("glass-tick".into());
        assert_eq!(s.resolve_completion_sound(), "glass-tick");
        // Unknown, blank and whitespace all fall back rather than go silent.
        s.completion_sound = Some("not-a-real-cue".into());
        assert_eq!(
            s.resolve_completion_sound(),
            crate::cues::DEFAULT_COMPLETION_SOUND
        );
        s.completion_sound = Some("   ".into());
        assert_eq!(
            s.resolve_completion_sound(),
            crate::cues::DEFAULT_COMPLETION_SOUND
        );
        // A padded but valid id is trimmed to the canonical spelling.
        s.completion_sound = Some("  glass-tick  ".into());
        assert_eq!(s.resolve_completion_sound(), "glass-tick");
    }
}

#[test]
fn start_sound_defaults_and_unknown_falls_back() {
    if std::env::var_os(START_SOUND_ENV).is_none() {
        // Unset ⇒ the currently installed rising pair, not an audition cue.
        assert_eq!(
            Settings::default().resolve_start_sound(),
            crate::cues::DEFAULT_START_SOUND
        );
        // A saved audition cue is honored.
        let mut s = Settings::default();
        s.start_sound = Some("glass-rise".into());
        assert_eq!(s.resolve_start_sound(), "glass-rise");
        // Unknown, blank and whitespace all fall back rather than go silent.
        s.start_sound = Some("not-a-real-cue".into());
        assert_eq!(
            s.resolve_start_sound(),
            crate::cues::DEFAULT_START_SOUND
        );
        s.start_sound = Some("   ".into());
        assert_eq!(
            s.resolve_start_sound(),
            crate::cues::DEFAULT_START_SOUND
        );
        // A completion id is not a start id: it falls back too.
        s.start_sound = Some("glass-tick".into());
        assert_eq!(
            s.resolve_start_sound(),
            crate::cues::DEFAULT_START_SOUND
        );
        // A padded but valid id is trimmed to the canonical spelling.
        s.start_sound = Some("  glass-rise  ".into());
        assert_eq!(s.resolve_start_sound(), "glass-rise");
    }
}

#[test]
fn completion_sound_round_trips_through_the_settings_file() {
    let dir = std::env::temp_dir().join(format!("fd-settings-sound-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("settings.json");

    let mut s = Settings::default();
    s.completion_sound = Some("glass-tick".into());
    s.start_sound = Some("glass-rise".into());
    s.save_to(&path).unwrap();

    let loaded = Settings::load_from(&path);
    assert_eq!(loaded.completion_sound.as_deref(), Some("glass-tick"));
    assert_eq!(loaded.start_sound.as_deref(), Some("glass-rise"));
    if std::env::var_os(COMPLETION_SOUND_ENV).is_none() {
        assert_eq!(loaded.resolve_completion_sound(), "glass-tick");
    }
    if std::env::var_os(START_SOUND_ENV).is_none() {
        assert_eq!(loaded.resolve_start_sound(), "glass-rise");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn double_tap_latch_on_by_default() {
    if std::env::var_os("DICTATE_DOUBLE_TAP_LATCH").is_none() {
        assert!(Settings::default().resolve_double_tap_latch());
        let mut s = Settings::default();
        s.double_tap_latch = Some(false);
        assert!(!s.resolve_double_tap_latch());
    }
}

#[test]
fn cleanup_flag_overrides_settings() {
    let mut s = Settings::default();
    s.cleanup_enabled = Some(true);
    assert!(!s.resolve_cleanup_enabled(true)); // CLI --no-cleanup wins
    assert!(s.resolve_cleanup_enabled(false));
}

#[test]
fn cleanup_defaults_enabled_when_unset() {
    let s = Settings::default();
    assert!(s.resolve_cleanup_enabled(false));
}

#[test]
fn hotkey_falls_back_to_default_when_unset() {
    // No env, no settings ⇒ default. (Env is process-global; we avoid
    // setting it here so the test stays hermetic.)
    if std::env::var_os("DICTATE_HOTKEY_KEYCODE").is_none() {
        let s = Settings::default();
        assert_eq!(s.resolve_hotkey(0x3d), 0x3d);
        let mut s2 = Settings::default();
        s2.hotkey_keycode = Some(0x36);
        assert_eq!(s2.resolve_hotkey(0x3d), 0x36);
    }
}

#[test]
fn llm_model_falls_back_to_default_when_unset() {
    if llm_model_env().is_none() {
        let s = Settings::default();
        assert_eq!(s.resolve_llm_model("default.gguf"), "default.gguf");
        let mut s2 = Settings::default();
        s2.llm_model = Some("chosen.gguf".to_string());
        assert_eq!(s2.resolve_llm_model("default.gguf"), "chosen.gguf");
    }
}

#[test]
fn resolve_format_from_settings_when_no_env() {
    // Hermetic: only assert the settings/blank behaviour when the env knob
    // isn't set (env is process-global).
    if std::env::var_os("DICTATE_FORMAT").is_none() {
        let s = Settings::default();
        assert_eq!(s.resolve_format(), None);

        let mut s2 = Settings::default();
        s2.active_format = Some("email".into());
        assert_eq!(s2.resolve_format(), Some("email".into()));

        let mut s3 = Settings::default();
        s3.active_format = Some("   ".into()); // blank ⇒ unset
        assert_eq!(s3.resolve_format(), None);
    }
}

#[test]
fn preroll_resolves_off_by_default_and_from_settings() {
    if std::env::var_os("DICTATE_PREROLL_MS").is_none() {
        assert_eq!(Settings::default().resolve_preroll_ms(), 0);
        let mut s = Settings::default();
        s.preroll_ms = Some(400);
        assert_eq!(s.resolve_preroll_ms(), 400);
    }
}

#[test]
fn listen_mode_off_by_default() {
    if std::env::var_os("DICTATE_LISTEN_MODE").is_none() {
        assert!(!Settings::default().resolve_listen_mode());
        let mut s = Settings::default();
        s.listen_mode = Some(true);
        assert!(s.resolve_listen_mode());
    }
}

#[test]
fn wake_word_falls_back_to_default_when_blank() {
    if std::env::var_os("DICTATE_WAKE_WORD").is_none() {
        assert_eq!(Settings::default().resolve_wake_word(), DEFAULT_WAKE_WORD);
        let mut s = Settings::default();
        s.wake_word = Some("  ".into());
        assert_eq!(s.resolve_wake_word(), DEFAULT_WAKE_WORD);
        let mut s2 = Settings::default();
        s2.wake_word = Some("jarvis".into());
        assert_eq!(s2.resolve_wake_word(), "jarvis");
    }
}

#[test]
fn one_bad_value_does_not_reset_the_other_settings() {
    // Regression: any single type error failed the whole document and
    // every setting silently reverted to its default.
    let dir = std::env::temp_dir().join(format!("fd-settings-lenient-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("settings.json");
    std::fs::write(
        &path,
        r#"{
                "hotkey_keycode": 54,
                "preroll_ms": "400",
                "warm_window_ms": -1,
                "tail_capture_cap_ms": 400.5,
                "cleanup_enabled": "yes",
                "appearance": "light",
                "cohere_language": "de",
                "translation_targets": ["Spanish", 7],
                "history_retention": "1week",
                "some_future_key": {"nested": true}
            }"#,
    )
    .unwrap();
    let s = Settings::load_from(&path);
    assert_eq!(s.hotkey_keycode, Some(54));
    assert_eq!(s.appearance.as_deref(), Some("light"));
    assert_eq!(s.cohere_language.as_deref(), Some("de"));
    assert_eq!(s.history_retention.as_deref(), Some("1week"));
    // The bad ones fall back to "unset" (their built-in default).
    assert_eq!(s.preroll_ms, None);
    assert_eq!(s.warm_window_ms, None);
    assert_eq!(s.tail_capture_cap_ms, None);
    assert_eq!(s.cleanup_enabled, None);
    assert_eq!(s.translation_targets, None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_file_with_one_bad_value_is_backed_up_before_it_is_rewritten() {
    // The bad value loads as "unset", so the next save drops it. The user
    // typed it; keep the original beside the rewritten file.
    let dir = std::env::temp_dir().join(format!("fd-settings-badkey-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("settings.json");
    let original = r#"{ "hotkey_keycode": 54, "preroll_ms": "400" }"#;
    std::fs::write(&path, original).unwrap();
    let s = Settings::load_from(&path);
    assert_eq!(s.hotkey_keycode, Some(54));
    s.save_to(&path).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("settings.json.bak")).unwrap(), original);
    assert_eq!(Settings::load_from(&path).hotkey_keycode, Some(54));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn lenient_parse_still_migrates_the_legacy_model_key() {
    let s = {
        let mut s = Settings::parse_lenient(
            r#"{ "gemma_model": "/tmp/legacy.gguf", "preroll_ms": "bad" }"#,
            "test",
        );
        s.migrate_legacy_keys();
        s
    };
    assert_eq!(s.llm_model.as_deref(), Some("/tmp/legacy.gguf"));
    assert_eq!(s.preroll_ms, None);
}

#[test]
fn unparseable_settings_are_backed_up_before_the_first_save() {
    let dir = std::env::temp_dir().join(format!("fd-settings-bak-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("settings.json");
    let broken = "{ \"hotkey_keycode\": 54,, }";
    std::fs::write(&path, broken).unwrap();
    // Unparseable → defaults, as before.
    let mut s = Settings::load_from(&path);
    assert_eq!(s, Settings::default());
    // The menu's load → mutate → save must not destroy the user's file.
    s.cleanup_enabled = Some(false);
    s.save_to(&path).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("settings.json.bak")).unwrap(), broken);
    assert_eq!(Settings::load_from(&path).cleanup_enabled, Some(false));
    // A healthy file is not backed up again.
    s.save_to(&path).unwrap();
    let baks = std::fs::read_dir(&dir)
        .unwrap()
        .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().ends_with(".bak"))
        .count();
    assert_eq!(baks, 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn discover_returns_empty_for_missing_dir() {
    let v = discover_llm_models_in(Path::new("/nonexistent/models/llm"));
    assert!(v.is_empty());
}

#[test]
fn discover_finds_gguf_and_sorts() {
    let tmp = std::env::temp_dir().join(format!("fd-models-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    for name in ["zeta-model", "alpha-model"] {
        let d = tmp.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("weights.gguf"), b"x").unwrap();
    }
    // A dir with no gguf should be skipped.
    std::fs::create_dir_all(tmp.join("empty-model")).unwrap();

    let v = discover_llm_models_in(&tmp);
    assert_eq!(v.len(), 2);
    assert_eq!(v[0].label, "alpha-model"); // sorted
    assert_eq!(v[1].label, "zeta-model");
    assert!(v[0].path.ends_with("weights.gguf"));
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn save_and_load_round_trip() {
    let dir =
        std::env::temp_dir().join(format!("fd-settings-roundtrip-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("settings.json");

    let mut s = Settings::default();
    s.hotkey_keycode = Some(0x36);
    s.cleanup_enabled = Some(false);
    s.active_format = Some("bullets".into());
    s.preroll_ms = Some(400);
    s.cohere_language = Some("de".into());
    s.completion_sound = Some("glass-tick".into());
    s.save_to(&path).unwrap();

    let loaded = Settings::load_from(&path);
    assert_eq!(loaded.hotkey_keycode, Some(0x36));
    assert_eq!(loaded.cleanup_enabled, Some(false));
    assert_eq!(loaded.active_format, Some("bullets".into()));
    assert_eq!(loaded.preroll_ms, Some(400));
    assert_eq!(loaded.cohere_language, Some("de".into()));
    assert_eq!(loaded.completion_sound.as_deref(), Some("glass-tick"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn legacy_settings_keys_still_deserialize() {
    // An old settings file carries the removed multi-backend keys. serde
    // must drop them silently (the struct has no deny_unknown_fields)
    // while keeping the surviving live field(s).
    let dir = std::env::temp_dir().join(format!("fd-settings-legacy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("settings.json");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        &path,
        r#"{
                "gemma_model": "/tmp/legacy.gguf",
                "cohere_language": "ja",
                "asr_backend": "cohere",
                "cohere_quant": "int4",
                "moonshine_variant": "tiny"
            }"#,
    )
    .unwrap();

    let mut expected = Settings::default();
    // `gemma_model` is folded into `llm_model` on load.
    expected.llm_model = Some("/tmp/legacy.gguf".into());
    expected.cohere_language = Some("ja".into());
    // Legacy-only keys are dropped (not preserved as noise); live fields
    // survive. Equality against `expected` proves this.
    let loaded = Settings::load_from(&path);
    assert_eq!(loaded, expected);
    assert!(loaded.gemma_model.is_none());
    // A save writes the new key only.
    loaded.save_to(&path).unwrap();
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("\"llm_model\""), "{body}");
    assert!(!body.contains("gemma_model"), "{body}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn llm_model_key_wins_over_legacy_gemma_model() {
    let dir = std::env::temp_dir().join(format!("fd-settings-llmwins-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("settings.json");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        &path,
        r#"{ "llm_model": "/tmp/new.gguf", "gemma_model": "/tmp/old.gguf" }"#,
    )
    .unwrap();
    let loaded = Settings::load_from(&path);
    assert_eq!(loaded.llm_model.as_deref(), Some("/tmp/new.gguf"));
    assert!(loaded.gemma_model.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_missing_file_returns_defaults() {
    let path = std::env::temp_dir().join(format!("fd-settings-missing-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let s = Settings::load_from(&path);
    assert_eq!(s, Settings::default());
}

#[test]
fn load_corrupt_file_returns_defaults() {
    let dir = std::env::temp_dir().join(format!("fd-settings-corrupt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("settings.json");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(&path, "{ this is not valid json").unwrap();
    let s = Settings::load_from(&path);
    assert_eq!(s, Settings::default());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resolve_streaming_asr_defaults_on_and_settings_can_disable() {
    if std::env::var_os("DICTATE_STREAMING_ASR").is_none() {
        assert!(Settings::default().resolve_streaming_asr());
        let mut s = Settings::default();
        s.streaming_asr = Some(false);
        assert!(!s.resolve_streaming_asr());
    }
}

#[test]
fn resolve_streaming_asr_disabled_by_preroll() {
    // Pre-roll and streaming are mutually exclusive for now (both drain the
    // capture ring differently); pre-roll wins. Only assert when no env knob
    // pins the outcome (env is process-global).
    if std::env::var_os("DICTATE_STREAMING_ASR").is_none()
        && std::env::var_os("DICTATE_PREROLL_MS").is_none()
    {
        let mut s = Settings::default();
        s.preroll_ms = Some(400);
        assert!(!s.resolve_streaming_asr());
    }
}

#[test]
fn live_preview_ui_preserves_saved_choice_while_preroll_blocks_it() {
    let blocked = LivePreviewControl::resolve(Some(true), None, 400);
    assert!(!blocked.enabled && !blocked.editable);
    assert!(blocked.hint.contains("Always-on mic"));
    let restored = LivePreviewControl::resolve(Some(true), None, 0);
    assert!(restored.enabled && restored.editable);
    assert!(!LivePreviewControl::resolve(Some(false), None, 0).enabled);
    let overridden = LivePreviewControl::resolve(Some(false), Some("true"), 0);
    assert!(overridden.enabled && !overridden.editable);
    assert!(overridden.hint.contains("DICTATE_STREAMING_ASR"));
}

#[test]
fn live_preview_control_matches_runtime_without_clearing_preferences() {
    for preference in [None, Some(false), Some(true)] {
        for preroll in [0, 400] {
            let mut settings = Settings::default();
            settings.streaming_asr = preference;
            settings.preroll_ms = Some(preroll);
            assert_eq!(
                settings.live_preview_control().enabled,
                settings.resolve_streaming_asr()
            );
            assert_eq!(settings.streaming_asr, preference);
            assert_eq!(settings.preroll_ms, Some(preroll));
        }
    }
}

#[test]
fn feedback_defaults_and_environment_precedence_are_explicit() {
    assert_eq!(PillSize::resolve(None, None), PillSize::Compact);
    assert_eq!(
        PillSize::resolve(Some("standard"), None),
        PillSize::Standard
    );
    assert_eq!(
        PillSize::resolve(Some("standard"), Some("compact")),
        PillSize::Compact
    );
    assert_eq!(PillSize::resolve(Some("unknown"), None), PillSize::Compact);
    assert_eq!(
        WaveformSensitivity::resolve(None, None),
        WaveformSensitivity::Balanced
    );
    assert_eq!(
        WaveformSensitivity::resolve(Some("high"), None),
        WaveformSensitivity::High
    );
    assert_eq!(
        WaveformSensitivity::resolve(Some("high"), Some("low")),
        WaveformSensitivity::Low
    );
    assert_eq!(
        WaveformSensitivity::resolve(Some("unknown"), None),
        WaveformSensitivity::Balanced
    );
    assert!(resolve_enabled_flag(None, None, true));
    assert!(!resolve_enabled_flag(Some(true), Some("off"), true));
    assert!(resolve_enabled_flag(Some(false), Some("1"), true));
}

#[test]
fn a_bad_flag_value_falls_through_like_a_bad_number() {
    // DICTATE_TAIL_CAPTURE=garbage used to mean on and
    // DICTATE_WHISPER_MODE=garbage used to mean off. Now neither is a value.
    for garbage in ["garbage", "", "  ", "2", "yes please"] {
        assert!(!resolve_enabled_flag(Some(false), Some(garbage), true), "{garbage:?}");
        assert!(resolve_enabled_flag(None, Some(garbage), true), "{garbage:?}");
        assert!(!resolve_enabled_flag(None, Some(garbage), false), "{garbage:?}");
    }
    for (value, expected) in [(" ON ", true), ("No", false), ("TRUE", true), ("0", false)] {
        assert_eq!(resolve_enabled_flag(Some(!expected), Some(value), !expected), expected, "{value:?}");
    }
}

#[test]
fn recording_feedback_choices_survive_save_and_reload() {
    let dir = std::env::temp_dir().join(format!("fd-feedback-settings-{}", std::process::id()));
    let path = dir.join("settings.json");
    let mut saved = Settings::default();
    saved.streaming_asr = Some(false);
    saved.pill_size = Some("standard".into());
    saved.waveform_sensitivity = Some("high".into());
    saved.show_processing_detail = Some(false);
    saved.audio_history_enabled = Some(false);
    saved.audio_retention = Some("1week".into());
    saved.audio_storage_cap_mb = Some(4096);
    saved.show_result = Some(true);
    saved.cleanup_enabled = Some(false);
    saved.preroll_ms = Some(400);
    saved.save_to(&path).unwrap();
    assert_eq!(Settings::load_from(&path), saved);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn audio_recovery_defaults_to_thirty_days_without_shortening_text_history() {
    assert_eq!(resolve_audio_retention_value(None, None), "1month");
    assert_eq!(
        resolve_audio_retention_value(Some("forever"), Some("1 week")),
        "1week"
    );
    assert_eq!(
        resolve_audio_retention_value(Some("unknown"), None),
        "forever"
    );
    assert_eq!(resolve_audio_cap(None, None), 0);
    assert_eq!(resolve_audio_cap(Some(4096), Some("2048")), 2048);
    assert_eq!(resolve_audio_cap(Some(4096), Some("0")), 0);
    let old: Settings = serde_json::from_str("{}").unwrap();
    assert_eq!(old.audio_history_enabled, None);
    assert_eq!(old.history_retention, None);
}

#[test]
fn tail_capture_defaults_on_and_can_be_disabled() {
    if std::env::var_os("DICTATE_TAIL_CAPTURE").is_none() {
        assert!(Settings::default().resolve_tail_capture());
        let mut s = Settings::default();
        s.tail_capture = Some(false);
        assert!(!s.resolve_tail_capture());
    }
}

#[test]
fn tail_capture_disabled_by_preroll() {
    // Always-on pre-roll keeps rolling past release, so release grace is off.
    if std::env::var_os("DICTATE_TAIL_CAPTURE").is_none()
        && std::env::var_os("DICTATE_PREROLL_MS").is_none()
    {
        let mut s = Settings::default();
        s.preroll_ms = Some(400);
        assert!(!s.resolve_tail_capture());
    }
}

#[test]
fn tail_capture_cap_defaults_and_parses() {
    if std::env::var_os("DICTATE_TAIL_CAPTURE_CAP_MS").is_none() {
        assert_eq!(Settings::default().resolve_tail_capture_cap_ms(), 1500);
        let mut s = Settings::default();
        s.tail_capture_cap_ms = Some(800);
        assert_eq!(s.resolve_tail_capture_cap_ms(), 800);
    }
}

#[test]
fn warm_window_defaults_and_can_be_zeroed() {
    if std::env::var_os("DICTATE_WARM_WINDOW_MS").is_none() {
        assert_eq!(Settings::default().resolve_warm_window_ms(), 60_000);
        let mut s = Settings::default();
        s.warm_window_ms = Some(0);
        assert_eq!(s.resolve_warm_window_ms(), 0);
        let mut s2 = Settings::default();
        s2.warm_window_ms = Some(2500);
        assert_eq!(s2.resolve_warm_window_ms(), 2500);
    }
}

#[test]
fn resolve_cohere_language_defaults_and_from_settings() {
    if std::env::var_os("DICTATE_COHERE_LANG").is_none() {
        let s = Settings::default();
        assert_eq!(s.resolve_asr().language, "en");

        let mut s2 = Settings::default();
        s2.cohere_language = Some("de".into());
        assert_eq!(s2.resolve_asr().language, "de");
    }
}
