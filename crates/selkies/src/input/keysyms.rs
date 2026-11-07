/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Which keysym spells which character, and which keysyms are keys rather than glyphs:
//! the policy half of every injector, decided here and never in a transport.
//!
//! Latin-1 keeps its own keysym and only codepoints above it ride the Unicode plane
//! (`0x0100_0000 | codepoint`). Encoding Latin-1 in that plane as well is the more uniform
//! rule and xkbcommon reads it back, but X clients that look keys up through an input
//! context get the bare Latin-1 byte out of `XmbLookupString`, which a UTF-8 client drops, so
//! XWayland apps would lose exactly the accented characters. The legacy planes (Cyrillic,
//! Greek, Arabic, ...) decode through libxkbcommon. An overlay bind is a key the server
//! invents, so any keysym that spells one character is bound in its universal (Latin-1 or
//! Unicode-plane) form, since the toolkits' tables for legacy national keysyms disagree across
//! versions while those two forms translate algorithmically everywhere.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use xkbcommon::xkb;

use super::keysym_names::KEYSYM_NAMES;

/// `Shift_L`.
pub const KEYSYM_SHIFT_L: u32 = 0xFFE1;
/// `Shift_R`.
pub const KEYSYM_SHIFT_R: u32 = 0xFFE2;
/// `Control_L`.
pub const KEYSYM_CONTROL_L: u32 = 0xFFE3;
/// `Control_R`.
pub const KEYSYM_CONTROL_R: u32 = 0xFFE4;
/// `Caps_Lock`.
pub const KEYSYM_CAPS_LOCK: u32 = 0xFFE5;
/// `Alt_L`; mouse back and forward buttons are injected as Alt+Left and Alt+Right.
pub const KEYSYM_ALT_L: u32 = 0xFFE9;
/// `Left`.
pub const KEYSYM_LEFT_ARROW: u32 = 0xFF51;
/// `Right`.
pub const KEYSYM_RIGHT_ARROW: u32 = 0xFF53;
/// `Insert`, the key of the Shift+Insert paste chord.
pub const KEYSYM_INSERT: u32 = 0xFF63;
/// `BackSpace`.
pub const KEYSYM_BACKSPACE: u32 = 0xFF08;
/// `Return`.
pub const KEYSYM_RETURN: u32 = 0xFF0D;
/// `Tab`.
pub const KEYSYM_TAB: u32 = 0xFF09;
/// `ISO_Level3_Shift`, the AltGr of a PC layout.
pub const KEYSYM_ISO_LEVEL3_SHIFT: u32 = 0xFE03;
/// `Mode_switch`, the AltGr of an older layout.
pub const KEYSYM_MODE_SWITCH: u32 = 0xFF7E;

/// Keysyms never armed for auto-repeat or routed as ordinary keys: Shift, Control, Alt,
/// ISO_Level3_Shift, Meta, Super, and Hyper. Super and Hyper are included because the client
/// maps the Meta/Windows key to Super, not Meta.
pub const MODIFIER_KEYSYMS: &[u32] = &[
    0xFFE1, 0xFFE2, 0xFFE3, 0xFFE4, 0xFFE9, 0xFFEA, 0xFE03, 0xFFE7, 0xFFE8, 0xFFEB, 0xFFEC, 0xFFED,
    0xFFEE,
];

/// The chord modifiers: Control, Alt, Meta, Super, and Hyper. While one is held every other held
/// modifier is part of the chord, and a Cyrillic keysym is translated to its QWERTY key.
pub const ACTION_MODIFIER_KEYSYMS: &[u32] = &[
    0xFFE3, 0xFFE4, 0xFFE9, 0xFFEA, 0xFFE7, 0xFFE8, 0xFFEB, 0xFFEC, 0xFFED, 0xFFEE,
];

/// Shift_L/R, ISO_Level3_Shift, and Mode_switch: the level-selecting modifiers whose
/// client-held state the injectors consult in place of a per-press server query.
pub const LEVEL_MODIFIER_KEYSYMS: &[u32] = &[0xFFE1, 0xFFE2, 0xFE03, 0xFF7E];

/// The X key names of the modifiers that make a printable key a shortcut for the xdotool path.
pub const SHORTCUT_MODIFIER_XKEY_NAMES: &[&str] = &[
    "Control_L",
    "Control_R",
    "Alt_L",
    "Alt_R",
    "Super_L",
    "Super_R",
    "Meta_L",
    "Meta_R",
];

/// Whether `keysym` is one of `MODIFIER_KEYSYMS`.
pub fn is_modifier(keysym: u32) -> bool {
    MODIFIER_KEYSYMS.contains(&keysym)
}

/// Whether `keysym` is one of `ACTION_MODIFIER_KEYSYMS`.
pub fn is_action_modifier(keysym: u32) -> bool {
    ACTION_MODIFIER_KEYSYMS.contains(&keysym)
}

/// Whether `keysym` is one of `LEVEL_MODIFIER_KEYSYMS`.
pub fn is_level_modifier(keysym: u32) -> bool {
    LEVEL_MODIFIER_KEYSYMS.contains(&keysym)
}

/// Cyrillic (ЙЦУКЕН) keysyms to the QWERTY keysym on the same physical key, row by row, so a
/// shortcut chorded on a Russian layout (Ctrl+C on ЙЦУКЕН) reaches the application as it
/// expects.
pub const CYRILLIC_TO_QWERTY_KEYSYM: &[(u32, u32)] = &[
    (0x06CA, 0x0071),
    (0x06C3, 0x0077),
    (0x06D5, 0x0065),
    (0x06CB, 0x0072),
    (0x06C5, 0x0074),
    (0x06CE, 0x0079),
    (0x06C7, 0x0075),
    (0x06DB, 0x0069),
    (0x06DD, 0x006F),
    (0x06DA, 0x0070),
    (0x06C6, 0x0061),
    (0x06D9, 0x0073),
    (0x06D7, 0x0064),
    (0x06C1, 0x0066),
    (0x06D0, 0x0067),
    (0x06D2, 0x0068),
    (0x06CF, 0x006A),
    (0x06CC, 0x006B),
    (0x06C4, 0x006C),
    (0x06D1, 0x007A),
    (0x06DE, 0x0078),
    (0x06D3, 0x0063),
    (0x06CD, 0x0076),
    (0x06C9, 0x0062),
    (0x06D4, 0x006E),
    (0x06D8, 0x006D),
];

/// The QWERTY keysym on the physical key a Cyrillic keysym names, if it is one of the table's.
pub fn cyrillic_to_qwerty(keysym: u32) -> Option<u32> {
    CYRILLIC_TO_QWERTY_KEYSYM
        .iter()
        .find(|(cyr, _)| *cyr == keysym)
        .map(|(_, qwerty)| *qwerty)
}

/// Whether the keysym is in the Unicode plane (`0x0100_0000 | codepoint`).
pub fn is_unicode_plane(keysym: u32) -> bool {
    (keysym & 0xFF00_0000) == 0x0100_0000
}

/// Whether the keysym spells a printable Latin-1 or Unicode-plane character by its value alone.
pub fn is_printable(keysym: u32) -> bool {
    (0x20..=0xFF).contains(&keysym) || is_unicode_plane(keysym)
}

/// The codepoint a printable keysym encodes (`is_printable`).
pub fn printable_codepoint(keysym: u32) -> u32 {
    if is_unicode_plane(keysym) {
        keysym & 0x00FF_FFFF
    } else {
        keysym
    }
}

/// The printable character a keysym types, or None for keysyms that are not plain text
/// (modifiers, navigation, anything decoding to a control code). Latin-1 and Unicode-plane
/// keysyms decode directly; the legacy planes resolve through libxkbcommon. Cached: a session
/// types the same keysyms over and over, and each miss costs a library call.
pub fn keysym_to_character(keysym: u32) -> Option<char> {
    static CACHE: OnceLock<Mutex<HashMap<u32, Option<char>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache.lock().ok().and_then(|c| c.get(&keysym).copied()) {
        return hit;
    }
    let decoded = decode_character(keysym);
    if let Ok(mut c) = cache.lock() {
        if c.len() >= 4096 {
            c.clear();
        }
        c.insert(keysym, decoded);
    }
    decoded
}

/// `keysym_to_character` without the cache.
fn decode_character(keysym: u32) -> Option<char> {
    let ch = if is_unicode_plane(keysym) {
        char::from_u32(keysym & 0x00FF_FFFF)?
    } else if (0x20..=0xFF).contains(&keysym) {
        char::from_u32(keysym)?
    } else if keysym == 0x20AC {
        '€'
    } else {
        let text = xkb::keysym_to_utf8(xkb::Keysym::new(keysym));
        let mut chars = text.chars();
        let first = chars.next()?;
        if chars.next().is_some() {
            return None;
        }
        first
    };
    let code = ch as u32;
    if code < 0x20 || (0x7F..=0x9F).contains(&code) {
        return None;
    }
    Some(ch)
}

/// The canonical keysym for a character (the one a physical keyboard layout binds:
/// `Cyrillic_ef` for ф, not its Unicode-plane alias), so text typed onto a seat whose layout
/// carries the script lands on real layout keys instead of overlay binds. Falls back to the
/// Latin-1/Unicode-plane encoding.
pub fn character_to_layout_keysym(ch: char) -> u32 {
    let codepoint = ch as u32;
    let keysym = xkb::utf32_to_keysym(codepoint).raw();
    if keysym != 0 {
        return keysym;
    }
    if (0x20..=0xFF).contains(&codepoint) {
        codepoint
    } else {
        0x0100_0000 | codepoint
    }
}

/// Whether a keysym names a key rather than a glyph.
///
/// The X function block (0xFF00 to 0xFFFF: navigation, editing, F-keys, and the keypad) and
/// the XF86 vendor block carry no shifted glyph for a held Shift or AltGr to move onto, so a
/// level modifier the client holds with one of them is the chord the user meant (Shift+Home
/// selects to the line start) and is never lifted around the press.
pub fn is_function_keysym(keysym: u32) -> bool {
    (0xFF00..=0xFFFF).contains(&keysym) || (0x1008_0000..=0x1008_FFFF).contains(&keysym)
}

/// The keysym value an overlay slot carries for `keysym`: Latin-1 and Unicode-plane keysyms
/// as they are, any other keysym spelling one character in the Unicode plane, and charless
/// keysyms (XF86, F-keys) at their semantic value (module docstring).
pub fn overlay_bind_keysym(keysym: u32) -> u32 {
    if is_printable(keysym) {
        return keysym;
    }
    match keysym_to_character(keysym) {
        Some(ch) => 0x0100_0000 | ch as u32,
        None => keysym,
    }
}

/// The keysym that types `ch` on a key no layout carries, or None when the character is not
/// typeable at all: Return and Tab are the only control bindings, and C1 types nothing.
pub fn universal_text_keysym(ch: char) -> Option<u32> {
    match ch {
        '\n' | '\r' => return Some(KEYSYM_RETURN),
        '\t' => return Some(KEYSYM_TAB),
        _ => {}
    }
    let codepoint = ch as u32;
    if codepoint < 0x20 || (0x7F..=0x9F).contains(&codepoint) {
        return None;
    }
    Some(if codepoint <= 0xFF {
        codepoint
    } else {
        0x0100_0000 | codepoint
    })
}

/// One keysym per typeable character for the virtual-keyboard typer, in the universal forms
/// overlay binds carry; untypeable characters are dropped. pixelflux taps whatever it is given,
/// so which keysym spells a character is decided here, never in the transport.
pub fn text_to_wayland_keysyms(text: &str) -> Vec<u32> {
    text.chars().filter_map(universal_text_keysym).collect()
}

/// The X key name of a keysym the server-side table names (`KEYSYM_NAMES`): the spelling
/// `xdotool key` accepts, and the set of keysyms the X11 injector treats as named keys.
pub fn xkey_name(keysym: u32) -> Option<&'static str> {
    KEYSYM_NAMES
        .binary_search_by_key(&keysym, |(k, _)| *k)
        .ok()
        .map(|i| KEYSYM_NAMES[i].1)
}

/// The xkbcommon name of any keysym, for the xdotool fallback of a key the table does not name.
pub fn keysym_name(keysym: u32) -> Option<String> {
    let name = xkb::keysym_get_name(xkb::Keysym::new(keysym));
    if name.is_empty() || name == "NoSymbol" {
        None
    } else {
        Some(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Which keysym spells a typed character.
    #[test]
    fn latin1_types_as_its_own_keysym() {
        let latin1 = (0x20u32..0x7F)
            .chain(0xA0..0x100)
            .filter_map(char::from_u32);
        let wrong: Vec<char> = latin1
            .filter(|c| universal_text_keysym(*c) != Some(*c as u32))
            .collect();
        assert!(wrong.is_empty(), "{wrong:?}");
    }

    #[test]
    fn codepoints_above_latin1_ride_the_unicode_plane() {
        let above = [
            (0x100u32, 0x0100_0100u32),
            (0x3A9, 0x0100_03A9),
            (0x4E2D, 0x0100_4E2D),
            (0x1F600, 0x0101_F600),
        ];
        for (cp, ks) in above {
            assert_eq!(
                universal_text_keysym(char::from_u32(cp).unwrap()),
                Some(ks),
                "{cp:#x}"
            );
        }
    }

    #[test]
    fn return_and_tab_are_the_only_control_bindings() {
        assert_eq!(universal_text_keysym('\n'), Some(0xFF0D));
        assert_eq!(universal_text_keysym('\r'), Some(0xFF0D));
        assert_eq!(universal_text_keysym('\t'), Some(0xFF09));
        let untypeable = (0x00u32..0x09)
            .chain(0x0B..0x0D)
            .chain(0x0E..0x20)
            .chain(0x7F..0xA0);
        let typed: Vec<u32> = untypeable
            .filter(|c| universal_text_keysym(char::from_u32(*c).unwrap()).is_some())
            .collect();
        assert!(
            typed.is_empty(),
            "controls and C1 have no keysym: {typed:x?}"
        );
    }

    #[test]
    fn the_typer_is_handed_one_keysym_per_typeable_character() {
        let sample = "aé ü ñ £ ¿ ÿ Ω ф 中 \u{1F600} Z9\t\n";
        let keysyms = text_to_wayland_keysyms(sample);
        let typeable = sample
            .chars()
            .filter(|c| universal_text_keysym(*c).is_some())
            .count();
        assert_eq!(keysyms.len(), typeable);
        let expected: Vec<u32> = sample.chars().filter_map(universal_text_keysym).collect();
        assert_eq!(
            keysyms, expected,
            "nothing typeable is dropped from a mixed string"
        );
        assert!(text_to_wayland_keysyms("\x00\x7f\u{80}\u{9f}\x1b").is_empty());
        let printable: Vec<char> = sample.chars().filter(|c| !"\t\n".contains(*c)).collect();
        for c in &printable {
            let ks = universal_text_keysym(*c).unwrap();
            assert_eq!(
                keysym_to_character(ks),
                Some(*c),
                "every keysym decodes back to its character"
            );
            assert_eq!(
                xkb::keysym_to_utf8(xkb::Keysym::new(ks)),
                c.to_string(),
                "xkbcommon decodes every keysym the same way"
            );
        }
    }

    #[test]
    fn layout_keysym_prefers_the_legacy_plane_and_overlay_binds_universalize_it() {
        let ef = character_to_layout_keysym('ф');
        assert_eq!(ef, 0x06C6, "Cyrillic_ef is the layout keysym for ф");
        assert_eq!(overlay_bind_keysym(ef), 0x0100_0444);
        assert_eq!(overlay_bind_keysym(0x61), 0x61);
        assert_eq!(
            overlay_bind_keysym(0xFFC0),
            0xFFC0,
            "a charless keysym keeps its value"
        );
        assert_eq!(keysym_to_character(0x06C6), Some('ф'));
        assert_eq!(keysym_to_character(0xFFE1), None);
        assert_eq!(
            keysym_to_character(0xFF0D),
            None,
            "Return decodes to a control code"
        );
    }

    #[test]
    fn function_keysyms_and_names() {
        assert!(is_function_keysym(0xFF50));
        assert!(is_function_keysym(0x1008_FF11));
        assert!(!is_function_keysym(0x61));
        assert_eq!(xkey_name(0xFFE1), Some("Shift_L"));
        assert_eq!(xkey_name(0x20), Some("space"));
        assert_eq!(xkey_name(0x0100_0000 | 0x1F600), None);
        assert_eq!(cyrillic_to_qwerty(0x06D3), Some(0x63));
        assert_eq!(keysym_name(0xFF08).as_deref(), Some("BackSpace"));
    }
}
