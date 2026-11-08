/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The WebSocket upgrade of an HTTP/1.1 request, and the socket on the connection it hands
//! over. tokio-websockets writes a frame's header and its payload in one vectored write, so a
//! payload goes to the kernel from the buffer that holds it.

use std::future::Future;

use aws_lc_rs::digest::{SHA1_FOR_LEGACY_USE_ONLY, digest};
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use hyper::upgrade::{OnUpgrade, Upgraded};
use hyper_util::rt::TokioIo;
use log::debug;
use tokio_websockets::{Limits, ServerBuilder, WebSocketStream};

/// The server's end of an upgraded WebSocket connection.
pub type WebSocket = WebSocketStream<TokioIo<Upgraded>>;

/// RFC 6455's GUID, hashed after the client's key into `Sec-WebSocket-Accept`.
const ACCEPT_GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Why a request cannot become a WebSocket.
#[derive(Debug)]
pub struct UpgradeRejection {
    status: StatusCode,
    reason: &'static str,
}

impl IntoResponse for UpgradeRejection {
    fn into_response(self) -> Response {
        (self.status, self.reason).into_response()
    }
}

/// A request asking to become a WebSocket.
pub struct WebSocketUpgrade {
    on_upgrade: OnUpgrade,
    accept: HeaderValue,
    offered: Option<HeaderValue>,
    protocol: Option<HeaderValue>,
    max_message: Option<usize>,
}

/// Whether one of the comma-separated values of `name` is `token`, ignoring case.
fn lists(headers: &HeaderMap, name: HeaderName, token: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|value| value.trim().eq_ignore_ascii_case(token))
}

impl<S: Send + Sync> FromRequestParts<S> for WebSocketUpgrade {
    type Rejection = UpgradeRejection;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let refuse = |status, reason| Err(UpgradeRejection { status, reason });
        if parts.method != Method::GET {
            return refuse(
                StatusCode::METHOD_NOT_ALLOWED,
                "Request method must be `GET`",
            );
        }
        if !lists(&parts.headers, header::CONNECTION, "upgrade") {
            return refuse(
                StatusCode::BAD_REQUEST,
                "`Connection` header did not include 'upgrade'",
            );
        }
        if !lists(&parts.headers, header::UPGRADE, "websocket") {
            return refuse(
                StatusCode::BAD_REQUEST,
                "`Upgrade` header did not include 'websocket'",
            );
        }
        if parts
            .headers
            .get(header::SEC_WEBSOCKET_VERSION)
            .is_none_or(|v| v != "13")
        {
            return refuse(
                StatusCode::BAD_REQUEST,
                "`Sec-WebSocket-Version` header did not include '13'",
            );
        }
        let Some(key) = parts.headers.get(header::SEC_WEBSOCKET_KEY) else {
            return refuse(
                StatusCode::BAD_REQUEST,
                "`Sec-WebSocket-Key` header missing",
            );
        };
        let accept = accept_key(key.as_bytes());
        let Some(on_upgrade) = parts.extensions.remove::<OnUpgrade>() else {
            return refuse(
                StatusCode::UPGRADE_REQUIRED,
                "The connection cannot be upgraded",
            );
        };
        Ok(WebSocketUpgrade {
            on_upgrade,
            accept,
            offered: parts.headers.get(header::SEC_WEBSOCKET_PROTOCOL).cloned(),
            protocol: None,
            max_message: None,
        })
    }
}

impl WebSocketUpgrade {
    /// Select the first of `protocols` the client offered.
    pub fn protocols<I>(mut self, protocols: I) -> Self
    where
        I: IntoIterator<Item = &'static str>,
    {
        if let Some(offered) = self.offered.as_ref().and_then(|v| v.to_str().ok()) {
            self.protocol = protocols
                .into_iter()
                .find(|protocol| offered.split(',').any(|o| o.trim() == *protocol))
                .map(HeaderValue::from_static);
        }
        self
    }

    /// The largest message the client may send, and so the largest frame.
    pub fn max_message_size(mut self, bytes: usize) -> Self {
        self.max_message = Some(bytes);
        self
    }

    /// Answer the upgrade, and run `serve` on the socket once hyper hands the connection over.
    pub fn on_upgrade<F, Fut>(self, serve: F) -> Response
    where
        F: FnOnce(WebSocket) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let WebSocketUpgrade {
            on_upgrade,
            accept,
            protocol,
            max_message,
            ..
        } = self;
        tokio::spawn(async move {
            let upgraded = match on_upgrade.await {
                Ok(upgraded) => upgraded,
                Err(e) => {
                    debug!(target: "ws", "WebSocket upgrade failed: {e}");
                    return;
                }
            };
            let mut limits = Limits::default();
            if max_message.is_some() {
                limits = limits.max_payload_len(max_message);
            }
            serve(
                ServerBuilder::new()
                    .limits(limits)
                    .serve(TokioIo::new(upgraded)),
            )
            .await;
        });
        let mut response = StatusCode::SWITCHING_PROTOCOLS.into_response();
        let headers = response.headers_mut();
        headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
        headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        headers.insert(header::SEC_WEBSOCKET_ACCEPT, accept);
        if let Some(protocol) = protocol {
            headers.insert(header::SEC_WEBSOCKET_PROTOCOL, protocol);
        }
        response
    }
}

/// `Sec-WebSocket-Accept` for a client's `Sec-WebSocket-Key` (RFC 6455 4.2.2).
fn accept_key(key: &[u8]) -> HeaderValue {
    let hashed = digest(&SHA1_FOR_LEGACY_USE_ONLY, &[key, ACCEPT_GUID].concat());
    let encoded = base64::engine::general_purpose::STANDARD.encode(hashed.as_ref());
    HeaderValue::from_str(&encoded).expect("base64 is a valid header value")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_accept_key_is_rfc_6455s() {
        assert_eq!(
            accept_key(b"dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }
}
