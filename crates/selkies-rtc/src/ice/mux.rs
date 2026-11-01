/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Shared ICE sockets: one UDP port and one TCP port for every session.
//!
//! A deployment that can expose only a port or two forwards one UDP port and one TCP port and
//! has every session's host candidates share them, instead of an ephemeral port (or a
//! `webrtc_port_range` window) per session. A `UdpMux` binds the port on each host address, a
//! `TcpMux` listens on it, and each ICE connection attaches one `Attachment` per address under
//! its local username fragment; the connection sees a datagram path and never learns that the
//! socket is shared.
//!
//! Demultiplexing follows the ICE handshake itself. A datagram from a remote address already
//! known belongs to the connection it was learned for. One from an unknown address is only ever
//! STUN: a request names its connection in the USERNAME (`local:remote` fragments); a response
//! answers a transaction that an attachment registered when it sent the request, a connectivity
//! check or a server-reflexive query. A peer's address is learned from its request once the
//! connection has authenticated it (`Attachment::learn_peer`), or from its answer to a check,
//! never from a STUN server's answer, so a STUN server shared by every session maps to none of
//! them. Anything else is dropped.
//!
//! ICE-TCP (RFC 6544) rides the TCP port. Each accepted stream carries RFC 4571 frames, a
//! 16-bit length ahead of every packet, and its first frame must be the peer's STUN binding
//! request, whose USERNAME hands the stream to the connection it belongs to; from then on the
//! stream is that connection's datagram path to the peer's address, and losing it while it
//! carries the nominated pair ends the connection at once rather than after consent expires. A
//! stream that sends anything else first, or nothing within the first-frame timeout, is closed.
//! Writes are bounded: once a stream's send backlog passes `TCP_WRITE_HIGH_WATER`, packets are
//! dropped the way a full UDP socket drops them, so a slow link costs frames rather than memory,
//! and the media layers recover from loss as they do on UDP.
//!
//! Every reader hands a datagram to its peer's task over a bounded channel with `try_send`, so
//! a peer that cannot keep up loses datagrams rather than stalling the socket every session
//! shares.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use log::{debug, info};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;

use super::stun::{self, Class};
use super::{Inbound, Packet, Protocol};

/// Seconds an accepted TCP stream has to send its STUN binding request.
pub const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(30);
/// Send backlog, per stream, past which packets are dropped instead of queued.
pub const TCP_WRITE_HIGH_WATER: usize = 1 << 20;
/// The most a peer's inbound channel holds before datagrams are dropped.
pub const INBOUND_CHANNEL_CAPACITY: usize = 2048;
const FRAME_HEADER: usize = 2;
const MAX_FRAME: usize = 0xFFFF;
/// The receive buffer a UDP reader carves datagrams out of.
const UDP_CHUNK: usize = 128 * 1024;
/// A UDP datagram can be this large; a reader never offers the kernel less room.
const UDP_MAX_DATAGRAM: usize = 65_536;
/// Frames queued per TCP stream before the backlog bound applies.
const TCP_STREAM_QUEUE: usize = 4096;

type Tid = [u8; 12];

/// What attaching to or opening a mux fails on.
#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    #[error("username fragment {0:?} is already attached on {1}")]
    Attached(String, SocketAddr),
    #[error("the mux is closed")]
    Closed,
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Where a point delivers a connection's datagrams.
#[derive(Clone)]
struct Route {
    id: u64,
    ufrag: String,
    tx: mpsc::Sender<Inbound>,
}

/// Bookkeeping of one shared socket or listener on one local address.
///
/// Attached routes are keyed by their local username fragment, learned peers by remote
/// address, and STUN transactions in flight by transaction id together with whether their
/// answer identifies a peer.
#[derive(Default)]
struct Table {
    by_ufrag: HashMap<String, Route>,
    by_addr: HashMap<SocketAddr, Route>,
    by_tid: HashMap<Tid, (Route, bool)>,
}

impl Table {
    fn attach(&mut self, route: Route, local: SocketAddr) -> Result<(), MuxError> {
        if self.by_ufrag.contains_key(&route.ufrag) {
            return Err(MuxError::Attached(route.ufrag, local));
        }
        self.by_ufrag.insert(route.ufrag.clone(), route);
        Ok(())
    }

    fn detach(&mut self, id: u64) {
        self.by_ufrag.retain(|_, r| r.id != id);
        self.by_addr.retain(|_, r| r.id != id);
        self.by_tid.retain(|_, (r, _)| r.id != id);
    }

    fn learn(&mut self, addr: SocketAddr, route: &Route, local: SocketAddr) {
        if self.by_addr.get(&addr).map(|r| r.id) != Some(route.id) {
            debug!(target: "ice.mux", "{local} learned peer {addr} for ufrag {}", route.ufrag);
            self.by_addr.insert(addr, route.clone());
        }
    }

    /// The socket closed: tell every attached connection and forget them all.
    fn end(&mut self, local: SocketAddr) {
        for route in self.by_ufrag.values() {
            let _ = route.tx.try_send(Inbound::MuxClosed { local });
        }
        *self = Table::default();
    }

    fn route_by_id(&self, id: u64) -> Option<Route> {
        self.by_ufrag.values().find(|r| r.id == id).cloned()
    }

    /// The attached route a datagram from `addr` belongs to, or None.
    ///
    /// A learned peer's datagrams are routed without a look inside; only STUN from an unknown
    /// address is parsed, to find the connection it names.
    fn route(&mut self, data: &[u8], addr: SocketAddr, local: SocketAddr) -> Option<Route> {
        if let Some(route) = self.by_addr.get(&addr) {
            return Some(route.clone());
        }
        if !stun::is_stun(data) {
            return None;
        }
        let message = stun::parse(data).ok()?;
        match stun::class(&message) {
            Class::Success | Class::Error => {
                let (route, names_peer) = self.by_tid.get(&message.transaction_id.0)?.clone();
                if names_peer {
                    self.learn(addr, &route, local);
                }
                Some(route)
            }
            // Learned only once the connection authenticates it (`learn_peer`).
            Class::Request => self
                .by_ufrag
                .get(&stun::username_fragment(&message)?)
                .cloned(),
            Class::Indication => None,
        }
    }
}

/// Strip the flow and scope of an IPv6 address, so one peer is one key.
fn normalize(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(addr.ip(), addr.port())
}

/// Deliver a datagram to its route, dropping it when the peer's channel is full.
fn deliver(route: &Route, packet: Packet, dropped: &AtomicU64) -> bool {
    match route.tx.try_send(Inbound::Packet(packet)) {
        Ok(()) => true,
        Err(mpsc::error::TrySendError::Full(_)) => {
            dropped.fetch_add(1, Ordering::Relaxed);
            true
        }
        Err(mpsc::error::TrySendError::Closed(_)) => false,
    }
}

/// One shared UDP socket, bound to one local address on the mux port.
pub struct UdpPoint {
    local: SocketAddr,
    /// Taken at close, so the port is free whatever connections still hold the point.
    socket: RwLock<Option<Arc<UdpSocket>>>,
    table: Mutex<Table>,
    reader: Mutex<Option<JoinHandle<()>>>,
    dropped: AtomicU64,
}

impl UdpPoint {
    fn bind(address: IpAddr, port: u16) -> io::Result<Arc<UdpPoint>> {
        let socket = Arc::new(super::gather::bind_udp(address, &[port])?);
        let local = socket.local_addr()?;
        let point = Arc::new(UdpPoint {
            local,
            socket: RwLock::new(Some(socket.clone())),
            table: Mutex::new(Table::default()),
            reader: Mutex::new(None),
            dropped: AtomicU64::new(0),
        });
        let reader = tokio::spawn(Self::read_loop(point.clone(), socket));
        *point.reader.lock().unwrap() = Some(reader);
        Ok(point)
    }

    async fn read_loop(point: Arc<UdpPoint>, socket: Arc<UdpSocket>) {
        let mut buf = BytesMut::with_capacity(UDP_CHUNK);
        loop {
            if buf.capacity() < UDP_MAX_DATAGRAM {
                buf = BytesMut::with_capacity(UDP_CHUNK);
            }
            let peer = match socket.recv_buf_from(&mut buf).await {
                Ok((_, peer)) => normalize(peer),
                Err(e) => {
                    debug!(target: "ice.mux", "{} error_received({e})", point.local);
                    continue;
                }
            };
            let data = buf.split();
            if data.is_empty() {
                continue;
            }
            let route = point.table.lock().unwrap().route(&data, peer, point.local);
            let Some(route) = route else {
                debug!(target: "ice.mux", "{} dropped {} bytes from unknown peer {peer}", point.local, data.len());
                continue;
            };
            let packet = Packet {
                now: Instant::now(),
                local: point.local,
                peer,
                protocol: Protocol::Udp,
                data,
            };
            if !deliver(&route, packet, &point.dropped) {
                point.table.lock().unwrap().detach(route.id);
            }
        }
    }

    async fn close(&self) {
        let reader = self.reader.lock().unwrap().take();
        if let Some(reader) = reader {
            reader.abort();
            let _ = reader.await;
        }
        *self.socket.write().unwrap() = None;
        self.table.lock().unwrap().end(self.local);
    }

    fn send(&self, peer: SocketAddr, data: &[u8]) -> io::Result<()> {
        let guard = self.socket.read().unwrap();
        let socket = guard
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "the mux is closed"))?;
        match socket.try_send_to(data, peer) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

/// Every session's UDP host candidates on one port, one socket per local address.
///
/// `open` binds the port on the addresses given, so a port in use fails the service at
/// startup rather than the first session; `attach` binds lazily for an address that appeared
/// later.
pub struct UdpMux {
    port: u16,
    points: Mutex<HashMap<IpAddr, Arc<UdpPoint>>>,
    closed: AtomicBool,
    next_id: AtomicU64,
}

impl UdpMux {
    /// A mux on `port`, bound by `open` or on first `attach`.
    pub fn new(port: u16) -> UdpMux {
        UdpMux {
            port,
            points: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
        }
    }

    /// The port every host candidate on this mux carries.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The addresses bound so far.
    pub fn listen_addresses(&self) -> Vec<SocketAddr> {
        self.points
            .lock()
            .unwrap()
            .values()
            .map(|p| p.local)
            .collect()
    }

    /// Connections attached on `address`, or on every address.
    pub fn attached(&self, address: Option<IpAddr>) -> usize {
        let points = self.points.lock().unwrap();
        points
            .iter()
            .filter(|(a, _)| address.is_none_or(|x| x == **a))
            .map(|(_, p)| p.table.lock().unwrap().by_ufrag.len())
            .sum()
    }

    /// The peer addresses learned on `address`, routed to a connection without a look inside.
    pub fn learned_peers(&self, address: IpAddr) -> Vec<SocketAddr> {
        self.points
            .lock()
            .unwrap()
            .get(&address)
            .map(|p| p.table.lock().unwrap().by_addr.keys().copied().collect())
            .unwrap_or_default()
    }

    /// Datagrams dropped because a peer's channel was full, over every socket.
    pub fn dropped(&self) -> u64 {
        self.points
            .lock()
            .unwrap()
            .values()
            .map(|p| p.dropped.load(Ordering::Relaxed))
            .sum()
    }

    /// Bind the port on each address; on failure nothing stays bound.
    ///
    /// # Errors
    /// The kernel's reason for a bind that failed, typically a port in use.
    pub async fn open(&self, addresses: &[IpAddr]) -> io::Result<()> {
        for &address in addresses {
            if let Err(e) = self.bind(address) {
                self.close().await;
                return Err(e);
            }
        }
        Ok(())
    }

    fn bind(&self, address: IpAddr) -> io::Result<Arc<UdpPoint>> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(io::Error::other("UDP mux is closed"));
        }
        let mut points = self.points.lock().unwrap();
        if let Some(point) = points.get(&address) {
            return Ok(point.clone());
        }
        let point = UdpPoint::bind(address, self.port)?;
        points.insert(address, point.clone());
        Ok(point)
    }

    /// Attach a connection with local fragment `ufrag` on `address`; its datagrams arrive on
    /// `tx`.
    ///
    /// # Errors
    /// `Io` when the address could not be bound on the mux port, `Attached` when the fragment
    /// is already attached on that address.
    pub fn attach(
        &self,
        address: IpAddr,
        ufrag: &str,
        tx: mpsc::Sender<Inbound>,
    ) -> Result<Attachment, MuxError> {
        let point = self.bind(address)?;
        let route = Route {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            ufrag: ufrag.to_string(),
            tx,
        };
        let id = route.id;
        point.table.lock().unwrap().attach(route, point.local)?;
        Ok(Attachment {
            point: PointRef::Udp(point),
            id,
            detached: AtomicBool::new(false),
        })
    }

    /// Detach every connection and release the sockets, which are free to rebind on return.
    pub async fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let points: Vec<Arc<UdpPoint>> = self
            .points
            .lock()
            .unwrap()
            .drain()
            .map(|(_, p)| p)
            .collect();
        for point in points {
            point.close().await;
        }
    }
}

/// The write half of one accepted ICE-TCP stream, as its owner sends through it.
#[derive(Clone)]
struct StreamSender {
    tx: mpsc::Sender<Bytes>,
    backlog: Arc<AtomicUsize>,
    dropped: Arc<AtomicU64>,
    shutdown: Arc<Notify>,
}

impl StreamSender {
    /// Queue one frame, or drop it when the backlog is past the high-water mark.
    fn send(&self, data: &[u8]) {
        if data.len() > MAX_FRAME || self.backlog.load(Ordering::Relaxed) > TCP_WRITE_HIGH_WATER {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let mut frame = BytesMut::with_capacity(FRAME_HEADER + data.len());
        frame.extend_from_slice(&(data.len() as u16).to_be_bytes());
        frame.extend_from_slice(data);
        self.backlog.fetch_add(frame.len(), Ordering::Relaxed);
        if self.tx.try_send(frame.freeze()).is_err() {
            self.backlog
                .fetch_sub(FRAME_HEADER + data.len(), Ordering::Relaxed);
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// End the stream: its reader stops and the socket closes.
    fn close(&self) {
        self.shutdown.notify_one();
    }
}

/// One listening socket on the mux port, bound to one local address.
pub struct TcpPoint {
    local: SocketAddr,
    table: Mutex<Table>,
    streams: Mutex<HashMap<(u64, SocketAddr), StreamSender>>,
    acceptor: Mutex<Option<JoinHandle<()>>>,
    first_frame_timeout: Duration,
    dropped: AtomicU64,
    /// Flipped at close, which ends the streams no connection has claimed yet as well.
    closing: watch::Sender<bool>,
}

impl TcpPoint {
    async fn listen(
        address: IpAddr,
        port: u16,
        first_frame_timeout: Duration,
    ) -> io::Result<Arc<TcpPoint>> {
        let listener = TcpListener::bind(SocketAddr::new(address, port)).await?;
        let local = listener.local_addr()?;
        let point = Arc::new(TcpPoint {
            local,
            table: Mutex::new(Table::default()),
            streams: Mutex::new(HashMap::new()),
            acceptor: Mutex::new(None),
            first_frame_timeout,
            dropped: AtomicU64::new(0),
            closing: watch::channel(false).0,
        });
        let acceptor = tokio::spawn(Self::accept_loop(point.clone(), listener));
        *point.acceptor.lock().unwrap() = Some(acceptor);
        Ok(point)
    }

    async fn accept_loop(point: Arc<TcpPoint>, listener: TcpListener) {
        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    let _ = stream.set_nodelay(true);
                    let point = point.clone();
                    tokio::spawn(async move {
                        point.run_stream(stream, normalize(peer)).await;
                    });
                }
                Err(e) => {
                    debug!(target: "ice.mux", "{} accept error: {e}", point.local);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }

    /// One accepted ICE-TCP stream: RFC 4571 frames in, bounded frames out.
    ///
    /// The receive buffer is read into directly, so a packet is sliced out of it without a
    /// copy. The first frame must be a STUN request naming a connection.
    async fn run_stream(self: Arc<Self>, stream: TcpStream, peer: SocketAddr) {
        let (mut reader, mut writer) = stream.into_split();
        let (tx, mut rx) = mpsc::channel::<Bytes>(TCP_STREAM_QUEUE);
        let backlog = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicU64::new(0));
        let shutdown = Arc::new(Notify::new());
        let mut closing = self.closing.subscribe();
        let writer_backlog = backlog.clone();
        let writer_task = tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                let len = frame.len();
                if writer.write_all(&frame).await.is_err() {
                    break;
                }
                writer_backlog.fetch_sub(len, Ordering::Relaxed);
            }
        });
        let sender = StreamSender {
            tx,
            backlog,
            dropped: dropped.clone(),
            shutdown: shutdown.clone(),
        };
        let mut owner: Option<Route> = None;
        let mut buf = BytesMut::with_capacity(2 * (FRAME_HEADER + MAX_FRAME));
        let deadline = tokio::time::Instant::now() + self.first_frame_timeout;
        'read: loop {
            if buf.capacity() - buf.len() < FRAME_HEADER + MAX_FRAME {
                buf.reserve(FRAME_HEADER + MAX_FRAME);
            }
            let unclaimed = owner.is_none();
            let read = tokio::select! {
                read = reader.read_buf(&mut buf) => read,
                _ = shutdown.notified() => break 'read,
                _ = closing.changed() => break 'read,
                _ = tokio::time::sleep_until(deadline), if unclaimed => {
                    info!(
                        target: "ice",
                        "{} closing {peer}: no STUN binding request within {:.0}s",
                        self.local,
                        self.first_frame_timeout.as_secs_f64()
                    );
                    break 'read;
                }
            };
            match read {
                Ok(0) | Err(_) => break 'read,
                Ok(_) => {}
            }
            while buf.len() >= FRAME_HEADER {
                let length = u16::from_be_bytes([buf[0], buf[1]]) as usize;
                if buf.len() < FRAME_HEADER + length {
                    break;
                }
                let mut frame = buf.split_to(FRAME_HEADER + length);
                let _ = frame.split_to(FRAME_HEADER);
                if frame.is_empty() {
                    continue;
                }
                let route = match &owner {
                    Some(route) => route.clone(),
                    None => {
                        let Some(route) = self.claim(&frame, peer) else {
                            break 'read;
                        };
                        self.add_stream(route.id, peer, sender.clone());
                        owner = Some(route.clone());
                        route
                    }
                };
                let packet = Packet {
                    now: Instant::now(),
                    local: self.local,
                    peer,
                    protocol: Protocol::Tcp,
                    data: frame,
                };
                if !deliver(&route, packet, &self.dropped) {
                    self.table.lock().unwrap().detach(route.id);
                    break 'read;
                }
            }
        }
        writer_task.abort();
        if let Some(route) = owner {
            let still_ours = {
                let mut streams = self.streams.lock().unwrap();
                match streams.get(&(route.id, peer)) {
                    Some(s) if Arc::ptr_eq(&s.dropped, &dropped) => {
                        streams.remove(&(route.id, peer));
                        true
                    }
                    _ => false,
                }
            };
            let attached = self.table.lock().unwrap().route_by_id(route.id).is_some();
            if still_ours && attached {
                let _ = route.tx.try_send(Inbound::TcpStreamLost {
                    local: self.local,
                    peer,
                });
            }
        }
        let count = dropped.load(Ordering::Relaxed);
        if count > 0 {
            info!(target: "ice", "{} stream from {peer} dropped {count} packets to a backed-up link", self.local);
        }
    }

    /// The connection a stream's first frame names, if it is a STUN request for an attached
    /// fragment.
    fn claim(&self, frame: &[u8], peer: SocketAddr) -> Option<Route> {
        if !stun::is_stun(frame) {
            info!(target: "ice", "{} closing {peer}: first frame is not STUN", self.local);
            return None;
        }
        let message = match stun::parse(frame) {
            Ok(m) => m,
            Err(e) => {
                info!(target: "ice", "{} closing {peer}: malformed STUN ({e})", self.local);
                return None;
            }
        };
        let ufrag = stun::username_fragment(&message);
        let owner = if stun::class(&message) == Class::Request {
            ufrag
                .as_ref()
                .and_then(|u| self.table.lock().unwrap().by_ufrag.get(u).cloned())
        } else {
            None
        };
        if owner.is_none() {
            info!(
                target: "ice",
                "{} closing {peer}: no connection for username fragment {:?}",
                self.local,
                ufrag.unwrap_or_default()
            );
        }
        owner
    }

    fn add_stream(&self, id: u64, peer: SocketAddr, sender: StreamSender) {
        let previous = self.streams.lock().unwrap().insert((id, peer), sender);
        if let Some(previous) = previous {
            previous.close();
        }
    }

    fn close_streams_of(&self, id: u64) {
        let mut streams = self.streams.lock().unwrap();
        let keys: Vec<_> = streams.keys().filter(|(i, _)| *i == id).copied().collect();
        for key in keys {
            if let Some(stream) = streams.remove(&key) {
                stream.close();
            }
        }
    }

    async fn close(&self) {
        let acceptor = self.acceptor.lock().unwrap().take();
        if let Some(acceptor) = acceptor {
            acceptor.abort();
            let _ = acceptor.await;
        }
        let streams: Vec<StreamSender> = self
            .streams
            .lock()
            .unwrap()
            .drain()
            .map(|(_, s)| s)
            .collect();
        for stream in streams {
            stream.close();
        }
        self.closing.send_replace(true);
        self.table.lock().unwrap().end(self.local);
    }
}

/// Every session's passive TCP host candidates on one port, one listener per address.
pub struct TcpMux {
    port: u16,
    points: Mutex<HashMap<IpAddr, Arc<TcpPoint>>>,
    closed: AtomicBool,
    next_id: AtomicU64,
    first_frame_timeout: Duration,
}

impl TcpMux {
    /// A mux on `port`, listening after `open` or on first `attach`.
    pub fn new(port: u16) -> TcpMux {
        TcpMux {
            port,
            points: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            first_frame_timeout: FIRST_FRAME_TIMEOUT,
        }
    }

    /// A mux whose streams have `timeout` to send their first STUN request.
    pub fn with_first_frame_timeout(mut self, timeout: Duration) -> TcpMux {
        self.first_frame_timeout = timeout;
        self
    }

    /// The port every passive TCP candidate on this mux carries.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The addresses listened on so far.
    pub fn listen_addresses(&self) -> Vec<SocketAddr> {
        self.points
            .lock()
            .unwrap()
            .values()
            .map(|p| p.local)
            .collect()
    }

    /// Connections attached on `address`, or on every address.
    pub fn attached(&self, address: Option<IpAddr>) -> usize {
        let points = self.points.lock().unwrap();
        points
            .iter()
            .filter(|(a, _)| address.is_none_or(|x| x == **a))
            .map(|(_, p)| p.table.lock().unwrap().by_ufrag.len())
            .sum()
    }

    /// Listen on each address; on failure nothing stays bound.
    ///
    /// # Errors
    /// The kernel's reason for a bind that failed, typically a port in use.
    pub async fn open(&self, addresses: &[IpAddr]) -> io::Result<()> {
        for &address in addresses {
            if let Err(e) = self.listen(address).await {
                self.close().await;
                return Err(e);
            }
        }
        Ok(())
    }

    async fn listen(&self, address: IpAddr) -> io::Result<Arc<TcpPoint>> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(io::Error::other("TCP mux is closed"));
        }
        if let Some(point) = self.points.lock().unwrap().get(&address) {
            return Ok(point.clone());
        }
        let point = TcpPoint::listen(address, self.port, self.first_frame_timeout).await?;
        let mut points = self.points.lock().unwrap();
        match points.get(&address) {
            Some(existing) => Ok(existing.clone()),
            None => {
                points.insert(address, point.clone());
                Ok(point)
            }
        }
    }

    /// Attach a connection with local fragment `ufrag` on `address`; frames of the streams the
    /// peer opens arrive on `tx`.
    ///
    /// # Errors
    /// `Io` when the address could not be bound on the mux port, `Attached` when the fragment
    /// is already attached on that address.
    pub async fn attach(
        &self,
        address: IpAddr,
        ufrag: &str,
        tx: mpsc::Sender<Inbound>,
    ) -> Result<Attachment, MuxError> {
        let point = self.listen(address).await?;
        let route = Route {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            ufrag: ufrag.to_string(),
            tx,
        };
        let id = route.id;
        point.table.lock().unwrap().attach(route, point.local)?;
        Ok(Attachment {
            point: PointRef::Tcp(point),
            id,
            detached: AtomicBool::new(false),
        })
    }

    /// Detach every connection, end their streams, and stop listening; the port is free on
    /// return.
    pub async fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let points: Vec<Arc<TcpPoint>> = self
            .points
            .lock()
            .unwrap()
            .drain()
            .map(|(_, p)| p)
            .collect();
        for point in points {
            point.close().await;
        }
    }
}

enum PointRef {
    Udp(Arc<UdpPoint>),
    Tcp(Arc<TcpPoint>),
}

/// A connection's face of one shared socket or listener: it sends and closes through it, and
/// tells the mux which transactions and peers are its own.
pub struct Attachment {
    point: PointRef,
    id: u64,
    detached: AtomicBool,
}

impl Attachment {
    /// The bound address of the shared socket.
    pub fn local_addr(&self) -> SocketAddr {
        match &self.point {
            PointRef::Udp(p) => p.local,
            PointRef::Tcp(p) => p.local,
        }
    }

    /// The transport of the shared socket.
    pub fn protocol(&self) -> Protocol {
        match &self.point {
            PointRef::Udp(_) => Protocol::Udp,
            PointRef::Tcp(_) => Protocol::Tcp,
        }
    }

    /// Send `data` to `peer`: on UDP through the shared socket, on TCP through the stream the
    /// peer opened, dropped when none is open or the stream's backlog is past its bound.
    pub fn send(&self, peer: SocketAddr, data: &[u8]) -> io::Result<()> {
        if self.detached.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "detached from the mux",
            ));
        }
        match &self.point {
            PointRef::Udp(p) => p.send(peer, data),
            PointRef::Tcp(p) => {
                let stream = p.streams.lock().unwrap().get(&(self.id, peer)).cloned();
                match stream {
                    Some(stream) => stream.send(data),
                    None => {
                        debug!(target: "ice.mux", "{} no stream to {peer}, dropping {} bytes", p.local, data.len());
                    }
                }
                Ok(())
            }
        }
    }

    /// Route the answer to a STUN request this connection sends back to it.
    ///
    /// `names_peer` says the request is an ICE check, so the answer's source is the peer and
    /// is learned; a STUN server's answer is not.
    pub fn expect_response(&self, transaction_id: Tid, names_peer: bool) {
        let mut table = self.table();
        if let Some(route) = table.route_by_id(self.id) {
            table.by_tid.insert(transaction_id, (route, names_peer));
        }
    }

    /// Forget a transaction whose answer came or timed out.
    pub fn forget_response(&self, transaction_id: &Tid) {
        self.table().by_tid.remove(transaction_id);
    }

    /// Route `addr`'s datagrams to this connection: a check from it authenticated.
    pub fn learn_peer(&self, addr: SocketAddr) {
        let local = self.local_addr();
        let mut table = self.table();
        if let Some(route) = table.route_by_id(self.id) {
            table.learn(normalize(addr), &route, local);
        }
    }

    /// Whether `addr` is learned as this connection's peer.
    pub fn knows_peer(&self, addr: SocketAddr) -> bool {
        self.table().by_addr.get(&normalize(addr)).map(|r| r.id) == Some(self.id)
    }

    fn table(&self) -> std::sync::MutexGuard<'_, Table> {
        match &self.point {
            PointRef::Udp(p) => p.table.lock().unwrap(),
            PointRef::Tcp(p) => p.table.lock().unwrap(),
        }
    }

    /// Leave the shared socket: nothing more is routed here, and the streams this connection
    /// owned are closed.
    pub fn detach(&self) {
        if self.detached.swap(true, Ordering::SeqCst) {
            return;
        }
        self.table().detach(self.id);
        if let PointRef::Tcp(p) = &self.point {
            p.close_streams_of(self.id);
        }
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        self.detach();
    }
}

/// A bounded inbound channel for one peer, sized so a burst never blocks a shared socket.
pub fn inbound_channel() -> (mpsc::Sender<Inbound>, mpsc::Receiver<Inbound>) {
    mpsc::channel(INBOUND_CHANNEL_CAPACITY)
}
