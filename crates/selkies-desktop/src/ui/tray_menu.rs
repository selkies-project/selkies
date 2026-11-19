/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The D-Bus tray items on the bar and their menus.
//!
//! An item ([`crate::sni`]) is drawn by the shell itself in the tray's slot,
//! left of the XEmbed icons, from the pixels it sent or the icon it named.
//! A primary click activates it, or opens its menu where the item says it is
//! one, a middle click is its secondary activation, the wheel scrolls it,
//! and a secondary click opens its menu, or asks the item to show its own
//! where it exports none.
//!
//! A menu is fetched when it is asked for and opens when it arrives, so it
//! never shows empty or stale; one that arrives empty falls back to the
//! item's own context menu.  It cascades: hovering an entry with a submenu
//! opens it beside its row, the first level above the item as the other
//! popups of the bar open, and a submenu on the side of its parent that has
//! room.  Every level is drawn from the tree the application sent, and the
//! entry chosen is sent back by its id.  A menu the application changes
//! while it is open is fetched again; one whose item goes away closes.

use std::sync::Arc;

use tiny_skia::{PathBuilder, Pixmap};

use crate::desktop::{Desktop, Hover, Popup};
use crate::event::{Button, Request};
use crate::geom::Rect;
use crate::render::{Align, Canvas};
use crate::sni::{Command, Incoming, Item, MenuNode, Toggle};
use crate::ui::{self, panel};

/// Height of a row.
const ROW: i32 = 30;
/// Height of a separator.
const SEPARATOR: i32 = 9;
/// Padding inside a menu's frame.
const PAD: i32 = 6;
/// Width of the column a toggle's mark sits in.
const MARK: i32 = 20;
/// Width of the column a submenu's arrow sits in.
const ARROW: i32 = 18;
/// Narrowest a menu level is.
const MIN_WIDTH: i32 = 160;
/// Widest a menu level is.
const MAX_WIDTH: i32 = 360;
/// The wheel step sent per notch, as other hosts send it.
const SCROLL_STEP: i32 = 120;

/// The menu of a tray item, asked for or open.
#[derive(Clone, Debug, Default)]
pub struct TrayMenu {
    /// The item's key.
    pub key: String,
    /// The entries of its top level.
    pub entries: Vec<MenuNode>,
    /// The row of each open submenu, level by level.
    pub open: Vec<usize>,
    /// Whether it arrived and is showing, rather than asked for.
    pub shown: bool,
}

impl TrayMenu {
    /// The entries of each level shown: the top one and every open submenu.
    fn levels(&self) -> Vec<&[MenuNode]> {
        let mut levels = vec![self.entries.as_slice()];
        for &row in &self.open {
            match levels.last().and_then(|level| level.get(row)) {
                Some(node) if !node.children.is_empty() => levels.push(&node.children),
                _ => break,
            }
        }
        levels
    }
}

/// Takes in what the tray host learned.
///
/// # Arguments
/// * `d` - The desktop.
/// * `incoming` - What arrived.
pub fn receive(d: &mut Desktop, incoming: Incoming) {
    match incoming {
        Incoming::Item(item) => {
            match d.status_items.iter_mut().find(|i| i.key == item.key) {
                Some(slot) => *slot = *item,
                None => d.status_items.push(*item),
            }
            d.redraw_panel();
        }
        Incoming::Removed(key) => {
            d.status_items.retain(|i| i.key != key);
            if d.tray_menu.as_ref().is_some_and(|m| m.key == key) {
                if d.popup == Popup::TrayMenu {
                    d.close_popup();
                }
                d.tray_menu = None;
            }
            d.redraw_panel();
        }
        Incoming::Menu { key, entries } => menu_arrived(d, key, entries),
        Incoming::MenuChanged(key) => {
            if d.popup == Popup::TrayMenu && d.tray_menu.as_ref().is_some_and(|m| m.key == key) {
                d.requests.push(Request::Tray(Command::OpenMenu { key }));
            }
        }
    }
}

/// Shows a menu that arrived for the menu asked for or open, keeping the
/// submenus open that still exist; an empty one falls back to the item's
/// own context menu, or closes the open one.
fn menu_arrived(d: &mut Desktop, key: String, entries: Vec<MenuNode>) {
    let Some(menu) = d.tray_menu.as_mut().filter(|m| m.key == key) else {
        return;
    };
    if entries.is_empty() {
        let shown = menu.shown;
        d.tray_menu = None;
        if shown {
            d.close_popup();
        } else if let Some(index) = d.status_items.iter().position(|i| i.key == key) {
            let (x, y) = screen_point(d, index);
            d.requests.push(Request::Tray(Command::ContextMenu { key, x, y }));
        }
        return;
    }
    menu.entries = entries;
    let valid = menu.levels().len() - 1;
    menu.open.truncate(valid);
    if menu.shown {
        d.redraw_panel();
    } else {
        menu.shown = true;
        d.open_popup(Popup::TrayMenu);
    }
}

/// The icon of an item at a logical size: the pixels the host read, else
/// its icon name in the icon theme.
///
/// # Arguments
/// * `d` - The desktop.
/// * `item` - The item.
/// * `size` - The logical size.
pub fn icon(d: &mut Desktop, item: &Item, size: i32) -> Option<Arc<Pixmap>> {
    if let Some(pixmap) = &item.pixmap {
        return Some(Arc::clone(pixmap));
    }
    let px = (size * d.panel.scale.max(1)) as u32;
    d.icons.get(item.icon_name.as_deref()?, px)
}

/// The point on the screen at the middle of an item's button, for the
/// calls that take one.
fn screen_point(d: &mut Desktop, index: usize) -> (i32, i32) {
    let rect = panel::layout(d)
        .status
        .iter()
        .find(|(i, _)| *i == index)
        .map(|(_, r)| *r)
        .unwrap_or_default();
    (
        d.panel_offset.0 + rect.x + rect.w / 2,
        d.panel_offset.1 + rect.y + rect.h / 2,
    )
}

/// Handles a click on an item's button.
///
/// # Arguments
/// * `d` - The desktop.
/// * `index` - The item's position among the items.
/// * `button` - The button.
pub fn click_item(d: &mut Desktop, index: usize, button: Button) {
    let Some(item) = d.status_items.get(index) else {
        return;
    };
    let (key, has_menu, item_is_menu) = (item.key.clone(), item.has_menu, item.item_is_menu);
    let (x, y) = screen_point(d, index);
    let command = match button {
        Button::Left if item_is_menu && has_menu => None,
        Button::Left => Some(Command::Activate {
            key: key.clone(),
            x,
            y,
        }),
        Button::Middle => Some(Command::SecondaryActivate {
            key: key.clone(),
            x,
            y,
        }),
        Button::Right if has_menu => None,
        Button::Right => Some(Command::ContextMenu {
            key: key.clone(),
            x,
            y,
        }),
    };
    match command {
        Some(command) => d.requests.push(Request::Tray(command)),
        None => {
            d.tray_menu = Some(TrayMenu {
                key: key.clone(),
                ..TrayMenu::default()
            });
            d.requests.push(Request::Tray(Command::OpenMenu { key }));
        }
    }
}

/// Handles the wheel over an item's button.
///
/// # Arguments
/// * `d` - The desktop.
/// * `index` - The item's position among the items.
/// * `delta` - Scroll amount, positive downwards.
pub fn scroll_item(d: &mut Desktop, index: usize, delta: f64) {
    if let Some(item) = d.status_items.get(index) {
        let delta = if delta < 0.0 { SCROLL_STEP } else { -SCROLL_STEP };
        d.requests.push(Request::Tray(Command::Scroll {
            key: item.key.clone(),
            delta,
            horizontal: false,
        }));
    }
}

/// The tooltip of an item: its tooltip's title, else its title.
///
/// # Arguments
/// * `item` - The item.
pub fn tooltip(item: &Item) -> Option<String> {
    [&item.tooltip, &item.title]
        .into_iter()
        .find(|t| !t.is_empty())
        .cloned()
}

/// The height of an entry's row.
fn row_height(node: &MenuNode) -> i32 {
    if node.separator { SEPARATOR } else { ROW }
}

/// The width of a menu level: room for its longest label, the toggle marks
/// where any entry toggles, and the arrows where any opens a submenu.
fn level_width(d: &mut Desktop, entries: &[MenuNode]) -> i32 {
    let marks = if entries.iter().any(|e| e.toggle != Toggle::None) {
        MARK
    } else {
        0
    };
    let arrows = if entries.iter().any(|e| !e.children.is_empty()) {
        ARROW
    } else {
        0
    };
    let text = entries
        .iter()
        .map(|e| ui::text_width(&mut d.fonts, &e.label, ui::FONT_SIZE, false, d.panel.scale))
        .max()
        .unwrap_or(0);
    (text + marks + arrows + 2 * PAD + 20).clamp(MIN_WIDTH, MAX_WIDTH)
}

/// A shown level: its frame and its rows.
struct Level {
    frame: Rect,
    rows: Vec<Rect>,
}

/// The rows of a level whose frame is placed.
fn rows(frame: Rect, entries: &[MenuNode]) -> Vec<Rect> {
    let mut y = frame.y + PAD;
    entries
        .iter()
        .map(|entry| {
            let rect = Rect::new(frame.x + PAD, y, frame.w - 2 * PAD, row_height(entry));
            y += rect.h;
            rect
        })
        .collect()
}

/// Where each shown level of the open menu sits on the panel surface: the
/// top one above or below the bar at its item, as the bar's other popups,
/// and each submenu beside the row that opened it, on the right of its
/// parent where it fits and on the left otherwise, kept clear of the bar.
fn placed(d: &mut Desktop) -> Vec<Level> {
    let Some(menu) = d.tray_menu.clone().filter(|m| m.shown) else {
        return Vec::new();
    };
    let layout = panel::layout(d);
    let Some(anchor) = d
        .status_items
        .iter()
        .position(|i| i.key == menu.key)
        .and_then(|index| layout.status.iter().find(|(i, _)| *i == index))
        .map(|(_, r)| *r)
    else {
        return Vec::new();
    };
    let margin = ui::POPUP_MARGIN;
    let bar = d.bar_rect();
    let (top, bottom) = if d.panel_top() {
        (bar.bottom() + margin, d.panel.height - margin)
    } else {
        (margin, bar.y - margin)
    };
    let mut levels: Vec<Level> = Vec::new();
    for (depth, entries) in menu.levels().into_iter().enumerate() {
        let w = level_width(d, entries);
        let h = entries.iter().map(row_height).sum::<i32>() + 2 * PAD;
        let frame = match levels.last() {
            None => {
                let f = ui::popups::anchor_above(d, anchor, w, h);
                Rect::new(f.x, f.y.clamp(top, (bottom - h).max(top)), w, h)
            }
            Some(parent) => {
                let row = parent.rows[menu.open[depth - 1]];
                let right = parent.frame.right() - 2;
                let x = if right + w <= d.panel.width - margin {
                    right
                } else {
                    (parent.frame.x - w + 2).max(margin)
                };
                let y = (row.y - PAD).clamp(top, (bottom - h).max(top));
                Rect::new(x, y, w, h)
            }
        };
        levels.push(Level {
            rows: rows(frame, entries),
            frame,
        });
    }
    levels
}

/// Draws the open menu; returns the rectangles drawn.
///
/// # Arguments
/// * `d` - The desktop.
/// * `canvas` - The panel canvas.
pub fn draw(d: &mut Desktop, canvas: &mut Canvas<'_>) -> Vec<Rect> {
    let levels = placed(d);
    let Some(menu) = d.tray_menu.clone() else {
        return Vec::new();
    };
    for (depth, (level, entries)) in levels.iter().zip(menu.levels()).enumerate() {
        ui::draw_popup_frame(canvas, level.frame);
        let marks = entries.iter().any(|e| e.toggle != Toggle::None);
        for (row, (entry, rect)) in entries.iter().zip(&level.rows).enumerate() {
            let rect = *rect;
            if entry.separator {
                canvas.fill_rect(
                    Rect::new(rect.x + 8, rect.y + rect.h / 2, rect.w - 16, 1),
                    ui::separator(),
                );
                continue;
            }
            let lit = d.hover == Hover::TrayMenuItem(depth, row) || menu.open.get(depth) == Some(&row);
            if entry.enabled && lit {
                canvas.fill_rounded(rect, 5.0, ui::hover_bg());
            }
            let color = if entry.enabled { ui::text() } else { ui::text_dim() };
            let mut x = rect.x + 10;
            if marks {
                let mark = Rect::new(x, rect.y, MARK, rect.h);
                match entry.toggle {
                    Toggle::Check(true) => draw_check(canvas, mark, color),
                    Toggle::Radio(true) => draw_dot(canvas, mark, color),
                    _ => {}
                }
                x += MARK;
            }
            let arrow = if entry.children.is_empty() { 0 } else { ARROW };
            ui::label(
                canvas,
                &mut d.fonts,
                &entry.label,
                Rect::new(x, rect.y, rect.right() - 10 - arrow - x, rect.h),
                ui::FONT_SIZE,
                false,
                Align::Left,
                color,
            );
            if arrow > 0 {
                draw_arrow(
                    canvas,
                    Rect::new(rect.right() - 8 - arrow, rect.y, arrow, rect.h),
                    color,
                );
            }
        }
    }
    levels.into_iter().map(|l| l.frame).collect()
}

/// A check mark centered in a box.
fn draw_check(canvas: &mut Canvas<'_>, rect: Rect, color: tiny_skia::Color) {
    let (cx, cy) = (
        rect.x as f32 + rect.w as f32 / 2.0,
        rect.y as f32 + rect.h as f32 / 2.0,
    );
    let mut pb = PathBuilder::new();
    pb.move_to(cx - 5.0, cy);
    pb.line_to(cx - 1.5, cy + 3.5);
    pb.line_to(cx + 5.0, cy - 4.0);
    if let Some(path) = pb.finish() {
        canvas.stroke_path(&path, 1.8, color);
    }
}

/// A radio button's dot centered in a box.
fn draw_dot(canvas: &mut Canvas<'_>, rect: Rect, color: tiny_skia::Color) {
    let side = 8;
    let dot = Rect::new(
        rect.x + (rect.w - side) / 2,
        rect.y + (rect.h - side) / 2,
        side,
        side,
    );
    canvas.fill_rounded(dot, side as f32 / 2.0, color);
}

/// A submenu's arrow centered in a box.
fn draw_arrow(canvas: &mut Canvas<'_>, rect: Rect, color: tiny_skia::Color) {
    let (cx, cy) = (
        rect.x as f32 + rect.w as f32 / 2.0,
        rect.y as f32 + rect.h as f32 / 2.0,
    );
    let mut pb = PathBuilder::new();
    pb.move_to(cx - 2.0, cy - 4.5);
    pb.line_to(cx + 2.5, cy);
    pb.line_to(cx - 2.0, cy + 4.5);
    if let Some(path) = pb.finish() {
        canvas.stroke_path(&path, 1.6, color);
    }
}

/// The entry under a point of the panel surface, or `None` outside the
/// menu.  Resting on an entry closes the submenus of other rows at its level
/// and opens its own, telling the application it is about to show.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
pub fn hit(d: &mut Desktop, x: f64, y: f64) -> Option<Hover> {
    if d.popup != Popup::TrayMenu {
        return None;
    }
    let levels = placed(d);
    let (depth, level) = levels
        .iter()
        .enumerate()
        .rev()
        .find(|(_, l)| l.frame.contains(x, y))?;
    let Some(row) = level.rows.iter().position(|r| r.contains(x, y)) else {
        return Some(Hover::None);
    };
    let menu = d.tray_menu.as_mut()?;
    let node = menu
        .levels()
        .get(depth)
        .and_then(|entries| entries.get(row))
        .cloned()?;
    if node.separator || !node.enabled {
        return Some(Hover::None);
    }
    let was_open = menu.open.get(depth) == Some(&row);
    let mut open = menu.open.clone();
    open.truncate(depth);
    if !node.children.is_empty() {
        open.push(row);
    }
    if open != menu.open {
        menu.open = open;
        if !node.children.is_empty() && !was_open {
            let key = menu.key.clone();
            d.requests
                .push(Request::Tray(Command::MenuAboutToShow { key, id: node.id }));
        }
        d.redraw_panel();
    }
    Some(Hover::TrayMenuItem(depth, row))
}

/// Handles a click; returns whether it landed in the menu.  Choosing an
/// entry without a submenu sends it to the application and closes the menu.
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
    if button == Button::Middle {
        return true;
    }
    if let Hover::TrayMenuItem(depth, row) = hover
        && let Some(menu) = d.tray_menu.as_ref()
        && let Some(node) = menu.levels().get(depth).and_then(|entries| entries.get(row))
        && node.children.is_empty()
    {
        let command = Command::MenuClicked {
            key: menu.key.clone(),
            id: node.id,
        };
        d.close_popup();
        d.requests.push(Request::Tray(command));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, SurfaceId};

    /// A menu entry.
    fn entry(id: i32, label: &str, children: Vec<MenuNode>) -> MenuNode {
        MenuNode {
            id,
            label: label.to_string(),
            enabled: true,
            children,
            ..MenuNode::default()
        }
    }

    /// A desktop with a configured bar and one item that exports a menu.
    fn desktop() -> Desktop {
        let mut d = Desktop::load(false);
        d.config.launchers.clear();
        d.config.show_tray = true;
        d.handle(Event::Configured {
            surface: SurfaceId::Panel,
            width: 1280,
            height: 800,
            scale: 1,
        });
        receive(
            &mut d,
            Incoming::Item(Box::new(Item {
                key: ":1.5/StatusNotifierItem".into(),
                title: "Downloads".into(),
                has_menu: true,
                ..Item::default()
            })),
        );
        d.take_requests();
        d
    }

    /// The center of a rectangle.
    fn center(r: Rect) -> (f64, f64) {
        (
            f64::from(r.x) + f64::from(r.w) / 2.0,
            f64::from(r.y) + f64::from(r.h) / 2.0,
        )
    }

    /// The requests the tray host is handed.
    fn tray_requests(d: &mut Desktop) -> Vec<Command> {
        d.take_requests()
            .into_iter()
            .filter_map(|r| match r {
                Request::Tray(command) => Some(command),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn an_item_gets_a_button_on_the_bar_that_activates_it_and_shows_its_title() {
        let mut d = desktop();
        let layout = panel::layout(&mut d);
        let [(0, rect)] = layout.status.as_slice() else {
            panic!("one button");
        };
        let (x, y) = center(*rect);
        assert_eq!(panel::hit(&mut d, x, y), Hover::StatusItem(0));
        assert_eq!(tooltip(&d.status_items[0]).as_deref(), Some("Downloads"));
        panel::click(&mut d, x, y, Button::Left, 0);
        assert!(matches!(
            tray_requests(&mut d).as_slice(),
            [Command::Activate { key, .. }] if key == ":1.5/StatusNotifierItem"
        ));
        panel::click(&mut d, x, y, Button::Middle, 0);
        assert!(matches!(
            tray_requests(&mut d).as_slice(),
            [Command::SecondaryActivate { .. }]
        ));
    }

    #[test]
    fn a_secondary_click_opens_the_menu_once_it_arrives_and_a_click_sends_the_entry() {
        let mut d = desktop();
        let (x, y) = center(panel::layout(&mut d).status[0].1);
        panel::click(&mut d, x, y, Button::Right, 0);
        assert!(matches!(
            tray_requests(&mut d).as_slice(),
            [Command::OpenMenu { .. }]
        ));
        assert_eq!(d.popup, Popup::None, "nothing shows before the menu arrives");
        let key = d.status_items[0].key.clone();
        receive(
            &mut d,
            Incoming::Menu {
                key,
                entries: vec![
                    entry(1, "Show", vec![]),
                    entry(2, "More", vec![entry(3, "Inner", vec![])]),
                    entry(4, "Quit", vec![]),
                ],
            },
        );
        assert_eq!(d.popup, Popup::TrayMenu);
        let levels = placed(&mut d);
        assert_eq!(levels.len(), 1);
        assert!(
            levels[0].frame.bottom() <= d.bar_rect().y,
            "the menu opens above the bar"
        );

        let (mx, my) = center(levels[0].rows[1]);
        assert_eq!(hit(&mut d, mx, my), Some(Hover::TrayMenuItem(0, 1)));
        assert!(matches!(
            tray_requests(&mut d).as_slice(),
            [Command::MenuAboutToShow { id: 2, .. }]
        ));
        let levels = placed(&mut d);
        assert_eq!(levels.len(), 2, "resting on a submenu's row opens it");
        let (parent, sub) = (levels[0].frame, levels[1].frame);
        assert!(
            sub.right() <= parent.x + 2,
            "beside the parent, on the left where the bar's right end leaves no room"
        );

        let (ix, iy) = center(levels[1].rows[0]);
        assert!(click(&mut d, ix, iy, Button::Left));
        assert!(matches!(
            tray_requests(&mut d).as_slice(),
            [Command::MenuClicked { id: 3, .. }]
        ));
        assert_eq!(d.popup, Popup::None);
    }

    #[test]
    fn an_empty_menu_falls_back_to_the_items_own_context_menu() {
        let mut d = desktop();
        let (x, y) = center(panel::layout(&mut d).status[0].1);
        panel::click(&mut d, x, y, Button::Right, 0);
        tray_requests(&mut d);
        let key = d.status_items[0].key.clone();
        receive(&mut d, Incoming::Menu { key, entries: vec![] });
        assert_eq!(d.popup, Popup::None);
        assert!(matches!(
            tray_requests(&mut d).as_slice(),
            [Command::ContextMenu { .. }]
        ));
    }

    #[test]
    fn an_item_going_away_takes_its_button_and_open_menu_with_it() {
        let mut d = desktop();
        let (x, y) = center(panel::layout(&mut d).status[0].1);
        panel::click(&mut d, x, y, Button::Right, 0);
        let key = d.status_items[0].key.clone();
        receive(
            &mut d,
            Incoming::Menu {
                key: key.clone(),
                entries: vec![entry(1, "Show", vec![])],
            },
        );
        assert_eq!(d.popup, Popup::TrayMenu);
        receive(&mut d, Incoming::Removed(key));
        assert_eq!(d.popup, Popup::None);
        assert!(panel::layout(&mut d).status.is_empty());
    }
}
