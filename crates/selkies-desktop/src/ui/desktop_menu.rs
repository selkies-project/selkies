/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The right-click menu of the desktop's icons: what the file manager's menu offers an item,
//! for the icons selected.
//!
//! A right click on an icon selects it unless it already is, so the menu acts on every
//! selected icon: Open, Open with an application where they are files of one type, Cut and
//! Copy onto the clipboard the file manager's windows share, Paste into folder on a single
//! folder, Rename on a single item, and Delete. Rename is grayed out on a launcher, whose name
//! is its application's, read from the file, rather than the file's own. The entries are worked
//! out once as the menu opens, since Open with reads desktop entries.
//!
//! A drop on the desktop that would copy or move files, from a window onto it or onto one of
//! its folders, opens the same menu with Copy here, Move here, and Cancel.
//!
//! The menu is a popup of the panel surface, which covers the primary output while a popup is
//! open, so a click anywhere dismisses it and its keys reach it on both backends; the desktop's
//! icons stand on the primary output too. The window manager leaves a right click on an icon to
//! the shell rather than opening its root menu (`selkies_draw::desktop_icons`).

use std::path::PathBuf;

use crate::desktop::{Desktop, Hover, Popup};
use crate::entries;
use crate::event::Button;
use crate::files;
use crate::geom::Rect;
use crate::i18n::{tr, trf};
use crate::render::{Align, Canvas};
use crate::ui;

/// Width of the menu.
const WIDTH: i32 = 250;
/// Height of a row.
const ROW: i32 = 30;
/// Height of a separator.
const SEPARATOR: i32 = 9;
/// Most applications offered to open the selection with.
const OPEN_WITH_MAX: usize = 5;

/// What a row of the menu does to the selected icons.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Open each icon as a double click would.
    Open,
    /// Open the files with an application, by its desktop file ID.
    OpenWith(String),
    /// Cut the items onto the clipboard.
    Cut,
    /// Copy the items onto the clipboard.
    Copy,
    /// Paste the clipboard into the folder selected.
    PasteInto(PathBuf),
    /// Ask for the item's new name.
    Rename,
    /// Ask whether to delete the items.
    Delete,
    /// Carry the drop waiting on the desktop out: a move where set, else a copy.
    Drop(bool),
    /// Leave the drop waiting on the desktop undone.
    CancelDrop,
}

/// One entry of the menu.
#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    /// A row: its label, what it does, and whether it can be chosen.
    Item {
        label: String,
        action: Action,
        enabled: bool,
    },
    /// A hairline between rows.
    Separator,
}

impl Entry {
    /// An enabled row.
    fn item(label: &str, action: Action) -> Entry {
        Entry::Item {
            label: label.to_string(),
            action,
            enabled: true,
        }
    }

    /// The entry's height.
    fn height(&self) -> i32 {
        match self {
            Entry::Item { .. } => ROW,
            Entry::Separator => SEPARATOR,
        }
    }
}

/// The entries for the selected icons.
///
/// # Arguments
/// * `d` - The desktop.
fn entries_for(d: &mut Desktop) -> Vec<Entry> {
    let selected = d.selected_desktop_items();
    let Some(first) = selected.first() else {
        return Vec::new();
    };
    let mut entries = vec![Entry::item(tr("Open"), Action::Open)];
    let one_mime = selected
        .iter()
        .all(|s| s.launcher.is_none() && !s.item.is_dir && s.item.mime == first.item.mime)
        .then(|| first.item.mime.clone());
    if let Some(mime) = one_mime {
        let apps = d
            .files
            .mime
            .handlers(&mime)
            .into_iter()
            .filter_map(|handler| entries::load_entry(&handler).map(|entry| (handler, entry)))
            .take(OPEN_WITH_MAX);
        for (handler, entry) in apps {
            entries.push(Entry::item(
                &trf("Open with {app}", &[("app", &entry.name)]),
                Action::OpenWith(handler),
            ));
        }
    }
    entries.push(Entry::Separator);
    entries.push(Entry::item(tr("Cut"), Action::Cut));
    entries.push(Entry::item(tr("Copy"), Action::Copy));
    let single = selected.len() == 1;
    if single && first.item.is_dir {
        entries.push(Entry::Item {
            label: tr("Paste into folder").to_string(),
            action: Action::PasteInto(first.item.path.clone()),
            enabled: d.files.clipboard.is_some(),
        });
    }
    entries.push(Entry::Separator);
    entries.push(Entry::Item {
        label: tr("Rename").to_string(),
        action: Action::Rename,
        enabled: single && first.launcher.is_none(),
    });
    entries.push(Entry::item(tr("Delete"), Action::Delete));
    entries
}

/// Opens the menu for the selected icons at a point of the primary background.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - The point's X on the background.
/// * `y` - The point's Y on the background.
pub fn open(d: &mut Desktop, x: f64, y: f64) {
    d.desktop_menu = entries_for(d);
    if d.desktop_menu.is_empty() {
        return;
    }
    let at = (x as i32 - d.panel_offset.0, y as i32 - d.panel_offset.1);
    d.open_popup(Popup::DesktopMenu(at.0, at.1));
}

/// Opens the menu a drop on the desktop asks with, Copy here or Move here, at a point of the
/// primary background; the drop waits in [`Desktop::desktop_drop`].
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - The drop's X on the background.
/// * `y` - The drop's Y on the background.
pub fn open_drop(d: &mut Desktop, x: f64, y: f64) {
    d.desktop_menu = vec![
        Entry::item(tr("Copy here"), Action::Drop(false)),
        Entry::item(tr("Move here"), Action::Drop(true)),
        Entry::Separator,
        Entry::item(tr("Cancel"), Action::CancelDrop),
    ];
    let at = (x as i32 - d.panel_offset.0, y as i32 - d.panel_offset.1);
    d.open_popup(Popup::DesktopMenu(at.0, at.1));
}

/// The menu's rectangle on the panel surface: from the point it opened at, kept inside the
/// surface and clear of the bar, opening up or left where it would not fit down and right.
fn menu_rect(d: &Desktop, (x, y): (i32, i32)) -> Rect {
    let h: i32 = d.desktop_menu.iter().map(Entry::height).sum::<i32>() + 12;
    let margin = ui::POPUP_MARGIN;
    let bar = d.bar_rect();
    let (top, bottom) = if d.panel_top() {
        (bar.bottom() + margin, d.panel.height - margin)
    } else {
        (margin, bar.y - margin)
    };
    let x = if x + WIDTH > d.panel.width - margin {
        x - WIDTH
    } else {
        x
    };
    let y = if y + h > bottom { y - h } else { y };
    Rect::new(
        x.clamp(margin, (d.panel.width - margin - WIDTH).max(margin)),
        y.clamp(top, (bottom - h).max(top)),
        WIDTH,
        h,
    )
}

/// The rectangles of the menu's entries, in order.
fn rows(d: &Desktop, frame: Rect) -> Vec<Rect> {
    let mut y = frame.y + 6;
    d.desktop_menu
        .iter()
        .map(|entry| {
            let rect = Rect::new(frame.x + 6, y, frame.w - 12, entry.height());
            y += entry.height();
            rect
        })
        .collect()
}

/// Draws the menu; returns its rectangle.
///
/// # Arguments
/// * `d` - The desktop.
/// * `canvas` - The panel canvas.
/// * `at` - Where it opened, on the panel surface.
pub fn draw(d: &mut Desktop, canvas: &mut Canvas<'_>, at: (i32, i32)) -> Rect {
    let frame = menu_rect(d, at);
    ui::draw_popup_frame(canvas, frame);
    let rows = rows(d, frame);
    for (row, (entry, rect)) in d.desktop_menu.clone().into_iter().zip(rows).enumerate() {
        let Entry::Item { label, enabled, .. } = entry else {
            canvas.fill_rect(
                Rect::new(rect.x + 8, rect.y + rect.h / 2, rect.w - 16, 1),
                ui::separator(),
            );
            continue;
        };
        if enabled && d.hover == Hover::DesktopMenuItem(row) {
            canvas.fill_rounded(rect, 5.0, ui::hover_bg());
        }
        ui::label(
            canvas,
            &mut d.fonts,
            &label,
            Rect::new(rect.x + 10, rect.y, rect.w - 20, rect.h),
            ui::FONT_SIZE,
            false,
            Align::Left,
            if enabled { ui::text() } else { ui::text_dim() },
        );
    }
    frame
}

/// The menu entry under a point of the panel surface, or `None` outside the menu.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
pub fn hit(d: &Desktop, x: f64, y: f64) -> Option<Hover> {
    let Popup::DesktopMenu(ax, ay) = d.popup else {
        return None;
    };
    let frame = menu_rect(d, (ax, ay));
    if !frame.contains(x, y) {
        return None;
    }
    let rows = rows(d, frame);
    for (row, (entry, rect)) in d.desktop_menu.iter().zip(rows).enumerate() {
        if rect.contains(x, y) {
            return match entry {
                Entry::Item { enabled: true, .. } => Some(Hover::DesktopMenuItem(row)),
                _ => Some(Hover::None),
            };
        }
    }
    Some(Hover::None)
}

/// Handles a click; returns whether it landed in the menu.
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
    if button != Button::Left {
        return true;
    }
    if let Hover::DesktopMenuItem(row) = hover
        && let Some(Entry::Item { action, .. }) = d.desktop_menu.get(row).cloned()
    {
        d.close_popup();
        perform(d, action);
    }
    true
}

/// Carries a row's action out on the selected icons.
fn perform(d: &mut Desktop, action: Action) {
    if !matches!(action, Action::Drop(_)) {
        d.desktop_drop = None;
    }
    let selected = d.selected_desktop_items();
    let paths: Vec<PathBuf> = selected.iter().map(|s| s.item.path.clone()).collect();
    match action {
        Action::Open => d.open_desktop_selection(),
        Action::OpenWith(handler) => {
            if let Some(entry) = entries::load_entry(&handler) {
                entries::launch_with(&entry, &d.config, &paths);
            }
        }
        Action::Cut | Action::Copy => {
            d.files.clipboard = Some(files::Clipboard {
                paths,
                cut: action == Action::Cut,
            });
            for window in &mut d.files.windows {
                window.dirty = true;
            }
        }
        Action::PasteInto(dir) => d.paste_into(dir),
        Action::Rename => {
            if let [item] = selected.as_slice() {
                let cell = d.desktop_cell_of(&item.file);
                ui::dialog::open_rename(d, item.item.path.clone(), cell);
            }
        }
        Action::Delete => ui::dialog::open_delete(d, paths),
        Action::Drop(moving) => d.finish_desktop_drop(moving),
        Action::CancelDrop => d.desktop_drop = None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::desktop_folder::DesktopItem;
    use crate::event::{Event, Modifiers, SurfaceId};
    use crate::ui::background;

    /// A Desktop item: a launcher where `launcher` is set, else a text file.
    fn item(file: &str, launcher: bool) -> DesktopItem {
        let path = PathBuf::from(format!("/nonexistent/Desktop/{file}"));
        DesktopItem {
            file: file.to_string(),
            name: file.to_string(),
            launcher: launcher.then(|| selkies_draw::apps::Entry {
                id: file.to_string(),
                name: file.to_string(),
                comment: None,
                generic_name: None,
                keywords: Vec::new(),
                exec: "true".to_string(),
                icon: None,
                path: path.clone(),
                terminal: false,
                emulator: false,
                working_dir: None,
                category: String::new(),
                wm_class: None,
            }),
            item: files::Item {
                name: file.to_string(),
                path,
                is_dir: false,
                is_symlink: false,
                size: 0,
                modified: 0,
                mime: if launcher {
                    "application/x-desktop".to_string()
                } else {
                    "text/plain".to_string()
                },
                hidden: false,
                executable: false,
            },
        }
    }

    /// A desktop showing items on a primary background tall enough for a column of them.
    fn desktop(items: Vec<DesktopItem>) -> Desktop {
        let mut d = Desktop::load(false);
        d.desktop_items = items;
        d.desktop_layout.clear();
        d.handle(Event::Configured {
            surface: SurfaceId::Background(1),
            width: 800,
            height: 600,
            scale: 1,
        });
        d.handle(Event::BackgroundLayout {
            surface: 1,
            monitors: vec![Rect::new(0, 0, 800, 600)],
            primary: true,
        });
        d
    }

    /// Presses and releases a button on an item's cell, with modifiers.
    fn click(d: &mut Desktop, index: usize, button: Button, modifiers: Modifiers, time: u32) {
        let cell = background::cells(d)[index];
        let (x, y) = (f64::from(cell.x + cell.w / 2), f64::from(cell.y + 20));
        for pressed in [true, false] {
            d.handle(Event::PointerButton {
                surface: SurfaceId::Background(1),
                x,
                y,
                button,
                pressed,
                time,
                modifiers,
            });
        }
    }

    fn selection(d: &Desktop) -> Vec<usize> {
        d.desktop_selection.iter().copied().collect()
    }

    fn rename_enabled(d: &Desktop) -> bool {
        d.desktop_menu.iter().any(|e| {
            matches!(
                e,
                Entry::Item {
                    action: Action::Rename,
                    enabled: true,
                    ..
                }
            )
        })
    }

    const NONE: Modifiers = Modifiers {
        shift: false,
        ctrl: false,
        alt: false,
        logo: false,
    };
    const CTRL: Modifiers = Modifiers { ctrl: true, ..NONE };
    const SHIFT: Modifiers = Modifiers { shift: true, ..NONE };

    /// Moves the pointer over the background.
    fn motion(d: &mut Desktop, x: f64, y: f64) {
        d.handle(Event::PointerMotion {
            surface: SurfaceId::Background(1),
            x,
            y,
        });
    }

    /// The center of a grid cell on the background.
    fn center(d: &Desktop, cell: (i32, i32)) -> (f64, f64) {
        let first = background::cells(d)[0];
        let x = first.x + cell.0 * crate::ui::DESKTOP_CELL_WIDTH + first.w / 2;
        let y = first.y + cell.1 * crate::ui::DESKTOP_CELL_HEIGHT + first.h / 2;
        (f64::from(x), f64::from(y))
    }

    #[test]
    fn a_box_dragged_from_an_empty_spot_selects_the_icons_it_touches() {
        let mut d = desktop(vec![
            item("a.txt", false),
            item("b.txt", false),
            item("c.txt", false),
        ]);
        let (x, y) = center(&d, (3, 1));
        d.handle(Event::PointerButton {
            surface: SurfaceId::Background(1),
            x,
            y,
            button: Button::Left,
            pressed: true,
            time: 1000,
            modifiers: NONE,
        });
        let first = background::cells(&d)[0];
        motion(&mut d, f64::from(first.x + 10), f64::from(first.y + 10));
        assert_eq!(selection(&d), [0, 1], "the box spans the first two rows");
        d.handle(Event::PointerButton {
            surface: SurfaceId::Background(1),
            x: f64::from(first.x + 10),
            y: f64::from(first.y + 10),
            button: Button::Left,
            pressed: false,
            time: 1100,
            modifiers: NONE,
        });
        assert!(d.desktop_band.is_none());
        assert_eq!(
            selection(&d),
            [0, 1],
            "the selection stays once the box is let go"
        );
    }

    #[test]
    fn selected_icons_drag_as_one_drag_of_their_files_and_drop_together() {
        let mut d = desktop(vec![
            item("a.txt", false),
            item("b.txt", false),
            item("c.txt", false),
        ]);
        click(&mut d, 0, Button::Left, NONE, 1000);
        click(&mut d, 1, Button::Left, CTRL, 2000);
        let (x, y) = center(&d, (0, 0));
        d.handle(Event::PointerButton {
            surface: SurfaceId::Background(1),
            x,
            y,
            button: Button::Left,
            pressed: true,
            time: 5000,
            modifiers: NONE,
        });
        motion(&mut d, x + 40.0, y);
        let uris: Vec<String> = d
            .requests
            .iter()
            .find_map(|r| match r {
                crate::event::Request::StartDrag { uris, .. } => Some(uris.clone()),
                _ => None,
            })
            .expect("the drag started");
        assert_eq!(uris.len(), 2, "the drag carries both selected files");
        let (tx, ty) = center(&d, (2, 0));
        d.handle(Event::DragMotion {
            surface: SurfaceId::Background(1),
            x: tx,
            y: ty,
        });
        d.handle(Event::DragDrop {
            surface: SurfaceId::Background(1),
            x: tx,
            y: ty,
        });
        d.handle(Event::DragEnd);
        assert_eq!(background::item_cells(&d), [(2, 0), (2, 1), (0, 2)]);
        assert!(d.popup == Popup::None, "a move on the desktop asks nothing");
    }

    #[test]
    fn files_dropped_on_a_folder_icon_ask_whether_to_copy_or_move() {
        let mut folder = item("Projects", false);
        folder.item.is_dir = true;
        let mut d = desktop(vec![folder, item("b.txt", false)]);
        d.files.drag = Some((7, vec![PathBuf::from("/nonexistent/elsewhere/report.txt")]));
        let (x, y) = center(&d, (0, 0));
        d.handle(Event::DragDrop {
            surface: SurfaceId::Background(1),
            x,
            y,
        });
        assert!(matches!(d.popup, Popup::DesktopMenu(..)));
        let actions: Vec<&Action> = d
            .desktop_menu
            .iter()
            .filter_map(|e| match e {
                Entry::Item { action, .. } => Some(action),
                Entry::Separator => None,
            })
            .collect();
        assert_eq!(
            actions,
            [&Action::Drop(false), &Action::Drop(true), &Action::CancelDrop]
        );
        let dest = d.desktop_drop.as_ref().map(|(drop, _)| drop.dest.clone());
        assert_eq!(dest, Some(PathBuf::from("/nonexistent/Desktop/Projects")));
    }

    #[test]
    fn control_toggles_and_shift_extends_the_desktop_selection() {
        let mut d = desktop(vec![
            item("a.txt", false),
            item("b.txt", false),
            item("c.txt", false),
        ]);
        click(&mut d, 0, Button::Left, NONE, 1000);
        assert_eq!(selection(&d), [0]);
        click(&mut d, 2, Button::Left, CTRL, 2000);
        assert_eq!(selection(&d), [0, 2]);
        click(&mut d, 0, Button::Left, CTRL, 3000);
        assert_eq!(selection(&d), [2]);
        click(&mut d, 0, Button::Left, NONE, 4000);
        click(&mut d, 2, Button::Left, SHIFT, 5000);
        assert_eq!(
            selection(&d),
            [0, 1, 2],
            "Shift takes the icons between in the grid"
        );
    }

    #[test]
    fn a_plain_click_on_one_of_several_selected_icons_selects_it_alone_on_release() {
        let mut d = desktop(vec![item("a.txt", false), item("b.txt", false)]);
        click(&mut d, 0, Button::Left, NONE, 1000);
        click(&mut d, 1, Button::Left, CTRL, 2000);
        assert_eq!(selection(&d), [0, 1]);
        click(&mut d, 1, Button::Left, NONE, 4000);
        assert_eq!(selection(&d), [1]);
    }

    #[test]
    fn a_right_click_selects_the_icon_and_rename_is_grayed_on_a_launcher_or_several() {
        let mut d = desktop(vec![item("app.desktop", true), item("notes.txt", false)]);
        click(&mut d, 1, Button::Right, NONE, 1000);
        assert_eq!(selection(&d), [1]);
        assert!(matches!(d.popup, Popup::DesktopMenu(..)));
        assert!(rename_enabled(&d), "a file is renamed");
        click(&mut d, 0, Button::Right, NONE, 2000);
        assert_eq!(
            selection(&d),
            [0],
            "a right click on an unselected icon selects it alone"
        );
        assert!(!rename_enabled(&d), "a launcher's name is its application's");
        d.close_popup();
        click(&mut d, 1, Button::Left, CTRL, 3000);
        click(&mut d, 1, Button::Right, NONE, 4000);
        assert_eq!(
            selection(&d),
            [0, 1],
            "a right click on a selected icon keeps the selection"
        );
        assert!(!rename_enabled(&d), "several items are not renamed at once");
        assert!(d.desktop_menu.iter().any(|e| matches!(
            e,
            Entry::Item {
                action: Action::Delete,
                enabled: true,
                ..
            }
        )));
    }
}
