/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The wire header names the picture the encoder produced, at the offset and in the nibbles a
//! consumer reads them.
//!
//! A WebRTC video bridge reads a frame's keyframe flag off the per-stripe header (the low
//! nibble of byte 1: a keyframe is 0x01; every JPEG picture stands alone) to keep the wire
//! decodable across a drop, so the header has to say so for every encoder and backend: the first
//! H.264 picture of a capture is an IDR, the pictures behind it are not, and a requested keyframe
//! arrives as one.
//!
//! The rest of the byte and the header's length are pinned too, because nothing else would
//! notice them moving. The codec rides the high nibble, so a reader comparing the whole byte sees
//! no keyframe ever, and the payload starts where the header ends, so a header that grew feeds its
//! own tail to the decoder as bitstream. Neither shows up as an error anywhere: the check is that
//! the bitstream really begins where the slice is taken, an Annex-B start code for H.264 and SOI
//! for JPEG.
//!
//! Driven on a throwaway Xvfb (`Xvfb` on PATH) and on the in-process Wayland compositor, one
//! per process, so the test is a binary of its own and ignored where cargo runs every test at
//! once: `cargo test -p pixelflux --test wire_header -- --include-ignored`.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use common::Results;
use pixelflux::encoders::Codec;
use pixelflux::sink::Frame;
use pixelflux::{CaptureSettings, ScreenCapture};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ChangeWindowAttributesAux, ConnectionExt};

const WIDTH: i32 = 640;
const HEIGHT: i32 = 400;
/// The type bytes a stripe opens with.
const H264: u8 = 0x04;
const JPEG: u8 = 0x03;
/// The low nibble of byte 1 on a picture that decodes on its own.
const IDR: u8 = 0x01;
/// The wire id H.264 rides in the type byte's high nibble.
const H264_WIRE_ID: u8 = 0x01;
/// Where the payload is sliced: a video stripe carries the frame and reference ids a JPEG one has
/// no use for, so the two headers are different lengths.
const VIDEO_HEADER_LEN: usize = 12;
const JPEG_HEADER_LEN: usize = 6;

/// One stripe as a consumer reads it: the type byte, the picture nibble, the codec nibble, and
/// the first bytes of the payload.
#[derive(Clone, Debug)]
struct Stripe {
    tag: u8,
    picture: u8,
    codec: u8,
    head: Vec<u8>,
}

/// Keeps the root window changing until `stop`: a striped capture sends only what damage covers.
fn repaint(display: String, stop: Arc<AtomicBool>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let Ok((conn, screen)) = x11rb::connect(Some(&display)) else {
            return;
        };
        let root = conn.setup().roots[screen].root;
        let colors = [0x3366cc, 0xcc6633, 0x33cc66];
        let mut index = 0;
        while !stop.load(Ordering::Relaxed) {
            let aux =
                ChangeWindowAttributesAux::new().background_pixel(colors[index % colors.len()]);
            let _ = conn.change_window_attributes(root, &aux);
            let _ = conn.clear_area(false, root, 0, 0, 0, 0);
            let _ = conn.flush();
            index += 1;
            thread::sleep(Duration::from_millis(100));
        }
    })
}

fn settings(codec: Codec, cpu: bool, wayland: bool) -> CaptureSettings {
    CaptureSettings {
        width: WIDTH,
        height: HEIGHT,
        target_fps: 30.0,
        use_cpu: cpu,
        use_wayland: Some(wayland),
        video_bitrate_kbps: 3000,
        video_crf: 25,
        video_cbr_mode: true,
        video_streaming_mode: true,
        use_paint_over_quality: true,
        omit_stripe_headers: false,
        codec,
        ..CaptureSettings::default()
    }
}

/// The stripes of two seconds of capture, and of the second after a keyframe was asked for.
fn capture(
    codec: Codec,
    cpu: bool,
    wayland: bool,
    damage: Option<&str>,
) -> Result<(Vec<Stripe>, Vec<Stripe>), String> {
    let seen: Arc<Mutex<Vec<Stripe>>> = Arc::default();
    let sink = {
        let seen = seen.clone();
        move |frames: Vec<Frame>| {
            let mut seen = seen.lock().expect("the stripes");
            for frame in frames {
                let view = frame.as_bytes();
                let head = if view.first() == Some(&JPEG) {
                    JPEG_HEADER_LEN
                } else {
                    VIDEO_HEADER_LEN
                };
                if view.len() > head {
                    seen.push(Stripe {
                        tag: view[0],
                        picture: view[1] & 0x0F,
                        codec: view[1] >> 4,
                        head: view[head..(head + 5).min(view.len())].to_vec(),
                    });
                }
            }
        }
    };
    let stop = Arc::new(AtomicBool::new(false));
    let painter = damage.map(|display| repaint(display.to_string(), stop.clone()));
    let cap = ScreenCapture::new();
    let outcome = cap
        .start_capture(Arc::new(sink), settings(codec, cpu, wayland))
        .map_err(|e| e.to_string())
        .and_then(|()| {
            thread::sleep(Duration::from_secs(2));
            let before = seen.lock().expect("the stripes").clone();
            cap.request_idr_frame().map_err(|e| e.to_string())?;
            thread::sleep(Duration::from_secs(1));
            let after = seen.lock().expect("the stripes")[before.len()..].to_vec();
            Ok((before, after))
        });
    let _ = cap.stop_capture();
    stop.store(true, Ordering::Relaxed);
    if let Some(painter) = painter {
        let _ = painter.join();
    }
    thread::sleep(Duration::from_millis(300));
    outcome
}

/// The NAL type an Annex-B payload opens with, or None when it opens with no start code, or with
/// one whose NAL header byte is zero: at a slice taken two bytes early the start code's own
/// leading zeros land where the NAL header belongs, so a prefix test alone would pass there.
fn annexb_nal(payload: &[u8]) -> Option<u8> {
    [&[0u8, 0, 0, 1][..], &[0, 0, 1][..]]
        .iter()
        .find_map(|code| {
            let rest = payload.strip_prefix(*code)?;
            rest.first()
                .filter(|&&byte| byte != 0)
                .map(|byte| byte & 0x1F)
        })
}

fn check(res: &mut Results, tag: &str, before: &[Stripe], after: &[Stripe], jpeg: bool) {
    res.check(
        &format!("{tag}: frames flow"),
        before.len() >= 10 && after.len() >= 5,
        (before.len(), after.len()),
    );
    if before.is_empty() {
        return;
    }
    let every: Vec<&Stripe> = before.iter().chain(after).collect();
    if jpeg {
        res.check(
            &format!("{tag}: every stripe carries the JPEG tag"),
            every.iter().all(|s| s.tag == JPEG),
            before
                .iter()
                .map(|s| s.tag)
                .collect::<std::collections::BTreeSet<_>>(),
        );
        res.check(
            &format!("{tag}: every stripe's picture starts where the payload is sliced"),
            every.iter().all(|s| s.head.starts_with(&[0xff, 0xd8])),
            before.first().map(|s| s.head.clone()),
        );
        return;
    }
    res.check(
        &format!("{tag}: every frame carries the H.264 tag"),
        every.iter().all(|s| s.tag == H264),
        before
            .iter()
            .map(|s| s.tag)
            .collect::<std::collections::BTreeSet<_>>(),
    );
    res.check(
        &format!("{tag}: the codec rides the type byte's high nibble"),
        every.iter().all(|s| s.codec == H264_WIRE_ID),
        before
            .iter()
            .map(|s| s.codec)
            .collect::<std::collections::BTreeSet<_>>(),
    );
    res.check(
        &format!("{tag}: every frame's bitstream starts where the payload is sliced"),
        every.iter().all(|s| annexb_nal(&s.head).is_some()),
        before.first().map(|s| s.head.clone()),
    );
    res.check(
        &format!("{tag}: the first picture is an IDR"),
        before[0].picture == IDR,
        &before[0],
    );
    let others: Vec<u8> = before[1..].iter().map(|s| s.picture).collect();
    res.check(
        &format!("{tag}: the pictures behind it are not"),
        !others.is_empty() && !others[..others.len() / 2].contains(&IDR),
        &others[..others.len().min(12)],
    );
    res.check(
        &format!("{tag}: a requested keyframe arrives as an IDR"),
        after.iter().any(|s| s.picture == IDR),
        after.iter().take(12).map(|s| s.picture).collect::<Vec<_>>(),
    );
}

#[test]
#[ignore = "starts an X server and the in-process Wayland compositor"]
fn the_wire_header_names_each_picture_where_a_consumer_reads_it() {
    let runtime = common::runtime_dir("wire");
    let (_server, display) = common::xvfb(WIDTH, HEIGHT);
    // SAFETY: the only test of this binary, before any capture or compositor thread starts.
    unsafe { common::isolate(&runtime, Some(&display)) };
    let mut res = Results::default();
    for (codec, cpu) in [
        (Codec::H264, false),
        (Codec::H264, true),
        (Codec::Jpeg, true),
    ] {
        let jpeg = codec == Codec::Jpeg;
        let tag = format!(
            "x11 {}{}",
            if jpeg { "jpeg" } else { "h264enc" },
            if cpu { " cpu" } else { "" }
        );
        match capture(codec, cpu, false, jpeg.then_some(display.as_str())) {
            Ok((before, after)) => check(&mut res, &tag, &before, &after, jpeg),
            Err(e) => res.check(&format!("{tag}: capture"), false, e),
        }
    }
    for cpu in [false, true] {
        let tag = format!("wayland h264enc{}", if cpu { " cpu" } else { "" });
        match capture(Codec::H264, cpu, true, None) {
            Ok((before, after)) => check(&mut res, &tag, &before, &after, false),
            Err(e) => res.check(&format!("{tag}: capture"), false, e),
        }
    }
    res.finish();
}
