/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A capture that withdraws its cursor sink hands the cursor back.
//!
//! The cursor slot is process-wide, and each display's capture registers its own sink in it: a
//! WebRTC transport runs one pipeline per display. A second display leaving withdraws its sink
//! while the primary keeps capturing, and the primary's sink has to take the slot back, with the
//! current cursor delivered to it again, or the primary's pages keep whatever cursor they last
//! received.
//!
//! Drives two captures on each backend, an X server of its own and the in-process Wayland
//! compositor: the first captures and registers, the second registers and withdraws. On X11 a
//! changed cursor on screen then has to reach the primary as well.
//!
//! A binary of its own, the compositor being one per process, and ignored where cargo runs
//! every test at once: `cargo test -p pixelflux --test cursor_handoff -- --include-ignored`.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use common::{Results, wait_until};
use pixelflux::encoders::Codec;
use pixelflux::sink::{CursorImage, Frame};
use pixelflux::{CaptureSettings, ScreenCapture};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ChangeWindowAttributesAux, ConfigureWindowAux, ConnectionExt, CreateWindowAux, StackMode,
    WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

const WIDTH: i32 = 640;
const HEIGHT: i32 = 400;
const WAIT: Duration = Duration::from_secs(5);

/// A raised input-only window over the whole screen, whose cursor is then the one on screen
/// wherever the pointer is.
struct Cover {
    conn: RustConnection,
    window: u32,
}

impl Cover {
    fn new(display: &str) -> Self {
        let (conn, screen) = x11rb::connect(Some(display)).expect("the X server");
        let root = conn.setup().roots[screen].root;
        let window = conn.generate_id().expect("an id");
        conn.create_window(
            0,
            window,
            root,
            0,
            0,
            WIDTH as u16,
            HEIGHT as u16,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().override_redirect(1),
        )
        .expect("the cover window");
        conn.map_window(window).expect("mapped");
        conn.configure_window(
            window,
            &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        )
        .expect("raised");
        conn.sync().expect("synced");
        Self { conn, window }
    }

    /// Shows the cursor font's glyph `shape` over the screen.
    fn set_cursor(&self, shape: u16) {
        let font = self.conn.generate_id().expect("an id");
        self.conn
            .open_font(font, b"cursor")
            .expect("the cursor font");
        let cursor = self.conn.generate_id().expect("an id");
        self.conn
            .create_glyph_cursor(
                cursor,
                font,
                font,
                shape,
                shape + 1,
                0,
                0,
                0,
                65535,
                65535,
                65535,
            )
            .expect("a glyph cursor");
        self.conn
            .change_window_attributes(
                self.window,
                &ChangeWindowAttributesAux::new().cursor(cursor),
            )
            .expect("the cursor set");
        self.conn.sync().expect("synced");
    }
}

/// A sink counting the cursor images it hears.
fn counter() -> (Arc<AtomicUsize>, Arc<dyn pixelflux::sink::CursorSink>) {
    let count = Arc::new(AtomicUsize::new(0));
    let sink = {
        let count = count.clone();
        move |_: CursorImage| {
            count.fetch_add(1, Ordering::SeqCst);
        }
    };
    (count, Arc::new(sink))
}

fn run(res: &mut Results, backend: &str, display: Option<&str>) {
    let wayland = display.is_none();
    let settings = CaptureSettings {
        width: WIDTH,
        height: HEIGHT,
        target_fps: 30.0,
        use_cpu: true,
        use_wayland: Some(wayland),
        codec: Codec::Jpeg,
        capture_cursor: false,
        ..CaptureSettings::default()
    };
    let cover = display.map(Cover::new);
    let primary = ScreenCapture::new();
    let secondary = ScreenCapture::new();
    let (primary_heard, primary_sink) = counter();
    let (secondary_heard, secondary_sink) = counter();
    primary
        .set_cursor_sink(Some(primary_sink))
        .expect("the primary's sink");
    primary
        .start_capture(Arc::new(|_: Vec<Frame>| {}), settings)
        .expect("the primary captures");
    let heard = |count: &Arc<AtomicUsize>, at_least: usize| {
        let count = count.clone();
        wait_until(WAIT, move || count.load(Ordering::SeqCst) >= at_least)
    };
    if let Some(cover) = &cover {
        cover.set_cursor(150);
    }
    res.check(
        &format!("{backend}: the primary's sink receives the cursor"),
        heard(&primary_heard, 1),
        primary_heard.load(Ordering::SeqCst),
    );
    secondary
        .set_cursor_sink(Some(secondary_sink))
        .expect("the secondary's sink");
    res.check(
        &format!("{backend}: a second capture's registration takes the slot"),
        heard(&secondary_heard, 1),
        secondary_heard.load(Ordering::SeqCst),
    );
    let before = primary_heard.load(Ordering::SeqCst);
    secondary
        .set_cursor_sink(None)
        .expect("the secondary withdraws");
    res.check(
        &format!("{backend}: its withdrawal delivers the current cursor to the primary again"),
        heard(&primary_heard, before + 1),
        primary_heard.load(Ordering::SeqCst) - before,
    );
    let after_withdrawal = secondary_heard.load(Ordering::SeqCst);
    match &cover {
        None => thread::sleep(Duration::from_secs(1)),
        Some(cover) => {
            let before = primary_heard.load(Ordering::SeqCst);
            cover.set_cursor(68);
            res.check(
                &format!("{backend}: a later cursor change reaches the primary"),
                heard(&primary_heard, before + 1),
                primary_heard.load(Ordering::SeqCst) - before,
            );
        }
    }
    res.check(
        &format!("{backend}: the withdrawn sink hears nothing more"),
        secondary_heard.load(Ordering::SeqCst) == after_withdrawal,
        secondary_heard.load(Ordering::SeqCst) - after_withdrawal,
    );
    let _ = primary.stop_capture();
    let _ = primary.set_cursor_sink(None);
    thread::sleep(Duration::from_millis(300));
}

#[test]
#[ignore = "starts an X server and the in-process Wayland compositor"]
fn a_withdrawn_cursor_sink_hands_the_cursor_back() {
    let runtime = common::runtime_dir("cursorhandoff");
    let (_server, display) = common::xvfb(WIDTH, HEIGHT);
    // SAFETY: the only test of this binary, before any capture or compositor thread starts.
    unsafe { common::isolate(&runtime, Some(&display)) };
    let mut res = Results::default();
    run(&mut res, "x11", Some(&display));
    run(&mut res, "wayland", None);
    res.finish();
}
