/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The XSETTINGS the X11 session is served: `~/.xsettingsd`, and the xsettingsd processes
//! serving this display, told to read it again.
//!
//! The file is shared by every setting Selkies hands over XSETTINGS (the density's `Xft/*`
//! keys, the GTK theme's `Net/ThemeName`), so a write replaces only the keys it names and keeps
//! every other line, comments included: one key's writer never drops another's. xsettingsd
//! reads the whole file again on SIGHUP, and the signal goes to every xsettingsd serving this
//! display (`pids_on_display`): the one serving it is not necessarily the oldest, and one
//! serving another display from the same home would read this file and serve that display
//! these values.

use std::path::{Path, PathBuf};

use log::{debug, warn};

use super::dpi::{atomic_write_text, home, pids_on_display};

/// Where the base image's xsettingsd reads its settings, in the home it runs with.
pub fn xsettingsd_path() -> PathBuf {
    home().join(".xsettingsd")
}

/// The setting a line of an xsettingsd file names, `None` for a blank line or a comment.
fn line_key(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    trimmed.split_whitespace().next()
}

/// `text` with each of `entries` (a setting and its value as xsettingsd writes it, a string
/// already quoted) set: a setting already in the text is replaced where it stands, a new one
/// appended, and every other line kept as it was.
pub fn with_entries(text: &str, entries: &[(&str, String)]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut placed = vec![false; entries.len()];
    for line in text.lines() {
        let key = line_key(line);
        match entries.iter().position(|(name, _)| Some(*name) == key) {
            Some(i) if placed[i] => {}
            Some(i) => {
                out.push(format!("{} {}", entries[i].0, entries[i].1));
                placed[i] = true;
            }
            None => out.push(line.to_string()),
        }
    }
    for (i, (name, value)) in entries.iter().enumerate() {
        if !placed[i] {
            out.push(format!("{name} {value}"));
        }
    }
    out.join("\n") + "\n"
}

/// The value of `name` in an xsettingsd file's text, a string setting unquoted.
pub fn entry(text: &str, name: &str) -> Option<String> {
    text.lines()
        .find(|line| line_key(line) == Some(name))
        .map(|line| {
            let value = line.trim_start()[name.len()..].trim();
            value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(value)
                .to_string()
        })
}

/// Set `entries` in the xsettingsd file at `path`, creating it where there is none. Blocking.
pub fn write_entries(path: &Path, entries: &[(&str, String)]) -> std::io::Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    atomic_write_text(path, &with_entries(&text, entries))
}

/// Tell every xsettingsd serving this display to read its file again; how many were told.
pub async fn reload() -> usize {
    let pids = tokio::task::spawn_blocking(|| pids_on_display("xsettingsd"))
        .await
        .unwrap_or_default();
    let mut signaled = Vec::new();
    for pid in &pids {
        match nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(*pid),
            nix::sys::signal::Signal::SIGHUP,
        ) {
            Ok(()) => signaled.push(pid.to_string()),
            Err(e) => {
                debug!(target: "display", "Failed to send SIGHUP to xsettingsd process {pid}: {e}")
            }
        }
    }
    if !signaled.is_empty() {
        debug!(target: "display", "Sent SIGHUP to xsettingsd to reload config ({}).", signaled.join(", "));
    } else if !pids.is_empty() {
        warn!(target: "display", "No xsettingsd process could be signaled to reload.");
    } else {
        debug!(target: "display", "No xsettingsd serves this display. Skipping reload.");
    }
    signaled.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_replaces_its_keys_and_keeps_every_other_line() {
        let text = "# mine\nXft/DPI 98304\nNet/ThemeName \"Adwaita\"\nGtk/FontName \"Sans 10\"\n";
        let out = with_entries(
            text,
            &[("Xft/DPI", "147456".into()), ("Xft/Antialias", "1".into())],
        );
        assert_eq!(
            out,
            "# mine\nXft/DPI 147456\nNet/ThemeName \"Adwaita\"\nGtk/FontName \"Sans 10\"\nXft/Antialias 1\n"
        );
        let out = with_entries(&out, &[("Net/ThemeName", "\"Adwaita-dark\"".into())]);
        assert!(
            out.contains("Net/ThemeName \"Adwaita-dark\"\n") && out.contains("Xft/DPI 147456\n"),
            "the theme's writer keeps the density: {out}"
        );
        assert_eq!(
            entry(&out, "Net/ThemeName").as_deref(),
            Some("Adwaita-dark")
        );
        assert_eq!(entry(&out, "Xft/DPI").as_deref(), Some("147456"));
        assert_eq!(entry(&out, "Net/IconThemeName"), None);
    }

    #[test]
    fn a_setting_named_twice_is_written_once() {
        let out = with_entries("Xft/DPI 1\nXft/DPI 2\n", &[("Xft/DPI", "3".into())]);
        assert_eq!(out, "Xft/DPI 3\n");
    }
}
