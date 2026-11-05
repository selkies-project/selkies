//! Virtual camera: the browser's webcam, delivered to applications as a V4L2 capture device.
//!
//! The client encodes its camera (H.264/VP8/… over the WebRTC media track or WebCodecs over the
//! WebSocket, MJPEG as the last-resort canvas path) and Selkies hands each encoded frame to
//! [`VirtualCamera::push`], which returns at once. A worker thread decodes, fits the picture into the
//! device's fixed raw format, and publishes it to every sink at once:
//!
//! - the shared-memory ring served over a Unix socket to the Selkies V4L2 interposer
//!   (`LD_PRELOAD`, no privileges, no kernel module), see [`ring`] and [`server`];
//! - a v4l2loopback output device where one is configured or found, see [`v4l2out`] — the kernel
//!   path for hosts and privileged containers, as `/dev/uinput` is for gamepads;
//! - a PipeWire `Video/Source` node where a daemon is reachable, see [`pipewire`] — for
//!   PipeWire-native consumers and the `pipewire-v4l2` wrapper.
//!
//! Nothing on the hot path copies more than the encoded bytes into a pooled buffer, and no
//! worker thread ever calls back into the consumer.

pub mod convert;
pub mod decode;
pub mod pipewire;
pub mod ring;
pub mod server;
pub mod v4l2out;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::Error;

use convert::{DeviceFormat, Normalizer, Orientation, orient_i420};
use decode::{Codec, DecodeError, Decoder, new_decoder, sniff_keyframe};
use pipewire::PipeWireSink;
use ring::{Ring, RingFormat};
use server::Server;
use v4l2out::V4l2Output;

/// Configuration for [`VirtualCamera::start`].
#[derive(Clone, Debug, PartialEq)]
pub struct VirtualCameraSettings {
    /// Unix socket the interposer connects to.
    pub socket_path: String,
    /// Advertised frame size; incoming frames are scaled and letterboxed to fit.
    pub width: u32,
    pub height: u32,
    /// Advertised frame rate as a fraction.
    pub fps_num: u32,
    pub fps_den: u32,
    /// Raw device pixel format: "I420", "NV12", or "YUYV".
    /// Device pixel format: "I420" (the default; the browsers' preference), "NV12", "YUYV",
    /// or "MJPEG" — a compressed device that carries an MJPEG uplink's frames as received
    /// (no decode, one copy) and re-encodes only frames that must be fitted into the device
    /// size or arrive in another codec.
    pub pixel_format: String,
    /// Ring slots (2..4); more slots tolerate slower readers.
    pub slots: u32,
    /// Encoded frames buffered ahead of the decoder before the oldest is dropped.
    pub queue_depth: u32,
    /// v4l2loopback output device: "" for none, "auto" to use the first one found, or a path.
    pub device_path: String,
    /// Also publish the camera as a PipeWire `Video/Source` node when a daemon is reachable.
    pub pipewire: bool,
    /// PipeWire node name (the description shown to users is fixed).
    pub pipewire_node_name: String,
}

impl Default for VirtualCameraSettings {
    fn default() -> Self {
        VirtualCameraSettings {
            socket_path: format!("{}/selkies_webcam0.sock", crate::webcam::socket_dir()),
            width: 1280,
            height: 720,
            fps_num: 30,
            fps_den: 1,
            pixel_format: "I420".into(),
            slots: 3,
            queue_depth: 4,
            device_path: "auto".into(),
            pipewire: true,
            pipewire_node_name: "selkies-webcam".into(),
        }
    }
}

/// Where the camera socket lives unless a caller names one: the session's private runtime
/// directory, where no other account can take the name first, else `/tmp`. The v4l2
/// interposer looks in the same place.
pub fn socket_dir() -> String {
    std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/tmp".into())
}

fn parse_pixel_format(name: &str) -> Option<u32> {
    match name.trim().to_ascii_uppercase().as_str() {
        "I420" | "YU12" | "YUV420" => Some(ring::V4L2_PIX_FMT_YUV420),
        "NV12" => Some(ring::V4L2_PIX_FMT_NV12),
        "YUYV" | "YUY2" => Some(ring::V4L2_PIX_FMT_YUYV),
        "MJPEG" | "MJPG" | "JPEG" => Some(ring::V4L2_PIX_FMT_MJPEG),
        _ => None,
    }
}

/// Counters read by `VirtualCamera.stats()`.
#[derive(Default)]
struct Stats {
    pushed: AtomicU64,
    decoded: AtomicU64,
    published: AtomicU64,
    passthrough: AtomicU64,
    dropped: AtomicU64,
    skipped: AtomicU64,
    errors: AtomicU64,
    input_width: AtomicU32,
    input_height: AtomicU32,
    input_codec: AtomicU32,
    input_rotation: AtomicU32,
    /// Whether a PipeWire consumer was linked at the last fan-out. The interposer counts its
    /// own clients and a kernel device's openers are the kernel's to know, so this is the one
    /// sink whose consumers only it can see.
    pipewire_streaming: AtomicBool,
}

struct Job {
    codec: Codec,
    keyframe: bool,
    orientation: Orientation,
    data: Vec<u8>,
}

struct QueueState {
    jobs: VecDeque<Job>,
    closed: bool,
}

/// Bounded hand-off to the decoder thread that drops the oldest frame when the decoder falls
/// behind: a camera must stay live, never grow latency.
struct Queue {
    state: Mutex<QueueState>,
    cv: Condvar,
    capacity: usize,
}

impl Queue {
    fn new(capacity: usize) -> Self {
        Queue {
            state: Mutex::new(QueueState {
                jobs: VecDeque::new(),
                closed: false,
            }),
            cv: Condvar::new(),
            capacity: capacity.max(1),
        }
    }

    /// Returns the job evicted to make room, if any.
    fn push(&self, job: Job) -> Option<Job> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let evicted = if st.jobs.len() >= self.capacity {
            st.jobs.pop_front()
        } else {
            None
        };
        st.jobs.push_back(job);
        drop(st);
        self.cv.notify_one();
        evicted
    }

    fn pop(&self) -> Option<Job> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(j) = st.jobs.pop_front() {
                return Some(j);
            }
            if st.closed {
                return None;
            }
            st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn close(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).closed = true;
        self.cv.notify_all();
    }
}

/// Recycled encoded-frame buffers so the steady state allocates nothing per frame.
struct Pool(Mutex<Vec<Vec<u8>>>);

impl Pool {
    fn take(&self, len: usize) -> Vec<u8> {
        let mut v = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop()
            .unwrap_or_default();
        v.clear();
        v.reserve(len);
        v
    }

    fn put(&self, v: Vec<u8>) {
        let mut pool = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if pool.len() < 8 {
            pool.push(v);
        }
    }
}

struct Running {
    queue: Arc<Queue>,
    thread: Option<JoinHandle<()>>,
    server: Arc<Server>,
    keyframe_wanted: Arc<AtomicBool>,
    pool: Arc<Pool>,
    device_path: Arc<Mutex<String>>,
    format: RingFormat,
    pipewire: bool,
}

/// A virtual camera fed with encoded frames by the consumer.
pub struct VirtualCamera {
    running: Mutex<Option<Running>>,
    stats: Arc<Stats>,
}

fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

struct WorkerConfig {
    queue: Arc<Queue>,
    ring: Ring,
    server: Arc<Server>,
    stats: Arc<Stats>,
    keyframe_wanted: Arc<AtomicBool>,
    pool: Arc<Pool>,
    v4l2out: Option<V4l2Output>,
    device_path: Arc<Mutex<String>>,
    pipewire: Option<PipeWireSink>,
}

/// Re-encode quality for frames that must be fitted into an MJPEG device (a camera of another
/// size, an H.264/VP8 uplink); frames of the device's own size pass through as received.
const MJPEG_REENCODE_QUALITY: i32 = 85;

/// JPEG writer for the MJPEG device: the fitted, full-range I420 picture compressed straight into
/// the ring slot.
struct MjpegEncoder {
    comp: turbojpeg::Compressor,
    i420: Vec<u8>,
}

impl MjpegEncoder {
    fn new() -> Result<Self, String> {
        let mut comp =
            turbojpeg::Compressor::new().map_err(|e| format!("turbojpeg compressor: {}", e))?;
        comp.set_quality(MJPEG_REENCODE_QUALITY)
            .map_err(|e| format!("turbojpeg quality: {}", e))?;
        comp.set_subsamp(turbojpeg::Subsamp::Sub2x2)
            .map_err(|e| format!("turbojpeg subsampling: {}", e))?;
        Ok(MjpegEncoder {
            comp,
            i420: Vec::new(),
        })
    }

    fn write_frame(
        &mut self,
        normalizer: &mut Normalizer,
        src: &convert::I420View<'_>,
        dev: &DeviceFormat,
        out: &mut [u8],
    ) -> usize {
        let need = dev.frame_bytes();
        self.i420.resize(need, 0);
        if normalizer.write_frame(src, dev, &mut self.i420) == 0 {
            return 0;
        }
        let img = turbojpeg::YuvImage {
            pixels: &self.i420[..],
            width: dev.width,
            align: 1,
            height: dev.height,
            subsamp: turbojpeg::Subsamp::Sub2x2,
        };
        self.comp.compress_yuv_to_slice(img, out).unwrap_or(0)
    }
}

/// After a publish: wake the socket clients and mirror the frame into the kernel device and the
/// PipeWire node where those sinks are up, retiring a sink that failed.
fn fan_out(
    ring: &Ring,
    server: &Server,
    v4l2out: &mut Option<V4l2Output>,
    device_path: &Mutex<String>,
    pipewire: &mut Option<PipeWireSink>,
    ts: u64,
    stats: &Stats,
) {
    server.ring_doorbell();
    if let Some(frame) = ring.latest_frame() {
        if let Some(out) = v4l2out.as_mut() {
            out.write_frame(frame);
            if out.is_failed() {
                device_path
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clear();
                *v4l2out = None;
            }
        }
        if let Some(pw) = pipewire.as_mut() {
            pw.publish(frame, ts);
            stats
                .pipewire_streaming
                .store(pw.is_streaming(), Ordering::Relaxed);
            if pw.is_failed() {
                eprintln!("[webcam] PipeWire node stopped; PipeWire sink disabled");
                *pipewire = None;
                stats.pipewire_streaming.store(false, Ordering::Relaxed);
            }
        }
    }
}

/// Decoder thread body: decode, fit, publish, wake clients. A dropped or undecodable inter-coded
/// frame parks the stream until the next keyframe and asks the client for one, so a consumer never
/// sees the smear of predictions built on a missing reference.
fn worker(cfg: WorkerConfig) {
    let WorkerConfig {
        queue,
        mut ring,
        server,
        stats,
        keyframe_wanted,
        pool,
        mut v4l2out,
        device_path,
        mut pipewire,
    } = cfg;
    let fmt = *ring.format();
    let dev = DeviceFormat {
        width: fmt.width as usize,
        height: fmt.height as usize,
        fourcc: fmt.fourcc,
    };
    let mut normalizer = Normalizer::new();
    let mut oriented = convert::I420Buffer::new(2, 2);
    let mut decoder: Option<Box<dyn Decoder>> = None;
    let mut need_keyframe = true;
    let mut last_error_log = Instant::now() - Duration::from_secs(60);
    let log_error = |msg: String, last: &mut Instant| {
        if last.elapsed() >= Duration::from_secs(5) {
            eprintln!("[webcam] {}", msg);
            *last = Instant::now();
        }
    };
    let mjpeg_device = dev.fourcc == ring::V4L2_PIX_FMT_MJPEG;
    let mut jpeg_out = if mjpeg_device {
        match MjpegEncoder::new() {
            Ok(enc) => Some(enc),
            Err(e) => {
                eprintln!(
                    "[webcam] {}; only MJPEG frames of the device size can be served",
                    e
                );
                None
            }
        }
    } else {
        None
    };

    while let Some(job) = queue.pop() {
        if mjpeg_device && job.codec == Codec::Jpeg && job.orientation.is_upright() {
            // The device speaks the uplink's own format: an upright frame of the device's
            // size is published as received, undecoded; any other goes through decode and fit.
            if let Ok(hdr) = turbojpeg::read_header(&job.data) {
                stats.input_width.store(hdr.width as u32, Ordering::Relaxed);
                stats
                    .input_height
                    .store(hdr.height as u32, Ordering::Relaxed);
                let n = job.data.len();
                if hdr.width == dev.width && hdr.height == dev.height && n <= fmt.sizeimage as usize
                {
                    let ts = monotonic_ns();
                    if ring.publish(ts, |slot| {
                        slot[..n].copy_from_slice(&job.data);
                        n
                    }) {
                        stats.passthrough.fetch_add(1, Ordering::Relaxed);
                        stats.published.fetch_add(1, Ordering::Relaxed);
                        fan_out(
                            &ring,
                            &server,
                            &mut v4l2out,
                            &device_path,
                            &mut pipewire,
                            ts,
                            &stats,
                        );
                    }
                    pool.put(job.data);
                    continue;
                }
            }
        }
        if decoder.as_ref().map(|d| d.codec()) != Some(job.codec) {
            match new_decoder(job.codec) {
                Ok(d) => decoder = Some(d),
                Err(e) => {
                    stats.errors.fetch_add(1, Ordering::Relaxed);
                    log_error(
                        format!("no decoder for {}: {}", job.codec.name(), e),
                        &mut last_error_log,
                    );
                    pool.put(job.data);
                    continue;
                }
            }
            need_keyframe = true;
        }
        let dec = decoder.as_mut().expect("decoder present");
        let keyframe = job.keyframe || sniff_keyframe(job.codec, &job.data).unwrap_or(false);
        if need_keyframe && job.codec.is_inter_coded() && !keyframe {
            stats.skipped.fetch_add(1, Ordering::Relaxed);
            keyframe_wanted.store(true, Ordering::Relaxed);
            pool.put(job.data);
            continue;
        }
        match dec.decode(&job.data) {
            Ok(true) => {
                need_keyframe = false;
                stats.decoded.fetch_add(1, Ordering::Relaxed);
                if let Some(view) = dec.frame() {
                    stats
                        .input_width
                        .store(view.width as u32, Ordering::Relaxed);
                    stats
                        .input_height
                        .store(view.height as u32, Ordering::Relaxed);
                    let view = if job.orientation.is_upright() {
                        view
                    } else {
                        orient_i420(&view, job.orientation, &mut oriented);
                        oriented.view(view.full_range)
                    };
                    let ts = monotonic_ns();
                    let published = ring.publish(ts, |slot| match jpeg_out.as_mut() {
                        Some(enc) => enc.write_frame(&mut normalizer, &view, &dev, slot),
                        None if mjpeg_device => 0,
                        None => normalizer.write_frame(&view, &dev, slot),
                    });
                    if published {
                        stats.published.fetch_add(1, Ordering::Relaxed);
                        fan_out(
                            &ring,
                            &server,
                            &mut v4l2out,
                            &device_path,
                            &mut pipewire,
                            ts,
                            &stats,
                        );
                    }
                }
            }
            Ok(false) => {}
            Err(DecodeError::Corrupt(e)) => {
                stats.errors.fetch_add(1, Ordering::Relaxed);
                if job.codec.is_inter_coded() {
                    need_keyframe = true;
                    keyframe_wanted.store(true, Ordering::Relaxed);
                }
                log_error(
                    format!("{} decode error: {}", job.codec.name(), e),
                    &mut last_error_log,
                );
            }
            Err(DecodeError::Fatal(e)) => {
                stats.errors.fetch_add(1, Ordering::Relaxed);
                log_error(
                    format!("{} decoder reset: {}", job.codec.name(), e),
                    &mut last_error_log,
                );
                decoder = None;
                need_keyframe = true;
                keyframe_wanted.store(true, Ordering::Relaxed);
            }
        }
        pool.put(job.data);
    }
}

impl Default for VirtualCamera {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtualCamera {
    /// The codec ids `push` takes, as the client names a frame's codec on the wire.
    pub const CODEC_MJPEG: u32 = Codec::Jpeg.id();
    pub const CODEC_H264: u32 = Codec::H264.id();
    pub const CODEC_VP8: u32 = Codec::Vp8.id();
    pub const CODEC_VP9: u32 = Codec::Vp9.id();
    pub const CODEC_AV1: u32 = Codec::Av1.id();
    pub const CODEC_HEVC: u32 = Codec::H265.id();
    /// `push()` result bit: the decoder needs a keyframe from the client to resume.
    pub const KEYFRAME_WANTED: u32 = 1;

    pub fn new() -> Self {
        VirtualCamera {
            running: Mutex::new(None),
            stats: Arc::new(Stats::default()),
        }
    }

    /// Bind the socket, allocate the ring, open the kernel device if configured, and start the
    /// decoder thread. Restarting a running camera stops it first.
    pub fn start(&self, settings: &VirtualCameraSettings) -> Result<(), Error> {
        let fourcc = parse_pixel_format(&settings.pixel_format).ok_or_else(|| {
            Error::Settings(format!(
                "unsupported pixel_format '{}'",
                settings.pixel_format
            ))
        })?;
        if settings.width < 2
            || settings.height < 2
            || settings.width > 8192
            || settings.height > 8192
        {
            return Err(Error::Settings("width/height out of range".into()));
        }
        if settings.fps_num == 0 || settings.fps_den == 0 {
            return Err(Error::Settings("fps_num/fps_den must be positive".into()));
        }
        let fmt = RingFormat::for_fourcc(
            fourcc,
            settings.width & !1,
            settings.height & !1,
            settings.fps_num,
            settings.fps_den,
        )
        .ok_or_else(|| Error::Settings("unsupported pixel_format".into()))?;
        let socket_path = settings.socket_path.clone();
        let slots = settings.slots;
        let queue_depth = settings.queue_depth.max(1) as usize;
        let device_setting = settings.device_path.trim().to_string();
        let want_pipewire = settings.pipewire;
        let pipewire_node_name = settings.pipewire_node_name.trim().to_string();
        let stats = self.stats.clone();

        self.stop();

        let started = (move || -> Result<Running, String> {
            let ring =
                Ring::new(fmt, slots).map_err(|e| format!("ring allocation failed: {}", e))?;
            let server = Arc::new(
                Server::bind(&socket_path, ring.config_bytes(), ring.fd())
                    .map_err(|e| format!("bind({}) failed: {}", socket_path, e))?,
            );
            let v4l2out = match device_setting.as_str() {
                "" | "false" | "no" | "off" | "none" => None,
                "auto" | "true" | "yes" | "on" => {
                    V4l2Output::find_loopback_device().and_then(|p| {
                        match V4l2Output::open(&p, &fmt) {
                            Ok(o) => Some(o),
                            Err(e) => {
                                eprintln!("[webcam] {}; kernel device sink disabled", e);
                                None
                            }
                        }
                    })
                }
                path => match V4l2Output::open(path, &fmt) {
                    Ok(o) => Some(o),
                    Err(e) => {
                        eprintln!("[webcam] {}; kernel device sink disabled", e);
                        None
                    }
                },
            };
            let device_path = Arc::new(Mutex::new(
                v4l2out
                    .as_ref()
                    .map(|o| o.path().to_string())
                    .unwrap_or_default(),
            ));
            let pipewire = if want_pipewire {
                match PipeWireSink::connect(&pipewire_node_name, "Selkies Virtual Camera", &fmt) {
                    Ok(sink) => Some(sink),
                    Err(e) => {
                        eprintln!("[webcam] PipeWire sink unavailable: {}", e);
                        None
                    }
                }
            } else {
                None
            };
            let pipewire_on = pipewire.is_some();
            let queue = Arc::new(Queue::new(queue_depth));
            let keyframe_wanted = Arc::new(AtomicBool::new(true));
            let pool = Arc::new(Pool(Mutex::new(Vec::new())));
            let cfg = WorkerConfig {
                queue: queue.clone(),
                ring,
                server: server.clone(),
                stats,
                keyframe_wanted: keyframe_wanted.clone(),
                pool: pool.clone(),
                v4l2out,
                device_path: device_path.clone(),
                pipewire,
            };
            let thread = thread::Builder::new()
                .name("pixelflux-webcam".into())
                .spawn(move || worker(cfg))
                .map_err(|e| format!("decoder thread spawn failed: {}", e))?;
            Ok(Running {
                queue,
                thread: Some(thread),
                server,
                keyframe_wanted,
                pool,
                device_path,
                format: fmt,
                pipewire: pipewire_on,
            })
        })();
        match started {
            Ok(r) => {
                *self.running.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
                Ok(())
            }
            Err(e) => Err(Error::Capture(e)),
        }
    }

    /// Hand one encoded frame to the decoder. `data` is the encoded payload; empty data pushes
    /// nothing and only reports. `rotation` (clockwise degrees: 0, 90, 180, or 270) and `flip`
    /// (a horizontal mirror applied after the rotation) carry the frame's upright transform when
    /// the client's encoder left it as metadata instead of baking it into the pixels; the decoded
    /// picture is oriented before it is fitted. Returns a bit set (`KEYFRAME_WANTED`) the caller
    /// relays to the client. Fails when the camera is not running, the codec id is unknown, or the
    /// rotation is not a quarter turn.
    pub fn push(
        &self,
        data: &[u8],
        codec: u32,
        keyframe: bool,
        rotation: u32,
        flip: bool,
    ) -> Result<u32, Error> {
        let codec = Codec::from_id(codec)
            .ok_or_else(|| Error::Settings(format!("unknown codec id {}", codec)))?;
        if !rotation.is_multiple_of(90) || rotation >= 360 {
            return Err(Error::Settings(
                "rotation must be 0, 90, 180, or 270".into(),
            ));
        }
        let orientation = Orientation {
            quarter_turns: (rotation / 90) as u8,
            hflip: flip,
        };
        let guard = self.running.lock().unwrap_or_else(|e| e.into_inner());
        let running = guard
            .as_ref()
            .ok_or_else(|| Error::Capture("virtual camera is not running".into()))?;
        if data.is_empty() {
            return Ok(running.keyframe_wanted.swap(false, Ordering::Relaxed) as u32);
        }
        let mut buf = running.pool.take(data.len());
        buf.extend_from_slice(data);
        self.stats.pushed.fetch_add(1, Ordering::Relaxed);
        self.stats
            .input_codec
            .store(codec as u32, Ordering::Relaxed);
        self.stats.input_rotation.store(rotation, Ordering::Relaxed);
        if let Some(evicted) = running.queue.push(Job {
            codec,
            keyframe,
            orientation,
            data: buf,
        }) {
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            if evicted.codec.is_inter_coded() {
                running.keyframe_wanted.store(true, Ordering::Relaxed);
            }
            running.pool.put(evicted.data);
        }
        Ok(running.keyframe_wanted.swap(false, Ordering::Relaxed) as u32)
    }

    /// Stop the decoder thread, close every interposer client, and remove the socket.
    pub fn stop(&self) {
        let running = self
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(mut r) = running {
            r.queue.close();
            if let Some(t) = r.thread.take() {
                let _ = t.join();
            }
            drop(r.server);
        }
    }

    pub fn is_running(&self) -> bool {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// Interposer clients that completed the handshake.
    pub fn clients(&self) -> usize {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|r| r.server.client_count())
            .unwrap_or(0)
    }

    pub fn socket_path(&self) -> Option<String> {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|r| r.server.path().to_string())
    }

    /// Kernel device currently mirrored, or "" when none is in use.
    pub fn device_path(&self) -> String {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|r| {
                r.device_path
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
            })
            .unwrap_or_default()
    }

    /// Counters and the negotiated geometry.
    pub fn stats(&self) -> CameraStats {
        let s = &self.stats;
        let guard = self.running.lock().unwrap_or_else(|e| e.into_inner());
        CameraStats {
            pushed: s.pushed.load(Ordering::Relaxed),
            decoded: s.decoded.load(Ordering::Relaxed),
            published: s.published.load(Ordering::Relaxed),
            passthrough: s.passthrough.load(Ordering::Relaxed),
            dropped: s.dropped.load(Ordering::Relaxed),
            skipped: s.skipped.load(Ordering::Relaxed),
            errors: s.errors.load(Ordering::Relaxed),
            input_width: s.input_width.load(Ordering::Relaxed),
            input_height: s.input_height.load(Ordering::Relaxed),
            input_rotation: s.input_rotation.load(Ordering::Relaxed),
            input_codec: Codec::from_id(s.input_codec.load(Ordering::Relaxed))
                .map(|c| c.name())
                .unwrap_or(""),
            running: guard.as_ref().map(|r| CameraRunning {
                clients: r.server.client_count(),
                width: r.format.width,
                height: r.format.height,
                fps_num: r.format.fps_num,
                fps_den: r.format.fps_den,
                pixel_format: v4l2out::fourcc_str(r.format.fourcc),
                socket_path: r.server.path().to_string(),
                device_path: r
                    .device_path
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
                pipewire: r.pipewire,
                pipewire_streaming: s.pipewire_streaming.load(Ordering::Relaxed),
            }),
        }
    }
}

/// What `VirtualCamera::stats` reports: the counters since the camera was created, what the
/// client last sent, and the running camera's geometry and sinks where one runs.
#[derive(Clone, Debug, PartialEq)]
pub struct CameraStats {
    pub pushed: u64,
    pub decoded: u64,
    pub published: u64,
    pub passthrough: u64,
    pub dropped: u64,
    pub skipped: u64,
    pub errors: u64,
    pub input_width: u32,
    pub input_height: u32,
    pub input_rotation: u32,
    /// The codec of the last frame pushed, empty before the first.
    pub input_codec: &'static str,
    pub running: Option<CameraRunning>,
}

/// The running camera's negotiated format and sinks (`CameraStats::running`).
#[derive(Clone, Debug, PartialEq)]
pub struct CameraRunning {
    pub clients: usize,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub pixel_format: String,
    pub socket_path: String,
    pub device_path: String,
    pub pipewire: bool,
    pub pipewire_streaming: bool,
}

impl Drop for VirtualCamera {
    fn drop(&mut self) {
        if let Some(mut r) = self
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            r.queue.close();
            if let Some(t) = r.thread.take() {
                let _ = t.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_drops_oldest() {
        let q = Queue::new(2);
        assert!(
            q.push(Job {
                codec: Codec::H264,
                keyframe: true,
                orientation: Orientation::UPRIGHT,
                data: vec![1]
            })
            .is_none()
        );
        assert!(
            q.push(Job {
                codec: Codec::H264,
                keyframe: false,
                orientation: Orientation::UPRIGHT,
                data: vec![2]
            })
            .is_none()
        );
        let evicted = q
            .push(Job {
                codec: Codec::H264,
                keyframe: false,
                orientation: Orientation::UPRIGHT,
                data: vec![3],
            })
            .unwrap();
        assert_eq!(evicted.data, vec![1]);
        assert_eq!(q.pop().unwrap().data, vec![2]);
        assert_eq!(q.pop().unwrap().data, vec![3]);
        q.close();
        assert!(q.pop().is_none());
    }

    #[test]
    fn pixel_format_names() {
        assert_eq!(parse_pixel_format("i420"), Some(ring::V4L2_PIX_FMT_YUV420));
        assert_eq!(parse_pixel_format("NV12"), Some(ring::V4L2_PIX_FMT_NV12));
        assert_eq!(parse_pixel_format(" yuyv "), Some(ring::V4L2_PIX_FMT_YUYV));
        assert_eq!(parse_pixel_format("MJPG"), Some(ring::V4L2_PIX_FMT_MJPEG));
        assert_eq!(parse_pixel_format("mjpeg"), Some(ring::V4L2_PIX_FMT_MJPEG));
        assert_eq!(parse_pixel_format("rgb"), None);
        let mjpeg = RingFormat::for_fourcc(ring::V4L2_PIX_FMT_MJPEG, 640, 480, 30, 1).unwrap();
        assert_eq!((mjpeg.bytesperline, mjpeg.sizeimage), (0, 640 * 480 * 2));
    }

    #[test]
    fn mjpeg_encoder_writes_a_jpeg_of_the_device_size() {
        let dev = DeviceFormat {
            width: 64,
            height: 32,
            fourcc: ring::V4L2_PIX_FMT_MJPEG,
        };
        let mut src = convert::I420Buffer::new(32, 32);
        src.data.fill(200);
        let mut enc = MjpegEncoder::new().unwrap();
        let mut normalizer = Normalizer::new();
        let mut slot = vec![0u8; 64 * 32 * 2];
        let n = enc.write_frame(&mut normalizer, &src.view(true), &dev, &mut slot);
        assert!(n > 0 && n <= slot.len());
        let hdr = turbojpeg::read_header(&slot[..n]).unwrap();
        assert_eq!((hdr.width, hdr.height), (64, 32));
    }
}
