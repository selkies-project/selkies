use super::*;
use crate::association::Event;
use shared::error::{Error, Result};

use crate::AssociationError;
use crate::association::state::{AckMode, AssociationState};
use crate::association::stream::{ReliabilityType, Stream};
use crate::chunk::chunk_abort::ChunkAbort;
use crate::chunk::chunk_cookie_echo::ChunkCookieEcho;
use crate::chunk::chunk_error::ChunkError;
use crate::chunk::chunk_forward_tsn::ChunkForwardTsn;
use crate::chunk::chunk_heartbeat::ChunkHeartbeat;
use crate::chunk::chunk_init::ChunkInit;
use crate::chunk::chunk_payload_data::{ChunkPayloadData, PayloadProtocolIdentifier};
use crate::chunk::chunk_reconfig::ChunkReconfig;
use crate::chunk::chunk_selective_ack::{ChunkSelectiveAck, GapAckBlock};
use crate::chunk::chunk_shutdown::ChunkShutdown;
use crate::chunk::chunk_shutdown_ack::ChunkShutdownAck;
use crate::chunk::chunk_shutdown_complete::ChunkShutdownComplete;
use crate::chunk::{ErrorCauseProtocolViolation, PROTOCOL_VIOLATION};
use crate::packet::{CommonHeader, Packet};
use crate::param::param_outgoing_reset_request::ParamOutgoingResetRequest;
use crate::param::param_reconfig_response::ParamReconfigResponse;
use assert_matches::assert_matches;
use lazy_static::lazy_static;
use log::{info, trace};
use std::collections::VecDeque;
use std::net::Ipv6Addr;
use std::ops::RangeFrom;
use std::str::FromStr;
use std::sync::Mutex;
use std::{cmp, mem, net::UdpSocket, time::Duration};

lazy_static! {
    pub static ref SERVER_PORTS: Mutex<RangeFrom<u16>> = Mutex::new(4433..);
    pub static ref CLIENT_PORTS: Mutex<RangeFrom<u16>> = Mutex::new(44433..);
}

fn min_opt<T: Ord>(x: Option<T>, y: Option<T>) -> Option<T> {
    match (x, y) {
        (Some(x), Some(y)) => Some(cmp::min(x, y)),
        (Some(x), _) => Some(x),
        (_, Some(y)) => Some(y),
        _ => None,
    }
}

/// The maximum of datagrams TestEndpoint will produce via `poll_transmit`
const MAX_DATAGRAMS: usize = 10;

fn split_transmit(transmit: TransportMessage<Payload>) -> Vec<TransportMessage<Payload>> {
    let mut transmits = Vec::new();
    if let Payload::RawEncode(contents) = transmit.message {
        for content in contents {
            transmits.push(TransportMessage {
                now: transmit.now,
                transport: transmit.transport,
                message: Payload::RawEncode(vec![content]),
            });
        }
    }

    transmits
}

pub fn client_config() -> ClientConfig {
    ClientConfig::new(TransportConfig::default())
}

pub fn server_config() -> ServerConfig {
    ServerConfig::new(TransportConfig::default())
}

struct TestEndpoint {
    endpoint: Endpoint,
    addr: SocketAddr,
    socket: Option<UdpSocket>,
    timeout: Option<Instant>,
    outbound: VecDeque<TransportMessage<Payload>>,
    delayed: VecDeque<TransportMessage<Payload>>,
    inbound: VecDeque<(Instant, Option<EcnCodepoint>, Bytes)>,
    accepted: Option<AssociationHandle>,
    associations: HashMap<AssociationHandle, Association>,
    conn_events: HashMap<AssociationHandle, VecDeque<AssociationEvent>>,
}

impl TestEndpoint {
    fn new(endpoint: Endpoint, addr: SocketAddr) -> Self {
        let socket = UdpSocket::bind(addr).expect("failed to bind UDP socket");
        socket
            .set_read_timeout(Some(Duration::new(0, 10_000_000)))
            .unwrap();

        Self {
            endpoint,
            addr,
            socket: Some(socket),
            timeout: None,
            outbound: VecDeque::new(),
            delayed: VecDeque::new(),
            inbound: VecDeque::new(),
            accepted: None,
            associations: HashMap::default(),
            conn_events: HashMap::default(),
        }
    }

    pub fn drive(&mut self, now: Instant, remote: SocketAddr) {
        if let Some(ref socket) = self.socket {
            loop {
                let mut buf = [0; 8192];
                if socket.recv_from(&mut buf).is_err() {
                    break;
                }
            }
        }

        while self.inbound.front().map_or(false, |x| x.0 <= now) {
            let (recv_time, ecn, packet) = self.inbound.pop_front().unwrap();
            if let Some((ch, event)) = self.endpoint.handle(recv_time, remote, ecn, packet.into()) {
                match event {
                    DatagramEvent::NewAssociation(conn) => {
                        self.associations.insert(ch, conn);
                        self.accepted = Some(ch);
                    }
                    DatagramEvent::AssociationEvent(event) => {
                        self.conn_events
                            .entry(ch)
                            .or_insert_with(VecDeque::new)
                            .push_back(event);
                    }
                }
            }
        }

        let mut endpoint_events: Vec<(AssociationHandle, EndpointEvent)> = vec![];
        for (ch, conn) in self.associations.iter_mut() {
            if self.timeout.map_or(false, |x| x <= now) {
                self.timeout = None;
                conn.handle_timeout(now);
            }

            for (_, mut events) in self.conn_events.drain() {
                for event in events.drain(..) {
                    conn.handle_event(event);
                }
            }

            while let Some(event) = conn.poll_endpoint_event() {
                endpoint_events.push((*ch, event));
            }

            while let Some(x) = conn.poll_transmit(now) {
                self.outbound.extend(split_transmit(x));
            }
            self.timeout = conn.poll_timeout();
        }

        for (ch, event) in endpoint_events {
            self.handle_event(ch, event);
        }
    }

    pub fn next_wakeup(&self) -> Option<Instant> {
        let next_inbound = self.inbound.front().map(|x| x.0);
        min_opt(self.timeout, next_inbound)
    }

    fn is_idle(&self) -> bool {
        self.associations.values().all(|x| x.is_idle())
    }

    pub fn delay_outbound(&mut self) {
        assert!(self.delayed.is_empty());
        mem::swap(&mut self.delayed, &mut self.outbound);
    }

    pub fn finish_delay(&mut self) {
        self.outbound.extend(self.delayed.drain(..));
    }

    pub fn assert_accept(&mut self) -> AssociationHandle {
        self.accepted.take().expect("server didn't connect")
    }
}

impl ::std::ops::Deref for TestEndpoint {
    type Target = Endpoint;
    fn deref(&self) -> &Endpoint {
        &self.endpoint
    }
}

impl ::std::ops::DerefMut for TestEndpoint {
    fn deref_mut(&mut self) -> &mut Endpoint {
        &mut self.endpoint
    }
}

struct Pair {
    server: TestEndpoint,
    client: TestEndpoint,
    time: Instant,
    latency: Duration, // One-way
}

impl Pair {
    pub fn new(endpoint_config: EndpointConfig, server_config: ServerConfig) -> Self {
        let endpoint_config = Arc::new(endpoint_config);
        let server_addr = SocketAddr::new(
            Ipv6Addr::LOCALHOST.into(),
            SERVER_PORTS.lock().unwrap().next().unwrap(),
        );
        let client_addr = SocketAddr::new(
            Ipv6Addr::LOCALHOST.into(),
            CLIENT_PORTS.lock().unwrap().next().unwrap(),
        );

        let server = Endpoint::new(
            server_addr,
            TransportProtocol::UDP,
            Arc::clone(&endpoint_config),
            Some(Arc::new(server_config)),
        );
        let client = Endpoint::new(client_addr, TransportProtocol::UDP, endpoint_config, None);

        Pair::new_from_endpoint(client, server, client_addr, server_addr)
    }

    pub fn new_from_endpoint(
        client: Endpoint,
        server: Endpoint,
        client_addr: SocketAddr,
        server_addr: SocketAddr,
    ) -> Self {
        Self {
            server: TestEndpoint::new(server, server_addr),
            client: TestEndpoint::new(client, client_addr),
            time: Instant::now(),
            latency: Duration::new(0, 0),
        }
    }

    /// Returns whether the association is not idle
    pub fn step(&mut self) -> bool {
        self.drive_client();
        self.drive_server();
        if self.client.is_idle() && self.server.is_idle() {
            return false;
        }

        let client_t = self.client.next_wakeup();
        let server_t = self.server.next_wakeup();
        match min_opt(client_t, server_t) {
            Some(t) if Some(t) == client_t => {
                if t != self.time {
                    self.time = self.time.max(t);
                    trace!("advancing to {:?} for client", self.time);
                }
                true
            }
            Some(t) if Some(t) == server_t => {
                if t != self.time {
                    self.time = self.time.max(t);
                    trace!("advancing to {:?} for server", self.time);
                }
                true
            }
            Some(_) => unreachable!(),
            None => false,
        }
    }

    /// Advance time until both associations are idle
    pub fn drive(&mut self) {
        while self.step() {}
    }

    pub fn drive_client(&mut self) {
        self.client.drive(self.time, self.server.addr);
        for x in self.client.outbound.drain(..) {
            if let Payload::RawEncode(contents) = x.message {
                for content in contents {
                    if let Some(ref socket) = self.client.socket {
                        socket.send_to(&content, x.transport.peer_addr).unwrap();
                    }
                    if self.server.addr == x.transport.peer_addr {
                        self.server.inbound.push_back((
                            self.time + self.latency,
                            x.transport.ecn,
                            content,
                        ));
                    }
                }
            }
        }
    }

    pub fn drive_server(&mut self) {
        self.server.drive(self.time, self.client.addr);
        for x in self.server.outbound.drain(..) {
            if let Payload::RawEncode(contents) = x.message {
                for content in contents {
                    if let Some(ref socket) = self.server.socket {
                        socket.send_to(&content, x.transport.peer_addr).unwrap();
                    }
                    if self.client.addr == x.transport.peer_addr {
                        self.client.inbound.push_back((
                            self.time + self.latency,
                            x.transport.ecn,
                            content,
                        ));
                    }
                }
            }
        }
    }

    pub fn connect(&mut self) -> (AssociationHandle, AssociationHandle) {
        self.connect_with(client_config())
    }

    pub fn connect_with(&mut self, config: ClientConfig) -> (AssociationHandle, AssociationHandle) {
        info!("connecting");
        let client_ch = self.begin_connect(config);
        self.drive();
        let server_ch = self.server.assert_accept();
        self.finish_connect(client_ch, server_ch);
        (client_ch, server_ch)
    }

    /// Just start connecting the client
    pub fn begin_connect(&mut self, config: ClientConfig) -> AssociationHandle {
        let (client_ch, client_conn) = self
            .client
            .connect(self.time, config, self.server.addr)
            .unwrap();
        self.client.associations.insert(client_ch, client_conn);
        client_ch
    }

    fn finish_connect(&mut self, client_ch: AssociationHandle, server_ch: AssociationHandle) {
        assert_matches!(
            self.client_conn_mut(client_ch).poll(),
            Some(Event::Connected { .. })
        );

        assert_matches!(
            self.server_conn_mut(server_ch).poll(),
            Some(Event::Connected { .. })
        );
    }

    pub fn client_conn_mut(&mut self, ch: AssociationHandle) -> &mut Association {
        self.client.associations.get_mut(&ch).unwrap()
    }

    pub fn client_stream(&mut self, ch: AssociationHandle, si: u16) -> Result<Stream<'_>> {
        self.client_conn_mut(ch).stream(si)
    }

    pub fn server_conn_mut(&mut self, ch: AssociationHandle) -> &mut Association {
        self.server.associations.get_mut(&ch).unwrap()
    }

    pub fn server_stream(&mut self, ch: AssociationHandle, si: u16) -> Result<Stream<'_>> {
        self.server_conn_mut(ch).stream(si)
    }
}

impl Default for Pair {
    fn default() -> Self {
        Pair::new(Default::default(), server_config())
    }
}

fn create_association_pair(
    ack_mode: AckMode,
    recv_buf_size: u32,
) -> Result<(Pair, AssociationHandle, AssociationHandle)> {
    let mut pair = Pair::new(
        EndpointConfig::default(),
        ServerConfig {
            transport: Arc::new(if recv_buf_size > 0 {
                TransportConfig::default().with_max_receive_buffer_size(recv_buf_size)
            } else {
                TransportConfig::default()
            }),
            ..Default::default()
        },
    );
    let (client_ch, server_ch) = pair.connect_with(ClientConfig {
        transport: Arc::new(if recv_buf_size > 0 {
            TransportConfig::default().with_max_receive_buffer_size(recv_buf_size)
        } else {
            TransportConfig::default()
        }),
    });
    pair.client_conn_mut(client_ch).ack_mode = ack_mode;
    pair.server_conn_mut(server_ch).ack_mode = ack_mode;
    Ok((pair, client_ch, server_ch))
}

fn establish_session_pair(
    pair: &mut Pair,
    client_ch: AssociationHandle,
    server_ch: AssociationHandle,
    si: u16,
) -> Result<()> {
    let hello_msg = Bytes::from_static(b"Hello");
    let _ = pair
        .client_conn_mut(client_ch)
        .open_stream(si, PayloadProtocolIdentifier::Binary)?;
    let now = pair.time;
    let _ = pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &hello_msg,
        PayloadProtocolIdentifier::Dcep,
    )?;
    pair.drive();

    {
        let s1 = pair.server_conn_mut(server_ch).accept_stream().unwrap();
        if si != s1.stream_identifier {
            return Err(Error::Other("si should match".to_owned()).into());
        }
    }
    pair.drive();

    let mut buf = vec![0u8; 1024];
    let chunks = pair.server_stream(server_ch, si)?.read_sctp()?.unwrap();
    let n = chunks.read(&mut buf)?;

    if n != hello_msg.len() {
        return Err(Error::Other("received data must by 3 bytes".to_owned()).into());
    }

    if chunks.ppi != PayloadProtocolIdentifier::Dcep {
        return Err(Error::Other("unexpected ppi".to_owned()).into());
    }

    if &buf[..n] != &hello_msg {
        return Err(Error::Other("received data mismatch".to_owned()).into());
    }
    pair.drive();

    Ok(())
}

fn close_association_pair(
    _pair: &mut Pair,
    _client_ch: AssociationHandle,
    _server_ch: AssociationHandle,
    _si: u16,
) {
    /*TODO:
    // Close client
    tokio::spawn(async move {
        client.close().await?;
        let _ = handshake0ch_tx.send(()).await;
        let _ = closed_rx0.recv().await;

        Result::<()>::Ok(())
    });

    // Close server
    tokio::spawn(async move {
        server.close().await?;
        let _ = handshake1ch_tx.send(()).await;
        let _ = closed_rx1.recv().await;

        Result::<()>::Ok(())
    });
    */
}

#[test]
fn test_assoc_reliable_simple() -> Result<()> {
    //let _guard = subscribe();

    let si: u16 = 1;
    let msg: Bytes = Bytes::from_static(b"ABC");

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;

    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    {
        let a = pair.client_conn_mut(client_ch);
        assert_eq!(0, a.buffered_amount(), "incorrect bufferedAmount");
    }

    let now = pair.time;
    let n = pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &msg,
        PayloadProtocolIdentifier::Binary,
    )?;
    assert_eq!(msg.len(), n, "unexpected length of received data");
    {
        let a = pair.client_conn_mut(client_ch);
        assert_eq!(msg.len(), a.buffered_amount(), "incorrect bufferedAmount");
    }

    pair.drive();

    let chunks = pair.server_stream(server_ch, si)?.read_sctp()?.unwrap();
    let (n, ppi) = (chunks.len(), chunks.ppi);
    assert_eq!(n, msg.len(), "unexpected length of received data");
    assert_eq!(ppi, PayloadProtocolIdentifier::Binary, "unexpected ppi");

    {
        let q = &pair
            .client_conn_mut(client_ch)
            .streams
            .get(&si)
            .unwrap()
            .reassembly_queue;
        assert!(!q.is_readable(), "should no longer be readable");
    }

    {
        let a = pair.client_conn_mut(client_ch);
        assert_eq!(0, a.buffered_amount(), "incorrect bufferedAmount");
    }

    close_association_pair(&mut pair, client_ch, server_ch, si);

    Ok(())
}

/// Same as `test_assoc_reliable_simple`, but through `write_chunk`, which
/// yields zero-copy refcounted slices of the caller's `Bytes`.
#[test]
fn test_assoc_reliable_write_chunk() -> Result<()> {
    let si: u16 = 1;
    let msg: Bytes = Bytes::from_static(b"WRITE_CHUNK");

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;

    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    {
        let now = pair.time;
        let mut s = pair.client_stream(client_ch, si)?;
        s.set_default_payload_type(PayloadProtocolIdentifier::Binary)?;
        let n = s.write_chunk(now, &msg)?;
        assert_eq!(msg.len(), n, "unexpected length of written data");
    }

    pair.drive();

    let chunks = pair.server_stream(server_ch, si)?.read_sctp()?.unwrap();
    assert_eq!(
        chunks.len(),
        msg.len(),
        "unexpected length of received data"
    );
    assert_eq!(
        chunks.ppi,
        PayloadProtocolIdentifier::Binary,
        "unexpected ppi"
    );
    let mut buf = vec![0u8; msg.len()];
    assert_eq!(chunks.read(&mut buf)?, msg.len());
    assert_eq!(&buf[..], &msg[..], "data should match");

    close_association_pair(&mut pair, client_ch, server_ch, si);

    Ok(())
}

#[test]
fn test_assoc_reliable_ordered_reordered() -> Result<()> {
    // let _guard = subscribe();

    let si: u16 = 2;
    let mut sbuf = vec![0u8; 1000];
    for i in 0..sbuf.len() {
        sbuf[i] = (i & 0xff) as u8;
    }

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;

    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    {
        let a = pair.client_conn_mut(client_ch);
        assert_eq!(0, a.buffered_amount(), "incorrect bufferedAmount");
    }

    sbuf[0..4].copy_from_slice(&0u32.to_be_bytes());
    let now = pair.time;
    let n = pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &Bytes::from(sbuf.clone()),
        PayloadProtocolIdentifier::Binary,
    )?;
    assert_eq!(sbuf.len(), n, "unexpected length of received data");
    pair.client.drive(pair.time, pair.server.addr);
    pair.client.delay_outbound(); // Delay it

    sbuf[0..4].copy_from_slice(&1u32.to_be_bytes());
    let now = pair.time;
    let n = pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &Bytes::from(sbuf.clone()),
        PayloadProtocolIdentifier::Binary,
    )?;
    assert_eq!(sbuf.len(), n, "unexpected length of received data");
    pair.client.drive(pair.time, pair.server.addr);
    pair.client.finish_delay(); // Reorder it

    pair.drive();

    let mut buf = vec![0u8; 2000];

    let chunks = pair.server_stream(server_ch, si)?.read_sctp()?.unwrap();
    let (n, ppi) = (chunks.len(), chunks.ppi);
    chunks.read(&mut buf)?;
    assert_eq!(n, sbuf.len(), "unexpected length of received data");
    assert_eq!(ppi, PayloadProtocolIdentifier::Binary, "unexpected ppi");
    assert_eq!(
        u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
        0,
        "unexpected received data"
    );

    let chunks = pair.server_stream(server_ch, si)?.read_sctp()?.unwrap();
    let (n, ppi) = (chunks.len(), chunks.ppi);
    chunks.read(&mut buf)?;
    assert_eq!(n, sbuf.len(), "unexpected length of received data");
    assert_eq!(ppi, PayloadProtocolIdentifier::Binary, "unexpected ppi");
    assert_eq!(
        u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
        1,
        "unexpected received data"
    );

    pair.drive();

    {
        let q = &pair
            .client_conn_mut(client_ch)
            .streams
            .get(&si)
            .unwrap()
            .reassembly_queue;
        assert!(!q.is_readable(), "should no longer be readable");
    }

    close_association_pair(&mut pair, client_ch, server_ch, si);

    Ok(())
}

#[test]
fn test_assoc_reliable_ordered_fragmented_then_defragmented() -> Result<()> {
    //let _guard = subscribe();

    let si: u16 = 3;
    let mut sbuf = vec![0u8; 1000];
    for i in 0..sbuf.len() {
        sbuf[i] = (i & 0xff) as u8;
    }
    let mut sbufl = vec![0u8; 2000];
    for i in 0..sbufl.len() {
        sbufl[i] = (i & 0xff) as u8;
    }

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;

    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    pair.client_stream(client_ch, si)?.set_reliability_params(
        false,
        ReliabilityType::Reliable,
        0,
    )?;
    pair.server_stream(server_ch, si)?.set_reliability_params(
        false,
        ReliabilityType::Reliable,
        0,
    )?;

    let now = pair.time;
    let n = pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &Bytes::from(sbufl.clone()),
        PayloadProtocolIdentifier::Binary,
    )?;
    assert_eq!(sbufl.len(), n, "unexpected length of received data");

    pair.drive();

    let mut rbuf = vec![0u8; 2000];
    let chunks = pair.server_stream(server_ch, si)?.read_sctp()?.unwrap();
    let (n, ppi) = (chunks.len(), chunks.ppi);
    chunks.read(&mut rbuf)?;
    assert_eq!(n, sbufl.len(), "unexpected length of received data");
    assert_eq!(&rbuf[..n], &sbufl, "unexpected received data");
    assert_eq!(ppi, PayloadProtocolIdentifier::Binary, "unexpected ppi");

    pair.drive();

    {
        let q = &pair
            .client_conn_mut(client_ch)
            .streams
            .get(&si)
            .unwrap()
            .reassembly_queue;
        assert!(!q.is_readable(), "should no longer be readable");
    }

    close_association_pair(&mut pair, client_ch, server_ch, si);

    Ok(())
}

#[test]
fn test_assoc_reliable_unordered_fragmented_then_defragmented() -> Result<()> {
    //let _guard = subscribe();

    let si: u16 = 4;
    let mut sbuf = vec![0u8; 1000];
    for i in 0..sbuf.len() {
        sbuf[i] = (i & 0xff) as u8;
    }
    let mut sbufl = vec![0u8; 2000];
    for i in 0..sbufl.len() {
        sbufl[i] = (i & 0xff) as u8;
    }

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;

    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    pair.client_stream(client_ch, si)?.set_reliability_params(
        true,
        ReliabilityType::Reliable,
        0,
    )?;
    pair.server_stream(server_ch, si)?.set_reliability_params(
        true,
        ReliabilityType::Reliable,
        0,
    )?;

    let now = pair.time;
    let n = pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &Bytes::from(sbufl.clone()),
        PayloadProtocolIdentifier::Binary,
    )?;
    assert_eq!(sbufl.len(), n, "unexpected length of received data");

    pair.drive();

    let mut rbuf = vec![0u8; 2000];
    let chunks = pair.server_stream(server_ch, si)?.read_sctp()?.unwrap();
    let (n, ppi) = (chunks.len(), chunks.ppi);
    chunks.read(&mut rbuf)?;
    assert_eq!(n, sbufl.len(), "unexpected length of received data");
    assert_eq!(&rbuf[..n], &sbufl, "unexpected received data");
    assert_eq!(ppi, PayloadProtocolIdentifier::Binary, "unexpected ppi");

    pair.drive();

    {
        let q = &pair
            .client_conn_mut(client_ch)
            .streams
            .get(&si)
            .unwrap()
            .reassembly_queue;
        assert!(!q.is_readable(), "should no longer be readable");
    }

    close_association_pair(&mut pair, client_ch, server_ch, si);

    Ok(())
}

#[test]
fn test_assoc_reliable_unordered_ordered() -> Result<()> {
    //let _guard = subscribe();

    let si: u16 = 5;
    let mut sbuf = vec![0u8; 1000];
    for i in 0..sbuf.len() {
        sbuf[i] = (i & 0xff) as u8;
    }

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;

    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    pair.client_stream(client_ch, si)?.set_reliability_params(
        true,
        ReliabilityType::Reliable,
        0,
    )?;
    pair.server_stream(server_ch, si)?.set_reliability_params(
        true,
        ReliabilityType::Reliable,
        0,
    )?;

    sbuf[0..4].copy_from_slice(&0u32.to_be_bytes());
    let now = pair.time;
    let n = pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &Bytes::from(sbuf.clone()),
        PayloadProtocolIdentifier::Binary,
    )?;
    assert_eq!(sbuf.len(), n, "unexpected length of received data");
    pair.client.drive(pair.time, pair.server.addr);
    pair.client.delay_outbound(); // Delay it

    sbuf[0..4].copy_from_slice(&1u32.to_be_bytes());
    let now = pair.time;
    let n = pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &Bytes::from(sbuf.clone()),
        PayloadProtocolIdentifier::Binary,
    )?;
    assert_eq!(sbuf.len(), n, "unexpected length of received data");
    pair.client.drive(pair.time, pair.server.addr);
    pair.client.finish_delay(); // Reorder it

    pair.drive();

    let mut buf = vec![0u8; 2000];

    let chunks = pair.server_stream(server_ch, si)?.read_sctp()?.unwrap();
    let (n, ppi) = (chunks.len(), chunks.ppi);
    chunks.read(&mut buf)?;
    assert_eq!(n, sbuf.len(), "unexpected length of received data");
    assert_eq!(ppi, PayloadProtocolIdentifier::Binary, "unexpected ppi");
    assert_eq!(
        u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
        1,
        "unexpected received data"
    );

    let chunks = pair.server_stream(server_ch, si)?.read_sctp()?.unwrap();
    let (n, ppi) = (chunks.len(), chunks.ppi);
    chunks.read(&mut buf)?;
    assert_eq!(n, sbuf.len(), "unexpected length of received data");
    assert_eq!(ppi, PayloadProtocolIdentifier::Binary, "unexpected ppi");
    assert_eq!(
        u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
        0,
        "unexpected received data"
    );

    pair.drive();

    {
        let q = &pair
            .client_conn_mut(client_ch)
            .streams
            .get(&si)
            .unwrap()
            .reassembly_queue;
        assert!(!q.is_readable(), "should no longer be readable");
    }

    close_association_pair(&mut pair, client_ch, server_ch, si);

    Ok(())
}

#[test]
fn test_assoc_reliable_retransmission() -> Result<()> {
    //let _guard = subscribe();

    let si: u16 = 6;
    let msg1: Bytes = Bytes::from_static(b"ABC");
    let msg2: Bytes = Bytes::from_static(b"DEFG");

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;

    {
        let a = pair.client_conn_mut(client_ch);
        a.rto_mgr.set_rto(100, true);
    }

    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    let now = pair.time;
    let n = pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &msg1,
        PayloadProtocolIdentifier::Binary,
    )?;
    assert_eq!(msg1.len(), n, "unexpected length of received data");
    pair.drive_client(); // send data to server
    pair.server.inbound.clear(); // Lose it
    debug!("dropping packet");

    let now = pair.time;
    let n = pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &msg2,
        PayloadProtocolIdentifier::Binary,
    )?;
    assert_eq!(msg2.len(), n, "unexpected length of received data");

    pair.drive();

    let mut buf = vec![0u8; 32];

    let chunks = pair.server_stream(server_ch, si)?.read_sctp()?.unwrap();
    let (n, ppi) = (chunks.len(), chunks.ppi);
    chunks.read(&mut buf)?;
    assert_eq!(n, msg1.len(), "unexpected length of received data");
    assert_eq!(&buf[..n], &msg1, "unexpected length of received data");
    assert_eq!(ppi, PayloadProtocolIdentifier::Binary, "unexpected ppi");

    let chunks = pair.server_stream(server_ch, si)?.read_sctp()?.unwrap();
    let (n, ppi) = (chunks.len(), chunks.ppi);
    chunks.read(&mut buf)?;
    assert_eq!(n, msg2.len(), "unexpected length of received data");
    assert_eq!(&buf[..n], &msg2, "unexpected length of received data");
    assert_eq!(ppi, PayloadProtocolIdentifier::Binary, "unexpected ppi");

    pair.drive();

    {
        let q = &pair
            .client_conn_mut(client_ch)
            .streams
            .get(&si)
            .unwrap()
            .reassembly_queue;
        assert!(!q.is_readable(), "should no longer be readable");
    }

    close_association_pair(&mut pair, client_ch, server_ch, si);

    Ok(())
}

#[test]
fn test_assoc_reliable_short_buffer() -> Result<()> {
    //let _guard = subscribe();

    let si: u16 = 1;
    let msg: Bytes = Bytes::from_static(b"Hello");

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;

    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    {
        let a = pair.client_conn_mut(client_ch);
        assert_eq!(0, a.buffered_amount(), "incorrect bufferedAmount");
    }

    let now = pair.time;
    let n = pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &msg,
        PayloadProtocolIdentifier::Binary,
    )?;
    assert_eq!(msg.len(), n, "unexpected length of received data");
    {
        let a = pair.client_conn_mut(client_ch);
        assert_eq!(msg.len(), a.buffered_amount(), "incorrect bufferedAmount");
    }

    pair.drive();

    let mut buf = vec![0u8; 3];
    let chunks = pair.server_stream(server_ch, si)?.read_sctp()?.unwrap();
    let result = chunks.read(&mut buf);
    assert!(result.is_err(), "expected error to be io.ErrShortBuffer");
    if let Err(err) = result {
        assert_eq!(
            Error::ErrShortBuffer,
            err,
            "expected error to be io.ErrShortBuffer"
        );
    }

    {
        let q = &pair
            .client_conn_mut(client_ch)
            .streams
            .get(&si)
            .unwrap()
            .reassembly_queue;
        assert!(!q.is_readable(), "should no longer be readable");
    }

    {
        let a = pair.client_conn_mut(client_ch);
        assert_eq!(0, a.buffered_amount(), "incorrect bufferedAmount");
    }

    close_association_pair(&mut pair, client_ch, server_ch, si);

    Ok(())
}

#[test]
fn test_assoc_unreliable_rexmit_ordered_no_fragment() -> Result<()> {
    //let _guard = subscribe();

    let si: u16 = 1;
    let mut sbuf = vec![0u8; 1000];
    for i in 0..sbuf.len() {
        sbuf[i] = (i & 0xff) as u8;
    }

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;

    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    // When we set the reliability value to 0 [times], then it will cause
    // the chunk to be abandoned immediately after the first transmission.
    pair.client_stream(client_ch, si)?
        .set_reliability_params(false, ReliabilityType::Rexmit, 0)?;
    pair.server_stream(server_ch, si)?
        .set_reliability_params(false, ReliabilityType::Rexmit, 0)?; // doesn't matter

    //br.drop_next_nwrites(0, 1).await; // drop the first packet (second one should be sacked)

    sbuf[0..4].copy_from_slice(&0u32.to_be_bytes());
    let now = pair.time;
    let n = pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &Bytes::from(sbuf.clone()),
        PayloadProtocolIdentifier::Binary,
    )?;
    assert_eq!(sbuf.len(), n, "unexpected length of received data");
    pair.drive_client(); // send data to server
    pair.server.inbound.clear(); // Lose it
    debug!("dropping packet");

    sbuf[0..4].copy_from_slice(&1u32.to_be_bytes());
    let now = pair.time;
    let n = pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &Bytes::from(sbuf.clone()),
        PayloadProtocolIdentifier::Binary,
    )?;
    assert_eq!(sbuf.len(), n, "unexpected length of received data");

    debug!("flush_buffers");
    pair.drive();

    let mut buf = vec![0u8; 2000];

    debug!("read_sctp");
    let chunks = pair.server_stream(server_ch, si)?.read_sctp()?.unwrap();
    let (n, ppi) = (chunks.len(), chunks.ppi);
    chunks.read(&mut buf)?;
    assert_eq!(n, sbuf.len(), "unexpected length of received data");
    assert_eq!(ppi, PayloadProtocolIdentifier::Binary, "unexpected ppi");
    assert_eq!(
        u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
        1,
        "unexpected received data"
    );

    debug!("process");
    pair.drive();

    {
        let q = &pair
            .client_conn_mut(client_ch)
            .streams
            .get(&si)
            .unwrap()
            .reassembly_queue;
        assert!(!q.is_readable(), "should no longer be readable");
    }

    close_association_pair(&mut pair, client_ch, server_ch, si);

    Ok(())
}

#[test]
fn test_forward_tsn_stream_map_populated_then_cleared() -> Result<()> {
    // Regression guard for the incremental forward-TSN stream map. Exercises
    // both halves of the new logic that the association-level unit tests can't
    // reach on their own:
    //   1. Population: an *ordered* message following a skipped (abandoned) one
    //      is only delivered if the FORWARD-TSN carried the skipped stream
    //      sequence number, i.e. the map was populated by the C2 loop.
    //   2. Clear: once the FORWARD-TSN is acknowledged and the window closes,
    //      the map must be emptied (the C1 / C3-else clear paths), otherwise a
    //      stale stream sequence would be re-reported on later FORWARD-TSNs.
    let si: u16 = 1;
    let sbuf = vec![0u8; 1000];

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;
    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    // Ordered stream; reliability 0 abandons a chunk after its first (here,
    // lost) transmission.
    pair.client_stream(client_ch, si)?
        .set_reliability_params(false, ReliabilityType::Rexmit, 0)?;
    pair.server_stream(server_ch, si)?
        .set_reliability_params(false, ReliabilityType::Rexmit, 0)?;

    // Send ordered ssn=0 and lose it in flight -> it becomes abandoned.
    let mut m0 = sbuf.clone();
    m0[0..4].copy_from_slice(&0u32.to_be_bytes());
    let now = pair.time;
    pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &Bytes::from(m0),
        PayloadProtocolIdentifier::Binary,
    )?;
    pair.drive_client();
    pair.server.inbound.clear(); // drop ssn=0

    // Send ordered ssn=1. The FORWARD-TSN generated for the abandoned ssn=0
    // must report the stream sequence so the receiver releases ssn=1.
    let mut m1 = sbuf.clone();
    m1[0..4].copy_from_slice(&1u32.to_be_bytes());
    let now = pair.time;
    pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &Bytes::from(m1),
        PayloadProtocolIdentifier::Binary,
    )?;
    pair.drive();

    // (1) Population check: the receiver skipped ssn=0 and delivered ssn=1.
    // For an ordered stream this delivery is impossible unless the FORWARD-TSN
    // carried the stream/SSN pair, so this proves the map was populated.
    let mut buf = vec![0u8; 2000];
    let chunks = pair.server_stream(server_ch, si)?.read_sctp()?.unwrap();
    let (n, ppi) = (chunks.len(), chunks.ppi);
    chunks.read(&mut buf)?;
    assert_eq!(n, sbuf.len(), "unexpected length of received data");
    assert_eq!(ppi, PayloadProtocolIdentifier::Binary, "unexpected ppi");
    assert_eq!(
        u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
        1,
        "receiver must skip abandoned ssn=0 and deliver ordered ssn=1"
    );

    // (2) Clear check: the FORWARD-TSN has been acknowledged and the window has
    // closed, so the sender's stream map must be empty.
    pair.drive();
    assert!(
        pair.client_conn_mut(client_ch)
            .fwd_tsn_stream_map_is_empty(),
        "forward-TSN stream map must be cleared once the window closes"
    );

    close_association_pair(&mut pair, client_ch, server_ch, si);

    Ok(())
}

#[test]
fn test_assoc_unreliable_rexmit_ordered_fragment() -> Result<()> {
    //let _guard = subscribe();

    let si: u16 = 1;
    let mut sbuf = vec![0u8; 2000];
    for i in 0..sbuf.len() {
        sbuf[i] = (i & 0xff) as u8;
    }

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;

    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    {
        // lock RTO value at 100 [msec]
        let a = pair.client_conn_mut(client_ch);
        a.rto_mgr.set_rto(100, true);
    }
    // When we set the reliability value to 0 [times], then it will cause
    // the chunk to be abandoned immediately after the first transmission.
    pair.client_stream(client_ch, si)?
        .set_reliability_params(false, ReliabilityType::Rexmit, 0)?;
    pair.server_stream(server_ch, si)?
        .set_reliability_params(false, ReliabilityType::Rexmit, 0)?; // doesn't matter

    //br.drop_next_nwrites(0, 1).await; // drop the first packet (second one should be sacked)

    sbuf[0..4].copy_from_slice(&0u32.to_be_bytes());
    let now = pair.time;
    let n = pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &Bytes::from(sbuf.clone()),
        PayloadProtocolIdentifier::Binary,
    )?;
    assert_eq!(sbuf.len(), n, "unexpected length of received data");
    pair.drive_client(); // send data to server
    pair.server.inbound.clear(); // Lose it

    sbuf[0..4].copy_from_slice(&1u32.to_be_bytes());
    let now = pair.time;
    let n = pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &Bytes::from(sbuf.clone()),
        PayloadProtocolIdentifier::Binary,
    )?;
    assert_eq!(sbuf.len(), n, "unexpected length of received data");

    //log::debug!("flush_buffers");
    pair.drive();

    let mut buf = vec![0u8; 2000];

    //log::debug!("read_sctp");
    let chunks = pair.server_stream(server_ch, si)?.read_sctp()?.unwrap();
    let (n, ppi) = (chunks.len(), chunks.ppi);
    chunks.read(&mut buf)?;
    assert_eq!(n, sbuf.len(), "unexpected length of received data");
    assert_eq!(ppi, PayloadProtocolIdentifier::Binary, "unexpected ppi");
    assert_eq!(
        u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
        1,
        "unexpected received data"
    );

    //log::debug!("process");
    pair.drive();

    {
        let q = &pair
            .client_conn_mut(client_ch)
            .streams
            .get(&si)
            .unwrap()
            .reassembly_queue;
        assert!(!q.is_readable(), "should no longer be readable");
    }

    close_association_pair(&mut pair, client_ch, server_ch, si);

    Ok(())
}

#[test]
fn test_assoc_unreliable_rexmit_unordered_no_fragment() -> Result<()> {
    //let _guard = subscribe();

    let si: u16 = 2;
    let mut sbuf = vec![0u8; 1000];
    for i in 0..sbuf.len() {
        sbuf[i] = (i & 0xff) as u8;
    }

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;

    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    // When we set the reliability value to 0 [times], then it will cause
    // the chunk to be abandoned immediately after the first transmission.
    pair.client_stream(client_ch, si)?
        .set_reliability_params(true, ReliabilityType::Rexmit, 0)?;
    pair.server_stream(server_ch, si)?
        .set_reliability_params(true, ReliabilityType::Rexmit, 0)?; // doesn't matter

    //br.drop_next_nwrites(0, 1).await; // drop the first packet (second one should be sacked)

    sbuf[0..4].copy_from_slice(&0u32.to_be_bytes());
    let now = pair.time;
    let n = pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &Bytes::from(sbuf.clone()),
        PayloadProtocolIdentifier::Binary,
    )?;
    assert_eq!(sbuf.len(), n, "unexpected length of received data");
    pair.drive_client(); // send data to server
    pair.server.inbound.clear(); // Lose it

    sbuf[0..4].copy_from_slice(&1u32.to_be_bytes());
    let now = pair.time;
    let n = pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &Bytes::from(sbuf.clone()),
        PayloadProtocolIdentifier::Binary,
    )?;
    assert_eq!(sbuf.len(), n, "unexpected length of received data");

    //log::debug!("flush_buffers");
    pair.drive();

    let mut buf = vec![0u8; 2000];

    //log::debug!("read_sctp");
    let chunks = pair.server_stream(server_ch, si)?.read_sctp()?.unwrap();
    let (n, ppi) = (chunks.len(), chunks.ppi);
    chunks.read(&mut buf)?;
    assert_eq!(n, sbuf.len(), "unexpected length of received data");
    assert_eq!(ppi, PayloadProtocolIdentifier::Binary, "unexpected ppi");
    assert_eq!(
        u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
        1,
        "unexpected received data"
    );

    //log::debug!("process");
    pair.drive();

    {
        let q = &pair
            .client_conn_mut(client_ch)
            .streams
            .get(&si)
            .unwrap()
            .reassembly_queue;
        assert!(!q.is_readable(), "should no longer be readable");
    }

    close_association_pair(&mut pair, client_ch, server_ch, si);

    Ok(())
}

#[test]
fn test_assoc_unreliable_rexmit_unordered_fragment() -> Result<()> {
    //let _guard = subscribe();

    let si: u16 = 1;
    let mut sbuf = vec![0u8; 2000];
    for i in 0..sbuf.len() {
        sbuf[i] = (i & 0xff) as u8;
    }

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;

    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    // When we set the reliability value to 0 [times], then it will cause
    // the chunk to be abandoned immediately after the first transmission.
    pair.client_stream(client_ch, si)?
        .set_reliability_params(true, ReliabilityType::Rexmit, 0)?;
    pair.server_stream(server_ch, si)?
        .set_reliability_params(true, ReliabilityType::Rexmit, 0)?; // doesn't matter

    sbuf[0..4].copy_from_slice(&0u32.to_be_bytes());
    let now = pair.time;
    let n = pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &Bytes::from(sbuf.clone()),
        PayloadProtocolIdentifier::Binary,
    )?;
    assert_eq!(sbuf.len(), n, "unexpected length of received data");
    pair.client.drive(pair.time, pair.server.addr);
    pair.client.outbound.clear();
    //debug!("outbound len={}", pair.client.outbound.len());

    sbuf[0..4].copy_from_slice(&1u32.to_be_bytes());
    let now = pair.time;
    let n = pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &Bytes::from(sbuf.clone()),
        PayloadProtocolIdentifier::Binary,
    )?;
    assert_eq!(sbuf.len(), n, "unexpected length of received data");

    pair.drive();

    let mut buf = vec![0u8; 2000];

    //log::debug!("read_sctp");
    let chunks = pair.server_stream(server_ch, si)?.read_sctp()?.unwrap();
    let (n, ppi) = (chunks.len(), chunks.ppi);
    chunks.read(&mut buf)?;
    assert_eq!(n, sbuf.len(), "unexpected length of received data");
    assert_eq!(ppi, PayloadProtocolIdentifier::Binary, "unexpected ppi");
    assert_eq!(
        u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
        1,
        "unexpected received data"
    );

    //log::debug!("process");
    pair.drive();

    {
        let q = &pair
            .client_conn_mut(client_ch)
            .streams
            .get(&si)
            .unwrap()
            .reassembly_queue;
        assert!(!q.is_readable(), "should no longer be readable");
        assert_eq!(
            0,
            q.unordered.len(),
            "should be nothing in the unordered queue"
        );
        assert_eq!(
            0,
            q.unordered_chunks.len(),
            "should be nothing in the unorderedChunks list"
        );
    }

    close_association_pair(&mut pair, client_ch, server_ch, si);

    Ok(())
}

/// `ReliabilityType::Timed` is the policy `maxPacketLifeTime` maps onto, and it is
/// supposed to bound how old a message can be when it reaches the peer.
///
/// It did not: the lifetime was measured from `c.since`, which is reassigned when
/// the chunk leaves the pending queue and is given a TSN. Time the message spent
/// waiting for a congestion window was therefore invisible to the policy, so the
/// message went out however stale it was, with a freshly reset lifetime.
#[test]
fn test_assoc_timed_reliability_bounds_time_spent_in_pending_queue() -> Result<()> {
    let si: u16 = 7;
    // maxPacketLifeTime = 100 ms.
    let lifetime_ms: u32 = 100;
    let stall = Duration::from_secs(1);

    let mut sbuf = vec![0u8; 1000];
    for i in 0..sbuf.len() {
        sbuf[i] = (i & 0xff) as u8;
    }

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;
    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    pair.client_stream(client_ch, si)?.set_reliability_params(
        true,
        ReliabilityType::Timed,
        lifetime_ms,
    )?;
    pair.server_stream(server_ch, si)?.set_reliability_params(
        true,
        ReliabilityType::Timed,
        lifetime_ms,
    )?;

    // Shrink the congestion window so exactly one message fits in flight. The
    // second write then has to wait in the pending queue behind the first.
    pair.client_conn_mut(client_ch).cwnd = 1200;

    // Message 0 leaves immediately and occupies the whole window.
    sbuf[0..4].copy_from_slice(&0u32.to_be_bytes());
    let now = pair.time;
    pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &Bytes::from(sbuf.clone()),
        PayloadProtocolIdentifier::Binary,
    )?;

    // Message 1 is blocked behind it.
    sbuf[0..4].copy_from_slice(&1u32.to_be_bytes());
    let now = pair.time;
    pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &Bytes::from(sbuf.clone()),
        PayloadProtocolIdentifier::Binary,
    )?;

    // Drive only the sender: message 0 goes on the wire, message 1 stays pending.
    // The peer is deliberately not driven, so nothing is acknowledged and the
    // window stays shut for as long as we like.
    pair.drive_client();
    assert!(
        pair.client_conn_mut(client_ch).buffered_amount() >= sbuf.len(),
        "message 1 should still be waiting in the pending queue"
    );

    // The stall lasts ten times the configured lifetime.
    pair.time += stall;

    // The stall clears and both sides run to completion.
    pair.drive();

    let mut buf = vec![0u8; 2000];
    let mut received = vec![];
    while let Some(chunks) = pair.server_stream(server_ch, si)?.read_sctp()? {
        chunks.read(&mut buf)?;
        received.push(u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]));
    }

    // Message 1 outlived its lifetime while queued and must be abandoned;
    // message 0 was already in flight when the stall began and must survive.
    // Asserting the whole vector also catches a fix that drops everything.
    assert_eq!(
        received,
        vec![0],
        "message 1 waited {stall:?} in the pending queue with maxPacketLifeTime={lifetime_ms}ms \
         and must have been abandoned, while message 0 must still arrive"
    );
    assert_eq!(
        0,
        pair.client_conn_mut(client_ch).buffered_amount(),
        "abandoning the expired message must release its buffered bytes"
    );

    close_association_pair(&mut pair, client_ch, server_ch, si);

    Ok(())
}

/// The expired-message path pops a whole fragment run, which is the part that
/// interacts with `PendingQueue`'s selected-queue state. A single-chunk message
/// never exercises it, so this drives a message large enough to fragment and
/// then checks that the queue is still usable afterwards.
#[test]
fn test_assoc_timed_reliability_abandons_a_whole_fragmented_message() -> Result<()> {
    let si: u16 = 8;
    let lifetime_ms: u32 = 100;
    let stall = Duration::from_secs(1);

    let small = Bytes::from(vec![0xAAu8; 1000]);
    // Comfortably over max_payload_size_for_mtu(INITIAL_MTU), so this fragments.
    let large = Bytes::from(vec![0xBBu8; 4000]);
    let tail = Bytes::from(vec![0xCCu8; 700]);

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;
    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    pair.client_stream(client_ch, si)?.set_reliability_params(
        true,
        ReliabilityType::Timed,
        lifetime_ms,
    )?;
    pair.server_stream(server_ch, si)?.set_reliability_params(
        true,
        ReliabilityType::Timed,
        lifetime_ms,
    )?;

    pair.client_conn_mut(client_ch).cwnd = 1200;

    let now = pair.time;
    pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &small,
        PayloadProtocolIdentifier::Binary,
    )?;

    let now = pair.time;
    pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &large,
        PayloadProtocolIdentifier::Binary,
    )?;

    pair.drive_client();
    assert!(
        pair.client_conn_mut(client_ch).buffered_amount() >= large.len(),
        "the fragmented message should still be waiting in the pending queue"
    );

    pair.time += stall;
    pair.drive();

    // A message that is still usable after the stall must get through, which is
    // only possible if abandoning the fragment run left the queue consistent.
    let now = pair.time;
    pair.client_stream(client_ch, si)?
        .write_sctp(now, &tail, PayloadProtocolIdentifier::Binary)?;
    pair.drive();

    let mut buf = vec![0u8; 8000];
    let mut lengths = vec![];
    while let Some(chunks) = pair.server_stream(server_ch, si)?.read_sctp()? {
        let n = chunks.len();
        chunks.read(&mut buf)?;
        lengths.push(n);
    }

    assert_eq!(
        lengths,
        vec![small.len(), tail.len()],
        "the fragmented message must be abandoned whole, and the queue must keep working"
    );
    assert_eq!(
        0,
        pair.client_conn_mut(client_ch).buffered_amount(),
        "abandoning a fragmented message must release every fragment's bytes"
    );

    close_association_pair(&mut pair, client_ch, server_ch, si);

    Ok(())
}

/// Under timed reliability the sender must stop transmitting a message once its
/// lifetime has expired. It does not: `abandoned` is only ever recomputed inside
/// `check_partial_reliability_status`, which never runs between the first
/// transmission and the T3 timeout, so the expired chunk still looks live when
/// `mark_all_to_retrasmit` inspects it.
#[test]
fn test_assoc_timed_reliability_does_not_retransmit_after_lifetime() -> Result<()> {
    let si: u16 = 9;
    let lifetime_ms: u32 = 100;
    let msg = Bytes::from_static(b"expires long before T3 can fire");

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;
    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    pair.client_stream(client_ch, si)?.set_reliability_params(
        true,
        ReliabilityType::Timed,
        lifetime_ms,
    )?;
    pair.server_stream(server_ch, si)?.set_reliability_params(
        true,
        ReliabilityType::Timed,
        lifetime_ms,
    )?;

    // Ignore whatever the handshake delivered.
    pair.server_conn_mut(server_ch).stats.reset();

    let now = pair.time;
    pair.client_stream(client_ch, si)?
        .write_sctp(now, &msg, PayloadProtocolIdentifier::Binary)?;

    // Transmit once, then lose it on the wire: the peer never sees it, and never
    // acknowledges it, so the retransmission timer is left to run.
    pair.drive_client();
    pair.server.inbound.clear();
    assert_eq!(
        0,
        pair.server_conn_mut(server_ch).stats.get_num_datas(),
        "the first transmission must have been lost for this test to mean anything"
    );

    // Well past the lifetime, still short of the one-second T3.
    pair.time += Duration::from_millis(500);
    pair.drive_client();

    // Let T3 fire, then deliver whatever the sender produced.
    pair.time += Duration::from_millis(1500);
    pair.drive_client();
    pair.drive_server();

    let redelivered = pair.server_conn_mut(server_ch).stats.get_num_datas();
    assert_eq!(
        0, redelivered,
        "a message whose maxPacketLifeTime is {lifetime_ms}ms expired at t+{lifetime_ms}ms and \
         must not be transmitted again, but the peer received it {redelivered} more time(s) \
         at t+2000ms"
    );

    Ok(())
}

/// The same defect on the fast-retransmit leg: three duplicate SACKs put the
/// sender into fast recovery, and the expired chunk is retransmitted because its
/// `abandoned` flag has not been recomputed since it was first sent.
#[test]
fn test_assoc_timed_reliability_does_not_fast_retransmit_after_lifetime() -> Result<()> {
    let si: u16 = 10;
    let lifetime_ms: u32 = 10;

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;
    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    pair.client_stream(client_ch, si)?.set_reliability_params(
        true,
        ReliabilityType::Timed,
        lifetime_ms,
    )?;
    pair.server_stream(server_ch, si)?.set_reliability_params(
        true,
        ReliabilityType::Timed,
        lifetime_ms,
    )?;

    let body = |tag: u32| {
        let mut v = vec![0u8; 200];
        v[0..4].copy_from_slice(&tag.to_be_bytes());
        Bytes::from(v)
    };

    // Message 0 goes out and is lost.
    let now = pair.time;
    pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &body(0),
        PayloadProtocolIdentifier::Binary,
    )?;
    pair.drive_client();
    pair.server.inbound.clear();

    // Its 10 ms lifetime is long gone.
    pair.time += Duration::from_millis(50);

    // Three more messages, each delivered and SACKed separately, so the peer
    // reports the gap three times and the sender enters fast recovery.
    for tag in 1..=3u32 {
        let now = pair.time;
        pair.client_stream(client_ch, si)?.write_sctp(
            now,
            &body(tag),
            PayloadProtocolIdentifier::Binary,
        )?;
        pair.drive_client();
        pair.drive_server();
        pair.drive_client();
        pair.time += Duration::from_millis(1);
    }

    pair.drive_server();

    let mut buf = vec![0u8; 1000];
    let mut got = vec![];
    while let Some(chunks) = pair.server_stream(server_ch, si)?.read_sctp()? {
        chunks.read(&mut buf)?;
        got.push(u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]));
    }

    assert_eq!(
        vec![1, 2, 3],
        got,
        "message 0 expired at t+{lifetime_ms}ms and must not be fast-retransmitted, \
         while the three fresh messages must still arrive"
    );
    assert_eq!(
        0,
        pair.client_conn_mut(client_ch).stats.get_num_fast_retrans(),
        "no fast retransmission should have been issued for an expired message"
    );

    Ok(())
}

/// Refusing to retransmit is only half of the contract: the association must also
/// make progress. On an ordered stream the peer cannot deliver anything behind the
/// expired message until a ForwardTSN tells it to skip, so the sender has to notice
/// the expiry on the same T3 that decided not to retransmit — not on the next,
/// backed-off one.
#[test]
fn test_assoc_timed_reliability_forwards_past_expired_without_extra_backoff() -> Result<()> {
    let si: u16 = 11;
    let lifetime_ms: u32 = 100;
    let stale = Bytes::from_static(b"AAAA");
    let fresh = Bytes::from_static(b"BBBBBBBB");

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;
    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    // Ordered, so the peer is head-of-line blocked behind the expired message.
    pair.client_stream(client_ch, si)?.set_reliability_params(
        false,
        ReliabilityType::Timed,
        lifetime_ms,
    )?;
    pair.server_stream(server_ch, si)?.set_reliability_params(
        false,
        ReliabilityType::Timed,
        lifetime_ms,
    )?;

    let t0 = pair.time;
    let now = pair.time;
    pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &stale,
        PayloadProtocolIdentifier::Binary,
    )?;
    pair.drive_client();
    pair.server.inbound.clear(); // lost on the wire, and never acknowledged

    // Long past its lifetime, queue a message that is still worth delivering.
    pair.time += Duration::from_millis(200);
    let now = pair.time;
    pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &fresh,
        PayloadProtocolIdentifier::Binary,
    )?;

    let mut buf = vec![0u8; 64];
    let mut delivered = None;
    for _ in 0..400 {
        pair.time += Duration::from_millis(25);
        pair.drive_client();
        pair.drive_server();
        if let Some(chunks) = pair.server_stream(server_ch, si)?.read_sctp()? {
            let n = chunks.len();
            chunks.read(&mut buf)?;
            delivered = Some((n, pair.time.duration_since(t0)));
            break;
        }
    }

    let (len, at) = delivered.expect("the fresh message must eventually be delivered");
    assert_eq!(
        fresh.len(),
        len,
        "the expired message must not be delivered; only the fresh one may be"
    );
    assert_eq!(
        1,
        pair.client_conn_mut(client_ch).stats.get_num_t3timeouts(),
        "the ForwardTSN that unblocks the stream must be sent by the T3 that skipped \
         the expired message, not by the next one; the fresh message arrived at {at:?}"
    );

    Ok(())
}

#[test]
fn test_assoc_timed_reliability_stops_retransmission() -> Result<()> {
    for unordered in [false, true] {
        for size in [32, 2000, 12000] {
            let si = 9;
            let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;
            establish_session_pair(&mut pair, client_ch, server_ch, si)?;
            pair.client_stream(client_ch, si)?.set_reliability_params(
                unordered,
                ReliabilityType::Timed,
                100,
            )?;
            pair.server_stream(server_ch, si)?.set_reliability_params(
                unordered,
                ReliabilityType::Timed,
                100,
            )?;
            let now = pair.time;
            pair.client_stream(client_ch, si)?.write_sctp(
                now,
                &Bytes::from(vec![0x55; size]),
                PayloadProtocolIdentifier::Binary,
            )?;
            pair.drive_client();
            assert!(!pair.server.inbound.is_empty());
            pair.server.inbound.clear(); // Lose the first transmission.
            pair.server_conn_mut(server_ch).stats.reset();

            pair.time += Duration::from_millis(500);
            pair.drive_client();
            pair.time += Duration::from_millis(1500);
            pair.drive_client();
            let mut forwards = 0;
            for (_, _, raw) in &pair.server.inbound {
                for c in Packet::unmarshal(raw)?.chunks {
                    assert!(
                        !c.as_any().is::<ChunkPayloadData>(),
                        "expired DATA was retransmitted: unordered={unordered}, size={size}"
                    );
                    forwards += usize::from(c.as_any().is::<ChunkForwardTsn>());
                }
            }
            assert!(
                forwards > 0,
                "the receiver must be able to skip the message"
            );
            pair.drive_server();
            pair.drive_client();
            assert_eq!(0, pair.server_conn_mut(server_ch).stats.get_num_datas());
            assert_eq!(0, pair.client_stream(client_ch, si)?.buffered_amount()?);
            assert!(pair.client_conn_mut(client_ch).is_idle());

            let now = pair.time;
            let next = Bytes::from_static(b"still useful");
            pair.client_stream(client_ch, si)?.write_sctp(
                now,
                &next,
                PayloadProtocolIdentifier::Binary,
            )?;
            pair.drive();
            let received = pair.server_stream(server_ch, si)?.read_sctp()?.unwrap();
            let mut buf = vec![0; received.len()];
            received.read(&mut buf)?;
            assert_eq!(next.as_ref(), buf);
        }
    }
    Ok(())
}

#[test]
fn test_assoc_timed_reliability_does_not_stall_shutdown() -> Result<()> {
    let si: u16 = 21;
    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;
    establish_session_pair(&mut pair, client_ch, server_ch, si)?;
    pair.client_stream(client_ch, si)?.set_reliability_params(
        false,
        ReliabilityType::Timed,
        100,
    )?;
    pair.server_stream(server_ch, si)?.set_reliability_params(
        false,
        ReliabilityType::Timed,
        100,
    )?;

    let now = pair.time;
    pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &Bytes::from_static(b"last message, lost on the wire"),
        PayloadProtocolIdentifier::Binary,
    )?;
    pair.drive_client();
    pair.server.inbound.clear(); // lost, and it expires while outstanding

    pair.time += Duration::from_millis(300);
    pair.client_conn_mut(client_ch).shutdown()?;

    for _ in 0..120 {
        pair.drive_client();
        pair.drive_server();
        pair.drive_client();
        pair.time += Duration::from_millis(50);
    }

    let state = pair.client_conn_mut(client_ch).state();
    assert_ne!(
        AssociationState::ShutdownPending,
        state,
        "the shutdown sequence stalled: a message abandoned while winding down is neither \
         retransmitted nor skipped with a ForwardTSN, so the in-flight queue never empties"
    );

    Ok(())
}

#[test]
fn test_assoc_unreliable_rexmit_timed_ordered() -> Result<()> {
    //let _guard = subscribe();

    let si: u16 = 3;
    let mut sbuf = vec![0u8; 1000];
    for i in 0..sbuf.len() {
        sbuf[i] = (i & 0xff) as u8;
    }

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;

    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    // When we set the reliability value to 0 [times], then it will cause
    // the chunk to be abandoned immediately after the first transmission.
    pair.client_stream(client_ch, si)?
        .set_reliability_params(false, ReliabilityType::Timed, 0)?;
    pair.server_stream(server_ch, si)?
        .set_reliability_params(false, ReliabilityType::Timed, 0)?; // doesn't matter

    //br.drop_next_nwrites(0, 1).await; // drop the first packet (second one should be sacked)

    sbuf[0..4].copy_from_slice(&0u32.to_be_bytes());
    let now = pair.time;
    let n = pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &Bytes::from(sbuf.clone()),
        PayloadProtocolIdentifier::Binary,
    )?;
    assert_eq!(sbuf.len(), n, "unexpected length of received data");
    pair.client.drive(pair.time, pair.server.addr);
    pair.client.outbound.clear();

    sbuf[0..4].copy_from_slice(&1u32.to_be_bytes());
    let now = pair.time;
    let n = pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &Bytes::from(sbuf.clone()),
        PayloadProtocolIdentifier::Binary,
    )?;
    assert_eq!(sbuf.len(), n, "unexpected length of received data");

    //log::debug!("flush_buffers");
    pair.drive();

    let mut buf = vec![0u8; 2000];

    //log::debug!("read_sctp");
    let chunks = pair.server_stream(server_ch, si)?.read_sctp()?.unwrap();
    let (n, ppi) = (chunks.len(), chunks.ppi);
    chunks.read(&mut buf)?;
    assert_eq!(n, sbuf.len(), "unexpected length of received data");
    assert_eq!(ppi, PayloadProtocolIdentifier::Binary, "unexpected ppi");
    assert_eq!(
        u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
        1,
        "unexpected received data"
    );

    //log::debug!("process");
    pair.drive();

    {
        let q = &pair
            .client_conn_mut(client_ch)
            .streams
            .get(&si)
            .unwrap()
            .reassembly_queue;
        assert!(!q.is_readable(), "should no longer be readable");
    }

    close_association_pair(&mut pair, client_ch, server_ch, si);

    Ok(())
}

#[test]
fn test_assoc_unreliable_rexmit_timed_unordered() -> Result<()> {
    //let _guard = subscribe();

    let si: u16 = 3;
    let mut sbuf = vec![0u8; 1000];
    for i in 0..sbuf.len() {
        sbuf[i] = (i & 0xff) as u8;
    }

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;

    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    // When we set the reliability value to 0 [times], then it will cause
    // the chunk to be abandoned immediately after the first transmission.
    pair.client_stream(client_ch, si)?
        .set_reliability_params(true, ReliabilityType::Timed, 0)?;
    pair.server_stream(server_ch, si)?
        .set_reliability_params(true, ReliabilityType::Timed, 0)?; // doesn't matter

    //br.drop_next_nwrites(0, 1).await; // drop the first packet (second one should be sacked)

    sbuf[0..4].copy_from_slice(&0u32.to_be_bytes());
    let now = pair.time;
    let n = pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &Bytes::from(sbuf.clone()),
        PayloadProtocolIdentifier::Binary,
    )?;
    assert_eq!(sbuf.len(), n, "unexpected length of received data");
    pair.client.drive(pair.time, pair.server.addr);
    pair.client.outbound.clear();

    sbuf[0..4].copy_from_slice(&1u32.to_be_bytes());
    let now = pair.time;
    let n = pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &Bytes::from(sbuf.clone()),
        PayloadProtocolIdentifier::Binary,
    )?;
    assert_eq!(sbuf.len(), n, "unexpected length of received data");

    //log::debug!("flush_buffers");
    pair.drive();

    let mut buf = vec![0u8; 2000];

    //log::debug!("read_sctp");
    let chunks = pair.server_stream(server_ch, si)?.read_sctp()?.unwrap();
    let (n, ppi) = (chunks.len(), chunks.ppi);
    chunks.read(&mut buf)?;
    assert_eq!(n, sbuf.len(), "unexpected length of received data");
    assert_eq!(ppi, PayloadProtocolIdentifier::Binary, "unexpected ppi");
    assert_eq!(
        u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
        1,
        "unexpected received data"
    );

    //log::debug!("process");
    pair.drive();

    {
        let q = &pair
            .client_conn_mut(client_ch)
            .streams
            .get(&si)
            .unwrap()
            .reassembly_queue;
        assert!(!q.is_readable(), "should no longer be readable");
        assert_eq!(
            0,
            q.unordered.len(),
            "should be nothing in the unordered queue"
        );
        assert_eq!(
            0,
            q.unordered_chunks.len(),
            "should be nothing in the unorderedChunks list"
        );
    }

    close_association_pair(&mut pair, client_ch, server_ch, si);

    Ok(())
}

//TODO: TestAssocT1InitTimer
//TODO: TestAssocT1CookieTimer
//TODO: TestAssocT3RtxTimer

/*FIXME
// 1) Send 4 packets. drop the first one.
// 2) Last 3 packet will be received, which triggers fast-retransmission
// 3) The first one is retransmitted, which makes s1 readable
// Above should be done before RTO occurs (fast recovery)
#[test]
fn test_assoc_congestion_control_fast_retransmission() -> Result<()> {
    let _guard = subscribe();

    let si: u16 = 6;
    let mut sbuf = vec![0u8; 1000];
    for i in 0..sbuf.len() {
        sbuf[i] = (i & 0xff) as u8;
    }

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::Normal, 0)?;

    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    //br.drop_next_nwrites(0, 1).await; // drop the first packet (second one should be sacked)

    for i in 0..4u32 {
        sbuf[0..4].copy_from_slice(&i.to_be_bytes());
        let now = pair.time;
        let n = pair.client_stream(client_ch, si)?.write_sctp(
            now,
            &Bytes::from(sbuf.clone()),
            PayloadProtocolIdentifier::Binary,
        )?;
        assert_eq!(sbuf.len(), n, "unexpected length of received data");
        pair.client.drive(pair.time, pair.server.addr);
        if i == 0 {
            //drop the first packet
            pair.client.outbound.clear();
        }
    }

    // process packets for 500 msec, assuming that the fast retrans/recover
    // should complete within 500 msec.
    /*for _ in 0..50 {
        br.tick().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }*/
    debug!("advance 500ms");
    pair.time += Duration::from_millis(500);
    pair.step();

    let mut buf = vec![0u8; 3000];

    // Try to read all 4 packets
    for i in 0..4 {
        {
            let q = &pair
                .server_conn_mut(server_ch)
                .streams
                .get(&si)
                .unwrap()
                .reassembly_queue;
            assert!(q.is_readable(), "should be readable at {}", i);
        }

        let chunks = pair.server_stream(server_ch, si)?.read_sctp()?.unwrap();
        let (n, ppi) = (chunks.len(), chunks.ppi);
        chunks.read(&mut buf)?;
        assert_eq!(n, sbuf.len(), "unexpected length of received data");
        assert_eq!(ppi, PayloadProtocolIdentifier::Binary, "unexpected ppi");
        assert_eq!(
            u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
            i,
            "unexpected received data"
        );
    }

    pair.drive();
    //br.process().await;

    {
        let a = pair.client_conn_mut(client_ch);
        assert!(!a.in_fast_recovery, "should not be in fast-recovery");
        debug!("nSACKs      : {}", a.stats.get_num_sacks());
        debug!("nFastRetrans: {}", a.stats.get_num_fast_retrans());

        assert_eq!(1, a.stats.get_num_fast_retrans(), "should be 1");
    }
    {
        let b = pair.server_conn_mut(server_ch);
        debug!("nDATAs      : {}", b.stats.get_num_datas());
        debug!("nAckTimeouts: {}", b.stats.get_num_ack_timeouts());
    }

    close_association_pair(&mut pair, client_ch, server_ch, si);

    Ok(())
}*/

#[test]
fn test_assoc_congestion_control_congestion_avoidance() -> Result<()> {
    //let _guard = subscribe();

    let max_receive_buffer_size: u32 = 64 * 1024;
    let si: u16 = 6;
    let n_packets_to_send: u32 = 2000;

    let mut sbuf = vec![0u8; 1000];
    for i in 0..sbuf.len() {
        sbuf[i] = (i & 0xff) as u8;
    }

    let (mut pair, client_ch, server_ch) =
        create_association_pair(AckMode::Normal, max_receive_buffer_size)?;

    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    {
        pair.client_conn_mut(client_ch).stats.reset();
        pair.server_conn_mut(server_ch).stats.reset();
    }

    for i in 0..n_packets_to_send {
        sbuf[0..4].copy_from_slice(&i.to_be_bytes());
        let now = pair.time;
        let n = pair.client_stream(client_ch, si)?.write_sctp(
            now,
            &Bytes::from(sbuf.clone()),
            PayloadProtocolIdentifier::Binary,
        )?;
        assert_eq!(sbuf.len(), n, "unexpected length of received data");
    }
    pair.drive_client();
    //debug!("pair.drive_client() done");

    let mut rbuf = vec![0u8; 3000];

    // Repeat calling br.Tick() until the buffered amount becomes 0
    let mut n_packets_received = 0u32;
    while pair.client_conn_mut(client_ch).buffered_amount() > 0
        && n_packets_received < n_packets_to_send
    {
        /*println!("timestamp: {:?}", pair.time);
        println!(
            "buffered_amount {}, pair.server.inbound {}, n_packets_received {}, n_packets_to_send {}",
            pair.client_conn_mut(client_ch).buffered_amount(),
            pair.server.inbound.len(),
            n_packets_received,
            n_packets_to_send
        );*/

        pair.step();

        while let Some(chunks) = pair.server_stream(server_ch, si)?.read_sctp()? {
            let (n, ppi) = (chunks.len(), chunks.ppi);
            chunks.read(&mut rbuf)?;
            assert_eq!(sbuf.len(), n, "unexpected length of received data");
            assert_eq!(
                n_packets_received,
                u32::from_be_bytes([rbuf[0], rbuf[1], rbuf[2], rbuf[3]]),
                "unexpected length of received data"
            );
            assert_eq!(ppi, PayloadProtocolIdentifier::Binary, "unexpected ppi");

            n_packets_received += 1;
        }

        //pair.drive_client();
    }

    pair.drive();
    //println!("timestamp: {:?}", pair.time);

    assert_eq!(
        n_packets_received, n_packets_to_send,
        "unexpected num of packets received"
    );

    {
        let a = pair.client_conn_mut(client_ch);

        assert!(!a.in_fast_recovery, "should not be in fast-recovery");
        assert!(
            a.cwnd > a.ssthresh,
            "should be in congestion avoidance mode"
        );
        assert!(
            a.ssthresh >= max_receive_buffer_size,
            "{} should not be less than the initial size of 128KB {}",
            a.ssthresh,
            max_receive_buffer_size
        );

        debug!("nSACKs      : {}", a.stats.get_num_sacks());
        debug!("nT3Timeouts: {}", a.stats.get_num_t3timeouts());

        assert!(
            a.stats.get_num_sacks() <= n_packets_to_send as u64 / 2,
            "too many sacks"
        );
        assert_eq!(0, a.stats.get_num_t3timeouts(), "should be no retransmit");
    }
    {
        assert_eq!(
            0,
            pair.server_conn_mut(server_ch)
                .streams
                .get(&si)
                .unwrap()
                .get_num_bytes_in_reassembly_queue(),
            "reassembly queue should be empty"
        );

        let b = pair.server_conn_mut(server_ch);

        debug!("nDATAs      : {}", b.stats.get_num_datas());

        assert_eq!(
            n_packets_to_send as u64,
            b.stats.get_num_datas(),
            "packet count mismatch"
        );
    }

    close_association_pair(&mut pair, client_ch, server_ch, si);

    Ok(())
}

#[test]
fn test_assoc_congestion_control_slow_reader() -> Result<()> {
    //let _guard = subscribe();

    let max_receive_buffer_size: u32 = 64 * 1024;
    let si: u16 = 6;
    let n_packets_to_send: u32 = 130;

    let mut sbuf = vec![0u8; 1000];
    for i in 0..sbuf.len() {
        sbuf[i] = (i & 0xff) as u8;
    }

    let (mut pair, client_ch, server_ch) =
        create_association_pair(AckMode::Normal, max_receive_buffer_size)?;

    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    for i in 0..n_packets_to_send {
        sbuf[0..4].copy_from_slice(&i.to_be_bytes());
        let now = pair.time;
        let n = pair.client_stream(client_ch, si)?.write_sctp(
            now,
            &Bytes::from(sbuf.clone()),
            PayloadProtocolIdentifier::Binary,
        )?;
        assert_eq!(sbuf.len(), n, "unexpected length of received data");
    }
    pair.drive_client();

    let mut rbuf = vec![0u8; 3000];

    // 1. First forward packets to receiver until rwnd becomes 0
    // 2. Wait until the sender's cwnd becomes 1*MTU (RTO occurred)
    // 3. Stat reading a1's data
    let mut n_packets_received = 0u32;
    let mut has_rtoed = false;
    while pair.client_conn_mut(client_ch).buffered_amount() > 0
        && n_packets_received < n_packets_to_send
    {
        /*println!(
            "buffered_amount {}, pair.server.inbound {}, n_packets_received {}, n_packets_to_send {}",
            pair.client_conn_mut(client_ch).buffered_amount(),
            pair.server.inbound.len(),
            n_packets_received,
            n_packets_to_send
        );*/

        if !has_rtoed {
            let rwnd = pair
                .server_conn_mut(server_ch)
                .get_my_receiver_window_credit();
            let cwnd = pair.client_conn_mut(client_ch).cwnd;
            let cmtu = pair.client_conn_mut(client_ch).mtu;
            if cwnd > cmtu || rwnd > 0 {
                // Do not read until a1.getMyReceiverWindowCredit() becomes zero
                pair.step();
                continue;
            }

            has_rtoed = true;
        }

        while let Some(chunks) = pair.server_stream(server_ch, si)?.read_sctp()? {
            let (n, ppi) = (chunks.len(), chunks.ppi);
            chunks.read(&mut rbuf)?;
            assert_eq!(sbuf.len(), n, "unexpected length of received data");
            assert_eq!(
                n_packets_received,
                u32::from_be_bytes([rbuf[0], rbuf[1], rbuf[2], rbuf[3]]),
                "unexpected length of received data"
            );
            assert_eq!(ppi, PayloadProtocolIdentifier::Binary, "unexpected ppi");

            n_packets_received += 1;
        }

        pair.step();
    }

    pair.drive();

    assert_eq!(
        n_packets_received, n_packets_to_send,
        "unexpected num of packets received"
    );
    assert_eq!(
        0,
        pair.server_conn_mut(server_ch)
            .streams
            .get(&si)
            .unwrap()
            .get_num_bytes_in_reassembly_queue(),
        "reassembly queue should be empty"
    );

    {
        let a = pair.client_conn_mut(client_ch);
        debug!("nSACKs      : {}", a.stats.get_num_sacks());
    }
    {
        let b = pair.server_conn_mut(server_ch);
        debug!("nDATAs      : {}", b.stats.get_num_datas());
        debug!("nAckTimeouts: {}", b.stats.get_num_ack_timeouts());
    }

    close_association_pair(&mut pair, client_ch, server_ch, si);

    Ok(())
}

#[test]
fn test_assoc_delayed_ack() -> Result<()> {
    //let _guard = subscribe();

    let si: u16 = 6;
    let mut sbuf = vec![0u8; 1000];
    let mut rbuf = vec![0u8; 1500];
    for i in 0..sbuf.len() {
        sbuf[i] = (i & 0xff) as u8;
    }

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::AlwaysDelay, 0)?;

    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    {
        pair.client_conn_mut(client_ch).stats.reset();
        pair.server_conn_mut(server_ch).stats.reset();
    }

    let now = pair.time;
    let n = pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &Bytes::from(sbuf.clone()),
        PayloadProtocolIdentifier::Binary,
    )?;
    assert_eq!(sbuf.len(), n, "unexpected length of received data");
    pair.drive_client();

    // Repeat calling br.Tick() until the buffered amount becomes 0
    let since = pair.time;
    let mut n_packets_received = 0;
    while pair.client_conn_mut(client_ch).buffered_amount() > 0 {
        pair.step();

        while let Some(chunks) = pair.server_stream(server_ch, si)?.read_sctp()? {
            let (n, ppi) = (chunks.len(), chunks.ppi);
            chunks.read(&mut rbuf)?;
            assert_eq!(sbuf.len(), n, "unexpected length of received data");
            assert_eq!(ppi, PayloadProtocolIdentifier::Binary, "unexpected ppi");

            n_packets_received += 1;
        }
    }
    let delay = (pair.time.duration_since(since).as_millis() as f64) / 1000.0;
    debug!("received in {} seconds", delay);
    assert!(delay >= 0.02, "should be >= 20msec");

    pair.drive();

    assert_eq!(n_packets_received, 1, "unexpected num of packets received");
    assert_eq!(
        0,
        pair.server_conn_mut(server_ch)
            .streams
            .get(&si)
            .unwrap()
            .get_num_bytes_in_reassembly_queue(),
        "reassembly queue should be empty"
    );

    let a_num_sacks = {
        let a = pair.client_conn_mut(client_ch);
        debug!("nSACKs      : {}", a.stats.get_num_sacks());
        assert_eq!(0, a.stats.get_num_t3timeouts(), "should be no retransmit");
        a.stats.get_num_sacks()
    };

    {
        let b = pair.server_conn_mut(server_ch);

        debug!("nDATAs      : {}", b.stats.get_num_datas());
        debug!("nAckTimeouts: {}", b.stats.get_num_ack_timeouts());

        assert_eq!(1, b.stats.get_num_datas(), "DATA chunk count mismatch");
        assert_eq!(
            a_num_sacks,
            b.stats.get_num_datas(),
            "sack count should be equal to the number of data chunks"
        );
        assert_eq!(
            1,
            b.stats.get_num_ack_timeouts(),
            "ackTimeout count mismatch"
        );
    }

    close_association_pair(&mut pair, client_ch, server_ch, si);

    Ok(())
}

#[test]
fn test_assoc_reset_close_one_way() -> Result<()> {
    //let _guard = subscribe();

    let si: u16 = 1;
    let msg: Bytes = Bytes::from_static(b"ABC");

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;

    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    {
        let a = pair.client_conn_mut(client_ch);
        assert_eq!(0, a.buffered_amount(), "incorrect bufferedAmount");
    }

    let now = pair.time;
    let n = pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &msg,
        PayloadProtocolIdentifier::Binary,
    )?;
    assert_eq!(msg.len(), n, "unexpected length of received data");
    {
        let a = pair.client_conn_mut(client_ch);
        assert_eq!(msg.len(), a.buffered_amount(), "incorrect bufferedAmount");
    }
    pair.step();

    let mut buf = vec![0u8; 32];

    while pair.server_stream(server_ch, si).is_ok() {
        debug!("s1.read_sctp begin");
        match pair.server_stream(server_ch, si)?.read_sctp() {
            Ok(chunks_opt) => {
                if let Some(chunks) = chunks_opt {
                    let (n, ppi) = (chunks.len(), chunks.ppi);
                    chunks.read(&mut buf)?;
                    debug!("s1.read_sctp done with {:?}", &buf[..n]);
                    assert_eq!(ppi, PayloadProtocolIdentifier::Binary, "unexpected ppi");
                    assert_eq!(n, msg.len(), "unexpected length of received data");
                }

                debug!("s0.close");
                let now = pair.time;
                pair.client_stream(client_ch, si)?.stop(now)?; // send reset

                pair.step();
            }
            Err(err) => {
                debug!("s1.read_sctp err {:?}", err);
                break;
            }
        }
    }

    pair.drive();

    close_association_pair(&mut pair, client_ch, server_ch, si);

    Ok(())
}

#[test]
fn test_assoc_reset_close_both_ways() -> Result<()> {
    //let _guard = subscribe();

    let si: u16 = 1;
    let msg: Bytes = Bytes::from_static(b"ABC");

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;

    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    {
        let a = pair.client_conn_mut(client_ch);
        assert_eq!(0, a.buffered_amount(), "incorrect bufferedAmount");
    }

    let now = pair.time;
    let n = pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &msg,
        PayloadProtocolIdentifier::Binary,
    )?;
    assert_eq!(msg.len(), n, "unexpected length of received data");
    {
        let a = pair.client_conn_mut(client_ch);
        assert_eq!(msg.len(), a.buffered_amount(), "incorrect bufferedAmount");
    }
    pair.step();

    let mut buf = vec![0u8; 32];

    while pair.server_stream(server_ch, si).is_ok() || pair.client_stream(client_ch, si).is_ok() {
        if pair.server_stream(server_ch, si).is_ok() {
            debug!("s1.read_sctp begin");
            match pair.server_stream(server_ch, si)?.read_sctp() {
                Ok(chunks_opt) => {
                    if let Some(chunks) = chunks_opt {
                        let (n, ppi) = (chunks.len(), chunks.ppi);
                        chunks.read(&mut buf)?;
                        debug!("s1.read_sctp done with {:?}", &buf[..n]);
                        assert_eq!(ppi, PayloadProtocolIdentifier::Binary, "unexpected ppi");
                        assert_eq!(n, msg.len(), "unexpected length of received data");
                    }
                }
                Err(err) => {
                    debug!("s1.read_sctp err {:?}", err);
                    break;
                }
            }
        }

        if pair.client_stream(client_ch, si).is_ok() {
            debug!("s0.read_sctp begin");
            match pair.client_stream(client_ch, si)?.read_sctp() {
                Ok(chunks_opt) => {
                    if let Some(chunks) = chunks_opt {
                        let (n, ppi) = (chunks.len(), chunks.ppi);
                        chunks.read(&mut buf)?;
                        debug!("s0.read_sctp done with {:?}", &buf[..n]);
                        assert_eq!(ppi, PayloadProtocolIdentifier::Binary, "unexpected ppi");
                        assert_eq!(n, msg.len(), "unexpected length of received data");
                    }
                }
                Err(err) => {
                    debug!("s0.read_sctp err {:?}", err);
                    break;
                }
            }
        }

        if pair.client_stream(client_ch, si).is_ok() {
            let now = pair.time;
            pair.client_stream(client_ch, si)?.stop(now)?; // send reset
        }
        if pair.server_stream(server_ch, si).is_ok() {
            let now = pair.time;
            pair.server_stream(server_ch, si)?.stop(now)?; // send reset
        }

        pair.step();
    }

    pair.drive();

    close_association_pair(&mut pair, client_ch, server_ch, si);

    Ok(())
}

#[test]
fn test_assoc_abort() -> Result<()> {
    //let _guard = subscribe();

    let si: u16 = 1;

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;

    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    let transmit = {
        let abort = ChunkAbort {
            error_causes: vec![ErrorCauseProtocolViolation {
                code: PROTOCOL_VIOLATION,
                ..Default::default()
            }],
        };

        let packet = pair
            .client_conn_mut(client_ch)
            .create_packet(vec![Box::new(abort)])
            .marshal()?;

        TransportMessage {
            now: pair.time,
            transport: TransportContext {
                local_addr: pair.client.addr,
                peer_addr: pair.server.addr,
                ecn: None,
                transport_protocol: Default::default(),
            },
            message: Payload::RawEncode(vec![packet]),
        }
    };

    // Both associations are established
    assert_eq!(
        AssociationState::Established,
        pair.client_conn_mut(client_ch).state()
    );
    assert_eq!(
        AssociationState::Established,
        pair.server_conn_mut(server_ch).state()
    );

    debug!("send ChunkAbort");
    pair.client.outbound.push_back(transmit);

    pair.drive();

    // The receiving association should be closed because it got an ABORT
    assert_eq!(
        AssociationState::Established,
        pair.client_conn_mut(client_ch).state()
    );
    assert_eq!(
        AssociationState::Closed,
        pair.server_conn_mut(server_ch).state()
    );

    close_association_pair(&mut pair, client_ch, server_ch, si);

    Ok(())
}

#[test]
fn test_association_handle_packet_before_init() -> Result<()> {
    //let _guard = subscribe();

    let tests = vec![
        (
            "InitAck",
            Packet {
                common_header: CommonHeader {
                    source_port: 1,
                    destination_port: 1,
                    verification_tag: 0,
                },
                chunks: vec![Box::new(ChunkInit {
                    is_ack: true,
                    initiate_tag: 1,
                    num_inbound_streams: 1,
                    num_outbound_streams: 1,
                    advertised_receiver_window_credit: 1500,
                    ..Default::default()
                })],
            },
        ),
        (
            "Abort",
            Packet {
                common_header: CommonHeader {
                    source_port: 1,
                    destination_port: 1,
                    verification_tag: 0,
                },
                chunks: vec![Box::new(ChunkAbort::default())],
            },
        ),
        (
            "CoockeEcho",
            Packet {
                common_header: CommonHeader {
                    source_port: 1,
                    destination_port: 1,
                    verification_tag: 0,
                },
                chunks: vec![Box::new(ChunkCookieEcho::default())],
            },
        ),
        (
            "HeartBeat",
            Packet {
                common_header: CommonHeader {
                    source_port: 1,
                    destination_port: 1,
                    verification_tag: 0,
                },
                chunks: vec![Box::new(ChunkHeartbeat::default())],
            },
        ),
        (
            "PayloadData",
            Packet {
                common_header: CommonHeader {
                    source_port: 1,
                    destination_port: 1,
                    verification_tag: 0,
                },
                chunks: vec![Box::new(ChunkPayloadData::default())],
            },
        ),
        (
            "Sack",
            Packet {
                common_header: CommonHeader {
                    source_port: 1,
                    destination_port: 1,
                    verification_tag: 0,
                },
                chunks: vec![Box::new(ChunkSelectiveAck {
                    cumulative_tsn_ack: 1000,
                    advertised_receiver_window_credit: 1500,
                    gap_ack_blocks: vec![GapAckBlock {
                        start: 100,
                        end: 200,
                    }],
                    ..Default::default()
                })],
            },
        ),
        (
            "Reconfig",
            Packet {
                common_header: CommonHeader {
                    source_port: 1,
                    destination_port: 1,
                    verification_tag: 0,
                },
                chunks: vec![Box::new(ChunkReconfig {
                    param_a: Some(Box::new(ParamOutgoingResetRequest::default())),
                    param_b: Some(Box::new(ParamReconfigResponse::default())),
                })],
            },
        ),
        (
            "ForwardTSN",
            Packet {
                common_header: CommonHeader {
                    source_port: 1,
                    destination_port: 1,
                    verification_tag: 0,
                },
                chunks: vec![Box::new(ChunkForwardTsn {
                    new_cumulative_tsn: 100,
                    ..Default::default()
                })],
            },
        ),
        (
            "Error",
            Packet {
                common_header: CommonHeader {
                    source_port: 1,
                    destination_port: 1,
                    verification_tag: 0,
                },
                chunks: vec![Box::new(ChunkError::default())],
            },
        ),
        (
            "Shutdown",
            Packet {
                common_header: CommonHeader {
                    source_port: 1,
                    destination_port: 1,
                    verification_tag: 0,
                },
                chunks: vec![Box::new(ChunkShutdown::default())],
            },
        ),
        (
            "ShutdownAck",
            Packet {
                common_header: CommonHeader {
                    source_port: 1,
                    destination_port: 1,
                    verification_tag: 0,
                },
                chunks: vec![Box::new(ChunkShutdownAck::default())],
            },
        ),
        (
            "ShutdownComplete",
            Packet {
                common_header: CommonHeader {
                    source_port: 1,
                    destination_port: 1,
                    verification_tag: 0,
                },
                chunks: vec![Box::new(ChunkShutdownComplete::default())],
            },
        ),
    ];

    let remote = SocketAddr::from_str("0.0.0.0:0").unwrap();
    let local = SocketAddr::from_str("0.0.0.0:0").unwrap();

    for (name, packet) in tests {
        debug!("testing {}", name);

        //let (a_conn, charlie_conn) = pipe();
        let config = Arc::new(TransportConfig::default());
        let mut a = Association::new(
            None,
            config,
            1400,
            0,
            remote,
            local,
            TransportProtocol::UDP,
            Instant::now(),
        );

        let packet = packet.marshal()?;
        a.handle_event(AssociationEvent(AssociationEventInner::Datagram(
            TransportMessage {
                now: Instant::now(),
                transport: TransportContext {
                    local_addr: local,
                    peer_addr: remote,
                    ecn: None,
                    transport_protocol: Default::default(),
                },
                message: Payload::RawEncode(vec![packet]),
            },
        )));

        a.close(AssociationError::LocallyClosed)?;
    }

    Ok(())
}

// This test reproduces an issue related to having regular messages (regular acks) which keep
// rescheduling the T3RTX timer before it can ever fire.
#[test]
fn test_old_rtx_on_regular_acks() -> Result<()> {
    let si: u16 = 6;
    let mut sbuf = vec![0u8; 1000];
    for i in 0..sbuf.len() {
        sbuf[i] = (i & 0xff) as u8;
    }

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::Normal, 0)?;
    pair.latency = Duration::from_millis(500);
    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    // Send 20 packet at a regular interval that is < RTO
    for i in 0..20u32 {
        //println!("sending packet {}", i);
        sbuf[0..4].copy_from_slice(&i.to_be_bytes());
        let now = pair.time;
        let n = pair.client_stream(client_ch, si)?.write_sctp(
            now,
            &Bytes::from(sbuf.clone()),
            PayloadProtocolIdentifier::Binary,
        )?;
        assert_eq!(sbuf.len(), n, "unexpected length of received data");
        pair.client.drive(pair.time, pair.server.addr);

        // drop a few transmits
        if i >= 5 && i < 10 {
            pair.client.outbound.clear();
        }

        pair.drive_client();
        pair.drive_server();
        pair.time += Duration::from_millis(500);
    }

    pair.drive_client();
    pair.drive_server();

    let mut buf = vec![0u8; 3000];

    // All packets must readable correctly
    for i in 0..20 {
        {
            let q = &pair
                .server_conn_mut(server_ch)
                .streams
                .get(&si)
                .unwrap()
                .reassembly_queue;
            //println!("q.is_readable()={}", q.is_readable());
            assert!(q.is_readable(), "should be readable at {}", i);
        }

        let chunks = pair.server_stream(server_ch, si)?.read_sctp()?.unwrap();
        let (n, ppi) = (chunks.len(), chunks.ppi);
        chunks.read(&mut buf)?;
        assert_eq!(n, sbuf.len(), "unexpected length of received data");
        assert_eq!(ppi, PayloadProtocolIdentifier::Binary, "unexpected ppi");
        assert_eq!(
            u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
            i,
            "unexpected received data"
        );
    }

    close_association_pair(&mut pair, client_ch, server_ch, si);

    Ok(())
}

/*
TODO: The following tests will be moved to sctp-async tests:
struct FakeEchoConn {
    wr_tx: Mutex<mpsc::Sender<Vec<u8>>>,
    rd_rx: Mutex<mpsc::Receiver<Vec<u8>>>,
    bytes_sent: AtomicUsize,
    bytes_received: AtomicUsize,
}

impl FakeEchoConn {
    fn new() -> impl Conn + AsAny {
        let (wr_tx, rd_rx) = mpsc::channel(1);
        FakeEchoConn {
            wr_tx: Mutex::new(wr_tx),
            rd_rx: Mutex::new(rd_rx),
            bytes_sent: AtomicUsize::new(0),
            bytes_received: AtomicUsize::new(0),
        }
    }
}

trait AsAny {
    fn as_any(&self) -> &(dyn std::any::Any);
}

impl AsAny for FakeEchoConn {
    fn as_any(&self) -> &(dyn std::any::Any) {
        self
    }
}

type UResult<T> = std::result::Result<T, util::Error>;

#[async_trait]
impl Conn for FakeEchoConn {
    fn connect(&self, _addr: SocketAddr) -> UResult<()> {
        Err(io::Error::new(io::ErrorKind::Other, "Not applicable").into())
    }

    fn recv(&self, b: &mut [u8]) -> UResult<usize> {
        let mut rd_rx = self.rd_rx.lock().await;
        let v = match rd_rx.recv().await {
            Some(v) => v,
            None => {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "Unexpected EOF").into())
            }
        };
        let l = std::cmp::min(v.len(), b.len());
        b[..l].copy_from_slice(&v[..l]);
        self.bytes_received.fetch_add(l, Ordering::SeqCst);
        Ok(l)
    }

    fn recv_from(&self, _buf: &mut [u8]) -> UResult<(usize, SocketAddr)> {
        Err(io::Error::new(io::ErrorKind::Other, "Not applicable").into())
    }

    fn send(&self, b: &[u8]) -> UResult<usize> {
        let wr_tx = self.wr_tx.lock().await;
        match wr_tx.send(b.to_vec()).await {
            Ok(_) => {}
            Err(err) => return Err(io::Error::new(io::ErrorKind::Other, err.to_string()).into()),
        };
        self.bytes_sent.fetch_add(b.len(), Ordering::SeqCst);
        Ok(b.len())
    }

    fn send_to(&self, _buf: &[u8], _target: SocketAddr) -> UResult<usize> {
        Err(io::Error::new(io::ErrorKind::Other, "Not applicable").into())
    }

    fn local_addr(&self) -> UResult<SocketAddr> {
        Err(io::Error::new(io::ErrorKind::AddrNotAvailable, "Addr Not Available").into())
    }

    fn remote_addr(&self) -> Option<SocketAddr> {
        None
    }

    fn close(&self) -> UResult<()> {
        Ok(())
    }
}

//use std::io::Write;

#[test]
fn test_stats() -> Result<()> {
    /*env_logger::Builder::new()
    .format(|buf, record| {
        writeln!(
            buf,
            "{}:{} [{}] {} - {}",
            record.file().unwrap_or("unknown"),
            record.line().unwrap_or(0),
            record.level(),
            chrono::Local::now().format("%H:%M:%S.%6f"),
            record.args()
        )
    })
    .filter(None, log::LevelFilter::Trace)
    .init();*/

    let conn = Arc::new(FakeEchoConn::new());
    let a = Association::client(Config {
        net_conn: Arc::clone(&conn) as Arc<dyn Conn>,
        max_receive_buffer_size: 0,
        max_message_size: 0,
        name: "client".to_owned(),
    })
    .await?;

    if let Some(conn) = conn.as_any().downcast_ref::<FakeEchoConn>() {
        assert_eq!(
            conn.bytes_received.load(Ordering::SeqCst),
            a.bytes_received()
        );
        assert_eq!(conn.bytes_sent.load(Ordering::SeqCst), a.bytes_sent());
    } else {
        assert!(false, "must be FakeEchoConn");
    }

    Ok(())
}

fn create_assocs() -> Result<(Association, Association)> {
    let addr1 = SocketAddr::from_str("0.0.0.0:0").unwrap();
    let addr2 = SocketAddr::from_str("0.0.0.0:0").unwrap();

    let udp1 = UdpSocket::bind(addr1).await.unwrap();
    let udp2 = UdpSocket::bind(addr2).await.unwrap();

    udp1.connect(udp2.local_addr().unwrap()).await.unwrap();
    udp2.connect(udp1.local_addr().unwrap()).await.unwrap();

    let (a1chan_tx, mut a1chan_rx) = mpsc::channel(1);
    let (a2chan_tx, mut a2chan_rx) = mpsc::channel(1);

    tokio::spawn(async move {
        let a = Association::client(Config {
            net_conn: Arc::new(udp1),
            max_receive_buffer_size: 0,
            max_message_size: 0,
            name: "client".to_owned(),
        })
        .await?;

        let _ = a1chan_tx.send(a).await;

        Result::<()>::Ok(())
    });

    tokio::spawn(async move {
        let a = Association::server(Config {
            net_conn: Arc::new(udp2),
            max_receive_buffer_size: 0,
            max_message_size: 0,
            name: "server".to_owned(),
        })
        .await?;

        let _ = a2chan_tx.send(a).await;

        Result::<()>::Ok(())
    });

    let timer1 = tokio::time::sleep(Duration::from_secs(1));
    tokio::pin!(timer1);
    let a1 = tokio::select! {
        _ = timer1.as_mut() =>{
            assert!(false,"timed out waiting for a1");
            return Err(Error::Other("timed out waiting for a1".to_owned()).into());
        },
        a1 = a1chan_rx.recv() => {
            a1.unwrap()
        }
    };

    let timer2 = tokio::time::sleep(Duration::from_secs(1));
    tokio::pin!(timer2);
    let a2 = tokio::select! {
        _ = timer2.as_mut() =>{
            assert!(false,"timed out waiting for a2");
            return Err(Error::Other("timed out waiting for a2".to_owned()).into());
        },
        a2 = a2chan_rx.recv() => {
            a2.unwrap()
        }
    };

    Ok((a1, a2))
}

//use std::io::Write;
//TODO: remove this conditional test
#[cfg(not(target_os = "windows"))]
#[test]
fn test_association_shutdown() -> Result<()> {
    /*env_logger::Builder::new()
    .format(|buf, record| {
        writeln!(
            buf,
            "{}:{} [{}] {} - {}",
            record.file().unwrap_or("unknown"),
            record.line().unwrap_or(0),
            record.level(),
            chrono::Local::now().format("%H:%M:%S.%6f"),
            record.args()
        )
    })
    .filter(None, log::LevelFilter::Trace)
    .init();*/

    let (a1, a2) = create_assocs().await?;

    let s11 = a1.open_stream(1, PayloadProtocolIdentifier::String).await?;
    let s21 = a2.open_stream(1, PayloadProtocolIdentifier::String).await?;

    let test_data = Bytes::from_static(b"test");

    let n = s11.write(&test_data).await?;
    assert_eq!(test_data.len(), n);

    let mut buf = vec![0u8; test_data.len()];
    let n = s21.read(&mut buf).await?;
    assert_eq!(test_data.len(), n);
    assert_eq!(&test_data, &buf[0..n]);

    if let Ok(result) = tokio::time::timeout(Duration::from_secs(1), a1.shutdown()).await {
        assert!(result.is_ok(), "shutdown should be ok");
    } else {
        assert!(false, "shutdown timeout");
    }

    {
        let mut close_loop_ch_rx = a2.close_loop_ch_rx.lock().await;

        // Wait for close read loop channels to prevent flaky tests.
        let timer2 = tokio::time::sleep(Duration::from_secs(1));
        tokio::pin!(timer2);
        tokio::select! {
            _ = timer2.as_mut() =>{
                assert!(false,"timed out waiting for a2 read loop to close");
            },
            _ = close_loop_ch_rx.recv() => {
                log::debug!("recv a2.close_loop_ch_rx");
            }
        };
    }
    Ok(())
}


fn test_association_shutdown_during_write() -> Result<()> {
    //let _guard = subscribe();

    let (a1, a2) = create_assocs().await?;

    let s11 = a1.open_stream(1, PayloadProtocolIdentifier::String).await?;
    let s21 = a2.open_stream(1, PayloadProtocolIdentifier::String).await?;

    let (writing_done_tx, mut writing_done_rx) = mpsc::channel::<()>(1);
    let ss21 = Arc::clone(&s21);
    tokio::spawn(async move {
        let mut i = 0;
        while ss21.write(&Bytes::from(vec![i])).await.is_ok() {
            if i == 255 {
                i = 0;
            } else {
                i += 1;
            }

            if i % 100 == 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        drop(writing_done_tx);
    });

    let test_data = Bytes::from_static(b"test");

    let n = s11.write(&test_data).await?;
    assert_eq!(test_data.len(), n);

    let mut buf = vec![0u8; test_data.len()];
    let n = s21.read(&mut buf).await?;
    assert_eq!(test_data.len(), n);
    assert_eq!(&test_data, &buf[0..n]);

    {
        let mut close_loop_ch_rx = a1.close_loop_ch_rx.lock().await;
        tokio::select! {
            res = tokio::time::timeout(Duration::from_secs(1), a1.shutdown()) => {
                if let Ok(result) = res {
                    assert!(result.is_ok(), "shutdown should be ok");
                } else {
                    assert!(false, "shutdown timeout");
                }
            }
            _ = writing_done_rx.recv() => {
                log::debug!("writing_done_rx");
                let result = close_loop_ch_rx.recv().await;
                log::debug!("a1.close_loop_ch_rx.recv: {:?}", result);
            },
        };
    }

    {
        let mut close_loop_ch_rx = a2.close_loop_ch_rx.lock().await;
        // Wait for close read loop channels to prevent flaky tests.
        let timer2 = tokio::time::sleep(Duration::from_secs(1));
        tokio::pin!(timer2);
        tokio::select! {
            _ = timer2.as_mut() =>{
                assert!(false,"timed out waiting for a2 read loop to close");
            },
            _ = close_loop_ch_rx.recv() => {
                log::debug!("recv a2.close_loop_ch_rx");
            }
        };
    }

    Ok(())
}*/

use crate::association::stream::StreamEvent;

/// Deliver every queued inbound packet to the server ONE AT A TIME, and after
/// each one poll association events and drain every `Readable` stream
/// immediately — exactly what `rtc`'s SCTP handler does
/// (rtc-0.20.3/src/peer_connection/handler/sctp.rs: `Event::Stream(Readable)`
/// => `while let Some(chunks) = stream.read_sctp()?`). This models the most
/// attentive application possible: it cannot read any sooner than this.
fn deliver_to_server_rtc_style(
    pair: &mut Pair,
    server_ch: AssociationHandle,
) -> (Vec<Vec<u8>>, Vec<String>) {
    let mut delivered: Vec<Vec<u8>> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let client_addr = pair.client.addr;

    while let Some((recv_time, ecn, packet)) = pair.server.inbound.pop_front() {
        if let Some((ch, event)) =
            pair.server
                .endpoint
                .handle(recv_time, client_addr, ecn, packet.into())
        {
            match event {
                DatagramEvent::NewAssociation(conn) => {
                    pair.server.associations.insert(ch, conn);
                }
                DatagramEvent::AssociationEvent(event) => {
                    if let Some(conn) = pair.server.associations.get_mut(&ch) {
                        conn.handle_event(event);
                    }
                }
            }
        }

        let conn = pair.server.associations.get_mut(&server_ch).unwrap();
        let mut readable: Vec<u16> = Vec::new();
        while let Some(event) = conn.poll() {
            if let Event::Stream(StreamEvent::Readable { id }) = event {
                readable.push(id);
            }
        }
        for id in readable {
            match conn.stream(id) {
                Ok(mut s) => loop {
                    match s.read_sctp() {
                        Ok(Some(chunks)) => {
                            let mut buf = vec![0u8; 256 * 1024];
                            match chunks.read(&mut buf) {
                                Ok(n) => delivered.push(buf[..n].to_vec()),
                                Err(e) => errors.push(format!("chunks.read: {e}")),
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            errors.push(format!("read_sctp(si={id}): {e}"));
                            break;
                        }
                    }
                },
                Err(e) => errors.push(format!("stream(si={id}): {e}")),
            }
        }
    }
    (delivered, errors)
}

/// webrtc-rs/webrtc#816 in the 0.20.x rewrite — the case the rewrite DOES fix.
///
/// A peer writes a message and immediately resets the stream (what a data
/// channel close does). rtc-sctp emits that as two packets, DATA first; the
/// application drains on the `Readable` event from the first packet before the
/// RECONFIG in the second is ever processed. Under webrtc-sctp 0.13.0 the same
/// sequence lost the data, because `read_sctp` returned EOF on `read_shutdown`
/// without draining the reassembly queue — the bug KPS patches.
///
/// PASSES on rtc-sctp 0.20.3. See the `kps_repro_816_*` tests for the two
/// orderings that still lose the data.
#[test]
fn kps_816_write_then_reset_delivered_in_order_survives() -> Result<()> {
    let si: u16 = 1;
    let msg: Vec<u8> = b"payload-that-must-survive-the-reset".to_vec();

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;
    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    let now = pair.time;
    // Write and reset with no server read in between.
    let n = pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &msg.clone().into(),
        PayloadProtocolIdentifier::Binary,
    )?;
    assert_eq!(msg.len(), n);
    let now = pair.time;
    pair.client_stream(client_ch, si)?.stop(now)?; // RFC 6525 outgoing reset

    // Flush the client's packets into the server's inbound queue, then feed
    // them to the server the way a real rtc application would consume them.
    pair.drive_client();

    // MECHANISM CHECK: how many packets carry the write+reset, and does the
    // reset actually get applied? If the reset landed in a later packet than
    // the DATA, the application would have drained in between and the test
    // would prove nothing.
    let n_packets = pair.server.inbound.len();
    let (delivered, errors) = deliver_to_server_rtc_style(&mut pair, server_ch);
    let stream_after = pair.server_conn_mut(server_ch).stream(si).is_ok();

    println!("inbound packets carrying write+reset = {n_packets}");
    println!("server still has stream {si} afterwards = {stream_after}");
    println!("delivered = {delivered:?}");
    println!("errors    = {errors:?}");
    assert_eq!(
        2, n_packets,
        "rtc-sctp gathers control chunks into their own packet, so write+reset \
         is two packets"
    );
    assert!(
        !stream_after,
        "mechanism not exercised: the incoming reset was never applied \
         (the stream is still registered on the receiver)"
    );
    assert_eq!(
        vec![msg],
        delivered,
        "data written before a stream reset was lost at the receiver (errors: {errors:?})"
    );
    Ok(())
}

/// webrtc-rs/webrtc#816 in the 0.20.x rewrite — the case that actually bit KPS.
///
/// The peer that writes-then-closes is `pion` (a Go WebRTC stack), and pion
/// bundles the RECONFIG with the DATA it is resetting: one SCTP packet,
/// `sender_last_tsn` == the TSN of the DATA chunk in the same packet. RFC 6525
/// then applies the reset in the same `handle()` call that delivered the data,
/// so "drain on Readable" gives the application no window at all.
///
/// rtc-sctp never emits that packet itself (control chunks are gathered into a
/// separate packet from DATA), so it is crafted here.
#[test]
fn kps_repro_816_bundled_data_and_reset_must_not_lose_data() -> Result<()> {
    let si: u16 = 1;
    let msg = Bytes::from_static(b"payload-that-must-survive-the-bundled-reset");

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;
    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    // Build one packet: DATA(tsn) + RECONFIG(sender_last_tsn = tsn) — pion's
    // write-then-close on a data channel.
    let (vtag, tsn, sport, dport) = {
        let a = pair.client_conn_mut(client_ch);
        (
            a.peer_verification_tag,
            a.my_next_tsn,
            a.source_port,
            a.destination_port,
        )
    };
    let packet = Packet {
        common_header: CommonHeader {
            source_port: sport,
            destination_port: dport,
            verification_tag: vtag,
        },
        chunks: vec![
            Box::new(ChunkPayloadData {
                beginning_fragment: true,
                ending_fragment: true,
                tsn,
                stream_identifier: si,
                stream_sequence_number: 1, // 0 was the DCEP hello in establish_session_pair
                payload_type: PayloadProtocolIdentifier::Binary,
                user_data: msg.clone(),
                ..Default::default()
            }),
            Box::new(ChunkReconfig {
                param_a: Some(Box::new(ParamOutgoingResetRequest {
                    reconfig_request_sequence_number: 100,
                    reconfig_response_sequence_number: 0,
                    sender_last_tsn: tsn,
                    stream_identifiers: vec![si],
                })),
                ..Default::default()
            }),
        ],
    };
    pair.server
        .inbound
        .push_back((pair.time, None, packet.marshal()?));

    let n_packets = pair.server.inbound.len();
    let (delivered, errors) = deliver_to_server_rtc_style(&mut pair, server_ch);
    let stream_after = pair.server_conn_mut(server_ch).stream(si).is_ok();

    println!("inbound packets = {n_packets}");
    println!("server still has stream {si} afterwards = {stream_after}");
    println!(
        "delivered = {:?}",
        delivered
            .iter()
            .map(|d| String::from_utf8_lossy(d).to_string())
            .collect::<Vec<_>>()
    );
    println!("errors    = {errors:?}");

    assert_eq!(
        1, n_packets,
        "mechanism: DATA and RECONFIG must share one packet"
    );
    assert!(
        !stream_after,
        "mechanism not exercised: the bundled reset was never applied"
    );
    assert_eq!(
        vec![msg.to_vec()],
        delivered,
        "data bundled with the stream reset was lost at the receiver (errors: {errors:?})"
    );
    Ok(())
}

/// webrtc-rs/webrtc#816 in the 0.20.x rewrite — no crafted packet needed.
///
/// rtc-sctp emits the write-then-close as two packets (control chunks are
/// gathered separately from DATA). Deliver them in the other order — which any
/// network may do, and which is the order a sender that gathers its control
/// queue first would produce anyway. The RECONFIG arrives with
/// `sender_last_tsn` ahead of `peer_last_tsn`, so it is deferred
/// (`ReconfigResult::InProgress`) and stored. When the DATA then arrives,
/// `handle_data` -> `handle_peer_last_tsn_and_acknowledgement` retries the
/// stored request *in the same `handle()` call*, unregistering the stream
/// before the application is ever handed the `Readable` event for the data
/// that came in with it.
#[test]
fn kps_repro_816_reordered_reset_before_data_must_not_lose_data() -> Result<()> {
    let si: u16 = 1;
    let msg = Bytes::from_static(b"payload-that-must-survive-the-reordered-reset");

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;
    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    let now = pair.time;
    let n = pair.client_stream(client_ch, si)?.write_sctp(
        now,
        &msg,
        PayloadProtocolIdentifier::Binary,
    )?;
    assert_eq!(msg.len(), n);
    let now = pair.time;
    pair.client_stream(client_ch, si)?.stop(now)?;

    pair.drive_client();
    let n_packets = pair.server.inbound.len();
    assert_eq!(
        2, n_packets,
        "expected DATA and RECONFIG in separate packets"
    );

    // Swap the two packets: RECONFIG first, DATA second.
    let mut pkts: Vec<_> = pair.server.inbound.drain(..).collect();
    pkts.reverse();
    pair.server.inbound.extend(pkts);

    let (delivered, errors) = deliver_to_server_rtc_style(&mut pair, server_ch);
    let stream_after = pair.server_conn_mut(server_ch).stream(si).is_ok();

    println!("server still has stream {si} afterwards = {stream_after}");
    println!(
        "delivered = {:?}",
        delivered
            .iter()
            .map(|d| String::from_utf8_lossy(d).to_string())
            .collect::<Vec<_>>()
    );
    println!("errors    = {errors:?}");

    assert!(
        !stream_after,
        "mechanism not exercised: the deferred reset was never applied"
    );
    assert_eq!(
        vec![msg.to_vec()],
        delivered,
        "data lost when the stream reset was processed ahead of the data it resets \
         (errors: {errors:?})"
    );
    Ok(())
}

/// The reset is what gets deferred, not the unregistration — so the two
/// invariants that shape has to keep:
///
///   * `Association::close()` still clears every stream, including one holding
///     unread data. Nothing will ever drain it, so deferring there would leak.
///   * a reset held back for an undrained stream is answered In Progress and
///     performed once the application takes the data, without waiting for the
///     peer to retransmit its RECONFIG.
#[test]
fn kps_816_close_unregisters_streams_holding_unread_data() -> Result<()> {
    let si: u16 = 1;
    let msg = Bytes::from_static(b"never collected");

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;
    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    let now = pair.time;
    pair.client_stream(client_ch, si)?
        .write_sctp(now, &msg, PayloadProtocolIdentifier::Binary)?;
    pair.drive_client();
    let _ = deliver_to_server_rtc_style_no_read(&mut pair, server_ch);

    {
        let a = pair.server_conn_mut(server_ch);
        assert!(
            a.streams
                .get(&si)
                .is_some_and(|s| s.reassembly_queue.is_readable()),
            "precondition: the server is holding a message the application never read"
        );
        a.close(AssociationError::LocallyClosed)?;
        assert!(
            a.streams.is_empty(),
            "close() must unregister every stream; deferring on unread data would leak \
             a stream nothing can ever drain"
        );
    }
    Ok(())
}

/// Deliver the queued packets without draining any stream — the application
/// simply never reads.
fn deliver_to_server_rtc_style_no_read(pair: &mut Pair, server_ch: AssociationHandle) -> usize {
    let client_addr = pair.client.addr;
    let mut n = 0;
    while let Some((recv_time, ecn, packet)) = pair.server.inbound.pop_front() {
        if let Some((ch, event)) =
            pair.server
                .endpoint
                .handle(recv_time, client_addr, ecn, packet.into())
        {
            match event {
                DatagramEvent::NewAssociation(conn) => {
                    pair.server.associations.insert(ch, conn);
                }
                DatagramEvent::AssociationEvent(event) => {
                    if let Some(conn) = pair.server.associations.get_mut(&ch) {
                        conn.handle_event(event);
                    }
                }
            }
        }
        let conn = pair.server.associations.get_mut(&server_ch).unwrap();
        while conn.poll().is_some() {}
        n += 1;
    }
    n
}

#[test]
fn kps_816_deferred_reset_completes_when_the_application_drains() -> Result<()> {
    let si: u16 = 1;
    let msg = Bytes::from_static(b"collected late");

    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;
    establish_session_pair(&mut pair, client_ch, server_ch, si)?;

    let now = pair.time;
    pair.client_stream(client_ch, si)?
        .write_sctp(now, &msg, PayloadProtocolIdentifier::Binary)?;
    let now = pair.time;
    pair.client_stream(client_ch, si)?.stop(now)?;
    pair.drive_client();

    // Reset ahead of the data, and the application does not read: the reset must
    // be held, not performed.
    let mut pkts: Vec<_> = pair.server.inbound.drain(..).collect();
    pkts.reverse();
    pair.server.inbound.extend(pkts);
    deliver_to_server_rtc_style_no_read(&mut pair, server_ch);

    assert!(
        pair.server_conn_mut(server_ch).stream(si).is_ok(),
        "the reset must be deferred while the application still has data to collect"
    );

    // Now the application collects it — and that alone must complete the reset,
    // with no RECONFIG retransmission from the peer.
    let mut buf = vec![0u8; 64];
    let chunks = pair
        .server_stream(server_ch, si)?
        .read_sctp()?
        .expect("the held data must still be there");
    let n = chunks.read(&mut buf)?;
    assert_eq!(&buf[..n], &msg[..], "the held data must be intact");

    // read_sctp retries the deferred request as the queue runs dry.
    let _ = pair
        .server_stream(server_ch, si)
        .and_then(|mut s| s.read_sctp());
    assert!(
        pair.server_conn_mut(server_ch).stream(si).is_err(),
        "once drained, the deferred reset must be performed"
    );
    Ok(())
}

/// `abort()` ends the association at once (RFC 9260 §9.1): the local side is closed, and the ABORT
/// it sends carries a "User-Initiated Abort" cause, so the peer closes its side too instead of
/// treating the chunk as an error. Each stream reports the loss, which is what closes data
/// channels above.
#[test]
fn test_abort_closes_both_sides() -> Result<()> {
    let si: u16 = 1;
    let (mut pair, client_ch, server_ch) = create_association_pair(AckMode::NoDelay, 0)?;
    establish_session_pair(&mut pair, client_ch, server_ch, si)?;
    while pair.server_conn_mut(server_ch).poll().is_some() {}

    pair.client_conn_mut(client_ch).abort()?;
    assert!(pair.client_conn_mut(client_ch).is_closed());

    pair.drive_client();
    pair.drive_server();

    let server = pair.server_conn_mut(server_ch);
    assert!(server.is_closed(), "the peer closes on the ABORT");
    let mut lost = vec![];
    while let Some(event) = server.poll() {
        if let Event::AssociationLost { reason, id } = event {
            lost.push((reason, id));
        }
    }
    assert_eq!(vec![(AssociationError::Reset, si)], lost);

    // Idempotent: the association is already closed.
    pair.client_conn_mut(client_ch).abort()?;
    Ok(())
}
