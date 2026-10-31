//! Process and system actions: relaunch, quit, the clipboard, the log, the
//! file pickers, the dictionary CSV import, and `run_on_main`.
use super::*;

// ─── Settings mutation + daemon relaunch ────────────────────────────────

/// Load settings, apply `mutate`, save, then relaunch the daemon so the
/// change takes effect. Runs on the main thread (inside a menu action).
pub(super) fn write_settings_and_relaunch(mutate: impl FnOnce(&mut Settings)) {
    let mut s = Settings::load();
    mutate(&mut s);
    if let Err(e) = s.save() {
        eprintln!("[menu] failed to save settings: {e}");
        return;
    }
    relaunch_daemon();
}

/// Spawn a fresh daemon (same binary, `daemon` subcommand) with stdout+stderr
/// pointed at the log file — matching how the daemon is normally launched so
/// Open/Export Log keep working — then terminate ourselves.
pub(crate) fn relaunch_daemon() {
    use std::process::{Command, Stdio};

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[menu] current_exe failed, not relaunching: {e}");
            return;
        }
    };

    let open_log = crate::daemon_log::open;
    let (stdout, stderr) = match (open_log(), open_log()) {
        (Ok(a), Ok(b)) => (Stdio::from(a), Stdio::from(b)),
        _ => (Stdio::inherit(), Stdio::inherit()),
    };

    eprintln!("[menu] settings changed — relaunching daemon…");
    match Command::new(&exe)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
    {
        Ok(_) => terminate_app(),
        Err(e) => eprintln!("[menu] relaunch spawn failed: {e}"),
    }
}

pub(super) fn terminate_app() {
    if let Some(mtm) = MainThreadMarker::new() {
        let app = NSApplication::sharedApplication(mtm);
        app.terminate(None);
    }
}

// ─── Menu action helpers (plain Rust, no AppKit) ────────────────────────

/// Copy behind any pending paste transaction without blocking AppKit. Confirm
/// on the main thread only after the clipboard write has actually succeeded.
pub(crate) fn copy_to_clipboard(text: String, finished: impl FnOnce(bool) + Send + 'static) {
    std::thread::spawn(move || {
        let copied = crate::clipboard_paste::copy_only(&text).is_ok();
        run_on_main(move || finished(copied));
    });
}

/// Open the daemon log in the default text editor ("Open Log").
pub(crate) fn open_log_in_editor() {
    let Some(log) = crate::app_paths::daemon_log_path() else {
        return;
    };
    let _ = std::process::Command::new("open").arg("-t").arg(log).status();
}

pub(crate) fn export_log_to_downloads() {
    let Some(home) = crate::app_paths::home_dir() else {
        return;
    };
    let Some(log) = crate::app_paths::daemon_log_path() else {
        return;
    };
    if !log.exists() {
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dest = home
        .join("Downloads")
        .join(format!("dictate-log-{ts}.txt"));
    if std::fs::copy(&log, &dest).is_ok() {
        // Reveal it in Finder so the user sees where it landed.
        let _ = std::process::Command::new("open")
            .arg("-R")
            .arg(&dest)
            .status();
    }
}

pub(crate) fn open_corrections_folder() {
    let Some(dir) = crate::app_paths::config_dir() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let corrections = dir.join("corrections.json");
    if corrections.exists() {
        let _ = std::process::Command::new("open")
            .arg("-R")
            .arg(&corrections)
            .status();
    } else {
        let _ = std::process::Command::new("open").arg(&dir).status();
    }
}

/// True while an AppleScript file or folder picker is open, so a second menu
/// click does not stack another dialog.
pub(super) static PICKER_OPEN: AtomicBool = AtomicBool::new(false);

/// Show an AppleScript `choose file` / `choose folder` dialog without blocking
/// the main thread, then hand the chosen POSIX path to `on_chosen` on the main
/// thread. The dialog runs in `/usr/bin/osascript`, so waiting for it on the
/// main thread froze the pill and the menu, and stalled the event tap that
/// lives on the main run loop (every key press, including the ones typed into
/// the dialog, waited for the tap to time out). A cancel or an error chooses
/// nothing and calls nothing. Returns false, and calls nothing, when a picker
/// is already open or could not start.
pub(super) fn pick_path_with_osascript(
    script: &str,
    label: &'static str,
    on_chosen: fn(PathBuf),
) -> bool {
    if PICKER_OPEN.swap(true, Ordering::SeqCst) {
        eprintln!("[menu] {label}: a picker is already open");
        return false;
    }
    let script = script.to_string();
    let spawned = std::thread::Builder::new()
        .name("osascript-picker".into())
        .spawn(move || {
            let chosen = run_osascript_picker(&script, label);
            PICKER_OPEN.store(false, Ordering::SeqCst);
            if let Some(path) = chosen {
                run_on_main(move || on_chosen(path));
            }
        });
    if spawned.is_err() {
        PICKER_OPEN.store(false, Ordering::SeqCst);
        eprintln!("[menu] {label}: could not start the picker thread");
        return false;
    }
    true
}

pub(super) fn run_osascript_picker(script: &str, label: &str) -> Option<PathBuf> {
    let out = std::process::Command::new("/usr/bin/osascript")
        .args(["-e", script])
        .output();
    let Ok(out) = out else {
        eprintln!("[menu] {label}: osascript failed to launch");
        return None;
    };
    if !out.status.success() {
        // User dismissed the dialog (AppleScript returns -128 on cancel) or
        // the script errored. Nothing was chosen, so there's nothing to do.
        eprintln!(
            "[menu] {label} cancelled or failed (osascript exit {:?})",
            out.status.code()
        );
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Run `work` once on the main run loop. Safe from any thread: it adds a
/// one-shot timer, which Core Foundation allows from any thread, in the
/// common modes so it also fires while a menu or modal is open.
pub(crate) fn run_on_main(work: impl FnOnce() + Send + 'static) {
    type Work = Box<dyn FnOnce() + Send>;
    unsafe extern "C-unwind" fn fire(_timer: *mut CFRunLoopTimer, info: *mut c_void) {
        // SAFETY: `info` is the `Box<Work>` leaked below, and a one-shot
        // timer fires exactly once.
        let work = unsafe { Box::from_raw(info as *mut Work) };
        work();
    }
    let info = Box::into_raw(Box::new(Box::new(work) as Work)) as *mut c_void;
    let mut ctx = CFRunLoopTimerContext {
        version: 0,
        info,
        retain: None,
        release: None,
        copyDescription: None,
    };
    // SAFETY: `fire` matches the callback signature and takes `info` back.
    let timer = unsafe {
        CFRunLoopTimer::new(
            None,
            CFAbsoluteTimeGetCurrent(),
            0.0,
            0,
            0,
            Some(fire),
            &mut ctx,
        )
    };
    match (timer, CFRunLoop::main()) {
        (Some(timer), Some(main)) => {
            main.add_timer(Some(&timer), unsafe { kCFRunLoopCommonModes });
            main.wake_up();
        }
        _ => {
            // Never fired: take the work back so it is dropped, not leaked.
            drop(unsafe { Box::from_raw(info as *mut Work) });
            eprintln!("[menu] could not schedule work on the main thread");
        }
    }
}

/// Import entries from a user-picked CSV file into the personal vocabulary
/// (corrections.json), then refresh the dictionary window's editor text and
/// confirm via the window subtitle.
///
/// The native file dialog is driven through AppleScript's `choose file` (which
/// presents the same macOS open panel) because `NSOpenPanel` isn't compiled
/// into this build. The dialog runs off the main thread
/// (`pick_path_with_osascript`); the merge runs on it once a file is chosen.
/// A CSV is matched by its standard UTI (`public.comma-separated-
/// values-text`) with a `public.text` fallback for pickers that don't register
/// the specific type.
pub(super) fn import_dictionary_csv() {
    let _ = pick_path_with_osascript(
        "POSIX path of (choose file of type {\"public.comma-separated-values-text\", \"public.text\"} with prompt \"Choose a dictionary CSV to import\")",
        "import CSV",
        finish_dictionary_import,
    );
}

/// Merge the chosen CSV into corrections.json and refresh the dictionary
/// window. Runs on the main thread, after the picker closed.
pub(super) fn finish_dictionary_import(path: PathBuf) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };

    let mut corrections = match crate::corrections::Corrections::load_default() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[menu] import CSV: failed to load corrections: {e}");
            return;
        }
    };
    let added = match corrections.import_csv_path(&path) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("[menu] import CSV: failed to read {}: {e}", path.display());
            return;
        }
    };
    if let Err(e) = corrections.save() {
        eprintln!("[menu] import CSV: failed to save corrections: {e}");
        return;
    }
    eprintln!("[menu] imported {added} term(s) from {}", path.display());

    // Refresh the dictionary window's editor text from the merged vocabulary so
    // the imported entries appear inline, and bring it forward. (We reuse the
    // build-once window; corrections are read fresh from disk here.
    let ui = dictionary_ui(mtm);
    let joined = crate::corrections::Corrections::load_default()
        .map(|c| c.to_editor_lines())
        .unwrap_or_default()
        .join("\n");
    ui.text_view.setString(&NSString::from_str(&joined));
    unsafe {
        let msg = NSString::from_str(&format!("Imported {added} term(s)"));
        let _: () = msg_send![&*ui.window, setSubtitle: &*msg];
    }
    ui.window.makeKeyAndOrderFront(None);
    let app = NSApplication::sharedApplication(mtm);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
}

/// Open the user's `prompts.json` (cleanup + transform system prompts) in a
/// text editor. The file is optional — the daemon falls back to built-in
/// defaults when it's missing — so the first time it doesn't exist we seed it
/// with the *currently active* prompts plus inline `_`-comments. That way the
/// user edits real, working text instead of staring at a blank file, and the
/// seeded values round-trip through `Prompts::load()` unchanged.
///
/// Edits take effect on the next daemon launch (prompts are read once at
/// startup), so the comment tells the user to relaunch — e.g. toggle "Cleanup
/// enabled" off and on, which relaunches the daemon.
pub(crate) fn open_prompts_file() {
    let Some(path) = crate::prompts::Prompts::config_path() else {
        eprintln!("[menu] cannot resolve prompts.json path (no $HOME?)");
        return;
    };

    if !path.exists() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let prompts = crate::prompts::Prompts::load();
        let esc = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string());
        let seed = format!(
            "{{\n  \"_comment\": {},\n\n  \"_cleanup\": {},\n  \"cleanup\": {},\n\n  \"_transform\": {},\n  \"transform\": {}\n}}\n",
            esc(
                "Edit the two system prompts below, then save. Changes take effect on the \
                 next daemon launch — toggle \u{201C}Cleanup enabled\u{201D} off and on \
                 (or quit and relaunch the daemon) to reload them. Blank or delete a field \
                 to restore that prompt\u{2019}s built-in default. \u{201C}_\u{201D}-prefixed \
                 keys are comments and are ignored."
            ),
            esc(
                "Always-on cleanup: tidies every normal dictation. Keep it conservative — \
                 its job is to clean up speech-to-text, not to rewrite or summarize."
            ),
            esc(&prompts.cleanup),
            esc(
                "Transform mode (Shift + push-to-talk): you select text, speak an \
                 instruction, and the model rewrites the selection in place."
            ),
            esc(&prompts.transform),
        );
        if let Err(e) = std::fs::write(&path, seed) {
            eprintln!(
                "[menu] failed to seed prompts.json ({}): {e}",
                path.display()
            );
            // Still try to open whatever exists / the folder below.
        }
    }

    // `open -t` opens in the default text editor (TextEdit), matching Open Log.
    let _ = std::process::Command::new("open")
        .arg("-t")
        .arg(&path)
        .status();
}
