/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The desktop's dialogs: a small toplevel of the shell's own that asks for a new folder's
//! name, for an item's new name, or whether to delete the selected items, with the action's
//! button and Cancel.
//!
//! It is a window rather than a field drawn on the wallpaper because the wallpaper never holds
//! the keyboard on either backend, while a toplevel takes it the way any application does; the
//! window manager places it, frames it, and gives it focus. A name field starts on a suggestion
//! drawn selected, which the first key typed replaces and an arrow, Home, or End keeps for
//! editing at its end: the next free "New folder" name, which Enter or Create makes in the
//! Desktop directory on the cell the desktop was right-clicked on, or the item's current name,
//! which Enter or Rename gives the item, keeping its cell. A name that is taken or that the file
//! system refuses is reported under the field without closing the dialog. The deletion asks
//! once for every selected item and deletes them as the file manager does, in the background.

use std::path::PathBuf;

use crate::desktop::{Desktop, Geometry, Hover};
use crate::desktop_folder::Cell;
use crate::entries;
use crate::event::{Button, Request};
use crate::files;
use crate::geom::Rect;
use crate::i18n::{tr, trf};
use crate::render::{Align, Canvas, rgba};
use crate::ui;
use crate::xkb::Key;
use tiny_skia::Pixmap;

/// Width of the dialog.
pub const WIDTH: i32 = 420;
/// Height of the dialog.
pub const HEIGHT: i32 = 170;
/// Inner padding.
const PAD: i32 = 18;
/// Height of the name field and the buttons.
const CONTROL: i32 = 32;
/// Width of a button.
const BUTTON_W: i32 = 96;

/// A part of the dialog the pointer can rest on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    /// The button that carries the dialog's action out: Create, Rename, or Delete.
    Confirm,
    Cancel,
}

/// What a dialog asks for.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Kind {
    /// A new folder in the Desktop directory, placed on a cell where one was chosen.
    NewFolder { cell: Option<Cell> },
    /// A new name for a desktop item, which keeps the cell it stands on.
    Rename { from: PathBuf, cell: Option<Cell> },
    /// Whether to delete the selected items.
    Delete { paths: Vec<PathBuf> },
    /// Nothing is open.
    #[default]
    Closed,
}

/// The dialog's state while it is open.
#[derive(Default)]
pub struct DialogState {
    /// The window's number, while the dialog is open.
    pub id: Option<u32>,
    pub geometry: Geometry,
    pub pix: Option<Pixmap>,
    pub dirty: bool,
    /// What the dialog asks for.
    pub kind: Kind,
    /// The folder the name is given in.
    pub dir: PathBuf,
    /// The name typed.
    pub text: String,
    /// Whether the name is still the one suggested, drawn selected and replaced by the first
    /// key typed, the way a dialog's preselected field behaves.
    pub suggested: bool,
    /// Why the last attempt failed.
    pub error: Option<String>,
}

impl DialogState {
    /// Whether the dialog asks for a name.
    fn has_field(&self) -> bool {
        matches!(self.kind, Kind::NewFolder { .. } | Kind::Rename { .. })
    }
}

/// The name field, for a window size.
fn field_rect(geometry: Geometry) -> Rect {
    Rect::new(PAD, PAD + 26, geometry.width - 2 * PAD, CONTROL)
}

/// A button's rectangle, along the bottom right.
fn button_rect(geometry: Geometry, part: Part) -> Rect {
    let y = geometry.height - PAD - CONTROL;
    let right = geometry.width - PAD;
    match part {
        Part::Confirm => Rect::new(right - BUTTON_W, y, BUTTON_W, CONTROL),
        Part::Cancel => Rect::new(right - 2 * BUTTON_W - 8, y, BUTTON_W, CONTROL),
    }
}

/// Opens a dialog, closing the one open.
///
/// # Arguments
/// * `d` - The desktop.
/// * `kind` - What it asks for.
/// * `text` - The name it suggests, empty where it asks for none.
/// * `title` - The window's title.
/// * `icon_name` - The window's icon.
fn open(d: &mut Desktop, kind: Kind, text: String, title: &str, icon_name: &'static str) {
    if let Some(id) = d.dialog.id.take() {
        d.requests.push(Request::CloseWindow { id });
    }
    let id = d.files.allocate_id();
    d.dialog = DialogState {
        id: Some(id),
        kind,
        dir: entries::desktop_dir(),
        suggested: !text.is_empty(),
        text,
        dirty: true,
        ..DialogState::default()
    };
    let icon = d.icons.get(icon_name, 48).map(|icon| Box::new((*icon).clone()));
    d.requests.push(Request::OpenWindow {
        id,
        title: title.to_string(),
        width: WIDTH,
        height: HEIGHT,
        app_id: entries::FILES_APP_ID,
        icon_name,
        icon,
    });
}

/// Opens the dialog for a new folder in the Desktop directory, to be placed on a cell.
///
/// # Arguments
/// * `d` - The desktop.
/// * `cell` - The cell the folder takes, where one was chosen.
pub fn open_new_folder(d: &mut Desktop, cell: Option<Cell>) {
    let dir = entries::desktop_dir();
    let text = files::ops::file_name(&files::ops::unique_name(&dir, tr("New folder"), false));
    open(d, Kind::NewFolder { cell }, text, tr("New folder"), "folder-new");
}

/// Opens the dialog for a desktop item's new name.
///
/// # Arguments
/// * `d` - The desktop.
/// * `from` - The item's file.
/// * `cell` - The cell the item stands on, which it keeps.
pub fn open_rename(d: &mut Desktop, from: PathBuf, cell: Option<Cell>) {
    let text = files::ops::file_name(&from);
    open(d, Kind::Rename { from, cell }, text, tr("Rename"), "edit-rename");
}

/// Opens the dialog asking whether to delete desktop items.
///
/// # Arguments
/// * `d` - The desktop.
/// * `paths` - The items' files.
pub fn open_delete(d: &mut Desktop, paths: Vec<PathBuf>) {
    if paths.is_empty() {
        return;
    }
    open(
        d,
        Kind::Delete { paths },
        String::new(),
        tr("Delete"),
        "edit-delete",
    );
}

/// Closes the dialog.
pub fn close(d: &mut Desktop) {
    if let Some(id) = d.dialog.id.take() {
        d.requests.push(Request::CloseWindow { id });
    }
    d.dialog.pix = None;
    d.dialog.kind = Kind::Closed;
    if matches!(d.hover, Hover::Dialog(_)) {
        d.set_hover(Hover::None);
    }
}

/// Reports why the dialog's action failed, under its field.
fn fail(d: &mut Desktop, error: String) {
    d.dialog.error = Some(error);
    d.dialog.dirty = true;
}

/// The name typed, or `None` after reporting why it cannot be one.
fn typed_name(d: &mut Desktop) -> Option<String> {
    let name = d.dialog.text.trim().to_string();
    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        let error = match d.dialog.kind {
            Kind::NewFolder { .. } => tr("Type a name for the folder"),
            _ => tr("Type a name"),
        };
        fail(d, error.to_string());
        return None;
    }
    Some(name)
}

/// Carries the dialog's action out and closes it, or reports why it cannot.
fn confirm(d: &mut Desktop) {
    match d.dialog.kind.clone() {
        Kind::NewFolder { cell } => {
            let Some(name) = typed_name(d) else {
                return;
            };
            let path = d.dialog.dir.join(&name);
            if path.exists() {
                fail(
                    d,
                    tr("A file or folder with this name already exists").to_string(),
                );
                return;
            }
            if let Err(error) = std::fs::create_dir(&path) {
                fail(
                    d,
                    trf(
                        "Cannot create the folder: {error}",
                        &[("error", &error.to_string())],
                    ),
                );
                return;
            }
            close(d);
            d.place_new_desktop_item(&name, cell);
        }
        Kind::Rename { from, cell } => {
            let Some(name) = typed_name(d) else {
                return;
            };
            if from.file_name().is_some_and(|f| f.to_string_lossy() == name) {
                close(d);
                return;
            }
            let target = d.dialog.dir.join(&name);
            if target.exists() {
                fail(
                    d,
                    tr("A file or folder with this name already exists").to_string(),
                );
                return;
            }
            if let Err(error) = std::fs::rename(&from, &target) {
                fail(d, trf("Cannot rename: {error}", &[("error", &error.to_string())]));
                return;
            }
            close(d);
            d.place_new_desktop_item(&name, cell);
        }
        Kind::Delete { paths } => {
            close(d);
            d.delete_desktop_items(paths);
        }
        Kind::Closed => close(d),
    }
}

/// The part of the dialog under a point.
pub fn hit(d: &Desktop, x: f64, y: f64) -> Option<Part> {
    [Part::Confirm, Part::Cancel]
        .into_iter()
        .find(|part| button_rect(d.dialog.geometry, *part).contains(x, y))
}

/// Follows the pointer for the buttons' hover.
pub fn pointer_motion(d: &mut Desktop, x: f64, y: f64) {
    let hover = hit(d, x, y).map(Hover::Dialog).unwrap_or(Hover::None);
    if hover != d.hover {
        d.set_hover(hover);
        d.dialog.dirty = true;
    }
}

/// Clears the hover when the pointer leaves.
pub fn pointer_leave(d: &mut Desktop) {
    if matches!(d.hover, Hover::Dialog(_)) {
        d.set_hover(Hover::None);
        d.dialog.dirty = true;
    }
}

/// Handles a button: the action's button carries it out, Cancel closes the dialog.
pub fn pointer_button(d: &mut Desktop, x: f64, y: f64, button: Button, pressed: bool) {
    if pressed || button != Button::Left {
        return;
    }
    match hit(d, x, y) {
        Some(Part::Confirm) => confirm(d),
        Some(Part::Cancel) => close(d),
        None => {}
    }
}

/// Handles a key: typing edits the name, Enter carries the action out, Escape cancels.
pub fn key(d: &mut Desktop, key: Key) {
    match key {
        Key::Enter => {
            confirm(d);
            return;
        }
        Key::Escape => {
            close(d);
            return;
        }
        _ if !d.dialog.has_field() => return,
        Key::Char(c) if c != '/' => {
            if d.dialog.suggested {
                d.dialog.text.clear();
            }
            d.dialog.text.push(c);
        }
        Key::Backspace if d.dialog.suggested => d.dialog.text.clear(),
        Key::Backspace => {
            d.dialog.text.pop();
        }
        Key::Left | Key::Right | Key::Home | Key::End => {}
        _ => return,
    }
    d.dialog.suggested = false;
    d.dialog.error = None;
    d.dialog.dirty = true;
}

/// Draws the dialog: the prompt, the field with its caret or the deletion's question, the
/// error, and the buttons.
pub fn draw(d: &mut Desktop, canvas: &mut Canvas<'_>) {
    let geometry = d.dialog.geometry;
    let whole = Rect::new(0, 0, geometry.width, geometry.height);
    canvas.fill_rect(whole, ui::popup_bg());
    let (prompt, action) = match &d.dialog.kind {
        Kind::NewFolder { .. } => (tr("Folder name").to_string(), tr("Create")),
        Kind::Rename { .. } => (tr("Name").to_string(), tr("Rename")),
        Kind::Delete { paths } if paths.len() == 1 => (
            trf("Delete “{name}”?", &[("name", &files::ops::file_name(&paths[0]))]),
            tr("Delete"),
        ),
        Kind::Delete { paths } => (
            trf("Delete {n} items?", &[("n", &paths.len().to_string())]),
            tr("Delete"),
        ),
        Kind::Closed => return,
    };
    ui::label(
        canvas,
        &mut d.fonts,
        &prompt,
        Rect::new(PAD, PAD, geometry.width - 2 * PAD, 20),
        ui::FONT_SIZE,
        true,
        Align::Left,
        ui::text(),
    );
    let field = field_rect(geometry);
    if d.dialog.has_field() {
        draw_field(d, canvas, field);
    } else {
        ui::label(
            canvas,
            &mut d.fonts,
            tr("This cannot be undone."),
            Rect::new(PAD, field.y, field.w, 22),
            ui::FONT_SMALL,
            false,
            Align::Left,
            ui::text_dim(),
        );
    }
    if let Some(error) = d.dialog.error.clone() {
        ui::label(
            canvas,
            &mut d.fonts,
            &error,
            Rect::new(PAD, field.bottom() + 6, geometry.width - 2 * PAD, 20),
            ui::FONT_SMALL,
            false,
            Align::Left,
            ui::text(),
        );
    }
    let destructive = matches!(d.dialog.kind, Kind::Delete { .. });
    for (part, label) in [(Part::Cancel, tr("Cancel")), (Part::Confirm, action)] {
        let rect = button_rect(geometry, part);
        let hovered = d.hover == Hover::Dialog(part);
        match part {
            Part::Confirm if destructive => canvas.fill_rounded(
                rect,
                7.0,
                if hovered {
                    rgba(0xe0, 0x50, 0x50, 0xff)
                } else {
                    rgba(0xc8, 0x3c, 0x3c, 0xff)
                },
            ),
            Part::Confirm => {
                canvas.fill_rounded(rect, 7.0, ui::accent());
                if hovered {
                    canvas.fill_rounded(rect, 7.0, ui::text_alpha(0x22));
                }
            }
            Part::Cancel => canvas.fill_rounded(rect, 7.0, ui::text_alpha(if hovered { 0x2a } else { 0x16 })),
        }
        let color = match part {
            Part::Confirm if destructive => rgba(0xff, 0xff, 0xff, 0xff),
            Part::Confirm => ui::on_accent(),
            Part::Cancel => ui::text(),
        };
        ui::label(
            canvas,
            &mut d.fonts,
            label,
            rect,
            ui::FONT_SIZE,
            part == Part::Confirm,
            Align::Center,
            color,
        );
    }
}

/// Draws the name field: the text, selected while it is the suggestion, and the caret.
fn draw_field(d: &mut Desktop, canvas: &mut Canvas<'_>, field: Rect) {
    let scale = d.dialog.geometry.scale;
    canvas.fill_rounded(field, 7.0, ui::field_bg());
    canvas.stroke_rounded(field, 7.0, 3.0, ui::accent());
    let text = d.dialog.text.clone();
    let text_rect = Rect::new(field.x + 10, field.y, field.w - 20, field.h);
    let text_w = ui::text_width(&mut d.fonts, &text, ui::FONT_SIZE, false, scale).min(text_rect.w - 2);
    if d.dialog.suggested && !text.is_empty() {
        canvas.fill_rounded(
            Rect::new(text_rect.x - 2, field.y + 6, text_w + 4, field.h - 12),
            3.0,
            ui::accent_alpha(0x90),
        );
    }
    ui::label(
        canvas,
        &mut d.fonts,
        &text,
        text_rect,
        ui::FONT_SIZE,
        false,
        Align::Left,
        ui::text(),
    );
    canvas.fill_rect(
        Rect::new(text_rect.x + text_w + 1, field.y + 8, 1, field.h - 16),
        ui::text(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The buttons sit along the bottom right inside the window, Cancel left of the action, and
    /// the field spans the width between the margins.
    #[test]
    fn the_dialog_lays_its_parts_inside_the_window() {
        let geometry = Geometry {
            width: WIDTH,
            height: HEIGHT,
            scale: 1,
            configured: true,
        };
        let create = button_rect(geometry, Part::Confirm);
        let cancel = button_rect(geometry, Part::Cancel);
        assert_eq!(create.right(), WIDTH - PAD);
        assert_eq!(create.bottom(), HEIGHT - PAD);
        assert!(cancel.right() < create.x);
        let field = field_rect(geometry);
        assert_eq!((field.x, field.right()), (PAD, WIDTH - PAD));
        assert!(field.bottom() < create.y);
    }
}
