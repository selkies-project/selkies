/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! One sans-IO surface over the two agents a peer may run: the ICE-lite agent (`lite`) and
//! the full `rtc_ice::Agent`.
//!
//! The driver in `connection` speaks to an `IceAgent` and never to either implementation: it
//! hands in authenticated STUN and the clock, and takes out datagrams to send
//! (`Outbound`), events (`AgentEvent`), and the next deadline. The full agent is the server
//! when `webrtc_ice_lite` is off, controlling as the offerer is; its `rtc_ice` core matches an
//! inbound datagram to a local candidate by address, so `FullAgent` translates between the
//! bound address a datagram arrives on and the address the candidate advertises (a public one
//! behind 1:1 NAT), remembering per peer which bound address a check came in on so the answer
//! leaves from the same socket.
//!
//! As the controlled side, `rtc_ice` neither answers a nominating check until its own triggered
//! check on that pair has succeeded nor remembers the nomination afterward, so a peer that got
//! its answer would never select. `FullAgent` answers such a check itself and feeds it in again
//! on each answer to a check of its own, until the pair is selected, as RFC 8445 7.3.1.5 reads.
//! Once selected the pair stays: `rtc_ice` does not move to a later nomination.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use log::{debug, info};
use rtc_ice::agent::agent_config::AgentConfig;
use rtc_ice::agent::{Agent, Event};
use rtc_ice::candidate::{
    Candidate as RtcCandidate, CandidateType as RtcCandidateType, unmarshal_candidate,
};
use rtc_ice::mdns::MulticastDnsMode;
use rtc_ice::state::ConnectionState;
use rtc_ice::tcp_type::TcpType as RtcTcpType;
use rtc_shared::{TaggedBytesMut, TransportContext};
use rtc_stun::message::Message;
use sansio::Protocol as _;

use super::candidate::{Candidate, CandidateType, TcpType};
use super::lite::{LiteAgent, LiteTimings};
use super::stun;
use super::{Outbound, Protocol};

/// The state of a peer's ICE, as `RTCIceTransport.state` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IceState {
    New,
    Checking,
    Connected,
    Completed,
    Failed,
    Disconnected,
    Closed,
}

impl IceState {
    /// The state as the W3C API spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            IceState::New => "new",
            IceState::Checking => "checking",
            IceState::Connected => "connected",
            IceState::Completed => "completed",
            IceState::Failed => "failed",
            IceState::Disconnected => "disconnected",
            IceState::Closed => "closed",
        }
    }
}

/// What an agent reports to its driver.
#[derive(Clone, Debug)]
pub enum AgentEvent {
    State(IceState),
    /// The pair media rides from now on.
    SelectedPair {
        local: Candidate,
        remote: Candidate,
    },
}

/// One local candidate as an agent knows it: what it advertises and where it is bound.
#[derive(Clone, Debug)]
pub struct LocalEntry {
    pub candidate: Candidate,
    pub bound: SocketAddr,
    pub protocol: Protocol,
}

/// The local point a datagram arrived on: its index among the connection's points, its
/// bound address, and its transport.
#[derive(Clone, Copy, Debug)]
pub struct Arrival {
    pub index: usize,
    pub local: SocketAddr,
    pub protocol: Protocol,
}

/// How long the full agent goes without hearing the peer before it is disconnected, and then
/// failed: together its consent timeout.
const DISCONNECTED_TIMEOUT: Duration = Duration::from_secs(5);
const FAILED_TIMEOUT: Duration = Duration::from_secs(25);
/// Binding requests per pair before the full agent gives a pair up: at its 200 ms check
/// interval, a few seconds, as a check's retransmissions give it.
const MAX_BINDING_REQUESTS: u16 = 20;

/// The full ICE agent: `rtc_ice::Agent` with the address translation its driver needs.
pub struct FullAgent {
    agent: Agent,
    name: String,
    /// Bound address and transport of each advertised address.
    bound_of: HashMap<(SocketAddr, Protocol), SocketAddr>,
    /// Advertised address of each bound address.
    advertised_of: HashMap<(SocketAddr, Protocol), SocketAddr>,
    /// The bound address a peer's datagram last arrived on, per advertised address.
    arrived_on: HashMap<(SocketAddr, SocketAddr), SocketAddr>,
    state: IceState,
    local_pwd: String,
    /// Answers to nominating checks the agent left unanswered.
    answers: VecDeque<Outbound>,
    /// The peer's nominating check, until the agent acts on it.
    nomination: Option<(SocketAddr, SocketAddr, Protocol, Vec<u8>)>,
}

impl FullAgent {
    fn new(
        now: Instant,
        name: &str,
        ufrag: &str,
        pwd: &str,
        controlling: bool,
    ) -> Result<FullAgent, String> {
        let config = AgentConfig {
            local_ufrag: ufrag.to_string(),
            local_pwd: pwd.to_string(),
            is_controlling: controlling,
            lite: false,
            candidate_types: vec![
                RtcCandidateType::Host,
                RtcCandidateType::ServerReflexive,
                RtcCandidateType::Relay,
            ],
            multicast_dns_mode: MulticastDnsMode::Disabled,
            keepalive_interval: Some(Duration::from_secs_f64(super::CONSENT_INTERVAL)),
            disconnected_timeout: Some(DISCONNECTED_TIMEOUT),
            failed_timeout: Some(FAILED_TIMEOUT),
            max_binding_requests: Some(MAX_BINDING_REQUESTS),
            // A browser hides its host candidates behind mDNS, so its direct path usually
            // arrives as peer-reflexive and must not wait a second to be nominated.
            prflx_acceptance_min_wait: Some(Duration::ZERO),
            ..AgentConfig::default()
        };
        let agent = Agent::new(now, Arc::new(config), stun::crypto()).map_err(|e| e.to_string())?;
        Ok(FullAgent {
            agent,
            name: name.to_string(),
            bound_of: HashMap::new(),
            advertised_of: HashMap::new(),
            arrived_on: HashMap::new(),
            state: IceState::New,
            local_pwd: pwd.to_string(),
            answers: VecDeque::new(),
            nomination: None,
        })
    }

    fn advertised(&self, bound: SocketAddr, protocol: Protocol) -> SocketAddr {
        self.advertised_of
            .get(&(bound, protocol))
            .copied()
            .unwrap_or(bound)
    }

    fn bound(&self, advertised: SocketAddr, peer: SocketAddr, protocol: Protocol) -> SocketAddr {
        self.arrived_on
            .get(&(advertised, peer))
            .or_else(|| self.bound_of.get(&(advertised, protocol)))
            .copied()
            .unwrap_or(advertised)
    }

    fn feed(
        &mut self,
        now: Instant,
        local: SocketAddr,
        peer: SocketAddr,
        protocol: Protocol,
        raw: &[u8],
    ) {
        let advertised = self.advertised(local, protocol);
        self.arrived_on.insert((advertised, peer), local);
        let tagged = TaggedBytesMut {
            now,
            transport: TransportContext {
                local_addr: advertised,
                peer_addr: peer,
                transport_protocol: protocol.into(),
                ecn: None,
            },
            message: BytesMut::from(raw),
        };
        if let Err(e) = self.agent.handle_read(tagged) {
            debug!(target: "ice.agent", "{} discarded STUN from {peer}: {e}", self.name);
        }
    }
}

/// A candidate as the full agent reports it, read field by field: the agent gives a
/// peer-reflexive candidate an empty related address, which its SDP form cannot round-trip.
fn from_rtc(c: &RtcCandidate) -> Candidate {
    let kind = match c.candidate_type() {
        RtcCandidateType::ServerReflexive => CandidateType::Srflx,
        RtcCandidateType::PeerReflexive => CandidateType::Prflx,
        RtcCandidateType::Relay => CandidateType::Relay,
        _ => CandidateType::Host,
    };
    let related = c.related_address().filter(|r| !r.address.is_empty());
    Candidate {
        foundation: c.foundation(),
        component: c.component(),
        transport: if c.network_type().is_tcp() {
            Protocol::Tcp
        } else {
            Protocol::Udp
        },
        priority: c.priority(),
        host: c.address().to_string(),
        port: c.port(),
        kind,
        related_address: related.as_ref().map(|r| r.address.clone()),
        related_port: related.as_ref().map(|r| r.port),
        tcptype: match c.tcp_type() {
            RtcTcpType::Active => Some(TcpType::Active),
            RtcTcpType::Passive => Some(TcpType::Passive),
            RtcTcpType::SimultaneousOpen => Some(TcpType::So),
            _ => None,
        },
        generation: None,
    }
}

impl FullAgent {
    /// An authenticated request: into the agent, answered here where the agent will not.
    fn on_request(
        &mut self,
        now: Instant,
        at: Arrival,
        peer: SocketAddr,
        message: &Message,
        raw: &[u8],
    ) {
        self.feed(now, at.local, peer, at.protocol, raw);
        if !stun::use_candidate(message)
            || self.agent.role()
            || self.agent.get_selected_candidate_pair().is_some()
        {
            return;
        }
        match stun::binding_success(message, peer, &self.local_pwd) {
            Ok(data) => self.answers.push_back(Outbound {
                local: at.local,
                peer,
                protocol: at.protocol,
                data,
            }),
            Err(e) => {
                debug!(target: "ice.agent", "{} cannot answer check from {peer}: {e}", self.name)
            }
        }
        self.nomination = Some((at.local, peer, at.protocol, raw.to_vec()));
    }

    /// An authenticated response: into the agent, then the nomination it may now act on.
    fn on_response(&mut self, now: Instant, at: Arrival, peer: SocketAddr, raw: &[u8]) {
        self.feed(now, at.local, peer, at.protocol, raw);
        if self.agent.get_selected_candidate_pair().is_some() {
            self.nomination = None;
        } else if let Some((local, peer, protocol, check)) = self.nomination.clone() {
            self.feed(now, local, peer, protocol, &check);
        }
    }
}

fn map_state(state: ConnectionState) -> Option<IceState> {
    match state {
        ConnectionState::New => Some(IceState::New),
        ConnectionState::Checking => Some(IceState::Checking),
        ConnectionState::Connected => Some(IceState::Connected),
        ConnectionState::Completed => Some(IceState::Completed),
        ConnectionState::Failed => Some(IceState::Failed),
        ConnectionState::Disconnected => Some(IceState::Disconnected),
        ConnectionState::Closed => Some(IceState::Closed),
        _ => None,
    }
}

/// The agent a peer runs, lite or full, behind one surface.
pub enum IceAgent {
    Lite(Box<LiteAgent>),
    Full(Box<FullAgent>),
}

impl IceAgent {
    /// An agent named `name` in the log with the local credentials given.
    pub fn new(
        now: Instant,
        name: &str,
        ufrag: &str,
        pwd: &str,
        lite: bool,
        controlling: bool,
        timings: LiteTimings,
    ) -> Result<IceAgent, String> {
        if lite {
            Ok(IceAgent::Lite(Box::new(LiteAgent::new(
                name, ufrag, pwd, timings,
            ))))
        } else {
            FullAgent::new(now, name, ufrag, pwd, controlling).map(|a| IceAgent::Full(Box::new(a)))
        }
    }

    /// Whether this is the ICE-lite agent.
    pub fn is_lite(&self) -> bool {
        matches!(self, IceAgent::Lite(_))
    }

    /// Whether this agent controls nomination; an ICE-lite agent never does.
    pub fn is_controlling(&self) -> bool {
        match self {
            IceAgent::Lite(_) => false,
            IceAgent::Full(a) => a.agent.role(),
        }
    }

    /// Give the agent a local candidate and the point it is bound on.
    pub fn add_local(&mut self, entry: LocalEntry) {
        match self {
            IceAgent::Lite(a) => a.add_local(entry),
            IceAgent::Full(a) => {
                let advertised = entry.candidate.addr().unwrap_or(entry.bound);
                a.bound_of.insert((advertised, entry.protocol), entry.bound);
                // A server-reflexive candidate shares its host candidate's socket, and an
                // inbound datagram belongs to the host candidate, the check's base.
                a.advertised_of
                    .entry((entry.bound, entry.protocol))
                    .or_insert(advertised);
                match unmarshal_candidate(&entry.candidate.to_sdp()) {
                    Ok(c) => {
                        if let Err(e) = a.agent.add_local_candidate(c) {
                            debug!(target: "ice.agent", "{} local candidate {} refused: {e}", a.name, entry.candidate);
                        }
                    }
                    Err(e) => {
                        debug!(target: "ice.agent", "{} local candidate {} unreadable: {e}", a.name, entry.candidate)
                    }
                }
            }
        }
    }

    /// Give the agent a remote candidate the peer signaled.
    pub fn add_remote(&mut self, now: Instant, candidate: &Candidate) {
        match self {
            IceAgent::Lite(a) => a.add_remote(now, candidate),
            IceAgent::Full(a) => match unmarshal_candidate(&candidate.to_sdp()) {
                Ok(c) => {
                    if let Err(e) = a.agent.add_remote_candidate(c) {
                        debug!(target: "ice.agent", "{} remote candidate {} refused: {e}", a.name, candidate);
                    }
                }
                Err(e) => {
                    debug!(target: "ice.agent", "{} remote candidate {} unreadable: {e}", a.name, candidate)
                }
            },
        }
    }

    /// Start: the remote credentials are known and the checks (or the wait for the peer's)
    /// begin.
    pub fn start(
        &mut self,
        now: Instant,
        remote_ufrag: &str,
        remote_pwd: &str,
        remote_is_lite: bool,
    ) {
        match self {
            IceAgent::Lite(a) => a.start(now, remote_ufrag, remote_pwd),
            IceAgent::Full(a) => {
                let controlling = a.agent.role();
                if let Err(e) = a.agent.start_connectivity_checks(
                    now,
                    controlling,
                    remote_ufrag.to_string(),
                    remote_pwd.to_string(),
                ) {
                    info!(target: "ice", "{} cannot start connectivity checks: {e}", a.name);
                }
                if remote_is_lite && !controlling {
                    info!(target: "ice", "{} the peer is ICE-lite; taking the controlling role", a.name);
                    a.agent.set_role(true);
                }
            }
        }
    }

    /// An authenticated binding request from `peer` on the point `at`; `raw` is the datagram
    /// as it arrived.
    pub fn handle_request(
        &mut self,
        now: Instant,
        at: Arrival,
        peer: SocketAddr,
        message: &Message,
        raw: &[u8],
    ) {
        match self {
            IceAgent::Lite(a) => a.handle_request(now, at.index, peer, message),
            IceAgent::Full(a) => a.on_request(now, at, peer, message, raw),
        }
    }

    /// A binding response from `peer` on the point `at`.
    pub fn handle_response(
        &mut self,
        now: Instant,
        at: Arrival,
        peer: SocketAddr,
        message: &Message,
        raw: &[u8],
    ) {
        match self {
            IceAgent::Lite(a) => a.handle_response(now, peer, message),
            IceAgent::Full(a) => a.on_response(now, at, peer, raw),
        }
    }

    /// A non-STUN datagram from an authenticated `peer` arrived: the pair is alive.
    pub fn note_received(
        &mut self,
        now: Instant,
        local: SocketAddr,
        peer: SocketAddr,
        protocol: Protocol,
    ) {
        match self {
            IceAgent::Lite(a) => a.note_received(now),
            IceAgent::Full(a) => {
                let advertised = a.advertised(local, protocol);
                let _ = a.agent.is_valid_non_stun_traffic(
                    now,
                    TransportContext {
                        local_addr: advertised,
                        peer_addr: peer,
                        transport_protocol: protocol.into(),
                        ecn: None,
                    },
                );
            }
        }
    }

    /// An ICE-TCP stream from `peer` closed.
    pub fn peer_stream_lost(&mut self, now: Instant, peer: SocketAddr) {
        match self {
            IceAgent::Lite(a) => a.peer_stream_lost(now, peer),
            IceAgent::Full(a) => {
                if a.agent
                    .get_selected_candidate_pair()
                    .is_some_and(|(_, r)| r.addr() == peer)
                {
                    info!(target: "ice", "{} Stream to {peer} lost, connection over", a.name);
                    let _ = a.agent.close();
                    a.state = IceState::Failed;
                }
            }
        }
    }

    /// End the connection as failed: a socket it needs is gone.
    pub fn fail(&mut self) {
        match self {
            IceAgent::Lite(a) => a.fail(),
            IceAgent::Full(a) => {
                let _ = a.agent.close();
                a.state = IceState::Failed;
            }
        }
    }

    /// Advance the clock.
    pub fn handle_timeout(&mut self, now: Instant) {
        match self {
            IceAgent::Lite(a) => a.handle_timeout(now),
            IceAgent::Full(a) => {
                let _ = a.agent.handle_timeout(now);
            }
        }
    }

    /// When `handle_timeout` is next due.
    pub fn poll_timeout(&mut self) -> Option<Instant> {
        match self {
            IceAgent::Lite(a) => a.poll_timeout(),
            IceAgent::Full(a) => a.agent.poll_timeout(),
        }
    }

    /// The next datagram to send.
    pub fn poll_write(&mut self) -> Option<Outbound> {
        match self {
            IceAgent::Lite(a) => a.poll_write(),
            IceAgent::Full(a) => {
                if let Some(answer) = a.answers.pop_front() {
                    return Some(answer);
                }
                let msg = a.agent.poll_write()?;
                let protocol = match msg.transport.transport_protocol {
                    rtc_shared::TransportProtocol::TCP => Protocol::Tcp,
                    _ => Protocol::Udp,
                };
                let local = a.bound(msg.transport.local_addr, msg.transport.peer_addr, protocol);
                Some(Outbound {
                    local,
                    peer: msg.transport.peer_addr,
                    protocol,
                    data: msg.message.to_vec(),
                })
            }
        }
    }

    /// The next event.
    pub fn poll_event(&mut self) -> Option<AgentEvent> {
        match self {
            IceAgent::Lite(a) => a.poll_event(),
            IceAgent::Full(a) => {
                if a.state == IceState::Failed && a.agent.state() != ConnectionState::Failed {
                    a.state = IceState::Closed;
                    return Some(AgentEvent::State(IceState::Failed));
                }
                loop {
                    let event = a.agent.poll_event()?;
                    match event.event {
                        Event::ConnectionStateChange(state) => {
                            if let Some(state) = map_state(state) {
                                a.state = state;
                                return Some(AgentEvent::State(state));
                            }
                        }
                        Event::SelectedCandidatePairChange(local, remote) => {
                            return Some(AgentEvent::SelectedPair {
                                local: from_rtc(&local),
                                remote: from_rtc(&remote),
                            });
                        }
                        Event::RoleChange(controlling) => {
                            info!(
                                target: "ice",
                                "{} Switching to {} role",
                                a.name,
                                if controlling { "controlling" } else { "controlled" }
                            );
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// The pair media rides, once one is selected.
    pub fn selected_pair(&self) -> Option<(Candidate, Candidate)> {
        match self {
            IceAgent::Lite(a) => a.selected_pair(),
            IceAgent::Full(a) => {
                let (l, r) = a.agent.get_selected_candidate_pair()?;
                Some((from_rtc(l), from_rtc(r)))
            }
        }
    }

    /// The agent's state.
    pub fn state(&self) -> IceState {
        match self {
            IceAgent::Lite(a) => a.state(),
            IceAgent::Full(a) => a.state,
        }
    }

    /// End the agent.
    pub fn close(&mut self) {
        match self {
            IceAgent::Lite(a) => a.close(),
            IceAgent::Full(a) => {
                let _ = a.agent.close();
                a.state = IceState::Closed;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ice::stun::{Class, Role};

    /// Why `lite` is Selkies code: what `rtc_ice`'s own flag does with a nomination, the first
    /// thing a browser sends an ICE-lite server. When an upgrade fails this, the flag may do.
    #[test]
    fn the_lite_flag_of_rtc_ice_still_checks_and_does_not_answer_a_nomination() {
        let now = Instant::now();
        let (ufrag, pwd) = ("serverfr", "serverpasswordserverpass");
        let config = AgentConfig {
            local_ufrag: ufrag.to_string(),
            local_pwd: pwd.to_string(),
            lite: true,
            candidate_types: vec![RtcCandidateType::Host],
            multicast_dns_mode: MulticastDnsMode::Disabled,
            ..AgentConfig::default()
        };
        let mut agent = Agent::new(now, Arc::new(config), stun::crypto()).unwrap();
        let local: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let peer: SocketAddr = "127.0.0.1:6000".parse().unwrap();
        let candidate = unmarshal_candidate("1 1 udp 2130706431 127.0.0.1 5000 typ host").unwrap();
        agent.add_local_candidate(candidate).unwrap();
        agent
            .start_connectivity_checks(
                now,
                false,
                "clientfr".into(),
                "clientpasswordclientpass".into(),
            )
            .unwrap();
        let _ = agent.handle_timeout(now);
        assert!(agent.poll_write().is_none(), "the periodic checks are off");

        let check =
            stun::binding_request(ufrag, "clientfr", 7, Role::Controlling(9), true, pwd).unwrap();
        agent
            .handle_read(TaggedBytesMut {
                now,
                transport: TransportContext {
                    local_addr: local,
                    peer_addr: peer,
                    transport_protocol: rtc_shared::TransportProtocol::UDP,
                    ecn: None,
                },
                message: BytesMut::from(&check.raw[..]),
            })
            .unwrap();
        let mut classes = Vec::new();
        while let Some(out) = agent.poll_write() {
            classes.push(stun::class(&stun::parse(&out.message).unwrap()));
        }
        assert_eq!(
            classes,
            vec![Class::Request],
            "a check of its own, and no answer to the peer's"
        );
        assert!(
            agent.get_selected_candidate_pair().is_none(),
            "and the nomination selects nothing"
        );
    }
}
