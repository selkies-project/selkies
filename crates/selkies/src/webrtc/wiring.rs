/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The input stack, the clipboard, and the gamepads over this transport: what the integrator hands
//! `Dispatcher`, `Clipboard`, and `GamepadSlots`, and the RTC configuration's HTTP client.
//!
//! The clipboard's sequencing is one for both transports (`clipboard::outbound`): a newer announcement
//! supersedes an older one in flight, a tagged reply completes, and a viewer is announced nothing.
//! That code writes the WebSockets frames, and `RtcClipboardHost` reads each into the message a data
//! channel page takes: `clipboard,<b64>` as `clipboard-msg`, `clipboard_binary,<mime>,<b64>` likewise,
//! and `clipboard_start`, `clipboard_data`, and `clipboard_finish` as `clipboard-msg-start`,
//! `clipboard-msg-data`, and `clipboard-msg-end`; a `clipboard_reply,<verb>` or `clipboard_secret`
//! frame marks the next start or whole message. An empty payload with neither mark goes nowhere.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use parking_lot::Mutex;
use selkies_rtc::ice::rtc_config::{HttpFetch, HttpRequest, HttpResponse, RtcConfigSettings};
use selkies_rtc::signaling::ClientType;
use serde_json::{Value as Json, json};

use crate::clipboard::outbound::{ClientId, ClipboardHost};
use crate::core::{DisplayId, Role};
use crate::gamepad::{GamepadHost, SlotAuthority};
use crate::input::authority::Sender;
use crate::input::{ConnId, CursorMessage, InputHost};
use crate::settings::settings;
use crate::webrtc::channel::ChannelTx;
use crate::webrtc::engine::RtcEngine;
use crate::webrtc::service::WebRtcService;

/// Seconds a clipboard chunk waits for its channel's queue before the peer is left out of the rest.
const BULK_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

impl RtcEngine {
    fn peer_by_conn(&self, conn: u64) -> Option<String> {
        self.peers
            .lock()
            .iter()
            .find(|(_, p)| p.conn == conn)
            .map(|(id, _)| id.clone())
    }

    fn channel_by_conn(&self, conn: u64) -> Option<ChannelTx> {
        self.peers
            .lock()
            .iter()
            .find(|(_, p)| p.conn == conn)
            .and_then(|(_, p)| p.channel.clone())
    }
}

/// What precedes a client's next clipboard payload, and whether it has fallen out of this one.
#[derive(Default)]
struct PendingMarks {
    reply_to: Option<String>,
    secret: bool,
    undrained: bool,
}

/// The clipboard's frames onto data channels (module docblock).
pub struct RtcClipboardHost {
    engine: Arc<RtcEngine>,
    marks: Mutex<HashMap<ClientId, PendingMarks>>,
}

impl RtcClipboardHost {
    pub fn new(engine: Arc<RtcEngine>) -> Arc<Self> {
        Arc::new(Self {
            engine,
            marks: Mutex::new(HashMap::new()),
        })
    }

    /// One frame as the message a channel carries; `None` for a mark taken for the next.
    fn translate(&self, client: ClientId, frame: &str) -> Option<(&'static str, Json)> {
        let mut marks = self.marks.lock();
        let marks = marks.entry(client).or_default();
        let (verb, rest) = frame.split_once(',').unwrap_or((frame, ""));
        let decorate = |mut payload: Json, marks: &mut PendingMarks| {
            if let Some(reply_to) = marks.reply_to.take() {
                payload["reply_to"] = json!(reply_to);
            }
            if std::mem::take(&mut marks.secret) {
                payload["secret"] = json!(true);
            }
            payload
        };
        let whole = |mime: &str, content: &str, binary: bool| {
            let total = BASE64.decode(content).map_or(0, |b| b.len());
            json!({"content": content, "mime_type": mime, "is_binary_data": binary, "total_size": total})
        };
        match verb {
            "clipboard_reply" => {
                marks.reply_to = Some(rest.to_string());
                None
            }
            "clipboard_secret" => {
                marks.secret = true;
                None
            }
            // An empty payload goes out only as a tagged reply or a secret's retraction.
            "clipboard" if rest.is_empty() && marks.reply_to.is_none() && !marks.secret => None,
            "clipboard" => Some((
                "clipboard-msg",
                decorate(whole("text/plain", rest, false), marks),
            )),
            "clipboard_binary" => {
                let (mime, content) = rest.split_once(',').unwrap_or((rest, ""));
                Some(("clipboard-msg", decorate(whole(mime, content, true), marks)))
            }
            "clipboard_start" => {
                let (mime, total) = rest.rsplit_once(',').unwrap_or((rest, "0"));
                let start = json!({
                    "mime_type": mime,
                    "is_binary_data": mime != "text/plain",
                    "total_size": total.parse::<u64>().unwrap_or(0),
                });
                Some(("clipboard-msg-start", decorate(start, marks)))
            }
            "clipboard_data" => Some(("clipboard-msg-data", json!({"content": rest}))),
            "clipboard_finish" => Some(("clipboard-msg-end", json!({}))),
            _ => None,
        }
    }
}

#[async_trait]
impl ClipboardHost for RtcClipboardHost {
    fn clients(&self) -> Vec<(ClientId, Role)> {
        self.engine
            .peers
            .lock()
            .iter()
            .filter(|(_, p)| p.channel.as_ref().is_some_and(ChannelTx::is_open))
            .map(|(_, p)| {
                (
                    p.conn,
                    if p.client_type == ClientType::Controller {
                        Role::Controller
                    } else {
                        Role::Viewer
                    },
                )
            })
            .collect()
    }

    async fn send_text(&self, client: ClientId, message: String) -> bool {
        let Some(channel) = self
            .engine
            .channel_by_conn(client)
            .filter(ChannelTx::is_open)
        else {
            return false;
        };
        if self.marks.lock().get(&client).is_some_and(|m| m.undrained) {
            return false;
        }
        if let Some((msg_type, data)) = self.translate(client, &message) {
            channel.send_typed(msg_type, &data);
        }
        true
    }

    async fn bulk_window(&self, client: ClientId, _len: usize) {
        let Some(channel) = self.engine.channel_by_conn(client) else {
            return;
        };
        if !channel.drain(BULK_DRAIN_TIMEOUT).await {
            log::warn!(target: "webrtc", "Data channel did not drain a clipboard chunk within {}s; leaving it out of this payload.", BULK_DRAIN_TIMEOUT.as_secs());
            self.marks.lock().entry(client).or_default().undrained = true;
        }
    }

    fn audit(&self, event: &str, mime: &str, size: usize, multipart: Option<bool>) {
        let mut fields = serde_json::Map::new();
        fields.insert("mime_type".into(), json!(mime));
        fields.insert("size_bytes".into(), json!(size));
        if let Some(multipart) = multipart {
            fields.insert("multipart".into(), json!(multipart));
        }
        crate::http::audit::emit(event, fields);
    }
}

/// The input stack's transport over this service.
pub struct RtcInputHost {
    pub service: Arc<WebRtcService>,
    pub screens: Arc<crate::session_screens::SessionScreens>,
}

#[async_trait]
impl InputHost for RtcInputHost {
    fn sender(&self, conn: Option<ConnId>) -> Sender {
        let engine = self.service.engine();
        let Some(details) = conn.and_then(|id| {
            engine
                .peers
                .lock()
                .iter()
                .find(|(_, p)| p.conn == id)
                .map(|(_, p)| (p.client_type, p.client_token.clone(), p.client_slot.clone()))
        }) else {
            return Sender::default();
        };
        let (client_type, token, slot) = details;
        let tokens = &self.service.deps.tokens;
        let slot = match token.as_deref() {
            Some(token) => tokens.lookup(token).map_or(Json::Null, |e| e.slot),
            None => slot,
        };
        Sender {
            role: Some(if client_type == ClientType::Controller {
                Role::Controller
            } else {
                Role::Viewer
            }),
            holds_mk_token: tokens
                .active_mk_token()
                .is_some_and(|mk| token.as_deref() == Some(mk.as_str())),
            input_authorized: engine.mk_input_authorized(token.as_deref()),
            slot: Some(slot).filter(|s| !s.is_null()),
            // Held back ahead of the dialect, in `on_peer_data_message`.
            co_controller: false,
        }
    }

    fn send_system_action(&self, conn: Option<ConnId>, action: &str) {
        let engine = self.service.engine();
        let peer = conn.and_then(|c| engine.peer_by_conn(c));
        engine.send_system_action(action, peer.as_deref(), false);
    }

    fn send_cursor(&self, cursor: &CursorMessage) {
        if let Ok(payload) = serde_json::to_value(cursor) {
            self.service.engine().send_cursor_data(payload);
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
        self.service.on_resize_handler(resolution, display_id).await;
    }

    async fn scaling_ratio(&self, scale: f64, display_id: &DisplayId) {
        self.service.handle_scaling(scale, display_id).await;
    }

    async fn update_settings(&self, settings_json: Json, display_id: &DisplayId) {
        self.service
            .handle_update_settings(&settings_json, display_id)
            .await;
    }

    async fn video_bitrate(&self, kbps: i64, display_id: &DisplayId) {
        self.service
            .handle_video_bitrate_change(kbps, display_id)
            .await;
    }

    async fn audio_bitrate(&self, kbps: i64) {
        self.service.handle_audio_bitrate_change(kbps).await;
    }

    async fn set_fps(&self, fps: f64, display_id: &DisplayId) {
        self.service.handle_fps_change(fps, display_id).await;
    }

    async fn request_keyframe(&self, display_id: &DisplayId) {
        self.service
            .request_idr_for_display(display_id, false)
            .await;
    }

    async fn pointer_visible(&self, visible: bool) {
        self.service.handle_pointer_visible(visible).await;
    }

    async fn rate_control_mode(&self, mode: &str, display_id: &DisplayId) {
        self.service
            .handle_rate_control_change(mode, display_id)
            .await;
    }

    async fn crf(&self, value: i64, display_id: &DisplayId) {
        self.service.handle_crf_change(value, display_id).await;
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

    async fn client_webrtc_stats(&self, kind: &str, csv: &str) {
        if let Some(metrics) = &self.service.deps.rtc_metrics {
            metrics.webrtc_stats(kind, csv.to_string()).await;
        }
    }

    fn ping_response(&self, latency_ms: f64) {
        self.service.engine().send_latency_time(latency_ms);
    }

    async fn set_client_stream_box(
        &self,
        display_id: &DisplayId,
        origin_x: f64,
        origin_y: f64,
        scale_x: f64,
        scale_y: f64,
    ) {
        self.service
            .set_client_stream_box(display_id, origin_x, origin_y, scale_x, scale_y);
    }

    fn app_compositor_adopted(&self, _display: &str) {
        self.screens.compositor_adopted(self.service.clone());
    }

    fn session_screens_changed(&self) {
        let service = self.service.clone();
        tokio::spawn(async move {
            crate::session_screens::ScreenTransport::republish_second_screen(&*service).await;
        });
    }
}

/// The gamepad gate and rumble over this service.
pub struct RtcGamepadHost(pub Arc<WebRtcService>);

impl GamepadHost for RtcGamepadHost {
    fn send_system_action(&self, conn: ConnId, action: &str) {
        let engine = self.0.engine();
        if let Some(peer) = engine.peer_by_conn(conn) {
            engine.send_system_action(action, Some(&peer), true);
        }
    }

    fn slot_authority(&self, conn: ConnId) -> SlotAuthority {
        let secure = self.0.deps.tokens.is_secure();
        let engine = self.0.engine();
        let details = engine
            .peers
            .lock()
            .iter()
            .find(|(_, p)| p.conn == conn)
            .map(|(_, p)| (p.client_type, p.client_token.clone(), p.client_slot.clone()));
        let Some((client_type, token, slot)) = details else {
            return SlotAuthority {
                role: Role::Viewer,
                slot: None,
                secure,
            };
        };
        let role = if client_type == ClientType::Controller {
            Role::Controller
        } else {
            Role::Viewer
        };
        // A controller's claim of slot 1 is registry identity rather than a gamepad restriction.
        let held = if secure {
            token
                .and_then(|t| self.0.deps.tokens.lookup(&t))
                .map(|e| e.slot)
        } else if role == Role::Viewer {
            Some(slot)
        } else {
            None
        };
        SlotAuthority {
            role,
            slot: held.filter(|s| !s.is_null()),
            secure,
        }
    }
}

/// The settings the RTC configuration resolves from.
pub fn rtc_config_settings() -> RtcConfigSettings {
    let s = settings();
    RtcConfigSettings {
        enable_cloudflare_turn: s.enable_cloudflare_turn().on,
        cloudflare_turn_token_id: s.cloudflare_turn_token_id().to_string(),
        cloudflare_turn_api_token: s.cloudflare_turn_api_token().to_string(),
        rtc_config_json: s.rtc_config_json().to_string(),
        turn_rest_uri: s.turn_rest_uri().to_string(),
        turn_rest_api_key: s.turn_rest_api_key().to_string(),
        turn_rest_username: s.turn_rest_username().to_string(),
        turn_rest_username_auth_header: s.turn_rest_username_auth_header().to_string(),
        turn_rest_protocol_header: s.turn_rest_protocol_header().to_string(),
        turn_rest_tls_header: s.turn_rest_tls_header().to_string(),
        turn_host: s.turn_host().to_string(),
        turn_port: s.turn_port().clamp(0, 65535) as u16,
        turn_protocol: s.turn_protocol().to_string(),
        turn_tls: s.turn_tls().on,
        turn_shared_secret: s.turn_shared_secret().to_string(),
        turn_username: s.turn_username().to_string(),
        turn_password: s.turn_password().map(str::to_owned),
        stun_host: s.stun_host().to_string(),
        stun_port: s.stun_port().clamp(0, 65535) as u16,
    }
}

/// The addresses a `webrtc_public_ip` value lists, separated by commas, whitespace, or both. An
/// entry that is not an IP address is left out, and logged.
pub fn public_ips(listed: &str) -> Vec<std::net::IpAddr> {
    listed
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| match entry.parse() {
            Ok(ip) => Some(ip),
            Err(_) => {
                log::warn!(target: "webrtc", "webrtc_public_ip: '{entry}' is not an IP address; no host candidate advertises it.");
                None
            }
        })
        .collect()
}

/// The ICE topology the settings decide, once.
pub fn ice_topology(
    servers: Vec<selkies_rtc::ice::IceServer>,
) -> Result<selkies_rtc::ice::IceTopology, String> {
    let s = settings();
    let public_ips = public_ips(s.webrtc_public_ip());
    Ok(selkies_rtc::ice::IceTopology {
        udp_mux_port: s.webrtc_udp_mux_port().clamp(0, 65535) as u16,
        tcp_mux_port: s.webrtc_tcp_mux_port().clamp(0, 65535) as u16,
        lite: s.webrtc_ice_lite().on,
        port_range: selkies_rtc::ice::parse_port_range(s.webrtc_port_range())
            .map_err(|e| e.to_string())?,
        public_ips,
        servers,
    })
}

/// A blocking-free HTTP client for the RTC configuration's REST sources, over `hyper-rustls`,
/// holding a request to 10 seconds in total and 5 to connect.
pub struct HyperFetch;

impl HttpFetch for HyperFetch {
    fn fetch(
        &self,
        request: HttpRequest,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, String>> + Send>> {
        Box::pin(async move {
            tokio::time::timeout(Duration::from_secs(10), fetch(request))
                .await
                .map_err(|_| "request timed out".to_string())?
        })
    }
}

async fn fetch(request: HttpRequest) -> Result<HttpResponse, String> {
    use http_body_util::{BodyExt, Full};
    use hyper_util::client::legacy::Client;
    use hyper_util::client::legacy::connect::HttpConnector;
    use hyper_util::rt::TokioExecutor;

    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_connect_timeout(Some(Duration::from_secs(5)));
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()
        .map_err(|e| e.to_string())?
        .https_or_http()
        .enable_http1()
        .wrap_connector(http);
    let client: Client<_, Full<bytes::Bytes>> = Client::builder(TokioExecutor::new()).build(https);
    let mut builder = http::Request::builder()
        .method(request.method)
        .uri(&request.url);
    for (name, value) in &request.headers {
        builder = builder.header(name, value);
    }
    let body = Full::new(bytes::Bytes::from(request.body.unwrap_or_default()));
    let response = client
        .request(builder.body(body).map_err(|e| e.to_string())?)
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    let body = response
        .into_body()
        .collect()
        .await
        .map_err(|e| e.to_string())?
        .to_bytes()
        .to_vec();
    Ok(HttpResponse { status, body })
}

/// A trackpad page's pointer echoes, to the one connection they answer.
pub struct RtcPointerSink(pub std::sync::Weak<WebRtcService>);

impl crate::input::PointerSink for RtcPointerSink {
    fn pointer_echo(&self, conn: ConnId, action: &str) {
        if let Some(service) = self.0.upgrade() {
            let engine = service.engine();
            if let Some(peer) = engine.peer_by_conn(conn) {
                engine.send_system_action(action, Some(&peer), true);
            }
        }
    }
}

/// The clipboard policy the service changes.
pub struct RtcClipboardPolicy(pub std::sync::OnceLock<Arc<crate::clipboard::Clipboard>>);

#[async_trait]
impl crate::ws::host::Clipboard for RtcClipboardPolicy {
    async fn set_binary_enabled(&self, enabled: bool) {
        if let Some(clipboard) = self.0.get() {
            clipboard.set_binary_clipboard(enabled).await;
        }
    }
}

/// The WebRTC transport as the supervisor drives it: the input stack comes up with the service and
/// goes down with it, as the WebSockets transport's does.
pub struct RtcStreaming {
    pub service: Arc<WebRtcService>,
    pub dispatcher: Arc<crate::input::Dispatcher>,
    pub clipboard: Arc<crate::clipboard::Clipboard>,
    pub gamepads: Arc<crate::gamepad::GamepadSlots>,
    /// The published keyboard and pointer, which the first transport to run opens.
    pub devices: Arc<crate::server::stack::PublishedDevices>,
    pub screens: Arc<crate::session_screens::SessionScreens>,
    pub stop: tokio::sync::Notify,
}

#[async_trait]
impl crate::http::supervisor::StreamingService for RtcStreaming {
    /// Resolve the RTC configuration, bind the ICE muxes, bring the input stack up, and serve until
    /// asked to stop.
    async fn start(&self) -> Result<(), String> {
        let http: Arc<dyn HttpFetch> = Arc::new(HyperFetch);
        let monitors = crate::webrtc::wiring::start_rtc_config(&self.service, http).await;
        self.service.begin().await?;
        let host: Arc<dyn GamepadHost> = Arc::new(RtcGamepadHost(self.service.clone()));
        self.gamepads.set_host(Some(host));
        self.dispatcher.connect().await;
        self.devices.open(&self.dispatcher).await;
        self.clipboard.spawn_monitor();
        let dispatcher = self.dispatcher.clone();
        tokio::spawn(async move { dispatcher.apps().probe().await });
        if settings().wayland().on {
            self.screens.realize_startup_dpi().await;
        }
        let ticks = self.service.clone();
        let watched = self.service.clone();
        let ping_clock = self.dispatcher.clone();
        self.service.deps.stats.start_monitor(
            Arc::new(move || !watched.engine().stats_displays().is_empty()),
            Arc::new(move || {
                ping_clock.note_ping_sent();
                ticks.handle_resource_tick();
            }),
        );
        self.stop.notified().await;
        self.service.deps.stats.stop_monitor().await;
        for monitor in monitors {
            monitor.stop().await;
        }
        self.service.shutdown().await;
        self.clipboard.stop();
        self.dispatcher.disconnect().await;
        self.gamepads.set_host(None);
        Ok(())
    }

    async fn stop(&self) {
        self.stop.notify_one();
    }

    async fn sessions(&self) -> Vec<Json> {
        self.service.engine().sessions().await
    }

    async fn disconnect_session(&self, id: &str) -> bool {
        self.service.engine().disconnect_peer(id)
    }
}

/// Resolve the RTC configuration from its prioritized sources, hand it to the service, and start
/// the monitor its source asks for, so refreshed credentials reach signaling and the server's own
/// agents.
pub async fn start_rtc_config(
    service: &Arc<WebRtcService>,
    http: Arc<dyn HttpFetch>,
) -> Vec<selkies_rtc::ice::rtc_config::RtcConfigMonitor> {
    use selkies_rtc::ice::rtc_config::{
        CLOUDFLARE_TTL, FILE_POLL, OnRtcConfig, ParsedRtcConfig, RtcConfigMonitor, RtcConfigSource,
        get_rtc_configuration, ice_servers_from_uris,
    };
    let config = rtc_config_settings();
    let (parsed, source) = get_rtc_configuration(&config, &*http).await;
    let apply = |service: &WebRtcService, parsed: &ParsedRtcConfig| {
        service.set_rtc_config(
            Some(String::from_utf8_lossy(&parsed.config).into_owned()),
            ice_servers_from_uris(&parsed.stun_uris, &parsed.turn_uris),
        );
    };
    apply(service, &parsed);
    let weak = Arc::downgrade(service);
    let on_config: OnRtcConfig = Arc::new(move |parsed: ParsedRtcConfig| {
        if let Some(service) = weak.upgrade() {
            log::debug!(target: "webrtc", "updating the RTC configuration");
            service.set_rtc_config(
                Some(String::from_utf8_lossy(&parsed.config).into_owned()),
                ice_servers_from_uris(&parsed.stun_uris, &parsed.turn_uris),
            );
        }
    });
    let period = Duration::from_secs(60);
    match source {
        RtcConfigSource::Hmac => vec![RtcConfigMonitor::hmac(config, period, on_config)],
        RtcConfigSource::Rest => vec![RtcConfigMonitor::rest(config, http, period, on_config)],
        RtcConfigSource::Cloudflare => vec![RtcConfigMonitor::cloudflare(
            config,
            http,
            CLOUDFLARE_TTL,
            Duration::from_secs(CLOUDFLARE_TTL / 2),
            on_config,
        )],
        RtcConfigSource::File => vec![RtcConfigMonitor::file(
            std::path::PathBuf::from(&config.rtc_config_json),
            FILE_POLL,
            on_config,
        )],
        RtcConfigSource::LongTerm | RtcConfigSource::Default => Vec::new(),
    }
}

/// The gauges over the server's metrics.
#[async_trait]
impl crate::webrtc::host::RtcMetrics for Arc<crate::stats::metrics::Metrics> {
    fn pacer_snapshot(&self, display: &str, snapshot: &selkies_rtc::rtp::pacer::Snapshot) {
        let s = &snapshot.stats;
        self.set_pacer_snapshot(
            display,
            &crate::stats::metrics::PacerSnapshot {
                pace_bps: snapshot.pace_bps as f64,
                queued_bytes: snapshot.queued_bytes as f64,
                video_bytes: snapshot.video_bytes as f64,
                idr_floor_bytes: snapshot.idr_floor_bytes as f64,
                video_dropped: s.video_dropped as f64,
                gop_resets: s.gop_resets as f64,
                keyreqs: s.keyreqs as f64,
                idr_resurrects: s.idr_resurrects as f64,
                timeout_resurrects: s.timeout_resurrects as f64,
                stale_resets: s.stale_resets as f64,
            },
        );
    }

    fn bridge_drops(&self, drops: &HashMap<String, (u64, u64)>) {
        self.set_bridge_drops(drops);
    }

    async fn webrtc_stats(&self, kind: &str, report: String) {
        self.set_webrtc_stats(kind, report).await;
    }

    async fn initialize_csv(&self, dir: &str) {
        self.initialize_webrtc_csv_file(dir).await;
    }
}

/// One signaling socket's ping, written behind everything sent on it so far.
struct SignalingPing(Arc<selkies_rtc::signaling::PeerSocket>);

impl crate::files::pacing::SessionPing for SignalingPing {
    fn ping(&self, payload: &[u8]) -> bool {
        if self.0.is_closed() {
            return false;
        }
        self.0.ping_with(payload.to_vec());
        true
    }
}

/// The signaling sockets as the upload gauge reads them: each stays open for its session's life, so
/// it is this transport's window onto the client's uplink. Print documents go to the controllers.
#[derive(Default)]
pub struct RtcSessions {
    service: std::sync::OnceLock<Arc<WebRtcService>>,
    clocks: Arc<Mutex<HashMap<u64, Arc<crate::files::pacing::UplinkClock>>>>,
}

impl RtcSessions {
    /// Bind the service, and have its signaling pongs feed the clocks.
    pub fn bind(&self, service: Arc<WebRtcService>) {
        let clocks = self.clocks.clone();
        service
            .signaling()
            .set_on_pong(Arc::new(move |socket_id, payload| {
                let clock = clocks.lock().get(&socket_id).cloned();
                if let Some(clock) = clock {
                    clock.note_pong(payload);
                }
            }));
        let _ = self.service.set(service);
    }
}

impl crate::files::SessionDirectory for RtcSessions {
    fn uplink_sessions(&self) -> Vec<crate::files::pacing::UplinkSession> {
        let Some(service) = self.service.get() else {
            return Vec::new();
        };
        let sockets = service.signaling().client_sockets();
        let mut clocks = self.clocks.lock();
        clocks.retain(|id, _| sockets.iter().any(|(socket, _, _)| socket.id() == *id));
        sockets
            .into_iter()
            .map(|(socket, token, raddr)| {
                let clock = clocks
                    .entry(socket.id())
                    .or_insert_with(|| {
                        Arc::new(crate::files::pacing::UplinkClock::new(Arc::new(
                            SignalingPing(socket.clone()),
                        )))
                    })
                    .clone();
                crate::files::pacing::UplinkSession {
                    clock,
                    token,
                    remote: raddr.parse::<std::net::SocketAddr>().ok().map(|a| a.ip()),
                }
            })
            .collect()
    }

    fn announce_print_document(&self, name: &str, size_bytes: u64) {
        if let Some(service) = self.service.get() {
            service.announce_print_document(name, size_bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::public_ips;

    #[test]
    fn public_addresses_are_separated_by_commas_whitespace_or_both() {
        let listed = |text: &str| -> Vec<String> {
            public_ips(text).iter().map(ToString::to_string).collect()
        };
        for text in [
            "203.0.113.5,2001:db8::5",
            "203.0.113.5 2001:db8::5",
            " 203.0.113.5 ,\t2001:db8::5 ",
        ] {
            assert_eq!(listed(text), ["203.0.113.5", "2001:db8::5"], "{text:?}");
        }
        assert_eq!(
            listed("203.0.113.5, desktop.example.test"),
            ["203.0.113.5"],
            "an entry that is not an address is left out"
        );
        assert!(listed("").is_empty());
    }
}
