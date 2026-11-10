//! The wallpaper layers and the launchers placed on the desktop.  Every
//! monitor gets the wallpaper, or the gradient with the logo, drawn for its
//! own rectangle; the launchers sit on the primary monitor.

use std::sync::Arc;

use tiny_skia::Pixmap;

use crate::config::{BackgroundStyle, Wallpaper, WallpaperMode};
use crate::desktop::{Desktop, Hover};
use crate::desktop_folder::{self, Cell};
use crate::geom::Rect;
use crate::render::{Align, Canvas, Fonts, Text, rgba};
use crate::ui;

/// Distance of the first cell from the monitor edges.
const MARGIN: i32 = 10;

/// The monitor rectangle the launchers are laid out in, on the primary
/// background surface, less the strip the bar takes.
fn icon_area(d: &Desktop) -> Rect {
    let area = d.primary_background().map(|b| b.icon_area()).unwrap_or_default();
    let bar = d.config.panel_height;
    if d.panel_top() {
        Rect::new(area.x, area.y + bar, area.w, area.h - bar)
    } else {
        Rect::new(area.x, area.y, area.w, area.h - bar)
    }
}

/// Rows of icon cells that fit in the icon area.
fn rows(area: Rect) -> i32 {
    ((area.h - MARGIN) / ui::DESKTOP_CELL_HEIGHT).max(1)
}

/// Columns of icon cells that fit in the icon area.
fn columns(area: Rect) -> i32 {
    ((area.w - MARGIN) / ui::DESKTOP_CELL_WIDTH).max(1)
}

/// The rectangle of a grid cell, in the primary background's coordinates.
fn cell_rect_in(area: Rect, (column, row): Cell) -> Rect {
    Rect::new(
        area.x + column * ui::DESKTOP_CELL_WIDTH + MARGIN,
        area.y + row * ui::DESKTOP_CELL_HEIGHT + MARGIN,
        ui::DESKTOP_CELL_WIDTH,
        ui::DESKTOP_CELL_HEIGHT,
    )
}

/// The grid cell each Desktop item stands on, as [`desktop_folder::place`] lays them out on
/// the grid that fits the screen now.
///
/// # Arguments
/// * `d` - The desktop.
pub fn item_cells(d: &Desktop) -> Vec<Cell> {
    let area = icon_area(d);
    desktop_folder::place(&d.desktop_items, &d.desktop_layout, columns(area), rows(area))
}

/// The grid cell under a point, `None` off the grid.
///
/// # Arguments
/// * `d` - The desktop.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
pub fn cell_at(d: &Desktop, x: f64, y: f64) -> Option<Cell> {
    let area = icon_area(d);
    let column = ((x - (area.x + MARGIN) as f64) / ui::DESKTOP_CELL_WIDTH as f64).floor() as i32;
    let row = ((y - (area.y + MARGIN) as f64) / ui::DESKTOP_CELL_HEIGHT as f64).floor() as i32;
    ((0..columns(area)).contains(&column) && (0..rows(area)).contains(&row)).then_some((column, row))
}

/// The rectangle of each Desktop item's cell, in the primary background's coordinates.
///
/// # Arguments
/// * `d` - The desktop.
pub fn cells(d: &Desktop) -> Vec<Rect> {
    let area = icon_area(d);
    item_cells(d)
        .into_iter()
        .map(|cell| cell_rect_in(area, cell))
        .collect()
}

/// The Desktop item under a point of a background surface; only the primary one carries
/// them.
///
/// # Arguments
/// * `d` - The desktop.
/// * `id` - The background surface.
/// * `x` - Pointer X.
/// * `y` - Pointer Y.
pub fn hit(d: &Desktop, id: u32, x: f64, y: f64) -> Option<usize> {
    if !d.background(id).is_some_and(|b| b.primary) {
        return None;
    }
    cells(d).iter().position(|rect| rect.contains(x, y))
}

/// Draws a background surface: the wallpaper of each of its monitors, then
/// the launchers when it is the primary one.
///
/// # Arguments
/// * `d` - The desktop.
/// * `id` - The background surface.
/// * `canvas` - The surface's canvas.
pub fn draw(d: &mut Desktop, id: u32, canvas: &mut Canvas<'_>) {
    draw_wallpaper_layer(d, id, canvas);
    draw_launchers(d, id, canvas);
}

/// Draws the wallpaper of each monitor a background surface spans.
///
/// # Arguments
/// * `d` - The desktop.
/// * `id` - The background surface.
/// * `canvas` - The surface's canvas.
pub fn draw_wallpaper_layer(d: &mut Desktop, id: u32, canvas: &mut Canvas<'_>) {
    let Some(index) = d.backgrounds.iter().position(|b| b.id == id) else {
        return;
    };
    let geometry = d.backgrounds[index].geometry;
    let whole = Rect::new(0, 0, geometry.width, geometry.height);
    let monitors = d.backgrounds[index].monitors.clone();
    let key = (geometry.width, geometry.height, geometry.scale, monitors.clone());
    if d.backgrounds[index]
        .wallpaper_cache
        .as_ref()
        .is_none_or(|(k, _)| *k != key)
    {
        let mut layer = Pixmap::new(
            canvas.width() as u32 * canvas.scale() as u32,
            canvas.height() as u32 * canvas.scale() as u32,
        );
        if let Some(layer) = layer.as_mut() {
            let mut layer_canvas = Canvas::new(layer.as_mut(), canvas.scale());
            let physical = |rect: Rect| {
                let scale = geometry.scale.max(1);
                ((rect.w * scale).max(1) as u32, (rect.h * scale).max(1) as u32)
            };
            let shared = d.painted_wallpaper(d.config.wallpaper.clone(), physical(whole));
            layer_canvas.fill_rect(whole, shared.color);
            for (monitor_index, monitor) in monitors.iter().enumerate() {
                let number = d.monitor_number(id, monitor_index);
                let wallpaper = d.painted_wallpaper(d.config.wallpaper_for(number), physical(*monitor));
                let image = wallpaper.image.as_ref().and_then(|p| d.wallpaper_image(p));
                let scale = layer_canvas.scale().max(1) as u32;
                let image = image.as_deref().map(|pixels| WallpaperImage {
                    pixels,
                    native: ((pixels.width() / scale) as i32, (pixels.height() / scale) as i32),
                });
                paint_wallpaper(&mut layer_canvas, monitor.intersect(&whole), &wallpaper, image);
            }
        }
        d.backgrounds[index].wallpaper_cache = layer.map(|l| (key, l));
    }
    if let Some((_, layer)) = &d.backgrounds[index].wallpaper_cache {
        canvas.draw_image_stretch(layer, whole);
    }
}

/// Draws the launchers over what a canvas already holds, on the primary
/// background surface only.
///
/// # Arguments
/// * `d` - The desktop.
/// * `id` - The background surface.
/// * `canvas` - The surface's canvas.
pub fn draw_launchers(d: &mut Desktop, id: u32, canvas: &mut Canvas<'_>) {
    if !d.background(id).is_some_and(|b| b.primary) {
        return;
    }

    let scale = canvas.scale() as f32;
    let font_px = ui::FONT_SMALL * scale;
    let line_h = 16;
    let rects = cells(d);
    let dragging = d.desktop_press.filter(|press| press.dragging);
    if let Some(press) = dragging
        && let Some(target) = cell_at(d, press.at.0, press.at.1)
    {
        let target = cell_rect_in(icon_area(d), target);
        canvas.fill_rounded(target, 8.0, rgba(0xff, 0xff, 0xff, 0x18));
        canvas.stroke_rounded(target, 8.0, 3.0, ui::accent_alpha(0xc0));
    }
    for (index, &home) in rects.iter().enumerate() {
        let cell = match dragging {
            Some(press) if press.index == index => Rect::new(
                home.x + (press.at.0 - press.start.0) as i32,
                home.y + (press.at.1 - press.start.1) as i32,
                home.w,
                home.h,
            ),
            _ => home,
        };
        let selected = d.desktop_selection == Some(index);
        let hovered = d.hover == Hover::DesktopIcon(index);
        let name = d.desktop_items[index].name.clone();
        let max_width = (cell.w - 6) as f32 * scale;
        let (line1, line2) = ui::wrap_two_lines(&mut d.fonts, &name, font_px, max_width);
        let icon_rect = Rect::new(
            cell.x + (cell.w - ui::DESKTOP_ICON_SIZE) / 2,
            cell.y + 6,
            ui::DESKTOP_ICON_SIZE,
            ui::DESKTOP_ICON_SIZE,
        );
        let text_top = icon_rect.bottom() + 4;
        let lines = if line2.is_some() { 2 } else { 1 };
        // The highlight hugs the icon and its label; the rest of the cell is the gap between
        // launchers.
        let highlight = Rect::new(cell.x, cell.y, cell.w, text_top + lines * line_h + 6 - cell.y);
        if selected {
            canvas.fill_rounded(highlight, 8.0, ui::accent_alpha(0x55));
            canvas.stroke_rounded(highlight, 8.0, 1.0, ui::accent_alpha(0x90));
        } else if hovered {
            canvas.fill_rounded(highlight, 8.0, rgba(0xff, 0xff, 0xff, 0x22));
        }
        match d.desktop_icon(index) {
            Some(icon) => canvas.draw_image_fit(&icon, icon_rect),
            None => {
                canvas.fill_rounded(icon_rect.inflate(-6), 6.0, rgba(0xff, 0xff, 0xff, 0x30));
            }
        }
        draw_shadowed(canvas, &mut d.fonts, &line1, cell, text_top + line_h / 2);
        if let Some(line2) = line2 {
            draw_shadowed(canvas, &mut d.fonts, &line2, cell, text_top + line_h + line_h / 2);
        }
    }
}

/// Draws a label white over a dark shadow so it reads on any wallpaper.
///
/// The colors are the wallpaper's rather than the scheme's: a light scheme's dark text would
/// vanish into a dark wallpaper, and the shadow keeps white legible on a light one.
fn draw_shadowed(canvas: &mut Canvas<'_>, fonts: &mut Fonts, text: &Arc<Text>, cell: Rect, center_y: i32) {
    canvas.draw_text(
        fonts,
        text,
        cell.x + 1,
        center_y + 1,
        cell.w,
        Align::Center,
        rgba(0, 0, 0, 0xc0),
    );
    canvas.draw_text(
        fonts,
        text,
        cell.x,
        center_y,
        cell.w,
        Align::Center,
        rgba(0xff, 0xff, 0xff, 0xff),
    );
}

/// A wallpaper image ready to paint.
#[derive(Clone, Copy)]
pub struct WallpaperImage<'a> {
    /// The pixels drawn, the decoded file or a copy shrunk for a preview.
    pub pixels: &'a Pixmap,
    /// The logical size the image covers at its own resolution on the
    /// monitor painted, which the center mode shows it at: its pixels over
    /// the monitor's scale on the desktop, that shrunk by the preview's
    /// ratio to the monitor in a preview.
    pub native: (i32, i32),
}

/// The logical size an image is drawn at in an area in a mode, for sizing
/// a preview's shrunk copy of it.
///
/// # Arguments
/// * `mode` - How the image is placed.
/// * `native` - The size the image covers at its own resolution, as in
///   [`WallpaperImage::native`].
/// * `area` - The area painted.
pub fn drawn_size(mode: WallpaperMode, native: (f32, f32), area: Rect) -> (f32, f32) {
    let (w, h) = (native.0.max(1.0), native.1.max(1.0));
    let (aw, ah) = (area.w as f32, area.h as f32);
    match mode {
        WallpaperMode::Fill => {
            let ratio = (aw / w).max(ah / h);
            (w * ratio, h * ratio)
        }
        WallpaperMode::Fit => {
            let ratio = (aw / w).min(ah / h);
            (w * ratio, h * ratio)
        }
        WallpaperMode::Stretch => (aw, ah),
        WallpaperMode::Center => (w, h),
    }
}

/// Paints one monitor: the solid color or the dithered gradient, and the
/// image over it in its mode.  The desktop and the settings window's
/// previews both paint through here, so a preview shows what the desktop
/// draws.  The gradient is dithered because the stream that carries the
/// desktop is 8-bit 4:2:0 video, where a smooth ramp quantizes into visible
/// bands.
///
/// # Arguments
/// * `canvas` - The canvas painted.
/// * `area` - The monitor's rectangle on it.
/// * `wallpaper` - The monitor's wallpaper settings.
/// * `image` - The wallpaper's image, when it has one that loaded.
pub fn paint_wallpaper(
    canvas: &mut Canvas<'_>,
    area: Rect,
    wallpaper: &Wallpaper,
    image: Option<WallpaperImage<'_>>,
) {
    if area.is_empty() {
        return;
    }
    match wallpaper.style {
        BackgroundStyle::Solid => canvas.fill_rect(area, wallpaper.color),
        BackgroundStyle::Gradient => {
            let base = wallpaper.color.to_color_u8();
            let lighten = |c: u8, by: i32| (c as i32 + by).clamp(0, 255) as u8;
            let top = tiny_skia::Color::from_rgba8(
                lighten(base.red(), 26),
                lighten(base.green(), 28),
                lighten(base.blue(), 34),
                255,
            );
            let bottom = tiny_skia::Color::from_rgba8(
                lighten(base.red(), -8),
                lighten(base.green(), -8),
                lighten(base.blue(), -6),
                255,
            );
            canvas.fill_dithered_gradient(area, top, bottom);
        }
    }
    if let Some(WallpaperImage { pixels, native }) = image {
        canvas.clipped(area, |canvas| match wallpaper.mode {
            WallpaperMode::Fill => canvas.draw_image_cover(pixels, area),
            WallpaperMode::Fit => canvas.draw_image_fit(pixels, area),
            WallpaperMode::Stretch => canvas.draw_image_stretch(pixels, area),
            WallpaperMode::Center => {
                let (w, h) = native;
                let rect = Rect::new(area.x + (area.w - w) / 2, area.y + (area.h - h) / 2, w, h);
                canvas.draw_image_fit(pixels, rect);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::entries::Entry;
    use crate::event::{Event, SurfaceId};

    fn launcher(name: &str) -> Entry {
        Entry {
            id: format!("{name}.desktop"),
            name: name.to_string(),
            comment: None,
            generic_name: None,
            keywords: Vec::new(),
            exec: name.to_string(),
            icon: None,
            path: PathBuf::from(format!("/nonexistent/{name}.desktop")),
            terminal: false,
            emulator: false,
            working_dir: None,
            category: String::new(),
            wm_class: None,
        }
    }

    fn item(name: &str) -> desktop_folder::DesktopItem {
        let entry = launcher(name);
        desktop_folder::DesktopItem {
            file: format!("{name}.desktop"),
            name: name.to_string(),
            item: crate::files::Item {
                name: format!("{name}.desktop"),
                path: entry.path.clone(),
                is_dir: false,
                is_symlink: false,
                size: 0,
                modified: 0,
                mime: "application/x-desktop".to_string(),
                hidden: false,
                executable: false,
            },
            launcher: Some(entry),
        }
    }

    fn configure(d: &mut Desktop, id: u32, primary: bool) {
        d.handle(Event::Configured {
            surface: SurfaceId::Background(id),
            width: 320,
            height: 240,
            scale: 1,
        });
        d.handle(Event::BackgroundLayout {
            surface: id,
            monitors: vec![Rect::new(0, 0, 320, 240)],
            primary,
        });
    }

    fn alpha(pixmap: &Pixmap, x: i32, y: i32) -> u8 {
        pixmap.pixel(x as u32, y as u32).map_or(0, |p| p.alpha())
    }

    #[test]
    fn layers_split_the_wallpaper_from_the_launchers() {
        let mut d = Desktop::load(false);
        d.desktop_items = vec![item("one")];
        d.desktop_layout.clear();
        configure(&mut d, 7, true);
        let (wallpaper, launchers) = d
            .render_background_layers(7)
            .expect("a configured background renders");
        assert!(wallpaper.input.is_empty(), "the wallpaper takes no input");
        assert_eq!(
            launchers.input,
            vec![Rect::new(0, 0, 320, 240)],
            "the launcher layer takes input over the whole desktop, so a click on an empty spot reaches the shell"
        );
        assert_eq!(launchers.visible.len(), 1, "the launcher layer draws its cell");
        let cell = launchers.visible[0];
        let outside = (cell.right() + 20, cell.bottom() + 20);
        assert_eq!(
            alpha(launchers.pixmap, outside.0, outside.1),
            0,
            "transparent outside the cells"
        );
        assert_eq!(
            alpha(wallpaper.pixmap, outside.0, outside.1),
            255,
            "the wallpaper is opaque"
        );
        let painted = (cell.x..cell.right())
            .flat_map(|x| (cell.y..cell.bottom()).map(move |y| (x, y)))
            .any(|(x, y)| alpha(launchers.pixmap, x, y) > 0);
        assert!(painted, "the launcher is drawn in its cell");
    }

    #[test]
    fn a_secondary_background_has_an_empty_launcher_layer_and_no_redraw_without_change() {
        let mut d = Desktop::load(false);
        d.desktop_items = vec![item("one")];
        d.desktop_layout.clear();
        configure(&mut d, 1, true);
        configure(&mut d, 2, false);
        {
            let (_, launchers) = d
                .render_background_layers(2)
                .expect("a configured background renders");
            assert!(launchers.input.is_empty());
            assert!(launchers.pixmap.pixels().iter().all(|p| p.alpha() == 0));
        }
        assert!(
            d.render_background_layers(2).is_none(),
            "nothing changed, nothing to render"
        );
    }
}
