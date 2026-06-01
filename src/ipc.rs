//! Local control socket — drive the running daemon from outside.
//!
//! The hotkey is the primary way to dictate, but a tiny Unix-domain-socket
//! control surface lets *other* tools start/stop/cancel the same flow without
//! loading a second copy of the models: macOS Shortcuts, Raycast, a Stream Deck
//! button, an Alfred workflow, or a hardware foot pedal can all just run
//! `fast-dictate-backend toggle`. It speaks a one-word line protocol so the
//! client side stays a three-line shell-out.
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
}

impl Command {
    /// Parse a single wire word (case-insensitive, surrounding whitespace
    /// ignored). Unknown words → `None`.
    pub fn parse(word: &str) -> Option<Command> {
        match word.trim().to_ascii_lowercase().as_str() {
            "start" => Some(Command::Start),
            "stop" => Some(Command::Stop),
            "toggle" => Some(Command::Toggle),
            "cancel" => Some(Command::Cancel),
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
        }
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
/// it, and write back a status line (`ok <cmd>` / `err ...`).
fn serve_conn<F: Fn(Command)>(stream: UnixStream, dispatch: &F) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let reply = match Command::parse(&line) {
        Some(cmd) => {
            let resolved = resolve(cmd);
            dispatch(resolved);
            format!("ok {}\n", resolved.as_str())
        }
        None => format!("err unknown command {:?}\n", line.trim()),
    };
    let mut stream = reader.into_inner();
    let _ = stream.write_all(reply.as_bytes());
}

/// Send one command to a running daemon and return its reply line (without the
/// trailing newline). Errors if no daemon is listening — the CLI surfaces that
/// as a friendly "is the daemon running?" message.
pub fn send(cmd: Command) -> std::io::Result<String> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path)?;
    stream.write_all(cmd.as_str().as_bytes())?;
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
        ] {
            assert_eq!(Command::parse(c.as_str()), Some(c));
        }
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
        // This is the only test that touches the global recording flag, so it
        // can read it back without racing the socket test (which uses concrete
        // start/cancel commands that never read the flag).
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
        let sock = std::env::temp_dir()
            .join(format!("dictate-control-test-{}.sock", std::process::id()));
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

        std::env::remove_var("DICTATE_CONTROL_SOCK");
        let _ = std::fs::remove_file(&sock);
    }
}
