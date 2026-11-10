//! The previews of the settings window.  The Theme page's is a small
//! desktop drawn with a [`Look`], the colors and the window style as they
//! are or as a preset would set them, with the wallpaper, two windows in the
//! window manager's frames, the start menu open in its layout and the bar in
//! its layout, so every choice shows at once without opening anything.  The
//! live preview paints straight onto the page; a preset card renders its
//! look into a pixmap of its own, which the page then shrinks.  The
//! Wallpaper page's is one monitor at its own aspect with the bar along its
//! edge.  Both paint the wallpaper through the desktop's own painter
//! ([`background::paint_wallpaper`]), from a copy of the image shrunk once to
//! the size it is drawn at, so a preview cannot disagree with the desktop.

use std::sync::Arc;

use tiny_skia::{Color, Pixmap};

use crate::config::{ButtonSide, Glyphs, MenuLayout, Wallpaper, WindowStyle};
use crate::desktop::Desktop;
use crate::geom::Rect;
use crate::i18n::tr;
use crate::render::{Align, Canvas};
use crate::ui::{self, background};
use crate::wmtheme::{self, Palette};

/// Width of the preview's screen.
pub const WIDTH: i32 = 352;
/// Height of the preview's screen.
pub const HEIGHT: i32 = 220;
/// Height of the bar in the Theme page's preview.
const BAR: i32 = 20;
/// The least height of the bar in the Wallpaper page's preview, which
/// leaves its items room.
const BAR_LEAST: i32 = 18;
/// Height of a title bar in the preview.
const TITLE: i32 = 18;
/// Side of a window button's bitmap.
const GLYPH: i32 = wmtheme::GLYPH as i32;
/// Font size of the preview's text.
const FONT: f32 = 8.5;
/// Height of a row in the preview's lists.
const ROW: i32 = 13;
/// The names the preview's file manager shows.
const FOLDERS: [&str; 6] = ["Documents", "Downloads", "Music", "Pictures", "Public", "Videos"];
/// The applications the preview's start menu lists.
const APPS: [&str; 5] = ["Files", "Terminal", "Browser", "Editor", "Settings"];

/// What the preview shows the desktop as: the colors, the window style, and
/// the three layout settings a style bundles with it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Look {
    /// The interface colors.
    pub theme: ui::Theme,
    /// The window frames, the bar and the popups.
    pub style: WindowStyle,
    /// Whether the bar's items sit in its middle.
    pub centered: bool,
    /// The floating bar's margin from the screen edges, 0 for a bar that does not float.
    pub margin: i32,
    /// The floating bar's corner radius.
    pub bar_corners: i32,
    /// The start menu's layout.
    pub menu: MenuLayout,
}

impl Look {
    /// The look the desktop has at the moment.
    pub fn current(d: &Desktop) -> Look {
        Look {
            theme: d.config.theme,
            style: d.config.window,
            centered: d.config.panel_centered,
            margin: d.float_margin(),
            bar_corners: d.config.panel_corners,
            menu: d.config.menu_layout,
        }
    }
}

/// Draws the preview of the current look centered in an area.
///
/// # Arguments
/// * `d` - The desktop.
/// * `canvas` - The settings window's canvas.
/// * `area` - The area the preview is centered in.
/// * `page` - The page area the canvas is clipped to, restored afterwards.
pub fn draw(d: &mut Desktop, canvas: &mut Canvas<'_>, area: Rect, page: Rect) {
    let screen = Rect::new(
        area.x + (area.w - WIDTH) / 2,
        area.y + (area.h - HEIGHT) / 2,
        WIDTH,
        HEIGHT,
    );
    canvas.clip(screen);
    paint(d, canvas, screen, &Look::current(d));
    canvas.reset_clip();
    canvas.clip(page);
}

/// Renders a look into a pixmap of the preview's size, for a preset card.
/// Every color helper reads the theme and style in use, so they are swapped
/// for the look while it paints and put back after.
///
/// # Arguments
/// * `d` - The desktop.
/// * `look` - The look to show.
/// * `scale` - Output scale the pixmap is rendered at.
pub fn render(d: &mut Desktop, look: &Look, scale: i32) -> Option<Pixmap> {
    let scale = scale.max(1);
    let mut pix = Pixmap::new((WIDTH * scale) as u32, (HEIGHT * scale) as u32)?;
    let (theme, style) = (ui::theme(), ui::style());
    ui::set_theme(look.theme);
    ui::set_style(look.style);
    {
        let mut canvas = Canvas::new(pix.as_mut(), scale);
        paint(d, &mut canvas, Rect::new(0, 0, WIDTH, HEIGHT), look);
    }
    ui::set_theme(theme);
    ui::set_style(style);
    Some(pix)
}

/// Paints the desktop of a look into the screen rectangle: the primary
/// monitor's wallpaper, the two windows, the start menu and the bar.
fn paint(d: &mut Desktop, canvas: &mut Canvas<'_>, screen: Rect, look: &Look) {
    let wallpaper = d.config.wallpaper_for(1);
    paint_wallpaper(d, canvas, screen, &wallpaper, monitor_geometry(d, None));
    let palette = Palette::new(look.theme);
    let top = d.panel_top();
    let margin = look.margin / 2;
    let desk_y = if top { screen.y + BAR + margin } else { screen.y };
    let back = Rect::new(screen.x + 214, desk_y + 18, 130, 104);
    draw_window(d, canvas, back, &palette, look.style, false, tr("Window"));
    draw_terminal(canvas, back);
    let front = Rect::new(screen.x + 150, desk_y + 42, 196, 148);
    draw_window(d, canvas, front, &palette, look.style, true, tr("Files"));
    draw_files(d, canvas, front);
    let menu_h = 130;
    let menu_y = if top {
        screen.y + BAR + margin + 4
    } else {
        screen.bottom() - BAR - margin - 4 - menu_h
    };
    draw_menu(
        d,
        canvas,
        Rect::new(screen.x + 4 + margin, menu_y, 150, menu_h),
        look.menu,
    );
    draw_bar(d, canvas, screen, look, BAR, margin);
    canvas.stroke_rounded(screen, 4.0, 1.0, ui::popup_border());
}

/// A monitor's logical rectangle and output scale, by its settings number,
/// the primary for `None`; a 1920x1080 screen before any is known.
///
/// # Arguments
/// * `d` - The desktop.
/// * `number` - The monitor's number, from 1.
pub fn monitor_geometry(d: &Desktop, number: Option<u32>) -> (Rect, i32) {
    let index = number.unwrap_or(1).max(1) as usize - 1;
    d.monitor_list()
        .get(index)
        .and_then(|(id, at)| {
            let background = d.background(*id)?;
            let whole = Rect::new(0, 0, background.geometry.width, background.geometry.height);
            let rect = background.monitors.get(*at).copied().unwrap_or(whole);
            (!rect.is_empty()).then_some((rect, background.geometry.scale.max(1)))
        })
        .unwrap_or((Rect::new(0, 0, 1920, 1080), 1))
}

/// How many shrunk wallpaper pictures the previews keep before starting over.
const PREVIEW_WALLPAPERS: usize = 8;

/// Paints a monitor's wallpaper into a preview's screen as the desktop
/// paints it there, its picture and color the theme being drawn chooses, the
/// image shrunk once per path and drawn size.
///
/// # Arguments
/// * `d` - The desktop.
/// * `canvas` - The canvas painted.
/// * `screen` - The preview's screen.
/// * `wallpaper` - The wallpaper settings.
/// * `monitor` - The monitor's logical rectangle and scale, which size the
///   image in the center mode.
fn paint_wallpaper(
    d: &mut Desktop,
    canvas: &mut Canvas<'_>,
    screen: Rect,
    wallpaper: &Wallpaper,
    monitor: (Rect, i32),
) {
    let size = (
        (monitor.0.w * monitor.1).max(1) as u32,
        (monitor.0.h * monitor.1).max(1) as u32,
    );
    let wallpaper = &d.painted_wallpaper(wallpaper.clone(), size);
    let full = wallpaper.image.as_ref().and_then(|path| d.wallpaper_image(path));
    let (Some(path), Some(full)) = (wallpaper.image.clone(), full) else {
        background::paint_wallpaper(canvas, screen, wallpaper, None);
        return;
    };
    let (rect, scale) = monitor;
    let zoom = screen.w as f32 / rect.w.max(1) as f32;
    let native = (
        full.width() as f32 / scale as f32 * zoom,
        full.height() as f32 / scale as f32 * zoom,
    );
    let (w, h) = background::drawn_size(wallpaper.mode, native, screen);
    let longest =
        ((w.max(h) * canvas.scale() as f32).ceil() as u32).clamp(1, full.width().max(full.height()));
    let key = (path, longest);
    let cached = d.preview_wallpaper.get(&key).cloned();
    let small = cached.or_else(|| {
        let small = Arc::new(crate::render::resize_pixmap(&full, longest)?);
        if d.preview_wallpaper.len() >= PREVIEW_WALLPAPERS {
            d.preview_wallpaper.clear();
        }
        d.preview_wallpaper.insert(key, small.clone());
        Some(small)
    });
    let image = small.as_deref().map(|pixels| background::WallpaperImage {
        pixels,
        native: (native.0.round() as i32, native.1.round() as i32),
    });
    background::paint_wallpaper(canvas, screen, wallpaper, image);
}

/// Draws the Wallpaper page's preview centered in an area: the monitor at
/// its own aspect with its wallpaper, and the bar along its edge, at the
/// monitor's proportions, when the bar is on that monitor.
///
/// # Arguments
/// * `d` - The desktop.
/// * `canvas` - The settings window's canvas.
/// * `area` - The area the preview is centered in.
/// * `wallpaper` - The wallpaper shown.
/// * `number` - The monitor's number, or `None` for the shared wallpaper,
///   shown on the primary.
pub fn draw_monitor(
    d: &mut Desktop,
    canvas: &mut Canvas<'_>,
    area: Rect,
    wallpaper: &Wallpaper,
    number: Option<u32>,
) {
    let monitor = monitor_geometry(d, number);
    let (rect, _) = monitor;
    let ratio = (area.w as f32 / rect.w.max(1) as f32).min(area.h as f32 / rect.h.max(1) as f32);
    let (w, h) = (
        (rect.w as f32 * ratio).round() as i32,
        (rect.h as f32 * ratio).round() as i32,
    );
    let screen = Rect::new(area.x + (area.w - w) / 2, area.y + (area.h - h) / 2, w, h);
    canvas.clipped(screen, |canvas| {
        paint_wallpaper(d, canvas, screen, wallpaper, monitor);
        if number.unwrap_or(1) == 1 {
            let height = ((d.config.panel_height as f32 * ratio).round() as i32).max(BAR_LEAST);
            let margin = (d.float_margin() as f32 * ratio).round() as i32;
            let look = Look::current(d);
            draw_bar(d, canvas, screen, &look, height, margin);
        }
    });
    canvas.stroke_rounded(screen, 4.0, 1.0, ui::popup_border());
}

/// Draws a window's frame: the title bar shaded in the surface style with
/// its buttons on their side, the border, and the body in the popup color.
fn draw_window(
    d: &mut Desktop,
    canvas: &mut Canvas<'_>,
    rect: Rect,
    palette: &Palette,
    style: WindowStyle,
    active: bool,
    title: &str,
) {
    let radius = (style.corners as f32 * 0.75).round();
    let title_rect = Rect::new(rect.x, rect.y, rect.w, TITLE);
    let border = if active {
        palette.border_active
    } else {
        palette.border_inactive
    };
    canvas.fill_rounded(rect, radius, border);
    let cap = Rect::new(rect.x, rect.y, rect.w, TITLE + radius as i32);
    let surface = style.surface;
    canvas.fill_rounded_shaded(cap, radius, |_, v| {
        let v = (v * cap.h as f32 / TITLE as f32).min(1.0);
        palette.title_shade(surface, active, v)
    });
    canvas.fill_rect(
        Rect::new(rect.x + 1, rect.y + TITLE, rect.w - 2, rect.h - TITLE - 1),
        ui::popup_bg(),
    );
    let label = if active {
        palette.label_active
    } else {
        palette.label_inactive
    };
    let (slot, gap) = match style.glyphs {
        Glyphs::Lines => (14, 0),
        Glyphs::Circles => (12, 2),
        Glyphs::Wide => (22, 0),
    };
    let names = ["iconify", "max", "close"];
    let order: Vec<&str> = match style.buttons {
        ButtonSide::Right => names.to_vec(),
        ButtonSide::Left => vec!["close", "iconify", "max"],
    };
    let total = 3 * slot + 2 * gap;
    let mut x = match style.buttons {
        ButtonSide::Right => title_rect.right() - 3 - total,
        ButtonSide::Left => title_rect.x + 3,
    };
    for name in &order {
        let cell = Rect::new(x, title_rect.y, slot, TITLE);
        let color = match (style.glyphs, active, *name) {
            (Glyphs::Circles, true, "close") => Palette::circle(0),
            (Glyphs::Circles, true, "iconify") => Palette::circle(1),
            (Glyphs::Circles, true, _) => Palette::circle(2),
            (_, true, _) => palette.button_active,
            (_, false, _) => palette.button_inactive,
        };
        draw_glyph(canvas, cell, name, style.glyphs, color);
        x += slot + gap;
    }
    let (text_x, text_w, align) = match (style.glyphs, style.buttons) {
        (Glyphs::Wide, ButtonSide::Right) => (title_rect.x + 8, title_rect.w - total - 14, Align::Left),
        (Glyphs::Wide, ButtonSide::Left) => {
            (title_rect.x + total + 10, title_rect.w - total - 14, Align::Left)
        }
        _ => (
            title_rect.x + total + 4,
            title_rect.w - 2 * total - 8,
            Align::Center,
        ),
    };
    ui::label(
        canvas,
        &mut d.fonts,
        title,
        Rect::new(text_x, title_rect.y, text_w, TITLE),
        FONT,
        false,
        align,
        label,
    );
}

/// Draws a window button's bitmap centered in a cell, pixel by pixel.
fn draw_glyph(canvas: &mut Canvas<'_>, cell: Rect, name: &str, glyphs: Glyphs, color: Color) {
    let bits = wmtheme::bitmap(name, glyphs);
    let x0 = cell.x + (cell.w - GLYPH) / 2;
    let y0 = cell.y + (cell.h - GLYPH) / 2;
    if glyphs == Glyphs::Circles {
        canvas.fill_rounded(
            Rect::new(x0 + 1, y0 + 1, GLYPH - 2, GLYPH - 2),
            (GLYPH - 2) as f32 / 2.0,
            color,
        );
        return;
    }
    for (y, row) in bits.iter().enumerate() {
        for (x, bit) in row.iter().enumerate() {
            if *bit {
                canvas.fill_rect(Rect::new(x0 + x as i32, y0 + y as i32, 1, 1), color);
            }
        }
    }
}

/// Fills the back window with a terminal: dark with a prompt line.
fn draw_terminal(canvas: &mut Canvas<'_>, rect: Rect) {
    let body = Rect::new(rect.x + 1, rect.y + TITLE, rect.w - 2, rect.h - TITLE - 1);
    canvas.fill_rect(body, crate::render::rgb(0x1c, 0x1c, 0x1c));
    canvas.fill_rect(
        Rect::new(body.x + 6, body.y + 6, 34, 2),
        crate::render::rgb(0x8f, 0xd1, 0x7a),
    );
    canvas.fill_rect(
        Rect::new(body.x + 44, body.y + 4, 4, 7),
        crate::render::rgb(0xd0, 0xd0, 0xd0),
    );
}

/// Fills the front window with the file manager: toolbar, places and a
/// grid of folders.
fn draw_files(d: &mut Desktop, canvas: &mut Canvas<'_>, rect: Rect) {
    let body = Rect::new(rect.x + 1, rect.y + TITLE, rect.w - 2, rect.h - TITLE - 1);
    let toolbar = Rect::new(body.x, body.y, body.w, 18);
    canvas.fill_rect(
        Rect::new(toolbar.x, toolbar.bottom() - 1, toolbar.w, 1),
        ui::separator(),
    );
    for i in 0..2 {
        canvas.fill_rounded(
            Rect::new(toolbar.x + 5 + i * 14, toolbar.y + 4, 10, 10),
            3.0,
            ui::field_bg(),
        );
    }
    let field = Rect::new(toolbar.x + 36, toolbar.y + 3, toolbar.w - 44, 12);
    canvas.fill_rounded(field, 4.0, ui::field_bg());
    ui::label(
        canvas,
        &mut d.fonts,
        tr("Home"),
        Rect::new(field.x + 5, field.y, field.w - 10, field.h),
        FONT,
        false,
        Align::Left,
        ui::text_dim(),
    );
    let sidebar = Rect::new(body.x, toolbar.bottom(), 50, body.h - toolbar.h);
    canvas.fill_rect(sidebar, crate::render::rgba(0, 0, 0, 0x30));
    for (i, place) in [tr("Home"), tr("Desktop"), tr("Filesystem")].iter().enumerate() {
        let row = Rect::new(sidebar.x + 3, sidebar.y + 4 + i as i32 * ROW, sidebar.w - 6, ROW);
        if i == 0 {
            canvas.fill_rounded(row, 3.0, ui::active_bg());
        }
        ui::label(
            canvas,
            &mut d.fonts,
            place,
            Rect::new(row.x + 4, row.y, row.w - 8, row.h),
            FONT,
            false,
            Align::Left,
            ui::text(),
        );
    }
    let grid = Rect::new(
        sidebar.right(),
        sidebar.y,
        body.right() - sidebar.right(),
        sidebar.h,
    );
    let (cell_w, cell_h) = (46, 44);
    let columns = (grid.w / cell_w).max(1);
    for (i, name) in FOLDERS.iter().enumerate() {
        let (col, row) = (i as i32 % columns, i as i32 / columns);
        let cell = Rect::new(
            grid.x + 2 + col * cell_w,
            grid.y + 4 + row * cell_h,
            cell_w,
            cell_h,
        );
        if cell.bottom() > grid.bottom() {
            break;
        }
        if i == 1 {
            canvas.fill_rounded(cell.inflate(-2), 4.0, ui::accent_alpha(0x55));
        }
        let folder = Rect::new(cell.x + (cell.w - 22) / 2, cell.y + 4, 22, 17);
        canvas.fill_rounded(folder, 2.0, ui::blend(ui::accent(), ui::text(), 0.35));
        canvas.fill_rounded(
            Rect::new(folder.x, folder.y, 9, 4),
            1.5,
            ui::blend(ui::accent(), ui::text(), 0.5),
        );
        ui::label(
            canvas,
            &mut d.fonts,
            name,
            Rect::new(cell.x, folder.bottom() + 1, cell.w, ROW),
            FONT,
            false,
            Align::Center,
            ui::text(),
        );
    }
    let status = Rect::new(body.x + sidebar.w, body.bottom() - 12, body.w - sidebar.w, 12);
    canvas.fill_rect(Rect::new(status.x, status.y, status.w, 1), ui::separator());
}

/// Draws the start menu open in its layout: the search box, then the
/// categories and application rows, the categories and a grid of icons,
/// or the application list with the favorites' tiles.
fn draw_menu(d: &mut Desktop, canvas: &mut Canvas<'_>, rect: Rect, layout: MenuLayout) {
    let radius = (ui::popup_radius() * 0.75).round();
    ui::fill_surface(canvas, rect, radius, ui::popup_bg());
    canvas.stroke_rounded(rect, radius, 1.0, ui::popup_border());
    let search = Rect::new(rect.x + 5, rect.y + 5, rect.w - 10, 14);
    canvas.fill_rounded(search, 4.0, ui::field_bg());
    ui::label(
        canvas,
        &mut d.fonts,
        tr("Search applications"),
        Rect::new(search.x + 6, search.y, search.w - 12, search.h),
        FONT,
        false,
        Align::Left,
        ui::text_dim(),
    );
    let columns_y = search.bottom() + 5;
    let content_h = rect.bottom() - columns_y - 16;
    let left_w = if layout == MenuLayout::Tiles { 64 } else { 56 };
    let left = Rect::new(rect.x + 5, columns_y, left_w, content_h);
    let left_rows: Vec<&str> = match layout {
        MenuLayout::Tiles => APPS.to_vec(),
        _ => vec![tr("Favorites"), tr("Recent"), tr("All")],
    };
    for (i, name) in left_rows.iter().enumerate() {
        let row = Rect::new(left.x, left.y + i as i32 * ROW, left.w, ROW);
        if row.bottom() > left.bottom() {
            break;
        }
        let marked = i == 0 && layout != MenuLayout::Tiles;
        if marked {
            canvas.fill_rounded(row, 3.0, ui::active_bg());
        }
        let mut text_x = row.x + 4;
        if layout == MenuLayout::Tiles {
            canvas.fill_rounded(Rect::new(row.x + 2, row.y + 2, 9, 9), 2.0, ui::accent_alpha(0x90));
            text_x = row.x + 14;
        }
        ui::label(
            canvas,
            &mut d.fonts,
            name,
            Rect::new(text_x, row.y, row.right() - text_x - 2, row.h),
            FONT,
            marked,
            Align::Left,
            ui::text(),
        );
    }
    let right = Rect::new(
        left.right() + 5,
        columns_y,
        rect.right() - 5 - left.right() - 5,
        content_h,
    );
    match layout {
        MenuLayout::Columns => {
            for (i, name) in APPS.iter().enumerate() {
                let row = Rect::new(right.x, right.y + i as i32 * ROW, right.w, ROW);
                if row.bottom() > right.bottom() {
                    break;
                }
                if i == 1 {
                    canvas.fill_rounded(row, 3.0, ui::hover_bg());
                }
                canvas.fill_rounded(
                    Rect::new(row.x + 3, row.y + 2, 9, 9),
                    2.0,
                    ui::accent_alpha(if i == 0 { 0xff } else { 0x70 }),
                );
                ui::label(
                    canvas,
                    &mut d.fonts,
                    name,
                    Rect::new(row.x + 16, row.y, row.w - 18, row.h),
                    FONT,
                    false,
                    Align::Left,
                    ui::text(),
                );
            }
        }
        MenuLayout::Grid => {
            let (cell_w, cell_h) = (28, 30);
            let columns = (right.w / cell_w).max(1);
            for (i, name) in APPS.iter().enumerate() {
                let (col, row) = (i as i32 % columns, i as i32 / columns);
                let cell = Rect::new(right.x + col * cell_w, right.y + row * cell_h, cell_w, cell_h);
                if cell.bottom() > right.bottom() {
                    break;
                }
                if i == 1 {
                    canvas.fill_rounded(cell, 3.0, ui::hover_bg());
                }
                canvas.fill_rounded(
                    Rect::new(cell.x + (cell.w - 14) / 2, cell.y + 3, 14, 14),
                    3.0,
                    ui::accent_alpha(if i == 0 { 0xff } else { 0x70 }),
                );
                ui::label(
                    canvas,
                    &mut d.fonts,
                    name,
                    Rect::new(cell.x, cell.y + 18, cell.w, 10),
                    FONT - 1.5,
                    false,
                    Align::Center,
                    ui::text(),
                );
            }
        }
        MenuLayout::Tiles => {
            let tile = 30;
            let columns = ((right.w + 3) / (tile + 3)).max(1);
            for (i, name) in APPS.iter().take(4).enumerate() {
                let (col, row) = (i as i32 % columns, i as i32 / columns);
                let cell = Rect::new(right.x + col * (tile + 3), right.y + row * (tile + 3), tile, tile);
                if cell.bottom() > right.bottom() {
                    break;
                }
                canvas.fill_rounded(cell, (radius / 2.0).min(3.0), ui::accent_alpha(0xd8));
                canvas.fill_rounded(
                    Rect::new(cell.x + (cell.w - 12) / 2, cell.y + 4, 12, 12),
                    3.0,
                    ui::on_accent(),
                );
                ui::label(
                    canvas,
                    &mut d.fonts,
                    name,
                    Rect::new(cell.x, cell.bottom() - 11, cell.w, 10),
                    FONT - 1.5,
                    false,
                    Align::Center,
                    ui::on_accent(),
                );
            }
        }
    }
    canvas.fill_rect(
        Rect::new(rect.x + 5, rect.bottom() - 13, rect.w - 10, 1),
        ui::separator(),
    );
}

/// Draws the bar along the configured edge in the look's layout, with the
/// start button, two taskbar buttons and the clock.
///
/// # Arguments
/// * `d` - The desktop.
/// * `canvas` - The canvas painted.
/// * `screen` - The preview's screen.
/// * `look` - The look drawn.
/// * `height` - The bar's height in the preview.
/// * `margin` - The bar's margin from the screen edges in the preview.
fn draw_bar(d: &mut Desktop, canvas: &mut Canvas<'_>, screen: Rect, look: &Look, height: i32, margin: i32) {
    let corners = look.bar_corners;
    let usable = screen.w - 2 * margin;
    let width = (usable * d.config.panel_width / 100).max(60);
    let x = screen.x + margin + (usable - width) / 2;
    let bar = if d.panel_top() {
        Rect::new(x, screen.y + margin, width, height)
    } else {
        Rect::new(x, screen.bottom() - height - margin, width, height)
    };
    let bg = ui::panel_bg(d.config.panel_opacity);
    if margin > 0 {
        let radius = if corners == 0 {
            0.0
        } else {
            (corners as f32 * 0.75).round() + 1.0
        };
        ui::fill_surface(canvas, bar, radius, bg);
        canvas.stroke_rounded(bar, radius, 1.0, ui::separator());
    } else if width < screen.w {
        let radius = if corners == 0 {
            0.0
        } else {
            (corners as f32 * 0.75).round() + 1.0
        };
        let overflow = radius as i32;
        let shape = if d.panel_top() {
            Rect::new(bar.x, bar.y - overflow, bar.w, bar.h + overflow)
        } else {
            Rect::new(bar.x, bar.y, bar.w, bar.h + overflow)
        };
        ui::fill_surface(canvas, shape, radius, bg);
        canvas.stroke_rounded(shape, radius, 1.0, ui::separator());
    } else {
        ui::fill_surface(canvas, bar, 0.0, bg);
        let edge_y = if d.panel_top() { bar.bottom() - 1 } else { bar.y };
        canvas.fill_rect(Rect::new(bar.x, edge_y, bar.w, 1), ui::separator());
    }
    let label = d.config.start_label.clone();
    let label_w = label.as_ref().map_or(0, |text| {
        ui::text_width(&mut d.fonts, text, FONT, true, d.panel.scale) + 4
    });
    let group_w = 24 + label_w + 4 + 2 * 57;
    let x0 = if look.centered {
        (bar.x + (bar.w - group_w) / 2).clamp(bar.x + 3, (bar.right() - 56 - group_w).max(bar.x + 3))
    } else {
        bar.x + 3
    };
    let start = Rect::new(x0, bar.y + 2, 24 + label_w, height - 4);
    canvas.fill_rounded(start, 3.0, ui::active_bg());
    if let Some(icon) = d.start_icon(14) {
        let icon_y = start.y + (start.h - 14) / 2;
        canvas.draw_image_fit(&icon, Rect::new(start.x + 5, icon_y, 14, 14));
    }
    if let Some(text) = label {
        ui::label(
            canvas,
            &mut d.fonts,
            &text,
            Rect::new(start.x + 24, start.y, label_w, start.h),
            FONT,
            true,
            Align::Left,
            ui::text(),
        );
    }
    let mut task_x = start.right() + 4;
    for (i, name) in [tr("Files"), tr("Window")].iter().enumerate() {
        let task = Rect::new(task_x, bar.y + 2, 54, height - 4);
        canvas.fill_rounded(task, 3.0, if i == 0 { ui::active_bg() } else { ui::field_bg() });
        if i == 0 {
            canvas.fill_rounded(
                Rect::new(task.x + 4, task.bottom() - 2, task.w - 8, 1),
                0.5,
                ui::accent(),
            );
        }
        ui::label(
            canvas,
            &mut d.fonts,
            name,
            Rect::new(task.x + 4, task.y, task.w - 8, task.h),
            FONT,
            false,
            Align::Left,
            ui::text(),
        );
        task_x += task.w + 3;
    }
    let clock = if d.clock.is_empty() {
        "12:00".to_string()
    } else {
        d.clock.clone()
    };
    ui::label(
        canvas,
        &mut d.fonts,
        &clock,
        Rect::new(bar.right() - 50, bar.y, 44, bar.h),
        FONT,
        false,
        Align::Right,
        ui::text(),
    );
}
