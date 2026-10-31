//! The menu bar action target (`MenuActions`), the menu delegate that
//! refreshes items on open, and the app delegate that handles quit.
use super::*;

// ─── Custom NSObject subclass holding all menu actions ──────────────────
//
// NSMenuItem dispatches its action via an objc selector to its target. We
// route every dynamic menu item to one instance of this class.
define_class!(
    #[unsafe(super(NSObject))]
    #[name = "FDMenuActions"]
    pub struct MenuActions;

    impl MenuActions {
        #[unsafe(method(openLog:))]
        fn open_log(&self, _sender: *mut AnyObject) {
            open_log_in_editor();
        }

        #[unsafe(method(exportLog:))]
        fn export_log(&self, _sender: *mut AnyObject) {
            export_log_to_downloads();
        }

        #[unsafe(method(copyLast:))]
        fn copy_last(&self, _sender: *mut AnyObject) {
            // Same code path as the global hotkey and `copy-last` over IPC,
            // with the same pill confirmation. Off the main thread: the
            // pasteboard lock can be held through a paste settle.
            std::thread::spawn(|| {
                crate::recovery::copy_last_dictation("menu");
            });
        }

        #[unsafe(method(openHistory:))]
        fn open_history(&self, _sender: *mut AnyObject) {
            show_history_window();
        }

        // A row in the history window was clicked. Its `tag` indexes into
        // HISTORY_ENTRIES (the texts shown, newest first); copy the full,
        // untruncated text and confirm via the window subtitle.
        #[unsafe(method(copyHistoryEntry:))]
        fn copy_history_entry(&self, sender: *mut AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            copy_history_entry_at(tag);
        }

        #[unsafe(method(openHistoryEntry:))]
        fn open_history_entry(&self, sender: *mut AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            open_history_entry_at(tag);
        }

        #[unsafe(method(control:textView:doCommandBySelector:))]
        fn history_search_command(&self, control: *mut AnyObject, _text: *mut AnyObject,
                                  command: objc2::runtime::Sel) -> bool {
            HISTORY_UI.get().is_some_and(|ui| {
                if control != (&*ui.search as *const NSSearchField).cast_mut().cast() {
                    false
                } else if command == sel!(moveUp:) {
                    move_history_selection(-1);
                    true
                } else if command == sel!(moveDown:) {
                    move_history_selection(1);
                    true
                } else if command == sel!(insertNewline:) {
                    open_selected_history_entry();
                    true
                } else {
                    false
                }
            })
        }

        // The history search field changed. Re-filter the cached entries by
        // case-insensitive substring and rebuild the list. Fires per keystroke
        // (the field is configured to send its string immediately).
        #[unsafe(method(searchHistory:))]
        fn search_history(&self, sender: *mut AnyObject) {
            let field: &NSSearchField = unsafe { &*(sender as *const NSSearchField) };
            let query = field.stringValue().to_string();
            apply_history_filter(&query);
        }

        #[unsafe(method(openCorrections:))]
        fn open_corrections(&self, _sender: *mut AnyObject) {
            open_corrections_folder();
        }

        #[unsafe(method(openDictionary:))]
        fn open_dictionary(&self, _sender: *mut AnyObject) {
            show_dictionary_window(self);
        }

        #[unsafe(method(saveDictionary:))]
        fn save_dictionary(&self, _sender: *mut AnyObject) {
            save_dictionary_from_view();
        }

        #[unsafe(method(addDictionaryEntry:))]
        fn add_dictionary_entry(&self, _sender: *mut AnyObject) {
            add_dictionary_entry_from_fields();
        }

        #[unsafe(method(editPrompts:))]
        fn edit_prompts(&self, _sender: *mut AnyObject) {
            open_prompts_file();
        }

        #[unsafe(method(selectModel:))]
        fn select_model(&self, sender: *mut AnyObject) {
            let path = unsafe {
                let obj: *mut AnyObject = msg_send![sender, representedObject];
                if obj.is_null() {
                    return;
                }
                let s: &NSString = &*(obj as *const NSString);
                s.to_string()
            };
            write_settings_and_relaunch(move |set| set.llm_model = Some(path));
        }

        #[unsafe(method(selectHotkey:))]
        fn select_hotkey(&self, sender: *mut AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            write_settings_and_relaunch(move |set| set.hotkey_keycode = Some(tag as i64));
        }

        #[unsafe(method(toggleCleanup:))]
        fn toggle_cleanup(&self, _sender: *mut AnyObject) {
            let current = Settings::load().cleanup_enabled.unwrap_or(true);
            write_settings_and_relaunch(move |set| set.cleanup_enabled = Some(!current));
        }

        #[unsafe(method(togglePreroll:))]
        fn toggle_preroll(&self, _sender: *mut AnyObject) {
            // On ⇒ keep the mic warm with DEFAULT_PREROLL_MS of lookback so the
            // first words aren't clipped; off ⇒ open-on-press (0 ms).
            let on = Settings::load().preroll_ms.unwrap_or(0) > 0;
            write_settings_and_relaunch(move |set| {
                set.preroll_ms = Some(if on { 0 } else { crate::audio::DEFAULT_PREROLL_MS });
            });
        }

        #[unsafe(method(selectFormat:))]
        fn select_format(&self, sender: *mut AnyObject) {
            // representedObject holds the preset name; the empty string means
            // "Default (no preset)", which clears the setting.
            let name = unsafe {
                let obj: *mut AnyObject = msg_send![sender, representedObject];
                if obj.is_null() {
                    String::new()
                } else {
                    let s: &NSString = &*(obj as *const NSString);
                    s.to_string()
                }
            };
            write_settings_and_relaunch(move |set| {
                set.active_format = if name.is_empty() { None } else { Some(name.clone()) };
            });
        }

        #[unsafe(method(openCommandsRef:))]
        fn open_commands_ref(&self, _sender: *mut AnyObject) {
            write_and_open_commands_reference();
        }

        #[unsafe(method(openSettings:))]
        fn open_settings(&self, _sender: *mut AnyObject) {
            crate::settings_window::show_settings_window();
        }

        /// The expanded pill's compact, keyboard-reachable escape hatch. It
        /// uses the same local control channel as the CLI/Shortcuts path, so
        /// cancelling never reaches into the audio worker from AppKit.
        #[unsafe(method(cancelPill:))]
        fn cancel_pill(&self, _sender: *mut AnyObject) {
            if ui_channel::state().is_cancellable() {
                let _ = crate::ipc::send(crate::ipc::Command::Cancel);
            }
        }

        // The success pill's Undo: send ⌘Z to the app that just received the
        // text. (It was wired to cancelPill: before, which discarded nothing
        // useful after the text had already landed.)
        #[unsafe(method(undoPill:))]
        fn undo_pill(&self, _sender: *mut AnyObject) {
            // A queued paste may still be settling: the visible text can belong
            // to that write, so an early ⌘Z would revert unrelated prior
            // content. The pill's Undo affordance is disabled while a
            // transaction is pending; this is the race guard behind it.
            if crate::clipboard_paste::transaction_pending() {
                eprintln!("[pill] undo deferred: a clipboard transaction is still settling");
                return;
            }
            // ⌘Z is global: only send it while the app that received the text
            // is still frontmost, the same rule the paste itself follows.
            let injected = crate::ui_channel::last_inject_pid();
            let frontmost = crate::injector::frontmost_pid();
            if !crate::ui_channel::undo_target_is_current(injected, frontmost) {
                eprintln!(
                    "[pill] undo skipped: text went to pid {injected:?}, frontmost is {frontmost:?}"
                );
            } else if let Err(e) = crate::clipboard_paste::synthesize_undo() {
                eprintln!("[pill] undo failed: {e}");
            }
            crate::ui_channel::set_state(crate::ui_channel::UiState::Idle);
        }

        // Re-run the pipeline on the last utterance's audio at the current
        // cursor. No-op when nothing has been dictated this session.
        #[unsafe(method(retryLast:))]
        fn retry_last(&self, _sender: *mut AnyObject) {
            if let Err(e) = crate::ipc::send(crate::ipc::Command::Retry) {
                eprintln!("[menu] retry last dictation failed: {e}");
            }
        }

        // Start / stop dictation from the menu — the same control the CLI,
        // Shortcuts and a foot pedal drive. `toggle` is resolved against the
        // live recording state by the control socket, so the menu never has to
        // guess which direction to fire.
        #[unsafe(method(recordToggle:))]
        fn record_toggle(&self, _sender: *mut AnyObject) {
            if let Err(e) = crate::ipc::send(crate::ipc::Command::Toggle) {
                eprintln!("[menu] toggle dictation failed: {e}");
            }
        }

        // Recovery paste: re-insert the last dictation text into the app that
        // was frontmost before this menu opened. This is NOT a re-transcribe —
        // it uses the already-captured text, restoring + verifying the target
        // app (see `crate::recovery`). No-op/logs when there is nothing to
        // paste or the target is unsafe (self / unknown / no editable field).
        #[unsafe(method(pasteLast:))]
        fn paste_last(&self, _sender: *mut AnyObject) {
            // Keep the captured field available when the user clicks while a
            // live utterance still owns the pipeline; reopening the menu can
            // retry once that visible activity settles.
            if !crate::recovery::recovery_may_write(
                crate::ui_channel::state(),
                crate::cancel::CANCEL.active(),
            ) {
                eprintln!("[recovery] dictation is still settling — paste deferred");
                return;
            }
            let (recorded, retained_focus) = match RECOVERY_TARGET.lock() {
                Ok(mut target) => (target.0, target.1.take()),
                Err(_) => (None, None),
            };
            crate::recovery::paste_last_dictation(recorded, retained_focus);
        }

        // Pick the active Translate-mode target. representedObject holds the
        // target name; the translate path reads settings per utterance, so a
        // save (no relaunch) is enough for the change to take effect.
        #[unsafe(method(selectTranslateTarget:))]
        fn select_translate_target(&self, sender: *mut AnyObject) {
            let target = unsafe {
                let obj: *mut AnyObject = msg_send![sender, representedObject];
                if obj.is_null() {
                    return;
                }
                let s: &NSString = &*(obj as *const NSString);
                s.to_string()
            };
            if target.is_empty() {
                return;
            }
            let mut s = Settings::load();
            s.active_translation_target = Some(target);
            if let Err(e) = s.save() {
                eprintln!("[menu] failed to save translate target: {e}");
            }
        }

        // Pick the history-retention window. representedObject holds the
        // canonical string ("forever" / "1year" / …); history reads retention
        // per record, so a save (no relaunch) is enough.
        #[unsafe(method(selectRetention:))]
        fn select_retention(&self, sender: *mut AnyObject) {
            let rep = unsafe {
                let obj: *mut AnyObject = msg_send![sender, representedObject];
                if obj.is_null() {
                    return;
                }
                let s: &NSString = &*(obj as *const NSString);
                s.to_string()
            };
            if rep.is_empty() {
                return;
            }
            let mut s = Settings::load();
            s.history_retention = Some(rep);
            if let Err(e) = s.save() {
                eprintln!("[menu] failed to save history retention: {e}");
            }
        }

        // Import a .csv of vocabulary entries into corrections.json and refresh
        // the dictionary window. Backs the "Import Dictionary CSV…" menu item.
        #[unsafe(method(importCsv:))]
        fn import_csv(&self, _sender: *mut AnyObject) {
            import_dictionary_csv();
        }

        // "Setup & permissions…" — the checklist window.
        #[unsafe(method(openSetup:))]
        fn open_setup(&self, _sender: *mut AnyObject) {
            setup::show_setup_window();
        }

        // A Setup row's action chip. Its `tag` names the requirement.
        #[unsafe(method(setupRowAction:))]
        fn setup_row_action(&self, sender: *mut AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            setup::row_action(tag);
        }
    }
);

impl MenuActions {
    pub(super) fn new() -> Retained<Self> {
        let alloc = Self::alloc();
        unsafe { msg_send![alloc, init] }
    }
}

// ─── Menu delegate: record the pre-menu frontmost app ──────────────────
//
// The recovery paste needs the app that was frontmost BEFORE the user opened
// the menu. This delegate's `menuWillOpen:` fires exactly when the status menu
// is about to show — i.e. right after the click but before any menu row is
// chosen — so it is the earliest reliable place to snapshot the foreign app.
// The value is stored in a process-wide static (the delegate is main-thread
// only and the action dispatch is main-thread too, so a Mutex keeps them in
// sync without plumbing a handle between the two objects).
define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "FDMenuDelegate"]
    pub(super) struct MenuDelegate;

    impl MenuDelegate {}

    unsafe impl NSObjectProtocol for MenuDelegate {}

    unsafe impl NSMenuDelegate for MenuDelegate {
        #[unsafe(method(menuWillOpen:))]
        fn menu_will_open(&self, _menu: &NSMenu) {
            // Snapshot the app that was frontmost when the menu opened; it is
            // the recovery-paste target (the guard refuses Local Dictation
            // itself / an unknown target).
            let pid = crate::injector::frontmost_pid();
            let retained_focus = pid.and_then(|target_pid| {
                match crate::injector::FocusTarget::capture_for_pid(target_pid) {
                    Ok(target) => Some(target),
                    Err(e) => {
                        eprintln!(
                            "[recovery] menu focus snapshot unavailable for pid {target_pid}: {e}"
                        );
                        None
                    }
                }
            });
            if let Ok(mut target) = RECOVERY_TARGET.lock() {
                *target = (pid, retained_focus);
            }
            refresh_recovery_items(_menu);
        }
    }
);

impl MenuDelegate {
    pub(super) fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm);
        unsafe { msg_send![this, init] }
    }
}

// ─── Application delegate: release the models before the process exits ──
//
// Quit runs `-[NSApplication terminate:]`, which calls `exit()` from inside
// `NSApp.run()`. `exit()` runs the C++ static destructors, and ggml frees its
// Metal devices from one of them — asserting (and aborting) if any Metal buffer
// is still alive. The daemon's model buffers live on the worker thread, so the
// process aborted on every quit.
//
// `applicationWillTerminate:` is the last main-thread callback before that
// `exit()`, and Metal is still alive here, so this is where the ordered release
// belongs. `crate::shutdown::release_models` asks the worker to drop its models
// and blocks until it has.
//
// Delete this once ggml's Metal teardown tolerates live buffers at exit.
define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "FDAppDelegate"]
    pub(super) struct AppDelegate;

    impl AppDelegate {}

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        #[unsafe(method(applicationWillTerminate:))]
        fn application_will_terminate(&self, _notification: &NSNotification) {
            // Bounded: a worker wedged mid-utterance must not hang the quit.
            // Timing out only restores the old behaviour (a possible abort in
            // ggml's static teardown), so it is never worse than not waiting.
            if !crate::shutdown::release_models(MODEL_RELEASE_TIMEOUT) {
                eprintln!(
                    "[quit] models not released within {} ms — exiting anyway",
                    MODEL_RELEASE_TIMEOUT.as_millis()
                );
            }
            // The worker's Shutdown arm does not unmute; a quit mid-capture
            // would otherwise leave the Mac muted after we are gone.
            crate::audio_duck::restore();
            // Only now. "Apply and restart" has already spawned the
            // replacement and it is sitting on the single-instance lock: the
            // moment it gets the lock it loads the whole model stack, so
            // handing it over before this line would put two full stacks of
            // Metal buffers in memory at once — the thing overlapping the two
            // processes was supposed to avoid. Releasing here instead of
            // waiting for `exit()` still saves the replacement the rest of
            // HANDOFF_WAIT.
            crate::single_instance::release_held();
        }
    }
);

impl AppDelegate {
    pub(super) fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm);
        unsafe { msg_send![this, init] }
    }
}

/// How long quit waits for the worker to drop its models. Generous enough to
/// cover a cleanup generation in flight, short enough that quit still feels
/// immediate if the worker is stuck.
pub(super) const MODEL_RELEASE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
