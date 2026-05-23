//! Audio cues for record-start / record-stop. Uses macOS `afplay` so we
//! don't need any audio output dependency. Non-blocking — we spawn afplay
//! and don't wait for it.
//!
//! Set `DICTATE_QUIET=1` to disable cues.

use std::process::Command;

const SYSTEM_SOUND_DIR: &str = "/System/Library/Sounds";

pub fn play_start() {
    play("Tink");
}

pub fn play_stop() {
    // Bottle is a soft completion "bloop" — distinct from the Tink start
    // cue, no bell tail (which Glass had).
    play("Bottle");
}

pub fn play_cancel() {
    play("Funk");
}

pub fn play_error() {
    play("Basso");
}

fn play(sound: &str) {
    if std::env::var("DICTATE_QUIET").is_ok() {
        return;
    }
    #[cfg(target_os = "macos")]
    {
        let path = format!("{SYSTEM_SOUND_DIR}/{sound}.aiff");
        let _ = Command::new("afplay")
            .arg("-v")
            .arg("0.3") // 30% volume — present but not jarring
            .arg(&path)
            .spawn();
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = sound;
    }
}
