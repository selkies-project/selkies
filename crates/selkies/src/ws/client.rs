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

use std::collections::VecDeque;
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
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio_websockets::{CloseCode, Message};

use crate::files::pacing::{SessionPing, UplinkClock};
use crate::settings::WS_MESSAGE_SIZE_HARD_CAP;
use crate::ws::flight::{FLIGHT_LATE_S, Flight, Said, frame_id};
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

/// How long an audio frame may wait in a socket's outbox before it is let go unwritten: the
/// player holds tens of milliseconds, so a frame this late would only be played out as a
/// backlog after a congested stretch.
pub const AUDIO_STALE_SECONDS: f64 = 0.2;

/// The most a WebSocket's kernel send buffer holds unsent (`TCP_NOTSENT_LOWAT`): the queue a
/// slow path builds stays in the outbox, where stale audio can be let go, rather than in a
/// buffer the kernel grows to megabytes. Bytes in flight are not bounded by it.
pub const NOTSENT_LOWAT_BYTES: libc::c_int = 64 * 1024;
/// How often the writer looks again at a late path that holds a bulk frame back, between
/// the acks that wake it.
const FLIGHT_HELD_TICK: Duration = Duration::from_millis(10);
/// The longest one bulk frame is held back from a late path: past it the frame goes as the
/// kernel takes it, whatever the path still holds.
pub const FLIGHT_HOLD_MAX_S: f64 = 2.0;
/// A write that took longer than this waited on the socket: the frame went out when the
/// path made room, not when the stream had it.
const FLIGHT_WRITE_WAIT_S: f64 = 0.005;
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
/// Hold a WebSocket's kernel send buffer to `NOTSENT_LOWAT_BYTES` unsent; a socket that refuses
/// keeps the kernel's default, which only moves where a backlog waits.
pub fn bound_unsent(fd: RawFd) {
    let lowat = NOTSENT_LOWAT_BYTES;
    // SAFETY: `fd` is the socket's own descriptor for the call, and the option is an int.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_NOTSENT_LOWAT,
            (&lowat as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        debug!(target: "ws", "TCP_NOTSENT_LOWAT refused: {}", std::io::Error::last_os_error());
    }
}

/// Linux `SIOCOUTQNSD`.
const SIOCOUTQNSD: libc::c_ulong = 0x894B;

impl SocketLink for TcpLink {
    fn unsent_bytes(&self) -> Option<i64> {
        let mut unsent: libc::c_int = 0;
        // SAFETY: ioctl on a descriptor this connection owns, writing one int it was given.
        let rc = unsafe { libc::ioctl(self.fd, SIOCOUTQNSD as _, &mut unsent as *mut libc::c_int) };
        (rc == 0).then_some(i64::from(unsent))
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
    /// When the frame is worth nothing any more (`ws::now()` seconds): one still queued then
    /// is let go when its turn comes rather than written.
    pub stale_at: Option<f64>,
    /// A bulk frame (video, padding), which waits while a late path delivers what it was
    /// handed (`ws::flight`), with its sender's word on what waits behind it.
    pub bulk: Option<Waiting>,
    /// A ping that reads the path for a sender off this socket (a transfer's gauge): it goes
    /// ahead of a bulk frame held back, whose wait in the writer is not the path's.
    pub path_ping: bool,
}

/// Whether more bulk frames wait with a sender behind the one it handed the writer, asked
/// each time the writer considers that frame: the sender's backlog grows while its frame is
/// held.
pub type Waiting = Arc<dyn Fn() -> bool + Send + Sync>;

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
    /// Audio frames let go because they went stale in the outbox.
    stale_audio: AtomicU64,
    /// What the path still holds of what the writer handed it, and the writer's wake for
    /// an ack that says more of it arrived.
    flight: Mutex<Flight>,
    flight_wake: Notify,
    me: Weak<Client>,
}

/// A transfer gauge's ping, written behind everything written so far: the transfer's own
/// bytes go over another connection, and the queue they stand in is the path's, so the ping
/// does not wait with a bulk frame the writer holds back.
struct ClientPing(Weak<Client>);

impl SessionPing for ClientPing {
    fn ping(&self, payload: &[u8]) -> bool {
        let Some(client) = self.0.upgrade().filter(|c| !c.closed()) else {
            return false;
        };
        client.fire_ahead(Frame::Ping(Bytes::copy_from_slice(payload)))
    }

    fn stream_bytes(&self) -> Option<u64> {
        self.0
            .upgrade()
            .map(|client| client.flight.lock().written())
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
            stale_audio: AtomicU64::new(0),
            flight: Mutex::new(Flight::default()),
            flight_wake: Notify::new(),
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
        self.enqueue_until(frame, done, None)
    }

    fn enqueue_until(
        &self,
        frame: Frame,
        done: Option<oneshot::Sender<Result<(), SendError>>>,
        stale_at: Option<f64>,
    ) -> Result<(), SendError> {
        self.enqueue_as(frame, done, stale_at, None)
    }

    fn enqueue_as(
        &self,
        frame: Frame,
        done: Option<oneshot::Sender<Result<(), SendError>>>,
        stale_at: Option<f64>,
        bulk: Option<Waiting>,
    ) -> Result<(), SendError> {
        if self.closed() {
            return Err(SendError::Closed);
        }
        self.queued.fetch_add(frame.len(), Ordering::Relaxed);
        let len = frame.len();
        self.tx
            .send(Outgoing {
                frame,
                done,
                stale_at,
                bulk,
                path_ping: false,
            })
            .map_err(|_| {
                self.queued.fetch_sub(len, Ordering::Relaxed);
                SendError::Closed
            })
    }

    /// Queue a frame without waiting for it.
    pub fn fire(&self, frame: Frame) {
        let _ = self.enqueue(frame, None);
    }

    /// `fire` for a ping that asks only whether the peer is there, or how long its path
    /// takes right now, and says nothing of what was queued before it: it goes ahead of a
    /// bulk frame the writer holds. Whether it was queued.
    pub fn fire_ahead(&self, frame: Frame) -> bool {
        if self.closed() {
            return false;
        }
        let len = frame.len();
        self.queued.fetch_add(len, Ordering::Relaxed);
        let queued = self.tx.send(Outgoing {
            frame,
            done: None,
            stale_at: None,
            bulk: None,
            path_ping: true,
        });
        if queued.is_err() {
            self.queued.fetch_sub(len, Ordering::Relaxed);
        }
        queued.is_ok()
    }

    /// Send one frame, waiting as long as the socket keeps draining; a peer that takes nothing
    /// for `SEND_STALL_SECONDS` is dead and the socket is aborted (`what` names the sender in
    /// the log line that drops it).
    pub async fn send_live(&self, frame: Frame, what: &str) -> Result<(), SendError> {
        let (done_tx, done_rx) = oneshot::channel();
        self.enqueue(frame, Some(done_tx))?;
        self.watch_send(done_rx, what).await
    }

    /// `send_live` for a bulk frame (video, padding), `waiting` saying whether more of them
    /// wait behind it with the caller: the writer holds it while the path is late and still
    /// delivering what it was handed (`ws::flight`), so the caller's frames wait with the
    /// caller instead of in the path. A ping is written right behind the frame when
    /// `ping_behind`: its pong says when the whole of that frame reached the peer, which a
    /// ping ahead of it would not.
    pub async fn send_bulk(
        &self,
        frame: Frame,
        what: &str,
        ping_behind: bool,
        waiting: &Waiting,
    ) -> Result<(), SendError> {
        let (done_tx, done_rx) = oneshot::channel();
        self.enqueue_as(frame, Some(done_tx), None, Some(waiting.clone()))?;
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

    /// `send_live_later` for an audio frame, which goes stale `AUDIO_STALE_SECONDS` after it
    /// is queued: audio that late is worth less than the gap it would fill, so a socket whose
    /// path backed up skips it and goes on with live audio rather than play a backlog out late.
    pub fn send_audio_later(
        self: &Arc<Self>,
        frame: Frame,
    ) -> Result<impl std::future::Future<Output = Result<(), SendError>> + Send + 'static, SendError>
    {
        let (done_tx, done_rx) = oneshot::channel();
        self.enqueue_until(frame, Some(done_tx), Some(now() + AUDIO_STALE_SECONDS))?;
        let client = self.clone();
        Ok(async move { client.watch_send(done_rx, "Audio").await })
    }

    /// Audio frames this socket let go because they went stale in its outbox.
    pub fn stale_audio(&self) -> u64 {
        self.stale_audio.load(Ordering::Relaxed)
    }

    /// Whether the writer paces this socket's bulk frames to what its late path delivers, a
    /// stream that has stopped being paced no more.
    pub fn paced(&self) -> bool {
        self.flight.lock().paced_at(now())
    }

    /// `paced`, or was within `span` seconds: what a check that runs every `span` reads.
    pub fn paced_within(&self, span: f64) -> bool {
        self.flight.lock().paced_within(now(), span)
    }

    /// What the writer's gauge reads its paced path delivering, in bits a second
    /// (`Flight::paced_rate`); `None` on a path that is not paced.
    pub fn paced_bps(&self) -> Option<f64> {
        self.flight.lock().paced_rate(now()).map(|rate| rate * 8.0)
    }

    /// What the sender of this socket's video asks before it hands a frame of `len` bytes
    /// over: whether the writer paced the path within `span` seconds, and whether the path
    /// has room for the frame now (`Flight::would_clear`).
    pub fn bulk_pacing(&self, span: f64, len: usize) -> (bool, bool) {
        let flight = self.flight.lock();
        let at = now();
        (flight.paced_within(at, span), flight.would_clear(at, len))
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
                stale_at: None,
                bulk: None,
                path_ping: false,
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
    /// The ping's counter, which `ping_outstanding` reads back. Behind a bulk frame the
    /// writer holds too: its pong moves the clock's `answered`, which the link reads as
    /// everything handed to the writer before the ping having arrived (a key frame's
    /// crossing, the gate's held path), and a frame held has not. Its round trip then holds
    /// the hold, which says what it should: what was sent before it is not there yet.
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

    /// The page's own clock, in milliseconds, as it sent the ack just read, and what the ack
    /// says of the stream (`Flight::note_stamp`): how long its way back held that ack, in
    /// seconds.
    pub fn note_stamp(&self, sent_ms: f64, said: Said) -> f64 {
        let (passed, kept_up, back) = {
            let mut flight = self.flight.lock();
            let was = flight.paced();
            (
                flight.note_stamp(sent_ms / 1000.0, said, now()),
                was && !flight.paced(),
                flight.way_back(now()),
            )
        };
        if kept_up {
            debug!(target: "ws", "Path of {} is a longer way round, not narrowed under its stream: its video is paced no more.", self.remote);
        }
        if passed {
            self.flight_wake.notify_one();
        }
        back
    }

    /// How long its way back held the page's last ack, by its stamp (`Flight::way_back`).
    pub fn held_back(&self) -> f64 {
        self.flight.lock().way_back(now())
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

    /// Keep the transfers' clock on this socket to the path's own floor while no transfer
    /// pings it (`UplinkClock::keep_floor`), on the reader's beat.
    pub fn keep_transfer_floor(&self) {
        self.uplink_clock().keep_floor();
    }

    /// The ping clock a file transfer's uplink gauge reads over this socket.
    pub fn uplink_clock(&self) -> Arc<UplinkClock> {
        self.transfer_clock
            .get_or_init(|| Arc::new(UplinkClock::new(Arc::new(ClientPing(self.me.clone())))))
            .clone()
    }

    /// One gauge sample over this socket's clock, pinging where due. The ping is the same
    /// clock's as `ping_behind`'s and waits behind a held bulk frame as that one does; the
    /// transfer it paces is slowed by the hold, on a path that is late.
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

    /// Whether a bulk frame may be written now (`Flight::clear`); `waiting` says whether more
    /// of them wait behind it with its sender, asked only of a paced path and between two
    /// lockings of the gauge, so the sender's own lock is never taken under it.
    fn bulk_clear(&self, waiting: &Waiting) -> bool {
        let paced = self.flight.lock().paced();
        let more = paced && waiting();
        let mut flight = self.flight.lock();
        let at = now();
        let was = flight.paced();
        let clear = flight.clear(at, FLIGHT_LATE_S, more);
        if flight.paced() != was {
            debug!(target: "ws", "Path of {} {}: {} KiB in flight, {:.0} kbit/s delivered, least round trip {:.0} ms, {:.0} ms allowed its unevenness.",
                self.remote,
                if was { "on time again" } else { "late, its video paced to what it delivers" },
                flight.in_flight() / 1024,
                flight.rate(at).map_or(0.0, |rate| rate * 8.0 / 1000.0),
                flight.floor().map_or(0.0, |floor| floor * 1000.0),
                flight.slack() * 1000.0);
        }
        clear
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
/// frame was handed to the kernel, but for audio (the frames that go stale), which leaves
/// ahead of whatever else is waiting, in its own order: a key frame queued behind the path
/// does not hold the audio queued after it, as WebRTC's pacer sends audio ahead of video.
/// A frame already being written stays whole. A bulk frame (video, padding) waits while a late
/// path delivers what it was handed (`ws::flight`), and what is queued behind it waits with
/// it, as behind a frame the kernel is slow to take; audio goes on meanwhile, and so does a
/// transfer's ping. The wait ends as well with a frame a sender awaits queued behind it, and
/// after `FLIGHT_HOLD_MAX_S`. Ends when the connection is aborted, the sink fails, or the
/// client is dropped.
pub async fn run_writer<S>(
    client: Arc<Client>,
    mut rx: mpsc::UnboundedReceiver<Outgoing>,
    mut abort: watch::Receiver<bool>,
    mut sink: S,
) where
    S: Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    let mut audio: VecDeque<Outgoing> = VecDeque::new();
    let mut rest: VecDeque<Outgoing> = VecDeque::new();
    // Since when the bulk frame at the head of the rest has been held back.
    let mut held_since: Option<f64> = None;
    let lane = |out: Outgoing, audio: &mut VecDeque<Outgoing>, rest: &mut VecDeque<Outgoing>| {
        if out.stale_at.is_some() {
            audio.push_back(out);
        } else {
            rest.push_back(out);
        }
    };
    loop {
        while let Ok(out) = rx.try_recv() {
            lane(out, &mut audio, &mut rest);
        }
        // A bulk frame at the head of the rest waits for a late path, and what is queued
        // behind it with it, but never a frame a sender awaits (a reply the reader's own
        // handler waits on would keep it from the acks that end the wait, and a bounded
        // send would run out of time), and never longer than `FLIGHT_HOLD_MAX_S`.
        let held = rest
            .front()
            .and_then(|out| out.bulk.as_ref())
            .is_some_and(|waiting| !client.bulk_clear(waiting))
            && !rest
                .iter()
                .skip(1)
                .any(|out| out.bulk.is_none() && out.done.is_some())
            && now() - *held_since.get_or_insert_with(now) < FLIGHT_HOLD_MAX_S;
        let next = audio.pop_front().or_else(|| {
            if held {
                // A transfer's ping reads the path, not this wait.
                let ahead = rest.iter().position(|out| out.path_ping);
                ahead.and_then(|index| rest.remove(index))
            } else {
                // The frame at the head goes: the next one's wait is its own. (Audio taken
                // ahead of a frame whose wait just ran out leaves that frame its count.)
                held_since = None;
                rest.pop_front()
            }
        });
        let Some(out) = next else {
            tokio::select! {
                out = rx.recv() => match out {
                    Some(out) => lane(out, &mut audio, &mut rest),
                    None => break,
                },
                changed = abort.changed() => {
                    if changed.is_err() || *abort.borrow() {
                        break;
                    }
                }
                // An ack says more arrived; the tick ends a wait that ran too long.
                _ = client.flight_wake.notified(), if held => {}
                _ = tokio::time::sleep(FLIGHT_HELD_TICK), if held => {}
            }
            continue;
        };
        client.dequeued(out.frame.len());
        if out.stale_at.is_some_and(|stale_at| now() > stale_at) {
            let dropped = client.stale_audio.fetch_add(1, Ordering::Relaxed) + 1;
            if dropped.is_power_of_two() {
                debug!(target: "ws", "{dropped} stale audio frames let go for {}", client.remote);
            }
            if let Some(done) = out.done {
                let _ = done.send(Ok(()));
            }
            continue;
        }
        let bulk = out.bulk.is_some();
        // What the page counts of it: the bytes of a binary message, and the frame it will
        // name once it has it.
        let (counted, frame) = match &out.frame {
            Frame::Binary(data) => (data.len(), frame_id(data).filter(|_| bulk)),
            _ => (0, None),
        };
        let began = now();
        let message = out.frame.into_message();
        let result = tokio::select! {
            sent = sink.send(message) => sent.map_err(|e| {
                debug!(target: "ws", "Socket write to {} failed: {e}", client.remote);
                SendError::Closed
            }),
            _ = abort.changed() => Err(SendError::Closed),
        };
        if result.is_ok() {
            let at = now();
            let waited = at - began > FLIGHT_WRITE_WAIT_S;
            client.flight.lock().wrote(counted, at, bulk, frame, waited);
        }
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
    let mut left = audio;
    left.extend(rest);
    while let Some(out) = left.pop_front().or_else(|| rx.try_recv().ok()) {
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
            let task_link = link;
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

        /// A page whose frames go through the socket's own writer (`run_writer`) into what
        /// it was sent, the writer's pings among them: what the writer holds back from a
        /// late path is held from this page too.
        pub fn over_a_writer(id: ClientId, role: &str) -> Page {
            let unsent = Arc::new(AtomicI64::new(0));
            let perms = Perms {
                token: None,
                role: role.into(),
                slot: Json::Null,
                id: format!("{id:08x}"),
                connected_at: 0.0,
            };
            let (client, rx, abort) = Client::new(
                id,
                "127.0.0.1:1".parse().unwrap(),
                perms,
                Box::new(PageLink(unsent.clone())),
                Liveness::default(),
            );
            let sent = Arc::new(Mutex::new(Vec::new()));
            let log = sent.clone();
            let sink = futures::sink::unfold((), move |(), message: Message| {
                let log = log.clone();
                async move {
                    let frame = if message.is_ping() {
                        Frame::Ping(Bytes::copy_from_slice(message.as_payload()))
                    } else if let Some(text) = message.as_text() {
                        Frame::text(text)
                    } else {
                        Frame::Binary(Bytes::copy_from_slice(message.as_payload()))
                    };
                    log.lock().push(frame);
                    Ok::<(), std::io::Error>(())
                }
            });
            let task = tokio::spawn(run_writer(client.clone(), rx, abort, Box::pin(sink)));
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
    use crate::ws::flight::FLIGHT_DISTRUST_S;

    /// Audio queued behind a frame the path takes a second to carry is stale by its turn and
    /// let go unwritten, each sender told it is done; audio queued once the path is clear is
    /// written, so the page goes on from live audio instead of a backlog.
    #[tokio::test]
    async fn stale_audio_is_let_go_and_live_audio_follows() {
        let perms = Perms {
            token: None,
            role: "controller".into(),
            slot: Json::Null,
            id: "00000001".into(),
            connected_at: 0.0,
        };
        let (client, rx, abort) = Client::new(
            1,
            "127.0.0.1:1".parse().unwrap(),
            perms,
            Box::new(NoLink),
            Liveness::default(),
        );
        let written = Arc::new(Mutex::new(Vec::<usize>::new()));
        let log = written.clone();
        let sink = futures::sink::unfold((), move |(), message: Message| {
            let log = log.clone();
            async move {
                let len = message.as_payload().len();
                if len >= 100_000 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                log.lock().push(len);
                Ok::<(), std::io::Error>(())
            }
        });
        let writer = tokio::spawn(run_writer(client.clone(), rx, abort, Box::pin(sink)));
        client.fire(Frame::Binary(Bytes::from(vec![0u8; 100_000])));
        // The key frame is being written before any audio is queued.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let mut waits = Vec::new();
        for _ in 0..10 {
            waits.push(
                client
                    .send_audio_later(Frame::Binary(Bytes::from(vec![1u8; 100])))
                    .unwrap(),
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        for wait in waits {
            assert!(wait.await.is_ok(), "a let-go frame's sender is answered");
        }
        client
            .send_audio_later(Frame::Binary(Bytes::from(vec![1u8; 200])))
            .unwrap()
            .await
            .unwrap();
        assert_eq!(client.stale_audio(), 10);
        assert_eq!(*written.lock(), vec![100_000, 200]);
        writer.abort();
    }

    /// Audio queued behind two key frames leaves as soon as the one being written is done,
    /// ahead of the second, which leaves after it in its own order.
    #[tokio::test]
    async fn audio_leaves_ahead_of_queued_video() {
        let perms = Perms {
            token: None,
            role: "controller".into(),
            slot: Json::Null,
            id: "00000002".into(),
            connected_at: 0.0,
        };
        let (client, rx, abort) = Client::new(
            2,
            "127.0.0.1:1".parse().unwrap(),
            perms,
            Box::new(NoLink),
            Liveness::default(),
        );
        let written = Arc::new(Mutex::new(Vec::<usize>::new()));
        let log = written.clone();
        let sink = futures::sink::unfold((), move |(), message: Message| {
            let log = log.clone();
            async move {
                let len = message.as_payload().len();
                if len >= 100_000 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                log.lock().push(len);
                Ok::<(), std::io::Error>(())
            }
        });
        let writer = tokio::spawn(run_writer(client.clone(), rx, abort, Box::pin(sink)));
        client.fire(Frame::Binary(Bytes::from(vec![0u8; 100_000])));
        // The first key frame is being written when the second and the audio are queued.
        tokio::time::sleep(Duration::from_millis(20)).await;
        client.fire(Frame::Binary(Bytes::from(vec![0u8; 100_001])));
        client
            .send_audio_later(Frame::Binary(Bytes::from(vec![1u8; 100])))
            .unwrap()
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(*written.lock(), vec![100_000, 100, 100_001]);
        assert_eq!(client.stale_audio(), 0);
        writer.abort();
    }

    /// A socket's client, the sizes of what it was written, the pings it was written, and
    /// its writer.
    type Logged = (
        Arc<Client>,
        Arc<Mutex<Vec<usize>>>,
        Arc<Mutex<Vec<Vec<u8>>>>,
        tokio::task::JoinHandle<()>,
    );

    /// A socket whose writes are logged by size, the pings among them kept apart.
    fn logged_socket(id: ClientId) -> Logged {
        let perms = Perms {
            token: None,
            role: "controller".into(),
            slot: Json::Null,
            id: format!("{id:08x}"),
            connected_at: 0.0,
        };
        let (client, rx, abort) = Client::new(
            id,
            "127.0.0.1:1".parse().unwrap(),
            perms,
            Box::new(NoLink),
            Liveness::default(),
        );
        let written = Arc::new(Mutex::new(Vec::<usize>::new()));
        let pings = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let (log, asked) = (written.clone(), pings.clone());
        let sink = futures::sink::unfold((), move |(), message: Message| {
            let (log, asked) = (log.clone(), asked.clone());
            async move {
                if message.is_ping() {
                    asked.lock().push(message.as_payload().to_vec());
                } else {
                    log.lock().push(message.as_payload().len());
                }
                Ok::<(), std::io::Error>(())
            }
        });
        let writer = tokio::spawn(run_writer(client.clone(), rx, abort, Box::pin(sink)));
        (client, written, pings, writer)
    }

    /// A video frame of `len` bytes that carries `id`, as the page reads it.
    fn video(id: u16, len: usize) -> Frame {
        let mut data = vec![0u8; len];
        data[0] = 0x04;
        data[2..4].copy_from_slice(&id.to_be_bytes());
        Frame::Binary(Bytes::from(data))
    }

    /// An ack of the page's that left it now and took no time back, its clock an hour from
    /// this one: it has counted `received` bytes, frame `frame` the last it got, just now.
    fn stamp(client: &Client, received: u64, frame: u16) {
        let said = Said {
            received,
            frame,
            held: 0.0,
        };
        client.note_stamp((now() + 3600.0) * 1000.0, said);
    }

    /// A video frame is written at once while the page counts what it is sent; once a frame
    /// has stood out past the bound the next one waits in the writer, audio queued meanwhile
    /// goes out ahead of it, and the ack that says the path took what it held lets it go.
    #[tokio::test]
    async fn video_waits_for_a_late_path_while_audio_goes_on() {
        let (client, written, _pings, writer) = logged_socket(3);
        let (alone, more): (Waiting, Waiting) = (Arc::new(|| false), Arc::new(|| true));
        stamp(&client, 0, 0);
        client
            .send_bulk(video(1, 1000), "Video", false, &alone)
            .await
            .unwrap();
        stamp(&client, 1000, 1);
        client
            .send_bulk(video(2, 2000), "Video", false, &alone)
            .await
            .unwrap();
        assert!(!client.paced());
        tokio::time::sleep(Duration::from_secs_f64(1.5 * FLIGHT_LATE_S)).await;
        // The page had not counted it by now, its ack says.
        stamp(&client, 1000, 1);
        let held = {
            let client = client.clone();
            tokio::spawn(async move {
                client
                    .send_bulk(video(3, 3000), "Video", false, &more)
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(client.paced());
        client
            .send_audio_later(Frame::Binary(Bytes::from(vec![1u8; 100])))
            .unwrap()
            .await
            .unwrap();
        assert_eq!(*written.lock(), vec![1000, 2000, 100]);
        assert!(!held.is_finished(), "the frame waits for the path");
        // The path takes several times as long over the second frame as its sender took to
        // write it: it is narrowed under the stream, not merely a longer way round.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!held.is_finished());
        stamp(&client, 3100, 2);
        held.await.unwrap().unwrap();
        assert_eq!(*written.lock(), vec![1000, 2000, 100, 3000]);
        assert!(client.paced(), "more waits behind it: the pacing goes on");
        writer.abort();
    }

    /// A socket whose second frame is still out, by the page's own word, past the bound.
    async fn late_socket(id: ClientId) -> Logged {
        let (client, written, pings, writer) = logged_socket(id);
        let alone: Waiting = Arc::new(|| false);
        stamp(&client, 0, 0);
        client
            .send_bulk(video(1, 1000), "Video", false, &alone)
            .await
            .unwrap();
        stamp(&client, 1000, 1);
        client
            .send_bulk(video(2, 2000), "Video", false, &alone)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs_f64(1.5 * FLIGHT_LATE_S)).await;
        stamp(&client, 1000, 1);
        (client, written, pings, writer)
    }

    #[tokio::test]
    async fn only_a_page_that_stamps_has_video_held() {
        // An older page, or one that is not its display's own: its socket carries what it
        // carried before there was a gauge, nothing more and nothing of the gauge's, and
        // nothing of its video ever waits.
        let (client, written, pings, writer) = logged_socket(10);
        let more: Waiting = Arc::new(|| true);
        for k in 0..8u16 {
            client
                .send_bulk(video(k, 1000), "Video", false, &more)
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        assert_eq!(written.lock().len(), 8);
        assert!(!client.paced());
        // From its first stamp on its frames are told apart: one it counted, and the next
        // standing out past the bound, hold the one after.
        stamp(&client, 8000, 7);
        client
            .send_bulk(video(8, 2000), "Video", false, &more)
            .await
            .unwrap();
        stamp(&client, 10_000, 8);
        client
            .send_bulk(video(9, 2000), "Video", false, &more)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs_f64(1.5 * FLIGHT_LATE_S)).await;
        stamp(&client, 10_000, 8);
        let held = {
            let client = client.clone();
            tokio::spawn(async move {
                client
                    .send_bulk(video(10, 2000), "Video", false, &more)
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(client.paced() && !held.is_finished());
        assert!(
            pings.lock().is_empty(),
            "the gauge writes nothing of its own"
        );
        writer.abort();
    }

    #[tokio::test]
    async fn video_never_waits_on_a_stamp_that_left_before_it_was_written() {
        // The second frame stands out past the bound, and the page's last stamp left before
        // that frame was written: it says nothing of it, and nothing is paced.
        let (client, written, _pings, writer) = logged_socket(8);
        let more: Waiting = Arc::new(|| true);
        stamp(&client, 0, 0);
        client
            .send_bulk(video(1, 1000), "Video", false, &more)
            .await
            .unwrap();
        stamp(&client, 1000, 1);
        client
            .send_bulk(video(2, 2000), "Video", false, &more)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_secs_f64(1.5 * FLIGHT_LATE_S)).await;
        client
            .send_bulk(video(3, 3000), "Video", false, &more)
            .await
            .unwrap();
        assert_eq!(*written.lock(), vec![1000, 2000, 3000]);
        assert!(!client.paced());
        // The page's word that it had not counted the second by now paces the next one.
        stamp(&client, 1000, 1);
        let held = {
            let client = client.clone();
            tokio::spawn(async move {
                client
                    .send_bulk(video(4, 4000), "Video", false, &more)
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(client.paced());
        assert!(!held.is_finished());
        writer.abort();
    }

    /// A frame a sender awaits never waits behind a held video frame: the video frame goes
    /// ahead of it as the kernel takes it, in the order the two were queued. A frame nobody
    /// awaits (a ping of the round-trip clock) waits with the video.
    #[tokio::test]
    async fn an_awaited_frame_ends_the_wait_of_the_video_ahead_of_it() {
        let (client, written, _pings, writer) = late_socket(5).await;
        let more: Waiting = Arc::new(|| true);
        let held = {
            let client = client.clone();
            tokio::spawn(
                async move { client.send_bulk(video(3, 3000), "Video", true, &more).await },
            )
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!held.is_finished(), "held, its own ping behind it");
        assert_eq!(*written.lock(), vec![1000, 2000]);
        client.send(Frame::text("VIDEO_STOPPED")).await.unwrap();
        held.await.unwrap().unwrap();
        assert_eq!(
            *written.lock(),
            vec![1000, 2000, 3000, "VIDEO_STOPPED".len()]
        );
        writer.abort();
    }

    /// A transfer's ping goes out while a video frame is held, a ping of the round-trip
    /// clock waits behind the frame it was written behind.
    #[tokio::test]
    async fn a_ping_that_only_measures_does_not_wait_with_the_video_held() {
        let (client, written, pings, writer) = late_socket(7).await;
        let more: Waiting = Arc::new(|| true);
        let held = {
            let client = client.clone();
            tokio::spawn(async move {
                client
                    .send_bulk(video(3, 3000), "Video", false, &more)
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(30)).await;
        let behind = client.ping_behind().to_be_bytes().to_vec();
        assert!(ClientPing(Arc::downgrade(&client)).ping(b"transfer"));
        // The reader's heartbeat asks only whether the peer is there.
        assert!(client.fire_ahead(Frame::Ping(Bytes::new())));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!held.is_finished());
        assert_eq!(*written.lock(), vec![1000, 2000]);
        let asked = pings.lock().clone();
        assert!(
            asked.iter().any(|p| p == b"transfer") && asked.iter().any(|p| p.is_empty()),
            "ahead of the held frame"
        );
        // The clock's own ping says everything queued before it arrived, the held frame
        // among it: it waits.
        assert!(!asked.contains(&behind), "behind it");
        assert_eq!(asked.len(), 2, "and the gauge writes none of its own");
        stamp(&client, 3000, 2);
        held.await.unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(pings.lock().contains(&behind));
        writer.abort();
    }

    /// No frame waits in the writer longer than `FLIGHT_HOLD_MAX_S`, whatever the gauge says.
    #[tokio::test]
    async fn a_frame_is_held_no_longer_than_the_cap_while_its_page_stamps_on() {
        // The path stays late by the word of a page that stamps its acks on and counts
        // nothing more: nothing in the gauge lets the frame go, its own patience being
        // seconds longer, so the writer's cap does, at the two seconds no frame is held
        // past, and the path stays paced for the frames behind. Audio goes on meanwhile, as
        // it does in a session, a frame every 10 ms ahead of the video held: the cap counts
        // the frame's wait, not the time since the writer last looked.
        let (client, written, _pings, writer) = late_socket(9).await;
        let more: Waiting = Arc::new(|| true);
        let stamping = {
            let client = client.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    stamp(&client, 1000, 1);
                }
            })
        };
        let sounding = {
            let client = client.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    let _ = client.send_audio_later(Frame::Binary(Bytes::from(vec![1u8; 100])));
                }
            })
        };
        let start = now();
        let sent = tokio::time::timeout(
            Duration::from_secs(3),
            client.send_bulk(video(3, 3000), "Video", false, &more),
        )
        .await;
        let waited = now() - start;
        stamping.abort();
        sounding.abort();
        assert!(
            sent.is_ok_and(|sent| sent.is_ok()),
            "still held after {waited:.2} s"
        );
        assert!((1.9..2.5).contains(&waited), "{waited}");
        let written = written.lock().clone();
        let audio = written.iter().filter(|len| **len == 100).count();
        assert!(audio > 100, "{audio} audio frames went ahead meanwhile");
        let video: Vec<usize> = written.into_iter().filter(|len| *len != 100).collect();
        assert_eq!(video, vec![1000, 2000, 3000]);
        assert!(client.paced(), "the path is late still");
        writer.abort();
    }

    /// A page that stops stamping holds a video frame back no longer than its last stamp
    /// says anything.
    #[tokio::test]
    async fn a_held_frame_goes_once_its_page_has_stamped_nothing_for_too_long() {
        // The page's last stamp is half a second old when the frame asks to go: the frame
        // waits for as long as that stamp still says anything, short of the writer's cap,
        // and then the path is judged no more.
        let (client, written, _pings, writer) = late_socket(6).await;
        let more: Waiting = Arc::new(|| true);
        tokio::time::sleep(Duration::from_millis(500)).await;
        let start = now();
        client
            .send_bulk(video(3, 3000), "Video", false, &more)
            .await
            .unwrap();
        let waited = now() - start;
        assert!(
            (FLIGHT_DISTRUST_S - 0.7..FLIGHT_DISTRUST_S - 0.2).contains(&waited),
            "{waited}"
        );
        assert_eq!(*written.lock(), vec![1000, 2000, 3000]);
        assert!(!client.paced());
        writer.abort();
    }

    /// A page whose count never passes a frame is written to as the kernel takes it:
    /// nothing waits.
    #[tokio::test]
    async fn video_never_waits_for_a_page_whose_count_never_moved() {
        let (client, written, _pings, writer) = logged_socket(4);
        let more: Waiting = Arc::new(|| true);
        for k in 0..8u16 {
            stamp(&client, 0, 0);
            client
                .send_bulk(video(k, 1000), "Video", false, &more)
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        assert_eq!(written.lock().len(), 8);
        assert!(!client.paced());
        writer.abort();
    }

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
