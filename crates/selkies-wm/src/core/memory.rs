/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Where an application's window last stood: its frame rectangle and whether it was maximized,
//! kept per app id across sessions so a program opens where the user left it rather than where
//! the placement policy finds room. The engine records a window when it is removed and reads the
//! record when it places a new window of the same app id that neither a rule nor the client
//! itself positions; a second window of an application already showing one at that spot is
//! placed by the policy instead, so two windows never open exactly over each other.
//!
//! The file is `$XDG_STATE_HOME/selkies-wm/windows` (`~/.local/state` by default), one line per
//! app id with tab-separated fields, rewritten as a whole on every change since it stays small:
//! an app id that stops being used keeps its line, and the oldest lines go once there are more
//! than a few hundred.

use std::path::PathBuf;

use pixelflux::wm::Rect;

/// Most app ids the file keeps.
const CAPACITY: usize = 400;

/// What was remembered of an application's last window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Remembered {
    /// The frame rectangle, the client grown by its decoration, in layout coordinates.
    pub frame: Rect,
    pub maximized: bool,
}

/// The remembered windows, by app id, backed by the state file.
pub struct WindowMemory {
    path: Option<PathBuf>,
    /// App ids in the order they were last written, oldest first, with their records.
    entries: Vec<(String, Remembered)>,
}

impl WindowMemory {
    /// The memory read from the state file; an unreadable file is an empty memory.
    pub fn load() -> WindowMemory {
        let path = selkies_draw::dirs::state_home().join("selkies-wm/windows");
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        WindowMemory {
            path: Some(path),
            entries: parse(&text),
        }
    }

    /// A memory that is never written, for tests.
    pub fn transient() -> WindowMemory {
        WindowMemory {
            path: None,
            entries: Vec::new(),
        }
    }

    /// The record of an app id, if any.
    pub fn get(&self, app_id: &str) -> Option<Remembered> {
        let key = key_of(app_id)?;
        self.entries.iter().find(|(k, _)| *k == key).map(|(_, r)| *r)
    }

    /// Record an application's window, writing the file when the record changed.
    pub fn remember(&mut self, app_id: &str, remembered: Remembered) {
        let Some(key) = key_of(app_id) else {
            return;
        };
        if self.get(app_id) == Some(remembered) {
            return;
        }
        self.entries.retain(|(k, _)| *k != key);
        self.entries.push((key, remembered));
        if self.entries.len() > CAPACITY {
            let excess = self.entries.len() - CAPACITY;
            self.entries.drain(..excess);
        }
        self.write();
    }

    fn write(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let text = format(&self.entries);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(error) = std::fs::write(path, text) {
            log::debug!(target: "wm", "window memory not written to {}: {error}", path.display());
        }
    }
}

/// The key an app id is remembered under: lowercased, or `None` for an empty one.
fn key_of(app_id: &str) -> Option<String> {
    let key = app_id.trim().to_ascii_lowercase();
    (!key.is_empty()).then_some(key)
}

fn parse(text: &str) -> Vec<(String, Remembered)> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            if fields.len() != 6 {
                return None;
            }
            let number = |i: usize| fields[i].parse::<i32>().ok();
            let frame = Rect::new(number(1)?, number(2)?, number(3)?, number(4)?);
            if frame.w <= 0 || frame.h <= 0 {
                return None;
            }
            Some((
                fields[0].to_string(),
                Remembered {
                    frame,
                    maximized: fields[5] == "maximized",
                },
            ))
        })
        .collect()
}

fn format(entries: &[(String, Remembered)]) -> String {
    let mut text = String::new();
    for (key, r) in entries {
        text.push_str(&format!(
            "{key}\t{}\t{}\t{}\t{}\t{}\n",
            r.frame.x,
            r.frame.y,
            r.frame.w,
            r.frame.h,
            if r.maximized { "maximized" } else { "-" }
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record survives the file format, a changed record replaces the old one, and the
    /// oldest records go once the capacity is reached.
    #[test]
    fn records_round_trip_and_age_out() {
        let mut memory = WindowMemory::transient();
        let first = Remembered {
            frame: Rect::new(10, 20, 640, 480),
            maximized: false,
        };
        memory.remember("Org.Example.App", first);
        assert_eq!(memory.get("org.example.app"), Some(first));
        let parsed = parse(&format(&memory.entries));
        assert_eq!(parsed, memory.entries);
        let second = Remembered {
            frame: Rect::new(0, 0, 800, 600),
            maximized: true,
        };
        memory.remember("org.example.app", second);
        assert_eq!(memory.entries.len(), 1);
        assert_eq!(memory.get("org.example.app"), Some(second));
        for i in 0..CAPACITY {
            memory.remember(&format!("app{i}"), first);
        }
        assert_eq!(memory.entries.len(), CAPACITY);
        assert_eq!(memory.get("org.example.app"), None);
        assert!(memory.get("").is_none());
        assert!(parse("broken\tline\n").is_empty());
        assert!(parse("a\t0\t0\t0\t10\t-\n").is_empty());
    }
}
