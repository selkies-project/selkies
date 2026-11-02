/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What a controller's page is told about its stream, the same on both transports, and the
//! host figures behind it.
//!
//! Two messages carry it. `stream_info` describes the path a display's capture took: the
//! capture backend and whether it is zero-copy, the encoder and whether it is hardware, the
//! GPU behind it, and the reason a faster path was declined, as pixelflux reports it
//! ([`pixelflux::ScreenCapture::stream_info`]), plus `gpu_present`, whether the server is
//! exposed a GPU at all, and `hardware_expected`, whether the session asked for hardware on a
//! server that has one: a software session somebody chose, and one on a host with no GPU,
//! which is an ordinary deployment, are told apart from one that fell back. It is sent once
//! when the capture settles and again when it changes, since a session can demote itself
//! mid-stream ([`StreamWatch::follow`]).
//!
//! `stream_stats` carries the numbers that move: the host's CPU, memory, and GPU
//! ([`host_stats`], from a [`ResourceMonitor`]), the rate and cost of the encode, how long a
//! frame took from its capture to the wire (`send_ms`, the delivery the encode figures cannot
//! see: the relay, the pacer), how long from the end of its encode to the wire as a
//! distribution (`wire_median_ms` and `wire_p99_ms`, from a [`SendTimer`]'s per-frame
//! record), the rate the video was sent at and its peak against the CBR target, and over
//! WebSockets the round trip and whether the server is holding frames back (a WebRTC page
//! measures its own link). It flows only to a connection that asked for it (the `_stats,1`
//! verb a page sends while its stats are on screen, `_stats,0` when they leave,
//! [`stats_request`]), so a session nobody inspects sends nothing periodic. A shared viewer is
//! sent neither message.
//!
//! The transport owns the envelope (`{"type": "stream_info", "displayId": ..., "info": ...}`
//! and `{"type": "stream_stats", "displayId": ..., "stats": ...}`); this module builds the
//! `info` and `stats` objects. A capture that reports nothing yet leaves `stream_info` unsent
//! and the encode figures out of `stream_stats`; nothing else depends on them.
//!
//! The submodules hold the host readings ([`resources`]: GPU utilization across vendors, the
//! session's CPU and memory, and the sampler both transports run), the Prometheus registry
//! the `/api/metrics` route renders ([`metrics`]), and the `selkies gpu-probe` command
//! ([`gpu_probe`]).

#[cfg(feature = "frame-log")]
pub mod frame_log;
pub mod gpu_probe;
mod gpu_probe_helpers;
pub mod metrics;
pub mod resources;
mod send_timer;

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use log::debug;
use pixelflux::{ScreenCapture, StreamInfo, StreamStats};
use serde_json::{Map, Value as Json, json};

use crate::core::DisplayId;
use crate::settings::{CodecBackends, canonical_encoder, codec_for_encoder, software_video_path};

pub use resources::ResourceMonitor;
pub use send_timer::{SendPercentiles, SendTimer};

/// The verb a page sends to start (`_stats,1`) or stop (`_stats,0`) the periodic figures.
pub const STATS_VERB: &str = "_stats";
/// How long after a capture start its description is first read.
pub const SETTLE_S: f64 = 1.0;
/// How often the description is read after it settled.
pub const WATCH_S: f64 = 3.0;
/// Two counter reads further apart than this span a time nobody watched, not a rate.
pub const RATE_WINDOW_MAX_S: f64 = 5.0;
/// Two closer than this hold too few frames to be one: a rate needs frames to count.
pub const RATE_WINDOW_MIN_S: f64 = 0.25;
/// The bytes sent are counted in buckets this long, and the stream's peak rate is the
/// fullest of them over the last `PEAK_WINDOW_S`: a key frame's burst, which a second's
/// average spreads out of sight.
pub const PEAK_BUCKET_NS: i64 = 250_000_000;
pub const PEAK_WINDOW_S: f64 = 10.0;

/// The monotonic clock in nanoseconds, the one pixelflux stamps its frames with
/// (`CLOCK_MONOTONIC`), so a frame's capture time and the moment it reaches the wire are
/// on one axis.
pub fn now_ns() -> i64 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `t` is a valid, writable timespec and CLOCK_MONOTONIC is always available.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) };
    t.tv_sec * 1_000_000_000 + t.tv_nsec
}

/// Round to `digits` decimals the way the figures are published.
fn round_to(value: f64, digits: i32) -> f64 {
    let scale = 10f64.powi(digits);
    (value * scale).round() / scale
}

/// The subscription a `_stats,<0|1>` message asks for, or `None` for any other message.
pub fn stats_request(message: &str) -> Option<bool> {
    let (verb, value) = match message.find(',') {
        Some(at) => (&message[..at], &message[at + 1..]),
        None => (message, ""),
    };
    (verb == STATS_VERB).then(|| value.trim() == "1")
}

/// Whether a GPU is exposed to this host or container: a DRM render node, or the NVIDIA
/// control device of a container given the card without its graphics stack, where NVENC
/// still encodes. A GPU on the machine that the container was not given is, from in here,
/// no GPU.
pub fn gpu_present() -> bool {
    let render_node = std::fs::read_dir("/dev/dri")
        .map(|entries| {
            entries
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("renderD"))
        })
        .unwrap_or(false);
    render_node || std::path::Path::new("/dev/nvidiactl").exists()
}

/// Whether a session with this encoder should be encoding on a GPU: there is one, it has an
/// engine for the codec, and the session asked for it, which JPEG and the striped encoder
/// never do and any other does unless software encoding is on.
///
/// `backends` is the startup probe's `Settings::encoder_backends`. A codec the probe found
/// no hardware backend for is encoded in software by the codec's choice, as a server with no
/// GPU is; `None`, a probe that has not run, leaves the engine assumed.
pub fn hardware_expected(
    encoder: &str,
    use_cpu: bool,
    gpu: bool,
    backends: Option<&HashMap<&'static str, CodecBackends>>,
) -> bool {
    if !gpu || canonical_encoder(encoder) == "jpeg" || software_video_path(encoder, use_cpu) {
        return false;
    }
    match backends {
        None => true,
        Some(table) => table
            .get(codec_for_encoder(encoder))
            .is_some_and(|codec| codec.hardware.is_some()),
    }
}

/// The `stream_info` object of a capture's report, keyed as the pages read it: pixelflux's
/// description, `encode_node` as the device path of a hardware session (empty otherwise),
/// `striped` for more than one stripe, and a Wayland capture's renderer beside it.
pub fn stream_info_message(report: &StreamInfo) -> Map<String, Json> {
    let info = &report.info;
    let mut map = Map::new();
    map.insert("backend".into(), json!(info.backend));
    map.insert("capture".into(), json!(info.capture));
    map.insert("zero_copy".into(), json!(info.zero_copy));
    map.insert(
        "zero_copy_available".into(),
        json!(info.zero_copy_available),
    );
    map.insert("capture_reason".into(), json!(info.capture_reason));
    map.insert("encoder".into(), json!(info.encoder));
    map.insert("hardware".into(), json!(info.hardware));
    map.insert("encoder_reason".into(), json!(info.encoder_reason));
    map.insert("codec".into(), json!(info.codec));
    map.insert("fullcolor".into(), json!(info.fullcolor));
    map.insert("bit_depth".into(), json!(info.bit_depth));
    map.insert("full_range".into(), json!(info.full_range));
    map.insert("striped".into(), json!(info.stripes > 1));
    map.insert("gpu".into(), json!(info.gpu));
    map.insert("driver".into(), json!(info.driver));
    let encode_node = if info.hardware {
        format!("/dev/dri/renderD{}", 128 + info.encode_node)
    } else {
        String::new()
    };
    map.insert("encode_node".into(), json!(encode_node));
    if let Some(renderer) = &report.renderer {
        map.insert("renderer".into(), json!(renderer.kind));
        map.insert("render_node".into(), json!(renderer.node));
        map.insert("render_gpu".into(), json!(renderer.gpu));
        map.insert("renderer_reason".into(), json!(renderer.reason));
    }
    map
}

/// The host half of a `stream_stats` object from a [`ResourceMonitor`]'s last sample:
/// `cpu_percent`, `mem_total`, `mem_used`, and where a GPU is read `gpu_percent`,
/// `gpu_mem_total`, `gpu_mem_used`. Empty while nothing was sampled.
pub fn host_stats(monitor: &ResourceMonitor) -> Map<String, Json> {
    let mut stats = Map::new();
    if let Some(system) = monitor.system() {
        stats.insert("cpu_percent".into(), json!(system.cpu_percent));
        stats.insert("mem_total".into(), json!(system.mem_total));
        stats.insert("mem_used".into(), json!(system.mem_used));
    }
    if let Some(gpu) = monitor.gpu() {
        stats.insert("gpu_percent".into(), json!(gpu.gpu_percent));
        stats.insert("gpu_mem_total".into(), json!(gpu.memory_total));
        stats.insert("gpu_mem_used".into(), json!(gpu.memory_used));
    }
    stats
}

/// A capture a [`StreamWatch`] follows: pixelflux's own handle, or the transport's wrapper
/// around one.
pub trait StreamSource: Send + Sync {
    /// What the capture streams and how, `None` before a start and after a stop.
    fn stream_info(&self) -> Option<StreamInfo>;
    /// The capture's counters, `None` without one.
    fn stream_stats(&self) -> Option<StreamStats>;
}

impl StreamSource for ScreenCapture {
    fn stream_info(&self) -> Option<StreamInfo> {
        ScreenCapture::stream_info(self)
    }

    fn stream_stats(&self) -> Option<StreamStats> {
        ScreenCapture::stream_stats(self)
    }
}

impl<T: StreamSource + ?Sized> StreamSource for Arc<T> {
    fn stream_info(&self) -> Option<StreamInfo> {
        (**self).stream_info()
    }

    fn stream_stats(&self) -> Option<StreamStats> {
        (**self).stream_stats()
    }
}

/// What a watch does with a fresh description: the transport sends it to the display's
/// controller as a `stream_info` message.
pub type Publish = Arc<
    dyn Fn(DisplayId, Map<String, Json>) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync,
>;

/// The frames sent in one window: count, total, least, and most, in milliseconds from
/// capture to the wire.
#[derive(Clone, Copy, Debug)]
struct Sends {
    count: u64,
    total_ms: f64,
    min_ms: f64,
    max_ms: f64,
}

/// What a watch keeps between reads.
struct WatchState {
    task: Option<tokio::task::JoinHandle<()>>,
    module: Option<Arc<dyn StreamSource>>,
    totals: Option<pixelflux::report::StreamTotals>,
    totals_at: Option<Instant>,
    /// The window's extremes: pixelflux's, merged over every read since the window began.
    peaks: pixelflux::report::StreamPeaks,
    sends: Option<Sends>,
    /// Bytes sent per `PEAK_BUCKET_NS` bucket, oldest first, each as its start and its sum.
    buckets: VecDeque<(i64, u64)>,
}

/// Follows one display's capture: publishes its description on every change and
/// differences its counters for whoever is watching the numbers.
///
/// Every method takes `&self`: the relay notes each frame it hands to the socket from its
/// own task while the monitor's tick reads the rates from another, and the description task
/// writes `info` from a third. Nothing here is held across an await.
pub struct StreamWatch {
    display_id: DisplayId,
    publish: Publish,
    state: Mutex<WatchState>,
    /// The description last published, for a page that connects later.
    info: Mutex<Option<Map<String, Json>>>,
    timer: SendTimer,
}

impl StreamWatch {
    /// A watch for `display_id` whose descriptions go to `publish`.
    pub fn new(display_id: impl Into<DisplayId>, publish: Publish) -> Arc<Self> {
        Arc::new(Self {
            display_id: display_id.into(),
            publish,
            state: Mutex::new(WatchState {
                task: None,
                module: None,
                totals: None,
                totals_at: None,
                peaks: Default::default(),
                sends: None,
                buckets: VecDeque::new(),
            }),
            info: Mutex::new(None),
            timer: SendTimer::new(),
        })
    }

    /// A watch with no publisher, for the counters alone.
    pub fn silent(display_id: impl Into<DisplayId>) -> Arc<Self> {
        Self::new(display_id, Arc::new(|_, _| Box::pin(async {})))
    }

    pub fn display_id(&self) -> &str {
        &self.display_id
    }

    /// The description last published, for a page that connects later.
    pub fn info(&self) -> Option<Map<String, Json>> {
        self.info.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The per-frame record of the time from the end of a frame's encode to the wire.
    pub fn send_timer(&self) -> &SendTimer {
        &self.timer
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, WatchState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Start following `module`, the capture that now serves the display with this encoder
    /// and software-encoding choice. Runs on the tokio runtime; the description is read off
    /// it, since the first read of a VA-API session brings a GL context up.
    pub fn follow(self: &Arc<Self>, module: Arc<dyn StreamSource>, encoder: &str, use_cpu: bool) {
        self.stop();
        let weak = Arc::downgrade(self);
        let encoder = encoder.to_string();
        let task = tokio::spawn(Self::watch(weak, module.clone(), encoder, use_cpu));
        let mut state = self.lock();
        state.module = Some(module);
        state.task = Some(task);
    }

    /// Follow a capture bound to no runtime task, for a reader of the counters alone.
    pub fn attach(&self, module: Arc<dyn StreamSource>) {
        self.lock().module = Some(module);
    }

    /// Stop following; the last description is forgotten with the capture.
    pub fn stop(&self) {
        let mut state = self.lock();
        if let Some(task) = state.task.take() {
            task.abort();
        }
        state.module = None;
        state.totals = None;
        Self::restart_window(&mut state);
        drop(state);
        self.timer.take();
        *self.info.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// One frame on its way to the display's controller: `size` bytes captured at
    /// `capture_ns` ([`now_ns`]'s clock, zero where the capture stamped none), noted when
    /// the bytes are handed to the socket over WebSockets and when the frame's last packet
    /// leaves the pacer over WebRTC.
    pub fn note_send(&self, capture_ns: i64, size: usize) {
        let now = now_ns();
        let mut state = self.lock();
        if capture_ns > 0 {
            let ms = (now - capture_ns) as f64 / 1e6;
            match &mut state.sends {
                None => {
                    state.sends = Some(Sends {
                        count: 1,
                        total_ms: ms,
                        min_ms: ms,
                        max_ms: ms,
                    })
                }
                Some(sends) => {
                    sends.count += 1;
                    sends.total_ms += ms;
                    sends.min_ms = sends.min_ms.min(ms);
                    sends.max_ms = sends.max_ms.max(ms);
                }
            }
        }
        Self::count_bytes(&mut state, now, size as u64);
    }

    /// One frame on its way to the wire, with the whole of its timing: `note_send`'s
    /// capture-to-wire figure and the [`SendTimer`]'s encode-end-to-wire record in one call.
    pub fn frame_sent(&self, frame_id: i32, capture_ns: i64, encode_end_ns: i64, size: usize) {
        self.note_send(capture_ns, size);
        self.timer.frame_sent(frame_id, encode_end_ns, now_ns());
    }

    fn count_bytes(state: &mut WatchState, now: i64, size: u64) {
        let start = now - now.rem_euclid(PEAK_BUCKET_NS);
        if let Some(last) = state.buckets.back_mut()
            && last.0 == start
        {
            last.1 += size;
            return;
        }
        state.buckets.push_back((start, size));
        let horizon = start - (PEAK_WINDOW_S * 1e9) as i64;
        while state.buckets.front().is_some_and(|b| b.0 <= horizon) {
            state.buckets.pop_front();
        }
    }

    fn restart_window(state: &mut WatchState) {
        state.peaks = Default::default();
        state.sends = None;
    }

    /// Fold one read's extremes into the window's: pixelflux starts its own again on every
    /// read, so a read that does not end the window must keep them.
    fn merge_peaks(state: &mut WatchState, read: &pixelflux::report::StreamPeaks) {
        let peaks = &mut state.peaks;
        if read.pipeline_min_ns > 0
            && (peaks.pipeline_min_ns == 0 || read.pipeline_min_ns < peaks.pipeline_min_ns)
        {
            peaks.pipeline_min_ns = read.pipeline_min_ns;
        }
        peaks.pipeline_max_ns = peaks.pipeline_max_ns.max(read.pipeline_max_ns);
        peaks.frame_max_bytes = peaks.frame_max_bytes.max(read.frame_max_bytes);
    }

    /// The video sent over the last `PEAK_WINDOW_S`, in Mbit/s: `video_mbps` on average,
    /// over as much of the window as the stream has run, at least a second, and `peak_mbps`,
    /// its fullest `PEAK_BUCKET_NS`. Video alone, which is what the encoder's rate is set for
    /// and the page's received figure, audio and all, is not. Empty where nothing was sent in
    /// it.
    pub fn sent_mbps(&self) -> Map<String, Json> {
        let state = self.lock();
        Self::sent_mbps_of(&state.buckets, now_ns())
    }

    fn sent_mbps_of(buckets: &VecDeque<(i64, u64)>, now: i64) -> Map<String, Json> {
        let horizon = now - (PEAK_WINDOW_S * 1e9) as i64;
        let recent: Vec<&(i64, u64)> = buckets.iter().filter(|b| b.0 > horizon).collect();
        let mut out = Map::new();
        let Some(first) = recent.first() else {
            return out;
        };
        let bits = recent.iter().map(|b| b.1).sum::<u64>() as f64 * 8.0;
        let span = ((now - first.0) as f64 / 1e9).clamp(1.0, PEAK_WINDOW_S);
        let fullest = recent.iter().map(|b| b.1).max().unwrap_or(0) as f64;
        out.insert("video_mbps".into(), json!(round_to(bits / span / 1e6, 2)));
        out.insert(
            "peak_mbps".into(),
            json!(round_to(
                fullest * 8.0 / (PEAK_BUCKET_NS as f64 / 1e9) / 1e6,
                2
            )),
        );
        out
    }

    /// The description task: reads the capture's report after `SETTLE_S`, then every
    /// `WATCH_S`, and publishes it whenever it differs from the last.
    async fn watch(
        weak: Weak<Self>,
        module: Arc<dyn StreamSource>,
        encoder: String,
        use_cpu: bool,
    ) {
        let gpu = gpu_present();
        let mut delay = SETTLE_S;
        loop {
            tokio::time::sleep(Duration::from_secs_f64(delay)).await;
            let Some(watch) = weak.upgrade() else {
                return;
            };
            let reader = module.clone();
            let report = match tokio::task::spawn_blocking(move || reader.stream_info()).await {
                Ok(report) => report,
                Err(e) => {
                    debug!(target: "stats", "Stream description of '{}' unreadable: {e}", watch.display_id);
                    return;
                }
            };
            let Some(report) = report else {
                continue;
            };
            if report.info.encoder.is_empty() {
                continue;
            }
            delay = WATCH_S;
            let mut info = stream_info_message(&report);
            info.insert("gpu_present".into(), json!(gpu));
            let backends = crate::settings::settings().encoder_backends();
            info.insert(
                "hardware_expected".into(),
                json!(hardware_expected(&encoder, use_cpu, gpu, backends.as_ref())),
            );
            {
                let mut last = watch.info.lock().unwrap_or_else(|e| e.into_inner());
                if last.as_ref() == Some(&info) {
                    continue;
                }
                *last = Some(info.clone());
            }
            debug!(target: "stats", "Display '{}' stream description: {}", watch.display_id, Json::Object(info.clone()));
            let publish = watch.publish.clone();
            let display_id = watch.display_id.clone();
            drop(watch);
            publish(display_id, info).await;
        }
    }

    /// The encode's rate and cost since the last call: frames a second, the milliseconds a
    /// frame spent encoding and from capture to the end of its encode, on average and at the
    /// extremes, the largest frame, the milliseconds from capture to the wire (`note_send`),
    /// and from the end of the encode to the wire at the median and the 99th percentile
    /// (`wire_median_ms`, `wire_p99_ms`, from the [`SendTimer`]). Empty without a capture
    /// that counts, on the first call after a start or a spell unwatched, which only takes
    /// the baseline, and within `RATE_WINDOW_MIN_S` of the last call, which leaves the window
    /// growing.
    pub fn rates(&self) -> Map<String, Json> {
        let mut rates = Map::new();
        let mut state = self.lock();
        let Some(module) = state.module.clone() else {
            return rates;
        };
        let read = module.stream_stats();
        if let Some(read) = &read {
            Self::merge_peaks(&mut state, &read.peaks);
        }
        let now = Instant::now();
        let last = state.totals;
        let elapsed = state
            .totals_at
            .map(|at| now.duration_since(at).as_secs_f64())
            .unwrap_or(f64::INFINITY);
        if let (Some(read), Some(last)) = (&read, &last)
            && read.totals.frames >= last.frames
            && elapsed < RATE_WINDOW_MIN_S
        {
            return rates;
        }
        state.totals = read.map(|r| r.totals);
        state.totals_at = Some(now);
        let peaks = state.peaks;
        let sends = state.sends;
        Self::restart_window(&mut state);
        let wire = self.timer.take();
        let (Some(read), Some(last)) = (read, last) else {
            return rates;
        };
        if read.totals.frames < last.frames || !(elapsed > 0.0 && elapsed <= RATE_WINDOW_MAX_S) {
            return rates;
        }
        let frames = read.totals.frames - last.frames;
        rates.insert(
            "encoded_fps".into(),
            json!(round_to(frames as f64 / elapsed, 1)),
        );
        if frames > 0 {
            let per_frame = |total: u64, before: u64| {
                round_to(total.saturating_sub(before) as f64 / frames as f64 / 1e6, 2)
            };
            rates.insert(
                "encode_ms".into(),
                json!(per_frame(read.totals.encode_ns, last.encode_ns)),
            );
            rates.insert(
                "pipeline_ms".into(),
                json!(per_frame(read.totals.pipeline_ns, last.pipeline_ns)),
            );
            if peaks.pipeline_max_ns > 0 {
                rates.insert(
                    "pipeline_min_ms".into(),
                    json!(round_to(peaks.pipeline_min_ns as f64 / 1e6, 2)),
                );
                rates.insert(
                    "pipeline_max_ms".into(),
                    json!(round_to(peaks.pipeline_max_ns as f64 / 1e6, 2)),
                );
            }
            if peaks.frame_max_bytes > 0 {
                rates.insert(
                    "frame_max_kb".into(),
                    json!(round_to(peaks.frame_max_bytes as f64 / 1000.0, 1)),
                );
            }
        }
        if let Some(sends) = sends {
            rates.insert(
                "send_ms".into(),
                json!(round_to(sends.total_ms / sends.count as f64, 2)),
            );
            rates.insert("send_min_ms".into(), json!(round_to(sends.min_ms, 2)));
            rates.insert("send_max_ms".into(), json!(round_to(sends.max_ms, 2)));
        }
        if let Some(wire) = wire {
            rates.insert("wire_median_ms".into(), json!(round_to(wire.median_ms, 3)));
            rates.insert("wire_p99_ms".into(), json!(round_to(wire.p99_ms, 3)));
        }
        rates.extend(Self::sent_mbps_of(&state.buckets, now_ns()));
        rates
    }
}

#[cfg(test)]
mod tests {
    //! The checks of `tests/unit/test_stream_stats.py` that belong to this module: the verb,
    //! the hardware expectation, GPU presence, and the differencing of a capture's counters
    //! with the figures a window adds. The monitor's own checks live in `resources`.

    use super::*;
    use crate::settings::{Sides, TenBit};
    use pixelflux::report::{StreamPeaks, StreamTotals};

    /// A capture whose encode counters the test advances by hand, reporting each read's
    /// extremes and starting them again, as pixelflux's report does.
    struct Counting {
        frames: Mutex<u64>,
        peaks: Mutex<StreamPeaks>,
    }

    impl Counting {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                frames: Mutex::new(0),
                peaks: Mutex::new(StreamPeaks::default()),
            })
        }

        fn advance(&self, frames: u64) {
            *self.frames.lock().unwrap() += frames;
        }

        fn set_peaks(&self, min_ns: u64, max_ns: u64, max_bytes: u64) {
            *self.peaks.lock().unwrap() = StreamPeaks {
                pipeline_min_ns: min_ns,
                pipeline_max_ns: max_ns,
                frame_max_bytes: max_bytes,
            };
        }
    }

    impl StreamSource for Counting {
        fn stream_info(&self) -> Option<StreamInfo> {
            None
        }

        fn stream_stats(&self) -> Option<StreamStats> {
            let frames = *self.frames.lock().unwrap();
            let peaks = std::mem::take(&mut *self.peaks.lock().unwrap());
            Some(StreamStats {
                totals: StreamTotals {
                    frames,
                    bytes: 0,
                    encode_ns: frames * 2_000_000,
                    pipeline_ns: frames * 3_000_000,
                },
                peaks,
            })
        }
    }

    fn backends(entries: &[(&'static str, Option<&str>)]) -> HashMap<&'static str, CodecBackends> {
        entries
            .iter()
            .map(|(codec, hw)| {
                (
                    *codec,
                    CodecBackends {
                        hardware: hw.map(str::to_string),
                        software: None,
                        fullcolor: Sides {
                            hardware: None,
                            software: None,
                        },
                        ten_bit: Sides::<Option<TenBit>> {
                            hardware: None,
                            software: None,
                        },
                    },
                )
            })
            .collect()
    }

    fn num(rates: &Map<String, Json>, key: &str) -> Option<f64> {
        rates.get(key).and_then(Json::as_f64)
    }

    #[test]
    fn the_verb_subscribes_and_unsubscribes() {
        assert_eq!(stats_request("_stats,1"), Some(true));
        assert_eq!(stats_request("_stats,0"), Some(false));
        assert_eq!(
            stats_request("_stats,yes"),
            Some(false),
            "anything but 1 is off"
        );
        assert_eq!(stats_request("_stats"), Some(false));
        assert_eq!(
            stats_request("_stats_video,{}"),
            None,
            "another message is not the verb"
        );
        assert_eq!(stats_request("kd,65"), None);
    }

    #[test]
    fn hardware_is_expected_where_a_gpu_was_asked_for() {
        assert!(hardware_expected("h264enc", false, true, None));
        assert!(
            !hardware_expected("h264enc", true, true, None),
            "unless software encoding is on"
        );
        assert!(
            !hardware_expected("h264enc-striped", false, true, None),
            "the striped encoder never does"
        );
        assert!(
            !hardware_expected("jpeg", false, true, None),
            "nor does JPEG"
        );
        assert!(
            !hardware_expected("h264enc", false, false, None),
            "a host with no GPU expects none"
        );
        let probed = backends(&[("h264", Some("nvenc")), ("av1", None)]);
        assert!(
            !hardware_expected("av1enc", false, true, Some(&probed)),
            "nor a codec the GPU has no engine for"
        );
        assert!(hardware_expected("h264enc", false, true, Some(&probed)));
        assert!(
            hardware_expected("av1enc", false, true, None),
            "an unknown probe leaves the engine assumed"
        );
    }

    #[test]
    fn gpu_presence_is_what_the_device_nodes_say() {
        let nodes = std::fs::read_dir("/dev/dri")
            .map(|d| {
                d.flatten()
                    .any(|e| e.file_name().to_string_lossy().starts_with("renderD"))
            })
            .unwrap_or(false);
        assert_eq!(
            gpu_present(),
            nodes || std::path::Path::new("/dev/nvidiactl").exists()
        );
    }

    #[test]
    fn no_sample_no_host_figures() {
        let monitor = ResourceMonitor::new(1.0, 0, "", None);
        assert!(host_stats(&monitor).is_empty());
    }

    #[test]
    fn the_stream_info_object_is_keyed_as_the_pages_read_it() {
        let mut report = StreamInfo {
            info: pixelflux::report::StreamInfo {
                backend: "x11",
                capture: "DRI3",
                encoder: "VAAPI".into(),
                hardware: true,
                codec: "h264",
                stripes: 1,
                encode_node: 1,
                ..Default::default()
            },
            renderer: None,
        };
        let info = stream_info_message(&report);
        assert_eq!(info["encode_node"], "/dev/dri/renderD129");
        assert_eq!(info["striped"], false);
        assert!(!info.contains_key("renderer"));
        report.info.hardware = false;
        report.info.stripes = 4;
        report.renderer = Some(pixelflux::report::Renderer {
            kind: "gl",
            node: "/dev/dri/renderD128".into(),
            gpu: "Arc".into(),
            reason: String::new(),
        });
        let info = stream_info_message(&report);
        assert_eq!(info["encode_node"], "");
        assert_eq!(info["striped"], true);
        assert_eq!(info["renderer"], "gl");
        assert_eq!(info["render_gpu"], "Arc");
    }

    #[test]
    fn a_window_too_short_for_a_rate_keeps_growing() {
        let watch = StreamWatch::silent("primary");
        let capture = Counting::new();
        watch.attach(capture.clone());
        assert!(
            watch.rates().is_empty(),
            "the first call only takes the baseline"
        );
        capture.advance(1);
        assert!(
            watch.rates().is_empty(),
            "a window too short to hold a rate reports none rather than one frame over a few microseconds"
        );
        std::thread::sleep(Duration::from_secs_f64(RATE_WINDOW_MIN_S));
        capture.advance(29);
        let rates = watch.rates();
        let fps = num(&rates, "encoded_fps").unwrap_or(0.0);
        assert!(fps > 0.0 && fps <= 30.0 / RATE_WINDOW_MIN_S, "{rates:?}");
        assert_eq!(num(&rates, "encode_ms"), Some(2.0), "{rates:?}");
        for key in [
            "pipeline_min_ms",
            "pipeline_max_ms",
            "frame_max_kb",
            "send_ms",
            "peak_mbps",
            "wire_p99_ms",
        ] {
            assert!(
                !rates.contains_key(key),
                "no extremes and nothing sent add no figures: {rates:?}"
            );
        }
    }

    #[test]
    fn a_window_keeps_its_extremes_and_times_the_way_to_the_wire() {
        let watch = StreamWatch::silent("primary");
        let capture = Counting::new();
        watch.attach(capture.clone());
        watch.rates();
        capture.advance(10);
        capture.set_peaks(2_500_000, 9_000_000, 180_000);
        assert!(watch.rates().is_empty(), "a read too soon for a window");
        std::thread::sleep(Duration::from_secs_f64(RATE_WINDOW_MIN_S));
        capture.advance(10);
        capture.set_peaks(3_000_000, 4_000_000, 20_000);
        let now = now_ns();
        for (age_ms, size) in [(4.0, 30_000), (12.0, 250_000), (8.0, 20_000)] {
            watch.note_send(now - (age_ms * 1e6) as i64, size);
        }
        watch.note_send(0, 10_000);
        for (frame, age_ms) in [(1, 0.5), (2, 0.7), (3, 9.0)] {
            watch
                .send_timer()
                .frame_sent(frame, now - (age_ms * 1e6) as i64, now);
        }
        let rates = watch.rates();
        assert_eq!(
            num(&rates, "pipeline_min_ms"),
            Some(2.5),
            "the extremes read are the whole window's: {rates:?}"
        );
        assert_eq!(num(&rates, "pipeline_max_ms"), Some(9.0));
        assert_eq!(num(&rates, "frame_max_kb"), Some(180.0));
        let send_min = num(&rates, "send_min_ms").unwrap_or(0.0);
        let send = num(&rates, "send_ms").unwrap_or(0.0);
        let send_max = num(&rates, "send_max_ms").unwrap_or(0.0);
        assert!(
            (4.0..5.0).contains(&send_min)
                && (8.0..9.0).contains(&send)
                && (12.0..13.0).contains(&send_max),
            "a frame's way to the wire is timed from its capture, least, mean, and most: {rates:?}"
        );
        assert_eq!(num(&rates, "wire_median_ms"), Some(0.7), "{rates:?}");
        assert_eq!(num(&rates, "wire_p99_ms"), Some(9.0), "{rates:?}");
        let peak = round_to(310_000.0 * 8.0 / 0.25 / 1e6, 2);
        assert_eq!(
            num(&rates, "peak_mbps"),
            Some(peak),
            "the fullest quarter second's bytes as a rate: {rates:?}"
        );
        assert_eq!(
            num(&rates, "video_mbps"),
            Some(round_to(310_000.0 * 8.0 / 1e6, 2)),
            "the video rate spreads them over the stream's first second: {rates:?}"
        );
        capture.advance(10);
        std::thread::sleep(Duration::from_secs_f64(RATE_WINDOW_MIN_S));
        let rates = watch.rates();
        assert!(
            !rates.contains_key("send_ms")
                && !rates.contains_key("pipeline_max_ms")
                && !rates.contains_key("wire_p99_ms"),
            "the next window starts afresh for the times: {rates:?}"
        );
        assert_eq!(
            num(&rates, "peak_mbps"),
            Some(peak),
            "while the peak looks back over ten seconds: {rates:?}"
        );
        {
            let mut state = watch.lock();
            let first = state.buckets.front().map(|b| b.0).unwrap();
            state.buckets.push_front((first - 4_000_000_000, 90_000));
        }
        let span_s = {
            let state = watch.lock();
            (now_ns() - state.buckets.front().unwrap().0) as f64 / 1e9
        };
        let sent = watch.sent_mbps();
        assert!(
            (num(&sent, "video_mbps").unwrap() - 400_000.0 * 8.0 / span_s / 1e6).abs() < 0.01,
            "and over as much of ten seconds as it has run: {sent:?} {span_s}"
        );
        {
            let mut state = watch.lock();
            for bucket in state.buckets.iter_mut() {
                bucket.0 -= (PEAK_WINDOW_S * 1e9) as i64;
            }
        }
        assert!(watch.sent_mbps().is_empty(), "past them it is gone");
        watch.note_send(now_ns() - 5_000_000, 1000);
        watch
            .send_timer()
            .frame_sent(9, now_ns() - 1_000_000, now_ns());
        watch.stop();
        let state = watch.lock();
        assert!(
            state.sends.is_none() && state.peaks == StreamPeaks::default(),
            "a stopped watch forgets the window it was in"
        );
        drop(state);
        assert!(watch.send_timer().take().is_none());
    }
}
