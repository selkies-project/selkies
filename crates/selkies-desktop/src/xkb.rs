//! Keyboard mapping for the menu's search box.
//!
//! A Wayland compositor hands clients its keymap as xkbcommon's text format.
//! Only the two pieces a text field needs are read: which keycode each key
//! name has, and the first group's symbols of each key, level one and the
//! shifted level two.  Keysym names are resolved through xkeysym's table and
//! the `Uxxxx` form.  The X11 backend fills the same structure from the core
//! protocol's keyboard mapping.

use std::collections::HashMap;
use std::sync::OnceLock;

use xkeysym::Keysym;

/// A key the UI reacts to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// A character to insert.
    Char(char),
    /// Backspace.
    Backspace,
    /// Delete.
    Delete,
    /// Enter or keypad enter.
    Enter,
    /// Escape.
    Escape,
    /// Tab.
    Tab,
    /// Cursor up.
    Up,
    /// Cursor down.
    Down,
    /// Cursor left.
    Left,
    /// Cursor right.
    Right,
    /// Page up.
    PageUp,
    /// Page down.
    PageDown,
    /// Home.
    Home,
    /// End.
    End,
    /// The Super key, which toggles the menu.
    Super,
    /// A function key, by number.
    Function(u8),
}

/// Keycode to keysym mapping, with the shifted level.
#[derive(Clone, Debug, Default)]
pub struct Keymap {
    /// Keycodes as the display server numbers them, each with its level one
    /// and level two keysyms.
    syms: HashMap<u32, (u32, u32)>,
}

impl Keymap {
    /// Parses an xkbcommon keymap in the text format.
    ///
    /// # Arguments
    /// * `text` - The keymap text, as received from the compositor.
    pub fn parse(text: &str) -> Keymap {
        let mut names: HashMap<String, u32> = HashMap::new();
        let mut aliases: Vec<(String, String)> = Vec::new();
        if let Some(section) = section(text, "xkb_keycodes") {
            for statement in section.split(';') {
                let statement = statement.trim();
                if let Some(rest) = statement.strip_prefix("alias") {
                    if let Some((alias, target)) = rest.split_once('=') {
                        aliases.push((alias.trim().to_string(), target.trim().to_string()));
                    }
                } else if let Some((name, value)) = statement.split_once('=') {
                    let name = name.trim();
                    if name.starts_with('<')
                        && let Ok(code) = value.trim().parse::<u32>()
                    {
                        names.insert(name.to_string(), code);
                    }
                }
            }
        }
        for (alias, target) in aliases {
            if let Some(code) = names.get(&target).copied() {
                names.entry(alias).or_insert(code);
            }
        }

        let mut syms = HashMap::new();
        if let Some(section) = section(text, "xkb_symbols") {
            let mut rest = section;
            while let Some(pos) = rest.find("key ") {
                rest = &rest[pos + 4..];
                let Some(end) = rest.find('}') else {
                    break;
                };
                let body = &rest[..end];
                let Some(name_end) = body.find('>') else {
                    continue;
                };
                let name = body[..=name_end].trim();
                let Some(code) = names.get(name).copied() else {
                    continue;
                };
                let Some(list) = first_symbol_list(body) else {
                    continue;
                };
                let mut entries = list.split(',').map(|s| keysym_from_name(s.trim()).unwrap_or(0));
                let level1 = entries.next().unwrap_or(0);
                let level2 = entries.next().unwrap_or(level1);
                syms.insert(code, (level1, if level2 == 0 { level1 } else { level2 }));
            }
        }
        Keymap { syms }
    }

    /// Builds a keymap from keysyms per keycode, as X11's keyboard mapping
    /// reports them.
    ///
    /// # Arguments
    /// * `first_keycode` - Keycode of the first group of keysyms.
    /// * `per_keycode` - Keysyms listed for each keycode.
    /// * `keysyms` - The keysyms, `per_keycode` of them for each keycode.
    pub fn from_x11(first_keycode: u32, per_keycode: usize, keysyms: &[u32]) -> Keymap {
        let mut syms = HashMap::new();
        if per_keycode == 0 {
            return Keymap { syms };
        }
        for (i, chunk) in keysyms.chunks(per_keycode).enumerate() {
            let level1 = chunk.first().copied().unwrap_or(0);
            let level2 = chunk.get(1).copied().filter(|s| *s != 0).unwrap_or(level1);
            if level1 != 0 {
                syms.insert(first_keycode + i as u32, (level1, level2));
            }
        }
        Keymap { syms }
    }

    /// Translates a key press into the key the UI handles.
    ///
    /// # Arguments
    /// * `keycode` - The keycode as the display server numbers it.
    /// * `shift` - Whether a shift modifier is held.
    /// * `caps_lock` - Whether caps lock is active.
    ///
    /// # Returns
    /// The key, or `None` when the keycode produces nothing of interest.
    pub fn translate(&self, keycode: u32, shift: bool, caps_lock: bool) -> Option<Key> {
        let (level1, level2) = self.syms.get(&keycode).copied()?;
        let sym = if shift { level2 } else { level1 };
        let keysym = Keysym::new(sym);
        if let Some(key) = special_key(keysym) {
            return Some(key);
        }
        let mut ch = keysym.key_char()?;
        if ch.is_control() {
            return None;
        }
        if caps_lock && ch.is_alphabetic() {
            ch = if shift {
                ch.to_lowercase().next().unwrap_or(ch)
            } else {
                ch.to_uppercase().next().unwrap_or(ch)
            };
        }
        Some(Key::Char(ch))
    }
}

/// The body of a top-level keymap section, between its braces.
fn section<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let start = text.find(name)?;
    let open = text[start..].find('{')? + start;
    let mut depth = 0usize;
    for (i, c) in text[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[open + 1..open + i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// The first bracketed keysym list in a key body, whether bare or assigned
/// through `symbols[Group1]=`.
fn first_symbol_list(body: &str) -> Option<&str> {
    let open = body.find('[')?;
    let candidate = &body[open..];
    let candidate = if let Some(rest) = candidate
        .strip_prefix("[Group1]")
        .or_else(|| candidate.strip_prefix("[group1]"))
    {
        let next = rest.find('[')?;
        &rest[next..]
    } else {
        candidate
    };
    let close = candidate.find(']')?;
    Some(&candidate[1..close])
}

/// Keysym names to values, built once from xkeysym's name table.
fn names() -> &'static HashMap<&'static str, u32> {
    static NAMES: OnceLock<HashMap<&'static str, u32>> = OnceLock::new();
    NAMES.get_or_init(|| {
        let mut map = HashMap::with_capacity(3000);
        let ranges = [0x20u32..0x10000, 0xFE00..0xFF00, 0x1008FF00..0x10090000];
        for range in ranges {
            for value in range {
                if let Some(name) = Keysym::new(value).name() {
                    map.entry(name.strip_prefix("XK_").unwrap_or(name))
                        .or_insert(value);
                }
            }
        }
        map
    })
}

/// Resolves a keysym name as it appears in a keymap.
///
/// # Arguments
/// * `name` - The name, `NoSymbol`, or the `Uxxxx` unicode form.
pub fn keysym_from_name(name: &str) -> Option<u32> {
    if name.is_empty() || name == "NoSymbol" || name == "VoidSymbol" {
        return None;
    }
    if let Some(hex) = name.strip_prefix('U')
        && hex.len() >= 4
        && let Ok(code) = u32::from_str_radix(hex, 16)
    {
        return Some(0x0100_0000 + code);
    }
    if let Some(hex) = name.strip_prefix("0x")
        && let Ok(code) = u32::from_str_radix(hex, 16)
    {
        return Some(code);
    }
    names().get(name).copied()
}

/// Keys that act rather than type.
fn special_key(sym: Keysym) -> Option<Key> {
    Some(match sym {
        Keysym::BackSpace => Key::Backspace,
        Keysym::Delete | Keysym::KP_Delete => Key::Delete,
        Keysym::Return | Keysym::KP_Enter | Keysym::ISO_Enter => Key::Enter,
        Keysym::Escape => Key::Escape,
        Keysym::Tab | Keysym::ISO_Left_Tab => Key::Tab,
        Keysym::Up | Keysym::KP_Up => Key::Up,
        Keysym::Down | Keysym::KP_Down => Key::Down,
        Keysym::Left | Keysym::KP_Left => Key::Left,
        Keysym::Right | Keysym::KP_Right => Key::Right,
        Keysym::Page_Up | Keysym::KP_Page_Up => Key::PageUp,
        Keysym::Page_Down | Keysym::KP_Page_Down => Key::PageDown,
        Keysym::Home | Keysym::KP_Home => Key::Home,
        Keysym::End | Keysym::KP_End => Key::End,
        Keysym::Super_L | Keysym::Super_R => Key::Super,
        Keysym::F1 => Key::Function(1),
        Keysym::F2 => Key::Function(2),
        Keysym::F3 => Key::Function(3),
        Keysym::F4 => Key::Function(4),
        Keysym::F5 => Key::Function(5),
        Keysym::F6 => Key::Function(6),
        Keysym::F7 => Key::Function(7),
        Keysym::F8 => Key::Function(8),
        Keysym::F9 => Key::Function(9),
        Keysym::F10 => Key::Function(10),
        Keysym::F11 => Key::Function(11),
        Keysym::F12 => Key::Function(12),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYMAP: &str = r#"xkb_keymap {
xkb_keycodes "evdev" {
	minimum = 8;
	maximum = 255;
	<ESC>  = 9;
	<AE01> = 10;
	<AD01> = 24;
	<SPCE> = 65;
	<RTRN> = 36;
	alias <AC00> = <AD01>;
};
xkb_types "complete" { virtual_modifiers NumLock; };
xkb_compatibility "complete" { interpret.useModMapMods= AnyLevel; };
xkb_symbols "pc+us" {
	name[Group1]="English (US)";
	key <ESC>  { [ Escape ] };
	key <AE01> { [ 1, exclam ] };
	key <AD01> {
		type= "ALPHABETIC",
		symbols[Group1]= [ q, Q ],
		symbols[Group2]= [ U00E9 ]
	};
	key <SPCE> { [ space ] };
	key <RTRN> { [ Return ] };
	modifier_map Shift { <LFSH> };
};
};
"#;

    #[test]
    fn parses_symbols_and_levels() {
        let map = Keymap::parse(KEYMAP);
        assert_eq!(map.translate(9, false, false), Some(Key::Escape));
        assert_eq!(map.translate(10, false, false), Some(Key::Char('1')));
        assert_eq!(map.translate(10, true, false), Some(Key::Char('!')));
        assert_eq!(map.translate(24, false, false), Some(Key::Char('q')));
        assert_eq!(map.translate(24, true, false), Some(Key::Char('Q')));
        assert_eq!(map.translate(24, false, true), Some(Key::Char('Q')));
        assert_eq!(map.translate(24, true, true), Some(Key::Char('q')));
        assert_eq!(map.translate(65, false, false), Some(Key::Char(' ')));
        assert_eq!(map.translate(36, false, false), Some(Key::Enter));
        assert_eq!(map.translate(200, false, false), None);
    }

    #[test]
    fn resolves_names() {
        assert_eq!(keysym_from_name("a"), Some(0x61));
        assert_eq!(keysym_from_name("BackSpace"), Some(0xff08));
        assert_eq!(keysym_from_name("U00E9"), Some(0x10000e9));
        assert_eq!(keysym_from_name("NoSymbol"), None);
        assert_eq!(keysym_from_name("Cyrillic_a"), Some(0x6c1));
    }

    #[test]
    fn x11_mapping() {
        let map = Keymap::from_x11(8, 2, &[0, 0, 0x61, 0x41, 0xff0d, 0]);
        assert_eq!(map.translate(9, false, false), Some(Key::Char('a')));
        assert_eq!(map.translate(9, true, false), Some(Key::Char('A')));
        assert_eq!(map.translate(10, true, false), Some(Key::Enter));
        assert_eq!(map.translate(8, false, false), None);
    }
}
