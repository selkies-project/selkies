/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! PNG watermark overlay composited onto captured frames before encoding.
//!
//! Supports static positioning (top-left, top-right, bottom-left, bottom-right, center) and an
//! animated "DVD-screensaver" bounce mode. The watermark is rendered into the compositor frame on
//! the GPU path (no readback) or blitted onto the host-ARGB buffer on the CPU path.

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            ImportMem, Renderer, Texture,
            element::{
                Kind,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
            },
        },
    },
    utils::{Physical, Point, Rectangle, Transform},
};
use std::path::Path;

#[derive(Clone, Copy, PartialEq)]
/// Watermark anchor: corners (TL/TR/BL/BR), middle (MI), or bouncing (AN).
pub enum WatermarkLocation {
    None = 0,
    TL = 1,
    TR = 2,
    BL = 3,
    BR = 4,
    MI = 5,
    AN = 6,
}

impl From<i32> for WatermarkLocation {
    fn from(v: i32) -> Self {
        match v {
            1 => Self::TL,
            2 => Self::TR,
            3 => Self::BL,
            4 => Self::BR,
            5 => Self::MI,
            6 => Self::AN,
            _ => Self::None,
        }
    }
}

/// The watermark's uploaded pixels plus its current placement and bounce state, kept across
/// frames so a moving (bouncing) watermark can be advanced and re-placed each tick without
/// re-reading or re-uploading the image.
pub struct OverlayState {
    wm_width: u32,
    wm_height: u32,
    wm_pos_x: i32,
    wm_pos_y: i32,
    wm_prev_pos: Option<(i32, i32)>,
    wm_velocity_x: f64,
    wm_velocity_y: f64,
    wm_subpixel_x: f64,
    wm_subpixel_y: f64,
    wm_loaded: bool,
    is_animated: bool,
    wm_pixels: Vec<u8>,
    render_buffer: Option<MemoryRenderBuffer>,
}

impl Default for OverlayState {
    fn default() -> Self {
        Self {
            wm_width: 0,
            wm_height: 0,
            wm_pos_x: 0,
            wm_pos_y: 0,
            wm_prev_pos: None,
            wm_velocity_x: 2.0,
            wm_velocity_y: 2.0,
            wm_subpixel_x: 0.0,
            wm_subpixel_y: 0.0,
            wm_loaded: false,
            is_animated: false,
            wm_pixels: Vec::new(),
            render_buffer: None,
        }
    }
}

/// Alpha-blend a source pixel (pre-split into r,g,b,a) over a BGRA destination pixel.
///
/// Overlay pixels are overwhelmingly either fully opaque or fully transparent, and this runs per
/// pixel per frame on the CPU, so the two extremes are special-cased to skip the blend arithmetic
/// entirely: an opaque source (`a == 255`) simply overwrites, a fully transparent source (`a == 0`)
/// is left as-is, and only genuine edge pixels pay for the integer source-over. In every case only
/// the B / G / R bytes are written; the destination's alpha byte is left as the capture delivered it.
#[inline]
pub(crate) fn blend_pixel(dst: &mut [u8], r: u8, g: u8, b: u8, a: u8) {
    if a == 255 {
        dst[0] = b;
        dst[1] = g;
        dst[2] = r;
    } else if a > 0 {
        let ia = 255 - a as u32;
        dst[0] = ((b as u32 * a as u32 + dst[0] as u32 * ia) / 255) as u8;
        dst[1] = ((g as u32 * a as u32 + dst[1] as u32 * ia) / 255) as u8;
        dst[2] = ((r as u32 * a as u32 + dst[2] as u32 * ia) / 255) as u8;
    }
}

/// Source-over compositing for PREMULTIPLIED sources (XFixes/Xcursor pixels are
/// premultiplied by format definition): dst = src + dst*(1-a). The straight-alpha
/// `blend_pixel` on premultiplied input multiplies alpha in twice and darkens
/// every translucent pixel (visible dark fringes on the composited cursor).
///
/// The sum saturates: some toolkits ship cursors whose color exceeds its alpha, and on those
/// a wrapping cast would turn an over-bright pixel into a dark one.
pub(crate) fn blend_pixel_premultiplied(dst: &mut [u8], r: u8, g: u8, b: u8, a: u8) {
    if a == 255 {
        dst[0] = b;
        dst[1] = g;
        dst[2] = r;
    } else if a > 0 {
        let ia = 255 - a as u32;
        dst[0] = (b as u32 + dst[0] as u32 * ia / 255).min(255) as u8;
        dst[1] = (g as u32 + dst[1] as u32 * ia / 255).min(255) as u8;
        dst[2] = (r as u32 + dst[2] as u32 * ia / 255).min(255) as u8;
    }
}

impl OverlayState {
    /// Load the watermark image from disk; `output_scale` is the output's fractional
    /// scale, ceiled to the integer buffer scale of the upload. A failed load clears the overlay.
    pub fn load_watermark(&mut self, path: &str, output_scale: f64) {
        if let Ok(img) = image::open(Path::new(path)) {
            let rgba = img.to_rgba8();
            self.wm_width = rgba.width();
            self.wm_height = rgba.height();
            self.wm_loaded = true;
            let buffer_scale = output_scale.ceil().max(1.0) as i32;

            let pixels = rgba.into_vec();
            self.render_buffer = Some(MemoryRenderBuffer::from_slice(
                &pixels,
                Fourcc::Abgr8888,
                (self.wm_width as i32, self.wm_height as i32),
                buffer_scale,
                Transform::Normal,
                None,
            ));
            self.wm_pixels = pixels;
        } else {
            self.wm_loaded = false;
            self.wm_pixels = Vec::new();
            self.render_buffer = None;
        }
    }

    /// Alpha-blend the watermark into a BGRA frame (row `stride` in bytes) at its
    /// current position. Clips per pixel at the frame bounds because the animated
    /// position — or a watermark larger than the capture — can leave part of the
    /// image off-frame, and only the in-frame portion may be written.
    pub fn blend_bgra(&self, frame: &mut [u8], stride: usize, frame_w: i32, frame_h: i32) {
        if !self.wm_loaded {
            return;
        }
        let (w, h) = (self.wm_width as i32, self.wm_height as i32);
        for y in 0..h {
            let ty = self.wm_pos_y + y;
            if ty < 0 || ty >= frame_h {
                continue;
            }
            for x in 0..w {
                let tx = self.wm_pos_x + x;
                if tx < 0 || tx >= frame_w {
                    continue;
                }
                let src = ((y * w + x) * 4) as usize;
                let (r, g, b, a) = (
                    self.wm_pixels[src],
                    self.wm_pixels[src + 1],
                    self.wm_pixels[src + 2],
                    self.wm_pixels[src + 3],
                );
                let off = ty as usize * stride + tx as usize * 4;
                blend_pixel(&mut frame[off..off + 4], r, g, b, a);
            }
        }
    }

    /// Frame-clipped union of the watermark's current and previous rectangles —
    /// the region a damage-gated encoder must repaint after a bounce step moved
    /// the image (the vacated area needs repainting as much as the new one).
    pub fn damage_rect(&self, frame_w: i32, frame_h: i32) -> Option<Rectangle<i32, Physical>> {
        if !self.wm_loaded {
            return None;
        }
        let (w, h) = (self.wm_width as i32, self.wm_height as i32);
        let (mut x0, mut y0) = (self.wm_pos_x, self.wm_pos_y);
        let (mut x1, mut y1) = (x0 + w, y0 + h);
        if let Some((px, py)) = self.wm_prev_pos {
            x0 = x0.min(px);
            y0 = y0.min(py);
            x1 = x1.max(px + w);
            y1 = y1.max(py + h);
        }
        x0 = x0.max(0);
        y0 = y0.max(0);
        x1 = x1.min(frame_w);
        y1 = y1.min(frame_h);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        Some(Rectangle::new((x0, y0).into(), (x1 - x0, y1 - y0).into()))
    }

    /// True once a watermark image has been loaded.
    pub fn is_active(&self) -> bool {
        self.wm_loaded
    }

    /// The loaded image and its size, for a backend that hands it to the GPU once instead of
    /// blending it into every frame's host pixels. Straight alpha, R,G,B,A per pixel.
    pub fn sprite(&self) -> Option<(&[u8], u32, u32)> {
        self.wm_loaded
            .then_some((self.wm_pixels.as_slice(), self.wm_width, self.wm_height))
    }

    /// Where [`Self::update_position`] last put the image's top-left corner in the frame.
    pub fn position(&self) -> (i32, i32) {
        (self.wm_pos_x, self.wm_pos_y)
    }

    /// True when the watermark moves and must be re-rendered every frame.
    pub fn is_animated(&self) -> bool {
        self.is_animated
    }

    /// Place the watermark for the current frame size. Fixed anchors (corners / middle) are
    /// pure geometry, but the `AN` anchor makes the watermark bounce, so each call also advances
    /// that animation one step and reflects it off the frame edges — which is precisely why an `AN`
    /// watermark has to be re-rendered every frame (see `is_animated`). `loc_enum` is the i32 form
    /// of `WatermarkLocation`.
    pub fn update_position(&mut self, frame_width: i32, frame_height: i32, loc_enum: i32) {
        if !self.wm_loaded {
            return;
        }

        let loc = WatermarkLocation::from(loc_enum);
        let w = self.wm_width as i32;
        let h = self.wm_height as i32;

        self.wm_prev_pos = Some((self.wm_pos_x, self.wm_pos_y));
        self.is_animated = matches!(loc, WatermarkLocation::AN);

        match loc {
            WatermarkLocation::TL => {
                self.wm_pos_x = 0;
                self.wm_pos_y = 0;
            }
            WatermarkLocation::TR => {
                self.wm_pos_x = frame_width - w;
                self.wm_pos_y = 0;
            }
            WatermarkLocation::BL => {
                self.wm_pos_x = 0;
                self.wm_pos_y = frame_height - h;
            }
            WatermarkLocation::BR => {
                self.wm_pos_x = frame_width - w;
                self.wm_pos_y = frame_height - h;
            }
            WatermarkLocation::MI => {
                self.wm_pos_x = (frame_width - w) / 2;
                self.wm_pos_y = (frame_height - h) / 2;
            }
            WatermarkLocation::AN => {
                self.wm_subpixel_x += self.wm_velocity_x;
                self.wm_subpixel_y += self.wm_velocity_y;

                if self.wm_subpixel_x <= 0.0 {
                    self.wm_subpixel_x = 0.0;
                    self.wm_velocity_x = self.wm_velocity_x.abs();
                } else if self.wm_subpixel_x + (w as f64) >= frame_width as f64 {
                    self.wm_subpixel_x = (frame_width - w) as f64;
                    self.wm_velocity_x = -self.wm_velocity_x.abs();
                }

                if self.wm_subpixel_y <= 0.0 {
                    self.wm_subpixel_y = 0.0;
                    self.wm_velocity_y = self.wm_velocity_y.abs();
                } else if self.wm_subpixel_y + (h as f64) >= frame_height as f64 {
                    self.wm_subpixel_y = (frame_height - h) as f64;
                    self.wm_velocity_y = -self.wm_velocity_y.abs();
                }

                self.wm_pos_x = self.wm_subpixel_x as i32;
                self.wm_pos_y = self.wm_subpixel_y as i32;
            }
            WatermarkLocation::None => {}
        }
    }

    /// Render element for the watermark; `None` when no watermark is loaded.
    pub fn get_watermark_element<R>(
        &self,
        renderer: &mut R,
    ) -> Option<MemoryRenderBufferRenderElement<R>>
    where
        R: Renderer + ImportMem,
        R::TextureId: Texture + Clone + Send + 'static,
    {
        if let Some(buffer) = &self.render_buffer {
            let location =
                Point::<f64, Physical>::from((self.wm_pos_x as f64, self.wm_pos_y as f64));
            MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                location,
                buffer,
                Some(1.0),
                None,
                None,
                Kind::Unspecified,
            )
            .ok()
        } else {
            None
        }
    }

    /// Render element for a software cursor `image` at logical `pos` on an output with
    /// fractional `scale`; the position converts to physical here so the composited cursor
    /// lands where the damage tracker and clients (which work in physical pixels) expect it.
    pub fn get_cursor_element<R>(
        &self,
        renderer: &mut R,
        image: xcursor::parser::Image,
        pos: Point<f64, smithay::utils::Logical>,
        scale: f64,
    ) -> Option<MemoryRenderBufferRenderElement<R>>
    where
        R: Renderer + ImportMem,
        R::TextureId: Texture + Clone + Send + 'static,
    {
        let buffer = MemoryRenderBuffer::from_slice(
            &image.pixels_rgba,
            Fourcc::Abgr8888,
            (image.width as i32, image.height as i32),
            1,
            Transform::Normal,
            None,
        );

        let hot: Point<i32, smithay::utils::Physical> =
            (image.xhot as i32, image.yhot as i32).into();
        let phys_pos = pos
            .to_physical(smithay::utils::Scale::from(scale))
            .to_i32_round();

        MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            (phys_pos - hot).to_f64(),
            &buffer,
            Some(1.0),
            None,
            None,
            Kind::Cursor,
        )
        .ok()
    }
}
