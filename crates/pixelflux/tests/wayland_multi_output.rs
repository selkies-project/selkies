/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Second-display placement on the Wayland backend, driven at the compositor level because that
//! is where it breaks: a nested compositor (labwc here, as in any wlroots desktop) opens one host
//! toplevel per screen, and those toplevels must land on separate outputs rather than stacking on
//! the first one.
//!
//! Output ids follow a host's usual mapping: the primary shows output 0 through the view (id 1)
//! covering it, and a secondary owns output 2.
//!
//! The placement decision runs on a toplevel's first commit. It cannot key off xdg's
//! initial_configure_sent, because clients that negotiate xdg-decoration are answered with a
//! configure before they ever commit.
//!
//! Needs `labwc` on PATH, with its output-management control (the suites' patched build). A binary
//! of its own, the compositor being one per process, and ignored where cargo runs every test at
//! once: `cargo test -p pixelflux --test wayland_multi_output -- --include-ignored`.

mod common;

use std::thread;
use std::time::{Duration, Instant};

use common::{Results, Scratch, inner_socket, labwc, labwc_log};
use pixelflux::{ScreenCapture, WindowDesc, ensure_wayland_display};

const WIDTH: i32 = 1920;
const HEIGHT: i32 = 1080;

/// The window list once `count` windows have mapped, or whatever mapped in 20 seconds.
fn windows_after(capture: &ScreenCapture, count: usize) -> Vec<WindowDesc> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut windows = Vec::new();
    while Instant::now() < deadline {
        windows = capture.list_windows().unwrap_or_default();
        if windows.len() >= count {
            break;
        }
        thread::sleep(Duration::from_millis(500));
    }
    windows
}

/// A nested labwc opening `outputs` screens on the capture socket.
fn nested(runtime: &Scratch, socket: &str, outputs: usize) -> common::Labwc {
    labwc(
        runtime,
        socket,
        &[("WLR_WL_OUTPUTS", &outputs.to_string())],
        &[],
    )
}

fn parked(windows: &[WindowDesc]) -> usize {
    windows.iter().filter(|w| w.4).count()
}

#[test]
#[ignore = "starts the in-process Wayland compositor and a nested labwc"]
fn a_nested_compositors_screens_land_on_their_own_outputs() {
    let runtime = common::runtime_dir("wl-multi-output");
    // SAFETY: the only test of this binary, before any capture or compositor thread starts.
    unsafe { common::isolate(&runtime, None) };
    let mut res = Results::default();
    let socket = ensure_wayland_display(WIDTH, HEIGHT, String::new(), String::new(), -1)
        .expect("the compositor starts");
    let capture = ScreenCapture::new();

    let created = capture
        .create_output(2, WIDTH, HEIGHT, WIDTH, 0, 1.0)
        .unwrap_or(false);
    let mut outputs: Vec<u32> = capture
        .list_outputs()
        .unwrap_or_default()
        .iter()
        .map(|o| o.0)
        .collect();
    outputs.sort_unstable();
    res.check(
        "second output created beside the primary's screen and view",
        created && outputs == [0, 1, 2],
        &outputs,
    );

    {
        let _labwc = nested(&runtime, &socket, 2);
        let windows = windows_after(&capture, 2);
        assert!(
            !windows.is_empty(),
            "the nested compositor did not come up:\n{}",
            labwc_log(&runtime)
        );
        let mut placed: Vec<u32> = windows.iter().map(|w| w.3).collect();
        placed.sort_unstable();
        placed.dedup();
        res.check(
            "nested compositor screens land on separate outputs",
            windows.len() >= 2 && placed.len() >= 2,
            format!("{} toplevel(s) on outputs {placed:?}", windows.len()),
        );

        // A nested compositor arranges the screens it opens by its own rule (wlroots places
        // them side by side, whatever the capture layout says), and that arrangement is the one
        // windows are placed and dragged in, so a display asked for above or to the left of the
        // primary lands right of it until the session is told otherwise. Every arrangement is
        // checked, since an overlap on the way to one is what a screen-at-a-time apply gets
        // wrong.
        let inner = inner_socket(&runtime, &socket);
        for (name, rects) in [
            (
                "left",
                vec![(WIDTH, 0, WIDTH, HEIGHT), (0, 0, WIDTH, HEIGHT)],
            ),
            (
                "up",
                vec![(0, HEIGHT, WIDTH, HEIGHT), (0, 0, WIDTH, HEIGHT)],
            ),
            (
                "down",
                vec![(0, 0, WIDTH, HEIGHT), (0, HEIGHT, WIDTH, HEIGHT)],
            ),
            (
                "right",
                vec![(0, 0, WIDTH, HEIGHT), (WIDTH, 0, WIDTH, HEIGHT)],
            ),
        ] {
            let label = format!("the session lays its screens out for '{name}'");
            let Some(inner) = &inner else {
                res.check(&label, false, "the nested compositor opened no socket");
                break;
            };
            match capture.set_app_screen_layout(inner, rects.clone()) {
                Err(e) => res.check(&label, false, e.to_string()),
                Ok(placed) => {
                    thread::sleep(Duration::from_secs(1));
                    let got: Vec<(i32, i32)> = capture
                        .list_app_screens(inner)
                        .unwrap_or_default()
                        .iter()
                        .map(|s| (s.1, s.2))
                        .collect();
                    let want: Vec<(i32, i32)> = rects.iter().map(|r| (r.0, r.1)).collect();
                    res.check(
                        &label,
                        placed == rects.len() && got == want,
                        format!("placed={placed} got={got:?} want={want:?}"),
                    );
                }
            }
        }
    }

    // One screen must still open on the primary rather than being spread.
    let _ = capture.destroy_output(2);
    {
        let _labwc = nested(&runtime, &socket, 1);
        let windows = windows_after(&capture, 1);
        res.check(
            "single screen stays on the primary output",
            windows.len() == 1 && windows[0].3 == 0,
            &windows,
        );
    }

    // Screens the layout has no display for wait off-display rather than cover the one the
    // primary is showing, so a session can be started with room for a second display before
    // anyone connects to it.
    {
        let _labwc = nested(&runtime, &socket, 2);
        let windows = windows_after(&capture, 2);
        res.check(
            "the spare screen waits instead of covering the primary",
            windows.len() >= 2 && parked(&windows) == 1,
            format!(
                "{} toplevel(s), {} waiting",
                windows.len(),
                parked(&windows)
            ),
        );
        let created = capture
            .create_output(2, WIDTH, HEIGHT, WIDTH, 0, 1.0)
            .unwrap_or(false);
        res.check(
            "a new output takes the waiting screen",
            created && parked(&windows_after(&capture, 2)) == 0,
            capture.list_windows().unwrap_or_default(),
        );
        let _ = capture.destroy_output(2);
        thread::sleep(Duration::from_secs(1));
        let windows = capture.list_windows().unwrap_or_default();
        res.check(
            "the screen waits again when its output goes away",
            parked(&windows) == 1,
            &windows,
        );
    }
    res.finish();
}
