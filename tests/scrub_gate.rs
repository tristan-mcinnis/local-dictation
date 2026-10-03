//! Runs `tests/scrub.sh` as part of `cargo test`, so a personal path, a key
//! shape or a name from the untracked `.scrub-private` list fails the suite.
//! Skipped outside a git checkout (a source tarball has no `git ls-files`).

use std::path::Path;
use std::process::Command;

#[test]
fn tracked_files_carry_no_private_content() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    if !root.join(".git").exists() {
        eprintln!("scrub gate skipped: not a git checkout");
        return;
    }
    let out = Command::new("/bin/sh")
        .arg(root.join("tests/scrub.sh"))
        .current_dir(root)
        .output()
        .expect("run tests/scrub.sh");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "tests/scrub.sh failed:\n{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("SCRUB_CLEAN"), "{stdout}");
}
