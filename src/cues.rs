//! Short, original feedback cues for the dictation state machine.
//!
//! Samples are embedded in the binary and registered with macOS' native
//! system-sound service on first use. Playback is asynchronous, so a cue never
//! sits on the recording or inference hot path. `DICTATE_QUIET` remains an
//! escape hatch for meetings, screen recordings, and tests.

const START: &[u8] = include_bytes!("../resources/sounds/start.wav");
const STOP: &[u8] = include_bytes!("../resources/sounds/stop.wav");
const SUCCESS: &[u8] = include_bytes!("../resources/sounds/success.wav");
const CANCEL: &[u8] = include_bytes!("../resources/sounds/cancel.wav");
const LATCH: &[u8] = include_bytes!("../resources/sounds/latch.wav");
const ERROR: &[u8] = include_bytes!("../resources/sounds/error.wav");

/// Recording started. A gentle, rising two-note cue.
pub fn play_start() {
    play(Cue::Start);
}

/// Recording stopped. A soft, descending release cue.
pub fn play_stop() {
    play(Cue::Stop);
}

/// Text was accepted and injected successfully.
pub fn play_success() {
    play(Cue::Success);
}

/// The current recording was cancelled by the user.
pub fn play_cancel() {
    play(Cue::Cancel);
}

/// Hands-free mode is latched; the user can release the key.
pub fn play_latch() {
    play(Cue::Latch);
}

/// A capture, model, or injection error occurred.
pub fn play_error() {
    play(Cue::Error);
}

/// Register the embedded cues with the native audio service ahead of the first
/// recording. Calling this during daemon startup keeps the first push-to-talk
/// transition free of filesystem and CoreAudio setup work. It is safe to call
/// repeatedly and is a no-op when cues are disabled.
pub fn preload() {
    if std::env::var_os("DICTATE_QUIET").is_some() {
        return;
    }
    #[cfg(target_os = "macos")]
    {
        let _ = macos::sounds();
    }
}

#[derive(Clone, Copy)]
enum Cue {
    Start,
    Stop,
    Success,
    Cancel,
    Latch,
    Error,
}

impl Cue {
    fn bytes(self) -> &'static [u8] {
        match self {
            Self::Start => START,
            Self::Stop => STOP,
            Self::Success => SUCCESS,
            Self::Cancel => CANCEL,
            Self::Latch => LATCH,
            Self::Error => ERROR,
        }
    }

    fn file_name(self) -> &'static str {
        match self {
            Self::Start => "start.wav",
            Self::Stop => "stop.wav",
            Self::Success => "success.wav",
            Self::Cancel => "cancel.wav",
            Self::Latch => "latch.wav",
            Self::Error => "error.wav",
        }
    }
}

fn play(cue: Cue) {
    if std::env::var_os("DICTATE_QUIET").is_some() {
        return;
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(sounds) = macos::sounds() {
            sounds.play(cue);
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = cue;
}

#[cfg(target_os = "macos")]
mod macos {
    use super::Cue;
    use std::{
        ffi::c_void,
        fs,
        os::unix::ffi::OsStrExt,
        path::{Path, PathBuf},
        sync::OnceLock,
    };

    type CFIndex = isize;
    type CFURLRef = *const c_void;
    type SystemSoundID = u32;

    #[link(name = "AudioToolbox", kind = "framework")]
    extern "C" {
        fn AudioServicesCreateSystemSoundID(
            file_url: CFURLRef,
            sound_id: *mut SystemSoundID,
        ) -> i32;
        fn AudioServicesPlaySystemSound(sound_id: SystemSoundID);
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFURLCreateFromFileSystemRepresentation(
            allocator: *const c_void,
            buffer: *const u8,
            buffer_len: CFIndex,
            is_directory: bool,
        ) -> CFURLRef;
        fn CFRelease(value: *const c_void);
    }

    pub(super) struct Sounds {
        ids: [SystemSoundID; 6],
        _directory: PathBuf,
    }
    static SOUNDS: OnceLock<Option<Sounds>> = OnceLock::new();

    pub(super) fn sounds() -> Option<&'static Sounds> {
        SOUNDS.get_or_init(Sounds::load).as_ref()
    }

    impl Sounds {
        fn load() -> Option<Self> {
            let directory = cache_directory()?;
            fs::create_dir_all(&directory).ok()?;
            let cues = [
                Cue::Start,
                Cue::Stop,
                Cue::Success,
                Cue::Cancel,
                Cue::Latch,
                Cue::Error,
            ];
            let mut ids = [0; 6];
            for (index, cue) in cues.into_iter().enumerate() {
                let path = directory.join(cue.file_name());
                if !matches_bytes(&path, cue.bytes()) {
                    fs::write(&path, cue.bytes()).ok()?;
                }
                ids[index] = create_id(&path)?;
            }
            Some(Self {
                ids,
                _directory: directory,
            })
        }

        pub(super) fn play(&self, cue: Cue) {
            let index = match cue {
                Cue::Start => 0,
                Cue::Stop => 1,
                Cue::Success => 2,
                Cue::Cancel => 3,
                Cue::Latch => 4,
                Cue::Error => 5,
            };
            unsafe {
                AudioServicesPlaySystemSound(self.ids[index]);
            }
        }
    }

    fn create_id(path: &Path) -> Option<SystemSoundID> {
        let bytes = path.as_os_str().as_bytes();
        let url = unsafe {
            CFURLCreateFromFileSystemRepresentation(
                std::ptr::null(),
                bytes.as_ptr(),
                bytes.len() as CFIndex,
                false,
            )
        };
        if url.is_null() {
            return None;
        }
        let mut id = 0;
        let status = unsafe { AudioServicesCreateSystemSoundID(url, &mut id) };
        unsafe {
            CFRelease(url);
        }
        (status == 0).then_some(id)
    }

    fn cache_directory() -> Option<PathBuf> {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join("Library/Caches/Local Dictation/Sounds"))
    }

    fn matches_bytes(path: &Path, expected: &[u8]) -> bool {
        fs::read(path)
            .map(|actual| actual == expected)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn wav_duration_ms(bytes: &[u8]) -> u32 {
        assert!(bytes.starts_with(b"RIFF"));
        assert_eq!(&bytes[8..12], b"WAVE");
        let rate = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
        let len = u32::from_le_bytes(bytes[40..44].try_into().unwrap());
        len.saturating_mul(1_000) / (rate * 2)
    }
    #[test]
    fn bundled_cues_are_short_and_valid() {
        for bytes in [START, STOP, SUCCESS, CANCEL, LATCH, ERROR] {
            assert!(bytes.len() > 44);
            assert!(wav_duration_ms(bytes) <= 240);
        }
    }
}
