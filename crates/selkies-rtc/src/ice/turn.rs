/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A relayed candidate: a TURN allocation driven through the sans-IO `rtc_turn` client.
//!
//! `allocate` reaches the server over UDP or TCP, authenticates with the long-term credential
//! the configuration carries, and returns once the server hands out a relayed address, which
//! the connection advertises as its relay candidate. From then on a task owns the socket to the
//! server: datagrams the peer sends to the relayed address arrive as `Inbound` packets tagged
//! with it, and what the connection sends to a peer goes out through the allocation, as a Send
//! indication until the server grants a permission and binds a channel for that peer, which
//! the client requests on the first send and refreshes itself. A `438 Stale nonce` to the
//! allocation starts it over, for the fresh nonce; `rtc_turn` reports it as a refusal, while a
//! server's nonce can lapse between the two requests or, on one balancing its listeners, differ
//! between them. Over TCP the stream carries
//! STUN and ChannelData messages padded to four bytes, each self-delimiting by its length
//! field, as RFC 5766 11.5 frames them; `turns:` wraps that stream in TLS, verified against the
//! system's root certificates under the server's name.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use log::{debug, info};
use rtc_shared::{TaggedBytesMut, TransportContext, TransportProtocol};
use rtc_turn::client::{Client, ClientConfig, Event};
use sansio::Protocol as _;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;

use super::config::TurnServer;
use super::gather::size_udp_socket;
use super::stun;
use super::{IceError, Inbound, Packet, Protocol};

/// Seconds an allocation has to come through before the relay candidate is given up.
pub const ALLOCATION_TIMEOUT: Duration = Duration::from_secs(5);
/// How many stale nonces an allocation goes through before it is given up.
const STALE_NONCE_RETRIES: u32 = 5;
/// Datagrams held for a peer while its permission is being created.
const PENDING_SENDS: usize = 64;

enum Command {
    Send { peer: SocketAddr, data: Vec<u8> },
    Close,
}

/// The connection's handle on one allocation: sends go through it, closing deletes it.
#[derive(Clone)]
pub struct TurnRelay {
    /// The relayed address the candidate advertises.
    pub relayed_addr: SocketAddr,
    /// The address of the socket to the server, the candidate's related address.
    pub related_addr: SocketAddr,
    tx: mpsc::Sender<Command>,
}

impl TurnRelay {
    /// Send `data` to `peer` through the relay, dropped when the relay task is backed up.
    pub fn send(&self, peer: SocketAddr, data: &[u8]) -> io::Result<()> {
        match self.tx.try_send(Command::Send {
            peer,
            data: data.to_vec(),
        }) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Ok(()),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "the TURN relay is gone",
            )),
        }
    }

    /// Delete the allocation.
    pub fn close(&self) {
        let _ = self.tx.try_send(Command::Close);
    }
}

/// A byte stream to the TURN server, plain or under TLS.
trait ServerStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> ServerStream for T {}

/// The socket to the TURN server.
enum ServerIo {
    Udp(UdpSocket),
    Stream {
        stream: Box<dyn ServerStream>,
        buffer: BytesMut,
    },
}

/// The system's root certificates, as a `turns:` connection trusts them.
fn system_roots() -> Result<rustls::RootCertStore, IceError> {
    let mut roots = rustls::RootCertStore::empty();
    for certificate in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(certificate);
    }
    if roots.is_empty() {
        return Err(IceError::Config(
            "no system root certificates for a turns: server".into(),
        ));
    }
    Ok(roots)
}

/// Open TLS to `host` over `stream`, trusting `roots`.
async fn connect_tls(
    stream: TcpStream,
    host: &str,
    roots: rustls::RootCertStore,
) -> Result<Box<dyn ServerStream>, IceError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| IceError::Config(format!("TURN TLS: {e}")))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| IceError::Config(format!("TURN TLS server name {host:?}: {e}")))?;
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    Ok(Box::new(connector.connect(name, stream).await?))
}

impl ServerIo {
    async fn send(&mut self, data: &[u8], server: SocketAddr) -> io::Result<()> {
        match self {
            ServerIo::Udp(socket) => match socket.send_to(data, server).await {
                // The ICMP answer to an earlier datagram: the client's retransmission covers it.
                Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => Ok(()),
                other => other.map(|_| ()),
            },
            ServerIo::Stream { stream, .. } => {
                let padding = (4 - data.len() % 4) % 4;
                stream.write_all(data).await?;
                if padding > 0 {
                    stream.write_all(&[0u8; 4][..padding]).await?;
                }
                Ok(())
            }
        }
    }

    /// The next message from the server: a datagram, or one framed message off the stream.
    async fn recv(&mut self) -> io::Result<BytesMut> {
        match self {
            ServerIo::Udp(socket) => {
                let mut buf = BytesMut::with_capacity(65_536);
                loop {
                    match socket.recv_buf(&mut buf).await {
                        // A server not up yet or restarting answers with ICMP on a connected
                        // socket; the client's retransmission covers it.
                        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => continue,
                        other => return other.map(|_| buf),
                    }
                }
            }
            ServerIo::Stream { stream, buffer } => loop {
                if let Some(message) = next_framed(buffer) {
                    return Ok(message);
                }
                if buffer.capacity() - buffer.len() < 65_536 {
                    buffer.reserve(65_536);
                }
                if stream.read_buf(buffer).await? == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "TURN stream closed",
                    ));
                }
            },
        }
    }
}

/// Split one STUN or ChannelData message, padded to four bytes, off a TURN stream.
fn next_framed(buffer: &mut BytesMut) -> Option<BytesMut> {
    if buffer.len() < 4 {
        return None;
    }
    let length = u16::from_be_bytes([buffer[2], buffer[3]]) as usize;
    let padded = length + (4 - length % 4) % 4;
    let header = if buffer[0] & 0xC0 == 0x40 { 4 } else { 20 };
    let total = header + padded;
    if buffer.len() < total {
        return None;
    }
    let mut message = buffer.split_to(total);
    message.truncate(header + length);
    Some(message)
}

/// Allocate a relayed address on `server`, delivering what peers send to it on `inbound`.
///
/// # Errors
/// The server could not be reached, refused the allocation, or did not answer within
/// `ALLOCATION_TIMEOUT`.
pub async fn allocate(
    server: TurnServer,
    inbound: mpsc::Sender<Inbound>,
) -> Result<TurnRelay, IceError> {
    let roots = if server.tls {
        Some(system_roots()?)
    } else {
        None
    };
    allocate_trusting(server, inbound, roots).await
}

/// `allocate`, a `turns:` server verified against `roots`.
pub(super) async fn allocate_trusting(
    server: TurnServer,
    inbound: mpsc::Sender<Inbound>,
    roots: Option<rustls::RootCertStore>,
) -> Result<TurnRelay, IceError> {
    let addresses: Vec<SocketAddr> = tokio::net::lookup_host((server.host.as_str(), server.port))
        .await?
        .collect();
    let first = addresses
        .first()
        .copied()
        .ok_or_else(|| IceError::Config(format!("TURN server {} does not resolve", server.host)))?;
    let (mut io, local_addr, protocol, server_addr) = match server.transport {
        Protocol::Udp => {
            // Nothing answers a UDP socket to say an address is the wrong one, so IPv4 first.
            let server_addr = addresses
                .iter()
                .copied()
                .find(SocketAddr::is_ipv4)
                .unwrap_or(first);
            let bind: SocketAddr = if server_addr.is_ipv4() {
                "0.0.0.0:0"
            } else {
                "[::]:0"
            }
            .parse()
            .expect("a literal");
            let std_socket = std::net::UdpSocket::bind(bind)?;
            std_socket.set_nonblocking(true)?;
            size_udp_socket(&socket2::SockRef::from(&std_socket));
            std_socket.connect(server_addr)?;
            let socket = UdpSocket::from_std(std_socket)?;
            let local = socket.local_addr()?;
            (
                ServerIo::Udp(socket),
                local,
                TransportProtocol::UDP,
                server_addr,
            )
        }
        Protocol::Tcp => {
            let stream = TcpStream::connect(&addresses[..]).await?;
            let server_addr = stream.peer_addr()?;
            let _ = stream.set_nodelay(true);
            let local = stream.local_addr()?;
            let stream: Box<dyn ServerStream> = match roots {
                Some(roots) => connect_tls(stream, &server.host, roots).await?,
                None => Box::new(stream),
            };
            (
                ServerIo::Stream {
                    stream,
                    buffer: BytesMut::with_capacity(65_536),
                },
                local,
                TransportProtocol::TCP,
                server_addr,
            )
        }
    };
    let config = ClientConfig {
        stun_serv_addr: String::new(),
        turn_serv_addr: server_addr.to_string(),
        local_addr,
        transport_protocol: protocol,
        username: server.username.clone().unwrap_or_default(),
        password: server.password.clone().unwrap_or_default(),
        realm: String::new(),
        software: String::new(),
        rto_in_ms: 0,
        allocation_refresh_interval_cap: None,
    };
    let mut client = Client::new(config, stun::crypto())
        .map_err(|e| IceError::Config(format!("TURN client: {e}")))?;
    let now = Instant::now();
    client
        .allocate(now)
        .map_err(|e| IceError::Config(format!("TURN allocate: {e}")))?;
    flush(&mut client, &mut io, server_addr).await?;
    let deadline = tokio::time::Instant::now() + ALLOCATION_TIMEOUT;
    let mut stale_nonces = 0;
    let relayed = loop {
        let timeout = client.poll_timeout().map(tokio::time::Instant::from_std);
        let recv = io.recv();
        let message = tokio::select! {
            r = recv => Some(r?),
            _ = async { match timeout { Some(t) => tokio::time::sleep_until(t).await, None => std::future::pending().await } } => None,
            _ = tokio::time::sleep_until(deadline) => {
                return Err(IceError::Config("TURN allocation timed out".into()));
            }
        };
        let now = Instant::now();
        match message {
            Some(data) => {
                let _ = client.handle_read(TaggedBytesMut {
                    now,
                    transport: TransportContext {
                        local_addr,
                        peer_addr: server_addr,
                        transport_protocol: protocol,
                        ecn: None,
                    },
                    message: data,
                });
            }
            None => {
                let _ = client.handle_timeout(now);
            }
        }
        flush(&mut client, &mut io, server_addr).await?;
        let mut outcome = None;
        while let Some(event) = client.poll_event() {
            match event {
                Event::AllocateResponse(_, relayed) => outcome = Some(Ok(relayed)),
                Event::AllocateError(_, e) => {
                    if e.to_string().contains("error 438") && stale_nonces < STALE_NONCE_RETRIES {
                        stale_nonces += 1;
                        debug!(target: "ice.turn", "stale nonce from {server_addr}, allocating again");
                        client
                            .allocate(Instant::now())
                            .map_err(|e| IceError::Config(format!("TURN allocate: {e}")))?;
                    } else {
                        outcome = Some(Err(IceError::Config(format!(
                            "TURN allocation refused: {e}"
                        ))));
                    }
                }
                Event::TransactionTimeout(_) => {
                    outcome = Some(Err(IceError::Config("TURN allocation timed out".into())))
                }
                _ => {}
            }
        }
        flush(&mut client, &mut io, server_addr).await?;
        if let Some(outcome) = outcome {
            break outcome?;
        }
    };
    info!(target: "ice", "TURN allocation created {relayed} on {}:{}", server.host, server.port);
    let (tx, rx) = mpsc::channel(1024);
    let relay = TurnRelay {
        relayed_addr: relayed,
        related_addr: local_addr,
        tx,
    };
    tokio::spawn(run(RelayTask {
        client,
        io,
        server_addr,
        local_addr,
        protocol,
        relayed,
        inbound,
        commands: rx,
    }));
    Ok(relay)
}

async fn flush(client: &mut Client, io: &mut ServerIo, server: SocketAddr) -> io::Result<()> {
    while let Some(message) = client.poll_write() {
        io.send(&message.message, server).await?;
    }
    Ok(())
}

/// The task that owns one allocation's socket and client.
struct RelayTask {
    client: Client,
    io: ServerIo,
    server_addr: SocketAddr,
    local_addr: SocketAddr,
    protocol: TransportProtocol,
    relayed: SocketAddr,
    inbound: mpsc::Sender<Inbound>,
    commands: mpsc::Receiver<Command>,
}

async fn run(task: RelayTask) {
    let RelayTask {
        mut client,
        mut io,
        server_addr,
        local_addr,
        protocol,
        relayed,
        inbound,
        mut commands,
    } = task;
    let mut permitted: HashSet<SocketAddr> = HashSet::new();
    let mut requested: HashSet<SocketAddr> = HashSet::new();
    let mut pending: HashMap<SocketAddr, VecDeque<Vec<u8>>> = HashMap::new();
    loop {
        let timeout = client.poll_timeout().map(tokio::time::Instant::from_std);
        tokio::select! {
            command = commands.recv() => {
                let now = Instant::now();
                match command {
                    Some(Command::Send { peer, data }) => {
                        if permitted.contains(&peer) {
                            if let Ok(mut relay) = client.relay(relayed)
                                && let Err(e) = relay.send_to(now, &data, peer)
                            {
                                debug!(target: "ice.turn", "relay send to {peer} failed: {e}");
                            }
                        } else {
                            let queue = pending.entry(peer).or_default();
                            if queue.len() < PENDING_SENDS {
                                queue.push_back(data);
                            }
                            if requested.insert(peer)
                                && let Ok(mut relay) = client.relay(relayed)
                                && let Err(e) = relay.create_permission(now, peer)
                            {
                                debug!(target: "ice.turn", "create permission for {peer} failed: {e}");
                            }
                        }
                    }
                    Some(Command::Close) | None => {
                        if let Ok(mut relay) = client.relay(relayed) {
                            let _ = relay.close(now);
                        }
                        let _ = flush(&mut client, &mut io, server_addr).await;
                        info!(target: "ice", "TURN allocation deleted {relayed}");
                        return;
                    }
                }
            }
            received = io.recv() => {
                let now = Instant::now();
                match received {
                    Ok(data) => {
                        let _ = client.handle_read(TaggedBytesMut {
                            now,
                            transport: TransportContext {
                                local_addr,
                                peer_addr: server_addr,
                                transport_protocol: protocol,
                                ecn: None,
                            },
                            message: data,
                        });
                    }
                    Err(e) => {
                        info!(target: "ice", "TURN server {server_addr} connection lost: {e}");
                        return;
                    }
                }
            }
            _ = async { match timeout { Some(t) => tokio::time::sleep_until(t).await, None => std::future::pending().await } } => {
                let _ = client.handle_timeout(Instant::now());
            }
        }
        let now = Instant::now();
        while let Some(event) = client.poll_event() {
            match event {
                Event::DataIndicationOrChannelData(_, peer, data) => {
                    let packet = Packet {
                        now,
                        local: relayed,
                        peer,
                        protocol: Protocol::Udp,
                        data,
                    };
                    if inbound.try_send(Inbound::Packet(packet)).is_err() && inbound.is_closed() {
                        return;
                    }
                }
                Event::CreatePermissionResponse(_, peer) => {
                    permitted.insert(peer);
                    if let Some(queue) = pending.remove(&peer)
                        && let Ok(mut relay) = client.relay(relayed)
                    {
                        for data in queue {
                            let _ = relay.send_to(now, &data, peer);
                        }
                    }
                }
                Event::CreatePermissionError(_, e) => {
                    debug!(target: "ice.turn", "TURN permission refused: {e}");
                    requested.clear();
                }
                _ => {}
            }
        }
        if flush(&mut client, &mut io, server_addr).await.is_err() {
            info!(target: "ice", "TURN server {server_addr} connection lost");
            return;
        }
    }
}
