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
//! `display()`, parses a CLI/shorthand string (`parse`), says why a hotkey is
//! unusable (`problem`) and decides whether held modifiers satisfy a chord
//! (`chord_modifiers_match`).
//!
//! There is deliberately no "is this chord taken?" probe. The old one called
//! Carbon's `RegisterEventHotKey` with the wrong signature, and even a correct
//! call does not fail when another app owns the chord and never sees system
//! shortcuts, so "no conflict detected" was a false reassurance.

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

/// The built-in push-to-talk key: Right Option (`kVK_RightOption`). Every
/// consumer that needs "the default hotkey" reads this one constant.
pub const DEFAULT_KEYCODE: i64 = 0x3D;

/// The keycodes that are themselves a modifier key (any side). A hotkey built
/// on one of these is detected by the daemon via `FlagsChanged`, not `KeyDown`.
/// Right-side codes first, then their left counterparts.
const MODIFIER_KEYCODES: [i64; 8] = [
    DEFAULT_KEYCODE, // Right Option
    0x36,            // Right Command
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
    /// CGEvent keycode to watch (e.g. `0x3D` for Right Option, `0x02` for D).
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
            keycode: settings.resolve_hotkey(DEFAULT_KEYCODE),
            modifiers: settings.resolve_hotkey_modifiers(),
        }
        .or_default_if_unusable()
    }

    /// Why this hotkey must not be bound, or `None` when it is fine.
    ///
    /// The event tap swallows every matching key-down system-wide, so a plain
    /// typing key with no modifier ("d", "space", an arrow) would take that key
    /// away from every app. Only a modifier key or F1–F20 may stand alone.
    /// Escape is refused in any chord: the tap handles Escape first (cancel),
    /// so such a hotkey could never fire.
    pub fn problem(&self) -> Option<&'static str> {
        if self.is_modifier_only() {
            return None;
        }
        if self.keycode == ESCAPE_KEYCODE {
            return Some("Escape cancels a dictation, so it cannot be the hotkey");
        }
        const FOUR: u64 = SHIFT | CONTROL | OPTION | COMMAND;
        if self.modifiers & FOUR == 0 && !is_function_key(self.keycode) {
            return Some("add a modifier (cmd, ctrl, option or shift): a plain key would stop typing everywhere");
        }
        None
    }

    /// A hand-edited or env-supplied hotkey that fails [`Hotkey::problem`]
    /// falls back to the default instead of swallowing a typing key.
    fn or_default_if_unusable(self) -> Hotkey {
        match self.problem() {
            None => self,
            Some(why) => {
                eprintln!(
                    "[hotkey] ignoring configured hotkey {} ({why}); using {}",
                    self.display(),
                    modifier_keycode_name(DEFAULT_KEYCODE)
                );
                Hotkey { keycode: DEFAULT_KEYCODE, modifiers: 0 }
            }
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

    /// Parse a shorthand like `"cmd+shift+d"`, `"ctrl+option+space"`, or a bare
    /// key `"f5"`. Parsing is syntactic only; check [`Hotkey::problem`] before
    /// binding the result.
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

/// Read a physical modifier's side-specific state from a FlagsChanged event.
/// The high Option/Command/etc. flags combine both sides: releasing Right
/// Option while Left Option is held still carries OPTION. Device masks come
/// from the macOS SDK's IOKit/hidsystem/IOLLEvent.h. Fall back to the aggregate
/// bit for keyboards that provide no side-specific bits for this modifier.
/// `(own, pair, aggregate)` masks for a physical modifier: the side-specific
/// device bit, both sides' device bits, and the aggregate flag. `None` for a
/// keycode that is not a modifier.
pub fn modifier_key_masks(keycode: i64) -> Option<(u64, u64, u64)> {
    Some(match keycode {
        0x3A => (0x20, 0x60, OPTION),
        0x3D => (0x40, 0x60, OPTION),
        0x37 => (0x08, 0x18, COMMAND),
        0x36 => (0x10, 0x18, COMMAND),
        0x3B => (0x01, 0x2001, CONTROL),
        0x3E => (0x2000, 0x2001, CONTROL),
        0x38 => (0x02, 0x06, SHIFT),
        0x3C => (0x04, 0x06, SHIFT),
        _ => return None,
    })
}

pub fn modifier_key_is_down(keycode: i64, flags: u64) -> bool {
    match modifier_key_masks(keycode) {
        Some((own, pair, aggregate)) => {
            if flags & pair != 0 {
                flags & own != 0
            } else {
                flags & aggregate != 0
            }
        }
        None => false,
    }
}

/// True when an event carried no side-specific device bit for this modifier, so
/// its state had to be read from the aggregate flag — where Left and Right are
/// indistinguishable. A stop diagnosed through this path cannot be attributed
/// to the configured side with certainty, only to "an Option of some side".
pub fn modifier_key_read_via_aggregate(keycode: i64, flags: u64) -> bool {
    matches!(modifier_key_masks(keycode), Some((_, pair, _)) if flags & pair == 0)
}

/// Map a modifier keycode to its human name (used for `display` of a
/// modifier-only hotkey). Unknown codes fall back to `key 0x{keycode:x}`.
pub fn modifier_keycode_name(keycode: i64) -> String {
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

/// The macOS virtual keycodes (`kVK_*` in Carbon's `HIToolbox/Events.h`) for
/// every key a chord may be built on, paired with the lowercase name
/// [`Hotkey::parse`] accepts. The codes are NOT contiguous or alphabetical
/// (`A`=0x00 `S`=0x01 `D`=0x02 `F`=0x03; `1`=0x12 … `0`=0x1D; `F1`=0x7A
/// `F2`=0x78 `F5`=0x60), so both directions read this one table. It names
/// physical key positions (ANSI layout), like every macOS hotkey.
const KEY_TABLE: &[(&str, i64)] = &[
    ("a", 0x00), ("s", 0x01), ("d", 0x02), ("f", 0x03), ("h", 0x04), ("g", 0x05),
    ("z", 0x06), ("x", 0x07), ("c", 0x08), ("v", 0x09), ("b", 0x0B), ("q", 0x0C),
    ("w", 0x0D), ("e", 0x0E), ("r", 0x0F), ("y", 0x10), ("t", 0x11), ("o", 0x1F),
    ("u", 0x20), ("i", 0x22), ("p", 0x23), ("l", 0x25), ("j", 0x26), ("k", 0x28),
    ("n", 0x2D), ("m", 0x2E),
    ("1", 0x12), ("2", 0x13), ("3", 0x14), ("4", 0x15), ("6", 0x16), ("5", 0x17),
    ("9", 0x19), ("7", 0x1A), ("8", 0x1C), ("0", 0x1D),
    ("space", 0x31), ("return", 0x24), ("tab", 0x30), ("escape", 0x35),
    ("left", 0x7B), ("right", 0x7C), ("down", 0x7D), ("up", 0x7E),
    ("f1", 0x7A), ("f2", 0x78), ("f3", 0x63), ("f4", 0x76), ("f5", 0x60),
    ("f6", 0x61), ("f7", 0x62), ("f8", 0x64), ("f9", 0x65), ("f10", 0x6D),
    ("f11", 0x67), ("f12", 0x6F), ("f13", 0x69), ("f14", 0x6B), ("f15", 0x71),
    ("f16", 0x6A), ("f17", 0x40), ("f18", 0x4F), ("f19", 0x50), ("f20", 0x5A),
];

const ESCAPE_KEYCODE: i64 = 0x35;

/// The parseable lowercase name of a non-modifier keycode (`"d"`, `"f5"`,
/// `"space"`), the inverse of [`keycode_for_name`]. `None` for a key outside
/// the table.
pub fn key_name_for_keycode(keycode: i64) -> Option<&'static str> {
    KEY_TABLE
        .iter()
        .find(|(_, code)| *code == keycode)
        .map(|(name, _)| *name)
}

/// True for F1–F20: the only non-modifier keys that may be a hotkey with no
/// modifier held, because nothing types with them.
pub fn is_function_key(keycode: i64) -> bool {
    key_name_for_keycode(keycode).is_some_and(|n| {
        n.len() >= 2 && n.starts_with('f') && n[1..].bytes().all(|b| b.is_ascii_digit())
    })
}

/// Glyph for a non-modifier keycode, used by `display` for the key portion of
/// a chord. Anything outside [`KEY_TABLE`] falls back to `key 0x{keycode:x}`.
fn key_glyph(keycode: i64) -> String {
    match key_name_for_keycode(keycode) {
        Some("space") => "Space".into(),
        Some("return") => "Return".into(),
        Some("tab") => "Tab".into(),
        Some("escape") => "Escape".into(),
        Some("left") => "←".into(),
        Some("right") => "→".into(),
        Some("down") => "↓".into(),
        Some("up") => "↑".into(),
        Some(name) => name.to_ascii_uppercase(),
        None => format!("key 0x{keycode:x}"),
    }
}

/// Map a key word to its macOS virtual keycode, or `None` if unknown. Covers
/// letters, digits, Space, Return/Enter, Tab, Escape/Esc, the four arrows and
/// F1–F20 (see [`KEY_TABLE`]). Case-insensitive.
pub fn keycode_for_name(name: &str) -> Option<i64> {
    let name = name.trim().to_ascii_lowercase();
    let name = match name.as_str() {
        "enter" => "return",
        "esc" => "escape",
        other => other,
    };
    KEY_TABLE
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, code)| *code)
}

/// True when the modifiers held on a key-down satisfy a chord that requires
/// exactly `required` (both are raw CGEventFlags). The four modifier bits are
/// compared for EQUALITY, so `⌘⇧D` does not also fire on (and swallow)
/// `⌘⇧⌥D`. Caps Lock, Fn and the side-specific device bits are ignored. One
/// exception: an extra Shift is allowed when the chord does not itself use
/// Shift, because Shift + hotkey is the transform gesture.
pub fn chord_modifiers_match(flags: u64, required: u64) -> bool {
    const FOUR: u64 = SHIFT | CONTROL | OPTION | COMMAND;
    let held = flags & FOUR;
    let required = required & FOUR;
    held == required || (required & SHIFT == 0 && held == required | SHIFT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_state_distinguishes_each_physical_side() {
        for (left, right, left_bit, right_bit, aggregate) in [
            (0x3A, 0x3D, 0x20, 0x40, OPTION),
            (0x37, 0x36, 0x08, 0x10, COMMAND),
            (0x3B, 0x3E, 0x01, 0x2000, CONTROL),
            (0x38, 0x3C, 0x02, 0x04, SHIFT),
        ] {
            assert!(modifier_key_is_down(left, aggregate | left_bit));
            assert!(!modifier_key_is_down(right, aggregate | left_bit));
            assert!(modifier_key_is_down(right, aggregate | right_bit));
            assert!(!modifier_key_is_down(left, aggregate | right_bit));
            assert!(modifier_key_is_down(right, aggregate | left_bit | right_bit));
            assert!(modifier_key_is_down(right, aggregate)); // aggregate-only keyboard
            assert!(!modifier_key_is_down(right, 0));
        }
    }

    #[test]
    fn aggregate_read_is_flagged_only_when_device_bits_are_absent() {
        // A keyboard that reports device bits: the side is knowable.
        assert!(!modifier_key_read_via_aggregate(0x3D, OPTION | 0x40));
        assert!(!modifier_key_read_via_aggregate(0x3D, OPTION | 0x20));
        // No device bit at all: the aggregate decides, so Left Option would read
        // as Right Option and the read is marked aggregate.
        assert!(modifier_key_read_via_aggregate(0x3D, OPTION));
        assert!(modifier_key_read_via_aggregate(0x3D, 0));
        // Not a modifier keycode.
        assert!(!modifier_key_read_via_aggregate(0x00, OPTION));
    }

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
        assert!(!Hotkey { keycode: 0x02, modifiers: 0 }.is_modifier_only()); // D
        assert!(!Hotkey { keycode: 0x00, modifiers: COMMAND }.is_modifier_only()); // A
        assert!(!Hotkey { keycode: 0x08, modifiers: SHIFT | CONTROL }.is_modifier_only()); // C
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
        // cmd+shift+d → ⌘⇧D (kVK_ANSI_D is 0x02; 0x03 is F)
        assert_eq!(
            Hotkey { keycode: 0x02, modifiers: COMMAND | SHIFT }.display(),
            "⌘⇧D"
        );
        assert_eq!(
            Hotkey { keycode: 0x03, modifiers: COMMAND | SHIFT }.display(),
            "⌘⇧F"
        );
        // ctrl+option+space → ⌃⌥Space
        assert_eq!(
            Hotkey { keycode: 0x31, modifiers: CONTROL | OPTION }.display(),
            "⌃⌥Space"
        );
        // Bare key (no modifiers) renders just the glyph.
        assert_eq!(Hotkey { keycode: 0x31, modifiers: 0 }.display(), "Space");
        assert_eq!(Hotkey { keycode: 0x02, modifiers: 0 }.display(), "D");
        // All four modifiers in the Command→Shift→Control→Option order.
        assert_eq!(
            Hotkey { keycode: 0x0B, modifiers: COMMAND | SHIFT | CONTROL | OPTION }.display(),
            "⌘⇧⌃⌥B"
        );
    }

    #[test]
    fn display_arrows_and_fkeys() {
        assert_eq!(Hotkey { keycode: 0x7B, modifiers: SHIFT }.display(), "⇧←");
        assert_eq!(Hotkey { keycode: 0x7E, modifiers: 0 }.display(), "↑");
        assert_eq!(Hotkey { keycode: 0x7A, modifiers: 0 }.display(), "F1");
        assert_eq!(Hotkey { keycode: 0x60, modifiers: 0 }.display(), "F5");
        assert_eq!(Hotkey { keycode: 0x5A, modifiers: 0 }.display(), "F20");
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
            Some(Hotkey { keycode: 0x02, modifiers: COMMAND | SHIFT })
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
            Some(Hotkey { keycode: 0x02, modifiers: COMMAND | SHIFT })
        );
        // Whitespace around '+' is tolerated.
        assert_eq!(
            Hotkey::parse(" cmd + shift + d "),
            Some(Hotkey { keycode: 0x02, modifiers: COMMAND | SHIFT })
        );
        // alt is an alias for option. kVK_F5 is 0x60 (0x7E is Up Arrow).
        assert_eq!(
            Hotkey::parse("alt+f5"),
            Some(Hotkey { keycode: 0x60, modifiers: OPTION })
        );
        // The recovery chord the audit called out: V is 0x09, not 0x15 ("4").
        assert_eq!(
            Hotkey::parse("ctrl+cmd+v"),
            Some(Hotkey { keycode: 0x09, modifiers: CONTROL | COMMAND })
        );
    }

    #[test]
    fn parse_bare_keys() {
        assert_eq!(Hotkey::parse("f5"), Some(Hotkey { keycode: 0x60, modifiers: 0 }));
        assert_eq!(Hotkey::parse("escape"), Some(Hotkey { keycode: 0x35, modifiers: 0 }));
        assert_eq!(Hotkey::parse("space"), Some(Hotkey { keycode: 0x31, modifiers: 0 }));
        assert_eq!(Hotkey::parse("d"), Some(Hotkey { keycode: 0x02, modifiers: 0 }));
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
        assert_eq!(Hotkey::parse("f0"), None);
        assert_eq!(Hotkey::parse("left+right"), None); // two key words
    }

    /// The full kVK table from Carbon HIToolbox/Events.h. The old code computed
    /// these arithmetically ("d" → 0x03 = F, "f5" → 0x7E = Up, "1" → 0x1E = ]).
    #[test]
    fn keycodes_are_the_real_macos_virtual_keycodes() {
        let expected: &[(&str, i64)] = &[
            ("a", 0x00), ("b", 0x0B), ("c", 0x08), ("d", 0x02), ("e", 0x0E), ("f", 0x03),
            ("g", 0x05), ("h", 0x04), ("i", 0x22), ("j", 0x26), ("k", 0x28), ("l", 0x25),
            ("m", 0x2E), ("n", 0x2D), ("o", 0x1F), ("p", 0x23), ("q", 0x0C), ("r", 0x0F),
            ("s", 0x01), ("t", 0x11), ("u", 0x20), ("v", 0x09), ("w", 0x0D), ("x", 0x07),
            ("y", 0x10), ("z", 0x06),
            ("0", 0x1D), ("1", 0x12), ("2", 0x13), ("3", 0x14), ("4", 0x15), ("5", 0x17),
            ("6", 0x16), ("7", 0x1A), ("8", 0x1C), ("9", 0x19),
            ("f1", 0x7A), ("f2", 0x78), ("f3", 0x63), ("f4", 0x76), ("f5", 0x60),
            ("f6", 0x61), ("f7", 0x62), ("f8", 0x64), ("f9", 0x65), ("f10", 0x6D),
            ("f11", 0x67), ("f12", 0x6F), ("f13", 0x69), ("f14", 0x6B), ("f15", 0x71),
            ("f16", 0x6A), ("f17", 0x40), ("f18", 0x4F), ("f19", 0x50), ("f20", 0x5A),
            ("space", 0x31), ("return", 0x24), ("tab", 0x30), ("escape", 0x35),
            ("left", 0x7B), ("right", 0x7C), ("down", 0x7D), ("up", 0x7E),
        ];
        assert_eq!(expected.len(), KEY_TABLE.len());
        for (name, code) in expected {
            assert_eq!(keycode_for_name(name), Some(*code), "{name}");
            assert_eq!(key_name_for_keycode(*code), Some(*name), "{code:#x}");
        }
    }

    #[test]
    fn key_table_has_no_duplicate_or_modifier_codes() {
        let mut codes: Vec<i64> = KEY_TABLE.iter().map(|(_, c)| *c).collect();
        codes.sort_unstable();
        let before = codes.len();
        codes.dedup();
        assert_eq!(before, codes.len(), "two names share a keycode");
        for code in codes {
            assert!(!MODIFIER_KEYCODES.contains(&code), "{code:#x}");
        }
    }

    #[test]
    fn every_name_round_trips_through_parse_and_display() {
        for (name, code) in KEY_TABLE {
            let hk = Hotkey::parse(&format!("cmd+{name}")).expect(name);
            assert_eq!(hk.keycode, *code);
            assert_eq!(key_name_for_keycode(hk.keycode), Some(*name));
            assert!(!hk.display().contains("0x"), "{name} has a glyph");
        }
    }

    #[test]
    fn keycode_for_name_aliases_and_rejects() {
        assert_eq!(keycode_for_name("A"), Some(0x00)); // case-insensitive
        assert_eq!(keycode_for_name("enter"), Some(0x24));
        assert_eq!(keycode_for_name("esc"), Some(0x35));
        assert_eq!(keycode_for_name("F12"), Some(0x6F));
        assert_eq!(keycode_for_name("f21"), None);
        assert_eq!(keycode_for_name("bogus"), None);
        assert_eq!(keycode_for_name(""), None);
        assert_eq!(keycode_for_name(" "), None);
        assert_eq!(keycode_for_name("é"), None);
        assert_eq!(key_name_for_keycode(0x3D), None); // a modifier, not a key
    }

    #[test]
    fn function_keys_are_recognised() {
        assert!(is_function_key(0x7A)); // F1
        assert!(is_function_key(0x60)); // F5
        assert!(is_function_key(0x5A)); // F20
        assert!(!is_function_key(0x03)); // the letter F
        assert!(!is_function_key(0x7E)); // Up
        assert!(!is_function_key(0x999));
    }

    #[test]
    fn bare_typing_keys_are_refused_but_modifiers_and_fkeys_stand_alone() {
        // Fine: a bare modifier, a bare F-key, any real chord.
        assert_eq!(Hotkey { keycode: 0x3D, modifiers: 0 }.problem(), None);
        assert_eq!(Hotkey::parse("f5").unwrap().problem(), None);
        assert_eq!(Hotkey::parse("cmd+shift+d").unwrap().problem(), None);
        assert_eq!(Hotkey::parse("ctrl+option+space").unwrap().problem(), None);
        assert_eq!(Hotkey::parse("shift+f5").unwrap().problem(), None);
        // Refused: a plain key would be swallowed in every app.
        for bare in ["d", "1", "space", "return", "tab", "left"] {
            assert!(Hotkey::parse(bare).unwrap().problem().is_some(), "{bare}");
        }
        // Caps Lock alone is not a modifier for this purpose.
        assert!(Hotkey { keycode: 0x02, modifiers: CAPS_LOCK }.problem().is_some());
        // An unknown bare keycode (hand-edited file) is refused too.
        assert!(Hotkey { keycode: 0x999, modifiers: 0 }.problem().is_some());
        // Escape never reaches the chord branch of the tap.
        assert!(Hotkey::parse("escape").unwrap().problem().is_some());
        assert!(Hotkey::parse("cmd+option+escape").unwrap().problem().is_some());
    }

    #[test]
    fn unusable_configured_hotkey_falls_back_to_the_default() {
        let bad = Hotkey { keycode: 0x02, modifiers: 0 };
        assert_eq!(
            bad.or_default_if_unusable(),
            Hotkey { keycode: DEFAULT_KEYCODE, modifiers: 0 }
        );
        let good = Hotkey { keycode: 0x02, modifiers: COMMAND | SHIFT };
        assert_eq!(good.or_default_if_unusable(), good);
    }

    #[test]
    fn chord_modifiers_must_be_equal_not_a_superset() {
        let req = COMMAND | SHIFT;
        assert!(chord_modifiers_match(COMMAND | SHIFT, req));
        // Regression: ⌘⇧⌥D used to fire (and swallow) a ⌘⇧D hotkey.
        assert!(!chord_modifiers_match(COMMAND | SHIFT | OPTION, req));
        assert!(!chord_modifiers_match(COMMAND | SHIFT | CONTROL, req));
        assert!(!chord_modifiers_match(COMMAND, req));
        assert!(!chord_modifiers_match(0, req));
        // Caps Lock, Fn (1<<23), non-coalesced (0x100) and device bits are ignored.
        assert!(chord_modifiers_match(req | CAPS_LOCK | (1 << 23) | 0x100 | 0x0A, req));
    }

    #[test]
    fn extra_shift_is_the_transform_gesture_only_when_the_chord_has_no_shift() {
        let req = CONTROL | OPTION;
        assert!(chord_modifiers_match(CONTROL | OPTION | SHIFT, req));
        assert!(!chord_modifiers_match(CONTROL | OPTION | SHIFT | COMMAND, req));
        // A bare F-key hotkey: nothing held, or Shift for transform.
        assert!(chord_modifiers_match(0, 0));
        assert!(chord_modifiers_match(SHIFT, 0));
        assert!(!chord_modifiers_match(COMMAND, 0));
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
