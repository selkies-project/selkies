/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The X11 cursor as a client message (`cursor_to_msg`): an XFixes image cropped to its visible
//! box, brought down to the delivery cap, straightened from premultiplied alpha, and encoded as
//! a base64 PNG with the content handle the live pixelflux path computes for the same shape.
//!
//! The cap bounds what is delivered, not the theme: an application's own sprite (a game's 128px
//! pointer) travels whole up to what a browser shows as a cursor, and a theme size past that
//! raises it further. XFixes pixels are premultiplied and are straightened only after any resize
//! (resampling is linear per channel in premultiplied space), matching pixelflux's cursor
//! pipeline; the handle shares its pixel-content space so the connect-time seed and the live
//! path dedupe one shape to one client cache entry.

use std::io::Cursor as IoCursor;

use base64::Engine as _;
use image::imageops::FilterType;
use image::{ImageFormat, RgbaImage};
use serde::Serialize;

use super::x11_link::XCursorImage;

/// The most a browser shows as a cursor image, and so the default delivery cap.
pub const DEFAULT_MAX_CURSOR_SIZE: u32 = 128;

/// One cursor as the clients receive it.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct CursorMessage {
    /// Base64 PNG; empty for a hidden cursor.
    pub curdata: String,
    pub width: u32,
    pub height: u32,
    pub hotx: u32,
    pub hoty: u32,
    /// Content handle; 0 means hide.
    pub handle: u32,
}

/// Convert premultiplied RGBA to straight alpha, the integer math pixelflux uses:
/// `floor((c * 255 + a / 2) / a)` clamped to 255, alpha-0 color forced to 0, so the seed and the
/// live path hash a cursor to the same content handle.
pub fn unpremultiply_rgba(img: &mut RgbaImage) {
    for px in img.pixels_mut() {
        let a = px[3] as u32;
        if a == 0 {
            px[0] = 0;
            px[1] = 0;
            px[2] = 0;
        } else if a < 255 {
            for c in &mut px.0[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

/// Encoder-independent cursor cache handle: a CRC over the straight-alpha pixels and geometry
/// rather than the PNG bytes, so two PNG encoders agree on one handle per shape. Never 0, which
/// the wire reserves for hide.
pub fn cursor_content_handle(rgba: &[u8], width: u32, height: u32, hot_x: u32, hot_y: u32) -> u32 {
    let mut crc = flate2::Crc::new();
    crc.update(rgba);
    let mut meta = Vec::with_capacity(16);
    meta.extend_from_slice(&(width as i32).to_le_bytes());
    meta.extend_from_slice(&(height as i32).to_le_bytes());
    meta.extend_from_slice(&(hot_x as i32).to_le_bytes());
    meta.extend_from_slice(&(hot_y as i32).to_le_bytes());
    crc.update(&meta);
    match crc.sum() {
        0 => 1,
        sum => sum,
    }
}

/// Encode an XFixes cursor image into the client cursor message at `size_cap`: crop to the
/// visible bounding box (clamping the hotspot with it, since browsers clamp a negative CSS
/// cursor hotspot to 0 silently), resize down to the cap, un-premultiply, and PNG-encode.
pub fn cursor_to_msg(cursor: &XCursorImage, size_cap: u32, debug_log: bool) -> CursorMessage {
    if cursor.width == 0
        || cursor.height == 0
        || cursor.pixels.len() < (cursor.width * cursor.height) as usize
    {
        return CursorMessage::default();
    }
    let (w, h) = (cursor.width, cursor.height);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for argb in &cursor.pixels[..(w * h) as usize] {
        rgba.extend_from_slice(&[
            (argb >> 16) as u8,
            (argb >> 8) as u8,
            *argb as u8,
            (argb >> 24) as u8,
        ]);
    }
    let Some(img) = RgbaImage::from_raw(w, h, rgba) else {
        return CursorMessage::default();
    };
    let mut bbox: Option<(u32, u32, u32, u32)> = None;
    for (x, y, px) in img.enumerate_pixels() {
        if px[3] != 0 {
            bbox = Some(match bbox {
                None => (x, y, x + 1, y + 1),
                Some((l, t, r, b)) => (l.min(x), t.min(y), r.max(x + 1), b.max(y + 1)),
            });
        }
    }
    let Some((left, upper, right, lower)) = bbox else {
        return CursorMessage::default();
    };
    let mut cropped =
        image::imageops::crop_imm(&img, left, upper, right - left, lower - upper).to_image();
    let mut hotx = (cursor.xhot - left as i32).max(0) as u32;
    let mut hoty = (cursor.yhot - upper as i32).max(0) as u32;
    if cropped.width() > size_cap || cropped.height() > size_cap {
        if debug_log {
            log::info!(target: "input", "Cursor ({}x{}) exceeds cap ({size_cap}x{size_cap}). Resizing.", cropped.width(), cropped.height());
        }
        let max_dim = cropped.width().max(cropped.height()) as f64;
        let scale = size_cap as f64 / max_dim;
        let new_w = ((cropped.width() as f64 * scale) as u32).max(1);
        let new_h = ((cropped.height() as f64 * scale) as u32).max(1);
        cropped = image::imageops::resize(&cropped, new_w, new_h, FilterType::Lanczos3);
        hotx = ((hotx as f64 * scale).round() as u32).min(new_w.saturating_sub(1));
        hoty = ((hoty as f64 * scale).round() as u32).min(new_h.saturating_sub(1));
    }
    // After the resize, never before (module docstring).
    unpremultiply_rgba(&mut cropped);
    let mut png = IoCursor::new(Vec::new());
    if cropped.write_to(&mut png, ImageFormat::Png).is_err() {
        return CursorMessage::default();
    }
    let (cw, ch) = (cropped.width(), cropped.height());
    let handle = cursor_content_handle(cropped.as_raw(), cw, ch, hotx, hoty);
    CursorMessage {
        curdata: base64::engine::general_purpose::STANDARD.encode(png.into_inner()),
        width: cw,
        height: ch,
        hotx,
        hoty,
        handle,
    }
}

/// `cursor_to_msg` behind a one-entry cache keyed by the XFixes cursor serial and the size cap
/// the encode depends on, so the per-connect fetch encodes an unchanged cursor only once.
#[derive(Default)]
pub struct CursorEncoder {
    cache: Option<((u32, u32), CursorMessage)>,
}

impl CursorEncoder {
    /// Encode, or return the cached message for the same serial and cap.
    pub fn encode(
        &mut self,
        cursor: &XCursorImage,
        size_cap: u32,
        debug_log: bool,
    ) -> CursorMessage {
        let key = (cursor.serial, size_cap);
        if let Some((k, msg)) = &self.cache
            && *k == key
        {
            return msg.clone();
        }
        let msg = cursor_to_msg(cursor, size_cap, debug_log);
        self.cache = Some((key, msg.clone()));
        msg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An XFixes cursor image: opaque black, premultiplied ARGB words.
    fn xcursor(size: u32, serial: u32) -> XCursorImage {
        XCursorImage {
            width: size,
            height: size,
            xhot: (size / 2) as i32,
            yhot: (size / 2) as i32,
            serial,
            pixels: vec![0xFF00_0000; (size * size) as usize],
        }
    }

    fn delivered(msg: &CursorMessage) -> ((u32, u32), (u32, u32)) {
        let png = base64::engine::general_purpose::STANDARD
            .decode(&msg.curdata)
            .unwrap();
        let img = image::load_from_memory(&png).unwrap();
        ((img.width(), img.height()), (msg.hotx, msg.hoty))
    }

    /// `tests/unit/test_cursor_cap.py`.
    #[test]
    fn delivery_cap() {
        assert_eq!(
            DEFAULT_MAX_CURSOR_SIZE, 128,
            "the default delivery cap is a browser's cursor ceiling"
        );
        let (size, hot) = delivered(&cursor_to_msg(&xcursor(128, 1), 128, false));
        assert_eq!(
            (size, hot),
            ((128, 128), (64, 64)),
            "a 128px application cursor is delivered whole at the default cap"
        );
        let (size, hot) = delivered(&cursor_to_msg(&xcursor(256, 2), 128, false));
        assert_eq!(
            (size, hot),
            ((128, 128), (64, 64)),
            "a sprite past the cap is brought down to it, hotspot with it"
        );
        let (size, hot) = delivered(&cursor_to_msg(&xcursor(256, 3), 256, false));
        assert_eq!(
            (size, hot),
            ((256, 256), (128, 128)),
            "a raised cap (a higher DPI) lets the larger sprite through"
        );
        let (size, _) = delivered(&cursor_to_msg(&xcursor(24, 4), 128, false));
        assert_eq!(
            size,
            (24, 24),
            "a theme-sized cursor is untouched by the cap"
        );
    }

    /// `tests/unit/test_input_housekeeping.py`: the per-connect fetch reuses the encode.
    #[test]
    fn encode_cache() {
        let mut enc = CursorEncoder::default();
        let a = enc.encode(&xcursor(8, 7), 64, false);
        let again = enc.encode(&xcursor(8, 7), 64, false);
        assert_eq!(a, again);
        let b = enc.encode(&xcursor(8, 8), 64, false);
        assert_eq!(b.width, 8);
        let c = enc.encode(&xcursor(8, 8), 4, false);
        assert_eq!(c.width, 4, "a changed size cap re-encodes");
        assert_ne!(a.handle, 0);
        let mut img = RgbaImage::from_raw(1, 1, vec![64, 64, 64, 128]).unwrap();
        unpremultiply_rgba(&mut img);
        assert_eq!(img.as_raw(), &[128, 128, 128, 128]);
    }
}
