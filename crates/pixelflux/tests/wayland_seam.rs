/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The seam between two Wayland displays is continuous, so a window crosses it.
//!
//! Each display is a screen of the capture compositor's own, and the nested session opens one
//! screen per capture output: two capture outputs side by side are still one compositor space,
//! not two desktops, so an element that overlaps both is composited onto both, each output
//! drawing the part that falls in its own rectangle. That is checked from the pixels each
//! display's capture actually delivers: a window walked across the boundary has to appear on
//! the first, then on both, then on the second.
//!
//! The pointer that would drive such a drag is checked too: a button held on the first screen
//! would keep the host delivering motion to that screen's window past its edge, where the nested
//! compositor clamps its cursor at the screen's last column, so the host releases the grab at
//! the crossing and the pointer enters the second screen's window; the nested compositor then
//! carries its cursor, and the window a drag holds, onto the second screen. The grab is observed
//! from a client of the NESTED compositor, since that is the chain a desktop session runs
//! through.
//!
//! The same seam between screens of different scales: the session lays its screens out in its
//! logical space, where a screen at scale 2 is half its mode, so its neighbor has to close up to
//! that logical edge, both when the scale lands and when the capture arrangement is applied
//! again, and a drag across the seam has to put the session's cursor where the host pointer is on
//! either screen, at that screen's own scale.
//!
//! Needs `labwc` (the suites' patched build) and Xwayland on PATH. A binary of its own, the
//! compositor being one per process, and ignored where cargo runs every test at once:
//! `cargo test -p pixelflux --test wayland_seam -- --include-ignored`.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use common::observer::{Event, Observer};
use common::{Results, Scratch, labwc, labwc_log};
use pixelflux::encoders::Codec;
use pixelflux::sink::Frame;
use pixelflux::{CaptureSettings, ScreenCapture, ensure_wayland_display};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConfigureWindowAux, ConnectionExt, CreateWindowAux, WindowClass};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// One display's size; the two screens sit side by side, twice as wide in all.
const WIDTH: i32 = 1920;
const HEIGHT: i32 = 1080;
/// The color the walked window is painted, and how much of a display it has to cover to count
/// as present there (a 500x400 window is ~10% of one screen).
const WIN_RGB: [u8; 3] = [240, 20, 20];
const PRESENT: f64 = 0.005;

/// Keeps the newest JPEG picture one display delivered.
#[derive(Clone, Default)]
struct Newest(Arc<Mutex<Option<Vec<u8>>>>);

impl Newest {
    fn sink(&self) -> impl Fn(Vec<Frame>) + Send + Sync + 'static {
        let newest = self.0.clone();
        move |frames: Vec<Frame>| {
            for frame in frames {
                let bytes = frame.as_bytes();
                if let Some(start) = bytes.windows(2).position(|w| w == [0xff, 0xd8]) {
                    *newest.lock().expect("the picture") = Some(bytes[start..].to_vec());
                }
            }
        }
    }

    /// The fraction of the newest picture within 70 levels of `rgb`, at 160x90.
    fn coverage(&self, rgb: [u8; 3]) -> f64 {
        let Some(jpeg) = self.0.lock().expect("the picture").clone() else {
            return 0.0;
        };
        let Ok(picture) = image::load_from_memory(&jpeg) else {
            return 0.0;
        };
        let small = picture
            .resize_exact(160, 90, image::imageops::FilterType::Triangle)
            .to_rgb8();
        let near = small
            .pixels()
            .filter(|p| {
                p.0.iter()
                    .zip(rgb)
                    .all(|(g, w)| (i32::from(*g) - i32::from(w)).abs() <= 70)
            })
            .count();
        near as f64 / f64::from(160 * 90)
    }
}

/// A JPEG capture of one display: the primary's view of screen 0, or a secondary's own screen.
fn settings(display_id: u32) -> CaptureSettings {
    CaptureSettings {
        use_wayland: Some(true),
        display_id,
        width: WIDTH,
        height: HEIGHT,
        target_fps: 10.0,
        codec: Codec::Jpeg,
        jpeg_quality: 80,
        use_cpu: true,
        capture_cursor: false,
        ..CaptureSettings::default()
    }
}

/// The nested compositor's own socket and the X display its Xwayland serves.
fn session(runtime: &Scratch, socket: &str) -> (Option<String>, Option<String>) {
    let deadline = Instant::now() + Duration::from_secs(40);
    let (mut inner, mut display) = (None, None);
    while Instant::now() < deadline && (inner.is_none() || display.is_none()) {
        inner = common::inner_socket(runtime, socket);
        display = std::fs::read_to_string(runtime.0.join("env"))
            .ok()
            .and_then(|dump| dump.trim().split_once('=').map(|(_, d)| d.to_string()))
            .filter(|d| !d.is_empty());
        thread::sleep(Duration::from_millis(250));
    }
    (inner, display)
}

/// A solid-colored window on the session's X display, placed by this test: override-redirect so
/// its position is the test's to state rather than the window manager's, and drawn by the X
/// server from the window background.
struct Patch {
    conn: RustConnection,
    window: u32,
}

impl Patch {
    fn new(display: &str, rgb: [u8; 3]) -> Self {
        let (conn, screen) = x11rb::connect(Some(display)).expect("the session's X display");
        let root = conn.setup().roots[screen].root;
        let window = conn.generate_id().expect("an id");
        let pixel = (u32::from(rgb[0]) << 16) | (u32::from(rgb[1]) << 8) | u32::from(rgb[2]);
        conn.create_window(
            0,
            window,
            root,
            300,
            300,
            500,
            400,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new()
                .override_redirect(1)
                .background_pixel(pixel),
        )
        .expect("the window");
        conn.map_window(window).expect("mapped");
        conn.sync().expect("synced");
        thread::sleep(Duration::from_millis(1500));
        Self { conn, window }
    }

    /// Maps the window with its left edge at `x`. Mapping again is idempotent, and re-issuing the
    /// whole placement is what lets a poll recover a window a loaded nested session dropped.
    fn place(&self, x: i32) {
        let _ = self.conn.map_window(self.window);
        let _ = self
            .conn
            .configure_window(self.window, &ConfigureWindowAux::new().x(x).y(300));
        let _ = self.conn.sync();
    }
}

impl Drop for Patch {
    fn drop(&mut self) {
        let _ = self.conn.destroy_window(self.window);
        let _ = self.conn.sync();
    }
}

/// Injects a held left-button drag along `xs` at height `y`, starting at `press`.
fn drag(cap: &ScreenCapture, press: (f64, f64), xs: impl Iterator<Item = i32>, y: f64) {
    let _ = cap.inject_mouse_move(press.0, press.1);
    thread::sleep(Duration::from_millis(400));
    let _ = cap.inject_mouse_button(272, 1);
    thread::sleep(Duration::from_millis(300));
    for x in xs {
        let _ = cap.inject_mouse_move(f64::from(x), y);
        thread::sleep(Duration::from_millis(30));
    }
    thread::sleep(Duration::from_millis(600));
    let _ = cap.inject_mouse_button(272, 0);
}

#[test]
#[ignore = "starts the in-process Wayland compositor and a nested labwc"]
fn a_window_crosses_the_seam_between_two_displays() {
    let runtime = common::runtime_dir("wl-seam");
    // SAFETY: the only test of this binary, before any capture or compositor thread starts.
    unsafe {
        common::isolate(&runtime, None);
        std::env::set_var("LIBGL_ALWAYS_SOFTWARE", "1");
    }
    let config = runtime.0.join("config");
    std::fs::create_dir_all(config.join("labwc")).expect("labwc's configuration");
    std::fs::write(
        config.join("labwc/rc.xml"),
        "<?xml version=\"1.0\"?>\n<labwc_config>\n  <core><decoration>server</decoration></core>\n</labwc_config>\n",
    )
    .expect("rc.xml");
    let startup = runtime.0.join("startup.sh");
    std::fs::write(
        &startup,
        format!(
            "#!/bin/sh\nenv | grep ^DISPLAY= > {}/env\n",
            runtime.0.display()
        ),
    )
    .expect("the startup script");
    std::fs::set_permissions(&startup, std::fs::Permissions::from_mode(0o755)).expect("executable");

    let mut res = Results::default();
    let socket = ensure_wayland_display(WIDTH, HEIGHT, String::new(), String::new(), -1)
        .expect("the compositor starts");
    let (left_cap, right_cap) = (ScreenCapture::new(), ScreenCapture::new());
    // The primary's screen carries its view from the start; the second display is a screen of
    // its own beside it.
    let default: Vec<u32> = left_cap
        .list_outputs()
        .unwrap_or_default()
        .iter()
        .map(|o| o.0)
        .collect();
    res.check(
        "the primary's screen comes with its view on it",
        default == [0, 1],
        &default,
    );
    res.check(
        "a second display gets a screen of its own",
        left_cap
            .create_output(2, WIDTH, HEIGHT, WIDTH, 0, 1.0)
            .unwrap_or(false),
        "",
    );
    let (left, right) = (Newest::default(), Newest::default());
    left_cap
        .start_capture(Arc::new(left.sink()), settings(1))
        .expect("the left capture");
    right_cap
        .start_capture(Arc::new(right.sink()), settings(2))
        .expect("the right capture");
    thread::sleep(Duration::from_millis(2500));

    // Two screens from the start: pre-provisioning is the one road a stock labwc has, and the
    // patched one takes a startup count the same way. Xwayland inherits the EGL vendor: its glamor
    // probe crashes inside the NVIDIA vendor when the renderer underneath is software.
    let mesa = "/usr/share/glvnd/egl_vendor.d/50_mesa.json";
    let config_home = config.to_string_lossy().into_owned();
    let mut env: Vec<(&str, &str)> =
        vec![("XDG_CONFIG_HOME", &config_home), ("WLR_WL_OUTPUTS", "2")];
    if std::path::Path::new(mesa).exists() {
        env.push(("__EGL_VENDOR_LIBRARY_FILENAMES", mesa));
    }
    let startup_arg = startup.to_string_lossy().into_owned();
    let nested = labwc(&runtime, &socket, &env, &["-s", &startup_arg]);
    let (inner, display) = session(&runtime, &socket);
    let (Some(inner), Some(display)) = (inner, display) else {
        panic!(
            "the nested compositor did not come up:\n{}",
            labwc_log(&runtime)
        );
    };
    // The session arranges the screens it opened by its own rule; it is told the capture
    // arrangement the way a host tells it.
    let _ = left_cap.set_app_screen_layout(
        &inner,
        vec![(0, 0, WIDTH, HEIGHT), (WIDTH, 0, WIDTH, HEIGHT)],
    );
    thread::sleep(Duration::from_millis(1500));
    let positions = |cap: &ScreenCapture| -> Vec<(i32, i32)> {
        cap.list_app_screens(&inner)
            .unwrap_or_default()
            .iter()
            .map(|s| (s.1, s.2))
            .collect()
    };
    let screens = positions(&left_cap);
    res.check(
        "the session's screens sit side by side",
        screens == [(0, 0), (WIDTH, 0)],
        &screens,
    );

    {
        let patch = Patch::new(&display, WIN_RGB);
        // Polls both displays for the wanted split, re-placing the window between samples:
        // encoding is damage-driven, so a fresh frame takes a moment, and a loaded nested session
        // can drop a placement.
        let settled = |want: &dyn Fn(f64, f64) -> bool, x: i32| -> (f64, f64) {
            let mut last = (left.coverage(WIN_RGB), right.coverage(WIN_RGB));
            let deadline = Instant::now() + Duration::from_secs(15);
            while !want(last.0, last.1) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(400));
                patch.place(x);
                last = (left.coverage(WIN_RGB), right.coverage(WIN_RGB));
            }
            last
        };
        patch.place(300);
        let first = settled(&|l, r| l > PRESENT && r <= PRESENT, 300);
        res.check(
            "a window on the first display shows only there",
            first.0 > PRESENT && first.1 <= PRESENT,
            first,
        );
        patch.place(WIDTH - 250);
        let across = settled(&|l, r| l > PRESENT && r > PRESENT, WIDTH - 250);
        res.check(
            "a window over the boundary shows on both",
            across.0 > PRESENT && across.1 > PRESENT,
            across,
        );
        patch.place(WIDTH + 400);
        let second = settled(&|l, r| l <= PRESENT && r > PRESENT, WIDTH + 400);
        res.check(
            "a window past the boundary shows only on the second",
            second.0 <= PRESENT && second.1 > PRESENT,
            second,
        );
    }

    // What a drag rides on. The grab is held across the boundary: the host hands the pointer to
    // the second screen's window at the crossing, and the nested compositor carries its cursor
    // onto that screen rather than clamp it at the first one's last column.
    let inner_path = runtime.0.join(&inner);
    {
        let obs = Observer::start(inner_path.clone(), 0);
        if obs.ready(Duration::from_secs(20)).is_none() {
            res.check(
                "a held grab crosses the boundary",
                false,
                "no observer surface",
            );
        } else {
            thread::sleep(Duration::from_secs(1));
            obs.clear();
            drag(
                &left_cap,
                (500.0, 400.0),
                (500..2 * WIDTH - 40).step_by(50),
                400.0,
            );
            let deadline = Instant::now() + Duration::from_secs(10);
            let (mut reach, mut clicks) = (-1.0f64, 0);
            while Instant::now() < deadline {
                let events = obs.events();
                reach = events
                    .iter()
                    .filter_map(|e| {
                        if let Event::Motion(x, _) = e {
                            Some(*x)
                        } else {
                            None
                        }
                    })
                    .fold(-1.0, f64::max);
                clicks = events
                    .iter()
                    .filter(|e| matches!(e, Event::Button(..)))
                    .count();
                if reach > f64::from(WIDTH + 500) && clicks > 0 {
                    break;
                }
                thread::sleep(Duration::from_millis(300));
            }
            res.check(
                "a held grab crosses the boundary",
                reach > f64::from(WIDTH + 500),
                format!("reached x={reach}, boundary at {WIDTH}"),
            );
            res.check(
                "injected buttons reach the session's clients",
                clicks >= 1,
                clicks,
            );
        }
    }

    let ctl = ScreenCapture::new();
    let half = (WIDTH / 2, HEIGHT / 2);
    let placed = |want: &[(i32, i32)]| -> Vec<(i32, i32)> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut got = positions(&ctl);
        while got != want && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(200));
            got = positions(&ctl);
        }
        got
    };
    res.check(
        "the first screen takes scale 2",
        ctl.set_app_screen_geometry(&inner, WIDTH, HEIGHT, 2.0, 0)
            .unwrap_or(false),
        "",
    );
    let want = [(0, 0), (half.0, 0)];
    let got = placed(&want);
    res.check(
        "the second screen closes up to the scaled screen's logical edge",
        got == want,
        &got,
    );
    let _ = ctl.set_app_screen_layout(
        &inner,
        vec![(0, 0, WIDTH, HEIGHT), (WIDTH, 0, WIDTH, HEIGHT)],
    );
    let got = placed(&want);
    res.check(
        "the capture arrangement applied again keeps it there",
        got == want,
        &got,
    );

    // A drag either way across the seam, observed by a client fullscreen on the screen it starts
    // on: the last position it is handed is where the host pointer ended, in the logical space
    // of the screen it ended on.
    for (tag, output, size, press, end, want_at) in [
        (
            "from the scaled screen",
            0,
            half,
            (1500, 400),
            (WIDTH + 600, 400),
            (half.0 + 600, 400),
        ),
        (
            "onto the scaled screen",
            1,
            (WIDTH, HEIGHT),
            (WIDTH + 480, 400),
            (500, 400),
            (250 - half.0, 200),
        ),
    ] {
        let obs = Observer::start(inner_path.clone(), output);
        let Some(mapped) = obs.ready(Duration::from_secs(20)) else {
            res.check(
                &format!("{tag}: the session's cursor lands where the pointer is"),
                false,
                "no observer surface",
            );
            continue;
        };
        if mapped.2 {
            res.check(
                &format!("{tag}: the observer covers its screen at that screen's scale"),
                (mapped.0, mapped.1) == size,
                mapped,
            );
        } else {
            println!(
                "SKIP  {tag}: the observer covers its screen at that screen's scale  this compositor leaves the fullscreen size to the client"
            );
        }
        thread::sleep(Duration::from_secs(1));
        obs.clear();
        let step: i32 = if end.0 > press.0 { 50 } else { -50 };
        let xs = std::iter::successors(Some(press.0), move |x| Some(x + step))
            .take_while(move |x| if step > 0 { *x < end.0 } else { *x > end.0 })
            .chain([end.0]);
        drag(
            &left_cap,
            (f64::from(press.0), f64::from(press.1)),
            xs,
            f64::from(end.1),
        );
        let released = obs.wait_button(0, Duration::from_secs(10));
        let last = obs.events().iter().rev().find_map(|e| {
            if let Event::Motion(x, y) = e {
                Some((*x, *y))
            } else {
                None
            }
        });
        res.check(
            &format!("{tag}: the session's cursor lands where the pointer is"),
            released
                && last.is_some_and(|(x, y)| {
                    (x - f64::from(want_at.0)).abs() <= 4.0
                        && (y - f64::from(want_at.1)).abs() <= 4.0
                }),
            format!("last {last:?}, wanted {want_at:?}"),
        );
    }
    drop(nested);
    let _ = left_cap.stop_capture();
    let _ = right_cap.stop_capture();
    res.finish();
}
