/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Where the desktop shell draws its icons, as the window manager learns it.
//!
//! A right click on the desktop opens the window manager's root menu, while one on an icon is
//! the shell's, which opens that icon's own menu. Only the shell knows where its icons stand,
//! and the window manager decides a click before any client sees it, so the shell publishes
//! the icons' cells in `selkies-desktop/icons` under the runtime directory and the window
//! manager reads that file on a click over the desktop, again only once its modification time
//! moved. Each line is one cell, `x y width height` in logical pixels from the primary
//! monitor's top-left corner: the shell knows its wallpaper's origin only relative to its own
//! output on Wayland, and the window manager places the rectangles on its primary output. A
//! line that does not read is skipped, and an absent file means no icons.

use std::path::PathBuf;

use crate::geom::Rect;

/// The file the shell publishes its icons in, under the runtime directory; `None` without one.
pub fn path() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty())?;
    Some(PathBuf::from(runtime).join("selkies-desktop").join("icons"))
}

/// The file's text for a list of cells.
pub fn format(cells: &[Rect]) -> String {
    cells
        .iter()
        .map(|r| format!("{} {} {} {}\n", r.x, r.y, r.w, r.h))
        .collect()
}

/// The cells a text lists, skipping a line that does not read as four integers or names an
/// empty rectangle.
pub fn parse(text: &str) -> Vec<Rect> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace().map(|f| f.parse::<i32>().ok());
            let rect = Rect::new(fields.next()??, fields.next()??, fields.next()??, fields.next()??);
            (fields.next().is_none() && !rect.is_empty()).then_some(rect)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_icons_read_back_as_they_were_written() {
        let cells = vec![Rect::new(10, 20, 96, 104), Rect::new(-4, 0, 1, 1)];
        assert_eq!(parse(&format(&cells)), cells);
    }

    #[test]
    fn a_line_that_does_not_read_is_skipped() {
        let text = "10 20 96 104\nnot a cell\n1 2 3\n1 2 3 4 5\n5 5 0 9\n0 0 8 8\n";
        assert_eq!(parse(text), [Rect::new(10, 20, 96, 104), Rect::new(0, 0, 8, 8)]);
    }
}
