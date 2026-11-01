/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Audit trail of clipboard and file transfers, POSTed to an operator's webhook.
//!
//! Every transfer the server carries is one JSON object on `audit_webhook_url`: its `event`,
//! an RFC 3339 `ts`, and metadata (byte size, MIME type, or file name), never the content.
//! Events queue in order and one task delivers them over a single keep-alive connection, so a
//! transfer pays an enqueue and nothing else; a collector that is slow or down loses what
//! overflows the queue of [`QUEUE_BOUND`] rather than stalling a session, each outage is
//! logged once, and once the queue drains an `audit.dropped` event carries the count it lost.
//! Without a URL every call is a no-op. [`install`] sets the process-wide sink [`emit`] feeds;
//! the events the HTTP plane emits are `file.upload.error {filename, error}`,
//! `file.upload.end {filename, size_bytes}`, `file.mkdir {filename}`,
//! `file.download {filename, size_bytes, partial}`, `print.document {filename, size_bytes,
//! partial}`, `recording.start {filename}`, `recording.stop {filename, size_bytes, duration_s,
//! frames}`, and `screenshot {display, size_bytes, role}`; the transports add
//! `session.connect`/`session.disconnect {transport, role, slot[, duration_s]}`,
//! `clipboard.send {mime_type, size_bytes}`, `clipboard.receive {mime_type, size_bytes,
//! multipart}`, and `capture.demand {subject, ...}`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use parking_lot::{Mutex, RwLock};
use serde_json::{Map, Value as Json};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;

/// The most events waiting for the collector before new ones are dropped.
pub const QUEUE_BOUND: usize = 1024;

/// The webhook settings.
#[derive(Clone, Debug)]
pub struct AuditConfig {
    /// Where events are POSTed; empty turns the trail off.
    pub url: String,
    /// Sent as `Authorization: Bearer <token>` when set.
    pub token: String,
    /// How long one POST may take before it counts as failed.
    pub timeout: Duration,
}

impl Default for AuditConfig {
    fn default() -> AuditConfig {
        AuditConfig {
            url: String::new(),
            token: String::new(),
            timeout: Duration::from_secs(2),
        }
    }
}

/// `ts`, seconds since the epoch, as an RFC 3339 UTC timestamp with milliseconds.
pub fn rfc3339(ts: f64) -> String {
    let millis = (ts * 1000.0).round() as i64;
    chrono::DateTime::from_timestamp_millis(millis)
        .map(|t| t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
        .unwrap_or_else(|| "1970-01-01T00:00:00.000Z".to_string())
}

fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

struct Event {
    event: String,
    ts: f64,
    fields: Map<String, Json>,
}

/// The queue's bookkeeping shared between the emitters and the sender.
struct Shared {
    /// Events queued and not yet attempted.
    pending: AtomicUsize,
    drained: Notify,
    dropped: AtomicU64,
    overflowing: AtomicBool,
}

/// One webhook sink: the queue and the task delivering from it.
pub struct Audit {
    cfg: AuditConfig,
    tx: mpsc::Sender<Event>,
    shared: Arc<Shared>,
    sender: Mutex<Option<JoinHandle<()>>>,
}

type Https = hyper_rustls::HttpsConnector<HttpConnector>;

impl Audit {
    /// Start delivering to `cfg.url`.
    pub fn start(cfg: AuditConfig) -> Arc<Audit> {
        let (tx, rx) = mpsc::channel(QUEUE_BOUND);
        let shared = Arc::new(Shared {
            pending: AtomicUsize::new(0),
            drained: Notify::new(),
            dropped: AtomicU64::new(0),
            overflowing: AtomicBool::new(false),
        });
        let sender = tokio::spawn(deliver(cfg.clone(), rx, tx.downgrade(), shared.clone()));
        Arc::new(Audit {
            cfg,
            tx,
            shared,
            sender: Mutex::new(Some(sender)),
        })
    }

    /// Queue one event and return at once; what overflows the queue is counted and reported
    /// once, and the dropped count rides an `audit.dropped` event when the queue drains.
    pub fn emit(&self, event: &str, fields: Map<String, Json>) {
        self.enqueue(Event {
            event: event.to_string(),
            ts: now_seconds(),
            fields,
        });
    }

    fn enqueue(&self, event: Event) {
        self.shared.pending.fetch_add(1, Ordering::SeqCst);
        if self.tx.try_send(event).is_err() {
            self.shared.pending.fetch_sub(1, Ordering::SeqCst);
            self.shared.dropped.fetch_add(1, Ordering::SeqCst);
            if !self.shared.overflowing.swap(true, Ordering::SeqCst) {
                log::warn!(target: "audit", "Audit webhook queue full ({QUEUE_BOUND} events); dropping events until it drains");
            }
        }
    }

    /// How many events wait for the collector.
    pub fn queued(&self) -> usize {
        self.shared.pending.load(Ordering::SeqCst)
    }

    /// Resolve once every queued event has been attempted.
    pub async fn drained(&self) {
        loop {
            let notified = self.shared.drained.notified();
            if self.queued() == 0 {
                return;
            }
            notified.await;
        }
    }

    /// Deliver what is queued, within one request timeout, and stop the sender.
    pub async fn close(&self) {
        let Some(sender) = self.sender.lock().take() else {
            return;
        };
        let _ = tokio::time::timeout(self.cfg.timeout, self.drained()).await;
        sender.abort();
        let _ = sender.await;
    }
}

/// The collector's connector, on this crate's crypto provider whatever else the build links.
/// A host with no root certificates can still deliver to an `http://` collector, so only an
/// `https://` one is warned about.
fn connector(cfg: &AuditConfig) -> Option<Https> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let builder = hyper_rustls::HttpsConnectorBuilder::new();
    let builder = match builder.with_provider_and_native_roots(provider.clone()) {
        Ok(builder) => builder,
        Err(e) => {
            if cfg.url.starts_with("https:") {
                log::warn!(target: "audit", "Audit webhook has no root certificates, so its HTTPS collector is refused: {e}");
            }
            let config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .inspect_err(
                    |e| log::error!(target: "audit", "Audit webhook cannot start a client: {e}"),
                )
                .ok()?
                .with_root_certificates(rustls::RootCertStore::empty())
                .with_no_client_auth();
            hyper_rustls::HttpsConnectorBuilder::new().with_tls_config(config)
        }
    };
    Some(builder.https_or_http().enable_http1().build())
}

/// Send queued events one by one; an outage is logged once, its end too.
async fn deliver(
    cfg: AuditConfig,
    mut rx: mpsc::Receiver<Event>,
    tx: mpsc::WeakSender<Event>,
    shared: Arc<Shared>,
) {
    let Some(https) = connector(&cfg) else { return };
    let client: Client<Https, Full<Bytes>> = Client::builder(TokioExecutor::new())
        .pool_max_idle_per_host(1)
        .build(https);
    let mut failing = false;
    while let Some(event) = rx.recv().await {
        let mut body = Map::new();
        body.insert("event".into(), Json::String(event.event));
        body.insert("ts".into(), Json::String(rfc3339(event.ts)));
        body.extend(event.fields);
        let failure = post(&client, &cfg, Json::Object(body)).await;
        if shared.pending.fetch_sub(1, Ordering::SeqCst) == 1 {
            shared.overflowing.store(false, Ordering::SeqCst);
            let dropped = shared.dropped.swap(0, Ordering::SeqCst);
            if dropped > 0 {
                let mut fields = Map::new();
                fields.insert("count".into(), Json::from(dropped));
                shared.pending.fetch_add(1, Ordering::SeqCst);
                let event = Event {
                    event: "audit.dropped".into(),
                    ts: now_seconds(),
                    fields,
                };
                let queued = tx.upgrade().is_some_and(|tx| tx.try_send(event).is_ok());
                if !queued {
                    shared.pending.fetch_sub(1, Ordering::SeqCst);
                }
            }
            shared.drained.notify_waiters();
        }
        match (&failure, failing) {
            (Some(reason), false) => {
                log::warn!(target: "audit", "Audit webhook failed: {reason}; events are dropped until it answers");
            }
            (None, true) => log::info!(target: "audit", "Audit webhook delivering again"),
            _ => {}
        }
        failing = failure.is_some();
    }
}

/// One POST within the timeout; the failure's description, or none on a 2xx/3xx answer. The
/// answer is read to its end and discarded, so the keep-alive connection goes back to the pool.
async fn post(
    client: &Client<Https, Full<Bytes>>,
    cfg: &AuditConfig,
    body: Json,
) -> Option<String> {
    let mut request =
        hyper::Request::post(&cfg.url).header(hyper::header::CONTENT_TYPE, "application/json");
    if !cfg.token.is_empty() {
        request = request.header(
            hyper::header::AUTHORIZATION,
            format!("Bearer {}", cfg.token),
        );
    }
    let request = match request.body(Full::new(Bytes::from(body.to_string()))) {
        Ok(request) => request,
        Err(e) => return Some(e.to_string()),
    };
    let exchange = async {
        let mut response = client.request(request).await.map_err(|e| e.to_string())?;
        while let Some(frame) = response.body_mut().frame().await {
            frame.map_err(|e| e.to_string())?;
        }
        Ok::<u16, String>(response.status().as_u16())
    };
    match tokio::time::timeout(cfg.timeout, exchange).await {
        Ok(Ok(status)) => (status >= 400).then(|| format!("HTTP {status}")),
        Ok(Err(e)) => Some(e),
        Err(_) => Some("timeout".to_string()),
    }
}

impl crate::files::AuditSink for Audit {
    fn emit(&self, event: &str, fields: Map<String, Json>) {
        Audit::emit(self, event, fields);
    }
}

static CURRENT: RwLock<Option<Arc<Audit>>> = RwLock::new(None);

/// Make `cfg` the process's audit sink, replacing any earlier one; a sink is started only
/// when a URL is set.
pub fn install(cfg: AuditConfig) -> Option<Arc<Audit>> {
    let sink = (!cfg.url.is_empty()).then(|| Audit::start(cfg));
    *CURRENT.write() = sink.clone();
    sink
}

/// The process's audit sink, if one is installed.
pub fn current() -> Option<Arc<Audit>> {
    CURRENT.read().clone()
}

/// Queue one event on the process's sink and return at once; a no-op without a webhook.
pub fn emit(event: &str, fields: Map<String, Json>) {
    if let Some(sink) = current() {
        sink.emit(event, fields);
    }
}

/// Deliver what is queued on the process's sink, within one timeout, and stop it.
pub async fn close() {
    let sink = CURRENT.write().take();
    if let Some(sink) = sink {
        sink.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::testing::{self, Capture};
    use crate::settings::Settings;
    use axum::Router;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post as post_route;
    use serde_json::json;
    use std::collections::HashSet;
    use std::net::SocketAddr;

    /// An audit collector that answers, rejects, or stalls on command.
    #[derive(Clone)]
    struct Collector {
        mode: Arc<Mutex<&'static str>>,
        release: Arc<Notify>,
        events: Arc<Mutex<Vec<Json>>>,
        headers: Arc<Mutex<Vec<HeaderMap>>>,
        peers: Arc<Mutex<HashSet<SocketAddr>>>,
    }

    async fn hook(
        State(c): State<Collector>,
        axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<SocketAddr>,
        headers: HeaderMap,
        body: Bytes,
    ) -> StatusCode {
        if *c.mode.lock() == "stall" {
            c.release.notified().await;
        }
        let parsed: Json = serde_json::from_slice(&body).unwrap();
        if *c.mode.lock() == "reject" {
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
        c.events.lock().push(parsed);
        c.headers.lock().push(headers);
        c.peers.lock().insert(peer);
        StatusCode::NO_CONTENT
    }

    async fn collector() -> (Collector, String, tokio::task::JoinHandle<()>, u16) {
        let c = Collector {
            mode: Arc::new(Mutex::new("ok")),
            release: Arc::new(Notify::new()),
            events: Arc::new(Mutex::new(Vec::new())),
            headers: Arc::new(Mutex::new(Vec::new())),
            peers: Arc::new(Mutex::new(HashSet::new())),
        };
        let app = Router::new()
            .route("/hook", post_route(hook))
            .with_state(c.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        (c, format!("http://127.0.0.1:{port}/hook"), server, port)
    }

    fn fields(pairs: &[(&str, Json)]) -> Map<String, Json> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    fn cfg(url: &str, timeout_ms: u64, token: &str) -> AuditConfig {
        AuditConfig {
            url: url.into(),
            token: token.into(),
            timeout: Duration::from_millis(timeout_ms),
        }
    }

    #[test]
    fn webhook_settings_stay_off_the_client() {
        let args: [String; 0] = [];
        let payload = Settings::parse(&args, &|_| None).client_payload();
        let leaked: Vec<&String> = payload
            .keys()
            .filter(|k| k.starts_with("audit_webhook"))
            .collect();
        assert!(
            leaked.is_empty(),
            "the webhook settings never reach a browser: {leaked:?}"
        );
    }

    #[test]
    fn timestamps() {
        assert_eq!(rfc3339(1_000_000.0), "1970-01-12T13:46:40.000Z");
        assert_eq!(rfc3339(1.5), "1970-01-01T00:00:01.500Z");
    }

    #[tokio::test]
    async fn delivery_and_outages() {
        if testing::rerun_alone("http::audit::tests::delivery_and_outages") {
            return;
        }
        let sink = Capture::install();
        let (c, url, server, port) = collector().await;

        let audit = Audit::start(cfg(&url, 2000, "s3cret"));
        audit.emit(
            "clipboard.send",
            fields(&[("mime_type", json!("text/plain")), ("size_bytes", json!(5))]),
        );
        audit.emit(
            "clipboard.receive",
            fields(&[
                ("mime_type", json!("image/png")),
                ("size_bytes", json!(7)),
                ("multipart", json!(true)),
            ]),
        );
        audit.emit(
            "file.upload.end",
            fields(&[("filename", json!("a/b.bin")), ("size_bytes", json!(9))]),
        );
        audit.emit(
            "file.upload.error",
            fields(&[
                ("filename", json!("c.bin")),
                ("error", json!("size mismatch")),
            ]),
        );
        audit.emit(
            "file.download",
            fields(&[("filename", json!("d.bin")), ("size_bytes", json!(11))]),
        );
        audit.drained().await;
        let got = c.events.lock().clone();
        let names: Vec<&str> = got.iter().map(|e| e["event"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "clipboard.send",
                "clipboard.receive",
                "file.upload.end",
                "file.upload.error",
                "file.download"
            ],
            "every event arrives, in emit order"
        );
        let mut second = got[1].clone();
        let ts = second.as_object_mut().unwrap().remove("ts").unwrap();
        assert_eq!(
            second,
            json!({"event": "clipboard.receive", "mime_type": "image/png", "size_bytes": 7, "multipart": true}),
            "an event carries its metadata and nothing else"
        );
        let re = regex::Regex::new(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z$").unwrap();
        assert!(
            got.iter().all(|e| re.is_match(e["ts"].as_str().unwrap())),
            "the timestamp is RFC 3339 UTC with milliseconds: {ts}"
        );
        let stamps: Vec<&str> = got.iter().map(|e| e["ts"].as_str().unwrap()).collect();
        let mut sorted = stamps.clone();
        sorted.sort();
        let first = chrono::DateTime::parse_from_rfc3339(stamps[0])
            .unwrap()
            .timestamp() as f64;
        assert!(
            stamps == sorted && (now_seconds() - first).abs() < 5.0,
            "timestamps come from emit time and keep order"
        );
        assert!(
            c.headers
                .lock()
                .iter()
                .all(|h| h.get("authorization").map(|v| v.to_str().unwrap())
                    == Some("Bearer s3cret")),
            "the token rides the Authorization header of every POST"
        );
        assert!(
            c.headers
                .lock()
                .iter()
                .all(|h| h.get("content-type").map(|v| v.to_str().unwrap())
                    == Some("application/json")),
            "the body is JSON"
        );
        assert_eq!(
            c.peers.lock().len(),
            1,
            "one keep-alive connection carries every event"
        );
        audit.close().await;

        let audit = Audit::start(cfg(&url, 2000, ""));
        audit.emit(
            "file.download",
            fields(&[("filename", json!("x")), ("size_bytes", json!(1))]),
        );
        audit.drained().await;
        assert!(
            !c.headers
                .lock()
                .last()
                .unwrap()
                .contains_key("authorization"),
            "no token, no Authorization header"
        );
        let n = 1000;
        let start = std::time::Instant::now();
        for i in 0..n {
            audit.emit(
                "clipboard.send",
                fields(&[("mime_type", json!("text/plain")), ("size_bytes", json!(i))]),
            );
        }
        let per_event = start.elapsed().as_secs_f64() / n as f64 * 1e6;
        audit.drained().await;
        assert!(
            per_event < 50.0 && c.events.lock().len() == 6 + n,
            "an emit costs the loop microseconds ({per_event:.1} us each) and loses nothing: {}",
            c.events.lock().len()
        );
        assert!(
            sink.lines_for("audit")
                .iter()
                .all(|l| !l.starts_with("WARNING")),
            "no warning during normal delivery: {:?}",
            sink.lines_for("audit")
        );
        audit.close().await;

        // A burst past the bound loses the overflow, and the drain reports how much.
        let audit = Audit::start(cfg(&url, 2000, ""));
        c.events.lock().clear();
        for i in 0..QUEUE_BOUND + 50 {
            audit.emit(
                "clipboard.send",
                fields(&[("mime_type", json!("text/plain")), ("size_bytes", json!(i))]),
            );
        }
        audit.drained().await;
        let events = c.events.lock().clone();
        assert_eq!(
            events.len(),
            QUEUE_BOUND + 1,
            "what fit in the queue, then the report"
        );
        let last = events.last().unwrap();
        assert_eq!(
            (last["event"].as_str(), last["count"].as_u64()),
            (Some("audit.dropped"), Some(50)),
            "once the queue drains an audit.dropped event carries the count it lost"
        );
        sink.clear();
        audit.close().await;

        // Outages.
        *c.mode.lock() = "reject";
        sink.clear();
        let audit = Audit::start(cfg(&url, 300, ""));
        for i in 0..3 {
            audit.emit(
                "file.download",
                fields(&[
                    ("filename", json!(format!("r{i}"))),
                    ("size_bytes", json!(i)),
                ]),
            );
        }
        audit.drained().await;
        let warnings: Vec<String> = sink
            .lines_for("audit")
            .into_iter()
            .filter(|l| l.starts_with("WARNING"))
            .collect();
        assert!(
            warnings.len() == 1 && warnings[0].contains("HTTP 500"),
            "a rejecting collector is logged once, with the status: {warnings:?}"
        );
        *c.mode.lock() = "ok";
        c.events.lock().clear();
        audit.emit(
            "file.download",
            fields(&[("filename", json!("back")), ("size_bytes", json!(1))]),
        );
        audit.drained().await;
        assert!(
            sink.lines_for("audit")
                .iter()
                .filter(|l| l.contains("delivering again"))
                .count()
                == 1
                && c.events
                    .lock()
                    .iter()
                    .map(|e| e["filename"].as_str().unwrap().to_string())
                    .collect::<Vec<_>>()
                    == ["back"],
            "recovery is logged once and the next event arrives"
        );

        *c.mode.lock() = "stall";
        sink.clear();
        let t0 = std::time::Instant::now();
        audit.emit(
            "file.download",
            fields(&[("filename", json!("slow")), ("size_bytes", json!(1))]),
        );
        audit.drained().await;
        let took = t0.elapsed().as_secs_f64();
        assert!(
            took > 0.25
                && took < 1.0
                && sink
                    .lines_for("audit")
                    .iter()
                    .filter(|l| l.starts_with("WARNING"))
                    .count()
                    == 1,
            "a stalled collector costs one timeout, then the sender moves on: {took:.2}s {:?}",
            sink.lines_for("audit")
        );

        sink.clear();
        let burst = QUEUE_BOUND + 50;
        let t0 = std::time::Instant::now();
        for i in 0..burst {
            audit.emit(
                "clipboard.send",
                fields(&[("mime_type", json!("text/plain")), ("size_bytes", json!(i))]),
            );
        }
        let emit_time = t0.elapsed().as_secs_f64();
        assert!(
            emit_time < 0.1
                && audit.queued() >= QUEUE_BOUND - 1
                && sink
                    .lines_for("audit")
                    .iter()
                    .filter(|l| l.contains("queue full"))
                    .count()
                    == 1,
            "a burst past the bound never blocks and is logged once: {emit_time:.3}s queued={} {:?}",
            audit.queued(),
            sink.lines_for("audit")
        );
        let t0 = std::time::Instant::now();
        audit.close().await;
        assert!(
            t0.elapsed().as_secs_f64() < 0.8,
            "shutdown with a stalled collector waits one timeout at most"
        );
        c.release.notify_waiters();
        server.abort();
        let _ = server.await;

        sink.clear();
        let audit = Audit::start(cfg(&format!("http://127.0.0.1:{port}/hook"), 300, ""));
        audit.emit(
            "file.download",
            fields(&[("filename", json!("down")), ("size_bytes", json!(1))]),
        );
        audit.drained().await;
        assert_eq!(
            sink.lines_for("audit")
                .iter()
                .filter(|l| l.starts_with("WARNING"))
                .count(),
            1,
            "a collector that is down is logged once: {:?}",
            sink.lines_for("audit")
        );
        audit.close().await;

        assert!(
            install(AuditConfig::default()).is_none(),
            "without a URL nothing is queued or started"
        );
        emit("file.download", fields(&[("filename", json!("nowhere"))]));
        close().await;
        sink.remove();
    }
}
