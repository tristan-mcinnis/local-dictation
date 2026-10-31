//! The flipped document view and `SettingsController`, the action target
//! for every control in the window.
use super::*;

// ─── Flipped container (top-left origin) for top-down row layout ──────────
define_class!(
    #[unsafe(super(NSView))]
    #[name = "FDSettingsFlippedView"]
    pub struct SettingsFlippedView;

    impl SettingsFlippedView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

// ─── Action target for every control in the window ───────────────────────
//
// One NSObject subclass receives every control's action. Each method reads the
// sender and updates `pending` (no restart). `applyAndRestart:` flushes. The
// file/log buttons call straight through to the menubar helpers.
define_class!(
    #[unsafe(super(NSObject))]
    #[name = "FDSettingsController"]
    pub struct SettingsController;

    impl SettingsController {
        #[unsafe(method(selectPane:))]
        fn select_pane(&self, sender: *mut AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            if let Some(ui) = SETTINGS_UI.get() {
                show_pane(ui, tag as usize);
            }
        }

        #[unsafe(method(changeHotkey:))]
        fn change_hotkey(&self, sender: *mut AnyObject) {
            let code = popup_selected_tag(sender);
            // The popup only offers bare modifier keys, so selecting one clears
            // any chord modifiers the custom-hotkey field may have set.
            with_pending(|p| {
                p.hotkey_keycode = Some(code as i64);
                p.hotkey_modifiers = Some(0);
            });
        }

        #[unsafe(method(changeCleanup:))]
        fn change_cleanup(&self, sender: *mut AnyObject) {
            let on = switch_on(sender);
            with_pending(|p| p.cleanup_enabled = Some(on));
        }

        #[unsafe(method(changePreroll:))]
        fn change_preroll(&self, sender: *mut AnyObject) {
            let on = switch_on(sender);
            with_pending(|p| {
                p.preroll_ms = Some(if on { crate::audio::DEFAULT_PREROLL_MS } else { 0 });
            });
            refresh_live_preview_control();
        }

        #[unsafe(method(changeFormat:))]
        fn change_format(&self, sender: *mut AnyObject) {
            let name = popup_selected_rep(sender);
            with_pending(|p| {
                p.active_format = if name.is_empty() { None } else { Some(name) };
            });
        }

        #[unsafe(method(changeAppearance:))]
        fn change_appearance(&self, sender: *mut AnyObject) {
            let v = popup_selected_rep(sender);
            // Appearance is the one setting that also applies immediately —
            // picking a look you cannot see until a restart is no choice at all.
            if let Some(mtm) = MainThreadMarker::new() {
                menubar::apply_app_appearance(mtm, &v);
            }
            with_pending(|p| p.appearance = if v.is_empty() { None } else { Some(v) });
        }

        #[unsafe(method(changeCohereLang:))]
        fn change_cohere_lang(&self, sender: *mut AnyObject) {
            let v = popup_selected_rep(sender);
            with_pending(|p| p.cohere_language = if v.is_empty() { None } else { Some(v) });
        }

        #[unsafe(method(changeModel:))]
        fn change_model(&self, sender: *mut AnyObject) {
            let path = popup_selected_rep(sender);
            with_pending(|p| p.llm_model = if path.is_empty() { None } else { Some(path) });
        }

        #[unsafe(method(changeInstructionModel:))]
        fn change_instruction_model(&self, sender: *mut AnyObject) {
            let value = popup_selected_rep(sender);
            with_pending(|p| p.instruction_model = match value.as_str() {
                "__auto__" => None,
                "__disabled__" => Some(String::new()),
                _ => Some(value),
            });
        }

        #[unsafe(method(changeStreaming:))]
        fn change_streaming(&self, sender: *mut AnyObject) {
            let on = switch_on(sender);
            with_pending(|p| p.streaming_asr = Some(on));
            refresh_live_preview_control();
        }

        #[unsafe(method(changeTailCapture:))]
        fn change_tail_capture(&self, sender: *mut AnyObject) {
            let on = switch_on(sender);
            with_pending(|p| p.tail_capture = Some(on));
        }

        #[unsafe(method(changeProcessingDetail:))]
        fn change_processing_detail(&self, sender: *mut AnyObject) {
            with_pending(|p| p.show_processing_detail = Some(switch_on(sender)));
        }

        #[unsafe(method(changePillSize:))]
        fn change_pill_size(&self, sender: *mut AnyObject) {
            with_pending(|p| p.pill_size = Some(popup_selected_rep(sender)));
        }

        #[unsafe(method(changeWaveformSensitivity:))]
        fn change_waveform_sensitivity(&self, sender: *mut AnyObject) {
            with_pending(|p| p.waveform_sensitivity = Some(popup_selected_rep(sender)));
        }

        #[unsafe(method(changeListen:))]
        fn change_listen(&self, sender: *mut AnyObject) {
            let on = switch_on(sender);
            with_pending(|p| p.listen_mode = Some(on));
        }

        #[unsafe(method(changeWakeWord:))]
        fn change_wake_word(&self, sender: *mut AnyObject) {
            let s = field_string(sender);
            let t = s.trim().to_string();
            with_pending(|p| {
                p.wake_word = if t.is_empty() { None } else { Some(t) };
            });
        }

        #[unsafe(method(addDictionaryEntry:))]
        fn add_dictionary_entry(&self, _sender: *mut AnyObject) {
            let Some(ui) = SETTINGS_UI.get() else {
                return;
            };
            if let Some(why) = &ui.dictionary_load_error {
                ui.dictionary_status.setStringValue(&NSString::from_str(why));
                return;
            }
            let heard = ui.dictionary_heard_field.stringValue().to_string();
            let correct = ui.dictionary_correct_field.stringValue().to_string();
            // A row click pulls an entry into the fields; saving it must drop
            // the original key, or renaming the heard side leaves both. Keep
            // the marker until the write lands, so a refused add can be retried.
            let editing = ui.dictionary_editing.lock().unwrap().clone();
            let Some(path) = crate::corrections::Corrections::config_path() else {
                ui.dictionary_status.setStringValue(&NSString::from_str(
                    "Not saved. The corrections file path could not be resolved.",
                ));
                return;
            };
            // Persist now, not at Apply & Restart: a term typed here must not be
            // lost by closing the window. The daemon still reads corrections
            // once at boot, so it takes effect after Apply & Restart.
            let outcome = {
                let mut corrections = ui.pending_corrections.lock().unwrap();
                corrections.add_and_save_to(editing.as_deref(), &heard, &correct, &path)
            };
            match outcome {
                Ok(crate::corrections::QuickAdd::NoTarget) => {
                    ui.dictionary_status.setStringValue(&NSString::from_str(
                        "Type the spelling you want in ‘Correct to’.",
                    ));
                    ui.window.makeFirstResponder(Some(&*ui.dictionary_correct_field));
                }
                Ok(crate::corrections::QuickAdd::Saved(count)) => {
                    *ui.dictionary_editing.lock().unwrap() = None;
                    ui.dictionary_heard_field.setStringValue(&NSString::from_str(""));
                    ui.dictionary_correct_field.setStringValue(&NSString::from_str(""));
                    ui.dictionary_status.setStringValue(&NSString::from_str(&format!(
                        "Saved. {count} terms will be active after Apply & Restart.",
                    )));
                    refresh_dictionary_list(ui);
                    ui.window.makeFirstResponder(Some(&*ui.dictionary_heard_field));
                }
                Err(e) => {
                    // Nothing was written; the list still shows the old state.
                    ui.dictionary_status.setStringValue(&NSString::from_str(&format!(
                        "Not saved, and nothing was added. {e}",
                    )));
                    ui.window.makeFirstResponder(Some(&*ui.dictionary_correct_field));
                }
            }
        }

        #[unsafe(method(searchDictionary:))]
        fn search_dictionary(&self, _sender: *mut AnyObject) {
            if let Some(ui) = SETTINGS_UI.get() {
                refresh_dictionary_list(ui);
            }
        }

        #[unsafe(method(editDictionaryEntry:))]
        fn edit_dictionary_entry(&self, sender: *mut AnyObject) {
            let Some(ui) = SETTINGS_UI.get() else {
                return;
            };
            let tag: isize = unsafe { msg_send![sender, tag] };
            let Some((key, value)) = dictionary_entry_at(ui, tag) else {
                return;
            };
            // An identity entry ("keep this spelling") has no heard side.
            let heard = if key.eq_ignore_ascii_case(&value) { "" } else { &key };
            ui.dictionary_heard_field
                .setStringValue(&NSString::from_str(heard));
            ui.dictionary_correct_field
                .setStringValue(&NSString::from_str(&value));
            *ui.dictionary_editing.lock().unwrap() = Some(key);
            ui.dictionary_status.setStringValue(&NSString::from_str(&format!(
                "Editing “{value}”. Change it above, then Add.",
            )));
            ui.window
                .makeFirstResponder(Some(&*ui.dictionary_correct_field));
        }

        #[unsafe(method(removeDictionaryEntry:))]
        fn remove_dictionary_entry(&self, sender: *mut AnyObject) {
            let Some(ui) = SETTINGS_UI.get() else {
                return;
            };
            let tag: isize = unsafe { msg_send![sender, tag] };
            let Some((key, value)) = dictionary_entry_at(ui, tag) else {
                return;
            };
            let Some(path) = crate::corrections::Corrections::config_path() else {
                ui.dictionary_status.setStringValue(&NSString::from_str(
                    "Not removed. The corrections file path could not be resolved.",
                ));
                return;
            };
            // Persist now, not at Apply & Restart (see add_dictionary_entry).
            let outcome = {
                let mut corrections = ui.pending_corrections.lock().unwrap();
                corrections.remove_and_save_to(&key, &path)
            };
            match outcome {
                Ok(true) => {
                    // Removing the entry that is open in the fields ends that edit.
                    let mut editing = ui.dictionary_editing.lock().unwrap();
                    if editing.as_deref() == Some(key.as_str()) {
                        *editing = None;
                    }
                    drop(editing);
                    let count = ui.pending_corrections.lock().unwrap().len();
                    ui.dictionary_status.setStringValue(&NSString::from_str(&format!(
                        "Removed “{value}”. {count} terms after Apply & Restart.",
                    )));
                    refresh_dictionary_list(ui);
                }
                // Already gone (for example a double click): nothing to save.
                Ok(false) => refresh_dictionary_list(ui),
                Err(e) => {
                    ui.dictionary_status.setStringValue(&NSString::from_str(&format!(
                        "Not removed, and nothing was changed. {e}",
                    )));
                }
            }
        }

        #[unsafe(method(searchHistory:))]
        fn search_history(&self, _sender: *mut AnyObject) {
            if let Some(ui) = SETTINGS_UI.get() {
                refresh_history_list(ui);
            }
        }

        #[unsafe(method(openHistoryDetail:))]
        fn open_history_detail(&self, sender: *mut AnyObject) {
            let Some(ui) = SETTINGS_UI.get() else { return; };
            let tag: isize = unsafe { msg_send![sender, tag] };
            if let Some((id, _)) = history_entry_at(ui, tag) { menubar::show_history_detail(id); }
        }

        #[unsafe(method(copyHistoryEntry:))]
        fn copy_history_entry(&self, sender: *mut AnyObject) {
            let Some(ui) = SETTINGS_UI.get() else {
                return;
            };
            let tag: isize = unsafe { msg_send![sender, tag] };
            let Some((id, text)) = history_entry_at(ui, tag) else {
                return;
            };
            if text.trim().is_empty() {
                return;
            }
            let message = format!("Copied “{}”.", history_preview(&text));
            set_history_status(ui, "Copying…");
            menubar::copy_to_clipboard(text, move |copied| {
                let Some(ui) = SETTINGS_UI.get() else { return; };
                set_history_status(ui, if copied { &message } else { "Could not copy this dictation." });
                if !copied || history_entry_at(ui, tag).is_none_or(|entry| entry.0 != id) { return; }
                flash_history_row(ui, tag as usize);
                // Start the confirmation window after the actual write, and
                // never highlight another entry if filtering changed meanwhile.
                unsafe {
                    let controller = &*ui._controller;
                    let obj: &AnyObject = &*(controller as *const SettingsController as *const AnyObject);
                    let _: () = msg_send![objc2::class!(NSObject), cancelPreviousPerformRequestsWithTarget: obj,
                        selector: sel!(clearHistoryFlash:), object: std::ptr::null::<AnyObject>()];
                    let _: () = msg_send![controller, performSelector: sel!(clearHistoryFlash:),
                        withObject: std::ptr::null::<AnyObject>(), afterDelay: HISTORY_COPIED_FLASH_S];
                }
            });
        }

        #[unsafe(method(toggleSidebar:))]
        fn toggle_sidebar(&self, _sender: *mut AnyObject) {
            if let Some(ui) = SETTINGS_UI.get() {
                set_sidebar_open(ui, !*ui.sidebar_open.lock().unwrap());
            }
        }

        /// Fired by `performSelector:afterDelay:` to end a copy flash.
        #[unsafe(method(clearHistoryFlash:))]
        fn clear_history_flash_action(&self, _sender: *mut AnyObject) {
            if let Some(ui) = SETTINGS_UI.get() {
                clear_history_flash(ui);
            }
        }

        #[unsafe(method(copyAllHistory:))]
        fn copy_all_history(&self, _sender: *mut AnyObject) {
            let Some(ui) = SETTINGS_UI.get() else {
                return;
            };
            // Everything currently shown, in the order it reads, so a search
            // scopes the copy the way it scopes the list.
            let texts = ui.history_texts.lock().unwrap().clone();
            if texts.is_empty() {
                set_history_status(ui, "Nothing to copy.");
                return;
            }
            let n = texts.len();
            set_history_status(ui, "Copying…");
            menubar::copy_to_clipboard(texts.join("\n\n"), move |copied| {
                if let Some(ui) = SETTINGS_UI.get() {
                    let plural = if n == 1 { "dictation" } else { "dictations" };
                    let message = if copied { format!("Copied {n} {plural}.") } else { "Could not copy the history.".to_string() };
                    set_history_status(ui, &message);
                }
            });
        }

        #[unsafe(method(deleteHistoryEntry:))]
        fn delete_history_entry(&self, sender: *mut AnyObject) {
            let Some(ui) = SETTINGS_UI.get() else {
                return;
            };
            let tag: isize = unsafe { msg_send![sender, tag] };
            let Some((id, text)) = history_entry_at(ui, tag) else {
                return;
            };
            // History is a store, not a pending setting: the delete lands now.
            // It runs off the main thread, since it waits on the write lock.
            set_history_status(ui, "Deleting…");
            std::thread::spawn(move || {
                let outcome = crate::history::delete_outcome(id);
                menubar::run_on_main(move || {
                    let Some(ui) = SETTINGS_UI.get() else { return };
                    refresh_history_list(ui);
                    use crate::history::DeleteOutcome;
                    match outcome {
                        DeleteOutcome::Deleted => {
                            set_history_status(ui, &format!("Deleted “{}”.", history_preview(&text)))
                        }
                        DeleteOutcome::Gone => set_history_status(ui, "That dictation was already gone."),
                        DeleteOutcome::Failed => set_history_status(ui, HISTORY_DELETE_FAILED),
                    }
                });
            });
        }

        #[unsafe(method(clearHistory:))]
        fn clear_history(&self, _sender: *mut AnyObject) {
            let Some(ui) = SETTINGS_UI.get() else {
                return;
            };
            set_history_status(ui, "Deleting…");
            std::thread::spawn(move || {
                let outcome = crate::history::clear_outcome();
                menubar::run_on_main(move || {
                    let Some(ui) = SETTINGS_UI.get() else { return };
                    refresh_history_list(ui);
                    use crate::history::ClearOutcome;
                    match outcome {
                        ClearOutcome::Cleared(0) => set_history_status(ui, "There was nothing to delete."),
                        ClearOutcome::Cleared(n) => {
                            let plural = if n == 1 { "dictation" } else { "dictations" };
                            set_history_status(ui, &format!("Deleted {n} {plural}."));
                        }
                        ClearOutcome::Failed => set_history_status(ui, HISTORY_DELETE_FAILED),
                    }
                });
            });
        }

        #[unsafe(method(applyAndRestart:))]
        fn apply_and_restart(&self, _sender: *mut AnyObject) {
            if let Some(ui) = SETTINGS_UI.get() {
                apply_and_restart(ui);
            }
        }

        #[unsafe(method(editPrompts:))]
        fn edit_prompts(&self, _sender: *mut AnyObject) {
            menubar::open_prompts_file();
        }

        #[unsafe(method(openCorrections:))]
        fn open_corrections(&self, _sender: *mut AnyObject) {
            menubar::open_corrections_folder();
        }

        #[unsafe(method(openCommandsRef:))]
        fn open_commands_ref(&self, _sender: *mut AnyObject) {
            menubar::write_and_open_commands_reference();
        }

        #[unsafe(method(openLog:))]
        fn open_log(&self, _sender: *mut AnyObject) {
            menubar::open_log_in_editor();
        }

        #[unsafe(method(exportLog:))]
        fn export_log(&self, _sender: *mut AnyObject) {
            menubar::export_log_to_downloads();
        }

        #[unsafe(method(changeShowResult:))]
        fn change_show_result(&self, sender: *mut AnyObject) {
            let on = switch_on(sender);
            with_pending(|p| p.show_result = Some(on));
        }

        #[unsafe(method(changeCompletionSound:))]
        fn change_completion_sound(&self, sender: *mut AnyObject) {
            let id = popup_selected_rep(sender);
            // Audition the cue at once. This is a preview only: nothing is
            // written until Apply & Restart, and no other setting is touched.
            if !id.is_empty() {
                crate::cues::preview_completion_sound(&id);
            }
            with_pending(|p| {
                p.completion_sound = if id.is_empty() { None } else { Some(id) };
            });
        }

        #[unsafe(method(changeStartSound:))]
        fn change_start_sound(&self, sender: *mut AnyObject) {
            let id = popup_selected_rep(sender);
            // Audition the cue at once; like the completion picker this is a
            // preview only until Apply & Restart.
            if !id.is_empty() {
                crate::cues::preview_start_sound(&id);
            }
            with_pending(|p| {
                p.start_sound = if id.is_empty() { None } else { Some(id) };
            });
        }

        #[unsafe(method(changeWhisper:))]
        fn change_whisper(&self, sender: *mut AnyObject) {
            let on = switch_on(sender);
            with_pending(|p| p.whisper_mode = Some(on));
        }

        #[unsafe(method(changeRetention:))]
        fn change_retention(&self, sender: *mut AnyObject) {
            let rep = popup_selected_rep(sender);
            with_pending(|p| p.history_retention = if rep.is_empty() { None } else { Some(rep) });
        }

        #[unsafe(method(changeAudioHistory:))]
        fn change_audio_history(&self, sender: *mut AnyObject) {
            with_pending(|p| p.audio_history_enabled = Some(switch_on(sender)));
        }

        #[unsafe(method(changeAudioRetention:))]
        fn change_audio_retention(&self, sender: *mut AnyObject) {
            with_pending(|p| p.audio_retention = Some(popup_selected_rep(sender)));
        }

        #[unsafe(method(changeAudioStorageCap:))]
        fn change_audio_storage_cap(&self, sender: *mut AnyObject) {
            with_pending(|p| p.audio_storage_cap_mb = popup_selected_rep(sender).parse().ok());
        }

        #[unsafe(method(changeTranslateTargets:))]
        fn change_translate_targets(&self, sender: *mut AnyObject) {
            let s = field_string(sender);
            let list: Vec<String> = s
                .split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect();
            with_pending(|p| p.translation_targets = if list.is_empty() { None } else { Some(list) });
        }

        #[unsafe(method(changeActiveTranslateTarget:))]
        fn change_active_translate_target(&self, sender: *mut AnyObject) {
            let rep = popup_selected_rep(sender);
            with_pending(|p| {
                p.active_translation_target = if rep.is_empty() { None } else { Some(rep) };
            });
        }

        #[unsafe(method(changeCopyLastHotkey:))]
        fn change_copy_last_hotkey(&self, sender: *mut AnyObject) {
            let typed = field_string(sender);
            let Some(ui) = SETTINGS_UI.get() else { return };
            let ptt = crate::hotkeys::Hotkey::from_loaded(&ui.pending.lock().unwrap());
            let (hint, value) = crate::hotkeys::copy_last_field(&typed, &ptt);
            if let Some(value) = value {
                with_pending(|p| p.copy_last_hotkey = Some(value));
            }
            ui.copy_last_hint.setStringValue(&NSString::from_str(&hint));
        }

        #[unsafe(method(changeCopyOnPasteFailure:))]
        fn change_copy_on_paste_failure(&self, sender: *mut AnyObject) {
            let on = switch_on(sender);
            with_pending(|p| p.copy_on_paste_failure = Some(on));
        }

        #[unsafe(method(changeMaxRecording:))]
        fn change_max_recording(&self, sender: *mut AnyObject) {
            let minutes = popup_selected_tag(sender).max(0) as u32;
            with_pending(|p| p.max_recording_minutes = Some(minutes));
        }

        #[unsafe(method(changeCustomHotkey:))]
        fn change_custom_hotkey(&self, sender: *mut AnyObject) {
            let s = field_string(sender);
            let (hint, applied) = parse_custom_hotkey(&s);
            with_pending(|p| match applied {
                Some((keycode, modifiers)) => {
                    p.hotkey_keycode = Some(keycode);
                    p.hotkey_modifiers = Some(modifiers);
                }
                None => {
                    p.hotkey_keycode = None;
                    p.hotkey_modifiers = None;
                }
            });
            if let Some(ui) = SETTINGS_UI.get() {
                ui.custom_hotkey_hint
                    .setStringValue(&NSString::from_str(&hint));
            }
        }
    }
);

impl SettingsController {
    pub(super) fn new() -> Retained<Self> {
        unsafe { msg_send![Self::alloc(), init] }
    }
}
