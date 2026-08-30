//! Arbitrary push-to-talk hotkey primitive: a raw keycode + CGEventFlags
//! modifier mask, plus a global conflict check.
//!
//! This is the shared contract for the "what key do I watch" question. The
//! daemon watches a single keycode (historically a bare modifier key like Right
//! Option, detected via [`FlagsChanged`]); this module generalises that to a
//! keycode plus an optional modifier mask so an arbitrary chord (e.g.
//! `⌘⇧D`) can be declared by the user without touching the daemon.
//!
//! [`Settings`](crate::settings::Settings) remains the source of truth at
//! boot (`from_settings` reads it); this module also renders a human-readable
//! `display()`, parses a CLI/shorthand string (`parse`), and can probe whether
//! a chord is already claimed system-wide (`detect_conflict`).

/// Raw [`CGEventFlags`] bit for Caps Lock (not currently used by the daemon,
/// but part of the real flag mask).
pub const CAPS_LOCK: u64 = 1 << 16;
/// Raw [`CGEventFlags`] bit for Shift.
pub const SHIFT: u64 = 1 << 17;
/// Raw [`CGEventFlags`] bit for Control.
pub const CONTROL: u64 = 1 << 18;
/// Raw [`CGEventFlags`] bit for Option (Alt).
pub const OPTION: u64 = 1 << 19;
/// Raw [`CGEventFlags`] bit for Command.
pub const COMMAND: u64 = 1 << 20;

/// Label→bit pairs for the four configurable modifiers, in the order the menu
/// will show them. Used by the UI to present a masked hotkey as a set of
/// toggleable modifiers.
pub fn modifier_names() -> &'static [(&'static str, u64)] {
    &[
        ("Shift", SHIFT),
        ("Control", CONTROL),
        ("Option", OPTION),
        ("Command", COMMAND),
    ]
}

/// The keycodes that are themselves a modifier key (any side). A hotkey built
/// on one of these is detected by the daemon via `FlagsChanged`, not `KeyDown`.
/// Right-side codes first, then their left counterparts.
const MODIFIER_KEYCODES: [i64; 8] = [
    0x3D, // Right Option
    0x36, // Right Command
    0x3E, // Right Control
    0x3C, // Right Shift
    0x3A, // Option
    0x37, // Command
    0x3B, // Control
    0x38, // Shift
];

/// A push-to-talk hotkey: a CGEvent keycode plus a CGEventFlags modifier mask.
///
/// `modifiers == 0` with a modifier `keycode` is the historical bare-modifier
/// hotkey (default: Right Option). A non-zero `modifiers` turns it into a chord
/// where the press is only recognised when the listed modifiers are also held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    /// CGEvent keycode to watch (e.g. `0x3D` for Right Option, `0x03` for D).
    pub keycode: i64,
    /// CGEventFlags modifier bitmask (`SHIFT | CONTROL | OPTION | COMMAND`).
    pub modifiers: u64,
}

impl Hotkey {
    /// Read the hotkey from the current settings, with the usual
    /// **env > settings > built-in default** precedence. The default is a bare
    /// Right Option (`0x3D`) with no modifiers — the historical push-to-talk
    /// key. See [`crate::settings::Settings::resolve_hotkey`] and
    /// [`crate::settings::Settings::resolve_hotkey_modifiers`].
    pub fn from_settings() -> Hotkey {
        let settings = crate::settings::Settings::load();
        Hotkey {
            keycode: settings.resolve_hotkey(0x3D),
            modifiers: settings.resolve_hotkey_modifiers(),
        }
    }

    /// True when the hotkey is a bare modifier key (no separate key to report).
    ///
    /// A modifier-only hotkey is observed by the daemon via `FlagsChanged`
    /// (the state-change event emitted when a modifier is pressed/released),
    /// not via a normal `KeyDown`. The daemon must therefore branch on this at
    /// boot to pick the right event type.
    pub fn is_modifier_only(&self) -> bool {
        MODIFIER_KEYCODES.contains(&self.keycode)
    }

    /// Human-readable rendering.
    ///
    /// For a modifier-only hotkey this is the key's name ("Right Option",
    /// "Command", …). For a chord it renders the modifier symbols in
    /// **Command, Shift, Control, Option** order, then the key glyph — e.g.
    /// `⌘⇧D` for `cmd+shift+d`, `⌃⌥Space` for `ctrl+option+space`.
    pub fn display(&self) -> String {
        if self.is_modifier_only() {
            return modifier_keycode_name(self.keycode);
        }
        let mut out = String::new();
        if self.modifiers & COMMAND != 0 {
            out.push('⌘');
        }
        if self.modifiers & SHIFT != 0 {
            out.push('⇧');
        }
        if self.modifiers & CONTROL != 0 {
            out.push('⌃');
        }
        if self.modifiers & OPTION != 0 {
            out.push('⌥');
        }
        out.push_str(&key_glyph(self.keycode));
        out
    }

    /// Parse a shorthand like `"cmd+shift+d"`, `"ctrl+option+space"`,
    /// `"command+option+escape"`, or a bare key `"f5"` / `"escape"`.
    ///
    /// Modifier words (`cmd`/`command`, `ctrl`/`control`, `alt`/`option`,
    /// `shift`) are joined with `+` to a single key word; a bare key word with
    /// no modifiers is also accepted. Matching is case-insensitive and
    /// whitespace-tolerant around the `+`. Returns `None` on anything
    /// unrecognized (unknown key, >1 key, a modifier with no key, etc.).
    pub fn parse(s: &str) -> Option<Hotkey> {
        let joined = s.trim();
        if joined.is_empty() {
            return None;
        }
        let joined = joined.to_ascii_lowercase();

        let mut modifiers = 0u64;
        let mut key: Option<i64> = None;
        for part in joined.split('+') {
            let part = part.trim();
            if part.is_empty() {
                return None;
            }
            match part {
                "cmd" | "command" => modifiers |= COMMAND,
                "ctrl" | "control" => modifiers |= CONTROL,
                "alt" | "option" => modifiers |= OPTION,
                "shift" => modifiers |= SHIFT,
                _ => {
                    if key.is_some() {
                        return None; // more than one key word
                    }
                    key = keycode_for_name(part);
                    if key.is_none() {
                        return None;
                    }
                }
            }
        }
        let keycode = key?;
        Some(Hotkey { keycode, modifiers })
    }
}

/// Map a modifier keycode to its human name (used for `display` of a
/// modifier-only hotkey). Unknown codes fall back to `key 0x{keycode:x}`.
fn modifier_keycode_name(keycode: i64) -> String {
    match keycode {
        0x3D => "Right Option".into(),
        0x36 => "Right Command".into(),
        0x3E => "Right Control".into(),
        0x3C => "Right Shift".into(),
        0x3A => "Option".into(),
        0x37 => "Command".into(),
        0x3B => "Control".into(),
        0x38 => "Shift".into(),
        _ => format!("key 0x{keycode:x}"),
    }
}

/// Best-effort glyph for a non-modifier keycode, used by `display` for the key
/// portion of a chord. Covers letters, digits, Space/Return/Tab/Escape, the
/// four arrows, and F1–F20; anything else falls back to `key 0x{keycode:x}`.
///
/// Named keys are matched before the F-key and digit ranges because the arrow
/// keycodes (`0x7B..0x7E`) sit inside the F1–F20 range and `0x24` (Return) sits
/// inside the digit range.
fn key_glyph(keycode: i64) -> String {
    match keycode {
        0x31 => return "Space".into(),
        0x24 => return "Return".into(),
        0x30 => return "Tab".into(),
        0x35 => return "Escape".into(),
        0x7B => return "←".into(),
        0x7C => return "→".into(),
        0x7D => return "↓".into(),
        0x7E => return "↑".into(),
        _ => {}
    }
    if (0x7A..=0x8D).contains(&keycode) {
        return format!("F{}", keycode - 0x7A + 1);
    }
    if (0x00..=0x19).contains(&keycode) {
        return ((b'A' + keycode as u8) as char).to_string();
    }
    if (0x1D..=0x26).contains(&keycode) {
        return ((b'0' + (keycode - 0x1D) as u8) as char).to_string();
    }
    format!("key 0x{keycode:x}")
}

/// Map a key word to a standard macOS virtual keycode, or `None` if unknown.
///
/// Covers single letters (`A`–`Z` → `0x00..0x19`), digits (`0`–`9` →
/// `0x1D..0x26`), and the named keys Space (`0x31`), Return/Enter (`0x24`),
/// Tab (`0x30`), Escape (`0x35`), the four arrows (`0x7B..0x7E`), and F1–F20
/// (`0x7A..0x8D`). Case-insensitive.
pub fn keycode_for_name(name: &str) -> Option<i64> {
    let name = name.trim().to_ascii_lowercase();
    if name.is_empty() {
        return None;
    }
    if name.len() == 1 {
        let c = name.as_bytes()[0];
        if c.is_ascii_lowercase() {
            return Some((c - b'a') as i64);
        }
        if c.is_ascii_digit() {
            return Some(0x1D + (c - b'0') as i64);
        }
        return None;
    }
    match name.as_str() {
        "space" => Some(0x31),
        "return" | "enter" => Some(0x24),
        "tab" => Some(0x30),
        "escape" | "esc" => Some(0x35),
        "left" => Some(0x7B),
        "right" => Some(0x7C),
        "down" => Some(0x7D),
        "up" => Some(0x7E),
        _ => {
            let n = name.strip_prefix('f')?.parse::<i64>().ok()?;
            if (1..=20).contains(&n) {
                Some(0x7A + n - 1)
            } else {
                None
            }
        }
    }
}

/// Carbon global-hotkey bindings (macOS + `ax-inject` only).
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
mod carbon {
    // `OSStatus RegisterEventHotKey(UInt32 hotKeyCode, UInt32 hotKeyModifiers,
    // EventHotKeyID hotKeyID, EventTargetRef target, OptionBits options,
    // EventHotKeyRef *outRef);`
    //
    // Exposed with the simplified pointer forms used by [`detect_conflict`]:
    // the ID and (ignored) out-parameter are passed as opaque pointers so callers
    // can hand a small fixed value without modelling the full EventHotKeyID
    // struct. Returns 0 on success, `-9878` (`kEventHotKeyExistsErr`) when an
    // app already owns the combo.
    #[link(name = "Carbon", kind = "framework")]
    extern "C" {
        pub fn RegisterEventHotKey(
            hotKeyCode: u32,
            hotKeyModifiers: u32,
            hotKeyID: *const std::ffi::c_void,
            target: *const std::ffi::c_void,
            options: u32,
            refcon: *mut std::ffi::c_void,
        ) -> i32;
        pub fn UnregisterEventHotKey(hotKeyID: *const std::ffi::c_void) -> i32;
    }
}

/// Check whether a hotkey is already claimed system-wide by another app.
///
/// Registers the combo transiently via Carbon's `RegisterEventHotKey` and
/// immediately unregisters it. Returns `None` when it is free (or the check
/// cannot be made), or `Some(message)` when another app already owns it.
///
/// Modifier-only hotkeys cannot be registered this way (they are seen via
/// `FlagsChanged`, not as a chord), so the check short-circuits to `None` for
/// them. This function requires a real macOS session and is not unit-tested.
#[cfg(all(target_os = "macos", feature = "ax-inject"))]
pub fn detect_conflict(hotkey: &Hotkey) -> Option<String> {
    if hotkey.is_modifier_only() {
        return None;
    }

    // Carbon modifier bit encoding: cmdKey=256, shiftKey=512, optionKey=2048,
    // controlKey=4096.
    let mut carbon_modifiers = 0u32;
    if hotkey.modifiers & COMMAND != 0 {
        carbon_modifiers |= 256;
    }
    if hotkey.modifiers & SHIFT != 0 {
        carbon_modifiers |= 512;
    }
    if hotkey.modifiers & OPTION != 0 {
        carbon_modifiers |= 2048;
    }
    if hotkey.modifiers & CONTROL != 0 {
        carbon_modifiers |= 4096;
    }

    // A fixed, arbitrary hotkey id. We only need it to attempt a transient
    // registration, then release it immediately.
    let hotkey_id: u32 = 42;
    let id_ptr = &hotkey_id as *const u32 as *const std::ffi::c_void;

    let status = unsafe {
        carbon::RegisterEventHotKey(
            hotkey.keycode as u32,
            carbon_modifiers,
            id_ptr,
            std::ptr::null(), // null == application's default event target
            0,
            std::ptr::null_mut(),
        )
    };

    if status == 0 {
        // We own it transiently; release it so we don't keep the registration.
        unsafe {
            carbon::UnregisterEventHotKey(id_ptr);
        }
        None
    } else if status == -9878 {
        // kEventHotKeyExistsErr: another app already owns this combo.
        Some(format!(
            "The hotkey {} is already in use by another app.",
            hotkey.display()
        ))
    } else {
        // Some unrelated failure (e.g. the keycode can't be registered at all).
        // We can't prove a conflict, so report the combo as free.
        None
    }
}

/// Stub on non-macOS or without `ax-inject`, so callers can compile uniformly
/// and always get a `detect_conflict` symbol.
#[cfg(not(all(target_os = "macos", feature = "ax-inject")))]
pub fn detect_conflict(_hotkey: &Hotkey) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_names_cover_the_four_configurable_modifiers() {
        let names = modifier_names();
        let labels: Vec<&str> = names.iter().map(|(l, _)| *l).collect();
        assert_eq!(labels, vec!["Shift", "Control", "Option", "Command"]);
        let bits: Vec<u64> = names.iter().map(|(_, b)| *b).collect();
        assert!(bits.contains(&SHIFT));
        assert!(bits.contains(&CONTROL));
        assert!(bits.contains(&OPTION));
        assert!(bits.contains(&COMMAND));
    }

    #[test]
    fn is_modifier_only_detects_all_eight_modifier_keycodes() {
        for code in [0x3D, 0x36, 0x3E, 0x3C, 0x3A, 0x37, 0x3B, 0x38] {
            assert!(
                Hotkey { keycode: code, modifiers: 0 }.is_modifier_only(),
                "expected {code:#x} to be modifier-only"
            );
        }
        // A regular key is never modifier-only, even with a modifier mask.
        assert!(!Hotkey { keycode: 0x03, modifiers: 0 }.is_modifier_only()); // D
        assert!(!Hotkey { keycode: 0x00, modifiers: COMMAND }.is_modifier_only()); // A
        assert!(!Hotkey { keycode: 0x02, modifiers: SHIFT | CONTROL }.is_modifier_only()); // C
    }

    #[test]
    fn display_modifier_only_names() {
        assert_eq!(Hotkey { keycode: 0x3D, modifiers: 0 }.display(), "Right Option");
        assert_eq!(Hotkey { keycode: 0x36, modifiers: 0 }.display(), "Right Command");
        assert_eq!(Hotkey { keycode: 0x3E, modifiers: 0 }.display(), "Right Control");
        assert_eq!(Hotkey { keycode: 0x3C, modifiers: 0 }.display(), "Right Shift");
        assert_eq!(Hotkey { keycode: 0x3A, modifiers: 0 }.display(), "Option");
        assert_eq!(Hotkey { keycode: 0x37, modifiers: 0 }.display(), "Command");
        assert_eq!(Hotkey { keycode: 0x3B, modifiers: 0 }.display(), "Control");
        assert_eq!(Hotkey { keycode: 0x38, modifiers: 0 }.display(), "Shift");
        // Modifier-only precedence: even with a modifier mask set, a modifier
        // keycode displays as its name, not a chord.
        assert_eq!(
            Hotkey { keycode: 0x3D, modifiers: CONTROL }.display(),
            "Right Option"
        );
        // Unknown modifier keycode falls back to hex.
        assert_eq!(Hotkey { keycode: 0x999, modifiers: 0 }.display(), "key 0x999");
    }

    #[test]
    fn display_chord_forms() {
        // cmd+shift+d → ⌘⇧D
        assert_eq!(
            Hotkey { keycode: 0x03, modifiers: COMMAND | SHIFT }.display(),
            "⌘⇧D"
        );
        // ctrl+option+space → ⌃⌥Space
        assert_eq!(
            Hotkey { keycode: 0x31, modifiers: CONTROL | OPTION }.display(),
            "⌃⌥Space"
        );
        // Bare key (no modifiers) renders just the glyph.
        assert_eq!(Hotkey { keycode: 0x31, modifiers: 0 }.display(), "Space");
        assert_eq!(Hotkey { keycode: 0x03, modifiers: 0 }.display(), "D");
        // All four modifiers in the Command→Shift→Control→Option order.
        assert_eq!(
            Hotkey { keycode: 0x01, modifiers: COMMAND | SHIFT | CONTROL | OPTION }.display(),
            "⌘⇧⌃⌥B"
        );
    }

    #[test]
    fn display_arrows_and_fkeys() {
        assert_eq!(Hotkey { keycode: 0x7B, modifiers: SHIFT }.display(), "⇧←");
        assert_eq!(Hotkey { keycode: 0x7E, modifiers: 0 }.display(), "↑");
        assert_eq!(Hotkey { keycode: 0x7A, modifiers: 0 }.display(), "F1");
        assert_eq!(Hotkey { keycode: 0x8D, modifiers: 0 }.display(), "F20");
        assert_eq!(
            Hotkey { keycode: 0x7A, modifiers: OPTION }.display(),
            "⌥F1"
        );
        // Unknown key in a chord falls back to hex.
        assert_eq!(
            Hotkey { keycode: 0x999, modifiers: SHIFT }.display(),
            "⇧key 0x999"
        );
    }

    #[test]
    fn parse_chords_and_case_insensitivity() {
        assert_eq!(
            Hotkey::parse("cmd+shift+d"),
            Some(Hotkey { keycode: 0x03, modifiers: COMMAND | SHIFT })
        );
        assert_eq!(
            Hotkey::parse("ctrl+option+space"),
            Some(Hotkey { keycode: 0x31, modifiers: CONTROL | OPTION })
        );
        assert_eq!(
            Hotkey::parse("command+option+escape"),
            Some(Hotkey { keycode: 0x35, modifiers: COMMAND | OPTION })
        );
        // Case-insensitive.
        assert_eq!(
            Hotkey::parse("CMd+ShIfT+D"),
            Some(Hotkey { keycode: 0x03, modifiers: COMMAND | SHIFT })
        );
        // Whitespace around '+' is tolerated.
        assert_eq!(
            Hotkey::parse(" cmd + shift + d "),
            Some(Hotkey { keycode: 0x03, modifiers: COMMAND | SHIFT })
        );
        // alt is an alias for option.
        assert_eq!(
            Hotkey::parse("alt+f5"),
            Some(Hotkey { keycode: 0x7A + 4, modifiers: OPTION })
        );
    }

    #[test]
    fn parse_bare_keys() {
        assert_eq!(
            Hotkey::parse("f5"),
            Some(Hotkey { keycode: 0x7A + 4, modifiers: 0 })
        );
        assert_eq!(Hotkey::parse("escape"), Some(Hotkey { keycode: 0x35, modifiers: 0 }));
        assert_eq!(Hotkey::parse("space"), Some(Hotkey { keycode: 0x31, modifiers: 0 }));
        assert_eq!(Hotkey::parse("d"), Some(Hotkey { keycode: 0x03, modifiers: 0 }));
        assert_eq!(Hotkey::parse("LEFT"), Some(Hotkey { keycode: 0x7B, modifiers: 0 }));
    }

    #[test]
    fn parse_rejects_unknown() {
        assert_eq!(Hotkey::parse(""), None);
        assert_eq!(Hotkey::parse("   "), None);
        assert_eq!(Hotkey::parse("bogus"), None);
        assert_eq!(Hotkey::parse("cmd+"), None); // modifier with no key
        assert_eq!(Hotkey::parse("cmd+shift"), None); // modifiers only, no key
        assert_eq!(Hotkey::parse("d+e"), None); // two key words
        assert_eq!(Hotkey::parse("cmd+bogus"), None);
        assert_eq!(Hotkey::parse("f21"), None); // F-key out of range
        assert_eq!(Hotkey::parse("left+right"), None); // two key words
    }

    #[test]
    fn keycode_for_name_lookup() {
        assert_eq!(keycode_for_name("a"), Some(0x00));
        assert_eq!(keycode_for_name("z"), Some(0x19));
        assert_eq!(keycode_for_name("A"), Some(0x00)); // case-insensitive
        assert_eq!(keycode_for_name("0"), Some(0x1D));
        assert_eq!(keycode_for_name("9"), Some(0x26));
        assert_eq!(keycode_for_name("space"), Some(0x31));
        assert_eq!(keycode_for_name("return"), Some(0x24));
        assert_eq!(keycode_for_name("enter"), Some(0x24));
        assert_eq!(keycode_for_name("tab"), Some(0x30));
        assert_eq!(keycode_for_name("escape"), Some(0x35));
        assert_eq!(keycode_for_name("esc"), Some(0x35));
        assert_eq!(keycode_for_name("left"), Some(0x7B));
        assert_eq!(keycode_for_name("right"), Some(0x7C));
        assert_eq!(keycode_for_name("down"), Some(0x7D));
        assert_eq!(keycode_for_name("up"), Some(0x7E));
        assert_eq!(keycode_for_name("f1"), Some(0x7A));
        assert_eq!(keycode_for_name("f20"), Some(0x8D));
        assert_eq!(keycode_for_name("f21"), None);
        assert_eq!(keycode_for_name("bogus"), None);
        assert_eq!(keycode_for_name(""), None);
        assert_eq!(keycode_for_name(" "), None);
    }

    #[test]
    fn from_settings_defaults_to_right_option() {
        // Hermetic: only assert default behaviour when the env knobs (which are
        // process-global) aren't set. Matches settings.rs's guards.
        if std::env::var_os("DICTATE_HOTKEY_KEYCODE").is_none()
            && std::env::var_os("DICTATE_HOTKEY_MODIFIERS").is_none()
        {
            let h = Hotkey::from_settings();
            assert_eq!(h.keycode, 0x3D);
            assert_eq!(h.modifiers, 0);
            assert!(h.is_modifier_only());
            assert_eq!(h.display(), "Right Option");
        }
    }
}
