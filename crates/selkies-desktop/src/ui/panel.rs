//! The bar: start button, quick launch buttons, taskbar, system tray slot,
//! volume, clock and the show desktop button.  The start button, quick
//! launch and taskbar sit at the left, or in the middle of the bar when
//! the layout is centered; the tray, volume, clock and show desktop button
//! stay at the right.

use crate::desktop::{Desktop, Hover, Popup};
use crate::event::{Button, WindowAction};
use crate::geom::Rect;
use crate::render::{Align, Canvas};
use crate::ui;

/// Space between bar items.
const GAP: i32 = 4;
/// Widest a taskbar button grows.
const TASK_MAX_WIDTH: i32 = 200;
/// Narrowest a taskbar button shrinks.
const TASK_MIN_WIDTH: i32 = 40;
/// Width of the volume button.
const VOLUME_WIDTH: i32 = 32;
/// Width of the window outline on the show desktop button for a button
/// height: two fifths of it, within the range that reads well.
fn show_desktop_glyph_width(height: i32) -> i32 {
    (height * 2 / 5).clamp(14, 20)
}

/// Side of the start button's icon for a button height.
fn start_icon_size(height: i32) -> i32 {
    (height * 22 / 26).clamp(16, 48)
}

/// Space between the start button's icon and its label.
const LABEL_GAP: i32 = 6;

/// Width of the start button: room for its icon, and for its label when it
/// has one.
fn start_width(d: &mut Desktop, height: i32) -> i32 {
    let base = ui::START_WIDTH.max(start_icon_size(height) + 10);
    match d.config.start_label.clone() {
        Some(label) => {
            base + LABEL_GAP + ui::text_width(&mut d.fonts, &label, ui::FONT_SIZE, true, d.panel.scale)
        }
        None => base,
    }
}

/// Where everything on the bar is.
pub struct Layout {
    /// The bar.
    pub bar: Rect,
    /// The start button.
    pub start: Rect,
    /// Each quick launch button with its application index.
    pub launchers: Vec<(usize, Rect)>,
    /// Each taskbar button with its window index.
    pub tasks: Vec<(usize, Rect)>,
    /// The strip the system tray icons occupy, when any are docked.
    pub tray: Option<Rect>,
    /// The volume button, when shown.
    pub volume: Option<Rect>,
    /// The clock, when shown.
    pub clock: Option<Rect>,
    /// The show desktop button, when shown.
    pub show_desktop: Option<Rect>,
}

impl Layout {
    /// The rectangle of a hover target on the bar, for tooltips.
    ///
    /// # Arguments
    /// * `hover` - The hover target.
    pub fn rect_of(&self, hover: Hover) -> Option<Rect> {
        match hover {
            Hover::Start => Some(self.start),
            Hover::Launcher(i) => self.launchers.get(i).map(|(_, r)| *r),
            Hover::Task(i) => self.tasks.iter().find(|(t, _)| *t == i).map(|(_, r)| *r),
            Hover::Volume => self.volume,
            Hover::Clock => self.clock,
            Hover::ShowDesktop => self.show_desktop,
            _ => None,
        }
    }
}

/// Computes the bar layout for the current state.
///
/// # Arguments
/// * `d` - The desktop.
pub fn layout(d: &mut Desktop) -> Layout {
    let bar = d.bar_rect();
    let start_width = start_width(d, bar.h - 6);
    let launcher_ids: Vec<usize> = d
        .config
        .launchers
        .iter()
        .filter_map(|id| d.apps.find(id))
        .collect();
    let launchers_width = launcher_ids.len() as i32 * (bar.h - 6 + GAP);
    let mut right = bar.right() - GAP;
    let show_desktop = if d.config.show_desktop_button {
        let width = show_desktop_glyph_width(bar.h - 6) + 8;
        right -= width;
        let r = Rect::new(right, bar.y + 3, width, bar.h - 6);
        right -= GAP;
        Some(r)
    } else {
        None
    };
    let clock = if d.config.show_clock && !d.clock.is_empty() {
        let width = ui::text_width(&mut d.fonts, &d.clock, ui::FONT_SIZE, false, d.panel.scale) + 18;
        right -= width;
        let r = Rect::new(right, bar.y + 3, width, bar.h - 6);
        right -= GAP;
        Some(r)
    } else {
        None
    };
    let volume = if d.config.show_volume && d.volume.is_some() {
        right -= VOLUME_WIDTH;
        let r = Rect::new(right, bar.y + 3, VOLUME_WIDTH, bar.h - 6);
        right -= GAP;
        Some(r)
    } else {
        None
    };
    let tray = if d.config.show_tray && d.tray_icons > 0 {
        let size = d.panel_icon_size() + 4;
        let width = d.tray_icons as i32 * (size + GAP) + GAP;
        right -= width;
        let r = Rect::new(right, bar.y + 3, width, bar.h - 6);
        right -= GAP;
        Some(r)
    } else {
        None
    };
    let left_edge = bar.x + GAP;
    let available = (right - GAP - (left_edge + start_width + GAP + launchers_width + GAP)).max(0);
    let count = d.toplevels.len() as i32;
    let task_width = if count > 0 {
        ((available - (count - 1) * GAP) / count).clamp(TASK_MIN_WIDTH, TASK_MAX_WIDTH)
    } else {
        0
    };
    let shown = if count > 0 {
        (((available + GAP) / (task_width + GAP)).max(0) as usize).min(d.toplevels.len())
    } else {
        0
    };
    let group_width = start_width + GAP + launchers_width + GAP + shown as i32 * (task_width + GAP);
    let x0 = if d.config.panel_centered {
        (bar.x + (bar.w - group_width) / 2).clamp(left_edge, (right - GAP - group_width).max(left_edge))
    } else {
        left_edge
    };
    let start = Rect::new(x0, bar.y + 3, start_width, bar.h - 6);
    let mut launchers = Vec::new();
    let mut x = start.right() + GAP;
    for index in launcher_ids {
        launchers.push((index, Rect::new(x, bar.y + 3, bar.h - 6, bar.h - 6)));
        x += bar.h - 6 + GAP;
    }
    let mut tasks = Vec::new();
    x += GAP;
    for index in 0..shown {
        tasks.push((index, Rect::new(x, bar.y + 3, task_width, bar.h - 6)));
        x += task_width + GAP;
    }
    if d.tray_slot != tray {
        d.tray_slot = tray;
        d.tray_moved = true;
    }
    Layout {
        bar,
        start,
        launchers,
        tasks,
        tray,
        volume,
        clock,
        show_desktop,
    }
}

/// What the pointer is over on the bar.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
pub fn hit(d: &mut Desktop, x: f64, y: f64) -> Hover {
    let layout = layout(d);
    if !layout.bar.contains(x, y) {
        return Hover::None;
    }
    if layout.start.contains(x, y) {
        return Hover::Start;
    }
    for (position, (_, rect)) in layout.launchers.iter().enumerate() {
        if rect.contains(x, y) {
            return Hover::Launcher(position);
        }
    }
    for (index, rect) in &layout.tasks {
        if rect.contains(x, y) {
            return Hover::Task(*index);
        }
    }
    if layout.volume.is_some_and(|r| r.contains(x, y)) {
        return Hover::Volume;
    }
    if layout.clock.is_some_and(|r| r.contains(x, y)) {
        return Hover::Clock;
    }
    if layout.show_desktop.is_some_and(|r| r.contains(x, y)) {
        return Hover::ShowDesktop;
    }
    Hover::None
}

/// Handles a button press on the bar.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
/// * `button` - The button.
/// * `time` - Event time in milliseconds.
pub fn click(d: &mut Desktop, x: f64, y: f64, button: Button, _time: u32) {
    match hit(d, x, y) {
        Hover::Start => match button {
            Button::Left => d.toggle_menu(),
            Button::Right => d.open_popup(Popup::PanelMenu(x as i32)),
            Button::Middle => {}
        },
        Hover::Launcher(position) => match button {
            Button::Left => {
                if let Some(index) = layout(d).launchers.get(position).map(|(i, _)| *i) {
                    d.launch_app(index);
                }
            }
            Button::Right => d.open_popup(Popup::PanelMenu(x as i32)),
            Button::Middle => {}
        },
        Hover::Task(index) => match button {
            Button::Left => d.task_click(index),
            Button::Middle => d.window_action(index, WindowAction::Close),
            Button::Right => d.open_popup(Popup::TaskMenu(index)),
        },
        Hover::Volume => match button {
            Button::Left => d.open_popup(Popup::Volume),
            Button::Middle => d.toggle_mute(),
            Button::Right => d.open_popup(Popup::PanelMenu(x as i32)),
        },
        Hover::Clock => match button {
            Button::Left => d.open_popup(Popup::Calendar),
            Button::Right => d.open_popup(Popup::PanelMenu(x as i32)),
            Button::Middle => {}
        },
        Hover::ShowDesktop => match button {
            Button::Left => d.show_desktop(),
            Button::Right => d.open_popup(Popup::PanelMenu(x as i32)),
            Button::Middle => {}
        },
        _ => {
            if button == Button::Right && d.bar_rect().contains(x, y) {
                d.open_popup(Popup::PanelMenu(x as i32));
            }
        }
    }
    d.redraw_panel();
}

/// Handles wheel scrolling on the bar: the volume over its button, and
/// cycling through the windows over the taskbar.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
/// * `delta` - Scroll amount, positive downwards.
pub fn scroll(d: &mut Desktop, x: f64, y: f64, delta: f64) {
    match hit(d, x, y) {
        Hover::Volume => d.adjust_volume(if delta < 0.0 { 5 } else { -5 }),
        Hover::Task(_) => cycle_windows(d, delta > 0.0),
        _ => {
            let layout = layout(d);
            let tasks_area = match (layout.tasks.first(), layout.tasks.last()) {
                (Some((_, first)), Some((_, last))) => first.union(last),
                _ => return,
            };
            if tasks_area.contains(x, y) {
                cycle_windows(d, delta > 0.0);
            }
        }
    }
}

/// Activates the window after or before the active one.
fn cycle_windows(d: &mut Desktop, forward: bool) {
    let count = d.toplevels.len();
    if count == 0 {
        return;
    }
    let current = d.toplevels.iter().position(|t| t.state.active);
    let next = match current {
        Some(i) if forward => (i + 1) % count,
        Some(i) => (i + count - 1) % count,
        None => 0,
    };
    if d.toplevels[next].state.minimized {
        d.window_action(next, WindowAction::Unminimize);
    }
    d.window_action(next, WindowAction::Activate);
}

/// Draws the bar.
///
/// # Arguments
/// * `d` - The desktop.
/// * `canvas` - The panel canvas.
pub fn draw(d: &mut Desktop, canvas: &mut Canvas<'_>) {
    let layout = layout(d);
    let bg = ui::panel_bg(d.config.panel_opacity);
    if d.config.panel_margin > 0 {
        let radius = ui::panel_radius();
        ui::fill_surface(canvas, layout.bar, radius, bg);
        canvas.stroke_rounded(layout.bar, radius, 1.0, ui::separator());
    } else if layout.bar.w < d.panel.width {
        let radius = ui::panel_radius();
        let overflow = radius as i32;
        let shape = if d.panel_top() {
            Rect::new(
                layout.bar.x,
                layout.bar.y - overflow,
                layout.bar.w,
                layout.bar.h + overflow,
            )
        } else {
            Rect::new(layout.bar.x, layout.bar.y, layout.bar.w, layout.bar.h + overflow)
        };
        ui::fill_surface(canvas, shape, radius, bg);
        canvas.stroke_rounded(shape, radius, 1.0, ui::separator());
    } else {
        ui::fill_surface(canvas, layout.bar, 0.0, bg);
        canvas.fill_rect(
            Rect::new(layout.bar.x, layout.bar.y, layout.bar.w, 1),
            ui::separator(),
        );
    }

    let start_bg = if d.popup == Popup::Menu {
        Some(ui::active_bg())
    } else if d.hover == Hover::Start {
        Some(ui::hover_bg())
    } else {
        None
    };
    if let Some(bg) = start_bg {
        canvas.fill_rounded(layout.start, 5.0, bg);
    }
    let size = start_icon_size(layout.start.h);
    let icon_width = ui::START_WIDTH.max(size + 10);
    let icon_slot = Rect::new(layout.start.x, layout.start.y, icon_width, layout.start.h);
    if let Some(icon) = d.start_icon(size) {
        let icon_rect = Rect::new(
            icon_slot.x + (icon_slot.w - size) / 2,
            icon_slot.y + (icon_slot.h - size) / 2,
            size,
            size,
        );
        canvas.draw_image_fit(&icon, icon_rect);
    } else {
        ui::label(
            canvas,
            &mut d.fonts,
            "⋮⋮⋮",
            icon_slot,
            15.0,
            true,
            Align::Center,
            ui::text(),
        );
    }
    if let Some(label) = d.config.start_label.clone() {
        let text_rect = Rect::new(
            icon_slot.right() - 2,
            layout.start.y,
            layout.start.right() - icon_slot.right(),
            layout.start.h,
        );
        ui::label(
            canvas,
            &mut d.fonts,
            &label,
            text_rect,
            ui::FONT_SIZE,
            true,
            Align::Left,
            ui::text(),
        );
    }

    for (position, (index, rect)) in layout.launchers.iter().copied().enumerate() {
        if d.hover == Hover::Launcher(position) {
            canvas.fill_rounded(rect, 5.0, ui::hover_bg());
        }
        let size = d.panel_icon_size() + 2;
        let icon_rect = Rect::new(
            rect.x + (rect.w - size) / 2,
            rect.y + (rect.h - size) / 2,
            size,
            size,
        );
        if let Some(icon) = d.app_icon_sized(index, size) {
            canvas.draw_image_fit(&icon, icon_rect);
        } else {
            canvas.fill_rounded(icon_rect.inflate(-3), 3.0, ui::field_bg());
        }
    }
    if let Some(rect) = layout.tray {
        canvas.fill_rect(
            Rect::new(rect.right() + GAP / 2, rect.y + 4, 1, rect.h - 8),
            ui::separator(),
        );
    }

    for (index, rect) in layout.tasks.iter().copied() {
        let (active, minimized) = {
            let t = &d.toplevels[index];
            (t.state.active && !t.state.minimized, t.state.minimized)
        };
        let hovered = d.hover == Hover::Task(index) || d.popup == Popup::TaskMenu(index);
        let bg = if active {
            ui::active_bg()
        } else if hovered {
            ui::hover_bg()
        } else {
            ui::field_bg()
        };
        canvas.fill_rounded(rect, 5.0, bg);
        if active {
            canvas.fill_rounded(
                Rect::new(rect.x + 6, rect.bottom() - 3, rect.w - 12, 2),
                1.0,
                ui::accent(),
            );
        }
        let mut text_x = rect.x + 7;
        let icon_size = d.panel_icon_size();
        if rect.w >= icon_size + 14 {
            let icon_rect = Rect::new(text_x, rect.y + (rect.h - icon_size) / 2, icon_size, icon_size);
            if let Some(icon) = d.toplevel_icon(index) {
                canvas.draw_image_fit(&icon, icon_rect);
            } else {
                draw_window_glyph(canvas, icon_rect);
            }
            text_x += icon_size + 7;
        }
        let text_rect = Rect::new(text_x, rect.y, rect.right() - text_x - 7, rect.h);
        if text_rect.w > 12 {
            let color = if minimized { ui::text_dim() } else { ui::text() };
            let label = d.toplevels[index].label().to_string();
            ui::label(
                canvas,
                &mut d.fonts,
                &label,
                text_rect,
                ui::FONT_SIZE,
                false,
                Align::Left,
                color,
            );
        }
    }

    if let Some(rect) = layout.volume {
        if d.hover == Hover::Volume || d.popup == Popup::Volume {
            canvas.fill_rounded(
                rect,
                5.0,
                if d.popup == Popup::Volume {
                    ui::active_bg()
                } else {
                    ui::hover_bg()
                },
            );
        }
        if let Some(volume) = d.volume {
            let (body, waves) = ui::speaker_path(rect, volume.percent, volume.muted);
            let color = if volume.muted { ui::text_dim() } else { ui::text() };
            canvas.fill_path(&body, color);
            if let Some(waves) = waves {
                canvas.stroke_path(&waves, 1.6, color);
            }
        }
    }

    if let Some(rect) = layout.clock {
        if d.hover == Hover::Clock || d.popup == Popup::Calendar {
            canvas.fill_rounded(
                rect,
                5.0,
                if d.popup == Popup::Calendar {
                    ui::active_bg()
                } else {
                    ui::hover_bg()
                },
            );
        }
        let clock = d.clock.clone();
        ui::label(
            canvas,
            &mut d.fonts,
            &clock,
            rect,
            ui::FONT_SIZE,
            false,
            Align::Center,
            ui::text(),
        );
    }

    if let Some(rect) = layout.show_desktop {
        canvas.fill_rect(
            Rect::new(rect.x - GAP / 2 - 1, rect.y + 4, 1, rect.h - 8),
            ui::separator(),
        );
        let shown = !d.shown_desktop.is_empty();
        if d.hover == Hover::ShowDesktop || shown {
            canvas.fill_rounded(rect, 4.0, if shown { ui::active_bg() } else { ui::hover_bg() });
        }
        let glyph_w = show_desktop_glyph_width(rect.h);
        let glyph_h = glyph_w * 5 / 7;
        let glyph = Rect::new(
            rect.x + (rect.w - glyph_w) / 2,
            rect.y + (rect.h - glyph_h) / 2 - 1,
            glyph_w,
            glyph_h,
        );
        canvas.stroke_rounded(glyph, 2.0, 1.3, ui::text());
        canvas.fill_rect(
            Rect::new(glyph.x + 4, glyph.bottom() + 2, glyph.w - 8, 1),
            ui::text(),
        );
        canvas.fill_rect(Rect::new(glyph.x + glyph.w / 2, glyph.bottom(), 1, 2), ui::text());
    }
}

/// A generic window outline for windows without an icon.
fn draw_window_glyph(canvas: &mut Canvas<'_>, rect: Rect) {
    let inner = rect.inflate(-2);
    canvas.stroke_rounded(inner, 2.0, 1.2, ui::text_dim());
    canvas.fill_rect(Rect::new(inner.x, inner.y, inner.w, 4), ui::text_dim());
}
