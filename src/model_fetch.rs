//! First-run model fetch for the **slim** shipped app.
//!
//! A slim `.app` (built with `build-app.sh --slim`) carries no models, so the
//! `.dmg` is a few MB instead of ~1.7 GB. On first boot the daemon downloads
//! the two defaults — Parakeet TDT v3 (ASR) + Qwen 2.5 1.5B (cleanup) — into the
//! writable Application Support models dir, where [`crate::app_paths`] resolution
//! then picks them up (Resources/models is absent in a slim bundle, so the base
//! resolves to `~/Library/Application Support/Local Dictation/models`).
//!
//! Mirrors `scripts/download-models.sh` exactly (same URLs + on-disk layout) and
//! shells out to the system `curl`, so there's no new runtime HTTP dependency.
//! Idempotent: a file already present with non-zero size is skipped, so a
//! partial/interrupted first run resumes cleanly on the next launch.

use std::path::{Path, PathBuf};
use std::process::Command;

const PARAKEET_REL: &str = "dictation/parakeet-tdt-v3-int8";
const LLM_REL: &str = "llm/qwen-2.5-1.5b-it";

/// Hugging Face origin for the model downloads. Defaults to huggingface.co, but
/// honours `HF_ENDPOINT` (the same env var the official `huggingface_hub`
/// respects) so users where huggingface.co is blocked/slow — notably mainland
/// China — can point at a mirror, e.g. `HF_ENDPOINT=https://hf-mirror.com`,
/// instead of needing a VPN.
fn hf_endpoint() -> String {
    std::env::var("HF_ENDPOINT")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://huggingface.co".to_string())
}

/// The default model files, as `(path relative to the models base, source URL)`.
/// Kept in lockstep with `scripts/download-models.sh`.
fn manifest() -> Vec<(PathBuf, String)> {
    let hf = hf_endpoint();
    let parakeet_base = format!("{hf}/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/main");
    let llm_base = format!("{hf}/Qwen/Qwen2.5-1.5B-Instruct-GGUF/resolve/main");
    let pk = PathBuf::from(PARAKEET_REL);
    let llm = PathBuf::from(LLM_REL);
    vec![
        (
            pk.join("encoder-model.int8.onnx"),
            format!("{parakeet_base}/encoder-model.int8.onnx"),
        ),
        (
            pk.join("decoder_joint-model.int8.onnx"),
            format!("{parakeet_base}/decoder_joint-model.int8.onnx"),
        ),
        (pk.join("vocab.txt"), format!("{parakeet_base}/vocab.txt")),
        (
            llm.join("qwen2.5-1.5b-instruct-q4_k_m.gguf"),
            format!("{llm_base}/qwen2.5-1.5b-instruct-q4_k_m.gguf"),
        ),
    ]
}

/// A path exists with non-zero size (a partial 0-byte file counts as missing).
fn present(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false)
}

/// True when every default model file is already present under `base`.
pub fn models_present(base: &Path) -> bool {
    manifest().iter().all(|(rel, _)| present(&base.join(rel)))
}

/// Best-effort macOS user notification (the slim app has no terminal, so this is
/// the only first-run progress signal a double-click user sees). Never fails the
/// caller.
fn notify(body: &str) {
    let body = body.replace('"', "'");
    let _ = Command::new("osascript")
        .args([
            "-e",
            &format!("display notification \"{body}\" with title \"Local Dictation\""),
        ])
        .status();
}

/// Download any missing default model files into `base`. Returns `Ok(true)` if
/// anything was fetched, `Ok(false)` if everything was already present. Each file
/// lands via a `.partial` sidecar renamed on success, so an interrupted download
/// never leaves a truncated model in place.
pub fn ensure_default_models(base: &Path) -> eyre::Result<bool> {
    let missing: Vec<(PathBuf, String)> = manifest()
        .into_iter()
        .filter(|(rel, _)| !present(&base.join(rel)))
        .collect();
    if missing.is_empty() {
        return Ok(false);
    }

    eprintln!(
        "[models] {} default model file(s) missing — downloading into {} (~1.7 GB, first run only)",
        missing.len(),
        base.display()
    );
    notify("Downloading models (~1.7 GB) — first run only…");

    for (rel, url) in &missing {
        let out = base.join(rel);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Append (not replace) the suffix so multi-dot names keep their real
        // extension: encoder-model.int8.onnx → encoder-model.int8.onnx.partial.
        let partial = PathBuf::from(format!("{}.partial", out.display()));
        eprintln!("[models] ↓ {}", rel.display());
        let status = Command::new("curl")
            .args([
                "-L",
                "--fail",
                "--retry",
                "3",
                "--retry-delay",
                "2",
                "--remote-time",
                "-o",
            ])
            .arg(&partial)
            .arg(url)
            .status()
            .map_err(|e| eyre::eyre!("could not run curl: {e}"))?;
        if !status.success() {
            let _ = std::fs::remove_file(&partial);
            return Err(eyre::eyre!(
                "download failed for {url} (curl exit {:?})",
                status.code()
            ));
        }
        std::fs::rename(&partial, &out)?;
    }

    eprintln!("[models] ✓ all default models present");
    notify("Models ready — dictation is live.");
    Ok(true)
}

/// First-run hook called at daemon boot. Only a **shipped bundle** with no
/// resolvable models triggers a download (a dev/CLI run uses `./models`, an
/// explicit `DICTATE_MODELS_DIR`, or a bundled/`--bundle-models` app, all of
/// which already have the files). Failures are logged + surfaced via
/// notification but never abort boot — the existing permission/model checks
/// downstream give the precise error.
pub fn ensure_models_on_first_run() {
    if !crate::app_paths::running_in_bundle() {
        return; // dev / CLI: ./models or env override owns model placement.
    }
    // If the normal resolution already finds a populated base (a --bundle-models
    // app, a dev symlink, or a prior successful first-run), there's nothing to do.
    if models_present(&crate::app_paths::models_base_dir()) {
        return;
    }
    let Some(target) = crate::app_paths::app_support_dir().map(|d| d.join("models")) else {
        eprintln!("[models] $HOME unset — cannot pick a first-run download location");
        return;
    };
    if let Err(e) = ensure_default_models(&target) {
        eprintln!("[models] first-run download failed: {e}");
        notify(&format!("Model download failed: {e}. Check your connection and relaunch."));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_covers_both_model_trees() {
        let m = manifest();
        assert_eq!(m.len(), 4, "3 Parakeet files + 1 Qwen GGUF");
        assert!(m.iter().any(|(p, _)| p.starts_with(PARAKEET_REL)));
        assert!(m.iter().any(|(p, _)| p.starts_with(LLM_REL)));
        // Every URL is absolute https.
        assert!(m.iter().all(|(_, u)| u.starts_with("https://")));
    }

    #[test]
    fn present_and_models_present_track_files() {
        let tmp = std::env::temp_dir().join(format!(
            "ld-modelfetch-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(!models_present(&tmp), "empty dir ⇒ not present");

        // Lay down every manifest file as a non-empty stub.
        for (rel, _) in manifest() {
            let f = tmp.join(&rel);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(&f, b"x").unwrap();
        }
        assert!(models_present(&tmp), "all stubs present ⇒ present");

        // A zero-byte file counts as missing (interrupted download).
        let one = tmp.join(manifest()[0].0.clone());
        std::fs::write(&one, b"").unwrap();
        assert!(!present(&one), "0-byte file ⇒ missing");
        assert!(!models_present(&tmp));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
