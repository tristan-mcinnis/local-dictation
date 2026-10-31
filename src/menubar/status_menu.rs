//! The status item and its menu: every submenu and the labels that change
//! with state.
use super::*;

pub(super) struct StatusItemBuild {
    pub(super) status_item: Retained<NSStatusItem>,
    pub(super) icon_idle: Option<Retained<NSImage>>,
    pub(super) icon_recording: Option<Retained<NSImage>>,
    pub(super) icon_processing: Option<Retained<NSImage>>,
    pub(super) last_item: Retained<NSMenuItem>,
    pub(super) menu_delegate: Retained<MenuDelegate>,
}

/// Build a plain menu item with a title, selector, and key-equivalent,
/// targeting `actions`. Always enabled (we disable auto-enabling on the
/// menu so we control state explicitly).
/// Generate the live voice-commands + settings reference and open it in the
/// default text editor. Generated from the actual command tables + settings so
/// it never drifts. Backs the menu's "Voice Commands & Shortcuts…" item.
pub(crate) fn write_and_open_commands_reference() {
    let s = Settings::load();
    let hotkey = settings::hotkey_name(s.hotkey_keycode.unwrap_or(crate::hotkeys::DEFAULT_KEYCODE));

    let mut body = String::new();
    body.push_str("Local Dictation — Voice Commands & Settings\n");
    body.push_str("===========================================\n\n");
    body.push_str(&crate::voice_commands::reference_text());
    body.push_str(&format!(
        "\n\nPUSH-TO-TALK KEY: {hotkey} (hold, speak, release)\n"
    ));
    body.push_str(
        "OUTPUT FORMATS (menu → Output format): default · numbered · bullets · email · code\n",
    );

    let Some(path) = crate::app_paths::config_file("voice-commands.txt") else {
        eprintln!("[menu] no config dir for commands reference");
        return;
    };
    if let Err(e) = std::fs::write(&path, body) {
        eprintln!("[menu] failed to write commands reference: {e}");
        return;
    }
    let _ = std::process::Command::new("open").arg(&path).status();
}

pub(super) fn action_item(
    mtm: MainThreadMarker,
    title: &str,
    selector: objc2::runtime::Sel,
    key: &str,
    actions: &Retained<MenuActions>,
) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(selector),
            &NSString::from_str(key),
        )
    };
    unsafe {
        item.setTarget(Some(actions));
        item.setEnabled(true);
    }
    item
}

pub(super) fn build_status_item(
    mtm: MainThreadMarker,
    actions: &Retained<MenuActions>,
) -> eyre::Result<StatusItemBuild> {
    let bar = NSStatusBar::systemStatusBar();
    let item = bar.statusItemWithLength(-1.0);
    let button = item
        .button(mtm)
        .ok_or_else(|| eyre::eyre!("status item has no button"))?;

    // One glyph family — the waveform, matching the app icon, so the icon and
    // the status item read as one app. Idle and processing are the template
    // image (it adopts the menu-bar tint); recording is the same shape drawn
    // once in `danger`, the only chroma the rule allows on chrome.
    let icon_idle = sf_symbol("waveform");
    let icon_recording =
        sf_symbol_tinted("waveform", &theme::danger()).or_else(|| sf_symbol("waveform"));
    let icon_processing = sf_symbol("waveform");
    if let Some(img) = &icon_idle {
        button.setImage(Some(img));
        button.setTitle(&NSString::from_str(""));
    } else {
        button.setTitle(&NSString::from_str("◯"));
    }

    let menu = NSMenu::new(mtm);
    // We manage enabled-state ourselves (lets us grey the preview label and
    // any env-locked items).
    menu.setAutoenablesItems(false);

    // Menu delegate: records the app that was frontmost just before this menu
    // opened — the recovery-paste target. Retained by the caller so it is not
    // freed (NSMenu keeps its delegate weakly).
    let menu_delegate = MenuDelegate::new(mtm);
    let delegate_proto = ProtocolObject::from_retained(menu_delegate.clone());
    menu.setDelegate(Some(&*delegate_proto));

    let settings = Settings::load();
    // Output-format presets: the built-in set (numbered / bullets / email /
    // code) plus any the user added/overrode in prompts.json `formats`. Uses
    // the non-logging loader so building the menu doesn't re-emit the boot
    // summary.
    let format_names = crate::prompts::Prompts::load_quiet().format_names();

    // ── Recovery: disabled preview + copy + explicit paste ──────────────
    let last_item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(&last_dictation_label()),
            None,
            &NSString::from_str(""),
        )
    };
    last_item.setEnabled(false);
    menu.addItem(&last_item);

    // The fault row: hidden while everything works, first in the menu when
    // something is wrong, and a route straight to the window that fixes it.
    // `refresh_recovery_items` decides each time the menu opens.
    let health_item = action_item(mtm, "", sel!(openSetup:), "", actions);
    health_item.setTag(TAG_HEALTH);
    health_item.setHidden(true);
    menu.insertItem_atIndex(&health_item, 0);

    // Hidden unless Secure Input is on; `refresh_recovery_items` decides each
    // time the menu opens.
    let secure_item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(crate::secure_input::PAUSED_TEXT),
            None,
            &NSString::from_str(""),
        )
    };
    secure_item.setEnabled(false);
    secure_item.setTag(TAG_SECURE_INPUT);
    secure_item.setHidden(true);
    menu.insertItem_atIndex(&secure_item, 0);

    // Record / Stop + Cancel:
    // The two actions most used next. Start/stop drives the same control the
    // CLI and Shortcuts use; Cancel is enabled only while an utterance can
    // still be cancelled.
    let record_item = action_item(mtm, &record_toggle_title(), sel!(recordToggle:), "", actions);
    record_item.setTag(TAG_RECORD);
    menu.addItem(&record_item);

    let cancel_item = action_item(mtm, "Cancel dictation", sel!(cancelPill:), "", actions);
    cancel_item.setTag(TAG_CANCEL);
    menu.addItem(&cancel_item);

    menu.addItem(&NSMenuItem::separatorItem(mtm));

    // Last text: copy / repaste, then retry the audio. "Copy last text" and
    // "Paste last text" reuse the already-captured transcript; "Retry last
    // audio" deliberately re-transcribes the retained recording.
    let copy_last = action_item(
        mtm,
        &copy_last_title(crate::hotkeys::copy_last_from_settings().as_ref()),
        sel!(copyLast:),
        "c",
        actions,
    );
    copy_last.setTag(TAG_COPY_LAST);
    menu.addItem(&copy_last);

    // Explicit recovery: paste the already-captured last dictation into the
    // app that was frontmost before this menu opened. This is NOT a
    // re-transcribe — it does not touch audio or the pipeline, only re-inserts
    // the text (restoring + verifying the target; see `crate::recovery`). The
    // injector's focus gate still applies — never a blind paste.
    let paste_last = action_item(mtm, "Paste last text", sel!(pasteLast:), "", actions);
    paste_last.setTag(TAG_PASTE_LAST);
    menu.addItem(&paste_last);

    // Re-run the last utterance's audio (including a cancelled one) at the
    // current cursor. Distinct from Paste last text: this re-transcribes.
    let retry_last = action_item(
        mtm,
        "Retry last audio (re-transcribe)",
        sel!(retryLast:),
        "r",
        actions,
    );
    retry_last.setTag(TAG_RETRY);
    menu.addItem(&retry_last);
    refresh_recovery_items(&menu);

    menu.addItem(&NSMenuItem::separatorItem(mtm));

    // ── Settings + More + Quit ─────────────────────────────────────────
    // The top level stays minimal: the recoverable last dictation (preview,
    // copy, paste), Settings, a single "More" that gathers everything else, and
    // Quit. Model choices are intentionally absent (this personal build is one
    // warmed Cohere MLX transcriber followed by S1-mini).
    let history_item = action_item(mtm, "Dictation History…", sel!(openHistory:), "h", actions);
    history_item.setTag(TAG_HISTORY);
    menu.addItem(&history_item);

    let settings_item = action_item(mtm, "Settings…", sel!(openSettings:), ",", actions);
    menu.addItem(&settings_item);

    // Always reachable, not only when something is broken: it is also where
    // the permission status and the model state are stated.
    menu.addItem(&action_item(
        mtm,
        "Setup & permissions…",
        sel!(openSetup:),
        "",
        actions,
    ));

    let more_parent = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("More"),
            None,
            &NSString::from_str(""),
        )
    };
    more_parent.setEnabled(true);
    let more_submenu = build_more_submenu(mtm, actions, &settings, &format_names);
    more_parent.setSubmenu(Some(&more_submenu));
    menu.addItem(&more_parent);

    menu.addItem(&NSMenuItem::separatorItem(mtm));

    // Latency (read-only): the app's own release-to-write timing, refreshed as
    // the menu opens. Never a claim about another app's visible pixels.
    let latency_item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(&latency_label()),
            None,
            &NSString::from_str(""),
        )
    };
    latency_item.setEnabled(false);
    latency_item.setTag(TAG_LATENCY);
    menu.addItem(&latency_item);

    // ── Quit (standard NSApp terminate: — works with nil target) ─────────
    let quit_item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Quit local-dictation"),
            Some(sel!(terminate:)),
            &NSString::from_str("q"),
        )
    };
    quit_item.setEnabled(true);
    menu.addItem(&quit_item);

    item.setMenu(Some(&menu));
    Ok(StatusItemBuild {
        status_item: item,
        icon_idle,
        icon_recording,
        icon_processing,
        last_item,
        menu_delegate,
    })
}

/// Build the "More" submenu: config and diagnostics.
/// History and retranscribe now live on the root menu (most-used first); this
/// keeps every other setting reachable. Pure NSMenu grouping with separators —
/// no new visual system.
pub(super) fn build_more_submenu(
    mtm: MainThreadMarker,
    actions: &Retained<MenuActions>,
    settings: &Settings,
    format_names: &[String],
) -> Retained<NSMenu> {
    let submenu = NSMenu::new(mtm);
    submenu.setAutoenablesItems(false);

    // ── History ─────────────────────────────────────────────────────────
    // The browsable history list moved to the root menu (most-used first);
    // only the retention window stays here with the other config.

    // How long to keep dictation history (read per record, so no relaunch).
    let retention_parent = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("History retention"),
            None,
            &NSString::from_str(""),
        )
    };
    retention_parent.setEnabled(true);
    let retention_submenu = build_retention_submenu(mtm, actions, settings);
    retention_parent.setSubmenu(Some(&retention_submenu));
    submenu.addItem(&retention_parent);

    submenu.addItem(&NSMenuItem::separatorItem(mtm));

    // ── Language / format ────────────────────────────────────────────────
    // (Retry last audio moved to the root menu, most-used first.)

    // Pick the active Translate-mode target (only meaningful when targets are
    // configured; reads settings per utterance, so no relaunch).
    let translate_parent = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Translate to"),
            None,
            &NSString::from_str(""),
        )
    };
    translate_parent.setEnabled(true);
    let translate_submenu = build_translate_submenu(mtm, actions, settings);
    translate_parent.setSubmenu(Some(&translate_submenu));
    submenu.addItem(&translate_parent);

    // Output-format preset picker. `format_names` is non-empty out of the box.
    if !format_names.is_empty() {
        let format_parent = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("Output format"),
                None,
                &NSString::from_str(""),
            )
        };
        format_parent.setEnabled(true);
        let format_submenu = build_format_submenu(mtm, actions, settings, format_names);
        format_parent.setSubmenu(Some(&format_submenu));
        submenu.addItem(&format_parent);
    }

    submenu.addItem(&NSMenuItem::separatorItem(mtm));

    // ── Config ───────────────────────────────────────────────────────────
    let hotkey_parent = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Push-to-talk key"),
            None,
            &NSString::from_str(""),
        )
    };
    hotkey_parent.setEnabled(true);
    let hotkey_submenu = build_hotkey_submenu(mtm, actions, settings);
    hotkey_parent.setSubmenu(Some(&hotkey_submenu));
    submenu.addItem(&hotkey_parent);

    let cleanup_item = action_item(mtm, "Cleanup enabled", sel!(toggleCleanup:), "", actions);
    let cleanup_on = settings.cleanup_enabled.unwrap_or(true);
    cleanup_item.setState(if cleanup_on {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    submenu.addItem(&cleanup_item);

    // Keeps the mic stream warm and retains a short lookback so the first
    // words aren't clipped and there's no stream-open latency on key-press.
    let preroll_item = action_item(
        mtm,
        "Always-on mic (pre-roll)",
        sel!(togglePreroll:),
        "",
        actions,
    );
    let preroll_on = settings.preroll_ms.unwrap_or(0) > 0;
    preroll_item.setState(if preroll_on {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    submenu.addItem(&preroll_item);

    // Opens prompts.json in a text editor (seeded with the live prompts the
    // first time, so there's real text to edit).
    submenu.addItem(&action_item(mtm, "Edit cleanup prompts…", sel!(editPrompts:), "", actions));

    // Simple editor for the personal dictionary (names/terms to keep spelled
    // verbatim).
    submenu.addItem(&action_item(mtm, "Dictionary…", sel!(openDictionary:), "d", actions));

    // Import a .csv of vocabulary entries into the personal dictionary.
    submenu.addItem(&action_item(mtm, "Import Dictionary CSV…", sel!(importCsv:), "", actions));

    submenu.addItem(&NSMenuItem::separatorItem(mtm));

    // ── Diagnostics ──────────────────────────────────────────────────────
    submenu.addItem(&action_item(mtm, "Open Log", sel!(openLog:), "l", actions));
    submenu.addItem(&action_item(mtm, "Export Log to Downloads…", sel!(exportLog:), "", actions));
    submenu.addItem(&action_item(mtm, "Open corrections folder", sel!(openCorrections:), "", actions));
    // A clear, always-current reference of every voice command + the wake-word
    // and shortcut settings (generated from the live tables, opened as text).
    submenu.addItem(&action_item(mtm, "Voice Commands & Shortcuts…", sel!(openCommandsRef:), "", actions));

    submenu
}

/// Build the output-format picker. A "Default (no preset)" row clears the
/// active format; each defined preset gets a checkable row. The active one is
/// checked. `DICTATE_FORMAT` env locks the picker (disabled + note), mirroring
/// the model / hotkey submenus.
pub(super) fn build_format_submenu(
    mtm: MainThreadMarker,
    actions: &Retained<MenuActions>,
    settings: &Settings,
    names: &[String],
) -> Retained<NSMenu> {
    let submenu = NSMenu::new(mtm);
    submenu.setAutoenablesItems(false);

    let env_locked = std::env::var_os("DICTATE_FORMAT").is_some();
    let active = settings.active_format.clone().unwrap_or_default();

    // "Default (no preset)" — empty representedObject clears the setting.
    let default_item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str("Default (no preset)"),
            Some(sel!(selectFormat:)),
            &NSString::from_str(""),
        )
    };
    unsafe {
        default_item.setTarget(Some(actions));
        default_item.setEnabled(!env_locked);
        let rep_ns = NSString::from_str("");
        let rep: &AnyObject = &rep_ns;
        default_item.setRepresentedObject(Some(rep));
        default_item.setState(if !env_locked && active.trim().is_empty() {
            NSControlStateValueOn
        } else {
            NSControlStateValueOff
        });
    }
    submenu.addItem(&default_item);
    submenu.addItem(&NSMenuItem::separatorItem(mtm));

    for name in names {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(name),
                Some(sel!(selectFormat:)),
                &NSString::from_str(""),
            )
        };
        unsafe {
            item.setTarget(Some(actions));
            item.setEnabled(!env_locked);
            let rep_ns = NSString::from_str(name);
            let rep: &AnyObject = &rep_ns;
            item.setRepresentedObject(Some(rep));
            item.setState(if !env_locked && active.eq_ignore_ascii_case(name) {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
        }
        submenu.addItem(&item);
    }

    if env_locked {
        submenu.addItem(&NSMenuItem::separatorItem(mtm));
        let note = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("(locked by DICTATE_FORMAT env var)"),
                None,
                &NSString::from_str(""),
            )
        };
        note.setEnabled(false);
        submenu.addItem(&note);
    }

    submenu
}

/// Build the Translate-to picker. Each configured target is a checkable row
/// (the active one checked); clicking saves the active target without a
/// relaunch (the translate path reads settings per utterance). When no targets
/// are configured, the submenu carries a single disabled note pointing at the
/// Settings window.
pub(super) fn build_translate_submenu(
    mtm: MainThreadMarker,
    actions: &Retained<MenuActions>,
    settings: &Settings,
) -> Retained<NSMenu> {
    let submenu = NSMenu::new(mtm);
    submenu.setAutoenablesItems(false);

    let targets = settings.resolve_translation_targets();
    if targets.is_empty() {
        let note = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("(add targets in Settings → Translate to)"),
                None,
                &NSString::from_str(""),
            )
        };
        note.setEnabled(false);
        submenu.addItem(&note);
        return submenu;
    }

    let active = settings.resolve_active_translation_target(&targets);
    for target in &targets {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(target),
                Some(sel!(selectTranslateTarget:)),
                &NSString::from_str(""),
            )
        };
        unsafe {
            item.setTarget(Some(actions));
            item.setEnabled(true);
            let rep_ns = NSString::from_str(target);
            let rep: &AnyObject = &rep_ns;
            item.setRepresentedObject(Some(rep));
            let is_active = active
                .as_deref()
                .map(|a| a.eq_ignore_ascii_case(target))
                .unwrap_or(false);
            item.setState(if is_active {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
        }
        submenu.addItem(&item);
    }

    submenu
}

/// Build the history-retention picker. Each window is a checkable row (the
/// active one checked); clicking saves the window without a relaunch (history
/// reads retention per record).
pub(super) fn build_retention_submenu(
    mtm: MainThreadMarker,
    actions: &Retained<MenuActions>,
    settings: &Settings,
) -> Retained<NSMenu> {
    let submenu = NSMenu::new(mtm);
    submenu.setAutoenablesItems(false);

    let active = settings.resolve_history_retention();
    const CHOICES: &[(&str, &str)] = &[
        ("Forever", "forever"),
        ("1 year", "1year"),
        ("1 month", "1month"),
        ("1 week", "1week"),
        ("24 hours", "24hours"),
        ("Never", "never"),
    ];
    for (label, rep) in CHOICES {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(label),
                Some(sel!(selectRetention:)),
                &NSString::from_str(""),
            )
        };
        unsafe {
            item.setTarget(Some(actions));
            item.setEnabled(true);
            let rep_ns = NSString::from_str(rep);
            let rep_obj: &AnyObject = &rep_ns;
            item.setRepresentedObject(Some(rep_obj));
            item.setState(if active.eq_ignore_ascii_case(rep) {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
        }
        submenu.addItem(&item);
    }

    submenu
}

/// Build the push-to-talk key submenu from `settings::HOTKEY_CHOICES`.
pub(super) fn build_hotkey_submenu(
    mtm: MainThreadMarker,
    actions: &Retained<MenuActions>,
    settings: &Settings,
) -> Retained<NSMenu> {
    let submenu = NSMenu::new(mtm);
    submenu.setAutoenablesItems(false);

    let env_locked = std::env::var_os("DICTATE_HOTKEY_KEYCODE").is_some();
    let active = settings.hotkey_keycode.unwrap_or(crate::hotkeys::DEFAULT_KEYCODE);

    for (label, code) in settings::HOTKEY_CHOICES {
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(label),
                Some(sel!(selectHotkey:)),
                &NSString::from_str(""),
            )
        };
        unsafe {
            item.setTarget(Some(actions));
            item.setEnabled(!env_locked);
            item.setTag(*code as isize);
            if !env_locked && *code == active {
                item.setState(NSControlStateValueOn);
            } else {
                item.setState(NSControlStateValueOff);
            }
        }
        submenu.addItem(&item);
    }

    if env_locked {
        submenu.addItem(&NSMenuItem::separatorItem(mtm));
        let note = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str("(locked by DICTATE_HOTKEY_KEYCODE env var)"),
                None,
                &NSString::from_str(""),
            )
        };
        note.setEnabled(false);
        submenu.addItem(&note);
    }

    submenu
}

pub(super) const TAG_HEALTH: isize = 9100;

pub(super) const TAG_COPY_LAST: isize = 9101;

pub(super) const TAG_PASTE_LAST: isize = 9102;

pub(super) const TAG_SECURE_INPUT: isize = 9103;

pub(super) const TAG_RECORD: isize = 9104;

pub(super) const TAG_CANCEL: isize = 9105;

pub(super) const TAG_RETRY: isize = 9106;

pub(super) const TAG_HISTORY: isize = 9107;

pub(super) const TAG_LATENCY: isize = 9108;

/// "Start dictation" or "Stop and transcribe", read at menu-open time from the
/// same flag the control socket resolves `toggle` against.
pub(super) fn record_toggle_title() -> String {
    if crate::ipc::DAEMON_RECORDING.load(Ordering::SeqCst) {
        "Stop and transcribe".to_string()
    } else {
        "Start dictation".to_string()
    }
}

/// The read-only timing line. It reports the app's own release-to-write timing
/// and never claims when another app's pixels became visible.
pub(super) fn latency_label() -> String {
    crate::latency::last()
        .map(|s| s.menu_label())
        .unwrap_or_else(|| "No insertion timing yet".to_string())
}

/// The copy item names its global hotkey, so the chord is discoverable.
pub(super) fn copy_last_title(hotkey: Option<&crate::hotkeys::Hotkey>) -> String {
    match hotkey {
        Some(h) => format!("Copy last text  ({})", h.display()),
        None => "Copy last text".to_string(),
    }
}

/// Bring the recovery rows up to date as the menu opens: the preview text,
/// Copy / Paste enabled only when there is a dictation, and the Secure Input
/// line shown only while it applies.
pub(super) fn refresh_recovery_items(menu: &NSMenu) {
    // Fault first: what is wrong, then the one thing to do about it.
    if let Some(item) = menu.itemWithTag(TAG_HEALTH) {
        match crate::health::current() {
            Some(problem) => {
                item.setTitle(&NSString::from_str(&problem.pill_line()));
                item.setEnabled(true);
                item.setHidden(false);
            }
            None => item.setHidden(true),
        }
    }
    let have_text = !ui_channel::last_dictation().trim().is_empty();
    for tag in [TAG_COPY_LAST, TAG_PASTE_LAST] {
        if let Some(item) = menu.itemWithTag(tag) {
            item.setEnabled(have_text);
        }
    }
    if let Some(item) = menu.itemWithTag(TAG_SECURE_INPUT) {
        item.setHidden(!crate::secure_input::query());
    }
    // Record/Stop follows the live capture flag the control socket resolves
    // `toggle` against; Cancel is live only while an utterance can still be
    // cancelled, so it never looks actionable when it would do nothing.
    if let Some(item) = menu.itemWithTag(TAG_RECORD) {
        item.setTitle(&NSString::from_str(&record_toggle_title()));
    }
    if let Some(item) = menu.itemWithTag(TAG_CANCEL) {
        item.setEnabled(ui_channel::state().is_cancellable());
    }
    if let Some(item) = menu.itemWithTag(TAG_LATENCY) {
        item.setTitle(&NSString::from_str(&latency_label()));
    }
    if let Some(globals) = GLOBALS.get() {
        globals
            .last_item
            .setTitle(&NSString::from_str(&last_dictation_label()));
    }
}

/// Title for the disabled preview item, truncated for the menu.
pub(super) fn last_dictation_label() -> String {
    let text = ui_channel::last_dictation();
    if text.is_empty() {
        return "No dictation yet".to_string();
    }
    let one_line = text.replace('\n', " ");
    let truncated: String = one_line.chars().take(48).collect();
    if one_line.chars().count() > 48 {
        format!("Last: \"{truncated}…\"")
    } else {
        format!("Last: \"{truncated}\"")
    }
}
