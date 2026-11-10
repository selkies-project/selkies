/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The per-peer ICE driver: local points, gathering, authentication, and the agent's clock.
//!
//! An `IceConnection` is the state one peer's task drives. It owns the peer's local points
//! (`LocalPoint`): on each host address a UDP mux attachment or a socket of its own bound
//! inside the port window, a passive TCP attachment where a TCP mux is configured, and a TURN
//! relay where one is configured for a full agent; every point delivers to the one inbound
//! channel the task reads. `gather` builds the candidates: host candidates advertising the public
//! address of their family behind 1:1 NAT (`IceConfig::public_ips`), with unique local preferences
//! per address, then server-reflexive candidates by a STUN query over the IPv4 host points within
//! a bound. A TURN allocation runs beside the queries: a relay ready by the time they end is
//! among the gathered candidates, and one still allocating does not hold the offer back (a TURN
//! server that answers nothing would hold it for the allocation's whole timeout); it is added
//! when it lands (`late_relay`), its candidate trickled to the peer.
//!
//! Every STUN request is authenticated here before any agent sees it (`stun::authenticate_request`):
//! one that fails is answered 400 and its source stays unlearned, one that passes teaches the
//! mux the peer's address and marks the source as authenticated, which is the gate for every
//! non-STUN datagram handed up to DTLS. A check arriving before the remote credentials are known
//! is answered and kept (`early`), then replayed into the agent at `start`, so a browser that
//! checks before its answer lands is not made to wait. The remote candidate list is bounded
//! (`MAX_REMOTE_CANDIDATES`) and deduplicated, since the signaling that hands candidates over
//! serves every peer; an mDNS candidate is resolved off the task (`IceConfig::mdns_lookup`), its
//! answer coming back on the inbound channel, a name that did not resolve is not looked up
//! again, and end-of-candidates waits for the lookups still out without holding anything.
//!
//! The answer to a request this connection sends is taken only with the peer's
//! MESSAGE-INTEGRITY, which authenticates the source as a request does. Its transaction id is
//! registered on a shared socket as the request leaves, so an answer from an address the mux has
//! not learned yet still finds this connection.

use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::BytesMut;
use log::{debug, info};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::agent::{AgentEvent, Arrival, IceAgent, IceState, LocalEntry};
use super::candidate::{
    Candidate, CandidateType, TcpType, candidate_foundation, candidate_priority,
    host_local_preference,
};
use super::config::{IceConfig, Servers};
use super::gather::{bind_ports, bind_udp, host_addresses};
use super::lite::LiteTimings;
use super::mux::Attachment;
use super::stun::{self, Class};
use super::turn::{self, TurnRelay};
use super::{
    IceError, Inbound, LOCAL_PWD_LENGTH, LOCAL_UFRAG_LENGTH, MAX_REMOTE_CANDIDATES, Outbound,
    Packet, Protocol,
};

/// Seconds the server-reflexive gathering waits in all.
const GATHER_TIMEOUT: Duration = Duration::from_secs(5);
/// The retransmission schedule of a server-reflexive query, RTO 0.5 s doubling.
const SRFLX_RTO: Duration = Duration::from_millis(500);
/// Early checks kept for replay at `start`.
const MAX_EARLY_CHECKS: usize = 64;
/// How long a request's transaction stays registered on a shared socket, past the longest
/// retransmission schedule of a check.
const TRANSACTION_MEMORY: Duration = Duration::from_secs(40);
/// How a UDP socket of the connection's own reads, as the mux sockets do.
const UDP_CHUNK: usize = 128 * 1024;
const UDP_MAX_DATAGRAM: usize = 65_536;
/// The number the next connection without a label logs under.
static NEXT_UNLABELED: AtomicU64 = AtomicU64::new(1);

/// A future's output if it is ready at this poll, without waiting for it.
fn futures_now<T>(future: &mut Pin<Box<dyn Future<Output = T> + Send>>) -> Option<T> {
    let waker = std::task::Waker::noop();
    let mut context = std::task::Context::from_waker(waker);
    match future.as_mut().poll(&mut context) {
        std::task::Poll::Ready(value) => Some(value),
        std::task::Poll::Pending => None,
    }
}

/// The ICE parameters one side signals.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IceParameters {
    pub username_fragment: String,
    pub password: String,
    pub ice_lite: bool,
}

/// What a connection reports to its task.
#[derive(Clone, Debug)]
pub enum ConnectionEvent {
    State(IceState),
    SelectedPair { local: Candidate, remote: Candidate },
}

enum Sender {
    Udp {
        socket: Arc<UdpSocket>,
        reader: JoinHandle<()>,
    },
    Mux(Attachment),
    Relay(TurnRelay),
}

/// One local candidate's socket: where the connection sends from and what it advertises.
pub struct LocalPoint {
    pub candidate: Candidate,
    pub bound: SocketAddr,
    pub protocol: Protocol,
    sender: Sender,
}

impl LocalPoint {
    /// Send `data` to `peer` from this point, without blocking.
    pub fn send(&self, peer: SocketAddr, data: &[u8]) -> io::Result<()> {
        match &self.sender {
            Sender::Udp { socket, .. } => match socket.try_send_to(data, peer) {
                Ok(_) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(()),
                Err(e) => Err(e),
            },
            Sender::Mux(attachment) => attachment.send(peer, data),
            Sender::Relay(relay) => relay.send(peer, data),
        }
    }

    fn learn_peer(&self, peer: SocketAddr) {
        if let Sender::Mux(attachment) = &self.sender {
            attachment.learn_peer(peer);
        }
    }

    fn expect_response(&self, tid: [u8; 12], names_peer: bool) {
        if let Sender::Mux(attachment) = &self.sender {
            attachment.expect_response(tid, names_peer);
        }
    }

    fn forget_response(&self, tid: &[u8; 12]) {
        if let Sender::Mux(attachment) = &self.sender {
            attachment.forget_response(tid);
        }
    }

    fn is_shared(&self) -> bool {
        matches!(self.sender, Sender::Mux(_))
    }

    fn close(&self) {
        match &self.sender {
            Sender::Udp { reader, .. } => reader.abort(),
            Sender::Mux(attachment) => attachment.detach(),
            Sender::Relay(relay) => relay.close(),
        }
    }
}

/// The pair media rides: the point to send from and the peer to send to.
#[derive(Clone)]
pub struct SelectedPath {
    pub point: Arc<LocalPoint>,
    pub peer: SocketAddr,
}

/// One peer's ICE: its points, its agent, and the rules between the wire and the agent.
pub struct IceConnection {
    name: String,
    config: IceConfig,
    local_username: String,
    local_password: String,
    agent: IceAgent,
    points: Vec<Arc<LocalPoint>>,
    local_candidates: Vec<Candidate>,
    gathered: bool,
    inbound_tx: mpsc::Sender<Inbound>,
    remote: Option<IceParameters>,
    remote_keys: HashSet<(u16, Protocol, String, u16, CandidateType, Option<TcpType>)>,
    remote_candidates: Vec<Candidate>,
    remote_end: bool,
    remote_ending: bool,
    unresolved: HashSet<String>,
    lookups: Vec<JoinHandle<()>>,
    lookups_out: usize,
    outstanding: VecDeque<(Instant, usize, [u8; 12])>,
    authenticated: HashSet<SocketAddr>,
    early: Vec<Packet>,
    deferred: Vec<Inbound>,
    started: bool,
    selected: Arc<Mutex<Option<SelectedPath>>>,
    events: VecDeque<ConnectionEvent>,
    last_received: Instant,
    state: IceState,
    closed: bool,
    /// A TURN allocation gathering left running (`late_relay`).
    pending_relay: Option<PendingRelay>,
}

/// A TURN allocation in flight.
type PendingRelay = Pin<Box<dyn Future<Output = Result<TurnRelay, IceError>> + Send>>;

impl IceConnection {
    /// A connection for one peer; its points deliver to `inbound_tx`.
    ///
    /// # Errors
    /// The configuration is refused (`IceConfig::validate`) or the agent cannot be built.
    pub fn new(
        config: IceConfig,
        inbound_tx: mpsc::Sender<Inbound>,
        timings: LiteTimings,
    ) -> Result<IceConnection, IceError> {
        config.validate()?;
        let local_username = config
            .local_username
            .clone()
            .unwrap_or_else(|| stun::random_string(LOCAL_UFRAG_LENGTH));
        let local_password = config
            .local_password
            .clone()
            .unwrap_or_else(|| stun::random_string(LOCAL_PWD_LENGTH));
        let name = match &config.label {
            Some(label) => format!("Connection({label})"),
            None => format!(
                "Connection({})",
                NEXT_UNLABELED.fetch_add(1, Ordering::Relaxed)
            ),
        };
        let agent = IceAgent::new(
            Instant::now(),
            &name,
            &local_username,
            &local_password,
            config.lite,
            config.controlling && !config.lite,
            timings,
        )
        .map_err(IceError::Config)?;
        Ok(IceConnection {
            name,
            config,
            local_username,
            local_password,
            agent,
            points: Vec::new(),
            local_candidates: Vec::new(),
            gathered: false,
            inbound_tx,
            remote: None,
            remote_keys: HashSet::new(),
            remote_candidates: Vec::new(),
            remote_end: false,
            remote_ending: false,
            unresolved: HashSet::new(),
            lookups: Vec::new(),
            lookups_out: 0,
            outstanding: VecDeque::new(),
            authenticated: HashSet::new(),
            early: Vec::new(),
            deferred: Vec::new(),
            started: false,
            selected: Arc::new(Mutex::new(None)),
            events: VecDeque::new(),
            last_received: Instant::now(),
            state: IceState::New,
            closed: false,
            pending_relay: None,
        })
    }

    /// The name this connection logs under.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether this is an ICE-lite agent.
    pub fn is_lite(&self) -> bool {
        self.agent.is_lite()
    }

    /// Whether this agent controls nomination.
    pub fn is_controlling(&self) -> bool {
        self.agent.is_controlling()
    }

    /// The local ICE parameters to signal.
    pub fn local_parameters(&self) -> IceParameters {
        IceParameters {
            username_fragment: self.local_username.clone(),
            password: self.local_password.clone(),
            ice_lite: self.agent.is_lite(),
        }
    }

    /// The local candidates gathered so far.
    pub fn local_candidates(&self) -> &[Candidate] {
        &self.local_candidates
    }

    /// The remote candidates taken so far.
    pub fn remote_candidates(&self) -> &[Candidate] {
        &self.remote_candidates
    }

    /// The connection's state.
    pub fn state(&self) -> IceState {
        self.state
    }

    /// When the peer was last heard from: media, DTLS, SCTP, or its checks.
    pub fn last_received(&self) -> Instant {
        self.last_received
    }

    /// The selected path, shared with whoever sends on the caller's task.
    pub fn selected_handle(&self) -> Arc<Mutex<Option<SelectedPath>>> {
        self.selected.clone()
    }

    /// The selected pair's candidates, once nominated.
    pub fn selected_pair(&self) -> Option<(Candidate, Candidate)> {
        self.agent.selected_pair()
    }

    /// The local points gathered.
    pub fn points(&self) -> &[Arc<LocalPoint>] {
        &self.points
    }

    /// Gather the local candidates, reading `inbound` for the answers to the server-reflexive
    /// queries and holding anything else for `take_deferred`.
    ///
    /// Host candidates come first, one per address on the UDP mux or a socket of their own,
    /// and one passive TCP candidate per address on the TCP mux; a STUN or TURN server adds
    /// its candidates within `GATHER_TIMEOUT`. An address that cannot be bound or shared is
    /// logged and skipped, never raised.
    pub async fn gather(
        &mut self,
        inbound: &mut mpsc::Receiver<Inbound>,
    ) -> Result<Vec<Candidate>, IceError> {
        if self.gathered {
            return Ok(self.local_candidates.clone());
        }
        self.gathered = true;
        let addresses = match &self.config.addresses {
            Some(a) => a.clone(),
            None => host_addresses(self.config.use_ipv4, self.config.use_ipv6),
        };
        let component = 1;
        let ports = bind_ports(if self.config.udp_mux.is_some() {
            None
        } else {
            self.config.port_range
        });
        let mut host_points = Vec::new();
        for (index, &address) in addresses.iter().enumerate() {
            let sender = if let Some(mux) = &self.config.udp_mux {
                match mux.attach(address, &self.local_username, self.inbound_tx.clone()) {
                    Ok(attachment) => Sender::Mux(attachment),
                    Err(e) => {
                        info!(target: "ice", "{} Could not share the UDP mux on {address} - {e}", self.name);
                        continue;
                    }
                }
            } else {
                match bind_udp(address, &ports) {
                    Ok(socket) => {
                        let socket = Arc::new(socket);
                        let reader =
                            tokio::spawn(read_udp(socket.clone(), self.inbound_tx.clone()));
                        Sender::Udp { socket, reader }
                    }
                    Err(e) => {
                        info!(target: "ice", "{} Could not bind to {address} - {e}", self.name);
                        continue;
                    }
                }
            };
            let bound = match &sender {
                Sender::Mux(a) => a.local_addr(),
                Sender::Udp { socket, .. } => socket.local_addr()?,
                Sender::Relay(_) => unreachable!("host points are never relays"),
            };
            let candidate = Candidate {
                foundation: candidate_foundation(
                    CandidateType::Host,
                    Protocol::Udp,
                    &bound.ip().to_string(),
                ),
                component,
                transport: Protocol::Udp,
                priority: candidate_priority(
                    component,
                    CandidateType::Host,
                    host_local_preference(index as u32, None),
                ),
                host: self.advertised_host(bound.ip()).to_string(),
                port: bound.port(),
                kind: CandidateType::Host,
                related_address: None,
                related_port: None,
                tcptype: None,
                generation: None,
            };
            let point = Arc::new(LocalPoint {
                candidate,
                bound,
                protocol: Protocol::Udp,
                sender,
            });
            host_points.push(point.clone());
            self.add_point(point);
        }
        if let Some(mux) = self.config.tcp_mux.clone() {
            for (index, &address) in addresses.iter().enumerate() {
                let attachment = match mux
                    .attach(address, &self.local_username, self.inbound_tx.clone())
                    .await
                {
                    Ok(a) => a,
                    Err(e) => {
                        info!(target: "ice", "{} Could not share the TCP mux on {address} - {e}", self.name);
                        continue;
                    }
                };
                let bound = attachment.local_addr();
                let candidate = Candidate {
                    foundation: candidate_foundation(
                        CandidateType::Host,
                        Protocol::Tcp,
                        &bound.ip().to_string(),
                    ),
                    component,
                    transport: Protocol::Tcp,
                    priority: candidate_priority(
                        component,
                        CandidateType::Host,
                        host_local_preference(index as u32, Some(TcpType::Passive)),
                    ),
                    host: self.advertised_host(bound.ip()).to_string(),
                    port: bound.port(),
                    kind: CandidateType::Host,
                    related_address: None,
                    related_port: None,
                    tcptype: Some(TcpType::Passive),
                    generation: None,
                };
                self.add_point(Arc::new(LocalPoint {
                    candidate,
                    bound,
                    protocol: Protocol::Tcp,
                    sender: Sender::Mux(attachment),
                }));
            }
        }
        let servers = Servers::from_ice_servers(&self.config.servers);
        if servers.stun.is_some() || servers.turn.is_some() {
            self.gather_from_servers(&servers, &host_points, inbound, component)
                .await;
        }
        Ok(self.local_candidates.clone())
    }

    fn add_point(&mut self, point: Arc<LocalPoint>) {
        self.agent.add_local(LocalEntry {
            candidate: point.candidate.clone(),
            bound: point.bound,
            protocol: point.protocol,
        });
        self.local_candidates.push(point.candidate.clone());
        self.points.push(point);
    }

    /// The address a host candidate bound on `bound` advertises: the configured public
    /// address of its family, where there is one, in place of the private one.
    fn advertised_host(&self, bound: IpAddr) -> IpAddr {
        self.config
            .public_ips
            .iter()
            .copied()
            .find(|ip| ip.is_ipv4() == bound.is_ipv4())
            .unwrap_or(bound)
    }

    /// Server-reflexive queries over the IPv4 host points within `GATHER_TIMEOUT`, and a TURN
    /// allocation beside them, kept among the candidates if it is done when they are and left
    /// running for `late_relay` otherwise; datagrams that are not their answers are kept for
    /// later.
    async fn gather_from_servers(
        &mut self,
        servers: &Servers,
        host_points: &[Arc<LocalPoint>],
        inbound: &mut mpsc::Receiver<Inbound>,
        component: u16,
    ) {
        let deadline = tokio::time::Instant::now() + GATHER_TIMEOUT;
        let mut relay: Option<PendingRelay> = servers.turn.as_ref().map(|server| {
            let tx = self.inbound_tx.clone();
            Box::pin(turn::allocate(server.clone(), tx)) as PendingRelay
        });
        let mut queries: Vec<SrflxQuery> = Vec::new();
        if let Some((host, port)) = &servers.stun {
            match tokio::net::lookup_host((host.as_str(), *port)).await {
                Ok(mut addrs) => {
                    if let Some(server) = addrs.find(|a| a.is_ipv4()) {
                        for point in host_points.iter().filter(|p| p.bound.is_ipv4()) {
                            if let Ok(request) = stun::plain_binding_request() {
                                point.expect_response(request.transaction_id.0, false);
                                let _ = point.send(server, &request.raw);
                                queries.push(SrflxQuery {
                                    point: point.clone(),
                                    tid: request.transaction_id.0,
                                    raw: request.raw,
                                    server,
                                    retry_at: tokio::time::Instant::now() + SRFLX_RTO,
                                    delay: SRFLX_RTO * 2,
                                    tries: 1,
                                });
                            }
                        }
                    }
                }
                Err(e) => {
                    info!(target: "ice", "{} STUN server {host} does not resolve: {e}", self.name)
                }
            }
        }
        let mut relay_result: Option<Result<TurnRelay, IceError>> = None;
        while !queries.is_empty() && tokio::time::Instant::now() < deadline {
            let next_retry = queries
                .iter()
                .map(|q| q.retry_at)
                .min()
                .unwrap_or(deadline)
                .min(deadline);
            tokio::select! {
                message = inbound.recv() => {
                    match message {
                        Some(Inbound::Packet(packet)) => {
                            if let Some(index) = self.srflx_answer(&packet, &queries) {
                                let query = queries.swap_remove(index);
                                query.point.forget_response(&query.tid);
                                if let Ok(message) = stun::parse(&packet.data)
                                    && let Some(mapped) = stun::mapped_address(&message)
                                {
                                    let base = &query.point.candidate;
                                    let candidate = Candidate {
                                        foundation: candidate_foundation(CandidateType::Srflx, Protocol::Udp, &base.host),
                                        component,
                                        transport: Protocol::Udp,
                                        priority: candidate_priority(component, CandidateType::Srflx, 65535),
                                        host: mapped.ip().to_string(),
                                        port: mapped.port(),
                                        kind: CandidateType::Srflx,
                                        related_address: Some(base.host.clone()),
                                        related_port: Some(base.port),
                                        tcptype: None,
                                        generation: None,
                                    };
                                    self.agent.add_local(LocalEntry {
                                        candidate: candidate.clone(),
                                        bound: query.point.bound,
                                        protocol: Protocol::Udp,
                                    });
                                    self.local_candidates.push(candidate);
                                }
                            } else {
                                self.deferred.push(Inbound::Packet(packet));
                            }
                        }
                        Some(other) => self.deferred.push(other),
                        None => break,
                    }
                }
                result = async { match relay.as_mut() { Some(r) => r.await, None => std::future::pending().await } }, if relay.is_some() && relay_result.is_none() => {
                    relay_result = Some(result);
                }
                _ = tokio::time::sleep_until(next_retry) => {
                    let now = tokio::time::Instant::now();
                    queries.retain_mut(|q| {
                        if now >= q.retry_at {
                            if q.tries > 6 {
                                q.point.forget_response(&q.tid);
                                return false;
                            }
                            q.tries += 1;
                            q.retry_at = now + q.delay;
                            q.delay *= 2;
                            let _ = q.point.send(q.server, &q.raw);
                        }
                        true
                    });
                }
            }
        }
        for query in queries {
            query.point.forget_response(&query.tid);
        }
        // A relay that answered at once is in the offer; any other follows it.
        if relay_result.is_none()
            && let Some(pending) = relay.as_mut()
        {
            relay_result = futures_now(pending);
        }
        match relay_result {
            Some(Ok(relay)) => {
                self.add_relay(relay, component);
            }
            Some(Err(e)) => {
                info!(target: "ice", "{} TURN relay candidate not gathered: {e}", self.name)
            }
            None => {
                if relay.is_some() {
                    debug!(target: "ice", "{} TURN relay candidate still allocating; it follows the offer", self.name);
                    self.pending_relay = relay;
                }
            }
        }
    }

    /// Whether a TURN allocation is still in flight, its candidate yet to follow the offer.
    pub fn relay_pending(&self) -> bool {
        self.pending_relay.is_some()
    }

    /// The relay candidate of the allocation `gather` left running, once it lands; `None` where
    /// it failed or there is none, never resolving while none is in flight. Cancel-safe: the
    /// allocation is kept between calls.
    pub async fn late_relay(&mut self) -> Option<Candidate> {
        let result = match self.pending_relay.as_mut() {
            Some(pending) => pending.await,
            None => return std::future::pending().await,
        };
        self.pending_relay = None;
        match result {
            Ok(relay) => Some(self.add_relay(relay, 1)),
            Err(e) => {
                info!(target: "ice", "{} TURN relay candidate not gathered: {e}", self.name);
                None
            }
        }
    }

    /// Add a relay point for an allocation that landed, returning its candidate.
    fn add_relay(&mut self, relay: TurnRelay, component: u16) -> Candidate {
        let candidate = Candidate {
            foundation: candidate_foundation(
                CandidateType::Relay,
                Protocol::Udp,
                &relay.relayed_addr.ip().to_string(),
            ),
            component,
            transport: Protocol::Udp,
            priority: candidate_priority(component, CandidateType::Relay, 65535),
            host: relay.relayed_addr.ip().to_string(),
            port: relay.relayed_addr.port(),
            kind: CandidateType::Relay,
            related_address: Some(relay.related_addr.ip().to_string()),
            related_port: Some(relay.related_addr.port()),
            tcptype: None,
            generation: None,
        };
        let bound = relay.relayed_addr;
        self.add_point(Arc::new(LocalPoint {
            candidate: candidate.clone(),
            bound,
            protocol: Protocol::Udp,
            sender: Sender::Relay(relay),
        }));
        candidate
    }

    fn srflx_answer(&self, packet: &Packet, queries: &[SrflxQuery]) -> Option<usize> {
        if !stun::is_stun(&packet.data) {
            return None;
        }
        let message = stun::parse(&packet.data).ok()?;
        if stun::class(&message) != Class::Success {
            return None;
        }
        queries
            .iter()
            .position(|q| q.tid == message.transaction_id.0 && q.point.bound == packet.local)
    }

    /// Datagrams that arrived during gathering and were not its answers, to feed through
    /// `handle_inbound` now.
    pub fn take_deferred(&mut self) -> Vec<Inbound> {
        std::mem::take(&mut self.deferred)
    }

    /// Take the remote ICE parameters.
    pub fn set_remote_parameters(&mut self, parameters: IceParameters) {
        self.remote = Some(parameters);
    }

    /// Add a remote candidate, or `None` for end-of-candidates.
    ///
    /// A candidate the peer repeats is taken once, `MAX_REMOTE_CANDIDATES` in all; one with a
    /// type this agent does not pair with or an address that does not parse is logged and
    /// dropped; an mDNS name is resolved off the caller's task, and end-of-candidates then
    /// lands once the lookups still out have settled.
    pub fn add_remote_candidate(&mut self, now: Instant, candidate: Option<Candidate>) {
        let Some(candidate) = candidate else {
            if self.lookups_out > 0 {
                self.remote_ending = true;
            } else {
                self.remote_end = true;
            }
            return;
        };
        if self.remote_end || self.remote_ending {
            return;
        }
        let key = candidate.dedupe_key();
        if self.remote_keys.contains(&key) {
            return;
        }
        if self.remote_keys.len() >= MAX_REMOTE_CANDIDATES {
            info!(
                target: "ice",
                "{} Remote candidate \"{}\" dropped: {MAX_REMOTE_CANDIDATES} remote candidates taken",
                self.name,
                candidate.host
            );
            return;
        }
        self.remote_keys.insert(key);
        if candidate.is_mdns() {
            if !self.unresolved.contains(&candidate.host) {
                self.lookups_out += 1;
                let lookup = self.config.mdns_lookup.clone();
                let tx = self.inbound_tx.clone();
                self.lookups.push(tokio::spawn(async move {
                    let address = lookup(candidate.host.clone()).await;
                    let _ = tx.send(Inbound::Resolved { candidate, address }).await;
                }));
            }
            return;
        }
        self.take_remote_candidate(now, candidate);
    }

    /// Validate a resolved remote candidate and hand it to the agent.
    fn take_remote_candidate(&mut self, now: Instant, candidate: Candidate) {
        if !matches!(
            candidate.kind,
            CandidateType::Host | CandidateType::Relay | CandidateType::Srflx
        ) {
            info!(
                target: "ice",
                "{} Remote candidate \"{}\" is not valid: unexpected candidate type \"{}\"",
                self.name,
                candidate.host,
                candidate.kind.as_str()
            );
            return;
        }
        if candidate.addr().is_none() {
            info!(target: "ice", "{} Remote candidate \"{}\" is not valid: not an IP address", self.name, candidate.host);
            return;
        }
        self.agent.add_remote(now, &candidate);
        self.remote_candidates.push(candidate);
    }

    /// Whether end-of-candidates landed: signaled, with every mDNS lookup settled.
    pub fn remote_candidates_ended(&self) -> bool {
        self.remote_end
    }

    /// Begin connectivity: the agent checks, or waits for the peer's checks, and every early
    /// check is replayed into it.
    ///
    /// # Errors
    /// Gathering was not performed, or the remote parameters are not set.
    pub fn start(&mut self, now: Instant) -> Result<(), IceError> {
        if !self.gathered {
            return Err(IceError::Failed(
                "Local candidates gathering was not performed".into(),
            ));
        }
        let remote = self
            .remote
            .clone()
            .ok_or_else(|| IceError::Failed("Remote username or password is missing".into()))?;
        if self.started {
            return Ok(());
        }
        self.started = true;
        self.agent.start(
            now,
            &remote.username_fragment,
            &remote.password,
            remote.ice_lite,
        );
        let early = std::mem::take(&mut self.early);
        for packet in early {
            self.replay(packet);
        }
        self.drain();
        Ok(())
    }

    fn replay(&mut self, packet: Packet) {
        let Some(local_index) = self.point_index(packet.local, packet.protocol) else {
            return;
        };
        let Ok(message) = stun::parse(&packet.data) else {
            return;
        };
        let at = Arrival {
            index: local_index,
            local: packet.local,
            protocol: packet.protocol,
        };
        self.agent
            .handle_request(packet.now, at, packet.peer, &message, &packet.data);
    }

    fn point_index(&self, local: SocketAddr, protocol: Protocol) -> Option<usize> {
        self.points
            .iter()
            .position(|p| p.bound == local && p.protocol == protocol)
    }

    /// Feed one inbound message; a non-STUN datagram from an authenticated peer is returned for
    /// the layers above.
    pub fn handle_inbound(&mut self, inbound: Inbound) -> Option<Packet> {
        if self.closed {
            return None;
        }
        let packet = match inbound {
            Inbound::TcpStreamLost { peer, .. } => {
                self.agent.peer_stream_lost(Instant::now(), peer);
                self.drain();
                return None;
            }
            Inbound::MuxClosed { local } => {
                info!(target: "ice", "{} The shared socket at {local} closed, connection over", self.name);
                self.agent.fail();
                self.drain();
                return None;
            }
            Inbound::Resolved {
                mut candidate,
                address,
            } => {
                self.lookups_out = self.lookups_out.saturating_sub(1);
                match address {
                    Some(address) => {
                        info!(target: "ice", "{} Remote candidate \"{}\" resolved to {address}", self.name, candidate.host);
                        candidate.host = address.to_string();
                        self.take_remote_candidate(Instant::now(), candidate);
                    }
                    None => {
                        info!(target: "ice", "{} Remote candidate \"{}\" could not be resolved", self.name, candidate.host);
                        self.unresolved.insert(candidate.host);
                    }
                }
                if self.remote_ending && self.lookups_out == 0 {
                    self.remote_ending = false;
                    self.remote_end = true;
                }
                self.drain();
                return None;
            }
            Inbound::Packet(packet) => packet,
        };
        let Some(local_index) = self.point_index(packet.local, packet.protocol) else {
            debug!(target: "ice.agent", "{} datagram on unknown point {}", self.name, packet.local);
            return None;
        };
        if !stun::is_stun(&packet.data) {
            if !self.authenticated.contains(&packet.peer) {
                debug!(target: "ice.agent", "{} dropped {} bytes from unauthenticated {}", self.name, packet.data.len(), packet.peer);
                return None;
            }
            self.last_received = packet.now;
            self.agent
                .note_received(packet.now, packet.local, packet.peer, packet.protocol);
            return Some(packet);
        }
        let mut message = match stun::parse(&packet.data) {
            Ok(m) => m,
            Err(e) => {
                debug!(target: "ice.agent", "{} malformed STUN from {}: {e}", self.name, packet.peer);
                return None;
            }
        };
        match stun::class(&message) {
            Class::Request => {
                let point = self.points[local_index].clone();
                if !stun::is_binding(&message) {
                    self.answer_error(&point, packet.peer, &message, stun::BAD_REQUEST);
                    return None;
                }
                let remote_ufrag = self.remote.as_ref().map(|r| r.username_fragment.as_str());
                if let Err(e) = stun::authenticate_request(
                    &mut message,
                    &self.local_username,
                    &self.local_password,
                    remote_ufrag,
                ) {
                    debug!(target: "ice.agent", "{} check from {} refused: {e:?}", self.name, packet.peer);
                    self.answer_error(&point, packet.peer, &message, stun::BAD_REQUEST);
                    return None;
                }
                self.last_received = packet.now;
                self.authenticated.insert(packet.peer);
                point.learn_peer(packet.peer);
                if !self.started {
                    if self.agent.is_lite() && stun::has(&message, stun::attrs::ATTR_ICE_CONTROLLED)
                    {
                        self.answer_error(&point, packet.peer, &message, stun::ROLE_CONFLICT);
                        return None;
                    }
                    match stun::binding_success(&message, packet.peer, &self.local_password) {
                        Ok(data) => {
                            let _ = point.send(packet.peer, &data);
                        }
                        Err(e) => {
                            debug!(target: "ice.agent", "{} cannot answer early check: {e}", self.name)
                        }
                    }
                    if self.early.len() < MAX_EARLY_CHECKS {
                        self.early.push(packet);
                    }
                    return None;
                }
                let at = Arrival {
                    index: local_index,
                    local: packet.local,
                    protocol: packet.protocol,
                };
                self.agent
                    .handle_request(packet.now, at, packet.peer, &message, &packet.data);
            }
            Class::Success | Class::Error => {
                let Some(remote) = &self.remote else {
                    return None;
                };
                if !stun::has(&message, rtc_stun::attributes::ATTR_MESSAGE_INTEGRITY)
                    || !stun::verify_integrity(&mut message, &remote.password)
                {
                    debug!(target: "ice.agent", "{} answer from {} fails integrity", self.name, packet.peer);
                    return None;
                }
                self.points[local_index].forget_response(&message.transaction_id.0);
                self.last_received = packet.now;
                self.authenticated.insert(packet.peer);
                self.points[local_index].learn_peer(packet.peer);
                let at = Arrival {
                    index: local_index,
                    local: packet.local,
                    protocol: packet.protocol,
                };
                self.agent
                    .handle_response(packet.now, at, packet.peer, &message, &packet.data);
            }
            Class::Indication => {}
        }
        self.drain();
        None
    }

    fn answer_error(
        &self,
        point: &LocalPoint,
        peer: SocketAddr,
        request: &rtc_stun::message::Message,
        error: (u16, &str),
    ) {
        if let Ok(data) = stun::binding_error(request, error.0, error.1, &self.local_password) {
            let _ = point.send(peer, &data);
        }
    }

    /// Advance the agent's clock.
    pub fn handle_timeout(&mut self, now: Instant) {
        if self.closed {
            return;
        }
        self.agent.handle_timeout(now);
        self.drain();
    }

    /// When `handle_timeout` is next due.
    pub fn poll_timeout(&mut self) -> Option<Instant> {
        if self.closed {
            None
        } else {
            self.agent.poll_timeout()
        }
    }

    /// Register a request about to leave a shared socket, so its answer finds this connection
    /// from an address the mux has not learned, and forget those long past answering.
    fn register_transaction(&mut self, index: usize, data: &[u8]) {
        let point = self.points[index].clone();
        if !point.is_shared() || !stun::is_stun(data) {
            return;
        }
        let now = Instant::now();
        while let Some((at, old_index, tid)) = self.outstanding.front().copied() {
            if now.saturating_duration_since(at) < TRANSACTION_MEMORY {
                break;
            }
            self.points[old_index].forget_response(&tid);
            self.outstanding.pop_front();
        }
        if let Ok(message) = stun::parse(data)
            && stun::class(&message) == Class::Request
        {
            point.expect_response(message.transaction_id.0, true);
            self.outstanding
                .push_back((now, index, message.transaction_id.0));
        }
    }

    /// The point a selected local candidate rides: its own, or for a server-reflexive one the
    /// host point it was derived from.
    fn point_of(&self, local: &Candidate) -> Option<Arc<LocalPoint>> {
        let found = self.points.iter().find(|p| {
            p.candidate.foundation == local.foundation
                && p.candidate.transport == local.transport
                && p.candidate.port == local.port
        });
        found
            .or_else(|| {
                self.points.iter().find(|p| {
                    p.candidate.transport == local.transport
                        && local.related_address.as_deref() == Some(p.candidate.host.as_str())
                        && local.related_port == Some(p.candidate.port)
                })
            })
            .cloned()
    }

    /// Send the agent's datagrams and translate its events.
    fn drain(&mut self) {
        while let Some(Outbound {
            local,
            peer,
            protocol,
            data,
        }) = self.agent.poll_write()
        {
            match self.point_index(local, protocol) {
                Some(index) => {
                    self.register_transaction(index, &data);
                    if let Err(e) = self.points[index].send(peer, &data) {
                        debug!(target: "ice.agent", "{} send to {peer} failed: {e}", self.name);
                    }
                }
                None => {
                    debug!(target: "ice.agent", "{} no point bound at {local} for {peer}", self.name)
                }
            }
        }
        while let Some(event) = self.agent.poll_event() {
            match event {
                AgentEvent::State(state) => {
                    self.state = state;
                    if matches!(state, IceState::Failed | IceState::Closed) {
                        *self.selected.lock().unwrap() = None;
                    }
                    self.events.push_back(ConnectionEvent::State(state));
                }
                AgentEvent::SelectedPair { local, remote } => {
                    match (self.point_of(&local), remote.addr()) {
                        (Some(point), Some(peer)) => {
                            *self.selected.lock().unwrap() = Some(SelectedPath { point, peer });
                            self.events
                                .push_back(ConnectionEvent::SelectedPair { local, remote });
                        }
                        _ => {
                            debug!(target: "ice.agent", "{} selected pair names no local point: {local}", self.name)
                        }
                    }
                }
            }
        }
    }

    /// The next event.
    pub fn poll_event(&mut self) -> Option<ConnectionEvent> {
        self.events.pop_front()
    }

    /// Send `data` to the peer on the selected pair.
    ///
    /// # Errors
    /// `NotConnected` before a pair is selected; the socket's error otherwise.
    pub fn send(&self, data: &[u8]) -> Result<Instant, IceError> {
        send_on(&self.selected, data)
    }

    /// Close the connection: the agent ends, every point is released.
    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        for lookup in self.lookups.drain(..) {
            lookup.abort();
        }
        self.agent.close();
        *self.selected.lock().unwrap() = None;
        for point in self.points.drain(..) {
            point.close();
        }
        self.local_candidates.clear();
        self.state = IceState::Closed;
        self.events
            .push_back(ConnectionEvent::State(IceState::Closed));
    }
}

/// Send `data` on a selected path, as the connection or a handle holding the path does.
pub fn send_on(selected: &Mutex<Option<SelectedPath>>, data: &[u8]) -> Result<Instant, IceError> {
    let path = selected
        .lock()
        .unwrap()
        .clone()
        .ok_or(IceError::NotConnected)?;
    path.point.send(path.peer, data)?;
    Ok(Instant::now())
}

impl Drop for IceConnection {
    fn drop(&mut self) {
        for point in &self.points {
            point.close();
        }
    }
}

struct SrflxQuery {
    point: Arc<LocalPoint>,
    tid: [u8; 12],
    raw: Vec<u8>,
    server: SocketAddr,
    retry_at: tokio::time::Instant,
    delay: Duration,
    tries: u32,
}

/// Read a socket of the connection's own, delivering every datagram to its task.
async fn read_udp(socket: Arc<UdpSocket>, tx: mpsc::Sender<Inbound>) {
    let Ok(local) = socket.local_addr() else {
        return;
    };
    let mut buf = BytesMut::with_capacity(UDP_CHUNK);
    loop {
        if buf.capacity() < UDP_MAX_DATAGRAM {
            buf = BytesMut::with_capacity(UDP_CHUNK);
        }
        let peer = match socket.recv_buf_from(&mut buf).await {
            Ok((_, peer)) => SocketAddr::new(peer.ip(), peer.port()),
            Err(_) => continue,
        };
        let data = buf.split();
        if data.is_empty() {
            continue;
        }
        let packet = Packet {
            now: Instant::now(),
            local,
            peer,
            protocol: Protocol::Udp,
            data,
        };
        match tx.try_send(Inbound::Packet(packet)) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => return,
        }
    }
}
