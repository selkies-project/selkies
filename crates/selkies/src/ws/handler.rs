/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! One data socket from upgrade to cleanup: `GET /api/websockets`, the handshake pushes, the
//! message dispatch, and the teardown behind the reconnect grace.
//!
//! A plain `GET` is the client's reconnect probe and is answered 204, that the transport is
//! up, rather than with an error status every browser logs as a failed load; 409 says another
//! transport is active, and 401 that secure mode wants a token. The socket runs uncompressed
//! (the frames are already H.264, JPEG, or Opus), selects the `selkies` subprotocol, and takes
//! messages up to `WS_MAX_MESSAGE_BYTES`. Protocol pings go out every 30 s, and a socket
//! unanswered 15 s later is cut as a cut peer would be (1006). Tungstenite answers an incoming
//! ping inside its read, so the loop needs no pong of its own; a pong reaches the loop, which
//! feeds it to the uplink clock.
//!
//! Dispatch order: `_gz,1` and `_pad,1`; the viewer gate; `SETTINGS,`; `_stats,`;
//! `CLIENT_FRAME_ACK`; `START_VIDEO`; `PAGE_HIDDEN`/`PAGE_SHOWN`; `STOP_VIDEO`; `DECODE_PACE `;
//! `LOST_FRAME `; `REQUEST_KEYFRAME`; `START_AUDIO`; `STOP_AUDIO`;
//! `SET_NATIVE_CURSOR_RENDERING,`; then the gamepad slot gate, the secure-input gate, the
//! beside-owner `r,`/`s,` drop, and the input stack. Held keys and buttons are one global
//! desktop state, so a departing socket releases them only if it could drive input and its
//! state is now unowned: the primary's owner always qualifies, anything else only as the last
//! input-capable client.

use std::net::SocketAddr;
use std::os::fd::RawFd;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::ConnectInfo;
use axum::extract::State;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;
use futures::StreamExt;
use log::{debug, error, info, warn};
use serde_json::{Map, Value as Json, json};
use tokio::task::JoinHandle;

use crate::audio::MicWrite;
use crate::core::{PRIMARY_DISPLAY, Role};
use crate::http::auth::{RoleCeiling, handshake_session_token, query_param};
use crate::http::listen::ConnInfo;
use crate::input::authority::{
    VIEWER_ALLOWED_PREFIXES, VIEWER_COLLAB_EXTRA_PREFIXES, VIEWER_SILENT_DROP_PREFIXES,
    page_put_to_use, starts_with_any,
};
use crate::settings::{WS_MAX_MESSAGE_BYTES, settings};
use crate::stats::stats_request;
use crate::ws::client::{
    BOUNDED_SEND_SECONDS, Client, ClientId, Frame, NoLink, Perms, SocketLink, TcpLink, run_writer,
};
use crate::ws::host::MicUplink;
use crate::ws::link::MAX_UINT16_FRAME_ID;
use crate::ws::now;
use crate::ws::protocol::{
    ClientSettings, OPCODE_GZIP, OPCODE_MIC, OPCODE_WEBCAM, SECURE_INPUT_PREFIXES,
    WEBCAM_HEADER_LEN, inflate_gz_bounded,
};
use crate::ws::server::{
    MAX_RECENT_CLIENTS, RECONNECT_DEBOUNCE_MS, VIEWER_RESUME_MIN_INTERVAL_S, WsService,
    session_time,
};
use crate::ws::state::{DisplayState, json_f64, sanitize, settings_changed_by};

/// How often a protocol ping goes out, and how long it may go unanswered after.
const HEARTBEAT_S: f64 = 30.0;
const HEARTBEAT_ANSWER_S: f64 = 15.0;
/// A display's reconnect wait extends this far while a newer connection is mid-handshake.
const GRACE_EXTENDED_S: f64 = 15.0;
/// The interval between a shared client's `START_VIDEO` requests.
const VIEWER_START_THROTTLE_S: f64 = 5.0;
/// The subprotocol the server selects.
const SESSION_TOKEN_PROTOCOL: &str = "selkies";

/// The accepted socket's descriptor, as a request extension the listener adds: what the
/// liveness watch and the frame count ask the kernel about. Without it a socket is judged by
/// what stands in this process's own queue and by its pings.
#[derive(Clone, Copy, Debug)]
pub struct PeerSocket(pub RawFd);

/// Who a connection is, as the HTTP layer knows it.
#[derive(Clone, Copy, Debug)]
pub struct PeerInfo {
    pub remote: Option<SocketAddr>,
    pub fd: Option<RawFd>,
}

impl PeerInfo {
    /// What the request's extensions say: the listener's `ConnInfo`, or axum's connect info.
    fn of(parts: &Parts) -> PeerInfo {
        let remote = parts
            .extensions
            .get::<ConnInfo>()
            .and_then(|c| c.remote)
            .or_else(|| {
                parts
                    .extensions
                    .get::<ConnectInfo<SocketAddr>>()
                    .map(|c| c.0)
            });
        PeerInfo {
            remote,
            fd: parts
                .extensions
                .get::<PeerSocket>()
                .map(|p| p.0)
                .or_else(|| parts.extensions.get::<ConnInfo>().and_then(|c| c.fd)),
        }
    }
}

/// The routes the HTTP router merges: `GET /api/websockets`, with its optional trailing slash.
pub fn routes<S: Clone + Send + Sync + 'static>(service: Arc<WsService>) -> Router<S> {
    Router::new()
        .route("/api/websockets", get(websocket_handler))
        .route("/api/websockets/", get(websocket_handler))
        .with_state(service)
}

/// `GET /api/websockets`: refuse when this transport is not the active mode, answer the
/// reconnect probe, and upgrade. A view-only credential caps the role at viewer no matter what
/// the query asks for; secure mode lets the token govern.
pub async fn websocket_handler(
    State(service): State<Arc<WsService>>,
    parts: Parts,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    if service.deps.supervisor.current_mode().as_deref() != Some("websockets") {
        return (StatusCode::CONFLICT, "WebSocket mode is inactive").into_response();
    }
    let wants_upgrade = parts
        .headers
        .get(axum::http::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    if !wants_upgrade {
        return StatusCode::NO_CONTENT.into_response();
    }
    let mut token = String::new();
    if service.deps.tokens.is_secure() {
        match handshake_session_token(&parts.headers, parts.uri.query()) {
            Ok(Some(presented)) => token = presented,
            Ok(None) => {
                return (StatusCode::UNAUTHORIZED, "Token missing in secure mode").into_response();
            }
            Err(e) => return (StatusCode::BAD_REQUEST, e.0).into_response(),
        }
    }
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(rejection) => return rejection.into_response(),
    };
    let viewer_ceiling = parts
        .extensions
        .get::<RoleCeiling>()
        .is_some_and(|c| c.0 == Role::Viewer);
    let (query_role, query_slot) = if viewer_ceiling {
        ("viewer".to_string(), None)
    } else {
        (
            query_param(parts.uri.query(), "role").unwrap_or_default(),
            query_param(parts.uri.query(), "slot"),
        )
    };
    let peer = PeerInfo::of(&parts);
    upgrade
        .protocols([SESSION_TOKEN_PROTOCOL])
        .max_message_size(WS_MAX_MESSAGE_BYTES)
        .max_frame_size(WS_MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
            run_socket(service, socket, peer, token, query_role, query_slot).await;
        })
}

/// One upgraded socket: the writer task and the read loop around the handler's connection.
async fn run_socket(
    service: Arc<WsService>,
    socket: WebSocket,
    peer: PeerInfo,
    token: String,
    query_role: String,
    query_slot: Option<String>,
) {
    let link: Box<dyn SocketLink> = match peer.fd {
        Some(fd) => Box::new(TcpLink::new(fd)),
        None => Box::new(NoLink),
    };
    let remote = peer
        .remote
        .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
    let perms = Perms {
        token: None,
        role: "controller".into(),
        slot: Json::Null,
        id: format!("{:08x}", rand::random::<u32>()),
        connected_at: session_time(),
    };
    let (client, outbox, abort) = Client::new(
        service.next_client_id(),
        remote,
        perms,
        link,
        service.timings.liveness,
    );
    let (sink, mut stream) = socket.split();
    let writer = tokio::spawn(run_writer(client.clone(), outbox, abort.clone(), sink));

    let mut conn = Connection::new(service.clone(), client.clone());
    if conn
        .handshake(&token, &query_role, query_slot.as_deref())
        .await
    {
        let mut abort = abort;
        let mut last_heard = now();
        let mut ping_out: Option<f64> = None;
        let mut beat = tokio::time::interval(Duration::from_secs_f64(1.0));
        loop {
            let message = tokio::select! {
                message = stream.next() => message,
                changed = abort.changed() => {
                    if changed.is_err() || *abort.borrow() {
                        client.set_closed(Some(1006));
                        break;
                    }
                    continue;
                }
                _ = beat.tick() => {
                    let at = now();
                    if ping_out.is_some_and(|sent| at - sent >= HEARTBEAT_ANSWER_S && last_heard < sent) {
                        client.set_closed(Some(1006));
                        client.abort();
                        break;
                    }
                    if ping_out.is_none_or(|sent| at - sent >= HEARTBEAT_S) && at - last_heard >= HEARTBEAT_S {
                        ping_out = Some(at);
                        client.fire(Frame::Ping(Bytes::new()));
                    }
                    continue;
                }
            };
            last_heard = now();
            let message = match message {
                Some(Ok(message)) => message,
                Some(Err(e)) => {
                    debug!(target: "ws", "Client {} read ended: {e}", client.remote);
                    client.set_closed(Some(1006));
                    break;
                }
                None => {
                    client.set_closed(Some(1006));
                    break;
                }
            };
            match message {
                Message::Ping(_) => {}
                Message::Pong(payload) => {
                    client.note_pong(&payload);
                }
                Message::Close(frame) => {
                    client.set_closed(Some(frame.map(|f| f.code).unwrap_or(1005)));
                    break;
                }
                Message::Text(text) => {
                    client.heard();
                    if conn.handle_text(text.as_str()).await.is_break() {
                        break;
                    }
                }
                Message::Binary(data) => {
                    client.heard();
                    if conn.handle_binary(data).await.is_break() {
                        break;
                    }
                }
            }
        }
        conn.teardown().await;
    }
    client.abort();
    let _ = writer.await;
    service.report_presence();
}

/// One text to one socket, unbounded; whether it went.
async fn say(client: &Client, text: impl Into<String>) -> bool {
    client.send(Frame::text(text)).await.is_ok()
}

/// What a message handler tells the read loop.
pub(crate) type Flow = std::ops::ControlFlow<()>;

/// One connection's state in the read loop.
pub(crate) struct Connection {
    pub(crate) service: Arc<WsService>,
    pub(crate) client: Arc<Client>,
    pub(crate) display_id: Option<String>,
    tab_id: Option<String>,
    initial_settings_processed: bool,
    visibility_asked: bool,
    mic: Option<Box<dyn MicUplink>>,
    mic_disabled_sent: bool,
    webcam_disabled_sent: bool,
    start_audio_task: Option<JoinHandle<()>>,
    initial_audio_task: Option<JoinHandle<()>>,
    registered: bool,
}

impl Connection {
    pub(crate) fn new(service: Arc<WsService>, client: Arc<Client>) -> Connection {
        Connection {
            service,
            client,
            display_id: None,
            tab_id: None,
            initial_settings_processed: false,
            visibility_asked: false,
            mic: None,
            mic_disabled_sent: false,
            webcam_disabled_sent: false,
            start_audio_task: None,
            initial_audio_task: None,
            registered: false,
        }
    }

    /// Auth, the reconnect debounce, and the handshake pushes, in the order the client reads
    /// them; false when the connection ends here.
    pub(crate) async fn handshake(
        &mut self,
        token: &str,
        query_role: &str,
        query_slot: Option<&str>,
    ) -> bool {
        let service = self.service.clone();
        let secure = service.deps.tokens.is_secure();
        let mut perms = self.client.perms();
        if secure {
            service.deps.tokens.wait_config_gate().await;
            let Some(entry) = service.deps.tokens.lookup(token) else {
                warn!(target: "ws", "Rejecting connection from {}: Missing or invalid token.", self.client.remote);
                self.client
                    .close(4001, "Invalid authentication token")
                    .await;
                return false;
            };
            perms.token = Some(token.to_string());
            perms.role = if entry.role == "controller" {
                "controller".into()
            } else {
                "viewer".into()
            };
            perms.slot = entry.slot;
            self.client.set_perms(perms.clone());
            let payload = json!({"role": perms.role, "slot": perms.slot});
            if !say(&self.client, format!("AUTH_SUCCESS,{payload}")).await {
                return false;
            }
        } else {
            let role = if query_role == "viewer" {
                "viewer"
            } else {
                "controller"
            };
            let slot = query_slot
                .and_then(|s| s.trim().parse::<i64>().ok())
                .filter(|s| (2..=4).contains(s));
            let refusal = {
                let s = settings();
                if role == "viewer" && slot.is_none() && !s.enable_shared().on {
                    Some("Strict shared clients are not enabled.".to_string())
                } else if let Some(slot) = slot {
                    let enabled = match slot {
                        2 => s.enable_player2().on,
                        3 => s.enable_player3().on,
                        _ => s.enable_player4().on,
                    };
                    (!enabled).then(|| format!("Player slot {slot} is not enabled."))
                } else {
                    None
                }
            };
            if let Some(refusal) = refusal {
                warn!(target: "ws", "Refusing legacy client {}: {refusal}", self.client.remote);
                if say(&self.client, format!("KILL {refusal}")).await {
                    self.client.close(1008, &refusal).await;
                }
                return false;
            }
            perms.role = role.into();
            perms.slot = slot.map(Json::from).unwrap_or(Json::Null);
            self.client.set_perms(perms.clone());
        }

        let at = now();
        let ip = self.client.remote.ip().to_string();
        // A peer with no address (a Unix socket) is every local page: the debounce says nothing.
        let debounced = !self.client.remote.ip().is_unspecified()
            && {
                let mut st = service.st.lock();
                let last = st
                    .last_connections
                    .iter()
                    .find(|(a, _)| *a == ip)
                    .map(|(_, t)| *t);
                match last {
                    Some(last) if (at - last) * 1000.0 < RECONNECT_DEBOUNCE_MS => {
                        warn!(target: "ws", "Client {ip} reconnecting too quickly ({:.1}ms). Rejecting connection.", (at - last) * 1000.0);
                        true
                    }
                    _ => {
                        st.last_connections.retain(|(a, _)| *a != ip);
                        st.last_connections.push_back((ip, at));
                        while st.last_connections.len() > MAX_RECENT_CLIENTS {
                            st.last_connections.pop_front();
                        }
                        false
                    }
                }
            };
        if debounced {
            self.client
                .close(4029, "Rate limited: reconnecting too quickly")
                .await;
            return false;
        }

        service
            .reg
            .lock()
            .clients
            .insert(self.client.id, self.client.clone());
        self.registered = true;
        info!(target: "ws", "Client {} connected ({}, role {}, slot {}).", self.client.remote,
            if secure { "token" } else { "legacy" }, perms.role, perms.slot);
        audit(
            "session.connect",
            json!({"transport": "websockets", "role": perms.role, "slot": perms.slot}),
        );
        service.report_presence();
        service.deps.audio.client_joined(self.client.id);
        service.start_singletons();

        if !say(&self.client, "MODE websockets").await {
            return self.handshake_failed();
        }
        if secure {
            // After MODE, which makes the page build the input context this verdict applies to.
            let granted = service.mk_access_verdict(&perms.role, perms.token.as_deref());
            say(
                &self.client,
                if granted {
                    "MK_ACCESS,1"
                } else {
                    "MK_ACCESS,0"
                },
            )
            .await;
        }
        // A page joining after a secondary attached must learn the roster now.
        say(
            &self.client,
            format!("DISPLAY_CONFIG_UPDATE,{}", service.display_config_payload()),
        )
        .await;
        service.send_current_cursor(&self.client).await;
        // Which display this socket renders is only known from its first SETTINGS; the primary's
        // live encoder is what a viewer renders and a later display seeds from.
        service.refresh_second_screen_capacity().await;
        let settings_message = json!({"type": "server_settings", "settings": service.settings_payload_for_display(PRIMARY_DISPLAY)});
        if !say(&self.client, settings_message.to_string()).await {
            return self.handshake_failed();
        }
        if !perms.is_viewer() {
            for (name, size) in service.deps.supervisor.pending_print_documents() {
                let message = json!({"type": "print_document", "name": name, "size_bytes": size});
                self.client
                    .send_live(Frame::text(message.to_string()), "Control message")
                    .await
                    .ok();
            }
        }
        service.deps.webcam.demand_sync(service.clone()).await;

        // The bounded connect keeps a missing sound server from stalling the handshake before
        // the client can claim its display.
        let mut mic = service
            .deps
            .audio
            .mic_uplink(&self.client.remote.to_string());
        mic.open().await;
        self.mic = Some(mic);
        true
    }

    fn handshake_failed(&mut self) -> bool {
        let service = &self.service;
        service.reg.lock().clients.remove(&self.client.id);
        self.registered = false;
        session_end(&self.client.perms());
        false
    }

    /// One binary frame, typed by its first byte. A `0x05` is gzip-wrapped control text inflated
    /// into a text message, so the dispatch, permission checks included, sees it as such.
    pub(crate) async fn handle_binary(&mut self, data: Bytes) -> Flow {
        let Some(opcode) = data.first().copied() else {
            return Flow::Continue(());
        };
        match opcode {
            OPCODE_GZIP => match inflate_gz_bounded(&data[1..]) {
                Ok(text) => return self.handle_text(&text).await,
                Err(e) => warn!(target: "ws", "Dropping client gzip frame: {e}"),
            },
            OPCODE_MIC => self.handle_mic(&data[1..]).await,
            OPCODE_WEBCAM => self.handle_webcam(data).await,
            _ => {}
        }
        Flow::Continue(())
    }

    /// `0x02`: one Opus microphone packet, gated as text input is, collab escape hatch included,
    /// so both transports gate the mixer alike.
    async fn handle_mic(&mut self, payload: &[u8]) {
        let perms = self.client.perms();
        let allowed = !perms.is_viewer() || self.service.is_collaborator(perms.token.as_deref());
        if !allowed {
            if !self.mic_disabled_sent {
                self.mic_disabled_sent = true;
                info!(target: "ws", "Dropping microphone data from view-only client {}.", self.client.remote);
                say(&self.client, "MICROPHONE_DISABLED").await;
            }
            return;
        }
        let Some(mic) = self.mic.as_mut() else {
            return;
        };
        if mic.write(payload).await == MicWrite::Refused && !self.mic_disabled_sent {
            self.mic_disabled_sent = true;
            info!(target: "ws", "Microphone is disabled/errored. Sending MICROPHONE_DISABLED to client.");
            say(&self.client, "MICROPHONE_DISABLED").await;
        }
    }

    /// `0x06`: one encoded webcam frame, gated like the microphone; the whole message goes to
    /// pixelflux with its payload sliced in place, never copied.
    async fn handle_webcam(&mut self, data: Bytes) {
        let perms = self.client.perms();
        let collaborator = self.service.is_collaborator(perms.token.as_deref());
        if !self
            .service
            .deps
            .webcam
            .uplink_allowed(perms.is_viewer(), collaborator)
        {
            if !self.webcam_disabled_sent {
                self.webcam_disabled_sent = true;
                say(&self.client, "WEBCAM_DISABLED").await;
            }
            return;
        }
        if data.len() <= WEBCAM_HEADER_LEN {
            return;
        }
        if self.service.deps.webcam.push(data).await == Some(true) {
            say(&self.client, "WEBCAM_KEYFRAME").await;
        }
    }

    /// One text message, in the dispatch order the module docs give.
    pub(crate) async fn handle_text(&mut self, message: &str) -> Flow {
        if message == "_gz,1" {
            // Echoed so the client gzips its own large sends too.
            self.client.set_gz();
            say(&self.client, "_gz,1").await;
            return Flow::Continue(());
        }
        if message == "_pad,1" {
            // The page drops `0x07` padding unread: its stream may be padded.
            self.client.set_pad();
            return Flow::Continue(());
        }
        let perms = self.client.perms();
        if perms.is_viewer() {
            let collaborator = self.service.is_collaborator(perms.token.as_deref());
            let allowed = starts_with_any(message, VIEWER_ALLOWED_PREFIXES)
                || (collaborator && starts_with_any(message, VIEWER_COLLAB_EXTRA_PREFIXES));
            if !allowed {
                if !starts_with_any(message, VIEWER_SILENT_DROP_PREFIXES) {
                    let head: String = message.chars().take(100).collect();
                    warn!(target: "ws", "DENIED unauthorized message from viewer {}: {head}...", self.client.remote);
                }
                return Flow::Continue(());
            }
        }
        if let Some(payload) = message.strip_prefix("SETTINGS,") {
            return self.handle_settings(payload).await;
        }
        if let Some(on) = stats_request(message) {
            let display = self.display_id.clone();
            self.service
                .set_stats_subscription(self.client.id, on, display.as_deref())
                .await;
            return Flow::Continue(());
        }
        if message.starts_with("CLIENT_FRAME_ACK") {
            self.handle_ack(message);
            return Flow::Continue(());
        }
        match message {
            "START_VIDEO" => {
                self.handle_start_video().await;
                return Flow::Continue(());
            }
            "PAGE_HIDDEN" | "PAGE_SHOWN" => {
                {
                    let mut st = self.service.st.lock();
                    let id = self.client.id;
                    if message == "PAGE_SHOWN" {
                        st.page_hidden_at.insert(id, None);
                        st.page_shown_at.insert(id, now());
                    } else if !matches!(st.page_hidden_at.get(&id), Some(Some(_))) {
                        st.page_hidden_at.insert(id, Some(now()));
                    }
                }
                self.service.settle_display_owner().await;
                return Flow::Continue(());
            }
            "STOP_VIDEO" => {
                self.handle_stop_video().await;
                return Flow::Continue(());
            }
            "REQUEST_KEYFRAME" => {
                self.service
                    .request_keyframe(&self.client, self.display_id.as_deref());
                return Flow::Continue(());
            }
            "START_AUDIO" => {
                self.handle_start_audio();
                return Flow::Continue(());
            }
            "STOP_AUDIO" => {
                self.handle_stop_audio();
                return Flow::Continue(());
            }
            _ => {}
        }
        if let Some(rest) = message.strip_prefix("DECODE_PACE ") {
            self.handle_decode_pace(rest);
            return Flow::Continue(());
        }
        if let Some(rest) = message.strip_prefix("LOST_FRAME ") {
            if let Ok(frame_id) = rest.trim().parse::<i64>() {
                let target = self
                    .display_id
                    .clone()
                    .unwrap_or_else(|| PRIMARY_DISPLAY.to_string());
                self.service.lost_frame(self.client.id, &target, frame_id);
            }
            return Flow::Continue(());
        }
        if let Some(rest) = message.strip_prefix("SET_NATIVE_CURSOR_RENDERING,") {
            let value = rest.split(',').next().unwrap_or("").trim().to_lowercase();
            let enabled = value == "1" || value == "true";
            info!(target: "ws", "Received SET_NATIVE_CURSOR_RENDERING: {}", if enabled { "True" } else { "False" });
            self.service.set_native_cursor_rendering(enabled).await;
            return Flow::Continue(());
        }
        self.handle_input(message, &perms).await;
        Flow::Continue(())
    }

    /// `CLIENT_FRAME_ACK <id> [<held_ms>]`: only the display's registered client acks the frames
    /// its relay stamped; a viewer's ack would throttle the controller against a stream it never
    /// got. The -1 sentinel is server-internal: accepted from the wire it would disable the gate.
    fn handle_ack(&mut self, message: &str) {
        let Some(display_id) = self.display_id.clone() else {
            return;
        };
        let mut parts = message.split(' ').skip(1);
        let parsed = (|| -> Option<(u16, f64)> {
            let id: i64 = parts.next()?.parse().ok()?;
            let held = match parts.next() {
                Some(h) => h.parse::<f64>().ok()?.max(0.0),
                None => 0.0,
            };
            (0..=MAX_UINT16_FRAME_ID)
                .contains(&id)
                .then_some((id as u16, held))
        })();
        let Some((frame_id, held_ms)) = parsed else {
            warn!(target: "ws", "Malformed CLIENT_FRAME_ACK from {}: {message}", self.client.remote);
            return;
        };
        let link = {
            let reg = self.service.reg.lock();
            reg.owns(self.client.id, &display_id)
                .then(|| reg.links.get(&display_id).cloned())
                .flatten()
        };
        if let Some(link) = link {
            link.lock().note_ack(frame_id, held_ms, now());
        }
    }

    /// `DECODE_PACE <fps>`: the rate this client's decoder keeps up with, 0 for any; the display
    /// it owns captures no faster. A viewer's is its own gate's to answer, since the capture is
    /// everyone's.
    fn handle_decode_pace(&mut self, rest: &str) {
        let Ok(pace) = rest.trim().parse::<f64>() else {
            return;
        };
        let target = self
            .display_id
            .clone()
            .unwrap_or_else(|| PRIMARY_DISPLAY.to_string());
        if !self.service.reg.lock().owns(self.client.id, &target) {
            return;
        }
        let pace = if pace > 0.0 {
            sanitize("framerate", Some(&json!(pace)))
                .as_ref()
                .and_then(json_f64)
        } else {
            None
        };
        self.service.apply_decode_pace(&target, pace);
    }

    /// `SETTINGS,{json}`: a viewer's joins the stream; a controller's seats it on its display, or
    /// beside the display's owner, and applies what it changed.
    async fn handle_settings(&mut self, payload: &str) -> Flow {
        let parsed = match ClientSettings::parse(payload) {
            Ok(parsed) => parsed,
            Err(e) => {
                error!(target: "ws", "{e}");
                return Flow::Continue(());
            }
        };
        debug!(target: "ws", "Parsed client settings for '{}'", parsed.display_id);
        let service = self.service.clone();
        let id = self.client.id;
        service
            .deps
            .audio
            .client_settings(id, parsed.audio_redundancy.unwrap_or(false));
        if self.client.is_viewer() {
            info!(target: "ws", "Viewer {} joins the 'primary' stream.", self.client.remote);
            self.initial_settings_processed = true;
            if !service.reg.lock().running.contains(PRIMARY_DISPLAY) {
                service.ensure_viewer_capture().await;
            }
            service.broadcast_stream_resolution().await;
            // Only the joining viewer is reset; the IDR opens its key frame gate now, since an
            // infinite GOP schedules none.
            say(&self.client, "PIPELINE_RESETTING primary").await;
            service.schedule_idr(PRIMARY_DISPLAY);
            return Flow::Continue(());
        }
        let display_id = parsed.display_id.clone();
        if display_id != PRIMARY_DISPLAY {
            // The published setting can lag a host-side change; re-read first.
            service.refresh_second_screen_capacity().await;
            let (available, reason) = service.second_screen_availability();
            if !available {
                warn!(target: "ws", "Client from {} attempted to connect as secondary display ('{display_id}'), but it is unavailable: {reason} Rejecting connection.", self.client.remote);
                if say(&self.client, format!("KILL {reason}")).await {
                    self.client
                        .close(1008, crate::ws::server::SECOND_SCREEN_UNAVAILABLE)
                        .await;
                }
                return Flow::Break(());
            }
        }
        self.display_id = Some(display_id.clone());
        self.tab_id = parsed.tab_id.clone();
        if display_id == PRIMARY_DISPLAY && !self.visibility_asked {
            // Its tab hidden may hand the display to a page beside it; an older page ignores it.
            self.visibility_asked = true;
            say(&self.client, "PAGE_VISIBILITY").await;
        }
        if display_id == PRIMARY_DISPLAY && service.joins_beside_owner(id, self.tab_id.as_deref()) {
            self.join_beside_owner(&parsed).await;
            return Flow::Continue(());
        }
        if display_id == PRIMARY_DISPLAY
            && let Some(tab) = &self.tab_id
            && !service.reg.lock().owns(id, PRIMARY_DISPLAY)
        {
            // The page that takes the primary is in use.
            service.st.lock().tab_used_at.insert(tab.clone(), now());
        }
        self.kill_superseded(&display_id).await;
        if display_id != PRIMARY_DISPLAY {
            self.deactivate_other_secondary(&display_id).await;
        }
        self.seat(&display_id, &parsed).await;
        if display_id == PRIMARY_DISPLAY {
            let mut reg = service.reg.lock();
            if let Some(audio) = reg.beside_audio.remove(&id) {
                // Owning the display, its own audio is the session's now.
                reg.session_audio = Some(audio);
            }
        }
        let initial = !self.initial_settings_processed;
        let applied = {
            let mut st = service.st.lock();
            let applied = if initial {
                parsed.clone()
            } else {
                settings_changed_by(st.page_settings.get(&id), &parsed)
            };
            st.page_settings.insert(id, parsed);
            applied
        };
        service
            .apply_client_settings(&self.client, applied, initial)
            .await;
        let beside: Vec<ClientId> = service
            .reg
            .lock()
            .co_controllers
            .iter()
            .map(|(c, _)| *c)
            .collect();
        if display_id == PRIMARY_DISPLAY {
            service.tell_display_settings(&display_id, &beside).await;
        }
        if initial {
            self.first_settings_applied(&display_id).await;
        }
        Flow::Continue(())
    }

    /// Streamed as a viewer is, with its input taken as a controller's; the owner keeps the
    /// display, and the page starts on what it streams with.
    async fn join_beside_owner(&mut self, parsed: &ClientSettings) {
        let service = self.service.clone();
        let id = self.client.id;
        let tab = self.tab_id.clone();
        let fresh = {
            let mut reg = service.reg.lock();
            if reg.is_beside(id) {
                false
            } else {
                reg.co_controllers.push((id, tab.clone()));
                true
            }
        };
        if !fresh {
            service.take_pick_beside_owner(&self.client, parsed).await;
            return;
        }
        let owner_tab = service
            .st
            .lock()
            .displays
            .get(PRIMARY_DISPLAY)
            .and_then(|d| d.tab_id.clone());
        let short = |t: &Option<String>| {
            t.as_deref()
                .unwrap_or("none")
                .chars()
                .take(8)
                .collect::<String>()
        };
        info!(target: "ws", "Controller {} joins 'primary' beside its owner (tab {}, the owner's {}).",
            self.client.remote, short(&tab), short(&owner_tab));
        if !service.reg.lock().running.contains(PRIMARY_DISPLAY) {
            service.capture_for_page_beside(self.client.clone());
        }
        service.broadcast_stream_resolution().await;
        say(&self.client, "PIPELINE_RESETTING primary").await;
        service.schedule_idr(PRIMARY_DISPLAY);
        service.tell_display_settings(PRIMARY_DISPLAY, &[id]).await;
    }

    /// A second controller of the same tab, or of none, takes an owned display over: the old
    /// socket is killed, each step bounded since it is the one most likely frozen. The entry is
    /// repointed before the close yields, so the superseded handler's teardown finds nothing it
    /// owns and does not stop the capture being taken over.
    async fn kill_superseded(&mut self, display_id: &str) {
        if display_id != PRIMARY_DISPLAY && display_id != "display2" {
            return;
        }
        let service = self.service.clone();
        let id = self.client.id;
        let old = {
            let mut reg = service.reg.lock();
            let old = reg
                .owners
                .get(display_id)
                .copied()
                .filter(|o| *o != id)
                .and_then(|o| reg.clients.get(&o).cloned());
            let old = old.filter(|o| !o.closed());
            if old.is_some() {
                reg.owners.insert(display_id.to_string(), id);
            }
            old
        };
        let Some(old) = old else {
            return;
        };
        let reason = format!("a new {display_id} client connected connection killed");
        let old_tab = service
            .st
            .lock()
            .displays
            .get(display_id)
            .and_then(|d| d.tab_id.clone());
        let short = |t: &Option<String>| {
            t.as_deref()
                .unwrap_or("none")
                .chars()
                .take(8)
                .collect::<String>()
        };
        warn!(target: "ws", "Killing old client for '{display_id}' at {}. Reason: {reason} (tab {}, the new page's {})",
            old.remote, short(&old_tab), short(&self.tab_id));
        service.apply_decode_pace(display_id, None);
        if old
            .send_bounded(Frame::text(format!("KILL {reason}")), BOUNDED_SEND_SECONDS)
            .await
            .is_ok()
        {
            old.close(1000, "Superseded by new client").await;
        }
    }

    /// A new secondary deactivates an existing different one: exactly one secondary at a time.
    async fn deactivate_other_secondary(&mut self, display_id: &str) {
        let service = self.service.clone();
        let id = self.client.id;
        let other = {
            let reg = service.reg.lock();
            reg.owners
                .iter()
                .find(|(d, owner)| {
                    d.as_str() != PRIMARY_DISPLAY && d.as_str() != display_id && **owner != id
                })
                .map(|(d, owner)| (d.clone(), reg.clients.get(owner).cloned()))
        };
        let Some((other_id, owner)) = other else {
            return;
        };
        warn!(target: "ws", "New secondary display '{display_id}' connected. Deactivating old secondary '{other_id}'.");
        service.stop_capture_for_display(&other_id).await;
        if let Some(d) = service.st.lock().displays.get_mut(&other_id) {
            d.video_active = false;
        }
        if let Some(owner) = owner {
            let _ = owner
                .send_bounded(Frame::text("VIDEO_STOPPED"), BOUNDED_SEND_SECONDS)
                .await;
        }
    }

    /// Register a new display entry, or take over an existing one: a warm takeover keeps the
    /// capture, so its reset and IDR go here, where no reconfigure may run.
    async fn seat(&mut self, display_id: &str, parsed: &ClientSettings) {
        let service = self.service.clone();
        let id = self.client.id;
        let exists = service.st.lock().displays.contains_key(display_id);
        if exists {
            debug!(target: "ws", "Client is taking over existing display '{display_id}'. Updating state for new connection.");
            service.seat_display_page(display_id, id, self.tab_id.clone());
            // Only a page's first SETTINGS reactivates video; a later one must not resurrect a
            // stream stopped with STOP_VIDEO.
            if !self.initial_settings_processed {
                let active = service.video_start_state(id, display_id);
                if let Some(d) = service.st.lock().displays.get_mut(display_id) {
                    d.video_active = active;
                }
                if let Some(link) = service.link(display_id) {
                    link.lock().forget_path();
                }
            }
            say(&self.client, format!("PIPELINE_RESETTING {display_id}")).await;
            service.schedule_idr(display_id);
            return;
        }
        debug!(target: "ws", "Registering new client for display: {display_id}");
        let video_active = service.video_start_state(id, display_id);
        let dpi = service
            .deps
            .display
            .applied_dpi()
            .map(|d| d.to_string())
            .unwrap_or_else(|| settings().scaling_dpi().to_string());
        let mut link = crate::ws::link::DisplayLink::new();
        link.expect_key_frame(now());
        {
            let mut st = service.st.lock();
            let seeds = st.seeds.clone();
            let entry = DisplayState::new(&seeds, self.tab_id.clone(), video_active, Some(dpi));
            // A steered display starts at the rate its path last carried for this page, else
            // climbs to its target from `START_KBPS`.
            if settings().congestion_control().on && seeds.rc_mode == "cbr" {
                let (lo, hi) = settings().video_bitrate();
                let remembered = parsed.cc_start_kbps.as_ref();
                let seeded = crate::ws::steer::start_kbps(remembered, lo.as_f64(), hi.as_f64());
                if seeded < seeds.video_bitrate {
                    link.link_kbps = Some(seeded);
                    let from = if crate::ws::steer::remembered_kbps(remembered).is_some() {
                        "from the rate its page last held"
                    } else {
                        "climbing to its target"
                    };
                    info!(target: "ws", "Congestion control[{display_id}]: starting at {seeded:.0} kbps, {from}");
                }
            }
            st.displays.insert(display_id.to_string(), entry);
        }
        {
            let mut reg = service.reg.lock();
            reg.owners.insert(display_id.to_string(), id);
            reg.links.insert(
                display_id.to_string(),
                Arc::new(parking_lot::Mutex::new(link)),
            );
        }
        // The page stops being a capture candidate with its socket still open.
        service.deps.webcam.demand_sync(service.clone()).await;
        if WsService::is_wayland() {
            // The ladder runs from the configured DPI before any client sync, so the first
            // capture starts at the intended scale.
            let dpi = settings().scaling_dpi().to_string();
            let scale = service
                .deps
                .display
                .realize_wayland_dpi(&dpi, display_id, None)
                .await;
            if let Some(d) = service.st.lock().displays.get_mut(display_id) {
                d.scale = scale;
            }
        }
    }

    /// What closes a page's first `SETTINGS`: the summary line, a settled description, and the
    /// audio start policy on its own task, since the start asks the sound server for the capture
    /// sink and one that never answers would hold every input frame this loop has yet to read.
    async fn first_settings_applied(&mut self, display_id: &str) {
        self.initial_settings_processed = true;
        let service = self.service.clone();
        info!(target: "ws", "{}", service.settings_applied_summary(&self.client.remote.to_string(), display_id));
        let settled = service
            .reg
            .lock()
            .watches
            .get(display_id)
            .and_then(|w| w.info());
        if let Some(info) = settled {
            service.publish_stream_info(display_id, info).await;
        }
        let wanted = service
            .st
            .lock()
            .displays
            .get(display_id)
            .is_some_and(|d| d.video_active);
        if wanted && !service.reg.lock().running.contains(display_id) {
            error!(target: "ws", "FATAL: Initial reconfiguration completed, but video pipeline did not start.");
        }
        let task_service = service.clone();
        let did = display_id.to_string();
        let id = self.client.id;
        self.initial_audio_task = Some(tokio::spawn(async move {
            let others = !task_service.reg.lock().audio_listeners(Some(id)).is_empty();
            task_service
                .deps
                .audio
                .apply_initial_policy(&did, false, others)
                .await;
        }));
    }

    /// `START_VIDEO`: a shared client or a page beside the owner is throttled, a resume held to
    /// its IDR floor and deferred if too soon; the owner's restarts its display's capture, and a
    /// shared client needs a decode entry point, not a pipeline rebuild, unless nothing runs.
    async fn handle_start_video(&mut self) {
        let service = self.service.clone();
        let id = self.client.id;
        let (was_paused, beside) = {
            let reg = service.reg.lock();
            (reg.video_paused.contains(&id), reg.is_beside(id))
        };
        if beside || self.client.is_viewer() {
            let at = now();
            let last = service
                .st
                .lock()
                .last_start_video
                .get(&id)
                .copied()
                .unwrap_or(f64::NEG_INFINITY);
            if was_paused {
                if at - last < VIEWER_RESUME_MIN_INTERVAL_S {
                    warn!(target: "ws", "Throttled rapid resume from viewer {}; deferring its rejoin.", self.client.remote);
                    service.schedule_deferred_viewer_rejoin(
                        &self.client,
                        VIEWER_RESUME_MIN_INTERVAL_S - (at - last),
                    );
                    return;
                }
            } else if at - last < VIEWER_START_THROTTLE_S {
                warn!(target: "ws", "Throttled START_VIDEO request from viewer {}. Ignoring.", self.client.remote);
                return;
            }
            service.st.lock().last_start_video.insert(id, at);
        }
        if was_paused {
            service.reg.lock().video_paused.remove(&id);
            info!(target: "ws", "START_VIDEO from resuming client ({}): rejoining its video feed.", self.client.remote);
        }
        let entry = self
            .display_id
            .clone()
            .filter(|d| !beside && service.st.lock().displays.contains_key(d));
        let Some(display_id) = entry else {
            self.start_shared_video().await;
            return;
        };
        if !service.reg.lock().owns(id, &display_id) {
            // A superseded connection (reload overlap) must not drive its successor's stream.
            debug!(target: "ws", "Ignoring START_VIDEO for '{display_id}' from a superseded connection.");
            return;
        }
        info!(target: "ws", "Received START_VIDEO for '{display_id}'. Starting its stream.");
        let resumed = service.reg.lock().running.contains(&display_id);
        let layout = {
            let mut st = service.st.lock();
            if let Some(d) = st.displays.get_mut(&display_id) {
                d.video_active = true;
            }
            st.layouts.get(&display_id).copied()
        };
        if let Some(layout) = layout {
            if !service.start_capture_for_display(&display_id, layout).await {
                // A full reconfigure instead of a false VIDEO_STARTED.
                warn!(target: "ws", "Capture start failed for '{display_id}'; reconfiguring.");
                service.reconfigure_displays().await;
                return;
            }
            service.start_backpressure_task(&display_id);
            if resumed {
                say(&self.client, format!("PIPELINE_RESETTING {display_id}")).await;
            }
            say(&self.client, "VIDEO_STARTED").await;
            // The client clears its cursor canvas on tab hide.
            service.send_current_cursor(&self.client).await;
            return;
        }
        warn!(target: "ws", "No layout found for '{display_id}' on START_VIDEO. Performing full reconfiguration.");
        service.reconfigure_displays().await;
        let live = service.reg.lock().running.contains(&display_id)
            && service
                .capture_of(&display_id)
                .is_some_and(|c| c.is_capturing());
        if live {
            say(&self.client, "VIDEO_STARTED").await;
            service.send_current_cursor(&self.client).await;
        } else {
            warn!(target: "ws", "Reconfigure did not start a live capture for '{display_id}'; not acking VIDEO_STARTED.");
        }
    }

    async fn start_shared_video(&mut self) {
        let service = self.service.clone();
        if service.reg.lock().running.contains(PRIMARY_DISPLAY) {
            debug!(target: "ws", "START_VIDEO from shared client ({}): sending reset + IDR.", self.client.remote);
        } else {
            info!(target: "ws", "START_VIDEO from shared client ({}) with no active capture. Starting primary capture.", self.client.remote);
            if !service.ensure_viewer_capture().await {
                // A no-op with zero display clients.
                service.reconfigure_displays().await;
                return;
            }
        }
        say(&self.client, "PIPELINE_RESETTING primary").await;
        service.schedule_idr(PRIMARY_DISPLAY);
        service.send_current_cursor(&self.client).await;
    }

    /// `STOP_VIDEO`: the controller hiding its tab must not stop an encoder shared viewers still
    /// consume; only its own socket pauses then. Always answered `VIDEO_STOPPED`.
    async fn handle_stop_video(&mut self) {
        let service = self.service.clone();
        let id = self.client.id;
        let beside = service.reg.lock().is_beside(id);
        let entry = self
            .display_id
            .clone()
            .filter(|d| !beside && service.st.lock().displays.contains_key(d));
        match entry {
            Some(display_id) if !service.reg.lock().owns(id, &display_id) => {
                // A dying page's tab-hide can arrive after the reloaded page owns the display.
                debug!(target: "ws", "Ignoring STOP_VIDEO for '{display_id}' from a superseded connection.");
            }
            Some(display_id) => {
                service.cancel_deferred_rejoin(id);
                let remaining = if display_id == PRIMARY_DISPLAY {
                    service.reg.lock().active_primary_consumers(Some(id)).len()
                } else {
                    0
                };
                if remaining > 0 {
                    info!(target: "ws", "STOP_VIDEO for 'primary' with {remaining} shared viewer(s) attached: pausing the controller, keeping the capture.");
                    service.reg.lock().video_paused.insert(id);
                } else {
                    info!(target: "ws", "Received STOP_VIDEO for '{display_id}'. Stopping stream.");
                    if let Some(d) = service.st.lock().displays.get_mut(&display_id) {
                        d.video_active = false;
                    }
                    service.stop_capture_for_display(&display_id).await;
                }
            }
            None => {
                service.cancel_deferred_rejoin(id);
                service.reg.lock().video_paused.insert(id);
                info!(target: "ws", "STOP_VIDEO from shared client ({}): pausing its video feed.", self.client.remote);
                service
                    .stop_primary_if_unconsumed("Last unpaused consumer of 'primary' hid its tab.")
                    .await;
            }
        }
        say(&self.client, "VIDEO_STOPPED").await;
    }

    /// `START_AUDIO`, its own task after the first controller's settings: a start the sound
    /// server is slow to answer holds up the audio alone. A page beside the owner starts its own
    /// audio; the owner's starts its own and the viewers'. A re-request supersedes a pending one.
    fn handle_start_audio(&mut self) {
        if let Some(task) = self.start_audio_task.take() {
            task.abort();
        }
        let service = self.service.clone();
        let client = self.client.clone();
        self.start_audio_task = Some(tokio::spawn(async move {
            service.wait_settings_received().await;
            if !settings().audio_enabled().on {
                debug!(target: "ws", "START_AUDIO: Audio is disabled by server settings. Sending AUDIO_DISABLED.");
                let _ = client.send(Frame::text("AUDIO_DISABLED")).await;
                return;
            }
            if !service.deps.audio.is_capturing() && !service.deps.audio.start().await {
                return;
            }
            let told = {
                let mut reg = service.reg.lock();
                if reg.is_beside(client.id) {
                    reg.beside_audio.insert(client.id, true);
                    vec![client.clone()]
                } else {
                    reg.session_audio = Some(true);
                    reg.clients
                        .values()
                        .filter(|c| !reg.is_beside(c.id))
                        .cloned()
                        .collect()
                }
            };
            service.tell(&told, "AUDIO_STARTED").await;
        }));
    }

    /// `STOP_AUDIO`: off the loop like `START_AUDIO`, taken in order behind that start and left
    /// to finish on disconnect.
    fn handle_stop_audio(&mut self) {
        let service = self.service.clone();
        let client = self.client.clone();
        tokio::spawn(async move {
            let told = {
                let mut reg = service.reg.lock();
                if reg.is_beside(client.id) {
                    reg.beside_audio.insert(client.id, false);
                    vec![client.clone()]
                } else {
                    reg.session_audio = Some(false);
                    reg.clients
                        .values()
                        .filter(|c| !reg.is_beside(c.id))
                        .cloned()
                        .collect()
                }
            };
            if service.deps.audio.is_capturing() && !service.audio_wanted() {
                service.deps.audio.stop().await;
            }
            service.tell(&told, "AUDIO_STOPPED").await;
        });
    }

    /// The input dialect's gates, then the input stack: the gamepad slot (read live from the
    /// token table, so a re-slotted token lands on its next message), the secure-input check on
    /// the first comma-separated token, and a page beside the owner's `r,`/`s,`, which are the
    /// owner's. A page on no display has nothing to resize.
    async fn handle_input(&mut self, message: &str, perms: &Perms) {
        let service = self.service.clone();
        let secure = service.deps.tokens.is_secure();
        if message.starts_with("js,") {
            let slot = if secure {
                perms
                    .token
                    .as_deref()
                    .and_then(|t| service.deps.tokens.lookup(t))
                    .map(|e| e.slot)
                    .unwrap_or(Json::Null)
            } else {
                perms.slot.clone()
            };
            let role = if perms.is_viewer() {
                Role::Viewer
            } else {
                Role::Controller
            };
            if crate::gamepad::slot_denied(
                message,
                role,
                Some(&slot).filter(|s| !s.is_null()),
                secure,
            ) {
                let head: String = message.chars().take(32).collect();
                warn!(target: "ws", "DENIED gamepad input from {}: {head} does not match slot {slot}.", self.client.remote);
                return;
            }
        }
        if secure
            && SECURE_INPUT_PREFIXES.contains(&message.split(',').next().unwrap_or(""))
            && !service.holds_input_authority(&perms.role, perms.token.as_deref())
        {
            return;
        }
        let sizing = message.starts_with("r,") || message.starts_with("s,");
        if sizing && (self.display_id.is_none() || service.reg.lock().is_beside(self.client.id)) {
            return;
        }
        if let Some(input) = service.input() {
            let display = self.display_id.as_deref().unwrap_or(PRIMARY_DISPLAY);
            input.on_message(message, display, self.client.id).await;
        }
        if self.display_id.as_deref() == Some(PRIMARY_DISPLAY) && page_put_to_use(message) {
            // After the input, which lands on the layout its page shows.
            if let Some(tab) = &self.tab_id {
                service.st.lock().tab_used_at.insert(tab.clone(), now());
            }
            if service.reg.lock().is_beside(self.client.id) {
                service.take_display_in_use(&self.client).await;
            }
        }
    }

    /// The connection's cleanup: it leaves every table, its gamepads and held input are released
    /// by the rule in the module docs, and a display it owned waits out the reconnect grace.
    pub(crate) async fn teardown(&mut self) {
        let service = self.service.clone();
        let id = self.client.id;
        let perms = self.client.perms();
        debug!(target: "ws", "Cleaning up Data WS handler for {} (Display ID: {:?})...", self.client.remote, self.display_id);
        service.cancel_deferred_rejoin(id);
        let (relays, had_own_audio, owned_display) = {
            let mut reg = service.reg.lock();
            reg.clients.remove(&id);
            reg.repair_times.remove(&(id, "keyframe"));
            reg.repair_times.remove(&(id, "lost"));
            reg.video_paused.remove(&id);
            reg.stats_subscribers.remove(&id);
            reg.bulk.remove(&id);
            reg.co_controllers.retain(|(c, _)| *c != id);
            let relays: Vec<_> = reg
                .relays
                .values_mut()
                .filter_map(|g| g.remove(&id))
                .collect();
            let had_own_audio = reg.beside_audio.remove(&id).unwrap_or(false);
            let owned = reg
                .owners
                .iter()
                .find(|(_, o)| **o == id)
                .map(|(d, _)| d.clone());
            (relays, had_own_audio, owned)
        };
        session_end(&perms);
        {
            let mut st = service.st.lock();
            st.last_start_video.remove(&id);
            st.page_settings.remove(&id);
            st.page_hidden_at.remove(&id);
            st.page_shown_at.remove(&id);
            let stale = now() - 3600.0;
            st.tab_used_at.retain(|_, at| *at >= stale);
        }
        service.deps.webcam.demand_sync(service.clone()).await;
        let input = service.input();
        if let Some(input) = &input {
            // A tab that dies mid-press never sends `js,d`.
            input.release_gamepads_for_conn(id).await;
        }
        // The release rule is in the module docs: primary owner, or last input-capable client.
        let is_primary_owner = service.reg.lock().owns(id, PRIMARY_DISPLAY);
        let others_hold = service
            .clients()
            .iter()
            .any(|c| service.holds_input_authority(&c.role(), c.token().as_deref()));
        let departing_authority = service
            .holds_input_authority(&perms.role, perms.token.as_deref())
            && (is_primary_owner || !others_hold);
        // A cut (no close frame: an ingress retiring, the network) leaves the page to reconnect
        // in place, still holding what it held: its keys and buttons stay down through the grace
        // and are let go only if it does not come back.
        let close_code = self.client.close_code();
        let hold_input = departing_authority && close_code == Some(1006) && owned_display.is_some();
        if departing_authority
            && !hold_input
            && let Some(input) = &input
        {
            input.release_held_input().await;
        }
        // Now rather than at the next fan-out: an idle capture sends no chunk to prune a relay.
        for relay in relays {
            relay.stop();
        }
        // A departing plain-only page may let the rest enable RED; a page beside the owner that
        // kept the capture running for itself ends it.
        service
            .deps
            .audio
            .client_left(id, had_own_audio && !service.audio_wanted())
            .await;

        if let Some(display_id) = owned_display {
            // Deferred: a reloading page takes the entry over with its capture warm. The code
            // says who ended it: 1000/1001 the page, 1006 a cut with no close frame, 4xxx this
            // server.
            let code = close_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "None".into());
            info!(target: "ws", "Client for '{display_id}' disconnected (code {code}). Deferring display teardown by {:.0}s for a possible reconnect.",
                service.timings.reconnect_grace_s);
            tokio::spawn(teardown_if_unclaimed(
                service.clone(),
                display_id,
                id,
                hold_input,
            ));
        } else {
            debug!(target: "ws", "Unregistered client at {} disconnected. No display reconfiguration needed.", self.client.remote);
            // Nothing else stops the primary capture for a socket owning no display.
            service
                .stop_primary_if_unconsumed("Last unpaused consumer of 'primary' disconnected.")
                .await;
        }
        for task in [self.start_audio_task.take(), self.initial_audio_task.take()]
            .into_iter()
            .flatten()
        {
            task.abort();
        }
        if let Some(mut mic) = self.mic.take() {
            mic.close().await;
        }
        let owned_any = service.reg.lock().owners.values().any(|o| *o == id);
        if !owned_any && service.reg.lock().clients.is_empty() {
            info!(target: "ws", "Last client ({}) gone; stopping the capture pipelines.", self.client.remote);
            service.stop_stats_collectors().await;
            service.st.lock().capture_cursor = false;
            service.st.lock().last_keyframe_request.clear();
            service.shutdown_pipelines().await;
        }
        debug!(target: "ws", "Data WS handler for {} finished all cleanup.", self.client.remote);
    }
}

/// The reconnect grace of a display whose socket left: claimed by a new connection, nothing
/// happens; a connection newer than the disconnect still in its handshake extends the wait;
/// then the oldest page beside it is promoted, and only with nobody left does the entry go.
pub(crate) async fn teardown_if_unclaimed(
    service: Arc<WsService>,
    display_id: String,
    dead: ClientId,
    held: bool,
) {
    let mut disconnect_ts = now();
    let mut deadline = disconnect_ts + GRACE_EXTENDED_S;
    let claimed = |service: &WsService| !service.reg.lock().owns(dead, &display_id);
    loop {
        tokio::time::sleep(Duration::from_secs_f64(service.timings.reconnect_grace_s)).await;
        if claimed(&service) {
            info!(target: "ws", "Display '{display_id}' was claimed by a new connection during the grace period; teardown skipped.");
            return;
        }
        let latest = service
            .st
            .lock()
            .last_connections
            .iter()
            .map(|(_, t)| *t)
            .fold(0.0, f64::max);
        if latest > disconnect_ts && now() < deadline {
            continue;
        }
        if service.promote_co_controller(&display_id).await {
            // Its SETTINGS claims the entry; waited for as a reconnect is.
            disconnect_ts = now();
            deadline = disconnect_ts + GRACE_EXTENDED_S;
            continue;
        }
        break;
    }
    if claimed(&service) {
        info!(target: "ws", "Display '{display_id}' was claimed by a new connection during the grace period; teardown skipped.");
        return;
    }
    if held && let Some(input) = service.input() {
        input.release_held_input().await;
    }
    service.st.lock().displays.remove(&display_id);
    {
        let mut reg = service.reg.lock();
        reg.owners.remove(&display_id);
        reg.links.remove(&display_id);
    }
    service.deps.webcam.demand_sync(service.clone()).await;
    info!(target: "ws", "Client for '{display_id}' did not return within the grace period. Removing and triggering full display reconfiguration.");
    service.reconfigure_displays().await;
    // A viewer-started capture has no owning display client, so the reconfigure never stops it.
    service
        .stop_primary_if_unconsumed(
            "No unpaused consumer of 'primary' left after the grace period.",
        )
        .await;
    if service.reg.lock().clients.is_empty() {
        info!(target: "ws", "Last client gone after the grace period. Tearing down singleton collectors and pipelines.");
        service.stop_stats_collectors().await;
        service.st.lock().capture_cursor = false;
        service.st.lock().last_keyframe_request.clear();
        service.shutdown_pipelines().await;
    }
}

/// One audit record with its fields.
fn audit(event: &str, fields: Json) {
    if let Json::Object(fields) = fields {
        crate::http::audit::emit(event, fields);
    }
}

/// Record the end of a page's connection.
fn session_end(perms: &Perms) {
    let mut fields = Map::new();
    fields.insert("transport".into(), json!("websockets"));
    fields.insert("role".into(), json!(perms.role));
    fields.insert("slot".into(), perms.slot.clone());
    fields.insert(
        "duration_s".into(),
        json!(((session_time() - perms.connected_at) * 1000.0).round() / 1000.0),
    );
    crate::http::audit::emit("session.disconnect", fields);
}
