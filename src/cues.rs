//! Feedback cues for the dictation state machine. The six state cues are
//! original: synthesized by `scripts/audio/generate_cues.py` (sine partials,
//! python3 stdlib only), so the embedded bytes are reproducible and free to
//! ship under this crate's licence. Do not swap in a sample taken from another
//! product; change the generator and re-run it instead.
//!
//! The start and completion sounds are each selectable.
//! `resources/start_sounds/` holds the start audition set and
//! `resources/completion_sounds/` the twenty completion candidates plus the
//! default; see [`START_SOUNDS`] and [`COMPLETION_SOUNDS`]. The start variants
//! are original local synthesis; of the completion set twelve are original
//! local synthesis and eight are CC0 by Kenney Vleugels (attribution in
//! `THIRD-PARTY-NOTICES.md`).
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

// ─── Selectable start sounds ─────────────────────────────────────────────
//
// One embedded entry per candidate. The default is the existing rising pair
// (`start.wav`), so an upgrade with no saved selection keeps the sound it
// already had. Every candidate rises, so "recording" and "landed" can never be
// confused by contour.

/// Stable id of the start cue used when nothing else is saved: the existing
/// rising pair.
pub const DEFAULT_START_SOUND: &str = "rising-pair";

const SS_SOFT_RISE: &[u8] = include_bytes!("../resources/start_sounds/soft-rise.wav");
const SS_BRIGHT_RISE: &[u8] = include_bytes!("../resources/start_sounds/bright-rise.wav");
const SS_LOW_RISE: &[u8] = include_bytes!("../resources/start_sounds/low-rise.wav");
const SS_QUICK_BLIP: &[u8] = include_bytes!("../resources/start_sounds/quick-blip.wav");
const SS_DOUBLE_TAP: &[u8] = include_bytes!("../resources/start_sounds/double-tap.wav");
const SS_GLASS_RISE: &[u8] = include_bytes!("../resources/start_sounds/glass-rise.wav");
const SS_WARM_RISE: &[u8] = include_bytes!("../resources/start_sounds/warm-rise.wav");

// ─── Selectable completion sounds ────────────────────────────────────────
//
// One embedded entry per candidate. `DEFAULT_COMPLETION_SOUND` is the existing
// tight thud (`success.wav`), so an upgrade with no saved selection keeps the
// sound it already had; the twenty others are the audition candidates. The set
// is a slice, not a fixed-size array, so adding a candidate cannot silently
// overrun a positional index.

/// Stable id of the completion cue used when nothing else is saved: the
/// existing tight completion thud.
pub const DEFAULT_COMPLETION_SOUND: &str = "tight-thud";

/// A selectable cue: a stable id (the value stored in `settings.json`), a
/// human-readable label, and the embedded WAV bytes.
pub struct CueSound {
    pub id: &'static str,
    pub label: &'static str,
    bytes: &'static [u8],
}

impl CueSound {
    /// The embedded WAV bytes for this cue.
    pub fn bytes(&self) -> &'static [u8] {
        self.bytes
    }
}

const CS_TIGHT_FELT_THUD: &[u8] =
    include_bytes!("../resources/completion_sounds/tight-felt-thud.wav");
const CS_COMPACT_WOOD_THUD: &[u8] =
    include_bytes!("../resources/completion_sounds/compact-wood-thud.wav");
const CS_SOFT_LANDING_THUD: &[u8] =
    include_bytes!("../resources/completion_sounds/soft-landing-thud.wav");
const CS_DEEP_SETTLE_THUD: &[u8] =
    include_bytes!("../resources/completion_sounds/deep-settle-thud.wav");
const CS_MUTED_STAMP: &[u8] = include_bytes!("../resources/completion_sounds/muted-stamp.wav");
const CS_SOFT_UI_CLICK: &[u8] = include_bytes!("../resources/completion_sounds/soft-ui-click.wav");
const CS_GLASS_TICK: &[u8] = include_bytes!("../resources/completion_sounds/glass-tick.wav");
const CS_WARM_KEY_TAP: &[u8] = include_bytes!("../resources/completion_sounds/warm-key-tap.wav");
const CS_BUBBLE_POP: &[u8] = include_bytes!("../resources/completion_sounds/bubble-pop.wav");
const CS_MUTED_PLUCK: &[u8] = include_bytes!("../resources/completion_sounds/muted-pluck.wav");
const CS_DOUBLE_SETTLE: &[u8] = include_bytes!("../resources/completion_sounds/double-settle.wav");
const CS_WARM_DONE_TONE: &[u8] =
    include_bytes!("../resources/completion_sounds/warm-done-tone.wav");
const CS_KENNEY_TIGHT_CLICK: &[u8] =
    include_bytes!("../resources/completion_sounds/kenney-tight-click.wav");
const CS_KENNEY_BONG_IMPACT: &[u8] =
    include_bytes!("../resources/completion_sounds/kenney-bong-impact.wav");
const CS_KENNEY_DROP_IMPACT: &[u8] =
    include_bytes!("../resources/completion_sounds/kenney-drop-impact.wav");
const CS_KENNEY_SOFT_IMPACT: &[u8] =
    include_bytes!("../resources/completion_sounds/kenney-soft-impact.wav");
const CS_KENNEY_HEAVY_WOOD_IMPACT: &[u8] =
    include_bytes!("../resources/completion_sounds/kenney-heavy-wood-impact.wav");
const CS_KENNEY_CONFIRMATION_1: &[u8] =
    include_bytes!("../resources/completion_sounds/kenney-confirmation-1.wav");
const CS_KENNEY_CONFIRMATION_2: &[u8] =
    include_bytes!("../resources/completion_sounds/kenney-confirmation-2.wav");
const CS_KENNEY_CONFIRMATION_3: &[u8] =
    include_bytes!("../resources/completion_sounds/kenney-confirmation-3.wav");

/// Every completion cue offered in Settings, default first. The order is the
/// audition order; ids are stable across releases and never renumbered.
pub static COMPLETION_SOUNDS: &[CueSound] = &[
    CueSound {
        id: DEFAULT_COMPLETION_SOUND,
        label: "Tight thud (default)",
        bytes: SUCCESS,
    },
    CueSound {
        id: "tight-felt-thud",
        label: "01 · Tight felt thud",
        bytes: CS_TIGHT_FELT_THUD,
    },
    CueSound {
        id: "compact-wood-thud",
        label: "02 · Compact wood thud",
        bytes: CS_COMPACT_WOOD_THUD,
    },
    CueSound {
        id: "soft-landing-thud",
        label: "03 · Soft landing thud",
        bytes: CS_SOFT_LANDING_THUD,
    },
    CueSound {
        id: "deep-settle-thud",
        label: "04 · Deep settle thud",
        bytes: CS_DEEP_SETTLE_THUD,
    },
    CueSound {
        id: "muted-stamp",
        label: "05 · Muted stamp",
        bytes: CS_MUTED_STAMP,
    },
    CueSound {
        id: "soft-ui-click",
        label: "06 · Soft UI click",
        bytes: CS_SOFT_UI_CLICK,
    },
    CueSound {
        id: "glass-tick",
        label: "07 · Glass tick",
        bytes: CS_GLASS_TICK,
    },
    CueSound {
        id: "warm-key-tap",
        label: "08 · Warm key tap",
        bytes: CS_WARM_KEY_TAP,
    },
    CueSound {
        id: "bubble-pop",
        label: "09 · Bubble pop",
        bytes: CS_BUBBLE_POP,
    },
    CueSound {
        id: "muted-pluck",
        label: "10 · Muted pluck",
        bytes: CS_MUTED_PLUCK,
    },
    CueSound {
        id: "double-settle",
        label: "11 · Double settle",
        bytes: CS_DOUBLE_SETTLE,
    },
    CueSound {
        id: "warm-done-tone",
        label: "12 · Warm done tone",
        bytes: CS_WARM_DONE_TONE,
    },
    CueSound {
        id: "kenney-tight-click",
        label: "13 · Tight click",
        bytes: CS_KENNEY_TIGHT_CLICK,
    },
    CueSound {
        id: "kenney-bong-impact",
        label: "14 · Bong impact",
        bytes: CS_KENNEY_BONG_IMPACT,
    },
    CueSound {
        id: "kenney-drop-impact",
        label: "15 · Drop impact",
        bytes: CS_KENNEY_DROP_IMPACT,
    },
    CueSound {
        id: "kenney-soft-impact",
        label: "16 · Soft impact",
        bytes: CS_KENNEY_SOFT_IMPACT,
    },
    CueSound {
        id: "kenney-heavy-wood-impact",
        label: "17 · Heavy wood impact",
        bytes: CS_KENNEY_HEAVY_WOOD_IMPACT,
    },
    CueSound {
        id: "kenney-confirmation-1",
        label: "18 · Confirmation 1",
        bytes: CS_KENNEY_CONFIRMATION_1,
    },
    CueSound {
        id: "kenney-confirmation-2",
        label: "19 · Confirmation 2",
        bytes: CS_KENNEY_CONFIRMATION_2,
    },
    CueSound {
        id: "kenney-confirmation-3",
        label: "20 · Confirmation 3",
        bytes: CS_KENNEY_CONFIRMATION_3,
    },
];

/// Every start cue offered in Settings, default first. Order is the audition
/// order; ids are stable across releases and never renumbered.
pub static START_SOUNDS: &[CueSound] = &[
    CueSound {
        id: DEFAULT_START_SOUND,
        label: "Rising pair (default)",
        bytes: START,
    },
    CueSound {
        id: "soft-rise",
        label: "01 · Soft rise",
        bytes: SS_SOFT_RISE,
    },
    CueSound {
        id: "bright-rise",
        label: "02 · Bright rise",
        bytes: SS_BRIGHT_RISE,
    },
    CueSound {
        id: "low-rise",
        label: "03 · Low rise",
        bytes: SS_LOW_RISE,
    },
    CueSound {
        id: "quick-blip",
        label: "04 · Quick blip",
        bytes: SS_QUICK_BLIP,
    },
    CueSound {
        id: "double-tap",
        label: "05 · Double tap",
        bytes: SS_DOUBLE_TAP,
    },
    CueSound {
        id: "glass-rise",
        label: "06 · Glass rise",
        bytes: SS_GLASS_RISE,
    },
    CueSound {
        id: "warm-rise",
        label: "07 · Warm rise",
        bytes: SS_WARM_RISE,
    },
];

/// A catalog lookup by id. Trims surrounding whitespace first so a
/// hand-edited settings value still matches.
fn find_sound(catalog: &'static [CueSound], id: &str) -> Option<&'static CueSound> {
    let id = id.trim();
    catalog.iter().find(|s| s.id == id)
}

/// The catalog entry for `id`, if it is a known completion sound.
pub fn completion_sound(id: &str) -> Option<&'static CueSound> {
    find_sound(COMPLETION_SOUNDS, id)
}

/// The catalog entry for `id`, if it is a known start sound.
pub fn start_sound(id: &str) -> Option<&'static CueSound> {
    find_sound(START_SOUNDS, id)
}

/// Any selectable cue (start or completion) by id. The two id sets are
/// disjoint, so one lookup serves the shared playback cache.
fn selectable_sound(id: &str) -> Option<&'static CueSound> {
    start_sound(id).or_else(|| completion_sound(id))
}

/// Canonicalize a completion-sound id: known ids pass through, anything else
/// (blank, unknown, hand-edited) falls back to [`DEFAULT_COMPLETION_SOUND`].
pub fn normalize_completion_sound(id: &str) -> &'static str {
    completion_sound(id)
        .map(|s| s.id)
        .unwrap_or(DEFAULT_COMPLETION_SOUND)
}

/// Canonicalize a start-sound id: known ids pass through, anything else
/// (blank, unknown, hand-edited) falls back to [`DEFAULT_START_SOUND`].
pub fn normalize_start_sound(id: &str) -> &'static str {
    start_sound(id)
        .map(|s| s.id)
        .unwrap_or(DEFAULT_START_SOUND)
}

/// The completion sound selected for this process, read from settings once and
/// cached. Settings are applied on restart, so one read stays correct.
fn selected_completion_sound() -> &'static str {
    static SELECTED: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SELECTED
        .get_or_init(|| crate::settings::Settings::load().resolve_completion_sound())
        .as_str()
}

/// The start sound selected for this process, read and cached the same way.
fn selected_start_sound() -> &'static str {
    static SELECTED: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SELECTED
        .get_or_init(|| crate::settings::Settings::load().resolve_start_sound())
        .as_str()
}

/// Recording started: the resolved start sound.
pub fn play_start() {
    play_selected(selected_start_sound());
}

/// Recording stopped. Silent by design (2026-09-09): the pair the user wants
/// is start-at-press and stop-when-the-text-lands, not a third cue at release.
/// The Stop sample stays registered for `DICTATE_STOP_CUE=1`.
pub fn play_stop() {
    if std::env::var_os("DICTATE_STOP_CUE").is_some() {
        play(Cue::Stop);
    }
}

/// Text was accepted and injected: the resolved completion sound. Played only
/// after a successful injection (see the `Injected` arm in `daemon::worker`).
pub fn play_success() {
    play_selected(selected_completion_sound());
}

/// Play a completion cue by id, for the Settings audition. Respects
/// `DICTATE_QUIET`; an unknown id falls back to the default.
pub fn preview_completion_sound(id: &str) {
    play_selected(normalize_completion_sound(id));
}

/// Play a start cue by id, for the Settings audition. Respects `DICTATE_QUIET`;
/// an unknown id falls back to the default.
pub fn preview_start_sound(id: &str) {
    play_selected(normalize_start_sound(id));
}

/// Play one selectable cue through the same native path as the state cues.
fn play_selected(id: &str) {
    if std::env::var_os("DICTATE_QUIET").is_some() {
        return;
    }
    // Resolve to the catalog's own `'static` id so the playback cache can key
    // on it. Unknown ids play nothing; the public previews normalize first.
    let Some(canonical) = selectable_sound(id).map(|s| s.id) else {
        return;
    };
    #[cfg(target_os = "macos")]
    {
        if let Some(sounds) = macos::sounds() {
            sounds.play_selected(canonical);
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = canonical;
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
/// Also preloads the currently selected completion cue, so the first dictation
/// pays no filesystem or CoreAudio setup.
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
        sync::{Mutex, OnceLock},
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
        /// Selectable cues (start and completion) loaded so far, keyed by their
        /// stable id. The selected ones are loaded with the state cues; an
        /// audition candidate is loaded on first play. A `Vec`, not a fixed
        /// array, so the catalog can grow without a positional index to keep in
        /// sync.
        selectable: Mutex<Vec<(&'static str, Id)>>,
        directory: PathBuf,
    }
    // NSSound is safe to `play` from any thread; the objects are created once
    // and never mutated afterwards. The completion cache is `Mutex`-guarded.
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
            // Preload the selected start and completion cues. A failure here
            // is not fatal: `play_selected` retries at play time, and a missing
            // cue must never take down the state cues.
            let mut selectable = Vec::new();
            for id in [
                super::selected_start_sound(),
                super::selected_completion_sound(),
            ] {
                if let Some(entry) = load_completion(&directory, id) {
                    selectable.push(entry);
                }
            }
            Some(Self {
                sounds,
                selectable: Mutex::new(selectable),
                directory,
            })
        }

        /// Play a selectable cue by canonical id, loading and caching it on
        /// first use.
        pub(super) fn play_selected(&self, id: &'static str) {
            let canonical = id;
            let sound = {
                let mut cache = self.selectable.lock().unwrap();
                match cache.iter().find(|(cached, _)| *cached == canonical) {
                    Some((_, sound)) => *sound,
                    None => match load_completion(&self.directory, canonical) {
                        Some((_, sound)) => {
                            cache.push((canonical, sound));
                            sound
                        }
                        None => return,
                    },
                }
            };
            unsafe {
                // Restart from the top if the same cue is still sounding.
                send0(sound, sel("stop"));
                send0(sound, sel("play"));
            }
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

    /// Materialize one selectable cue in the cache directory and wrap it as an
    /// `NSSound`. Returns the canonical id beside it so the cache can key on it.
    fn load_completion(directory: &Path, id: &str) -> Option<(&'static str, Id)> {
        let entry = super::selectable_sound(id)?;
        let path = directory.join(format!("{}.wav", entry.id));
        if !matches_bytes(&path, entry.bytes()) {
            fs::write(&path, entry.bytes()).ok()?;
        }
        let sound = create_sound(&path)?;
        Some((entry.id, sound))
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

    /// WAV header facts for any channel count: (channels, rate, bits, data len).
    fn wav_info(bytes: &[u8]) -> (u16, u32, u16, u32) {
        assert!(bytes.starts_with(b"RIFF"), "not a RIFF file");
        assert_eq!(&bytes[8..12], b"WAVE");
        (
            u16::from_le_bytes(bytes[22..24].try_into().unwrap()),
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            u16::from_le_bytes(bytes[34..36].try_into().unwrap()),
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
        )
    }

    #[test]
    fn completion_catalog_lists_default_plus_twenty_auditions() {
        assert_eq!(COMPLETION_SOUNDS.len(), 21);
        assert_eq!(COMPLETION_SOUNDS[0].id, DEFAULT_COMPLETION_SOUND);
        // The default must be the currently installed tight thud, byte for byte.
        assert_eq!(COMPLETION_SOUNDS[0].bytes, SUCCESS);
        assert_eq!(COMPLETION_SOUNDS[0].label, "Tight thud (default)");

        let ids: Vec<&str> = COMPLETION_SOUNDS.iter().map(|s| s.id).collect();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "completion ids must be unique");

        // Stable ids, in audition order. Renaming or reordering one is a
        // settings migration, not a rename.
        assert_eq!(
            &ids[1..],
            &[
                "tight-felt-thud",
                "compact-wood-thud",
                "soft-landing-thud",
                "deep-settle-thud",
                "muted-stamp",
                "soft-ui-click",
                "glass-tick",
                "warm-key-tap",
                "bubble-pop",
                "muted-pluck",
                "double-settle",
                "warm-done-tone",
                "kenney-tight-click",
                "kenney-bong-impact",
                "kenney-drop-impact",
                "kenney-soft-impact",
                "kenney-heavy-wood-impact",
                "kenney-confirmation-1",
                "kenney-confirmation-2",
                "kenney-confirmation-3",
            ]
        );
    }

    #[test]
    fn completion_cues_are_valid_48k_wavs() {
        for entry in COMPLETION_SOUNDS {
            assert!(!entry.label.is_empty(), "{} has no label", entry.id);
            let bytes = entry.bytes;
            assert!(bytes.len() > 44, "{} is not a WAV", entry.id);
            let (channels, rate, bits, data_len) = wav_info(bytes);
            assert!(channels >= 1 && channels <= 2, "{} channels", entry.id);
            assert_eq!(bits, 16, "{} sample width", entry.id);
            assert_eq!(rate, 48_000, "{} sample rate", entry.id);
            let bytes_per_frame = u32::from(channels) * 2;
            let ms = data_len.saturating_mul(1_000) / (rate * bytes_per_frame);
            assert!(ms <= 800, "{} is {ms} ms", entry.id);
        }
    }

    #[test]
    fn unknown_completion_sound_falls_back_to_default() {
        assert_eq!(normalize_completion_sound(""), DEFAULT_COMPLETION_SOUND);
        assert_eq!(normalize_completion_sound("nope"), DEFAULT_COMPLETION_SOUND);
        assert_eq!(normalize_completion_sound("   "), DEFAULT_COMPLETION_SOUND);
        assert_eq!(normalize_completion_sound("  glass-tick  "), "glass-tick");
        assert_eq!(
            normalize_completion_sound(DEFAULT_COMPLETION_SOUND),
            DEFAULT_COMPLETION_SOUND
        );
        assert!(completion_sound("glass-tick").is_some());
        assert!(completion_sound("glass-tick")
            .unwrap()
            .label
            .contains("Glass tick"));
        assert!(completion_sound("nope").is_none());
    }

    #[test]
    fn start_catalog_lists_default_plus_seven_auditions() {
        assert_eq!(START_SOUNDS.len(), 8);
        assert_eq!(START_SOUNDS[0].id, DEFAULT_START_SOUND);
        assert_eq!(START_SOUNDS[0].bytes, START);
        assert_eq!(START_SOUNDS[0].label, "Rising pair (default)");

        let ids: Vec<&str> = START_SOUNDS.iter().map(|s| s.id).collect();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "start ids must be unique");

        assert_eq!(
            &ids[1..],
            &[
                "soft-rise",
                "bright-rise",
                "low-rise",
                "quick-blip",
                "double-tap",
                "glass-rise",
                "warm-rise",
            ]
        );

        // The two selectable id sets are disjoint so one playback cache serves
        // both.
        for start in START_SOUNDS {
            assert!(
                completion_sound(start.id).is_none(),
                "{} collides with a completion id",
                start.id
            );
            assert_eq!(selectable_sound(start.id).unwrap().id, start.id);
        }
    }

    #[test]
    fn start_cues_are_valid_48k_wavs() {
        for entry in START_SOUNDS {
            assert!(!entry.label.is_empty(), "{} has no label", entry.id);
            let bytes = entry.bytes;
            assert!(bytes.len() > 44, "{} is not a WAV", entry.id);
            let (channels, rate, bits, data_len) = wav_info(bytes);
            assert!(channels >= 1 && channels <= 2, "{} channels", entry.id);
            assert_eq!(bits, 16, "{} sample width", entry.id);
            assert_eq!(rate, 48_000, "{} sample rate", entry.id);
            let bytes_per_frame = u32::from(channels) * 2;
            let ms = data_len.saturating_mul(1_000) / (rate * bytes_per_frame);
            assert!(ms <= 800, "{} is {ms} ms", entry.id);
        }
    }

    #[test]
    fn unknown_start_sound_falls_back_to_default() {
        assert_eq!(normalize_start_sound(""), DEFAULT_START_SOUND);
        assert_eq!(normalize_start_sound("nope"), DEFAULT_START_SOUND);
        assert_eq!(normalize_start_sound("   "), DEFAULT_START_SOUND);
        assert_eq!(normalize_start_sound("  glass-rise  "), "glass-rise");
        assert_eq!(normalize_start_sound("glass-tick"), DEFAULT_START_SOUND);
        assert!(start_sound("glass-rise").unwrap().label.contains("Glass rise"));
        assert!(start_sound("glass-tick").is_none());
    }
}
