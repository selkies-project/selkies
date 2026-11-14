/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Every frame a capture delivers carries its capture and encode instants, on X11 and on the
//! in-process compositor, encoded in hardware where the node has it and in software:
//! CLOCK_MONOTONIC nanoseconds that order capture, encode start and encode end, land before the
//! frame reaches its consumer, and put the whole host leg of a 720p frame well under a frame
//! interval. A software session delivers a frame as stripes, each carrying the frame's stamps,
//! so the collection is timed rather than counted.
//!
//! A binary of its own, the compositor being one per process, and ignored where cargo runs
//! every test at once: `cargo test -p pixelflux --test frame_timing -- --include-ignored`.

mod common;

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use common::Results;
use pixelflux::encoders::Codec;
use pixelflux::sink::Frame;
use pixelflux::{CaptureSettings, ScreenCapture, ensure_wayland_display};

const WIDTH: i32 = 1280;
const HEIGHT: i32 = 720;

/// One stripe's stamps and the instant it reached the consumer.
type Stamps = (i64, i64, i64, i64);

fn monotonic_ns() -> i64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: a valid timespec to fill.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    now.tv_sec * 1_000_000_000 + now.tv_nsec
}

fn settings(wayland: bool, software: bool) -> CaptureSettings {
    CaptureSettings {
        width: WIDTH,
        height: HEIGHT,
        codec: Codec::H264,
        target_fps: 30.0,
        video_streaming_mode: true,
        use_cpu: software,
        use_wayland: Some(wayland),
        display_id: if wayland { 1 } else { 0 },
        ..CaptureSettings::default()
    }
}

/// Three seconds of a capture's stripes.
fn collect(settings: CaptureSettings) -> Result<Vec<Stamps>, String> {
    let seen: Arc<Mutex<Vec<Stamps>>> = Arc::default();
    let sink = {
        let seen = seen.clone();
        move |frames: Vec<Frame>| {
            let now = monotonic_ns();
            let mut seen = seen.lock().expect("the stamps");
            for frame in frames {
                let t = frame.timing;
                seen.push((t.capture_ns, t.encode_start_ns, t.encode_end_ns, now));
            }
        }
    };
    let cap = ScreenCapture::new();
    cap.start_capture(Arc::new(sink), settings)
        .map_err(|e| e.to_string())?;
    thread::sleep(Duration::from_secs(3));
    let _ = cap.stop_capture();
    thread::sleep(Duration::from_millis(300));
    Ok(seen.lock().expect("the stamps").clone())
}

fn judge(res: &mut Results, label: &str, frames: Result<Vec<Stamps>, String>) {
    let frames = match frames {
        Ok(frames) => frames,
        Err(e) => return res.check(&format!("{label}: capture"), false, e),
    };
    res.check(
        &format!("{label}: frames arrive"),
        frames.len() >= 5,
        frames.len(),
    );
    if frames.is_empty() {
        return;
    }
    let disordered: Vec<&Stamps> = frames
        .iter()
        .filter(|(c, s, e, now)| !(0 < *c && c <= s && s <= e && e <= now))
        .take(2)
        .collect();
    res.check(
        &format!("{label}: capture, encode start, encode end, and arrival are ordered"),
        disordered.is_empty(),
        disordered,
    );
    // The encoder's own start (an NVENC session takes hundreds of milliseconds to open) is
    // charged to the first frames, the ones captured while it opened included, so the bound is
    // on the frames captured a second or more into the stream.
    let first = frames[0].0;
    let mut host_ms: Vec<f64> = frames
        .iter()
        .filter(|(c, ..)| c - first >= 1_000_000_000)
        .map(|(c, _, e, _)| (e - c) as f64 / 1e6)
        .collect();
    host_ms.sort_by(f64::total_cmp);
    let max = host_ms.last().copied().unwrap_or(0.0);
    res.check(
        &format!("{label}: the host leg of a 720p frame stays under 100 ms once streaming"),
        !host_ms.is_empty() && max < 100.0,
        format!(
            "{} frames, max {max:.1} ms, median {:.1} ms",
            host_ms.len(),
            host_ms.get(host_ms.len() / 2).copied().unwrap_or(0.0)
        ),
    );
}

#[test]
#[ignore = "starts an X server and the in-process Wayland compositor"]
fn every_frame_carries_ordered_capture_and_encode_instants() {
    let runtime = common::runtime_dir("frametiming");
    let (_server, display) = common::xvfb(WIDTH, HEIGHT);
    // SAFETY: the only test of this binary, before any capture or compositor thread starts.
    unsafe { common::isolate(&runtime, Some(&display)) };
    let mut res = Results::default();
    judge(&mut res, "x11 h264", collect(settings(false, false)));
    judge(
        &mut res,
        "x11 h264 software",
        collect(settings(false, true)),
    );
    ensure_wayland_display(WIDTH, HEIGHT, String::new(), String::new(), 0)
        .expect("the compositor starts");
    judge(&mut res, "wayland h264", collect(settings(true, false)));
    judge(
        &mut res,
        "wayland h264 software",
        collect(settings(true, true)),
    );
    res.finish();
}
