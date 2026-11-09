/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A decoder for the XPM icons some legacy applications still ship, since no
//! image library in the dependency set reads them.

use std::collections::HashMap;

use tiny_skia::Pixmap;

/// Largest side accepted for an XPM image.
const MAX_SIDE: usize = 2048;

/// Decodes an XPM file into a premultiplied pixmap at its native size.
///
/// The file is walked as a sequence of C string literals: the header, the
/// color table, then one string per pixel row.  Pixels whose characters are
/// missing from the table are left transparent.
///
/// # Arguments
/// * `text` - The file contents.
///
/// # Returns
/// The decoded image, or `None` when the text is not a well formed XPM.
pub fn decode(text: &str) -> Option<Pixmap> {
    let mut strings = Strings { rest: text };
    let header = strings.next()?;
    let mut fields = header.split_whitespace().map(|f| f.parse::<usize>().ok());
    let w = fields.next()??;
    let h = fields.next()??;
    let ncolors = fields.next()??;
    let cpp = fields.next()??;
    if !(1..=MAX_SIDE).contains(&w) || !(1..=MAX_SIDE).contains(&h) || !(1..=65536).contains(&ncolors) {
        return None;
    }
    if !(1..=4).contains(&cpp) {
        return None;
    }

    let mut colors: HashMap<&str, [u8; 4]> = HashMap::with_capacity(ncolors);
    for _ in 0..ncolors {
        let line = strings.next()?;
        if line.len() < cpp || !line.is_char_boundary(cpp) {
            return None;
        }
        let (key, spec) = line.split_at(cpp);
        colors.insert(key, parse_color_spec(spec));
    }

    let mut pix = Pixmap::new(w as u32, h as u32)?;
    let data = pix.data_mut();
    for y in 0..h {
        let row = strings.next()?;
        if row.len() < w * cpp {
            return None;
        }
        for x in 0..w {
            let start = x * cpp;
            let key = row.get(start..start + cpp)?;
            if let Some(color) = colors.get(key) {
                let d = (y * w + x) * 4;
                data[d..d + 4].copy_from_slice(color);
            }
        }
    }
    Some(pix)
}

/// Iterates over the C string literals in an XPM file, skipping block comments.
struct Strings<'a> {
    rest: &'a str,
}

impl<'a> Iterator for Strings<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        loop {
            let bytes = self.rest.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    let end = self.rest[i + 2..].find("*/")?;
                    i += 2 + end + 2;
                } else if bytes[i] == b'"' {
                    let start = i + 1;
                    let end = self.rest[start..].find('"')?;
                    let literal = &self.rest[start..start + end];
                    self.rest = &self.rest[start + end + 1..];
                    return Some(literal);
                } else {
                    i += 1;
                }
            }
            if i >= bytes.len() {
                return None;
            }
        }
    }
}

/// Ranks a color key so the color visual wins over the grayscale and
/// monochrome ones when a line carries several.
fn key_rank(token: &str) -> Option<u8> {
    match token {
        "c" => Some(4),
        "g" => Some(3),
        "g4" => Some(2),
        "m" => Some(1),
        "s" => Some(0),
        _ => None,
    }
}

/// Parses the part of a color line that follows the pixel characters: a run
/// of key and value pairs where a value may span several words.
fn parse_color_spec(spec: &str) -> [u8; 4] {
    let mut best: Option<(u8, String)> = None;
    let mut current: Option<(u8, String)> = None;
    for token in spec.split_whitespace() {
        if let Some(rank) = key_rank(token) {
            if let Some((r, value)) = current.take()
                && !value.is_empty()
                && best.as_ref().is_none_or(|(br, _)| r > *br)
            {
                best = Some((r, value));
            }
            current = Some((rank, String::new()));
        } else if let Some((_, value)) = current.as_mut() {
            if !value.is_empty() {
                value.push(' ');
            }
            value.push_str(token);
        }
    }
    if let Some((r, value)) = current
        && !value.is_empty()
        && best.as_ref().is_none_or(|(br, _)| r > *br)
    {
        best = Some((r, value));
    }
    match best {
        Some((rank, value)) if rank > 0 => resolve_color(&value),
        _ => [0, 0, 0, 255],
    }
}

/// Resolves a color value to premultiplied RGBA: `None`, hex values of one to
/// four digits per channel, a few common names, then the X11 `rgb.txt`
/// database when it is installed.  Unknown names become opaque black.
fn resolve_color(value: &str) -> [u8; 4] {
    if value.eq_ignore_ascii_case("none") {
        return [0, 0, 0, 0];
    }
    if let Some(hex) = value.strip_prefix('#') {
        let digits = hex.len() / 3;
        if (1..=4).contains(&digits) && hex.len() == digits * 3 && hex.is_ascii() {
            let mut out = [0u8, 0, 0, 255];
            for (i, slot) in out.iter_mut().take(3).enumerate() {
                let part = &hex[i * digits..(i + 1) * digits];
                let v = u32::from_str_radix(part, 16).unwrap_or(0);
                *slot = if digits == 1 {
                    (v * 17) as u8
                } else {
                    (v >> (4 * (digits - 2))) as u8
                };
            }
            return out;
        }
        return [0, 0, 0, 255];
    }
    let named: [(&str, [u8; 3]); 10] = [
        ("black", [0, 0, 0]),
        ("white", [255, 255, 255]),
        ("red", [255, 0, 0]),
        ("green", [0, 255, 0]),
        ("blue", [0, 0, 255]),
        ("yellow", [255, 255, 0]),
        ("cyan", [0, 255, 255]),
        ("magenta", [255, 0, 255]),
        ("gray", [190, 190, 190]),
        ("grey", [190, 190, 190]),
    ];
    for (name, rgb) in named {
        if value.eq_ignore_ascii_case(name) {
            return [rgb[0], rgb[1], rgb[2], 255];
        }
    }
    if let Ok(db) = std::fs::read_to_string("/usr/share/X11/rgb.txt") {
        for line in db.lines() {
            let mut parts = line.split_whitespace();
            let (Some(r), Some(g), Some(b)) = (parts.next(), parts.next(), parts.next()) else {
                continue;
            };
            let name: Vec<&str> = parts.collect();
            if name.join(" ").eq_ignore_ascii_case(value)
                && let (Ok(r), Ok(g), Ok(b)) = (r.parse::<u8>(), g.parse::<u8>(), b.parse::<u8>())
            {
                return [r, g, b, 255];
            }
        }
    }
    [0, 0, 0, 255]
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"/* XPM */
static char * sample_xpm[] = {
"3 2 3 1",
" 	c None",
".	c #FF0000",
"+	c #00f s background",
" .+",
"+. "};
"#;

    #[test]
    fn decodes_pixels_and_transparency() {
        let pix = decode(SAMPLE).unwrap();
        assert_eq!((pix.width(), pix.height()), (3, 2));
        let px = |x: usize, y: usize| &pix.data()[(y * 3 + x) * 4..(y * 3 + x) * 4 + 4];
        assert_eq!(px(0, 0), &[0, 0, 0, 0]);
        assert_eq!(px(1, 0), &[255, 0, 0, 255]);
        assert_eq!(px(2, 0), &[0, 0, 255, 255]);
        assert_eq!(px(0, 1), &[0, 0, 255, 255]);
    }

    #[test]
    fn rejects_short_rows() {
        assert!(decode("\"2 1 1 1\",\n\"a c #000\",\n\"a\"").is_none());
    }

    #[test]
    fn prefers_color_over_mono() {
        assert_eq!(parse_color_spec(" m black c #0f0"), [0, 255, 0, 255]);
        assert_eq!(parse_color_spec(" s mask c None"), [0, 0, 0, 0]);
    }
}
