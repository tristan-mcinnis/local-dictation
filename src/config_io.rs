//! Safe reads and writes for the user's JSON files (`settings.json`,
//! `corrections.json`, `dictionary.json`).
//!
//! Two rules, shared by every writer:
//!   * a write is atomic: temp file in the same directory, fsync, rename, so a
//!     crash or the relaunch that follows a save can never leave a truncated
//!     file;
//!   * a file this build cannot parse is the user's data, not garbage. It is
//!     never silently overwritten: either the writer refuses
//!     ([`refuse_if_unreadable`], for the dictionary files whose content cannot
//!     be rebuilt) or it keeps a `.bak` copy first ([`backup`], for settings).

use std::io::Write;
use std::path::{Path, PathBuf};

/// Write `body` to `path` atomically, keeping the existing file's permissions
/// (a new file is created `0600`: these files hold the user's own words).
///
/// A `path` that is a symlink (dotfile managers link `settings.json` into a
/// repo) is written THROUGH: the temp file and the rename happen beside the
/// link's target, so the link survives. A plain rename onto `path` would
/// replace the link with a regular file and silently detach the user's
/// dotfiles copy.
pub fn write_atomic(path: &Path, body: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    let target = resolve_symlink(path);
    let path = target.as_path();
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(format!(".tmp-{}", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    let result = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(body)?;
        f.sync_all()?;
        if let Ok(meta) = std::fs::metadata(path) {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// The file a write to `path` should land in: the link's target when `path`
/// is a symlink whose target resolves, else `path` itself (a dangling link is
/// replaced, as before).
fn resolve_symlink(path: &Path) -> PathBuf {
    let is_link = std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    if is_link {
        if let Ok(real) = std::fs::canonicalize(path) {
            return real;
        }
    }
    path.to_path_buf()
}

/// Copy `path` to `<name>.bak` beside it (or `<name>.<unix secs>.bak` when a
/// backup already exists, so an older backup is never replaced). Returns the
/// backup path.
pub fn backup(path: &Path) -> std::io::Result<PathBuf> {
    let name = path.file_name().unwrap_or_default().to_os_string();
    let mut first = name.clone();
    first.push(".bak");
    let mut dest = path.with_file_name(first);
    if dest.exists() {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut stamped = name;
        stamped.push(format!(".{secs}.bak"));
        dest = path.with_file_name(stamped);
    }
    std::fs::copy(path, &dest)?;
    Ok(dest)
}

/// Guard for a writer whose file cannot be rebuilt from anything else: when
/// `path` exists, is not blank, and `readable` says this build cannot parse it,
/// return an error instead of letting the caller overwrite it.
pub fn refuse_if_unreadable(
    path: &Path,
    readable: impl FnOnce(&str) -> bool,
) -> eyre::Result<()> {
    let Ok(body) = std::fs::read_to_string(path) else {
        return Ok(()); // missing (or unreadable as UTF-8 is handled by the caller's load)
    };
    if body.trim().is_empty() || readable(&body) {
        return Ok(());
    }
    Err(eyre::eyre!(
        "{} could not be read, so it was left untouched. Fix or remove the file, then try again.",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("the file")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fd-config-io-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn atomic_write_replaces_content_and_leaves_no_temp_file() {
        let dir = scratch("atomic");
        let path = dir.join("nested").join("settings.json");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_keeps_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("perm");
        let path = dir.join("corrections.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        write_atomic(&path, b"{\"a\":\"b\"}").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_goes_through_a_symlink_and_keeps_it() {
        // Regression: rename-over-path replaced a dotfiles symlink with a
        // regular file, so the user's linked copy silently stopped updating.
        let dir = scratch("symlink");
        let real = dir.join("dotfiles").join("settings.json");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "old").unwrap();
        let link = dir.join("settings.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_atomic(&link, b"new").unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("newperm");
        let path = dir.join("corrections.json");
        write_atomic(&path, b"{}").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backup_never_replaces_an_older_backup() {
        let dir = scratch("backup");
        let path = dir.join("settings.json");
        std::fs::write(&path, "first").unwrap();
        let a = backup(&path).unwrap();
        assert_eq!(a, dir.join("settings.json.bak"));
        std::fs::write(&path, "second").unwrap();
        let b = backup(&path).unwrap();
        assert_ne!(a, b);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "first");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "second");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_only_an_existing_unreadable_file() {
        let dir = scratch("refuse");
        let path = dir.join("corrections.json");
        assert!(refuse_if_unreadable(&path, |_| false).is_ok(), "missing");
        std::fs::write(&path, "  \n").unwrap();
        assert!(refuse_if_unreadable(&path, |_| false).is_ok(), "blank");
        std::fs::write(&path, "{ broken").unwrap();
        assert!(refuse_if_unreadable(&path, |_| true).is_ok(), "readable");
        let err = refuse_if_unreadable(&path, |_| false).unwrap_err().to_string();
        assert!(err.contains("corrections.json could not be read"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ broken");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
