//! Fixed-stack model readiness checks.
//!
//! This personal build never downloads models silently. Cohere Q4 is converted
//! from the user's local checkpoint and S1-mini is linked/copied explicitly.

use std::path::Path;

const COHERE_MODEL: &str = "dictation/cohere-mlx-q4/model.safetensors";
const COHERE_CONFIG: &str = "dictation/cohere-mlx-q4/config.json";
const S1_MODEL: &str = "llm/s1-mini/s1-mini.gguf";

pub fn models_present(base: &Path) -> bool {
    [COHERE_MODEL, COHERE_CONFIG, S1_MODEL]
        .iter()
        .all(|relative| {
            base.join(relative)
                .metadata()
                .map(|m| m.len() > 0)
                .unwrap_or(false)
        })
}

pub fn ensure_default_models(base: &Path) -> eyre::Result<bool> {
    if models_present(base) {
        return Ok(false);
    }
    Err(eyre::eyre!(
        "fixed model stack is incomplete under {}. Run the Cohere conversion +         script and link S1-mini; see README.md",
        base.display()
    ))
}

pub fn ensure_models_on_first_run() {
    let base = crate::app_paths::models_base_dir();
    if !models_present(&base) {
        eprintln!(
            "[models] fixed stack missing under {} — run setup from the repository",
            base.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_exact_fixed_stack() {
        let root =
            std::env::temp_dir().join(format!("local-dictation-models-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        assert!(!models_present(&root));
        for relative in [COHERE_MODEL, COHERE_CONFIG, S1_MODEL] {
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"x").unwrap();
        }
        assert!(models_present(&root));
        let _ = std::fs::remove_dir_all(root);
    }
}
