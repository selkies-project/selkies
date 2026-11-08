/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! One connected data socket: its identity and permissions, its ping clock, and the one
//! writer every sender on it shares, with the liveness rule that keeps a slow client and
//! drops a dead one.
//!
//! Every frame goes through one writer task per socket, so the relay, the audio fan-out,
//! and the control broadcasts never interleave frames, and a socket's frames leave in the
//! order they were queued. A send waits for its frame to be handed to the kernel, which a
//! far or slow path takes seconds to do for a key frame; the wait is bounded by progress
//! rather than by time (`send_live`): a watch probes every `SEND_PROBE_SECONDS` for the
//! bytes the peer acknowledged (`TCP_INFO`, else the unsent backlog shrinking) and aborts
//! the socket once nothing moved for `SEND_STALL_SECONDS`. Nothing is canceled on the way, so
//! a slow socket stays usable and the drain its other senders share is left alone. A dead
//! socket is aborted, never closed: a close frame would queue behind the very backlog the
//! peer stopped taking.

use std::net::SocketAddr;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use bytes::Bytes;
use futures::{Sink, SinkExt};
use log::{debug, error, warn};
use parking_lot::{Mutex, RwLock};
use serde_json::Value as Json;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_websockets::{CloseCode, Message};

use crate::files::pacing::{SessionPing, UplinkClock};
use crate::settings::WS_MESSAGE_SIZE_HARD_CAP;
use crate::ws::now;
use crate::ws::protocol::{WS_GZIP_MIN_BYTES, WS_GZIP_OFFLOAD_BYTES, gzip_frame};
use crate::ws::uplink::{Gauge, Uplink};

/// A connection's identity for the server's lifetime, never reused.
pub type ClientId = u64;

/// What a ping written behind a send does with its pong: called with the round trip in seconds.
pub type PongAnswer = Box<dyn FnOnce(f64) + Send>;

/// A socket whose peer takes nothing of what a send waits on for this long is dead and is
/// aborted. Longer than the pauses a live path shows (a Wi-Fi roam, TCP backing its
/// retransmission timer off, a tab the engine froze for a moment).
pub const SEND_STALL_SECONDS: f64 = 10.0;
/// How often a waiting send looks at its socket for that progress.
pub const SEND_PROBE_SECONDS: f64 = 0.5;
/// How long a bounded control send (a kill, a `VIDEO_STOPPED` to a superseded page) waits.
pub const BOUNDED_SEND_SECONDS: f64 = 2.0;

/// UTF-8 text that many frames share without copying.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Utf8Bytes(Bytes);

impl Utf8Bytes {
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).expect("built from a String")
    }
}

impl From<String> for Utf8Bytes {
    fn from(text: String) -> Self {
        Utf8Bytes(Bytes::from(text))
    }
}

impl std::ops::Deref for Utf8Bytes {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

/// A close frame of `code` and `reason`: a code the wire cannot carry is left out, and the reason
/// is cut to the 123 bytes a close frame holds.
pub fn close_message(code: u16, reason: &str) -> Message {
    let code = CloseCode::try_from(code).ok().filter(|c| !c.is_reserved());
    let mut end = reason.len().min(123);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    Message::close(code, &reason[..end])
}

/// One frame to write.
#[derive(Clone, Debug)]
pub enum Frame {
    Text(Utf8Bytes),
    Binary(Bytes),
    Ping(Bytes),
    Close { code: u16, reason: String },
}

impl Frame {
    /// A text frame.
    pub fn text(text: impl Into<String>) -> Frame {
        Frame::Text(Utf8Bytes::from(text.into()))
    }

    /// The frame's payload length.
    pub fn len(&self) -> usize {
        match self {
            Frame::Text(t) => t.len(),
            Frame::Binary(b) | Frame::Ping(b) => b.len(),
            Frame::Close { reason, .. } => reason.len() + 2,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The WebSocket message this frame is; a payload goes out as the `Bytes` it is.
    pub fn into_message(self) -> Message {
        match self {
            Frame::Text(t) => Message::text(t.0),
            Frame::Binary(b) => Message::binary(b),
            Frame::Ping(b) => Message::ping(b),
            Frame::Close { code, reason } => close_message(code, &reason),
        }
    }
}

/// Why a send did not reach the socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendError {
    /// The socket is closed or was aborted.
    Closed,
    /// The peer took nothing for `SEND_STALL_SECONDS`; the socket was aborted.
    Stalled,
    /// A bounded send ran out of time; the socket is being closed in the background.
    Timeout,
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SendError::Closed => "connection closed",
            SendError::Stalled => "the client took nothing",
            SendError::Timeout => "send timed out",
        })
    }
}

/// The kernel's view of one socket, for the liveness watch and the bulk admission.
pub trait SocketLink: Send + Sync {
    /// Bytes the socket has accepted but not yet sent (`SIOCOUTQNSD`), or `None` where
    /// nothing can say. Not `SIOCOUTQ`: sent-but-unacked bytes scale with the path's
    /// bandwidth-delay product and keep a healthy long link reading as backlogged.
    fn unsent_bytes(&self) -> Option<i64>;

    /// A count that grows while the peer takes what was written: the bytes it acknowledged
    /// (`TCP_INFO.tcpi_bytes_acked`), which the socket's other writers cannot move; `None`
    /// where the kernel reports none.
    fn bytes_acked(&self) -> Option<i64>;
}

/// A TCP socket's kernel view, by its descriptor. The descriptor stays valid for as long as
/// the connection it belongs to, which outlives every send that probes it.
pub struct TcpLink {
    fd: RawFd,
}

impl TcpLink {
    pub fn new(fd: RawFd) -> TcpLink {
        TcpLink { fd }
    }
}

/// Offset of `tcpi_bytes_acked` in Linux's `struct tcp_info`.
const TCPI_BYTES_ACKED: usize = 120;
/// Linux `SIOCOUTQNSD`.
const SIOCOUTQNSD: libc::c_ulong = 0x894B;

impl SocketLink for TcpLink {
    fn unsent_bytes(&self) -> Option<i64> {
        let mut unsent: libc::c_int = 0;
        // SAFETY: ioctl on a descriptor this connection owns, writing one int it was given.
        let rc = unsafe { libc::ioctl(self.fd, SIOCOUTQNSD as _, &mut unsent as *mut libc::c_int) };
        (rc == 0).then_some(unsent as i64)
    }

    fn bytes_acked(&self) -> Option<i64> {
        let mut info = [0u8; TCPI_BYTES_ACKED + 8];
        let mut len = info.len() as libc::socklen_t;
        // SAFETY: getsockopt writes at most `len` bytes into the buffer it is given.
        let rc = unsafe {
            libc::getsockopt(
                self.fd,
                libc::IPPROTO_TCP,
                libc::TCP_INFO,
                info.as_mut_ptr() as *mut libc::c_void,
                &mut len,
            )
        };
        if rc != 0 || (len as usize) < info.len() {
            return None;
        }
        let mut acked = [0u8; 8];
        acked.copy_from_slice(&info[TCPI_BYTES_ACKED..TCPI_BYTES_ACKED + 8]);
        Some(u64::from_ne_bytes(acked) as i64)
    }
}

/// A socket the kernel says nothing about (a Unix socket, a test).
pub struct NoLink;

impl SocketLink for NoLink {
    fn unsent_bytes(&self) -> Option<i64> {
        None
    }

    fn bytes_acked(&self) -> Option<i64> {
        None
    }
}

/// A queued frame and who waits for it.
pub struct Outgoing {
    pub frame: Frame,
    pub done: Option<oneshot::Sender<Result<(), SendError>>>,
}

/// What a connection connected as (`client_permissions[websocket]`).
#[derive(Clone, Debug, PartialEq)]
pub struct Perms {
    /// The session token, `None` in legacy mode.
    pub token: Option<String>,
    /// `controller` or `viewer`.
    pub role: String,
    /// `null`, a slot number, or a list of them.
    pub slot: Json,
    /// Eight hex characters naming the session on `/api/sessions`.
    pub id: String,
    /// Unix time the connection was recorded.
    pub connected_at: f64,
}

impl Perms {
    pub fn is_viewer(&self) -> bool {
        self.role == "viewer"
    }
}

/// The liveness bounds a socket's sends run under; the defaults are the module's, the suites
/// shorten them.
#[derive(Clone, Copy, Debug)]
pub struct Liveness {
    pub stall_s: f64,
    pub probe_s: f64,
}

impl Default for Liveness {
    fn default() -> Self {
        Liveness {
            stall_s: SEND_STALL_SECONDS,
            probe_s: SEND_PROBE_SECONDS,
        }
    }
}

/// One connected socket.
pub struct Client {
    pub id: ClientId,
    pub remote: SocketAddr,
    perms: RwLock<Perms>,
    pub uplink: Mutex<Uplink>,
    gz: AtomicBool,
    pad: AtomicBool,
    /// The answers of the pings written by `ping_behind_answered`, by counter.
    pong_answers: Mutex<Vec<(u64, PongAnswer)>>,
    tx: mpsc::UnboundedSender<Outgoing>,
    queued: AtomicUsize,
    closed: AtomicBool,
    aborted: AtomicBool,
    abort_tx: watch::Sender<bool>,
    close_code: Mutex<Option<u16>>,
    link: Box<dyn SocketLink>,
    heard_at: AtomicU64,
    liveness: Liveness,
    /// The ping clock the file transfers' uplink gauge reads over this socket.
    transfer_clock: OnceLock<Arc<UplinkClock>>,
    me: Weak<Client>,
}

/// A transfer gauge's ping, written behind everything sent so far.
struct ClientPing(Weak<Client>);

impl SessionPing for ClientPing {
    fn ping(&self, payload: &[u8]) -> bool {
        let Some(client) = self.0.upgrade().filter(|c| !c.closed()) else {
            return false;
        };
        client.fire(Frame::Ping(Bytes::copy_from_slice(payload)));
        true
    }
}

impl Client {
    /// A client with its outbox and abort signal, which the connection's writer and reader
    /// tasks take.
    pub fn new(
        id: ClientId,
        remote: SocketAddr,
        perms: Perms,
        link: Box<dyn SocketLink>,
        liveness: Liveness,
    ) -> (
        Arc<Client>,
        mpsc::UnboundedReceiver<Outgoing>,
        watch::Receiver<bool>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (abort_tx, abort_rx) = watch::channel(false);
        let client = Arc::new_cyclic(|me| Client {
            id,
            remote,
            perms: RwLock::new(perms),
            uplink: Mutex::new(Uplink::default()),
            gz: AtomicBool::new(false),
            pad: AtomicBool::new(false),
            pong_answers: Mutex::new(Vec::new()),
            tx,
            queued: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            aborted: AtomicBool::new(false),
            abort_tx,
            close_code: Mutex::new(None),
            link,
            heard_at: AtomicU64::new(now().to_bits()),
            liveness,
            transfer_clock: OnceLock::new(),
            me: me.clone(),
        });
        (client, rx, abort_rx)
    }

    pub fn perms(&self) -> Perms {
        self.perms.read().clone()
    }

    pub fn set_perms(&self, perms: Perms) {
        *self.perms.write() = perms;
    }

    pub fn role(&self) -> String {
        self.perms.read().role.clone()
    }

    pub fn is_viewer(&self) -> bool {
        self.perms.read().is_viewer()
    }

    pub fn token(&self) -> Option<String> {
        self.perms.read().token.clone()
    }

    /// Whether the client sent `_gz,1` and takes `0x05` frames.
    pub fn gz(&self) -> bool {
        self.gz.load(Ordering::Relaxed)
    }

    pub fn set_gz(&self) {
        self.gz.store(true, Ordering::Relaxed);
    }

    /// Whether the page sent `_pad,1` and drops `0x07` padding unread.
    pub fn pad(&self) -> bool {
        self.pad.load(Ordering::Relaxed)
    }

    pub fn set_pad(&self) {
        self.pad.store(true, Ordering::Relaxed);
    }

    /// The page sent something (pings and pongs are the browser's own).
    pub fn heard(&self) {
        self.heard_at.store(now().to_bits(), Ordering::Relaxed);
    }

    /// When the page last sent anything.
    pub fn heard_at(&self) -> f64 {
        f64::from_bits(self.heard_at.load(Ordering::Relaxed))
    }

    /// Whether the socket is closed, closing, or aborted.
    pub fn closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed) || self.aborted.load(Ordering::Relaxed)
    }

    /// Whether the socket was aborted for a dead peer.
    pub fn aborted(&self) -> bool {
        self.aborted.load(Ordering::Relaxed)
    }

    /// The reader saw the socket end with `code` (1006 for a cut with no close frame).
    pub fn set_closed(&self, code: Option<u16>) {
        self.closed.store(true, Ordering::Relaxed);
        *self.close_code.lock() = code;
    }

    pub fn close_code(&self) -> Option<u16> {
        *self.close_code.lock()
    }

    /// Bytes this socket still owes the network: what waits in the outbox and what the kernel
    /// accepted but has not sent. The outbox alone would read empty while the kernel buffer
    /// absorbs megabytes.
    pub fn write_backlog(&self) -> Option<i64> {
        let queued = self.queued.load(Ordering::Relaxed) as i64;
        Some(queued + self.link.unsent_bytes().unwrap_or(0))
    }

    /// A count that grows while the peer takes what was written: the bytes it acknowledged
    /// where the kernel reports them, else the write backlog negated, so a backlog that
    /// shrinks reads as growth; `None` when nothing can say.
    pub fn peer_progress(&self) -> Option<i64> {
        if let Some(acked) = self.link.bytes_acked() {
            return Some(acked);
        }
        self.write_backlog().map(|b| -b)
    }

    fn enqueue(
        &self,
        frame: Frame,
        done: Option<oneshot::Sender<Result<(), SendError>>>,
    ) -> Result<(), SendError> {
        if self.closed() {
            return Err(SendError::Closed);
        }
        self.queued.fetch_add(frame.len(), Ordering::Relaxed);
        let len = frame.len();
        self.tx.send(Outgoing { frame, done }).map_err(|_| {
            self.queued.fetch_sub(len, Ordering::Relaxed);
            SendError::Closed
        })
    }

    /// Queue a frame without waiting for it.
    pub fn fire(&self, frame: Frame) {
        let _ = self.enqueue(frame, None);
    }

    /// Send one frame, waiting as long as the socket keeps draining; a peer that takes nothing
    /// for `SEND_STALL_SECONDS` is dead and the socket is aborted (`what` names the sender in
    /// the log line that drops it).
    pub async fn send_live(&self, frame: Frame, what: &str) -> Result<(), SendError> {
        let (done_tx, done_rx) = oneshot::channel();
        self.enqueue(frame, Some(done_tx))?;
        self.watch_send(done_rx, what).await
    }

    /// `send_live` with a ping written right behind the frame when `ping_behind`: its pong says
    /// when the whole of that frame reached the peer, which a ping ahead of it would not.
    pub async fn send_live_pinged(
        &self,
        frame: Frame,
        what: &str,
        ping_behind: bool,
    ) -> Result<(), SendError> {
        let (done_tx, done_rx) = oneshot::channel();
        self.enqueue(frame, Some(done_tx))?;
        if ping_behind {
            self.ping_behind();
        }
        self.watch_send(done_rx, what).await
    }

    /// Queue one frame now and return the wait for it, watched as `send_live` watches: a
    /// fan-out that enqueues every client's chunk before awaiting any keeps each socket's
    /// chunks in order while a slow socket holds only its own.
    pub fn send_live_later(
        self: &Arc<Self>,
        frame: Frame,
        what: &'static str,
    ) -> Result<impl std::future::Future<Output = Result<(), SendError>> + Send + 'static, SendError>
    {
        let (done_tx, done_rx) = oneshot::channel();
        self.enqueue(frame, Some(done_tx))?;
        let client = self.clone();
        Ok(async move { client.watch_send(done_rx, what).await })
    }

    /// Wait for a queued frame while the peer keeps taking bytes.
    async fn watch_send(
        &self,
        mut done_rx: oneshot::Receiver<Result<(), SendError>>,
        what: &str,
    ) -> Result<(), SendError> {
        let probe = Duration::from_secs_f64(self.liveness.probe_s);
        let mut mark: Option<i64> = None;
        let mut since: Option<f64> = None;
        loop {
            match tokio::time::timeout(probe, &mut done_rx).await {
                Ok(Ok(result)) => return result,
                Ok(Err(_)) => return Err(SendError::Closed),
                Err(_) => {}
            }
            let progress = self.peer_progress();
            let at = now();
            let moved = match (progress, mark) {
                (Some(p), Some(m)) => p > m,
                (Some(_), None) => true,
                (None, _) => false,
            };
            if since.is_none() || moved {
                mark = progress;
                since = Some(at);
            } else if at - since.unwrap_or(at) >= self.liveness.stall_s {
                warn!(target: "ws", "{what} send stalled past {:.0}s with nothing taken by the client; dropping it.",
                    self.liveness.stall_s);
                self.abort();
                return Err(SendError::Stalled);
            }
        }
    }

    /// Send one frame, waiting unbounded for the socket to take it.
    pub async fn send(&self, frame: Frame) -> Result<(), SendError> {
        let (done_tx, done_rx) = oneshot::channel();
        self.enqueue(frame, Some(done_tx))?;
        done_rx.await.unwrap_or(Err(SendError::Closed))
    }

    /// Send one frame within `seconds`; a socket that does not take it by then is closed in
    /// the background rather than waited on (the superseded page most likely frozen).
    pub async fn send_bounded(&self, frame: Frame, seconds: f64) -> Result<(), SendError> {
        let (done_tx, done_rx) = oneshot::channel();
        self.enqueue(frame, Some(done_tx))?;
        match tokio::time::timeout(Duration::from_secs_f64(seconds), done_rx).await {
            Ok(result) => result.unwrap_or(Err(SendError::Closed)),
            Err(_) => {
                self.close_abandoned();
                Err(SendError::Timeout)
            }
        }
    }

    /// Send a text frame, watched or not.
    pub async fn send_text(&self, text: &str, watched: bool) -> Result<(), SendError> {
        if watched {
            self.send_live(Frame::text(text), "Control message").await
        } else {
            self.send(Frame::text(text)).await
        }
    }

    /// Drop a dead socket at once: every send waiting on it fails as a lost connection.
    pub fn abort(&self) {
        if !self.aborted.swap(true, Ordering::Relaxed) {
            let _ = self.abort_tx.send(true);
        }
    }

    /// Close a dropped socket in the background: a close can itself block on the paused
    /// transport that stalled the send, so it never runs inline on a broadcast path.
    pub fn close_abandoned(&self) {
        self.abort();
    }

    /// Ask the socket to close with `code` and `reason`, bounded like every control send;
    /// the reader ends the connection when the peer's close frame comes back or the socket
    /// drops.
    pub async fn close(&self, code: u16, reason: &str) {
        if self.closed() {
            return;
        }
        self.closed.store(true, Ordering::Relaxed);
        *self.close_code.lock() = Some(code);
        let (done_tx, done_rx) = oneshot::channel();
        self.queued.fetch_add(reason.len() + 2, Ordering::Relaxed);
        if self
            .tx
            .send(Outgoing {
                frame: Frame::Close {
                    code,
                    reason: reason.to_string(),
                },
                done: Some(done_tx),
            })
            .is_err()
        {
            return;
        }
        if tokio::time::timeout(Duration::from_secs_f64(BOUNDED_SEND_SECONDS), done_rx)
            .await
            .is_err()
        {
            self.abort();
        }
    }

    /// Ping behind everything written so far; the pong says all of that reached the peer.
    /// The ping's counter, which `ping_outstanding` reads back.
    pub fn ping_behind(&self) -> u64 {
        let payload = {
            let mut uplink = self.uplink.lock();
            uplink.ping_behind(now())
        };
        let counter = u64::from_be_bytes(payload[..8].try_into().unwrap_or([0; 8]));
        self.fire(Frame::Ping(Bytes::from(payload)));
        counter
    }

    /// `ping_behind`, with `answer` called with the pong's round trip; answers whose ping fell
    /// out of the clock's pending set are dropped.
    pub fn ping_behind_answered(&self, answer: PongAnswer) -> u64 {
        let counter = self.ping_behind();
        let uplink = self.uplink.lock();
        let mut answers = self.pong_answers.lock();
        answers.retain(|(c, _)| uplink.ping_outstanding(*c));
        answers.push((counter, answer));
        counter
    }

    /// Resolve a received pong against the ping clocks: each ignores the other's payloads. A
    /// ping written with an answer has it called with the round trip.
    pub fn note_pong(&self, payload: &[u8]) -> bool {
        if let Some(clock) = self.transfer_clock.get() {
            clock.note_pong(payload);
        }
        let (matched, rtt_us) = {
            let mut uplink = self.uplink.lock();
            (uplink.note_pong(payload, now()), uplink.rtt_us)
        };
        if !matched {
            return false;
        }
        let counter = <[u8; 8]>::try_from(payload).map(u64::from_be_bytes);
        let answer = counter.ok().and_then(|counter| {
            let mut answers = self.pong_answers.lock();
            let index = answers.iter().position(|(c, _)| *c == counter)?;
            Some(answers.swap_remove(index).1)
        });
        if let (Some(answer), Some(rtt_us)) = (answer, rtt_us) {
            answer(rtt_us as f64 / 1e6);
        }
        true
    }

    /// The ping clock a file transfer's uplink gauge reads over this socket.
    pub fn uplink_clock(&self) -> Arc<UplinkClock> {
        self.transfer_clock
            .get_or_init(|| Arc::new(UplinkClock::new(Arc::new(ClientPing(self.me.clone())))))
            .clone()
    }

    /// One gauge sample over this socket's clock, pinging where due.
    pub fn gauge_sample(&self, gauge: &mut Gauge) -> Option<bool> {
        let (ping, verdict) = {
            let mut uplink = self.uplink.lock();
            gauge.sample(&mut uplink, now())
        };
        if let Some(payload) = ping {
            self.fire(Frame::Ping(Bytes::from(payload)));
        }
        verdict
    }

    /// The round trip the ping clock last measured, in milliseconds.
    pub fn rtt_ms(&self) -> Option<f64> {
        self.uplink.lock().rtt_ms()
    }

    /// The writer's bookkeeping: a frame left the outbox.
    pub(crate) fn dequeued(&self, len: usize) {
        self.queued.fetch_sub(
            len.min(self.queued.load(Ordering::Relaxed)),
            Ordering::Relaxed,
        );
    }

    /// Whether `_gz,1` sockets and a text of this size take a `0x05` frame.
    pub fn takes_gzip(&self, text_len: usize) -> bool {
        self.gz() && text_len >= WS_GZIP_MIN_BYTES
    }
}

/// The writer task of one socket: frames leave in outbox order, each sender told when its
/// frame was handed to the kernel. Ends when the connection is aborted, the sink fails, or
/// the client is dropped.
pub async fn run_writer<S>(
    client: Arc<Client>,
    mut rx: mpsc::UnboundedReceiver<Outgoing>,
    mut abort: watch::Receiver<bool>,
    mut sink: S,
) where
    S: Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    loop {
        let out = tokio::select! {
            out = rx.recv() => match out {
                Some(out) => out,
                None => break,
            },
            changed = abort.changed() => {
                if changed.is_err() || *abort.borrow() {
                    break;
                }
                continue;
            }
        };
        client.dequeued(out.frame.len());
        let message = out.frame.into_message();
        let result = tokio::select! {
            sent = sink.send(message) => sent.map_err(|e| {
                debug!(target: "ws", "Socket write to {} failed: {e}", client.remote);
                SendError::Closed
            }),
            _ = abort.changed() => Err(SendError::Closed),
        };
        let failed = result.is_err();
        if let Some(done) = out.done {
            let _ = done.send(result);
        }
        if failed {
            break;
        }
    }
    client.closed.store(true, Ordering::Relaxed);
    rx.close();
    while let Ok(out) = rx.try_recv() {
        client.dequeued(out.frame.len());
        if let Some(done) = out.done {
            let _ = done.send(Err(SendError::Closed));
        }
    }
    let _ = sink.close().await;
}

/// Send one text to one client: gzipped
/// for a gzip-capable socket at or over `WS_GZIP_MIN_BYTES`, watched or not. `Err` means the
/// client is gone or dead and should leave the registry.
pub async fn send_text_to(client: &Client, text: &str, watched: bool) -> Result<(), SendError> {
    if text.len() > WS_MESSAGE_SIZE_HARD_CAP {
        error!(target: "ws", "Refusing to broadcast a {}-byte WebSocket message (hard cap {} bytes); message dropped.",
            text.len(), WS_MESSAGE_SIZE_HARD_CAP);
        return Ok(());
    }
    if client.closed() {
        return Err(SendError::Closed);
    }
    let frame = if client.takes_gzip(text.len()) {
        Frame::Binary(gzip_text(text).await)
    } else {
        Frame::text(text)
    };
    if watched {
        client.send_live(frame, "Control message").await
    } else {
        client.send(frame).await
    }
}

/// The `0x05` frame of `text`, compressed on a blocking thread from `WS_GZIP_OFFLOAD_BYTES` up.
async fn gzip_text(text: &str) -> Bytes {
    if text.len() >= WS_GZIP_OFFLOAD_BYTES {
        let owned = text.to_string();
        tokio::task::spawn_blocking(move || gzip_frame(owned.as_bytes()))
            .await
            .unwrap_or_else(|_| gzip_frame(text.as_bytes()))
    } else {
        gzip_frame(text.as_bytes())
    }
}

/// Broadcast one text to `clients` concurrently; the clients dropped for a dead or closed
/// socket, which the caller takes out of its registry. A watched send drops a client whose
/// socket stopped taking bytes; an unwatched one waits unbounded. The gzip frame for the
/// gzip-capable sockets is computed at most once and shared.
pub async fn broadcast(clients: &[Arc<Client>], text: &str, watched: bool) -> Vec<ClientId> {
    if clients.is_empty() {
        return Vec::new();
    }
    if text.len() > WS_MESSAGE_SIZE_HARD_CAP {
        error!(target: "ws", "Refusing to broadcast a {}-byte WebSocket message (hard cap {} bytes); message dropped.",
            text.len(), WS_MESSAGE_SIZE_HARD_CAP);
        return Vec::new();
    }
    let gz = if clients.iter().any(|c| c.takes_gzip(text.len())) {
        Some(gzip_text(text).await)
    } else {
        None
    };
    let plain = Utf8Bytes::from(text.to_string());
    let sends = clients.iter().map(|client| {
        let frame = match &gz {
            Some(bytes) if client.takes_gzip(text.len()) => Frame::Binary(bytes.clone()),
            _ => Frame::Text(plain.clone()),
        };
        async move {
            if client.closed() {
                return Some(client.id);
            }
            let result = if watched {
                client.send_live(frame, "Control message").await
            } else {
                client.send(frame).await
            };
            result.err().map(|_| client.id)
        }
    });
    futures::future::join_all(sends)
        .await
        .into_iter()
        .flatten()
        .collect()
}

#[cfg(test)]
pub(crate) mod testing {
    //! Stand-in sockets whose far side takes bytes at a set rate (none at all for a dead
    //! one).

    use super::*;
    use std::sync::atomic::AtomicI64;

    /// The kernel's view of a stand-in socket: bytes pending in the "kernel" and acked by
    /// the far side.
    pub struct FakeLink {
        pub pending: AtomicI64,
        pub acked: AtomicI64,
    }

    impl SocketLink for FakeLink {
        fn unsent_bytes(&self) -> Option<i64> {
            Some(self.pending.load(Ordering::Relaxed))
        }

        fn bytes_acked(&self) -> Option<i64> {
            Some(self.acked.load(Ordering::Relaxed))
        }
    }

    /// What a stand-in socket was written: when, the frame's first bytes, and its size.
    pub type Writes = Arc<Mutex<Vec<(f64, Vec<u8>, usize)>>>;

    /// A stand-in socket: a send writes its whole frame into the kernel buffer, then waits
    /// while more than `HIGH` stands unsent until the far side has taken it under `LOW`,
    /// a transport's flow control; the far side takes `rate` bytes a second.
    pub struct FakeSocket {
        pub client: Arc<Client>,
        pub hold_stream_info: Arc<AtomicBool>,
        pub writes: Writes,
        task: tokio::task::JoinHandle<()>,
    }

    pub const HIGH: i64 = 64 * 1024;
    pub const LOW: i64 = 16 * 1024;

    impl FakeSocket {
        pub fn new(id: ClientId, rate: f64, pending: i64, liveness: Liveness) -> FakeSocket {
            let link = Arc::new(FakeLink {
                pending: AtomicI64::new(pending),
                acked: AtomicI64::new(0),
            });
            let perms = Perms {
                token: None,
                role: "controller".into(),
                slot: Json::Null,
                id: format!("{id:08x}"),
                connected_at: 0.0,
            };
            let (client, mut rx, mut abort) = Client::new(
                id,
                "127.0.0.1:1".parse().unwrap(),
                perms,
                Box::new(SharedLink(link.clone())),
                liveness,
            );
            let writes = Arc::new(Mutex::new(Vec::new()));
            let hold_stream_info = Arc::new(AtomicBool::new(false));
            let task_hold_stream_info = hold_stream_info.clone();
            let task_writes = writes.clone();
            let task_link = link.clone();
            let task_client = client.clone();
            let task = tokio::spawn(async move {
                let far_side = {
                    let link = task_link.clone();
                    tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                            let pending = link.pending.load(Ordering::Relaxed);
                            let take = pending.min((rate * 0.01) as i64);
                            link.pending.fetch_sub(take, Ordering::Relaxed);
                            link.acked.fetch_add(take, Ordering::Relaxed);
                        }
                    })
                };
                loop {
                    let out = tokio::select! {
                        out = rx.recv() => match out { Some(out) => out, None => break },
                        _ = abort.changed() => break,
                    };
                    task_client.dequeued(out.frame.len());
                    let len = out.frame.len();
                    let head: Vec<u8> = match &out.frame {
                        Frame::Text(t) => t.as_bytes().iter().take(4).copied().collect(),
                        Frame::Binary(b) | Frame::Ping(b) => b.iter().take(4).copied().collect(),
                        Frame::Close { .. } => vec![],
                    };
                    task_writes.lock().push((now(), head, len));
                    task_link.pending.fetch_add(len as i64, Ordering::Relaxed);
                    let result = loop {
                        if *abort.borrow() {
                            break Err(SendError::Closed);
                        }
                        if task_link.pending.load(Ordering::Relaxed) <= HIGH {
                            break Ok(());
                        }
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_millis(5)) => {}
                            _ = abort.changed() => {}
                        }
                        if task_link.pending.load(Ordering::Relaxed) <= LOW {
                            break Ok(());
                        }
                    };
                    while matches!(&out.frame, Frame::Text(t) if t.contains("\"stream_info\""))
                        && task_hold_stream_info.load(Ordering::Relaxed)
                    {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    if let Some(done) = out.done {
                        let _ = done.send(result);
                    }
                    if result.is_err() {
                        break;
                    }
                }
                far_side.abort();
                task_client.closed.store(true, Ordering::Relaxed);
                while let Ok(out) = rx.try_recv() {
                    if let Some(done) = out.done {
                        let _ = done.send(Err(SendError::Closed));
                    }
                }
            });
            FakeSocket {
                client,
                hold_stream_info,
                writes,
                task,
            }
        }

        pub fn stop(&self) {
            self.task.abort();
        }
    }

    struct SharedLink(Arc<FakeLink>);

    impl SocketLink for SharedLink {
        fn unsent_bytes(&self) -> Option<i64> {
            self.0.unsent_bytes()
        }

        fn bytes_acked(&self) -> Option<i64> {
            self.0.bytes_acked()
        }
    }

    /// A client with no socket behind it, for the relay and gate suites: what it is sent is
    /// collected, and its kernel view is the test's to set.
    pub struct Page {
        pub client: Arc<Client>,
        pub unsent: Arc<AtomicI64>,
        pub sent: Arc<Mutex<Vec<Frame>>>,
        task: tokio::task::JoinHandle<()>,
    }

    pub struct PageLink(pub Arc<AtomicI64>);

    impl SocketLink for PageLink {
        fn unsent_bytes(&self) -> Option<i64> {
            Some(self.0.load(Ordering::Relaxed))
        }

        fn bytes_acked(&self) -> Option<i64> {
            None
        }
    }

    impl Page {
        pub fn new(id: ClientId, role: &str) -> Page {
            let unsent = Arc::new(AtomicI64::new(0));
            let perms = Perms {
                token: None,
                role: role.into(),
                slot: Json::Null,
                id: format!("{id:08x}"),
                connected_at: 0.0,
            };
            let (client, mut rx, _abort) = Client::new(
                id,
                "127.0.0.1:1".parse().unwrap(),
                perms,
                Box::new(PageLink(unsent.clone())),
                Liveness::default(),
            );
            let sent = Arc::new(Mutex::new(Vec::new()));
            let task_sent = sent.clone();
            let task_client = client.clone();
            let task = tokio::spawn(async move {
                while let Some(out) = rx.recv().await {
                    task_client.dequeued(out.frame.len());
                    task_sent.lock().push(out.frame);
                    if let Some(done) = out.done {
                        let _ = done.send(Ok(()));
                    }
                }
            });
            Page {
                client,
                unsent,
                sent,
                task,
            }
        }

        /// The texts the page was sent.
        pub fn texts(&self) -> Vec<String> {
            self.sent
                .lock()
                .iter()
                .filter_map(|f| match f {
                    Frame::Text(t) => Some(t.to_string()),
                    _ => None,
                })
                .collect()
        }
    }

    impl Drop for Page {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[tokio::test]
    async fn a_control_send_behind_a_slow_drain_keeps_its_client() {
        let socket = FakeSocket::new(1, 150.0 * 1024.0, 600 * 1024, Liveness::default());
        let start = now();
        let dropped = broadcast(
            std::slice::from_ref(&socket.client),
            &format!("cursor,{}", "x".repeat(200)),
            true,
        )
        .await;
        assert!(dropped.is_empty());
        assert!(!socket.client.aborted());
        assert!(
            now() - start > 2.0,
            "waited for the 600 KB backlog to drain"
        );
        socket.stop();
    }

    #[tokio::test]
    async fn a_dead_socket_is_dropped_and_only_it() {
        let short = Liveness {
            stall_s: 1.5,
            probe_s: 0.1,
        };
        let dead = FakeSocket::new(1, 0.0, 600 * 1024, short);
        let fast = FakeSocket::new(2, 10.0 * 1024.0 * 1024.0, 0, short);
        let start = now();
        let dropped = broadcast(
            &[dead.client.clone(), fast.client.clone()],
            &format!("cursor,{}", "x".repeat(200)),
            true,
        )
        .await;
        assert_eq!(dropped, vec![1]);
        assert!(dead.client.aborted() && !fast.client.aborted());
        assert!(now() - start < 3.0, "dropped after {:.1} s", now() - start);
        dead.stop();
        fast.stop();
    }

    #[tokio::test]
    async fn a_gzip_capable_client_takes_large_text_compressed() {
        let page = Page::new(3, "controller");
        page.client.set_gz();
        let text = format!("cursor,{}", "y".repeat(2000));
        assert!(send_text_to(&page.client, &text, true).await.is_ok());
        send_text_to(&page.client, "MODE websockets", true)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let frames = page.sent.lock();
        assert!(matches!(&frames[0], Frame::Binary(b) if b[0] == 0x05));
        assert!(matches!(&frames[1], Frame::Text(t) if t.as_str() == "MODE websockets"));
    }
}
