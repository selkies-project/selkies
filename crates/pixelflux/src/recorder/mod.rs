/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Built-in MP4 recorder: an independent pipeline that captures the desktop to a
//! ready-to-play fragmented MP4 without needing any connected client.
//!
//! One implementation serves three control surfaces — the Python module functions
//! (`start_recording` / `stop_recording` / `recording_status`), the `PIXELFLUX_RECORD*`
//! environment variables (record from process start), and the `record_start` /
//! `record_stop` / `record_status` commands of [`crate::computer_use`] — so their
//! behavior cannot drift.
//!
//! The recorder is a consumer of the existing capture machinery, never a hook inside an
//! encoder:
//!
//! * **X11**: it always runs its own [`crate::x11::run_capture`] instance against the root
//!   window (a second capture of the same root is independent of any streaming session), with
//!   the encoded frames delivered straight into the recorder's queue.
//! * **Wayland**: the compositor lives in this process and allows one capture per output, so
//!   the recorder attaches at the delivery layer. When the output is already being captured
//!   for a streaming client it taps that stream (and paces recovery keyframes through the
//!   standard `RequestIdr` command); when the output is idle it starts its own capture with no
//!   Python callback and taps the identical delivery point.
//!
//! Frames cross into the recorder through one bounded queue, mirroring the Unix-socket sink's
//! isolation contract: the delivery thread only clones an `Arc` and `try_send`s, an overflowing
//! queue drops frames (never blocks the pipeline), and all muxing happens on the recorder's own
//! writer thread. Timestamps are wall-clock, so the damage-driven, variable-rate frame flow
//! lands at its true times in the MP4.
//!
//! Sound comes from an Ogg Opus stream on a Unix socket, the shape pcmflux serves, read by a
//! thread of its own: the `OpusHead` declares the track, and each packet's decode time is
//! its place under its page's granule position, laid onto the recording clock from the first
//! packet's arrival.

pub mod mp4;
pub mod ogg;

use std::collections::VecDeque;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crate::CaptureSettings;
use crate::ThreadCommand;
use crate::encoders::software::EncodedStripe;

/// Recorder queue bound, matching the socket sink's stalled-consumer policy: a writer that
/// falls this far behind loses frames instead of growing memory or blocking the encoder.
const QUEUE_CAP: usize = 256;

/// What crosses into the writer: a video frame as its `Arc`-shared encoded payload, the byte
/// offset where the Annex-B stream starts (past the wire header when present), and the
/// wall-clock capture time; or an audio packet with its decode time on the 48 kHz clock.
enum Tap {
    Video(Arc<Vec<u8>>, usize, u64),
    Audio(Vec<u8>, u64),
}

/// Audio packets held before the video's first IDR opens the file: ten seconds of 20 ms
/// packets, the oldest dropped past that.
const EARLY_AUDIO_CAP: usize = 500;

/// Which capture system feeds the recording.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PreferredBackend {
    X11,
    Wayland,
}

/// Resolved recording parameters. Environment variables provide the defaults; explicit
/// Python/REST arguments override them.
#[derive(Clone)]
pub struct RecordOptions {
    pub path: String,
    pub display_id: u32,
    /// Capture fps cap for a recorder-owned capture (`<= 0` = default 30). An attached
    /// recording follows the live session's rate.
    pub fps: f64,
    /// Bitrate override in kbps for a recorder-owned capture (`<= 0` = settings default).
    pub bitrate_kbps: i32,
    /// Recovery-keyframe cadence in seconds (`<= 0` = default 2.0). Recorder-owned captures
    /// carry it in their settings (scheduled IDRs); attached recordings pace standard
    /// request-IDR commands at this interval.
    pub keyframe_interval_s: f64,
    pub backend: Option<PreferredBackend>,
    /// Full capture settings for a recorder-owned capture; `None` derives them (full root /
    /// current output geometry, H.264 full-frame).
    pub capture: Option<CaptureSettings>,
    /// Unix socket serving an Ogg Opus stream to record as the audio track; empty for none.
    pub audio_socket: String,
}

impl RecordOptions {
    /// Options seeded entirely from `PIXELFLUX_RECORD_*` environment variables.
    pub fn from_env(path: String) -> Self {
        let f = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<f64>().ok());
        let backend = match std::env::var("PIXELFLUX_RECORD_BACKEND").ok().as_deref() {
            Some("x11") => Some(PreferredBackend::X11),
            Some("wayland") => Some(PreferredBackend::Wayland),
            _ => None,
        };
        Self {
            path,
            display_id: f("PIXELFLUX_RECORD_DISPLAY").map(|v| v as u32).unwrap_or(0),
            fps: f("PIXELFLUX_RECORD_FPS").unwrap_or(0.0),
            bitrate_kbps: f("PIXELFLUX_RECORD_BITRATE").map(|v| v as i32).unwrap_or(0),
            keyframe_interval_s: f("PIXELFLUX_RECORD_KEYFRAME_S").unwrap_or(0.0),
            backend,
            capture: None,
            audio_socket: std::env::var("PIXELFLUX_RECORD_AUDIO").unwrap_or_default(),
        }
    }

    fn effective_fps(&self) -> f64 {
        if self.fps > 0.0 { self.fps } else { 30.0 }
    }

    fn effective_keyframe_s(&self) -> f64 {
        if self.keyframe_interval_s > 0.0 {
            self.keyframe_interval_s
        } else {
            2.0
        }
    }
}

/// Live counters shared between the feeding side (delivery threads), the writer thread, and
/// the status surfaces.
struct RecShared {
    start: Instant,
    enqueued: AtomicU64,
    dropped: AtomicU64,
    skipped_non_h264: AtomicU64,
    muxed: AtomicU64,
    sync_frames: AtomicU64,
    audio_muxed: AtomicU64,
    bytes: AtomicU64,
    width: AtomicU32,
    height: AtomicU32,
    last_idr_req_us: AtomicU64,
    error: Mutex<Option<String>>,
}

impl RecShared {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            start: Instant::now(),
            enqueued: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            skipped_non_h264: AtomicU64::new(0),
            muxed: AtomicU64::new(0),
            sync_frames: AtomicU64::new(0),
            audio_muxed: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            last_idr_req_us: AtomicU64::new(0),
            error: Mutex::new(None),
        })
    }

    fn set_error(&self, msg: String) {
        let mut g = self.error.lock().unwrap();
        if g.is_none() {
            eprintln!("[recorder] {msg}");
            *g = Some(msg);
        }
    }
}

/// Point-in-time view of the recorder, identical across the Python, env, and REST surfaces.
#[derive(Clone, Debug)]
pub struct RecordingStatus {
    pub active: bool,
    pub path: String,
    pub backend: &'static str,
    pub mode: &'static str,
    pub frames: u64,
    pub sync_frames: u64,
    pub audio_frames: u64,
    pub dropped: u64,
    pub skipped_non_h264: u64,
    pub bytes: u64,
    pub duration_s: f64,
    pub width: u32,
    pub height: u32,
    pub error: Option<String>,
}

/// The wayland-side tap handle consulted by [`wayland_tap`]; cloneable so the delivery
/// thread can drop the registry lock before doing any work.
#[derive(Clone)]
struct WlTap {
    display_id: u32,
    tx: SyncSender<Tap>,
    shared: Arc<RecShared>,
    /// Standard request-IDR path for attached recordings (`None` when the recorder owns the
    /// capture and scheduled keyframes ride in its settings).
    idr: Option<IdrRequester>,
    keyframe_interval_us: u64,
}

/// Sends `ThreadCommand::RequestIdr` for one display over the compositor's command channel.
#[derive(Clone)]
struct IdrRequester {
    tx: smithay::reexports::calloop::channel::Sender<ThreadCommand>,
    display_id: u32,
}

impl IdrRequester {
    fn request(&self) {
        let _ = self.tx.send(ThreadCommand::RequestIdr {
            display_id: self.display_id,
        });
    }
}

/// How the active recording is fed and what must be torn down on stop.
enum RecordingMode {
    /// Recorder-owned X11 capture: its controls and capture thread.
    X11Own {
        controls: Arc<crate::x11::Controls>,
        join: thread::JoinHandle<()>,
    },
    /// Recorder-owned Wayland capture on `display_id` (stopped unless a streaming client has
    /// since taken ownership of the display).
    WaylandOwn { display_id: u32 },
    /// Tap on a streaming client's live Wayland capture; nothing to stop.
    WaylandAttached,
}

struct ActiveRecording {
    path: String,
    backend: &'static str,
    mode_name: &'static str,
    mode: RecordingMode,
    tx: SyncSender<Tap>,
    shared: Arc<RecShared>,
    writer: thread::JoinHandle<()>,
    audio_stop: Arc<AtomicBool>,
    audio: Option<thread::JoinHandle<()>>,
}

static ACTIVE: Mutex<Option<ActiveRecording>> = Mutex::new(None);
static LAST_FINISHED: Mutex<Option<RecordingStatus>> = Mutex::new(None);
/// Fast idle guard for [`wayland_tap`]: the delivery threads pay one relaxed atomic load
/// per frame while no recording is armed.
static WL_TAP_ARMED: AtomicBool = AtomicBool::new(false);
static WL_TAP: Mutex<Option<WlTap>> = Mutex::new(None);

/// Delivery-layer feed from the Wayland pipelines (readback encode loop and zero-copy
/// tick). Near-zero cost when idle; while recording it clones an `Arc` into the bounded
/// queue and never blocks.
pub(crate) fn wayland_tap(display_id: u32, stripes: &[EncodedStripe]) {
    if !WL_TAP_ARMED.load(Ordering::Relaxed) {
        return;
    }
    let Some(tap) = WL_TAP.lock().unwrap().clone() else {
        return;
    };
    if tap.display_id != display_id {
        return;
    }
    offer_frame(&tap.shared, &tap.tx, stripes);
    if let Some(ref idr) = tap.idr {
        pace_idr_requests(&tap.shared, idr, tap.keyframe_interval_us);
    }
}

/// Issue a standard request-IDR when the cadence interval has elapsed. Compare-and-swap on
/// the last-request stamp so concurrent delivery threads emit one request per interval.
fn pace_idr_requests(shared: &RecShared, idr: &IdrRequester, interval_us: u64) {
    if interval_us == 0 {
        return;
    }
    let now = shared.start.elapsed().as_micros() as u64;
    let last = shared.last_idr_req_us.load(Ordering::Relaxed);
    if now.saturating_sub(last) >= interval_us
        && shared
            .last_idr_req_us
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        idr.request();
    }
}

/// Enqueue one delivered frame for muxing. Only H.264 is recordable; JPEG or another codec is
/// counted and skipped so the surfaces can report a
/// clear "nothing recordable" error instead of writing a corrupt file.
fn offer_frame(shared: &RecShared, tx: &SyncSender<Tap>, stripes: &[EncodedStripe]) {
    if stripes.is_empty() {
        return;
    }
    if stripes.len() != 1
        || stripes[0].codec != crate::encoders::Codec::H264
        || stripes[0].stripe_y_start != 0
    {
        shared.skipped_non_h264.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let s = &stripes[0];
    if s.data.is_empty() {
        return;
    }
    let offset = if s.data.len() >= 10 && s.data[0] == 0x04 {
        10
    } else {
        0
    };
    if s.data.len() == offset {
        return;
    }
    let pts_us = shared.start.elapsed().as_micros() as u64;
    match tx.try_send(Tap::Video(s.data.clone(), offset, pts_us)) {
        Ok(()) => {
            shared.enqueued.fetch_add(1, Ordering::Relaxed);
        }
        Err(TrySendError::Full(_)) => {
            shared.dropped.fetch_add(1, Ordering::Relaxed);
        }
        Err(TrySendError::Disconnected(_)) => {}
    }
}

/// Writer-thread body: drain the queue, convert Annex-B to AVCC, and mux. Output starts at
/// the first IDR with parameter sets; earlier frames are discarded and audio packets held.
/// Runs until every sender is dropped (stop) or a write error occurs, then finalizes the
/// file and publishes the final status.
fn writer_thread(
    rx: Receiver<Tap>,
    file: std::fs::File,
    path: String,
    backend: &'static str,
    mode_name: &'static str,
    audio: Option<mp4::AudioTrackConfig>,
    shared: Arc<RecShared>,
) {
    let mut writer = mp4::FragmentWriter::new(std::io::BufWriter::new(file));
    if let Some(cfg) = audio {
        writer = writer.with_audio(cfg);
    }
    let mut builder = mp4::H264SampleBuilder::new();
    let mut early_audio: VecDeque<(Vec<u8>, u64)> = VecDeque::new();

    'recv: for item in rx.iter() {
        let (buf, offset, pts_us) = match item {
            Tap::Video(buf, offset, pts_us) => (buf, offset, pts_us),
            Tap::Audio(packet, dts) => {
                if !writer.init_written() {
                    if early_audio.len() >= EARLY_AUDIO_CAP {
                        early_audio.pop_front();
                    }
                    early_audio.push_back((packet, dts));
                } else if let Err(e) = writer.push_audio(packet, dts) {
                    shared.set_error(format!("MP4 write failed: {e}"));
                    break 'recv;
                }
                shared
                    .audio_muxed
                    .store(writer.stats().audio_samples, Ordering::Relaxed);
                continue;
            }
        };
        let Some(sample) = builder.build_sample(&buf[offset..]) else {
            continue;
        };
        if !writer.init_written() {
            if !sample.sync || !builder.have_parameter_sets() {
                continue;
            }
            let Some(cfg) = builder.track_config() else {
                shared.set_error("failed to parse H.264 SPS for MP4 init".to_string());
                break 'recv;
            };
            shared.width.store(cfg.width, Ordering::Relaxed);
            shared.height.store(cfg.height, Ordering::Relaxed);
            if let Err(e) = writer.write_init(&cfg) {
                shared.set_error(format!("MP4 init write failed: {e}"));
                break 'recv;
            }
            for (packet, dts) in early_audio.drain(..) {
                if let Err(e) = writer.push_audio(packet, dts) {
                    shared.set_error(format!("MP4 write failed: {e}"));
                    break 'recv;
                }
            }
        }
        if let Err(e) = writer.push_sample(sample.data, sample.sync, pts_us) {
            shared.set_error(format!("MP4 write failed: {e}"));
            break 'recv;
        }
        let st = writer.stats();
        // The writer always holds one sample back to learn its duration, so it counts as muxed.
        shared.muxed.store(st.samples + 1, Ordering::Relaxed);
        shared.sync_frames.store(st.sync_samples, Ordering::Relaxed);
        shared.bytes.store(st.bytes, Ordering::Relaxed);
    }

    let final_stats = match writer.finish() {
        Ok(st) => st,
        Err(e) => {
            shared.set_error(format!("MP4 finalize failed: {e}"));
            mp4::Mp4Stats::default()
        }
    };
    if final_stats.samples == 0 {
        shared.set_error(
            "no recordable H.264 frames received (the session must produce a single \
             full-frame H.264 stream; JPEG and the other codecs cannot be recorded)"
                .to_string(),
        );
        let _ = std::fs::remove_file(&path);
    }
    let status = RecordingStatus {
        active: false,
        path,
        backend,
        mode: mode_name,
        frames: final_stats.samples,
        sync_frames: final_stats.sync_samples,
        audio_frames: final_stats.audio_samples,
        dropped: shared.dropped.load(Ordering::Relaxed),
        skipped_non_h264: shared.skipped_non_h264.load(Ordering::Relaxed),
        bytes: final_stats.bytes,
        duration_s: final_stats.duration_us as f64 / 1e6,
        width: shared.width.load(Ordering::Relaxed),
        height: shared.height.load(Ordering::Relaxed),
        error: shared.error.lock().unwrap().clone(),
    };
    println!(
        "[recorder] finished {}: {} frames ({} sync), {} audio packets, {:.2}s, {} bytes",
        status.path,
        status.frames,
        status.sync_frames,
        status.audio_frames,
        status.duration_s,
        status.bytes
    );
    *LAST_FINISHED.lock().unwrap() = Some(status);
}

/// Connect to the audio sink within a moment and read its headers: the reader positioned
/// at the first audio packet, and the `OpusHead` the track is declared from.
fn open_audio(path: &str) -> Result<(ogg::PageReader<UnixStream>, ogg::OpusHead), String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let stream = loop {
        match UnixStream::connect(path) {
            Ok(s) => break s,
            Err(e) if Instant::now() < deadline => {
                let _ = e;
                thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(format!("audio socket {path}: {e}")),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    let mut reader = ogg::PageReader::new(stream);
    let (first, _) = reader
        .next_packet()
        .map_err(|e| format!("audio socket {path}: {e}"))?
        .ok_or_else(|| format!("audio socket {path} closed before its headers"))?;
    let head = ogg::parse_opus_head(&first)
        .ok_or_else(|| format!("audio socket {path} does not start with OpusHead"))?;
    reader
        .next_packet()
        .map_err(|e| format!("audio socket {path}: {e}"))?;
    Ok((reader, head))
}

/// Where a page's first packet starts, in samples: its granule position counts to the end of
/// its last packet, so the page starts what its packets hold (`samples`) before that. A
/// producer that sends nothing through a silence (pcmflux's silence gate) leaves the gap
/// before the page, which then lands after it rather than against the page before; a final
/// page whose granule trims its end starts where the one before ended.
fn page_start(position: u64, granule: u64, samples: u64) -> u64 {
    granule.saturating_sub(samples).max(position)
}

/// Audio-thread body: dates each page's packets from `page_start`, laid onto the recording
/// clock from the first packet's arrival. Packets wait for their page's last, which carries
/// the granule; an unfinished page's packets at the stream's end follow the page before. Ends
/// with the stream, or on stop.
fn audio_thread(
    mut reader: ogg::PageReader<UnixStream>,
    tx: SyncSender<Tap>,
    shared: Arc<RecShared>,
    stop: Arc<AtomicBool>,
) {
    let _ = reader
        .inner_mut()
        .set_read_timeout(Some(Duration::from_millis(200)));
    let mut position: u64 = 0;
    let mut anchor: Option<(u64, u64)> = None;
    let mut page: Vec<(Vec<u8>, u64)> = Vec::new();
    let mut open = true;
    while open {
        let granule = if stop.load(Ordering::Relaxed) {
            None
        } else {
            match reader.next_packet() {
                Ok(Some((packet, granule))) => {
                    let samples = mp4::opus_packet_samples(&packet) as u64;
                    page.push((packet, samples));
                    match granule {
                        Some(granule) => Some(granule),
                        None => continue,
                    }
                }
                Ok(None) => None,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }
                Err(e) => {
                    eprintln!("[recorder] audio stream ended: {e}");
                    None
                }
            }
        };
        open = granule.is_some();
        let samples: u64 = page.iter().map(|p| p.1).sum();
        position = granule.map_or(position, |g| page_start(position, g, samples));
        let (clock, base) = *anchor.get_or_insert_with(|| {
            (
                shared.start.elapsed().as_micros() as u64 * (mp4::OPUS_TIMESCALE as u64)
                    / 1_000_000,
                position,
            )
        });
        for (packet, samples) in page.drain(..) {
            let dts = clock + position.saturating_sub(base);
            position += samples;
            match tx.try_send(Tap::Audio(packet, dts)) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    shared.dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
    }
}

/// Capture settings for a recorder-owned capture: explicit settings when given (validated
/// H.264), otherwise derived defaults, always forced to the one recordable shape (full-frame
/// H.264, no socket rebind, scheduled recovery keyframes).
fn own_capture_settings(opts: &RecordOptions) -> Result<CaptureSettings, String> {
    let mut s = match &opts.capture {
        Some(explicit) => {
            if explicit.codec != crate::encoders::Codec::H264 {
                return Err("recording requires H.264 capture settings (codec='h264')".to_string());
            }
            explicit.clone()
        }
        None => CaptureSettings {
            width: 0,
            height: 0,
            codec: crate::encoders::Codec::H264,
            capture_cursor: true,
            target_fps: opts.effective_fps(),
            ..Default::default()
        },
    };
    s.recording_socket = String::new();
    if opts.fps > 0.0 {
        s.target_fps = opts.fps;
    }
    if opts.bitrate_kbps > 0 {
        s.video_bitrate_kbps = opts.bitrate_kbps;
    }
    if s.keyframe_interval_s <= 0.0 {
        s.keyframe_interval_s = opts.effective_keyframe_s();
    }
    Ok(s)
}

/// Start a recording. Exactly one may be active per process; returns the initial status.
pub fn start(opts: RecordOptions) -> Result<RecordingStatus, String> {
    let mut guard = ACTIVE.lock().unwrap();
    if guard.is_some() {
        return Err("a recording is already active".to_string());
    }
    if opts.path.is_empty() {
        return Err("recording path is empty".to_string());
    }
    if let Some(cap) = &opts.capture
        && cap.codec != crate::encoders::Codec::H264
    {
        return Err("recording requires H.264 capture settings (codec='h264')".to_string());
    }

    let wl_tx = crate::computer_use::wayland_command_sender();
    let backend = match opts.backend {
        Some(PreferredBackend::X11) => PreferredBackend::X11,
        Some(PreferredBackend::Wayland) => {
            if wl_tx.is_none() {
                return Err("no Wayland compositor is running in this process".to_string());
            }
            PreferredBackend::Wayland
        }
        None => {
            if wl_tx.is_some() {
                PreferredBackend::Wayland
            } else if std::env::var("DISPLAY")
                .map(|v| !v.is_empty())
                .unwrap_or(false)
            {
                PreferredBackend::X11
            } else {
                return Err(
                    "no capture backend available: no in-process Wayland compositor and DISPLAY is unset"
                        .to_string(),
                );
            }
        }
    };

    let file = open_recording(&opts.path)?;
    let shared = RecShared::new();
    let (tx, rx) = sync_channel::<Tap>(QUEUE_CAP);
    let audio_source = if opts.audio_socket.is_empty() {
        None
    } else {
        match open_audio(&opts.audio_socket) {
            Ok(source) => Some(source),
            Err(e) => {
                eprintln!("[recorder] recording without sound: {e}");
                None
            }
        }
    };

    let (mode, mode_name, backend_name) = match backend {
        PreferredBackend::Wayland => {
            let cmd_tx = wl_tx.unwrap();
            let display_id = opts.display_id;
            let attached = crate::wayland_alive().lock().unwrap().contains(&display_id);
            if attached {
                *WL_TAP.lock().unwrap() = Some(WlTap {
                    display_id,
                    tx: tx.clone(),
                    shared: shared.clone(),
                    idr: Some(IdrRequester {
                        tx: cmd_tx.clone(),
                        display_id,
                    }),
                    keyframe_interval_us: (opts.effective_keyframe_s() * 1e6) as u64,
                });
                WL_TAP_ARMED.store(true, Ordering::Relaxed);
                // The stream is mid-GOP: a standard request-IDR gives the file its first
                // decodable frame promptly.
                let _ = cmd_tx.send(ThreadCommand::RequestIdr { display_id });
                (RecordingMode::WaylandAttached, "attached", "wayland")
            } else {
                let mut settings = own_capture_settings(&opts)?;
                if settings.width <= 0 || settings.height <= 0 {
                    let (reply_tx, reply_rx) = mpsc::channel();
                    cmd_tx
                        .send(ThreadCommand::ListOutputs { reply: reply_tx })
                        .map_err(|_| "wayland compositor is not accepting commands".to_string())?;
                    let outputs = reply_rx
                        .recv_timeout(Duration::from_secs(5))
                        .map_err(|_| "wayland compositor did not report its outputs".to_string())?;
                    let out = outputs
                        .iter()
                        .find(|o| o.0 == display_id)
                        .ok_or_else(|| format!("no wayland output with display id {display_id}"))?;
                    settings.width = out.3;
                    settings.height = out.4;
                    settings.scale = out.5;
                }
                *WL_TAP.lock().unwrap() = Some(WlTap {
                    display_id,
                    tx: tx.clone(),
                    shared: shared.clone(),
                    idr: None,
                    keyframe_interval_us: 0,
                });
                WL_TAP_ARMED.store(true, Ordering::Relaxed);
                cmd_tx
                    .send(ThreadCommand::StartCapture {
                        display_id,
                        callback: None,
                        settings,
                    })
                    .map_err(|_| {
                        disarm_tap();
                        "wayland compositor is not accepting commands".to_string()
                    })?;
                // StartCapture has no reply; liveness shows up in the alive set.
                let deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    if crate::wayland_alive().lock().unwrap().contains(&display_id) {
                        break;
                    }
                    if Instant::now() >= deadline {
                        disarm_tap();
                        return Err(format!(
                            "wayland capture on display {display_id} failed to start"
                        ));
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                (
                    RecordingMode::WaylandOwn { display_id },
                    "own-capture",
                    "wayland",
                )
            }
        }
        PreferredBackend::X11 => {
            let settings = own_capture_settings(&opts)?;
            let controls = Arc::new(crate::x11::Controls::new(&settings));
            crate::live_x11().lock().unwrap().push(controls.clone());
            let err_slot: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
            let err_slot2 = err_slot.clone();
            let (etid_tx, etid_rx) = mpsc::channel();
            let cap_controls = controls.clone();
            let feed_tx = tx.clone();
            let feed_shared = shared.clone();
            let join = thread::spawn(move || {
                let on_frame = move |stripes: Vec<EncodedStripe>| {
                    offer_frame(&feed_shared, &feed_tx, &stripes);
                };
                if let Err(e) = crate::x11::run_capture(settings, cap_controls, etid_tx, on_frame) {
                    eprintln!("[recorder] x11 capture error: {e}");
                    *err_slot2.lock().unwrap() = Some(e);
                }
            });
            match etid_rx.recv_timeout(Duration::from_secs(3)) {
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = join.join();
                    crate::live_x11()
                        .lock()
                        .unwrap()
                        .retain(|c| !Arc::ptr_eq(c, &controls));
                    let msg = err_slot
                        .lock()
                        .unwrap()
                        .clone()
                        .unwrap_or_else(|| "X11 capture exited during start".to_string());
                    return Err(msg);
                }
                // Slow X server setup: the capture is still coming up; proceed.
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            (
                RecordingMode::X11Own { controls, join },
                "own-capture",
                "x11",
            )
        }
    };

    let audio_stop = Arc::new(AtomicBool::new(false));
    let (audio_cfg, audio) = match audio_source {
        Some((reader, head)) => {
            let cfg = mp4::AudioTrackConfig {
                sample_entry: mp4::opus_sample_entry(&head),
            };
            let (tx, shared, stop) = (tx.clone(), shared.clone(), audio_stop.clone());
            let join = thread::Builder::new()
                .name("pf-recorder-audio".to_string())
                .spawn(move || audio_thread(reader, tx, shared, stop))
                .map_err(|e| format!("failed to spawn recorder audio thread: {e}"))?;
            (Some(cfg), Some(join))
        }
        None => (None, None),
    };
    let writer = {
        let shared = shared.clone();
        let path = opts.path.clone();
        thread::Builder::new()
            .name("pf-recorder".to_string())
            .spawn(move || {
                writer_thread(rx, file, path, backend_name, mode_name, audio_cfg, shared)
            })
            .map_err(|e| format!("failed to spawn recorder writer thread: {e}"))?
    };

    println!(
        "[recorder] recording to {} ({} {})",
        opts.path, backend_name, mode_name
    );
    let status = RecordingStatus {
        active: true,
        path: opts.path.clone(),
        backend: backend_name,
        mode: mode_name,
        frames: 0,
        sync_frames: 0,
        audio_frames: 0,
        dropped: 0,
        skipped_non_h264: 0,
        bytes: 0,
        duration_s: 0.0,
        width: 0,
        height: 0,
        error: None,
    };
    *guard = Some(ActiveRecording {
        path: opts.path,
        backend: backend_name,
        mode_name,
        mode,
        tx,
        shared,
        writer,
        audio_stop,
        audio,
    });
    Ok(status)
}

fn disarm_tap() {
    WL_TAP_ARMED.store(false, Ordering::Relaxed);
    *WL_TAP.lock().unwrap() = None;
}

/// Stop the active recording, finalize the MP4, and return the final status. Errors when no
/// recording is active or when nothing recordable was ever received.
pub fn stop() -> Result<RecordingStatus, String> {
    let active = ACTIVE
        .lock()
        .unwrap()
        .take()
        .ok_or_else(|| "no recording is active".to_string())?;
    disarm_tap();
    match active.mode {
        RecordingMode::X11Own { controls, join } => {
            controls.stop.store(true, Ordering::Relaxed);
            let _ = join.join();
            crate::live_x11()
                .lock()
                .unwrap()
                .retain(|c| !Arc::ptr_eq(c, &controls));
        }
        RecordingMode::WaylandOwn { display_id } => {
            // A streaming client that reconfigured this display now owns it; the capture
            // must survive the recorder's exit.
            let client_owns = crate::wayland_owners()
                .lock()
                .unwrap()
                .contains_key(&display_id);
            if !client_owns && let Some(tx) = crate::computer_use::wayland_command_sender() {
                let _ = tx.send(ThreadCommand::StopCapture { display_id });
                // The next start classifies attached-vs-own against wayland_alive:
                // wait for the compositor to drain the stop, or an immediate restart
                // attaches to the dying capture and records nothing.
                let (ack_tx, ack_rx) = mpsc::channel();
                if tx.send(ThreadCommand::Barrier { reply: ack_tx }).is_ok() {
                    let _ = ack_rx.recv_timeout(Duration::from_secs(2));
                }
            }
        }
        RecordingMode::WaylandAttached => {}
    }
    // The audio thread holds a sender of its own: it goes first, so the writer's queue
    // closes once the last packet is in.
    active.audio_stop.store(true, Ordering::Relaxed);
    if let Some(join) = active.audio {
        let _ = join.join();
    }
    drop(active.tx);
    let _ = active.writer.join();
    let finished = LAST_FINISHED
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| format!("recording of {} produced no final status", active.path))?;
    match finished.error {
        Some(ref e) => Err(e.clone()),
        None => Ok(finished),
    }
}

/// The current status: the live recording when one is active, otherwise the last finished
/// one; `None` when the process has never recorded.
pub fn status() -> Option<RecordingStatus> {
    let guard = ACTIVE.lock().unwrap();
    if let Some(a) = guard.as_ref() {
        return Some(RecordingStatus {
            active: true,
            path: a.path.clone(),
            backend: a.backend,
            mode: a.mode_name,
            frames: a.shared.muxed.load(Ordering::Relaxed),
            sync_frames: a.shared.sync_frames.load(Ordering::Relaxed),
            audio_frames: a.shared.audio_muxed.load(Ordering::Relaxed),
            dropped: a.shared.dropped.load(Ordering::Relaxed),
            skipped_non_h264: a.shared.skipped_non_h264.load(Ordering::Relaxed),
            bytes: a.shared.bytes.load(Ordering::Relaxed),
            duration_s: a.shared.start.elapsed().as_secs_f64(),
            width: a.shared.width.load(Ordering::Relaxed),
            height: a.shared.height.load(Ordering::Relaxed),
            error: a.shared.error.lock().unwrap().clone(),
        });
    }
    drop(guard);
    LAST_FINISHED.lock().unwrap().clone()
}

/// Finalize any active recording at interpreter shutdown so the MP4's last buffered sample
/// is flushed. Best-effort; the fragmented layout keeps even an unflushed file playable.
pub fn finalize_on_exit() {
    if ACTIVE.lock().unwrap().is_some() {
        let _ = stop();
    }
}

/// `PIXELFLUX_RECORD=<path>`: record from process start. X11 (via `DISPLAY`) starts
/// immediately; otherwise a background thread waits for the in-process Wayland compositor to
/// come up and then starts. Retries transient failures until the deadline.
pub fn autostart_from_env() {
    use std::sync::OnceLock;
    static STARTED: OnceLock<()> = OnceLock::new();
    let Ok(path) = std::env::var("PIXELFLUX_RECORD") else {
        return;
    };
    if path.is_empty() {
        return;
    }
    let mut first = false;
    STARTED.get_or_init(|| first = true);
    if !first {
        return;
    }
    thread::spawn(move || {
        let opts = RecordOptions::from_env(path);
        let deadline = Instant::now() + Duration::from_secs(300);
        let mut last_err = String::new();
        loop {
            let wayland_up = crate::computer_use::wayland_command_sender().is_some();
            let x11_up = std::env::var("DISPLAY")
                .map(|v| !v.is_empty())
                .unwrap_or(false);
            let ready = match opts.backend {
                Some(PreferredBackend::X11) => x11_up,
                Some(PreferredBackend::Wayland) => wayland_up,
                None => wayland_up || x11_up,
            };
            if ready {
                match start(opts.clone()) {
                    Ok(_) => return,
                    Err(e) => last_err = e,
                }
            }
            if Instant::now() >= deadline {
                eprintln!(
                    "[recorder] PIXELFLUX_RECORD autostart gave up: {}",
                    if last_err.is_empty() {
                        "no capture backend appeared"
                    } else {
                        &last_err
                    }
                );
                return;
            }
            thread::sleep(Duration::from_millis(250));
        }
    });
}

/// Serialize a status into the JSON shape shared by the REST endpoints.
pub fn status_to_json(s: &RecordingStatus) -> serde_json::Value {
    serde_json::json!({
        "active": s.active,
        "path": s.path,
        "backend": s.backend,
        "mode": s.mode,
        "frames": s.frames,
        "sync_frames": s.sync_frames,
        "audio_frames": s.audio_frames,
        "dropped": s.dropped,
        "skipped_non_h264": s.skipped_non_h264,
        "bytes": s.bytes,
        "duration_s": s.duration_s,
        "width": s.width,
        "height": s.height,
        "error": s.error,
    })
}

/// Open the recording's file. The recording is the desktop: readable by its owner alone,
/// never written through a link someone else may have planted at a predictable name in a
/// shared directory, nor into a file there that is another's, or has another name too.
fn open_recording(path: &str) -> Result<std::fs::File, String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| format!("cannot create {path}: {e}"))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("cannot inspect {path}: {e}"))?;
    if meta.uid() != unsafe { libc::geteuid() } || meta.nlink() != 1 {
        return Err(format!("{path} is not this user's file alone"));
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("cannot restrict {path}: {e}"))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The recording opens only into a file of this user's alone: a new one, or one of its
    /// own made private, never through a link nor into a file that has another name too.
    #[test]
    fn a_recording_opens_only_into_a_file_of_this_users_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("pf-record-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let p = |name: &str| dir.join(name).to_string_lossy().into_owned();
        let mode = |name: &str| {
            std::fs::metadata(dir.join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };

        assert!(open_recording(&p("new.mp4")).is_ok());
        assert_eq!(mode("new.mp4"), 0o600);
        std::fs::write(dir.join("own.mp4"), b"old").unwrap();
        std::fs::set_permissions(dir.join("own.mp4"), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        assert!(open_recording(&p("own.mp4")).is_ok());
        assert_eq!(mode("own.mp4"), 0o600);
        std::os::unix::fs::symlink(dir.join("own.mp4"), dir.join("link.mp4")).unwrap();
        assert!(open_recording(&p("link.mp4")).is_err());
        std::fs::hard_link(dir.join("own.mp4"), dir.join("second-name.mp4")).unwrap();
        assert!(open_recording(&p("second-name.mp4")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn ogg_page(granule: u64, packets: &[&[u8]]) -> Vec<u8> {
        let mut out = b"OggS\0\0".to_vec();
        out.extend_from_slice(&granule.to_le_bytes());
        out.extend_from_slice(&[0u8; 12]);
        out.push(packets.len() as u8);
        out.extend(packets.iter().map(|p| p.len() as u8));
        packets.iter().for_each(|p| out.extend_from_slice(p));
        out
    }

    /// Each packet keeps its place under its page's granule: contiguous pages follow each other,
    /// the page after a gap a gated producer left sits after that gap, the packets of one page
    /// follow one another, and a final page whose granule trims its end follows the one before.
    #[test]
    fn audio_packets_keep_their_place_across_a_gap() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        let packet: &[u8] = &[0xf8, 0, 0];
        for (granule, packets) in [(960, 1), (1_920, 1), (50_880, 1), (52_800, 2), (53_000, 1)] {
            writer
                .write_all(&ogg_page(granule, &vec![packet; packets]))
                .unwrap();
        }
        drop(writer);
        let (tx, rx) = sync_channel::<Tap>(16);
        audio_thread(
            ogg::PageReader::new(reader),
            tx,
            RecShared::new(),
            Arc::new(AtomicBool::new(false)),
        );
        let dts: Vec<u64> = rx
            .try_iter()
            .map(|t| match t {
                Tap::Audio(_, dts) => dts,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            dts.iter().map(|d| d - dts[0]).collect::<Vec<_>>(),
            [0, 960, 49_920, 50_880, 51_840, 52_800]
        );
    }
}
