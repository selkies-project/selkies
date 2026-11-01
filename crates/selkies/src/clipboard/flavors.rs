/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The flavors of a copy: the envelope markup and its plain text travel in, and the rule
//! that decides whether a copy offering both markup and a picture is text or a picture.

use serde_json::{Map, Value};

/// A copy carrying more than one flavor (markup and the plain text a source wrote for it)
/// rides the binary clipboard verbs under this type, as a JSON object of mime to content. It
/// never reaches a real clipboard: the flavors inside it do.
pub const FLAVORS_MIME: &str = "application/x-selkies-clipboard-flavours";

/// One flavor of a copy: its mime and its bytes, as pixelflux hands and takes them.
pub type Entry = (String, Vec<u8>);

/// Elements whose content is never shown as text.
const UNSHOWN: [&str; 6] = ["head", "style", "script", "title", "noscript", "template"];

/// Pack `(mime, data)` entries into the multi-flavor payload. Every flavor it carries is
/// text, so JSON holds them as they are.
pub fn envelope(entries: &[Entry]) -> Vec<u8> {
    let mut object = Map::new();
    for (mime, data) in entries {
        object.insert(
            mime.clone(),
            Value::String(String::from_utf8_lossy(data).into_owned()),
        );
    }
    serde_json::to_vec(&Value::Object(object)).unwrap_or_default()
}

/// Unpack the multi-flavor payload into `(mime, data)` entries, richest first, so a caller
/// writes markup as the session's own flavor and offers the plain text beside it.
///
/// Returns an error when the payload is not the documented JSON object with a usable flavor.
pub fn flavors(payload: &[u8]) -> Result<Vec<Entry>, String> {
    let decoded: Value = serde_json::from_slice(payload).map_err(|e| e.to_string())?;
    let Value::Object(object) = decoded else {
        return Err("clipboard flavors must be a non-empty object".into());
    };
    if object.is_empty() {
        return Err("clipboard flavors must be a non-empty object".into());
    }
    let entries: Vec<Entry> = ["text/html", "text/plain"]
        .iter()
        .filter_map(|mime| match object.get(*mime) {
            Some(Value::String(text)) if !text.is_empty() => {
                Some((mime.to_string(), text.as_bytes().to_vec()))
            }
            _ => None,
        })
        .collect();
    if entries.is_empty() {
        let mut names: Vec<&String> = object.keys().collect();
        names.sort();
        return Err(format!("no usable clipboard flavor in {names:?}"));
    }
    Ok(entries)
}

/// Whether a copy's markup carries text, which makes the copy text.
///
/// An office application copying formatted text offers a picture of the selection beside its
/// markup (Word's bitmap, LibreOffice's PNG and BMP), so that a paint program can take it,
/// while a copied picture offers markup of its own that is only an `img` tag pointing back at
/// its page. So the markup decides which the copy is: one with text to show is text, and its
/// picture is left out; one with none is a picture.
///
/// Returns true when any of it outside the document head, styles, scripts, and comments is
/// text other than whitespace (character references decoded, so `&nbsp;` is whitespace).
pub fn markup_has_text(html: &[u8]) -> bool {
    let text = String::from_utf8_lossy(html);
    let s: &str = &text;
    let mut unshown = 0usize;
    let mut i = 0usize;
    while i < s.len() {
        let rest = &s[i..];
        if rest.starts_with("<!--") {
            i += rest.find("-->").map_or(rest.len(), |end| end + 3);
            continue;
        }
        if opens_tag(rest) {
            let end = tag_end(rest);
            let closing = rest.as_bytes()[1] == b'/';
            let self_closed = rest[..end].ends_with("/>");
            let name = rest[1..]
                .trim_start_matches('/')
                .chars()
                .take_while(char::is_ascii_alphanumeric)
                .collect::<String>()
                .to_ascii_lowercase();
            i += end;
            if self_closed || !UNSHOWN.contains(&name.as_str()) {
                continue;
            }
            if closing {
                unshown = unshown.saturating_sub(1);
            } else {
                unshown += 1;
                if name == "script" || name == "style" {
                    i += raw_text_end(&s[i..], &name);
                }
            }
            continue;
        }
        let first = rest.chars().next().map_or(1, char::len_utf8);
        let run_end = rest[first..].find('<').map_or(rest.len(), |e| e + first);
        if unshown == 0 && has_visible(&rest[..run_end]) {
            return true;
        }
        i += run_end;
    }
    false
}

/// Whether `rest` starts a tag rather than a bare less-than, which is text.
fn opens_tag(rest: &str) -> bool {
    let bytes = rest.as_bytes();
    bytes.first() == Some(&b'<')
        && bytes
            .get(1)
            .is_some_and(|b| b.is_ascii_alphabetic() || matches!(*b, b'/' | b'!' | b'?'))
}

/// The byte just past the `>` ending the tag `rest` starts, or the end of `rest` when it never
/// closes. A `>` inside a quoted attribute value does not end it; a quote opens a value only
/// after its `=`, so an apostrophe in an unquoted value does not swallow the document.
fn tag_end(rest: &str) -> usize {
    let mut quote = None;
    let mut after_equals = false;
    for (i, b) in rest.bytes().enumerate() {
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None if b == b'>' => return i + 1,
            None if after_equals && matches!(b, b'"' | b'\'') => quote = Some(b),
            None => {}
        }
        if !b.is_ascii_whitespace() {
            after_equals = b == b'=';
        }
    }
    rest.len()
}

/// Where a `script` or `style` element's raw content ends: at its closing tag, whatever its
/// case, or at the end. Scans no further than it skips, so markup made of nothing but such
/// elements stays linear.
fn raw_text_end(rest: &str, name: &str) -> usize {
    let mut from = 0;
    while let Some(found) = rest[from..].find("</") {
        let at = from + found;
        let start = at + 2;
        let closes = rest
            .as_bytes()
            .get(start..start + name.len())
            .is_some_and(|tag| tag.eq_ignore_ascii_case(name.as_bytes()));
        if closes {
            return at;
        }
        from = start;
    }
    rest.len()
}

/// Whether a text run shows anything once its character references are decoded.
fn has_visible(run: &str) -> bool {
    let mut chars = run.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '&' {
            let tail = &run[i + 1..];
            if let Some(end) = tail.find(';').filter(|e| *e <= 32) {
                let name = &tail[..end];
                for _ in 0..end + 1 {
                    chars.next();
                }
                let decoded = decode_reference(name);
                match decoded {
                    Some(ch) if ch.is_whitespace() => continue,
                    _ => return true,
                }
            }
            return true;
        }
        if !c.is_whitespace() {
            return true;
        }
    }
    false
}

/// The character a reference names, where it is numeric or a named space; None for a named
/// reference that stands for visible text.
fn decode_reference(name: &str) -> Option<char> {
    if let Some(number) = name.strip_prefix('#') {
        let code = match number.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => number.parse::<u32>().ok()?,
        };
        return char::from_u32(code);
    }
    match name {
        "nbsp" | "NonBreakingSpace" => Some('\u{a0}'),
        "ensp" => Some('\u{2002}'),
        "emsp" => Some('\u{2003}'),
        "thinsp" | "ThinSpace" => Some('\u{2009}'),
        "Tab" => Some('\t'),
        "NewLine" => Some('\n'),
        _ => None,
    }
}
