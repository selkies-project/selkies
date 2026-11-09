//! The file manager window: a toolbar with history buttons and a breadcrumb
//! path, a sidebar of places, the folder as an icon grid or a list, a status
//! bar, and the context menu, rename field and confirmation that open inside
//! it.  Every window is a toplevel the window manager places and decorates.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use tiny_skia::{Color, PathBuilder, Pixmap};

use crate::desktop::{Desktop, Hover};
use crate::entries;
use crate::event::{Button, Modifiers, Request};
use crate::files::{
    self, Clipboard, Confirm, ContextMenu, DropTarget, FilesWindow, Item, MenuAction, MenuRow, PickTarget,
    Press, Rename, ScrollArea, SortKey, View,
};
use crate::geom::Rect;
use crate::i18n::{tr, trf};
use crate::render::{Align, Canvas, Fonts, rgba};
use crate::ui::{self, settings};
use crate::xkb::Key;

/// Height of the toolbar.
const TOOLBAR_H: i32 = 46;
/// Width of the sidebar.
const SIDEBAR_W: i32 = 190;
/// Height of the status bar.
const STATUS_H: i32 = 28;
/// Height of the picker footer.
const FOOTER_H: i32 = 50;
/// Height of the list header.
const HEADER_H: i32 = 28;
/// Height of a list row.
const LIST_ROW: i32 = 30;
/// Height of a sidebar row.
const PLACE_ROW: i32 = 32;
/// Width of an icon cell.
const CELL_W: i32 = 112;
/// Height of an icon cell.
const CELL_H: i32 = 108;
/// Icon size in the icon view.
const GRID_ICON: i32 = 48;
/// Side of a toolbar button.
const TOOL: i32 = 32;
/// Inner padding.
const PAD: i32 = 8;
/// Width of the context menu.
const MENU_W: i32 = 240;
/// Height of a context menu row.
const MENU_ROW: i32 = 28;
/// Height of a context menu separator.
const MENU_SEP: i32 = 9;
/// Pixels the pointer moves with the button held before a drag starts.
const DRAG_THRESHOLD: f64 = 6.0;
/// Width of the size column.
const SIZE_COL: i32 = 90;
/// Width of the modified column.
const DATE_COL: i32 = 140;
/// Width of the type column.
const KIND_COL: i32 = 170;
/// Most applications offered in the context menu.
const OPEN_WITH_MAX: usize = 5;
/// Side of the drag ghost in logical pixels.
const GHOST: i32 = 64;
/// Space between breadcrumbs, where the chevron sits.
const CRUMB_GAP: i32 = 16;

/// What the pointer is over in a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    /// Nothing interactive.
    None,
    /// The back button.
    Back,
    /// The forward button.
    Forward,
    /// The up button.
    Up,
    /// A breadcrumb, by component index.
    Crumb(usize),
    /// Empty space of the path bar, which opens the location field.
    PathField,
    /// The view toggle.
    ViewToggle,
    /// The sort button.
    SortButton,
    /// A sidebar place.
    Place(usize),
    /// An item of the listing.
    Item(usize),
    /// A list column header.
    Header(usize),
    /// Empty content area.
    Content,
    /// A context menu row.
    MenuRow(usize),
    /// A confirmation button: cancel, then delete.
    Confirm(usize),
    /// A picker footer button: cancel, then open.
    PickerButton(usize),
    /// The scrollbar of an area.
    Scrollbar(ScrollArea),
}

impl Hit {
    /// Whether the hand cursor suits the target.
    pub fn clickable(self) -> bool {
        !matches!(
            self,
            Hit::None | Hit::Item(_) | Hit::Content | Hit::PathField | Hit::Scrollbar(_)
        )
    }
}

/// The rectangles of a window.
struct Layout {
    frame: Rect,
    back: Rect,
    forward: Rect,
    up: Rect,
    path: Rect,
    view_toggle: Rect,
    sort: Rect,
    sidebar: Rect,
    header: Rect,
    content: Rect,
    status: Rect,
    footer: Option<Rect>,
}

/// The columns of the list view, as (key, rect) pairs.
fn columns(content: Rect) -> Vec<(SortKey, Rect)> {
    let mut x = content.right() - PAD;
    let mut cols = Vec::new();
    for (key, w) in [
        (SortKey::Kind, KIND_COL),
        (SortKey::Modified, DATE_COL),
        (SortKey::Size, SIZE_COL),
    ] {
        if content.w > 520 || key == SortKey::Size {
            x -= w;
            cols.push((key, Rect::new(x, content.y, w, HEADER_H)));
        }
    }
    cols.push((
        SortKey::Name,
        Rect::new(content.x + PAD, content.y, x - content.x - PAD, HEADER_H),
    ));
    cols.reverse();
    cols
}

/// Lays a window out for its geometry.
fn layout(w: &FilesWindow) -> Layout {
    let frame = Rect::new(0, 0, w.geometry.width, w.geometry.height);
    let ty = (TOOLBAR_H - TOOL) / 2;
    let back = Rect::new(PAD, ty, TOOL, TOOL);
    let forward = Rect::new(back.right() + 2, ty, TOOL, TOOL);
    let up = Rect::new(forward.right() + 2, ty, TOOL, TOOL);
    let sort = Rect::new(frame.right() - PAD - TOOL, ty, TOOL, TOOL);
    let view_toggle = Rect::new(sort.x - 2 - TOOL, ty, TOOL, TOOL);
    let path = Rect::new(up.right() + PAD, ty, view_toggle.x - PAD - up.right() - PAD, TOOL);
    let footer_h = if w.picker.is_some() { FOOTER_H } else { 0 };
    let status = Rect::new(0, frame.h - footer_h - STATUS_H, frame.w, STATUS_H);
    let footer = (footer_h > 0).then(|| Rect::new(0, frame.h - footer_h, frame.w, footer_h));
    let body_top = TOOLBAR_H;
    let body_h = status.y - body_top;
    let sidebar_w = if frame.w >= 640 { SIDEBAR_W } else { 0 };
    let sidebar = Rect::new(0, body_top, sidebar_w, body_h);
    let header_h = if w.view == View::List { HEADER_H } else { 0 };
    let header = Rect::new(sidebar.right(), body_top, frame.w - sidebar.right(), header_h);
    let content = Rect::new(
        sidebar.right(),
        body_top + header_h,
        frame.w - sidebar.right(),
        body_h - header_h,
    );
    Layout {
        frame,
        back,
        forward,
        up,
        path,
        view_toggle,
        sort,
        sidebar,
        header,
        content,
        status,
        footer,
    }
}

/// The breadcrumb components of a folder: the root, then each name.
fn crumbs(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut list = Vec::new();
    let mut acc = PathBuf::new();
    for component in dir.components() {
        acc.push(component);
        let name = match component {
            std::path::Component::RootDir => "/".to_string(),
            _ => files::display_name(&acc),
        };
        list.push((name, acc.clone()));
    }
    list
}

/// The chip rectangles of the breadcrumbs that fit in the path bar, right
/// aligned so the current folder always shows, with the index of each.
fn crumb_rects(fonts: &mut Fonts, dir: &Path, scale: i32, path: Rect) -> Vec<(usize, Rect)> {
    let crumbs = crumbs(dir);
    let widths: Vec<i32> = crumbs
        .iter()
        .map(|(name, _)| ui::text_width(fonts, name, ui::FONT_SIZE, false, scale) + 18)
        .collect();
    let mut total = 0;
    let mut first = crumbs.len();
    for (i, width) in widths.iter().enumerate().rev() {
        if total + width + CRUMB_GAP > path.w - 8 && i + 1 < crumbs.len() {
            break;
        }
        total += width + CRUMB_GAP;
        first = i;
    }
    let mut rects = Vec::new();
    let mut x = path.x + 4;
    if first > 0 {
        x += 18;
    }
    for (i, width) in widths.iter().enumerate().skip(first) {
        let rect = Rect::new(x, path.y + 3, *width, path.h - 6);
        rects.push((i, rect));
        x += width + CRUMB_GAP;
    }
    rects
}

/// Columns of the icon grid for a content width.
fn grid_columns(content: Rect) -> i32 {
    ((content.w - PAD) / CELL_W).max(1)
}

/// The cell of an item in the icon view, before scrolling.
fn cell_rect(content: Rect, index: usize) -> Rect {
    let cols = grid_columns(content);
    let index = index as i32;
    let left = content.x + PAD;
    Rect::new(
        left + (index % cols) * CELL_W,
        content.y + PAD / 2 + (index / cols) * CELL_H,
        CELL_W,
        CELL_H,
    )
}

/// The row of an item in the list view, before scrolling.
fn row_rect(content: Rect, index: usize) -> Rect {
    Rect::new(
        content.x,
        content.y + index as i32 * LIST_ROW,
        content.w,
        LIST_ROW,
    )
}

/// The rectangle of an item in the current view, before scrolling.
fn item_rect(w: &FilesWindow, content: Rect, index: usize) -> Rect {
    match w.view {
        View::Icons => cell_rect(content, index),
        View::List => row_rect(content, index),
    }
}

/// Height of the whole listing in the current view.
fn content_height(w: &FilesWindow, content: Rect) -> i32 {
    match w.view {
        View::Icons => {
            let cols = grid_columns(content);
            let rows = (w.items.len() as i32 + cols - 1) / cols;
            rows * CELL_H + PAD
        }
        View::List => w.items.len() as i32 * LIST_ROW,
    }
}

/// The scrollbar of an area of a window, when its content scrolls.
///
/// # Arguments
/// * `w` - The window.
/// * `layout` - The window's layout.
/// * `area` - The area.
/// * `places` - How many places the sidebar lists.
fn scrollbar(w: &FilesWindow, layout: &Layout, area: ScrollArea, places: usize) -> Option<ui::Scrollbar> {
    match area {
        ScrollArea::Sidebar if layout.sidebar.w > 0 => {
            let content = places as i32 * PLACE_ROW + 2 * PAD;
            ui::Scrollbar::new(layout.sidebar, content, w.sidebar_scroll)
        }
        ScrollArea::Sidebar => None,
        ScrollArea::Content => {
            ui::Scrollbar::new(layout.content, content_height(w, layout.content), w.scroll)
        }
    }
}

/// Clamps a window's scroll to its content.
fn clamp_scroll(w: &mut FilesWindow, content: Rect) {
    let max = (content_height(w, content) - content.h).max(0) as f64;
    w.scroll = w.scroll.clamp(0.0, max);
}

/// The rectangle of the context menu, kept inside the window.
fn menu_rect(w: &FilesWindow, menu: &ContextMenu) -> Rect {
    let height: i32 = menu
        .rows
        .iter()
        .map(|r| match r {
            MenuRow::Action(..) => MENU_ROW,
            MenuRow::Separator => MENU_SEP,
        })
        .sum::<i32>()
        + 12;
    let frame = Rect::new(0, 0, w.geometry.width, w.geometry.height);
    let x = menu.x.min(frame.w - MENU_W - 4).max(4);
    let y = menu.y.min(frame.h - height - 4).max(4);
    Rect::new(x, y, MENU_W, height)
}

/// The row of the context menu at a point.
fn menu_row_at(w: &FilesWindow, menu: &ContextMenu, x: f64, y: f64) -> Option<usize> {
    let frame = menu_rect(w, menu);
    if !frame.contains(x, y) {
        return None;
    }
    let mut top = frame.y + 6;
    for (i, row) in menu.rows.iter().enumerate() {
        let h = match row {
            MenuRow::Action(..) => MENU_ROW,
            MenuRow::Separator => MENU_SEP,
        };
        if y >= top as f64 && y < (top + h) as f64 {
            return matches!(row, MenuRow::Action(..)).then_some(i);
        }
        top += h;
    }
    None
}

/// The confirmation box and its two buttons.
fn confirm_rects(w: &FilesWindow) -> (Rect, Rect, Rect) {
    let frame = Rect::new(0, 0, w.geometry.width, w.geometry.height);
    let width = 380.min(frame.w - 20);
    let bx = Rect::new((frame.w - width) / 2, (frame.h - 130) / 2, width, 130);
    let delete = Rect::new(bx.right() - 16 - 90, bx.bottom() - 16 - 30, 90, 30);
    let cancel = Rect::new(delete.x - 8 - 90, delete.y, 90, 30);
    (bx, cancel, delete)
}

/// The picker footer's two buttons.
fn footer_buttons(footer: Rect) -> (Rect, Rect) {
    let open = Rect::new(footer.right() - PAD - 96, footer.y + (footer.h - 32) / 2, 96, 32);
    let cancel = Rect::new(open.x - 8 - 96, open.y, 96, 32);
    (cancel, open)
}

/// The hover target under a point.
///
/// # Arguments
/// * `d` - The desktop.
/// * `id` - The window.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
pub fn hit(d: &mut Desktop, id: u32, x: f64, y: f64) -> Hit {
    let Some(index) = d.files.windows.iter().position(|w| w.id == id) else {
        return Hit::None;
    };
    let layout = layout(&d.files.windows[index]);
    if layout.path.contains(x, y) && d.files.windows[index].path_edit.is_none() {
        let dir = d.files.windows[index].dir.clone();
        let scale = d.files.windows[index].geometry.scale;
        let rects = crumb_rects(&mut d.fonts, &dir, scale, layout.path);
        let w = &d.files.windows[index];
        if w.menu.is_none() && w.confirm.is_none() {
            for (i, rect) in rects {
                if rect.contains(x, y) {
                    return Hit::Crumb(i);
                }
            }
            return Hit::PathField;
        }
    }
    let w = &d.files.windows[index];
    if let Some(menu) = &w.menu {
        return match menu_row_at(w, menu, x, y) {
            Some(row) => Hit::MenuRow(row),
            None => Hit::None,
        };
    }
    if w.confirm.is_some() {
        let (_, cancel, delete) = confirm_rects(w);
        if cancel.contains(x, y) {
            return Hit::Confirm(0);
        }
        if delete.contains(x, y) {
            return Hit::Confirm(1);
        }
        return Hit::None;
    }
    if let Some(footer) = layout.footer
        && footer.contains(x, y)
    {
        let (cancel, open) = footer_buttons(footer);
        if cancel.contains(x, y) {
            return Hit::PickerButton(0);
        }
        if open.contains(x, y) {
            return Hit::PickerButton(1);
        }
        return Hit::None;
    }
    if layout.back.contains(x, y) {
        return Hit::Back;
    }
    if layout.forward.contains(x, y) {
        return Hit::Forward;
    }
    if layout.up.contains(x, y) {
        return Hit::Up;
    }
    if layout.view_toggle.contains(x, y) {
        return Hit::ViewToggle;
    }
    if layout.sort.contains(x, y) {
        return Hit::SortButton;
    }
    if layout.path.contains(x, y) {
        return Hit::PathField;
    }
    for area in [ScrollArea::Sidebar, ScrollArea::Content] {
        if scrollbar(w, &layout, area, d.files.places.len()).is_some_and(|bar| bar.track.contains(x, y)) {
            return Hit::Scrollbar(area);
        }
    }
    if layout.sidebar.w > 0 && layout.sidebar.contains(x, y) {
        let row = ((y - layout.sidebar.y as f64 - PAD as f64 + w.sidebar_scroll) / PLACE_ROW as f64).floor();
        if row >= 0.0 && (row as usize) < d.files.places.len() {
            return Hit::Place(row as usize);
        }
        return Hit::None;
    }
    if layout.header.h > 0 && layout.header.contains(x, y) {
        for (i, (_, rect)) in columns(layout.header).iter().enumerate() {
            if rect.contains(x, y) {
                return Hit::Header(i);
            }
        }
        return Hit::None;
    }
    if layout.content.contains(x, y) {
        let sy = y + w.scroll;
        let found = match w.view {
            View::Icons => {
                let cols = grid_columns(layout.content);
                let first = cell_rect(layout.content, 0);
                let col = ((x - first.x as f64) / CELL_W as f64).floor();
                let row = ((sy - first.y as f64) / CELL_H as f64).floor();
                if col >= 0.0 && (col as i32) < cols && row >= 0.0 {
                    let index = row as usize * cols as usize + col as usize;
                    let cell = cell_rect(layout.content, index);
                    (index < w.items.len() && cell.inflate(-4).contains(x, sy)).then_some(index)
                } else {
                    None
                }
            }
            View::List => {
                let row = ((sy - layout.content.y as f64) / LIST_ROW as f64).floor();
                (row >= 0.0 && (row as usize) < w.items.len()).then_some(row as usize)
            }
        };
        return found.map_or(Hit::Content, Hit::Item);
    }
    Hit::None
}

/// Handles pointer motion: hover, and the start of a drag.
///
/// # Arguments
/// * `d` - The desktop.
/// * `id` - The window.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
pub fn pointer_motion(d: &mut Desktop, id: u32, x: f64, y: f64) {
    let places = d.files.places.len();
    if let Some(w) = d.files.window_mut(id)
        && let Some((area, grab)) = w.bar_drag
    {
        let layout = layout(w);
        if let Some(bar) = scrollbar(w, &layout, area, places) {
            let scroll = bar.scroll_at(y, grab);
            match area {
                ScrollArea::Sidebar => w.sidebar_scroll = scroll,
                ScrollArea::Content => w.scroll = scroll,
            }
            w.dirty = true;
        }
        return;
    }
    if let Some(press) = d.files.window(id).and_then(|w| w.press) {
        let moved = (x - press.x).abs() > DRAG_THRESHOLD || (y - press.y).abs() > DRAG_THRESHOLD;
        if moved {
            if let Some(w) = d.files.window_mut(id) {
                w.press = None;
            }
            if let Some(item) = press.item {
                start_drag(d, id, item);
                return;
            }
        }
    }
    let hit = hit(d, id, x, y);
    d.set_hover(Hover::Files(id, hit));
}

/// Starts dragging the selection, or the pressed item when it is not part
/// of it, with a ghost of its icon.
fn start_drag(d: &mut Desktop, id: u32, item: usize) {
    let Some(w) = d.files.window(id) else {
        return;
    };
    if w.picker.is_some() {
        return;
    }
    let paths = if w.selection.contains(&item) {
        w.selected_paths()
    } else {
        w.items
            .get(item)
            .map(|i| vec![i.path.clone()])
            .unwrap_or_default()
    };
    if paths.is_empty() {
        return;
    }
    let scale = w.geometry.scale;
    let first = w.items.get(item).cloned();
    let count = paths.len();
    let icon = first.and_then(|item| ghost(d, id, &item, count));
    d.files.drag = Some((id, paths.clone()));
    let uris = paths.iter().map(|p| files::file_uri(p)).collect();
    d.requests.push(Request::StartDrag {
        window: id,
        uris,
        icon: icon.map(Box::new),
        scale,
    });
}

/// Draws the ghost that follows a drag: the item's icon with a count badge.
fn ghost(d: &mut Desktop, id: u32, item: &Item, count: usize) -> Option<Pixmap> {
    let scale = d.files.window(id)?.geometry.scale;
    let mut pix = Pixmap::new((GHOST * scale) as u32, (GHOST * scale) as u32)?;
    {
        let mut canvas = Canvas::new(pix.as_mut(), scale);
        let icon_rect = Rect::new(
            (GHOST - GRID_ICON) / 2,
            (GHOST - GRID_ICON) / 2,
            GRID_ICON,
            GRID_ICON,
        );
        if let (Some(picture), _) = item_picture(d, item, GRID_ICON, scale) {
            canvas.draw_image_fit(&picture, icon_rect);
        }
        if count > 1 {
            let badge = Rect::new(GHOST - 26, 2, 24, 20);
            canvas.fill_rounded(badge, 10.0, ui::accent());
            ui::label(
                &mut canvas,
                &mut d.fonts,
                &count.to_string(),
                badge,
                ui::FONT_SMALL,
                true,
                Align::Center,
                ui::text(),
            );
        }
    }
    Some(pix)
}

/// Handles a button press or release.
///
/// # Arguments
/// * `d` - The desktop.
/// * `id` - The window.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
/// * `button` - The button.
/// * `pressed` - Whether the button went down.
/// * `time` - The event time, for double clicks.
/// * `modifiers` - The modifier keys held.
#[allow(clippy::too_many_arguments)]
pub fn pointer_button(
    d: &mut Desktop,
    id: u32,
    x: f64,
    y: f64,
    button: Button,
    pressed: bool,
    time: u32,
    modifiers: Modifiers,
) {
    if !pressed {
        if button == Button::Left
            && let Some(w) = d.files.window_mut(id)
            && w.bar_drag.take().is_some()
        {
            update_hover(d, id, x, y);
            return;
        }
        if button == Button::Left
            && let Some(w) = d.files.window_mut(id)
            && let Some(press) = w.press.take()
            && press.was_selected
            && !modifiers.ctrl
            && !modifiers.shift
            && let Some(item) = press.item
            && w.selection.len() > 1
        {
            w.selection.clear();
            w.selection.insert(item);
            w.cursor = Some(item);
            w.dirty = true;
        }
        return;
    }
    let hit = hit(d, id, x, y);
    let places = d.files.places.len();
    let Some(w) = d.files.window_mut(id) else {
        return;
    };
    w.dirty = true;
    if let Some(menu) = w.menu.take() {
        if let Hit::MenuRow(row) = hit
            && button == Button::Left
            && let Some(MenuRow::Action(_, action, true)) = menu.rows.get(row)
        {
            perform(d, id, action.clone());
        }
        update_hover(d, id, x, y);
        return;
    }
    if w.confirm.is_some() {
        if let Hit::Confirm(which) = hit
            && button == Button::Left
        {
            let confirm = w.confirm.take();
            if which == 1
                && let Some(confirm) = confirm
            {
                d.files.delete(id, confirm.paths);
            }
        }
        update_hover(d, id, x, y);
        return;
    }
    if hit != Hit::PathField && w.path_edit.is_some() {
        w.path_edit = None;
    }
    if !matches!(hit, Hit::Item(i) if w.rename.as_ref().is_some_and(|r| r.index == i)) {
        commit_rename(d, id);
    }
    let Some(w) = d.files.window_mut(id) else {
        return;
    };
    match button {
        Button::Left => match hit {
            Hit::Back => d.files.step_history(id, -1),
            Hit::Forward => d.files.step_history(id, 1),
            Hit::Up => go_up(d, id),
            Hit::Crumb(i) => {
                if let Some((_, path)) = crumbs(&w.dir).get(i) {
                    let path = path.clone();
                    d.files.navigate(id, &path);
                }
            }
            Hit::PathField => {
                if w.path_edit.is_none() {
                    w.path_edit = Some(w.dir.to_string_lossy().to_string());
                }
            }
            Hit::ViewToggle => {
                w.view = if w.view == View::Icons {
                    View::List
                } else {
                    View::Icons
                };
                w.scroll = 0.0;
            }
            Hit::SortButton => {
                let rows = sort_rows(w);
                w.menu = Some(ContextMenu {
                    x: x as i32 - MENU_W + 8,
                    y: y as i32,
                    rows,
                });
            }
            Hit::Place(i) => {
                if let Some(place) = d.files.places.get(i) {
                    let path = place.path.clone();
                    d.files.navigate(id, &path);
                }
            }
            Hit::Header(i) => {
                let layout = layout(w);
                if let Some((key, _)) = columns(layout.header).get(i) {
                    if w.sort == *key {
                        w.ascending = !w.ascending;
                    } else {
                        w.sort = *key;
                        w.ascending = true;
                    }
                    w.refilter();
                }
            }
            Hit::Item(i) => {
                let double = w
                    .last_click
                    .is_some_and(|(t, item)| item == i && time.wrapping_sub(t) < ui::DOUBLE_CLICK_MS);
                let was_selected = w.selection.contains(&i);
                if modifiers.ctrl {
                    if was_selected {
                        w.selection.remove(&i);
                    } else {
                        w.selection.insert(i);
                    }
                    w.cursor = Some(i);
                } else if modifiers.shift {
                    let from = w.cursor.unwrap_or(i);
                    let (lo, hi) = (from.min(i), from.max(i));
                    w.selection.extend(lo..=hi);
                } else if !was_selected {
                    w.selection.clear();
                    w.selection.insert(i);
                    w.cursor = Some(i);
                }
                if double && !modifiers.ctrl && !modifiers.shift {
                    w.last_click = None;
                    w.press = None;
                    open_item(d, id, i);
                    return;
                }
                w.last_click = Some((time, i));
                w.press = Some(Press {
                    x,
                    y,
                    item: Some(i),
                    was_selected: was_selected && !modifiers.ctrl && !modifiers.shift,
                });
            }
            Hit::Content => {
                if !modifiers.ctrl && !modifiers.shift {
                    w.selection.clear();
                }
                w.last_click = None;
                w.press = Some(Press {
                    x,
                    y,
                    item: None,
                    was_selected: false,
                });
            }
            Hit::Scrollbar(area) => {
                let layout = layout(w);
                if let Some(bar) = scrollbar(w, &layout, area, places) {
                    let grab = bar.grab(y);
                    let scroll = bar.scroll_at(y, grab);
                    match area {
                        ScrollArea::Sidebar => w.sidebar_scroll = scroll,
                        ScrollArea::Content => w.scroll = scroll,
                    }
                    w.bar_drag = Some((area, grab));
                }
            }
            Hit::PickerButton(0) => close(d, id),
            Hit::PickerButton(_) => {
                if let Some(cursor) = w.cursor.filter(|c| w.selection.contains(c)) {
                    open_item(d, id, cursor);
                }
            }
            Hit::MenuRow(_) | Hit::Confirm(_) | Hit::None => {}
        },
        Button::Right => match hit {
            Hit::Item(i) => {
                if !w.selection.contains(&i) {
                    w.selection.clear();
                    w.selection.insert(i);
                    w.cursor = Some(i);
                }
                let rows = item_rows(d, id);
                if let Some(w) = d.files.window_mut(id) {
                    w.menu = Some(ContextMenu {
                        x: x as i32,
                        y: y as i32,
                        rows,
                    });
                }
            }
            Hit::Content => {
                w.selection.clear();
                let rows = folder_rows(w);
                w.menu = Some(ContextMenu {
                    x: x as i32,
                    y: y as i32,
                    rows,
                });
            }
            _ => {}
        },
        Button::Middle => {
            if let Hit::Item(i) = hit
                && let Some(item) = w.items.get(i)
                && item.is_dir
                && w.picker.is_none()
            {
                let dir = item.path.clone();
                d.open_files(Some(dir), None);
            }
        }
    }
    update_hover(d, id, x, y);
}

/// Recomputes the hover after a click changed the window.
fn update_hover(d: &mut Desktop, id: u32, x: f64, y: f64) {
    let hit = hit(d, id, x, y);
    d.set_hover(Hover::Files(id, hit));
    if let Some(w) = d.files.window_mut(id) {
        w.dirty = true;
    }
}

/// Shows the parent folder.
fn go_up(d: &mut Desktop, id: u32) {
    let Some(parent) = d
        .files
        .window(id)
        .and_then(|w| w.dir.parent().map(Path::to_path_buf))
    else {
        return;
    };
    let from = d.files.window(id).map(|w| w.dir.clone());
    d.files.navigate(id, &parent);
    if let Some(from) = from
        && let Some(w) = d.files.window_mut(id)
        && let Some(index) = w.items.iter().position(|i| i.path == from)
    {
        w.selection.insert(index);
        w.cursor = Some(index);
        scroll_to(w, index);
    }
}

/// Opens an item: a folder is shown, a file is handed to its application, or
/// chosen when the window is a picker.
fn open_item(d: &mut Desktop, id: u32, index: usize) {
    let Some(item) = d.files.window(id).and_then(|w| w.items.get(index).cloned()) else {
        return;
    };
    if item.is_dir {
        d.files.navigate(id, &item.path);
        return;
    }
    let picker = d.files.window(id).and_then(|w| w.picker.clone());
    if let Some(picker) = picker {
        let (key, monitor, page) = match picker.target {
            PickTarget::Wallpaper { key, monitor } => (key, monitor, settings::Page::Wallpaper),
            PickTarget::StartIcon => ("start_icon", None, settings::Page::Panel),
        };
        let value = item.path.to_string_lossy().to_string();
        if let Err(error) = crate::config::save(&[(key, value)]) {
            eprintln!("selkies-desktop: cannot write the config file: {error}");
        }
        d.apply_config();
        close(d, id);
        d.open_settings(Some(page));
        d.settings.monitor = monitor;
        return;
    }
    open_with_default(d, id, std::slice::from_ref(&item.path), &item.mime);
}

/// Launches the default application of a type on files, or `xdg-open`.
fn open_with_default(d: &mut Desktop, id: u32, paths: &[PathBuf], mime: &str) {
    let handlers = d.files.mime.handlers(mime);
    for handler in handlers {
        if let Some(entry) = entries::load_entry(&handler) {
            entries::launch_with(&entry, &d.config, paths);
            return;
        }
    }
    if entries::resolve_program("xdg-open").is_some() {
        for path in paths {
            entries::spawn_detached(
                &["xdg-open".to_string(), path.to_string_lossy().to_string()],
                None,
            );
        }
        return;
    }
    let kind = d.files.mime.comment(mime).unwrap_or_else(|| mime.to_string());
    status(d, id, trf("No application opens {kind}", &[("kind", &kind)]));
}

/// Shows a message in a window's status bar.
fn status(d: &mut Desktop, id: u32, text: String) {
    if let Some(w) = d.files.window_mut(id) {
        w.status = Some((text, Instant::now()));
        w.dirty = true;
    }
}

/// Closes a window.
pub fn close(d: &mut Desktop, id: u32) {
    d.files.close(id);
    d.requests.push(Request::CloseWindow { id });
    if matches!(d.hover, Hover::Files(h, _) if h == id) {
        d.set_hover(Hover::None);
    }
}

/// The context menu rows for the selected items.
fn item_rows(d: &mut Desktop, id: u32) -> Vec<MenuRow> {
    let Some(w) = d.files.window(id) else {
        return Vec::new();
    };
    let selected: Vec<Item> = {
        let mut rows: Vec<usize> = w.selection.iter().copied().collect();
        rows.sort();
        rows.into_iter().filter_map(|i| w.items.get(i).cloned()).collect()
    };
    let editable = w.editable();
    let single_dir = selected.len() == 1 && selected[0].is_dir;
    let one_mime = selected
        .first()
        .filter(|f| selected.iter().all(|s| s.mime == f.mime && !s.is_dir))
        .map(|f| f.mime.clone());
    let mut rows = vec![MenuRow::Action(tr("Open").to_string(), MenuAction::Open, true)];
    if single_dir && editable {
        rows.push(MenuRow::Action(
            tr("Open in new window").to_string(),
            MenuAction::OpenInNewWindow,
            true,
        ));
    }
    if let Some(mime) = one_mime {
        let handlers = d.files.mime.handlers(&mime);
        let mut shown = 0;
        for handler in handlers {
            if shown >= OPEN_WITH_MAX {
                break;
            }
            if let Some(entry) = entries::load_entry(&handler) {
                rows.push(MenuRow::Action(
                    trf("Open with {app}", &[("app", &entry.name)]),
                    MenuAction::OpenWith(handler.clone()),
                    true,
                ));
                shown += 1;
            }
        }
    }
    if editable {
        rows.push(MenuRow::Separator);
        rows.push(MenuRow::Action(tr("Cut").to_string(), MenuAction::Cut, true));
        rows.push(MenuRow::Action(tr("Copy").to_string(), MenuAction::Copy, true));
        if single_dir {
            rows.push(MenuRow::Action(
                tr("Paste into folder").to_string(),
                MenuAction::Paste,
                d.files.clipboard.is_some(),
            ));
        }
        rows.push(MenuRow::Separator);
        rows.push(MenuRow::Action(
            tr("Rename").to_string(),
            MenuAction::Rename,
            selected.len() == 1,
        ));
        rows.push(MenuRow::Action(
            tr("Delete").to_string(),
            MenuAction::Delete,
            true,
        ));
    }
    rows
}

/// The context menu rows for empty space of the folder.
fn folder_rows(w: &FilesWindow) -> Vec<MenuRow> {
    let mut rows = Vec::new();
    if w.editable() {
        rows.push(MenuRow::Action(
            tr("New folder").to_string(),
            MenuAction::NewFolder,
            true,
        ));
        rows.push(MenuRow::Action(tr("Paste").to_string(), MenuAction::Paste, true));
        rows.push(MenuRow::Separator);
    }
    rows.extend(sort_rows(w));
    rows.push(MenuRow::Separator);
    rows.push(MenuRow::Action(
        if w.show_hidden {
            tr("Hide hidden files").to_string()
        } else {
            tr("Show hidden files").to_string()
        },
        MenuAction::ToggleHidden,
        true,
    ));
    rows.push(MenuRow::Action(
        if w.view == View::Icons {
            tr("List view").to_string()
        } else {
            tr("Icon view").to_string()
        },
        MenuAction::View(if w.view == View::Icons {
            View::List
        } else {
            View::Icons
        }),
        true,
    ));
    if w.editable() {
        rows.push(MenuRow::Separator);
        rows.push(MenuRow::Action(
            tr("Open in terminal").to_string(),
            MenuAction::Terminal,
            true,
        ));
    }
    rows
}

/// The sort rows of a menu, the current key marked.
fn sort_rows(w: &FilesWindow) -> Vec<MenuRow> {
    [SortKey::Name, SortKey::Size, SortKey::Modified, SortKey::Kind]
        .into_iter()
        .map(|key| {
            let mark = if w.sort == key {
                if w.ascending { "  ▲" } else { "  ▼" }
            } else {
                ""
            };
            MenuRow::Action(format!("{}{mark}", key.sort_label()), MenuAction::Sort(key), true)
        })
        .collect()
}

/// Carries out an action on a window.
fn perform(d: &mut Desktop, id: u32, action: MenuAction) {
    let Some(w) = d.files.window_mut(id) else {
        return;
    };
    w.dirty = true;
    match action {
        MenuAction::Open => {
            let mut rows: Vec<usize> = w.selection.iter().copied().collect();
            rows.sort();
            if rows.len() == 1 {
                open_item(d, id, rows[0]);
            } else if !rows.is_empty() {
                let items: Vec<Item> = rows.iter().filter_map(|i| w.items.get(*i).cloned()).collect();
                if let Some(first) = items.iter().find(|i| !i.is_dir) {
                    let mime = first.mime.clone();
                    let paths: Vec<PathBuf> = items
                        .iter()
                        .filter(|i| !i.is_dir && i.mime == mime)
                        .map(|i| i.path.clone())
                        .collect();
                    open_with_default(d, id, &paths, &mime);
                }
            }
        }
        MenuAction::OpenWith(handler) => {
            let paths = w.selected_paths();
            if let Some(entry) = entries::load_entry(&handler) {
                entries::launch_with(&entry, &d.config, &paths);
            }
        }
        MenuAction::OpenInNewWindow => {
            let dirs: Vec<PathBuf> = w.selected_paths().into_iter().filter(|p| p.is_dir()).collect();
            for dir in dirs {
                d.open_files(Some(dir), None);
            }
        }
        MenuAction::Cut | MenuAction::Copy => {
            let paths = w.selected_paths();
            if !paths.is_empty() {
                let cut = action == MenuAction::Cut;
                d.files.clipboard = Some(Clipboard { paths, cut });
                for window in &mut d.files.windows {
                    window.dirty = true;
                }
            }
        }
        MenuAction::Paste => {
            let dest = match w.selection.iter().next().and_then(|i| w.items.get(*i)) {
                Some(item) if item.is_dir && w.selection.len() == 1 => item.path.clone(),
                _ => w.dir.clone(),
            };
            paste(d, id, dest);
        }
        MenuAction::Rename => {
            if let Some(index) = w.cursor.filter(|c| w.selection.contains(c))
                && let Some(item) = w.items.get(index)
            {
                w.rename = Some(Rename {
                    index,
                    text: item.name.clone(),
                });
                scroll_to(w, index);
            }
        }
        MenuAction::Delete => {
            let paths = w.selected_paths();
            if !paths.is_empty() {
                w.confirm = Some(Confirm { paths });
            }
        }
        MenuAction::NewFolder => new_folder(d, id),
        MenuAction::ToggleHidden => {
            w.show_hidden = !w.show_hidden;
            w.refilter();
        }
        MenuAction::View(view) => {
            w.view = view;
            w.scroll = 0.0;
        }
        MenuAction::Sort(key) => {
            if w.sort == key {
                w.ascending = !w.ascending;
            } else {
                w.sort = key;
                w.ascending = true;
            }
            w.refilter();
        }
        MenuAction::Terminal => {
            let dir = w.dir.to_string_lossy().to_string();
            if let Some(terminal) = entries::find_terminal(&d.config)
                && let Some(program) = terminal.first()
            {
                entries::spawn_detached(std::slice::from_ref(program), Some(&dir));
            } else {
                status(d, id, tr("No terminal is installed").to_string());
            }
        }
    }
}

/// Pastes the clipboard into a folder; a cut is a move and empties the clipboard.
fn paste(d: &mut Desktop, id: u32, dest: PathBuf) {
    let Some(clipboard) = d.files.clipboard.clone() else {
        return;
    };
    d.files.transfer(id, clipboard.paths.clone(), dest, clipboard.cut);
    if clipboard.cut {
        d.files.clipboard = None;
        for window in &mut d.files.windows {
            window.dirty = true;
        }
    }
}

/// Creates a folder and starts renaming it.
fn new_folder(d: &mut Desktop, id: u32) {
    let Some(w) = d.files.window(id) else {
        return;
    };
    let path = files::ops::unique_name(&w.dir, tr("New folder"), false);
    if let Err(error) = std::fs::create_dir(&path) {
        status(
            d,
            id,
            trf(
                "Cannot create a folder here: {error}",
                &[("error", &error.to_string())],
            ),
        );
        return;
    }
    d.files.refresh(id);
    if let Some(w) = d.files.window_mut(id)
        && let Some(index) = w.items.iter().position(|i| i.path == path)
    {
        w.selection.clear();
        w.selection.insert(index);
        w.cursor = Some(index);
        w.rename = Some(Rename {
            index,
            text: files::ops::file_name(&path),
        });
        scroll_to(w, index);
    }
}

/// Applies the rename field, if one is open.
fn commit_rename(d: &mut Desktop, id: u32) {
    let Some(w) = d.files.window_mut(id) else {
        return;
    };
    let Some(rename) = w.rename.take() else {
        return;
    };
    w.dirty = true;
    let Some(item) = w.items.get(rename.index).cloned() else {
        return;
    };
    let name = rename.text.trim();
    if name.is_empty() || name == item.name || name.contains('/') {
        return;
    }
    let target = w.dir.join(name);
    if target.exists() {
        status(d, id, trf("“{name}” already exists", &[("name", name)]));
        return;
    }
    match std::fs::rename(&item.path, &target) {
        Ok(()) => {
            d.files.refresh(id);
            if let Some(w) = d.files.window_mut(id)
                && let Some(index) = w.items.iter().position(|i| i.path == target)
            {
                w.selection.clear();
                w.selection.insert(index);
                w.cursor = Some(index);
            }
        }
        Err(error) => status(
            d,
            id,
            trf("Cannot rename: {error}", &[("error", &error.to_string())]),
        ),
    }
}

/// Scrolls so an item is in view.
fn scroll_to(w: &mut FilesWindow, index: usize) {
    let layout = layout(w);
    let rect = item_rect(w, layout.content, index);
    let top = (rect.y - layout.content.y) as f64;
    let bottom = top + rect.h as f64;
    if top < w.scroll {
        w.scroll = top;
    } else if bottom > w.scroll + layout.content.h as f64 {
        w.scroll = bottom - layout.content.h as f64;
    }
    clamp_scroll(w, layout.content);
}

/// Handles wheel scrolling.
///
/// # Arguments
/// * `d` - The desktop.
/// * `id` - The window.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
/// * `delta` - Scroll amount, positive downwards.
pub fn pointer_axis(d: &mut Desktop, id: u32, x: f64, y: f64, delta: f64) {
    let places = d.files.places.len();
    let Some(w) = d.files.window_mut(id) else {
        return;
    };
    let layout = layout(w);
    let step = delta.signum() * ui::SCROLL_STEP;
    if layout.sidebar.w > 0 && layout.sidebar.contains(x, y) {
        let content = places as f64 * PLACE_ROW as f64 + PAD as f64 * 2.0;
        let max = (content - layout.sidebar.h as f64).max(0.0);
        w.sidebar_scroll = (w.sidebar_scroll + step).clamp(0.0, max);
    } else if layout.content.contains(x, y) || layout.header.contains(x, y) {
        w.scroll += step;
        clamp_scroll(w, layout.content);
    } else {
        return;
    }
    w.dirty = true;
    update_hover(d, id, x, y);
}

/// Handles the pointer leaving the window.
///
/// # Arguments
/// * `d` - The desktop.
/// * `id` - The window.
pub fn pointer_leave(d: &mut Desktop, id: u32) {
    if let Some(w) = d.files.window_mut(id) {
        w.press = None;
    }
    if matches!(d.hover, Hover::Files(h, _) if h == id) {
        d.set_hover(Hover::None);
    }
}

/// Handles a key press.
///
/// # Arguments
/// * `d` - The desktop.
/// * `id` - The window.
/// * `key` - The key.
/// * `modifiers` - The modifier keys held.
pub fn key(d: &mut Desktop, id: u32, key: Key, modifiers: Modifiers) {
    let Some(w) = d.files.window_mut(id) else {
        return;
    };
    w.dirty = true;
    if w.confirm.is_some() {
        match key {
            Key::Enter => {
                if let Some(confirm) = w.confirm.take() {
                    d.files.delete(id, confirm.paths);
                }
            }
            Key::Escape => w.confirm = None,
            _ => {}
        }
        return;
    }
    if w.menu.is_some() {
        if key == Key::Escape {
            w.menu = None;
        }
        return;
    }
    if let Some(rename) = w.rename.as_mut() {
        match key {
            Key::Char(c) if !modifiers.ctrl => rename.text.push(c),
            Key::Backspace => {
                rename.text.pop();
            }
            Key::Enter => commit_rename(d, id),
            Key::Escape => w.rename = None,
            _ => {}
        }
        return;
    }
    if let Some(edit) = w.path_edit.as_mut() {
        match key {
            Key::Char(c) if !modifiers.ctrl => edit.push(c),
            Key::Backspace => {
                edit.pop();
            }
            Key::Enter => {
                let text = edit.trim().to_string();
                w.path_edit = None;
                let path = if let Some(rest) = text.strip_prefix("~/") {
                    crate::config::home().join(rest)
                } else if text == "~" {
                    crate::config::home()
                } else {
                    PathBuf::from(text)
                };
                if path.is_dir() {
                    d.files.navigate(id, &path);
                } else {
                    status(
                        d,
                        id,
                        trf("{path} is not a folder", &[("path", &path.display().to_string())]),
                    );
                }
            }
            Key::Escape => w.path_edit = None,
            _ => {}
        }
        return;
    }
    if modifiers.ctrl {
        match key {
            Key::Char('a') | Key::Char('A') => {
                w.selection = (0..w.items.len()).collect();
            }
            Key::Char('c') | Key::Char('C') => perform(d, id, MenuAction::Copy),
            Key::Char('x') | Key::Char('X') => perform(d, id, MenuAction::Cut),
            Key::Char('v') | Key::Char('V') => {
                let dest = w.dir.clone();
                if w.editable() {
                    paste(d, id, dest);
                }
            }
            Key::Char('h') | Key::Char('H') => perform(d, id, MenuAction::ToggleHidden),
            Key::Char('l') | Key::Char('L') => {
                w.path_edit = Some(w.dir.to_string_lossy().to_string());
            }
            Key::Char('n') | Key::Char('N') if w.editable() => {
                if modifiers.shift {
                    new_folder(d, id);
                } else {
                    let dir = w.dir.clone();
                    d.open_files(Some(dir), None);
                }
            }
            Key::Char('w') | Key::Char('W') => close(d, id),
            _ => {}
        }
        return;
    }
    if modifiers.alt {
        match key {
            Key::Left => d.files.step_history(id, -1),
            Key::Right => d.files.step_history(id, 1),
            Key::Up => go_up(d, id),
            _ => {}
        }
        return;
    }
    let layout = layout(w);
    let count = w.items.len();
    match key {
        Key::Char(c) => {
            w.filter.push(c);
            w.refilter();
            if !w.items.is_empty() {
                w.cursor = Some(0);
                w.selection.clear();
                w.selection.insert(0);
            }
            w.scroll = 0.0;
        }
        Key::Backspace => {
            if w.filter.pop().is_some() {
                w.refilter();
            } else {
                go_up(d, id);
            }
        }
        Key::Escape => {
            if !w.filter.is_empty() {
                w.filter.clear();
                w.refilter();
            } else if w.picker.is_some() {
                close(d, id);
            } else {
                w.selection.clear();
            }
        }
        Key::Enter => {
            if let Some(cursor) = w.cursor.filter(|c| w.selection.contains(c)) {
                open_item(d, id, cursor);
            } else if w.selection.len() == 1
                && let Some(only) = w.selection.iter().next().copied()
            {
                open_item(d, id, only);
            }
        }
        Key::Delete => {
            if w.editable() {
                perform(d, id, MenuAction::Delete);
            }
        }
        Key::Function(2) => {
            if w.editable() {
                perform(d, id, MenuAction::Rename);
            }
        }
        Key::Function(5) => d.files.refresh(id),
        Key::Up | Key::Down | Key::Left | Key::Right | Key::Home | Key::End | Key::PageUp | Key::PageDown => {
            if count == 0 {
                return;
            }
            let cols = if w.view == View::Icons {
                grid_columns(layout.content)
            } else {
                1
            };
            let page =
                (layout.content.h / if w.view == View::Icons { CELL_H } else { LIST_ROW }).max(1) * cols;
            let current = w.cursor.map(|c| c as i32);
            let next = match key {
                Key::Up => current.map_or(0, |c| c - cols),
                Key::Down => current.map_or(0, |c| c + cols),
                Key::Left => current.map_or(0, |c| c - 1),
                Key::Right => current.map_or(0, |c| c + 1),
                Key::Home => 0,
                Key::End => count as i32 - 1,
                Key::PageUp => current.map_or(0, |c| c - page),
                _ => current.map_or(0, |c| c + page),
            }
            .clamp(0, count as i32 - 1) as usize;
            if modifiers.shift {
                let from = w.cursor.unwrap_or(next);
                let (lo, hi) = (from.min(next), from.max(next));
                w.selection.extend(lo..=hi);
            } else {
                w.selection.clear();
                w.selection.insert(next);
            }
            w.cursor = Some(next);
            scroll_to(w, next);
        }
        Key::Tab | Key::Super | Key::Function(_) => {}
    }
}

/// Handles a drag entering or moving over the window: finds where it would drop.
///
/// # Arguments
/// * `d` - The desktop.
/// * `id` - The window.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
pub fn drag_motion(d: &mut Desktop, id: u32, x: f64, y: f64) {
    let hit = hit(d, id, x, y);
    let dragged: Vec<PathBuf> = d.files.drag.as_ref().map(|(_, p)| p.clone()).unwrap_or_default();
    let Some(w) = d.files.window_mut(id) else {
        return;
    };
    let target = match hit {
        Hit::Item(i) => match w.items.get(i) {
            Some(item) if item.is_dir && !dragged.contains(&item.path) => Some(DropTarget::Item(i)),
            Some(_) => Some(DropTarget::Folder),
            None => None,
        },
        Hit::Content | Hit::Header(_) => Some(DropTarget::Folder),
        Hit::Place(i) => Some(DropTarget::Place(i)),
        _ => None,
    };
    let target = if w.picker.is_some() { None } else { target };
    if w.drop_target != target {
        w.drop_target = target;
        w.dirty = true;
    }
}

/// Handles a drag leaving the window.
///
/// # Arguments
/// * `d` - The desktop.
/// * `id` - The window.
pub fn drag_leave(d: &mut Desktop, id: u32) {
    if let Some(w) = d.files.window_mut(id)
        && w.drop_target.take().is_some()
    {
        w.dirty = true;
    }
}

/// Drops the dragged files where the drag last hovered in the window.
///
/// # Arguments
/// * `d` - The desktop.
/// * `id` - The window.
/// * `copy` - Whether to copy rather than move.
pub fn drop(d: &mut Desktop, id: u32, copy: bool) {
    let Some((source, paths)) = d.files.drag.clone() else {
        return;
    };
    let Some(w) = d.files.window_mut(id) else {
        return;
    };
    let target = w.drop_target.take();
    w.dirty = true;
    let dest = match target {
        Some(DropTarget::Folder) => Some(w.dir.clone()),
        Some(DropTarget::Item(i)) => w.items.get(i).filter(|i| i.is_dir).map(|i| i.path.clone()),
        Some(DropTarget::Place(i)) => d.files.places.get(i).map(|p| p.path.clone()),
        None => None,
    };
    let Some(dest) = dest else {
        return;
    };
    drop_into(d, source, paths, dest, copy);
}

/// Moves or copies dragged files into a folder, skipping a move onto the
/// folder they are already in.
///
/// # Arguments
/// * `d` - The desktop.
/// * `window` - The window that reports progress.
/// * `paths` - The files.
/// * `dest` - The folder.
/// * `copy` - Whether to copy rather than move.
pub fn drop_into(d: &mut Desktop, window: u32, paths: Vec<PathBuf>, dest: PathBuf, copy: bool) {
    let same = paths.iter().all(|p| p.parent() == Some(dest.as_path()));
    if same && !copy {
        return;
    }
    let reporter = if d.files.window(window).is_some() {
        window
    } else {
        d.files.ids().first().copied().unwrap_or(window)
    };
    d.files.transfer(reporter, paths, dest, !copy);
}

/// The themed icon of an item at a logical size: the folder icon of its
/// kind, or the first icon its type names that the theme has.
fn item_icon(d: &mut Desktop, item: &Item, size: i32) -> Option<Arc<Pixmap>> {
    let scale = d.panel.scale.max(1);
    let px = (size * scale) as u32;
    if item.is_dir {
        let name = files::folder_icon(&d.files.places, &item.path);
        return d
            .icons
            .get(name, px)
            .or_else(|| d.icons.get("folder", px))
            .or_else(|| d.icons.get("inode-directory", px));
    }
    if item.executable
        && item.mime == files::mime::UNKNOWN
        && let Some(icon) = d.icons.get("application-x-executable", px)
    {
        return Some(icon);
    }
    for name in d.files.mime.icon_names(&item.mime) {
        if let Some(icon) = d.icons.get(&name, px) {
            return Some(icon);
        }
    }
    d.icons.get("text-x-generic", px)
}

/// The picture shown for an item: a thumbnail for an image, else its icon.
fn item_picture(d: &mut Desktop, item: &Item, size: i32, scale: i32) -> (Option<Arc<Pixmap>>, bool) {
    if files::mime::is_thumbnail_image(&item.mime)
        && let Some(thumb) = d.files.thumbnail(item, (size * scale) as u32)
    {
        return (Some(thumb), true);
    }
    (item_icon(d, item, size), false)
}

/// A left or right pointing chevron for the history buttons.
fn arrow_path(rect: Rect, direction: i32) -> Option<tiny_skia::Path> {
    let cx = rect.x as f32 + rect.w as f32 / 2.0;
    let cy = rect.y as f32 + rect.h as f32 / 2.0;
    let mut pb = PathBuilder::new();
    let dx = 4.0 * direction as f32;
    pb.move_to(cx + dx, cy - 5.0);
    pb.line_to(cx - dx, cy);
    pb.line_to(cx + dx, cy + 5.0);
    pb.finish()
}

/// An upward chevron for the up button.
fn up_path(rect: Rect) -> Option<tiny_skia::Path> {
    let cx = rect.x as f32 + rect.w as f32 / 2.0;
    let cy = rect.y as f32 + rect.h as f32 / 2.0;
    let mut pb = PathBuilder::new();
    pb.move_to(cx - 5.0, cy + 3.0);
    pb.line_to(cx, cy - 3.0);
    pb.line_to(cx + 5.0, cy + 3.0);
    pb.finish()
}

/// Draws a toolbar button with its glyph.
fn tool_button(
    canvas: &mut Canvas<'_>,
    rect: Rect,
    hovered: bool,
    enabled: bool,
    glyph: Option<tiny_skia::Path>,
) {
    if hovered && enabled {
        canvas.fill_rounded(rect, 6.0, ui::hover_bg());
    }
    let color = if enabled { ui::text() } else { ui::text_dim() };
    if let Some(path) = glyph {
        canvas.stroke_path(&path, 1.8, color);
    }
}

/// Draws the icons/list glyph of the view toggle.
fn draw_view_glyph(canvas: &mut Canvas<'_>, rect: Rect, view: View, color: Color) {
    let cx = rect.x + rect.w / 2;
    let cy = rect.y + rect.h / 2;
    match view {
        View::Icons => {
            for (dx, dy) in [(-6, -6), (1, -6), (-6, 1), (1, 1)] {
                canvas.fill_rounded(Rect::new(cx + dx, cy + dy, 5, 5), 1.0, color);
            }
        }
        View::List => {
            for dy in [-6, -1, 4] {
                canvas.fill_rect(Rect::new(cx - 7, cy + dy, 14, 2), color);
            }
        }
    }
}

/// Draws the sort glyph: three bars of falling length.
fn draw_sort_glyph(canvas: &mut Canvas<'_>, rect: Rect, color: Color) {
    let cx = rect.x + rect.w / 2;
    let cy = rect.y + rect.h / 2;
    for (i, w) in [14, 10, 6].iter().enumerate() {
        canvas.fill_rect(Rect::new(cx - 7, cy - 6 + i as i32 * 5, *w, 2), color);
    }
}

/// Draws a text field with a caret.
fn draw_field(canvas: &mut Canvas<'_>, d: &mut Desktop, rect: Rect, text: &str, scale: i32) {
    canvas.fill_rounded(rect, 5.0, ui::field_bg());
    canvas.stroke_rounded(rect, 5.0, 1.0, ui::accent());
    let text_rect = Rect::new(rect.x + 8, rect.y, rect.w - 16, rect.h);
    let width = ui::text_width(&mut d.fonts, text, ui::FONT_SIZE, false, scale);
    let shown: String = if width > text_rect.w {
        let mut chars: Vec<char> = text.chars().collect();
        while !chars.is_empty()
            && ui::text_width(
                &mut d.fonts,
                &chars.iter().collect::<String>(),
                ui::FONT_SIZE,
                false,
                scale,
            ) > text_rect.w - 4
        {
            chars.remove(0);
        }
        chars.into_iter().collect()
    } else {
        text.to_string()
    };
    ui::label(
        canvas,
        &mut d.fonts,
        &shown,
        text_rect,
        ui::FONT_SIZE,
        false,
        Align::Left,
        ui::text(),
    );
    let caret_x =
        text_rect.x + ui::text_width(&mut d.fonts, &shown, ui::FONT_SIZE, false, scale).min(text_rect.w - 2);
    canvas.fill_rect(
        Rect::new(caret_x + 1, text_rect.y + 7, 1, text_rect.h - 14),
        ui::text(),
    );
}

/// Draws a push button.
fn draw_button(
    canvas: &mut Canvas<'_>,
    d: &mut Desktop,
    rect: Rect,
    label: &str,
    hovered: bool,
    primary: bool,
) {
    let bg = if primary {
        ui::accent()
    } else if hovered {
        ui::hover_bg()
    } else {
        ui::field_bg()
    };
    canvas.fill_rounded(rect, 6.0, bg);
    if !primary {
        canvas.stroke_rounded(rect, 6.0, 1.0, ui::popup_border());
    }
    ui::label(
        canvas,
        &mut d.fonts,
        label,
        rect,
        ui::FONT_SIZE,
        primary,
        Align::Center,
        ui::text(),
    );
}

/// Draws a window.
///
/// # Arguments
/// * `d` - The desktop.
/// * `id` - The window.
/// * `canvas` - The window's canvas.
pub fn draw(d: &mut Desktop, id: u32, canvas: &mut Canvas<'_>) {
    let Some(index) = d.files.windows.iter().position(|w| w.id == id) else {
        return;
    };
    let scale = canvas.scale();
    let layout = layout(&d.files.windows[index]);
    let hover = match d.hover {
        Hover::Files(h, hit) if h == id => hit,
        _ => Hit::None,
    };
    canvas.fill_rect(layout.frame, ui::popup_bg());

    draw_toolbar(d, index, canvas, &layout, hover, scale);
    if layout.sidebar.w > 0 {
        draw_sidebar(d, index, canvas, &layout, hover, scale);
    }
    match d.files.windows[index].view {
        View::Icons => draw_grid(d, index, canvas, &layout, hover, scale),
        View::List => draw_list(d, index, canvas, &layout, hover, scale),
    }
    let places = d.files.places.len();
    let w = &d.files.windows[index];
    for area in [ScrollArea::Sidebar, ScrollArea::Content] {
        if let Some(bar) = scrollbar(w, &layout, area, places) {
            let active =
                hover == Hit::Scrollbar(area) || w.bar_drag.is_some_and(|(dragged, _)| dragged == area);
            bar.draw(canvas, active);
        }
    }
    draw_status(d, index, canvas, &layout, scale);
    if let Some(footer) = layout.footer {
        draw_footer(d, index, canvas, footer, hover);
    }
    if d.files.windows[index].confirm.is_some() {
        draw_confirm(d, index, canvas, &layout, hover);
    }
    if d.files.windows[index].menu.is_some() {
        draw_menu(d, index, canvas, hover);
    }
}

/// Draws the toolbar: history buttons, the path and the view controls.
fn draw_toolbar(
    d: &mut Desktop,
    index: usize,
    canvas: &mut Canvas<'_>,
    layout: &Layout,
    hover: Hit,
    scale: i32,
) {
    let (can_back, can_forward, has_parent, view, dir, path_edit) = {
        let w = &d.files.windows[index];
        (
            w.history_pos > 0,
            w.history_pos + 1 < w.history.len(),
            w.dir.parent().is_some(),
            w.view,
            w.dir.clone(),
            w.path_edit.clone(),
        )
    };
    canvas.fill_rect(Rect::new(0, TOOLBAR_H - 1, layout.frame.w, 1), ui::separator());
    tool_button(
        canvas,
        layout.back,
        hover == Hit::Back,
        can_back,
        arrow_path(layout.back, 1),
    );
    tool_button(
        canvas,
        layout.forward,
        hover == Hit::Forward,
        can_forward,
        arrow_path(layout.forward, -1),
    );
    tool_button(
        canvas,
        layout.up,
        hover == Hit::Up,
        has_parent,
        up_path(layout.up),
    );

    if let Some(text) = path_edit {
        draw_field(canvas, d, layout.path, &text, scale);
    } else {
        canvas.fill_rounded(layout.path, 6.0, ui::field_bg());
        let rects = crumb_rects(&mut d.fonts, &dir, scale, layout.path);
        let crumbs = crumbs(&dir);
        canvas.clip(layout.path);
        if rects.first().is_some_and(|(i, _)| *i > 0) {
            ui::label(
                canvas,
                &mut d.fonts,
                "…",
                Rect::new(layout.path.x + 4, layout.path.y, 18, layout.path.h),
                ui::FONT_SIZE,
                false,
                Align::Center,
                ui::text_dim(),
            );
        }
        let last = crumbs.len().saturating_sub(1);
        for (i, rect) in &rects {
            let current = *i == last;
            if hover == Hit::Crumb(*i) && !current {
                canvas.fill_rounded(*rect, 5.0, ui::hover_bg());
            } else if current {
                canvas.fill_rounded(*rect, 5.0, ui::active_bg());
            }
            ui::label(
                canvas,
                &mut d.fonts,
                &crumbs[*i].0,
                *rect,
                ui::FONT_SIZE,
                current,
                Align::Center,
                ui::text(),
            );
            if !current
                && let Some(chevron) = ui::chevron_path(rect.right() + CRUMB_GAP / 2 - 2, rect.y + rect.h / 2)
            {
                canvas.stroke_path(&chevron, 1.2, ui::text_dim());
            }
        }
        canvas.reset_clip();
    }

    if hover == Hit::ViewToggle {
        canvas.fill_rounded(layout.view_toggle, 6.0, ui::hover_bg());
    }
    draw_view_glyph(
        canvas,
        layout.view_toggle,
        if view == View::Icons {
            View::List
        } else {
            View::Icons
        },
        ui::text(),
    );
    if hover == Hit::SortButton {
        canvas.fill_rounded(layout.sort, 6.0, ui::hover_bg());
    }
    draw_sort_glyph(canvas, layout.sort, ui::text());
}

/// Draws the sidebar places.
fn draw_sidebar(
    d: &mut Desktop,
    index: usize,
    canvas: &mut Canvas<'_>,
    layout: &Layout,
    hover: Hit,
    scale: i32,
) {
    canvas.fill_rect(layout.sidebar, rgba(0, 0, 0, 0x30));
    canvas.fill_rect(
        Rect::new(layout.sidebar.right() - 1, layout.sidebar.y, 1, layout.sidebar.h),
        ui::separator(),
    );
    let (dir, sidebar_scroll, drop_target) = {
        let w = &d.files.windows[index];
        (w.dir.clone(), w.sidebar_scroll, w.drop_target)
    };
    canvas.clip(layout.sidebar);
    let places = d.files.places.clone();
    for (i, place) in places.iter().enumerate() {
        let y = layout.sidebar.y + PAD + i as i32 * PLACE_ROW - sidebar_scroll as i32;
        let rect = Rect::new(
            layout.sidebar.x + PAD,
            y,
            layout.sidebar.w - 2 * PAD - 1,
            PLACE_ROW,
        );
        let current = place.path == dir;
        if drop_target == Some(DropTarget::Place(i)) {
            canvas.fill_rounded(rect, 6.0, ui::accent_alpha(0x50));
            canvas.stroke_rounded(rect, 6.0, 1.0, ui::accent());
        } else if current {
            canvas.fill_rounded(rect, 6.0, ui::active_bg());
        } else if hover == Hit::Place(i) {
            canvas.fill_rounded(rect, 6.0, ui::hover_bg());
        }
        let icon_rect = Rect::new(
            rect.x + 8,
            rect.y + (PLACE_ROW - ui::ICON_SIZE) / 2,
            ui::ICON_SIZE,
            ui::ICON_SIZE,
        );
        if let Some(icon) = d
            .icons
            .get(place.icon, (ui::ICON_SIZE * scale) as u32)
            .or_else(|| d.icons.get("folder", (ui::ICON_SIZE * scale) as u32))
        {
            canvas.draw_image_fit(&icon, icon_rect);
        }
        ui::label(
            canvas,
            &mut d.fonts,
            &place.name,
            Rect::new(icon_rect.right() + 8, rect.y, rect.w - 34, rect.h),
            ui::FONT_SIZE,
            current,
            Align::Left,
            ui::text(),
        );
    }
    canvas.reset_clip();
}

/// The fill and border of an item's background for its state.
fn item_background(selected: bool, hovered: bool, drop: bool) -> (Option<Color>, Option<Color>) {
    if drop {
        (Some(ui::accent_alpha(0x40)), Some(ui::accent()))
    } else if selected {
        (Some(ui::accent_alpha(0x55)), Some(ui::accent_alpha(0x90)))
    } else if hovered {
        (Some(ui::hover_bg()), None)
    } else {
        (None, None)
    }
}

/// Draws the icon grid.
fn draw_grid(
    d: &mut Desktop,
    index: usize,
    canvas: &mut Canvas<'_>,
    layout: &Layout,
    hover: Hit,
    scale: i32,
) {
    let content = layout.content;
    canvas.clip(content);
    let (count, scroll) = {
        let w = &d.files.windows[index];
        (w.items.len(), w.scroll as i32)
    };
    if count == 0 {
        draw_empty(d, index, canvas, content);
        canvas.reset_clip();
        return;
    }
    let cut_paths: Vec<PathBuf> = d
        .files
        .clipboard
        .as_ref()
        .filter(|c| c.cut)
        .map(|c| c.paths.clone())
        .unwrap_or_default();
    let font_px = ui::FONT_SMALL * scale as f32;
    for i in 0..count {
        let cell = cell_rect(content, i);
        let cell = Rect::new(cell.x, cell.y - scroll, cell.w, cell.h);
        if cell.bottom() < content.y || cell.y > content.bottom() {
            continue;
        }
        let (item, selected, drop, renaming) = {
            let w = &d.files.windows[index];
            (
                w.items[i].clone(),
                w.selection.contains(&i),
                w.drop_target == Some(DropTarget::Item(i)),
                w.rename.as_ref().filter(|r| r.index == i).map(|r| r.text.clone()),
            )
        };
        let cut = cut_paths.contains(&item.path);
        let (fill, border) = item_background(selected, hover == Hit::Item(i), drop);
        let inner = cell.inflate(-4);
        if let Some(fill) = fill {
            canvas.fill_rounded(inner, 8.0, fill);
        }
        if let Some(border) = border {
            canvas.stroke_rounded(inner, 8.0, 1.0, border);
        }
        let icon_rect = Rect::new(
            cell.x + (cell.w - GRID_ICON) / 2,
            cell.y + 10,
            GRID_ICON,
            GRID_ICON,
        );
        let (picture, is_thumb) = item_picture(d, &item, GRID_ICON, scale);
        match picture {
            Some(picture) => {
                if is_thumb {
                    let w = (picture.width() / scale as u32) as i32;
                    let h = (picture.height() / scale as u32) as i32;
                    let rect = Rect::new(
                        icon_rect.x + (GRID_ICON - w) / 2,
                        icon_rect.y + (GRID_ICON - h) / 2,
                        w,
                        h,
                    );
                    canvas.fill_rect(rect.inflate(1), rgba(0xff, 0xff, 0xff, 0x30));
                    canvas.draw_image_fit(&picture, rect);
                } else {
                    canvas.draw_image_fit(&picture, icon_rect);
                }
            }
            None => canvas.fill_rounded(icon_rect.inflate(-8), 6.0, ui::field_bg()),
        }
        if item.is_symlink {
            let badge = Rect::new(icon_rect.right() - 12, icon_rect.bottom() - 12, 12, 12);
            canvas.fill_rounded(badge, 3.0, ui::tooltip_bg());
            if let Some(arrow) = arrow_path(badge, -1) {
                canvas.stroke_path(&arrow, 1.2, ui::text());
            }
        }
        let text_top = icon_rect.bottom() + 6;
        if let Some(text) = renaming {
            let field = Rect::new(cell.x + 2, text_top - 2, cell.w - 4, 24);
            draw_field(canvas, d, field, &text, scale);
            continue;
        }
        let color = if cut { ui::text_dim() } else { ui::text() };
        let max_width = (cell.w - 10) as f32 * scale as f32;
        let (line1, line2) = ui::wrap_two_lines(&mut d.fonts, &item.name, font_px, max_width);
        let line_h = 16;
        canvas.draw_text(
            &mut d.fonts,
            &line1,
            cell.x,
            text_top + line_h / 2,
            cell.w,
            Align::Center,
            color,
        );
        if let Some(line2) = line2 {
            canvas.draw_text(
                &mut d.fonts,
                &line2,
                cell.x,
                text_top + line_h + line_h / 2,
                cell.w,
                Align::Center,
                color,
            );
        }
    }
    canvas.reset_clip();
}

/// Draws the list with its column header.
fn draw_list(
    d: &mut Desktop,
    index: usize,
    canvas: &mut Canvas<'_>,
    layout: &Layout,
    hover: Hit,
    scale: i32,
) {
    let (sort, ascending) = {
        let w = &d.files.windows[index];
        (w.sort, w.ascending)
    };
    let cols = columns(layout.header);
    for (i, (key, rect)) in cols.iter().enumerate() {
        if hover == Hit::Header(i) {
            canvas.fill_rounded(rect.inflate(-2), 4.0, ui::hover_bg());
        }
        let mut text = key.label().to_string();
        if *key == sort {
            text.push_str(if ascending { "  ▲" } else { "  ▼" });
        }
        let align = if *key == SortKey::Size {
            Align::Right
        } else {
            Align::Left
        };
        ui::label(
            canvas,
            &mut d.fonts,
            &text,
            Rect::new(rect.x + 6, rect.y, rect.w - 12, rect.h),
            ui::FONT_SMALL,
            true,
            align,
            ui::text_dim(),
        );
    }
    canvas.fill_rect(
        Rect::new(layout.header.x, layout.header.bottom() - 1, layout.header.w, 1),
        ui::separator(),
    );
    let content = layout.content;
    canvas.clip(content);
    let (count, scroll) = {
        let w = &d.files.windows[index];
        (w.items.len(), w.scroll as i32)
    };
    if count == 0 {
        draw_empty(d, index, canvas, content);
        canvas.reset_clip();
        return;
    }
    let cut_paths: Vec<PathBuf> = d
        .files
        .clipboard
        .as_ref()
        .filter(|c| c.cut)
        .map(|c| c.paths.clone())
        .unwrap_or_default();
    let first = (scroll / LIST_ROW).max(0) as usize;
    let last = (((scroll + content.h) / LIST_ROW) as usize + 1).min(count);
    for i in first..last {
        let row = row_rect(content, i);
        let row = Rect::new(row.x, row.y - scroll, row.w, row.h);
        let (item, selected, drop, renaming) = {
            let w = &d.files.windows[index];
            (
                w.items[i].clone(),
                w.selection.contains(&i),
                w.drop_target == Some(DropTarget::Item(i)),
                w.rename.as_ref().filter(|r| r.index == i).map(|r| r.text.clone()),
            )
        };
        let cut = cut_paths.contains(&item.path);
        let (fill, border) = item_background(selected, hover == Hit::Item(i), drop);
        let inner = Rect::new(row.x + 4, row.y + 1, row.w - 8, row.h - 2);
        if let Some(fill) = fill {
            canvas.fill_rounded(inner, 5.0, fill);
        }
        if let Some(border) = border {
            canvas.stroke_rounded(inner, 5.0, 1.0, border);
        }
        let color = if cut { ui::text_dim() } else { ui::text() };
        for (key, col) in &cols {
            let col = Rect::new(col.x, row.y, col.w, row.h);
            match key {
                SortKey::Name => {
                    let icon_rect = Rect::new(
                        col.x + 4,
                        row.y + (LIST_ROW - ui::ICON_SIZE) / 2,
                        ui::ICON_SIZE,
                        ui::ICON_SIZE,
                    );
                    let (picture, _) = item_picture(d, &item, ui::ICON_SIZE, scale);
                    if let Some(picture) = picture {
                        canvas.draw_image_fit(&picture, icon_rect);
                    }
                    let name_rect =
                        Rect::new(icon_rect.right() + 8, row.y, col.w - ui::ICON_SIZE - 16, row.h);
                    if let Some(text) = &renaming {
                        draw_field(
                            canvas,
                            d,
                            Rect::new(name_rect.x - 4, row.y + 2, name_rect.w, row.h - 4),
                            text,
                            scale,
                        );
                    } else {
                        ui::label(
                            canvas,
                            &mut d.fonts,
                            &item.name,
                            name_rect,
                            ui::FONT_SIZE,
                            false,
                            Align::Left,
                            color,
                        );
                    }
                }
                SortKey::Size => {
                    let text = if item.is_dir {
                        String::new()
                    } else {
                        files::human_size(item.size)
                    };
                    ui::label(
                        canvas,
                        &mut d.fonts,
                        &text,
                        Rect::new(col.x + 6, row.y, col.w - 12, row.h),
                        ui::FONT_SMALL,
                        false,
                        Align::Right,
                        ui::text_dim(),
                    );
                }
                SortKey::Modified => {
                    let text = files::human_time(item.modified);
                    ui::label(
                        canvas,
                        &mut d.fonts,
                        &text,
                        Rect::new(col.x + 6, row.y, col.w - 12, row.h),
                        ui::FONT_SMALL,
                        false,
                        Align::Left,
                        ui::text_dim(),
                    );
                }
                SortKey::Kind => {
                    let text = if item.is_dir {
                        tr("Folder").to_string()
                    } else {
                        d.files
                            .mime
                            .comment(&item.mime)
                            .unwrap_or_else(|| item.mime.clone())
                    };
                    ui::label(
                        canvas,
                        &mut d.fonts,
                        &text,
                        Rect::new(col.x + 6, row.y, col.w - 12, row.h),
                        ui::FONT_SMALL,
                        false,
                        Align::Left,
                        ui::text_dim(),
                    );
                }
            }
        }
    }
    canvas.reset_clip();
}

/// Draws the message of an empty listing.
fn draw_empty(d: &mut Desktop, index: usize, canvas: &mut Canvas<'_>, content: Rect) {
    let w = &d.files.windows[index];
    let message = if !w.filter.is_empty() {
        tr("No matches")
    } else if w.picker.is_some() {
        tr("No images in this folder")
    } else if w.all_items().is_empty() {
        tr("This folder is empty")
    } else {
        tr("Only hidden files here")
    };
    ui::label(
        canvas,
        &mut d.fonts,
        message,
        Rect::new(content.x, content.y + 20, content.w, 30),
        ui::FONT_SIZE,
        false,
        Align::Center,
        ui::text_dim(),
    );
}

/// Draws the status bar.
fn draw_status(d: &mut Desktop, index: usize, canvas: &mut Canvas<'_>, layout: &Layout, scale: i32) {
    let status = layout.status;
    canvas.fill_rect(Rect::new(status.x, status.y, status.w, 1), ui::separator());
    let (text, filter) = {
        let w = &d.files.windows[index];
        let text = if let Some((_, progress)) = &w.progress {
            trf(
                "Processing {name} ({done} of {total})",
                &[
                    ("name", &progress.current),
                    ("done", &(progress.done + 1).to_string()),
                    ("total", &progress.total.to_string()),
                ],
            )
        } else if let Some((message, _)) = &w.status {
            message.clone()
        } else {
            let count = w.items.len();
            let mut text = if count == 1 {
                tr("1 item").to_string()
            } else {
                trf("{n} items", &[("n", &count.to_string())])
            };
            if !w.selection.is_empty() {
                let size: u64 = w
                    .selection
                    .iter()
                    .filter_map(|i| w.items.get(*i))
                    .map(|i| i.size)
                    .sum();
                text.push_str(", ");
                text.push_str(&trf("{n} selected", &[("n", &w.selection.len().to_string())]));
                if size > 0 {
                    text.push_str(&format!(" ({})", files::human_size(size)));
                }
            }
            text
        };
        (text, w.filter.clone())
    };
    let mut right = status.right() - PAD;
    if !filter.is_empty() {
        let label = trf("Filter: {text}", &[("text", &filter)]);
        let width = ui::text_width(&mut d.fonts, &label, ui::FONT_SMALL, false, scale) + 16;
        let chip = Rect::new(right - width, status.y + 4, width, status.h - 8);
        canvas.fill_rounded(chip, 5.0, ui::active_bg());
        ui::label(
            canvas,
            &mut d.fonts,
            &label,
            chip,
            ui::FONT_SMALL,
            false,
            Align::Center,
            ui::text(),
        );
        right = chip.x - PAD;
    }
    ui::label(
        canvas,
        &mut d.fonts,
        &text,
        Rect::new(status.x + PAD + 2, status.y, right - status.x - PAD - 2, status.h),
        ui::FONT_SMALL,
        false,
        Align::Left,
        ui::text_dim(),
    );
}

/// Draws the picker footer.
fn draw_footer(d: &mut Desktop, index: usize, canvas: &mut Canvas<'_>, footer: Rect, hover: Hit) {
    canvas.fill_rect(Rect::new(footer.x, footer.y, footer.w, 1), ui::separator());
    let chosen = {
        let w = &d.files.windows[index];
        w.cursor
            .filter(|c| w.selection.contains(c))
            .and_then(|c| w.items.get(c))
            .filter(|i| !i.is_dir)
            .map(|i| i.name.clone())
    };
    let (cancel, open) = footer_buttons(footer);
    ui::label(
        canvas,
        &mut d.fonts,
        &chosen
            .clone()
            .unwrap_or_else(|| tr("Choose an image").to_string()),
        Rect::new(
            footer.x + PAD + 2,
            footer.y,
            cancel.x - footer.x - 2 * PAD,
            footer.h,
        ),
        ui::FONT_SIZE,
        false,
        Align::Left,
        if chosen.is_some() {
            ui::text()
        } else {
            ui::text_dim()
        },
    );
    draw_button(
        canvas,
        d,
        cancel,
        tr("Cancel"),
        hover == Hit::PickerButton(0),
        false,
    );
    draw_button(
        canvas,
        d,
        open,
        tr("Open"),
        hover == Hit::PickerButton(1),
        chosen.is_some(),
    );
}

/// Draws the deletion confirmation over the content.
fn draw_confirm(d: &mut Desktop, index: usize, canvas: &mut Canvas<'_>, layout: &Layout, hover: Hit) {
    canvas.fill_rect(layout.frame, rgba(0, 0, 0, 0x70));
    let (bx, cancel, delete) = confirm_rects(&d.files.windows[index]);
    let paths = d.files.windows[index]
        .confirm
        .as_ref()
        .map(|c| c.paths.clone())
        .unwrap_or_default();
    ui::draw_popup_frame(canvas, bx);
    let title = if paths.len() == 1 {
        trf("Delete “{name}”?", &[("name", &files::ops::file_name(&paths[0]))])
    } else {
        trf("Delete {n} items?", &[("n", &paths.len().to_string())])
    };
    ui::label(
        canvas,
        &mut d.fonts,
        &title,
        Rect::new(bx.x + 16, bx.y + 14, bx.w - 32, 26),
        ui::FONT_SIZE + 1.0,
        true,
        Align::Left,
        ui::text(),
    );
    ui::label(
        canvas,
        &mut d.fonts,
        tr("This cannot be undone."),
        Rect::new(bx.x + 16, bx.y + 44, bx.w - 32, 22),
        ui::FONT_SMALL,
        false,
        Align::Left,
        ui::text_dim(),
    );
    draw_button(canvas, d, cancel, tr("Cancel"), hover == Hit::Confirm(0), false);
    let hovered = hover == Hit::Confirm(1);
    canvas.fill_rounded(
        delete,
        6.0,
        if hovered {
            rgba(0xe0, 0x50, 0x50, 0xff)
        } else {
            rgba(0xc8, 0x3c, 0x3c, 0xff)
        },
    );
    ui::label(
        canvas,
        &mut d.fonts,
        tr("Delete"),
        delete,
        ui::FONT_SIZE,
        true,
        Align::Center,
        ui::text(),
    );
}

/// Draws the context menu.
fn draw_menu(d: &mut Desktop, index: usize, canvas: &mut Canvas<'_>, hover: Hit) {
    let Some(menu) = d.files.windows[index].menu.clone() else {
        return;
    };
    let frame = menu_rect(&d.files.windows[index], &menu);
    canvas.fill_rounded(frame.inflate(1), ui::popup_radius(), rgba(0, 0, 0, 0x50));
    ui::draw_popup_frame(canvas, frame);
    let mut y = frame.y + 6;
    for (i, row) in menu.rows.iter().enumerate() {
        match row {
            MenuRow::Separator => {
                canvas.fill_rect(
                    Rect::new(frame.x + 10, y + MENU_SEP / 2, frame.w - 20, 1),
                    ui::separator(),
                );
                y += MENU_SEP;
            }
            MenuRow::Action(label, _, enabled) => {
                let rect = Rect::new(frame.x + 6, y, frame.w - 12, MENU_ROW);
                if hover == Hit::MenuRow(i) && *enabled {
                    canvas.fill_rounded(rect, 5.0, ui::hover_bg());
                }
                ui::label(
                    canvas,
                    &mut d.fonts,
                    label,
                    Rect::new(rect.x + 10, rect.y, rect.w - 20, rect.h),
                    ui::FONT_SIZE,
                    false,
                    Align::Left,
                    if *enabled { ui::text() } else { ui::text_dim() },
                );
                y += MENU_ROW;
            }
        }
    }
}

/// The surface a drop on the wallpaper lands in: the Desktop folder.
///
/// # Arguments
/// * `d` - The desktop.
/// * `copy` - Whether to copy rather than move.
pub fn drop_on_desktop(d: &mut Desktop, copy: bool) {
    let Some((source, paths)) = d.files.drag.clone() else {
        return;
    };
    let dest = entries::desktop_dir();
    if !dest.is_dir() {
        return;
    }
    drop_into(d, source, paths, dest, copy);
}
