//! Local control socket — drive the running daemon from outside.
//!
//! The hotkey is the primary way to dictate, but a tiny Unix-domain-socket
//! control surface lets *other* tools start/stop/cancel the same flow without
//! loading a second copy of the models: macOS Shortcuts, Raycast, a Stream Deck
//! button, an Alfred workflow, or a hardware foot pedal can all just run
//! `fast-dictate-backend toggle`. It speaks a one-word line protocol so the
//! client side stays a three-line shell-out.
//!
//! It also answers `status` with one line of JSON and publishes a manifest of
//! its commands under `~/Library/Application Support/House/commands/`, so Quick
//! Launch can list and drive dictation without knowing anything about it. That
//! contract is `design-system/docs/app-commands.md`, and this file is the
//! reference it was written from.
//!
//! Design: the socket thread is deliberately dumb — it parses a word, resolves
//! `toggle` against the live recording state, and hands a concrete command to a
//! closure the daemon provides (which forwards it onto the same mpsc channel the
//! event tap uses). No model, no AX, nothing on the audio critical path. If the
//! socket can't be bound the daemon logs and carries on — losing remote control
//! must never cost you the hotkey.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

/// True while the daemon is mid-utterance (capturing). The worker loop owns the
/// truth and writes it here; the control server reads it so `toggle` knows which
/// direction to fire. A process-wide static mirrors the existing `TAP_PORT`
/// pattern in `daemon.rs` — there's only ever one daemon per process.
pub static DAEMON_RECORDING: AtomicBool = AtomicBool::new(false);

/// A control action. `Toggle` is resolved to `Start`/`Stop` by the server before
/// it reaches the daemon, so the daemon's dispatch closure only ever sees a
/// concrete intent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Start,
    Stop,
    Toggle,
    Cancel,
    /// Re-run the pipeline on the last utterance's audio (re-transcribe +
    /// cleanup + inject at the current cursor). No-op when nothing has been
    /// dictated yet in this daemon session.
    Retry,
    /// Recover a stored dictation by database id. Re-transcribe and clean
    /// locally, then copy to the clipboard; never paste or execute commands.
    Recover(i64),
    /// Copy the most recent finished transcript to the clipboard. Copy only:
    /// nothing is pasted and no app is activated. Same code path as the global
    /// hotkey and the menu item.
    CopyLast,
    /// Report live state as one line of JSON. Answered by the socket thread
    /// itself — it never reaches the daemon's dispatch closure, so asking is
    /// free and cannot touch the audio path. See the house command contract
    /// (`design-system/docs/app-commands.md`).
    Status,
}

impl Command {
    /// Parse a single wire word (case-insensitive, surrounding whitespace
    /// ignored). Unknown words → `None`.
    pub fn parse(word: &str) -> Option<Command> {
        let normalized = word.trim().to_ascii_lowercase();
        let fields: Vec<&str> = normalized.split_whitespace().collect();
        if let ["recover", id] = fields.as_slice() {
            if !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()) {
                return id
                    .parse::<i64>()
                    .ok()
                    .filter(|id| *id > 0)
                    .map(Command::Recover);
            }
            return None;
        }
        match normalized.as_str() {
            "start" => Some(Command::Start),
            "stop" => Some(Command::Stop),
            "toggle" => Some(Command::Toggle),
            "cancel" => Some(Command::Cancel),
            "retry" => Some(Command::Retry),
            "copy-last" => Some(Command::CopyLast),
            "status" => Some(Command::Status),
            _ => None,
        }
    }

    /// The wire word for this command — also the CLI subcommand name.
    pub fn as_str(self) -> &'static str {
        match self {
            Command::Start => "start",
            Command::Stop => "stop",
            Command::Toggle => "toggle",
            Command::Cancel => "cancel",
            Command::Retry => "retry",
            Command::Recover(_) => "recover",
            Command::CopyLast => "copy-last",
            Command::Status => "status",
        }
    }

    /// Wire form carries only a positive database id, never a file path.
    pub fn wire(self) -> String {
        match self {
            Command::Recover(id) => format!("recover {id}"),
            other => other.as_str().to_string(),
        }
    }
}

/// The app id this daemon answers to in the house command contract. Also the
/// manifest's filename stem.
pub const APP_ID: &str = "dictation";

/// Live state as one line of JSON, per the house status document: `app`, `ok`,
/// `busy`, `detail`, plus every boolean the manifest's `unavailableWhen`
/// clauses name (here: `recording`). `busy` mirrors `recording` — capturing an
/// utterance is the only thing this daemon is ever busy with from outside.
pub fn status_json() -> String {
    status_json_for(
        DAEMON_RECORDING.load(Ordering::SeqCst),
        crate::secure_input::query(),
    )
}

/// `hotkeyPaused` is true while another app holds Secure Input: macOS then
/// delivers no key events, so the hotkeys are silent and this socket is the
/// way to dictate. The detail line says so in words.
fn status_json_for(recording: bool, hotkey_paused: bool) -> String {
    let detail = match (recording, hotkey_paused) {
        (true, _) => "Listening",
        (false, true) => crate::secure_input::PAUSED_TEXT,
        (false, false) => "Idle",
    };
    serde_json::json!({
        "app": APP_ID,
        "ok": true,
        "busy": recording,
        "recording": recording,
        "hotkeyPaused": hotkey_paused,
        "detail": detail,
    })
    .to_string()
}

/// The same document for a daemon that isn't running, so a caller that can't
/// reach the socket still gets a well-formed answer instead of a parse error.
/// `ok: false` is the honest signal; nothing is claimed about the daemon.
pub fn offline_status_json() -> String {
    serde_json::json!({
        "app": APP_ID,
        "ok": false,
        "busy": false,
        "recording": false,
        "detail": "Not running",
    })
    .to_string()
}

/// The command manifest, built against an explicit endpoint. Factored out from
/// [`manifest_json`] so it's unit-testable without depending on where this
/// machine's socket happens to resolve.
fn manifest_for(endpoint: &str) -> serde_json::Value {
    serde_json::json!({
        "schema": 1,
        "app": APP_ID,
        "name": "Local Dictation",
        "transport": "socket",
        "endpoint": endpoint,
        "status": "status",
        "commands": [
            // Toggle is always offered: it resolves itself against the live
            // recording state, so it is correct in both directions.
            {"id": "dictation.toggle", "title": "Toggle Dictation", "verb": "toggle",
             "needs": null, "unavailableWhen": null},
            {"id": "dictation.start", "title": "Start Dictation", "verb": "start",
             "needs": null, "unavailableWhen": "recording"},
            {"id": "dictation.stop", "title": "Stop Dictation", "verb": "stop",
             "needs": null, "unavailableWhen": "!recording"},
            {"id": "dictation.cancel", "title": "Cancel Dictation", "verb": "cancel",
             "needs": null, "unavailableWhen": "!recording"},
            // Retry re-runs the pipeline on the *last* utterance, so it only
            // makes sense between utterances.
            {"id": "dictation.retry", "title": "Retry Last Dictation", "verb": "retry",
             "needs": null, "unavailableWhen": "recording"},
            // Copy only; never pastes. Offered at any time: it does not touch
            // the recording in flight.
            {"id": "dictation.copy-last", "title": "Copy Last Dictation", "verb": "copy-last",
             "needs": null, "unavailableWhen": null},
        ],
    })
}

/// The command manifest for this daemon, as pretty JSON with the endpoint
/// resolved to the socket path actually in use.
pub fn manifest_json() -> String {
    let value = manifest_for(&socket_path().to_string_lossy());
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
}

/// Write the manifest to `path`, creating the directory if needed.
fn write_manifest_to(path: &std::path::Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, format!("{}\n", manifest_json()))
}

/// Publish this app's command manifest under the shared house commands
/// directory, so Quick Launch can list what we do without knowing us by name.
/// Every failure is logged and swallowed: a daemon that can't advertise itself
/// still dictates, and that is the part that matters.
pub fn write_manifest() {
    let Some(dir) = crate::app_paths::house_commands_dir() else {
        eprintln!("[ctl]  no $HOME — skipping the command manifest");
        return;
    };
    let path = dir.join(format!("{APP_ID}.json"));
    match write_manifest_to(&path) {
        Ok(()) => eprintln!("[boot] commands   manifest {}", path.display()),
        Err(e) => eprintln!(
            "[ctl]  couldn't write the command manifest at {} ({e}) — carrying on",
            path.display()
        ),
    }
}

/// Path to the control socket. `DICTATE_CONTROL_SOCK` overrides it; otherwise it
/// lives next to the other user config (`~/.config/local-dictation/control.sock`).
/// A *stable, login-session-independent* location is the point: `temp_dir()`
/// resolves differently for a GUI-launched daemon (its own per-session `$TMPDIR`
/// under `/var/folders/…`) than for a separately-spawned client (Shortcut,
/// Raycast, a terminal), so the two would never meet at the same path. The
/// config dir is identical for both.
pub fn socket_path() -> PathBuf {
    socket_path_from(std::env::var("DICTATE_CONTROL_SOCK").ok())
}

/// Resolve the socket path from an optional explicit override. Factored out from
/// [`socket_path`] so it's unit-testable without mutating process-global env. A
/// non-empty override is used verbatim; otherwise the path sits in the shared
/// config dir, falling back to the temp dir only if `$HOME` is unset.
fn socket_path_from(env_override: Option<String>) -> PathBuf {
    if let Some(p) = env_override.filter(|s| !s.trim().is_empty()) {
        return PathBuf::from(p);
    }
    crate::app_paths::config_file("control.sock")
        .unwrap_or_else(|| std::env::temp_dir().join("dictate-control.sock"))
}

/// Resolve `Toggle` to a concrete `Start`/`Stop` from the live recording state.
/// Everything else passes through unchanged.
fn resolve(cmd: Command) -> Command {
    match cmd {
        Command::Toggle => {
            if DAEMON_RECORDING.load(Ordering::SeqCst) {
                Command::Stop
            } else {
                Command::Start
            }
        }
        other => other,
    }
}

/// Spawn the control server on a background thread. For every well-formed line,
/// `dispatch` is called with a concrete command (`Toggle` already resolved
/// against `DAEMON_RECORDING`). Binding failures are logged, not fatal — the
/// hotkey remains the primary control path. Returns once the listener is bound
/// (or has failed to); the accept loop runs detached.
pub fn serve<F>(dispatch: F)
where
    F: Fn(Command) + Send + 'static,
{
    let path = socket_path();
    // The config dir is the default home now, so make sure it exists before we
    // bind (it normally does — settings.json etc. live there — but a fresh
    // install might not have written anything yet).
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // A leftover socket file from a crashed run blocks bind(); clear it first.
    // The daemon relaunch flow always kills the previous instance before the
    // new one boots (reload-daemon.sh), so we never race two live owners — a
    // stale path is always genuinely stale.
    let _ = std::fs::remove_file(&path);
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "[ctl]  control socket unavailable at {} ({e}) — hotkey still works",
                path.display()
            );
            return;
        }
    };
    eprintln!("[boot] control     socket {}", path.display());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(s) => serve_conn(s, &dispatch),
                Err(_) => continue, // transient accept error; keep listening
            }
        }
    });
}

/// Handle one client connection: read a command word, resolve `toggle`, dispatch
/// it, and write back a status line (`ok <cmd>` / `err ...`). `status` is the
/// one verb answered here instead of dispatched — it only reads an atomic, so
/// it stays off the daemon's channel entirely.
fn serve_conn<F: Fn(Command)>(stream: UnixStream, dispatch: &F) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let reply = match Command::parse(&line) {
        Some(Command::Status) => format!("{}\n", status_json()),
        Some(Command::Recover(id)) => recovery_reply(
            id,
            DAEMON_RECORDING.load(Ordering::SeqCst),
            crate::history::recovery_audio_path,
            dispatch,
        ),
        Some(Command::CopyLast) => copy_last_reply(
            crate::ui_channel::last_dictation().trim().is_empty(),
            dispatch,
        ),
        Some(cmd) => {
            let resolved = resolve(cmd);
            dispatch(resolved);
            format!("ok {}\n", resolved.wire())
        }
        None => format!("err unknown command {:?}\n", line.trim()),
    };
    let mut stream = reader.into_inner();
    let _ = stream.write_all(reply.as_bytes());
}

/// `copy-last` answers honestly: with nothing dictated yet there is nothing to
/// copy, and the daemon is not bothered.
fn copy_last_reply(nothing_yet: bool, dispatch: impl FnOnce(Command)) -> String {
    if nothing_yet {
        return "err No dictation to copy yet.\n".to_string();
    }
    dispatch(Command::CopyLast);
    "ok copy-last\n".to_string()
}

fn recovery_reply(
    id: i64,
    recording: bool,
    lookup: impl FnOnce(i64) -> eyre::Result<Option<PathBuf>>,
    dispatch: impl FnOnce(Command),
) -> String {
    if recording {
        return "err Finish the current recording before recovering saved audio.\n".to_string();
    }
    match lookup(id) {
        Ok(Some(_)) => {
            dispatch(Command::Recover(id));
            format!("ok recover {id}\n")
        }
        Ok(None) => {
            "err Original audio is unavailable. It may have been deleted, expired, or never saved.\n"
                .to_string()
        }
        Err(_) => "err Could not read the original audio.\n".to_string(),
    }
}

/// Send one command to a running daemon and return its reply line (without the
/// trailing newline). Errors if no daemon is listening — the CLI surfaces that
/// as a friendly "is the daemon running?" message.
pub fn send(cmd: Command) -> std::io::Result<String> {
    if matches!(cmd, Command::Recover(id) if id <= 0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Recovery needs a positive dictation id",
        ));
    }
    let path = socket_path();
    let mut stream = UnixStream::connect(&path)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(2)))?;
    stream.write_all(cmd.wire().as_bytes())?;
    stream.write_all(b"\n")?;
    let _ = stream.flush();
    // Half-close our write side so the server's read_line sees EOF promptly.
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    reader.read_line(&mut reply)?;
    Ok(reply.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// `DAEMON_RECORDING` is process-global, so every test that writes it takes
    /// this lock first. Poisoning is ignored: a panicking test tells us about
    /// its own failure, it shouldn't cascade into the others.
    static FLAG_LOCK: Mutex<()> = Mutex::new(());

    fn lock_flag() -> std::sync::MutexGuard<'static, ()> {
        FLAG_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn status_says_when_secure_input_has_paused_the_hotkey() {
        let v: serde_json::Value = serde_json::from_str(&status_json_for(false, true)).unwrap();
        assert_eq!(v["hotkeyPaused"], true);
        assert!(v["detail"].as_str().unwrap().contains("Secure Input"));
        let v: serde_json::Value = serde_json::from_str(&status_json_for(true, true)).unwrap();
        assert_eq!(v["detail"], "Listening");
        let v: serde_json::Value = serde_json::from_str(&status_json_for(false, false)).unwrap();
        assert_eq!(v["hotkeyPaused"], false);
        assert_eq!(v["detail"], "Idle");
    }

    #[test]
    fn copy_last_parses_and_replies_honestly() {
        assert_eq!(Command::parse(" Copy-Last\n"), Some(Command::CopyLast));
        assert_eq!(Command::CopyLast.wire(), "copy-last");
        let mut sent = None;
        assert_eq!(copy_last_reply(false, |c| sent = Some(c)), "ok copy-last\n");
        assert_eq!(sent, Some(Command::CopyLast));
        let mut sent = None;
        let reply = copy_last_reply(true, |c| sent = Some(c));
        assert!(reply.starts_with("err No dictation"));
        assert_eq!(sent, None, "nothing to copy never reaches the daemon");
    }

    #[test]
    fn the_manifest_offers_copy_last_at_any_time() {
        let m = manifest_for("/tmp/x.sock");
        let cmd = m["commands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["verb"] == "copy-last")
            .expect("copy-last is published");
        assert_eq!(cmd["id"], "dictation.copy-last");
        assert!(cmd["unavailableWhen"].is_null());
    }

    #[test]
    fn parse_is_case_insensitive_and_trims() {
        assert_eq!(Command::parse(" Toggle\n"), Some(Command::Toggle));
        assert_eq!(Command::parse("CANCEL"), Some(Command::Cancel));
        assert_eq!(Command::parse("start"), Some(Command::Start));
        assert_eq!(Command::parse("stop"), Some(Command::Stop));
        assert_eq!(Command::parse("nonsense"), None);
        assert_eq!(Command::parse(""), None);
    }

    #[test]
    fn as_str_parse_roundtrips() {
        for c in [
            Command::Start,
            Command::Stop,
            Command::Toggle,
            Command::Cancel,
            Command::Retry,
            Command::Status,
        ] {
            assert_eq!(Command::parse(c.as_str()), Some(c));
        }
    }

    #[test]
    fn recovery_accepts_only_a_positive_database_id() {
        assert_eq!(Command::parse(" RECOVER 42\n"), Some(Command::Recover(42)));
        assert_eq!(Command::Recover(42).wire(), "recover 42");
        assert_eq!(
            Command::parse(&Command::Recover(i64::MAX).wire()),
            Some(Command::Recover(i64::MAX))
        );
        for invalid in [
            "recover",
            "recover 0",
            "recover -1",
            "recover +1",
            "recover /tmp/audio.wav",
            "recover ../audio.wav",
            "recover 42 extra",
            "recover 9223372036854775808",
        ] {
            assert_eq!(Command::parse(invalid), None, "{invalid}");
        }
        assert_eq!(
            send(Command::Recover(-1)).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn recovery_rejects_busy_or_missing_audio_before_dispatch() {
        let reply = recovery_reply(
            42,
            true,
            |_| panic!("busy must not read audio"),
            |_| panic!("busy must not dispatch"),
        );
        assert!(reply.starts_with("err Finish"));
        let reply = recovery_reply(
            42,
            false,
            |_| Ok(None),
            |_| panic!("missing audio must not dispatch"),
        );
        assert!(reply.contains("unavailable"));
        let mut dispatched = None;
        let reply = recovery_reply(
            42,
            false,
            |id| {
                assert_eq!(id, 42);
                Ok(Some(PathBuf::from("/tmp/owned-audio.wav")))
            },
            |command| dispatched = Some(command),
        );
        assert_eq!(reply, "ok recover 42\n");
        assert_eq!(dispatched, Some(Command::Recover(42)));
    }

    #[test]
    fn socket_path_uses_override_then_config_dir() {
        // An explicit override is used verbatim…
        assert_eq!(
            socket_path_from(Some("/tmp/custom.sock".to_string())),
            PathBuf::from("/tmp/custom.sock")
        );
        // …a blank/whitespace override is ignored (falls through to the default)…
        assert!(socket_path_from(Some("   ".to_string()))
            .to_string_lossy()
            .ends_with("control.sock"));
        // …and the default sits next to the other config, not in temp_dir.
        let def = socket_path_from(None);
        assert!(def.ends_with("control.sock"), "{def:?}");
        if std::env::var_os("HOME").is_some() {
            assert!(
                def.to_string_lossy().contains("local-dictation"),
                "default should live in the config dir: {def:?}"
            );
        }
    }

    #[test]
    fn toggle_resolves_against_recording_state() {
        // The recording flag is process-global; `FLAG_LOCK` serialises the
        // tests that write it so neither reads the other's value.
        let _guard = lock_flag();
        DAEMON_RECORDING.store(false, Ordering::SeqCst);
        assert_eq!(resolve(Command::Toggle), Command::Start);
        DAEMON_RECORDING.store(true, Ordering::SeqCst);
        assert_eq!(resolve(Command::Toggle), Command::Stop);
        // Concrete commands pass through regardless of recording state.
        assert_eq!(resolve(Command::Cancel), Command::Cancel);
        assert_eq!(resolve(Command::Start), Command::Start);
        DAEMON_RECORDING.store(false, Ordering::SeqCst);
    }

    #[test]
    fn server_dispatches_commands_over_socket() {
        use std::sync::mpsc;
        // Pin the socket to a unique per-process temp path so this test is
        // hermetic and doesn't collide with a real daemon's socket.
        let sock =
            std::env::temp_dir().join(format!("dictate-control-test-{}.sock", std::process::id()));
        std::env::set_var("DICTATE_CONTROL_SOCK", &sock);

        let (tx, rx) = mpsc::channel();
        serve(move |cmd| {
            let _ = tx.send(cmd);
        });

        // Concrete commands (no dependence on the global recording flag) so the
        // assertion is deterministic under parallel test execution.
        let reply = send(Command::Start).expect("send start");
        assert_eq!(reply, "ok start");
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap(),
            Command::Start
        );

        let reply = send(Command::Cancel).expect("send cancel");
        assert_eq!(reply, "ok cancel");
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap(),
            Command::Cancel
        );

        // `status` is answered by the server itself — nothing reaches the
        // dispatch closure — and comes back as one line of JSON.
        {
            let _guard = lock_flag();
            DAEMON_RECORDING.store(true, Ordering::SeqCst);
            let reply = send(Command::Status).expect("send status");
            let doc: serde_json::Value = serde_json::from_str(&reply).expect("status is JSON");
            assert_eq!(doc["recording"], serde_json::json!(true));
            assert_eq!(doc["detail"], serde_json::json!("Listening"));
            DAEMON_RECORDING.store(false, Ordering::SeqCst);
        }
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "status must not reach the daemon's dispatch closure"
        );

        std::env::remove_var("DICTATE_CONTROL_SOCK");
        let _ = std::fs::remove_file(&sock);
    }

    #[test]
    fn status_json_carries_the_contract_fields() {
        let _guard = lock_flag();

        DAEMON_RECORDING.store(false, Ordering::SeqCst);
        let doc: serde_json::Value = serde_json::from_str(&status_json()).expect("valid JSON");
        assert_eq!(doc["app"], serde_json::json!("dictation"));
        assert_eq!(doc["ok"], serde_json::json!(true));
        assert_eq!(doc["busy"], serde_json::json!(false));
        assert_eq!(doc["recording"], serde_json::json!(false));
        assert_eq!(doc["detail"], serde_json::json!("Idle"));
        // One line, always: the protocol is one reply line per request.
        assert!(!status_json().contains('\n'));

        DAEMON_RECORDING.store(true, Ordering::SeqCst);
        let doc: serde_json::Value = serde_json::from_str(&status_json()).expect("valid JSON");
        assert_eq!(doc["busy"], serde_json::json!(true));
        assert_eq!(doc["recording"], serde_json::json!(true));
        assert_eq!(doc["detail"], serde_json::json!("Listening"));
        DAEMON_RECORDING.store(false, Ordering::SeqCst);
    }

    #[test]
    fn offline_status_is_a_well_formed_not_running_document() {
        let doc: serde_json::Value = serde_json::from_str(&offline_status_json()).expect("JSON");
        assert_eq!(doc["app"], serde_json::json!("dictation"));
        assert_eq!(doc["ok"], serde_json::json!(false));
        assert_eq!(doc["busy"], serde_json::json!(false));
        assert_eq!(doc["recording"], serde_json::json!(false));
        assert_eq!(doc["detail"], serde_json::json!("Not running"));
    }

    #[test]
    fn manifest_matches_the_house_contract() {
        let m = manifest_for("/tmp/test-control.sock");
        assert_eq!(m["schema"], serde_json::json!(1));
        assert_eq!(m["app"], serde_json::json!("dictation"));
        assert_eq!(m["name"], serde_json::json!("Local Dictation"));
        assert_eq!(m["transport"], serde_json::json!("socket"));
        assert_eq!(m["endpoint"], serde_json::json!("/tmp/test-control.sock"));
        assert_eq!(m["status"], serde_json::json!("status"));

        let cmds = m["commands"].as_array().expect("commands is an array");
        let verbs: Vec<&str> = cmds.iter().map(|c| c["verb"].as_str().unwrap()).collect();
        assert_eq!(verbs, ["toggle", "start", "stop", "cancel", "retry", "copy-last"]);

        for c in cmds {
            // Every advertised verb must actually be one we parse…
            let verb = c["verb"].as_str().unwrap();
            assert!(Command::parse(verb).is_some(), "unparseable verb {verb}");
            // …every command carries the full contract shape…
            assert!(c["id"].is_string(), "{verb} needs an id");
            assert!(c["title"].is_string(), "{verb} needs a title");
            assert!(c.get("needs").is_some(), "{verb} needs a `needs` field");
            // …and `unavailableWhen` is null or names a status boolean.
            let when = &c["unavailableWhen"];
            assert!(
                when.is_null() || when.is_string(),
                "{verb}: bad unavailableWhen"
            );
            if let Some(s) = when.as_str() {
                assert_eq!(s.trim_start_matches('!'), "recording", "{verb}");
            }
        }

        // The named clauses, spelled out: start is gone mid-utterance, stop and
        // cancel only exist mid-utterance, retry works on the last one.
        let when = |verb: &str| -> serde_json::Value {
            cmds.iter()
                .find(|c| c["verb"] == serde_json::json!(verb))
                .unwrap()["unavailableWhen"]
                .clone()
        };
        assert_eq!(when("toggle"), serde_json::Value::Null);
        assert_eq!(when("start"), serde_json::json!("recording"));
        assert_eq!(when("stop"), serde_json::json!("!recording"));
        assert_eq!(when("cancel"), serde_json::json!("!recording"));
        assert_eq!(when("retry"), serde_json::json!("recording"));
        assert_eq!(when("copy-last"), serde_json::Value::Null);
    }

    #[test]
    fn manifest_is_written_as_parseable_json() {
        let dir =
            std::env::temp_dir().join(format!("dictate-manifest-test-{}", std::process::id()));
        let path = dir.join("dictation.json");
        write_manifest_to(&path).expect("write manifest");

        let text = std::fs::read_to_string(&path).expect("read manifest back");
        let doc: serde_json::Value = serde_json::from_str(&text).expect("manifest is valid JSON");
        assert_eq!(doc["app"], serde_json::json!("dictation"));
        assert_eq!(doc["schema"], serde_json::json!(1));
        assert_eq!(doc["commands"].as_array().unwrap().len(), 6);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
