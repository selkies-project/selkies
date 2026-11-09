/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The New Folder dialog the desktop's menu opens: a small toplevel of the shell's own asking
//! for the folder's name, with Create and Cancel.
//!
//! It is a window rather than a field drawn on the wallpaper because the wallpaper never holds
//! the keyboard on either backend, while a toplevel takes it the way any application does; the
//! window manager places it, frames it, and gives it focus. The name starts as the next free
//! "New folder" name, Enter or Create makes the folder in the Desktop directory and gives it the
//! cell the desktop was right-clicked on, and a name that is taken or that the file system
//! refuses is reported under the field without closing the dialog.

use std::path::PathBuf;

use crate::desktop::{Desktop, Geometry, Hover};
use crate::desktop_folder::Cell;
use crate::entries;
use crate::event::{Button, Request};
use crate::files;
use crate::geom::Rect;
use crate::i18n::{tr, trf};
use crate::render::{Align, Canvas};
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
    Create,
    Cancel,
}

/// The dialog's state while it is open.
#[derive(Default)]
pub struct DialogState {
    /// The window's number, while the dialog is open.
    pub id: Option<u32>,
    pub geometry: Geometry,
    pub pix: Option<Pixmap>,
    pub dirty: bool,
    /// The folder the new folder is made in.
    pub dir: PathBuf,
    /// The desktop cell the folder is placed on, where one was chosen.
    pub cell: Option<Cell>,
    /// The name typed.
    pub text: String,
    /// Whether the name is still the one suggested, drawn selected and replaced by the first
    /// key typed, the way a dialog's preselected field behaves.
    pub suggested: bool,
    /// Why the last attempt failed.
    pub error: Option<String>,
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
        Part::Create => Rect::new(right - BUTTON_W, y, BUTTON_W, CONTROL),
        Part::Cancel => Rect::new(right - 2 * BUTTON_W - 8, y, BUTTON_W, CONTROL),
    }
}

/// Opens the dialog for a new folder in the Desktop directory, to be placed on a cell.
///
/// # Arguments
/// * `d` - The desktop.
/// * `cell` - The cell the folder takes, where one was chosen.
pub fn open_new_folder(d: &mut Desktop, cell: Option<Cell>) {
    if let Some(id) = d.dialog.id.take() {
        d.requests.push(Request::CloseWindow { id });
    }
    let dir = entries::desktop_dir();
    let text = files::ops::file_name(&files::ops::unique_name(&dir, tr("New folder"), false));
    let id = d.files.allocate_id();
    d.dialog = DialogState {
        id: Some(id),
        dir,
        cell,
        text,
        suggested: true,
        dirty: true,
        ..DialogState::default()
    };
    let icon = d
        .icons
        .get("folder-new", 48)
        .map(|icon| Box::new((*icon).clone()));
    d.requests.push(Request::OpenWindow {
        id,
        title: tr("New folder").to_string(),
        width: WIDTH,
        height: HEIGHT,
        app_id: entries::FILES_APP_ID,
        icon_name: "folder-new",
        icon,
    });
}

/// Closes the dialog.
pub fn close(d: &mut Desktop) {
    if let Some(id) = d.dialog.id.take() {
        d.requests.push(Request::CloseWindow { id });
    }
    d.dialog.pix = None;
    if matches!(d.hover, Hover::Dialog(_)) {
        d.set_hover(Hover::None);
    }
}

/// Makes the folder and closes the dialog, or reports why it cannot.
fn create(d: &mut Desktop) {
    let name = d.dialog.text.trim().to_string();
    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        d.dialog.error = Some(tr("Type a name for the folder").to_string());
        d.dialog.dirty = true;
        return;
    }
    let path = d.dialog.dir.join(&name);
    if path.exists() {
        d.dialog.error = Some(tr("A file or folder with this name already exists").to_string());
        d.dialog.dirty = true;
        return;
    }
    if let Err(error) = std::fs::create_dir(&path) {
        d.dialog.error = Some(trf(
            "Cannot create the folder: {error}",
            &[("error", &error.to_string())],
        ));
        d.dialog.dirty = true;
        return;
    }
    let cell = d.dialog.cell;
    close(d);
    d.place_new_desktop_item(&name, cell);
}

/// The part of the dialog under a point.
pub fn hit(d: &Desktop, x: f64, y: f64) -> Option<Part> {
    [Part::Create, Part::Cancel]
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

/// Handles a button: Create makes the folder, Cancel closes the dialog.
pub fn pointer_button(d: &mut Desktop, x: f64, y: f64, button: Button, pressed: bool) {
    if pressed || button != Button::Left {
        return;
    }
    match hit(d, x, y) {
        Some(Part::Create) => create(d),
        Some(Part::Cancel) => close(d),
        None => {}
    }
}

/// Handles a key: typing edits the name, Enter creates, Escape cancels.
pub fn key(d: &mut Desktop, key: Key) {
    match key {
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
        Key::Enter => {
            create(d);
            return;
        }
        Key::Escape => {
            close(d);
            return;
        }
        _ => return,
    }
    d.dialog.suggested = false;
    d.dialog.error = None;
    d.dialog.dirty = true;
}

/// Draws the dialog: the prompt, the field with its caret, the error, and the buttons.
pub fn draw(d: &mut Desktop, canvas: &mut Canvas<'_>) {
    let geometry = d.dialog.geometry;
    let whole = Rect::new(0, 0, geometry.width, geometry.height);
    canvas.fill_rect(whole, ui::popup_bg());
    ui::label(
        canvas,
        &mut d.fonts,
        tr("Folder name"),
        Rect::new(PAD, PAD, geometry.width - 2 * PAD, 20),
        ui::FONT_SIZE,
        true,
        Align::Left,
        ui::text(),
    );
    let field = field_rect(geometry);
    canvas.fill_rounded(field, 7.0, ui::field_bg());
    canvas.stroke_rounded(field, 7.0, 3.0, ui::accent());
    let text = d.dialog.text.clone();
    let text_rect = Rect::new(field.x + 10, field.y, field.w - 20, field.h);
    let text_w =
        ui::text_width(&mut d.fonts, &text, ui::FONT_SIZE, false, geometry.scale).min(text_rect.w - 2);
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
    for (part, label) in [(Part::Cancel, tr("Cancel")), (Part::Create, tr("Create"))] {
        let rect = button_rect(geometry, part);
        let hovered = d.hover == Hover::Dialog(part);
        if part == Part::Create {
            canvas.fill_rounded(rect, 7.0, ui::accent());
            if hovered {
                canvas.fill_rounded(rect, 7.0, ui::text_alpha(0x22));
            }
        } else {
            canvas.fill_rounded(rect, 7.0, ui::text_alpha(if hovered { 0x2a } else { 0x16 }));
        }
        ui::label(
            canvas,
            &mut d.fonts,
            label,
            rect,
            ui::FONT_SIZE,
            part == Part::Create,
            Align::Center,
            if part == Part::Create {
                ui::on_accent()
            } else {
                ui::text()
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The buttons sit along the bottom right inside the window, Cancel left of Create, and
    /// the field spans the width between the margins.
    #[test]
    fn the_dialog_lays_its_parts_inside_the_window() {
        let geometry = Geometry {
            width: WIDTH,
            height: HEIGHT,
            scale: 1,
            configured: true,
        };
        let create = button_rect(geometry, Part::Create);
        let cancel = button_rect(geometry, Part::Cancel);
        assert_eq!(create.right(), WIDTH - PAD);
        assert_eq!(create.bottom(), HEIGHT - PAD);
        assert!(cancel.right() < create.x);
        let field = field_rect(geometry);
        assert_eq!((field.x, field.right()), (PAD, WIDTH - PAD));
        assert!(field.bottom() < create.y);
    }
}
