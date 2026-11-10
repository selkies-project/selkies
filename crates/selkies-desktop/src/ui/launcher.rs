//! The quick launcher: a popup with one search field and a short list of
//! results, the window manager opens on a release of the Super key by running
//! `selkies-desktop --launcher`.  An empty field lists the recently launched
//! applications, or the favorites when there are none; typing searches the
//! applications, and a query whose first word is a program on the search
//! path, or that matches no application, ends in a row that runs it through
//! the shell.  It is the start menu's search without the menu around it, so
//! it borrows the menu's favorites and the same launch path.  Under the
//! rows of an empty field one dim line names the window manager's tiling
//! and desktop keys, the one place a new user is sure to pass.

use crate::desktop::{Desktop, Hover, Popup};
use crate::entries;
use crate::event::Button;
use crate::geom::Rect;
use crate::i18n::tr;
use crate::render::{Align, Canvas};
use crate::ui::{self, menu};
use crate::xkb::Key;

/// Launcher width.
const WIDTH: i32 = 560;
/// Height of the search field.
const SEARCH_HEIGHT: i32 = 46;
/// Font size of the search field.
const SEARCH_FONT: f32 = 16.0;
/// Height of a result row.
const ROW: i32 = 40;
/// Icon size of a result row.
const ICON: i32 = 22;
/// Inner padding.
const PAD: i32 = 8;
/// Width of the accent bar at the left of the selected row.
const ACCENT_BAR: i32 = 4;
/// Height of the hint line under the rows of an empty field.
const HINT: i32 = 24;
/// Space between a top bar and the launcher.
const TOP_OFFSET: i32 = 48;
/// Most rows the list shows.
const MAX_ITEMS: usize = 8;

/// One row of the result list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    /// An application, by entry index.
    App(usize),
    /// A command line to run through the shell.
    Command(String),
}

/// The launcher's state between events.
#[derive(Default)]
pub struct LauncherState {
    /// The search text.
    pub query: String,
    /// Row of the list the keyboard selected.
    pub selected: usize,
    items: Vec<Item>,
    items_valid: bool,
}

impl LauncherState {
    /// Resets the launcher for opening.
    pub fn open(&mut self) {
        self.query.clear();
        self.selected = 0;
        self.items_valid = false;
    }

    /// Clears the query when the launcher closes.
    pub fn close(&mut self) {
        self.query.clear();
        self.selected = 0;
        self.items_valid = false;
    }

    /// Forgets the computed list.
    pub fn invalidate(&mut self) {
        self.items_valid = false;
    }
}

/// The rows a query produces from the applications it matched: the matches
/// first, then a command row when the query's first word runs as a program
/// or nothing matched, so what is typed can always be run.
///
/// # Arguments
/// * `query` - The trimmed, non-empty query.
/// * `matches` - Entry indexes the query matched, best first.
/// * `runnable` - Whether the query's first word resolves as a program.
fn items_for(query: &str, matches: Vec<usize>, runnable: bool) -> Vec<Item> {
    let mut items: Vec<Item> = matches.into_iter().take(MAX_ITEMS).map(Item::App).collect();
    if runnable || items.is_empty() {
        items.push(Item::Command(query.to_string()));
    }
    items
}

/// Recomputes the list when the query or the applications changed.
fn ensure_items(d: &mut Desktop) {
    if d.launcher.items_valid {
        return;
    }
    let query = d.launcher.query.trim().to_string();
    let items = if query.is_empty() {
        let recent: Vec<usize> = if d.config.track_recent {
            entries::recent()
                .iter()
                .filter_map(|id| d.apps.find(id))
                .collect()
        } else {
            Vec::new()
        };
        let shown = if recent.is_empty() {
            menu::favorite_entries(d)
        } else {
            recent
        };
        shown.into_iter().take(MAX_ITEMS).map(Item::App).collect()
    } else {
        let runnable = query
            .split_whitespace()
            .next()
            .is_some_and(|word| entries::resolve_program(word).is_some());
        let mut items = items_for(&query, d.apps.search(&query), runnable);
        if !d.config.run_command {
            items.retain(|item| !matches!(item, Item::Command(_)));
        }
        items
    };
    d.launcher.items = items;
    d.launcher.items_valid = true;
    if !d.launcher.items.is_empty() && d.launcher.selected >= d.launcher.items.len() {
        d.launcher.selected = d.launcher.items.len() - 1;
    }
}

/// The rectangles of the launcher.
struct Layout {
    frame: Rect,
    search: Rect,
    /// The result rows, zero tall when there are none.
    rows: Rect,
}

/// Computes the launcher rectangles for the current list: centered on the
/// panel surface, below a top bar, and centered in the popup area above a
/// bottom bar.
fn layout(d: &Desktop) -> Layout {
    let bar = d.bar_rect();
    let count = d.launcher.items.len() as i32;
    let rows_h = if count > 0 { count * ROW + PAD } else { 0 };
    let hint_h = if d.launcher.query.is_empty() { HINT } else { 0 };
    let height = PAD + SEARCH_HEIGHT + rows_h + hint_h + PAD;
    let width = WIDTH.min(d.panel.width - 2 * ui::POPUP_MARGIN).max(1);
    let x = (d.panel.width - width) / 2;
    let y = if d.panel_top() {
        bar.bottom() + TOP_OFFSET
    } else {
        let room = bar.y - ui::POPUP_MARGIN - height;
        (room - (room / 2).max(0)).max(ui::POPUP_MARGIN)
    };
    let frame = Rect::new(x, y, width, height);
    let search = Rect::new(frame.x + PAD, frame.y + PAD, frame.w - 2 * PAD, SEARCH_HEIGHT);
    let rows = Rect::new(
        frame.x + PAD,
        search.bottom() + PAD,
        frame.w - 2 * PAD,
        (rows_h - PAD).max(0),
    );
    Layout { frame, search, rows }
}

/// The rectangle of a result row.
fn row_rect(layout: &Layout, row: usize) -> Rect {
    Rect::new(
        layout.rows.x,
        layout.rows.y + row as i32 * ROW,
        layout.rows.w,
        ROW,
    )
}

/// The hover target under a point, or `None` outside the launcher.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
pub fn hit(d: &mut Desktop, x: f64, y: f64) -> Option<Hover> {
    ensure_items(d);
    let layout = layout(d);
    if !layout.frame.contains(x, y) {
        return None;
    }
    if layout.rows.contains(x, y) {
        let row = ((y - layout.rows.y as f64) / ROW as f64).floor();
        if row >= 0.0 && (row as usize) < d.launcher.items.len() {
            return Some(Hover::LauncherRow(row as usize));
        }
    }
    Some(Hover::None)
}

/// Handles a click; returns whether it landed inside the launcher.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
/// * `button` - The button.
pub fn click(d: &mut Desktop, x: f64, y: f64, button: Button) -> bool {
    let Some(hover) = hit(d, x, y) else {
        return false;
    };
    if button == Button::Left
        && let Hover::LauncherRow(row) = hover
    {
        activate(d, row);
    }
    true
}

/// Handles wheel scrolling, which moves the selection; returns whether it
/// landed inside the launcher.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
/// * `delta` - Scroll amount, positive downwards.
pub fn scroll(d: &mut Desktop, x: f64, y: f64, delta: f64) -> bool {
    ensure_items(d);
    if !layout(d).frame.contains(x, y) {
        return false;
    }
    move_selection(d, if delta > 0.0 { 1 } else { -1 }, false);
    d.redraw_panel();
    true
}

/// Handles a key press while the launcher is open: text edits the query,
/// the arrows and Tab move the selection, and Enter activates it.
///
/// # Arguments
/// * `d` - The desktop.
/// * `key` - The key.
pub fn key(d: &mut Desktop, key: Key) {
    ensure_items(d);
    match key {
        Key::Char(c) => {
            d.launcher.query.push(c);
            d.launcher.selected = 0;
            d.launcher.items_valid = false;
        }
        Key::Backspace => {
            if d.launcher.query.pop().is_some() {
                d.launcher.selected = 0;
                d.launcher.items_valid = false;
            }
        }
        Key::Enter => {
            let row = d.launcher.selected;
            if row < d.launcher.items.len() {
                activate(d, row);
                return;
            }
        }
        Key::Down | Key::Tab => move_selection(d, 1, true),
        Key::Up => move_selection(d, -1, true),
        Key::PageDown => move_selection(d, MAX_ITEMS as i32, false),
        Key::PageUp => move_selection(d, -(MAX_ITEMS as i32), false),
        Key::Home => d.launcher.selected = 0,
        Key::End => d.launcher.selected = d.launcher.items.len().saturating_sub(1),
        Key::Left | Key::Right | Key::Delete | Key::Escape | Key::Super | Key::Function(_) => {}
    }
    d.redraw_panel();
}

/// Moves the selection by a number of rows, wrapping around the ends when
/// asked and stopping at them otherwise.
fn move_selection(d: &mut Desktop, by: i32, wrap: bool) {
    let count = d.launcher.items.len() as i32;
    if count == 0 {
        d.launcher.selected = 0;
        return;
    }
    let current = d.launcher.selected as i32;
    let next = if wrap {
        (current + by).rem_euclid(count)
    } else {
        (current + by).clamp(0, count - 1)
    };
    d.launcher.selected = next as usize;
}

/// Launches the item at a row and closes the launcher.
fn activate(d: &mut Desktop, row: usize) {
    match d.launcher.items.get(row).cloned() {
        Some(Item::App(index)) => d.launch_app(index),
        Some(Item::Command(command)) => {
            run_command(&command);
            d.close_popup();
        }
        None => {}
    }
}

/// Runs a command line through the shell, detached from the panel the way
/// an application launch is, in the user's home directory.
///
/// # Arguments
/// * `command` - The line as typed.
fn run_command(command: &str) {
    let argv = ["sh", "-c", command].map(str::to_string);
    let home = std::env::var("HOME").ok().filter(|h| !h.is_empty());
    entries::spawn_detached(&argv, home.as_deref());
}

/// Draws the launcher and returns its rectangle.
///
/// # Arguments
/// * `d` - The desktop.
/// * `canvas` - The panel canvas.
pub fn draw(d: &mut Desktop, canvas: &mut Canvas<'_>) -> Rect {
    ensure_items(d);
    let layout = layout(d);
    ui::draw_popup_frame(canvas, layout.frame);

    canvas.fill_rounded(layout.search, 8.0, ui::field_bg());
    if let Some(glass) = ui::search_path(layout.search.x + 22, layout.search.y + layout.search.h / 2) {
        canvas.stroke_path(&glass, 1.8, ui::text_dim());
    }
    let text_rect = Rect::new(
        layout.search.x + 44,
        layout.search.y,
        layout.search.w - 56,
        layout.search.h,
    );
    let query = d.launcher.query.clone();
    if query.is_empty() {
        ui::label(
            canvas,
            &mut d.fonts,
            tr("Type to search"),
            text_rect,
            SEARCH_FONT,
            false,
            Align::Left,
            ui::text_dim(),
        );
    } else {
        ui::label(
            canvas,
            &mut d.fonts,
            &query,
            text_rect,
            SEARCH_FONT,
            false,
            Align::Left,
            ui::text(),
        );
    }
    if d.popup == Popup::Launcher {
        let caret_x = text_rect.x
            + ui::text_width(&mut d.fonts, &query, SEARCH_FONT, false, d.panel.scale).min(text_rect.w - 2);
        canvas.fill_rect(
            Rect::new(caret_x + 1, text_rect.y + 12, 1, text_rect.h - 24),
            ui::text(),
        );
    }

    let items = d.launcher.items.clone();
    for (row, item) in items.iter().enumerate() {
        let rect = row_rect(&layout, row);
        let selected = d.launcher.selected == row;
        let hovered = d.hover == Hover::LauncherRow(row);
        if selected {
            canvas.fill_rounded(rect, 6.0, ui::active_bg());
            canvas.fill_rounded(
                Rect::new(rect.x, rect.y + 8, ACCENT_BAR, rect.h - 16),
                ACCENT_BAR as f32 / 2.0,
                ui::accent(),
            );
        } else if hovered {
            canvas.fill_rounded(rect, 6.0, ui::hover_bg());
        }
        let icon_rect = Rect::new(rect.x + 12, rect.y + (ROW - ICON) / 2, ICON, ICON);
        let (name, detail) = match item {
            Item::App(index) => {
                if let Some(icon) = d.app_icon_sized(*index, ICON) {
                    canvas.draw_image_fit(&icon, icon_rect);
                } else {
                    canvas.fill_rounded(icon_rect.inflate(-3), 4.0, ui::field_bg());
                }
                let entry = &d.apps.entries[*index];
                let detail = entry
                    .comment
                    .clone()
                    .or_else(|| entry.generic_name.clone())
                    .filter(|text| text != &entry.name);
                (entry.name.clone(), detail)
            }
            Item::Command(command) => {
                draw_prompt(canvas, icon_rect);
                (tr("Run command").to_string(), Some(command.clone()))
            }
        };
        let text_x = icon_rect.right() + 12;
        let text_w = rect.right() - 12 - text_x;
        let name_w = ui::text_width(&mut d.fonts, &name, ui::FONT_SIZE, false, d.panel.scale);
        let name_w = if detail.is_some() {
            name_w.min(text_w * 3 / 5)
        } else {
            name_w.min(text_w)
        };
        ui::label(
            canvas,
            &mut d.fonts,
            &name,
            Rect::new(text_x, rect.y, name_w, rect.h),
            ui::FONT_SIZE,
            false,
            Align::Left,
            ui::text(),
        );
        if let Some(detail) = detail {
            let detail_x = text_x + name_w + 14;
            ui::label(
                canvas,
                &mut d.fonts,
                &detail,
                Rect::new(detail_x, rect.y, rect.right() - 12 - detail_x, rect.h),
                ui::FONT_SMALL,
                false,
                Align::Right,
                ui::text_dim(),
            );
        }
    }
    if query.is_empty() {
        ui::label(
            canvas,
            &mut d.fonts,
            tr("Super+Arrows tile windows · Ctrl+Super+Arrows switch desktops"),
            Rect::new(
                layout.frame.x + PAD,
                layout.frame.bottom() - PAD - HINT,
                layout.frame.w - 2 * PAD,
                HINT,
            ),
            ui::FONT_SMALL,
            false,
            Align::Center,
            ui::text_dim(),
        );
    }
    layout.frame
}

/// Draws a shell prompt, a chevron over a cursor line, where a command row
/// has no icon.
fn draw_prompt(canvas: &mut Canvas<'_>, rect: Rect) {
    canvas.fill_rounded(rect, 4.0, ui::field_bg());
    if let Some(chevron) = ui::chevron_path(rect.x + 6, rect.y + rect.h / 2) {
        canvas.stroke_path(&chevron, 1.6, ui::text());
    }
    canvas.fill_rect(Rect::new(rect.x + 12, rect.y + rect.h / 2 + 4, 6, 2), ui::text());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_ends_in_a_command_row_when_it_runs_or_matches_nothing() {
        assert_eq!(
            items_for("fire", vec![3, 1], false),
            vec![Item::App(3), Item::App(1)]
        );
        assert_eq!(
            items_for("ls -la", vec![], false),
            vec![Item::Command("ls -la".to_string())]
        );
        assert_eq!(
            items_for("sh", vec![7], true),
            vec![Item::App(7), Item::Command("sh".to_string())]
        );
        let many: Vec<usize> = (0..20).collect();
        let items = items_for("a", many, true);
        assert_eq!(items.len(), MAX_ITEMS + 1);
        assert_eq!(
            items[..MAX_ITEMS],
            (0..MAX_ITEMS).map(Item::App).collect::<Vec<_>>()
        );
        assert_eq!(items[MAX_ITEMS], Item::Command("a".to_string()));
    }
}
