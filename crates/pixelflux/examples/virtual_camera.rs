/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A camera for trying the V4L2 interposer without the rest of the stack: a
//! [`VirtualCamera`] on the interposer's socket, fed synthetic MJPEG frames (a color field
//! that moves frame by frame, under a row of bars) as a browser's camera uplink would feed it.
//! Run it, then any consumer under the interposer:
//!
//! ```text
//! cargo run --release -p pixelflux --example virtual_camera -- --width 640 --height 480 &
//! WEBCAM_LOG=1 LD_PRELOAD="$PWD/selkies_v4l2_interposer.so" \
//!     ffmpeg -f v4l2 -i /dev/video0 -frames:v 10 out.mkv
//! ```
//!
//! Options, each taking a value but the last three: `--socket` (default: the session's camera
//! socket, where the interposer looks), `--width` and `--height` of the device (1280x720),
//! `--fps` (30), `--pixel-format` (`I420`, `NV12`, `YUYV`, or `MJPEG`), `--device` (a
//! v4l2loopback output to mirror: `auto` or a path; none by default), `--source-width` and
//! `--source-height` of the synthetic frames (the device's size), `--picture` (a JPEG file to
//! show instead of the synthetic frames; given more than once, the pictures are shown in turn),
//! `--always` (feed frames with no consumer connected), `--no-pipewire` (publish no PipeWire
//! node, so the camera is reached over its socket alone) and `--stats` (print the camera's
//! counters once a second).

use std::process::ExitCode;
use std::time::Duration;

use image::codecs::jpeg::JpegEncoder;
use image::{Rgb, RgbImage};
use pixelflux::webcam::{VirtualCamera, VirtualCameraSettings};

/// What the command line asked for.
struct Args {
    settings: VirtualCameraSettings,
    source: (u32, u32),
    pictures: Vec<Vec<u8>>,
    always: bool,
    stats: bool,
}

/// The command line, or what is wrong with it.
fn parse(mut argv: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut settings = VirtualCameraSettings {
        device_path: String::new(),
        ..VirtualCameraSettings::default()
    };
    let (mut source_width, mut source_height) = (0, 0);
    let (mut always, mut stats) = (false, false);
    let mut pictures = Vec::new();
    while let Some(flag) = argv.next() {
        let mut value = || argv.next().ok_or_else(|| format!("{flag} takes a value"));
        let number = |text: String| {
            text.parse::<u32>()
                .map_err(|_| format!("{flag} takes a number, not {text}"))
        };
        match flag.as_str() {
            "--socket" => settings.socket_path = value()?,
            "--width" => settings.width = number(value()?)?,
            "--height" => settings.height = number(value()?)?,
            "--fps" => settings.fps_num = number(value()?)?.max(1),
            "--pixel-format" => settings.pixel_format = value()?,
            "--device" => settings.device_path = value()?,
            "--source-width" => source_width = number(value()?)?,
            "--source-height" => source_height = number(value()?)?,
            "--picture" => {
                let path = value()?;
                pictures.push(std::fs::read(&path).map_err(|e| format!("{path}: {e}"))?);
            }
            "--always" => always = true,
            "--no-pipewire" => settings.pipewire = false,
            "--stats" => stats = true,
            _ => return Err(format!("unknown option {flag}")),
        }
    }
    settings.fps_den = 1;
    let source = (
        if source_width == 0 {
            settings.width
        } else {
            source_width
        },
        if source_height == 0 {
            settings.height
        } else {
            source_height
        },
    );
    Ok(Args {
        settings,
        source,
        pictures,
        always,
        stats,
    })
}

/// The low byte of `value`: the color channels cycle through 0..=255.
fn byte(value: u32) -> u8 {
    value.to_le_bytes()[0]
}

/// Frame `phase` as MJPEG: a color field that moves with the phase, under eight bars across
/// the top quarter whose shades move with it too.
fn frame((width, height): (u32, u32), phase: u32) -> Result<Vec<u8>, String> {
    let bar_width = (width / 8).max(1);
    let field = Rgb([
        byte(phase),
        byte(phase.wrapping_mul(3)),
        byte(phase.wrapping_mul(7)),
    ]);
    let picture = RgbImage::from_fn(width, height, |x, y| {
        let bar = x / bar_width;
        if bar < 8 && y < height / 4 {
            let shade = byte(bar * 32 + phase);
            Rgb([shade, 255 - shade, byte(u32::from(shade) * 5)])
        } else {
            field
        }
    });
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 80)
        .encode_image(&picture)
        .map_err(|e| format!("encoding a frame: {e}"))?;
    Ok(jpeg)
}

fn main() -> ExitCode {
    let args = match parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let camera = VirtualCamera::new();
    if let Err(e) = camera.start(&args.settings) {
        eprintln!("the camera did not start: {e}");
        return ExitCode::FAILURE;
    }
    let s = &args.settings;
    println!(
        "Serving {} ({}x{} {} @ {} fps); run a consumer under the interposer.",
        camera.socket_path().unwrap_or_default(),
        s.width,
        s.height,
        s.pixel_format,
        s.fps_num
    );
    let interval = Duration::from_secs(1) / s.fps_num;
    let mut phase = 0u32;
    loop {
        if camera.clients() > 0 || args.always {
            let picture = if args.pictures.is_empty() {
                frame(args.source, phase)
            } else {
                Ok(args.pictures[phase as usize % args.pictures.len()].clone())
            };
            let pushed = picture.and_then(|jpeg| {
                camera
                    .push(&jpeg, VirtualCamera::CODEC_MJPEG, true, 0, false)
                    .map_err(|e| e.to_string())
            });
            if let Err(e) = pushed {
                eprintln!("{e}");
                camera.stop();
                return ExitCode::FAILURE;
            }
            phase = phase.wrapping_add(1);
            if args.stats && phase.is_multiple_of(s.fps_num) {
                println!("{:?}", camera.stats());
            }
        }
        std::thread::sleep(interval);
    }
}
