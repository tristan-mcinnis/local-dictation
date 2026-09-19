pub mod app_paths;
pub mod app_tones;
pub mod cancel;
pub mod audio;
pub mod audio_duck;
#[cfg(feature = "cohere-mlx")]
pub mod cohere_mlx;
pub mod corrections;
pub mod cues;
pub mod daemon_log;
pub mod dictionary;
pub mod history;
pub mod hotkeys;
pub mod injector;
pub mod ipc;
pub mod model_fetch;
pub mod numbers;
pub mod prompts;
pub mod recovery;
pub mod reprocess;
pub mod refiner;
pub mod screen_context;
pub mod settings;
pub mod shutdown;
pub mod smart_pad;
pub mod spoken_command;
pub mod text_polish;
pub mod transcriber;
pub mod ui_channel;
pub mod vad;
pub mod voice_commands;
pub mod wake_word;
pub mod waveform;

#[cfg(feature = "cleaner")]
pub mod cleaner;

#[cfg(all(target_os = "macos", feature = "ax-inject"))]
pub mod clipboard_paste;

#[cfg(all(target_os = "macos", feature = "ax-inject"))]
pub mod menubar;

#[cfg(all(target_os = "macos", feature = "ax-inject"))]
pub mod settings_window;

// Push-to-talk daemon. Needs the fixed Cohere MLX stack and AX injection.
#[cfg(all(target_os = "macos", feature = "cohere-mlx", feature = "ax-inject"))]
pub mod daemon;
