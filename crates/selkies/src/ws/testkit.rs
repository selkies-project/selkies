/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Stand-ins for what the plane drives, recording what they are told, and the suites that run
//! the service over them, the service itself real.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::Mutex;
use pixelflux::encoders::Codec;
use pixelflux::sink::{CursorSink, Frame as PxFrame, FrameTiming, Reference};
use pixelflux::{CaptureSettings, StreamInfo, StreamStats};
use serde_json::{Map, Value as Json, json};
use tokio::sync::broadcast;

use crate::audio::{AudioPacket, MicWrite};
use crate::capture::FrameBatch;
use crate::display::{Layout, Layouts};
use crate::settings::Settings;
use crate::stats::StreamSource;
use crate::webcam::demand::DemandTransport;
use crate::ws::client::testing::Page;
use crate::ws::client::{Client, ClientId, Frame, Liveness};
use crate::ws::handler::Connection;
use crate::ws::host::*;
use crate::ws::server::{Timings, WsService};

/// Install the default settings once, for the suites that read them.
pub fn settings_installed() {
    crate::settings::install(Settings::parse(&[], &|_| None));
}

/// A `ScreenCapture` stand-in recording the live requests it is given.
#[derive(Default)]
pub struct FakeCapture {
    pub calls: Mutex<Vec<String>>,
    pub capturing: Mutex<bool>,
    pub starts: Mutex<Vec<CaptureSettings>>,
    pub active: Mutex<Option<String>>,
    pub tx: Mutex<Option<broadcast::Sender<Arc<FrameBatch>>>>,
    pub fail_start: Mutex<bool>,
}

impl FakeCapture {
    fn note(&self, call: String) {
        self.calls.lock().push(call);
    }

    pub fn has(&self, call: &str) -> bool {
        self.calls.lock().iter().any(|c| c == call)
    }

    pub fn is_capturing_now(&self) -> bool {
        *self.capturing.lock()
    }

    pub fn count(&self, prefix: &str) -> usize {
        self.calls
            .lock()
            .iter()
            .filter(|c| c.starts_with(prefix))
            .count()
    }

    /// One video frame through the capture's channel: a `0x04` header naming `reference`.
    pub fn deliver(&self, frame_id: u16, reference: u16, key: bool) {
        let mut data = vec![0x04, 0x10 | u8::from(key)];
        data.extend_from_slice(&frame_id.to_be_bytes());
        data.extend_from_slice(&[0, 0, 7, 128, 4, 56]);
        data.extend_from_slice(&reference.to_be_bytes());
        data.extend_from_slice(&[0; 100]);
        let frame = PxFrame {
            data: Arc::new(data),
            codec: Codec::H264,
            stripe_y_start: 0,
            stripe_height: 0,
            frame_id: frame_id as i32,
            timing: FrameTiming::default(),
            reference: Reference::None,
        };
        if let Some(tx) = self.tx.lock().as_ref() {
            let _ = tx.send(Arc::new(FrameBatch {
                frames: vec![frame],
            }));
        }
    }
}

impl StreamSource for FakeCapture {
    fn stream_info(&self) -> Option<StreamInfo> {
        None
    }

    fn stream_stats(&self) -> Option<StreamStats> {
        None
    }
}

impl CaptureHandle for FakeCapture {
    fn start(&self, settings: CaptureSettings) -> Result<(), String> {
        if *self.fail_start.lock() {
            return Err("no encoder".into());
        }
        self.note(format!("start {}x{}", settings.width, settings.height));
        self.starts.lock().push(settings);
        *self.capturing.lock() = true;
        Ok(())
    }

    fn stop(&self) -> Result<(), String> {
        self.note("stop".into());
        *self.capturing.lock() = false;
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<Arc<FrameBatch>> {
        let mut tx = self.tx.lock();
        tx.get_or_insert_with(|| broadcast::channel(64).0)
            .subscribe()
    }

    fn is_capturing(&self) -> bool {
        *self.capturing.lock()
    }

    fn request_idr_frame(&self) {
        self.note("idr".into());
    }

    fn invalidate_reference(&self, frame_id: u16, withheld: bool) {
        self.note(format!("invalidate {frame_id} {withheld}"));
    }

    fn acknowledge_reference(&self, frame_id: u16, held: bool) {
        self.note(format!("acknowledge {frame_id} {held}"));
    }

    fn update_video_bitrate(&self, kbps: i32) -> Result<(), String> {
        self.note(format!("bitrate {kbps}"));
        Ok(())
    }

    fn update_link_rate(&self, kbps: i32) -> Result<(), String> {
        self.note(format!("link {kbps}"));
        Ok(())
    }

    fn update_framerate(&self, fps: f64) -> Result<(), String> {
        self.note(format!("framerate {fps}"));
        Ok(())
    }

    fn update_tunables(&self, _settings: &CaptureSettings) -> Result<(), String> {
        self.note("tunables".into());
        Ok(())
    }

    fn update_capture_region(&self, x: i32, y: i32, w: i32, h: i32) -> Result<(), String> {
        self.note(format!("region {x} {y} {w} {h}"));
        Ok(())
    }

    fn active_codec(&self) -> Option<String> {
        self.active.lock().clone()
    }

    fn set_cursor_sink(&self, _sink: Option<Arc<dyn CursorSink>>) {}

    fn realized_geometry(&self, _output: u32) -> Result<Option<(i32, i32, f64)>, String> {
        Ok(None)
    }

    fn capture_state(&self, _output: u32) -> (String, Option<String>) {
        ("running".into(), None)
    }
}

/// One `FakeCapture` per display.
#[derive(Default)]
pub struct FakeCaptures {
    pub made: Mutex<Vec<(String, Arc<FakeCapture>)>>,
}

impl FakeCaptures {
    pub fn of(&self, display_id: &str) -> Option<Arc<FakeCapture>> {
        self.made
            .lock()
            .iter()
            .find(|(d, _)| d == display_id)
            .map(|(_, c)| c.clone())
    }
}

impl CaptureFactory for FakeCaptures {
    fn capture(&self, display_id: &str) -> Arc<dyn CaptureHandle> {
        let capture = Arc::new(FakeCapture::default());
        self.made
            .lock()
            .push((display_id.to_string(), capture.clone()));
        capture
    }
}

/// A display server whose every request is realized, or refused at `root`.
#[derive(Default)]
pub struct FakeDisplay {
    pub root: Mutex<Option<(i32, i32)>>,
    pub calls: Mutex<Vec<String>>,
}

#[async_trait]
impl Display for FakeDisplay {
    async fn ensure_window_manager(&self, _display_count: usize) {}

    async fn retire_displays(&self) {
        self.calls.lock().push("retire".into());
    }

    async fn current_root_size(&self) -> Option<(i32, i32)> {
        Some(self.root.lock().unwrap_or((1920, 1080)))
    }

    async fn grow_framebuffer(&self, _w: i32, _h: i32) -> bool {
        true
    }

    async fn apply_extended_layout(
        &self,
        _layouts: &mut Layouts,
        w: i32,
        h: i32,
        _fps: f64,
    ) -> bool {
        self.calls.lock().push(format!("layout {w}x{h}"));
        true
    }

    async fn read_realized_root(&self, fallback: (i32, i32)) -> (i32, i32) {
        self.root.lock().unwrap_or(fallback)
    }

    async fn refresh_output_mode(&self, _fps: f64) {}

    async fn set_dpi(&self, dpi: i32) -> bool {
        self.calls.lock().push(format!("dpi {dpi}"));
        true
    }

    async fn set_cursor_size(&self, _size: i32) -> bool {
        true
    }

    fn applied_dpi(&self) -> Option<i32> {
        Some(96)
    }

    async fn wayland_layout_before_start(
        &self,
        _l: &mut Layouts,
        _k: &HashSet<String>,
        _s: f64,
    ) -> LayoutOutcome {
        LayoutOutcome::default()
    }

    async fn wayland_layout_after_primary(
        &self,
        _l: &mut Layouts,
        _s: &HashMap<String, f64>,
    ) -> LayoutOutcome {
        LayoutOutcome::default()
    }

    async fn wayland_size_screen(&self, _w: i32, _h: i32, _s: f64, _g: bool) {}

    async fn wayland_scale_screen(&self, _s: f64) {}

    async fn wayland_destroy_output(&self, _d: &str) {}

    async fn wayland_set_cursor_size(&self, _size: i32) -> bool {
        true
    }

    async fn realize_wayland_dpi(&self, _dpi: &str, _d: &str, _s: Option<(i32, i32)>) -> f64 {
        1.0
    }

    async fn host_output_capacity(&self) -> Option<i64> {
        None
    }

    fn session_screen_capability(&self) -> (bool, String) {
        (true, String::new())
    }

    async fn probe_session_screen_capability(&self) -> (bool, String) {
        (true, String::new())
    }

    fn schedule_session_screen_layout(&self, _layouts: &Layouts) {}
}

pub struct FakeMic;

#[async_trait]
impl MicUplink for FakeMic {
    async fn open(&mut self) -> bool {
        true
    }
    async fn write(&mut self, _packet: &[u8]) -> MicWrite {
        MicWrite::Written
    }
    async fn close(&mut self) {}
}

pub struct FakeAudio {
    pub capturing: Mutex<bool>,
    pub bitrates: Mutex<Vec<i32>>,
    pub left: Mutex<Vec<(ClientId, bool)>>,
    pub tx: broadcast::Sender<AudioPacket>,
}

impl Default for FakeAudio {
    fn default() -> Self {
        FakeAudio {
            capturing: Mutex::new(false),
            bitrates: Mutex::new(Vec::new()),
            left: Mutex::new(Vec::new()),
            tx: broadcast::channel(16).0,
        }
    }
}

#[async_trait]
impl Audio for FakeAudio {
    async fn start(&self) -> bool {
        *self.capturing.lock() = true;
        true
    }
    async fn stop(&self) {
        *self.capturing.lock() = false;
    }
    fn is_capturing(&self) -> bool {
        *self.capturing.lock()
    }
    fn set_bitrate(&self, bps: i32) {
        self.bitrates.lock().push(bps);
    }
    fn subscribe(&self) -> broadcast::Receiver<AudioPacket> {
        self.tx.subscribe()
    }
    fn client_joined(&self, _id: ClientId) {}
    fn client_settings(&self, _id: ClientId, _r: bool) {}
    async fn client_left(&self, id: ClientId, ends: bool) {
        self.left.lock().push((id, ends));
    }
    async fn apply_initial_policy(&self, _d: &str, _v: bool, _o: bool) {}
    fn mic_uplink(&self, _peer: &str) -> Box<dyn MicUplink> {
        Box::new(FakeMic)
    }
}

pub struct FakeWebcam;

#[async_trait]
impl Webcam for FakeWebcam {
    fn uplink_allowed(&self, is_viewer: bool, collaborator: bool) -> bool {
        !is_viewer || collaborator
    }
    async fn push(&self, _message: Bytes) -> Option<bool> {
        Some(true)
    }
    async fn demand_sync(&self, _port: Arc<dyn DemandTransport>) {}
    fn demand_detach(&self, _port: &Arc<dyn DemandTransport>) {}
}

pub struct FakeStats;

#[async_trait]
impl Stats for FakeStats {
    fn start_monitor(
        &self,
        _w: Arc<dyn Fn() -> bool + Send + Sync>,
        _t: Arc<dyn Fn() + Send + Sync>,
    ) {
    }
    async fn stop_monitor(&self) {}
    async fn rush(&self) {}
    fn host_stats(&self) -> Map<String, Json> {
        let mut host = Map::new();
        host.insert("cpu_percent".into(), json!(12.5));
        host
    }
}

pub struct FakeSupervisor;

impl Supervisor for FakeSupervisor {
    fn current_mode(&self) -> Option<String> {
        Some("websockets".into())
    }
    fn set_clients_present(&self, _present: bool) {}
    fn pending_print_documents(&self) -> Vec<(String, u64)> {
        Vec::new()
    }
}

/// The token table, secure when a table was given.
#[derive(Default)]
pub struct FakeTokens {
    pub table: Mutex<Option<Vec<(String, TokenEntry)>>>,
    pub mk: Mutex<Option<String>>,
}

#[async_trait]
impl Tokens for FakeTokens {
    fn is_secure(&self) -> bool {
        self.table.lock().is_some()
    }
    async fn wait_config_gate(&self) {}
    fn lookup(&self, token: &str) -> Option<TokenEntry> {
        self.table
            .lock()
            .as_ref()?
            .iter()
            .find(|(t, _)| t == token)
            .map(|(_, e)| e.clone())
    }
    fn active_mk_token(&self) -> Option<String> {
        self.mk.lock().clone()
    }
}

#[derive(Default)]
pub struct FakeInput {
    pub messages: Mutex<Vec<(String, String, ClientId)>>,
    pub released: Mutex<u32>,
    pub gamepads: Mutex<Vec<ClientId>>,
    pub lost_slots: Mutex<Vec<(ClientId, Vec<u32>)>>,
}

#[async_trait]
impl Input for FakeInput {
    async fn on_message(&self, message: &str, display_id: &str, conn_id: ClientId) {
        self.messages
            .lock()
            .push((message.to_string(), display_id.to_string(), conn_id));
    }
    async fn release_held_input(&self) {
        *self.released.lock() += 1;
    }
    async fn release_gamepads_for_conn(&self, conn_id: ClientId) {
        self.gamepads.lock().push(conn_id);
    }
    async fn release_gamepad_slots_for_conn(&self, conn_id: ClientId, lost: &[u32]) {
        self.lost_slots.lock().push((conn_id, lost.to_vec()));
    }
    async fn apply_client_keyboard_layout(&self, _layout: &str) {}
    fn finger_scroll_available(&self) -> bool {
        false
    }
    async fn current_cursor(&self) -> Option<Json> {
        None
    }
    fn set_system_dpi(&self, _dpi: f64) {}
    fn cursor_size_cap(&self) -> i32 {
        64
    }
    fn apps_available(&self) -> bool {
        true
    }
    fn installed_apps(&self) -> Option<Vec<String>> {
        None
    }
}

pub struct FakeClipboard;

#[async_trait]
impl Clipboard for FakeClipboard {
    async fn set_binary_enabled(&self, _enabled: bool) {}
}

/// A service over the stand-ins, with short periods, and its pages.
pub struct Rig {
    pub service: Arc<WsService>,
    pub captures: Arc<FakeCaptures>,
    pub display: Arc<FakeDisplay>,
    pub audio: Arc<FakeAudio>,
    pub tokens: Arc<FakeTokens>,
    pub input: Arc<FakeInput>,
    next: std::sync::atomic::AtomicU64,
}

/// One page connected to the rig's service.
pub struct RigPage {
    pub page: Page,
    pub conn: Connection,
}

impl RigPage {
    pub fn client(&self) -> &Arc<Client> {
        &self.page.client
    }

    pub fn id(&self) -> ClientId {
        self.page.client.id
    }

    /// The texts this page was sent, once its queued frames were read.
    pub async fn texts(&self) -> Vec<String> {
        tokio::time::sleep(Duration::from_millis(5)).await;
        self.page.texts()
    }

    pub async fn has(&self, text: &str) -> bool {
        self.texts().await.iter().any(|t| t == text)
    }

    pub async fn clear(&self) {
        tokio::time::sleep(Duration::from_millis(5)).await;
        self.page.sent.lock().clear();
    }

    pub async fn say(&mut self, text: &str) {
        let _ = self.conn.handle_text(text).await;
    }

    /// The binary frames this page was sent.
    pub fn binaries(&self) -> Vec<Bytes> {
        self.page
            .sent
            .lock()
            .iter()
            .filter_map(|f| match f {
                Frame::Binary(b) => Some(b.clone()),
                _ => None,
            })
            .collect()
    }
}

impl Rig {
    pub fn new() -> Rig {
        Self::with_timings(Timings {
            reconnect_grace_s: 0.05,
            hidden_owner_handoff_s: 0.05,
            idle_owner_handoff_s: 3.0,
            backpressure_check_s: 3600.0,
            liveness: Liveness::default(),
        })
    }

    pub fn with_timings(timings: Timings) -> Rig {
        settings_installed();
        let captures = Arc::new(FakeCaptures::default());
        let display = Arc::new(FakeDisplay::default());
        let audio = Arc::new(FakeAudio::default());
        let tokens = Arc::new(FakeTokens::default());
        let input = Arc::new(FakeInput::default());
        let deps = WsDeps {
            tokens: tokens.clone(),
            clipboard: Arc::new(FakeClipboard),
            display: display.clone(),
            captures: captures.clone(),
            audio: audio.clone(),
            webcam: Arc::new(FakeWebcam),
            stats: Arc::new(FakeStats),
            supervisor: Arc::new(FakeSupervisor),
            metrics: None,
        };
        let service = WsService::with_timings(deps, timings);
        service.attach_input(input.clone());
        Rig {
            service,
            captures,
            display,
            audio,
            tokens,
            input,
            next: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// A page as the handshake leaves it, without its pushes on the wire yet read.
    pub async fn connect(&self, token: &str, role: &str, slot: Option<&str>) -> Option<RigPage> {
        let n = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let page = Page::new(self.service.next_client_id(), "controller");
        // Every page its own address: the reconnect debounce is per address.
        let _ = n;
        let mut conn = Connection::new(self.service.clone(), page.client.clone());
        if !conn.handshake(token, role, slot).await {
            return None;
        }
        Some(RigPage { page, conn })
    }

    pub async fn controller(&self) -> RigPage {
        self.service.st.lock().last_connections.clear();
        self.connect("", "", None)
            .await
            .expect("a legacy controller connects")
    }

    pub async fn viewer(&self) -> RigPage {
        self.service.st.lock().last_connections.clear();
        self.connect("", "viewer", None)
            .await
            .expect("a legacy viewer connects")
    }

    /// A controller seated on the primary at 1280x720 from tab `tab`.
    pub async fn owner(&self, tab: &str) -> RigPage {
        let mut page = self.controller().await;
        page.say(&format!(
            r#"SETTINGS,{{"initialClientWidth":1280,"initialClientHeight":720,"tabId":"{tab}","encoder":"h264enc"}}"#
        ))
        .await;
        page
    }

    pub fn primary_capture(&self) -> Arc<FakeCapture> {
        self.captures
            .of("primary")
            .expect("the primary has a capture")
    }

    pub fn layout(&self, display_id: &str) -> Option<Layout> {
        self.service.st.lock().layouts.get(display_id).copied()
    }
}

/// A SETTINGS frame's `displayPosition`-less body for a secondary.
pub fn secondary_settings(display: &str, position: &str) -> String {
    format!(
        r#"SETTINGS,{{"displayId":"{display}","displayPosition":"{position}","initialClientWidth":800,"initialClientHeight":600}}"#
    )
}

/// The `type`d JSON message a page was sent, last first.
pub fn typed(texts: &[String], kind: &str) -> Option<Json> {
    texts
        .iter()
        .rev()
        .filter_map(|t| serde_json::from_str::<Json>(t).ok())
        .find(|v| v["type"] == json!(kind))
}
