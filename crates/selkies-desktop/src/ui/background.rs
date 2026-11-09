//! The wallpaper layers and the launchers placed on the desktop.  Every
//! monitor gets the wallpaper, or the gradient with the logo, drawn for its
//! own rectangle; the launchers sit on the primary monitor.

use std::sync::Arc;

use tiny_skia::Pixmap;

use crate::config::{BackgroundStyle, Wallpaper, WallpaperMode};
use crate::desktop::{Desktop, Hover};
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

/// Rows of launcher cells that fit in the icon area.
fn rows(area: Rect) -> i32 {
    ((area.h - MARGIN) / ui::DESKTOP_CELL_HEIGHT).max(1)
}

/// The cell of a launcher, in the primary background's coordinates.  Cells
/// fill columns top to bottom and wrap to the next column above the bar.
///
/// # Arguments
/// * `d` - The desktop.
/// * `index` - Index into the desktop launcher list.
pub fn cell_rect(d: &Desktop, index: usize) -> Rect {
    let area = icon_area(d);
    let rows = rows(area);
    let index = index as i32;
    Rect::new(
        area.x + (index / rows) * ui::DESKTOP_CELL_WIDTH + MARGIN,
        area.y + (index % rows) * ui::DESKTOP_CELL_HEIGHT + MARGIN,
        ui::DESKTOP_CELL_WIDTH,
        ui::DESKTOP_CELL_HEIGHT,
    )
}

/// Every launcher cell, which is the primary background's input region.
///
/// # Arguments
/// * `d` - The desktop.
pub fn cells(d: &Desktop) -> Vec<Rect> {
    (0..d.apps.desktop.len()).map(|i| cell_rect(d, i)).collect()
}

/// The launcher under a point of a background surface; only the primary one
/// carries launchers.
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
    (0..d.apps.desktop.len()).find(|i| cell_rect(d, *i).contains(x, y))
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
            layer_canvas.fill_rect(whole, d.config.wallpaper.color);
            for (monitor_index, monitor) in monitors.iter().enumerate() {
                let number = d.monitor_number(id, monitor_index);
                let wallpaper = d.config.wallpaper_for(number);
                let image = wallpaper.image.as_ref().and_then(|p| d.wallpaper_image(p));
                draw_wallpaper(
                    &mut layer_canvas,
                    monitor.intersect(&whole),
                    &wallpaper,
                    image.as_deref(),
                );
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
    for index in 0..d.apps.desktop.len() {
        let cell = cell_rect(d, index);
        let selected = d.desktop_selection == Some(index);
        let hovered = d.hover == Hover::DesktopIcon(index);
        if selected {
            canvas.fill_rounded(cell, 8.0, ui::accent_alpha(0x55));
            canvas.stroke_rounded(cell, 8.0, 1.0, ui::accent_alpha(0x90));
        } else if hovered {
            canvas.fill_rounded(cell, 8.0, rgba(0xff, 0xff, 0xff, 0x22));
        }
        let icon_rect = Rect::new(
            cell.x + (cell.w - ui::DESKTOP_ICON_SIZE) / 2,
            cell.y + 6,
            ui::DESKTOP_ICON_SIZE,
            ui::DESKTOP_ICON_SIZE,
        );
        match d.desktop_icon(index) {
            Some(icon) => canvas.draw_image_fit(&icon, icon_rect),
            None => {
                canvas.fill_rounded(icon_rect.inflate(-6), 6.0, rgba(0xff, 0xff, 0xff, 0x30));
            }
        }
        let name = d.apps.desktop[index].name.clone();
        let max_width = (cell.w - 6) as f32 * scale;
        let (line1, line2) = ui::wrap_two_lines(&mut d.fonts, &name, font_px, max_width);
        let text_top = icon_rect.bottom() + 4;
        let line_h = 16;
        draw_shadowed(canvas, &mut d.fonts, &line1, cell, text_top + line_h / 2);
        if let Some(line2) = line2 {
            draw_shadowed(canvas, &mut d.fonts, &line2, cell, text_top + line_h + line_h / 2);
        }
    }
}

/// Draws a label with a contrasting shadow so it reads on any wallpaper.
fn draw_shadowed(canvas: &mut Canvas<'_>, fonts: &mut Fonts, text: &Arc<Text>, cell: Rect, center_y: i32) {
    canvas.draw_text(
        fonts,
        text,
        cell.x + 1,
        center_y + 1,
        cell.w,
        Align::Center,
        ui::text_shadow(),
    );
    canvas.draw_text(fonts, text, cell.x, center_y, cell.w, Align::Center, ui::text());
}

/// Paints one monitor: the solid color or the dithered gradient, and the
/// image over it in its mode.  The gradient is dithered
/// because the stream that carries the desktop is 8-bit 4:2:0 video, where a
/// smooth ramp quantizes into visible bands.
fn draw_wallpaper(canvas: &mut Canvas<'_>, area: Rect, wallpaper: &Wallpaper, image: Option<&Pixmap>) {
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
    if let Some(image) = image {
        canvas.clip(area);
        match wallpaper.mode {
            WallpaperMode::Fill => canvas.draw_image_cover(image, area),
            WallpaperMode::Fit => canvas.draw_image_fit(image, area),
            WallpaperMode::Stretch => canvas.draw_image_stretch(image, area),
            WallpaperMode::Center => {
                let w = (image.width() / canvas.scale() as u32) as i32;
                let h = (image.height() / canvas.scale() as u32) as i32;
                let rect = Rect::new(area.x + (area.w - w) / 2, area.y + (area.h - h) / 2, w, h);
                canvas.draw_image_fit(image, rect);
            }
        }
        canvas.reset_clip();
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
            working_dir: None,
            category: String::new(),
            wm_class: None,
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
        d.apps.desktop = vec![launcher("one")];
        configure(&mut d, 7, true);
        let (wallpaper, launchers) = d
            .render_background_layers(7)
            .expect("a configured background renders");
        assert!(wallpaper.input.is_empty(), "the wallpaper takes no input");
        assert_eq!(
            launchers.input.len(),
            1,
            "the launcher layer takes input on its cell"
        );
        let cell = launchers.input[0];
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
        d.apps.desktop = vec![launcher("one")];
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
