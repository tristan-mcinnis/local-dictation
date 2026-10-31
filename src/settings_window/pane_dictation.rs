//! The Dictation pane, plus the hotkey parsing it shares with the controller.
use super::*;

pub(super) const WAVEFORM_SENSITIVITY_NOTE: &str =
    "Visual response only. Microphone gain and recognition stay the same.";

/// Under the completion-sound popup: the picker auditions on selection.
pub(super) const COMPLETION_SOUND_NOTE: &str = "Plays when the text lands. Pick one to hear it.";

/// Under the start-sound popup: the picker auditions on selection.
pub(super) const START_SOUND_NOTE: &str = "Plays when recording starts. Pick one to hear it.";

// ─── State plumbing ──────────────────────────────────────────────────────

/// Render a keycode as a parseable key name ("d", "f5", "space", …) so a
/// configured chord can be round-tripped back through `Hotkey::parse`. The
/// table lives in `hotkeys.rs`; macOS keycodes are not alphabetical.
pub(super) fn key_name_from_keycode(code: i64) -> String {
    crate::hotkeys::key_name_for_keycode(code)
        .map(str::to_string)
        .unwrap_or_else(|| format!("key 0x{code:x}"))
}

/// Render a `Hotkey` as a parseable shorthand ("cmd+shift+d", "f5", …).
pub(super) fn hotkey_to_parseable(h: &crate::hotkeys::Hotkey) -> String {
    let mut parts: Vec<String> = Vec::new();
    if h.modifiers & crate::hotkeys::COMMAND != 0 {
        parts.push("cmd".into());
    }
    if h.modifiers & crate::hotkeys::SHIFT != 0 {
        parts.push("shift".into());
    }
    if h.modifiers & crate::hotkeys::CONTROL != 0 {
        parts.push("ctrl".into());
    }
    if h.modifiers & crate::hotkeys::OPTION != 0 {
        parts.push("option".into());
    }
    parts.push(key_name_from_keycode(h.keycode));
    parts.join("+")
}

/// Parse a custom-hotkey field string. Returns the hint line to display and,
/// on a valid AND usable hotkey, the `(keycode, modifiers)` to write into
/// `pending`. A plain typing key with no modifier is refused: the event tap
/// would swallow that key in every app.
pub(super) fn parse_custom_hotkey(s: &str) -> (String, Option<(i64, u64)>) {
    match crate::hotkeys::Hotkey::parse(s) {
        Some(hk) => match hk.problem() {
            Some(why) => (format!("{} — {why}", hk.display()), None),
            None => (hk.display(), Some((hk.keycode, hk.modifiers))),
        },
        None => ("unrecognized hotkey".to_string(), None),
    }
}

/// Under the recording-limit popup.
pub(super) const MAX_RECORDING_NOTE: &str =
    "A recording stopped by the limit is kept, never pasted.";

/// The initial (field value, hint) for the copy-last field, from the loaded
/// settings: the chord in use (the built-in default when nothing is saved),
/// blank when it is off, or the saved text with the reason it is not used.
pub(super) fn initial_copy_last_hotkey(loaded: &Settings) -> (String, String) {
    let ptt = crate::hotkeys::Hotkey::from_loaded(loaded);
    let configured = loaded.resolve_copy_last_hotkey();
    match crate::hotkeys::copy_last_hotkey(configured.as_deref(), &ptt) {
        Ok(Some(h)) => {
            let text = crate::hotkeys::to_parseable(&h);
            let hint = crate::hotkeys::copy_last_field(&text, &ptt).0;
            (text, hint)
        }
        Ok(None) => (String::new(), crate::hotkeys::copy_last_field("", &ptt).0),
        Err(why) => (configured.unwrap_or_default(), format!("Not used: {why}.")),
    }
}

/// The initial (field value, hint) for the custom-hotkey field, derived from
/// the loaded hotkey. A bare modifier (the historical default, Right Option)
/// is owned by the popup above; any real key renders as a parseable chord.
pub(super) fn initial_custom_hotkey() -> (String, String) {
    let hk = crate::hotkeys::Hotkey::from_settings();
    if hk.is_modifier_only() {
        (
            String::new(),
            "The key is a bare modifier — set it in the popup above, or type a \
             chord like cmd+shift+d here."
                .to_string(),
        )
    } else {
        let text = hotkey_to_parseable(&hk);
        (text, hk.display())
    }
}

pub(super) fn refresh_live_preview_control() {
    if let Some(ui) = SETTINGS_UI.get() {
        let state = ui.pending.lock().unwrap().live_preview_control();
        ui.streaming_switch.set_on(state.enabled);
        ui.streaming_switch.set_active(state.editable);
        ui.streaming_hint
            .setStringValue(&NSString::from_str(state.hint));
        unsafe {
            let _: () = msg_send![&*ui.streaming_switch, setAccessibilityHelp: &*NSString::from_str(state.hint)];
        }
    }
}

/// The Dictation pane: the hotkeys, the safety net, recording feedback and capture.
pub(super) struct DictationPane {
    pub(super) view: Retained<SettingsFlippedView>,
    pub(super) hotkey_popup: Retained<NSPopUpButton>,
    pub(super) cleanup_switch: Retained<slate::InkToggle>,
    pub(super) preroll_switch: Retained<slate::InkToggle>,
    pub(super) streaming_switch: Retained<slate::InkToggle>,
    pub(super) streaming_hint: Retained<NSTextField>,
    pub(super) processing_detail_switch: Retained<slate::InkToggle>,
    pub(super) pill_size_popup: Retained<NSPopUpButton>,
    pub(super) waveform_sensitivity_popup: Retained<NSPopUpButton>,
    pub(super) completion_sound_popup: Retained<NSPopUpButton>,
    pub(super) start_sound_popup: Retained<NSPopUpButton>,
    pub(super) feedback_card_y: f64,
    pub(super) tail_capture_switch: Retained<slate::InkToggle>,
    pub(super) custom_hotkey_field: Retained<NSTextField>,
    pub(super) custom_hotkey_hint: Retained<NSTextField>,
    pub(super) copy_last_hint: Retained<NSTextField>,
    pub(super) whisper_switch: Retained<slate::InkToggle>,
}

pub(super) fn build_dictation_pane(cx: &PaneCx) -> DictationPane {
    let mtm = cx.mtm;
    let controller = cx.controller;
    let loaded = cx.loaded;
    let dictation = cx.new_pane();
    let mut y = FIRST_CARD_Y;

    let mut card = card_begin(mtm, &dictation, y, "Input");

    let hotkey_locked = std::env::var_os("DICTATE_HOTKEY_KEYCODE").is_some();
    let hotkey_active = loaded.resolve_hotkey(0x3D);
    let hotkey_popup = popup_row(mtm, &mut card, "Push-to-talk key");
    for (label_str, code) in settings::HOTKEY_CHOICES {
        popup_add_item(&hotkey_popup, label_str, Some(*code as isize), None);
    }
    select_popup_tag(&hotkey_popup, hotkey_active);
    finish_popup(&hotkey_popup);
    wire_popup(
        &hotkey_popup,
        controller,
        sel!(changeHotkey:),
        hotkey_locked,
    );
    if hotkey_locked {
        env_hint(mtm, &mut card, "DICTATE_HOTKEY_KEYCODE");
    }

    // Custom hotkey (chord) — an arbitrary modifiers + key combo, e.g.
    // cmd+shift+d. The popup above only offers bare modifier keys; this field
    // is where a chord is declared. A live hint below reports the parsed
    // result, or why the key cannot be used (a plain key with no modifier).
    let (initial_custom_hotkey, initial_hotkey_hint) = initial_custom_hotkey();
    let custom_hotkey_field = text_row(mtm, &mut card, "Custom hotkey", &initial_custom_hotkey);
    custom_hotkey_field.setPlaceholderString(Some(&NSString::from_str("e.g. cmd+shift+d or f5")));
    unsafe {
        custom_hotkey_field.setTarget(Some(controller));
        custom_hotkey_field.setAction(Some(sel!(changeCustomHotkey:)));
        custom_hotkey_field.setEnabled(!hotkey_locked);
    }
    let custom_hotkey_hint = hint_line(mtm, &mut card, &initial_hotkey_hint);

    // The second global hotkey, next to the first: copy the last dictation
    // from anywhere. Empty turns it off. Checked against the dictation hotkey.
    let copy_last_locked = std::env::var_os("DICTATE_COPY_LAST_HOTKEY").is_some();
    let (initial_copy_last, initial_copy_last_hint) = initial_copy_last_hotkey(loaded);
    let copy_last_field = text_row(mtm, &mut card, "Copy last dictation", &initial_copy_last);
    copy_last_field.setPlaceholderString(Some(&NSString::from_str("e.g. ctrl+cmd+v · empty = off")));
    unsafe {
        copy_last_field.setTarget(Some(controller));
        copy_last_field.setAction(Some(sel!(changeCopyLastHotkey:)));
        copy_last_field.setEnabled(!copy_last_locked);
    }
    let copy_last_hint = hint_line(mtm, &mut card, &initial_copy_last_hint);
    if copy_last_locked {
        env_hint(mtm, &mut card, "DICTATE_COPY_LAST_HOTKEY");
    }

    y = card_end(&card);

    // Never lose a dictation: what happens when the text cannot be inserted,
    // and the limit that ends a recording nobody is stopping.
    let mut card = card_begin(mtm, &dictation, y, "Safety net");
    let copy_on_failure_switch = toggle_row(
        mtm,
        &mut card,
        "Copy when a paste fails",
        "Text goes to the clipboard, then press ⌘V",
    );
    copy_on_failure_switch.set_on(loaded.resolve_copy_on_paste_failure());
    wire_switch(
        &copy_on_failure_switch,
        controller,
        sel!(changeCopyOnPasteFailure:),
        std::env::var_os("DICTATE_COPY_ON_PASTE_FAILURE").is_some(),
    );
    if std::env::var_os("DICTATE_COPY_ON_PASTE_FAILURE").is_some() {
        env_hint(mtm, &mut card, "DICTATE_COPY_ON_PASTE_FAILURE");
    }

    let max_recording_popup = popup_row(mtm, &mut card, "Stop recording after");
    let max_recording_active = loaded.resolve_max_recording_minutes();
    for (minutes, title) in settings::MAX_RECORDING_CHOICES {
        popup_add_item(&max_recording_popup, title, Some(*minutes as isize), None);
    }
    if !settings::MAX_RECORDING_CHOICES.iter().any(|(m, _)| *m == max_recording_active) {
        // A hand-edited value stays visible and selected instead of being
        // silently shown as another choice.
        popup_add_item(
            &max_recording_popup,
            &format!("{max_recording_active} minutes"),
            Some(max_recording_active as isize),
            None,
        );
    }
    select_popup_tag(&max_recording_popup, i64::from(max_recording_active));
    finish_popup(&max_recording_popup);
    wire_popup(
        &max_recording_popup,
        controller,
        sel!(changeMaxRecording:),
        std::env::var_os("DICTATE_MAX_RECORDING_MINUTES").is_some(),
    );
    if std::env::var_os("DICTATE_MAX_RECORDING_MINUTES").is_some() {
        env_hint(mtm, &mut card, "DICTATE_MAX_RECORDING_MINUTES");
    }
    hint_line(mtm, &mut card, MAX_RECORDING_NOTE);

    y = card_end(&card);

    let feedback_card_y = y;
    let mut card = card_begin(mtm, &dictation, y, "Recording feedback");
    let preview = loaded.live_preview_control();
    let streaming_switch = toggle_row(
        mtm,
        &mut card,
        "Live transcript preview",
        "Draft while speaking",
    );
    streaming_switch.set_on(preview.enabled);
    wire_switch(
        &streaming_switch,
        controller,
        sel!(changeStreaming:),
        !preview.editable,
    );
    let streaming_hint = hint_line(mtm, &mut card, preview.hint);
    unsafe {
        let _: () =
            msg_send![&*streaming_switch, setAccessibilityHelp: &*NSString::from_str(preview.hint)];
    }

    let processing_detail_switch = toggle_row(
        mtm,
        &mut card,
        "Processing detail",
        "Show each step before paste",
    );
    processing_detail_switch.set_on(loaded.resolve_show_processing_detail());
    wire_switch(
        &processing_detail_switch,
        controller,
        sel!(changeProcessingDetail:),
        std::env::var_os("DICTATE_SHOW_PROCESSING_DETAIL").is_some(),
    );
    if std::env::var_os("DICTATE_SHOW_PROCESSING_DETAIL").is_some() {
        env_hint(mtm, &mut card, "DICTATE_SHOW_PROCESSING_DETAIL");
    }

    let pill_size_popup = popup_row(mtm, &mut card, "Pill size");
    for (title, value) in [("Compact", "compact"), ("Standard", "standard")] {
        popup_add_item(&pill_size_popup, title, None, Some(value));
    }
    select_popup_rep(&pill_size_popup, loaded.resolve_pill_size().as_str());
    finish_popup(&pill_size_popup);
    wire_popup(
        &pill_size_popup,
        controller,
        sel!(changePillSize:),
        std::env::var_os("DICTATE_PILL_SIZE").is_some(),
    );
    if std::env::var_os("DICTATE_PILL_SIZE").is_some() {
        env_hint(mtm, &mut card, "DICTATE_PILL_SIZE");
    }

    let waveform_sensitivity_popup = popup_row(mtm, &mut card, "Waveform sensitivity");
    for (title, value) in [("Low", "low"), ("Balanced", "balanced"), ("High", "high")] {
        popup_add_item(&waveform_sensitivity_popup, title, None, Some(value));
    }
    select_popup_rep(
        &waveform_sensitivity_popup,
        loaded.resolve_waveform_sensitivity().as_str(),
    );
    finish_popup(&waveform_sensitivity_popup);
    wire_popup(
        &waveform_sensitivity_popup,
        controller,
        sel!(changeWaveformSensitivity:),
        std::env::var_os("DICTATE_WAVEFORM_SENSITIVITY").is_some(),
    );
    if std::env::var_os("DICTATE_WAVEFORM_SENSITIVITY").is_some() {
        env_hint(mtm, &mut card, "DICTATE_WAVEFORM_SENSITIVITY");
    }
    hint_line(mtm, &mut card, WAVEFORM_SENSITIVITY_NOTE);
    unsafe {
        let note = NSString::from_str(WAVEFORM_SENSITIVITY_NOTE);
        let _: () = msg_send![&*waveform_sensitivity_popup, setToolTip: &*note];
        let _: () = msg_send![&*waveform_sensitivity_popup, setAccessibilityHelp: &*note];
    }
    let cleanup_switch = toggle_row(
        mtm,
        &mut card,
        "Enable cleanup",
        "Only runs when speech is messy",
    );
    cleanup_switch.set_on(loaded.cleanup_enabled.unwrap_or(true));
    wire_switch(&cleanup_switch, controller, sel!(changeCleanup:), false);

    let whisper_switch = toggle_row(
        mtm,
        &mut card,
        "Whisper mode",
        "Lower gate for quiet speech",
    );
    whisper_switch.set_on(loaded.resolve_whisper_mode());
    wire_switch(
        &whisper_switch,
        controller,
        sel!(changeWhisper:),
        std::env::var_os("DICTATE_WHISPER_MODE").is_some(),
    );
    if std::env::var_os("DICTATE_WHISPER_MODE").is_some() {
        env_hint(mtm, &mut card, "DICTATE_WHISPER_MODE");
    }

    let result_switch = toggle_row(
        mtm,
        &mut card,
        "Show result",
        "\"Landed · N words\" pill for 2 s after each dictation",
    );
    result_switch.set_on(loaded.resolve_show_result());
    wire_switch(
        &result_switch,
        controller,
        sel!(changeShowResult:),
        std::env::var_os("DICTATE_SHOW_RESULT").is_some(),
    );
    if std::env::var_os("DICTATE_SHOW_RESULT").is_some() {
        env_hint(mtm, &mut card, "DICTATE_SHOW_RESULT");
    }

    // The cue that plays when recording starts. Selecting an item auditions it
    // right away; Apply & Restart persists the choice.
    let start_sound_locked = std::env::var_os(settings::START_SOUND_ENV).is_some();
    let start_sound_popup = popup_row(mtm, &mut card, "Start sound");
    for sound in crate::cues::START_SOUNDS {
        popup_add_item(&start_sound_popup, sound.label, None, Some(sound.id));
    }
    select_popup_rep(&start_sound_popup, &loaded.resolve_start_sound());
    finish_popup(&start_sound_popup);
    wire_popup(
        &start_sound_popup,
        controller,
        sel!(changeStartSound:),
        start_sound_locked,
    );
    if start_sound_locked {
        env_hint(mtm, &mut card, settings::START_SOUND_ENV);
    }
    hint_line(mtm, &mut card, START_SOUND_NOTE);

    // The cue that plays once the text lands. Selecting an item auditions it
    // right away; Apply & Restart persists the choice.
    let completion_sound_locked = std::env::var_os(settings::COMPLETION_SOUND_ENV).is_some();
    let completion_sound_popup = popup_row(mtm, &mut card, "Completion sound");
    for sound in crate::cues::COMPLETION_SOUNDS {
        popup_add_item(&completion_sound_popup, sound.label, None, Some(sound.id));
    }
    select_popup_rep(&completion_sound_popup, &loaded.resolve_completion_sound());
    finish_popup(&completion_sound_popup);
    wire_popup(
        &completion_sound_popup,
        controller,
        sel!(changeCompletionSound:),
        completion_sound_locked,
    );
    if completion_sound_locked {
        env_hint(mtm, &mut card, settings::COMPLETION_SOUND_ENV);
    }
    hint_line(mtm, &mut card, COMPLETION_SOUND_NOTE);

    y = card_end(&card);
    let mut card = card_begin(mtm, &dictation, y, "Capture");
    let preroll_switch = toggle_row(
        mtm,
        &mut card,
        "Always-on mic",
        "Pre-roll so the first words aren't clipped",
    );
    preroll_switch.set_on(loaded.resolve_preroll_ms() > 0);
    wire_switch(
        &preroll_switch,
        controller,
        sel!(changePreroll:),
        std::env::var_os("DICTATE_PREROLL_MS").is_some(),
    );
    if std::env::var_os("DICTATE_PREROLL_MS").is_some() {
        env_hint(mtm, &mut card, "DICTATE_PREROLL_MS");
    }

    let tail_capture_switch = toggle_row(
        mtm,
        &mut card,
        "Tail-capture",
        "Keep listening ~250 ms after release",
    );
    tail_capture_switch.set_on(loaded.resolve_tail_capture());
    wire_switch(
        &tail_capture_switch,
        controller,
        sel!(changeTailCapture:),
        std::env::var_os("DICTATE_TAIL_CAPTURE").is_some(),
    );
    let _ = card_end(&card);
    finish_pane(&dictation, &card);
    DictationPane {
        view: dictation,
        hotkey_popup,
        cleanup_switch,
        preroll_switch,
        streaming_switch,
        streaming_hint,
        processing_detail_switch,
        pill_size_popup,
        waveform_sensitivity_popup,
        completion_sound_popup,
        start_sound_popup,
        feedback_card_y,
        tail_capture_switch,
        custom_hotkey_field,
        custom_hotkey_hint,
        copy_last_hint,
        whisper_switch,
    }
}
