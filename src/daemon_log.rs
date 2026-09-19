//! The daemon log file: where it lives, who can read it, how big it may get,
//! and what it is allowed to contain.
//!
//! - Location: `app_paths::daemon_log_path()` →
//!   `~/Library/Logs/Local Dictation/daemon.log` (per user; never `/tmp`).
//! - Permissions: the directory is `0700`, the file `0600`, and the open
//!   refuses to follow a symlink (`O_NOFOLLOW`).
//! - Size: capped at [`MAX_LOG_BYTES`]. A file past the cap is renamed to
//!   `daemon.log.1` (one generation kept) at open time, and again at runtime
//!   through [`rotate_stdio_if_needed`].
//! - Content: dictated text, spoken instructions and harvested on-screen terms
//!   are NOT logged. [`text`] renders them as lengths unless the debug env var
//!   `DICTATE_LOG_TEXT=1` is set. History retention and "Delete all" cannot
//!   reach the log, so text must not land there by default.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// Size cap for `daemon.log`. One rotated generation is kept, so the log
/// never occupies more than about twice this.
pub const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// Debug env var that lets dictated text into the log.
pub const LOG_TEXT_ENV: &str = "DICTATE_LOG_TEXT";

static STDIO_REDIRECTED: AtomicBool = AtomicBool::new(false);

/// True when `DICTATE_LOG_TEXT` is set to a truthy value. Read once.
pub fn text_logging_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| env_truthy(std::env::var(LOG_TEXT_ENV).ok().as_deref()))
}

fn env_truthy(value: Option<&str>) -> bool {
    match value.map(|v| v.trim().to_ascii_lowercase()) {
        None => false,
        Some(v) => !matches!(v.as_str(), "" | "0" | "false" | "off" | "no"),
    }
}

/// Render user text for a log line: the quoted text when text logging is on,
/// otherwise its size only.
pub fn text(s: &str) -> String {
    render_text(text_logging_enabled(), s)
}

/// Render a list of harvested terms for a log line (count only by default).
pub fn terms(list: &[String]) -> String {
    render_terms(text_logging_enabled(), list)
}

fn render_text(enabled: bool, s: &str) -> String {
    if enabled {
        format!("{s:?}")
    } else {
        format!(
            "<{} chars · {} words>",
            s.chars().count(),
            s.split_whitespace().count()
        )
    }
}

fn render_terms(enabled: bool, list: &[String]) -> String {
    if enabled {
        list.join(", ")
    } else {
        format!("<{} terms>", list.len())
    }
}

/// `daemon.log.1` beside `log`.
fn rotated_path(log: &Path) -> PathBuf {
    let mut name = log.file_name().unwrap_or_default().to_os_string();
    name.push(".1");
    log.with_file_name(name)
}

/// Rename `log` to `log.1` when it is larger than `max_bytes`. Returns whether
/// a rotation happened. A missing file is not an error.
pub fn rotate_if_oversize(log: &Path, max_bytes: u64) -> io::Result<bool> {
    let len = match std::fs::symlink_metadata(log) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if len <= max_bytes {
        return Ok(false);
    }
    std::fs::rename(log, rotated_path(log))?;
    Ok(true)
}

/// Open (create + append) the log at `log`: private directory, private file,
/// rotation at the size cap, never through a symlink.
pub fn open_at(log: &Path) -> io::Result<File> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    if let Some(dir) = log.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        // An existing directory keeps whatever mode it had; tighten it.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let _ = rotate_if_oversize(log, MAX_LOG_BYTES);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(log)?;
    // `mode` only applies on creation; tighten a file an older build made.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

/// Open the per-user daemon log. `NotFound` when `$HOME` is unset.
pub fn open() -> io::Result<File> {
    let path = crate::app_paths::daemon_log_path()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is unset"))?;
    open_at(&path)
}

/// Point this process's stdout and stderr at the daemon log. Used by a bundled
/// app, which has no terminal. Returns false (and leaves stdio alone) when the
/// log cannot be opened.
pub fn redirect_stdio() -> bool {
    use std::os::unix::io::AsRawFd;
    match open() {
        Ok(f) => {
            // SAFETY: dup2 onto the process's own stdout/stderr with a valid fd.
            unsafe {
                libc::dup2(f.as_raw_fd(), 1);
                libc::dup2(f.as_raw_fd(), 2);
            }
            // fds 1 and 2 are now independent duplicates; `f` may close.
            STDIO_REDIRECTED.store(true, Ordering::Relaxed);
            true
        }
        Err(_) => false,
    }
}

/// Runtime half of the size cap: when stdio was redirected by
/// [`redirect_stdio`] and the log has passed the cap, rotate it and re-point
/// stdio at a fresh file. One `stat` per call; safe to call per utterance.
pub fn rotate_stdio_if_needed() {
    if !STDIO_REDIRECTED.load(Ordering::Relaxed) {
        return;
    }
    let Some(path) = crate::app_paths::daemon_log_path() else {
        return;
    };
    let oversize = std::fs::symlink_metadata(&path)
        .map(|m| m.len() > MAX_LOG_BYTES)
        .unwrap_or(false);
    if oversize {
        // `open` rotates before it opens.
        redirect_stdio();
        eprintln!("[log] rotated at {} MB", MAX_LOG_BYTES / (1024 * 1024));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fd-log-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn text_is_redacted_unless_enabled() {
        let said = "call Dr. Okafor about the biopsy";
        let off = render_text(false, said);
        assert_eq!(off, "<32 chars · 6 words>");
        assert!(!off.contains("Okafor"));
        assert_eq!(render_text(true, "hi \"you\""), "\"hi \\\"you\\\"\"");
    }

    #[test]
    fn terms_are_counted_unless_enabled() {
        let list = vec!["Okafor".to_string(), "Todoist".to_string()];
        assert_eq!(render_terms(false, &list), "<2 terms>");
        assert_eq!(render_terms(true, &list), "Okafor, Todoist");
    }

    #[test]
    fn env_truthy_parses_the_usual_spellings() {
        assert!(!env_truthy(None));
        for off in ["", "0", "false", "OFF", " no "] {
            assert!(!env_truthy(Some(off)), "{off:?}");
        }
        for on in ["1", "true", "yes", "on"] {
            assert!(env_truthy(Some(on)), "{on:?}");
        }
    }

    #[test]
    fn open_creates_private_dir_and_file() {
        let dir = scratch("perm");
        let log = dir.join("Logs").join("daemon.log");
        let mut f = open_at(&log).expect("open");
        writeln!(f, "[boot] hello").unwrap();
        let file_mode = std::fs::metadata(&log).unwrap().permissions().mode() & 0o777;
        let dir_mode = std::fs::metadata(log.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);
        assert_eq!(dir_mode, 0o700);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_tightens_a_world_readable_file() {
        let dir = scratch("tighten");
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("daemon.log");
        std::fs::write(&log, "old\n").unwrap();
        std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o644)).unwrap();
        open_at(&log).expect("open");
        let mode = std::fs::metadata(&log).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "old\n", "append, not truncate");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_refuses_a_symlink() {
        let dir = scratch("symlink");
        std::fs::create_dir_all(&dir).unwrap();
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, "keep").unwrap();
        let log = dir.join("daemon.log");
        std::os::unix::fs::symlink(&victim, &log).unwrap();
        assert!(open_at(&log).is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotation_keeps_one_generation_and_only_past_the_cap() {
        let dir = scratch("rotate");
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("daemon.log");
        assert!(!rotate_if_oversize(&log, 10).unwrap(), "missing file");
        std::fs::write(&log, "12345").unwrap();
        assert!(!rotate_if_oversize(&log, 10).unwrap(), "under the cap");
        std::fs::write(&log, "0123456789ABCDEF").unwrap();
        assert!(rotate_if_oversize(&log, 10).unwrap());
        assert!(!log.exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("daemon.log.1")).unwrap(),
            "0123456789ABCDEF"
        );
        // A second rotation replaces the single kept generation.
        std::fs::write(&log, "second-generation").unwrap();
        assert!(rotate_if_oversize(&log, 10).unwrap());
        assert_eq!(
            std::fs::read_to_string(dir.join("daemon.log.1")).unwrap(),
            "second-generation"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
