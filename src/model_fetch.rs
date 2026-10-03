//! The model manifest, the readiness check, and the one download the app can
//! do for itself.
//!
//! **Boundary.** Dictation still never touches the network: the daemon loads
//! from disk and nothing in the pipeline opens a socket. The single exception
//! is this module, and only when the user presses Get model in the Setup
//! window. Nothing here runs unasked.
//!
//! Everything the app is allowed to fetch is named in [`MANIFEST`]: the URL,
//! the exact byte count, the SHA-256, and the licence. A file whose digest
//! does not match the manifest is deleted, never loaded. The ASR model has no
//! URL entry yet: its public 4-bit MLX conversion is several files and this
//! fetcher handles one. `scripts/download-models.sh` fetches it (pinned
//! revision, per-file SHA-256); in the app its row asks for a folder.
//!
//! The download itself is `/usr/bin/curl` driven with an argument array (repo
//! rule: no shell interpolation), resuming into a `.part` file, verified with
//! the in-tree [`crate::sha256`] and moved into place with a single rename, so
//! a half-written file can never be mistaken for a model.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// What a model is for. The cleanup model is optional; the ASR model is not.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Asr,
    Cleanup,
}

/// Where a model may legitimately be fetched from. `None` on an entry means
/// "there is no published artefact to download" — the user supplies it.
#[derive(Clone, Copy, Debug)]
pub struct Source {
    pub url: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
    /// The model's own LICENSE, saved beside the weights: the licence requires
    /// it to travel with the file.
    pub licence_url: &'static str,
    pub notice_url: Option<&'static str>,
}

/// One model in the fixed stack.
#[derive(Clone, Copy, Debug)]
pub struct ModelEntry {
    pub id: &'static str,
    pub role: Role,
    /// Exactly as the licence requires it to be written.
    pub credit: &'static str,
    /// Path under the models base directory.
    pub relative_path: &'static str,
    /// Files that must exist inside `relative_path` when it is a directory.
    /// Empty when the entry is itself a file.
    pub required_files: &'static [&'static str],
    pub licence: &'static str,
    pub licence_link: &'static str,
    pub source: Option<Source>,
    /// What the Setup window tells a user who has to supply this themselves.
    pub manual_hint: &'static str,
}

impl ModelEntry {
    /// Where this model belongs under `base`.
    pub fn path_in(&self, base: &Path) -> PathBuf {
        base.join(self.relative_path)
    }

    /// Is this model usable at `path`? A directory entry needs every one of
    /// `required_files` to be non-empty; a file entry needs itself.
    pub fn present_at(&self, path: &Path) -> bool {
        if self.required_files.is_empty() {
            return non_empty_file(path);
        }
        self.required_files
            .iter()
            .all(|name| non_empty_file(&path.join(name)))
    }

    /// Can the app get this one by itself?
    pub fn is_fetchable(&self) -> bool {
        self.source.is_some()
    }
}

fn non_empty_file(path: &Path) -> bool {
    path.metadata().map(|m| m.len() > 0).unwrap_or(false)
}

/// Speech model. Fixed by the repo contract: Cohere Transcribe Q4 on the
/// public MLX Swift runtime. The public 4-bit conversion
/// (beshkenadze/cohere-transcribe-03-2026-mlx-4bit) is several files, and
/// `Source` describes one, so there is no URL here yet; download-models.sh
/// fetches it with the same pinning and digest rules.
pub const ASR: ModelEntry = ModelEntry {
    id: "asr",
    role: Role::Asr,
    credit: "Cohere Transcribe 03-2026 by Cohere Labs (Apache-2.0)",
    relative_path: "dictation/cohere-mlx-q4",
    required_files: &["model.safetensors", "config.json"],
    licence: "Apache-2.0",
    licence_link: "https://huggingface.co/CohereLabs/cohere-transcribe-03-2026",
    source: None,
    manual_hint: "The app cannot download this model yet. Run \
scripts/download-models.sh from the source tree (it fetches a pinned public 4-bit MLX \
build), or choose a folder that holds model.safetensors and config.json.",
};

/// Cleanup model. Open weights with a published digest, so the app can get it.
pub const CLEANUP: ModelEntry = ModelEntry {
    id: "cleanup",
    role: Role::Cleanup,
    credit: "\"S1-mini\" by \"Superwhisper\" (Apache-2.0)",
    relative_path: "llm/s1-mini/s1-mini.gguf",
    required_files: &[],
    licence: "Apache-2.0 with a naming clause",
    licence_link: "https://huggingface.co/superwhisper/s1-mini-GGUF",
    source: Some(Source {
        url: "https://huggingface.co/superwhisper/s1-mini-GGUF/resolve/main/s1-mini-q4_k_m.gguf",
        bytes: 484_219_808,
        sha256: "3b41ebe2502cbd03e811d5d16b022f5ab551eda58d62597d152f89535003c634",
        licence_url: "https://huggingface.co/superwhisper/s1-mini-GGUF/raw/main/LICENSE",
        notice_url: Some("https://huggingface.co/superwhisper/s1-mini-GGUF/raw/main/NOTICE"),
    }),
    manual_hint: "Cleanup polishes the transcript. Dictation works without it: turn \
Cleanup off in Settings and the raw transcript is used.",
};

/// Every model the app knows about.
pub const MANIFEST: &[ModelEntry] = &[ASR, CLEANUP];

// ─── Readiness ──────────────────────────────────────────────────────────

/// One manifest row plus where it lives and whether it is there.
#[derive(Clone, Debug)]
pub struct ModelStatus {
    pub entry: ModelEntry,
    pub path: PathBuf,
    pub present: bool,
    /// False when this model is not needed in the current configuration —
    /// the cleanup model with cleanup switched off.
    pub required: bool,
}

impl ModelStatus {
    /// The only case that stops dictation working.
    pub fn is_blocking(&self) -> bool {
        self.required && !self.present
    }
}

/// Status of the whole stack.
///
/// `asr_path` and `llm_path` are the *resolved* model locations, not the
/// manifest defaults. Both have documented env overrides
/// (`DICTATE_COHERE_MLX_MODEL_DIR`, `DICTATE_LLM_MODEL_PATH`) and the cleanup
/// model also has a settings key the picker writes, so checking the manifest
/// default instead reported a missing model on a Mac where the daemon loads
/// one perfectly well. `None` means "use the manifest default under `base`",
/// which is what `fetch-models <dir>` wants.
/// `cleanup_enabled` false makes the cleanup row optional (CP-11): a missing
/// LLM alone must never dead-end the app.
pub fn stack_status(
    base: &Path,
    asr_path: Option<&Path>,
    llm_path: Option<&Path>,
    cleanup_enabled: bool,
) -> Vec<ModelStatus> {
    MANIFEST
        .iter()
        .map(|entry| {
            let path = match (entry.role, asr_path, llm_path) {
                (Role::Asr, Some(p), _) => p.to_path_buf(),
                (Role::Cleanup, _, Some(p)) => p.to_path_buf(),
                _ => entry.path_in(base),
            };
            let present = entry.present_at(&path);
            let required = match entry.role {
                Role::Asr => true,
                Role::Cleanup => cleanup_enabled,
            };
            ModelStatus {
                entry: *entry,
                path,
                present,
                required,
            }
        })
        .collect()
}

/// True when every model the current configuration needs is on disk.
pub fn models_present(
    base: &Path,
    asr_path: Option<&Path>,
    llm_path: Option<&Path>,
    cleanup_enabled: bool,
) -> bool {
    !stack_status(base, asr_path, llm_path, cleanup_enabled)
        .iter()
        .any(ModelStatus::is_blocking)
}

/// The stack as *this* Mac resolves it: the same paths the daemon loads from,
/// including every documented override. The one place that resolution lives —
/// the boot check, the Setup window and the Models action all read it, so they
/// can never disagree about whether a model is there.
pub fn resolved_status() -> Vec<ModelStatus> {
    let settings = crate::settings::Settings::load();
    let cleanup_enabled = settings.resolve_cleanup_enabled(false);
    let asr = PathBuf::from(crate::app_paths::cohere_mlx_default_dir());
    let llm = PathBuf::from(settings.resolve_llm_model(&crate::app_paths::llm_default_path()));
    stack_status(
        &crate::app_paths::models_base_dir(),
        Some(&asr),
        Some(&llm),
        cleanup_enabled,
    )
}

/// Boot check. Raises a health fault rather than only writing a log line
/// nobody reads, so the menu bar and the Setup window can say what is missing.
///
/// The two models are *not* the same kind of problem, and were reported as one
/// until this split: a missing speech model stops dictation
/// ([`crate::health::Fault::ModelsMissing`], which blocks a press), while a
/// missing cleanup model only costs the polish
/// ([`crate::health::Fault::CleanupUnavailable`], which does not) — exactly
/// what the worker does when it loads them (CP-11). Raising the blocking fault
/// for a missing GGUF left a fresh install with the speech model but no
/// cleanup model unable to dictate at all.
///
/// `CleanupUnavailable` is raised here but never cleared: once the worker has
/// tried to load the file it owns that verdict, and a model that has landed on
/// disk since boot is still not in memory until a relaunch.
pub fn ensure_models_on_first_run() {
    let statuses = resolved_status();
    let blocking: Vec<&ModelStatus> = statuses.iter().filter(|s| s.is_blocking()).collect();
    let describe = |rows: &[&ModelStatus]| {
        rows.iter()
            .map(|s| format!("{} ({})", s.entry.id, s.path.display()))
            .collect::<Vec<_>>()
            .join(", ")
    };

    let asr: Vec<&ModelStatus> = blocking
        .iter()
        .copied()
        .filter(|s| s.entry.role == Role::Asr)
        .collect();
    if asr.is_empty() {
        crate::health::clear(crate::health::Fault::ModelsMissing);
    } else {
        crate::health::raise(
            crate::health::Fault::ModelsMissing,
            format!("not on disk: {}", describe(&asr)),
        );
    }

    let cleanup: Vec<&ModelStatus> = blocking
        .iter()
        .copied()
        .filter(|s| s.entry.role == Role::Cleanup)
        .collect();
    if !cleanup.is_empty() {
        crate::health::raise(
            crate::health::Fault::CleanupUnavailable,
            format!("not on disk: {}", describe(&cleanup)),
        );
    }
}

// ─── Download ───────────────────────────────────────────────────────────

/// Where a fetch has got to. Cloned into the process-wide slot the Setup
/// window polls.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Phase {
    /// Checking free disk space.
    Preflight,
    Downloading { done: u64, total: u64 },
    Verifying { done: u64, total: u64 },
    Done,
    Cancelled,
    Failed(String),
}

impl Phase {
    /// One line for the Models row.
    pub fn label(&self) -> String {
        match self {
            Phase::Preflight => "Checking disk space…".to_string(),
            Phase::Downloading { done, total } => {
                format!("Downloading… {}", percent(*done, *total))
            }
            Phase::Verifying { done, total } => format!("Checking… {}", percent(*done, *total)),
            Phase::Done => "Ready".to_string(),
            Phase::Cancelled => "Cancelled".to_string(),
            Phase::Failed(why) => format!("Failed · {why}"),
        }
    }

    /// True while work is still going on.
    pub fn is_running(&self) -> bool {
        matches!(
            self,
            Phase::Preflight | Phase::Downloading { .. } | Phase::Verifying { .. }
        )
    }
}

fn percent(done: u64, total: u64) -> String {
    if total == 0 {
        return format!("{} MB", done / 1_000_000);
    }
    format!("{}%", (done.min(total) * 100 / total))
}

/// Headroom over the model's own size, so a download never fills the disk.
const DISK_HEADROOM: u64 = 200 * 1_000_000;

/// Bytes free on the volume holding `path` (or its nearest existing parent).
pub fn free_disk_bytes(path: &Path) -> Option<u64> {
    let mut probe = path.to_path_buf();
    while !probe.exists() {
        probe = probe.parent()?.to_path_buf();
    }
    let c_path = std::ffi::CString::new(probe.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: `statvfs` fills a plain POD struct; the path is NUL-terminated.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

/// Do we have room for `needed` bytes at `dest`, given `already` bytes of a
/// resumable part file?
pub fn has_room(free: Option<u64>, needed: u64, already: u64) -> Result<(), String> {
    let Some(free) = free else {
        // Unknown free space is not a reason to refuse; curl will fail
        // honestly if the disk fills.
        return Ok(());
    };
    let want = needed.saturating_sub(already).saturating_add(DISK_HEADROOM);
    if free >= want {
        return Ok(());
    }
    Err(format!(
        "needs {} MB free, this disk has {} MB",
        want / 1_000_000,
        free / 1_000_000
    ))
}

/// The live fetch, for the Setup window to read and to cancel.
pub struct FetchState {
    phase: Mutex<Option<Phase>>,
    cancel: Mutex<Option<Arc<AtomicBool>>>,
}

static FETCH: FetchState = FetchState {
    phase: Mutex::new(None),
    cancel: Mutex::new(None),
};

/// What the running (or last) fetch is doing.
pub fn fetch_phase() -> Option<Phase> {
    FETCH.phase.lock().ok().and_then(|p| p.clone())
}

/// Ask the running fetch to stop. The part file is kept, so pressing Get model
/// again resumes rather than starting over.
pub fn cancel_fetch() {
    if let Ok(flag) = FETCH.cancel.lock() {
        if let Some(flag) = flag.as_ref() {
            flag.store(true, Ordering::SeqCst);
        }
    }
}

/// True while a fetch is running.
pub fn fetch_running() -> bool {
    fetch_phase().is_some_and(|p| p.is_running())
}

/// Start fetching `entry` into the models base directory on a background
/// thread. A no-op when one is already running or the entry has no source.
/// Returns false when nothing was started.
pub fn start_fetch(entry: ModelEntry, base: PathBuf) -> bool {
    if entry.source.is_none() || fetch_running() {
        return false;
    }
    let cancel = Arc::new(AtomicBool::new(false));
    if let Ok(mut slot) = FETCH.cancel.lock() {
        *slot = Some(cancel.clone());
    }
    set_phase(Phase::Preflight);
    std::thread::spawn(move || {
        let dest = entry.path_in(&base);
        let result = fetch(&entry, &dest, &cancel, &set_phase);
        match result {
            Ok(()) => set_phase(Phase::Done),
            Err(e) if cancel.load(Ordering::SeqCst) => {
                eprintln!("[models] fetch cancelled: {e}");
                set_phase(Phase::Cancelled);
            }
            Err(e) => {
                eprintln!("[models] fetch failed: {e}");
                set_phase(Phase::Failed(e.to_string()));
            }
        }
        ensure_models_on_first_run();
    });
    true
}

fn set_phase(phase: Phase) {
    if let Ok(mut slot) = FETCH.phase.lock() {
        *slot = Some(phase);
    }
}

/// Download `entry` to `dest`, resuming any `.part` beside it, verifying the
/// digest before the file is moved into place.
///
/// Public so the tests can drive it against a loopback server; the app calls
/// [`start_fetch`].
pub fn fetch(
    entry: &ModelEntry,
    dest: &Path,
    cancel: &AtomicBool,
    report: &dyn Fn(Phase),
) -> eyre::Result<()> {
    let source = entry
        .source
        .ok_or_else(|| eyre::eyre!("{} cannot be downloaded: {}", entry.id, entry.manual_hint))?;

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Already correct on disk? Then this is free.
    if entry.present_at(dest) && digest_matches(dest, source.sha256, cancel, &report, source.bytes)?
    {
        return Ok(());
    }

    let part = part_path(dest);
    report(Phase::Preflight);
    let already = part.metadata().map(|m| m.len()).unwrap_or(0);
    if already == 0 {
        // curl declines to resume a zero-length file on some transports.
        let _ = std::fs::remove_file(&part);
    }
    has_room(free_disk_bytes(&part), source.bytes, already).map_err(|e| eyre::eyre!("{e}"))?;

    report(Phase::Downloading {
        done: already,
        total: source.bytes,
    });
    run_curl(source.url, &part, source.bytes, cancel, &report)?;

    let downloaded = part.metadata()?.len();
    if downloaded != source.bytes {
        let _ = std::fs::remove_file(&part);
        return Err(eyre::eyre!(
            "{} is {downloaded} bytes, the manifest says {}",
            entry.id,
            source.bytes
        ));
    }
    if !digest_matches(&part, source.sha256, cancel, &report, source.bytes)? {
        // A file that is not what the manifest names is never kept, and never
        // loaded: it is deleted so the next attempt starts clean.
        let _ = std::fs::remove_file(&part);
        if cancel.load(Ordering::SeqCst) {
            return Err(eyre::eyre!("cancelled"));
        }
        return Err(eyre::eyre!("{} failed its checksum", entry.id));
    }
    std::fs::rename(&part, dest)?;
    write_licence_beside(entry, &source, dest);
    eprintln!(
        "[models] {} ready at {} · {}",
        entry.id,
        dest.display(),
        entry.credit
    );
    Ok(())
}

/// `<dest>.part` — the resumable download.
pub fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.as_os_str().to_os_string();
    name.push(".part");
    PathBuf::from(name)
}

fn digest_matches(
    path: &Path,
    expected: &str,
    cancel: &AtomicBool,
    report: &dyn Fn(Phase),
    total: u64,
) -> eyre::Result<bool> {
    let digest = crate::sha256::hash_file(path, |done| {
        report(Phase::Verifying { done, total });
        !cancel.load(Ordering::SeqCst)
    })?;
    Ok(digest.as_deref() == Some(expected))
}

/// Attribution is a licence obligation: the model's own LICENSE and NOTICE are
/// fetched beside the weights, exactly as `scripts/download-models.sh` does.
/// A failure here is logged, never fatal — the weights are already verified.
fn write_licence_beside(entry: &ModelEntry, source: &Source, dest: &Path) {
    let Some(dir) = dest.parent() else { return };
    let stem = dest
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(entry.id);
    for (url, suffix) in [
        (Some(source.licence_url), "LICENSE.txt"),
        (source.notice_url, "NOTICE.txt"),
    ] {
        let Some(url) = url else { continue };
        let out = dir.join(format!("{stem}-{suffix}"));
        let status = std::process::Command::new("/usr/bin/curl")
            .args(["-fsL", "--retry", "2", "-o"])
            .arg(&out)
            .arg(url)
            .status();
        match status {
            Ok(s) if s.success() => {}
            other => eprintln!("[models] could not save {} ({other:?})", out.display()),
        }
    }
}

/// Run `curl` into `part`, polling its size for progress and killing it on
/// cancel. Argument array, no shell.
fn run_curl(
    url: &str,
    part: &Path,
    total: u64,
    cancel: &AtomicBool,
    report: &dyn Fn(Phase),
) -> eyre::Result<()> {
    let mut child = std::process::Command::new("/usr/bin/curl")
        .args([
            "-fL",             // fail on HTTP errors, follow redirects
            "--retry", "3",    // transient network errors
            "-C", "-",         // resume from whatever is already in the part file
            "--silent",
            "--show-error",
            "-o",
        ])
        .arg(part)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| eyre::eyre!("could not run curl: {e}"))?;

    let stderr = child.stderr.take();
    loop {
        if cancel.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(eyre::eyre!("cancelled"));
        }
        match child.try_wait()? {
            Some(status) => {
                if status.success() {
                    return Ok(());
                }
                let mut why = String::new();
                if let Some(mut stderr) = stderr {
                    use std::io::Read;
                    let _ = stderr.read_to_string(&mut why);
                }
                let why = why.trim();
                return Err(eyre::eyre!(
                    "curl exited with {status}{}",
                    if why.is_empty() {
                        String::new()
                    } else {
                        format!(": {why}")
                    }
                ));
            }
            None => {
                let done = part.metadata().map(|m| m.len()).unwrap_or(0);
                report(Phase::Downloading { done, total });
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "local-dictation-models-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn digest(bytes: &[u8]) -> String {
        let mut h = crate::sha256::Sha256::new();
        h.update(bytes);
        h.finish_hex()
    }

    /// A one-shot HTTP/1.1 server on the loopback interface that understands
    /// `Range: bytes=N-`. Real sockets, no internet: the download path is
    /// proved against a server, not a mock. The join handle yields the byte
    /// offset the client actually asked for, so a test can prove that resume
    /// resumed instead of quietly starting over.
    fn serve_once(body: &'static [u8]) -> (String, std::thread::JoinHandle<usize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/model.bin", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if stream.read(&mut byte).unwrap_or(0) == 0 {
                    return 0;
                }
                head.push(byte[0]);
            }
            let request = String::from_utf8_lossy(&head).to_lowercase();
            let start = request
                .split("range: bytes=")
                .nth(1)
                .and_then(|rest| rest.split('-').next())
                .and_then(|n| n.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let slice = &body[start.min(body.len())..];
            let header = if start > 0 {
                format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                    slice.len(),
                    start,
                    body.len().saturating_sub(1),
                    body.len()
                )
            } else {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                    slice.len()
                )
            };
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(slice);
            let _ = stream.flush();
            start
        });
        (url, handle)
    }

    const BODY: &[u8] = b"local dictation cleanup model bytes, pretend this is a GGUF";

    fn entry_for(url: &'static str) -> ModelEntry {
        // `Source` is a compile-time struct, so the test leaks the two strings
        // it needs rather than widening the type to owned data.
        ModelEntry {
            id: "test",
            role: Role::Cleanup,
            credit: "test model",
            relative_path: "llm/test/test.gguf",
            required_files: &[],
            licence: "Apache-2.0",
            licence_link: "https://example.invalid",
            source: Some(Source {
                url,
                bytes: BODY.len() as u64,
                sha256: Box::leak(digest(BODY).into_boxed_str()),
                // Deliberately unreachable: a licence fetch that fails must not
                // fail the model fetch.
                licence_url: "http://127.0.0.1:1/LICENSE",
                notice_url: None,
            }),
            manual_hint: "",
        }
    }

    #[test]
    fn a_verified_download_lands_atomically_with_its_licence_attempted() {
        let dir = scratch("download");
        let (url, server) = serve_once(BODY);
        let entry = entry_for(Box::leak(url.into_boxed_str()));
        let dest = dir.join("test.gguf");
        let cancel = AtomicBool::new(false);
        let seen = Mutex::new(Vec::new());
        fetch(&entry, &dest, &cancel, &|p| seen.lock().unwrap().push(p)).unwrap();
        assert_eq!(server.join().unwrap(), 0, "a fresh download starts at byte 0");

        assert_eq!(std::fs::read(&dest).unwrap(), BODY);
        assert!(!part_path(&dest).exists(), "the part file must be gone");
        let phases = seen.lock().unwrap().clone();
        assert!(phases.iter().any(|p| matches!(p, Phase::Preflight)));
        assert!(phases.iter().any(|p| matches!(p, Phase::Verifying { .. })));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_interrupted_download_resumes_from_the_part_file() {
        // Regression for the whole point of `-C -`: a 462 MB model must not
        // restart from zero because a connection dropped.
        let dir = scratch("resume");
        let dest = dir.join("test.gguf");
        let part = part_path(&dest);
        let half = BODY.len() / 2;
        std::fs::write(&part, &BODY[..half]).unwrap();

        let (url, server) = serve_once(BODY);
        let entry = entry_for(Box::leak(url.into_boxed_str()));
        let cancel = AtomicBool::new(false);
        fetch(&entry, &dest, &cancel, &|_| {}).unwrap();
        assert_eq!(
            server.join().unwrap(),
            half,
            "curl must ask for the remaining bytes, not the whole file again"
        );
        assert_eq!(std::fs::read(&dest).unwrap(), BODY);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_fails_its_checksum_is_deleted_and_never_moved_into_place() {
        let dir = scratch("bad-digest");
        let (url, server) = serve_once(b"these are not the bytes the manifest names");
        let entry = entry_for(Box::leak(url.into_boxed_str()));
        let dest = dir.join("test.gguf");
        let cancel = AtomicBool::new(false);
        let err = fetch(&entry, &dest, &cancel, &|_| {}).unwrap_err().to_string();
        let _ = server.join();
        assert!(err.contains("bytes") || err.contains("checksum"), "{err}");
        assert!(!dest.exists(), "a model that failed verification must not exist");
        assert!(!part_path(&dest).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_entry_with_no_source_says_what_to_do_instead_of_inventing_a_url() {
        assert!(!ASR.is_fetchable());
        assert!(ASR.source.is_none(), "never invent an ASR download URL");
        assert!(!ASR.manual_hint.is_empty());
        let dir = scratch("no-source");
        let err = fetch(&ASR, &dir.join("asr"), &AtomicBool::new(false), &|_| {})
            .unwrap_err()
            .to_string();
        assert!(err.contains(ASR.manual_hint), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cancelled_fetch_keeps_the_part_file_so_the_next_try_resumes() {
        let dir = scratch("cancel");
        let dest = dir.join("test.gguf");
        std::fs::write(part_path(&dest), &BODY[..4]).unwrap();
        // Nothing listens on this port, so curl is still the one that fails —
        // but the cancel flag is what the caller sees first.
        let entry = entry_for("http://127.0.0.1:1/model.bin");
        let cancel = AtomicBool::new(true);
        assert!(fetch(&entry, &dest, &cancel, &|_| {}).is_err());
        assert!(part_path(&dest).exists(), "a cancel must keep the resume point");
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_cleanup_model_is_only_required_when_cleanup_is_on() {
        // CP-11: a missing LLM alone must not dead-end the app.
        let base = scratch("stack");
        let asr = ASR.path_in(&base);
        std::fs::create_dir_all(&asr).unwrap();
        for name in ASR.required_files {
            std::fs::write(asr.join(name), b"x").unwrap();
        }
        let llm = base.join("llm/chosen/other.gguf");

        assert!(
            models_present(&base, None, Some(&llm), false),
            "cleanup off ⇒ a missing cleanup model is not a problem"
        );
        assert!(!models_present(&base, None, Some(&llm), true));

        // And the check follows the RESOLVED path, not the manifest default:
        // the picker saves an absolute path to whatever GGUF the user chose.
        std::fs::create_dir_all(llm.parent().unwrap()).unwrap();
        std::fs::write(&llm, b"gguf").unwrap();
        assert!(models_present(&base, None, Some(&llm), true));
        assert!(
            !models_present(&base, None, Some(&base.join("llm/missing.gguf")), true),
            "a chosen model that is gone must still be reported"
        );

        // The ASR model is never optional.
        std::fs::remove_file(asr.join("model.safetensors")).unwrap();
        assert!(!models_present(&base, None, Some(&llm), false));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Regression: the speech model has a documented env override
    /// (`DICTATE_COHERE_MLX_MODEL_DIR`) that `app_paths::cohere_mlx_default_dir`
    /// honours and the readiness check did not, so a Mac whose daemon loads a
    /// checkpoint from elsewhere was told its speech model was missing — and
    /// `ModelsMissing` blocks every hotkey press.
    #[test]
    fn the_speech_model_is_found_where_the_daemon_actually_loads_it() {
        let base = scratch("asr-override");
        let elsewhere = base.join("somewhere/else/cohere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        for name in ASR.required_files {
            std::fs::write(elsewhere.join(name), b"x").unwrap();
        }
        // Nothing at the manifest default…
        assert!(!models_present(&base, None, None, false));
        // …but the resolved path is where the model really is.
        assert!(models_present(&base, Some(&elsewhere), None, false));
        let rows = stack_status(&base, Some(&elsewhere), None, false);
        let asr = rows.iter().find(|s| s.entry.role == Role::Asr).unwrap();
        assert_eq!(asr.path, elsewhere);
        assert!(asr.present);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Regression (CP-11): a missing cleanup model used to raise the blocking
    /// `ModelsMissing` fault, so a fresh install that had the speech model but
    /// not the GGUF could not dictate at all — even though the worker treats
    /// the cleanup model as optional and falls back to the raw transcript.
    #[test]
    fn a_missing_cleanup_model_is_a_degradation_not_a_dead_hotkey() {
        use crate::health::Fault;
        let _serial = crate::health::test_lock();
        for fault in [Fault::ModelsMissing, Fault::CleanupUnavailable] {
            crate::health::clear(fault);
        }

        let base = scratch("fault-split");
        let asr = ASR.path_in(&base);
        std::fs::create_dir_all(&asr).unwrap();
        for name in ASR.required_files {
            std::fs::write(asr.join(name), b"x").unwrap();
        }
        let llm = CLEANUP.path_in(&base);

        // Present speech model, missing cleanup model, cleanup on.
        let rows = stack_status(&base, Some(&asr), Some(&llm), true);
        let blocking: Vec<&ModelStatus> = rows.iter().filter(|s| s.is_blocking()).collect();
        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].entry.role, Role::Cleanup);
        // Which fault that becomes is the whole point: the cleanup one, which
        // `blocks_dictation` says false to.
        assert!(!Fault::CleanupUnavailable.blocks_dictation());
        assert!(Fault::ModelsMissing.blocks_dictation());

        for fault in [Fault::ModelsMissing, Fault::CleanupUnavailable] {
            crate::health::clear(fault);
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_half_written_model_directory_does_not_count_as_present() {
        let base = scratch("partial");
        let asr = ASR.path_in(&base);
        std::fs::create_dir_all(&asr).unwrap();
        std::fs::write(asr.join("config.json"), b"{}").unwrap();
        assert!(!ASR.present_at(&asr), "config.json alone is not a model");
        std::fs::write(asr.join("model.safetensors"), b"").unwrap();
        assert!(!ASR.present_at(&asr), "a zero-byte weight file is not a model");
        std::fs::write(asr.join("model.safetensors"), b"w").unwrap();
        assert!(ASR.present_at(&asr));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_disk_preflight_counts_what_is_left_to_fetch_not_the_whole_file() {
        assert!(has_room(Some(u64::MAX), 1_000, 0).is_ok());
        assert!(has_room(None, u64::MAX, 0).is_ok(), "unknown free space must not block");
        // 400 MB left of a 500 MB file, and only 100 MB free: refused.
        assert!(has_room(Some(100 * 1_000_000), 500 * 1_000_000, 100 * 1_000_000).is_err());
        // The same file with 380 MB already downloaded fits in 400 MB.
        assert!(has_room(Some(400 * 1_000_000), 500 * 1_000_000, 380 * 1_000_000).is_ok());
    }

    #[test]
    fn the_manifest_names_a_licence_and_a_credit_for_every_model() {
        for entry in MANIFEST {
            assert!(!entry.credit.is_empty(), "{}", entry.id);
            assert!(!entry.licence.is_empty(), "{}", entry.id);
            assert!(entry.licence_link.starts_with("https://"), "{}", entry.id);
            if let Some(source) = entry.source {
                assert!(source.url.starts_with("https://"), "{}", entry.id);
                assert_eq!(source.sha256.len(), 64, "{}", entry.id);
                assert!(source.bytes > 0, "{}", entry.id);
                assert!(source.licence_url.starts_with("https://"), "{}", entry.id);
            }
        }
        // The naming clause: this exact capitalisation, wherever it appears.
        assert!(CLEANUP.credit.contains("\"S1-mini\" by \"Superwhisper\""));
        // Matches scripts/download-models.sh, which is the verified source.
        let source = CLEANUP.source.unwrap();
        assert_eq!(source.bytes, 484_219_808);
        assert_eq!(
            source.sha256,
            "3b41ebe2502cbd03e811d5d16b022f5ab551eda58d62597d152f89535003c634"
        );
    }

    #[test]
    fn progress_labels_read_as_plain_english() {
        assert_eq!(
            Phase::Downloading { done: 50, total: 100 }.label(),
            "Downloading… 50%"
        );
        assert_eq!(Phase::Verifying { done: 1, total: 4 }.label(), "Checking… 25%");
        assert_eq!(Phase::Done.label(), "Ready");
        assert!(Phase::Failed("no disk".into()).label().contains("no disk"));
        assert!(Phase::Preflight.is_running());
        assert!(!Phase::Done.is_running());
        assert!(!Phase::Cancelled.is_running());
        // A total of zero must not divide by zero.
        assert_eq!(Phase::Downloading { done: 3_000_000, total: 0 }.label(), "Downloading… 3 MB");
    }
}
