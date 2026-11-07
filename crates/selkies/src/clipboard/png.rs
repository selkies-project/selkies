/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The PNG offered beside a client's picture in another format.

use std::io::Cursor;

use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::metadata::Orientation;
use image::{ColorType, DynamicImage, ImageDecoder, ImageReader};
use log::debug;

/// A PNG of an image a client sent in another format, to offer beside it.
///
/// Chromium pastes no image type but PNG from either selection, and neither do most other
/// applications, while a picture uploaded from disk arrives as the JPEG or WebP it was saved
/// as. The pixels are decoded as the browser that sent them showed them, EXIF orientation
/// included, and deflated at the fastest level, since a clipboard copy is read once, nearby;
/// a format the decoder cannot read is offered only as it came.
///
/// Returns the PNG, or None for anything but a decodable non-PNG image.
pub fn png_beside(mime: &str, data: &[u8]) -> Option<Vec<u8>> {
    if !mime.starts_with("image/") || mime == "image/png" {
        return None;
    }
    match convert(data) {
        Ok(png) => Some(png),
        Err(e) => {
            debug!(target: "input", "clipboard {mime} offered without a PNG beside it: {e}");
            None
        }
    }
}

/// Decode `data`, apply its orientation, and encode it as a PNG.
fn convert(data: &[u8]) -> Result<Vec<u8>, image::ImageError> {
    let reader = ImageReader::new(Cursor::new(data)).with_guessed_format()?;
    let mut decoder = reader.into_decoder()?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut frame = DynamicImage::from_decoder(decoder)?;
    frame.apply_orientation(orientation);
    let frame = match frame.color() {
        ColorType::L8
        | ColorType::La8
        | ColorType::Rgb8
        | ColorType::Rgba8
        | ColorType::L16
        | ColorType::La16
        | ColorType::Rgb16
        | ColorType::Rgba16 => frame,
        other if other.has_alpha() => DynamicImage::ImageRgba8(frame.to_rgba8()),
        _ => DynamicImage::ImageRgb8(frame.to_rgb8()),
    };
    let mut out = Vec::new();
    let encoder =
        PngEncoder::new_with_quality(&mut out, CompressionType::Fast, FilterType::Adaptive);
    frame.write_with_encoder(encoder)?;
    Ok(out)
}
