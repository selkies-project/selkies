/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The nginx fancyindex fragments, written from the file index Selkies serves.
//!
//! Selkies serves the directory listing itself, but a deployment that fronts it with nginx
//! (the LinuxServer.io images do) renders the same listing through fancyindex, which takes a
//! header and a footer fragment. Those are the served page's two halves verbatim
//! (`FILE_INDEX_HEADER` and `FILE_INDEX_FOOTER` in `crates/selkies/src/files/index_html.rs`),
//! so they are generated rather than kept by hand, where a copy would drift to a palette the
//! dashboards stopped using.

use std::fs;

use crate::repo;

const SERVER: &str = "crates/selkies/src/files/index_html.rs";
const FRAGMENTS: [(&str, &str); 2] = [
    ("header.html", "FILE_INDEX_HEADER"),
    ("footer.html", "FILE_INDEX_FOOTER"),
];

/// Rewrite the fragments, or under `check` report the stale ones; `Ok(false)` when one is.
pub fn run(check: bool) -> Result<bool, String> {
    let source = fs::read_to_string(repo().join(SERVER)).map_err(|e| format!("{SERVER}: {e}"))?;
    let mut stale = Vec::new();
    for (file, constant) in FRAGMENTS {
        let body = raw_constant(&source, constant)
            .ok_or_else(|| format!("{SERVER} does not define {constant}"))?;
        let name = format!("web/selkies-web-core/nginx/{file}");
        let path = repo().join(&name);
        if fs::read_to_string(&path).is_ok_and(|current| current == body) {
            continue;
        }
        if check {
            stale.push(name);
        } else {
            fs::write(&path, body).map_err(|e| format!("{name}: {e}"))?;
            println!("wrote {name}");
        }
    }
    if !stale.is_empty() {
        eprintln!("stale, rerun cargo xtask file-index: {}", stale.join(", "));
        return Ok(false);
    }
    if check {
        println!("the nginx fancyindex fragments are up to date");
    }
    Ok(true)
}

/// The body of the raw string `pub const <name>: &str = r#"..."#;` in `source`, whatever
/// number of hashes it is quoted with.
fn raw_constant<'a>(source: &'a str, name: &str) -> Option<&'a str> {
    let opening = format!("pub const {name}: &str = r");
    let rest = &source[source.find(&opening)? + opening.len()..];
    let hashes = rest.len() - rest.trim_start_matches('#').len();
    let body = rest[hashes..].strip_prefix('"')?;
    let closing = format!("\"{};", "#".repeat(hashes));
    Some(&body[..body.find(&closing)?])
}

#[cfg(test)]
mod tests {
    use super::raw_constant;

    #[test]
    fn a_raw_string_ends_at_its_own_number_of_hashes() {
        let source = "pub const A: &str = r##\"one \"# two\"##;\npub const B: &str = r#\"b\"#;\n";
        assert_eq!(raw_constant(source, "A"), Some("one \"# two"));
        assert_eq!(raw_constant(source, "B"), Some("b"));
        assert_eq!(raw_constant(source, "C"), None);
    }
}
