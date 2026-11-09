/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The window manager's menus: the root menu `menu.xml` describes, with its pipe menus, the
//! window menu, and the window list. A menu is a chain of levels, each a column of entries
//! rasterized into one image the backend presents as an overlay, so a menu looks the same on
//! both backends and over any client. The entries are resolved by the engine (a pipe menu's
//! command runs on a thread of its own and its entries land on a later tick); this module lays
//! them out, draws them, and answers where a point lands.

use pixelflux::wm::{Image, Rect, Size, WindowId};
use selkies_draw::geom::Rect as DrawRect;
use selkies_draw::render::{Align, Canvas};
use selkies_draw::tiny_skia::{PathBuilder, Pixmap};

use crate::config::Action;
use crate::frame::{Renderer, draw_rect, image_of, shape_fit};
use crate::theme::Look;

/// Height of an entry in logical pixels.
pub const ROW: i32 = 28;
/// Height of a separator.
const SEPARATOR: i32 = 9;
/// Height of a separator with a label, a heading.
const HEADING: i32 = 24;
/// Padding inside the menu's border.
const PAD: i32 = 4;
/// Horizontal padding of an entry's label.
const LABEL_PAD: i32 = 10;
/// Side of an entry's icon.
const ICON: i32 = 16;
/// The font size of entries.
const FONT: f32 = 13.0;
/// Narrowest and widest a level is made.
const MIN_WIDTH: i32 = 180;
const MAX_WIDTH: i32 = 440;
/// Width kept for the submenu arrow.
const ARROW: i32 = 18;

/// What a submenu entry opens.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// A menu the files define, by id.
    Menu(String),
}

/// One row of a level.
#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    Item {
        label: String,
        icon: Option<String>,
        actions: Vec<Action>,
        /// The window the actions apply to, for the window menu and the window list.
        window: Option<WindowId>,
        enabled: bool,
        /// Drawn with a mark, for a state the entry toggles that is on.
        checked: bool,
    },
    Separator {
        label: Option<String>,
    },
    Submenu {
        label: String,
        icon: Option<String>,
        target: Target,
    },
}

impl Entry {
    /// Whether the pointer or the keyboard can rest on the entry.
    pub fn selectable(&self) -> bool {
        match self {
            Entry::Item { enabled, .. } => *enabled,
            Entry::Separator { .. } => false,
            Entry::Submenu { .. } => true,
        }
    }

    fn height(&self) -> i32 {
        match self {
            Entry::Separator { label: Some(_) } => HEADING,
            Entry::Separator { label: None } => SEPARATOR,
            _ => ROW,
        }
    }
}

/// One open level of a menu: its entries, where it is on the layout, and the row rectangles.
#[derive(Clone, Debug, PartialEq)]
pub struct Level {
    /// The id of the menu this level shows.
    pub id: String,
    pub entries: Vec<Entry>,
    /// The level's rectangle in layout coordinates.
    pub rect: Rect,
    /// Each entry's row, relative to the level's top-left corner.
    pub rows: Vec<Rect>,
    /// The entry the pointer rests on or the keyboard selected.
    pub selected: Option<usize>,
    /// The entry whose submenu is the next level.
    pub open: Option<usize>,
    /// The overlay key the level is presented under.
    pub key: u32,
    /// Whether the level still waits for a pipe menu's output.
    pub loading: bool,
}

impl Level {
    /// Lay the entries out at a position, within `bounds` (the output's workarea), the width
    /// measured from the labels.
    pub fn new(
        renderer: &mut Renderer,
        id: String,
        entries: Vec<Entry>,
        at: (i32, i32),
        bounds: Rect,
        scale: i32,
        key: u32,
    ) -> Level {
        let scale = scale.max(1);
        let mut width = MIN_WIDTH;
        for entry in &entries {
            let (label, extra) = match entry {
                Entry::Item { label, .. } => (label.as_str(), 0),
                Entry::Submenu { label, .. } => (label.as_str(), ARROW),
                Entry::Separator { label: Some(label) } => (label.as_str(), 0),
                Entry::Separator { label: None } => continue,
            };
            let text = renderer.fonts.shape(label, FONT * scale as f32, false);
            let needed = (text.width / scale as f32).ceil() as i32 + 2 * PAD + 3 * LABEL_PAD + ICON + extra;
            width = width.max(needed);
        }
        width = width.min(MAX_WIDTH);
        let mut rows = Vec::with_capacity(entries.len());
        let mut y = PAD;
        for entry in &entries {
            let h = entry.height();
            rows.push(Rect::new(PAD, y, width - 2 * PAD, h));
            y += h;
        }
        let height = y + PAD;
        let mut x = at.0;
        let mut y = at.1;
        if x + width > bounds.right() {
            x = (bounds.right() - width).max(bounds.x);
        }
        if y + height > bounds.bottom() {
            y = (bounds.bottom() - height).max(bounds.y);
        }
        Level {
            id,
            entries,
            rect: Rect::new(x, y, width, height),
            rows,
            selected: None,
            open: None,
            key,
            loading: false,
        }
    }

    /// The entry under a layout point.
    pub fn entry_at(&self, x: f64, y: f64) -> Option<usize> {
        if !self.rect.contains_f64(x, y) {
            return None;
        }
        let lx = x as i32 - self.rect.x;
        let ly = y as i32 - self.rect.y;
        self.rows.iter().position(|r| r.contains(lx, ly))
    }

    /// The layout rectangle of an entry's row.
    pub fn row_rect(&self, index: usize) -> Rect {
        let r = self.rows[index];
        Rect::new(self.rect.x + r.x, self.rect.y + r.y, r.w, r.h)
    }

    /// The next selectable entry after `from` in `step` (1 or -1), wrapping.
    pub fn step_selection(&self, from: Option<usize>, step: i32) -> Option<usize> {
        let n = self.entries.len();
        if n == 0 {
            return None;
        }
        let mut index = match from {
            Some(i) => i as i32,
            None if step > 0 => -1,
            None => n as i32,
        };
        for _ in 0..n {
            index = (index + step).rem_euclid(n as i32);
            if self.entries[index as usize].selectable() {
                return Some(index as usize);
            }
        }
        None
    }

    /// Draw the level as the overlay the backend presents.
    pub fn render(&self, renderer: &mut Renderer, look: &Look, scale: i32) -> Option<Image> {
        let scale = scale.max(1);
        let w = self.rect.w;
        let h = self.rect.h;
        let mut pixmap = Pixmap::new((w * scale) as u32, (h * scale) as u32)?;
        {
            let palette = &look.palette;
            let mut canvas = Canvas::new(pixmap.as_mut(), scale);
            let radius = look.style.corners.min(8) as f32;
            let whole = DrawRect::new(0, 0, w, h);
            canvas.fill_rounded(whole, radius, palette.menu_bg);
            canvas.stroke_rounded(whole, radius, 1.0, palette.menu_border);
            for (index, entry) in self.entries.iter().enumerate() {
                let row = draw_rect(self.rows[index]);
                let selected = self.selected == Some(index) || self.open == Some(index);
                match entry {
                    Entry::Separator { label: None } => {
                        canvas.fill_rect(
                            DrawRect::new(row.x + LABEL_PAD, row.y + row.h / 2, row.w - 2 * LABEL_PAD, 1),
                            palette.menu_separator,
                        );
                    }
                    Entry::Separator { label: Some(label) } => {
                        canvas.fill_rect(row, palette.menu_title_bg);
                        let text = shape_fit(
                            &mut renderer.fonts,
                            label,
                            FONT * scale as f32 * 0.92,
                            (row.w - 2 * LABEL_PAD) as f32 * scale as f32,
                        );
                        canvas.draw_text(
                            &mut renderer.fonts,
                            &text,
                            row.x + LABEL_PAD,
                            row.y + row.h / 2,
                            row.w - 2 * LABEL_PAD,
                            Align::Left,
                            palette.menu_disabled,
                        );
                    }
                    Entry::Item {
                        label,
                        icon,
                        enabled,
                        checked,
                        ..
                    } => {
                        draw_row(
                            &mut canvas,
                            renderer,
                            look,
                            row,
                            label,
                            icon.as_deref(),
                            *enabled,
                            *checked,
                            selected,
                            false,
                            scale,
                        );
                    }
                    Entry::Submenu { label, icon, .. } => {
                        draw_row(
                            &mut canvas,
                            renderer,
                            look,
                            row,
                            label,
                            icon.as_deref(),
                            true,
                            false,
                            selected,
                            true,
                            scale,
                        );
                    }
                }
            }
        }
        Some(image_of(pixmap, scale))
    }
}

/// One selectable row: the highlight, the icon or the check mark, the label, and the submenu arrow.
#[allow(clippy::too_many_arguments)]
fn draw_row(
    canvas: &mut Canvas<'_>,
    renderer: &mut Renderer,
    look: &Look,
    row: DrawRect,
    label: &str,
    icon: Option<&str>,
    enabled: bool,
    checked: bool,
    selected: bool,
    submenu: bool,
    scale: i32,
) {
    let palette = &look.palette;
    let text_color = if selected && enabled {
        canvas.fill_rounded(row, 4.0, palette.menu_active_bg);
        palette.menu_active_text
    } else if enabled {
        palette.menu_text
    } else {
        palette.menu_disabled
    };
    let icon_rect = DrawRect::new(row.x + LABEL_PAD, row.y + (row.h - ICON) / 2, ICON, ICON);
    if let Some(name) = icon
        && let Some(pixmap) = renderer.icons.get(name, (ICON * scale) as u32)
    {
        canvas.draw_image_fit(&pixmap, icon_rect);
    } else if checked {
        let mut path = PathBuilder::new();
        let x = icon_rect.x as f32;
        let y = icon_rect.y as f32;
        path.move_to(x + 3.0, y + 8.5);
        path.line_to(x + 6.5, y + 12.0);
        path.line_to(x + 13.0, y + 4.5);
        if let Some(path) = path.finish() {
            canvas.stroke_path(&path, 1.8, text_color);
        }
    }
    let label_x = icon_rect.right() + LABEL_PAD;
    let label_w = (row.right() - label_x - LABEL_PAD - if submenu { ARROW } else { 0 }).max(0);
    let shaped = shape_fit(
        &mut renderer.fonts,
        label,
        FONT * scale as f32,
        label_w as f32 * scale as f32,
    );
    canvas.draw_text(
        &mut renderer.fonts,
        &shaped,
        label_x,
        row.y + row.h / 2,
        label_w,
        Align::Left,
        text_color,
    );
    if submenu {
        let mut path = PathBuilder::new();
        let x = (row.right() - LABEL_PAD - 6) as f32;
        let y = (row.y + row.h / 2) as f32;
        path.move_to(x, y - 4.0);
        path.line_to(x + 4.0, y);
        path.line_to(x, y + 4.0);
        if let Some(path) = path.finish() {
            canvas.stroke_path(&path, 1.5, text_color);
        }
    }
}

/// Where a submenu of `parent` opened from its row `index` goes: beside the row to the right,
/// or to the left where the right would leave `bounds`.
pub fn submenu_position(parent: &Level, index: usize, width_hint: i32, bounds: Rect) -> (i32, i32) {
    let row = parent.row_rect(index);
    let mut x = parent.rect.right() - 2;
    if x + width_hint > bounds.right() {
        x = (parent.rect.x - width_hint + 2).max(bounds.x);
    }
    (x, row.y - PAD)
}

/// A size in logical pixels of a level with these entries, before it is laid out, for placing a
/// submenu that does not fit to the right.
pub fn size_hint(entries: &[Entry]) -> Size {
    Size {
        w: MIN_WIDTH,
        h: entries.iter().map(Entry::height).sum::<i32>() + 2 * PAD,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> Vec<Entry> {
        vec![
            Entry::Item {
                label: "Terminal".to_string(),
                icon: None,
                actions: vec![Action::Execute("x-terminal-emulator".to_string())],
                window: None,
                enabled: true,
                checked: false,
            },
            Entry::Separator { label: None },
            Entry::Submenu {
                label: "Applications".to_string(),
                icon: None,
                target: Target::Menu("selkies-applications".to_string()),
            },
            Entry::Item {
                label: "Disabled".to_string(),
                icon: None,
                actions: Vec::new(),
                window: None,
                enabled: false,
                checked: false,
            },
        ]
    }

    /// A level lays its rows out in order, stays inside its bounds, finds the entry under a
    /// point, and steps the selection over separators and disabled entries.
    #[test]
    fn a_level_lays_out_and_hit_tests() {
        let mut renderer = Renderer::new(None);
        let bounds = Rect::new(0, 0, 800, 600);
        let level = Level::new(
            &mut renderer,
            "root-menu".to_string(),
            entries(),
            (700, 550),
            bounds,
            1,
            1,
        );
        assert!(level.rect.right() <= 800 && level.rect.bottom() <= 600);
        assert_eq!(level.rows.len(), 4);
        assert_eq!(level.rows[1].h, SEPARATOR);
        let first = level.row_rect(0);
        assert_eq!(
            level.entry_at(first.x as f64 + 5.0, first.y as f64 + 5.0),
            Some(0)
        );
        assert_eq!(level.entry_at(0.0, 0.0), None);
        assert_eq!(level.step_selection(None, 1), Some(0));
        assert_eq!(level.step_selection(Some(0), 1), Some(2));
        assert_eq!(level.step_selection(Some(2), 1), Some(0));
        assert_eq!(level.step_selection(Some(0), -1), Some(2));
        let image = level.render(&mut renderer, &Look::default(), 2).unwrap();
        assert_eq!(image.width as i32, level.rect.w * 2);
        let (x, _) = submenu_position(&level, 2, MIN_WIDTH, bounds);
        assert!(x < level.rect.x);
    }
}
