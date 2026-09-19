//! Layout-independent keycodes for the shortcuts the app synthesizes.
//!
//! A synthesized ⌘V is a *physical key position* plus the Command flag; the
//! receiving app resolves the key equivalent through the active keyboard
//! layout. The US-ANSI positions (V = 9, C = 8, Z = 6) are wrong on layouts that
//! move those letters: on Dvorak the V position types "k", so a clipboard paste
//! sent ⌘K and nothing landed; on AZERTY the Z position types "w", so "undo
//! that" sent ⌘W and closed the window; on QWERTZ it sent ⌘Y.
//!
//! So the keycode for each letter is looked up in the current ASCII-capable
//! layout (`TISCopyCurrentASCIICapableKeyboardLayoutInputSource` +
//! `UCKeyTranslate`), cached in atomics, and the ANSI code is the fallback
//! whenever the lookup cannot answer. Text Input Source calls must run on the
//! main thread (they assert otherwise on current macOS), so [`refresh`] is a
//! no-op anywhere else; the menu bar's main-thread timer calls it at boot and
//! every couple of seconds, which also follows a layout switch.

use std::sync::atomic::{AtomicU16, Ordering};

/// US-ANSI virtual keycodes: the fallbacks, and the fast path on QWERTY.
pub const ANSI_C: u16 = 8;
pub const ANSI_V: u16 = 9;
pub const ANSI_Z: u16 = 6;

/// Keycodes 0..=50 are the typing block (letters, digits, punctuation).
const LAST_TYPING_KEYCODE: u16 = 50;

static KEY_C: AtomicU16 = AtomicU16::new(ANSI_C);
static KEY_V: AtomicU16 = AtomicU16::new(ANSI_V);
static KEY_Z: AtomicU16 = AtomicU16::new(ANSI_Z);

/// Keycode to post with Command for Copy / Paste / Undo in the current layout.
pub fn copy_key() -> u16 {
    KEY_C.load(Ordering::Relaxed)
}
pub fn paste_key() -> u16 {
    KEY_V.load(Ordering::Relaxed)
}
pub fn undo_key() -> u16 {
    KEY_Z.load(Ordering::Relaxed)
}

/// The keycode that types `target` under `translate` (keycode → character), or
/// `fallback` when no key does. The ANSI position is tried first, so a layout
/// that keeps the letter in place costs one lookup and can never pick a
/// duplicate elsewhere on the keyboard.
pub fn keycode_for_char(target: char, fallback: u16, translate: impl Fn(u16) -> Option<char>) -> u16 {
    let hits = |code: u16| translate(code).is_some_and(|c| c.eq_ignore_ascii_case(&target));
    if hits(fallback) {
        return fallback;
    }
    (0..=LAST_TYPING_KEYCODE).find(|&code| hits(code)).unwrap_or(fallback)
}

/// Resolve one letter: the Command-held table first (that is what a key
/// equivalent is matched against, and it is what makes "Dvorak - QWERTY ⌘"
/// come out as QWERTY), then the plain table, then the ANSI fallback.
fn resolve(target: char, fallback: u16, translate: &dyn Fn(u16, bool) -> Option<char>) -> u16 {
    let with_cmd = keycode_for_char(target, u16::MAX, |code| {
        if code == u16::MAX { None } else { translate(code, true) }
    });
    if with_cmd != u16::MAX {
        return with_cmd;
    }
    keycode_for_char(target, fallback, |code| translate(code, false))
}

/// Store the three keycodes from a translate function. Returns them as
/// `(c, v, z)`. Split from the FFI so the whole decision is unit-tested.
fn store_from(translate: &dyn Fn(u16, bool) -> Option<char>) -> (u16, u16, u16) {
    let keys = (
        resolve('c', ANSI_C, translate),
        resolve('v', ANSI_V, translate),
        resolve('z', ANSI_Z, translate),
    );
    KEY_C.store(keys.0, Ordering::Relaxed);
    KEY_V.store(keys.1, Ordering::Relaxed);
    KEY_Z.store(keys.2, Ordering::Relaxed);
    keys
}

/// Re-read the current layout. Main thread only; elsewhere (and off macOS, or
/// when the layout has no Unicode data) it changes nothing and returns `None`.
pub fn refresh() -> Option<(u16, u16, u16)> {
    #[cfg(target_os = "macos")]
    {
        macos::with_layout(|translate| store_from(translate))
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::os::raw::c_void;

    #[link(name = "Carbon", kind = "framework")]
    extern "C" {
        fn TISCopyCurrentASCIICapableKeyboardLayoutInputSource() -> *mut c_void;
        fn TISGetInputSourceProperty(source: *mut c_void, key: *const c_void) -> *mut c_void;
        static kTISPropertyUnicodeKeyLayoutData: *const c_void;
        fn LMGetKbdType() -> u8;
        /// `OSStatus UCKeyTranslate(const UCKeyboardLayout*, UInt16, UInt16,
        /// UInt32, UInt32, OptionBits, UInt32*, UniCharCount, UniCharCount*,
        /// UniChar[])`; `UniCharCount` is `unsigned long`.
        fn UCKeyTranslate(
            layout: *const c_void,
            virtual_key_code: u16,
            key_action: u16,
            modifier_key_state: u32,
            keyboard_type: u32,
            options: u32,
            dead_key_state: *mut u32,
            max_length: usize,
            actual_length: *mut usize,
            out: *mut u16,
        ) -> i32;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFDataGetBytePtr(data: *const c_void) -> *const u8;
        fn CFRelease(cf: *const c_void);
    }

    extern "C" {
        fn pthread_main_np() -> i32;
    }

    const KEY_ACTION_DOWN: u16 = 0;
    /// `kUCKeyTranslateNoDeadKeysMask`.
    const NO_DEAD_KEYS: u32 = 1;
    /// `cmdKey >> 8`: the Command bit in UCKeyTranslate's modifier byte.
    const CMD_STATE: u32 = 1;

    /// Run `f` with a `(keycode, command_held) -> char` translate function for
    /// the current ASCII-capable layout. `None` off the main thread or when the
    /// input source has no Unicode layout data.
    pub(super) fn with_layout<R>(f: impl FnOnce(&dyn Fn(u16, bool) -> Option<char>) -> R) -> Option<R> {
        unsafe {
            if pthread_main_np() == 0 {
                return None;
            }
            let source = TISCopyCurrentASCIICapableKeyboardLayoutInputSource();
            if source.is_null() {
                return None;
            }
            // A "Get": the property is owned by `source`, which we release.
            let data = TISGetInputSourceProperty(source, kTISPropertyUnicodeKeyLayoutData);
            let result = if data.is_null() {
                None
            } else {
                let layout = CFDataGetBytePtr(data as *const c_void) as *const c_void;
                if layout.is_null() {
                    None
                } else {
                    let keyboard_type = u32::from(LMGetKbdType());
                    let translate = move |code: u16, cmd: bool| -> Option<char> {
                        let mut dead = 0u32;
                        let mut len = 0usize;
                        let mut out = [0u16; 4];
                        let status = UCKeyTranslate(
                            layout,
                            code,
                            KEY_ACTION_DOWN,
                            if cmd { CMD_STATE } else { 0 },
                            keyboard_type,
                            NO_DEAD_KEYS,
                            &mut dead,
                            out.len(),
                            &mut len,
                            out.as_mut_ptr(),
                        );
                        if status != 0 || len != 1 {
                            return None;
                        }
                        char::from_u32(u32::from(out[0]))
                    };
                    Some(f(&translate))
                }
            };
            CFRelease(source as *const c_void);
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A layout as a list of `(keycode, plain char, char with Command held)`.
    fn layout(keys: &'static [(u16, char, char)]) -> impl Fn(u16, bool) -> Option<char> {
        move |code, cmd| {
            keys.iter()
                .find(|(k, _, _)| *k == code)
                .map(|(_, plain, with_cmd)| if cmd { *with_cmd } else { *plain })
        }
    }

    const QWERTY: &[(u16, char, char)] = &[(6, 'z', 'z'), (8, 'c', 'c'), (9, 'v', 'v'), (13, 'w', 'w')];
    // Dvorak: the ANSI C position types "j", V types "k", Z types ";". The
    // letters live at I (34) = c, the period key (47) = v, the slash key (44) = z.
    const DVORAK: &[(u16, char, char)] = &[
        (6, ';', ';'), (8, 'j', 'j'), (9, 'k', 'k'), (34, 'c', 'c'), (47, 'v', 'v'), (44, 'z', 'z'),
    ];
    // "Dvorak - QWERTY ⌘": Dvorak when typing, QWERTY while Command is held.
    const DVORAK_QWERTY_CMD: &[(u16, char, char)] = &[
        (6, ';', 'z'), (8, 'j', 'c'), (9, 'k', 'v'), (34, 'c', 'i'), (47, 'v', '.'), (44, 'z', '/'),
    ];
    // AZERTY: the ANSI Z position types "w"; Z is at the ANSI W position (13).
    const AZERTY: &[(u16, char, char)] = &[(6, 'w', 'w'), (8, 'c', 'c'), (9, 'v', 'v'), (13, 'z', 'z')];
    // QWERTZ: the ANSI Z position types "y"; Z is at the ANSI Y position (16).
    const QWERTZ: &[(u16, char, char)] = &[(6, 'y', 'y'), (8, 'c', 'c'), (9, 'v', 'v'), (16, 'z', 'z')];

    fn keys(l: &'static [(u16, char, char)]) -> (u16, u16, u16) {
        let t = layout(l);
        (resolve('c', ANSI_C, &t), resolve('v', ANSI_V, &t), resolve('z', ANSI_Z, &t))
    }

    #[test]
    fn qwerty_keeps_the_ansi_codes() {
        assert_eq!(keys(QWERTY), (ANSI_C, ANSI_V, ANSI_Z));
    }

    #[test]
    fn dvorak_pastes_with_the_key_that_types_v() {
        // Regression: ⌘ + keycode 9 is ⌘K on Dvorak, so nothing pasted.
        assert_eq!(keys(DVORAK), (34, 47, 44));
    }

    #[test]
    fn dvorak_qwerty_command_uses_the_command_table() {
        assert_eq!(keys(DVORAK_QWERTY_CMD), (ANSI_C, ANSI_V, ANSI_Z));
    }

    #[test]
    fn azerty_and_qwertz_undo_with_the_key_that_types_z() {
        // Regression: keycode 6 is ⌘W on AZERTY (closes the window), ⌘Y on QWERTZ.
        assert_eq!(keys(AZERTY), (ANSI_C, ANSI_V, 13));
        assert_eq!(keys(QWERTZ), (ANSI_C, ANSI_V, 16));
    }

    #[test]
    fn an_unanswering_layout_falls_back_to_ansi() {
        let none = |_: u16, _: bool| None;
        assert_eq!(resolve('v', ANSI_V, &none), ANSI_V);
        assert_eq!(keycode_for_char('v', ANSI_V, |_| None), ANSI_V);
        // A layout with no Latin letters at all (the lookup finds nothing).
        let cyrillic = |_: u16, _: bool| Some('м');
        assert_eq!(resolve('v', ANSI_V, &cyrillic), ANSI_V);
    }

    #[test]
    fn uppercase_output_still_matches() {
        assert_eq!(keycode_for_char('v', ANSI_V, |c| (c == 20).then_some('V')), 20);
    }

    #[test]
    fn refresh_off_the_main_thread_changes_nothing() {
        // Test threads are never the main thread: TIS must not be touched.
        assert_eq!(refresh(), None);
    }
}
