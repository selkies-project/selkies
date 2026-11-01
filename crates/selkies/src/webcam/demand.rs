/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Asks a page for its camera or microphone only while the session reads the virtual device.
//!
//! A device requested at connect is held until the session ends, so the indicator beside the
//! user's camera reports the session's length rather than the desktop's use. With
//! `webcam_on_start` or `microphone_on_start` set to `demand`, a [`CaptureDemand`] per device
//! polls its readers instead: the interposer's clients, PipeWire's consumers, and a kernel
//! device's openers for the camera ([`VirtualWebcam::consumers`]), the sound server's recording
//! streams for the microphone (the [`Recorders`] the audio engine implements). A reader is
//! passed on at the poll that finds it; its absence only once it has lasted the device's
//! hold-off and [`MIN_IDLE_POLLS`] idle readings, so an application's probe-then-open never
//! blinks the light and one long poll cycle cannot release on two readings taken far apart.
//!
//! The answer goes to one page, since every page that captures pushes into the same device:
//! the first candidate a transport offers, told to release before another is asked or once it
//! may no longer capture. A transport takes part as a [`DemandTransport`]: `capture_candidates`
//! lists the pages that may capture with the preferred one first, `tell_capture` sends the
//! answer to one; it calls [`sync`] wherever that list changes and [`detach`] at shutdown. The
//! wire verb is `CAPTURE_DEMAND <subject> <0|1>` over WebSockets and the
//! `capture_demand,<subject>,<0|1>` system action over WebRTC, obeyed by the page unless the
//! user toggled the device themselves. Neither policy defaults to it, because a process on the
//! desktop can then cause a permission prompt; every transition is logged and audited as
//! `capture.demand` with the reader's name through the [`AuditSink`] the integrator installs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use log::{error, info};
use serde_json::{Value as Json, json};
use tokio::task::JoinHandle;

use super::{VirtualWebcam, shared_webcam};
use crate::settings::{Settings, settings};

/// The WebSocket verb, followed by the subject and `0` or `1`.
pub const MSG_CAPTURE_DEMAND: &str = "CAPTURE_DEMAND";
/// Time between two readings of a device's readers.
pub const POLL: Duration = Duration::from_secs(1);
/// Idle polls a release needs besides the hold-off.
pub const MIN_IDLE_POLLS: u32 = 3;
/// Floor between `/proc` walks for a kernel device's openers; the interposer and PipeWire
/// answer on every poll.
pub const WEBCAM_DEVICE_RECHECK: Duration = Duration::from_secs(4);
/// The camera's hold-off: re-taken a moment later it shows one late frame.
pub const WEBCAM_RELEASE: Duration = Duration::from_secs(2);
/// The microphone's hold-off: re-taken it has lost the start of a sentence and, on an engine
/// that forgot the grant, raised a second prompt.
pub const MICROPHONE_RELEASE: Duration = Duration::from_secs(10);

/// A page as a transport names it: the connection id of the WebSocket or the data channel.
pub type Page = String;

/// A transport the watch can ask: the pages that may capture, and a send to one of them.
#[async_trait]
pub trait DemandTransport: Send + Sync {
    /// The pages that may capture, the preferred one first: controllers on the primary
    /// display in connection order, with no viewer and no second display page.
    fn capture_candidates(&self) -> Vec<Page>;

    /// Tell `page` whether `subject` is wanted; `false` when the page has stopped reading,
    /// which hands the device to the next candidate.
    async fn tell_capture(&self, page: &str, subject: &str, wanted: bool) -> bool;
}

/// The sound server's recording streams, as the audio engine lists them.
pub trait Recorders: Send + Sync {
    /// Bring the virtual microphone up, since nothing can record from a source that is not
    /// there; false when the sound server cannot be reached.
    fn prepare(&self) -> bool;

    /// The applications with an uncorked stream recording from the virtual source, the
    /// source's own bridge from the master left out.
    fn recorders(&self) -> Vec<String>;
}

/// Where `capture.demand` events go: the audit trail the HTTP module delivers.
pub trait AuditSink: Send + Sync {
    /// Queue one event with its fields and return at once.
    fn emit(&self, event: &str, fields: Json);
}

/// One device the watch polls.
#[async_trait]
pub trait DemandDevice: Send + Sync {
    /// `webcam` or `microphone`, as the wire names it.
    fn subject(&self) -> &'static str;

    /// How long a reader's absence must last before the page is released.
    fn release(&self) -> Duration;

    /// Brings the device up, since nothing can read a device that is not there.
    async fn prepare(&self) -> bool;

    /// What reads the device, or `None` when nothing does or nothing can say.
    async fn reader(&self) -> Option<String>;
}

/// Whether `subject` (`webcam` or `microphone`) is asked for on demand; a lock-off wins.
pub fn on_demand(subject: &str) -> bool {
    on_demand_in(&settings(), subject)
}

/// [`on_demand`] against an explicit settings table.
pub fn on_demand_in(settings: &Settings, subject: &str) -> bool {
    let (enabled, policy) = match subject {
        "webcam" => (settings.webcam_enabled(), settings.webcam_on_start()),
        "microphone" => (
            settings.microphone_enabled(),
            settings.microphone_on_start(),
        ),
        _ => return false,
    };
    policy == "demand" && (enabled.on || !enabled.locked)
}

/// The watch's bookkeeping, held briefly and never across an await.
struct State {
    ports: Vec<Arc<dyn DemandTransport>>,
    told: Option<(Arc<dyn DemandTransport>, Page)>,
    wanted: bool,
    reader: Option<String>,
    idle_since: Option<Instant>,
    idle_polls: u32,
    task: Option<JoinHandle<()>>,
}

/// Polls one device's readers and keeps the page that may capture told of the answer.
pub struct CaptureDemand {
    device: Arc<dyn DemandDevice>,
    poll: Duration,
    release: Duration,
    audit: OnceLock<Arc<dyn AuditSink>>,
    state: Mutex<State>,
    /// Serializes routes, which await the transports' sends.
    routing: tokio::sync::Mutex<()>,
}

impl CaptureDemand {
    /// A watch over `device` at its own hold-off and the standard poll.
    pub fn new(device: Arc<dyn DemandDevice>) -> Arc<CaptureDemand> {
        let release = device.release();
        Self::with_timing(device, POLL, release)
    }

    /// A watch polling every `poll` and releasing after `release`.
    pub fn with_timing(
        device: Arc<dyn DemandDevice>,
        poll: Duration,
        release: Duration,
    ) -> Arc<CaptureDemand> {
        Arc::new(CaptureDemand {
            device,
            poll,
            release,
            audit: OnceLock::new(),
            state: Mutex::new(State {
                ports: Vec::new(),
                told: None,
                wanted: false,
                reader: None,
                idle_since: None,
                idle_polls: 0,
                task: None,
            }),
            routing: tokio::sync::Mutex::new(()),
        })
    }

    /// Audits this watch's transitions to `sink` instead of the process-wide one.
    pub fn audit_to(self: Arc<Self>, sink: Arc<dyn AuditSink>) -> Arc<CaptureDemand> {
        let _ = self.audit.set(sink);
        self
    }

    /// The device's wire name.
    pub fn subject(&self) -> &'static str {
        self.device.subject()
    }

    /// Whether something reads the device right now, as last published.
    pub fn wanted(&self) -> bool {
        self.lock().wanted
    }

    /// The transports attached.
    fn ports(&self) -> Vec<Arc<dyn DemandTransport>> {
        self.lock().ports.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether the watch task is running.
    pub fn watching(&self) -> bool {
        self.lock().task.as_ref().is_some_and(|t| !t.is_finished())
    }

    /// Adds a transport; the first one starts the watch.
    pub fn attach(self: &Arc<Self>, port: Arc<dyn DemandTransport>) {
        let mut state = self.lock();
        if !state.ports.iter().any(|p| Arc::ptr_eq(p, &port)) {
            state.ports.push(port);
        }
        if state.task.as_ref().is_none_or(|t| t.is_finished()) {
            state.task = Some(tokio::spawn(Arc::clone(self).run()));
        }
    }

    /// Drops a transport; the watch ends with the last one and the device outlives it.
    pub fn detach(&self, port: &Arc<dyn DemandTransport>) {
        let mut state = self.lock();
        state.ports.retain(|p| !Arc::ptr_eq(p, port));
        if state
            .told
            .as_ref()
            .is_some_and(|(p, _)| Arc::ptr_eq(p, port))
        {
            state.told = None;
        }
    }

    /// Re-addresses the answer after a transport's candidates changed.
    ///
    /// The page asked last is released when it may no longer capture, and the page that may
    /// is asked if it was not; a send that fails hands the device to the next candidate.
    pub async fn route(&self) {
        let _serial = self.routing.lock().await;
        let subject = self.subject();
        let candidates: Vec<(Arc<dyn DemandTransport>, Page)> = self
            .ports()
            .into_iter()
            .flat_map(|port| {
                port.capture_candidates()
                    .into_iter()
                    .map(move |page| (Arc::clone(&port), page))
            })
            .collect();
        let target = candidates.first();
        let release = {
            let mut state = self.lock();
            let stale = state.told.as_ref().is_some_and(|(port, page)| {
                !state.wanted
                    || !target.is_some_and(|(tp, tpage)| Arc::ptr_eq(port, tp) && page == tpage)
            });
            if stale { state.told.take() } else { None }
        };
        if let Some((port, page)) = release {
            port.tell_capture(&page, subject, false).await;
        }
        {
            let state = self.lock();
            if !state.wanted || state.told.is_some() {
                return;
            }
        }
        for (port, page) in candidates {
            if port.tell_capture(&page, subject, true).await {
                self.lock().told = Some((port, page));
                return;
            }
        }
    }

    /// The watch: bring the device up, then poll until the last transport leaves.
    async fn run(self: Arc<Self>) {
        if !self.device.prepare().await {
            error!(
                target: "capture_demand",
                "The {} cannot be watched for readers: its device did not start.",
                self.subject()
            );
            return;
        }
        loop {
            tokio::time::sleep(self.poll).await;
            if self.lock().ports.is_empty() {
                break;
            }
            self.poll().await;
        }
        let mut state = self.lock();
        state.wanted = false;
        state.idle_since = None;
        state.idle_polls = 0;
    }

    /// One reading of the device, published once it is believed.
    async fn poll(&self) {
        if !self
            .ports()
            .iter()
            .any(|port| !port.capture_candidates().is_empty())
        {
            self.note_idle(false);
            self.publish(false).await;
            return;
        }
        match self.device.reader().await {
            Some(name) => {
                self.lock().reader = Some(name);
                self.note_idle(false);
                self.publish(true).await;
            }
            None => {
                if self.note_idle(true) {
                    self.publish(false).await;
                }
            }
        }
    }

    /// Count an idle reading, or reset the count on a reader or an empty poll; the result is
    /// whether the idle run has lasted the hold-off and [`MIN_IDLE_POLLS`].
    fn note_idle(&self, idle: bool) -> bool {
        let mut state = self.lock();
        if !idle {
            state.idle_since = None;
            state.idle_polls = 0;
            return false;
        }
        state.idle_polls += 1;
        match state.idle_since {
            None => {
                state.idle_since = Some(Instant::now());
                false
            }
            Some(since) => state.idle_polls >= MIN_IDLE_POLLS && since.elapsed() >= self.release,
        }
    }

    /// Record a changed answer and route it.
    async fn publish(&self, wanted: bool) {
        let changed = {
            let mut state = self.lock();
            if wanted == state.wanted {
                None
            } else {
                state.wanted = wanted;
                Some(state.reader.clone())
            }
        };
        if let Some(reader) = changed {
            let subject = self.subject();
            if wanted {
                info!(
                    target: "capture_demand",
                    "The {subject} is read by {}; the page is asked for it.",
                    reader.as_deref().unwrap_or("something")
                );
            } else {
                info!(target: "capture_demand", "The {subject} is released.");
            }
            self.audit(json!({
                "subject": subject,
                "action": if wanted { "ask" } else { "release" },
                "reader": if wanted { reader } else { None },
            }));
        }
        self.route().await;
    }

    fn audit(&self, fields: Json) {
        let sink = self.audit.get().or_else(|| AUDIT.get()).cloned();
        if let Some(sink) = sink {
            sink.emit("capture.demand", fields);
        }
    }
}

/// The virtual camera's readers, as its sinks report an open device.
///
/// The camera is started before any uplink, since an application can only open a device that
/// exists, so an `auto` format resolves to I420 ([`super::device_pixel_format`]).
pub struct WebcamDevice {
    webcam: Arc<VirtualWebcam>,
}

impl WebcamDevice {
    /// The device behind the process-wide camera.
    pub fn new() -> WebcamDevice {
        Self::over(shared_webcam())
    }

    /// The device behind `webcam`.
    pub fn over(webcam: Arc<VirtualWebcam>) -> WebcamDevice {
        WebcamDevice { webcam }
    }
}

impl Default for WebcamDevice {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DemandDevice for WebcamDevice {
    fn subject(&self) -> &'static str {
        "webcam"
    }

    fn release(&self) -> Duration {
        WEBCAM_RELEASE
    }

    async fn prepare(&self) -> bool {
        self.webcam.ensure(None).await.is_some()
    }

    async fn reader(&self) -> Option<String> {
        let webcam = Arc::clone(&self.webcam);
        tokio::task::spawn_blocking(move || webcam.consumers(WEBCAM_DEVICE_RECHECK))
            .await
            .ok()
            .flatten()
    }
}

/// The virtual microphone's recorders, as the sound server lists them.
///
/// A conferencing application holds an uncorked stream for as long as it runs and mutes in
/// software, so this decides when the browser is asked, not whether the indicator is honest.
pub struct MicrophoneDevice {
    recorders: Arc<dyn Recorders>,
}

impl MicrophoneDevice {
    pub fn new(recorders: Arc<dyn Recorders>) -> MicrophoneDevice {
        MicrophoneDevice { recorders }
    }
}

#[async_trait]
impl DemandDevice for MicrophoneDevice {
    fn subject(&self) -> &'static str {
        "microphone"
    }

    fn release(&self) -> Duration {
        MICROPHONE_RELEASE
    }

    async fn prepare(&self) -> bool {
        let recorders = Arc::clone(&self.recorders);
        tokio::task::spawn_blocking(move || recorders.prepare())
            .await
            .unwrap_or(false)
    }

    async fn reader(&self) -> Option<String> {
        let recorders = Arc::clone(&self.recorders);
        tokio::task::spawn_blocking(move || recorders.recorders().into_iter().next())
            .await
            .ok()
            .flatten()
    }
}

/// The watches by subject, created on first use for the subjects a policy turns on.
pub struct Demands {
    watches: Mutex<HashMap<&'static str, Arc<CaptureDemand>>>,
}

impl Default for Demands {
    fn default() -> Self {
        Self::new()
    }
}

impl Demands {
    pub fn new() -> Demands {
        Demands {
            watches: Mutex::new(HashMap::new()),
        }
    }

    /// Puts `watch` in under its subject, for a device standing in for the real one.
    pub fn insert(&self, watch: Arc<CaptureDemand>) {
        self.watches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(watch.subject(), watch);
    }

    /// The subjects with a watch.
    pub fn subjects(&self) -> Vec<&'static str> {
        let mut out: Vec<_> = self
            .watches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect();
        out.sort_unstable();
        out
    }

    /// The watches `on_demand` turns on, created where missing.
    fn active(&self, on_demand: &(dyn Fn(&str) -> bool + Sync)) -> Vec<Arc<CaptureDemand>> {
        let mut watches = self.watches.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        for subject in ["webcam", "microphone"] {
            if !on_demand(subject) {
                continue;
            }
            let watch = watches.entry(subject).or_insert_with(|| match subject {
                "webcam" => CaptureDemand::new(Arc::new(WebcamDevice::new())),
                _ => CaptureDemand::new(Arc::new(MicrophoneDevice::new(microphone_recorders()))),
            });
            out.push(Arc::clone(watch));
        }
        out
    }

    /// Attaches `port` to every watch `on_demand` turns on and re-routes the answer.
    pub async fn sync_with(
        &self,
        on_demand: &(dyn Fn(&str) -> bool + Sync),
        port: Arc<dyn DemandTransport>,
    ) {
        for watch in self.active(on_demand) {
            watch.attach(Arc::clone(&port));
            watch.route().await;
        }
    }

    /// Takes `port` out of every watch.
    pub fn detach(&self, port: &Arc<dyn DemandTransport>) {
        let watches: Vec<_> = self
            .watches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for watch in watches {
            watch.detach(port);
        }
    }
}

static DEMANDS: OnceLock<Demands> = OnceLock::new();
static AUDIT: OnceLock<Arc<dyn AuditSink>> = OnceLock::new();
static RECORDERS: OnceLock<Arc<dyn Recorders>> = OnceLock::new();

/// The process-wide watches.
fn demands() -> &'static Demands {
    DEMANDS.get_or_init(Demands::new)
}

/// Installs where `capture.demand` events go; a second call is ignored.
pub fn install_audit_sink(sink: Arc<dyn AuditSink>) {
    let _ = AUDIT.set(sink);
}

/// Installs the sound server's view of the virtual microphone's recorders, before any
/// transport syncs; a second call is ignored.
pub fn install_recorders(recorders: Arc<dyn Recorders>) {
    let _ = RECORDERS.set(recorders);
}

/// The installed recorders, or a stand-in that never prepares, so the watch reports a
/// microphone it cannot watch instead of a device it believes unread.
fn microphone_recorders() -> Arc<dyn Recorders> {
    struct NoSoundServer;
    impl Recorders for NoSoundServer {
        fn prepare(&self) -> bool {
            false
        }
        fn recorders(&self) -> Vec<String> {
            Vec::new()
        }
    }
    RECORDERS
        .get()
        .cloned()
        .unwrap_or_else(|| Arc::new(NoSoundServer))
}

/// Attaches `port` to every watch its settings turn on and re-routes the answer.
pub async fn sync(port: Arc<dyn DemandTransport>) {
    demands().sync_with(&on_demand, port).await;
}

/// Takes `port` out of every watch, at the transport's shutdown.
pub fn detach(port: &Arc<dyn DemandTransport>) {
    demands().detach(port);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use crate::webcam::{CODEC_H264, test_socket_dir, test_webcam};

    /// A transport as the watch sees it: an ordered candidate list and a send that records,
    /// or fails for a page that has stopped reading.
    struct Port {
        pages: Mutex<Vec<Page>>,
        dead: Mutex<HashSet<Page>>,
        sent: Mutex<Vec<(Page, bool)>>,
        slow: bool,
        in_flight: AtomicU32,
        overlapped: AtomicBool,
    }

    impl Port {
        fn new(pages: &[&str]) -> Arc<Port> {
            Arc::new(Port {
                pages: Mutex::new(pages.iter().map(|p| p.to_string()).collect()),
                dead: Mutex::new(HashSet::new()),
                sent: Mutex::new(Vec::new()),
                slow: false,
                in_flight: AtomicU32::new(0),
                overlapped: AtomicBool::new(false),
            })
        }

        fn set_pages(&self, pages: &[&str]) {
            *self.pages.lock().unwrap() = pages.iter().map(|p| p.to_string()).collect();
        }

        fn sent(&self) -> Vec<(String, bool)> {
            self.sent.lock().unwrap().clone()
        }

        fn as_transport(self: &Arc<Self>) -> Arc<dyn DemandTransport> {
            Arc::clone(self) as Arc<dyn DemandTransport>
        }
    }

    #[async_trait]
    impl DemandTransport for Port {
        fn capture_candidates(&self) -> Vec<Page> {
            self.pages.lock().unwrap().clone()
        }

        async fn tell_capture(&self, page: &str, _subject: &str, wanted: bool) -> bool {
            if self.slow {
                if self.in_flight.fetch_add(1, Ordering::SeqCst) > 0 {
                    self.overlapped.store(true, Ordering::SeqCst);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
            }
            if self.dead.lock().unwrap().contains(page) {
                return false;
            }
            self.sent.lock().unwrap().push((page.to_string(), wanted));
            true
        }
    }

    /// A device the test reads and releases by hand.
    struct Device {
        reading: Mutex<Option<String>>,
        prepared: AtomicU32,
    }

    impl Device {
        fn new() -> Arc<Device> {
            Arc::new(Device {
                reading: Mutex::new(None),
                prepared: AtomicU32::new(0),
            })
        }

        fn set(&self, reading: Option<&str>) {
            *self.reading.lock().unwrap() = reading.map(str::to_string);
        }
    }

    #[async_trait]
    impl DemandDevice for Device {
        fn subject(&self) -> &'static str {
            "webcam"
        }
        fn release(&self) -> Duration {
            Duration::from_millis(200)
        }
        async fn prepare(&self) -> bool {
            self.prepared.fetch_add(1, Ordering::SeqCst);
            true
        }
        async fn reader(&self) -> Option<String> {
            self.reading.lock().unwrap().clone()
        }
    }

    fn make_watch(device: &Arc<Device>) -> Arc<CaptureDemand> {
        CaptureDemand::with_timing(
            Arc::clone(device) as Arc<dyn DemandDevice>,
            Duration::from_millis(20),
            Duration::from_millis(200),
        )
    }

    fn told(watch: &CaptureDemand, port: &Arc<Port>, page: &str) {
        watch.lock().told = Some((port.as_transport(), page.to_string()));
    }

    fn sent(pairs: &[(&str, bool)]) -> Vec<(String, bool)> {
        pairs.iter().map(|(p, w)| (p.to_string(), *w)).collect()
    }

    async fn settle(ms: u64) {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }

    #[tokio::test]
    async fn the_watch_follows_the_device() {
        let device = Device::new();
        let watch = make_watch(&device);
        let port = Port::new(&["a"]);
        watch.attach(port.as_transport());
        settle(100).await;
        assert_eq!(
            device.prepared.load(Ordering::SeqCst),
            1,
            "the device is brought up when the first transport attaches"
        );
        assert!(
            port.sent().is_empty() && !watch.wanted(),
            "an unread device asks for nothing"
        );
        device.set(Some("an application"));
        settle(100).await;
        assert_eq!(
            port.sent(),
            sent(&[("a", true)]),
            "a reader is passed on at the poll that finds it"
        );
        device.set(None);
        settle(100).await;
        assert_eq!(
            port.sent(),
            sent(&[("a", true)]),
            "its absence is not believed at once"
        );
        settle(300).await;
        assert_eq!(
            port.sent(),
            sent(&[("a", true), ("a", false)]),
            "and is once it has lasted the hold-off"
        );
        for reading in [Some("an application"), None, Some("an application")] {
            device.set(reading);
            settle(100).await;
        }
        assert_eq!(
            port.sent(),
            sent(&[("a", true), ("a", false), ("a", true)]),
            "a reader that returns inside the hold-off changes nothing"
        );
        watch.attach(port.as_transport());
        assert!(
            watch.ports().len() == 1 && watch.watching(),
            "attaching the same transport twice keeps one entry and one watch"
        );
        watch.detach(&port.as_transport());
        settle(100).await;
        assert!(!watch.watching(), "the watch ends with the last transport");
        assert!(
            !watch.wanted(),
            "and forgets the answer, so the next page is not told a stale one"
        );
        watch.attach(port.as_transport());
        settle(100).await;
        let all = port.sent();
        assert!(
            all.last() == Some(&("a".to_string(), true)) && all.len() == 4,
            "a later transport restarts the watch and is told afresh: {all:?}"
        );
        watch.detach(&port.as_transport());
        settle(100).await;
    }

    /// Elapsed time alone must not release: a long poll cycle would satisfy it on one reading.
    #[tokio::test]
    async fn two_idle_readings_do_not_release() {
        let device = Device::new();
        let watch = CaptureDemand::with_timing(
            Arc::clone(&device) as Arc<dyn DemandDevice>,
            POLL,
            Duration::ZERO,
        );
        let port = Port::new(&["a"]);
        {
            let mut state = watch.lock();
            state.ports = vec![port.as_transport()];
            state.wanted = true;
        }
        told(&watch, &port, "a");
        watch.poll().await;
        watch.poll().await;
        assert!(
            watch.wanted() && port.sent().is_empty(),
            "two idle readings do not release even with the hold-off elapsed"
        );
        watch.poll().await;
        assert!(
            !watch.wanted() && port.sent() == sent(&[("a", false)]),
            "a third does"
        );
    }

    #[tokio::test]
    async fn routing_asks_one_page_at_a_time() {
        let device = Device::new();
        let watch = make_watch(&device);
        let port = Port::new(&["first", "second"]);
        {
            let mut state = watch.lock();
            state.ports = vec![port.as_transport()];
            state.wanted = true;
        }
        watch.route().await;
        assert_eq!(
            port.sent(),
            sent(&[("first", true)]),
            "only the first candidate is asked"
        );
        watch.route().await;
        assert_eq!(
            port.sent(),
            sent(&[("first", true)]),
            "and not again while it stays the candidate"
        );
        port.set_pages(&["second", "first"]);
        watch.route().await;
        assert_eq!(
            port.sent()[1..],
            sent(&[("first", false), ("second", true)])[..],
            "a page that stops being the candidate is released before the next is asked"
        );
        port.set_pages(&[]);
        watch.route().await;
        assert_eq!(
            port.sent().last(),
            Some(&("second".to_string(), false)),
            "the last candidate leaving is still released"
        );
        port.set_pages(&["wedged", "third"]);
        port.dead.lock().unwrap().insert("wedged".to_string());
        watch.route().await;
        assert_eq!(
            port.sent().last(),
            Some(&("third".to_string(), true)),
            "a send that fails hands the device to the next page"
        );
        let other = Port::new(&["elsewhere"]);
        watch.lock().ports = vec![port.as_transport(), other.as_transport()];
        watch.route().await;
        assert!(
            other.sent().is_empty(),
            "a second transport's pages come after the first's"
        );
        port.set_pages(&[]);
        watch.route().await;
        assert!(
            port.sent().last() == Some(&("third".to_string(), false))
                && other.sent() == sent(&[("elsewhere", true)]),
            "and take over when the first has none"
        );
        watch.detach(&other.as_transport());
        assert!(
            watch.lock().told.is_none() && other.sent() == sent(&[("elsewhere", true)]),
            "detaching the transport that was asked forgets it without a send"
        );
        watch.lock().wanted = false;
        port.set_pages(&["fourth"]);
        watch.route().await;
        assert_eq!(
            port.sent().last(),
            Some(&("third".to_string(), false)),
            "nothing is asked while the device is unread"
        );

        let device = Device::new();
        let watch = make_watch(&device);
        let port = Port::new(&[]);
        {
            let mut state = watch.lock();
            state.ports = vec![port.as_transport()];
            state.wanted = true;
        }
        told(&watch, &port, "a");
        watch.poll().await;
        assert!(
            port.sent() == sent(&[("a", false)]) && !watch.wanted(),
            "a poll with no candidate releases the page asked before and forgets the answer"
        );
    }

    /// Two routes in flight must not interleave their sends.
    #[tokio::test]
    async fn overlapping_routes_send_one_at_a_time() {
        let device = Device::new();
        let watch = make_watch(&device);
        let port = Arc::new(Port {
            pages: Mutex::new(vec!["a".to_string()]),
            dead: Mutex::new(HashSet::new()),
            sent: Mutex::new(Vec::new()),
            slow: true,
            in_flight: AtomicU32::new(0),
            overlapped: AtomicBool::new(false),
        });
        {
            let mut state = watch.lock();
            state.ports = vec![port.as_transport()];
            state.wanted = true;
        }
        tokio::join!(watch.route(), watch.route(), watch.route());
        assert!(!port.overlapped.load(Ordering::SeqCst));
        assert_eq!(port.sent(), sent(&[("a", true)]));
    }

    struct Recording(Mutex<Vec<(String, Json)>>);

    impl AuditSink for Recording {
        fn emit(&self, event: &str, fields: Json) {
            self.0.lock().unwrap().push((event.to_string(), fields));
        }
    }

    #[tokio::test]
    async fn transitions_are_audited() {
        let events = Arc::new(Recording(Mutex::new(Vec::new())));
        let device = Device::new();
        let watch = make_watch(&device).audit_to(Arc::clone(&events) as Arc<dyn AuditSink>);
        let port = Port::new(&["a"]);
        watch.lock().ports = vec![port.as_transport()];
        device.set(Some("ffmpeg"));
        watch.poll().await;
        let got = events.0.lock().unwrap().clone();
        assert_eq!(
            got,
            vec![(
                "capture.demand".to_string(),
                json!({"subject": "webcam", "action": "ask", "reader": "ffmpeg"})
            )],
            "an ask is recorded with what reads the device"
        );
        device.set(None);
        for _ in 0..MIN_IDLE_POLLS {
            watch.poll().await;
            settle(100).await;
        }
        let got = events.0.lock().unwrap().clone();
        assert_eq!(
            got.last(),
            Some(&(
                "capture.demand".to_string(),
                json!({"subject": "webcam", "action": "release", "reader": null})
            )),
            "and the release, without one"
        );
        watch.poll().await;
        assert_eq!(
            events.0.lock().unwrap().len(),
            2,
            "an unchanged answer records nothing"
        );
    }

    #[tokio::test]
    async fn sync_attaches_the_watches_the_settings_turn_on() {
        let demands = Demands::new();
        let device = Device::new();
        let watch = make_watch(&device);
        demands.insert(Arc::clone(&watch));
        let policy = |subject: &str| subject == "webcam";
        let port = Port::new(&["a"]);
        let other = Port::new(&["b"]);
        demands.sync_with(&policy, port.as_transport()).await;
        assert_eq!(
            watch.ports().len(),
            1,
            "the transport is attached to the watch the settings turn on"
        );
        assert_eq!(
            demands.subjects(),
            vec!["webcam"],
            "and none is created for a device left on its start state"
        );
        device.set(Some("an application"));
        settle(100).await;
        demands.sync_with(&policy, other.as_transport()).await;
        assert!(
            port.sent() == sent(&[("a", true)]) && other.sent().is_empty(),
            "a second transport joining while the first holds the answer is not asked"
        );
        demands.detach(&port.as_transport());
        demands.sync_with(&policy, other.as_transport()).await;
        assert_eq!(
            other.sent(),
            sent(&[("b", true)]),
            "the first transport leaving hands the device to the second"
        );
        demands.detach(&other.as_transport());
        settle(100).await;
        assert!(
            !watch.watching(),
            "detaching the last transport ends the watch"
        );
        demands.sync_with(&|_| false, port.as_transport()).await;
        assert!(
            watch.ports().is_empty(),
            "a policy other than demand attaches nothing"
        );
    }

    fn parsed(args: &[&str]) -> Settings {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        Settings::parse(&args, &|_| None)
    }

    #[test]
    fn the_demand_policy_and_the_start_state() {
        for subject in ["webcam", "microphone"] {
            for policy in ["false", "true"] {
                let s = parsed(&[&format!("--{subject}-on-start={policy}")]);
                assert!(
                    !on_demand_in(&s, subject),
                    "{subject}: the {policy} policy is not on demand"
                );
            }
            let s = parsed(&[&format!("--{subject}-on-start=true")]);
            assert!(
                s.pipeline_starts_on(subject, "primary", false),
                "{subject}: the true policy starts the uplink at connect"
            );
            let s = parsed(&[&format!("--{subject}-on-start=demand")]);
            assert!(
                on_demand_in(&s, subject),
                "{subject}: the demand policy turns the watch on"
            );
            assert!(
                !s.pipeline_starts_on(subject, "primary", false),
                "{subject}: and starts nothing at connect"
            );
            let s = parsed(&[
                &format!("--{subject}-on-start=demand"),
                &format!("--{subject}-enabled=false|locked"),
            ]);
            assert!(
                !on_demand_in(&s, subject),
                "{subject}: a device locked off is never asked for"
            );
            let s = parsed(&[
                &format!("--{subject}-on-start=demand"),
                &format!("--{subject}-enabled=false"),
            ]);
            assert!(
                on_demand_in(&s, subject),
                "{subject}: merely off is not locked off"
            );
        }
        assert!(!on_demand_in(&parsed(&[]), "clipboard"));
    }

    #[tokio::test]
    async fn the_camera_is_brought_up_before_any_uplink() {
        let dir = test_socket_dir("demand");
        let webcam = test_webcam(&dir, "auto");
        let device = WebcamDevice::over(Arc::clone(&webcam));
        assert!(
            device.prepare().await,
            "the camera is brought up before any uplink asks for it"
        );
        let camera = webcam.camera().expect("running");
        assert_eq!(
            camera.stats().running.map(|r| r.pixel_format),
            Some("YU12".to_string()),
            "an auto format resolves to I420"
        );
        assert_eq!(
            device.reader().await,
            None,
            "an unread device has no reader"
        );
        let mut client =
            std::os::unix::net::UnixStream::connect(camera.socket_path().unwrap()).unwrap();
        let mut config = [0u8; pixelflux::webcam::ring::CONFIG_SIZE];
        std::io::Read::read_exact(&mut client, &mut config).unwrap();
        std::io::Write::write_all(&mut client, &[1]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while camera.clients() == 0 && Instant::now() < deadline {
            settle(5).await;
        }
        assert_eq!(
            device.reader().await.as_deref(),
            Some("interposer client"),
            "an interposer client is one"
        );
        let again = webcam.ensure(Some(CODEC_H264)).await.expect("camera");
        assert!(
            Arc::ptr_eq(&again, &camera),
            "and the camera is never re-created for it"
        );
        drop(client);
        webcam.stop().await;
    }

    struct SoundServer {
        prepared: AtomicU32,
    }

    impl Recorders for SoundServer {
        fn prepare(&self) -> bool {
            self.prepared.fetch_add(1, Ordering::SeqCst);
            true
        }
        fn recorders(&self) -> Vec<String> {
            vec!["parecord".to_string(), "arecord".to_string()]
        }
    }

    #[tokio::test]
    async fn the_microphone_follows_its_recorders() {
        let server = Arc::new(SoundServer {
            prepared: AtomicU32::new(0),
        });
        let device = MicrophoneDevice::new(Arc::clone(&server) as Arc<dyn Recorders>);
        assert!(
            device.prepare().await && server.prepared.load(Ordering::SeqCst) == 1,
            "preparing makes the virtual source"
        );
        assert!(
            device.prepare().await && server.prepared.load(Ordering::SeqCst) == 2,
            "preparing again re-ensures the source"
        );
        assert_eq!(
            device.reader().await.as_deref(),
            Some("parecord"),
            "the first recorder is the reader"
        );
        assert!(
            MICROPHONE_RELEASE > WEBCAM_RELEASE,
            "letting go of a microphone is held off longer than a camera"
        );
        assert_eq!(device.release(), MICROPHONE_RELEASE);
        assert_eq!(
            WebcamDevice::over(test_webcam(&test_socket_dir("mic"), "auto")).release(),
            WEBCAM_RELEASE
        );
    }
}
