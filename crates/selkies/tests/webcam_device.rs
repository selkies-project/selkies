/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The virtual webcam end to end below the transports: encoded frames pushed into pixelflux's
//! `VirtualCamera`, as the server pushes a client's uplink, must come out of the V4L2 device an
//! application sees through the interposer (addons/v4l2-interposer): right geometry, right pixel
//! format, right colors, in MMAP and read() mode, for every device format, letterboxed when the
//! camera does not match the device, and through ffmpeg where it is installed. Also the keyframe
//! handshake: an inter-coded stream without a keyframe asks for one; the interposer's PipeWire
//! frame source against the camera's own node; the sysfs view v4l2-ctl and statx users rely on;
//! and the end-of-source ENODEV a reader gets.
//!
//! Builds the interposer and tests/tools' v4l2probe with make, and encodes its H.264 and VP8
//! streams with pixelflux's software encoders on an Xvfb of its own, so it is ignored where
//! cargo runs every test at once:
//! `cargo test -p selkies --test webcam_device -- --include-ignored --nocapture`.
//! ffmpeg, v4l2-ctl and a reachable PipeWire daemon each add their checks where present.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use image::codecs::jpeg::JpegEncoder;
use image::{Rgb, RgbImage};
use pixelflux::encoders::{Codec, software_encoder};
use pixelflux::sink::Frame;
use pixelflux::webcam::{CameraStats, VirtualCamera, VirtualCameraSettings};
use pixelflux::{CaptureSettings, ScreenCapture};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ChangeWindowAttributesAux, ConnectionExt};

/// Limited-range BT.601 of the solid colors the feeder paints (JPEG is full range and the camera
/// compresses it into the range V4L2 consumers assume).
const RED: (i32, i32, i32) = (81, 90, 240);
const BLUE: (i32, i32, i32) = (41, 240, 110);
const BLACK: (i32, i32, i32) = (16, 128, 128);
/// Pure green as pixelflux's encoders carry it, limited range: BT.709 for H.264, which declares it,
/// and BT.601 for VP8, whose bitstream can name no other matrix. The camera hands the decoded
/// samples on as they are.
const GREEN_H264: (i32, i32, i32) = (173, 42, 26);
const GREEN_VP8: (i32, i32, i32) = (145, 54, 34);

/// An encoded stream: each frame's bytes and whether it is a key frame.
type Packets = Vec<(Vec<u8>, bool)>;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn interposer() -> PathBuf {
    repo().join("addons/v4l2-interposer/selkies_v4l2_interposer.so")
}

fn probe_tool() -> PathBuf {
    repo().join("tests/tools/v4l2probe")
}

fn on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
}

/// Every check's outcome, printed as it is made, so one failure does not hide the rest.
#[derive(Default)]
struct Results(Vec<String>);

impl Results {
    fn check(&mut self, label: &str, ok: bool, detail: impl std::fmt::Debug) {
        println!("{}  {label}  {detail:?}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            self.0.push(label.to_string());
        }
    }

    fn skip(&mut self, label: &str, why: &str) {
        println!("SKIP  {label}  {why}");
    }
}

/// A solid `rgb` picture as a JPEG at quality 90.
fn jpeg(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, 90)
        .encode_image(&RgbImage::from_pixel(width, height, Rgb(rgb)))
        .expect("a JPEG");
    out
}

/// Pushes alternating solid MJPEG frames at 30 fps from a thread until dropped.
struct Feeder {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Feeder {
    fn new(cam: &Arc<VirtualCamera>, width: u32, height: u32, colors: &[[u8; 3]]) -> Self {
        let frames: Vec<Vec<u8>> = colors.iter().map(|c| jpeg(width, height, *c)).collect();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (cam, stop) = (cam.clone(), stop.clone());
            thread::spawn(move || {
                let mut pushed = 0;
                while !stop.load(Ordering::Relaxed) {
                    let _ = cam.push(
                        &frames[pushed % frames.len()],
                        VirtualCamera::CODEC_MJPEG,
                        true,
                        0,
                        false,
                    );
                    pushed += 1;
                    thread::sleep(Duration::from_secs(1) / 30);
                }
            })
        };
        Self {
            stop,
            thread: Some(thread),
        }
    }

    fn red_blue(cam: &Arc<VirtualCamera>, width: u32, height: u32) -> Self {
        Self::new(cam, width, height, &[[255, 0, 0], [0, 0, 255]])
    }
}

impl Drop for Feeder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Pushes an encoded stream in a loop at 30 fps from a thread until dropped.
fn feed_stream(cam: &Arc<VirtualCamera>, packets: &[(Vec<u8>, bool)], codec: u32) -> Feeder {
    let packets = packets.to_vec();
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let (cam, stop) = (cam.clone(), stop.clone());
        thread::spawn(move || {
            let mut i = 0;
            while !stop.load(Ordering::Relaxed) {
                let (data, key) = &packets[i % packets.len()];
                let _ = cam.push(data, codec, *key, 0, false);
                i += 1;
                thread::sleep(Duration::from_secs(1) / 30);
            }
        })
    };
    Feeder {
        stop,
        thread: Some(thread),
    }
}

fn start_camera(
    sock_dir: &Path,
    width: u32,
    height: u32,
    pixel_format: &str,
) -> Arc<VirtualCamera> {
    let cam = VirtualCamera::new();
    cam.start(&VirtualCameraSettings {
        socket_path: sock_dir
            .join("selkies_webcam0.sock")
            .to_string_lossy()
            .into_owned(),
        width,
        height,
        pixel_format: pixel_format.to_string(),
        device_path: String::new(),
        ..VirtualCameraSettings::default()
    })
    .expect("the camera starts");
    Arc::new(cam)
}

/// What v4l2probe printed: its key=value lines and the samples it was asked for.
#[derive(Debug, Default)]
struct Probe {
    rc: i32,
    values: HashMap<String, String>,
    samples: HashMap<(i32, i32), (i32, i32, i32)>,
}

impl Probe {
    fn get(&self, key: &str) -> &str {
        self.values.get(key).map_or("", String::as_str)
    }

    fn frames(&self, n: usize) -> bool {
        self.rc == 0 && self.get("frames") == n.to_string()
    }

    fn sample(&self, x: i32, y: i32) -> Option<(i32, i32, i32)> {
        self.samples.get(&(x, y)).copied()
    }
}

/// Options of a probe run beyond the frame count.
struct Run<'a> {
    read: bool,
    samples: &'a [(i32, i32)],
    source: &'a str,
    wait_ms: u32,
    dump: Option<&'a Path>,
}

impl Default for Run<'_> {
    fn default() -> Self {
        Self {
            read: false,
            samples: &[],
            source: "socket",
            wait_ms: 4000,
            dump: None,
        }
    }
}

/// The libc probe under the interposer, reading `frames` frames of /dev/video0 from the camera
/// behind `sock_dir` (or its PipeWire node, by `run.source`).
fn probe(sock_dir: &Path, frames: usize, run: Run) -> Probe {
    let mut command = Command::new(probe_tool());
    command
        .env("LD_PRELOAD", interposer())
        .env("SELKIES_WEBCAM_SOCKET_PATH", sock_dir)
        .env("SELKIES_WEBCAM_SOURCE", run.source);
    if run.read {
        command.arg("--read");
    }
    command.args(["--timeout", &run.wait_ms.to_string()]);
    if let Some(dump) = run.dump {
        command.arg("--dump").arg(dump);
    }
    for (x, y) in run.samples {
        command.args(["--sample", &format!("{x},{y}")]);
    }
    command.args(["/dev/video0", &frames.to_string()]);
    let Ok(child) = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    else {
        return Probe {
            rc: -1,
            ..Probe::default()
        };
    };
    let Some(output) = wait_output(child, Duration::from_secs(20)) else {
        let mut timed_out = Probe {
            rc: -1,
            ..Probe::default()
        };
        timed_out
            .values
            .insert("error".into(), "probe timed out".into());
        return timed_out;
    };
    let mut out = Probe {
        rc: output.status.code().unwrap_or(-1),
        ..Probe::default()
    };
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key == "sample" {
            let parse = |text: &str| -> Option<Vec<i32>> {
                text.split(',').map(|n| n.trim().parse().ok()).collect()
            };
            if let Some((pos, yuv)) = value.split_once(':')
                && let (Some(pos), Some(yuv)) = (parse(pos), parse(yuv))
                && pos.len() == 2
                && yuv.len() == 3
            {
                out.samples
                    .insert((pos[0], pos[1]), (yuv[0], yuv[1], yuv[2]));
            }
        } else {
            out.values.insert(key.to_string(), value.to_string());
        }
    }
    out
}

/// A child's output, or None once `timeout` passes, the child killed.
fn wait_output(mut child: Child, timeout: Duration) -> Option<std::process::Output> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// A command under the interposer, reading the camera behind `sock_dir`: its status and output.
fn under_interposer(sock_dir: &Path, program: &str, args: &[&str]) -> (bool, String, String) {
    let child = Command::new(program)
        .args(args)
        .env("LD_PRELOAD", interposer())
        .env("SELKIES_WEBCAM_SOCKET_PATH", sock_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    match child
        .ok()
        .and_then(|child| wait_output(child, Duration::from_secs(40)))
    {
        Some(out) => (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ),
        None => (false, String::new(), format!("{program} did not finish")),
    }
}

fn near(got: Option<(i32, i32, i32)>, want: (i32, i32, i32), tol: i32) -> bool {
    got.is_some_and(|g| {
        (g.0 - want.0).abs() <= tol && (g.1 - want.1).abs() <= tol && (g.2 - want.2).abs() <= tol
    })
}

fn red_or_blue(yuv: Option<(i32, i32, i32)>) -> bool {
    near(yuv, RED, 6) || near(yuv, BLUE, 6)
}

/// The size and center color of a dumped JPEG frame, or None where it does not decode.
fn jpeg_center(path: &Path) -> Option<((u32, u32), [u8; 3])> {
    let picture = image::load_from_memory(&std::fs::read(path).ok()?)
        .ok()?
        .to_rgb8();
    let center = picture
        .get_pixel(picture.width() / 2, picture.height() / 2)
        .0;
    Some(((picture.width(), picture.height()), center))
}

fn rgb_near(got: [u8; 3], want: [u8; 3]) -> bool {
    got.iter()
        .zip(want)
        .all(|(g, w)| (i32::from(*g) - i32::from(w)).abs() <= 8)
}

/// Whether an encoded frame decodes on its own: an IDR slice in an H.264 access unit, or the
/// key-frame bit (0) of a VP8 frame tag.
fn is_key_frame(codec: Codec, data: &[u8]) -> bool {
    if codec == Codec::Vp8 {
        return data.first().is_some_and(|b| b & 1 == 0);
    }
    data.windows(3)
        .enumerate()
        .any(|(i, w)| w == [0, 0, 1] && data.get(i + 3).is_some_and(|nal| nal & 0x1F == 5))
}

/// A throwaway X server, stopped when dropped.
struct Xvfb(Child);

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn xvfb(width: u32, height: u32) -> (Xvfb, String) {
    let number = (140..200)
        .find(|n| {
            !Path::new(&format!("/tmp/.X11-unix/X{n}")).exists()
                && !Path::new(&format!("/tmp/.X{n}-lock")).exists()
        })
        .expect("a free display number");
    let display = format!(":{number}");
    let child = Command::new("Xvfb")
        .args([
            display.as_str(),
            "-screen",
            "0",
            &format!("{width}x{height}x24"),
            "-nolisten",
            "tcp",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("Xvfb on PATH");
    let server = Xvfb(child);
    let deadline = Instant::now() + Duration::from_secs(15);
    while !Path::new(&format!("/tmp/.X11-unix/X{number}")).exists() {
        assert!(Instant::now() < deadline, "Xvfb {display} never listened");
        thread::sleep(Duration::from_millis(100));
    }
    (server, display)
}

/// `frames` solid-green pictures encoded with pixelflux's own software encoder (H.264 with x264,
/// or OpenH264 in a GPL-free build; VP8 with libvpx) from the X server on `display`, painted green:
/// `[(bytes, keyframe)]` from a key frame on, or None where pixelflux encodes no `codec` here. A
/// key frame every 15, and H.264 one Annex-B access unit a frame with its parameter sets on each
/// key frame, as the WebRTC depacketizer and WebCodecs' annexb mode hand the camera. The root
/// turns between two greens a level apart at the rate, so every tick codes a frame.
fn encode_stream(
    codec: Codec,
    display: &str,
    width: u32,
    height: u32,
    frames: usize,
) -> Option<Packets> {
    software_encoder(codec)?;
    let packets: Arc<Mutex<Packets>> = Arc::default();
    let keys = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let painter = {
        let (display, stop) = (display.to_string(), stop.clone());
        thread::spawn(move || {
            let Ok((conn, screen)) = x11rb::connect(Some(&display)) else {
                return;
            };
            let root = conn.setup().roots[screen].root;
            let mut shade = 0;
            while !stop.load(Ordering::Relaxed) {
                let pixel = [0x00FF00, 0x00FE00][shade];
                let _ = conn.change_window_attributes(
                    root,
                    &ChangeWindowAttributesAux::new().background_pixel(pixel),
                );
                let _ = conn.clear_area(false, root, 0, 0, 0, 0);
                let _ = conn.flush();
                shade ^= 1;
                thread::sleep(Duration::from_secs(1) / 30);
            }
        })
    };
    let sink = {
        let (packets, keys) = (packets.clone(), keys.clone());
        move |stripes: Vec<Frame>| {
            let mut packets = packets.lock().expect("the packets");
            for stripe in stripes {
                let data = stripe.as_bytes().to_vec();
                let key = is_key_frame(codec, &data);
                if key {
                    keys.fetch_add(1, Ordering::SeqCst);
                }
                packets.push((data, key));
            }
        }
    };
    let cap = ScreenCapture::new();
    let started = cap.start_capture(
        Arc::new(sink),
        CaptureSettings {
            width: width as i32,
            height: height as i32,
            target_fps: 30.0,
            codec,
            use_cpu: true,
            use_wayland: Some(false),
            omit_stripe_headers: true,
            keyframe_interval_s: 0.5,
            video_crf: 25,
            use_paint_over_quality: false,
            ..CaptureSettings::default()
        },
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while started.is_ok()
        && Instant::now() < deadline
        && !(keys.load(Ordering::SeqCst) >= 2
            && packets.lock().expect("the packets").len() >= frames + 30)
    {
        thread::sleep(Duration::from_millis(100));
    }
    let _ = cap.stop_capture();
    stop.store(true, Ordering::Relaxed);
    let _ = painter.join();
    let packets = packets.lock().expect("the packets").clone();
    let first = packets.iter().position(|(_, key)| *key)?;
    (packets.len() - first >= frames).then(|| packets[first..first + frames].to_vec())
}

fn stats(cam: &VirtualCamera) -> CameraStats {
    cam.stats()
}

fn publishes_pipewire(stats: &CameraStats) -> bool {
    stats.running.as_ref().is_some_and(|r| r.pipewire)
}

/// Whether the interposer carries its PipeWire node reader, which its Makefile leaves out where
/// PipeWire's headers are missing: a node the camera publishes is then one it cannot read.
fn interposer_reads_pipewire() -> bool {
    std::fs::read(interposer()).is_ok_and(|so| so.windows(20).any(|w| w == b"libpipewire-0.3.so.0"))
}

fn build() {
    for (dir, target) in [
        ("addons/v4l2-interposer", None),
        ("tests/tools", Some("v4l2probe")),
    ] {
        let status = Command::new("make")
            .arg("-C")
            .arg(repo().join(dir))
            .args(target)
            .stdout(Stdio::null())
            .status()
            .expect("make on PATH");
        assert!(status.success(), "make -C {dir} failed");
    }
}

#[test]
#[ignore = "builds the interposer and v4l2probe, and starts an X server to encode with"]
fn the_virtual_camera_comes_out_of_the_interposers_device() {
    build();
    // The H.264 and VP8 streams come first, from an X server of their own, before any thread of
    // the test reads the environment.
    let (server, display) = xvfb(320, 240);
    // SAFETY: no thread of this test has started yet.
    unsafe { std::env::set_var("DISPLAY", &display) };
    let h264 = encode_stream(Codec::H264, &display, 320, 240, 45);
    let vp8 = encode_stream(Codec::Vp8, &display, 320, 240, 45);
    drop(server);

    let mut res = Results::default();
    let scratch = tempfile_dir();
    let sock_dir = scratch.as_path();

    let cam = start_camera(sock_dir, 640, 480, "I420");
    let ended: Arc<Mutex<Option<Probe>>> = Arc::default();
    let blocked;
    let stopped_at;
    {
        let _feeder = Feeder::red_blue(&cam, 640, 480);
        let r = probe(
            sock_dir,
            15,
            Run {
                samples: &[(320, 240), (10, 10), (630, 470)],
                ..Run::default()
            },
        );
        res.check("mmap: 15 frames delivered", r.frames(15), &r);
        res.check(
            "mmap: device is 640x480 YU12",
            (r.get("format"), r.get("width"), r.get("height")) == ("YU12", "640", "480"),
            (r.get("format"), r.get("width"), r.get("height")),
        );
        res.check(
            "mmap: bytesperline/sizeimage for I420",
            (r.get("bytesperline"), r.get("sizeimage"))
                == ("640", &*(640 * 480 * 3 / 2).to_string()),
            (r.get("bytesperline"), r.get("sizeimage")),
        );
        res.check(
            "mmap: one format, one size, 30 fps interval",
            r.get("nformats") == "1" && r.get("timeperframe") == "1/30",
            (r.get("nformats"), r.get("timeperframe")),
        );
        res.check(
            "mmap: frame rate keeps up",
            r.get("fps").parse::<f64>().is_ok_and(|fps| fps >= 15.0),
            r.get("fps"),
        );
        res.check(
            "mmap: center is limited-range red or blue",
            red_or_blue(r.sample(320, 240)),
            &r.samples,
        );
        res.check(
            "mmap: corners carry the same solid color",
            red_or_blue(r.sample(10, 10)) && red_or_blue(r.sample(630, 470)),
            &r.samples,
        );
        let r = probe(
            sock_dir,
            10,
            Run {
                read: true,
                samples: &[(320, 240)],
                ..Run::default()
            },
        );
        res.check("read(): 10 frames delivered", r.frames(10), &r);
        res.check(
            "read(): center color",
            red_or_blue(r.sample(320, 240)),
            &r.samples,
        );
        // The second consumer opens its own handle on the same ring.
        let other = {
            let sock_dir = sock_dir.to_path_buf();
            thread::spawn(move || probe(&sock_dir, 20, Run::default()))
        };
        let r = probe(sock_dir, 20, Run::default());
        let _ = other.join();
        res.check("two concurrent consumers", r.frames(20), &r);
        if on_path("ffmpeg") {
            let (ok, _, err) = under_interposer(
                sock_dir,
                "ffmpeg",
                &[
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-f",
                    "v4l2",
                    "-i",
                    "/dev/video0",
                    "-frames:v",
                    "10",
                    "-f",
                    "null",
                    "-",
                ],
            );
            res.check("ffmpeg captures 10 frames", ok, err.trim());
        } else {
            res.skip("ffmpeg captures 10 frames", "ffmpeg not installed");
        }
        let st = stats(&cam);
        res.check(
            "stats count decoded and published frames",
            st.decoded > 0 && st.published > 0 && st.errors == 0,
            &st,
        );
        res.check(
            "stats report the input geometry",
            (st.input_width, st.input_height) == (640, 480),
            &st,
        );
        if publishes_pipewire(&st) {
            let nodes = if on_path("pw-cli") {
                Command::new("pw-cli")
                    .args(["ls", "Node"])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                    .ok()
                    .and_then(|child| wait_output(child, Duration::from_secs(20)))
                    .map_or("pw-cli did not answer in 20 s".to_string(), |o| {
                        String::from_utf8_lossy(&o.stdout).into_owned()
                    })
            } else {
                String::new()
            };
            res.check(
                "PipeWire node is published while the daemon is reachable",
                nodes.contains("selkies-webcam"),
                nodes.len(),
            );
            if !interposer_reads_pipewire() {
                res.skip(
                    "PipeWire source reads",
                    "the interposer was built without PipeWire's headers",
                );
            } else {
                // The same frames, taken from the node instead of the backend socket.
                let nowhere = sock_dir.join("no-socket-here");
                let r = probe(
                    &nowhere,
                    15,
                    Run {
                        samples: &[(320, 240)],
                        source: "pipewire",
                        ..Run::default()
                    },
                );
                res.check("PipeWire source: mmap delivers 15 frames", r.frames(15), &r);
                res.check(
                    "PipeWire source: device geometry follows the node",
                    (r.get("format"), r.get("width"), r.get("height")) == ("YU12", "640", "480"),
                    &r,
                );
                res.check(
                    "PipeWire source: frame color",
                    red_or_blue(r.sample(320, 240)),
                    &r.samples,
                );
                let r = probe(
                    &nowhere,
                    10,
                    Run {
                        read: true,
                        samples: &[(320, 240)],
                        source: "pipewire",
                        ..Run::default()
                    },
                );
                res.check(
                    "PipeWire source: read() delivers 10 frames",
                    r.frames(10),
                    &r,
                );
                let r = probe(
                    &nowhere,
                    5,
                    Run {
                        source: "auto",
                        ..Run::default()
                    },
                );
                res.check(
                    "auto source falls back to PipeWire without a socket",
                    r.frames(5),
                    &r,
                );
            }
        } else {
            res.skip(
                "PipeWire node is published",
                "no PipeWire daemon reachable from the test",
            );
        }
        if on_path("v4l2-ctl") {
            let (ok, out, err) = under_interposer(
                sock_dir,
                "v4l2-ctl",
                &["-d", "/dev/video0", "--list-formats-ext"],
            );
            res.check(
                "v4l2-ctl identifies the device through sysfs and lists YU12",
                ok && out.contains("YU12"),
                format!("{out}{err}").trim().to_string(),
            );
            let (_, out, err) = under_interposer(sock_dir, "v4l2-ctl", &["--list-devices"]);
            res.check(
                "v4l2-ctl --list-devices shows the camera",
                out.contains("Selkies Virtual Camera") && out.contains("/dev/video0"),
                format!("{out}{err}").trim().to_string(),
            );
        } else {
            res.skip("v4l2-ctl identifies the device", "v4l2-ctl not installed");
        }
        let (ok, out, err) = under_interposer(sock_dir, "ls", &["-l", "/dev/video0"]);
        res.check(
            "ls sees the character device (statx)",
            ok && out.starts_with("crw"),
            format!("{out}{err}").trim().to_string(),
        );
        let (_, out, err) =
            under_interposer(sock_dir, "cat", &["/sys/class/video4linux/video0/name"]);
        res.check(
            "sysfs class entry carries the card name",
            out.trim() == "Selkies Virtual Camera",
            format!("{out}{err}").trim().to_string(),
        );
        // A reader blocked on the device sees the source end as ENODEV rather than waiting or
        // spinning on EAGAIN.
        blocked = {
            let (sock_dir, ended) = (sock_dir.to_path_buf(), ended.clone());
            thread::spawn(move || {
                let r = probe(
                    &sock_dir,
                    1000,
                    Run {
                        wait_ms: 8000,
                        ..Run::default()
                    },
                );
                *ended.lock().expect("the probe") = Some(r);
            })
        };
        thread::sleep(Duration::from_secs(2));
        stopped_at = Instant::now();
    }
    cam.stop();
    let deadline = Instant::now() + Duration::from_secs(6);
    while !blocked.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    let finished = blocked.is_finished();
    let ended = ended.lock().expect("the probe").take();
    res.check(
        "stopped source ends a blocked reader with ENODEV",
        finished
            && stopped_at.elapsed() < Duration::from_secs(5)
            && ended
                .as_ref()
                .is_some_and(|r| r.get("error").contains("No such device")),
        &ended,
    );
    res.check(
        "stop removes the socket",
        !sock_dir.join("selkies_webcam0.sock").exists(),
        sock_dir,
    );

    for (format, fourcc, bpl, size) in [
        ("NV12", "NV12", 640, 640 * 480 * 3 / 2),
        ("YUYV", "YUYV", 1280, 640 * 480 * 2),
    ] {
        let cam = start_camera(sock_dir, 640, 480, format);
        {
            let _feeder = Feeder::red_blue(&cam, 640, 480);
            let r = probe(
                sock_dir,
                10,
                Run {
                    samples: &[(320, 240)],
                    ..Run::default()
                },
            );
            res.check(
                &format!("{format}: format/stride/size"),
                (r.get("format"), r.get("bytesperline"), r.get("sizeimage"))
                    == (fourcc, &*bpl.to_string(), &*size.to_string()),
                (r.get("format"), r.get("bytesperline"), r.get("sizeimage")),
            );
            res.check(
                &format!("{format}: 10 frames with the right color"),
                r.get("frames") == "10" && red_or_blue(r.sample(320, 240)),
                &r.samples,
            );
        }
        cam.stop();
    }

    let cam = start_camera(sock_dir, 640, 480, "MJPEG");
    let dump = sock_dir.join("frame.jpg");
    {
        let _feeder = Feeder::red_blue(&cam, 640, 480);
        let r = probe(
            sock_dir,
            10,
            Run {
                dump: Some(&dump),
                ..Run::default()
            },
        );
        res.check(
            "MJPEG: device format/stride/size",
            (r.get("format"), r.get("bytesperline"), r.get("sizeimage"))
                == ("MJPG", "0", &*(640 * 480 * 2).to_string()),
            (r.get("format"), r.get("bytesperline"), r.get("sizeimage")),
        );
        res.check(
            "MJPEG: 10 JPEG frames",
            r.get("frames") == "10" && r.get("first_bytes").starts_with("ffd8"),
            &r,
        );
        let px = jpeg_center(&dump);
        res.check(
            "MJPEG: a frame of the device size passes through as the camera's JPEG",
            px.is_some_and(|(size, c)| {
                size == (640, 480) && (rgb_near(c, [255, 0, 0]) || rgb_near(c, [0, 0, 255]))
            }),
            px,
        );
        let st = stats(&cam);
        res.check(
            "MJPEG: passed through undecoded",
            st.passthrough > 0 && st.decoded == 0,
            &st,
        );
        if publishes_pipewire(&st) && interposer_reads_pipewire() {
            let r = probe(
                &sock_dir.join("no-socket-here"),
                5,
                Run {
                    source: "pipewire",
                    dump: Some(&dump),
                    ..Run::default()
                },
            );
            res.check(
                "MJPEG: the PipeWire source serves the video/mjpg node",
                r.rc == 0 && r.get("format") == "MJPG" && r.get("first_bytes").starts_with("ffd8"),
                &r,
            );
        }
    }
    // Near-white and near-black pictures: a re-encode that confused full- and limited-range
    // samples would land ~20 levels off.
    {
        let _feeder = Feeder::new(&cam, 320, 240, &[[250, 250, 250], [5, 5, 5]]);
        let r = probe(
            sock_dir,
            10,
            Run {
                dump: Some(&dump),
                ..Run::default()
            },
        );
        let px = jpeg_center(&dump);
        res.check(
            "MJPEG: a camera of another size is re-encoded at the device size, full range",
            r.get("frames") == "10"
                && px.is_some_and(|(size, c)| {
                    size == (640, 480) && (rgb_near(c, [250, 250, 250]) || rgb_near(c, [5, 5, 5]))
                }),
            px,
        );
        res.check(
            "MJPEG: re-encoded frames are decoded first",
            stats(&cam).decoded > 0,
            stats(&cam),
        );
    }
    cam.stop();
    match &h264 {
        None => res.skip(
            "MJPEG: H.264 uplink is re-encoded",
            "pixelflux encodes no H.264 here",
        ),
        Some(packets) => {
            let cam = start_camera(sock_dir, 320, 240, "MJPEG");
            let r = {
                let _feeder = feed_stream(&cam, packets, VirtualCamera::CODEC_H264);
                probe(
                    sock_dir,
                    10,
                    Run {
                        dump: Some(&dump),
                        ..Run::default()
                    },
                )
            };
            let px = jpeg_center(&dump);
            res.check(
                "MJPEG: an H.264 uplink is decoded and re-encoded as JPEG",
                r.get("frames") == "10"
                    && r.get("format") == "MJPG"
                    && px.is_some_and(|(size, c)| {
                        size == (320, 240) && c[1] > 150 && c[0] < 90 && c[2] < 90
                    }),
                (r.get("format"), px),
            );
            cam.stop();
        }
    }

    let cam = start_camera(sock_dir, 640, 480, "I420");
    {
        let _feeder = Feeder::red_blue(&cam, 640, 360);
        let r = probe(
            sock_dir,
            10,
            Run {
                samples: &[(320, 240), (320, 20), (320, 460), (5, 240)],
                ..Run::default()
            },
        );
        res.check(
            "letterbox: center keeps the camera color",
            red_or_blue(r.sample(320, 240)),
            &r.samples,
        );
        res.check(
            "letterbox: top and bottom bars are black",
            near(r.sample(320, 20), BLACK, 6) && near(r.sample(320, 460), BLACK, 6),
            &r.samples,
        );
        res.check(
            "letterbox: left edge is picture (not pillarboxed)",
            red_or_blue(r.sample(5, 240)),
            &r.samples,
        );
    }
    cam.stop();

    let cam = start_camera(sock_dir, 640, 480, "I420");
    {
        let _feeder = Feeder::red_blue(&cam, 240, 320);
        let r = probe(
            sock_dir,
            10,
            Run {
                samples: &[(320, 240), (20, 240), (620, 240)],
                ..Run::default()
            },
        );
        res.check(
            "pillarbox: center keeps the camera color",
            red_or_blue(r.sample(320, 240)),
            &r.samples,
        );
        res.check(
            "pillarbox: side bars are black",
            near(r.sample(20, 240), BLACK, 6) && near(r.sample(620, 240), BLACK, 6),
            &r.samples,
        );
    }
    cam.stop();

    for (name, packets, codec, green) in [
        ("h264", &h264, VirtualCamera::CODEC_H264, GREEN_H264),
        ("vp8", &vp8, VirtualCamera::CODEC_VP8, GREEN_VP8),
    ] {
        let Some(packets) = packets else {
            res.skip(
                &format!("{name}: decoded color"),
                &format!("pixelflux encodes no {name} here"),
            );
            continue;
        };
        let cam = start_camera(sock_dir, 320, 240, "I420");
        let r = {
            let _feeder = feed_stream(&cam, packets, codec);
            probe(
                sock_dir,
                20,
                Run {
                    samples: &[(160, 120), (10, 10)],
                    ..Run::default()
                },
            )
        };
        let st = stats(&cam);
        res.check(
            &format!("{name}: 20 frames decoded and delivered"),
            r.frames(20) && st.decoded >= 20,
            (&r.values, &st),
        );
        res.check(
            &format!("{name}: decoded color is the encoded green"),
            near(r.sample(160, 120), green, 10) && near(r.sample(10, 10), green, 10),
            &r.samples,
        );
        res.check(&format!("{name}: no decode errors"), st.errors == 0, &st);
        cam.stop();
    }

    let cam = start_camera(sock_dir, 320, 240, "I420");
    let p_frame = [0u8, 0, 0, 1, 0x41, 0x9A, 0x00, 0x10];
    let mut wanted = 0;
    for _ in 0..10 {
        wanted |= cam
            .push(&p_frame, VirtualCamera::CODEC_H264, false, 0, false)
            .unwrap_or(0);
        thread::sleep(Duration::from_millis(20));
    }
    res.check(
        "H.264 without a keyframe asks for one",
        wanted & VirtualCamera::KEYFRAME_WANTED != 0,
        wanted,
    );
    let st = stats(&cam);
    res.check(
        "non-keyframes are skipped, not decoded",
        st.skipped > 0 && st.decoded == 0,
        &st,
    );
    res.check(
        "unknown codec id is rejected",
        cam.push(&[0], 42, false, 0, false).is_err(),
        "",
    );
    cam.stop();
    res.check(
        "push after stop fails",
        cam.push(&[0], VirtualCamera::CODEC_MJPEG, false, 0, false)
            .is_err(),
        "",
    );
    assert!(res.0.is_empty(), "failed: {:?}", res.0);
}

/// A private directory under /tmp, removed when dropped: the camera's socket and the dumps.
struct TempDir(PathBuf);

impl TempDir {
    fn as_path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tempfile_dir() -> TempDir {
    let dir = PathBuf::from(format!("/tmp/selkies-webcam-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    TempDir(dir)
}
