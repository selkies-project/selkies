/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Wayland cursor shape resolution: converts a `CursorImageStatus` into a PNG image buffer
//! for the cursor sink. Uses `xcursor` to load the system cursor theme and render the
//! appropriate frame at the current scale.

use crate::sink::{CursorImage, CursorSink};
use image::{ImageBuffer, Rgba, RgbaImage};
use std::collections::HashMap;
use std::io::Cursor as IoCursor;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;
use xcursor::{
    CursorTheme,
    parser::{Image, parse_xcursor},
};

/// One unit of cursor-callback work handed from the calloop thread to the `wl-cursor`
/// worker. The calloop resolves everything renderer/surface-affine (SHM copies, dmabuf
/// readbacks, hotspots); the worker does the PNG encode, the cache, and the call into the
/// sink so none of that latency lands on the input/render thread. One channel keeps cursor
/// updates ordered with sink (re)registration.
pub enum CursorJob {
    /// `None` withdraws the sink, releasing what it holds.
    SetCallback(Option<Arc<dyn CursorSink>>),
    /// Reload the worker's theme handle at a new pixel size; later `Named` jobs render at it.
    SetSize(i32),
    /// Cap the longest delivered cursor edge in pixels; larger sprites are downscaled with
    /// the hotspot. `<= 0` delivers them uncapped.
    SetSizeCap(i32),
    Named {
        name: &'static str,
    },
    Hide,
    /// wl_shm cursor sprite: raw pool bytes plus the sub-image descriptor.
    Shm {
        hash: u64,
        width: i32,
        height: i32,
        stride: i32,
        offset: i32,
        opaque: bool,
        bytes: Vec<u8>,
        hot_x: i32,
        hot_y: i32,
    },
    /// dmabuf cursor sprite already read back to tightly-mapped RGBA on the calloop.
    Gles {
        hash: u64,
        width: i32,
        height: i32,
        bytes: Vec<u8>,
        hot_x: i32,
        hot_y: i32,
    },
}

/// Spawn the cursor delivery worker; returns its job channel. The worker owns its own
/// theme handle, the PNG cache, and the sink until one replaces or withdraws it (like the
/// compositor thread itself); `SHUTDOWN` gates every call into it.
pub fn spawn_cursor_worker(cursor_size: i32, size_cap: i32) -> std::sync::mpsc::Sender<CursorJob> {
    let (tx, rx) = std::sync::mpsc::channel::<CursorJob>();
    let _ = std::thread::Builder::new()
        .name("wl-cursor".into())
        .spawn(move || {
            let mut helper = Cursor::load(cursor_size);
            let mut cap = size_cap;
            let mut cache: HashMap<(u64, i32), CappedSprite> = HashMap::new();
            let mut callback: Option<Arc<dyn CursorSink>> = None;
            while let Ok(job) = rx.recv() {
                let (msg_type, data, hot_x, hot_y): (&'static str, Vec<u8>, i32, i32) = match job {
                    CursorJob::SetCallback(cb) => {
                        callback = cb;
                        continue;
                    }
                    CursorJob::SetSize(size) => {
                        helper = Cursor::load(size);
                        continue;
                    }
                    CursorJob::SetSizeCap(c) => {
                        cap = c;
                        continue;
                    }
                    CursorJob::Named { name } => match helper.get_sprite(name) {
                        Some((img, x, y)) => match cap_and_encode(img, cap) {
                            Some(sprite) => {
                                let (sx, sy) = scaled_hotspot(&sprite, x as i32, y as i32);
                                ("png", sprite.png, sx, sy)
                            }
                            None => ("error", Vec::new(), 0, 0),
                        },
                        None => ("error", Vec::new(), 0, 0),
                    },
                    CursorJob::Hide => ("hide", Vec::new(), 0, 0),
                    CursorJob::Shm {
                        hash,
                        width,
                        height,
                        stride,
                        offset,
                        opaque,
                        bytes,
                        hot_x,
                        hot_y,
                    } => capped_job(&mut cache, hash, cap, hot_x, hot_y, || {
                        decode_shm_cursor(width, height, stride, offset, opaque, &bytes)
                    }),
                    CursorJob::Gles {
                        hash,
                        width,
                        height,
                        bytes,
                        hot_x,
                        hot_y,
                    } => capped_job(&mut cache, hash, cap, hot_x, hot_y, || {
                        decode_gles_cursor(width, height, &bytes)
                    }),
                };
                // A sprite whose pixels could not be read yields empty data; suppressing it
                // preserves the consumer's last cursor instead of blanking it (only an
                // intentional hide passes with no payload).
                if data.is_empty() && msg_type != "hide" {
                    continue;
                }
                if crate::SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed) {
                    continue;
                }
                if let Some(cb) = &callback {
                    cb.cursor(CursorImage {
                        kind: msg_type,
                        png: data,
                        hot_x,
                        hot_y,
                    });
                }
            }
        });
    tx
}

/// A cursor sprite encoded at its delivered size: the straight-alpha PNG, the factor the
/// source was scaled by, and the payload's dimensions. Hotspots are per-job rather than a
/// property of the pixels, so they are scaled against `scale` instead of being stored here.
struct CappedSprite {
    png: Vec<u8>,
    scale: f64,
    width: u32,
    height: u32,
}

/// Sprites retained before arbitrary eviction; an evicted sprite re-encodes on next appearance.
const SPRITE_CACHE_MAX: usize = 100;

/// Apply the size cap to a **premultiplied** sprite and encode it as a straight-alpha PNG.
///
/// `cap <= 0`, or a sprite already within it, is encoded at its source size. Resizing happens
/// while the pixels are still premultiplied — filtering straight alpha pulls the (zero) color
/// of transparent texels into the edges and leaves a dark fringe, and Lanczos3 overshoot has no
/// headroom to land in — which is the order the X11 XFixes path uses.
fn cap_and_encode(mut img: RgbaImage, cap: i32) -> Option<CappedSprite> {
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return None;
    }
    let longest = w.max(h);
    let mut scale = 1.0;
    if cap > 0 && longest > cap as u32 {
        scale = f64::from(cap) / f64::from(longest);
        let nw = ((f64::from(w) * scale).round() as u32).max(1);
        let nh = ((f64::from(h) * scale).round() as u32).max(1);
        img = image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Lanczos3);
    }
    crate::unpremultiply_rgba(&mut img);
    let mut png = Vec::new();
    img.write_to(&mut IoCursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(CappedSprite {
        png,
        scale,
        width: img.width(),
        height: img.height(),
    })
}

/// Scale a source hotspot onto a delivered sprite. Consumers treat the hotspot as an offset
/// INTO the bitmap, so the rounded coordinate is clamped to the payload rather than allowed to
/// land one pixel past its right or bottom edge.
fn scaled_hotspot(sprite: &CappedSprite, hot_x: i32, hot_y: i32) -> (i32, i32) {
    (
        ((f64::from(hot_x) * sprite.scale).round() as i32).clamp(0, sprite.width as i32 - 1),
        ((f64::from(hot_y) * sprite.scale).round() as i32).clamp(0, sprite.height as i32 - 1),
    )
}

/// Cache wrapper for sprite jobs, keyed by `(sprite hash, cap)` so a repeat of a sprite already
/// delivered under the same cap is a plain clone of its PNG; a miss decodes the sprite, caps it,
/// and encodes it once. Keying on the cap keeps a live `SetSizeCap` from serving stale sizes.
fn capped_job(
    cache: &mut HashMap<(u64, i32), CappedSprite>,
    hash: u64,
    cap: i32,
    hot_x: i32,
    hot_y: i32,
    decode: impl FnOnce() -> Option<RgbaImage>,
) -> (&'static str, Vec<u8>, i32, i32) {
    use std::collections::hash_map::Entry;
    let key = (hash, cap);
    if cache.len() >= SPRITE_CACHE_MAX
        && !cache.contains_key(&key)
        && let Some(&evict) = cache.keys().next()
    {
        cache.remove(&evict);
    }
    let sprite = match cache.entry(key) {
        Entry::Occupied(e) => e.into_mut(),
        // An unreadable sprite yields an empty payload, which the worker suppresses so the
        // consumer keeps the cursor it already has.
        Entry::Vacant(e) => match decode().and_then(|img| cap_and_encode(img, cap)) {
            Some(sprite) => e.insert(sprite),
            None => return ("png", Vec::new(), 0, 0),
        },
    };
    let (sx, sy) = scaled_hotspot(sprite, hot_x, hot_y);
    ("png", sprite.png.clone(), sx, sy)
}

/// Longest side of a cursor surface still read: an application's own pointer can be
/// well past what is delivered (the cap downscales it), but a surface this large is not
/// a cursor at all and its copy is skipped.
const MAX_SPRITE_SIDE: i32 = 1024;

/// Read a wl_shm BGRA/XRGB sprite sub-image into a premultiplied RGBA image, the form
/// `cap_and_encode` must resize before it unpremultiplies. Stride/offset are clamped
/// non-negative with checked arithmetic so a garbage descriptor skips pixels instead of
/// panicking; a sprite past `MAX_SPRITE_SIDE` is ignored.
fn decode_shm_cursor(
    width: i32,
    height: i32,
    stride: i32,
    offset: i32,
    opaque: bool,
    raw_bytes: &[u8],
) -> Option<RgbaImage> {
    if width <= 0
        || height <= 0
        || width > MAX_SPRITE_SIDE
        || height > MAX_SPRITE_SIDE
        || raw_bytes.is_empty()
    {
        return None;
    }
    let mut img_buf = ImageBuffer::<Rgba<u8>, Vec<u8>>::new(width as u32, height as u32);
    let stride_usize = stride.max(0) as usize;
    let base_offset = offset.max(0) as usize;
    for y in 0..(height as u32) {
        for x in 0..(width as u32) {
            let offset = (y as usize)
                .checked_mul(stride_usize)
                .and_then(|row| base_offset.checked_add(row))
                .and_then(|o| o.checked_add((x as usize) * 4));
            let offset = match offset {
                Some(o) => o,
                None => continue,
            };
            if offset
                .checked_add(4)
                .is_some_and(|end| end <= raw_bytes.len())
            {
                let alpha = if opaque { 255 } else { raw_bytes[offset + 3] };
                img_buf.put_pixel(
                    x,
                    y,
                    Rgba([
                        raw_bytes[offset + 2],
                        raw_bytes[offset + 1],
                        raw_bytes[offset],
                        alpha,
                    ]),
                );
            }
        }
    }
    Some(img_buf)
}

/// Read a dmabuf sprite's RGBA readback (stride recovered from the mapping length) into a
/// premultiplied RGBA image.
fn decode_gles_cursor(width: i32, height: i32, raw_bytes: &[u8]) -> Option<RgbaImage> {
    if width <= 0
        || height <= 0
        || width > MAX_SPRITE_SIDE
        || height > MAX_SPRITE_SIDE
        || raw_bytes.is_empty()
    {
        return None;
    }
    let stride =
        super::frontend::rgba_readback_stride(raw_bytes.len(), height as usize, width as usize);
    let mut img_buf = ImageBuffer::<Rgba<u8>, Vec<u8>>::new(width as u32, height as u32);
    for y in 0..(height as u32) {
        for x in 0..(width as u32) {
            let offset = (y as usize * stride) + (x as usize * 4);
            if offset + 4 <= raw_bytes.len() {
                img_buf.put_pixel(
                    x,
                    y,
                    Rgba([
                        raw_bytes[offset],
                        raw_bytes[offset + 1],
                        raw_bytes[offset + 2],
                        raw_bytes[offset + 3],
                    ]),
                );
            }
        }
    }
    Some(img_buf)
}

/// The loaded XCursor theme, held for the whole capture so cursor lookups stay cheap.
///
/// The default cursor's frames are parsed once and kept here because they are consulted on nearly
/// every frame; the theme handle is retained alongside them so the rarer named cursors (`hand1`,
/// `text`, …) can still be resolved on demand. Everything is sized to the one resolved pixel size.
pub struct Cursor {
    icons: Vec<Image>,
    theme: CursorTheme,
    size: u32,
}

impl Cursor {
    /// Load the theme named by `XCURSOR_THEME` (default `"default"`) at the requested size.
    ///
    /// `size_override` comes from `CaptureSettings.cursor_size` (selkies `--cursor-size` /
    /// `XCURSOR_SIZE`); a value `<= 0` falls back to 24. A named theme that is not installed, or
    /// that has no default icon, gives way to the `default` theme the system points at, as
    /// libXcursor's lookup does; only where that has none either does a 16×16 solid-red
    /// placeholder stand in, so the caller always has a valid image.
    pub fn load(size_override: i32) -> Cursor {
        let name = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".into());
        let size: u32 = if size_override > 0 {
            size_override as u32
        } else {
            24
        };

        let mut theme = CursorTheme::load(&name);
        let mut icons = load_icon(&theme, "default");
        if icons.is_err() && name != "default" {
            crate::log::debug!(
                "[Wayland] Cursor theme {name:?} has no default cursor; using the default theme."
            );
            theme = CursorTheme::load("default");
            icons = load_icon(&theme, "default");
        }
        let icons = icons.unwrap_or_else(|_| {
            let size = 16;
            let mut pixels = Vec::with_capacity((size * size * 4) as usize);
            for _ in 0..(size * size) {
                pixels.extend_from_slice(&[255, 0, 0, 255]);
            }

            vec![Image {
                size,
                width: size,
                height: size,
                xhot: 0,
                yhot: 0,
                delay: 1,
                pixels_rgba: pixels,
                pixels_argb: vec![],
            }]
        });

        Cursor { icons, theme, size }
    }

    /// The default cursor's animation frame for `time`, at the theme size times `scale`.
    pub fn get_image(&self, scale: u32, time: Duration) -> Image {
        let size = self.size * scale;
        frame(time.as_millis() as u32, size, &self.icons)
    }

    /// A named cursor's animation frame for `time`, or `None` when the theme lacks it.
    pub fn get_image_by_name(&self, name: &str, scale: u32, time: Duration) -> Option<Image> {
        let icons = load_icon(&self.theme, name).ok()?;
        let size = self.size * scale;
        Some(frame(time.as_millis() as u32, size, &icons))
    }

    /// A named cursor icon as a premultiplied RGBA image plus its hotspot (x, y), ready for
    /// `cap_and_encode` to size and convert to the straight alpha web clients want. Xcursor
    /// stores premultiplied color, which is also what the compositing paths (`get_image*`)
    /// need for blending, so it is carried through unconverted.
    pub fn get_sprite(&self, name: &str) -> Option<(RgbaImage, u32, u32)> {
        let icons = load_icon(&self.theme, name).ok()?;
        let image_data = nearest_images(self.size, &icons).next()?;

        let img_buf: RgbaImage = ImageBuffer::from_raw(
            image_data.width,
            image_data.height,
            image_data.pixels_rgba.clone(),
        )?;

        Some((img_buf, image_data.xhot, image_data.yhot))
    }
}

/// All frames of the theme variant whose pixel size is closest to `size`. XCursor files
/// bundle the same cursor at several sizes, and choosing the nearest one avoids scaling a
/// mismatched bitmap into a blurry or aliased cursor.
fn nearest_images(size: u32, images: &[Image]) -> impl Iterator<Item = &Image> {
    let nearest_image = images
        .iter()
        .min_by_key(|image| (size as i32 - image.size as i32).abs())
        .unwrap();
    images.iter().filter(move |image| {
        image.width == nearest_image.width && image.height == nearest_image.height
    })
}

/// Pick which animation frame to show for the elapsed time, so animated cursors (a spinner,
/// a progress ring) actually advance instead of freezing on frame zero; it maps the time onto the
/// frames by cycling their cumulative delays.
fn frame(mut millis: u32, size: u32, images: &[Image]) -> Image {
    let total = nearest_images(size, images).fold(0, |acc, image| acc + image.delay);
    if total == 0 {
        return nearest_images(size, images).next().unwrap().clone();
    }
    millis %= total;
    for img in nearest_images(size, images) {
        if millis < img.delay {
            return img.clone();
        }
        millis -= img.delay;
    }
    unreachable!()
}

/// Parse the named icon from the theme's cursor file into its frames.
///
/// Empty parses are rejected so `nearest_images`'s `min_by_key().unwrap()` cannot panic on a
/// cursor file that has a valid header but zero images.
fn load_icon(theme: &CursorTheme, name: &str) -> Result<Vec<Image>, String> {
    let icon_path = theme.load_icon(name).ok_or("Icon not found")?;
    let mut cursor_file = std::fs::File::open(icon_path).map_err(|e| e.to_string())?;
    let mut cursor_data = Vec::new();
    cursor_file
        .read_to_end(&mut cursor_data)
        .map_err(|e| e.to_string())?;
    let imgs = parse_xcursor(&cursor_data).ok_or("Failed to parse".to_string())?;
    if imgs.is_empty() {
        return Err("Cursor file has no images".to_string());
    }
    Ok(imgs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_large_cursor_surface_is_read_and_capped() {
        // An application's 256px pointer as a wl_shm surface: read whole, then
        // brought down to the cap; a surface too large to be a cursor is skipped.
        let side = 256;
        let raw = vec![0xffu8; (side * side * 4) as usize];
        let img = decode_shm_cursor(side, side, side * 4, 0, false, &raw).unwrap();
        assert_eq!((img.width(), img.height()), (256, 256));
        let sprite = cap_and_encode(img, 128).unwrap();
        let png = image::load_from_memory(&sprite.png).unwrap();
        assert_eq!((png.width(), png.height()), (128, 128));
        assert!(decode_shm_cursor(2048, 2048, 2048 * 4, 0, false, &raw).is_none());
        assert!(decode_gles_cursor(2048, 2048, &raw).is_none());
    }

    #[test]
    fn cap_keeps_a_sprite_within_it_and_shrinks_one_past_it() {
        let whole = RgbaImage::from_pixel(128, 128, image::Rgba([0, 0, 0, 255]));
        let sprite = cap_and_encode(whole, 128).unwrap();
        let img = image::load_from_memory(&sprite.png).unwrap();
        assert_eq!((img.width(), img.height()), (128, 128));
        let big = RgbaImage::from_pixel(256, 64, image::Rgba([0, 0, 0, 255]));
        let sprite = cap_and_encode(big, 128).unwrap();
        let img = image::load_from_memory(&sprite.png).unwrap();
        assert_eq!((img.width(), img.height()), (128, 32));
    }
}
