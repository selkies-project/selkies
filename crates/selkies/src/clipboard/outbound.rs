/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The session's clipboard on its way to the clients: the frames, their chunking, who
//! receives them, and which payload wins when two cross.
//!
//! A payload under `CHUNK_SIZE` goes as one `clipboard,<base64>` or
//! `clipboard_binary,<mime>,<base64>` frame; a larger one as `clipboard_start,<mime>,<size>`,
//! `clipboard_data,<base64>` per chunk, and `clipboard_finish`. A secret's frames follow a
//! `clipboard_secret` frame and an answer to a client's fetch a `clipboard_reply,<verb>`
//! frame, both on the same ordered socket, so the client needs no time heuristics. Each
//! chunk waits for the host's bulk window, which is the end-to-end gauge and the local send
//! queue together, so the audio and input sharing the connection are never queued behind more
//! than one of them.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use log::{debug, warn};

use super::{Clipboard, FLAVORS_MIME, Payload, prune};
use crate::core::Role;
use crate::settings::WS_MAX_MESSAGE_BYTES;

/// A connected client, as the host identifies it.
pub type ClientId = u64;

/// Raw bytes per multipart clipboard frame, sized for latency rather than capacity: a chunk
/// is the unit at which the audio and input sharing the socket can overtake a transfer. A
/// multiple of 3, so chunks concatenate as base64.
pub const CHUNK_SIZE: usize = 16 * 1024 / 3 * 3;

const _: () = assert!(
    CHUNK_SIZE / 3 * 4 + 32 < WS_MAX_MESSAGE_BYTES,
    "a clipboard frame fits the message ceiling"
);

/// What the clipboard needs from the transport that carries it.
#[async_trait]
pub trait ClipboardHost: Send + Sync + 'static {
    /// Every connected client and its role.
    fn clients(&self) -> Vec<(ClientId, Role)>;

    /// Send one text frame to `client` with the liveness bound a control frame gets; false
    /// when the client is gone or the send failed, which ends its transfer.
    async fn send_text(&self, client: ClientId, message: String) -> bool;

    /// Wait until `client`'s path admits a bulk chunk of `len` bytes: the end-to-end gauge
    /// and the local send queue.
    async fn bulk_window(&self, client: ClientId, len: usize);

    /// Whether any connected client would receive an outbound clipboard send.
    fn has_consumers(&self) -> bool {
        !self.clients().is_empty()
    }

    /// Record a clipboard transfer for the audit collector; the content never travels.
    fn audit(&self, event: &str, mime: &str, size: usize, multipart: Option<bool>) {
        let _ = (event, mime, size, multipart);
    }
}

/// Per-client send state: one payload at a time per client, and the newest announcement each
/// was given, so a send racing another cannot interleave two payloads' chunks into one
/// assembly and an older announcement still in flight stops.
#[derive(Default)]
pub(crate) struct Sends {
    locks: HashMap<ClientId, Arc<tokio::sync::Mutex<()>>>,
    latest: HashMap<ClientId, u64>,
}

impl Clipboard {
    /// Send clipboard content to the session's clients, multipart when large.
    ///
    /// Args:
    ///     payload: The content; anything but `text/plain` is binary and, unless it is markup
    ///         or the flavor envelope, gated on the binary switch.
    ///     reply_to: The requesting verb (`cr`) when this answers a client fetch rather than
    ///         announcing a change; a `clipboard_reply,<verb>` frame then precedes the payload.
    ///     client: The client that asked. An answer goes to that client alone: every other
    ///         already holds the content or is about to be told of a change, and a tagged
    ///         reply they did not ask for would be cached without ever reaching their
    ///         clipboard. An announcement goes to every client but the viewers: a viewer's
    ///         page never takes the session's clipboard.
    ///
    /// A newer announcement supersedes an older one still queued or in flight to the same
    /// client, which then gets no further chunks: the clipboard is last-value-wins, and the
    /// client drops a payload whose chunks stop at the next start. A tagged reply is neither
    /// superseded nor supersedes, since its payload is only cached and never pasted.
    pub async fn send(&self, payload: Payload, reply_to: Option<&str>, client: Option<ClientId>) {
        let clients = self.host.clients();
        if clients.is_empty() {
            warn!(target: "ws", "Cannot send clipboard: no clients or server not ready.");
            return;
        }
        let is_binary = !payload.is_text();
        if is_binary
            && !payload.mime.starts_with("text/")
            && payload.mime != FLAVORS_MIME
            && !self.policy().binary
        {
            warn!(
                target: "ws",
                "Attempted to send binary clipboard data ({}) but feature is disabled on server.",
                payload.mime
            );
            return;
        }
        let total = payload.bytes.len();
        if total > 0 {
            self.host
                .audit("clipboard.send", &payload.mime, total, None);
        }
        let live: Vec<ClientId> = clients.iter().map(|(id, _)| *id).collect();
        let recipients: Vec<ClientId> = match client {
            Some(asked) => live.iter().copied().filter(|id| *id == asked).collect(),
            None => clients
                .iter()
                .filter(|(_, role)| *role != Role::Viewer)
                .map(|(id, _)| *id)
                .collect(),
        };
        let announcement = reply_to
            .is_none()
            .then(|| self.announcements.fetch_add(1, Ordering::Relaxed) + 1);
        let locks: Vec<(ClientId, Arc<tokio::sync::Mutex<()>>)> = {
            let mut sends = self.sends.lock();
            prune(&mut sends.locks, &live);
            prune(&mut sends.latest, &live);
            if let Some(a) = announcement {
                for id in &recipients {
                    sends.latest.insert(*id, a);
                }
            }
            recipients
                .iter()
                .map(|id| (*id, sends.locks.entry(*id).or_default().clone()))
                .collect()
        };
        let small = total < CHUNK_SIZE;
        if !small {
            let size = if payload.secret {
                "a secret".to_string()
            } else {
                format!("{total} bytes")
            };
            debug!(target: "ws", "Sending large clipboard data ({}, {size}) via multipart.", payload.mime);
        }
        let payload = Arc::new(payload);
        let deliveries = locks.into_iter().map(|(id, lock)| {
            let payload = payload.clone();
            async move {
                let _serial = lock.lock().await;
                self.deliver(id, &payload, reply_to, announcement).await;
            }
        });
        futures::future::join_all(deliveries).await;
        if !small {
            debug!(target: "ws", "Finished sending multi-part clipboard data.");
        }
    }

    /// Whether `client` was given a newer announcement than `announcement`.
    fn superseded(&self, client: ClientId, announcement: Option<u64>) -> bool {
        match announcement {
            None => false,
            Some(a) => self.sends.lock().latest.get(&client) != Some(&a),
        }
    }

    /// One pipeline per client, a chunk at a time: a slow link paces only its own transfer and
    /// a dead one drops out without touching anyone else's.
    async fn deliver(
        &self,
        client: ClientId,
        payload: &Payload,
        reply_to: Option<&str>,
        announcement: Option<u64>,
    ) {
        if self.superseded(client, announcement) {
            return;
        }
        if let Some(verb) = reply_to
            && !self
                .host
                .send_text(client, format!("clipboard_reply,{verb}"))
                .await
        {
            return;
        }
        if payload.secret && !self.host.send_text(client, "clipboard_secret".into()).await {
            return;
        }
        let total = payload.bytes.len();
        if total < CHUNK_SIZE {
            let encoded = BASE64.encode(&payload.bytes);
            let message = if payload.is_text() {
                format!("clipboard,{encoded}")
            } else {
                format!("clipboard_binary,{},{encoded}", payload.mime)
            };
            self.host.send_text(client, message).await;
            return;
        }
        if !self
            .host
            .send_text(client, format!("clipboard_start,{},{total}", payload.mime))
            .await
        {
            return;
        }
        let mut offset = 0;
        while offset < total {
            let end = (offset + CHUNK_SIZE).min(total);
            let message = format!(
                "clipboard_data,{}",
                BASE64.encode(&payload.bytes[offset..end])
            );
            self.host.bulk_window(client, message.len()).await;
            if self.superseded(client, announcement) {
                return;
            }
            if !self.host.send_text(client, message).await {
                return;
            }
            offset = end;
            tokio::task::yield_now().await;
        }
        self.host.send_text(client, "clipboard_finish".into()).await;
    }
}
