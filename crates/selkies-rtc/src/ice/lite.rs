/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The ICE-lite agent (RFC 8445 appendix A): the server answers checks and sends none.
//!
//! An ICE-lite agent gathers host candidates only, is the controlled agent whatever it was
//! asked to be, and answers a peer that claims the controlled role with 487 rather than
//! switching (only the full agent can take the controlling role). It never sends a
//! connectivity check: the peer's nomination alone puts a pair in its valid list
//! (RFC 8445 7.3.1.5), and the agent follows the peer's latest nomination. Its one request
//! is consent freshness (RFC 7675) on the selected pair, every `CONSENT_INTERVAL` seconds
//! jittered by 0.8 to 1.2, retransmitted 0.5, 1, and 2 seconds apart so an answer can come
//! back from behind a standing queue, and consent lapses only after `CONSENT_TIMEOUT` without
//! one. A peer that never checks is waited for within `INBOUND_CHECK_TIMEOUT`, since no local
//! check ever fails the connection.
//!
//! Everything here is sans-IO: the driver authenticates each request before handing it in,
//! and takes the answers and consent checks out through `poll_write`. The timings are
//! parameters (`LiteTimings`) so a test can scale them, as the fork's suites do.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use log::{debug, info};
use rand::Rng;
use rtc_stun::attributes::ATTR_ICE_CONTROLLED;
use rtc_stun::message::Message;

use super::agent::{AgentEvent, IceState, LocalEntry};
use super::candidate::{Candidate, CandidateType, TcpType, candidate_priority};
use super::stun::{self, Class, Role};
use super::{
    CONSENT_INTERVAL, CONSENT_RETRANSMISSIONS, CONSENT_TIMEOUT, INBOUND_CHECK_TIMEOUT, Outbound,
    Protocol,
};

/// The first retransmission delay of a consent check; each later one doubles it.
const RETRY_RTO: Duration = Duration::from_millis(500);

/// The timings an ICE-lite agent runs on, scaled down by tests.
#[derive(Clone, Copy, Debug)]
pub struct LiteTimings {
    pub consent_interval: Duration,
    pub consent_timeout: Duration,
    pub consent_retransmissions: u32,
    pub inbound_check_timeout: Duration,
}

impl Default for LiteTimings {
    fn default() -> Self {
        LiteTimings {
            consent_interval: Duration::from_secs_f64(CONSENT_INTERVAL),
            consent_timeout: Duration::from_secs_f64(CONSENT_TIMEOUT),
            consent_retransmissions: CONSENT_RETRANSMISSIONS,
            inbound_check_timeout: Duration::from_secs_f64(INBOUND_CHECK_TIMEOUT),
        }
    }
}

struct Pair {
    local: usize,
    remote: usize,
    nominated: bool,
}

/// A consent check in flight.
struct Pending {
    transaction_id: [u8; 12],
    message: Vec<u8>,
    tries: u32,
    retry_at: Instant,
    delay: Duration,
}

/// The ICE-lite agent of one peer connection.
pub struct LiteAgent {
    name: String,
    local_ufrag: String,
    local_pwd: String,
    remote: Option<(String, String)>,
    tie_breaker: u64,
    locals: Vec<LocalEntry>,
    remotes: Vec<Candidate>,
    pairs: Vec<Pair>,
    selected: Option<usize>,
    state: IceState,
    inbound_deadline: Option<Instant>,
    timings: LiteTimings,
    next_consent_at: Option<Instant>,
    pending: Option<Pending>,
    consented_at: Option<Instant>,
    last_received: Option<Instant>,
    writes: VecDeque<Outbound>,
    events: VecDeque<AgentEvent>,
}

impl LiteAgent {
    /// An agent answering checks for `ufrag` with `pwd`.
    pub fn new(name: &str, ufrag: &str, pwd: &str, timings: LiteTimings) -> LiteAgent {
        LiteAgent {
            name: name.to_string(),
            local_ufrag: ufrag.to_string(),
            local_pwd: pwd.to_string(),
            remote: None,
            tie_breaker: rand::rng().random(),
            locals: Vec::new(),
            remotes: Vec::new(),
            pairs: Vec::new(),
            selected: None,
            state: IceState::New,
            inbound_deadline: None,
            timings,
            next_consent_at: None,
            pending: None,
            consented_at: None,
            last_received: None,
            writes: VecDeque::new(),
            events: VecDeque::new(),
        }
    }

    /// Add a local candidate; `locals` are indexed as the driver's points are.
    pub fn add_local(&mut self, entry: LocalEntry) {
        self.locals.push(entry);
    }

    /// Take a remote candidate; an ICE-lite agent pairs it but never checks it.
    pub fn add_remote(&mut self, _now: Instant, candidate: &Candidate) {
        if self.remotes.iter().any(|c| c == candidate) {
            return;
        }
        self.remotes.push(candidate.clone());
    }

    /// The remote credentials are known: answer with the full username from now on and bound
    /// the wait for the peer's nomination.
    pub fn start(&mut self, now: Instant, remote_ufrag: &str, remote_pwd: &str) {
        self.remote = Some((remote_ufrag.to_string(), remote_pwd.to_string()));
        if self.selected.is_none() {
            self.inbound_deadline = Some(now + self.timings.inbound_check_timeout);
        }
        self.set_state(IceState::Checking);
    }

    /// The agent's state.
    pub fn state(&self) -> IceState {
        self.state
    }

    /// The pair the peer nominated, once it has.
    pub fn selected_pair(&self) -> Option<(Candidate, Candidate)> {
        let pair = &self.pairs[self.selected?];
        Some((
            self.locals[pair.local].candidate.clone(),
            self.remotes[pair.remote].clone(),
        ))
    }

    /// A datagram from the peer arrived: it is alive.
    pub fn note_received(&mut self, now: Instant) {
        self.last_received = Some(now);
    }

    /// An authenticated binding request from `peer` on local point `local_index`.
    ///
    /// Answers it, learns a peer-reflexive candidate when the source is new, and takes the
    /// peer's nomination; a request claiming the controlled role is a role conflict answered
    /// 487, since this agent never controls.
    pub fn handle_request(
        &mut self,
        now: Instant,
        local_index: usize,
        peer: SocketAddr,
        message: &Message,
    ) {
        if local_index >= self.locals.len() {
            return;
        }
        self.last_received = Some(now);
        if message.contains(ATTR_ICE_CONTROLLED) {
            self.send_error(local_index, peer, message, stun::ROLE_CONFLICT);
            return;
        }
        match stun::binding_success(message, peer, &self.local_pwd) {
            Ok(data) => self.send(local_index, peer, data),
            Err(e) => {
                debug!(target: "ice.agent", "{} cannot answer check from {peer}: {e}", self.name)
            }
        }
        if self.remote.is_none() {
            return;
        }
        let remote_index = match self.remotes.iter().position(|c| c.addr() == Some(peer)) {
            Some(i) => i,
            None => {
                let local = &self.locals[local_index];
                let candidate = Candidate {
                    foundation: stun::random_string(10),
                    component: local.candidate.component,
                    transport: local.protocol,
                    priority: stun::priority(message).unwrap_or_else(|| {
                        candidate_priority(local.candidate.component, CandidateType::Prflx, 65535)
                    }),
                    host: peer.ip().to_string(),
                    port: peer.port(),
                    kind: CandidateType::Prflx,
                    related_address: None,
                    related_port: None,
                    tcptype: (local.protocol == Protocol::Tcp).then_some(TcpType::Active),
                    generation: None,
                };
                info!(target: "ice", "{} Discovered peer reflexive candidate {candidate}", self.name);
                self.remotes.push(candidate);
                self.remotes.len() - 1
            }
        };
        let pair_index = match self
            .pairs
            .iter()
            .position(|p| p.local == local_index && p.remote == remote_index)
        {
            Some(i) => i,
            None => {
                self.pairs.push(Pair {
                    local: local_index,
                    remote: remote_index,
                    nominated: false,
                });
                self.pairs.len() - 1
            }
        };
        if stun::use_candidate(message) && !self.pairs[pair_index].nominated {
            self.pairs[pair_index].nominated = true;
            self.select(now, pair_index);
        }
    }

    fn select(&mut self, now: Instant, pair_index: usize) {
        let first = self.selected.is_none();
        self.selected = Some(pair_index);
        self.inbound_deadline = None;
        let (local, remote) = self.selected_pair().expect("the pair just selected exists");
        info!(
            target: "ice",
            "{} ICE completed: {} -> {}",
            self.name,
            local.to_sdp(),
            remote.to_sdp()
        );
        self.events
            .push_back(AgentEvent::SelectedPair { local, remote });
        if first {
            self.consented_at = Some(now);
            self.next_consent_at = Some(now + self.jittered_interval());
            self.set_state(IceState::Connected);
            self.set_state(IceState::Completed);
        }
    }

    /// A binding response from `peer`: the answer to a consent check, if one is out.
    ///
    /// The driver checked its MESSAGE-INTEGRITY against the peer's password before it got here.
    pub fn handle_response(&mut self, now: Instant, peer: SocketAddr, message: &Message) {
        let Some(pending) = &self.pending else { return };
        if pending.transaction_id != message.transaction_id.0 {
            return;
        }
        let selected_peer = self
            .selected
            .and_then(|i| self.remotes[self.pairs[i].remote].addr());
        if selected_peer != Some(peer) {
            debug!(target: "ice.agent", "{} consent answer from {peer} is not the pair's", self.name);
            return;
        }
        self.pending = None;
        self.last_received = Some(now);
        if stun::class(message) == Class::Success {
            self.consented_at = Some(now);
        }
        self.after_consent_transaction(now);
    }

    fn after_consent_transaction(&mut self, now: Instant) {
        let Some(consented_at) = self.consented_at else {
            return;
        };
        if now.saturating_duration_since(consented_at) >= self.timings.consent_timeout {
            info!(target: "ice", "{} Consent to send expired", self.name);
            self.fail();
            return;
        }
        self.next_consent_at = Some(now + self.jittered_interval());
    }

    fn jittered_interval(&self) -> Duration {
        let factor = 0.8 + 0.4 * rand::rng().random::<f64>();
        self.timings.consent_interval.mul_f64(factor)
    }

    /// Advance the clock: the inbound-check bound and the consent schedule.
    pub fn handle_timeout(&mut self, now: Instant) {
        if matches!(self.state, IceState::Failed | IceState::Closed) {
            return;
        }
        if self.selected.is_none() {
            if let Some(deadline) = self.inbound_deadline
                && now >= deadline
            {
                info!(
                    target: "ice",
                    "{} ICE failed: no check from the peer completed within {}s",
                    self.name,
                    self.timings.inbound_check_timeout.as_secs_f64()
                );
                self.inbound_deadline = None;
                self.fail();
            }
            return;
        }
        if let Some(pending) = &mut self.pending {
            if now >= pending.retry_at {
                if pending.tries > self.timings.consent_retransmissions {
                    debug!(target: "ice.agent", "{} consent check timed out", self.name);
                    self.pending = None;
                    self.after_consent_transaction(now);
                } else {
                    pending.tries += 1;
                    pending.retry_at = now + pending.delay;
                    pending.delay *= 2;
                    let (message, pair) = (pending.message.clone(), self.selected);
                    if let Some(pair) = pair {
                        let (local, remote) = (self.pairs[pair].local, self.pairs[pair].remote);
                        if let Some(peer) = self.remotes[remote].addr() {
                            self.send(local, peer, message);
                        }
                    }
                }
            }
        } else if let Some(at) = self.next_consent_at
            && now >= at
        {
            self.send_consent_check(now);
        }
    }

    fn send_consent_check(&mut self, now: Instant) {
        let (Some(pair), Some((remote_ufrag, remote_pwd))) = (self.selected, &self.remote) else {
            return;
        };
        let (local, remote) = (self.pairs[pair].local, self.pairs[pair].remote);
        let Some(peer) = self.remotes[remote].addr() else {
            return;
        };
        let priority = candidate_priority(
            self.locals[local].candidate.component,
            CandidateType::Prflx,
            65535,
        );
        let request = match stun::binding_request(
            remote_ufrag,
            &self.local_ufrag,
            priority,
            Role::Controlled(self.tie_breaker),
            false,
            remote_pwd,
        ) {
            Ok(m) => m,
            Err(e) => {
                debug!(target: "ice.agent", "{} cannot build consent check: {e}", self.name);
                return;
            }
        };
        self.next_consent_at = None;
        self.pending = Some(Pending {
            transaction_id: request.transaction_id.0,
            message: request.raw.clone(),
            tries: 1,
            retry_at: now + RETRY_RTO,
            delay: RETRY_RTO * 2,
        });
        self.send(local, peer, request.raw);
    }

    /// When `handle_timeout` is next due.
    pub fn poll_timeout(&self) -> Option<Instant> {
        if matches!(self.state, IceState::Failed | IceState::Closed) {
            return None;
        }
        if self.selected.is_none() {
            return self.inbound_deadline;
        }
        match &self.pending {
            Some(pending) => Some(pending.retry_at),
            None => self.next_consent_at,
        }
    }

    /// An ICE-TCP stream from `peer` closed; when the selected pair rode it the connection
    /// ends with it instead of waiting for consent to expire.
    pub fn peer_stream_lost(&mut self, _now: Instant, peer: SocketAddr) {
        let selected_peer = self
            .selected
            .and_then(|i| self.remotes[self.pairs[i].remote].addr());
        if selected_peer == Some(peer) {
            info!(target: "ice", "{} Stream to {peer} lost, connection over", self.name);
            self.fail();
        }
    }

    /// The next datagram to send.
    pub fn poll_write(&mut self) -> Option<Outbound> {
        self.writes.pop_front()
    }

    /// The next event.
    pub fn poll_event(&mut self) -> Option<AgentEvent> {
        self.events.pop_front()
    }

    /// End the agent.
    pub fn close(&mut self) {
        self.pending = None;
        self.next_consent_at = None;
        self.inbound_deadline = None;
        self.set_state(IceState::Closed);
    }

    /// End the connection as failed.
    pub fn fail(&mut self) {
        self.pending = None;
        self.next_consent_at = None;
        self.set_state(IceState::Failed);
    }

    fn set_state(&mut self, state: IceState) {
        if self.state != state {
            debug!(target: "ice.agent", "{} {} -> {}", self.name, self.state.as_str(), state.as_str());
            self.state = state;
            self.events.push_back(AgentEvent::State(state));
        }
    }

    fn send(&mut self, local_index: usize, peer: SocketAddr, data: Vec<u8>) {
        let local = &self.locals[local_index];
        self.writes.push_back(Outbound {
            local: local.bound,
            peer,
            protocol: local.protocol,
            data,
        });
    }

    fn send_error(
        &mut self,
        local_index: usize,
        peer: SocketAddr,
        request: &Message,
        error: (u16, &str),
    ) {
        match stun::binding_error(request, error.0, error.1, &self.local_pwd) {
            Ok(data) => self.send(local_index, peer, data),
            Err(e) => {
                debug!(target: "ice.agent", "{} cannot answer check from {peer}: {e}", self.name)
            }
        }
    }
}
