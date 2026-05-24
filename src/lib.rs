pub mod audio;
pub mod corrections;
pub mod cues;
pub mod history;
pub mod injector;
pub mod prompts;
pub mod refiner;
pub mod settings;
pub mod smart_pad;
pub mod text_polish;
pub mod transcriber;
pub mod ui_channel;
pub mod voice_commands;

#[cfg(feature = "cleaner")]
pub mod cleaner;

#[cfg(all(target_os = "macos", feature = "ax-inject"))]
pub mod clipboard_paste;

#[cfg(all(target_os = "macos", feature = "ax-inject"))]
pub mod menubar;

// Push-to-talk daemon. Needs the full feature stack — Parakeet for ASR and
// AX for the event tap + injection.
#[cfg(all(target_os = "macos", feature = "parakeet", feature = "ax-inject"))]
pub mod daemon;
