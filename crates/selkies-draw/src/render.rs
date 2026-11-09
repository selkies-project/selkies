/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Software rendering: a canvas over a tiny-skia pixmap for shapes and images,
//! and text shaped and rasterized by cosmic-text from the system fonts.
//!
//! Every drawing call takes logical coordinates; the canvas multiplies them by
//! the output scale, so HiDPI outputs get crisp rendering without the UI code
//! knowing about it.  The pixel format is tiny-skia's premultiplied RGBA, which
//! the backends swizzle into what their display server expects.

use std::collections::HashMap;
use std::sync::Arc;

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache, SwashContent, Weight};
use tiny_skia::{
    Color, FillRule, FilterQuality, IntRect, Mask, Paint, Path, PathBuilder, Pixmap, PixmapMut, PixmapPaint,
    PixmapRef, Stroke, Transform,
};

use crate::geom::Rect;

/// Sans-serif families tried in order for the UI font; the first one installed wins.
const UI_FONT_CANDIDATES: [&str; 10] = [
    "Inter",
    "Sora",
    "Noto Sans",
    "DejaVu Sans",
    "Liberation Sans",
    "Cantarell",
    "Ubuntu",
    "Open Sans",
    "Roboto",
    "FreeSans",
];

/// Shaped text cache entries kept before the cache is cleared.
const TEXT_CACHE_LIMIT: usize = 2048;

/// The 8x8 ordered dithering thresholds, each the fraction of a step a pixel
/// rounds up at.
const BAYER: [u8; 64] = [
    0, 32, 8, 40, 2, 34, 10, 42, 48, 16, 56, 24, 50, 18, 58, 26, 12, 44, 4, 36, 14, 46, 6, 38, 60, 28, 52,
    20, 62, 30, 54, 22, 3, 35, 11, 43, 1, 33, 9, 41, 51, 19, 59, 27, 49, 17, 57, 25, 15, 47, 7, 39, 13, 45,
    5, 37, 63, 31, 55, 23, 61, 29, 53, 21,
];

/// An opaque color from 8-bit channels.
///
/// # Arguments
/// * `r` - Red.
/// * `g` - Green.
/// * `b` - Blue.
pub fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::from_rgba8(r, g, b, 255)
}

/// A translucent color from 8-bit channels.
///
/// # Arguments
/// * `r` - Red.
/// * `g` - Green.
/// * `b` - Blue.
/// * `a` - Alpha, 255 being opaque.
pub fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color {
    Color::from_rgba8(r, g, b, a)
}

/// Parses a `#rrggbb` or `#rrggbbaa` color.
///
/// # Arguments
/// * `text` - The color text, with or without the leading hash.
///
/// # Returns
/// The color, or `None` when the text is not a hex color.
pub fn parse_color(text: &str) -> Option<Color> {
    let hex = text.trim().trim_start_matches('#');
    if hex.len() != 6 && hex.len() != 8 {
        return None;
    }
    let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    let a = if hex.len() == 8 { channel(6)? } else { 255 };
    Some(Color::from_rgba8(channel(0)?, channel(2)?, channel(4)?, a))
}

/// An opaque color from a hue in degrees and a saturation and value from 0 to 1.
///
/// # Arguments
/// * `h` - Hue in degrees; any value, taken modulo 360.
/// * `s` - Saturation, 0 for gray to 1 for the pure hue.
/// * `v` - Value, 0 for black to 1 for the brightest.
pub fn hsv_to_color(h: f32, s: f32, v: f32) -> Color {
    let (s, v) = (s.clamp(0.0, 1.0), v.clamp(0.0, 1.0));
    let h = h.rem_euclid(360.0) / 60.0;
    let i = h.floor();
    let f = h - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));
    let (r, g, b) = match i as i32 {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    Color::from_rgba(r, g, b, 1.0).unwrap_or(Color::BLACK)
}

/// The hue in degrees, saturation and value of a color; gray has hue 0.
///
/// # Arguments
/// * `color` - The color; its alpha is ignored.
pub fn color_to_hsv(color: Color) -> (f32, f32, f32) {
    let (r, g, b) = (color.red(), color.green(), color.blue());
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let h = if delta <= f32::EPSILON {
        0.0
    } else if max == r {
        60.0 * ((g - b) / delta).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    let s = if max <= 0.0 { 0.0 } else { delta / max };
    (h, s, max)
}

/// A line of shaped text ready to be drawn any number of times.
pub struct Text {
    buffer: Buffer,
    /// Advance width in physical pixels.
    pub width: f32,
    /// Physical pixel size the text was shaped at.
    pub px: f32,
}

/// The font system shared by every canvas, with caches for shaped lines and
/// rasterized glyphs.
pub struct Fonts {
    system: FontSystem,
    glyphs: SwashCache,
    family: String,
    shaped: HashMap<(String, u32, bool), Arc<Text>>,
}

impl Default for Fonts {
    fn default() -> Fonts {
        Fonts::new()
    }
}

impl Fonts {
    /// Loads the system fonts and picks the UI family.
    pub fn new() -> Fonts {
        let mut system = FontSystem::new();
        let installed: Vec<String> = system
            .db()
            .faces()
            .flat_map(|face| face.families.iter().map(|(name, _)| name.clone()))
            .collect();
        let family = UI_FONT_CANDIDATES
            .iter()
            .find(|candidate| installed.iter().any(|name| name == *candidate))
            .map(|name| name.to_string())
            .unwrap_or_else(|| {
                installed
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "sans-serif".to_string())
            });
        system.db_mut().set_sans_serif_family(family.clone());
        Fonts {
            system,
            glyphs: SwashCache::new(),
            family,
            shaped: HashMap::new(),
        }
    }

    /// Shapes one line of text at a physical pixel size, from the cache when it
    /// was shaped before.
    ///
    /// # Arguments
    /// * `text` - The text; control characters are shown as given.
    /// * `px` - Font size in physical pixels.
    /// * `bold` - Whether to use the bold weight.
    pub fn shape(&mut self, text: &str, px: f32, bold: bool) -> Arc<Text> {
        let key = (text.to_string(), px.to_bits(), bold);
        if let Some(shaped) = self.shaped.get(&key) {
            return shaped.clone();
        }
        if self.shaped.len() >= TEXT_CACHE_LIMIT {
            self.shaped.clear();
        }
        let metrics = Metrics::new(px, px * 1.3);
        let mut buffer = Buffer::new(&mut self.system, metrics);
        buffer.set_size(None, None);
        let attrs = Attrs::new().family(Family::Name(&self.family)).weight(if bold {
            Weight::BOLD
        } else {
            Weight::NORMAL
        });
        buffer.set_text(text, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(&mut self.system, false);
        let width = buffer.layout_runs().map(|run| run.line_w).fold(0.0, f32::max);
        let shaped = Arc::new(Text { buffer, width, px });
        self.shaped.insert(key, shaped.clone());
        shaped
    }

    /// Drops every shaped line, after a font or scale change.
    pub fn clear_cache(&mut self) {
        self.shaped.clear();
    }
}

/// Horizontal placement of text within a box.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Align {
    /// Flush with the left edge.
    Left,
    /// Centered in the box.
    Center,
    /// Flush with the right edge.
    Right,
}

/// A drawing target over a pixmap, at a fixed integer output scale.
pub struct Canvas<'a> {
    pix: PixmapMut<'a>,
    scale: i32,
    clip: Option<IntRect>,
}

impl<'a> Canvas<'a> {
    /// Wraps a pixmap whose dimensions are logical size times scale.
    ///
    /// # Arguments
    /// * `pix` - The target pixels.
    /// * `scale` - The output scale factor applied to every coordinate.
    pub fn new(pix: PixmapMut<'a>, scale: i32) -> Canvas<'a> {
        let clip = IntRect::from_xywh(0, 0, pix.width(), pix.height());
        Canvas { pix, scale, clip }
    }

    /// The output scale factor.
    pub fn scale(&self) -> i32 {
        self.scale
    }

    /// The scale as a float for arithmetic.
    fn s(&self) -> f32 {
        self.scale as f32
    }

    /// Logical width of the canvas.
    pub fn width(&self) -> i32 {
        (self.pix.width() / self.scale as u32) as i32
    }

    /// Logical height of the canvas.
    pub fn height(&self) -> i32 {
        (self.pix.height() / self.scale as u32) as i32
    }

    /// Restricts drawing to a logical rectangle until `reset_clip`, intersected
    /// with the current clip.
    ///
    /// # Arguments
    /// * `rect` - The logical rectangle to keep drawing inside.
    pub fn clip(&mut self, rect: Rect) {
        let r = rect.scaled(self.scale);
        self.clip = IntRect::from_xywh(r.x, r.y, r.w.max(0) as u32, r.h.max(0) as u32)
            .and_then(|next| next.intersect(&self.clip?));
    }

    /// Runs a drawing restricted to a logical rectangle, intersected with the
    /// current clip, and puts the clip back as it was after, so a caller's
    /// own clip survives what it calls.
    ///
    /// # Arguments
    /// * `rect` - The logical rectangle to keep the drawing inside.
    /// * `draw` - The drawing.
    pub fn clipped<R>(&mut self, rect: Rect, draw: impl FnOnce(&mut Self) -> R) -> R {
        let saved = self.clip;
        self.clip(rect);
        let out = draw(self);
        self.clip = saved;
        out
    }

    /// Whether the current clip leaves nothing to draw.
    fn clip_empty(&self) -> bool {
        self.clip.is_none()
    }

    /// Removes any clip set with `clip`.
    pub fn reset_clip(&mut self) {
        self.clip = IntRect::from_xywh(0, 0, self.pix.width(), self.pix.height());
    }

    /// Current clip in physical pixels, or `None` when nothing can be drawn.
    fn clip_rect(&self) -> Option<IntRect> {
        self.clip
    }

    /// A mask of the current clip when it is smaller than the pixmap, for the
    /// path operations that cannot be clipped arithmetically.
    fn clip_mask(&self) -> Option<Mask> {
        let whole = IntRect::from_xywh(0, 0, self.pix.width(), self.pix.height())?;
        let clip = self.clip?;
        if clip == whole {
            return None;
        }
        let mut mask = Mask::new(self.pix.width(), self.pix.height())?;
        let rect = tiny_skia::Rect::from_xywh(
            clip.x() as f32,
            clip.y() as f32,
            clip.width() as f32,
            clip.height() as f32,
        )?;
        mask.fill_path(
            &PathBuilder::from_rect(rect),
            FillRule::Winding,
            true,
            Transform::identity(),
        );
        Some(mask)
    }

    /// Makes a logical rectangle fully transparent.
    ///
    /// # Arguments
    /// * `rect` - The logical rectangle to clear.
    pub fn clear(&mut self, rect: Rect) {
        let Some(area) = self.physical_area(rect) else {
            return;
        };
        let stride = self.pix.width() as usize * 4;
        let data = self.pix.data_mut();
        for y in area.y()..area.bottom() {
            let start = y as usize * stride + area.x() as usize * 4;
            data[start..start + area.width() as usize * 4].fill(0);
        }
    }

    /// The physical pixels a logical rectangle covers inside the clip.
    fn physical_area(&self, rect: Rect) -> Option<IntRect> {
        let r = rect.scaled(self.scale);
        IntRect::from_xywh(r.x, r.y, r.w.max(0) as u32, r.h.max(0) as u32)?.intersect(&self.clip_rect()?)
    }

    /// Fills a logical rectangle with a color.
    ///
    /// # Arguments
    /// * `rect` - The logical rectangle.
    /// * `color` - The fill color, blended over what is there.
    pub fn fill_rect(&mut self, rect: Rect, color: Color) {
        let Some(area) = self.physical_area(rect) else {
            return;
        };
        let mut paint = Paint::default();
        paint.set_color(color);
        paint.anti_alias = false;
        let skia = tiny_skia::Rect::from_xywh(
            area.x() as f32,
            area.y() as f32,
            area.width() as f32,
            area.height() as f32,
        );
        if let Some(skia) = skia {
            self.pix.fill_rect(skia, &paint, Transform::identity(), None);
        }
    }

    /// Fills a logical rectangle with a top to bottom gradient, ordered
    /// dithering breaking the 8-bit steps into noise the eye does not read as
    /// bands.
    ///
    /// # Arguments
    /// * `rect` - The logical rectangle.
    /// * `top` - Color at the top edge.
    /// * `bottom` - Color at the bottom edge.
    pub fn fill_dithered_gradient(&mut self, rect: Rect, top: Color, bottom: Color) {
        let Some(area) = self.physical_area(rect) else {
            return;
        };
        let r = rect.scaled(self.scale);
        let (top, bottom) = (top.to_color_u8(), bottom.to_color_u8());
        let channels_top = [top.red() as f32, top.green() as f32, top.blue() as f32];
        let channels_bottom = [bottom.red() as f32, bottom.green() as f32, bottom.blue() as f32];
        let span = (r.h - 1).max(1) as f32;
        let stride = self.pix.width() as usize * 4;
        let data = self.pix.data_mut();
        for y in area.y()..area.bottom() {
            let t = ((y - r.y) as f32 / span).clamp(0.0, 1.0);
            let row = [0, 1, 2].map(|i| channels_top[i] + (channels_bottom[i] - channels_top[i]) * t);
            let base = y as usize * stride;
            for x in area.x()..area.right() {
                let noise = BAYER[(y as usize & 7) * 8 + (x as usize & 7)] as f32 / 64.0;
                let d = base + x as usize * 4;
                data[d] = (row[0] + noise).floor().clamp(0.0, 255.0) as u8;
                data[d + 1] = (row[1] + noise).floor().clamp(0.0, 255.0) as u8;
                data[d + 2] = (row[2] + noise).floor().clamp(0.0, 255.0) as u8;
                data[d + 3] = 255;
            }
        }
    }

    /// Builds the path of a rounded rectangle in physical pixels.
    fn rounded_path(&self, rect: Rect, radius: f32) -> Option<Path> {
        self.rounded_path_inset(rect, radius, 0.0)
    }

    /// Builds the path of a rounded rectangle in physical pixels, pulled in
    /// from the rectangle's edges by a physical distance.
    ///
    /// # Arguments
    /// * `rect` - The logical rectangle.
    /// * `radius` - Corner radius in logical pixels.
    /// * `inset` - Physical pixels between the rectangle's edges and the path.
    fn rounded_path_inset(&self, rect: Rect, radius: f32, inset: f32) -> Option<Path> {
        let s = self.s();
        let x = rect.x as f32 * s + inset;
        let y = rect.y as f32 * s + inset;
        let w = rect.w as f32 * s - 2.0 * inset;
        let h = rect.h as f32 * s - 2.0 * inset;
        let r = (radius * s - inset).min(w / 2.0).min(h / 2.0).max(0.0);
        if w <= 0.0 || h <= 0.0 {
            return None;
        }
        let mut pb = PathBuilder::new();
        if r <= 0.0 {
            pb.push_rect(tiny_skia::Rect::from_xywh(x, y, w, h)?);
            return pb.finish();
        }
        let k = 0.552_284_8 * r;
        pb.move_to(x + r, y);
        pb.line_to(x + w - r, y);
        pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
        pb.line_to(x + w, y + h - r);
        pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
        pb.line_to(x + r, y + h);
        pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
        pb.line_to(x, y + r);
        pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
        pb.close();
        pb.finish()
    }

    /// Fills a rounded rectangle.
    ///
    /// # Arguments
    /// * `rect` - The logical rectangle.
    /// * `radius` - Corner radius in logical pixels.
    /// * `color` - The fill color.
    pub fn fill_rounded(&mut self, rect: Rect, radius: f32, color: Color) {
        if self.clip_empty() {
            return;
        }
        let Some(path) = self.rounded_path(rect, radius) else {
            return;
        };
        let mut paint = Paint::default();
        paint.set_color(color);
        paint.anti_alias = true;
        let mask = self.clip_mask();
        self.pix.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            mask.as_ref(),
        );
    }

    /// Fills a rounded rectangle with a color computed for every pixel from
    /// its place in the rectangle, for gradients and color spectra; the
    /// colors' alpha is kept.
    ///
    /// # Arguments
    /// * `rect` - The logical rectangle.
    /// * `radius` - Corner radius in logical pixels.
    /// * `shade` - The color at a point, given its horizontal and vertical
    ///   position from 0 at the left or top edge to 1 at the right or bottom.
    pub fn fill_rounded_shaded(&mut self, rect: Rect, radius: f32, shade: impl Fn(f32, f32) -> Color) {
        if self.clip_empty() {
            return;
        }
        let r = rect.scaled(self.scale);
        let (Ok(w), Ok(h)) = (u32::try_from(r.w), u32::try_from(r.h)) else {
            return;
        };
        let Some(mut tile) = Pixmap::new(w, h) else {
            return;
        };
        {
            let data = tile.data_mut();
            let span_x = (w - 1).max(1) as f32;
            let span_y = (h - 1).max(1) as f32;
            for y in 0..h {
                let v = y as f32 / span_y;
                for x in 0..w {
                    let c = shade(x as f32 / span_x, v).premultiply().to_color_u8();
                    let i = ((y * w + x) * 4) as usize;
                    data[i..i + 4].copy_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
                }
            }
        }
        let Some(path) = self.rounded_path(rect, radius) else {
            return;
        };
        let paint = Paint {
            anti_alias: true,
            shader: tiny_skia::Pattern::new(
                tile.as_ref(),
                tiny_skia::SpreadMode::Pad,
                FilterQuality::Nearest,
                1.0,
                Transform::from_translate(r.x as f32, r.y as f32),
            ),
            ..Paint::default()
        };
        let mask = self.clip_mask();
        self.pix.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            mask.as_ref(),
        );
    }

    /// Strokes the outline of a rounded rectangle.
    ///
    /// # Arguments
    /// * `rect` - The logical rectangle.
    /// * `radius` - Corner radius in logical pixels.
    /// * `width` - Line width in logical pixels.
    /// * `color` - The stroke color.
    pub fn stroke_rounded(&mut self, rect: Rect, radius: f32, width: f32, color: Color) {
        if self.clip_empty() {
            return;
        }
        let Some(path) = self.rounded_path_inset(rect, radius, width * self.s() / 2.0) else {
            return;
        };
        let mut paint = Paint::default();
        paint.set_color(color);
        paint.anti_alias = true;
        let stroke = Stroke {
            width: width * self.s(),
            ..Stroke::default()
        };
        let mask = self.clip_mask();
        self.pix
            .stroke_path(&path, &paint, &stroke, Transform::identity(), mask.as_ref());
    }

    /// Fills an arbitrary path given in logical coordinates.
    ///
    /// # Arguments
    /// * `path` - The path in logical coordinates.
    /// * `color` - The fill color.
    pub fn fill_path(&mut self, path: &Path, color: Color) {
        if self.clip_empty() {
            return;
        }
        let mut paint = Paint::default();
        paint.set_color(color);
        paint.anti_alias = true;
        let mask = self.clip_mask();
        let transform = Transform::from_scale(self.s(), self.s());
        self.pix
            .fill_path(path, &paint, FillRule::Winding, transform, mask.as_ref());
    }

    /// Strokes an arbitrary path given in logical coordinates.
    ///
    /// # Arguments
    /// * `path` - The path in logical coordinates.
    /// * `width` - Line width in logical pixels.
    /// * `color` - The stroke color.
    pub fn stroke_path(&mut self, path: &Path, width: f32, color: Color) {
        if self.clip_empty() {
            return;
        }
        let mut paint = Paint::default();
        paint.set_color(color);
        paint.anti_alias = true;
        let stroke = Stroke {
            width,
            line_cap: tiny_skia::LineCap::Round,
            line_join: tiny_skia::LineJoin::Round,
            ..Stroke::default()
        };
        let mask = self.clip_mask();
        let transform = Transform::from_scale(self.s(), self.s());
        self.pix
            .stroke_path(path, &paint, &stroke, transform, mask.as_ref());
    }

    /// Draws an image scaled to fit a logical box, centered, keeping its aspect.
    ///
    /// # Arguments
    /// * `image` - The premultiplied image.
    /// * `rect` - The logical box to fit into.
    pub fn draw_image_fit(&mut self, image: &Pixmap, rect: Rect) {
        if self.clip_empty() || image.width() == 0 || image.height() == 0 || rect.is_empty() {
            return;
        }
        let s = self.s();
        let box_w = rect.w as f32 * s;
        let box_h = rect.h as f32 * s;
        let scale = (box_w / image.width() as f32).min(box_h / image.height() as f32);
        let draw_w = image.width() as f32 * scale;
        let draw_h = image.height() as f32 * scale;
        let x = rect.x as f32 * s + (box_w - draw_w) / 2.0;
        let y = rect.y as f32 * s + (box_h - draw_h) / 2.0;
        self.draw_image_physical(image.as_ref(), x, y, scale);
    }

    /// Draws an image covering a logical box, cropped to the box when the
    /// aspect differs.
    ///
    /// # Arguments
    /// * `image` - The premultiplied image.
    /// * `rect` - The logical box to cover.
    pub fn draw_image_cover(&mut self, image: &Pixmap, rect: Rect) {
        if self.clip_empty() || image.width() == 0 || image.height() == 0 || rect.is_empty() {
            return;
        }
        let saved = self.clip;
        self.clip(rect);
        let s = self.s();
        let box_w = rect.w as f32 * s;
        let box_h = rect.h as f32 * s;
        let scale = (box_w / image.width() as f32).max(box_h / image.height() as f32);
        let draw_w = image.width() as f32 * scale;
        let draw_h = image.height() as f32 * scale;
        let x = rect.x as f32 * s + (box_w - draw_w) / 2.0;
        let y = rect.y as f32 * s + (box_h - draw_h) / 2.0;
        self.draw_image_physical(image.as_ref(), x, y, scale);
        self.clip = saved;
    }

    /// Draws an image stretched to a logical box.
    ///
    /// # Arguments
    /// * `image` - The premultiplied image.
    /// * `rect` - The logical box.
    pub fn draw_image_stretch(&mut self, image: &Pixmap, rect: Rect) {
        if self.clip_empty() || image.width() == 0 || image.height() == 0 || rect.is_empty() {
            return;
        }
        let s = self.s();
        let sx = rect.w as f32 * s / image.width() as f32;
        let sy = rect.h as f32 * s / image.height() as f32;
        let transform = Transform::from_row(sx, 0.0, 0.0, sy, rect.x as f32 * s, rect.y as f32 * s);
        let paint = PixmapPaint {
            quality: FilterQuality::Bilinear,
            ..PixmapPaint::default()
        };
        let mask = self.clip_mask();
        self.pix
            .draw_pixmap(0, 0, image.as_ref(), &paint, transform, mask.as_ref());
    }

    /// Draws an image at a physical position and uniform scale.
    fn draw_image_physical(&mut self, image: PixmapRef<'_>, x: f32, y: f32, scale: f32) {
        let quality = if (scale - 1.0).abs() < 0.001 {
            FilterQuality::Nearest
        } else {
            FilterQuality::Bilinear
        };
        let paint = PixmapPaint {
            quality,
            ..PixmapPaint::default()
        };
        let transform = Transform::from_row(scale, 0.0, 0.0, scale, x, y);
        let mask = self.clip_mask();
        self.pix
            .draw_pixmap(0, 0, image, &paint, transform, mask.as_ref());
    }

    /// Draws one shaped line of text with its vertical center on a logical Y.
    ///
    /// # Arguments
    /// * `fonts` - The font system that shaped the text.
    /// * `text` - The shaped line.
    /// * `x` - Logical X of the text box.
    /// * `center_y` - Logical Y the glyphs are centered on.
    /// * `width` - Logical width of the box; text is aligned within and clipped to it.
    /// * `align` - Horizontal alignment within the box.
    /// * `color` - Text color.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_text(
        &mut self,
        fonts: &mut Fonts,
        text: &Text,
        x: i32,
        center_y: i32,
        width: i32,
        align: Align,
        color: Color,
    ) {
        if self.clip_empty() {
            return;
        }
        let s = self.s();
        let box_x = x as f32 * s;
        let box_w = width as f32 * s;
        let start_x = match align {
            Align::Left => box_x,
            Align::Center => box_x + ((box_w - text.width) / 2.0).max(0.0),
            Align::Right => box_x + (box_w - text.width).max(0.0),
        };
        let line_height = text.px * 1.3;
        let top = center_y as f32 * s - line_height / 2.0;
        let clip = Rect::new(
            x,
            center_y - (line_height / s) as i32,
            width,
            (2.0 * line_height / s) as i32 + 2,
        );
        let saved = self.clip;
        self.clip(clip);
        self.blit_text(fonts, text, start_x, top, color);
        self.clip = saved;
    }

    /// Rasterizes the glyphs of a shaped line onto the canvas.
    fn blit_text(&mut self, fonts: &mut Fonts, text: &Text, origin_x: f32, origin_y: f32, color: Color) {
        let Some(clip) = self.clip_rect() else {
            return;
        };
        let pix_w = self.pix.width() as i32;
        let pix_h = self.pix.height() as i32;
        let stride = pix_w as usize * 4;
        let (cr, cg, cb, ca) = {
            let c = color.to_color_u8();
            (
                c.red() as u32,
                c.green() as u32,
                c.blue() as u32,
                c.alpha() as u32,
            )
        };
        let data = self.pix.data_mut();
        for run in text.buffer.layout_runs() {
            for glyph in run.glyphs {
                let physical = glyph.physical((origin_x, origin_y + run.line_y), 1.0);
                let Some(image) = fonts.glyphs.get_image(&mut fonts.system, physical.cache_key) else {
                    continue;
                };
                let gx = physical.x + image.placement.left;
                let gy = physical.y - image.placement.top;
                let gw = image.placement.width as i32;
                let gh = image.placement.height as i32;
                let x0 = gx.max(clip.x()).max(0);
                let y0 = gy.max(clip.y()).max(0);
                let x1 = (gx + gw).min(clip.right()).min(pix_w);
                let y1 = (gy + gh).min(clip.bottom()).min(pix_h);
                if x1 <= x0 || y1 <= y0 {
                    continue;
                }
                match image.content {
                    SwashContent::Mask => {
                        for py in y0..y1 {
                            let src_row = ((py - gy) * gw) as usize;
                            let dst_row = py as usize * stride;
                            for px in x0..x1 {
                                let cov = image.data[src_row + (px - gx) as usize] as u32;
                                if cov == 0 {
                                    continue;
                                }
                                let a = ca * cov / 255;
                                let d = dst_row + px as usize * 4;
                                blend_pixel(&mut data[d..d + 4], cr * a / 255, cg * a / 255, cb * a / 255, a);
                            }
                        }
                    }
                    SwashContent::Color => {
                        for py in y0..y1 {
                            let src_row = ((py - gy) * gw * 4) as usize;
                            let dst_row = py as usize * stride;
                            for px in x0..x1 {
                                let sidx = src_row + (px - gx) as usize * 4;
                                let a = image.data[sidx + 3] as u32;
                                if a == 0 {
                                    continue;
                                }
                                let r = image.data[sidx] as u32 * a / 255;
                                let g = image.data[sidx + 1] as u32 * a / 255;
                                let b = image.data[sidx + 2] as u32 * a / 255;
                                let d = dst_row + px as usize * 4;
                                blend_pixel(&mut data[d..d + 4], r, g, b, a);
                            }
                        }
                    }
                    SwashContent::SubpixelMask => {
                        for py in y0..y1 {
                            let src_row = ((py - gy) * gw * 4) as usize;
                            let dst_row = py as usize * stride;
                            for px in x0..x1 {
                                let sidx = src_row + (px - gx) as usize * 4;
                                let cov = (image.data[sidx] as u32
                                    + image.data[sidx + 1] as u32
                                    + image.data[sidx + 2] as u32)
                                    / 3;
                                if cov == 0 {
                                    continue;
                                }
                                let a = ca * cov / 255;
                                let d = dst_row + px as usize * 4;
                                blend_pixel(&mut data[d..d + 4], cr * a / 255, cg * a / 255, cb * a / 255, a);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Composites one premultiplied source pixel over a destination pixel.
#[inline]
fn blend_pixel(dst: &mut [u8], r: u32, g: u32, b: u32, a: u32) {
    let inv = 255 - a;
    dst[0] = (r + dst[0] as u32 * inv / 255).min(255) as u8;
    dst[1] = (g + dst[1] as u32 * inv / 255).min(255) as u8;
    dst[2] = (b + dst[2] as u32 * inv / 255).min(255) as u8;
    dst[3] = (a + dst[3] as u32 * inv / 255).min(255) as u8;
}

/// Converts straight-alpha RGBA pixels into a premultiplied pixmap.
///
/// # Arguments
/// * `width` - Image width.
/// * `height` - Image height.
/// * `rgba` - Straight alpha pixels, four bytes each, row major.
///
/// # Returns
/// The pixmap, or `None` for an empty image or a short buffer.
pub fn pixmap_from_rgba(width: u32, height: u32, rgba: &[u8]) -> Option<Pixmap> {
    if rgba.len() < (width as usize) * (height as usize) * 4 {
        return None;
    }
    let mut pix = Pixmap::new(width, height)?;
    let data = pix.data_mut();
    for (dst, src) in data
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(rgba.as_chunks::<4>().0)
    {
        let a = src[3] as u32;
        dst[0] = (src[0] as u32 * a / 255) as u8;
        dst[1] = (src[1] as u32 * a / 255) as u8;
        dst[2] = (src[2] as u32 * a / 255) as u8;
        dst[3] = src[3];
    }
    Some(pix)
}

/// Decodes a PNG or JPEG file and scales it so its longer side is `size`
/// pixels; a zero size keeps the native dimensions.
///
/// # Arguments
/// * `path` - The image file.
/// * `size` - Target length of the longer side, or 0 for no scaling.
///
/// # Returns
/// The premultiplied pixmap, or `None` when the file cannot be decoded.
pub fn load_raster(path: &std::path::Path, size: u32) -> Option<Pixmap> {
    let bytes = std::fs::read(path).ok()?;
    decode_raster(&bytes, size)
}

/// Decodes PNG or JPEG bytes the way `load_raster` does for a file.
///
/// # Arguments
/// * `bytes` - The encoded image.
/// * `size` - Target length of the longer side, or 0 for no scaling.
pub fn decode_raster(bytes: &[u8], size: u32) -> Option<Pixmap> {
    let decoded = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (w, h) = decoded.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    let scaled = if size > 0 && w.max(h) != size {
        let (nw, nh) = if w >= h {
            (size, (h as u64 * size as u64 / w as u64).max(1) as u32)
        } else {
            ((w as u64 * size as u64 / h as u64).max(1) as u32, size)
        };
        image::imageops::resize(&decoded, nw, nh, image::imageops::FilterType::Triangle)
    } else {
        decoded
    };
    pixmap_from_rgba(scaled.width(), scaled.height(), scaled.as_raw())
}

/// Scales a premultiplied pixmap so its longer side is `size` pixels.
///
/// # Arguments
/// * `image` - The source image.
/// * `size` - Target length of the longer side.
pub fn resize_pixmap(image: &Pixmap, size: u32) -> Option<Pixmap> {
    let (w, h) = (image.width(), image.height());
    if w == 0 || h == 0 || size == 0 {
        return None;
    }
    if w.max(h) == size {
        return Some(image.clone());
    }
    let (nw, nh) = if w >= h {
        (size, (h as u64 * size as u64 / w as u64).max(1) as u32)
    } else {
        ((w as u64 * size as u64 / h as u64).max(1) as u32, size)
    };
    let mut straight = Vec::with_capacity(image.data().len());
    for px in image.data().as_chunks::<4>().0 {
        let a = px[3] as u32;
        let unmul = |c: u8| (c as u32 * 255).checked_div(a).unwrap_or(0).min(255) as u8;
        straight.extend_from_slice(&[unmul(px[0]), unmul(px[1]), unmul(px[2]), px[3]]);
    }
    let buffer = image::RgbaImage::from_raw(w, h, straight)?;
    let scaled = image::imageops::resize(&buffer, nw, nh, image::imageops::FilterType::Triangle);
    pixmap_from_rgba(nw, nh, scaled.as_raw())
}

/// The system fonts SVG icons with text are rendered with, loaded once.
fn svg_fonts() -> &'static std::sync::Arc<resvg::usvg::fontdb::Database> {
    static FONTS: std::sync::OnceLock<std::sync::Arc<resvg::usvg::fontdb::Database>> =
        std::sync::OnceLock::new();
    FONTS.get_or_init(|| {
        let mut db = resvg::usvg::fontdb::Database::new();
        db.load_system_fonts();
        std::sync::Arc::new(db)
    })
}

/// Renders an SVG file into a square pixmap of the given size, the drawing
/// fitted and centered.
///
/// # Arguments
/// * `path` - The SVG file.
/// * `size` - Side of the square in pixels.
pub fn load_svg(path: &std::path::Path, size: u32) -> Option<Pixmap> {
    let bytes = std::fs::read(path).ok()?;
    let options = resvg::usvg::Options {
        fontdb: svg_fonts().clone(),
        ..resvg::usvg::Options::default()
    };
    let tree = resvg::usvg::Tree::from_data(&bytes, &options).ok()?;
    let tree_size = tree.size();
    if tree_size.width() <= 0.0 || tree_size.height() <= 0.0 || size == 0 {
        return None;
    }
    let scale = (size as f32 / tree_size.width()).min(size as f32 / tree_size.height());
    let tx = (size as f32 - tree_size.width() * scale) / 2.0;
    let ty = (size as f32 - tree_size.height() * scale) / 2.0;
    let mut pix = Pixmap::new(size, size)?;
    let transform = Transform::from_row(scale, 0.0, 0.0, scale, tx, ty);
    resvg::render(&tree, transform, &mut pix.as_mut());
    Some(pix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_colors() {
        let c = parse_color("#1e2a3b").unwrap().to_color_u8();
        assert_eq!((c.red(), c.green(), c.blue(), c.alpha()), (0x1e, 0x2a, 0x3b, 255));
        let c = parse_color("ff000080").unwrap().to_color_u8();
        assert_eq!((c.red(), c.alpha()), (255, 0x80));
        assert!(parse_color("#12345").is_none());
        assert!(parse_color("blue").is_none());
    }

    #[test]
    fn hsv_round_trips_through_rgb() {
        for hex in [
            "#7967c5", "#0a0e17", "#eef2f8", "#ff0000", "#00ff00", "#0000ff", "#000000", "#ffffff", "#808080",
        ] {
            let color = parse_color(hex).unwrap();
            let (h, s, v) = color_to_hsv(color);
            let back = hsv_to_color(h, s, v).to_color_u8();
            let c = color.to_color_u8();
            assert_eq!(
                (back.red(), back.green(), back.blue()),
                (c.red(), c.green(), c.blue()),
                "{hex}"
            );
        }
        let (h, s, v) = color_to_hsv(rgb(0, 255, 0));
        assert!((h - 120.0).abs() < 0.01 && (s - 1.0).abs() < 0.01 && (v - 1.0).abs() < 0.01);
        let (h, s, _) = color_to_hsv(rgb(128, 128, 128));
        assert_eq!((h, s), (0.0, 0.0));
        let red = hsv_to_color(360.0, 1.0, 1.0).to_color_u8();
        assert_eq!((red.red(), red.green(), red.blue()), (255, 0, 0));
    }

    #[test]
    fn shaded_fill_follows_position() {
        let mut pix = Pixmap::new(10, 4).unwrap();
        {
            let mut canvas = Canvas::new(pix.as_mut(), 1);
            canvas.fill_rounded_shaded(Rect::new(0, 0, 10, 4), 0.0, |u, _| {
                Color::from_rgba(u, 0.0, 0.0, 1.0).unwrap()
            });
        }
        let px = |x: usize| pix.data()[(10 + x) * 4];
        assert_eq!(px(0), 0);
        assert_eq!(px(9), 255);
        assert!(px(4) > px(1) && px(4) < px(8));
    }

    #[test]
    fn fill_respects_clip_and_scale() {
        let mut pix = Pixmap::new(20, 20).unwrap();
        {
            let mut canvas = Canvas::new(pix.as_mut(), 2);
            canvas.clip(Rect::new(0, 0, 5, 5));
            canvas.fill_rect(Rect::new(0, 0, 10, 10), rgb(255, 0, 0));
        }
        let px = |x: usize, y: usize| pix.data()[(y * 20 + x) * 4 + 3];
        assert_eq!(px(0, 0), 255);
        assert_eq!(px(9, 9), 255);
        assert_eq!(px(10, 10), 0);
    }

    #[test]
    fn premultiplies_rgba() {
        let pix = pixmap_from_rgba(1, 1, &[255, 255, 255, 128]).unwrap();
        assert_eq!(&pix.data()[..4], &[128, 128, 128, 128]);
    }

    #[test]
    fn blend_over_is_source_when_opaque() {
        let mut dst = [10u8, 20, 30, 255];
        blend_pixel(&mut dst, 200, 100, 50, 255);
        assert_eq!(dst, [200, 100, 50, 255]);
    }
}
