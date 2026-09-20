//! One daemon per user: an advisory `flock` on a lock file.
//!
//! Two live daemons is not a cosmetic problem. Each installs its own HID event
//! tap, so one key press starts two recordings and pastes the text twice; each
//! loads the full model stack; and the second one deletes the first one's
//! control socket (`ipc::serve` clears a stale path before it binds). A
//! stranger reaches that state easily — the app in `~/Downloads` and in
//! `/Applications`, or `fast-dictate-backend daemon` run in a terminal beside
//! the login item.
//!
//! The guard cannot be "exit if anything else holds the lock", because the
//! menu's **Apply and restart** deliberately overlaps two processes:
//! `menubar::relaunch_daemon` spawns the child and only then terminates the
//! parent. So the child *waits*: it retries for [`HANDOFF_WAIT`], which is
//! comfortably longer than the parent's model-release timeout, and gives up
//! only when the old instance is still alive at the end of it. Giving up is
//! the safe outcome — the user keeps the daemon that is already working.
//!
//! The parent shortens that wait by calling [`InstanceLock::release`] from the
//! quit path, right after its models are dropped.
//!
//! `flock` is released by the kernel when the process exits, however it exits,
//! so a crash never leaves a lock behind. The file's *contents* (the holder's
//! pid) are advisory only, for the log line.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a starting daemon waits for a departing one to let go. Must stay
/// longer than `menubar::MODEL_RELEASE_TIMEOUT` (3 s) plus the model teardown,
/// or every "Apply and restart" would end with no daemon at all.
pub const HANDOFF_WAIT: Duration = Duration::from_secs(12);

/// Gap between lock attempts while waiting for a handoff.
const RETRY_EVERY: Duration = Duration::from_millis(100);

/// A held lock. Dropping it (or exiting) releases the lock.
#[derive(Debug)]
pub struct InstanceLock {
    file: Option<File>,
    path: PathBuf,
}

impl InstanceLock {
    /// Let go early, so a replacement daemon that is already waiting can take
    /// over without burning the rest of [`HANDOFF_WAIT`].
    pub fn release(&mut self) {
        if self.file.take().is_some() {
            eprintln!("[lock] released {}", self.path.display());
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// What [`acquire`] found.
#[derive(Debug)]
pub enum Outcome {
    /// This process now owns the lock.
    Held(InstanceLock),
    /// Another daemon held it for the whole wait. `pid` is what that instance
    /// wrote into the file, when it is readable.
    Busy { pid: Option<u32> },
    /// The lock file itself could not be used (unwritable directory, and so
    /// on). Not a reason to refuse to start: the daemon carries on unguarded
    /// rather than leaving the user with no dictation.
    Unavailable { reason: String },
}

/// Take the exclusive daemon lock at `path`, waiting up to `wait` for a
/// departing instance to release it.
pub fn acquire(path: &Path, wait: Duration) -> Outcome {
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            return Outcome::Unavailable {
                reason: format!("{}: {e}", dir.display()),
            };
        }
    }
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
    {
        Ok(f) => f,
        Err(e) => {
            return Outcome::Unavailable {
                reason: format!("{}: {e}", path.display()),
            }
        }
    };

    let deadline = Instant::now() + wait;
    let mut waited = false;
    loop {
        if try_lock(&file) {
            let mut file = file;
            record_pid(&mut file, std::process::id());
            if waited {
                eprintln!("[lock] took over after the previous daemon exited");
            }
            return Outcome::Held(InstanceLock {
                file: Some(file),
                path: path.to_path_buf(),
            });
        }
        if Instant::now() >= deadline {
            return Outcome::Busy {
                pid: read_pid(path),
            };
        }
        if !waited {
            waited = true;
            eprintln!(
                "[lock] another daemon holds {} — waiting up to {} s",
                path.display(),
                wait.as_secs()
            );
        }
        std::thread::sleep(RETRY_EVERY);
    }
}

/// The daemon's lock file: `~/Library/Application Support/Local Dictation/
/// daemon.lock`, falling back to the temp dir when `$HOME` is unset. The
/// config dir is deliberately not used — it is the user's own dotfile
/// directory and it syncs on some Macs.
pub fn default_path() -> PathBuf {
    crate::app_paths::app_support_dir()
        .map(|dir| dir.join("daemon.lock"))
        .unwrap_or_else(|| std::env::temp_dir().join("local-dictation-daemon.lock"))
}

/// Non-blocking `flock(LOCK_EX | LOCK_NB)`. True when this file description
/// now owns the lock.
fn try_lock(file: &File) -> bool {
    // SAFETY: `fd` is owned by `file` and stays open for the call.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// Write the owning pid, for the log line a blocked instance prints. Failure
/// is ignored: the lock, not the file body, is what enforces exclusivity.
fn record_pid(file: &mut File, pid: u32) {
    let _ = file.set_len(0);
    let _ = file.seek(SeekFrom::Start(0));
    let _ = file.write_all(pid.to_string().as_bytes());
    let _ = file.flush();
}

/// Read the pid a live holder wrote. `None` when the file is empty or junk.
fn read_pid(path: &Path) -> Option<u32> {
    let mut text = String::new();
    File::open(path).ok()?.read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "local-dictation-lock-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("daemon.lock")
    }

    #[test]
    fn a_second_daemon_cannot_hold_the_lock_at_the_same_time() {
        let path = scratch("exclusive");
        let first = match acquire(&path, Duration::ZERO) {
            Outcome::Held(lock) => lock,
            other => panic!("the first daemon must get the lock, got {other:?}"),
        };
        assert_eq!(read_pid(&path), Some(std::process::id()));

        // flock is per open file description, so a second `open` in this same
        // process is exactly as blocked as a second process would be.
        match acquire(&path, Duration::from_millis(50)) {
            Outcome::Busy { pid } => assert_eq!(pid, Some(std::process::id())),
            other => panic!("a second daemon must be refused, got {other:?}"),
        }
        drop(first);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn apply_and_restart_hands_the_lock_over_instead_of_killing_the_relaunch() {
        // Regression for the guard that would have broken "Apply and restart":
        // relaunch_daemon spawns the child BEFORE the parent terminates, so the
        // child must wait for the lock rather than exit on the first refusal.
        let path = scratch("handoff");
        let mut parent = match acquire(&path, Duration::ZERO) {
            Outcome::Held(lock) => lock,
            other => panic!("setup: {other:?}"),
        };
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            parent.release();
            drop(parent);
        });
        match acquire(&path, Duration::from_secs(5)) {
            Outcome::Held(lock) => assert_eq!(lock.path(), path.as_path()),
            other => panic!("the relaunched child must get the lock, got {other:?}"),
        }
        releaser.join().unwrap();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_released_lock_leaves_no_stale_holder_behind() {
        let path = scratch("release");
        let mut lock = match acquire(&path, Duration::ZERO) {
            Outcome::Held(lock) => lock,
            other => panic!("setup: {other:?}"),
        };
        lock.release();
        lock.release(); // idempotent
        match acquire(&path, Duration::ZERO) {
            Outcome::Held(_) => {}
            other => panic!("the lock must be free after release, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn an_unusable_lock_path_never_stops_the_daemon_starting() {
        // A lock we cannot take is not a reason to leave the user with no
        // dictation: the daemon runs unguarded and says so.
        let path = scratch("unusable");
        std::fs::create_dir_all(&path).unwrap();
        match acquire(&path, Duration::ZERO) {
            Outcome::Unavailable { reason } => assert!(reason.contains("daemon.lock"), "{reason}"),
            other => panic!("a directory in the lock's place must be Unavailable, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn the_lock_lives_beside_the_app_not_in_the_users_dotfiles() {
        let path = default_path();
        assert!(path.ends_with("daemon.lock"), "{}", path.display());
        if let Some(support) = crate::app_paths::app_support_dir() {
            assert_eq!(path.parent(), Some(support.as_path()));
        }
    }
}
