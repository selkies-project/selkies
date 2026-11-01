/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The `host` traits over the server's own modules: what the integrator hands `WsDeps`.
//!
//! The Wayland side goes to `session_screens`: its layout passes split around the primary's
//! capture start, the session compositor's DPI ladder, and what backs a second display.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use log::warn;
use parking_lot::Mutex;
use pixelflux::sink::CursorSink;
use pixelflux::{CaptureSettings, StreamInfo, StreamStats};
use serde_json::{Map, Value as Json};
use tokio::sync::broadcast;

use crate::audio::{AudioEngine, AudioPacket, MicWrite, MicrophoneUplink};
use crate::capture::{DisplayCapture, FrameBatch};
use crate::core::{DisplayId, Role};
use crate::display::{Layouts, WaylandOutputs, wayland_output_id};
use crate::input::authority::Sender;
use crate::input::{ConnId, CursorMessage, Dispatcher, InputHost};
use crate::session_screens::{ScreenTransport, SessionScreens, layout};
use crate::settings::settings;
use crate::stats::StreamSource;
use crate::stats::resources::ResourceMonitor;
use crate::webcam::demand::DemandTransport;
use crate::webcam::{WebcamFrame, shared_webcam, webcam_uplink_allowed};
use crate::ws::client::ClientId;
use crate::ws::host::{
    Audio, CaptureFactory, CaptureHandle, Display, Input, LayoutOutcome, MicUplink, Stats,
    TokenEntry, Tokens, Webcam,
};
use crate::ws::server::WsService;

fn text<E: std::fmt::Display>(result: Result<(), E>) -> Result<(), String> {
    result.map_err(|e| e.to_string())
}

impl StreamSource for DisplayCapture {
    fn stream_info(&self) -> Option<StreamInfo> {
        DisplayCapture::stream_info(self)
    }

    fn stream_stats(&self) -> Option<StreamStats> {
        DisplayCapture::stream_stats(self)
    }
}

impl CaptureHandle for DisplayCapture {
    fn start(&self, settings: CaptureSettings) -> Result<(), String> {
        text(DisplayCapture::start(self, settings))
    }

    fn stop(&self) -> Result<(), String> {
        text(DisplayCapture::stop(self))
    }

    fn subscribe(&self) -> broadcast::Receiver<Arc<FrameBatch>> {
        DisplayCapture::subscribe(self)
    }

    fn is_capturing(&self) -> bool {
        DisplayCapture::is_capturing(self)
    }

    fn request_idr_frame(&self) {
        let _ = DisplayCapture::request_idr_frame(self);
    }

    fn invalidate_reference(&self, frame_id: u16, withheld: bool) {
        let _ = DisplayCapture::invalidate_reference(self, frame_id, withheld);
    }

    fn acknowledge_reference(&self, frame_id: u16, held: bool) {
        let _ = DisplayCapture::acknowledge_reference(self, frame_id, held);
    }

    fn update_video_bitrate(&self, kbps: i32) -> Result<(), String> {
        text(DisplayCapture::update_video_bitrate(self, kbps))
    }

    fn update_link_rate(&self, kbps: i32) -> Result<(), String> {
        text(DisplayCapture::update_link_rate(self, kbps))
    }

    fn update_framerate(&self, fps: f64) -> Result<(), String> {
        text(DisplayCapture::update_framerate(self, fps))
    }

    fn update_tunables(&self, settings: &CaptureSettings) -> Result<(), String> {
        text(DisplayCapture::update_tunables(self, settings))
    }

    fn update_capture_region(&self, x: i32, y: i32, w: i32, h: i32) -> Result<(), String> {
        text(DisplayCapture::update_capture_region(self, x, y, w, h))
    }

    fn active_codec(&self) -> Option<String> {
        DisplayCapture::active_codec(self)
    }

    fn set_cursor_sink(&self, sink: Option<Arc<dyn CursorSink>>) {
        if let Err(e) = self.inner().set_cursor_sink(sink) {
            warn!(target: "ws", "Cursor sink not set on '{}': {e}", self.display_id());
        }
    }

    fn realized_geometry(&self, output: u32) -> Result<Option<(i32, i32, f64)>, String> {
        self.inner()
            .get_realized_geometry(output)
            .map_err(|e| e.to_string())
    }

    fn capture_state(&self, output: u32) -> (String, Option<String>) {
        self.inner().capture_state(output)
    }
}

/// Each display's persistent `capture::DisplayCapture`.
pub struct Captures;

impl CaptureFactory for Captures {
    fn capture(&self, display_id: &str) -> Arc<dyn CaptureHandle> {
        Arc::new(DisplayCapture::new(display_id.to_string()))
    }
}

#[async_trait]
impl MicUplink for MicrophoneUplink {
    async fn open(&mut self) -> bool {
        MicrophoneUplink::open(self).await
    }

    async fn write(&mut self, packet: &[u8]) -> MicWrite {
        MicrophoneUplink::write(self, packet).await
    }

    async fn close(&mut self) {
        MicrophoneUplink::close(self).await
    }
}

/// The audio engine, `Framing::Headered`, as the plane reads it.
pub struct EngineAudio(pub Arc<AudioEngine>);

#[async_trait]
impl Audio for EngineAudio {
    async fn start(&self) -> bool {
        self.0.start().await
    }

    async fn stop(&self) {
        self.0.stop().await;
    }

    fn is_capturing(&self) -> bool {
        self.0.is_capturing()
    }

    fn set_bitrate(&self, bps: i32) {
        self.0.set_bitrate(bps);
    }

    fn subscribe(&self) -> broadcast::Receiver<AudioPacket> {
        self.0.subscribe()
    }

    fn client_joined(&self, id: ClientId) {
        self.0.client_joined(id);
    }

    fn client_settings(&self, id: ClientId, audio_redundancy: bool) {
        self.0.client_settings(id, audio_redundancy);
    }

    async fn client_left(&self, id: ClientId, ends_capture: bool) {
        self.0.client_left(id, ends_capture).await;
    }

    async fn apply_initial_policy(&self, display_id: &str, viewer: bool, others_listen: bool) {
        self.0
            .apply_initial_policy(display_id, viewer, others_listen)
            .await;
    }

    fn mic_uplink(&self, peer: &str) -> Box<dyn MicUplink> {
        Box::new(MicrophoneUplink::new(self.0.clone(), peer))
    }
}

/// The shared virtual camera and the process-wide demand watches.
pub struct SharedWebcam;

#[async_trait]
impl Webcam for SharedWebcam {
    fn uplink_allowed(&self, is_viewer: bool, collaborator: bool) -> bool {
        let role = if is_viewer && !collaborator {
            Role::Viewer
        } else {
            Role::Controller
        };
        webcam_uplink_allowed(role)
    }

    async fn push(&self, message: Bytes) -> Option<bool> {
        let frame = WebcamFrame::parse(&message)?;
        let camera = shared_webcam();
        if camera.needs_ensure(Some(frame.codec)) {
            camera.ensure(Some(frame.codec)).await?;
        }
        let flags = camera.push(
            frame.payload,
            frame.codec,
            frame.keyframe,
            frame.rotation,
            frame.flip,
        );
        Some(crate::webcam::VirtualWebcam::keyframe_wanted(flags))
    }

    async fn demand_sync(&self, port: Arc<dyn DemandTransport>) {
        crate::webcam::demand::sync(port).await;
    }

    fn demand_detach(&self, port: &Arc<dyn DemandTransport>) {
        crate::webcam::demand::detach(port);
    }
}

/// One `ResourceMonitor` per run of connections: started with the first and stopped with the
/// last, so a fast reconnect restarts a fresh one and never reads a dead one's figures.
#[derive(Default)]
pub struct MonitorStats {
    monitor: Mutex<Option<Arc<ResourceMonitor>>>,
}

#[async_trait]
impl Stats for MonitorStats {
    fn start_monitor(
        &self,
        watched: Arc<dyn Fn() -> bool + Send + Sync>,
        on_tick: Arc<dyn Fn() + Send + Sync>,
    ) {
        let mut slot = self.monitor.lock();
        if slot.is_some() {
            return;
        }
        let (gpu_id, dri_node) = {
            let s = settings();
            (
                crate::settings::parse_gpu_id(s.gpu_id()).unwrap_or(0) as i64,
                s.encode_dri().to_string(),
            )
        };
        let monitor = ResourceMonitor::new(1.0, gpu_id, &dri_node, None);
        monitor.set_watched(move || watched());
        monitor.set_on_tick(move |_now| {
            on_tick();
            Box::pin(async {})
        });
        monitor.start();
        *slot = Some(monitor);
    }

    async fn stop_monitor(&self) {
        let monitor = self.monitor.lock().take();
        if let Some(monitor) = monitor {
            monitor.stop().await;
        }
    }

    async fn rush(&self) {
        let monitor = self.monitor.lock().clone();
        if let Some(monitor) = monitor {
            monitor.rush().await;
        }
    }

    fn host_stats(&self) -> Map<String, Json> {
        self.monitor
            .lock()
            .as_ref()
            .map(|m| crate::stats::host_stats(m))
            .unwrap_or_default()
    }
}

/// The display server, through `crate::display` and `crate::session_screens`.
pub struct ServerDisplay {
    wm: crate::display::monitors::MultiMonitorWindowManager,
    screens: Arc<SessionScreens>,
}

impl ServerDisplay {
    /// The display server over the session's screens, which `WsInputHost` shares.
    pub fn new(screens: Arc<SessionScreens>) -> ServerDisplay {
        ServerDisplay {
            wm: Default::default(),
            screens,
        }
    }
}

#[async_trait]
impl Display for ServerDisplay {
    async fn ensure_window_manager(&self, display_count: usize) {
        self.wm.ensure_for(display_count, false).await;
    }

    async fn retire_displays(&self) {
        crate::display::resize::retire_displays().await;
    }

    async fn current_root_size(&self) -> Option<(i32, i32)> {
        let info = crate::display::resize::get_new_res("1x1").await;
        let (w, h) = info.current.split_once('x')?;
        Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
    }

    async fn grow_framebuffer(&self, w: i32, h: i32) -> bool {
        crate::display::resize::grow_framebuffer(w, h).await
    }

    async fn apply_extended_layout(&self, layouts: &mut Layouts, w: i32, h: i32, fps: f64) -> bool {
        crate::display::resize::apply_extended_layout(layouts, w, h, Some(fps)).await
    }

    async fn read_realized_root(&self, fallback: (i32, i32)) -> (i32, i32) {
        crate::display::resize::read_realized_root(fallback).await
    }

    async fn refresh_output_mode(&self, fps: f64) {
        crate::display::resize::refresh_output_mode(Some(fps)).await;
    }

    async fn set_dpi(&self, dpi: i32) -> bool {
        crate::display::set_dpi(dpi).await
    }

    async fn set_cursor_size(&self, size: i32) -> bool {
        crate::display::set_cursor_size(size).await
    }

    fn applied_dpi(&self) -> Option<i32> {
        crate::display::applied_dpi()
    }

    async fn wayland_layout_before_start(
        &self,
        layouts: &mut Layouts,
        keep: &HashSet<String>,
        primary_scale: f64,
    ) -> LayoutOutcome {
        match WaylandOutputs::running() {
            Some(outputs) => {
                layout::before_primary(&outputs, Some(&self.screens), layouts, keep, primary_scale)
                    .await
            }
            None => LayoutOutcome::default(),
        }
    }

    async fn wayland_layout_after_primary(
        &self,
        layouts: &mut Layouts,
        dpis: &HashMap<String, String>,
    ) -> LayoutOutcome {
        match WaylandOutputs::running() {
            Some(outputs) => {
                layout::after_primary(&outputs, Some(&self.screens), layouts, dpis).await
            }
            None => LayoutOutcome::default(),
        }
    }

    async fn wayland_size_screen(&self, w: i32, h: i32, scale: f64, grow_only: bool) {
        if let Some(outputs) = WaylandOutputs::running() {
            outputs.size_screen(w, h, scale, grow_only).await;
        }
    }

    async fn wayland_scale_screen(&self, scale: f64) {
        if let Some(outputs) = WaylandOutputs::running() {
            outputs.scale_screen(scale).await;
        }
    }

    async fn wayland_destroy_output(&self, display_id: &str) {
        if let Some(outputs) = WaylandOutputs::running() {
            outputs
                .destroy_output(wayland_output_id(Some(display_id)))
                .await;
        }
    }

    async fn retire_session_screens(&self, secondaries: &[String]) {
        self.screens.ensure_screens(secondaries).await;
    }

    async fn wayland_set_cursor_size(&self, size: i32) -> bool {
        match WaylandOutputs::running() {
            Some(outputs) => outputs.set_cursor_size(size).await,
            None => false,
        }
    }

    async fn realize_wayland_dpi(
        &self,
        dpi: &str,
        display_id: &str,
        size: Option<(i32, i32)>,
    ) -> f64 {
        self.screens.realize_dpi(dpi, display_id, size).await
    }

    async fn host_output_capacity(&self) -> Option<i64> {
        let compositor = pixelflux::running_compositor()?;
        tokio::task::spawn_blocking(move || compositor.output_capacity().ok())
            .await
            .ok()
            .flatten()
    }

    fn session_screen_capability(&self) -> (bool, String) {
        self.screens.capability()
    }

    async fn probe_session_screen_capability(&self) -> (bool, String) {
        self.screens.probe_capability().await
    }

    fn schedule_session_screen_layout(&self, layouts: &Layouts) {
        self.screens.schedule_layout(layouts);
    }
}

/// The token table `http` provisions.
impl From<crate::http::tokens::TokenEntry> for TokenEntry {
    fn from(entry: crate::http::tokens::TokenEntry) -> TokenEntry {
        TokenEntry {
            role: if entry.role == Role::Controller {
                "controller".into()
            } else {
                "viewer".into()
            },
            slot: entry.slot_json(),
        }
    }
}

#[async_trait]
impl Tokens for crate::http::tokens::TokenTable {
    fn is_secure(&self) -> bool {
        !settings().master_token().is_empty()
    }

    async fn wait_config_gate(&self) {
        self.wait_gate().await;
    }

    fn lookup(&self, token: &str) -> Option<TokenEntry> {
        crate::http::tokens::TokenTable::lookup(self, token).map(TokenEntry::from)
    }

    fn active_mk_token(&self) -> Option<String> {
        crate::http::tokens::TokenTable::active_mk_token(self)
    }
}

/// The input stack as the plane drives it.
pub struct DispatcherInput(pub Arc<Dispatcher>);

#[async_trait]
impl Input for DispatcherInput {
    async fn on_message(&self, message: &str, display_id: &str, conn_id: ClientId) {
        self.0.on_message(message, display_id, Some(conn_id)).await;
    }

    async fn release_held_input(&self) {
        self.0.release_mouse_buttons().await;
        self.0.reset_keyboard().await;
    }

    async fn release_gamepads_for_conn(&self, conn_id: ClientId) {
        self.0.release_conn(conn_id);
    }

    async fn release_gamepad_slots_for_conn(&self, conn_id: ClientId, lost: &[u32]) {
        let lost: Vec<u8> = lost.iter().map(|s| *s as u8).collect();
        self.0.release_gamepad_slots_for_conn(conn_id, &lost);
    }

    async fn apply_client_keyboard_layout(&self, layout: &str) {
        self.0.apply_client_keyboard_layout(layout).await;
    }

    fn finger_scroll_available(&self) -> bool {
        self.0.finger_scroll_available()
    }

    async fn current_cursor(&self) -> Option<Json> {
        self.0
            .current_cursor()
            .await
            .and_then(|c| serde_json::to_value(c).ok())
    }

    fn set_system_dpi(&self, dpi: f64) {
        self.0.set_system_dpi(dpi);
    }

    fn cursor_size_cap(&self) -> i32 {
        self.0.cursor_metrics().1 as i32
    }

    fn apps_available(&self) -> bool {
        self.0.apps().available()
    }

    fn installed_apps(&self) -> Option<Vec<String>> {
        self.0.apps().installed()
    }
}

#[async_trait]
impl ScreenTransport for WsService {
    async fn resync_session_scale(&self, dpi: i32) {
        self.resync_wayland_session_scale(dpi).await;
    }

    async fn republish_second_screen(&self) {
        WsService::republish_second_screen(self).await;
    }
}

/// `input::InputHost` over the service: what the input stack's verbs do to the stream.
pub struct WsInputHost {
    pub service: Arc<WsService>,
    pub screens: Arc<SessionScreens>,
}

#[async_trait]
impl InputHost for WsInputHost {
    fn sender(&self, conn: Option<ConnId>) -> Sender {
        let Some(client) = conn.and_then(|id| self.service.client(id)) else {
            return Sender::default();
        };
        let perms = client.perms();
        let tokens = &self.service.deps.tokens;
        let slot = match perms.token.as_deref() {
            Some(token) => tokens.lookup(token).map(|e| e.slot).unwrap_or(Json::Null),
            None => perms.slot.clone(),
        };
        Sender {
            role: Some(if perms.is_viewer() {
                Role::Viewer
            } else {
                Role::Controller
            }),
            holds_mk_token: tokens
                .active_mk_token()
                .is_some_and(|mk| perms.token.as_deref() == Some(mk.as_str())),
            input_authorized: self
                .service
                .holds_input_authority(&perms.role, perms.token.as_deref()),
            slot: Some(slot).filter(|s| !s.is_null()),
            co_controller: self.service.reg.lock().is_beside(client.id),
        }
    }

    fn send_system_action(&self, conn: Option<ConnId>, action: &str) {
        self.service.send_system_action(action, conn);
    }

    fn send_cursor(&self, cursor: &CursorMessage) {
        if let Ok(payload) = serde_json::to_value(cursor) {
            self.service.send_cursor_data(payload);
        }
    }

    fn display_layouts(&self) -> crate::input::Layouts {
        self.service
            .st
            .lock()
            .layouts
            .iter()
            .map(|(d, l)| {
                (
                    d.clone(),
                    crate::input::Rect {
                        x: l.x,
                        y: l.y,
                        w: l.w,
                        h: l.h,
                    },
                )
            })
            .collect()
    }

    async fn resize(&self, resolution: &str, display_id: &DisplayId) {
        self.service.handle_resize(resolution, display_id).await;
    }

    async fn scaling_ratio(&self, scale: f64, display_id: &DisplayId) {
        self.service.handle_scaling(scale, display_id).await;
    }

    /// The plane consumes `SETTINGS` itself, ahead of the dialect.
    async fn update_settings(&self, _settings: Json, _display_id: &DisplayId) {}

    async fn video_bitrate(&self, kbps: i64, display_id: &DisplayId) {
        self.service
            .handle_opcode_video_bitrate(kbps as f64, display_id)
            .await;
    }

    async fn audio_bitrate(&self, kbps: i64) {
        self.service.handle_opcode_audio_bitrate(kbps as f64).await;
    }

    async fn set_fps(&self, fps: f64, display_id: &DisplayId) {
        self.service.handle_opcode_fps(fps, display_id).await;
    }

    /// The plane consumes `REQUEST_KEYFRAME` itself, ahead of the dialect.
    async fn request_keyframe(&self, _display_id: &DisplayId) {}

    async fn pointer_visible(&self, visible: bool) {
        self.service.set_native_cursor_rendering(visible).await;
    }

    async fn rate_control_mode(&self, mode: &str, display_id: &DisplayId) {
        self.service
            .handle_opcode_rate_control(mode, display_id)
            .await;
    }

    async fn crf(&self, value: i64, display_id: &DisplayId) {
        self.service.handle_opcode_crf(value, display_id).await;
    }

    fn client_fps(&self, fps: i64) {
        if let Some(metrics) = &self.service.deps.metrics {
            metrics.set_fps(fps as f64);
        }
    }

    fn client_latency(&self, ms: i64) {
        if let Some(metrics) = &self.service.deps.metrics {
            metrics.set_latency(ms as f64);
        }
    }

    /// WebRTC statistics, a no-op over WebSockets.
    async fn client_webrtc_stats(&self, _kind: &str, _csv: &str) {}

    /// Only the WebRTC transport sends an app-level `ping`.
    fn ping_response(&self, _latency_ms: f64) {}

    async fn set_client_stream_box(
        &self,
        display_id: &DisplayId,
        origin_x: f64,
        origin_y: f64,
        scale_x: f64,
        scale_y: f64,
    ) {
        self.service
            .set_client_stream_box(display_id, origin_x, origin_y, scale_x, scale_y)
            .await;
    }

    fn session_screens_changed(&self) {
        let service = self.service.clone();
        tokio::spawn(async move { service.republish_second_screen().await });
    }

    fn app_compositor_adopted(&self, _display: &str) {
        self.screens.compositor_adopted(self.service.clone());
    }
}
