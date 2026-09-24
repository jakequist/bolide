//! Key names → X11 keysyms.
//!
//! CONTRACT — `bolide-rfb`'s to finish; the signatures below are fixed because
//! `bolide-server` parses a computer-use `key` action through them.
//!
//! # The spelling bolide accepts
//!
//! The names are the ones `XStringToKeysym` takes — the same strings `xdotool key` is
//! fed, which is what every computer-use tool's `key` action already emits: `Return`,
//! `Escape`, `Tab`, `BackSpace`, `Delete`, `Page_Up`, `Page_Down`, `Home`, `End`,
//! `Left`/`Up`/`Right`/`Down`, `F1`..`F12`, `Insert`, `Menu`, `Num_Lock`,
//! `Scroll_Lock`, `Caps_Lock`, the modifiers `Shift_L`/`Shift_R`, `Control_L`/`_R`,
//! `Alt_L`/`_R`, `Meta_L`/`_R`, `Super_L`/`_R`, and `space`.
//!
//! Models do not write them consistently, so resolution is **case-insensitive with
//! aliases**: `enter`, `esc`, `del`, `backspace`, `arrowleft`…`arrowdown`, `pageup`,
//! `pagedown`, `ctrl`, `control`, `alt`, `option`, `cmd`, `command`, `win`, `windows`,
//! `super`, `meta`, `shift`, `spacebar`. A bare modifier name means the **left** one.
//!
//! `cmd` deserves its own line, because the Mac is why bolide exists: it resolves to
//! `Meta_L` (0xffe7), which is what a Mac VNC server maps to Command. `cmd+space`
//! therefore opens Spotlight on the far end, and that is the acceptance a Mac user
//! will try first.
//!
//! A single printable character is its own code point when it is Latin-1, and
//! `0x01000000 + codepoint` above that (the Unicode keysym range).
//!
//! # Chords
//!
//! [`parse_chord`] splits on `+` and returns the keysyms **in press order**; the caller
//! presses them in order and releases in reverse, so `ctrl+shift+t` holds both
//! modifiers while `t` goes down. A literal `+` is spelled `plus`, and a trailing `+`
//! (as in `ctrl++`) means the plus key — a lone `+` segment is the character, not a
//! separator.
//!
//! # Tests this module owes
//!
//! Each named key's exact keysym (they are ABI, not preference), the case-insensitive
//! and alias paths, a printable char, a non-Latin-1 char, `ctrl+c` ordering,
//! `cmd+space`, `ctrl++`, an unknown name returning `None`/`Err`, and — the one that
//! catches a copy-paste table — that no two distinct names collide onto a keysym that
//! is not deliberately shared.

use std::fmt;

/// The one table. Both directions are derived from it — [`keysym_for`] reads it
/// forwards, [`name_for_keysym`] reads it backwards, and `ALIASES` can only point at a
/// name that appears in it (there is a test that says so). A second table would be a
/// second chance to be wrong about `Meta_L`.
const CANONICAL: &[(&str, u32)] = &[
    ("BackSpace", 0xff08),
    ("Tab", 0xff09),
    ("Return", 0xff0d),
    ("Pause", 0xff13),
    ("Scroll_Lock", 0xff14),
    ("Escape", 0xff1b),
    ("Home", 0xff50),
    ("Left", 0xff51),
    ("Up", 0xff52),
    ("Right", 0xff53),
    ("Down", 0xff54),
    ("Page_Up", 0xff55),
    ("Page_Down", 0xff56),
    ("End", 0xff57),
    ("Print", 0xff61),
    ("Insert", 0xff63),
    ("Menu", 0xff67),
    ("Num_Lock", 0xff7f),
    ("F1", 0xffbe),
    ("F2", 0xffbf),
    ("F3", 0xffc0),
    ("F4", 0xffc1),
    ("F5", 0xffc2),
    ("F6", 0xffc3),
    ("F7", 0xffc4),
    ("F8", 0xffc5),
    ("F9", 0xffc6),
    ("F10", 0xffc7),
    ("F11", 0xffc8),
    ("F12", 0xffc9),
    ("Shift_L", 0xffe1),
    ("Shift_R", 0xffe2),
    ("Control_L", 0xffe3),
    ("Control_R", 0xffe4),
    ("Caps_Lock", 0xffe5),
    ("Meta_L", 0xffe7),
    ("Meta_R", 0xffe8),
    ("Alt_L", 0xffe9),
    ("Alt_R", 0xffea),
    ("Super_L", 0xffeb),
    ("Super_R", 0xffec),
    ("Delete", 0xffff),
    // Latin-1 keysyms whose X11 *name* is worth accepting: `space` because the docs
    // promise it, `plus` because `+` is the chord separator and so cannot be spelled
    // literally, `minus` for symmetry with it.
    ("space", 0x20),
    ("plus", 0x2b),
    ("minus", 0x2d),
];

/// Alias → canonical **name**, never alias → keysym: an alias carrying its own number
/// could disagree with `CANONICAL` about what `cmd` means. Lookup is on an
/// ASCII-lowercased spelling, so only lowercase entries belong here.
const ALIASES: &[(&str, &str)] = &[
    ("enter", "Return"),
    ("ret", "Return"),
    ("esc", "Escape"),
    ("del", "Delete"),
    ("ins", "Insert"),
    ("arrowleft", "Left"),
    ("arrowright", "Right"),
    ("arrowup", "Up"),
    ("arrowdown", "Down"),
    ("pageup", "Page_Up"),
    ("pagedown", "Page_Down"),
    ("pgup", "Page_Up"),
    ("pgdn", "Page_Down"),
    ("pgdown", "Page_Down"),
    // A bare modifier is the LEFT one — that is the key a human means by "ctrl".
    ("ctrl", "Control_L"),
    ("control", "Control_L"),
    ("alt", "Alt_L"),
    ("option", "Alt_L"),
    ("opt", "Alt_L"),
    // The Mac is why bolide exists: a Mac VNC server maps Meta to Command, so
    // `cmd+space` has to become Meta_L or Spotlight never opens.
    ("cmd", "Meta_L"),
    ("command", "Meta_L"),
    ("meta", "Meta_L"),
    ("win", "Super_L"),
    ("windows", "Super_L"),
    ("super", "Super_L"),
    ("shift", "Shift_L"),
    ("spacebar", "space"),
    ("capslock", "Caps_Lock"),
    ("numlock", "Num_Lock"),
    ("scrolllock", "Scroll_Lock"),
    ("printscreen", "Print"),
    ("prtsc", "Print"),
    ("break", "Pause"),
];

/// A key name bolide does not know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownKey(pub String);

impl fmt::Display for UnknownKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown key name: {}", self.0)
    }
}

impl std::error::Error for UnknownKey {}

fn canonical_keysym(name: &str) -> Option<u32> {
    CANONICAL.iter().find(|(n, _)| *n == name).map(|(_, k)| *k)
}

/// The keysym for one key name (`"Return"`, `"ctrl"`, `"F5"`, `"a"`, `"ArrowLeft"`).
pub fn keysym_for(name: &str) -> Option<u32> {
    if name.is_empty() {
        return None;
    }
    // Exact X11 spelling first, so `Return` costs one comparison.
    if let Some(k) = canonical_keysym(name) {
        return Some(k);
    }
    // A single character is itself — `a`, `+`, `é`, `漢`.
    let mut chars = name.chars();
    let first = chars.next()?;
    if chars.next().is_none() {
        return Some(keysym_for_char(first));
    }
    if let Some((_, k)) = CANONICAL.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
        return Some(*k);
    }
    let lower = name.to_ascii_lowercase();
    ALIASES
        .iter()
        .find(|(a, _)| *a == lower)
        .and_then(|(_, canon)| canonical_keysym(canon))
}

/// The keysym for one character, as `type` needs it.
pub fn keysym_for_char(c: char) -> u32 {
    // The control characters a typed string actually contains. Their code points are
    // not keysyms, and a server handed 0x0a types nothing at all.
    match c {
        '\n' | '\r' => return 0xff0d,
        '\t' => return 0xff09,
        '\u{8}' => return 0xff08,
        '\u{7f}' => return 0xffff,
        _ => {}
    }
    let cp = c as u32;
    if cp < 0x100 {
        cp
    } else {
        0x0100_0000 + cp
    }
}

/// The canonical X11 name for a keysym, for logs and errors. `None` when unnamed.
pub fn name_for_keysym(keysym: u32) -> Option<&'static str> {
    CANONICAL
        .iter()
        .find(|(_, k)| *k == keysym)
        .map(|(n, _)| *n)
}

/// Split a chord like `"ctrl+shift+t"` into keysyms in press order.
pub fn parse_chord(spec: &str) -> std::result::Result<Vec<u32>, UnknownKey> {
    let segments: Vec<&str> = spec.split('+').collect();
    let mut out = Vec::with_capacity(segments.len());
    let mut i = 0;
    while i < segments.len() {
        let seg = segments[i];
        if seg.is_empty() {
            // `+` is both the separator and a key. A literal one leaves *two* empty
            // segments behind (`"ctrl++"` → `["ctrl", "", ""]`), which is how the plus
            // key is told apart from a trailing separator.
            if segments.get(i + 1) == Some(&"") {
                out.push(canonical_keysym("plus").expect("plus is in the table"));
                i += 2;
                continue;
            }
            return Err(UnknownKey(String::new()));
        }
        match keysym_for(seg) {
            Some(k) => out.push(k),
            None => return Err(UnknownKey(seg.to_string())),
        }
        i += 1;
    }
    if out.is_empty() {
        return Err(UnknownKey(spec.to_string()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These numbers are ABI: `bolide-server` hands them to a desktop, and a wrong one
    /// is a keypress that lands somewhere else. Spelled out rather than derived.
    #[test]
    fn the_named_keys_have_their_x11_keysyms() {
        for (name, expected) in [
            ("Return", 0xff0d),
            ("Escape", 0xff1b),
            ("Tab", 0xff09),
            ("BackSpace", 0xff08),
            ("Delete", 0xffff),
            ("Page_Up", 0xff55),
            ("Page_Down", 0xff56),
            ("Home", 0xff50),
            ("End", 0xff57),
            ("Left", 0xff51),
            ("Up", 0xff52),
            ("Right", 0xff53),
            ("Down", 0xff54),
            ("Insert", 0xff63),
            ("Caps_Lock", 0xffe5),
            ("Num_Lock", 0xff7f),
            ("Scroll_Lock", 0xff14),
            ("Menu", 0xff67),
            ("Shift_L", 0xffe1),
            ("Shift_R", 0xffe2),
            ("Control_L", 0xffe3),
            ("Control_R", 0xffe4),
            ("Meta_L", 0xffe7),
            ("Meta_R", 0xffe8),
            ("Alt_L", 0xffe9),
            ("Alt_R", 0xffea),
            ("Super_L", 0xffeb),
            ("Super_R", 0xffec),
            ("space", 0x20),
        ] {
            assert_eq!(keysym_for(name), Some(expected), "{name}");
        }
        let fkeys = [
            "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12",
        ];
        for (i, name) in fkeys.iter().enumerate() {
            assert_eq!(keysym_for(name), Some(0xffbe + i as u32), "{name}");
        }
        assert_eq!(keysym_for("F12"), Some(0xffc9));
    }

    #[test]
    fn names_resolve_case_insensitively() {
        for spelling in ["Return", "return", "RETURN", "ReTuRn"] {
            assert_eq!(keysym_for(spelling), Some(0xff0d), "{spelling}");
        }
        assert_eq!(keysym_for("f5"), Some(0xffc2));
        assert_eq!(keysym_for("page_up"), Some(0xff55));
        assert_eq!(keysym_for("SHIFT_l"), Some(0xffe1));
    }

    #[test]
    fn aliases_resolve_and_a_bare_modifier_is_the_left_one() {
        for (alias, expected) in [
            ("enter", 0xff0d),
            ("esc", 0xff1b),
            ("del", 0xffff),
            ("backspace", 0xff08),
            ("arrowleft", 0xff51),
            ("arrowright", 0xff53),
            ("arrowup", 0xff52),
            ("arrowdown", 0xff54),
            ("pageup", 0xff55),
            ("pagedown", 0xff56),
            ("ctrl", 0xffe3),
            ("control", 0xffe3),
            ("CTRL", 0xffe3),
            ("alt", 0xffe9),
            ("option", 0xffe9),
            ("win", 0xffeb),
            ("windows", 0xffeb),
            ("super", 0xffeb),
            ("meta", 0xffe7),
            ("shift", 0xffe1),
            ("spacebar", 0x20),
        ] {
            assert_eq!(keysym_for(alias), Some(expected), "{alias}");
        }
    }

    /// The Mac is why bolide exists, and this is the first thing a Mac user types.
    #[test]
    fn cmd_is_meta_l_so_cmd_space_opens_spotlight() {
        assert_eq!(keysym_for("cmd"), Some(0xffe7));
        assert_eq!(keysym_for("command"), Some(0xffe7));
        assert_eq!(parse_chord("cmd+space").unwrap(), vec![0xffe7, 0x20]);
    }

    #[test]
    fn a_printable_character_is_its_own_code_point() {
        assert_eq!(keysym_for("a"), Some(0x61));
        assert_eq!(keysym_for("A"), Some(0x41));
        assert_eq!(keysym_for("7"), Some(0x37));
        assert_eq!(keysym_for_char(' '), 0x20);
        assert_eq!(keysym_for_char('é'), 0xe9);
    }

    #[test]
    fn a_non_latin1_character_uses_the_unicode_keysym_range() {
        assert_eq!(keysym_for_char('漢'), 0x0100_0000 + 0x6f22);
        assert_eq!(keysym_for_char('€'), 0x0100_0000 + 0x20ac);
        assert_eq!(keysym_for("😀"), Some(0x0100_0000 + 0x1f600));
    }

    /// `bolide-server`'s `type` action feeds whole strings through here; a newline that
    /// stayed 0x0a would type nothing on the far end.
    #[test]
    fn the_control_characters_a_typed_string_contains_become_real_keys() {
        assert_eq!(keysym_for_char('\n'), 0xff0d);
        assert_eq!(keysym_for_char('\r'), 0xff0d);
        assert_eq!(keysym_for_char('\t'), 0xff09);
    }

    #[test]
    fn a_chord_is_modifiers_first_in_press_order() {
        assert_eq!(
            parse_chord("ctrl+shift+t").unwrap(),
            vec![0xffe3, 0xffe1, 0x74]
        );
        assert_eq!(parse_chord("ctrl+c").unwrap(), vec![0xffe3, 0x63]);
        assert_eq!(parse_chord("a").unwrap(), vec![0x61]);
    }

    #[test]
    fn a_literal_plus_survives_being_the_separator() {
        assert_eq!(parse_chord("ctrl++").unwrap(), vec![0xffe3, 0x2b]);
        assert_eq!(parse_chord("plus").unwrap(), vec![0x2b]);
        assert_eq!(parse_chord("+").unwrap(), vec![0x2b]);
        assert_eq!(parse_chord("ctrl+plus").unwrap(), vec![0xffe3, 0x2b]);
    }

    #[test]
    fn an_unknown_name_is_an_error_and_says_which() {
        assert_eq!(keysym_for("Wingding"), None);
        assert_eq!(keysym_for(""), None);
        assert_eq!(
            parse_chord("ctrl+wingding"),
            Err(UnknownKey("wingding".to_string()))
        );
        assert!(parse_chord("ctrl+").is_err());
        assert!(parse_chord("").is_err());
    }

    /// The inverse direction is *derived*, so this asserts the derivation rather than
    /// a second table.
    #[test]
    fn name_for_keysym_inverts_the_canonical_table() {
        for (name, keysym) in CANONICAL {
            assert_eq!(name_for_keysym(*keysym), Some(*name), "{name}");
            assert_eq!(keysym_for(name), Some(*keysym), "{name}");
        }
        assert_eq!(name_for_keysym(0xff0d), Some("Return"));
        assert_eq!(name_for_keysym(0x61), None);
    }

    /// The copy-paste failure: two rows of the table sharing a number, so one of them
    /// is silently unreachable through `name_for_keysym`.
    #[test]
    fn no_two_canonical_names_share_a_keysym() {
        let mut seen: Vec<u32> = CANONICAL.iter().map(|(_, k)| *k).collect();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(before, seen.len(), "a keysym appears twice in CANONICAL");
        let mut names: Vec<&str> = CANONICAL.iter().map(|(n, _)| *n).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "a name appears twice in CANONICAL");
    }

    /// An alias may only point at a name that exists, or it resolves to nothing at
    /// run time and the failure is a key that quietly does not press.
    #[test]
    fn every_alias_points_at_a_canonical_name() {
        for (alias, canon) in ALIASES {
            assert!(
                canonical_keysym(canon).is_some(),
                "alias {alias} points at {canon}, which is not in CANONICAL"
            );
            assert_eq!(
                *alias,
                alias.to_ascii_lowercase(),
                "alias {alias} must be lowercase"
            );
        }
    }
}
