//! Feedback cues for the dictation state machine. Every cue is original:
//! all six are synthesized by `scripts/audio/generate_cues.py` (sine partials,
//! python3 stdlib only), so the embedded bytes are reproducible and free to
//! ship under this crate's licence. Do not swap in a sample taken from another
//! product; change the generator and re-run it instead.
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

/// Recording started: a short rising pair.
pub fn play_start() {
    play(Cue::Start);
}

/// Recording stopped. Silent by design (2026-09-09): the pair the user wants
/// is start-at-press and stop-when-the-text-lands, not a third cue at release.
/// The Stop sample stays registered for `DICTATE_STOP_CUE=1`.
pub fn play_stop() {
    if std::env::var_os("DICTATE_STOP_CUE").is_some() {
        play(Cue::Stop);
    }
}

/// Text was accepted and injected: a soft, weighty landing thud.
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
    //! Playback goes through `NSSound`, the same path Superwhisper uses, so the
    //! cues follow the normal output volume. The previous
    //! `AudioServicesPlaySystemSound` route played on the *alert* channel, and
    //! a Mac with alert volume at 0 (the user's, 2026-09-09) never heard a cue.
    //! Driven through the Objective-C runtime directly so this module stays
    //! free of the optional objc2 crates and the featureless build links.
    use super::Cue;
    use std::{
        ffi::{c_void, CString},
        fs,
        path::{Path, PathBuf},
        sync::OnceLock,
    };

    type Id = *mut c_void;
    type Sel = *const c_void;

    #[link(name = "objc", kind = "dylib")]
    extern "C" {
        fn objc_getClass(name: *const std::os::raw::c_char) -> Id;
        fn sel_registerName(name: *const std::os::raw::c_char) -> Sel;
        fn objc_msgSend();
    }
    #[link(name = "AppKit", kind = "framework")]
    extern "C" {}
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringCreateWithCString(
            alloc: *const c_void,
            c_str: *const std::os::raw::c_char,
            encoding: u32,
        ) -> Id;
        fn CFRelease(value: *const c_void);
    }
    const UTF8: u32 = 0x0800_0100;

    fn sel(name: &str) -> Sel {
        let c = CString::new(name).expect("selector");
        unsafe { sel_registerName(c.as_ptr()) }
    }

    unsafe fn send0(receiver: Id, selector: Sel) -> Id {
        let f: unsafe extern "C" fn(Id, Sel) -> Id = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        f(receiver, selector)
    }
    unsafe fn send2(receiver: Id, selector: Sel, a: Id, b: bool) -> Id {
        let f: unsafe extern "C" fn(Id, Sel, Id, bool) -> Id =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        f(receiver, selector, a, b)
    }
    unsafe fn send_f32(receiver: Id, selector: Sel, v: f32) {
        let f: unsafe extern "C" fn(Id, Sel, f32) = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        f(receiver, selector, v)
    }

    pub(super) struct Sounds {
        sounds: [Id; 6],
        _directory: PathBuf,
    }
    // NSSound is safe to `play` from any thread; the objects are created once
    // and never mutated afterwards.
    unsafe impl Send for Sounds {}
    unsafe impl Sync for Sounds {}

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
            let mut sounds = [std::ptr::null_mut(); 6];
            for (index, cue) in cues.into_iter().enumerate() {
                let path = directory.join(cue.file_name());
                if !matches_bytes(&path, cue.bytes()) {
                    fs::write(&path, cue.bytes()).ok()?;
                }
                sounds[index] = create_sound(&path)?;
            }
            Some(Self {
                sounds,
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
            let sound = self.sounds[index];
            unsafe {
                // Restart from the top if the same cue is still sounding.
                send0(sound, sel("stop"));
                send0(sound, sel("play"));
            }
        }
    }

    /// `[[NSSound alloc] initWithContentsOfFile:path byReference:NO]`, retained
    /// for the life of the process.
    fn create_sound(path: &Path) -> Option<Id> {
        let c_path = CString::new(path.to_str()?).ok()?;
        unsafe {
            let cls = objc_getClass(c"NSSound".as_ptr());
            if cls.is_null() {
                return None;
            }
            let ns_path = CFStringCreateWithCString(std::ptr::null(), c_path.as_ptr(), UTF8);
            if ns_path.is_null() {
                return None;
            }
            let alloc = send0(cls, sel("alloc"));
            let sound = send2(alloc, sel("initWithContentsOfFile:byReference:"), ns_path, false);
            CFRelease(ns_path);
            if sound.is_null() {
                return None;
            }
            send_f32(sound, sel("setVolume:"), 1.0);
            Some(sound)
        }
    }

    fn cache_directory() -> Option<PathBuf> {
        crate::app_paths::cache_dir("Sounds")
    }

    fn matches_bytes(path: &Path, bytes: &[u8]) -> bool {
        fs::read(path).map(|existing| existing == bytes).unwrap_or(false)
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
            assert!(wav_duration_ms(bytes) <= 800);
        }
    }

    /// Every embedded cue must be exactly what `scripts/audio/generate_cues.py`
    /// writes: 48 kHz mono 16-bit, with the generator's durations. Regression
    /// guard: start/stop/success were once replaced by samples lifted from
    /// another product (60 KB files against the generator's 11–14 KB).
    #[test]
    fn embedded_cues_match_the_generator() {
        for (name, bytes, seconds) in [
            ("start", START, 0.115),
            ("stop", STOP, 0.125),
            ("success", SUCCESS, 0.145),
            ("cancel", CANCEL, 0.135),
            ("latch", LATCH, 0.090),
            ("error", ERROR, 0.145),
        ] {
            let channels = u16::from_le_bytes(bytes[22..24].try_into().unwrap());
            let rate = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
            let bits = u16::from_le_bytes(bytes[34..36].try_into().unwrap());
            assert_eq!((channels, rate, bits), (1, 48_000, 16), "{name}");
            let frames = (seconds * 48_000.0_f64) as usize;
            assert_eq!(bytes.len(), 44 + frames * 2, "{name} is not the generated cue");
        }
    }
}
