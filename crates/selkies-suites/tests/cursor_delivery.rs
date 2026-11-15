/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Where a client's cursor shapes come from on X11.
//!
//! Two sources feed one client message. A connecting client is seeded straight from XFixes,
//! because pixelflux only delivers a cursor while something is capturing; every later shape
//! arrives from pixelflux's own XFixes thread. There is no cursor loop of the server's own between
//! them, so this pins that the seed lands, that a change made while streaming reaches the client,
//! and that a change made while nothing captures is not lost: the capture restart re-fetches it.

use std::time::Duration;

use serde_json::json;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ChangeWindowAttributesAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, Font, Rectangle,
    Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use selkies_suites::server::test_display;
use selkies_suites::{Checks, Client, server};

/// Two X font-cursor glyphs whose sprites differ, so a delivered PNG identifies which one the
/// server read.
const GLYPH_A: u16 = 68; // XC_hand2
const GLYPH_B: u16 = 150; // XC_spraycan

/// An override-redirect window under the pointer whose cursor the test sets.
///
/// The displayed cursor, the one XFixes reports, is the pointer window's, so the shape is driven
/// from a window this test owns rather than from the root, which the session's window manager
/// covers.
struct CursorWindow {
    conn: RustConnection,
    win: Window,
    font: Font,
}

impl CursorWindow {
    fn open(display: &str) -> CursorWindow {
        let (conn, screen) = RustConnection::connect(Some(display)).expect("the X display");
        let (root, white) = {
            let screen = &conn.setup().roots[screen];
            (screen.root, screen.white_pixel)
        };
        let font = conn.generate_id().expect("an id");
        conn.open_font(font, b"cursor").expect("the cursor font");
        let win = conn.generate_id().expect("an id");
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            win,
            root,
            300,
            300,
            200,
            200,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new()
                .background_pixel(white)
                .override_redirect(1),
        )
        .expect("the window");
        conn.map_window(win).expect("map");
        conn.warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, 380, 380)
            .expect("warp");
        conn.sync().expect("sync");
        std::thread::sleep(Duration::from_millis(500));
        CursorWindow { conn, win, font }
    }

    fn show(&self, cursor: u32) {
        self.conn
            .change_window_attributes(self.win, &ChangeWindowAttributesAux::new().cursor(cursor))
            .expect("the cursor");
        self.conn.sync().expect("sync");
    }

    fn set(&self, glyph: u16) {
        let cursor = self.conn.generate_id().expect("an id");
        self.conn
            .create_glyph_cursor(
                cursor,
                self.font,
                self.font,
                glyph,
                glyph + 1,
                0,
                0,
                0,
                65535,
                65535,
                65535,
            )
            .expect("a glyph cursor");
        self.show(cursor);
    }

    /// A solid bitmap cursor of `size` pixels a side: what an application with its own large
    /// pointer hands the server.
    fn set_sized(&self, size: u16) {
        let shape = self.conn.generate_id().expect("an id");
        self.conn
            .create_pixmap(1, shape, self.win, size, size)
            .expect("a bitmap");
        let gc = self.conn.generate_id().expect("an id");
        self.conn
            .create_gc(gc, shape, &CreateGCAux::new().foreground(1).background(0))
            .expect("a gc");
        self.conn
            .poly_fill_rectangle(
                shape,
                gc,
                &[Rectangle {
                    x: 0,
                    y: 0,
                    width: size,
                    height: size,
                }],
            )
            .expect("fill");
        let cursor = self.conn.generate_id().expect("an id");
        self.conn
            .create_cursor(
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
            )
            .expect("a bitmap cursor");
        self.show(cursor);
        let _ = self.conn.free_gc(gc);
        let _ = self.conn.free_pixmap(shape);
    }
}

impl Drop for CursorWindow {
    fn drop(&mut self) {
        let _ = self.conn.destroy_window(self.win);
        let _ = self.conn.sync();
    }
}

/// The base64 PNG of a `cursor,` message, or "" for anything else.
fn curdata(message: &str) -> String {
    message
        .strip_prefix("cursor,")
        .and_then(|body| serde_json::from_str::<serde_json::Value>(body).ok())
        .and_then(|v| v.get("curdata").and_then(|d| d.as_str()).map(str::to_owned))
        .unwrap_or_default()
}

/// The first bytes base64 `text` decodes to.
fn base64_head(text: &str, bytes: usize) -> Vec<u8> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    };
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for v in text.bytes().map_while(value) {
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((acc >> bits) & 0xff).unwrap_or(0));
            if out.len() == bytes {
                break;
            }
        }
    }
    out
}

/// Pixel size of a base64 PNG sprite, from its IHDR.
fn png_size(data: &str) -> Option<(u32, u32)> {
    let head = base64_head(data, 24);
    if head.len() < 24 || &head[..8] != b"\x89PNG\r\n\x1a\n" || &head[12..16] != b"IHDR" {
        return None;
    }
    let word = |at: usize| u32::from_be_bytes([head[at], head[at + 1], head[at + 2], head[at + 3]]);
    Some((word(16), word(20)))
}

/// Every distinct cursor sprite the page receives within `window`.
async fn collect(page: &Client, window: Duration) -> Vec<String> {
    let mark = page.texts();
    let mut seen: Vec<String> = Vec::new();
    for message in page
        .texts_during(mark, window, |t| t.starts_with("cursor,"))
        .await
    {
        let data = curdata(&message);
        if !data.is_empty() && seen.last() != Some(&data) {
            seen.push(data);
        }
    }
    seen
}

#[tokio::test]
#[ignore = "needs a built selkies server and the X test display (E2E_DISPLAY)"]
async fn the_cursor_reaches_the_client_from_both_sources() {
    let mut checks = Checks::new("cursor-delivery");
    let server = server::launch("cursor-delivery").start().await;
    let cursors = CursorWindow::open(&test_display());
    cursors.set(GLYPH_A);
    let page = Client::connect(&server, "").await;
    let settings = json!({
        "displayId": "primary", "initialClientWidth": 1280, "initialClientHeight": 720,
        "manual_resolution": false, "framerate": 60, "encoder": "h264enc",
        "video_crf": 25, "video_bitrate": 6000, "audio_bitrate": 128_000,
        "scaling_dpi": 96, "displayPosition": "right",
    });
    page.send(&format!("SETTINGS,{settings}"));
    let seeded = collect(&page, Duration::from_secs(4)).await;
    checks.check(
        "a connecting client is given a cursor",
        !seeded.is_empty(),
        format!("{} sprite(s)", seeded.len()),
    );

    cursors.set(GLYPH_B);
    let changed = collect(&page, Duration::from_secs(4)).await;
    checks.check(
        "a change while streaming reaches the client",
        !changed.is_empty() && changed.last() != seeded.last(),
        format!("{} sprite(s)", changed.len()),
    );

    cursors.set_sized(128);
    let large = collect(&page, Duration::from_secs(4)).await;
    let size = large.last().and_then(|d| png_size(d));
    checks.check(
        "an application's 128px cursor is delivered whole",
        size == Some((128, 128)),
        format!("{} sprite(s), last {size:?}", large.len()),
    );
    cursors.set_sized(256);
    let larger = collect(&page, Duration::from_secs(4)).await;
    let size = larger.last().and_then(|d| png_size(d));
    checks.check(
        "one past the cap is brought down to it",
        size == Some((128, 128)),
        format!("{} sprite(s), last {size:?}", larger.len()),
    );

    // Stopping the video releases the capture, and with it pixelflux's cursor thread.
    page.send("STOP_VIDEO");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    cursors.set(GLYPH_A);
    let quiet = collect(&page, Duration::from_secs(2)).await;
    checks.check(
        "nothing is delivered while nothing captures",
        quiet.is_empty(),
        format!("{} sprite(s)", quiet.len()),
    );

    page.send("START_VIDEO");
    let resumed = collect(&page, Duration::from_secs(6)).await;
    checks.check(
        "the resumed capture re-delivers the shape set while it was down",
        !resumed.is_empty() && resumed.last() != changed.last() && resumed.last() == seeded.last(),
        format!("{} sprite(s)", resumed.len()),
    );
    drop(cursors);
    let log = server.stop().await;
    checks.finish(&log);
}
