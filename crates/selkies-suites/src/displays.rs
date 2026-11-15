/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Two displays on one X desktop, read from the wire: the layout the server calculated, each
//! display's region painted a color of its own on the root, and the JPEG stripes a client bound
//! to a display is sent, decoded.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ConfigureWindowAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, Rectangle, StackMode,
    Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::Server;

/// The color the primary display's region is painted.
pub const PRIMARY_RGB: [i64; 3] = [220, 30, 30];
/// The color the second display's region is painted.
pub const SECONDARY_RGB: [i64; 3] = [30, 60, 220];

/// A display's rectangle on the desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i64,
    pub y: i64,
    pub w: i64,
    pub h: i64,
}

/// A JPEG-encoder SETTINGS message binding a client to `display_id`, laid out to the right.
pub fn jpeg_settings(display_id: &str) -> String {
    let settings = serde_json::json!({
        "displayId": display_id, "initialClientWidth": 1280, "initialClientHeight": 720,
        "manual_resolution": false, "framerate": 30, "encoder": "jpeg",
        "video_crf": 25, "video_bitrate": 6000, "audio_bitrate": 128_000,
        "scaling_dpi": 96, "displayPosition": "right",
    });
    format!("SETTINGS,{settings}")
}

/// The layout the server last calculated, by display id, from its log.
pub fn layout(server: &Server) -> Option<BTreeMap<String, Rect>> {
    let log = server.log();
    let line = log.lines().rfind(|l| l.contains("Layout calculated"))?;
    let (_, literal) = line.split_once("Layouts: ")?;
    let value: serde_json::Value =
        serde_json::from_str(&literal.trim().replace('\'', "\"")).ok()?;
    value
        .as_object()?
        .iter()
        .map(|(id, rect)| {
            let field = |name: &str| rect.get(name).and_then(serde_json::Value::as_i64);
            Some((
                id.clone(),
                Rect {
                    x: field("x")?,
                    y: field("y")?,
                    w: field("w")?,
                    h: field("h")?,
                },
            ))
        })
        .collect()
}

/// Waits up to thirty seconds for the X root to grow to at least `width`.
pub async fn wait_root_width(display: &str, width: i64) -> Option<(u16, u16)> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if let Some(size) = crate::x11::root_size(display)
            && i64::from(size.0) >= width
        {
            return Some(size);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    None
}

fn coord(v: i64) -> i16 {
    i16::try_from(v).unwrap_or(if v < 0 { i16::MIN } else { i16::MAX })
}

fn side(v: i64) -> u16 {
    u16::try_from(v.max(0)).unwrap_or(u16::MAX)
}

fn pixel(rgb: [i64; 3]) -> u32 {
    u32::try_from((rgb[0] << 16) | (rgb[1] << 8) | rgb[2]).unwrap_or(0)
}

/// A connection to the desktop that paints on its root; what it drew stays after it closes.
pub struct Desktop {
    conn: RustConnection,
    root: Window,
}

impl Desktop {
    /// # Panics
    ///
    /// When the display does not answer.
    pub fn open(display: &str) -> Desktop {
        let (conn, screen) = RustConnection::connect(Some(display)).expect("the X display");
        let root = conn.setup().roots[screen].root;
        Desktop { conn, root }
    }

    fn fill(&self, rgb: [i64; 3], rect: Rectangle) {
        let Ok(gc) = self.conn.generate_id() else {
            return;
        };
        let _ = self
            .conn
            .create_gc(gc, self.root, &CreateGCAux::new().foreground(pixel(rgb)));
        let _ = self.conn.poly_fill_rectangle(self.root, gc, &[rect]);
        let _ = self.conn.free_gc(gc);
        let _ = self.conn.sync();
    }

    /// Fills each display's region with its own color: the primary's [`PRIMARY_RGB`], any other's
    /// [`SECONDARY_RGB`].
    pub fn paint(&self, layout: &BTreeMap<String, Rect>) {
        for (id, rect) in layout {
            let rgb = if id == "primary" {
                PRIMARY_RGB
            } else {
                SECONDARY_RGB
            };
            self.fill(
                rgb,
                Rectangle {
                    x: coord(rect.x),
                    y: coord(rect.y),
                    width: side(rect.w),
                    height: side(rect.h),
                },
            );
        }
    }

    /// Fills a band `height` pixels tall centered on `y`, across `rect`, with `rgb`.
    pub fn band(&self, rect: Rect, y: i64, height: i64, rgb: [i64; 3]) {
        self.fill(
            rgb,
            Rectangle {
                x: coord(rect.x),
                y: coord((y - height / 2).max(0)),
                width: side(rect.w),
                height: side(height),
            },
        );
    }

    /// Where the pointer is on the root.
    pub fn pointer(&self) -> Option<(i64, i64)> {
        let reply = self.conn.query_pointer(self.root).ok()?.reply().ok()?;
        Some((i64::from(reply.root_x), i64::from(reply.root_y)))
    }

    /// An input-only window over the whole desktop carrying a solid cursor `size` pixels a side,
    /// so the displayed cursor is the test's rather than whatever the desktop last set, and
    /// nothing covers what was painted. It lasts as long as this connection.
    pub fn cursor_window(&self, size: u16) {
        let Some((width, height)) = self
            .conn
            .get_geometry(self.root)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|g| (g.width, g.height))
        else {
            return;
        };
        let ids: Option<(u32, u32, u32, u32)> = (|| {
            Some((
                self.conn.generate_id().ok()?,
                self.conn.generate_id().ok()?,
                self.conn.generate_id().ok()?,
                self.conn.generate_id().ok()?,
            ))
        })();
        let Some((shape, gc, cursor, win)) = ids else {
            return;
        };
        let _ = self.conn.create_pixmap(1, shape, self.root, size, size);
        let _ = self
            .conn
            .create_gc(gc, shape, &CreateGCAux::new().foreground(1).background(0));
        let _ = self.conn.poly_fill_rectangle(
            shape,
            gc,
            &[Rectangle {
                x: 0,
                y: 0,
                width: size,
                height: size,
            }],
        );
        let _ = self.conn.create_cursor(
            cursor,
            shape,
            shape,
            0,
            0,
            0,
            65535,
            65535,
            65535,
            size / 2,
            size / 2,
        );
        let _ = self.conn.create_window(
            0,
            win,
            self.root,
            0,
            0,
            width,
            height,
            0,
            WindowClass::INPUT_ONLY,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new().override_redirect(1).cursor(cursor),
        );
        let _ = self.conn.map_window(win);
        let _ = self
            .conn
            .configure_window(win, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE));
        let _ = self.conn.free_gc(gc);
        let _ = self.conn.free_pixmap(shape);
        let _ = self.conn.sync();
    }
}

/// One JPEG stripe of a display's stream.
pub struct Stripe {
    /// The frame it belongs to.
    pub frame: u16,
    /// The row of the display it starts at.
    pub y: u16,
    /// Its image.
    pub jpeg: Vec<u8>,
}

/// The JPEG stripes among a client's binary payloads, in arrival order.
pub fn stripes(payloads: Vec<Vec<u8>>) -> Vec<Stripe> {
    payloads
        .into_iter()
        .filter(|m| m.len() > 6 && m[0] == 0x03)
        .map(|m| Stripe {
            frame: u16::from_be_bytes([m[2], m[3]]),
            y: u16::from_be_bytes([m[4], m[5]]),
            jpeg: m[6..].to_vec(),
        })
        .collect()
}

/// A JPEG decoded to RGB: its width, its height and its pixels, three bytes each.
pub fn decode_rgb(jpeg: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    use zune_core::colorspace::ColorSpace;
    use zune_core::options::DecoderOptions;
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(
        zune_core::bytestream::ZCursor::new(jpeg),
        options,
    );
    let pixels = decoder.decode().ok()?;
    let (w, h) = decoder.dimensions()?;
    (pixels.len() == w * h * 3).then_some((w, h, pixels))
}
