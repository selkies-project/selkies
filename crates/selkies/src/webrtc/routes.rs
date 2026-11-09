/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The signaling socket's route and its bridge from axum to the `PeerRegistry`.
//!
//! Every endpoint lives under `/api`, so the single proxy rule fronting them forwards the socket
//! too. A plain GET is the page's probe before it reloads: answered 204 when the transport is up,
//! rather than with an error status every browser logs as a failed load; 409 is the other transport
//! serving and 503 a service starting or stopping.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use axum::extract::{ConnectInfo, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::{SinkExt, StreamExt};
use selkies_rtc::signaling::{ClientType, Inbound, Outbound, PeerRegistry, socket};
use tokio::sync::mpsc;
use tokio_websockets::Message;

use crate::core::Role;
use crate::http::auth::RoleCeiling;
use crate::http::listen::ConnInfo;
use crate::webrtc::service::WebRtcService;
use crate::ws::client::close_message;
use crate::ws::upgrade::{UpgradeRejection, WebSocket, WebSocketUpgrade};

/// The routes this transport mounts: `/api/ws` and `/api/webrtc/signaling`. `/api/turn` is served by
/// `crate::http`, which reads the service as its `TurnSource`.
pub fn routes<S: Clone + Send + Sync + 'static>(service: Arc<WebRtcService>) -> Router<S> {
    Router::new()
        .route("/api/ws", get(signaling_handler))
        .route("/api/webrtc/signaling", get(signaling_handler))
        .route("/api/webrtc/signaling/", get(signaling_handler))
        .with_state(service)
}

/// `GET /api/ws`: refuse while this transport is inactive, answer the probe, and upgrade.
pub async fn signaling_handler(
    State(service): State<Arc<WebRtcService>>,
    parts: Parts,
    upgrade: Result<WebSocketUpgrade, UpgradeRejection>,
) -> Response {
    if service.current_mode().as_deref() != Some("webrtc") {
        return (StatusCode::CONFLICT, "WebRTC mode is inactive").into_response();
    }
    let Some(registry) = service.peer_registry() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "1")],
            "WebRTC service is starting or stopping",
        )
            .into_response();
    };
    let wants_upgrade = parts
        .headers
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    if !wants_upgrade {
        return StatusCode::NO_CONTENT.into_response();
    }
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(rejection) => return rejection.into_response(),
    };
    let ceiling = parts.extensions.get::<RoleCeiling>().map(|c| {
        if c.0 == Role::Controller {
            ClientType::Controller
        } else {
            ClientType::Viewer
        }
    });
    let raddr = parts
        .extensions
        .get::<ConnInfo>()
        .and_then(|c| c.remote)
        .map(|a| a.to_string())
        .or_else(|| {
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|c| c.0.to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());
    let host = parts
        .headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    upgrade.on_upgrade(move |ws| serve_signaling(registry, ws, raddr, host, ceiling))
}

/// The IP address a request's `Host` header names, with or without its port (`203.0.113.7:8080`,
/// `[2001:db8::1]:8080`, `203.0.113.7`); `None` for a host name.
pub fn entry_address(host: &str) -> Option<IpAddr> {
    let host = host.trim();
    host.parse::<SocketAddr>()
        .map(|address| address.ip())
        .or_else(|_| host.trim_start_matches('[').trim_end_matches(']').parse())
        .ok()
}

/// Serve one signaling WebSocket through the registry until it ends. `host` is the request's
/// `Host` header and `ceiling` the highest role the credential behind the connection grants.
pub async fn serve_signaling(
    registry: Arc<PeerRegistry>,
    ws: WebSocket,
    raddr: String,
    host: Option<String>,
    ceiling: Option<ClientType>,
) {
    let (peer_socket, mut outbox) = socket();
    let (inbound_tx, inbound_rx) = mpsc::channel(1024);
    let (mut sink, mut stream) = ws.split();
    let writer = tokio::spawn(async move {
        while let Some(outbound) = outbox.recv().await {
            let sent = match outbound {
                Outbound::Text(text) => sink.send(Message::text(text)).await,
                Outbound::Ping(data) => sink.send(Message::ping(data)).await,
                Outbound::Pong(data) => sink.send(Message::pong(data)).await,
                Outbound::Close { code, reason } => {
                    let _ = sink.send(close_message(code, &reason)).await;
                    return;
                }
                Outbound::Abort => return,
            };
            if sent.is_err() {
                return;
            }
        }
    });
    let reader = tokio::spawn(async move {
        loop {
            let inbound = match stream.next().await {
                Some(Ok(message)) if message.is_close() => {
                    let _ = inbound_tx.send(Inbound::CloseFrame).await;
                    return;
                }
                Some(Ok(message)) => match message.as_text() {
                    Some(text) => Inbound::Text(text.to_string()),
                    None if message.is_binary() => Inbound::Binary,
                    None if message.is_ping() => Inbound::Ping(message.as_payload().to_vec()),
                    None => Inbound::Pong(message.as_payload().to_vec()),
                },
                Some(Err(_)) | None => {
                    let _ = inbound_tx.send(Inbound::Cut).await;
                    return;
                }
            };
            if inbound_tx.send(inbound).await.is_err() {
                return;
            }
        }
    });
    registry
        .serve(peer_socket, inbound_rx, &raddr, host.as_deref(), ceiling)
        .await;
    reader.abort();
    let _ = writer.await;
}

#[cfg(test)]
mod tests {
    use super::entry_address;

    #[test]
    fn a_host_header_names_its_address_with_or_without_a_port() {
        for (host, address) in [
            ("203.0.113.7:8080", Some("203.0.113.7")),
            ("203.0.113.7", Some("203.0.113.7")),
            ("[2001:db8::1]:8080", Some("2001:db8::1")),
            ("[2001:db8::1]", Some("2001:db8::1")),
            ("desktop.example.test:8080", None),
            ("desktop.example.test", None),
            ("", None),
        ] {
            assert_eq!(
                entry_address(host),
                address.map(|a| a.parse().unwrap()),
                "{host}"
            );
        }
    }
}
