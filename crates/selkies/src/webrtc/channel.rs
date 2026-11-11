/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A peer's input channel as the server writes to it: typed JSON messages through one writer task.
//!
//! Sends on a peer's channel come from many tasks and must not interleave or wait on a slow peer,
//! so each goes through an unbounded queue to one writer task that awaits the association's
//! answer. A payload of 512 bytes or more (cursor PNGs, settings, clipboard, stats) is gzipped for
//! a channel that completed the `_gz,1` handshake, on the writer task; smaller ones are not worth
//! the CPU or the risk to input latency. A message larger than the peer's negotiated
//! `a=max-message-size` is dropped and logged, which beats the peer hard-closing the channel; one
//! that finds the channel no longer open is dropped like any not-ready channel's.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use log::{debug, error};
use selkies_rtc::sctp::{DATA_CHANNEL_BULK_HIGH_WATER, DataChannel, Message, SendError};
use serde_json::{Value as Json, json};
use tokio::sync::mpsc;

/// The bytes a message must reach before it is worth compressing.
pub const GZIP_MIN_BYTES: usize = 512;

/// One message on its way out.
#[derive(Debug, Clone, PartialEq)]
pub enum ChannelOut {
    Text(String),
    Binary(Bytes),
}

/// What sits behind a channel's writer.
#[derive(Clone)]
enum Sink {
    Channel(DataChannel),
    /// A suite's recording of what would have been sent.
    Record,
}

/// The writing end of a peer's input channel.
#[derive(Clone)]
pub struct ChannelTx {
    tx: mpsc::UnboundedSender<ChannelOut>,
    sink: Sink,
    gz: Arc<AtomicBool>,
    open: Arc<AtomicBool>,
}

/// Compress for the wire at gzip's default level.
fn gzip(text: &str) -> Option<Bytes> {
    use std::io::Write as _;
    let mut encoder = flate2::write::GzEncoder::new(
        Vec::with_capacity(text.len() / 2),
        flate2::Compression::new(6),
    );
    encoder.write_all(text.as_bytes()).ok()?;
    encoder.finish().ok().map(Bytes::from)
}

impl ChannelTx {
    /// A channel's writer, on a task of its own.
    pub fn spawn(channel: DataChannel) -> ChannelTx {
        let (tx, mut rx) = mpsc::unbounded_channel::<ChannelOut>();
        let open = Arc::new(AtomicBool::new(true));
        let writer_open = open.clone();
        let writer = channel.clone();
        tokio::spawn(async move {
            while let Some(out) = rx.recv().await {
                let sent = match out {
                    ChannelOut::Text(text) => writer.send_text(text).await,
                    ChannelOut::Binary(bytes) => writer.send_binary(bytes).await,
                };
                match sent {
                    Ok(()) => {}
                    Err(SendError::TooLarge { .. }) => {
                        error!(target: "webrtc", "dropping oversized data channel message: {}", sent.unwrap_err());
                    }
                    Err(e) => {
                        debug!(target: "webrtc", "skipping message because the data channel closed mid-send: {e}");
                        writer_open.store(false, Ordering::Relaxed);
                    }
                }
            }
        });
        ChannelTx {
            tx,
            sink: Sink::Channel(channel),
            gz: Arc::new(AtomicBool::new(false)),
            open,
        }
    }

    /// A writer recording what it sends, for the suites.
    pub fn recording() -> (ChannelTx, mpsc::UnboundedReceiver<ChannelOut>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            ChannelTx {
                sink: Sink::Record,
                tx,
                gz: Arc::new(AtomicBool::new(false)),
                open: Arc::new(AtomicBool::new(true)),
            },
            rx,
        )
    }

    /// Whether the channel still takes messages.
    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Relaxed)
    }

    /// Mark the channel closed; later sends are dropped.
    pub fn mark_closed(&self) {
        self.open.store(false, Ordering::Relaxed);
    }

    /// Mark this channel for gzip'd sends (`_gz,1`).
    pub fn set_gz(&self) {
        self.gz.store(true, Ordering::Relaxed);
    }

    /// Whether the channel completed the gzip handshake.
    pub fn gz(&self) -> bool {
        self.gz.load(Ordering::Relaxed)
    }

    /// Send a string as it is.
    pub fn send_text(&self, text: impl Into<String>) {
        if self.is_open() {
            let _ = self.tx.send(ChannelOut::Text(text.into()));
        }
    }

    /// Send one typed message: `{"type": ..., "data": ...}`.
    pub fn send_typed(&self, msg_type: &str, data: &Json) {
        let payload = json!({"type": msg_type, "data": data}).to_string();
        self.send_prepared(payload, None);
    }

    /// Send a serialized message, compressed where the channel takes it; `gz` reuses a compression
    /// made once for many channels.
    pub fn send_prepared(&self, payload: String, gz: Option<Bytes>) {
        if !self.is_open() {
            return;
        }
        if self.gz() {
            if let Some(bytes) = gz {
                let _ = self.tx.send(ChannelOut::Binary(bytes));
                return;
            }
            if payload.len() >= GZIP_MIN_BYTES
                && let Some(bytes) = gzip(&payload)
            {
                let _ = self.tx.send(ChannelOut::Binary(bytes));
                return;
            }
        }
        let _ = self.tx.send(ChannelOut::Text(payload));
    }

    /// A `system` action, as the page's channel handler reads it.
    pub fn send_system(&self, action: &str) {
        self.send_typed("system", &json!({"action": action}));
    }

    /// Wait until what the channel has unacknowledged is within its bulk bound, about twice the
    /// path's bandwidth-delay product and at least `DATA_CHANNEL_BULK_HIGH_WATER`; false where
    /// it did not within `timeout`, or the channel closed.
    pub async fn drain(&self, timeout: Duration) -> bool {
        match &self.sink {
            Sink::Channel(channel) => {
                channel.drain(DATA_CHANNEL_BULK_HIGH_WATER, timeout).await && self.is_open()
            }
            Sink::Record => {
                // As a queue does, so a later payload can overtake a transfer in a suite.
                tokio::time::sleep(Duration::from_millis(1)).await;
                self.is_open()
            }
        }
    }
}

/// The payload of a `Message` a peer sent, as the engine reads it: text, with a gzip'd binary
/// inflated under the shared bound; `Err` for a frame past the verbs.
pub fn message_text(message: Message) -> Result<String, MessageRefused> {
    match message {
        Message::Text(text) => Ok(text),
        Message::Binary(bytes) if bytes.starts_with(&[0x1f, 0x8b]) => {
            crate::ws::protocol::inflate_gz_bounded(&bytes).map_err(|_| MessageRefused::Undecodable)
        }
        Message::Binary(_) => Err(MessageRefused::Binary),
    }
}

/// Why a channel message reaches no verb.
#[derive(Debug, PartialEq, Eq)]
pub enum MessageRefused {
    /// Compressed and undecodable.
    Undecodable,
    /// A binary frame: every verb is text.
    Binary,
}
