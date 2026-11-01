/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A peer's socket as the registry sees it, with no HTTP library in it.
//!
//! The server crate's WebSocket route makes one with `socket()`, forwards what the WebSocket
//! receives as `Inbound`, and writes what `SocketOutbox` yields. A send is a queue push, so
//! it never waits under the registry's lock and one slow peer serializes nothing; a peer that
//! lets its queue pass `OUTBOX_MAX_BYTES` is cut, its session held for a resume like any
//! other cut, so a page that stopped reading neither stalls nor grows the server.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use tokio::sync::{Notify, mpsc};

/// What one peer may have queued and unwritten before it is cut: many descriptions, far past
/// what a page reading its socket leaves behind.
pub const OUTBOX_MAX_BYTES: usize = 8 * 1024 * 1024;

static NEXT_SOCKET_ID: AtomicU64 = AtomicU64::new(1);

/// What a WebSocket delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inbound {
    Text(String),
    /// A binary frame; signaling is text, so it is dropped with a warning.
    Binary,
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    /// The peer's close frame: a reload, a closed tab.
    CloseFrame,
    /// The socket ended with no close frame: an ingress retiring its connections.
    Cut,
}

/// What to write to a WebSocket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outbound {
    Text(String),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    /// Send this close frame, then end the socket.
    Close {
        code: u16,
        reason: String,
    },
    /// End the socket with no close frame.
    Abort,
}

/// The send half of a peer's socket.
#[derive(Debug)]
pub struct PeerSocket {
    id: u64,
    tx: mpsc::UnboundedSender<Outbound>,
    queued: Arc<AtomicUsize>,
    closed: AtomicBool,
    aborted: AtomicBool,
    closing: Notify,
}

/// The queue the route writes from.
#[derive(Debug)]
pub struct SocketOutbox {
    rx: mpsc::UnboundedReceiver<Outbound>,
    queued: Arc<AtomicUsize>,
}

/// Make a socket and the outbox its writes come out of.
pub fn socket() -> (Arc<PeerSocket>, SocketOutbox) {
    let (tx, rx) = mpsc::unbounded_channel();
    let queued = Arc::new(AtomicUsize::new(0));
    (
        Arc::new(PeerSocket {
            id: NEXT_SOCKET_ID.fetch_add(1, Ordering::Relaxed),
            tx,
            queued: Arc::clone(&queued),
            closed: AtomicBool::new(false),
            aborted: AtomicBool::new(false),
            closing: Notify::new(),
        }),
        SocketOutbox { rx, queued },
    )
}

impl SocketOutbox {
    /// The next thing to write, `None` once the socket's handles are gone.
    pub async fn recv(&mut self) -> Option<Outbound> {
        let next = self.rx.recv().await?;
        if let Outbound::Text(text) = &next {
            self.queued.fetch_sub(text.len(), Ordering::AcqRel);
        }
        Some(next)
    }
}

impl PeerSocket {
    /// What tells this socket from another; a peer that resumed sits on a new one.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Whether the socket has ended or is ending.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Queue a line.
    ///
    /// # Errors
    ///
    /// The socket is closed, or this line took its queue past `OUTBOX_MAX_BYTES`, which cuts
    /// it. Either way the line was not queued.
    pub fn send_text(&self, text: String) -> Result<(), SocketGone> {
        if self.is_closed() {
            return Err(SocketGone);
        }
        let queued = self.queued.fetch_add(text.len(), Ordering::AcqRel) + text.len();
        if queued > OUTBOX_MAX_BYTES {
            self.queued.fetch_sub(text.len(), Ordering::AcqRel);
            self.abort();
            return Err(SocketGone);
        }
        self.tx.send(Outbound::Text(text)).map_err(|_| SocketGone)
    }

    /// Queue a keepalive ping.
    pub fn ping(&self) {
        if !self.is_closed() {
            let _ = self.tx.send(Outbound::Ping(Vec::new()));
        }
    }

    /// Queue a ping carrying `data`, written behind everything sent so far; its pong measures
    /// the round trip (`PeerRegistry::set_on_pong`).
    pub fn ping_with(&self, data: Vec<u8>) {
        if !self.is_closed() {
            let _ = self.tx.send(Outbound::Ping(data));
        }
    }

    /// Queue the answer to a ping.
    pub fn pong(&self, data: Vec<u8>) {
        if !self.is_closed() {
            let _ = self.tx.send(Outbound::Pong(data));
        }
    }

    /// Close with a close frame. The first close wins.
    pub fn close(&self, code: u16, reason: &str) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            let _ = self.tx.send(Outbound::Close {
                code,
                reason: reason.to_owned(),
            });
            self.closing.notify_waiters();
        }
    }

    /// End the socket with no close frame, which the peer reads as a cut.
    fn abort(&self) {
        self.aborted.store(true, Ordering::Release);
        if !self.closed.swap(true, Ordering::AcqRel) {
            let _ = self.tx.send(Outbound::Abort);
            self.closing.notify_waiters();
        }
    }

    /// Whether this side cut the socket for not keeping up.
    pub(super) fn was_aborted(&self) -> bool {
        self.aborted.load(Ordering::Acquire)
    }

    /// Note that the socket ended from the far side.
    pub(super) fn mark_closed(&self) {
        self.closed.store(true, Ordering::Release);
    }

    /// Resolve once this side closed the socket.
    pub(super) async fn closed_here(&self) {
        let notified = self.closing.notified();
        if self.is_closed() {
            return;
        }
        notified.await;
    }
}

/// The socket took no line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the signaling socket is closed")]
pub struct SocketGone;
