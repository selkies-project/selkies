/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Prometheus gauges and histograms both transports feed, rendered by the `/api/metrics`
//! route, plus an optional per-connection CSV dump of client-reported WebRTC statistics.
//!
//! A [`Metrics`] owns its registry: a mode switch builds a new one and drops the old, so
//! nothing is ever registered twice in a process-wide table. The HTTP agent calls
//! [`Metrics::render`] for the exposition text and serves it as [`CONTENT_TYPE`].
//!
//! The WebRTC statistics are an info metric whose labels are the client's flattened report
//! ([`Metrics::sanitize_json_stats`]), so they change on every report; the registry's
//! collectors are fixed at registration, which is why that one is a [`Collector`] reading a
//! shared cell rather than an `Info`.
//!
//! CSV writes run on one dedicated thread fed by a channel, so they preserve row order,
//! never block the runtime, and are drained deterministically at teardown
//! ([`Metrics::shutdown`]). The field names and values come from the untrusted browser: the
//! header width and the retained rows are capped, and a cell led by a formula character is
//! quoted as text ([`csv_cell`]) so a spreadsheet opening the file runs nothing.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use log::{debug, error, warn};
use prometheus_client::collector::Collector;
use prometheus_client::encoding::{DescriptorEncoder, text};
use prometheus_client::metrics::MetricType;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Registry;
use serde_json::Value as Json;

/// The buckets of the client FPS histogram.
pub const FPS_HIST_BUCKETS: [f64; 4] = [0.0, 20.0, 40.0, 60.0];
/// The widest a stats CSV header grows: the names come from the client.
pub const WEBRTC_CSV_MAX_HEADERS: usize = 2048;
/// The most data rows a stats CSV keeps, so it cannot grow unbounded.
pub const WEBRTC_CSV_MAX_RETAINED_ROWS: usize = 100_000;
/// The media type of [`Metrics::render`]'s text.
pub const CONTENT_TYPE: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

/// What a spreadsheet opening the CSV runs as a formula when a cell starts with it.
const CSV_FORMULA_LEADS: [char; 6] = ['=', '+', '-', '@', '\t', '\r'];

type Labels = Vec<(String, String)>;
type FloatGauge = Gauge<f64, AtomicU64>;

/// A stats cell or header from the client as the spreadsheet should show it: one led by a
/// formula character is quoted as text, unless it is a number.
pub fn csv_cell(text: &str) -> String {
    if text.starts_with(CSV_FORMULA_LEADS) && text.trim().parse::<f64>().is_err() {
        return format!("'{text}");
    }
    text.to_string()
}

/// Open a statistics file without following a link at its name, creating it readable by its
/// owner alone: the names are predictable, and in a shared directory another account could
/// plant a link there first.
fn open_no_follow(path: &Path, options: &mut OpenOptions) -> std::io::Result<File> {
    options
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(path)
}

fn open_append(path: &Path) -> std::io::Result<File> {
    open_no_follow(path, OpenOptions::new().append(true).create(true))
}

fn open_truncate(path: &Path) -> std::io::Result<File> {
    open_no_follow(
        path,
        OpenOptions::new().write(true).create(true).truncate(true),
    )
}

fn open_read(path: &Path) -> std::io::Result<File> {
    open_no_follow(path, OpenOptions::new().read(true))
}

/// One CSV row, fields quoted where they hold the delimiter, a quote, or a line break, with
/// the CRLF terminator spreadsheets expect.
fn csv_row(fields: &[String]) -> String {
    let mut row = String::new();
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            row.push(',');
        }
        if field.contains([',', '"', '\r', '\n']) {
            row.push('"');
            row.push_str(&field.replace('"', "\"\""));
            row.push('"');
        } else {
            row.push_str(field);
        }
    }
    row.push_str("\r\n");
    row
}

/// The rows of a CSV text, quotes and embedded line breaks honored.
pub(crate) fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    let mut pending = false;
    while let Some(c) = chars.next() {
        pending = true;
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' => quoted = true,
            ',' => row.push(std::mem::take(&mut field)),
            '\r' | '\n' => {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
                pending = false;
            }
            _ => field.push(c),
        }
    }
    if pending {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// Read a CSV file's rows; `None` where there is no file.
fn read_csv(path: &Path) -> std::io::Result<Option<Vec<Vec<String>>>> {
    let mut file = match open_read(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(Some(parse_csv(&text)))
}

/// Write `rows` to `path` through a temp file and an atomic replace, so an interrupted
/// rewrite cannot truncate the stats.
fn replace_csv(path: &Path, rows: impl Iterator<Item = String>) -> std::io::Result<()> {
    let tmp = path.with_file_name(format!(
        "{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    {
        let mut file = open_truncate(&tmp)?;
        for row in rows {
            file.write_all(row.as_bytes())?;
        }
    }
    std::fs::rename(&tmp, path)
}

/// The header state of one stats CSV, tracked so the append path knows when a report's
/// field set changed and bounds the file without re-reading it each write.
#[derive(Clone, Debug, Default, PartialEq)]
struct HeaderState {
    len: Option<usize>,
    /// Header names beside the length, so a same-count field swap still triggers a remap.
    names: Option<Vec<String>>,
    /// On-disk data rows, excluding the header.
    rows: usize,
}

/// The CSV capture's state, held under one lock by whoever writes.
#[derive(Debug, Default)]
pub(crate) struct CsvSink {
    video: HeaderState,
    audio: HeaderState,
}

impl CsvSink {
    fn state(&mut self, is_audio: bool) -> &mut HeaderState {
        if is_audio {
            &mut self.audio
        } else {
            &mut self.video
        }
    }

    fn reset(&mut self) {
        self.video = HeaderState::default();
        self.audio = HeaderState::default();
    }

    /// Append one sanitized stats report to the CSV file.
    ///
    /// Three schema cases: the same field set in a different order (a single-row remap, no
    /// rewrite), a changed field set (a full union-schema rewrite through [`Self::update`]),
    /// and a fresh file (header plus first row). Reconnecting clients send near-empty
    /// reports, too few fields to be a real stats sample, which are not written.
    fn write(&mut self, stats: &[(String, String)], path: &Path, is_audio: bool) {
        let timestamp = chrono::Local::now().format("%d/%B/%Y:%H:%M:%S").to_string();
        let mut headers = vec!["timestamp".to_string()];
        let mut values = vec![timestamp];
        for (key, value) in stats {
            headers.push(csv_cell(key));
            values.push(csv_cell(value));
        }
        if headers.len() < 15 {
            return;
        }
        let state = self.state(is_audio).clone();
        let result = (|| -> std::io::Result<HeaderState> {
            if let Some(prev_names) = &state.names
                && *prev_names != headers
            {
                let same_set = prev_names.iter().collect::<HashSet<_>>()
                    == headers.iter().collect::<HashSet<_>>();
                if same_set {
                    let by_name: HashMap<&String, &String> =
                        headers.iter().zip(values.iter()).collect();
                    let remapped: Vec<String> = prev_names
                        .iter()
                        .map(|n| by_name.get(n).map(|v| (*v).clone()).unwrap_or("NaN".into()))
                        .collect();
                    open_append(path)?.write_all(csv_row(&remapped).as_bytes())?;
                    return Self::bumped(state, path);
                }
                return Ok(Self::update(&state, path, &headers, &values));
            }
            let mut file = open_append(path)?;
            if state.len.is_none() {
                file.write_all(csv_row(&headers).as_bytes())?;
                file.write_all(csv_row(&values).as_bytes())?;
                return Ok(HeaderState {
                    len: Some(headers.len()),
                    names: Some(headers.clone()),
                    rows: 1,
                });
            }
            file.write_all(csv_row(&values).as_bytes())?;
            drop(file);
            Self::bumped(state, path)
        })();
        match result {
            Ok(next) => *self.state(is_audio) = next,
            Err(e) => error!(target: "metrics", "writing WebRTC Statistics to CSV file: {e}"),
        }
    }

    /// The state after one appended data row, the file trimmed once over the cap. Tracking
    /// the count avoids re-reading the file on every write; the O(N) trim runs only when the
    /// count exceeds `WEBRTC_CSV_MAX_RETAINED_ROWS`.
    fn bumped(mut state: HeaderState, path: &Path) -> std::io::Result<HeaderState> {
        state.rows += 1;
        if state.rows > WEBRTC_CSV_MAX_RETAINED_ROWS {
            state.rows = Self::trim_to_cap(path)?;
        }
        Ok(state)
    }

    /// Drop the oldest data rows so the file stays within the row cap, keeping at most
    /// `WEBRTC_CSV_MAX_RETAINED_ROWS` rows plus the header. Returns the resulting on-disk
    /// data-row count.
    fn trim_to_cap(path: &Path) -> std::io::Result<usize> {
        let Some(rows) = read_csv(path)? else {
            return Ok(0);
        };
        let Some((header, data)) = rows.split_first() else {
            return Ok(0);
        };
        if data.len() <= WEBRTC_CSV_MAX_RETAINED_ROWS {
            return Ok(data.len());
        }
        let kept = &data[data.len() - WEBRTC_CSV_MAX_RETAINED_ROWS..];
        replace_csv(
            path,
            std::iter::once(csv_row(header)).chain(kept.iter().map(|r| csv_row(r))),
        )?;
        Ok(kept.len())
    }

    /// Rewrite the CSV when the set of stat fields changes.
    ///
    /// The stored rows are aligned by field name onto the union header (prior order, new
    /// fields appended, width capped at `WEBRTC_CSV_MAX_HEADERS` because the names come from
    /// the client), gaps filled with `NaN`, and only the most recent
    /// `WEBRTC_CSV_MAX_RETAINED_ROWS` rows are carried forward. A file deleted since the last
    /// write, or holding only a header, is recreated with the current schema. Returns the new
    /// header state, or the previous one on failure so the caller's state stays consistent
    /// with the file.
    fn update(
        prev: &HeaderState,
        path: &Path,
        headers: &[String],
        values: &[String],
    ) -> HeaderState {
        let result = (|| -> std::io::Result<HeaderState> {
            let rows = read_csv(path)?.unwrap_or_default();
            let Some((prev_headers, prev_values)) =
                rows.split_first().filter(|(h, _)| !h.is_empty())
            else {
                replace_csv(path, [csv_row(headers), csv_row(values)].into_iter())?;
                return Ok(HeaderState {
                    len: Some(headers.len()),
                    names: Some(headers.to_vec()),
                    rows: 1,
                });
            };
            let mut merged: Vec<String> = prev_headers.clone();
            let mut seen: HashSet<&String> = prev_headers.iter().collect();
            for name in headers {
                if !seen.contains(name) {
                    if merged.len() >= WEBRTC_CSV_MAX_HEADERS {
                        warn!(target: "metrics",
                            "WebRTC Statistics header width capped at {WEBRTC_CSV_MAX_HEADERS} columns; dropping additional fields");
                        break;
                    }
                    merged.push(name.clone());
                    seen.insert(name);
                }
            }
            let prev_values = if prev_values.len() > WEBRTC_CSV_MAX_RETAINED_ROWS {
                &prev_values[prev_values.len() - WEBRTC_CSV_MAX_RETAINED_ROWS..]
            } else {
                prev_values
            };
            let index_of = |names: &[String]| -> HashMap<String, usize> {
                names
                    .iter()
                    .enumerate()
                    .map(|(i, n)| (n.clone(), i))
                    .collect()
            };
            let prev_index = index_of(prev_headers);
            let new_index = index_of(headers);
            let remap = |row: &[String], index: &HashMap<String, usize>| -> Vec<String> {
                merged
                    .iter()
                    .map(|name| {
                        index
                            .get(name)
                            .and_then(|&i| row.get(i))
                            .cloned()
                            .unwrap_or("NaN".into())
                    })
                    .collect()
            };
            let remapped_prev: Vec<Vec<String>> =
                prev_values.iter().map(|r| remap(r, &prev_index)).collect();
            let remapped_new = remap(values, &new_index);
            replace_csv(
                path,
                std::iter::once(csv_row(&merged))
                    .chain(remapped_prev.iter().map(|r| csv_row(r)))
                    .chain(std::iter::once(csv_row(&remapped_new))),
            )?;
            debug!(target: "metrics", "WebRTC Statistics file {} rewritten with updated schema", path.display());
            Ok(HeaderState {
                len: Some(merged.len()),
                names: Some(merged),
                rows: remapped_prev.len() + 1,
            })
        })();
        result.unwrap_or_else(|e| {
            error!(target: "metrics", "writing WebRTC Statistics to CSV file: {e}");
            prev.clone()
        })
    }
}

/// The client's last WebRTC report as the labels of the `webrtc_statistics` info metric.
#[derive(Debug, Default)]
struct WebRtcStatistics(Mutex<Labels>);

impl Collector for WebRtcStatistics {
    fn encode(&self, mut encoder: DescriptorEncoder) -> Result<(), std::fmt::Error> {
        let labels = self.0.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let mut metric = encoder.encode_descriptor(
            "webrtc_statistics",
            "WebRTC Statistics from the client",
            None,
            MetricType::Info,
        )?;
        metric.encode_info(&labels)
    }
}

/// A CSV write queued for the writer thread.
struct CsvJob {
    stats: Vec<(String, String)>,
    path: String,
    is_audio: bool,
}

/// The CSV writer thread and the channel feeding it.
struct CsvWriter {
    sender: Option<mpsc::Sender<CsvJob>>,
    thread: Option<std::thread::JoinHandle<()>>,
    closing: Arc<AtomicBool>,
}

/// One pacer's snapshot, for the per-display pacer gauges.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PacerSnapshot {
    pub pace_bps: f64,
    pub queued_bytes: f64,
    pub video_bytes: f64,
    pub idr_floor_bytes: f64,
    pub video_dropped: f64,
    pub gop_resets: f64,
    pub keyreqs: f64,
    pub idr_resurrects: f64,
    pub timeout_resurrects: f64,
    pub stale_resets: f64,
}

/// Prometheus metrics plus optional CSV capture of client WebRTC stats.
///
/// The pacer gauges are per display and exist only while a pacer is attached; the event
/// counters are cumulative since transport start. The bridge drop gauges are per display,
/// cumulative since the graph was built, and published whether or not a pacer is attached;
/// `webrtc_bridge_invalidated_frames` counts the share of those the encoder was told to
/// predict past, the rest being frames that predicted from one already dropped.
pub struct Metrics {
    registry: Registry,
    fps: FloatGauge,
    fps_hist: Histogram,
    gpu_utilization: FloatGauge,
    latency: FloatGauge,
    webrtc_statistics: Arc<WebRtcStatistics>,
    webrtc_pacer_pace_bps: Family<Labels, FloatGauge>,
    webrtc_pacer_queue_bytes: Family<Labels, FloatGauge>,
    webrtc_pacer_idr_floor_bytes: Family<Labels, FloatGauge>,
    webrtc_pacer_events: Family<Labels, FloatGauge>,
    webrtc_bridge_dropped_frames: Family<Labels, FloatGauge>,
    webrtc_bridge_invalidated_frames: Family<Labels, FloatGauge>,
    using_webrtc_csv: bool,
    csv: Arc<Mutex<CsvSink>>,
    /// The video and audio CSV paths of the current connection.
    paths: Mutex<(Option<String>, Option<String>)>,
    writer: Mutex<CsvWriter>,
}

impl std::fmt::Debug for Metrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Metrics")
            .field("using_webrtc_csv", &self.using_webrtc_csv)
            .finish()
    }
}

fn labels(pairs: &[(&str, &str)]) -> Labels {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

impl Metrics {
    /// Build the registry and its collectors; with `using_webrtc_csv`, client reports are
    /// also appended to per-connection CSV files.
    pub fn new(using_webrtc_csv: bool) -> Self {
        let mut registry = Registry::default();
        let fps = FloatGauge::default();
        registry.register("fps", "Frames per second observed by client", fps.clone());
        let fps_hist = Histogram::new(FPS_HIST_BUCKETS);
        registry.register(
            "fps_hist",
            "Histogram of FPS observed by client",
            fps_hist.clone(),
        );
        let gpu_utilization = FloatGauge::default();
        registry.register(
            "gpu_utilization",
            "Utilization percentage reported by GPU",
            gpu_utilization.clone(),
        );
        let latency = FloatGauge::default();
        registry.register("latency", "Latency observed by client", latency.clone());
        let webrtc_statistics = Arc::new(WebRtcStatistics::default());
        registry.register_collector(Box::new(SharedCollector(webrtc_statistics.clone())));
        let webrtc_pacer_pace_bps = Family::<Labels, FloatGauge>::default();
        registry.register(
            "webrtc_pacer_pace_bps",
            "Current pacer rate in bits per second",
            webrtc_pacer_pace_bps.clone(),
        );
        let webrtc_pacer_queue_bytes = Family::<Labels, FloatGauge>::default();
        registry.register(
            "webrtc_pacer_queue_bytes",
            "Bytes queued in the pacer",
            webrtc_pacer_queue_bytes.clone(),
        );
        let webrtc_pacer_idr_floor_bytes = Family::<Labels, FloatGauge>::default();
        registry.register(
            "webrtc_pacer_idr_floor_bytes",
            "IDR floor of the pacer video queue budget in bytes",
            webrtc_pacer_idr_floor_bytes.clone(),
        );
        let webrtc_pacer_events = Family::<Labels, FloatGauge>::default();
        registry.register(
            "webrtc_pacer_events",
            "Cumulative pacer event counter",
            webrtc_pacer_events.clone(),
        );
        let webrtc_bridge_dropped_frames = Family::<Labels, FloatGauge>::default();
        registry.register(
            "webrtc_bridge_dropped_frames",
            "Encoded frames dropped before packetization, per display",
            webrtc_bridge_dropped_frames.clone(),
        );
        let webrtc_bridge_invalidated_frames = Family::<Labels, FloatGauge>::default();
        registry.register(
            "webrtc_bridge_invalidated_frames",
            "Dropped frames the encoder was told to predict past, per display",
            webrtc_bridge_invalidated_frames.clone(),
        );
        let csv = Arc::new(Mutex::new(CsvSink::default()));
        let (sender, receiver) = mpsc::channel::<CsvJob>();
        let closing = Arc::new(AtomicBool::new(false));
        let thread = {
            let csv = csv.clone();
            let closing = closing.clone();
            std::thread::Builder::new()
                .name("webrtc-csv".into())
                .spawn(move || {
                    while let Ok(job) = receiver.recv() {
                        if closing.load(Ordering::Acquire) {
                            continue;
                        }
                        let mut sink = csv.lock().unwrap_or_else(|e| e.into_inner());
                        sink.write(&job.stats, Path::new(&job.path), job.is_audio);
                    }
                })
                .ok()
        };
        Self {
            registry,
            fps,
            fps_hist,
            gpu_utilization,
            latency,
            webrtc_statistics,
            webrtc_pacer_pace_bps,
            webrtc_pacer_queue_bytes,
            webrtc_pacer_idr_floor_bytes,
            webrtc_pacer_events,
            webrtc_bridge_dropped_frames,
            webrtc_bridge_invalidated_frames,
            using_webrtc_csv,
            csv,
            paths: Mutex::new((None, None)),
            writer: Mutex::new(CsvWriter {
                sender: Some(sender),
                thread,
                closing,
            }),
        }
    }

    pub fn using_webrtc_csv(&self) -> bool {
        self.using_webrtc_csv
    }

    /// The registry in OpenMetrics text, what `/api/metrics` serves.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if let Err(e) = text::encode(&mut out, &self.registry) {
            error!(target: "metrics", "rendering the metrics registry: {e}");
        }
        out
    }

    /// Record the client-observed FPS in both the gauge and the histogram.
    pub fn set_fps(&self, fps: f64) {
        self.fps.set(fps);
        self.fps_hist.observe(fps);
    }

    pub fn set_gpu_utilization(&self, utilization: f64) {
        self.gpu_utilization.set(utilization);
    }

    pub fn set_latency(&self, latency_ms: f64) {
        self.latency.set(latency_ms);
    }

    /// Publish one pacer snapshot for `display` (`primary` where empty).
    pub fn set_pacer_snapshot(&self, display: &str, snap: &PacerSnapshot) {
        let display = if display.is_empty() {
            "primary"
        } else {
            display
        };
        self.webrtc_pacer_pace_bps
            .get_or_create(&labels(&[("display", display)]))
            .set(snap.pace_bps);
        self.webrtc_pacer_queue_bytes
            .get_or_create(&labels(&[("display", display), ("kind", "total")]))
            .set(snap.queued_bytes);
        self.webrtc_pacer_queue_bytes
            .get_or_create(&labels(&[("display", display), ("kind", "video")]))
            .set(snap.video_bytes);
        self.webrtc_pacer_idr_floor_bytes
            .get_or_create(&labels(&[("display", display)]))
            .set(snap.idr_floor_bytes);
        for (event, value) in [
            ("video_dropped", snap.video_dropped),
            ("gop_resets", snap.gop_resets),
            ("keyreqs", snap.keyreqs),
            ("idr_resurrects", snap.idr_resurrects),
            ("timeout_resurrects", snap.timeout_resurrects),
            ("stale_resets", snap.stale_resets),
        ] {
            self.webrtc_pacer_events
                .get_or_create(&labels(&[("display", display), ("event", event)]))
                .set(value);
        }
    }

    /// Publish each display's bridge drop and invalidation counts.
    pub fn set_bridge_drops(&self, drops: &HashMap<String, (u64, u64)>) {
        for (display, (dropped, invalidated)) in drops {
            let display = if display.is_empty() {
                "primary"
            } else {
                display.as_str()
            };
            self.webrtc_bridge_dropped_frames
                .get_or_create(&labels(&[("display", display)]))
                .set(*dropped as f64);
            self.webrtc_bridge_invalidated_frames
                .get_or_create(&labels(&[("display", display)]))
                .set(*invalidated as f64);
        }
    }

    /// Publish a client stats report to Prometheus and, optionally, CSV.
    ///
    /// `kind` is `_stats_audio` for the audio stream; anything else is video. `report` is the
    /// client's raw JSON list of RTCStats-shaped objects; parsing and sanitizing run off the
    /// runtime, since a report can be large. The CSV write goes to the writer thread; one
    /// submitted after [`Self::shutdown`] is dropped.
    pub async fn set_webrtc_stats(self: &Arc<Self>, kind: &str, report: String) {
        let stats = match tokio::task::spawn_blocking(move || {
            serde_json::from_str::<Json>(&report).map(|parsed| Self::sanitize_json_stats(&parsed))
        })
        .await
        {
            Ok(Ok(stats)) => stats,
            Ok(Err(e)) => {
                warn!(target: "metrics", "WebRTC statistics report unreadable: {e}");
                return;
            }
            Err(_) => return,
        };
        if self.using_webrtc_csv {
            let is_audio = kind == "_stats_audio";
            let path = {
                let paths = self.paths.lock().unwrap_or_else(|e| e.into_inner());
                if is_audio {
                    paths.1.clone()
                } else {
                    paths.0.clone()
                }
            };
            if let Some(path) = path {
                let writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(sender) = &writer.sender {
                    let _ = sender.send(CsvJob {
                        stats: stats.clone(),
                        path,
                        is_audio,
                    });
                }
            }
        }
        *self
            .webrtc_statistics
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = stats;
    }

    /// Flatten a list of RTCStats objects into `reportName.fieldName` keys, in order.
    ///
    /// The first entry of each stat type gets the bare type as its report name; later
    /// same-type entries get a `-id` suffix, or `-n` (the per-type occurrence index) without
    /// an id, plus a collision counter. Both stay stable across reorders and inserts, unlike a
    /// global list index, which would shift every column name whenever the browser reordered
    /// the list and churn the CSV schema. Entries are sorted by `(type, id)` first for the
    /// same reason: the browser may emit same-type stats (two `inbound-rtp` for distinct
    /// SSRCs) in a different order each message, and without a fixed order the bare-named
    /// first occurrence could be a different SSRC on each row; the sort is stable, so entries
    /// sharing a key keep their input order. All values are stringified. Entries that are not
    /// objects are skipped and a missing or non-string `type` reads `unknown`, since the list
    /// comes from the untrusted browser client.
    pub fn sanitize_json_stats(list: &Json) -> Vec<(String, String)> {
        let Json::Array(entries) = list else {
            return Vec::new();
        };
        let identity = |entry: &Json| -> (String, String) {
            match entry.as_object() {
                None => (String::new(), String::new()),
                Some(obj) => (
                    obj.get("type")
                        .and_then(Json::as_str)
                        .unwrap_or("unknown")
                        .to_string(),
                    obj.get("id")
                        .and_then(Json::as_str)
                        .unwrap_or("")
                        .to_string(),
                ),
            }
        };
        let mut sorted: Vec<&Json> = entries.iter().collect();
        sorted.sort_by_key(|e| identity(e));
        let mut names: HashSet<String> = HashSet::new();
        let mut type_counts: HashMap<String, usize> = HashMap::new();
        let mut out = Vec::new();
        for entry in sorted {
            let Some(obj) = entry.as_object() else {
                continue;
            };
            let base = obj
                .get("type")
                .and_then(Json::as_str)
                .unwrap_or("unknown")
                .to_string();
            let occurrence = *type_counts.get(&base).unwrap_or(&0);
            type_counts.insert(base.clone(), occurrence + 1);
            let mut name = base.clone();
            if names.contains(&name) {
                let suffix = match obj.get("id").and_then(Json::as_str) {
                    Some(id) if !id.is_empty() => id.to_string(),
                    _ => occurrence.to_string(),
                };
                let mut candidate = format!("{base}-{suffix}");
                let mut collision = 0;
                while names.contains(&candidate) {
                    collision += 1;
                    candidate = format!("{base}-{suffix}-{collision}");
                }
                name = candidate;
            }
            names.insert(name.clone());
            for (key, value) in obj {
                let text = match value {
                    Json::String(s) => s.clone(),
                    Json::Bool(true) => "True".to_string(),
                    Json::Bool(false) => "False".to_string(),
                    Json::Null => "None".to_string(),
                    Json::Number(n) => n.to_string(),
                    other => other.to_string(),
                };
                out.push((format!("{name}.{key}"), text));
            }
        }
        out
    }

    /// Append one sanitized stats report to the CSV at `path` now, on the caller's thread.
    pub fn write_webrtc_stats_csv(&self, stats: &[(String, String)], path: &str, is_audio: bool) {
        self.csv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write(stats, Path::new(path), is_audio);
    }

    /// Point CSV capture at fresh timestamped files for a new connection. The header state
    /// is reset off the runtime: a rewrite in flight on the writer may hold the lock.
    pub async fn initialize_webrtc_csv_file(&self, webrtc_stats_dir: &str) {
        let timestamp = chrono::Local::now().format("%Y-%m-%d:%H:%M:%S").to_string();
        {
            let mut paths = self.paths.lock().unwrap_or_else(|e| e.into_inner());
            paths.0 = Some(format!(
                "{webrtc_stats_dir}/selkies-stats-video-{timestamp}.csv"
            ));
            paths.1 = Some(format!(
                "{webrtc_stats_dir}/selkies-stats-audio-{timestamp}.csv"
            ));
        }
        let csv = self.csv.clone();
        let _ = tokio::task::spawn_blocking(move || {
            csv.lock().unwrap_or_else(|e| e.into_inner()).reset()
        })
        .await;
    }

    /// Drain the CSV writer: writes not yet started are dropped, the one in flight finishes,
    /// and the thread is joined, so no writer is still running after teardown. The
    /// collectors go with the registry when this is dropped; a mode switch builds a fresh
    /// `Metrics`.
    pub fn shutdown(&self) {
        let (sender, thread) = {
            let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
            writer.closing.store(true, Ordering::Release);
            (writer.sender.take(), writer.thread.take())
        };
        drop(sender);
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }
}

impl Drop for Metrics {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// A collector the registry owns a share of, so the metrics can keep writing to it.
#[derive(Debug)]
struct SharedCollector(Arc<WebRtcStatistics>);

impl Collector for SharedCollector {
    fn encode(&self, encoder: DescriptorEncoder) -> Result<(), std::fmt::Error> {
        self.0.encode(encoder)
    }
}

#[cfg(test)]
mod tests {
    //! The CSV cells of a client report, and the collectors a second build registers.

    use super::*;
    use serde_json::json;

    fn report() -> Json {
        let mut plain = serde_json::Map::new();
        plain.insert("type".into(), json!("inbound-rtp"));
        plain.insert("id".into(), json!("IT01"));
        for i in 0..16 {
            plain.insert(format!("field{i}"), json!(i));
        }
        let hostile = json!({
            "type": "=HYPERLINK(\"http://x.test\")", "id": "H1", "-2+3": "@SUM(A1:A9)",
            "codec": "=cmd|' /C calc'!A0", "jitter": "-0.004", "gain": "+3", "note": "\tTAB"
        });
        Json::Array(vec![Json::Object(plain), hostile])
    }

    fn is_number(text: &str) -> bool {
        text.trim().parse::<f64>().is_ok()
    }

    #[test]
    fn the_csv_shows_what_the_client_reported_as_text() {
        assert_eq!(
            csv_cell("-0.004"),
            "-0.004",
            "a negative number stays a number"
        );
        assert_eq!(csv_cell("=1+1"), "'=1+1", "a formula is quoted");
        let metrics = Metrics::new(true);
        let dir = std::env::temp_dir().join(format!("selkies-stats-csv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("video.csv");
        let stats = Metrics::sanitize_json_stats(&report());
        metrics.write_webrtc_stats_csv(&stats, path.to_str().unwrap(), false);
        metrics.write_webrtc_stats_csv(
            &Metrics::sanitize_json_stats(&report()),
            path.to_str().unwrap(),
            false,
        );
        let rows = parse_csv(&std::fs::read_to_string(&path).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
        let cells: Vec<&String> = rows.iter().flatten().collect();
        let led: Vec<&&String> = cells
            .iter()
            .filter(|c| c.starts_with(CSV_FORMULA_LEADS) && !is_number(c))
            .collect();
        assert!(
            led.is_empty(),
            "no cell or header is led by a formula character: {led:?}"
        );
        assert_eq!(rows.len(), 3, "one header and both reports");
        assert!(
            cells.iter().any(|c| *c == "'=cmd|' /C calc'!A0")
                && cells.iter().any(|c| *c == "'@SUM(A1:A9)")
                && cells.iter().any(|c| *c == "-0.004"),
            "the hostile names and values are kept, quoted"
        );
        assert!(
            stats.iter().any(|(_, v)| v == "=cmd|' /C calc'!A0"),
            "the stats the metrics keep are as reported"
        );
        assert!(
            stats.iter().any(|(k, _)| k == "inbound-rtp.field3"),
            "{stats:?}"
        );
        metrics.shutdown();
    }

    #[test]
    fn same_type_entries_are_named_stably() {
        let list = json!([
            {"type": "inbound-rtp", "id": "B", "x": 1},
            {"type": "inbound-rtp", "id": "A", "x": 2},
            {"type": "codec", "y": true},
            {"type": "codec", "y": null},
            "not a dict"
        ]);
        let keys: Vec<String> = Metrics::sanitize_json_stats(&list)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(
            keys,
            vec![
                "codec.type",
                "codec.y",
                "codec-1.type",
                "codec-1.y",
                "inbound-rtp.id",
                "inbound-rtp.type",
                "inbound-rtp.x",
                "inbound-rtp-B.id",
                "inbound-rtp-B.type",
                "inbound-rtp-B.x"
            ]
        );
        let values: Vec<String> = Metrics::sanitize_json_stats(&list)
            .into_iter()
            .map(|(_, v)| v)
            .collect();
        assert!(values.contains(&"True".to_string()) && values.contains(&"None".to_string()));
    }

    #[test]
    fn csv_rows_round_trip() {
        let fields = vec![
            "plain".to_string(),
            "a,b".into(),
            "say \"hi\"".into(),
            "line\nbreak".into(),
            String::new(),
        ];
        let rows = parse_csv(&format!("{}{}", csv_row(&fields), csv_row(&fields)));
        assert_eq!(rows, vec![fields.clone(), fields]);
    }

    #[test]
    fn a_second_build_after_the_first_is_dropped_registers_every_collector() {
        let names = [
            "fps",
            "fps_hist",
            "gpu_utilization",
            "latency",
            "webrtc_statistics",
            "webrtc_pacer_pace_bps",
            "webrtc_pacer_queue_bytes",
            "webrtc_pacer_idr_floor_bytes",
            "webrtc_pacer_events",
            "webrtc_bridge_dropped_frames",
            "webrtc_bridge_invalidated_frames",
        ];
        let first = Metrics::new(false);
        first.set_fps(58.0);
        first.set_pacer_snapshot(
            "",
            &PacerSnapshot {
                pace_bps: 1e6,
                ..Default::default()
            },
        );
        let text = first.render();
        for name in names {
            assert!(
                text.contains(&format!("# HELP {name} ")),
                "the build registers {name}: {text}"
            );
        }
        assert!(
            text.contains("fps 58.0") || text.contains("fps 58"),
            "{text}"
        );
        assert!(
            text.contains("webrtc_pacer_pace_bps{display=\"primary\"}"),
            "{text}"
        );
        first.shutdown();
        drop(first);
        let second = Metrics::new(false);
        let text = second.render();
        assert!(
            names.iter().all(|n| text.contains(&format!("# HELP {n} "))),
            "a second build after the first succeeds"
        );
        assert!(text.contains("webrtc_statistics_info"), "{text}");
    }
}
