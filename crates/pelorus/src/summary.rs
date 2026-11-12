/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The desktop as the few lines a model reads first, and what an action changed.
//!
//! [`write`] lists the windows top of the stack first, each with the number every other tool
//! names it by, its title, its application, where it is, and its state. [`changes`] compares
//! two such lists and says what is different, which is what an action answers with: a click
//! that opened a dialog reads "opened #7", so a model learns what its action did without
//! asking for the whole desktop again, and an action that changed no window says only that.

use std::fmt::Write as _;

use crate::element::Rect;

/// One window as the summary lists it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WindowLine {
    pub id: u32,
    pub title: String,
    /// The application's id or class.
    pub app: String,
    /// The window with its frame, in screen pixels.
    pub rect: Rect,
    pub focused: bool,
    pub minimized: bool,
    pub maximized: bool,
    pub fullscreen: bool,
    /// On a virtual desktop other than the one shown.
    pub elsewhere: bool,
}

impl WindowLine {
    fn line(&self) -> String {
        let mut flags = Vec::new();
        if self.focused {
            flags.push("focused");
        }
        if self.minimized {
            flags.push("minimized");
        }
        if self.elsewhere {
            flags.push("on another desktop");
        }
        if self.fullscreen {
            flags.push("fullscreen");
        } else if self.maximized {
            flags.push("maximized");
        }
        let mut line = format!("#{} \"{}\"", self.id, self.title);
        if !self.app.is_empty() {
            let _ = write!(line, " ({})", self.app);
        }
        if !flags.is_empty() {
            let _ = write!(line, " [{}]", flags.join(", "));
        }
        if !self.minimized && !self.elsewhere {
            let _ = write!(
                line,
                " at {},{} size {}x{}",
                self.rect.x, self.rect.y, self.rect.w, self.rect.h
            );
        }
        line
    }
}

/// The summary: the screen's size and the windows, `windows` given top of the stack first.
pub fn write(screen: (i32, i32), windows: &[WindowLine]) -> String {
    let mut out = format!("Screen {}x{}. ", screen.0, screen.1);
    if windows.is_empty() {
        out.push_str("No windows are open.\n");
        return out;
    }
    let _ = writeln!(out, "{} windows, topmost first:", windows.len());
    for window in windows {
        let _ = writeln!(out, "{}", window.line());
    }
    out
}

/// What differs between two lists of windows, as sentences; empty where nothing does.
pub fn changes(before: &[WindowLine], after: &[WindowLine]) -> String {
    let mut out = String::new();
    for window in after {
        match before.iter().find(|w| w.id == window.id) {
            None => {
                let _ = writeln!(out, "Opened {}", window.line());
            }
            Some(old) => {
                if old.title != window.title {
                    let _ = writeln!(out, "#{} is now titled \"{}\"", window.id, window.title);
                }
                if !old.focused && window.focused {
                    let _ = writeln!(out, "#{} \"{}\" now has focus", window.id, window.title);
                }
                if old.minimized != window.minimized {
                    let what = if window.minimized {
                        "minimized"
                    } else {
                        "restored"
                    };
                    let _ = writeln!(out, "#{} was {what}", window.id);
                }
                if old.rect != window.rect && !window.minimized {
                    let _ = writeln!(
                        out,
                        "#{} is now at {},{} size {}x{}",
                        window.id, window.rect.x, window.rect.y, window.rect.w, window.rect.h
                    );
                }
            }
        }
    }
    for window in before {
        if !after.iter().any(|w| w.id == window.id) {
            let _ = writeln!(out, "Closed #{} \"{}\"", window.id, window.title);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(id: u32, title: &str, focused: bool) -> WindowLine {
        WindowLine {
            id,
            title: title.to_string(),
            app: "app".to_string(),
            rect: Rect {
                x: 10,
                y: 20,
                w: 300,
                h: 200,
            },
            focused,
            ..WindowLine::default()
        }
    }

    #[test]
    fn the_summary_names_each_window_by_its_number_and_place() {
        let text = write((1920, 1080), &[window(3, "Notes", true)]);
        assert_eq!(
            text,
            "Screen 1920x1080. 1 windows, topmost first:\n#3 \"Notes\" (app) [focused] at 10,20 size 300x200\n"
        );
        assert_eq!(
            write((800, 600), &[]),
            "Screen 800x600. No windows are open.\n"
        );
    }

    #[test]
    fn a_minimized_window_is_listed_without_a_place() {
        let mut hidden = window(4, "Mail", false);
        hidden.minimized = true;
        assert!(write((800, 600), &[hidden]).contains("#4 \"Mail\" (app) [minimized]\n"));
    }

    #[test]
    fn changes_say_what_opened_closed_and_took_focus() {
        let before = [window(1, "A", true), window(2, "B", false)];
        let mut b = window(2, "B2", true);
        b.rect.x = 50;
        let after = [b, window(5, "Save As", false)];
        let text = changes(&before, &after);
        assert!(text.contains("#2 is now titled \"B2\""));
        assert!(text.contains("#2 \"B2\" now has focus"));
        assert!(text.contains("#2 is now at 50,20 size 300x200"));
        assert!(text.contains("Opened #5 \"Save As\""));
        assert!(text.contains("Closed #1 \"A\""));
        assert_eq!(changes(&before, &before), "");
    }
}
