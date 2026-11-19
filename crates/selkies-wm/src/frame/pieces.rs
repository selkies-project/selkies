/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Frames assembled from cached pieces, for a backend that stretches and blends parts.
//!
//! Away from its corners each side of a frame repeats one row or column. Each part is drawn
//! once around a frame of a canonical size and cut into its two ends and a band of [`BAND`]
//! identical pixels; a frame of any larger size is the ends at its corners and the band's middle
//! pixel stretched between them, read from inside the band so filtering stays exact. A resize
//! draws no piece, and a backend keeping a part's buffer while its pixels stay uploads none.
//!
//! The title is drawn on its own where [`Canvas::draw_text`] would place it and laid over the
//! bar. A frame too small for the canonical pieces, or a shaded one, is drawn whole.

use std::collections::HashMap;
use std::sync::Arc;

use pixelflux::wm::{Frame, FramePart, Image, Rect, Size};
use selkies_draw::render::{Canvas, Text};
use selkies_draw::tiny_skia::Pixmap;

use super::{
    Appearance, Buttons, Context, Renderer, SHADOW, SHADOW_OFFSET, Shadow, TITLE_FONT, image_of, layout,
    render_bottom, render_shadow, render_side, shape_fit, title_align,
};
use crate::theme::Look;

/// Width in logical pixels of the band a stretched middle is read from.
const BAND: i32 = 3;
/// Entries kept before the cache is emptied.
const CACHE_LIMIT: usize = 128;

/// What a set of pieces is drawn from, besides the look.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum PieceKey {
    /// The untitled title bar's ends, `left` and `right` wide, and band.
    TitleBar {
        active: bool,
        maximized: bool,
        hover: Option<Context>,
        pressed: Option<Context>,
        shadow: bool,
        buttons: Buttons,
        app_id: String,
        icon: Option<usize>,
        scale: i32,
        left: i32,
        right: i32,
    },
    /// The bottom border's two ends and band.
    Bottom { active: bool, shadow: bool, scale: i32 },
    /// The left and right borders' bands.
    Sides { active: bool, scale: i32 },
    /// The shadow's top, bottom, left, and right strips, each as ends and band.
    Shadow { active: bool, scale: i32 },
    /// A title `width` wide, its text `offset` physical pixels (as bits) in.
    Title {
        text: usize,
        active: bool,
        scale: i32,
        offset: u32,
        width: i32,
    },
}

/// Pieces kept, with the shaped text and icon their keys point at.
struct Kept {
    pieces: Vec<Image>,
    _text: Option<Arc<Text>>,
    _icon: Option<Arc<[u8]>>,
}

/// The pieces drawn so far, for the look they were drawn in.
#[derive(Default)]
pub(super) struct Pieces {
    look: Option<Look>,
    kept: HashMap<PieceKey, Kept>,
}

/// Logical pixels from a frame's edge that its corners and the shadow's reach, plus antialiasing.
fn corner_reach(radius: f32) -> i32 {
    radius.ceil() as i32 + 2
}

/// The logical rectangle `rect` of an image, as an image of its own.
fn crop(image: &Image, rect: Rect) -> Image {
    let s = image.scale.max(1) as usize;
    let (x, y) = (rect.x as usize * s, rect.y as usize * s);
    let (w, h) = (rect.w as usize * s, rect.h as usize * s);
    let stride = image.width as usize * 4;
    let mut rgba = Vec::with_capacity(w * h * 4);
    for row in y..y + h {
        let start = row * stride + x * 4;
        rgba.extend_from_slice(&image.rgba[start..start + w * 4]);
    }
    Image {
        width: w as u32,
        height: h as u32,
        scale: image.scale,
        rgba: Arc::from(rgba),
    }
}

/// A horizontal run cut into its two ends, `left` and `right` pixels wide, and the band between.
fn cut_row(image: &Image, len: i32, height: i32, left: i32, right: i32) -> [Image; 3] {
    [
        crop(image, Rect::new(0, 0, left, height)),
        crop(image, Rect::new(left, 0, BAND, height)),
        crop(image, Rect::new(len - right, 0, right, height)),
    ]
}

/// A vertical run cut into its two ends, `top` and `bottom` pixels tall, and the band between.
fn cut_column(image: &Image, width: i32, len: i32, top: i32, bottom: i32) -> [Image; 3] {
    [
        crop(image, Rect::new(0, 0, width, top)),
        crop(image, Rect::new(0, top, width, BAND)),
        crop(image, Rect::new(0, len - bottom, width, bottom)),
    ]
}

/// The parts of a run along x from `x` to `x + len`: its ends and its band stretched between.
fn row_parts(
    pieces: &[Image],
    x: i32,
    y: i32,
    len: i32,
    height: i32,
    left: i32,
    right: i32,
) -> [FramePart; 3] {
    [
        FramePart {
            rect: Rect::new(x, y, left, height),
            image: pieces[0].clone(),
            source: None,
        },
        FramePart {
            rect: Rect::new(x + left, y, len - left - right, height),
            image: pieces[1].clone(),
            source: Some(Rect::new(1, 0, 1, height)),
        },
        FramePart {
            rect: Rect::new(x + len - right, y, right, height),
            image: pieces[2].clone(),
            source: None,
        },
    ]
}

/// The parts of a run along y from `y` to `y + len`, as [`row_parts`] lays one along x.
fn column_parts(
    pieces: &[Image],
    x: i32,
    y: i32,
    width: i32,
    len: i32,
    top: i32,
    bottom: i32,
) -> [FramePart; 3] {
    [
        FramePart {
            rect: Rect::new(x, y, width, top),
            image: pieces[0].clone(),
            source: None,
        },
        FramePart {
            rect: Rect::new(x, y + top, width, len - top - bottom),
            image: pieces[1].clone(),
            source: Some(Rect::new(0, 1, width, 1)),
        },
        FramePart {
            rect: Rect::new(x, y + len - bottom, width, bottom),
            image: pieces[2].clone(),
            source: None,
        },
    ]
}

impl Renderer {
    /// The frame around a client of `client` logical pixels from cached pieces, front to back;
    /// `None` for a frame to draw whole.
    pub(super) fn render_sliced(
        &mut self,
        look: &Look,
        client: Size,
        appearance: &Appearance<'_>,
    ) -> Option<Frame> {
        if appearance.shaded || client.w <= 0 || client.h <= 0 {
            return None;
        }
        if self.pieces.look != Some(*look) || self.pieces.kept.len() > CACHE_LIMIT {
            self.pieces = Pieces {
                look: Some(*look),
                kept: HashMap::new(),
            };
        }
        let style = &look.style;
        let real = layout(style, client, appearance.maximized, false, appearance.buttons);
        let e = real.extents;
        let Size { w, h } = real.size;
        let scale = appearance.scale.max(1);
        let radius = if appearance.maximized {
            0.0
        } else {
            style.corners as f32
        };
        let reach = corner_reach(radius);
        let left = real.title.x.max(reach);
        let right = (w - real.title.right()).max(reach);
        let shadow_top = SHADOW_OFFSET + reach;
        let canonical = Size {
            w: left + BAND + right,
            h: e.top + e.bottom + 2 * (reach + SHADOW_OFFSET) + BAND + SHADOW,
        };
        if w < canonical.w || h < canonical.h {
            return None;
        }
        let canonical_client = Size {
            w: canonical.w - e.left - e.right,
            h: canonical.h - e.top - e.bottom,
        };
        let canonical_layout = layout(
            style,
            canonical_client,
            appearance.maximized,
            false,
            appearance.buttons,
        );
        let (ox, oy) = (-e.left, -e.top);
        let canonical_shadow = Shadow::cast(appearance, *style, Rect::new(ox, oy, canonical.w, canonical.h));
        let shadowed = canonical_shadow.is_some();
        let active = appearance.active;
        let mut parts = Vec::new();

        if let Some(title) = self.title_piece(look, &real, appearance) {
            parts.push(title);
        }

        let bar_key = PieceKey::TitleBar {
            active,
            maximized: appearance.maximized,
            hover: appearance.hover,
            pressed: appearance.pressed,
            shadow: shadowed,
            buttons: appearance.buttons,
            app_id: appearance.app_id.to_string(),
            icon: appearance
                .icon
                .map(|i| Arc::as_ptr(&i.rgba).cast::<u8>() as usize),
            scale,
            left,
            right,
        };
        if !self.pieces.kept.contains_key(&bar_key) {
            let untitled = Appearance {
                title: "",
                ..appearance.clone()
            };
            let bar = self.render_titlebar(
                look,
                &canonical_layout,
                &untitled,
                canonical_shadow.as_ref(),
                scale,
            )?;
            let pieces = cut_row(&bar, canonical.w, e.top, left, right).to_vec();
            self.pieces.kept.insert(
                bar_key.clone(),
                Kept {
                    pieces,
                    _text: None,
                    _icon: appearance.icon.map(|i| i.rgba.clone()),
                },
            );
        }
        parts.extend(row_parts(
            &self.pieces.kept[&bar_key].pieces,
            ox,
            oy,
            w,
            e.top,
            left,
            right,
        ));

        let palette = &look.palette;
        let (base, border) = if active {
            (palette.title_active, palette.border_active)
        } else {
            (palette.title_inactive, palette.border_inactive)
        };
        if e.left > 0 {
            let key = PieceKey::Sides { active, scale };
            let sides = &self
                .pieces
                .kept
                .entry(key)
                .or_insert_with(|| Kept {
                    pieces: vec![
                        render_side(e.left, BAND, scale, base, border, 0),
                        render_side(e.right, BAND, scale, base, border, e.right - 1),
                    ],
                    _text: None,
                    _icon: None,
                })
                .pieces;
            parts.push(FramePart {
                rect: Rect::new(ox, 0, e.left, client.h),
                image: sides[0].clone(),
                source: Some(Rect::new(0, 1, e.left, 1)),
            });
            parts.push(FramePart {
                rect: Rect::new(client.w, 0, e.right, client.h),
                image: sides[1].clone(),
                source: Some(Rect::new(0, 1, e.right, 1)),
            });
        }

        if e.bottom > 0 {
            let key = PieceKey::Bottom {
                active,
                shadow: shadowed,
                scale,
            };
            let bottom_y = canonical.h - e.top - e.bottom;
            let pieces = &self
                .pieces
                .kept
                .entry(key)
                .or_insert_with(|| {
                    let under = canonical_shadow.as_ref().map(|s| (s, ox, bottom_y));
                    let image = render_bottom(canonical.w, e.bottom, scale, base, border, radius, under);
                    Kept {
                        pieces: cut_row(&image, canonical.w, e.bottom, reach, reach).to_vec(),
                        _text: None,
                        _icon: None,
                    }
                })
                .pieces;
            parts.extend(row_parts(pieces, ox, client.h, w, e.bottom, reach, reach));
        }

        if let Some(shadow) = canonical_shadow.as_ref() {
            let key = PieceKey::Shadow { active, scale };
            if !self.pieces.kept.contains_key(&key) {
                let strips = render_shadow(shadow, scale);
                let [top, bottom, side_left, side_right] = strips.as_slice() else {
                    return None;
                };
                let across = canonical.w + 2 * SHADOW;
                let mut pieces = Vec::new();
                for strip in [top, bottom] {
                    pieces.extend(cut_row(
                        &strip.image,
                        across,
                        strip.rect.h,
                        SHADOW + reach,
                        SHADOW + reach,
                    ));
                }
                for strip in [side_left, side_right] {
                    pieces.extend(cut_column(
                        &strip.image,
                        strip.rect.w,
                        canonical.h,
                        shadow_top,
                        reach,
                    ));
                }
                self.pieces.kept.insert(
                    key.clone(),
                    Kept {
                        pieces,
                        _text: None,
                        _icon: None,
                    },
                );
            }
            let pieces = &self.pieces.kept[&key].pieces;
            let (top_h, bottom_h) = (SHADOW - SHADOW_OFFSET, SHADOW + SHADOW_OFFSET);
            let (strip_x, across) = (ox - SHADOW, w + 2 * SHADOW);
            let ends = SHADOW + reach;
            parts.extend(row_parts(
                &pieces[0..3],
                strip_x,
                oy - top_h,
                across,
                top_h,
                ends,
                ends,
            ));
            parts.extend(row_parts(
                &pieces[3..6],
                strip_x,
                oy + h,
                across,
                bottom_h,
                ends,
                ends,
            ));
            parts.extend(column_parts(
                &pieces[6..9],
                strip_x,
                oy,
                SHADOW,
                h,
                shadow_top,
                reach,
            ));
            parts.extend(column_parts(
                &pieces[9..12],
                ox + w,
                oy,
                SHADOW,
                h,
                shadow_top,
                reach,
            ));
        }

        Some(Frame { extents: e, parts })
    }

    /// The title in a picture just wide enough for it, placed where [`Canvas::draw_text`] puts
    /// it in the slot, so a resize moves it without drawing it again. `None` for no title.
    fn title_piece(
        &mut self,
        look: &Look,
        layout: &super::Layout,
        appearance: &Appearance<'_>,
    ) -> Option<FramePart> {
        let slot = layout.title;
        if appearance.title.is_empty() || slot.w <= 0 {
            return None;
        }
        let scale = appearance.scale.max(1);
        let s = scale as f32;
        let shaped = shape_fit(
            &mut self.fonts,
            appearance.title,
            TITLE_FONT * s,
            slot.w as f32 * s,
        );
        let align = title_align(look.style);
        let box_x = slot.x as f32 * s;
        let box_w = slot.w as f32 * s;
        let start = match align {
            selkies_draw::render::Align::Left => box_x,
            selkies_draw::render::Align::Center => box_x + ((box_w - shaped.width) / 2.0).max(0.0),
            selkies_draw::render::Align::Right => box_x + (box_w - shaped.width).max(0.0),
        };
        let x = (start / s).floor() as i32;
        let offset = start - x as f32 * s;
        let width = (((offset + shaped.width) / s).ceil() as i32 + 2).min(slot.right() - x);
        if width <= 0 {
            return None;
        }
        let key = PieceKey::Title {
            text: Arc::as_ptr(&shaped) as usize,
            active: appearance.active,
            scale,
            offset: offset.to_bits(),
            width,
        };
        if !self.pieces.kept.contains_key(&key) {
            let label = if appearance.active {
                look.palette.label_active
            } else {
                look.palette.label_inactive
            };
            let height = layout.extents.top;
            let mut pixmap = Pixmap::new((width * scale) as u32, (height * scale) as u32)?;
            Canvas::new(pixmap.as_mut(), scale).draw_text(
                &mut self.fonts,
                &shaped,
                slot.x - x,
                slot.y + slot.h / 2,
                slot.w,
                align,
                label,
            );
            self.pieces.kept.insert(
                key.clone(),
                Kept {
                    pieces: vec![image_of(pixmap, scale)],
                    _text: Some(shaped),
                    _icon: None,
                },
            );
        }
        Some(FramePart {
            rect: Rect::new(
                x - layout.extents.left,
                -layout.extents.top,
                width,
                layout.extents.top,
            ),
            image: self.pieces.kept[&key].pieces[0].clone(),
            source: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::Buttons;
    use super::*;

    /// A frame's parts painted back to front, stretched by nearest sampling, onto a canvas whose
    /// top-left corner is `origin` relative to the client, as premultiplied pixels.
    fn composite(frame: &Frame, origin: (i32, i32), size: (i32, i32), scale: i32) -> Vec<[u8; 4]> {
        let (cw, ch) = (size.0 * scale, size.1 * scale);
        let mut out = vec![[0u8; 4]; (cw * ch) as usize];
        for part in frame.parts.iter().rev() {
            let source = part.source.unwrap_or(Rect::new(0, 0, part.rect.w, part.rect.h));
            let (rw, rh) = (part.rect.w * scale, part.rect.h * scale);
            for py in 0..rh {
                for px in 0..rw {
                    let dx = (part.rect.x - origin.0) * scale + px;
                    let dy = (part.rect.y - origin.1) * scale + py;
                    if dx < 0 || dy < 0 || dx >= cw || dy >= ch {
                        continue;
                    }
                    let sx = source.x * scale + px * source.w * scale / rw;
                    let sy = source.y * scale + py * source.h * scale / rh;
                    let i = ((sy * part.image.width as i32 + sx) * 4) as usize;
                    let src = &part.image.rgba[i..i + 4];
                    let dst = &mut out[(dy * cw + dx) as usize];
                    let inv = 255 - u32::from(src[3]);
                    for c in 0..4 {
                        dst[c] = (u32::from(src[c]) + (u32::from(dst[c]) * inv + 127) / 255).min(255) as u8;
                    }
                }
            }
        }
        out
    }

    fn appearance(scale: i32, active: bool, sliced: bool) -> Appearance<'static> {
        Appearance {
            title: "A window with a title",
            icon: None,
            app_id: "",
            active,
            maximized: false,
            shaded: false,
            hover: None,
            pressed: None,
            scale,
            shadow: true,
            buttons: Buttons::ALL,
            sliced,
        }
    }

    /// A sliced frame shows what the whole frame shows within a level, but for a few corner
    /// pixels the rasterizer rounds up to a supersample differently at another position.
    #[test]
    fn a_sliced_frame_shows_what_the_whole_frame_shows() {
        let look = Look::default();
        let mut renderer = Renderer::new(None);
        for scale in [1, 2] {
            for active in [true, false] {
                for client in [
                    Size { w: 300, h: 120 },
                    Size { w: 640, h: 377 },
                    Size { w: 1200, h: 211 },
                ] {
                    let whole = renderer.render(&look, client, &appearance(scale, active, false));
                    let sliced = renderer.render(&look, client, &appearance(scale, active, true));
                    assert!(
                        sliced.parts.iter().any(|p| p.source.is_some()),
                        "the frame was sliced"
                    );
                    let origin = (
                        -super::super::BORDER - SHADOW,
                        -super::super::TITLE_HEIGHT - SHADOW,
                    );
                    let size = (
                        client.w + 2 * (super::super::BORDER + SHADOW),
                        client.h + 34 + 2 * SHADOW + 8,
                    );
                    let a = composite(&whole, origin, size, scale);
                    let b = composite(&sliced, origin, size, scale);
                    let diffs: Vec<u8> = a
                        .iter()
                        .zip(&b)
                        .map(|(x, y)| (0..4).map(|c| x[c].abs_diff(y[c])).max().unwrap_or(0))
                        .filter(|&d| d > 1)
                        .collect();
                    let worst = diffs.iter().copied().max().unwrap_or(0);
                    assert!(
                        diffs.len() <= 4 && worst <= 16,
                        "scale {scale}, active {active}, {client:?}: {} pixels off, by up to {worst}",
                        diffs.len()
                    );
                }
            }
        }
    }

    /// A resize hands every piece, the title's at the same sub-pixel phase included, again.
    #[test]
    fn a_resize_draws_no_piece_again() {
        let look = Look::default();
        let mut renderer = Renderer::new(None);
        let a = renderer.render(&look, Size { w: 500, h: 300 }, &appearance(1, true, true));
        let b = renderer.render(&look, Size { w: 702, h: 451 }, &appearance(1, true, true));
        assert_eq!(a.parts.len(), b.parts.len());
        for (a, b) in a.parts.iter().zip(&b.parts) {
            assert!(
                Arc::ptr_eq(&a.image.rgba, &b.image.rgba),
                "{:?} was drawn again",
                b.rect
            );
        }
    }

    /// A frame too small for the canonical pieces is drawn whole.
    #[test]
    fn a_small_frame_is_drawn_whole() {
        let look = Look::default();
        let mut renderer = Renderer::new(None);
        let frame = renderer.render(&look, Size { w: 60, h: 20 }, &appearance(1, true, true));
        assert!(frame.parts.iter().all(|p| p.source.is_none()));
    }
}
