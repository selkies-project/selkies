//! The Sans-I/O ICE agent.
//!
//! An [`Agent`] is given local and remote candidates and inbound datagrams; it produces the
//! connectivity checks to send, the state transitions to report, and eventually a selected
//! candidate pair. It owns no sockets and no clock — the caller drives time with
//! `handle_timeout`.
//!
//! Two roles exist: the controlling agent nominates the pair that will carry media, the
//! controlled agent accepts that choice. Which side controls is decided by which offered, and the
//! two must not agree — see [`Credentials`] and [`Agent::set_role`].
#[cfg(test)]
mod agent_test;

/// Configuration for a new [`Agent`]: servers, timeouts and role.
pub mod agent_config;
mod agent_proto;
/// Pair selection and nomination — which candidate pair becomes the selected one.
pub mod agent_selector;
/// Snapshot statistics for an agent's candidates and pairs.
pub mod agent_stats;

use agent_config::*;
use bytes::BytesMut;
use crypto::{RTCCrypto, RTCCryptoProvider};
use log::{debug, error, info, trace, warn};
use mdns::{Mdns, QueryId};
use sansio::Protocol;
use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use stun::attributes::*;
use stun::fingerprint::*;
use stun::integrity::*;
use stun::message::*;
use stun::textattrs::*;
use stun::xoraddr::*;

use crate::candidate::candidate_peer_reflexive::CandidatePeerReflexiveConfig;
use crate::candidate::{candidate_pair::*, *};
use crate::mdns::{MulticastDnsMode, create_multicast_dns, generate_multicast_dns_name};
use crate::network_type::NetworkType;
use crate::rand::*;
use crate::state::*;
use crate::tcp_type::TcpType;
use crate::url::*;
use shared::error::*;
use shared::{TaggedBytesMut, TransportContext, TransportProtocol};

const ZERO_DURATION: Duration = Duration::from_secs(0);

#[derive(Debug, Clone)]
pub(crate) struct BindingRequest {
    pub(crate) timestamp: Instant,
    pub(crate) transaction_id: TransactionId,
    pub(crate) destination: SocketAddr,
    pub(crate) is_use_candidate: bool,
}

#[derive(Default, Clone)]
/// The ICE credentials for one side of a session, exchanged in SDP.
pub struct Credentials {
    /// The username fragment, echoed in every STUN check so a receiver can demultiplex.
    pub ufrag: String,
    /// The password, used as the `MESSAGE-INTEGRITY` key.
    pub pwd: String,
}

#[derive(Default, Clone)]
pub(crate) struct UfragPwd {
    pub(crate) local_credentials: Credentials,
    pub(crate) remote_credentials: Option<Credentials>,
    /// Credentials generated for an ICE restart that has not been applied yet.
    ///
    /// JSEP requires `createOffer` to be free of side effects, so the offer advertises these
    /// while the live session keeps authenticating with `local_credentials`. They are installed
    /// by [`Agent::apply_restart`] when the local description is set. If the offer is discarded,
    /// they are simply dropped and the existing session is undisturbed.
    pub(crate) pending_local_credentials: Option<Credentials>,
}

fn assert_inbound_username(m: &Message, expected_username: &str) -> Result<()> {
    let mut username = Username::new(ATTR_USERNAME, String::new());
    username.get_from(m)?;

    if username.to_string() != expected_username {
        return Err(Error::Other(format!(
            "{:?} expected({}) actual({})",
            Error::ErrMismatchUsername,
            expected_username,
            username,
        )));
    }

    Ok(())
}

fn assert_inbound_message_integrity(
    m: &mut Message,
    key: &[u8],
    crypto: &dyn RTCCrypto,
) -> Result<()> {
    MessageIntegrity::check(m, key, crypto)
}

/// What the agent reports to its caller.
#[non_exhaustive]
pub enum Event {
    /// The agent's connection state changed.
    ConnectionStateChange(ConnectionState),
    /// A new pair was selected, carrying the local and remote candidates.
    ///
    /// Media should be sent on this pair from now on.
    SelectedCandidatePairChange(Box<Candidate>, Box<Candidate>),
    /// Emitted when the ICE role switches due to a role conflict (RFC 8445 §7.3.1.1).
    /// The bool is `true` if the agent is now controlling, `false` if now controlled.
    RoleChange(bool),
}

/// An [`Event`] together with the instant its condition was observed at.
///
/// The agent's events are produced inside `handle_read` / `handle_timeout` / `close`, all of
/// which know the time, but they are consumed from `poll_event`, which does not. Carrying the
/// instant in the payload means the consumer is *told* when the condition happened rather than
/// having to retain an instant of its own and guess — the same channel `TaggedBytesMut` already
/// provides on the read path.
pub struct TaggedEvent {
    /// When the condition this event reports was observed.
    pub now: Instant,
    /// The event itself.
    pub event: Event,
}

/// Represents the ICE agent.
pub struct Agent {
    pub(crate) crypto_provider: Arc<dyn RTCCryptoProvider>,
    pub(crate) tie_breaker: u64,
    pub(crate) is_controlling: bool,
    pub(crate) lite: bool,

    pub(crate) start_time: Instant,

    pub(crate) connection_state: ConnectionState,
    pub(crate) last_connection_state: ConnectionState,

    //pub(crate) started_ch_tx: Mutex<Option<broadcast::Sender<()>>>,
    pub(crate) ufrag_pwd: UfragPwd,

    pub(crate) local_candidates: Vec<Candidate>,
    pub(crate) remote_candidates: Vec<Candidate>,
    pub(crate) candidate_pairs: Vec<CandidatePair>,
    pub(crate) nominated_pair: Option<usize>,
    pub(crate) selected_pair: Option<usize>,

    // LRU of outbound Binding request Transaction IDs
    pub(crate) pending_binding_requests: Vec<BindingRequest>,

    // the following variables won't be changed after init_with_defaults()
    pub(crate) insecure_skip_verify: bool,
    pub(crate) max_binding_requests: u16,
    pub(crate) host_acceptance_min_wait: Duration,
    pub(crate) srflx_acceptance_min_wait: Duration,
    pub(crate) prflx_acceptance_min_wait: Duration,
    pub(crate) relay_acceptance_min_wait: Duration,
    // How long connectivity checks can fail before the ICE Agent
    // goes to disconnected
    pub(crate) disconnected_timeout: Duration,
    // How long connectivity checks can fail before the ICE Agent
    // goes to failed
    pub(crate) failed_timeout: Duration,
    // How often should we send keepalive packets?
    // 0 means never
    pub(crate) keepalive_interval: Duration,
    // When the last STUN consent ping was sent.
    pub(crate) last_consent_sent: Instant,
    // How often should we run our internal taskLoop to check for state changes when connecting
    pub(crate) check_interval: Duration,
    pub(crate) checking_duration: Instant,
    pub(crate) last_checking_time: Instant,
    // When set, a connectivity check has been requested and must be run on the next
    // `handle_timeout`. Checks are deferred (rather than run inline) so that adding a
    // candidate can never re-enter `contact()` and mutate agent state - e.g. transition
    // to `Failed` and drop candidates - while a caller still holds indices into the
    // candidate vectors. See issue #88.
    pub(crate) force_candidate_contact: bool,

    pub(crate) mdns: Option<Mdns>,
    pub(crate) mdns_queries: HashMap<QueryId, Candidate>,

    pub(crate) mdns_mode: MulticastDnsMode,
    pub(crate) mdns_local_name: String,
    pub(crate) mdns_local_ip: Option<IpAddr>,

    pub(crate) candidate_types: Vec<CandidateType>,
    pub(crate) network_types: Vec<NetworkType>,
    pub(crate) urls: Vec<Url>,

    pub(crate) write_outs: VecDeque<TaggedBytesMut>,
    pub(crate) event_outs: VecDeque<TaggedEvent>,
}

impl Agent {
    /// Creates a new Agent.
    ///
    /// The crypto provider is supplied by the caller; this crate never resolves a default.
    /// Builds an agent whose clock starts at `now`.
    ///
    /// `now` is a constructor argument rather than an `AgentConfig` field because it is a value
    /// consumed once at construction, not a setting that persists for the agent's lifetime — and
    /// because `Instant` has no `Default`, so a config field would force `AgentConfig` to drop
    /// `#[derive(Default)]` and break every `..Default::default()` in its 31 call sites.
    pub fn new(
        now: Instant,
        config: Arc<AgentConfig>,
        crypto_provider: Arc<dyn RTCCryptoProvider>,
    ) -> Result<Self> {
        let tie_breaker = generate_tie_breaker(crypto_provider.random())?;

        let mut mdns_local_name = config.multicast_dns_local_name.clone();
        if mdns_local_name.is_empty() {
            mdns_local_name = generate_multicast_dns_name();
        }

        if !mdns_local_name.ends_with(".local") || mdns_local_name.split('.').count() != 2 {
            return Err(Error::ErrInvalidMulticastDnshostName);
        }

        let mdns_mode = config.multicast_dns_mode;
        let mdns = create_multicast_dns(
            now,
            mdns_mode,
            &mdns_local_name,
            &config.multicast_dns_local_ip,
            &config.multicast_dns_query_timeout,
        )
        .unwrap_or_else(|err| {
            // Opportunistic mDNS: If we can't open the connection, that's ok: we
            // can continue without it.
            warn!("Failed to initialize mDNS {mdns_local_name}: {err}");
            None
        });

        let candidate_types = if config.candidate_types.is_empty() {
            default_candidate_types()
        } else {
            config.candidate_types.clone()
        };

        if config.lite && (candidate_types.len() != 1 || candidate_types[0] != CandidateType::Host)
        {
            return Err(Error::ErrLiteUsingNonHostCandidates);
        }

        if !config.urls.is_empty()
            && !contains_candidate_type(CandidateType::ServerReflexive, &candidate_types)
            && !contains_candidate_type(CandidateType::Relay, &candidate_types)
        {
            return Err(Error::ErrUselessUrlsProvided);
        }

        let mut agent = Self {
            crypto_provider,
            tie_breaker,
            is_controlling: config.is_controlling,
            lite: config.lite,

            start_time: now,

            nominated_pair: None,
            selected_pair: None,
            candidate_pairs: vec![],

            connection_state: ConnectionState::New,

            insecure_skip_verify: config.insecure_skip_verify,

            //started_ch_tx: MuteSome(started_ch_tx)),

            //won't change after init_with_defaults()
            max_binding_requests: if let Some(max_binding_requests) = config.max_binding_requests {
                max_binding_requests
            } else {
                DEFAULT_MAX_BINDING_REQUESTS
            },
            host_acceptance_min_wait: if let Some(host_acceptance_min_wait) =
                config.host_acceptance_min_wait
            {
                host_acceptance_min_wait
            } else {
                DEFAULT_HOST_ACCEPTANCE_MIN_WAIT
            },
            srflx_acceptance_min_wait: if let Some(srflx_acceptance_min_wait) =
                config.srflx_acceptance_min_wait
            {
                srflx_acceptance_min_wait
            } else {
                DEFAULT_SRFLX_ACCEPTANCE_MIN_WAIT
            },
            prflx_acceptance_min_wait: if let Some(prflx_acceptance_min_wait) =
                config.prflx_acceptance_min_wait
            {
                prflx_acceptance_min_wait
            } else {
                DEFAULT_PRFLX_ACCEPTANCE_MIN_WAIT
            },
            relay_acceptance_min_wait: if let Some(relay_acceptance_min_wait) =
                config.relay_acceptance_min_wait
            {
                relay_acceptance_min_wait
            } else {
                DEFAULT_RELAY_ACCEPTANCE_MIN_WAIT
            },

            // How long connectivity checks can fail before the ICE Agent
            // goes to disconnected
            disconnected_timeout: if let Some(disconnected_timeout) = config.disconnected_timeout {
                disconnected_timeout
            } else {
                DEFAULT_DISCONNECTED_TIMEOUT
            },

            // How long connectivity checks can fail before the ICE Agent
            // goes to failed
            failed_timeout: if let Some(failed_timeout) = config.failed_timeout {
                failed_timeout
            } else {
                DEFAULT_FAILED_TIMEOUT
            },

            // How often should we send keepalive packets?
            // 0 means never
            keepalive_interval: if let Some(keepalive_interval) = config.keepalive_interval {
                keepalive_interval
            } else {
                DEFAULT_KEEPALIVE_INTERVAL
            },

            // How often should we run our internal taskLoop to check for state changes when connecting
            check_interval: if config.check_interval == Duration::from_secs(0) {
                DEFAULT_CHECK_INTERVAL
            } else {
                config.check_interval
            },
            last_consent_sent: now,
            checking_duration: now,
            last_checking_time: now,
            force_candidate_contact: false,
            last_connection_state: ConnectionState::Unspecified,

            mdns,
            mdns_queries: HashMap::new(),

            mdns_mode,
            mdns_local_name,
            mdns_local_ip: config.multicast_dns_local_ip,

            ufrag_pwd: UfragPwd::default(),

            local_candidates: vec![],
            remote_candidates: vec![],

            // LRU of outbound Binding request Transaction IDs
            pending_binding_requests: vec![],

            candidate_types,
            network_types: config.network_types.clone(),
            urls: config.urls.clone(),

            write_outs: VecDeque::new(),
            event_outs: VecDeque::new(),
        };

        // Restart is also used to initialize the agent for the first time
        if let Err(err) = agent.restart(
            now,
            config.local_ufrag.clone(),
            config.local_pwd.clone(),
            false,
        ) {
            let _ = agent.close();
            return Err(err);
        }

        Ok(agent)
    }

    /// Adds a new local candidate.
    pub fn add_local_candidate(&mut self, mut c: Candidate) -> Result<bool> {
        // Filter by network type if network_types is configured
        if !self.network_types.is_empty() {
            let candidate_network_type = c.network_type();
            if !self.network_types.contains(&candidate_network_type) {
                debug!(
                    "Ignoring local candidate with network type {:?} (not in configured network types: {:?})",
                    candidate_network_type, self.network_types
                );
                return Ok(false);
            }
        }

        // Filter by candidate type if candidate_types is configured.
        let candidate_type = c.candidate_type();
        if !self.candidate_types.is_empty() && !self.candidate_types.contains(&candidate_type) {
            debug!(
                "Ignoring local candidate with type {:?} (not in configured candidate types: {:?})",
                candidate_type, self.candidate_types
            );
            return Ok(false);
        }

        if c.candidate_type() == CandidateType::Host
            && self.mdns_mode == MulticastDnsMode::QueryAndGather
            && c.network_type == NetworkType::Udp4
            && self
                .mdns_local_ip
                .is_some_and(|local_ip| local_ip == c.addr().ip())
        {
            // only one .local mDNS host candidate per IPv4 is supported
            // when registered local ip matches, use mdns_local_name to hide local host ip
            trace!(
                "mDNS hides local ip {} with local name {}",
                c.address, self.mdns_local_name
            );
            c.address = self.mdns_local_name.clone();
        }

        for cand in &self.local_candidates {
            if cand.equal(&c) {
                return Ok(false);
            }
        }

        self.local_candidates.push(c);
        let local_index = self.local_candidates.len() - 1;

        for remote_index in 0..self.remote_candidates.len() {
            if self.local_candidates[local_index]
                .can_pair_with(&self.remote_candidates[remote_index])
            {
                self.add_pair(local_index, remote_index);
            }
        }

        self.request_connectivity_check();

        Ok(true)
    }

    /// Adds a new remote candidate.
    pub fn add_remote_candidate(&mut self, c: Candidate) -> Result<bool> {
        // Filter by network type if network_types is configured
        if !self.network_types.is_empty() {
            let candidate_network_type = c.network_type();
            if !self.network_types.contains(&candidate_network_type) {
                debug!(
                    "Ignoring remote candidate with network type {:?} (not in configured network types: {:?})",
                    candidate_network_type, self.network_types
                );
                return Ok(false);
            }
        }

        // TCP active candidates don't have a listening port - they initiate connections.
        // The remote active side will probe our passive candidates, so we don't need
        // to do anything with remote active candidates.
        if c.tcp_type() == TcpType::Active {
            debug!(
                "Ignoring remote candidate with tcptype active: {}",
                c.address()
            );
            return Ok(false);
        }

        // If we have a mDNS Candidate lets fully resolve it before adding it locally
        if c.candidate_type() == CandidateType::Host && c.address().ends_with(".local") {
            if self.mdns_mode == MulticastDnsMode::Disabled {
                warn!(
                    "remote mDNS candidate added, but mDNS is disabled: ({})",
                    c.address()
                );
                return Ok(false);
            }

            if c.candidate_type() != CandidateType::Host {
                return Err(Error::ErrAddressParseFailed);
            }

            if let Some(mdns_conn) = &mut self.mdns {
                let query_id = mdns_conn.schedule_query(c.address());
                self.mdns_queries.insert(query_id, c);
            }

            return Ok(false);
        }

        self.trigger_request_connectivity_check(vec![c]);
        Ok(true)
    }

    fn trigger_request_connectivity_check(&mut self, remote_candidates: Vec<Candidate>) {
        for c in remote_candidates {
            if !self.remote_candidates.iter().any(|cand| cand.equal(&c)) {
                self.remote_candidates.push(c);
                let remote_index = self.remote_candidates.len() - 1;

                for local_index in 0..self.local_candidates.len() {
                    if self.local_candidates[local_index]
                        .can_pair_with(&self.remote_candidates[remote_index])
                    {
                        self.add_pair(local_index, remote_index);
                    }
                }

                self.request_connectivity_check();
            }
        }
    }

    /// Sets the credentials of the remote agent.
    pub fn set_remote_credentials(
        &mut self,
        remote_ufrag: String,
        remote_pwd: String,
    ) -> Result<()> {
        if remote_ufrag.is_empty() {
            return Err(Error::ErrRemoteUfragEmpty);
        } else if remote_pwd.is_empty() {
            return Err(Error::ErrRemotePwdEmpty);
        }

        self.ufrag_pwd.remote_credentials = Some(Credentials {
            ufrag: remote_ufrag,
            pwd: remote_pwd,
        });

        Ok(())
    }

    /// Returns the remote credentials.
    pub fn get_remote_credentials(&self) -> Option<&Credentials> {
        self.ufrag_pwd.remote_credentials.as_ref()
    }

    /// Returns the local credentials.
    /// The credentials to advertise, which is what SDP generation wants.
    ///
    /// Returns credentials staged by [`Agent::generate_restart_credentials`] when an ICE restart
    /// is pending, so an offer carries the new ufrag/pwd. Inbound STUN validation deliberately
    /// does *not* go through here — it reads `local_credentials` directly, so the live session
    /// keeps working until [`Agent::apply_restart`] installs the new pair.
    pub fn get_local_credentials(&self) -> &Credentials {
        self.ufrag_pwd
            .pending_local_credentials
            .as_ref()
            .unwrap_or(&self.ufrag_pwd.local_credentials)
    }

    /// Whether this agent is the controlling one, which decides nomination.
    pub fn role(&self) -> bool {
        self.is_controlling
    }

    /// Sets the controlling role.
    ///
    /// Determined by which side offered; the two agents must not agree, or nomination stalls.
    pub fn set_role(&mut self, is_controlling: bool) {
        self.is_controlling = is_controlling;
    }

    /// The agent's current connection state.
    pub fn state(&self) -> ConnectionState {
        self.connection_state
    }

    /// Whether a non-STUN datagram on `transport` should be accepted as media.
    ///
    /// Guards against accepting media from an address that has not passed a connectivity check.
    pub fn is_valid_non_stun_traffic(&mut self, now: Instant, transport: TransportContext) -> bool {
        self.find_local_candidate(transport.local_addr, transport.transport_protocol)
            .is_some()
            && self.validate_non_stun_traffic(now, transport.peer_addr)
    }

    fn get_timeout_interval(&self) -> Duration {
        let (check_interval, keepalive_interval, disconnected_timeout, failed_timeout) = (
            self.check_interval,
            self.keepalive_interval,
            self.disconnected_timeout,
            self.failed_timeout,
        );
        let mut interval = DEFAULT_CHECK_INTERVAL;

        let mut update_interval = |x: Duration| {
            if x != ZERO_DURATION && (interval == ZERO_DURATION || interval > x) {
                interval = x;
            }
        };

        match self.last_connection_state {
            ConnectionState::New | ConnectionState::Checking => {
                // While connecting, check candidates more frequently
                update_interval(check_interval);
            }
            ConnectionState::Connected | ConnectionState::Disconnected => {
                update_interval(keepalive_interval);
            }
            _ => {}
        };
        // Ensure we run our task loop as quickly as the minimum of our various configured timeouts
        update_interval(disconnected_timeout);
        update_interval(failed_timeout);
        interval
    }

    /// Returns the selected pair (local_candidate, remote_candidate) or none
    pub fn get_selected_candidate_pair(&self) -> Option<(&Candidate, &Candidate)> {
        if let Some(pair_index) = self.get_selected_pair() {
            let candidate_pair = &self.candidate_pairs[pair_index];
            Some((
                &self.local_candidates[candidate_pair.local_index],
                &self.remote_candidates[candidate_pair.remote_index],
            ))
        } else {
            None
        }
    }

    /// The highest-priority pair that is still usable, whether or not it has been nominated.
    pub fn get_best_available_candidate_pair(&self) -> Option<(&Candidate, &Candidate)> {
        if let Some(pair_index) = self.get_best_available_pair() {
            let candidate_pair = &self.candidate_pairs[pair_index];
            Some((
                &self.local_candidates[candidate_pair.local_index],
                &self.remote_candidates[candidate_pair.remote_index],
            ))
        } else {
            None
        }
    }

    /// start connectivity checks
    pub fn start_connectivity_checks(
        &mut self,
        now: Instant,
        is_controlling: bool,
        remote_ufrag: String,
        remote_pwd: String,
    ) -> Result<()> {
        debug!(
            "Started agent: isControlling? {}, remoteUfrag: {}, remotePwd: {}",
            is_controlling, remote_ufrag, remote_pwd
        );
        self.set_remote_credentials(remote_ufrag, remote_pwd)?;
        self.is_controlling = is_controlling;
        self.start(now);

        self.update_connection_state(Some(now), ConnectionState::Checking);
        self.request_connectivity_check();

        Ok(())
    }

    /// Restarts the ICE Agent with the provided ufrag/pwd
    /// If no ufrag/pwd is provided the Agent will generate one itself.
    /// Stages new local credentials for an ICE restart, without disturbing the live session.
    ///
    /// This is the half of an ICE restart that an offerer needs *before* generating SDP: the offer
    /// must carry the new ufrag/pwd. It deliberately does not touch `local_credentials`,
    /// candidate pairs, or connection state, because JSEP requires `createOffer` to be free of
    /// side effects — and because inbound STUN is still being validated against the current
    /// credentials until the local description is applied.
    ///
    /// Empty `ufrag` / `pwd` are filled from the crypto provider's RNG. Call
    /// [`Agent::apply_restart`] to install them.
    pub fn generate_restart_credentials(
        &mut self,
        mut ufrag: String,
        mut pwd: String,
    ) -> Result<()> {
        if ufrag.is_empty() {
            ufrag = generate_ufrag_with_random(self.crypto_provider.random())?;
        }
        if pwd.is_empty() {
            pwd = generate_pwd_with_random(self.crypto_provider.random())?;
        }

        if ufrag.len() * 8 < 24 {
            return Err(Error::ErrLocalUfragInsufficientBits);
        }
        if pwd.len() * 8 < 128 {
            return Err(Error::ErrLocalPwdInsufficientBits);
        }

        self.ufrag_pwd.pending_local_credentials = Some(Credentials { ufrag, pwd });

        Ok(())
    }

    /// Whether [`Agent::generate_restart_credentials`] has staged a restart that is not yet applied.
    pub fn has_pending_restart(&self) -> bool {
        self.ufrag_pwd.pending_local_credentials.is_some()
    }

    /// Applies a staged ICE restart, tearing down the old session and starting a new one at `now`.
    ///
    /// Installs the credentials staged by [`Agent::generate_restart_credentials`], if any, then
    /// discards the remote credentials, pending binding requests, candidate pairs and selected
    /// pair, and restarts the agent's timers. With nothing staged, the current credentials are
    /// kept and only the session is restarted.
    pub fn apply_restart(&mut self, now: Instant, keep_local_candidates: bool) -> Result<()> {
        if let Some(credentials) = self.ufrag_pwd.pending_local_credentials.take() {
            self.ufrag_pwd.local_credentials = credentials;
        }
        self.ufrag_pwd.remote_credentials = None;

        self.pending_binding_requests = vec![];

        self.candidate_pairs = vec![];

        self.set_selected_pair(Some(now), None);
        self.delete_all_candidates(keep_local_candidates);
        self.start(now);

        // Restart is used by NewAgent. Accept/Connect should be used to move to checking
        // for new Agents
        if self.connection_state != ConnectionState::New {
            self.update_connection_state(Some(now), ConnectionState::Checking);
        }

        Ok(())
    }

    /// Generates and immediately applies an ICE restart.
    ///
    /// Equivalent to [`Agent::generate_restart_credentials`] followed by
    /// [`Agent::apply_restart`]. A JSEP-conformant offerer wants the two halves separately —
    /// credentials at `createOffer`, application at `setLocalDescription` — but this remains the
    /// right call for anything that restarts in one step.
    pub fn restart(
        &mut self,
        now: Instant,
        ufrag: String,
        pwd: String,
        keep_local_candidates: bool,
    ) -> Result<()> {
        self.generate_restart_credentials(ufrag, pwd)?;
        self.apply_restart(now, keep_local_candidates)
    }

    /// Returns the local candidates.
    pub fn get_local_candidates(&self) -> &[Candidate] {
        &self.local_candidates
    }

    /// The remote candidates this agent has been given, in the order they were added.
    pub fn get_remote_candidates(&self) -> &[Candidate] {
        &self.remote_candidates
    }

    fn contact(&mut self, now: Instant) {
        // Consume any pending deferred-check request now that we are running one.
        // Reset before the early returns below so a failed/settled agent does not
        // keep asking `poll_timeout` for an immediate wake-up.
        self.force_candidate_contact = false;

        if self.connection_state == ConnectionState::Failed {
            // The connection is currently failed so don't send any checks
            // In the future it may be restarted though
            self.last_connection_state = self.connection_state;
            return;
        }
        if self.connection_state == ConnectionState::Checking {
            // We have just entered checking for the first time so update our checking timer
            if self.last_connection_state != self.connection_state {
                self.checking_duration = now;
            }

            // We have been in checking longer then Disconnect+Failed timeout, set the connection to Failed
            if self.failed_timeout != ZERO_DURATION
                && now
                    .checked_duration_since(self.checking_duration)
                    .unwrap_or_else(|| Duration::from_secs(0))
                    > self.disconnected_timeout + self.failed_timeout
            {
                self.update_connection_state(Some(now), ConnectionState::Failed);
                self.last_connection_state = self.connection_state;
                return;
            }
        }

        self.contact_candidates(now);

        self.last_connection_state = self.connection_state;
        self.last_checking_time = now;
    }

    pub(crate) fn update_connection_state(
        &mut self,
        now: Option<Instant>,
        new_state: ConnectionState,
    ) {
        if self.connection_state != new_state {
            // Connection has gone to failed, release all gathered candidates
            if new_state == ConnectionState::Failed {
                self.set_selected_pair(now, None);
                self.delete_all_candidates(false);
            }

            info!(
                "[{}]: Setting new connection state: {}",
                self.get_name(),
                new_state
            );
            self.connection_state = new_state;
            if let Some(now) = now {
                self.event_outs.push_back(TaggedEvent {
                    now,
                    event: Event::ConnectionStateChange(new_state),
                });
            }
        }
    }

    pub(crate) fn set_selected_pair(&mut self, now: Option<Instant>, selected_pair: Option<usize>) {
        if let Some(pair_index) = selected_pair {
            trace!(
                "[{}]: Set selected candidate pair: {:?}",
                self.get_name(),
                self.candidate_pairs[pair_index]
            );

            self.candidate_pairs[pair_index].nominated = true;
            self.selected_pair = Some(pair_index);

            self.update_connection_state(now, ConnectionState::Connected);

            // Notify when the selected pair changes
            let candidate_pair = &self.candidate_pairs[pair_index];
            if let Some(now) = now {
                self.event_outs.push_back(TaggedEvent {
                    now,
                    event: Event::SelectedCandidatePairChange(
                        Box::new(self.local_candidates[candidate_pair.local_index].clone()),
                        Box::new(self.remote_candidates[candidate_pair.remote_index].clone()),
                    ),
                });
            }
        } else {
            self.selected_pair = None;
        }
    }

    pub(crate) fn ping_all_candidates(&mut self, now: Instant) {
        let mut pairs: Vec<(usize, usize)> = vec![];

        let name = self.get_name().to_string();
        if self.candidate_pairs.is_empty() {
            warn!(
                "[{}]: pingAllCandidates called with no candidate pairs. Connection is not possible yet.",
                name,
            );
        }
        for p in &mut self.candidate_pairs {
            if p.state == CandidatePairState::Waiting {
                p.state = CandidatePairState::InProgress;
            } else if p.state != CandidatePairState::InProgress {
                continue;
            }

            if p.binding_request_count > self.max_binding_requests {
                trace!(
                    "[{}]: max requests reached for pair {} (local_addr {} <-> remote_addr {}), marking it as failed",
                    name,
                    *p,
                    self.local_candidates[p.local_index].addr(),
                    self.remote_candidates[p.remote_index].addr()
                );
                p.state = CandidatePairState::Failed;
            } else {
                p.binding_request_count += 1;
                let local = p.local_index;
                let remote = p.remote_index;
                pairs.push((local, remote));
            }
        }

        if !pairs.is_empty() {
            trace!(
                "[{}]: pinging all {} candidates",
                self.get_name(),
                pairs.len()
            );
        }

        for (local, remote) in pairs {
            self.ping_candidate(now, local, remote);
        }
    }

    pub(crate) fn add_pair(&mut self, local_index: usize, remote_index: usize) {
        let p = CandidatePair::new(
            local_index,
            remote_index,
            self.local_candidates[local_index].priority(),
            self.remote_candidates[remote_index].priority(),
            self.is_controlling,
        );
        self.candidate_pairs.push(p);
    }

    pub(crate) fn find_pair(&self, local_index: usize, remote_index: usize) -> Option<usize> {
        for (index, p) in self.candidate_pairs.iter().enumerate() {
            if p.local_index == local_index && p.remote_index == remote_index {
                return Some(index);
            }
        }
        None
    }

    /// Checks if the selected pair is (still) valid.
    /// Note: the caller should hold the agent lock.
    /// Re-evaluates the selected pair's liveness as of `now`.
    ///
    /// `now` comes from the caller so that this decision and the keepalive decision below are
    /// made against the same instant as the `contact(now)` that reached them.
    pub(crate) fn validate_selected_pair(&mut self, now: Instant) -> bool {
        let (valid, disconnected_time) = {
            self.selected_pair.as_ref().map_or_else(
                || (false, Duration::from_secs(0)),
                |&pair_index| {
                    let remote_index = self.candidate_pairs[pair_index].remote_index;

                    let last_received = self.remote_candidates[remote_index]
                        .last_received()
                        .unwrap_or(self.start_time);
                    let disconnected_time = now.saturating_duration_since(last_received);
                    (true, disconnected_time)
                },
            )
        };

        if valid {
            // Only allow transitions to fail if a.failedTimeout is non-zero
            let mut total_time_to_failure = self.failed_timeout;
            if total_time_to_failure != Duration::from_secs(0) {
                total_time_to_failure += self.disconnected_timeout;
            }

            if total_time_to_failure != Duration::from_secs(0)
                && disconnected_time > total_time_to_failure
            {
                self.update_connection_state(Some(now), ConnectionState::Failed);
            } else if self.disconnected_timeout != Duration::from_secs(0)
                && disconnected_time > self.disconnected_timeout
            {
                self.update_connection_state(Some(now), ConnectionState::Disconnected);
            } else {
                self.update_connection_state(Some(now), ConnectionState::Connected);
            }
        }

        valid
    }

    /// Sends STUN Binding Requests to the selected pair at `keepalive_interval` to
    /// maintain consent freshness (RFC 7675).
    pub(crate) fn check_keepalive(&mut self, now: Instant) {
        let (local_index, remote_index, pair_index) = {
            self.selected_pair
                .as_ref()
                .map_or((None, None, None), |&pair_index| {
                    let p = &self.candidate_pairs[pair_index];
                    (Some(p.local_index), Some(p.remote_index), Some(pair_index))
                })
        };

        if let (Some(local_index), Some(remote_index), Some(pair_index)) =
            (local_index, remote_index, pair_index)
            && self.keepalive_interval != Duration::from_secs(0)
            && now.saturating_duration_since(self.last_consent_sent) >= self.keepalive_interval
        {
            self.last_consent_sent = now;
            self.candidate_pairs[pair_index].on_consent_request_sent();
            self.ping_candidate(now, local_index, remote_index);
        }
    }

    fn request_connectivity_check(&mut self) {
        // Defer the connectivity check to `handle_timeout` instead of running it
        // inline. Running `contact()` here is re-entrant: it can advance the state
        // to `Failed`, which calls `delete_all_candidates()` and wipes the candidate
        // and pair vectors while a caller still holds indices into them (for example
        // `handle_inbound` while adding a peer-reflexive candidate). See issue #88.
        self.force_candidate_contact = true;
    }

    /// Remove all candidates.
    /// This closes any listening sockets and removes both the local and remote candidate lists.
    ///
    /// This is used for restarts, failures and on close.
    pub(crate) fn delete_all_candidates(&mut self, keep_local_candidates: bool) {
        if !keep_local_candidates {
            self.local_candidates.clear();
        }
        self.remote_candidates.clear();

        // Candidate pairs reference candidates by index, so once the candidates are
        // removed every pair - and the `selected_pair`/`nominated_pair` indices into
        // the pair list - is dangling and must be dropped to avoid out-of-bounds
        // access. `remote_candidates` is always cleared here, so no pair can remain
        // valid even when the local candidates are kept. See issue #88.
        self.candidate_pairs.clear();
        self.selected_pair = None;
        self.nominated_pair = None;
    }

    pub(crate) fn find_remote_candidate(&self, addr: SocketAddr) -> Option<usize> {
        for (index, c) in self.remote_candidates.iter().enumerate() {
            if c.addr() == addr {
                return Some(index);
            }
        }
        None
    }

    pub(crate) fn find_local_candidate(
        &self,
        addr: SocketAddr,
        transport_protocol: TransportProtocol,
    ) -> Option<usize> {
        for (index, c) in self.local_candidates.iter().enumerate() {
            if c.network_type().to_protocol() != transport_protocol {
                continue;
            }

            // For TCP active candidates, match by IP only (ignore port).
            // TCP active candidates use port 9 as placeholder in signaling,
            // but the actual connection uses an ephemeral port.
            if c.tcp_type() == TcpType::Active && transport_protocol == TransportProtocol::TCP {
                if c.addr().ip() == addr.ip() {
                    return Some(index);
                }
            } else if c.addr() == addr {
                return Some(index);
            } else if let Some(related_address) = c.related_address()
                && related_address.address == addr.ip().to_string()
                && related_address.port == addr.port()
            {
                return Some(index);
            }
        }
        None
    }

    pub(crate) fn send_binding_request(
        &mut self,
        now: Instant,
        m: &Message,
        local_index: usize,
        remote_index: usize,
    ) {
        trace!(
            "[{}]: ping STUN from {} to {}",
            self.get_name(),
            self.local_candidates[local_index],
            self.remote_candidates[remote_index],
        );

        self.invalidate_pending_binding_requests(now);

        self.pending_binding_requests.push(BindingRequest {
            timestamp: now,
            transaction_id: m.transaction_id,
            destination: self.remote_candidates[remote_index].addr(),
            is_use_candidate: m.contains(ATTR_USE_CANDIDATE),
        });

        // Track request sent on the candidate pair
        if let Some(pair_index) = self.find_pair(local_index, remote_index) {
            self.candidate_pairs[pair_index].on_request_sent();
        }

        self.send_stun(now, m, local_index, remote_index);
    }

    pub(crate) fn send_binding_success(
        &mut self,
        now: Instant,
        m: &Message,
        local_index: usize,
        remote_index: usize,
    ) {
        let addr = self.remote_candidates[remote_index].addr();
        let (ip, port) = (addr.ip(), addr.port());
        let local_pwd = self.ufrag_pwd.local_credentials.pwd.clone();

        let (out, result) = {
            let mut out = Message::new();
            let result = out.build(&[
                Box::new(m.clone()),
                Box::new(BINDING_SUCCESS),
                Box::new(XorMappedAddress { ip, port }),
                Box::new(MessageIntegrity::new_short_term_integrity_with_provider(
                    local_pwd,
                    self.crypto_provider.crypto(),
                )),
                Box::new(FINGERPRINT),
            ]);
            (out, result)
        };

        if let Err(err) = result {
            warn!(
                "[{}]: Failed to handle inbound ICE from: {} to: {} error: {}",
                self.get_name(),
                self.local_candidates[local_index],
                self.remote_candidates[remote_index],
                err
            );
        } else {
            // Track response sent on the candidate pair
            if let Some(pair_index) = self.find_pair(local_index, remote_index) {
                self.candidate_pairs[pair_index].on_response_sent();
            }
            self.send_stun(now, &out, local_index, remote_index);
        }
    }

    /// Sends a 487 (Role Conflict) error response.
    /// RFC 8445 Section 7.3.1.1
    pub(crate) fn send_role_conflict_error(
        &mut self,
        now: Instant,
        m: &Message,
        local_index: usize,
        remote_index: usize,
    ) {
        use stun::error_code::*;

        let local_pwd = self.ufrag_pwd.local_credentials.pwd.clone();

        let (out, result) = {
            let mut out = Message::new();
            let result = out.build(&[
                Box::new(m.clone()),
                Box::new(stun::message::BINDING_ERROR),
                Box::new(CODE_ROLE_CONFLICT),
                Box::new(MessageIntegrity::new_short_term_integrity_with_provider(
                    local_pwd,
                    self.crypto_provider.crypto(),
                )),
                Box::new(FINGERPRINT),
            ]);
            (out, result)
        };

        if let Err(err) = result {
            warn!(
                "[{}]: Failed to send role conflict error from: {} to: {} error: {}",
                self.get_name(),
                self.local_candidates[local_index],
                self.remote_candidates[remote_index],
                err
            );
        } else {
            debug!(
                "[{}]: Sent 487 Role Conflict error from {} to {}",
                self.get_name(),
                self.local_candidates[local_index],
                self.remote_candidates[remote_index]
            );
            self.send_stun(now, &out, local_index, remote_index);
        }
    }

    /// Switches the ICE agent role and recomputes all candidate pair priorities.
    /// RFC 8445 Section 7.3.1.1
    pub(crate) fn switch_role(&mut self, now: Instant) {
        self.is_controlling = !self.is_controlling;

        // Recompute priorities for all candidate pairs
        // The priority calculation depends on ice_role_controlling
        for pair in &mut self.candidate_pairs {
            pair.ice_role_controlling = self.is_controlling;
        }

        // Clear nominated pair when switching roles
        self.nominated_pair = None;

        info!(
            "[{}]: Role switched, recomputed {} candidate pair priorities",
            self.get_name(),
            self.candidate_pairs.len()
        );

        self.event_outs.push_back(TaggedEvent {
            now,
            event: Event::RoleChange(self.is_controlling),
        });
    }

    /// Removes pending binding requests that are over `maxBindingRequestTimeout` old Let HTO be the
    /// transaction timeout, which SHOULD be 2*RTT if RTT is known or 500 ms otherwise.
    ///
    /// reference: (IETF ref-8445)[https://tools.ietf.org/html/rfc8445#appendix-B.1].
    pub(crate) fn invalidate_pending_binding_requests(&mut self, filter_time: Instant) {
        let pending_binding_requests = &mut self.pending_binding_requests;
        let initial_size = pending_binding_requests.len();

        let mut temp = vec![];
        for binding_request in pending_binding_requests.drain(..) {
            if filter_time
                .checked_duration_since(binding_request.timestamp)
                .map(|duration| duration < MAX_BINDING_REQUEST_TIMEOUT)
                .unwrap_or(true)
            {
                temp.push(binding_request);
            }
        }

        *pending_binding_requests = temp;
        let bind_requests_remaining = pending_binding_requests.len();
        let bind_requests_removed = initial_size - bind_requests_remaining;
        if bind_requests_removed > 0 {
            trace!(
                "[{}]: Discarded {} binding requests because they expired, still {} remaining",
                self.get_name(),
                bind_requests_removed,
                bind_requests_remaining,
            );
        }
    }

    /// Assert that the passed `TransactionID` is in our `pendingBindingRequests` and returns the
    /// destination, If the bindingRequest was valid remove it from our pending cache.
    pub(crate) fn handle_inbound_binding_success(
        &mut self,
        now: Instant,
        id: TransactionId,
    ) -> Option<BindingRequest> {
        self.invalidate_pending_binding_requests(now);

        let pending_binding_requests = &mut self.pending_binding_requests;
        for i in 0..pending_binding_requests.len() {
            if pending_binding_requests[i].transaction_id == id {
                let valid_binding_request = pending_binding_requests.remove(i);
                return Some(valid_binding_request);
            }
        }
        None
    }

    /// Processes STUN traffic from a remote candidate.
    pub(crate) fn handle_inbound(
        &mut self,
        now: Instant,
        m: &mut Message,
        local_index: usize,
        remote_addr: SocketAddr,
    ) -> Result<()> {
        if m.typ.method != METHOD_BINDING
            || !(m.typ.class == CLASS_SUCCESS_RESPONSE
                || m.typ.class == CLASS_REQUEST
                || m.typ.class == CLASS_INDICATION)
        {
            trace!(
                "[{}]: unhandled STUN from {} to {} class({}) method({})",
                self.get_name(),
                remote_addr,
                self.local_candidates[local_index],
                m.typ.class,
                m.typ.method
            );
            return Err(Error::ErrUnhandledStunpacket);
        }

        // RFC 8445 Section 7.3.1.1 - Detecting and Repairing Role Conflicts
        if self.is_controlling {
            if m.contains(ATTR_ICE_CONTROLLING) {
                // Both agents are controlling - role conflict detected
                let mut remote_controlling = crate::attributes::control::AttrControlling::default();
                if let Err(err) = remote_controlling.get_from(m) {
                    warn!(
                        "[{}]: Failed to get remote ICE-CONTROLLING attribute: {}",
                        self.get_name(),
                        err
                    );
                    return Err(err);
                }

                debug!(
                    "[{}]: Role conflict detected (both controlling), local tiebreaker: {}, remote tiebreaker: {}",
                    self.get_name(),
                    self.tie_breaker,
                    remote_controlling.0
                );

                // Only process if this is a request (not a response)
                if m.typ.class == CLASS_REQUEST {
                    // Send 487 Role Conflict error
                    if let Some(remote_index) = self.find_remote_candidate(remote_addr) {
                        self.send_role_conflict_error(now, m, local_index, remote_index);
                    }

                    // Compare tiebreakers - if ours is smaller, we switch to controlled
                    if self.tie_breaker < remote_controlling.0 {
                        info!(
                            "[{}]: Switching from controlling to controlled due to role conflict (smaller tiebreaker)",
                            self.get_name()
                        );
                        self.switch_role(now);
                    }
                }
                // Continue processing the message after handling role conflict
            } else if m.contains(ATTR_USE_CANDIDATE) {
                debug!(
                    "[{}]: useCandidate && a.isControlling == true",
                    self.get_name(),
                );
                return Err(Error::ErrUnexpectedStunrequestMessage);
            }
        } else if m.contains(ATTR_ICE_CONTROLLED) {
            // Both agents are controlled - role conflict detected
            let mut remote_controlled = crate::attributes::control::AttrControlled::default();
            if let Err(err) = remote_controlled.get_from(m) {
                warn!(
                    "[{}]: Failed to get remote ICE-CONTROLLED attribute: {}",
                    self.get_name(),
                    err
                );
                return Err(err);
            }

            debug!(
                "[{}]: Role conflict detected (both controlled), local tiebreaker: {}, remote tiebreaker: {}",
                self.get_name(),
                self.tie_breaker,
                remote_controlled.0
            );

            // Only process if this is a request (not a response)
            if m.typ.class == CLASS_REQUEST {
                // Send 487 Role Conflict error
                if let Some(remote_index) = self.find_remote_candidate(remote_addr) {
                    self.send_role_conflict_error(now, m, local_index, remote_index);
                }

                // Compare tiebreakers - if ours is larger, we switch to controlling
                if self.tie_breaker > remote_controlled.0 {
                    info!(
                        "[{}]: Switching from controlled to controlling due to role conflict (larger tiebreaker)",
                        self.get_name()
                    );
                    self.switch_role(now);
                }
            }
            // Continue processing the message after handling role conflict
        }

        let Some(remote_credentials) = &self.ufrag_pwd.remote_credentials else {
            debug!(
                "[{}]: ufrag_pwd.remote_credentials.is_none",
                self.get_name(),
            );
            return Err(Error::ErrPasswordEmpty);
        };

        let mut remote_candidate_index = self.find_remote_candidate(remote_addr);
        if m.typ.class == CLASS_SUCCESS_RESPONSE {
            if let Err(err) = assert_inbound_message_integrity(
                m,
                remote_credentials.pwd.as_bytes(),
                self.crypto_provider.crypto(),
            ) {
                warn!(
                    "[{}]: discard message from ({}), {}",
                    self.get_name(),
                    remote_addr,
                    err
                );
                return Err(err);
            }

            if let Some(remote_index) = &remote_candidate_index {
                self.handle_success_response(now, m, local_index, *remote_index, remote_addr);
            } else {
                warn!(
                    "[{}]: discard success message from ({}), no such remote",
                    self.get_name(),
                    remote_addr
                );
                return Err(Error::ErrUnhandledStunpacket);
            }
        } else if m.typ.class == CLASS_REQUEST {
            {
                let username = self.ufrag_pwd.local_credentials.ufrag.clone()
                    + ":"
                    + remote_credentials.ufrag.as_str();
                if let Err(err) = assert_inbound_username(m, &username) {
                    warn!(
                        "[{}]: discard message from ({}), {}",
                        self.get_name(),
                        remote_addr,
                        err
                    );
                    return Err(err);
                } else if let Err(err) = assert_inbound_message_integrity(
                    m,
                    self.ufrag_pwd.local_credentials.pwd.as_bytes(),
                    self.crypto_provider.crypto(),
                ) {
                    warn!(
                        "[{}]: discard message from ({}), {}",
                        self.get_name(),
                        remote_addr,
                        err
                    );
                    return Err(err);
                }
            }

            if remote_candidate_index.is_none() {
                // Use the local candidate's network type for the peer-reflexive candidate
                let network_type = self.local_candidates[local_index].network_type();
                let (ip, port) = (remote_addr.ip(), remote_addr.port());

                let prflx_candidate_config = CandidatePeerReflexiveConfig {
                    base_config: CandidateConfig {
                        network: network_type.to_string(),
                        address: ip.to_string(),
                        port,
                        component: self.local_candidates[local_index].component(),
                        ..CandidateConfig::default()
                    },
                    rel_addr: "".to_owned(),
                    rel_port: 0,
                };

                match prflx_candidate_config.new_candidate_peer_reflexive() {
                    Ok(prflx_candidate) => {
                        if let Ok(added) = self.add_remote_candidate(prflx_candidate)
                            && added
                        {
                            // Look the candidate up by address rather than assuming it
                            // is the last element: `add_remote_candidate` may not have
                            // appended it (e.g. a duplicate), and this stays correct if
                            // the vector is ever mutated underneath us. See issue #88.
                            remote_candidate_index = self.find_remote_candidate(remote_addr);
                        }
                    }
                    Err(err) => {
                        error!(
                            "[{}]: Failed to create new remote prflx candidate ({})",
                            self.get_name(),
                            err
                        );
                        return Err(err);
                    }
                };

                debug!(
                    "[{}]: adding a new peer-reflexive candidate: {} ",
                    self.get_name(),
                    remote_addr
                );
            }

            trace!(
                "[{}]: inbound STUN (Request) from {} to {}",
                self.get_name(),
                remote_addr,
                self.local_candidates[local_index]
            );

            if let Some(remote_index) = &remote_candidate_index {
                self.handle_binding_request(now, m, local_index, *remote_index);
            }
        }

        if let Some(remote_index) = remote_candidate_index {
            self.remote_candidates[remote_index].seen(now, false);
        }

        Ok(())
    }

    // Processes non STUN traffic from a remote candidate, and returns true if it is an actual
    // remote candidate.
    pub(crate) fn validate_non_stun_traffic(
        &mut self,
        now: Instant,
        remote_addr: SocketAddr,
    ) -> bool {
        self.find_remote_candidate(remote_addr)
            .is_some_and(|remote_index| {
                self.remote_candidates[remote_index].seen(now, false);
                true
            })
    }

    pub(crate) fn send_stun(
        &mut self,
        now: Instant,
        msg: &Message,
        local_index: usize,
        remote_index: usize,
    ) {
        let peer_addr = self.remote_candidates[remote_index].addr();
        // RFC 8445 §6.1.2: checks for a (server/peer-)reflexive candidate must
        // be sent from its base, the bound local socket the candidate was
        // derived from; the mapped address is not a local socket.
        let local_addr = self.local_candidates[local_index].base_addr();
        let transport_protocol = if self.local_candidates[local_index].network_type().is_tcp() {
            TransportProtocol::TCP
        } else {
            TransportProtocol::UDP
        };

        self.write_outs.push_back(TaggedBytesMut {
            now,
            transport: TransportContext {
                local_addr,
                peer_addr,
                ecn: None,
                transport_protocol,
            },
            message: BytesMut::from(&msg.raw[..]),
        });

        self.local_candidates[local_index].seen(now, true);
    }

    fn handle_inbound_candidate_msg(
        &mut self,
        local_index: usize,
        msg: TaggedBytesMut,
    ) -> Result<()> {
        if is_stun_message(&msg.message) {
            let mut m = Message {
                raw: msg.message.to_vec(),
                ..Message::default()
            };

            if let Err(err) = m.decode() {
                warn!(
                    "[{}]: Failed to handle decode ICE from {} to {}: {}",
                    self.get_name(),
                    msg.transport.local_addr,
                    msg.transport.peer_addr,
                    err
                );
                Err(err)
            } else {
                self.handle_inbound(msg.now, &mut m, local_index, msg.transport.peer_addr)
            }
        } else {
            if !self.validate_non_stun_traffic(msg.now, msg.transport.peer_addr) {
                warn!(
                    "[{}]: Discarded message, not a valid remote candidate from {}",
                    self.get_name(),
                    msg.transport.peer_addr,
                );
            } else {
                warn!(
                    "[{}]: non-STUN traffic message from a valid remote candidate from {}",
                    self.get_name(),
                    msg.transport.peer_addr
                );
            }
            Err(Error::ErrNonStunmessage)
        }
    }

    pub(crate) fn get_name(&self) -> &str {
        if self.is_controlling {
            "controlling"
        } else {
            "controlled"
        }
    }

    pub(crate) fn get_selected_pair(&self) -> Option<usize> {
        self.selected_pair
    }

    pub(crate) fn get_best_available_pair(&self) -> Option<usize> {
        let mut best_pair_index: Option<usize> = None;

        for (index, p) in self.candidate_pairs.iter().enumerate() {
            if p.state == CandidatePairState::Failed {
                continue;
            }

            if let Some(pair_index) = &mut best_pair_index {
                let b = &self.candidate_pairs[*pair_index];
                if b.priority() < p.priority() {
                    *pair_index = index;
                }
            } else {
                best_pair_index = Some(index);
            }
        }

        best_pair_index
    }

    pub(crate) fn get_best_valid_candidate_pair(&self) -> Option<usize> {
        let mut best_pair_index: Option<usize> = None;

        for (index, p) in self.candidate_pairs.iter().enumerate() {
            if p.state != CandidatePairState::Succeeded {
                continue;
            }

            if let Some(pair_index) = &mut best_pair_index {
                let b = &self.candidate_pairs[*pair_index];
                if b.priority() < p.priority() {
                    *pair_index = index;
                }
            } else {
                best_pair_index = Some(index);
            }
        }

        best_pair_index
    }
}
