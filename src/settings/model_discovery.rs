//! Cleanup-model discovery: list the GGUF files in the models folder with a
//! size hint for the picker.
use super::*;

/// A discovered cleanup-model candidate for the menu picker.
#[derive(Debug, Clone)]
pub struct ModelChoice {
    /// Human label — the model's directory name, e.g. `gemma-3-1b-it`.
    pub label: String,
    /// Absolute path to the `.gguf` file.
    pub path: String,
    /// Qualitative speed/accuracy hint for the menu, inferred from the model's
    /// parameter count (e.g. "fast · recommended", "most accurate · ~2.5× slower").
    /// `None` when the size can't be parsed from the name.
    pub hint: Option<String>,
}

/// Parse a parameter count (in billions) from a model directory name by looking
/// for a `<n>b` or `<n>m` token (e.g. `gemma-3-1b-it` → 1.0, `smollm2-360m-it`
/// → 0.36, `qwen-2.5-0.5b-it` → 0.5). `None` if no size token is present.
pub(super) fn parse_param_billions(name: &str) -> Option<f32> {
    let lower = name.to_lowercase();
    for tok in lower.split(|c: char| !(c.is_ascii_alphanumeric() || c == '.')) {
        // A size token ends in 'b' or 'm' with a numeric body, e.g. "1b", "0.5b",
        // "360m". Guard against the "3" in "gemma-3" (no unit suffix).
        let (unit, body) = match tok.strip_suffix('b') {
            Some(b) => ('b', b),
            None => match tok.strip_suffix('m') {
                Some(m) => ('m', m),
                None => continue,
            },
        };
        if body.is_empty() {
            continue;
        }
        if let Ok(n) = body.parse::<f32>() {
            return Some(if unit == 'm' { n / 1000.0 } else { n });
        }
    }
    None
}

/// Map a parameter count to a one-line speed/accuracy hint for the picker.
/// Thresholds are grounded in the measured ~2.5× cleanup-latency gap between
/// Gemma 1B and 4B on this machine (see examples/latency_lab).
pub(super) fn model_hint(name: &str) -> Option<String> {
    let normalized = name.to_ascii_lowercase();
    if normalized.contains("s1-mini") {
        return Some("English normalization".to_string());
    }
    if normalized.contains("tiny-aya") {
        return Some("NonCommercial licence · personal use only".to_string());
    }
    parse_param_billions(name).map(|size| format!("{size}B parameters"))
}

/// Enumerate cleanup models under `<models base>/llm/<name>/*.gguf`, where the
/// base is resolved bundle-aware (see [`crate::app_paths`]). Each subdirectory
/// contributes its first `.gguf`. Returns paths canonicalized to absolute so a
/// selection survives a daemon relaunch regardless of CWD.
pub fn discover_llm_models() -> Vec<ModelChoice> {
    let mut choices = discover_llm_models_in(&crate::app_paths::llm_models_dir());
    if let Some(path) = crate::app_paths::instruction_default_path() {
        if !choices.iter().any(|choice| choice.path == path) {
            let label = Path::new(&path).file_stem().and_then(|name| name.to_str())
                .unwrap_or("Instruction model").to_string();
            choices.push(ModelChoice { hint: model_hint(&label), label, path });
        }
    }
    choices.sort_by(|a, b| a.label.cmp(&b.label));
    choices
}

pub(super) fn discover_llm_models_in(root: &Path) -> Vec<ModelChoice> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let label = match dir.file_name().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        // First *.gguf inside this model directory.
        if let Ok(files) = std::fs::read_dir(&dir) {
            for f in files.flatten() {
                let p = f.path();
                if p.extension().and_then(|e| e.to_str()) == Some("gguf") {
                    let abs = std::fs::canonicalize(&p).unwrap_or(p);
                    out.push(ModelChoice {
                        hint: model_hint(&label),
                        label: label.clone(),
                        path: abs.to_string_lossy().into_owned(),
                    });
                    break;
                }
            }
        }
    }
    out.sort_by(|a, b| a.label.cmp(&b.label));
    out
}
