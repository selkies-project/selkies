//! The small popups above the bar: the volume slider, the calendar, the
//! context menu of a taskbar button, and tooltips.

use crate::clock;
use crate::desktop::{Desktop, Hover, Popup};
use crate::event::{Button, WindowAction};
use crate::geom::Rect;
use crate::i18n::tr;
use crate::render::{Align, Canvas, rgba};
use crate::ui::{self, menu, panel, settings};
use crate::xkb::Key;

/// Volume popup size.
const VOLUME_SIZE: (i32, i32) = (260, 56);
/// Width of a calendar day cell.
const DAY: i32 = 34;
/// Height of a calendar row.
const DAY_ROW: i32 = 28;
/// Calendar padding.
const CAL_PAD: i32 = 12;
/// Task menu width.
const TASK_MENU_WIDTH: i32 = 190;
/// Task menu row height.
const TASK_ROW: i32 = 30;

/// Places a popup of a size beside the bar, above it when the bar is at the
/// bottom and below it when at the top, horizontally centered on an anchor
/// and kept inside the surface.
///
/// # Arguments
/// * `d` - The desktop.
/// * `anchor` - The bar item the popup belongs to.
/// * `w` - Popup width.
/// * `h` - Popup height.
pub fn anchor_above(d: &Desktop, anchor: Rect, w: i32, h: i32) -> Rect {
    let bar = d.bar_rect();
    let max_x = d.panel.width - ui::POPUP_MARGIN - w;
    let x = (anchor.x + anchor.w / 2 - w / 2).clamp(ui::POPUP_MARGIN, max_x.max(ui::POPUP_MARGIN));
    let y = if d.panel_top() {
        bar.bottom() + ui::POPUP_MARGIN
    } else {
        bar.y - ui::POPUP_MARGIN - h
    };
    Rect::new(x, y, w, h)
}

/// Draws the open popup and the tooltip; returns the rectangles drawn.
///
/// # Arguments
/// * `d` - The desktop.
/// * `canvas` - The panel canvas.
pub fn draw(d: &mut Desktop, canvas: &mut Canvas<'_>) -> Vec<Rect> {
    let mut rects = Vec::new();
    match d.popup {
        Popup::None => {}
        Popup::Menu => rects.push(menu::draw(d, canvas)),
        Popup::Volume => rects.extend(draw_volume(d, canvas)),
        Popup::Calendar => rects.extend(draw_calendar(d, canvas)),
        Popup::TaskMenu(index) => rects.extend(draw_task_menu(d, canvas, index)),
        Popup::PanelMenu(x) => rects.push(settings::draw_panel_menu(d, canvas, x)),
        Popup::Settings => rects.push(settings::draw(d, canvas)),
    }
    if let Some(text) = d.tooltip.clone()
        && let Some(rect) = draw_tooltip(d, canvas, &text)
    {
        rects.push(rect);
    }
    rects
}

/// The volume popup rectangle, when it can be placed.
fn volume_rect(d: &mut Desktop) -> Option<Rect> {
    let anchor = panel::layout(d).volume?;
    Some(anchor_above(d, anchor, VOLUME_SIZE.0, VOLUME_SIZE.1))
}

/// The slider track inside the volume popup.
fn volume_track(frame: Rect) -> Rect {
    Rect::new(frame.x + 48, frame.y + frame.h / 2 - 3, frame.w - 48 - 60, 6)
}

/// Draws the volume popup.
fn draw_volume(d: &mut Desktop, canvas: &mut Canvas<'_>) -> Option<Rect> {
    let frame = volume_rect(d)?;
    let volume = d.volume?;
    ui::draw_popup_frame(canvas, frame);
    let icon_rect = Rect::new(frame.x + 10, frame.y, 30, frame.h);
    let (body, waves) = ui::speaker_path(icon_rect, volume.percent, volume.muted);
    let color = if volume.muted { ui::text_dim() } else { ui::text() };
    canvas.fill_path(&body, color);
    if let Some(waves) = waves {
        canvas.stroke_path(&waves, 1.6, color);
    }
    let track = volume_track(frame);
    canvas.fill_rounded(track, 3.0, ui::field_bg());
    let fill_w = (track.w as f64 * (volume.percent.min(100) as f64 / 100.0)) as i32;
    let filled = Rect::new(track.x, track.y, fill_w, track.h);
    canvas.fill_rounded(
        filled,
        3.0,
        if volume.muted {
            ui::text_dim()
        } else {
            ui::accent()
        },
    );
    let knob_x = track.x + fill_w;
    canvas.fill_rounded(
        Rect::new(knob_x - 7, track.y + track.h / 2 - 7, 14, 14),
        7.0,
        ui::text(),
    );
    let label = if volume.muted {
        tr("Muted").to_string()
    } else {
        format!("{}%", volume.percent)
    };
    let label_rect = Rect::new(
        track.right() + 10,
        frame.y,
        frame.right() - track.right() - 14,
        frame.h,
    );
    ui::label(
        canvas,
        &mut d.fonts,
        &label,
        label_rect,
        ui::FONT_SIZE,
        false,
        Align::Right,
        ui::text(),
    );
    Some(frame)
}

/// Sets the volume from a pointer X over the slider.
fn volume_from_x(d: &mut Desktop, frame: Rect, x: f64) {
    let track = volume_track(frame);
    let ratio = ((x - track.x as f64) / track.w as f64).clamp(0.0, 1.0);
    d.set_volume_target((ratio * 100.0).round() as u8);
}

/// Handles a click; returns whether it landed in the volume popup.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
/// * `button` - The button.
pub fn volume_click(d: &mut Desktop, x: f64, y: f64, button: Button) -> bool {
    let Some(frame) = volume_rect(d) else {
        return false;
    };
    if !frame.contains(x, y) {
        return false;
    }
    if button != Button::Left {
        d.toggle_mute();
        return true;
    }
    let icon_rect = Rect::new(frame.x, frame.y, 44, frame.h);
    if icon_rect.contains(x, y) {
        d.toggle_mute();
        return true;
    }
    let track = volume_track(frame).inflate(8);
    if track.contains(x, y) {
        d.volume_drag = true;
        volume_from_x(d, frame, x);
    }
    true
}

/// Follows a slider drag.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
pub fn volume_drag(d: &mut Desktop, x: f64) {
    if let Some(frame) = volume_rect(d) {
        volume_from_x(d, frame, x);
    }
}

/// Handles wheel scrolling; returns whether it landed in the volume popup.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
/// * `delta` - Scroll amount, positive downwards.
pub fn volume_scroll(d: &mut Desktop, x: f64, y: f64, delta: f64) -> bool {
    let Some(frame) = volume_rect(d) else {
        return false;
    };
    if !frame.contains(x, y) {
        return false;
    }
    d.adjust_volume(if delta < 0.0 { 5 } else { -5 });
    true
}

/// Handles keys while the volume popup is open.
///
/// # Arguments
/// * `d` - The desktop.
/// * `key` - The key.
pub fn volume_key(d: &mut Desktop, key: Key) {
    match key {
        Key::Up | Key::Right => d.adjust_volume(5),
        Key::Down | Key::Left => d.adjust_volume(-5),
        Key::Char('m') | Key::Char('M') => d.toggle_mute(),
        _ => {}
    }
}

/// The calendar popup rectangle.
fn calendar_rect(d: &mut Desktop) -> Option<Rect> {
    let anchor = panel::layout(d).clock?;
    let w = DAY * 7 + CAL_PAD * 2;
    let h = CAL_PAD + 30 + 22 + DAY_ROW * 6 + CAL_PAD;
    Some(anchor_above(d, anchor, w, h))
}

/// The month the calendar shows, from the current one and the offset.
fn shown_month(d: &Desktop) -> (i32, u32) {
    let now = clock::now();
    let total = now.year * 12 + now.month as i32 - 1 + d.calendar_offset;
    (total.div_euclid(12), (total.rem_euclid(12) + 1) as u32)
}

/// Draws the calendar popup.
fn draw_calendar(d: &mut Desktop, canvas: &mut Canvas<'_>) -> Option<Rect> {
    let frame = calendar_rect(d)?;
    ui::draw_popup_frame(canvas, frame);
    let (year, month) = shown_month(d);
    let today = clock::now();

    let header = Rect::new(frame.x + CAL_PAD, frame.y + CAL_PAD, frame.w - 2 * CAL_PAD, 30);
    let title = format!("{} {}", clock::month_name(month), year);
    ui::label(
        canvas,
        &mut d.fonts,
        &title,
        header,
        ui::FONT_SIZE,
        true,
        Align::Center,
        ui::text(),
    );
    for (label, rect) in [
        ("‹", Rect::new(header.x, header.y, 30, 30)),
        ("›", Rect::new(header.right() - 30, header.y, 30, 30)),
    ] {
        canvas.fill_rounded(rect, 5.0, ui::field_bg());
        ui::label(
            canvas,
            &mut d.fonts,
            label,
            rect,
            16.0,
            false,
            Align::Center,
            ui::text(),
        );
    }

    let names = clock::weekday_names();
    let grid_x = frame.x + CAL_PAD;
    let names_y = header.bottom() + 2;
    for (i, name) in names.iter().enumerate() {
        let rect = Rect::new(grid_x + i as i32 * DAY, names_y, DAY, 20);
        let short: String = name.chars().take(2).collect();
        ui::label(
            canvas,
            &mut d.fonts,
            &short,
            rect,
            ui::FONT_SMALL,
            false,
            Align::Center,
            ui::text_dim(),
        );
    }

    let first_weekday = clock::weekday(year, month, 1) as i32;
    let days = clock::days_in_month(year, month) as i32;
    let grid_y = names_y + 22;
    for day in 1..=days {
        let slot = first_weekday + day - 1;
        let col = slot % 7;
        let row = slot / 7;
        let rect = Rect::new(grid_x + col * DAY, grid_y + row * DAY_ROW, DAY, DAY_ROW);
        let is_today = year == today.year && month == today.month && day as u32 == today.day;
        if is_today {
            canvas.fill_rounded(
                Rect::new(rect.x + 3, rect.y + 1, rect.w - 6, rect.h - 2),
                6.0,
                ui::accent(),
            );
        }
        let color = if col == 0 || col == 6 {
            ui::text_dim()
        } else {
            ui::text()
        };
        ui::label(
            canvas,
            &mut d.fonts,
            &day.to_string(),
            rect,
            ui::FONT_SIZE,
            is_today,
            Align::Center,
            if is_today { ui::text() } else { color },
        );
    }
    Some(frame)
}

/// Handles a click; returns whether it landed in the calendar.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
/// * `button` - The button.
pub fn calendar_click(d: &mut Desktop, x: f64, y: f64, button: Button) -> bool {
    let Some(frame) = calendar_rect(d) else {
        return false;
    };
    if !frame.contains(x, y) {
        return false;
    }
    if button != Button::Left {
        return true;
    }
    let header = Rect::new(frame.x + CAL_PAD, frame.y + CAL_PAD, frame.w - 2 * CAL_PAD, 30);
    if Rect::new(header.x, header.y, 30, 30).contains(x, y) {
        d.calendar_offset -= 1;
    } else if Rect::new(header.right() - 30, header.y, 30, 30).contains(x, y) {
        d.calendar_offset += 1;
    } else if header.contains(x, y) {
        d.calendar_offset = 0;
    }
    d.redraw_panel();
    true
}

/// Handles wheel scrolling over the calendar: moves by months.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
/// * `delta` - Scroll amount, positive downwards.
pub fn calendar_scroll(d: &mut Desktop, x: f64, y: f64, delta: f64) -> bool {
    let Some(frame) = calendar_rect(d) else {
        return false;
    };
    if !frame.contains(x, y) {
        return false;
    }
    d.calendar_offset += if delta > 0.0 { 1 } else { -1 };
    d.redraw_panel();
    true
}

/// Handles keys while the calendar is open.
///
/// # Arguments
/// * `d` - The desktop.
/// * `key` - The key.
pub fn calendar_key(d: &mut Desktop, key: Key) {
    match key {
        Key::Left | Key::Up | Key::PageUp => d.calendar_offset -= 1,
        Key::Right | Key::Down | Key::PageDown => d.calendar_offset += 1,
        Key::Home => d.calendar_offset = 0,
        _ => return,
    }
    d.redraw_panel();
}

/// The actions a task menu offers for a window.
fn task_menu_items(d: &Desktop, index: usize) -> Vec<(&'static str, WindowAction)> {
    let Some(toplevel) = d.toplevels.get(index) else {
        return Vec::new();
    };
    let mut items = Vec::new();
    if toplevel.state.minimized {
        items.push((tr("Restore"), WindowAction::Unminimize));
    } else {
        items.push((tr("Minimize"), WindowAction::Minimize));
    }
    if toplevel.state.maximized {
        items.push((tr("Unmaximize"), WindowAction::Unmaximize));
    } else {
        items.push((tr("Maximize"), WindowAction::Maximize));
    }
    items.push((tr("Close"), WindowAction::Close));
    items
}

/// The task menu rectangle for a window.
fn task_menu_rect(d: &mut Desktop, index: usize) -> Option<Rect> {
    let anchor = panel::layout(d).rect_of(Hover::Task(index))?;
    let items = task_menu_items(d, index).len() as i32;
    Some(anchor_above(d, anchor, TASK_MENU_WIDTH, items * TASK_ROW + 12))
}

/// Draws the task menu.
fn draw_task_menu(d: &mut Desktop, canvas: &mut Canvas<'_>, index: usize) -> Option<Rect> {
    let frame = task_menu_rect(d, index)?;
    ui::draw_popup_frame(canvas, frame);
    for (row, (label, _)) in task_menu_items(d, index).into_iter().enumerate() {
        let rect = Rect::new(
            frame.x + 6,
            frame.y + 6 + row as i32 * TASK_ROW,
            frame.w - 12,
            TASK_ROW,
        );
        if d.hover == Hover::TaskMenuItem(row) {
            canvas.fill_rounded(rect, 5.0, ui::hover_bg());
        }
        let label_rect = Rect::new(rect.x + 10, rect.y, rect.w - 20, rect.h);
        ui::label(
            canvas,
            &mut d.fonts,
            label,
            label_rect,
            ui::FONT_SIZE,
            false,
            Align::Left,
            ui::text(),
        );
    }
    Some(frame)
}

/// The task menu item under a point, or `None` outside the menu.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
pub fn task_menu_hit(d: &mut Desktop, x: f64, y: f64) -> Option<Hover> {
    let Popup::TaskMenu(index) = d.popup else {
        return None;
    };
    let frame = task_menu_rect(d, index)?;
    if !frame.contains(x, y) {
        return None;
    }
    let row = ((y - frame.y as f64 - 6.0) / TASK_ROW as f64).floor();
    if row >= 0.0 && (row as usize) < task_menu_items(d, index).len() {
        return Some(Hover::TaskMenuItem(row as usize));
    }
    Some(Hover::None)
}

/// Handles a click; returns whether it landed in the task menu.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
/// * `button` - The button.
pub fn task_menu_click(d: &mut Desktop, x: f64, y: f64, button: Button) -> bool {
    let Popup::TaskMenu(index) = d.popup else {
        return false;
    };
    let Some(hover) = task_menu_hit(d, x, y) else {
        return false;
    };
    if button == Button::Left
        && let Hover::TaskMenuItem(row) = hover
        && let Some((_, action)) = task_menu_items(d, index).get(row).copied()
    {
        d.window_action(index, action);
        d.close_popup();
    }
    true
}

/// Draws the tooltip above the hovered bar item; returns its rectangle.
fn draw_tooltip(d: &mut Desktop, canvas: &mut Canvas<'_>, text: &str) -> Option<Rect> {
    let anchor = panel::layout(d).rect_of(d.hover)?;
    let width =
        ui::text_width(&mut d.fonts, text, ui::FONT_SMALL, false, d.panel.scale).min(d.panel.width - 40) + 18;
    let frame = anchor_above(d, anchor, width, 26);
    canvas.fill_rounded(frame.inflate(1), 6.0, rgba(0, 0, 0, 0x60));
    canvas.fill_rounded(frame, 5.0, ui::tooltip_bg());
    canvas.stroke_rounded(frame, 5.0, 1.0, ui::popup_border());
    let text_rect = Rect::new(frame.x + 9, frame.y, frame.w - 18, frame.h);
    ui::label(
        canvas,
        &mut d.fonts,
        text,
        text_rect,
        ui::FONT_SMALL,
        false,
        Align::Center,
        ui::text(),
    );
    Some(frame)
}
