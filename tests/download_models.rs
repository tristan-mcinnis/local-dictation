//! Runs `tests/download_models.zsh`: download-models.sh against a fake curl and
//! invented bytes in a temporary HOME. No network, no real model store.

use std::path::Path;
use std::process::Command;

#[test]
fn download_models_never_keeps_unverified_or_partial_models() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let out = Command::new("/bin/zsh")
        .arg(root.join("tests/download_models.zsh"))
        .output()
        .expect("run tests/download_models.zsh");
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
