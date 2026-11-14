/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Screenshots of the in-process Wayland compositor, as a Computer Use client takes them: a
//! view answers at its own size with no capture running, two outputs asked at once both
//! answer at theirs, and the geometry a session reads while screenshots compress stays the
//! output's. A test binary of its own, because the compositor is one per process.

use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::sync::{Arc, Barrier};
use std::thread;

use pixelflux::{ScreenCapture, ensure_wayland_display, screenshot_png};

const WIDTH: i32 = 1280;
const HEIGHT: i32 = 720;

/// A private runtime directory, removed when dropped.
struct Runtime(PathBuf);

impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The size a PNG's header gives, or why `png` is no PNG.
fn png_size(png: &[u8]) -> Result<(i32, i32), String> {
    const HEAD: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
    if png.len() < 33 || !png.starts_with(HEAD) {
        return Err(format!(
            "{} bytes without a PNG signature and IHDR",
            png.len()
        ));
    }
    let field = |at: usize| {
        let bytes: [u8; 4] = png[at..at + 4].try_into().expect("four bytes");
        i32::try_from(u32::from_be_bytes(bytes)).map_err(|e| e.to_string())
    };
    Ok((field(16)?, field(20)?))
}

/// The size of display `display`'s screenshot.
fn screenshot_size(display: u32) -> Result<(i32, i32), String> {
    screenshot_png(display)
        .map_err(|e| e.to_string())
        .and_then(|png| png_size(&png))
}

#[test]
#[ignore = "starts the in-process Wayland compositor"]
fn screenshots_answer_at_each_displays_size_and_leave_its_geometry_readable() {
    let runtime =
        Runtime(std::env::temp_dir().join(format!("pixelflux-shots-{}", std::process::id())));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&runtime.0)
        .expect("a private runtime directory");
    // SAFETY: the only test of this binary, before it or the compositor starts a thread.
    unsafe {
        std::env::set_var("XDG_RUNTIME_DIR", &runtime.0);
        for name in [
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "PIXELFLUX_CU",
            "PIXELFLUX_RECORD",
        ] {
            std::env::remove_var(name);
        }
    }
    let socket = ensure_wayland_display(WIDTH, HEIGHT, String::new(), String::new(), 0)
        .expect("the compositor starts");
    assert!(!socket.is_empty(), "the compositor names its socket");
    let control = ScreenCapture::new();
    assert!(
        control
            .create_output(2, WIDTH, HEIGHT, WIDTH, 0, 1.0)
            .expect("an output request"),
        "a second output beside the first"
    );

    let view = (WIDTH.min(256), HEIGHT.min(128));
    assert!(
        control
            .create_view(7, 0, 0, 0, view.0, view.1)
            .expect("a view request"),
        "a view over the first output"
    );
    assert_eq!(
        screenshot_size(7),
        Ok(view),
        "a view with no capture running answers at its own size"
    );

    for wave in 0..3 {
        let barrier = Arc::new(Barrier::new(2));
        let shots: Vec<_> = [0, 2]
            .into_iter()
            .map(|display| {
                let barrier = barrier.clone();
                thread::spawn(move || {
                    barrier.wait();
                    screenshot_size(display)
                })
            })
            .collect();
        for (display, shot) in [0, 2].into_iter().zip(shots) {
            assert_eq!(
                shot.join().expect("the screenshot thread"),
                Ok((WIDTH, HEIGHT)),
                "wave {wave}: output {display} asked beside the other answers at its size"
            );
        }
    }

    let load = thread::spawn(|| (0..8).map(|_| screenshot_size(0)).collect::<Vec<_>>());
    let mut reads = 0;
    while !load.is_finished() {
        assert_eq!(
            control.get_realized_geometry(0).expect("a geometry read"),
            Some((WIDTH, HEIGHT, 1.0)),
            "the geometry read while screenshots compress is the output's"
        );
        reads += 1;
        thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(reads > 0, "the geometry was read while the screenshots ran");
    for shot in load.join().expect("the screenshot thread") {
        assert_eq!(
            shot,
            Ok((WIDTH, HEIGHT)),
            "a screenshot under load answers at its size"
        );
    }
}
