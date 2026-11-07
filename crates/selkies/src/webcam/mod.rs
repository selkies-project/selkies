/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Virtual webcam: the client's camera uplink, served to applications as a V4L2 device.
//!
//! Both transports deliver encoded camera frames here, the WebSocket as `0x06` binary
//! messages carrying WebCodecs output and WebRTC as the depacketized frames of the recvonly
//! video m-line, and this module forwards them to the process-wide
//! [`pixelflux::webcam::VirtualCamera`]. Decoding, fitting, and publishing to every device
//! sink (the shared-memory ring behind the `selkies_v4l2_interposer.so` `LD_PRELOAD` library,
//! a PipeWire node, and a v4l2loopback device where one is configured) happen on pixelflux's
//! own thread; this module only gates the sender and hands the bytes over without copying
//! them.
//!
//! Lifetime is process-wide, like the gamepad interposer servers: applications open
//! `/dev/videoN` once at their own startup and must survive transport-mode switches and
//! browser reconnects, so one camera is shared by every client and is only stopped with the
//! server. Its format follows the first uplink, so a browser sending JPEG gets an MJPEG device
//! its frames pass through untouched and a WebCodecs/WebRTC browser an I420 device. A later
//! uplink of the other kind would otherwise be converted for the life of the process, so an
//! `auto` device is re-created for it, but only while no sink reports a consumer, since an
//! application holding the device is exactly what the process-wide lifetime is for
//! ([`VirtualWebcam::ensure`]). Under the `demand` webcam policy the same sinks decide when a
//! page is asked for its camera at all ([`demand`]).
//!
//! The WebSocket carries one encoded frame per `0x06` message: opcode, codec id, flags,
//! payload ([`WebcamFrame::parse`]). Besides the keyframe bit the flags byte carries the
//! frame's upright transform, which the client's encoder leaves out of the bitstream: bits 1-2
//! are the clockwise rotation in quarter turns, bit 3 a horizontal flip applied after the
//! rotation.

pub mod demand;

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use log::{error, info};
use pixelflux::webcam::{VirtualCamera, VirtualCameraSettings};

use crate::core::Role;
use crate::settings::{Settings, settings, socket_dir};

/// File name of the interposer socket inside the configured socket directory.
pub const WEBCAM_SOCKET_NAME: &str = "selkies_webcam0.sock";

/// Codec ids shared by the WebSocket frame header, the WebRTC codec names, and pixelflux's
/// `VirtualCamera::CODEC_*` constants.
pub const CODEC_MJPEG: u32 = VirtualCamera::CODEC_MJPEG;
pub const CODEC_H264: u32 = VirtualCamera::CODEC_H264;
pub const CODEC_VP8: u32 = VirtualCamera::CODEC_VP8;
pub const CODEC_VP9: u32 = VirtualCamera::CODEC_VP9;
pub const CODEC_AV1: u32 = VirtualCamera::CODEC_AV1;
pub const CODEC_HEVC: u32 = VirtualCamera::CODEC_HEVC;

/// The codec id a wire or settings name stands for (`jpeg` and `mjpeg`, `h265` and `hevc`
/// are the same codec), or `None` for a name no codec carries.
pub fn codec_by_name(name: &str) -> Option<u32> {
    match name.trim().to_ascii_lowercase().as_str() {
        "mjpeg" | "jpeg" => Some(CODEC_MJPEG),
        "h264" => Some(CODEC_H264),
        "vp8" => Some(CODEC_VP8),
        "vp9" => Some(CODEC_VP9),
        "av1" => Some(CODEC_AV1),
        "hevc" | "h265" => Some(CODEC_HEVC),
        _ => None,
    }
}

/// The binary opcode of a camera frame on the WebSocket.
pub const WS_OPCODE_WEBCAM: u8 = 0x06;
/// Opcode, codec id, and flags ahead of the payload.
pub const WS_HEADER_LEN: usize = 3;
pub const WS_FLAG_KEYFRAME: u8 = 0x01;
pub const WS_FLAG_ROTATION_SHIFT: u32 = 1;
pub const WS_FLAG_ROTATION_MASK: u8 = 0x06;
pub const WS_FLAG_HFLIP: u8 = 0x08;

/// Text control messages the server sends back on the WebSocket.
pub const MSG_WEBCAM_DISABLED: &str = "WEBCAM_DISABLED";
pub const MSG_WEBCAM_KEYFRAME: &str = "WEBCAM_KEYFRAME";

/// Clockwise rotation degrees and horizontal flip carried by a `0x06` flags byte.
pub fn orientation_from_flags(flags: u8) -> (u32, bool) {
    let rotation = ((flags & WS_FLAG_ROTATION_MASK) as u32 >> WS_FLAG_ROTATION_SHIFT) * 90;
    (rotation, flags & WS_FLAG_HFLIP != 0)
}

/// One encoded camera frame as the WebSocket carries it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WebcamFrame<'a> {
    pub codec: u32,
    pub keyframe: bool,
    /// Clockwise degrees that make the decoded frame upright.
    pub rotation: u32,
    /// Horizontal mirror, applied after the rotation.
    pub flip: bool,
    /// The encoded bytes, a slice of the message so nothing is copied.
    pub payload: &'a [u8],
}

impl<'a> WebcamFrame<'a> {
    /// Split a `0x06` message into its header fields and payload; `None` for a message that
    /// is too short or carries another opcode.
    pub fn parse(message: &'a [u8]) -> Option<Self> {
        if message.len() < WS_HEADER_LEN || message[0] != WS_OPCODE_WEBCAM {
            return None;
        }
        let (rotation, flip) = orientation_from_flags(message[2]);
        Some(WebcamFrame {
            codec: message[1] as u32,
            keyframe: message[2] & WS_FLAG_KEYFRAME != 0,
            rotation,
            flip,
            payload: &message[WS_HEADER_LEN..],
        })
    }
}

/// True when the operator locked the webcam off, so no client may feed it.
pub fn webcam_locked_off() -> bool {
    webcam_locked_off_in(&settings())
}

/// [`webcam_locked_off`] against an explicit settings table.
pub fn webcam_locked_off_in(settings: &Settings) -> bool {
    let enabled = settings.webcam_enabled();
    enabled.locked && !enabled.on
}

/// The shared gate for both transports: controllers and collaborators may feed the camera,
/// read-only viewers may not, and nobody may when it is locked off.
pub fn webcam_uplink_allowed(role: Role) -> bool {
    !webcam_locked_off() && role != Role::Viewer
}

/// Full path of the interposer socket, inside the configured socket directory.
pub fn webcam_socket_path() -> String {
    webcam_socket_path_in(&settings())
}

/// [`webcam_socket_path`] against an explicit settings table.
pub fn webcam_socket_path_in(settings: &Settings) -> String {
    Path::new(&socket_dir(settings.webcam_socket_path()))
        .join(WEBCAM_SOCKET_NAME)
        .to_string_lossy()
        .into_owned()
}

/// The device format for a `webcam_pixel_format` value and the codec of the frame that
/// brings the camera up.
///
/// `auto` follows that first uplink: an MJPEG uplink (a browser without WebCodecs) becomes
/// an MJPEG device carrying the browser's JPEG frames as received, with no decode on the
/// server, and anything else an I420 device; an explicit format is used as given, trimmed.
pub fn device_pixel_format(setting: &str, codec: Option<u32>) -> String {
    let name = setting.trim();
    if !name.is_empty() && !name.eq_ignore_ascii_case("auto") {
        return name.to_string();
    }
    if codec == Some(CODEC_MJPEG) {
        "MJPEG"
    } else {
        "I420"
    }
    .to_string()
}

/// Whether a `webcam_pixel_format` value leaves the format to the uplink.
fn auto_format(setting: &str) -> bool {
    let name = setting.trim();
    name.is_empty() || name.eq_ignore_ascii_case("auto")
}

/// Whether a device format name is the compressed one.
fn is_mjpeg_format(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_uppercase().as_str(),
        "MJPEG" | "MJPG" | "JPEG"
    )
}

/// Floor between the checks that ask whether anything still reads the camera. The answer
/// only changes when an application opens or closes the device, while the uplink asks on
/// every frame.
pub const REFORMAT_RECHECK: Duration = Duration::from_secs(5);

/// Whether any other process holds `path` open.
///
/// Only this process's own view of `/proc` is available, which in a container is every
/// process that matters; a reader it cannot see is why re-creating the device asks the sinks
/// first. Descriptors are compared as the kernel resolved them, since `canonicalize` would
/// re-resolve each.
pub fn device_has_openers(path: &str) -> bool {
    let Ok(target) = fs::canonicalize(path) else {
        return false;
    };
    let me = std::process::id().to_string();
    let Ok(procs) = fs::read_dir("/proc") else {
        return false;
    };
    for entry in procs.flatten() {
        let pid = entry.file_name();
        let Some(pid) = pid.to_str() else { continue };
        if pid == me || !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(fds) = fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            if fs::read_link(fd.path()).is_ok_and(|link| link == target) {
                return true;
            }
        }
    }
    false
}

/// What the camera is started with: the `webcam_*` settings as `VirtualCameraSettings`
/// reads them, read anew for every start so a changed setting reaches the next device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebcamConfig {
    pub socket_path: String,
    pub width: u32,
    pub height: u32,
    /// The `webcam_pixel_format` setting as given; `auto` is resolved per uplink.
    pub pixel_format: String,
    /// The `webcam_device` setting: `auto`, `false`, or a v4l2loopback path.
    pub device_path: String,
    /// Whether to publish a PipeWire node where a daemon is reachable.
    pub pipewire: bool,
}

impl WebcamConfig {
    /// The configuration the process's settings name.
    pub fn from_settings(settings: &Settings) -> WebcamConfig {
        WebcamConfig {
            socket_path: webcam_socket_path_in(settings),
            width: settings.webcam_width().max(0) as u32,
            height: settings.webcam_height().max(0) as u32,
            pixel_format: settings.webcam_pixel_format().to_string(),
            device_path: settings.webcam_device().to_string(),
            pipewire: true,
        }
    }

    /// The camera settings for the uplink `codec` brings up.
    fn camera_settings(&self, codec: Option<u32>) -> VirtualCameraSettings {
        VirtualCameraSettings {
            socket_path: self.socket_path.clone(),
            width: self.width,
            height: self.height,
            fps_num: 30,
            fps_den: 1,
            pixel_format: device_pixel_format(&self.pixel_format, codec),
            device_path: self.device_path.clone(),
            pipewire: self.pipewire,
            ..VirtualCameraSettings::default()
        }
    }
}

/// The source a [`VirtualWebcam`] reads its configuration from.
type ConfigSource = Arc<dyn Fn() -> WebcamConfig + Send + Sync>;

/// The device walk's last answer and when it was taken, for callers that accept an older one.
#[derive(Default)]
struct DeviceWalk {
    opener: Option<String>,
    checked_at: Option<Instant>,
}

/// Lazy owner of the process-wide `pixelflux` virtual camera.
///
/// The camera is started on the first frame rather than at server start, so sessions that
/// never enable the webcam pay nothing; a start failure is logged once and retried on a later
/// frame. The hot path, [`VirtualWebcam::push`] and [`VirtualWebcam::needs_ensure`] on a
/// running camera, takes one read lock and no allocation.
pub struct VirtualWebcam {
    config: ConfigSource,
    cam: RwLock<Option<Arc<VirtualCamera>>>,
    device_mjpeg: AtomicBool,
    start_failed_logged: AtomicBool,
    reformat_blocked_logged: AtomicBool,
    reformat_next_check: Mutex<Option<Instant>>,
    device_walk: Mutex<DeviceWalk>,
    /// Serializes starts, stops, and the re-create decision; held across the blocking
    /// work they run off the runtime, which is why it is tokio's.
    lock: tokio::sync::Mutex<()>,
}

impl Default for VirtualWebcam {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtualWebcam {
    /// A webcam configured from the process's settings.
    pub fn new() -> VirtualWebcam {
        Self::with_config(Arc::new(|| WebcamConfig::from_settings(&settings())))
    }

    /// A webcam reading its configuration from `config` at every start.
    pub fn with_config(config: ConfigSource) -> VirtualWebcam {
        VirtualWebcam {
            config,
            cam: RwLock::new(None),
            device_mjpeg: AtomicBool::new(false),
            start_failed_logged: AtomicBool::new(false),
            reformat_blocked_logged: AtomicBool::new(false),
            reformat_next_check: Mutex::new(None),
            device_walk: Mutex::new(DeviceWalk::default()),
            lock: tokio::sync::Mutex::new(()),
        }
    }

    /// The running camera, or `None` until the first successful start.
    pub fn camera(&self) -> Option<Arc<VirtualCamera>> {
        self.cam.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Whether [`VirtualWebcam::ensure`] has work to do for a frame of this codec.
    ///
    /// The per-frame answer for a running camera whose format already suits the uplink is
    /// no, which is the whole of the hot path; the rest is the rare case where an `auto`
    /// device was shaped by an uplink of the other kind and may be worth re-creating.
    pub fn needs_ensure(&self, codec: Option<u32>) -> bool {
        if self.camera().is_none() {
            return true;
        }
        let Some(codec) = codec else {
            return false;
        };
        if (codec == CODEC_MJPEG) == self.device_mjpeg.load(Ordering::Relaxed) {
            return false;
        }
        if !auto_format(&(self.config)().pixel_format) {
            return false;
        }
        match *self
            .reformat_next_check
            .lock()
            .unwrap_or_else(|e| e.into_inner())
        {
            None => true,
            Some(next) => Instant::now() >= next,
        }
    }

    /// What is reading the camera right now, or `None` when nothing is.
    ///
    /// Each sink answers for its own: the interposer counts the clients on its socket,
    /// PipeWire reports whether a consumer is linked to its node, and a kernel device's
    /// openers are found the only way a process can, through `/proc`.
    ///
    /// Args:
    ///     device_recheck: How long the device walk's last answer is reused; the walk visits
    ///         every descriptor in `/proc`, which a poll cannot afford every time. Zero walks
    ///         every time, as the format decision does.
    pub fn consumers(&self, device_recheck: Duration) -> Option<String> {
        let cam = self.camera()?;
        let running = cam.stats().running?;
        if running.clients > 0 {
            return Some("interposer client".to_string());
        }
        if running.pipewire_streaming {
            return Some("PipeWire consumer".to_string());
        }
        if running.device_path.is_empty() {
            return None;
        }
        self.device_opener(&running.device_path, device_recheck)
    }

    /// Whether an application holds the kernel device open, from a `/proc` walk no more often
    /// than `device_recheck`; in between, the last walk's answer.
    fn device_opener(&self, device: &str, device_recheck: Duration) -> Option<String> {
        let mut walk = self.device_walk.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let fresh = walk
            .checked_at
            .is_some_and(|at| now.duration_since(at) < device_recheck);
        if !fresh {
            walk.checked_at = Some(now);
            walk.opener =
                device_has_openers(device).then(|| format!("an application holding {device}"));
        }
        walk.opener.clone()
    }

    /// Returns the running camera, starting it on first use (off the runtime's workers).
    ///
    /// An `auto` device already running in the other format is re-created for this codec
    /// when nothing is reading it, so a session that follows one of the other kind neither
    /// transcodes every frame (a video uplink fitted into an MJPEG device) nor decodes one it
    /// could have passed through. A device with a consumer is left exactly as it is:
    /// applications hold it open across mode switches, and pulling the format out from under
    /// one is worse than the transcode.
    ///
    /// Args:
    ///     codec: Codec of the frame that brings the camera up, or that found the running
    ///         device in the other format; an `auto` device format follows it
    ///         ([`device_pixel_format`]).
    pub async fn ensure(self: &Arc<Self>, codec: Option<u32>) -> Option<Arc<VirtualCamera>> {
        if let Some(cam) = self.camera() {
            if !self.needs_ensure(codec) {
                return Some(cam);
            }
            let _guard = self.lock.lock().await;
            let Some(cam) = self.camera() else {
                return self.start_locked(codec).await;
            };
            if !self.needs_ensure(codec) {
                return Some(cam);
            }
            let me = Arc::clone(self);
            let reader = tokio::task::spawn_blocking(move || me.consumers(Duration::ZERO))
                .await
                .ok()
                .flatten();
            if let Some(reader) = reader {
                *self
                    .reformat_next_check
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(Instant::now() + REFORMAT_RECHECK);
                if !self.reformat_blocked_logged.swap(true, Ordering::Relaxed) {
                    info!(
                        target: "webcam",
                        "Virtual webcam stays {} for {}: {} is reading it. Frames are converted for the device; pin webcam_pixel_format to avoid it.",
                        if self.device_mjpeg.load(Ordering::Relaxed) { "MJPEG" } else { "raw" },
                        if codec == Some(CODEC_MJPEG) { "an MJPEG uplink" } else { "a video uplink" },
                        reader
                    );
                }
                return Some(cam);
            }
            info!(
                target: "webcam",
                "Virtual webcam re-created for the {} uplink now that nothing reads it.",
                if codec == Some(CODEC_MJPEG) { "MJPEG" } else { "video" }
            );
            self.stop_locked().await;
            return self.start_locked(codec).await;
        }
        let _guard = self.lock.lock().await;
        if let Some(cam) = self.camera() {
            return Some(cam);
        }
        self.start_locked(codec).await
    }

    /// Start a camera for `codec`; the caller holds the lock and no camera runs.
    async fn start_locked(&self, codec: Option<u32>) -> Option<Arc<VirtualCamera>> {
        let settings = (self.config)().camera_settings(codec);
        let cam = Arc::new(VirtualCamera::new());
        let started = {
            let (cam, settings) = (Arc::clone(&cam), settings.clone());
            tokio::task::spawn_blocking(move || cam.start(&settings)).await
        };
        let failure = match started {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e.to_string()),
            Err(e) => Some(e.to_string()),
        };
        if let Some(failure) = failure {
            if !self.start_failed_logged.swap(true, Ordering::Relaxed) {
                error!(target: "webcam", "Virtual webcam start failed: {failure}");
            }
            return None;
        }
        self.device_mjpeg
            .store(is_mjpeg_format(&settings.pixel_format), Ordering::Relaxed);
        self.reformat_blocked_logged.store(false, Ordering::Relaxed);
        *self
            .reformat_next_check
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        *self.device_walk.lock().unwrap_or_else(|e| e.into_inner()) = DeviceWalk::default();
        let stats = cam.stats();
        let running = stats.running.as_ref();
        let device = cam.device_path();
        info!(
            target: "webcam",
            "Virtual webcam serving {} ({}x{} {}, kernel device: {}, PipeWire node: {})",
            cam.socket_path().unwrap_or_default(),
            settings.width,
            settings.height,
            settings.pixel_format,
            if device.is_empty() { "none" } else { device.as_str() },
            if running.is_some_and(|r| r.pipewire) { "yes" } else { "no" }
        );
        *self.cam.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&cam));
        Some(cam)
    }

    /// Hands one encoded frame to the camera; returns its flags (the `KEYFRAME_WANTED` bit).
    ///
    /// A camera that is not running yet (`ensure` pending) drops the frame silently.
    ///
    /// Args:
    ///     rotation: Clockwise degrees (0/90/180/270) that make the decoded frame upright.
    ///     flip: Horizontal mirror, applied after the rotation.
    pub fn push(&self, data: &[u8], codec: u32, keyframe: bool, rotation: u32, flip: bool) -> u32 {
        let Some(cam) = self.camera() else {
            return 0;
        };
        match cam.push(data, codec, keyframe, rotation, flip) {
            Ok(flags) => flags,
            Err(e) => {
                error!(target: "webcam", "Virtual webcam push failed: {e}");
                0
            }
        }
    }

    /// Whether `push`'s flags ask the client for a keyframe.
    pub fn keyframe_wanted(flags: u32) -> bool {
        flags & VirtualCamera::KEYFRAME_WANTED != 0
    }

    /// Stops the running camera; the caller holds the lock.
    async fn stop_locked(&self) {
        let cam = self.cam.write().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(cam) = cam {
            let _ = tokio::task::spawn_blocking(move || cam.stop()).await;
        }
    }

    /// Stops the camera, closing every interposer client and removing the socket.
    pub async fn stop(&self) {
        let _guard = self.lock.lock().await;
        self.stop_locked().await;
    }
}

static SHARED: Mutex<Option<Arc<VirtualWebcam>>> = Mutex::new(None);

/// The process-wide webcam (created lazily, started on the first frame).
pub fn shared_webcam() -> Arc<VirtualWebcam> {
    let mut shared = SHARED.lock().unwrap_or_else(|e| e.into_inner());
    Arc::clone(shared.get_or_insert_with(|| Arc::new(VirtualWebcam::new())))
}

/// Stops and clears the process-wide webcam, if any.
pub async fn stop_shared_webcam() {
    let cam = SHARED.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(cam) = cam {
        cam.stop().await;
    }
}

/// A directory of this process's own for a test camera's socket.
#[cfg(test)]
fn test_socket_dir(tag: &str) -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "selkies-webcam-{}-{}-{}",
        std::process::id(),
        tag,
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).expect("test socket dir");
    dir
}

/// A configuration for a test camera: its own socket, no kernel device, no PipeWire node.
#[cfg(test)]
pub(crate) fn test_config(dir: &Path, pixel_format: &str) -> WebcamConfig {
    WebcamConfig {
        socket_path: dir.join(WEBCAM_SOCKET_NAME).to_string_lossy().into_owned(),
        width: 64,
        height: 48,
        pixel_format: pixel_format.to_string(),
        device_path: String::new(),
        pipewire: false,
    }
}

#[cfg(test)]
pub(crate) fn test_webcam(dir: &Path, pixel_format: &str) -> Arc<VirtualWebcam> {
    let config = test_config(dir, pixel_format);
    Arc::new(VirtualWebcam::with_config(Arc::new(move || config.clone())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::process::{Command, Stdio};

    use pixelflux::webcam::ring;

    /// The interposer's view of the ring, as `v4l2_interposer.c` mirrors it.
    #[test]
    fn ring_layout_matches_the_interposer() {
        assert_eq!(ring::SHM_MAGIC, 0x434B5753);
        assert_eq!(ring::SHM_VERSION, 1);
        assert_eq!(ring::CTRL_OFFSET, 128);
        assert_eq!(ring::CTRL_STRIDE, 64);
        assert_eq!(ring::DATA_OFFSET, 4096);
        assert_eq!(ring::MAX_SLOTS, 4);
        assert_eq!(ring::CONFIG_SIZE, 64);
        const { assert!(ring::CTRL_OFFSET + ring::MAX_SLOTS * ring::CTRL_STRIDE <= ring::DATA_OFFSET) };
    }

    #[test]
    fn codec_ids_match_the_wire() {
        let ids = [
            ("mjpeg", CODEC_MJPEG, 0),
            ("h264", CODEC_H264, 1),
            ("vp8", CODEC_VP8, 2),
            ("vp9", CODEC_VP9, 3),
            ("av1", CODEC_AV1, 4),
            ("hevc", CODEC_HEVC, 5),
        ];
        for (name, id, wire) in ids {
            assert_eq!(id, wire, "{name}");
            assert_eq!(codec_by_name(name), Some(wire), "{name}");
        }
        assert_eq!(codec_by_name("jpeg"), Some(CODEC_MJPEG));
        assert_eq!(codec_by_name("h265"), Some(CODEC_HEVC));
        assert_eq!(codec_by_name("gif"), None);
    }

    #[test]
    fn pixel_format_follows_the_first_uplink() {
        assert_eq!(device_pixel_format("auto", Some(CODEC_MJPEG)), "MJPEG");
        assert_eq!(device_pixel_format("auto", Some(CODEC_H264)), "I420");
        assert_eq!(device_pixel_format("auto", Some(CODEC_VP8)), "I420");
        assert_eq!(device_pixel_format("auto", None), "I420");
        assert_eq!(device_pixel_format("I420", Some(CODEC_MJPEG)), "I420");
        assert_eq!(device_pixel_format("MJPEG", Some(CODEC_H264)), "MJPEG");
        assert_eq!(device_pixel_format(" NV12 ", None), "NV12");
        assert_eq!(device_pixel_format("", Some(CODEC_MJPEG)), "MJPEG");
    }

    #[test]
    fn flags_carry_the_orientation() {
        assert_eq!(
            (
                WS_FLAG_KEYFRAME,
                WS_FLAG_ROTATION_SHIFT,
                WS_FLAG_ROTATION_MASK,
                WS_FLAG_HFLIP
            ),
            (0x01, 1, 0x06, 0x08)
        );
        let cases = [
            (0x00, (0, false)),
            (0x01, (0, false)),
            (0x03, (90, false)),
            (0x05, (180, false)),
            (0x07, (270, false)),
            (0x09, (0, true)),
            (0x0F, (270, true)),
        ];
        for (flags, expected) in cases {
            assert_eq!(
                orientation_from_flags(flags),
                expected,
                "flags {flags:#04x}"
            );
        }
        let frame = WebcamFrame::parse(&[0x06, 1, 0x0B, 9, 9]).expect("a frame");
        assert_eq!(
            frame,
            WebcamFrame {
                codec: 1,
                keyframe: true,
                rotation: 90,
                flip: true,
                payload: &[9, 9]
            }
        );
        assert_eq!(WebcamFrame::parse(&[0x06, 1]), None);
        assert_eq!(WebcamFrame::parse(&[0x04, 1, 0, 0]), None);
    }

    #[test]
    fn no_camera_drops_silently() {
        let cam = test_webcam(&test_socket_dir("drop"), "auto");
        assert_eq!(cam.push(b"x", CODEC_H264, true, 180, false), 0);
        assert!(cam.needs_ensure(Some(CODEC_H264)));
        assert_eq!(cam.consumers(Duration::ZERO), None);
    }

    #[tokio::test]
    async fn push_forwards_the_orientation() {
        let cam = test_webcam(&test_socket_dir("orient"), "auto");
        let camera = cam.ensure(Some(CODEC_H264)).await.expect("camera started");
        cam.push(b"x", CODEC_H264, true, 180, false);
        let stats = camera.stats();
        assert_eq!((stats.input_codec, stats.input_rotation), ("h264", 180));
        cam.push(b"x", CODEC_VP8, false, 0, true);
        assert_eq!(camera.stats().input_codec, "vp8");
        assert_eq!(
            cam.push(b"x", CODEC_H264, false, 45, false),
            0,
            "a bad rotation is refused"
        );
        assert!(!VirtualWebcam::keyframe_wanted(0));
        assert!(VirtualWebcam::keyframe_wanted(
            VirtualCamera::KEYFRAME_WANTED
        ));
        cam.stop().await;
    }

    #[tokio::test]
    async fn format_decision_reads_the_running_device() {
        let dir = test_socket_dir("reformat");
        let cam = test_webcam(&dir, "auto");
        cam.ensure(Some(CODEC_MJPEG)).await.expect("MJPEG camera");
        assert!(
            !cam.needs_ensure(Some(CODEC_MJPEG)),
            "an MJPEG uplink into an MJPEG device asks nothing"
        );
        assert!(
            cam.needs_ensure(Some(CODEC_H264)),
            "a video uplink into an MJPEG device asks"
        );
        assert!(!cam.needs_ensure(None));
        cam.stop().await;

        let cam = test_webcam(&dir, "auto");
        cam.ensure(Some(CODEC_VP8)).await.expect("I420 camera");
        assert!(
            !cam.needs_ensure(Some(CODEC_VP8)),
            "a video uplink into a raw device asks nothing"
        );
        assert!(
            cam.needs_ensure(Some(CODEC_MJPEG)),
            "an MJPEG uplink into a raw device asks"
        );
        cam.stop().await;

        let cam = test_webcam(&dir, "MJPEG");
        cam.ensure(Some(CODEC_MJPEG)).await.expect("pinned camera");
        assert!(
            !cam.needs_ensure(Some(CODEC_H264)),
            "a pinned format is never second-guessed"
        );
        cam.stop().await;
    }

    /// An interposer client: connected, handed the ring, and acknowledged with its one byte.
    fn interposer_client(socket_path: &str) -> UnixStream {
        let mut client = UnixStream::connect(socket_path).expect("connect");
        let mut config = [0u8; ring::CONFIG_SIZE];
        client.read_exact(&mut config).expect("config");
        assert_eq!(
            u32::from_le_bytes(config[..4].try_into().unwrap()),
            ring::SHM_MAGIC
        );
        client.write_all(&[1]).expect("handshake");
        client
    }

    async fn wait_for_clients(camera: &VirtualCamera, want: usize) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while camera.clients() != want && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(camera.clients(), want);
    }

    #[tokio::test]
    async fn an_unread_device_is_recreated_and_a_read_one_kept() {
        let cam = test_webcam(&test_socket_dir("recreate"), "auto");
        let old = cam.ensure(Some(CODEC_MJPEG)).await.expect("MJPEG camera");
        assert_eq!(
            old.stats().running.map(|r| r.pixel_format),
            Some("MJPG".to_string())
        );
        assert_eq!(
            cam.consumers(Duration::ZERO),
            None,
            "a device nothing has open does not hold it"
        );

        let new = cam
            .ensure(Some(CODEC_H264))
            .await
            .expect("re-created camera");
        assert!(
            !Arc::ptr_eq(&old, &new),
            "an unread device is re-created for the video uplink"
        );
        assert!(!old.is_running());
        assert_eq!(
            new.stats().running.map(|r| r.pixel_format),
            Some("YU12".to_string())
        );

        let client = interposer_client(&new.socket_path().unwrap());
        wait_for_clients(&new, 1).await;
        assert_eq!(
            cam.consumers(Duration::ZERO).as_deref(),
            Some("interposer client")
        );
        let kept = cam.ensure(Some(CODEC_MJPEG)).await.expect("kept camera");
        assert!(
            Arc::ptr_eq(&kept, &new),
            "a device with a reader is kept as it is"
        );
        assert!(new.is_running());
        assert!(
            !cam.needs_ensure(Some(CODEC_MJPEG)),
            "and is not asked again until the floor passes"
        );
        drop(client);
        cam.stop().await;
    }

    #[test]
    fn device_walk_finds_an_opener_in_another_process() {
        let dir = test_socket_dir("walk");
        let held = dir.join("device");
        fs::write(&held, b"").unwrap();
        let path = held.to_string_lossy().into_owned();
        assert!(
            !device_has_openers(&path),
            "a path nothing holds has no openers"
        );
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::from(fs::File::open(&held).unwrap()))
            .stdout(Stdio::null())
            .spawn()
            .expect("sleep");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !device_has_openers(&path) {
            std::thread::sleep(Duration::from_millis(100));
        }
        let found = device_has_openers(&path);
        let _ = child.kill();
        let _ = child.wait();
        assert!(found, "a path a child process holds open is found");
        assert!(!device_has_openers("/nonexistent/device"));
    }

    #[test]
    fn device_walk_is_held_to_its_floor() {
        let dir = test_socket_dir("floor");
        let device = dir.join("video");
        fs::write(&device, b"").unwrap();
        let device = device.to_string_lossy().into_owned();
        let cam = test_webcam(&dir, "auto");
        let recheck = Duration::from_secs(4);
        assert_eq!(cam.device_opener(&device, recheck), None);
        let first = cam.device_walk.lock().unwrap().checked_at;
        cam.device_opener(&device, recheck);
        cam.device_opener(&device, recheck);
        assert_eq!(
            cam.device_walk.lock().unwrap().checked_at,
            first,
            "three polls inside the floor walk once"
        );
        cam.device_walk.lock().unwrap().checked_at = first.map(|t| t - recheck);
        cam.device_opener(&device, recheck);
        let second = cam.device_walk.lock().unwrap().checked_at;
        assert_ne!(second, first, "past the floor it walks again");
        cam.device_opener(&device, Duration::ZERO);
        assert_ne!(
            cam.device_walk.lock().unwrap().checked_at,
            second,
            "the format decision walks every time"
        );
        let holder = fs::File::open(&device).unwrap();
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::from(holder))
            .spawn()
            .expect("sleep");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && cam.device_opener(&device, Duration::ZERO).is_none() {
            std::thread::sleep(Duration::from_millis(100));
        }
        let named = cam.device_opener(&device, Duration::ZERO);
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(
            named,
            Some(format!("an application holding {device}")),
            "the walk's answer names the device"
        );
    }
}
