/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The clipboard unit suites: the fakes stand at the pixelflux and transport boundaries (the
//! compositor surface, the host's frames, the key injection), and everything between them is
//! the real path.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use log::{Level, LevelFilter, Log, Metadata, Record};
use parking_lot::{Condvar, Mutex};

use super::flavors::{FLAVORS_MIME, envelope, flavors, markup_has_text};
use super::outbound::{CHUNK_SIZE, ClientId, ClipboardHost};
use super::paste::{PASTE_FETCH, PASTE_FETCH_MAX, PasteKeys};
use super::x11::{Atoms, Reply, read_selection};
use super::{
    Clipboard, CompositorClipboard, Entry, PNG_WAIT, Payload, Policy, Session, native_payload, png,
};
use crate::core::Role;

const PASSWORD: &str = "correct horse battery staple";
/// The spellings password managers offer the hint under, written out rather than read from
/// the server's table: the checks hold the server to them.
const HINTS: [&str; 3] = [
    "x-kde-passwordManagerHint",
    "text/x-kde-passwordManagerHint",
    "application/x-kde-passwordManagerHint",
];

// --- Log recording -------------------------------------------------------------------------

/// Records what the `input` and `ws` targets log, so a check can read the level a line took.
struct Recorder {
    lines: Mutex<Vec<(Level, String)>>,
}

impl Log for Recorder {
    fn enabled(&self, _: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        if matches!(record.target(), "input" | "ws") {
            self.lines
                .lock()
                .push((record.level(), record.args().to_string()));
        }
    }

    fn flush(&self) {}
}

static RECORDER: Recorder = Recorder {
    lines: Mutex::new(Vec::new()),
};
/// Tests that read the log, or run a monitor loop, take turns.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn record_logs() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        log::set_logger(&RECORDER).expect("no other logger in the test binary");
        log::set_max_level(LevelFilter::Trace);
    });
}

fn lines_with(text: &str) -> Vec<(Level, String)> {
    RECORDER
        .lines
        .lock()
        .iter()
        .filter(|(_, l)| l.contains(text))
        .cloned()
        .collect()
}

fn clear_logs() {
    RECORDER.lines.lock().clear();
}

// --- Fakes ----------------------------------------------------------------------------------

/// One audit event: its name, the mime, the size, and whether the transfer was multipart.
type Audit = (String, String, usize, Option<bool>);

/// The transport: clients with roles, recording every frame each is sent.
#[derive(Default)]
struct FakeHost {
    clients: Mutex<Vec<(ClientId, Role)>>,
    frames: Mutex<HashMap<ClientId, Vec<String>>>,
    /// A moment per frame, so a newer payload can overtake an older one.
    delay: Option<Duration>,
    audits: Mutex<Vec<Audit>>,
}

impl FakeHost {
    fn with(clients: &[(ClientId, Role)]) -> Arc<FakeHost> {
        Arc::new(FakeHost {
            clients: Mutex::new(clients.to_vec()),
            ..Default::default()
        })
    }

    fn slow(clients: &[(ClientId, Role)]) -> Arc<FakeHost> {
        Arc::new(FakeHost {
            clients: Mutex::new(clients.to_vec()),
            delay: Some(Duration::from_millis(2)),
            ..Default::default()
        })
    }

    fn frames(&self, client: ClientId) -> Vec<String> {
        self.frames.lock().get(&client).cloned().unwrap_or_default()
    }

    fn verbs(&self, client: ClientId) -> Vec<String> {
        self.frames(client)
            .iter()
            .map(|f| f.split(',').next().unwrap_or("").to_string())
            .collect()
    }

    fn clear(&self) {
        self.frames.lock().clear();
    }

    /// The payloads the frames to `client` reassemble to, as the page would see them: a secret
    /// where a `clipboard_secret` frame preceded it, multipart runs joined.
    fn payloads(&self, client: ClientId) -> Vec<Payload> {
        let mut out = Vec::new();
        let mut secret = false;
        let mut assembling: Option<(String, Vec<u8>)> = None;
        for frame in self.frames(client) {
            let mut parts = frame.splitn(3, ',');
            let verb = parts.next().unwrap_or("");
            match verb {
                "clipboard_secret" => secret = true,
                "clipboard_reply" => {}
                "clipboard" => {
                    let data = BASE64.decode(parts.next().unwrap_or("")).unwrap();
                    let text = String::from_utf8_lossy(&data).into_owned();
                    out.push(if secret {
                        Payload::secret(text)
                    } else {
                        Payload::text(text)
                    });
                    secret = false;
                }
                "clipboard_binary" => {
                    let mime = parts.next().unwrap_or("").to_string();
                    let data = BASE64.decode(parts.next().unwrap_or("")).unwrap();
                    out.push(Payload::binary(mime, data));
                    secret = false;
                }
                "clipboard_start" => {
                    assembling = Some((parts.next().unwrap_or("").to_string(), Vec::new()));
                }
                "clipboard_data" => {
                    if let Some((_, buf)) = assembling.as_mut() {
                        buf.extend(BASE64.decode(parts.next().unwrap_or("")).unwrap());
                    }
                }
                "clipboard_finish" => {
                    if let Some((mime, buf)) = assembling.take() {
                        if mime == "text/plain" {
                            let text = String::from_utf8_lossy(&buf).into_owned();
                            out.push(if secret {
                                Payload::secret(text)
                            } else {
                                Payload::text(text)
                            });
                        } else {
                            out.push(Payload::binary(mime, buf));
                        }
                    }
                    secret = false;
                }
                _ => {}
            }
        }
        out
    }
}

#[async_trait::async_trait]
impl ClipboardHost for FakeHost {
    fn clients(&self) -> Vec<(ClientId, Role)> {
        self.clients.lock().clone()
    }

    async fn send_text(&self, client: ClientId, message: String) -> bool {
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        self.frames.lock().entry(client).or_default().push(message);
        true
    }

    async fn bulk_window(&self, _client: ClientId, _len: usize) {}

    fn audit(&self, event: &str, mime: &str, size: usize, multipart: Option<bool>) {
        self.audits
            .lock()
            .push((event.into(), mime.into(), size, multipart));
    }
}

/// The session's topology.
struct FakeSession {
    wayland: bool,
    separate: bool,
    app_display: String,
    x11_display: Option<String>,
    x11_session_display: Mutex<Option<String>>,
    invalidated: Mutex<Vec<String>>,
    /// Whether the next liveness check finds the app compositor's socket dead.
    dead: AtomicBool,
}

impl FakeSession {
    fn x11(display: Option<&str>) -> Arc<FakeSession> {
        Arc::new(FakeSession {
            wayland: false,
            separate: false,
            app_display: String::new(),
            x11_display: display.map(str::to_string),
            x11_session_display: Mutex::new(None),
            invalidated: Mutex::new(Vec::new()),
            dead: AtomicBool::new(false),
        })
    }

    fn wayland(separate: bool, app_display: &str) -> Arc<FakeSession> {
        Arc::new(FakeSession {
            wayland: true,
            separate,
            app_display: app_display.into(),
            x11_display: None,
            x11_session_display: Mutex::new(None),
            invalidated: Mutex::new(Vec::new()),
            dead: AtomicBool::new(false),
        })
    }
}

impl Session for FakeSession {
    fn is_wayland(&self) -> bool {
        self.wayland
    }

    fn app_display(&self) -> String {
        self.app_display.clone()
    }

    fn has_separate_app_compositor(&self) -> bool {
        self.separate
    }

    fn invalidate_app_display_if_dead(&self, display: &str) -> bool {
        self.invalidated.lock().push(display.to_string());
        self.dead.swap(false, Ordering::Relaxed)
    }

    fn x11_display(&self) -> Option<String> {
        self.x11_display.clone()
    }

    fn x11_session_display(&self) -> Option<String> {
        self.x11_session_display.lock().clone()
    }
}

/// pixelflux's clipboard surface: the capture compositor's sink and offer, and an app
/// compositor's data-control selection, each failing on demand.
#[derive(Default)]
struct FakeCompositor {
    /// The sink the capture compositor holds; every registration stages a re-read of the
    /// current selection, like the real one.
    sink: Mutex<Option<Arc<dyn pixelflux::sink::ClipboardSink>>>,
    arms: AtomicUsize,
    arm_error: Mutex<Option<String>>,
    selection: Mutex<Option<Vec<Entry>>>,
    reads: AtomicUsize,
    /// What `set_clipboard` offered, per call.
    offers: Mutex<Vec<(Vec<Entry>, Instant)>>,
    set_error: Mutex<Option<String>>,
    /// The app compositor's selection, served by mime.
    app: Mutex<HashMap<String, Vec<u8>>>,
    app_reads: Mutex<Vec<String>>,
    dc_error: Mutex<Option<String>>,
    /// What `clipboard_write_app` offered, per call.
    app_writes: Mutex<Vec<(Vec<Entry>, Instant)>>,
    app_write_ok: Mutex<bool>,
    cleared: Mutex<Vec<String>>,
    watch_error: Mutex<Option<String>>,
    watch_attempts: Mutex<Vec<String>>,
    watches: Mutex<Vec<(String, Arc<dyn pixelflux::sink::SelectionWatch>)>>,
    /// The mimes the app selection has served, grown by `Fetches`.
    served: Mutex<Vec<String>>,
    served_grew: Condvar,
    /// `(delay after the chord, mime)` fetches the focused app and a history make.
    fetches: Mutex<Vec<(Duration, String)>>,
}

impl FakeCompositor {
    fn new() -> Arc<FakeCompositor> {
        Arc::new(FakeCompositor {
            app_write_ok: Mutex::new(true),
            ..Default::default()
        })
    }

    /// A client copy offering these flavors.
    fn copy(&self, entries: Vec<Entry>) {
        *self.selection.lock() = Some(entries);
        self.deliver_current();
    }

    /// Hand the current selection to the sink from a foreign thread, as pixelflux does.
    fn deliver_current(&self) {
        let sink = self.sink.lock().clone();
        let selection = self.selection.lock().clone();
        if let Some(sink) = sink {
            self.reads.fetch_add(1, Ordering::Relaxed);
            let entries = selection.unwrap_or_default();
            let t = std::thread::spawn(move || sink.selection(entries));
            let _ = t.join();
        }
    }

    /// The flavors `set_clipboard` offered, per call.
    fn offered_mimes(&self) -> Vec<Vec<String>> {
        self.offers
            .lock()
            .iter()
            .map(|(e, _)| e.iter().map(|(m, _)| m.clone()).collect())
            .collect()
    }

    fn with_app(self: &Arc<Self>, entries: &[(&str, &[u8])]) -> Arc<Self> {
        *self.app.lock() = entries
            .iter()
            .map(|(m, d)| (m.to_string(), d.to_vec()))
            .collect();
        self.clone()
    }

    /// Start the fetches the chord triggers.
    fn chord(self: &Arc<Self>) {
        for (delay, mime) in self.fetches.lock().clone() {
            let me = self.clone();
            std::thread::spawn(move || {
                std::thread::sleep(delay);
                let mut served = me.served.lock();
                served.push(mime);
                me.served_grew.notify_all();
            });
        }
    }
}

impl CompositorClipboard for FakeCompositor {
    fn set_clipboard_sink(
        &self,
        sink: Arc<dyn pixelflux::sink::ClipboardSink>,
    ) -> Result<(), String> {
        self.arms.fetch_add(1, Ordering::Relaxed);
        if let Some(e) = self.arm_error.lock().clone() {
            return Err(e);
        }
        *self.sink.lock() = Some(sink);
        if self.selection.lock().is_some() {
            self.deliver_current();
        }
        Ok(())
    }

    fn set_clipboard(&self, entries: Vec<Entry>) -> Result<(), String> {
        if let Some(e) = self.set_error.lock().clone() {
            return Err(e);
        }
        self.offers.lock().push((entries, Instant::now()));
        Ok(())
    }

    fn clipboard_types_app(&self, _display: &str) -> Result<Vec<String>, String> {
        if let Some(e) = self.dc_error.lock().clone() {
            return Err(e);
        }
        let mut types: Vec<String> = self.app.lock().keys().cloned().collect();
        types.sort();
        Ok(types)
    }

    fn clipboard_read_app(&self, _display: &str, mime: &str) -> Result<Option<Vec<u8>>, String> {
        if let Some(e) = self.dc_error.lock().clone() {
            return Err(e);
        }
        self.app_reads.lock().push(mime.to_string());
        Ok(self.app.lock().get(mime).cloned())
    }

    fn clipboard_write_app(&self, _display: &str, entries: Vec<Entry>) -> Result<(), String> {
        if !*self.app_write_ok.lock() {
            return Err("write refused".into());
        }
        self.app_writes.lock().push((entries, Instant::now()));
        Ok(())
    }

    fn clipboard_served_app(&self, after: usize, timeout: Duration) -> Vec<String> {
        let mut served = self.served.lock();
        if served.len() <= after {
            self.served_grew.wait_for(&mut served, timeout);
        }
        served.clone()
    }

    fn clipboard_clear_app(&self, display: &str) -> Result<(), String> {
        self.cleared.lock().push(display.to_string());
        Ok(())
    }

    fn clipboard_watch_app(
        &self,
        display: &str,
        watch: Arc<dyn pixelflux::sink::SelectionWatch>,
    ) -> Result<(), String> {
        self.watch_attempts.lock().push(display.to_string());
        if let Some(e) = self.watch_error.lock().clone() {
            return Err(e);
        }
        self.watches.lock().push((display.to_string(), watch));
        Ok(())
    }

    fn clipboard_unwatch_app(&self, _display: &str) {}
}

/// The key injection, recording every event and its time; an Insert press starts the fake
/// compositor's fetches.
struct FakeKeys {
    keys: Mutex<Vec<(u32, bool)>>,
    times: Mutex<Vec<Instant>>,
    active: Mutex<Vec<u32>>,
    compositor: Option<Arc<FakeCompositor>>,
}

const SHIFT_L: u32 = 0xFFE1;
const INSERT: u32 = 0xFF63;
const CTRL_L: u32 = 0xFFE3;
const PASTE_CHORD: [(u32, bool); 4] = [
    (SHIFT_L, true),
    (INSERT, true),
    (INSERT, false),
    (SHIFT_L, false),
];

impl FakeKeys {
    fn new(active: &[u32], compositor: Option<Arc<FakeCompositor>>) -> FakeKeys {
        FakeKeys {
            keys: Mutex::new(Vec::new()),
            times: Mutex::new(Vec::new()),
            active: Mutex::new(active.to_vec()),
            compositor,
        }
    }

    fn chord_of(&self) -> Vec<(u32, bool)> {
        self.keys
            .lock()
            .iter()
            .copied()
            .filter(|(k, _)| *k == SHIFT_L || *k == INSERT)
            .collect()
    }

    fn time_of(&self, event: (u32, bool)) -> Instant {
        let keys = self.keys.lock();
        let index = keys.iter().position(|k| *k == event).expect("event sent");
        self.times.lock()[index]
    }
}

#[async_trait::async_trait]
impl PasteKeys for FakeKeys {
    async fn key(&self, keysym: u32, down: bool, _neutralize: bool) {
        self.keys.lock().push((keysym, down));
        self.times.lock().push(Instant::now());
        if keysym == INSERT
            && down
            && let Some(comp) = &self.compositor
        {
            comp.chord();
        }
    }

    fn held_modifiers(&self) -> Vec<u32> {
        self.active.lock().clone()
    }

    fn modifier_held(&self, keysym: u32) -> bool {
        self.active.lock().contains(&keysym)
    }
}

fn policy(clipboard: &str, binary: bool) -> Policy {
    Policy {
        clipboard: clipboard.into(),
        binary,
    }
}

/// A handler on a plain pixelflux session (no X server, no nested app compositor), one
/// controller client recording what it is sent.
fn wayland_handler(host: Arc<FakeHost>, comp: Arc<FakeCompositor>, binary: bool) -> Arc<Clipboard> {
    Clipboard::new(
        host,
        FakeSession::wayland(false, "wayland-1"),
        Some(comp),
        policy("true", binary),
    )
}

/// A handler under a nested app compositor.
fn nested_handler(host: Arc<FakeHost>, comp: Arc<FakeCompositor>, binary: bool) -> Arc<Clipboard> {
    Clipboard::new(
        host,
        FakeSession::wayland(true, "wayland-9"),
        Some(comp),
        policy("true", binary),
    )
}

fn controller() -> Arc<FakeHost> {
    FakeHost::with(&[(1, Role::Controller)])
}

fn is_secret(payload: &Payload, text: &str) -> bool {
    payload.secret && payload.is_text() && payload.text_lossy() == text
}

fn text_entries(entries: &[(&str, &[u8])]) -> Vec<Entry> {
    entries
        .iter()
        .map(|(m, d)| (m.to_string(), d.to_vec()))
        .collect()
}

fn jpeg(width: u32, height: u32) -> Vec<u8> {
    let img = image::RgbImage::from_pixel(width, height, image::Rgb([200, 40, 40]));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Jpeg).unwrap();
    out.into_inner()
}

async fn stop_monitor(h: &Arc<Clipboard>, task: tokio::task::JoinHandle<()>) {
    h.request_stop();
    if tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .is_err()
    {
        panic!("monitor loop did not stop");
    }
}

// --- flavors --------------------------------------------------------------------------------

#[test]
fn envelope_round_trips_both_flavors() {
    let entries = text_entries(&[
        ("text/html", "<b>héllo</b>".as_bytes()),
        ("text/plain", "héllo".as_bytes()),
    ]);
    let packed = envelope(&entries);
    assert_eq!(flavors(&packed).unwrap(), entries);
    let json: serde_json::Value = serde_json::from_slice(&packed).unwrap();
    assert_eq!(
        json["text/plain"], "héllo",
        "JSON a collector or a browser can read without base64"
    );
    let markup_only = text_entries(&[("text/html", b"<i>x</i>")]);
    assert_eq!(
        flavors(&envelope(&markup_only)).unwrap(),
        markup_only,
        "markup alone is a valid envelope"
    );
    let reversed: Vec<Entry> = entries.iter().rev().cloned().collect();
    assert_eq!(
        flavors(&envelope(&reversed)).unwrap()[0].0,
        "text/html",
        "the richest flavor comes first"
    );
    for (junk, why) in [
        (&b"[]"[..], "a list"),
        (b"{}", "an empty object"),
        (br#"{"image/png": "x"}"#, "no text flavor"),
        (br#"{"text/html": ""}"#, "an empty flavor"),
    ] {
        assert!(flavors(junk).is_err(), "{why} is refused");
    }
}

/// An office application's formatted text, as Word writes its markup: styles in the head, the
/// selection between fragment comments; a picture of it rides beside.
const WORD_HTML: &[u8] = b"<html><head><style>p.MsoNormal{margin:0}</style></head><body>\
<!--StartFragment--><p class=MsoNormal><b>Quarterly</b> report&nbsp;draft</p>\
<!--EndFragment--></body></html>";
const PICTURE_HTML: &[u8] =
    b"<meta charset=\"utf-8\"><img src=\"https://example.org/logo.png\" alt=\"Logo\">";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nfake";

#[test]
fn markup_decides_whether_a_copy_is_text() {
    assert!(
        markup_has_text(WORD_HTML),
        "formatted text's markup carries text"
    );
    assert!(
        !markup_has_text(PICTURE_HTML),
        "a copied picture's markup carries none"
    );
    assert!(
        !markup_has_text(
            b"<html><head><title>Doc</title></head><body><p>&nbsp;</p><img src='file:///tmp/clip_image001.png'></body></html>"
        ),
        "nor does a picture copied out of a document, spaces around its img"
    );
    assert!(
        markup_has_text(b"<b>bold</b> words"),
        "a fragment's text counts without a body"
    );
    assert!(
        !markup_has_text(b"<style>b{}</style><script>x()</script><img src=a>"),
        "styles and scripts are not text"
    );
    let entries = text_entries(&[
        ("text/html", WORD_HTML),
        ("text/plain", b"Quarterly report draft"),
        ("image/png", PNG),
    ]);
    let payload = native_payload(&entries, true).unwrap();
    assert_eq!(
        payload.mime, FLAVORS_MIME,
        "the compositor's formatted text with its picture goes as the text"
    );
    assert_eq!(
        flavors(&payload.bytes).unwrap(),
        text_entries(&[
            ("text/html", WORD_HTML),
            ("text/plain", b"Quarterly report draft")
        ])
    );
    assert_eq!(
        native_payload(&entries, false).unwrap().mime,
        FLAVORS_MIME,
        "and so where pictures are not taken"
    );
    let picture = text_entries(&[("text/html", PICTURE_HTML), ("image/png", PNG)]);
    assert_eq!(
        native_payload(&picture, true),
        Some(Payload::binary("image/png", PNG)),
        "a copied picture goes as the picture"
    );
    assert_eq!(
        native_payload(&picture, false),
        None,
        "and as nothing where pictures are not taken"
    );
}

#[test]
fn clipboard_settings_are_client_overridable_bools_on_by_default() {
    for name in ["clipboard_seamless", "keyboard_shortcuts"] {
        let def = crate::settings::DEFINITIONS
            .iter()
            .find(|d| d.name == name)
            .expect(name);
        assert_eq!(def.kind, crate::settings::Kind::Bool, "{name}");
        assert!(
            crate::settings::parse_bool(Some(def.default), false),
            "{name} defaults on"
        );
    }
}

fn wire(entries: &[Entry]) -> String {
    format!("cb,{FLAVORS_MIME},{}", BASE64.encode(envelope(entries)))
}

#[tokio::test(flavor = "multi_thread")]
async fn dispatch_writes_a_flavor_set_as_the_markup_with_every_flavor_offered() {
    let entries = text_entries(&[("text/html", b"<b>rich</b>"), ("text/plain", b"rich")]);

    let comp = FakeCompositor::new();
    let h = wayland_handler(controller(), comp.clone(), true);
    assert!(h.on_message(1, &wire(&entries)).await);
    assert_eq!(
        comp.offers
            .lock()
            .iter()
            .map(|(e, _)| e.clone())
            .collect::<Vec<_>>(),
        vec![entries.clone()]
    );

    let comp = FakeCompositor::new();
    let h = wayland_handler(controller(), comp.clone(), false);
    h.on_message(1, &wire(&entries)).await;
    assert_eq!(
        comp.offers.lock().len(),
        1,
        "the image switch does not gate it"
    );
    h.on_message(1, &format!("cb,image/png,{}", BASE64.encode(b"\x89PNG")))
        .await;
    assert_eq!(
        comp.offers.lock().len(),
        1,
        "an image still obeys that switch"
    );

    let comp = FakeCompositor::new();
    let h = Clipboard::new(
        controller(),
        FakeSession::wayland(false, "wayland-1"),
        Some(comp.clone()),
        policy("out", true),
    );
    h.on_message(1, &wire(&entries)).await;
    assert!(
        comp.offers.lock().is_empty(),
        "a closed inbound direction still closes it"
    );

    let comp = FakeCompositor::new();
    let h = wayland_handler(controller(), comp.clone(), true);
    h.on_message(1, &format!("cb,{FLAVORS_MIME},{}", BASE64.encode(b"{}")))
        .await;
    assert!(
        comp.offers.lock().is_empty(),
        "a malformed envelope writes nothing"
    );

    let comp = FakeCompositor::new();
    let h = wayland_handler(controller(), comp.clone(), true);
    h.on_message(1, &format!("cbs,t1,{FLAVORS_MIME},2")).await;
    h.on_message(1, &format!("cbd,t1,{}", BASE64.encode(b"{}")))
        .await;
    h.on_message(1, "cbe,t1").await;
    assert!(
        comp.offers.lock().is_empty() && !h.inbound.lock().in_progress(),
        "a malformed multi-part envelope writes nothing and closes the transfer"
    );

    let comp = FakeCompositor::new();
    let h = wayland_handler(controller(), comp.clone(), true);
    let raw = envelope(&entries);
    let half = raw.len() / 2;
    h.on_message(1, &format!("cbs,t1,{FLAVORS_MIME},{}", raw.len()))
        .await;
    for chunk in [&raw[..half], &raw[half..]] {
        h.on_message(1, &format!("cbd,t1,{}", BASE64.encode(chunk)))
            .await;
    }
    h.on_message(1, "cbe,t1").await;
    assert_eq!(
        comp.offers
            .lock()
            .iter()
            .map(|(e, _)| e.clone())
            .collect::<Vec<_>>(),
        vec![entries],
        "a multipart flavor set arrives whole"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn multipart_transfers_are_bounded_and_one_at_a_time() {
    let comp = FakeCompositor::new();
    let h = wayland_handler(controller(), comp.clone(), true);
    h.on_message(
        1,
        &format!("cws,t1,{}", super::verbs::MULTIPART_MAX_BYTES + 1),
    )
    .await;
    assert!(
        !h.inbound.lock().in_progress(),
        "an oversized declaration opens no transfer"
    );
    h.on_message(1, "cws,t1,4").await;
    h.on_message(1, &format!("cwd,t1,{}", BASE64.encode(b"abcde")))
        .await;
    assert!(
        !h.inbound.lock().in_progress(),
        "a chunk past the declared size aborts the transfer"
    );
    h.on_message(1, "cws,t2,4").await;
    h.on_message(1, &format!("cwd,t2,{}", BASE64.encode(b"ab")))
        .await;
    h.on_message(1, "cwe,t2").await;
    assert!(
        comp.offers.lock().is_empty(),
        "a size mismatch writes nothing"
    );
    h.on_message(1, "cws,t3,4").await;
    h.on_message(1, &format!("cwd,t3,{}", BASE64.encode(b"ab")))
        .await;
    h.on_message(1, &format!("cbd,t3,{}", BASE64.encode(b"cd")))
        .await;
    h.on_message(1, &format!("cwd,t3,{}", BASE64.encode(b"cd")))
        .await;
    h.on_message(1, "cwe,t3").await;
    assert_eq!(
        comp.offers.lock()[0].0,
        text_entries(&[("text/plain", b"abcd")]),
        "a mismatched kind is ignored, the right chunks land"
    );
    let audits = controller();
    let h = wayland_handler(audits.clone(), comp.clone(), true);
    h.on_message(1, &format!("cw,{}", BASE64.encode("hello".as_bytes())))
        .await;
    assert_eq!(
        audits.audits.lock().last().cloned(),
        Some((
            "clipboard.receive".into(),
            "text/plain".into(),
            5,
            Some(false)
        ))
    );
}

// --- echo -----------------------------------------------------------------------------------

/// A handler whose session clipboard last read `baseline` (`mime`).
fn with_baseline(baseline: Option<(&[u8], &str)>) -> (Arc<Clipboard>, Arc<FakeCompositor>) {
    let comp = FakeCompositor::new();
    let h = wayland_handler(controller(), comp.clone(), true);
    if let Some((bytes, mime)) = baseline {
        let mut st = h.state.lock();
        st.last_bytes = Some(Bytes::copy_from_slice(bytes));
        st.last_mime = Some(mime.into());
    }
    (h, comp)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_push_of_the_sessions_own_text_is_left_to_its_owner() {
    let paths = b"file:///home/u/report.odt\nfile:///home/u/notes.txt";
    let (h, comp) = with_baseline(Some((paths, "text/plain")));
    let crlf = String::from_utf8_lossy(paths).replace('\n', "\r\n");
    assert!(h.write(Payload::text(crlf), None).await);
    assert!(
        comp.offers.lock().is_empty(),
        "the session's own text with CRLF line ends is no copy"
    );
    assert!(
        h.write(
            Payload::text(format!("{}\n", String::from_utf8_lossy(paths))),
            None
        )
        .await
    );
    assert!(
        comp.offers.lock().is_empty(),
        "nor with a line end after it"
    );
    h.write(Payload::text("file:///home/u/other.txt"), None)
        .await;
    assert_eq!(
        comp.offers.lock()[0].0,
        text_entries(&[("text/plain", b"file:///home/u/other.txt")]),
        "other text is written"
    );

    let (h, comp) = with_baseline(Some((b"some text", "text/plain")));
    h.write(Payload::secret("some text"), None).await;
    assert_eq!(
        comp.offers.lock().len(),
        1,
        "a secret is written even when its text is the session's, for its hint"
    );

    let markup = envelope(&text_entries(&[
        ("text/html", b"<p>from <b>the session</b></p>"),
        ("text/plain", b"from the session"),
    ]));
    let (h, comp) = with_baseline(Some((&markup, FLAVORS_MIME)));
    let back = text_entries(&[
        (
            "text/html",
            b"<meta charset='utf-8'><p>from <b>the session</b></p>",
        ),
        ("text/plain", b"from the session"),
    ]);
    h.write(
        Payload::binary("text/html", back[0].1.clone()),
        Some(back.clone()),
    )
    .await;
    assert!(
        comp.offers.lock().is_empty(),
        "the session's formatted copy come back rewrapped is no copy"
    );
    h.write(Payload::text("from the session"), None).await;
    assert!(comp.offers.lock().is_empty(), "nor is its plain text alone");
    let edited = text_entries(&[("text/html", b"<p>edited</p>"), ("text/plain", b"edited")]);
    h.write(
        Payload::binary("text/html", edited[0].1.clone()),
        Some(edited),
    )
    .await;
    assert_eq!(
        comp.offers.lock().len(),
        1,
        "a different formatted copy is written"
    );

    let (h, comp) = with_baseline(Some((b"\x89PNG...", "image/png")));
    h.write(Payload::text("text"), None).await;
    assert_eq!(
        comp.offers.lock().len(),
        1,
        "over an image, text is written"
    );
    let (h, comp) = with_baseline(None);
    h.write(Payload::text("text"), None).await;
    assert_eq!(
        comp.offers.lock().len(),
        1,
        "with nothing read yet, text is written"
    );
}

// --- png ------------------------------------------------------------------------------------

fn slow_png(mime: &str, data: &[u8]) -> Option<Vec<u8>> {
    std::thread::sleep(Duration::from_millis(800));
    png::png_beside(mime, data)
}

/// A handler whose PNG conversion takes `converter`.
fn png_handler(converter: super::PngConverter) -> (Arc<Clipboard>, Arc<FakeCompositor>) {
    let comp = FakeCompositor::new();
    let mut h = Arc::try_unwrap(wayland_handler(controller(), comp.clone(), true))
        .ok()
        .expect("fresh handler");
    h.png_converter = converter;
    (Arc::new(h), comp)
}

#[test]
fn a_jpeg_converts_to_a_png_of_the_same_pixels() {
    let photo = jpeg(40, 30);
    let converted = png::png_beside("image/jpeg", &photo).expect("converted");
    let decoded = image::load_from_memory(&converted).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (40, 30));
    assert!(
        png::png_beside("image/png", &converted).is_none(),
        "a PNG gets none"
    );
    assert!(
        png::png_beside("text/plain", b"x").is_none(),
        "text gets none"
    );
    assert!(
        png::png_beside("image/webp", b"not an image").is_none(),
        "undecodable bytes get none"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_clients_picture_is_offered_as_png_beside_it() {
    let photo = jpeg(40, 30);
    let (h, comp) = png_handler(png::png_beside);
    h.write(Payload::binary("image/jpeg", photo.clone()), None)
        .await;
    assert_eq!(
        comp.offered_mimes(),
        vec![vec!["image/png", "image/jpeg"]],
        "a quick conversion is offered once, PNG first"
    );

    let (h, comp) = png_handler(slow_png);
    let start = Instant::now();
    h.write(Payload::binary("image/jpeg", photo.clone()), None)
        .await;
    let took = start.elapsed();
    assert!(
        took < PNG_WAIT + Duration::from_millis(200),
        "the write waits no longer than the bound: {took:?}"
    );
    assert_eq!(
        comp.offered_mimes(),
        vec![vec!["image/jpeg"]],
        "a slow conversion is offered as it came"
    );
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(
        comp.offered_mimes(),
        vec![vec!["image/jpeg"], vec!["image/png", "image/jpeg"]],
        "its PNG joins it once ready"
    );

    for (name, modifier, key) in [
        ("Ctrl+V", 0xffe3u32, 0x76u32),
        ("Shift+Insert", 0xffe1, 0xff63),
    ] {
        let (h, comp) = png_handler(slow_png);
        h.write(Payload::binary("image/jpeg", photo.clone()), None)
            .await;
        let start = Instant::now();
        h.paste_waits_for_png(key, |_| false).await;
        let bare = start.elapsed();
        let start = Instant::now();
        h.paste_waits_for_png(0x61, |m| m == modifier).await;
        let other = start.elapsed();
        let start = Instant::now();
        h.paste_waits_for_png(key, |m| m == modifier).await;
        let waited = start.elapsed();
        assert!(
            bare < Duration::from_millis(50),
            "{name}: the chord's key with no modifier does not wait: {bare:?}"
        );
        assert!(
            other < Duration::from_millis(50),
            "{name}: another key does not wait: {other:?}"
        );
        assert!(
            waited > Duration::from_millis(200),
            "{name}: the chord waits until the PNG is offered: {waited:?}"
        );
        assert_eq!(
            comp.offered_mimes().last().unwrap(),
            &vec!["image/png", "image/jpeg"]
        );
        let start = Instant::now();
        h.paste_waits_for_png(key, |m| m == modifier).await;
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "once the PNG is offered a paste does not wait"
        );
    }

    let (h, comp) = png_handler(slow_png);
    h.write(Payload::binary("image/jpeg", photo), None).await;
    h.write(Payload::text("copied since"), None).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(
        comp.offered_mimes(),
        vec![vec!["image/jpeg"], vec!["text/plain"]],
        "a copy made while the PNG was being made keeps the clipboard"
    );
}

// --- ladder ---------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_xfixes_rung_backs_off_and_reprobes() {
    let _serial = SERIAL.lock().await;
    record_logs();
    clear_logs();
    let h = Clipboard::new(
        controller(),
        FakeSession::x11(Some(":7777")),
        None,
        policy("true", true),
    );
    assert!(h.ensure_x11_monitor().await.is_none());
    assert!(h.ensure_x11_monitor().await.is_none());
    let unavailable = lines_with("XFixes monitor unavailable");
    assert_eq!(
        unavailable.len(),
        1,
        "a failed build backs off instead of retrying per call: {unavailable:?}"
    );
    assert_eq!(unavailable[0].0, Level::Info, "first failure logs at info");
    *h.x11.retry_at.lock() = None;
    assert!(h.ensure_x11_monitor().await.is_none());
    let unavailable = lines_with("XFixes monitor unavailable");
    assert_eq!(
        unavailable.len(),
        2,
        "cooldown expiry re-probes the top rung"
    );
    assert_eq!(
        unavailable[1].0,
        Level::Debug,
        "repeat failures drop to debug"
    );

    clear_logs();
    let session = FakeSession::wayland(false, "wayland-1");
    let hw = Clipboard::new(
        controller(),
        session.clone(),
        Some(FakeCompositor::new()),
        policy("true", true),
    );
    assert!(hw.ensure_x11_monitor().await.is_none());
    assert!(
        lines_with("XFixes monitor unavailable").is_empty(),
        "wayland backend without an unbridged X server builds no X11 monitor"
    );
    assert!(
        hw.x11.retry_at.lock().is_none(),
        "and does not back off (the X server may appear later)"
    );
    *session.x11_session_display.lock() = Some(":81".into());
    assert!(hw.ensure_x11_monitor().await.is_none());
    let on_display = lines_with("XFixes monitor unavailable on :81");
    assert_eq!(
        on_display.len(),
        1,
        "wayland backend with a rootful Xwayland builds the X11 monitor on its display"
    );
    assert!(
        hw.x11.retry_at.lock().is_some(),
        "a failed build on that display backs off like on X11"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_xclip_warns_once() {
    let _serial = SERIAL.lock().await;
    record_logs();
    clear_logs();
    let h = Clipboard::new(
        controller(),
        FakeSession::x11(Some(":7777")),
        None,
        policy("true", true),
    );
    *h.x11.retry_at.lock() = Some(Instant::now() + Duration::from_secs(3600));
    *h.xclip.program.lock() = "selkies-no-such-xclip-binary".into();
    assert!(h.read(false).await.is_none());
    assert!(
        h.read(false).await.is_none(),
        "missing xclip reads return empty"
    );
    assert_eq!(
        lines_with("xclip is not installed").len(),
        1,
        "missing xclip warns once across polls"
    );
    assert!(
        !h.write(Payload::text("text"), None).await,
        "missing xclip write fails quietly"
    );
    assert_eq!(
        lines_with("xclip is not installed").len(),
        1,
        "under the same warning"
    );
    assert!(
        lines_with("Error reading clipboard with xclip").is_empty(),
        "no traceback spam for the missing binary"
    );
}

// --- secret ---------------------------------------------------------------------------------

/// The X11 monitor's read, its conversions answered by a table of what the owner offers.
fn x11_table_read(offer: &[(&str, &[u8])], use_binary: bool) -> Option<Payload> {
    let names = [
        "TARGETS",
        "UTF8_STRING",
        "text/plain;charset=utf-8",
        "STRING",
        "text/html",
        "text/uri-list",
        "image/png",
        HINTS[0],
        HINTS[1],
        HINTS[2],
    ];
    let atom_of = |name: &str| {
        names
            .iter()
            .position(|n| *n == name)
            .map(|i| i as u32 + 100)
            .expect(name)
    };
    let atoms = Atoms {
        clipboard: 1,
        primary: 2,
        targets: atom_of("TARGETS"),
        incr: 3,
        multiple: 4,
        atom: 5,
        html: atom_of("text/html"),
        uri_list: atom_of("text/uri-list"),
        images: vec![(atom_of("image/png"), "image/png")],
        texts: vec![
            (atom_of("UTF8_STRING"), "UTF8_STRING"),
            (
                atom_of("text/plain;charset=utf-8"),
                "text/plain;charset=utf-8",
            ),
            (atom_of("STRING"), "STRING"),
        ],
        text_aliases: vec![],
        secret_hints: HINTS.iter().map(|h| atom_of(h)).collect(),
        props: vec![],
    };
    let mut convert = |atom: u32| -> Option<Reply> {
        if atom == atoms.targets {
            let mut list = vec![atoms.targets];
            list.extend(offer.iter().map(|(n, _)| atom_of(n)));
            return Some(Reply::Atoms(list));
        }
        let name = names[(atom - 100) as usize];
        offer
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, d)| Reply::Bytes(d.to_vec()))
    };
    read_selection(&atoms, &mut convert, use_binary)
}

#[test]
fn x11_reads_a_password_managers_copy_as_a_secret() {
    for hint in HINTS {
        let p = x11_table_read(
            &[("UTF8_STRING", PASSWORD.as_bytes()), (hint, b"secret")],
            true,
        )
        .unwrap();
        assert!(
            is_secret(&p, PASSWORD),
            "x11: a copy offering {hint} reads as its text, marked secret"
        );
    }
    let p = x11_table_read(
        &[
            ("UTF8_STRING", PASSWORD.as_bytes()),
            ("image/png", b"\x89PNG"),
            ("text/html", b"<b>x</b>"),
            (HINTS[0], b"secret"),
        ],
        true,
    )
    .unwrap();
    assert!(
        is_secret(&p, PASSWORD),
        "a secret copy reads as its text alone, whatever else it offers"
    );
    let p = x11_table_read(&[("UTF8_STRING", b""), (HINTS[0], b"secret")], false).unwrap();
    assert!(is_secret(&p, ""), "an empty secret reads as an empty one");
    let p = x11_table_read(&[("UTF8_STRING", b"hello"), (HINTS[0], b"public")], false).unwrap();
    assert!(
        !p.secret && p.text_lossy() == "hello",
        "a hint valued anything but secret leaves the text ordinary"
    );
    let p = x11_table_read(&[("UTF8_STRING", b"hello")], false).unwrap();
    assert!(
        !p.secret && p.text_lossy() == "hello",
        "an ordinary copy reads as before"
    );
}

#[test]
fn the_compositors_flavors_read_a_secret_as_its_text() {
    for hint in HINTS {
        let p = native_payload(
            &text_entries(&[
                ("text/plain;charset=utf-8", PASSWORD.as_bytes()),
                (hint, b"secret"),
            ]),
            true,
        )
        .unwrap();
        assert!(
            is_secret(&p, PASSWORD),
            "compositor: a copy offering {hint} reads as its text, marked secret"
        );
    }
    let p = native_payload(&text_entries(&[(HINTS[1], b"secret")]), true).unwrap();
    assert!(
        is_secret(&p, ""),
        "a secret whose text is empty reads as an empty one"
    );
    let p = native_payload(
        &text_entries(&[("text/plain", b"hello"), (HINTS[0], b"no")]),
        true,
    )
    .unwrap();
    assert!(
        !p.secret && p.text_lossy() == "hello",
        "a hint valued anything but secret leaves the text ordinary"
    );
    assert_eq!(
        native_payload(&[], true),
        None,
        "a cleared selection reads as nothing"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn data_control_reads_a_secret_as_its_text_alone() {
    for hint in HINTS {
        let comp = FakeCompositor::new().with_app(&[
            ("image/png", b"\x89PNG"),
            ("text/plain", PASSWORD.as_bytes()),
            (hint, b"secret"),
        ]);
        let h = nested_handler(controller(), comp.clone(), true);
        let p = h.app_clipboard_read(true).await.unwrap();
        assert!(
            is_secret(&p, PASSWORD),
            "data-control: a copy offering {hint} reads as its text alone, marked secret"
        );
        assert!(
            !comp.app_reads.lock().contains(&"image/png".to_string()),
            "the picture beside it is never read"
        );
    }
    let comp = FakeCompositor::new().with_app(&[("text/plain", b"hello")]);
    let h = nested_handler(controller(), comp.clone(), true);
    let p = h.app_clipboard_read(true).await.unwrap();
    assert!(!p.secret && p.text_lossy() == "hello");
    assert_eq!(
        *comp.app_reads.lock(),
        vec!["text/plain"],
        "an ordinary copy costs no hint read"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_monitor_retracts_a_secret_the_session_let_go() {
    let _serial = SERIAL.lock().await;
    record_logs();
    let host = controller();
    let comp = FakeCompositor::new();
    let h = wayland_handler(host.clone(), comp.clone(), true);
    let task = tokio::spawn(h.clone().run_monitor());
    tokio::time::sleep(Duration::from_millis(300)).await;
    comp.copy(text_entries(&[
        ("text/plain", PASSWORD.as_bytes()),
        (HINTS[0], b"secret"),
    ]));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let sent = host.payloads(1);
    assert!(
        sent.len() == 1 && is_secret(&sent[0], PASSWORD),
        "a secret copy reaches the clients marked: {sent:?}"
    );
    comp.copy(vec![]);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let sent = host.payloads(1);
    assert!(
        sent.len() == 2 && is_secret(&sent[1], ""),
        "the selection cleared after it goes out as an empty secret: {sent:?}"
    );
    comp.copy(text_entries(&[("text/plain", b"ordinary")]));
    tokio::time::sleep(Duration::from_millis(300)).await;
    comp.copy(vec![]);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let sent = host.payloads(1);
    assert_eq!(
        &sent[2..],
        &[Payload::text("ordinary")],
        "a selection cleared after ordinary text sends nothing"
    );
    comp.copy(text_entries(&[
        ("text/plain", PASSWORD.as_bytes()),
        (HINTS[2], b"secret"),
    ]));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let sent = host.payloads(1);
    assert!(
        sent.len() == 4 && is_secret(&sent[3], PASSWORD),
        "the same secret copied again after its retraction goes out again"
    );
    stop_monitor(&h, task).await;

    let comp = FakeCompositor::new();
    let h = wayland_handler(controller(), comp.clone(), true);
    h.set_clipboard(Payload::secret(PASSWORD), None, false)
        .await;
    let offered: HashMap<String, Vec<u8>> = comp.offers.lock()[0].0.iter().cloned().collect();
    assert_eq!(
        offered.get("text/plain").map(Vec::as_slice),
        Some(PASSWORD.as_bytes())
    );
    assert!(
        HINTS
            .iter()
            .all(|h| offered.get(*h).map(Vec::as_slice) == Some(b"secret")),
        "write-back: a secret is offered with the hint in every spelling"
    );
    h.set_clipboard(Payload::text("ordinary"), None, false)
        .await;
    assert_eq!(
        comp.offered_mimes()[1],
        vec!["text/plain"],
        "ordinary text is offered without it"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_secret_is_marked_ahead_of_its_frames_and_never_announced_to_a_viewer() {
    let host = controller();
    let h = wayland_handler(host.clone(), FakeCompositor::new(), true);
    h.send(Payload::secret(PASSWORD), None, None).await;
    assert_eq!(
        host.verbs(1),
        vec!["clipboard_secret", "clipboard"],
        "a secret's payload follows a clipboard_secret frame"
    );
    host.clear();
    h.send(Payload::secret(""), None, None).await;
    assert_eq!(
        host.frames(1),
        vec!["clipboard_secret", "clipboard,"],
        "an empty secret goes out, marked"
    );
    host.clear();
    h.send(Payload::secret("x".repeat(40000)), None, None).await;
    assert_eq!(
        &host.verbs(1)[..2],
        &["clipboard_secret", "clipboard_start"],
        "a secret too large for one message is marked ahead of its first frame"
    );
    host.clear();
    h.send(Payload::text("ordinary"), None, None).await;
    assert_eq!(
        host.verbs(1),
        vec!["clipboard"],
        "ordinary text carries no mark"
    );
    host.clear();
    host.clients.lock().push((2, Role::Viewer));
    h.send(Payload::secret(PASSWORD), None, None).await;
    h.send(Payload::text("ordinary"), None, None).await;
    h.send(Payload::text("asked for"), Some("cr"), Some(2))
        .await;
    assert_eq!(host.frames(1).len(), 3);
    assert_eq!(
        host.verbs(2),
        vec!["clipboard_reply", "clipboard"],
        "a viewer is announced nothing, a secret or not, and answered what it asked"
    );
}

// --- supersede ------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_newer_copy_supersedes_one_still_crossing_the_link() {
    let host = FakeHost::slow(&[(1, Role::Controller)]);
    let h = wayland_handler(host.clone(), FakeCompositor::new(), true);
    let big: Vec<u8> = (0..=255u8).cycle().take(256 * 800).collect();
    let chunks = big.len().div_ceil(CHUNK_SIZE);

    let older = tokio::spawn({
        let (h, big) = (h.clone(), big.clone());
        async move { h.send(Payload::binary("image/png", big), None, None).await }
    });
    tokio::time::sleep(Duration::from_millis(5)).await;
    h.send(Payload::binary("image/png", &b"newer"[..]), None, None)
        .await;
    older.await.unwrap();
    let verbs = host.verbs(1);
    assert_eq!(verbs[0], "clipboard_start");
    assert_eq!(
        verbs.last().unwrap(),
        "clipboard_binary",
        "a newer copy goes out at once"
    );
    assert!(
        !verbs.contains(&"clipboard_finish".to_string()),
        "the older one is never finished"
    );
    assert!(
        verbs.iter().filter(|v| *v == "clipboard_data").count() < chunks,
        "and gets no further chunks: {verbs:?}"
    );

    host.clear();
    let reply = tokio::spawn({
        let (h, big) = (h.clone(), big.clone());
        async move {
            h.send(Payload::binary("image/png", big), Some("cr"), None)
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(5)).await;
    h.send(Payload::binary("image/png", &b"newer"[..]), None, None)
        .await;
    reply.await.unwrap();
    let verbs = host.verbs(1);
    assert_eq!(&verbs[..2], &["clipboard_reply", "clipboard_start"]);
    assert_eq!(
        verbs.iter().filter(|v| *v == "clipboard_data").count(),
        chunks,
        "a tagged reply completes"
    );
    assert_eq!(
        &verbs[verbs.len() - 2..],
        &["clipboard_finish", "clipboard_binary"],
        "before the copy that followed it"
    );

    host.clear();
    host.clients.lock().push((2, Role::Viewer));
    h.send(Payload::text("a copy"), None, None).await;
    h.send(Payload::text("asked for"), Some("cr"), Some(2))
        .await;
    assert_eq!(host.verbs(1), vec!["clipboard"]);
    assert_eq!(
        host.verbs(2),
        vec!["clipboard_reply", "clipboard"],
        "a viewer is announced no copy, and is answered what it asked"
    );
}

// --- rearm ----------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_compositor_sink_is_armed_once_per_monitor_start() {
    let _serial = SERIAL.lock().await;
    record_logs();
    clear_logs();
    let host = controller();
    let comp = FakeCompositor::new();
    let h = wayland_handler(host.clone(), comp.clone(), true);
    let task = tokio::spawn(h.clone().run_monitor());
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        comp.arms.load(Ordering::Relaxed),
        1,
        "the compositor callback is armed at monitor start"
    );
    assert!(comp.sink.lock().is_some());
    assert_eq!(
        lines_with("native compositor callback active").len(),
        1,
        "arming is said once"
    );
    comp.copy(text_entries(&[("text/plain", b"one")]));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        host.payloads(1),
        vec![Payload::text("one")],
        "a copy reaches the clients"
    );
    // Two full idle timeouts of the loop.
    tokio::time::sleep(Duration::from_millis(4600)).await;
    assert_eq!(
        comp.arms.load(Ordering::Relaxed),
        1,
        "idle ticks do not re-register the callback"
    );
    assert_eq!(
        comp.reads.load(Ordering::Relaxed),
        1,
        "idle ticks do not re-read the selection"
    );
    assert_eq!(
        host.payloads(1),
        vec![Payload::text("one")],
        "idle ticks do not re-send"
    );

    comp.deliver_current();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        host.payloads(1),
        vec![Payload::text("one"), Payload::text("one")],
        "the same selection handed over again reaches the clients"
    );
    let image: Vec<u8> = (0..=255u8).cycle().take(256 * 3 * 1024 * 4).collect();
    comp.copy(text_entries(&[("image/png", &image)]));
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        &host.payloads(1)[2..],
        &[Payload::binary("image/png", image.clone())],
        "a large image is sent once"
    );
    comp.deliver_current();
    tokio::time::sleep(Duration::from_millis(2600)).await;
    assert!(
        host.payloads(1).len() == 4 && comp.arms.load(Ordering::Relaxed) == 1,
        "a re-copied image reaches the clients without a re-arm"
    );
    let reads_before = comp.reads.load(Ordering::Relaxed);
    let read = h.read(true).await.unwrap();
    assert!(
        read == Payload::binary("image/png", image.clone())
            && comp.reads.load(Ordering::Relaxed) == reads_before,
        "an on-demand read is served from the delivered cache, without a compositor read"
    );

    stop_monitor(&h, task).await;
    assert!(!h.monitor_active(), "monitor loop stops");
    let task = tokio::spawn(h.clone().run_monitor());
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        comp.arms.load(Ordering::Relaxed),
        2,
        "a restarted monitor registers once more"
    );
    assert_eq!(
        host.payloads(1).len(),
        4,
        "the re-staged read of the unchanged selection is not re-sent"
    );
    stop_monitor(&h, task).await;

    // The backend not up yet: retried per tick, said once.
    clear_logs();
    let host = controller();
    let comp = FakeCompositor::new();
    *comp.arm_error.lock() = Some("wayland backend not running".into());
    let h = wayland_handler(host.clone(), comp.clone(), true);
    let task = tokio::spawn(h.clone().run_monitor());
    tokio::time::sleep(Duration::from_millis(1700)).await;
    let warned = lines_with("native callback failed to arm");
    assert!(
        comp.arms.load(Ordering::Relaxed) >= 2,
        "a failed arm is retried per tick"
    );
    assert!(
        warned.len() == 1 && warned[0].0 == Level::Warn,
        "and reported once: {warned:?}"
    );
    *comp.arm_error.lock() = None;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let armed = comp.sink.lock().is_some();
    let arms_after = comp.arms.load(Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(2600)).await;
    assert!(
        armed && comp.arms.load(Ordering::Relaxed) == arms_after,
        "the arm holds once the backend answers, with no further registration"
    );
    comp.copy(text_entries(&[("text/plain", b"two")]));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        host.payloads(1),
        vec![Payload::text("two")],
        "copies flow after the late arm"
    );
    let rich = text_entries(&[("text/html", b"<b>rich</b>"), ("text/plain", b"rich")]);
    comp.copy(text_entries(&[
        ("text/html", b"<b>rich</b>"),
        ("text/plain;charset=utf-8", b"rich"),
    ]));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        &host.payloads(1)[1..],
        &[Payload::envelope(&rich)],
        "a rich copy reaches the clients as one envelope of its flavors"
    );
    assert_eq!(
        h.read(false).await,
        Some(Payload::envelope(&rich)),
        "and is read on demand as the same envelope"
    );
    stop_monitor(&h, task).await;
}

// --- app poll -------------------------------------------------------------------------------

const NO_DC: &str = "app compositor advertises neither ext_data_control_manager_v1 nor zwlr_data_control_manager_v1";

/// A nested session whose app compositor serves text, with the binary clipboard off.
fn nested_text(host: Arc<FakeHost>) -> (Arc<Clipboard>, Arc<FakeCompositor>) {
    let comp = FakeCompositor::new()
        .with_app(&[("text/plain;charset=utf-8", b"one"), ("text/plain", b"one")]);
    (nested_handler(host, comp.clone(), false), comp)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_nested_compositor_without_data_control_is_polled_and_said_once() {
    let _serial = SERIAL.lock().await;
    record_logs();
    clear_logs();
    let (h, comp) = nested_text(controller());
    *comp.dc_error.lock() = Some(NO_DC.into());
    assert!(h.arm_app_compositor_watch().await.is_none());
    assert!(h.arm_app_compositor_watch().await.is_none());
    assert!(
        comp.watches.lock().is_empty() && comp.watch_attempts.lock().is_empty(),
        "no data-control: no watch armed"
    );
    let warned = lines_with("no selection watch on app compositor");
    assert!(
        warned.len() == 1 && warned[0].0 == Level::Warn && warned[0].1.contains("polling"),
        "the failure is reported once, at arm time, naming the fallback: {warned:?}"
    );

    assert!(h.app_clipboard_read(false).await.is_none());
    assert!(
        h.app_clipboard_read(false).await.is_none(),
        "reads return empty"
    );
    let levels: Vec<Level> = lines_with("data-control clipboard read failed")
        .iter()
        .map(|(l, _)| *l)
        .collect();
    assert_eq!(
        levels,
        vec![Level::Warn, Level::Debug],
        "read failure warns once, then debug"
    );
    *comp.dc_error.lock() = None;
    h.app_clipboard_read(false).await;
    *comp.dc_error.lock() = Some(NO_DC.into());
    h.app_clipboard_read(false).await;
    let levels: Vec<Level> = lines_with("data-control clipboard read failed")
        .iter()
        .map(|(l, _)| *l)
        .collect();
    assert_eq!(
        levels.last(),
        Some(&Level::Warn),
        "a failure returning after a success warns again"
    );

    let (h, comp) = nested_text(controller());
    let queue = h
        .arm_app_compositor_watch()
        .await
        .expect("data-control present: watch armed");
    assert_eq!(comp.watches.lock().len(), 1);
    let watch = comp.watches.lock()[0].1.clone();
    std::thread::spawn(move || watch.mimes(vec!["text/plain".into()]));
    let got = queue.recv(Duration::from_secs(1)).await;
    assert_eq!(
        got,
        Some(vec!["text/plain".to_string()]),
        "watch callback from a foreign thread reaches the loop queue"
    );

    clear_logs();
    let (h, comp) = nested_text(controller());
    *comp.watch_error.lock() = Some("spawn: resource temporarily unavailable".into());
    assert!(
        h.arm_app_compositor_watch().await.is_none(),
        "a failed arm returns no watch"
    );
    h.arm_app_compositor_watch().await;
    assert_eq!(
        lines_with("resource temporarily unavailable").len(),
        1,
        "and is reported once"
    );

    let host = controller();
    let (h, comp) = nested_text(host.clone());
    *comp.watch_error.lock() = Some("spawn: resource temporarily unavailable".into());
    let task = tokio::spawn(h.clone().run_monitor());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        host.payloads(1),
        vec![Payload::text("one")],
        "first pass publishes the current selection"
    );
    comp.with_app(&[("text/plain;charset=utf-8", b"two"), ("text/plain", b"two")]);
    tokio::time::sleep(Duration::from_millis(2400)).await;
    assert!(
        host.payloads(1).contains(&Payload::text("two")),
        "poll rung picks up a change within a tick"
    );
    assert!(
        comp.watch_attempts.lock().len() >= 2,
        "the arm is retried per tick"
    );
    let sent_before = host.payloads(1).len();
    *comp.watch_error.lock() = None;
    tokio::time::sleep(Duration::from_millis(2400)).await;
    assert_eq!(
        comp.watches.lock().len(),
        1,
        "the watch takes over once it arms"
    );
    assert_eq!(
        host.payloads(1).len(),
        sent_before,
        "unchanged content is not re-sent while polling"
    );
    stop_monitor(&h, task).await;
    assert!(!h.monitor_active(), "monitor loop stops");

    let word: &[u8] = b"<html><head><style>p{margin:0}</style></head><body><!--StartFragment--><p><b>Quarterly</b> report</p><!--EndFragment--></body></html>";
    let comp = FakeCompositor::new().with_app(&[
        ("text/html", word),
        ("text/plain", b"Quarterly report"),
        ("image/png", PNG),
    ]);
    let h = nested_handler(controller(), comp, true);
    let p = h.app_clipboard_read(true).await.unwrap();
    assert_eq!(
        p.mime, FLAVORS_MIME,
        "data-control: formatted text with its picture reads as the text"
    );
    assert_eq!(
        flavors(&p.bytes).unwrap(),
        text_entries(&[("text/html", word), ("text/plain", b"Quarterly report")])
    );
    let comp = FakeCompositor::new().with_app(&[
        ("text/html", b"<img src=\"https://example.org/a.png\">"),
        ("image/png", PNG),
    ]);
    let h = nested_handler(controller(), comp, true);
    assert_eq!(
        h.app_clipboard_read(true).await,
        Some(Payload::binary("image/png", PNG)),
        "a copied picture reads as the picture"
    );
}

// --- typing ---------------------------------------------------------------------------------

/// A nested session whose app selection holds `clipboard` before the paste.
fn typing_handler(
    clipboard: Option<(&str, &[u8])>,
    separate: bool,
) -> (Arc<Clipboard>, Arc<FakeCompositor>) {
    let comp = FakeCompositor::new();
    if let Some((mime, data)) = clipboard {
        comp.with_app(&[(mime, data)]);
    }
    let session = FakeSession::wayland(separate, "wayland-app");
    (
        Clipboard::new(
            controller(),
            session,
            Some(comp.clone()),
            policy("true", true),
        ),
        comp,
    )
}

/// The texts `clipboard_write_app` offered, per call, by their `text/plain` flavor or mime.
fn written(comp: &FakeCompositor) -> Vec<(Vec<u8>, bool)> {
    comp.app_writes
        .lock()
        .iter()
        .map(|(entries, _)| {
            let secret = entries
                .iter()
                .any(|(m, d)| HINTS.contains(&m.as_str()) && d == b"secret");
            let data = entries
                .iter()
                .find(|(m, _)| m == "text/plain")
                .or_else(|| entries.first())
                .map(|(_, d)| d.clone())
                .unwrap_or_default();
            (data, secret)
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn text_is_typed_through_a_secret_paste_and_the_clipboard_restored() {
    let (h, comp) = typing_handler(Some(("text/plain", b"before")), true);
    let keys = FakeKeys::new(&[], None);
    assert!(h.inject_text_via_clipboard("한국어", &keys).await);
    let writes = written(&comp);
    assert_eq!(
        writes[0],
        ("한국어".as_bytes().to_vec(), true),
        "the text is written once, offered as a secret"
    );
    assert_eq!(
        keys.chord_of(),
        PASTE_CHORD,
        "paste chord is Shift+Insert in order"
    );
    assert_eq!(
        writes.last().unwrap(),
        &(b"before".to_vec(), false),
        "previous clipboard is restored after the paste, not as a secret"
    );
    let restored_at = comp.app_writes.lock().last().unwrap().1;
    let fetch = restored_at.saturating_duration_since(keys.time_of((SHIFT_L, false)));
    assert!(
        fetch >= PASTE_FETCH - Duration::from_millis(5),
        "the clipboard is restored only once the focused app has had time to fetch: {fetch:?}"
    );

    let (h, comp) = typing_handler(None, true);
    let keys = FakeKeys::new(&[], None);
    assert!(h.inject_text_via_clipboard("組み合わせ", &keys).await);
    assert_eq!(
        *comp.cleared.lock(),
        vec!["wayland-app"],
        "an empty clipboard is cleared, not repopulated"
    );
    assert_eq!(written(&comp).len(), 1);

    let (h, _comp) = typing_handler(Some(("text/plain", b"before")), true);
    let keys = FakeKeys::new(&[CTRL_L], None);
    assert!(h.inject_text_via_clipboard("ㅎ", &keys).await);
    let sent = keys.keys.lock().clone();
    assert_eq!(
        sent[0],
        (CTRL_L, false),
        "held modifier is lifted before the chord"
    );
    assert_eq!(*sent.last().unwrap(), (CTRL_L, true), "and restored after");
    assert_eq!(keys.chord_of(), PASTE_CHORD);

    let (h, comp) = typing_handler(Some(("image/png", b"\x89PNG")), true);
    let keys = FakeKeys::new(&[], None);
    assert!(h.inject_text_via_clipboard("text", &keys).await);
    let last = comp.app_writes.lock().last().unwrap().0.clone();
    assert_eq!(
        last,
        text_entries(&[("image/png", b"\x89PNG")]),
        "binary clipboard is restored under its own mime"
    );

    let (h, comp) = typing_handler(Some(("text/plain", b"before")), true);
    *comp.app_write_ok.lock() = false;
    let keys = FakeKeys::new(&[], None);
    assert!(!h.inject_text_via_clipboard("text", &keys).await);
    assert!(
        keys.chord_of().is_empty(),
        "failed clipboard write sends no chord"
    );

    let (h, comp) = typing_handler(Some(("text/plain", b"before")), true);
    h.inject_active.store(true, Ordering::Relaxed);
    let keys = FakeKeys::new(&[], None);
    assert!(
        !h.inject_text_via_clipboard("text", &keys).await,
        "reentry is refused while an injection is running"
    );
    assert!(comp.app_writes.lock().is_empty());

    let (h, comp) = typing_handler(Some(("text/plain", b"before")), false);
    let keys = FakeKeys::new(&[], None);
    assert!(h.inject_text_via_clipboard("direct", &keys).await);
    let offered: Vec<Vec<u8>> = comp
        .offers
        .lock()
        .iter()
        .map(|(e, _)| e[0].1.clone())
        .collect();
    assert_eq!(
        offered,
        vec![b"direct".to_vec(), b"before".to_vec()],
        "non-nested topology injects and restores the same way"
    );
}

/// Seconds from the paste chord to the restore, against a selection reporting `fetches` (and
/// `early` reads before the chord).
async fn restored_after(fetches: &[(u64, &str)], early: &[&str]) -> Duration {
    let (h, comp) = typing_handler(Some(("text/plain", b"before")), true);
    *comp.fetches.lock() = fetches
        .iter()
        .map(|(ms, m)| (Duration::from_millis(*ms), m.to_string()))
        .collect();
    *comp.served.lock() = early.iter().map(|m| m.to_string()).collect();
    let keys = FakeKeys::new(&[], Some(comp.clone()));
    assert!(h.inject_text_via_clipboard("느린", &keys).await);
    let writes = comp.app_writes.lock();
    assert_eq!(
        writes.last().unwrap().0,
        text_entries(&[
            ("text/plain;charset=utf-8", b"before"),
            ("text/plain", b"before"),
            ("UTF8_STRING", b"before"),
            ("STRING", b"before"),
            ("TEXT", b"before")
        ])
    );
    writes
        .last()
        .unwrap()
        .1
        .saturating_duration_since(keys.time_of((INSERT, true)))
}

#[tokio::test(flavor = "multi_thread")]
async fn the_restore_follows_the_paste_fetch_where_the_selection_reports_it() {
    let text = "text/plain;charset=utf-8";
    let took = restored_after(&[(300, text)], &[]).await;
    assert!(
        took >= Duration::from_millis(300) && took > PASTE_FETCH,
        "a fetch slower than the fixed margin is waited for: {took:?}"
    );
    let took = restored_after(&[(30, text)], &[]).await;
    assert!(
        took >= Duration::from_millis(30) && took < PASTE_FETCH - Duration::from_millis(50),
        "a quick fetch is followed at once, sooner than the fixed margin: {took:?}"
    );
    let took = restored_after(
        &[(20, "application/x-kde-passwordManagerHint"), (250, text)],
        &[],
    )
    .await;
    assert!(
        took >= Duration::from_millis(250),
        "a clipboard history's read of the secret hint is not the paste: {took:?}"
    );
    let took = restored_after(&[(200, text)], &[text]).await;
    assert!(
        took >= Duration::from_millis(200),
        "a read before the chord is not the paste: {took:?}"
    );
    let took = restored_after(&[], &[]).await;
    assert!(
        took >= PASTE_FETCH_MAX - Duration::from_millis(10)
            && took < PASTE_FETCH_MAX + Duration::from_millis(150),
        "an app that never fetches holds the clipboard no longer than the most it waits: {took:?}"
    );
}

// --- defects the suites do not reach --------------------------------------------------------

#[test]
fn markup_of_any_text_is_read_without_a_panic() {
    assert!(
        markup_has_text("한국어".as_bytes()),
        "text starting with a multibyte character"
    );
    assert!(markup_has_text("<p>한국어</p>".as_bytes()));
    assert!(
        !markup_has_text("<a é".as_bytes()),
        "a tag cut off inside a multibyte character"
    );
    assert!(markup_has_text("é<".as_bytes()));
    assert!(markup_has_text("&é".as_bytes()));
    assert!(
        markup_has_text("1 < 2".as_bytes()),
        "a bare less-than is text"
    );
    assert!(
        !markup_has_text(br#"<img alt="a>b" src="x.png">"#),
        "a closing bracket in a quoted attribute does not end the tag"
    );
    assert!(
        !markup_has_text(b"<STYLE>b{}</sTyLe><SCRIPT>x()</script>"),
        "element names ignore case"
    );
}

#[test]
fn empty_flavors_stay_out_of_the_envelope() {
    let entries = text_entries(&[("text/html", b"<b>x</b>"), ("text/plain", b"")]);
    let payload = native_payload(&entries, true).unwrap();
    assert_eq!(
        flavors(&payload.bytes).unwrap(),
        text_entries(&[("text/html", b"<b>x</b>")])
    );
    let raw: serde_json::Value = serde_json::from_slice(&payload.bytes).unwrap();
    assert!(
        raw.get("text/plain").is_none(),
        "an empty plain flavor is left out: {raw}"
    );
    let entries = text_entries(&[("text/html", b""), ("text/plain", b"plain")]);
    assert_eq!(
        native_payload(&entries, true),
        Some(Payload::text("plain")),
        "empty markup is no markup"
    );
}

/// Wait until the fake's watch is armed.
async fn watch_armed(
    comp: &FakeCompositor,
    count: usize,
) -> Arc<dyn pixelflux::sink::SelectionWatch> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while comp.watches.lock().len() < count {
        assert!(Instant::now() < deadline, "the watch never armed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    comp.watches.lock()[count - 1].1.clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_clients_rich_copy_read_back_is_not_broadcast() {
    let _serial = SERIAL.lock().await;
    record_logs();
    let host = controller();
    let comp = FakeCompositor::new();
    let h = nested_handler(host.clone(), comp.clone(), true);
    let task = tokio::spawn(h.clone().run_monitor());
    let watch = watch_armed(&comp, 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let rich = text_entries(&[("text/html", b"<b>rich</b>"), ("text/plain", b"rich")]);
    h.on_message(1, &wire(&rich)).await;
    assert_eq!(comp.app_writes.lock().len(), 1);
    comp.with_app(&[("text/html", b"<b>rich</b>"), ("text/plain", b"rich")]);
    watch.mimes(vec!["text/html".into(), "text/plain".into()]);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        host.payloads(1).is_empty(),
        "the write coming back is not a copy: {:?}",
        host.payloads(1)
    );
    watch.mimes(vec!["text/html".into(), "text/plain".into()]);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        host.payloads(1),
        vec![Payload::envelope(&rich)],
        "the next edge carrying those bytes is someone's copy"
    );
    stop_monitor(&h, task).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_writes_edge_is_spent_even_when_its_read_came_back_empty() {
    let _serial = SERIAL.lock().await;
    record_logs();
    let host = controller();
    let comp = FakeCompositor::new();
    let h = nested_handler(host.clone(), comp.clone(), true);
    let task = tokio::spawn(h.clone().run_monitor());
    let watch = watch_armed(&comp, 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.on_message(1, &format!("cw,{}", BASE64.encode(b"same")))
        .await;
    watch.mimes(vec!["text/plain".into()]);
    // The two re-reads an empty edge is given.
    tokio::time::sleep(Duration::from_millis(800)).await;
    comp.with_app(&[("text/plain", b"same")]);
    watch.mimes(vec!["text/plain".into()]);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        host.payloads(1),
        vec![Payload::text("same")],
        "a later edge with those bytes is someone's copy"
    );
    stop_monitor(&h, task).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn typing_with_nothing_to_restore_leaves_no_cached_text() {
    let (h, comp) = typing_handler(None, false);
    let keys = FakeKeys::new(&[], None);
    assert!(h.inject_text_via_clipboard("typed", &keys).await);
    assert_eq!(
        comp.offers.lock().last().unwrap().0,
        text_entries(&[("text/plain", b"")]),
        "the selection is cleared"
    );
    assert_eq!(
        h.read(false).await,
        None,
        "a fetch after the paste does not read the typed text back"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rich_clipboard_is_restored_as_its_flavors() {
    let (h, comp) = typing_handler(None, true);
    comp.with_app(&[("text/html", b"<b>rich</b>"), ("text/plain", b"rich")]);
    let keys = FakeKeys::new(&[], None);
    assert!(h.inject_text_via_clipboard("typed", &keys).await);
    let last = comp.app_writes.lock().last().unwrap().0.clone();
    let mimes: Vec<&str> = last.iter().map(|(m, _)| m.as_str()).collect();
    assert!(
        mimes.contains(&"text/html") && mimes.contains(&"text/plain"),
        "{mimes:?}"
    );
    assert!(
        !mimes.contains(&FLAVORS_MIME),
        "the envelope never reaches a real clipboard: {mimes:?}"
    );
}

#[test]
fn markup_made_of_nothing_but_scripts_is_read_in_linear_time() {
    let markup = "<script></script>".repeat(1_000_000);
    let start = Instant::now();
    assert!(!markup_has_text(markup.as_bytes()));
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_watch_is_armed_again_when_its_compositor_returns_on_the_same_socket() {
    let _serial = SERIAL.lock().await;
    record_logs();
    let comp = FakeCompositor::new();
    let session = FakeSession::wayland(true, "wayland-9");
    let h = Clipboard::new(
        controller(),
        session.clone(),
        Some(comp.clone()),
        policy("true", true),
    );
    let task = tokio::spawn(h.clone().run_monitor());
    watch_armed(&comp, 1).await;
    session.dead.store(true, Ordering::Relaxed);
    watch_armed(&comp, 2).await;
    stop_monitor(&h, task).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reset_abandons_the_transfer_in_progress() {
    let comp = FakeCompositor::new();
    let h = wayland_handler(controller(), comp.clone(), true);
    h.on_message(1, "cws,t1,2").await;
    h.reset_inbound();
    h.on_message(1, &format!("cwd,t1,{}", BASE64.encode(b"ab")))
        .await;
    h.on_message(1, "cwe,t1").await;
    assert!(comp.offers.lock().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_verbs_can_run_on_a_task_of_their_own() {
    let comp = FakeCompositor::new();
    let h = wayland_handler(controller(), comp.clone(), true);
    let task = tokio::spawn(async move {
        h.on_message(1, &format!("cw,{}", BASE64.encode(b"hi")))
            .await
    });
    assert!(task.await.unwrap());
    assert_eq!(comp.offers.lock().len(), 1);
}

// --- a real X server ------------------------------------------------------------------------

/// An Xvfb of this test's own, on a display number the server picked, so nothing here touches
/// the clipboard of a display another suite shares; killed by its pid on drop, which also ends
/// any `xclip` that was serving a selection on it.
struct PrivateX {
    child: std::process::Child,
    display: String,
}

impl PrivateX {
    fn start() -> PrivateX {
        use std::io::BufRead;
        let mut child = std::process::Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "64x64x24",
                "-nolisten",
                "tcp",
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("Xvfb runs");
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().expect("piped"))
            .read_line(&mut line)
            .expect("Xvfb names its display");
        let display = format!(":{}", line.trim());
        PrivateX { child, display }
    }

    /// `xclip` with `args` on this display: its stdout on exit.
    async fn xclip_out(&self, args: &[&str]) -> Vec<u8> {
        tokio::process::Command::new("xclip")
            .args(args)
            .env("DISPLAY", &self.display)
            .stderr(std::process::Stdio::null())
            .output()
            .await
            .expect("xclip runs")
            .stdout
    }

    /// Copy `data` with `xclip -i`, which forks to serve it until another owner takes over.
    async fn xclip_copy(&self, selection: &str, data: &[u8]) {
        let mut child = tokio::process::Command::new("xclip")
            .args(["-selection", selection, "-i"])
            .env("DISPLAY", &self.display)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("xclip runs");
        let mut stdin = child.stdin.take().expect("piped");
        tokio::io::AsyncWriteExt::write_all(&mut stdin, data)
            .await
            .expect("xclip reads");
        drop(stdin);
        child.wait().await.expect("xclip forks and exits");
    }
}

impl Drop for PrivateX {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The monitor on a private server, read and offered through blocking threads.
struct LiveMonitor(Arc<super::X11Monitor>);

impl LiveMonitor {
    async fn offer(&self, entries: Vec<Entry>) -> bool {
        let m = self.0.clone();
        tokio::task::spawn_blocking(move || m.offer(entries))
            .await
            .unwrap()
            .unwrap()
    }

    async fn read(&self, use_binary: bool) -> Option<Payload> {
        let m = self.0.clone();
        tokio::task::spawn_blocking(move || m.read(use_binary))
            .await
            .unwrap()
            .unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Xvfb and xclip"]
async fn x11_monitor_offers_and_reads_against_a_live_server() {
    let x = PrivateX::start();
    let monitor = LiveMonitor(Arc::new(
        super::X11Monitor::new(Some(&x.display)).expect("monitor builds"),
    ));
    assert!(monitor.0.alive());
    assert!(
        monitor
            .offer(text_entries(&[("text/plain", b"from selkies")]))
            .await,
        "ownership was acquired"
    );
    assert!(monitor.0.owns_selection());
    assert_eq!(
        x.xclip_out(&["-selection", "clipboard", "-o"]).await,
        b"from selkies",
        "xclip reads the offer"
    );
    assert_eq!(
        x.xclip_out(&["-selection", "primary", "-o"]).await,
        b"from selkies",
        "and from PRIMARY, for a middle click"
    );

    x.xclip_copy("primary", b"selected").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        monitor.0.owns_selection(),
        "a PRIMARY steal does not take CLIPBOARD"
    );
    assert_eq!(
        x.xclip_out(&["-selection", "clipboard", "-o"]).await,
        b"from selkies",
        "whose payload stays served"
    );

    monitor.0.changed().take();
    x.xclip_copy("clipboard", b"copied in X").await;
    assert!(
        monitor.0.changed().wait(Duration::from_secs(3)).await,
        "an X copy raises the change edge"
    );
    assert!(!monitor.0.owns_selection());
    assert_eq!(
        monitor.read(false).await,
        Some(Payload::text("copied in X")),
        "the copy reads back through the monitor"
    );

    let mut hinted = text_entries(&[("text/plain", b"hunter2")]);
    hinted.push((HINTS[0].to_string(), b"secret".to_vec()));
    assert!(monitor.offer(hinted).await);
    assert_eq!(
        x.xclip_out(&["-selection", "clipboard", "-o", "-t", HINTS[0]])
            .await,
        b"secret",
        "a secret is offered with its hint"
    );
    let rich = text_entries(&[("text/html", b"<b>rich</b>"), ("text/plain", b"rich")]);
    assert!(monitor.offer(rich).await);
    assert_eq!(
        x.xclip_out(&["-selection", "clipboard", "-o", "-t", "text/html"])
            .await,
        b"<b>rich</b>",
        "a rich editor takes the markup"
    );
    assert_eq!(
        x.xclip_out(&["-selection", "clipboard", "-o", "-t", "UTF8_STRING"])
            .await,
        b"rich",
        "a plain field the text"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Xvfb and xclip"]
async fn x11_monitor_carries_large_payloads_both_ways() {
    let x = PrivateX::start();
    let monitor = LiveMonitor(Arc::new(
        super::X11Monitor::new(Some(&x.display)).expect("monitor builds"),
    ));
    let big: Vec<u8> = b"0123456789abcdef"
        .iter()
        .copied()
        .cycle()
        .take(5 * 1024 * 1024)
        .collect();
    x.xclip_copy("clipboard", &big).await;
    assert!(monitor.0.changed().wait(Duration::from_secs(3)).await);
    let read = monitor
        .read(false)
        .await
        .expect("an INCR transfer reads whole");
    assert!(
        read.bytes == big,
        "an INCR transfer reads whole: {} bytes",
        read.bytes.len()
    );
    assert!(
        monitor
            .offer(vec![("text/plain".to_string(), big.clone())])
            .await
    );
    let served = x.xclip_out(&["-selection", "clipboard", "-o"]).await;
    assert!(
        served == big,
        "an offer larger than a request is served whole: {} bytes",
        served.len()
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Xvfb and xclip"]
async fn x11_monitor_notices_its_server_dying() {
    let x = PrivateX::start();
    let monitor = super::X11Monitor::new(Some(&x.display)).expect("monitor builds");
    assert!(monitor.alive());
    drop(x);
    let deadline = Instant::now() + Duration::from_secs(3);
    while monitor.alive() {
        assert!(
            Instant::now() < deadline,
            "a dead server leaves no live monitor"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        monitor.read(false).is_err(),
        "a read on a dead monitor falls to the next rung"
    );
    assert!(
        monitor
            .offer(text_entries(&[("text/plain", b"x")]))
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Xvfb and xclip"]
async fn the_xclip_rung_writes_and_reads_a_live_selection() {
    let _serial = SERIAL.lock().await;
    let x = PrivateX::start();
    let h = Clipboard::new(
        controller(),
        FakeSession::x11(Some(&x.display)),
        None,
        policy("true", true),
    );
    *h.x11.retry_at.lock() = Some(Instant::now() + Duration::from_secs(3600));
    let start = Instant::now();
    assert!(
        h.write(Payload::text("through xclip"), None).await,
        "xclip took the write"
    );
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "without waiting on the daemon xclip forks: {:?}",
        start.elapsed()
    );
    assert_eq!(
        x.xclip_out(&["-selection", "clipboard", "-o"]).await,
        b"through xclip"
    );
    assert_eq!(
        h.read(false).await,
        Some(Payload::text("through xclip")),
        "the rung reads it back"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs Xvfb and xclip"]
async fn the_x11_session_moves_copies_both_ways_without_an_echo() {
    let _serial = SERIAL.lock().await;
    let x = PrivateX::start();
    let host = controller();
    let h = Clipboard::new(
        host.clone(),
        FakeSession::x11(Some(&x.display)),
        None,
        policy("true", true),
    );
    let task = tokio::spawn(h.clone().run_monitor());
    tokio::time::sleep(Duration::from_millis(300)).await;

    x.xclip_copy("clipboard", b"copied in X").await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        host.payloads(1),
        vec![Payload::text("copied in X")],
        "an X copy reaches the client"
    );

    host.clear();
    h.on_message(1, &format!("cw,{}", BASE64.encode(b"from the client")))
        .await;
    assert_eq!(
        x.xclip_out(&["-selection", "clipboard", "-o"]).await,
        b"from the client",
        "a client's text is pasteable"
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        host.payloads(1).is_empty(),
        "and does not come back: {:?}",
        host.payloads(1)
    );

    let rich = text_entries(&[("text/html", b"<b>rich</b>"), ("text/plain", b"rich")]);
    h.on_message(1, &wire(&rich)).await;
    assert_eq!(
        x.xclip_out(&["-selection", "clipboard", "-o", "-t", "text/html"])
            .await,
        b"<b>rich</b>"
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        host.payloads(1).is_empty(),
        "nor does a rich copy: {:?}",
        host.payloads(1)
    );

    h.on_message(1, "cr").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(host.verbs(1)[0], "clipboard_reply", "a fetch is tagged");
    assert_eq!(
        host.payloads(1),
        vec![Payload::envelope(&rich)],
        "and answers with the flavors"
    );
    h.stop();
    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
}

// --- a real compositor ----------------------------------------------------------------------

/// Wait for `check`, five seconds at most: a real compositor answers in its own time.
async fn eventually(what: &str, check: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !check() {
        assert!(Instant::now() < deadline, "never: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// `wl-paste` of `mime` on the compositor at `dir`'s `socket`: its stdout.
async fn wl_paste(dir: &str, socket: &str, mime: &str) -> Vec<u8> {
    tokio::process::Command::new("wl-paste")
        .args(["-n", "-t", mime])
        .env("XDG_RUNTIME_DIR", dir)
        .env("WAYLAND_DISPLAY", socket)
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .expect("wl-paste runs")
        .stdout
}

/// `wl-copy` of `data` under `mime`; it forks to serve it until another owner takes over or
/// its compositor goes away.
async fn wl_copy(dir: &str, socket: &str, mime: &str, data: &[u8]) {
    let mut child = tokio::process::Command::new("wl-copy")
        .args(["-t", mime])
        .env("XDG_RUNTIME_DIR", dir)
        .env("WAYLAND_DISPLAY", socket)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("wl-copy runs");
    let mut stdin = child.stdin.take().expect("piped");
    tokio::io::AsyncWriteExt::write_all(&mut stdin, data)
        .await
        .expect("wl-copy reads");
    drop(stdin);
    child.wait().await.expect("wl-copy forks and exits");
}

/// A headless labwc of this test's own, in a runtime directory of its own, standing in for the
/// nested session compositor the apps run under. Its process group ends on drop, Xwayland with
/// it, and `wl-copy` follows its compositor.
struct PrivateLabwc {
    child: std::process::Child,
    dir: std::path::PathBuf,
}

impl PrivateLabwc {
    fn start() -> PrivateLabwc {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::process::CommandExt;
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after the epoch")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("clipboard-labwc-{}-{unique}", std::process::id()));
        std::fs::create_dir(&dir).expect("runtime directory");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("private");
        let child = std::process::Command::new("labwc")
            .env("XDG_RUNTIME_DIR", &dir)
            .env("WLR_BACKENDS", "headless")
            .env("WLR_RENDERER", "pixman")
            .env("LIBSEAT_BACKEND", "noop")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DISPLAY")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .expect("labwc runs");
        let labwc = PrivateLabwc { child, dir };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !super::x11::socket_live(&labwc.dir.join("wayland-0")) {
            assert!(Instant::now() < deadline, "labwc never listened");
            std::thread::sleep(Duration::from_millis(20));
        }
        labwc
    }

    fn dir(&self) -> String {
        self.dir.display().to_string()
    }

    /// The socket as pixelflux takes it, whatever this process's own runtime directory is.
    fn socket(&self) -> String {
        self.dir.join("wayland-0").display().to_string()
    }
}

impl Drop for PrivateLabwc {
    fn drop(&mut self) {
        let group = i32::try_from(self.child.id()).expect("a pid fits");
        // SAFETY: kill has no memory preconditions, and the group is the one this test started.
        unsafe { libc::kill(-group, libc::SIGTERM) };
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs labwc, wl-copy, and wl-paste"]
async fn a_nested_compositor_moves_copies_through_the_real_data_control_client() {
    let _serial = SERIAL.lock().await;
    record_logs();
    let labwc = PrivateLabwc::start();
    let (dir, socket) = (labwc.dir(), labwc.socket());
    let surface: Arc<dyn CompositorClipboard> = Arc::new(pixelflux::ScreenCapture::new());
    let host = controller();
    let h = Clipboard::new(
        host.clone(),
        FakeSession::wayland(true, &socket),
        Some(surface),
        policy("true", true),
    );
    clear_logs();
    let task = tokio::spawn(h.clone().run_monitor());
    eventually("the watch armed", || {
        !lines_with("data-control watch active").is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        lines_with("no selection watch").is_empty(),
        "labwc offers data-control"
    );

    wl_copy(&dir, "wayland-0", "text/plain", b"copied in the session").await;
    eventually("an app's copy reached the client", || {
        host.payloads(1) == vec![Payload::text("copied in the session")]
    })
    .await;

    let png: Vec<u8> = (0..=255u8).cycle().take(64 * 1024).collect();
    wl_copy(&dir, "wayland-0", "image/png", &png).await;
    eventually("a large picture reached the client", || {
        host.payloads(1).last() == Some(&Payload::binary("image/png", png.clone()))
    })
    .await;

    host.clear();
    h.on_message(1, &format!("cw,{}", BASE64.encode(b"from the client")))
        .await;
    assert_eq!(
        wl_paste(&dir, "wayland-0", "text/plain").await,
        b"from the client",
        "a client's text is pasteable"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        host.payloads(1).is_empty(),
        "and does not come back: {:?}",
        host.payloads(1)
    );

    let rich = text_entries(&[("text/html", b"<b>rich</b>"), ("text/plain", b"rich")]);
    h.on_message(1, &wire(&rich)).await;
    assert_eq!(
        wl_paste(&dir, "wayland-0", "text/html").await,
        b"<b>rich</b>",
        "a rich editor takes the markup"
    );
    assert_eq!(
        wl_paste(&dir, "wayland-0", "text/plain").await,
        b"rich",
        "a plain field the text"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        host.payloads(1).is_empty(),
        "nor does a rich copy: {:?}",
        host.payloads(1)
    );
    assert_eq!(
        h.read(true).await,
        Some(Payload::envelope(&rich)),
        "the selection reads as the same envelope"
    );

    assert!(h.write(Payload::secret(PASSWORD), None).await);
    assert_eq!(
        wl_paste(&dir, "wayland-0", HINTS[0]).await,
        b"secret",
        "a secret is offered with its hint"
    );
    let read = h.read(true).await.expect("the secret reads back");
    assert!(is_secret(&read, PASSWORD), "as its text, marked: {read:?}");

    let keys = FakeKeys::new(&[], None);
    assert!(h.write(Payload::text("before"), None).await);
    let start = Instant::now();
    assert!(h.inject_text_via_clipboard("typed", &keys).await);
    assert!(
        start.elapsed() >= PASTE_FETCH_MAX,
        "an app that never fetches is waited for: {:?}",
        start.elapsed()
    );
    assert_eq!(
        wl_paste(&dir, "wayland-0", "text/plain").await,
        b"before",
        "the clipboard is restored after typing"
    );
    assert_eq!(keys.chord_of(), PASTE_CHORD);

    stop_monitor(&h, task).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs wl-copy, wl-paste, and XDG_RUNTIME_DIR for a real pixelflux compositor"]
async fn the_capture_compositor_moves_copies_through_its_real_sink() {
    let _serial = SERIAL.lock().await;
    record_logs();
    let dir =
        std::env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR holds the compositor's socket");
    pixelflux::ensure_compositor(640, 480, String::new(), String::new(), String::new(), 16)
        .expect("compositor starts");
    let socket = tokio::task::spawn_blocking(pixelflux::wayland_display_name)
        .await
        .unwrap()
        .expect("compositor listens");
    let surface: Arc<dyn CompositorClipboard> = Arc::new(pixelflux::ScreenCapture::new());
    let host = controller();
    let h = Clipboard::new(
        host.clone(),
        FakeSession::wayland(false, &socket),
        Some(surface),
        policy("true", true),
    );
    clear_logs();
    let task = tokio::spawn(h.clone().run_monitor());
    eventually("the sink armed", || {
        !lines_with("native compositor callback active").is_empty()
    })
    .await;

    h.on_message(1, &format!("cw,{}", BASE64.encode(b"from the client")))
        .await;
    assert_eq!(
        wl_paste(&dir, &socket, "text/plain").await,
        b"from the client",
        "a client's text is pasteable"
    );
    assert_eq!(
        h.read(false).await,
        Some(Payload::text("from the client")),
        "and read back from the cache"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        host.payloads(1).is_empty(),
        "and does not come back: {:?}",
        host.payloads(1)
    );

    wl_copy(&dir, &socket, "text/plain", b"copied in the session").await;
    eventually("an app's copy reached the client", || {
        host.payloads(1)
            .contains(&Payload::text("copied in the session"))
    })
    .await;
    stop_monitor(&h, task).await;
}
