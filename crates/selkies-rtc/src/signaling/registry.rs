/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The signaling server: the peer registry, the session and room relay, and the gates.
//!
//! Every registry change happens under one lock that is never held across an await: a send is
//! a queue push (`socket`), so a slow socket serializes no handshake. `PeerRegistry::serve`
//! runs one connection from its hello to its removal; dropping that future ends the peer the
//! way the connection ending does.
//!
//! An identity collision resolves newest-wins, as in websockets mode where a new display
//! connection supersedes the old one: a page refresh reconnects before its old socket is
//! reaped and must not bounce off itself, so dead holders are reaped and a live one is closed
//! with 4001. The identities are the display's sole client without sharing, a player slot
//! with it (`-1` is the unassigned sentinel, exempt) or, with a master token, the page's
//! token, since every page opened without `#playerN` names slot 1 and the pages of a
//! collaboration room each hold their own; and a display's controller, unless it stays
//! beside the newcomer as a page of another tab (`beside`). Each is scoped per display, so
//! a second display never supersedes the primary. Two live auto-reconnecting pages would
//! trade a session forever, so past `eviction_storm_limit` takeovers inside
//! `eviction_storm_window` the newest claimant is refused with 4000 and the holder stays. A
//! viewer never supersedes a live controller; it reaps a dead one, and a secondary display's
//! viewer without a controller is refused, since that display exists only through its
//! controller's layout.
//!
//! A client whose socket was cut with no close frame keeps its session for `session_hold`
//! when its hello named a resume key: a hello naming that key takes the session over under
//! the old peer id with the lines held for it, and the server peer is told nothing. A close
//! frame ends the session at once. The server peer leaving closes every client as going away
//! (1001), which a page reconnects from, and never with the 4000 of a refusal.
//!
//! With a master token configured the auth middleware forwards upgrades without Basic auth,
//! so the checks here are the only gate: a client presents a provisioned token, whose role
//! caps the one it claims, and a server peer proves itself with `server_peer_secret` or the
//! master token.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, Instant};

use log::{debug, error, info, warn};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::socket::{Inbound, PeerSocket};
use super::wire::{ClientType, Hello, Refusal, SessionStart, parse_hello, split_word};
use crate::sdp::offer::random_id;

/// How long a client whose signaling socket was cut keeps its session for a resuming hello.
pub const SESSION_HOLD: Duration = Duration::from_secs(3);
/// Lines addressed to a held client, kept for its resume.
pub const HELD_LINES_MAX: usize = 256;
/// The window the takeover-storm breaker counts takeovers in.
pub const EVICTION_STORM_WINDOW: Duration = Duration::from_secs(5);
/// The takeovers inside the window that make a storm.
pub const EVICTION_STORM_LIMIT: usize = 3;

/// What the session-token table says a token may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenPermissions {
    pub role: ClientType,
}

/// The live control plane the handshake reads, per handshake, so re-provisioning is honored:
/// the integrator wires it to the settings and to `/api/tokens`.
pub trait SessionAuthority: Send + Sync + 'static {
    /// The master token, when secure mode is on.
    fn master_token(&self) -> Option<String>;
    /// A token's entry, compared in constant time so the reply says nothing of a guess.
    fn lookup(&self, token: &str) -> Option<TokenPermissions>;
}

/// An authority with no master token: secure mode off.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenAuthority;

impl SessionAuthority for OpenAuthority {
    fn master_token(&self) -> Option<String> {
        None
    }

    fn lookup(&self, _token: &str) -> Option<TokenPermissions> {
        None
    }
}

/// How a registry behaves.
#[derive(Clone)]
pub struct SignalingOptions {
    /// How long a socket stays quiet before a keepalive ping.
    pub keepalive_timeout: Duration,
    pub enable_sharing: bool,
    pub enable_shared: bool,
    pub enable_player2: bool,
    pub enable_player3: bool,
    pub enable_player4: bool,
    pub session_hold: Duration,
    pub eviction_storm_window: Duration,
    pub eviction_storm_limit: usize,
    pub authority: Arc<dyn SessionAuthority>,
}

impl Default for SignalingOptions {
    fn default() -> SignalingOptions {
        SignalingOptions {
            keepalive_timeout: Duration::from_secs(30),
            enable_sharing: false,
            enable_shared: true,
            enable_player2: false,
            enable_player3: false,
            enable_player4: false,
            session_hold: SESSION_HOLD,
            eviction_storm_window: EVICTION_STORM_WINDOW,
            eviction_storm_limit: EVICTION_STORM_LIMIT,
            authority: Arc::new(OpenAuthority),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Status {
    Idle,
    Session,
    Room(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum StormKey {
    Client(String),
    Controller(String),
    Slot(String, i64),
    Token(String, Option<String>),
}

struct Peer {
    peer_id: String,
    socket: Arc<PeerSocket>,
    raddr: String,
    host: Option<String>,
    is_server: bool,
    seq: u64,
    client_type: Option<ClientType>,
    client_slot: Option<i64>,
    client_token: Option<String>,
    status: Status,
    display_id: String,
    display_position: String,
    fullcolor_codecs: Option<Vec<String>>,
    tenbit_codecs: Option<Vec<String>>,
    tab_id: Option<String>,
    cc_start_kbps: Option<Value>,
    page_size: Option<(u32, u32)>,
    resume_key: Option<String>,
    held: Vec<String>,
}

/// What the service reads of a registered peer: the signaling server that validated these
/// claims is this process, so they come from here and not from SESSION_START's fields.
#[derive(Debug, Clone, PartialEq)]
pub struct PeerInfo {
    pub peer_id: String,
    pub raddr: String,
    /// The `Host` its signaling request named: the address the page reached this server at,
    /// which tells apart the public entries of a server published at more than one.
    pub host: Option<String>,
    pub is_server: bool,
    pub client_type: Option<ClientType>,
    pub client_slot: Option<i64>,
    pub client_token: Option<String>,
    pub display_id: String,
    pub display_position: String,
    pub fullcolor_codecs: Option<Vec<String>>,
    pub tenbit_codecs: Option<Vec<String>>,
    pub tab_id: Option<String>,
    /// The rate congestion control last held this page's display at, as the page kept it.
    pub cc_start_kbps: Option<Value>,
    /// The width and height the page's first resize will ask for.
    pub page_size: Option<(u32, u32)>,
}

#[derive(Default)]
struct State {
    peers: HashMap<String, Peer>,
    /// Client to server: stored one way so a session's end is unambiguous.
    sessions: HashMap<String, String>,
    rooms: HashMap<String, BTreeSet<String>>,
    eviction_times: HashMap<StormKey, Vec<Instant>>,
    holds: HashMap<String, JoinHandle<()>>,
    next_seq: u64,
}

impl State {
    /// The peers oldest first, which is the order `next()` over a registry reads them in.
    fn by_age(&self) -> Vec<&Peer> {
        let mut peers: Vec<&Peer> = self.peers.values().collect();
        peers.sort_unstable_by_key(|peer| peer.seq);
        peers
    }
}

/// Called with whether any browser client is connected; the server peer does not count.
pub type PresenceCallback = Arc<dyn Fn(bool) + Send + Sync>;
/// Called with a socket's id and a pong's payload, for the uplink gauge.
pub type PongCallback = Arc<dyn Fn(u64, &[u8]) + Send + Sync>;

/// The signaling plane of one server.
pub struct PeerRegistry {
    options: SignalingOptions,
    server_peer_secret: String,
    state: Mutex<State>,
    on_client_presence: RwLock<Option<PresenceCallback>>,
    on_pong: RwLock<Option<PongCallback>>,
    rtc_config: RwLock<Option<String>>,
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The end of a connection's service, run when `serve` returns or is dropped.
struct ServeEnd<'a> {
    registry: &'a Arc<PeerRegistry>,
    peer_id: String,
    socket: Arc<PeerSocket>,
    cut: bool,
}

impl Drop for ServeEnd<'_> {
    fn drop(&mut self) {
        if !(self.cut && self.registry.hold_session(&self.peer_id, &self.socket)) {
            self.registry.remove_peer(&self.peer_id, Some(&self.socket));
        }
        self.registry.notify_client_presence();
    }
}

impl PeerRegistry {
    /// A registry with a fresh per-process server secret.
    pub fn new(options: SignalingOptions) -> Arc<PeerRegistry> {
        Arc::new(PeerRegistry {
            options,
            server_peer_secret: format!("{}{}", random_id(), random_id()),
            state: Mutex::new(State::default()),
            on_client_presence: RwLock::new(None),
            on_pong: RwLock::new(None),
            rtc_config: RwLock::new(None),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What the in-process server peer proves itself with: random per process, and sent only
    /// over the local signaling connection.
    pub fn server_peer_secret(&self) -> &str {
        &self.server_peer_secret
    }

    /// Set what is told of client presence.
    pub fn set_on_client_presence(&self, callback: PresenceCallback) {
        *self
            .on_client_presence
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(callback);
    }

    /// Set what is told of a pong, which feeds the uplink gauge.
    pub fn set_on_pong(&self, callback: PongCallback) {
        *self.on_pong.write().unwrap_or_else(PoisonError::into_inner) = Some(callback);
    }

    /// The configuration `/api/turn` serves, the one the server resolved for itself, so a
    /// client negotiates with the same ICE servers; `None` is that route's 404.
    pub fn rtc_config(&self) -> Option<String> {
        self.rtc_config
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Set the configuration `/api/turn` serves.
    pub fn set_rtc_config(&self, config: Option<String>) {
        *self
            .rtc_config
            .write()
            .unwrap_or_else(PoisonError::into_inner) = config;
    }

    /// What is known of a registered peer.
    pub fn peer(&self, peer_id: &str) -> Option<PeerInfo> {
        let state = self.state();
        let peer = state.peers.get(peer_id)?;
        Some(PeerInfo {
            peer_id: peer.peer_id.clone(),
            raddr: peer.raddr.clone(),
            host: peer.host.clone(),
            is_server: peer.is_server,
            client_type: peer.client_type,
            client_slot: peer.client_slot,
            client_token: peer.client_token.clone(),
            display_id: peer.display_id.clone(),
            display_position: peer.display_position.clone(),
            fullcolor_codecs: peer.fullcolor_codecs.clone(),
            tenbit_codecs: peer.tenbit_codecs.clone(),
            tab_id: peer.tab_id.clone(),
            cc_start_kbps: peer.cc_start_kbps.clone(),
            page_size: peer.page_size,
        })
    }

    /// A fatal verdict on one peer's socket, as a refused secondary display gets.
    pub fn close_peer(&self, peer_id: &str, code: u16, reason: &str) {
        if let Some(peer) = self.state().peers.get(peer_id) {
            peer.socket.close(code, reason);
        }
    }

    /// Every browser client's socket, session token, and remote address, oldest first: the
    /// signaling socket stays open for a session's life, which makes it the transport's window
    /// onto the client's uplink.
    pub fn client_sockets(&self) -> Vec<(Arc<PeerSocket>, Option<String>, String)> {
        self.state()
            .by_age()
            .into_iter()
            .filter(|peer| !peer.is_server)
            .map(|peer| {
                (
                    peer.socket.clone(),
                    peer.client_token.clone(),
                    peer.raddr.clone(),
                )
            })
            .collect()
    }

    /// Close every client still registered as going away, for a service shutting down: its
    /// server peer may already be gone, and an open socket keeps the listener waiting.
    pub fn close_clients(&self) {
        for peer in self.state().peers.values() {
            if !peer.is_server {
                peer.socket.close(1001, "Server going away.");
            }
        }
    }

    fn notify_client_presence(&self) {
        let present = self.state().peers.values().any(|peer| !peer.is_server);
        let callback = self
            .on_client_presence
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(callback) = callback {
            callback(present);
        }
    }

    /// Serve one connection end to end: its hello, its messages, and its removal.
    ///
    /// `ceiling` is the highest role the transport credential grants; `Viewer` caps a
    /// self-asserted controller. `host` is the request's `Host` header, kept for the peer
    /// (`PeerInfo::host`). The route forwards the WebSocket's frames to `inbound` and
    /// writes the socket's outbox; a dropped `inbound` reads as a cut.
    pub async fn serve(
        self: &Arc<PeerRegistry>,
        socket: Arc<PeerSocket>,
        mut inbound: mpsc::Receiver<Inbound>,
        raddr: &str,
        host: Option<&str>,
        ceiling: Option<ClientType>,
    ) {
        let text = loop {
            match inbound.recv().await {
                Some(Inbound::Ping(data)) => socket.pong(data),
                Some(Inbound::Pong(_)) => {}
                Some(Inbound::Text(text)) => break text,
                Some(Inbound::Binary) => {
                    warn!(target: "signaling", "Refusing {raddr}: a hello that is not text");
                    socket.close(1002, "invalid protocol");
                    return;
                }
                Some(Inbound::CloseFrame | Inbound::Cut) | None => {
                    socket.mark_closed();
                    return;
                }
            }
        };
        let peer_id = match self.hello_peer(&text, &socket, raddr, host, ceiling) {
            Ok(peer_id) => peer_id,
            Err(refusal) => {
                error!(target: "signaling", "Error during handshake with peer {raddr}: {}", refusal.log);
                socket.close(refusal.code, &refusal.reason);
                return;
            }
        };
        self.notify_client_presence();
        let mut end = ServeEnd {
            registry: self,
            peer_id: peer_id.clone(),
            socket: Arc::clone(&socket),
            cut: false,
        };
        let keepalive = self.options.keepalive_timeout;
        loop {
            let frame = tokio::select! {
                frame = tokio::time::timeout(keepalive, inbound.recv()) => frame,
                () = socket.closed_here() => {
                    end.cut = socket.was_aborted();
                    return;
                }
            };
            match frame {
                Err(_) => {
                    debug!(target: "signaling", "Keepalive ping to {raddr}");
                    socket.ping();
                }
                Ok(Some(Inbound::Text(text))) => {
                    if !self.handle_text(&peer_id, &socket, &text) {
                        return;
                    }
                }
                Ok(Some(Inbound::Binary)) => {
                    warn!(target: "signaling", "Ignoring non-text message from peer {peer_id}");
                }
                Ok(Some(Inbound::Ping(data))) => socket.pong(data),
                Ok(Some(Inbound::Pong(data))) => {
                    let callback = self
                        .on_pong
                        .read()
                        .unwrap_or_else(PoisonError::into_inner)
                        .clone();
                    if let Some(callback) = callback {
                        callback(socket.id(), &data);
                    }
                }
                Ok(Some(Inbound::CloseFrame)) => {
                    info!(target: "signaling", "Peer connection closed: {peer_id}");
                    socket.mark_closed();
                    return;
                }
                Ok(Some(Inbound::Cut) | None) => {
                    info!(target: "signaling", "Peer connection cut: {peer_id}");
                    socket.mark_closed();
                    end.cut = true;
                    return;
                }
            }
        }
    }

    /// Whether a controller joining with `tab_id` stays beside `peer`, a controller of the
    /// same display, rather than superseding it: with sharing on, a page of another tab. A
    /// page that names no tab, or the peer's own tab reconnecting, supersedes it.
    fn beside(&self, peer: &Peer, client_type: Option<ClientType>, tab_id: Option<&str>) -> bool {
        self.options.enable_sharing
            && client_type == Some(ClientType::Controller)
            && peer.client_type == Some(ClientType::Controller)
            && tab_id.is_some_and(|tab| peer.tab_id.as_deref().is_some_and(|own| own != tab))
    }

    /// Whether a newcomer of `client_type` may take `peer`'s place: a viewer never supersedes
    /// a live controller, as on the websockets transport; it still reaps a dead holder and
    /// supersedes another viewer.
    fn supersedes(client_type: Option<ClientType>, peer: &Peer) -> bool {
        client_type == Some(ClientType::Controller)
            || peer.client_type != Some(ClientType::Controller)
            || peer.socket.is_closed()
    }

    /// Whether `key` has been taken over the limit's times inside the window.
    fn eviction_storm(&self, state: &mut State, key: &StormKey) -> bool {
        let window = self.options.eviction_storm_window;
        let recent = state.eviction_times.get(key).map_or(0, |times| {
            times.iter().filter(|t| t.elapsed() < window).count()
        });
        recent >= self.options.eviction_storm_limit
    }

    /// Note a takeover of `key`, forgetting every identity whose last takeover left the
    /// window, so the table holds only what a storm could still trip on, whatever names the
    /// hellos carry.
    fn record_eviction(&self, state: &mut State, key: StormKey) {
        let window = self.options.eviction_storm_window;
        state.eviction_times.retain(|_, times| {
            times.retain(|t| t.elapsed() < window);
            !times.is_empty()
        });
        state
            .eviction_times
            .entry(key)
            .or_default()
            .push(Instant::now());
    }

    /// Detach a peer from shared state, telling its partner: the server is told the session
    /// ended and keeps its socket, since it serves every client.
    fn evict_dead_peer(state: &mut State, pid: &str) {
        let peer_type = state.peers.get(pid).and_then(|peer| peer.client_type);
        if let Some(other_id) = state.sessions.remove(pid)
            && let Some(client_type) = peer_type
            && let Some(other) = state.peers.get(&other_id)
        {
            let _ = other
                .socket
                .send_text(format!("SESSION_END {pid} {}", client_type.as_str()));
        }
        Self::leave_room(state, pid);
        state.peers.remove(pid);
    }

    fn leave_room(state: &mut State, pid: &str) {
        let Some(Status::Room(room_id)) = state.peers.get(pid).map(|peer| peer.status.clone())
        else {
            return;
        };
        let Some(members) = state.rooms.get_mut(&room_id) else {
            return;
        };
        if !members.remove(pid) {
            return;
        }
        let others: Vec<String> = members.iter().cloned().collect();
        if members.is_empty() {
            state.rooms.remove(&room_id);
        }
        for other in others {
            if let Some(peer) = state.peers.get(&other) {
                let _ = peer.socket.send_text(format!("ROOM_PEER_LEFT {pid}"));
            }
        }
    }

    /// Evict a peer whatever its liveness: a live holder is superseded with 4001, and that
    /// counts toward the storm; a dead one is only reaped, which is routine housekeeping.
    fn evict_peer(&self, state: &mut State, pid: &str, reason: &str, storm_key: StormKey) {
        if let Some(peer) = state.peers.get(pid)
            && !peer.socket.is_closed()
        {
            peer.socket.close(4001, reason);
            self.record_eviction(state, storm_key);
        }
        Self::evict_dead_peer(state, pid);
    }

    /// A client's token gate, with a master token configured.
    fn secure_token_rejected(&self, token: Option<&str>) -> bool {
        let authority = &self.options.authority;
        if authority.master_token().is_none() {
            return false;
        }
        token.and_then(|token| authority.lookup(token)).is_none()
    }

    /// The server peer owns the media graph for every client, and leaving closes them all, so
    /// registering as one is an administrative act: a credential that only lets a page in is
    /// no proof of being the streaming server.
    fn server_token_rejected(&self, token: Option<&str>) -> bool {
        let Some(token) = token.filter(|token| !token.is_empty()) else {
            return true;
        };
        let mut accepted = constant_time_eq(token.as_bytes(), self.server_peer_secret.as_bytes());
        if let Some(master) = self.options.authority.master_token() {
            accepted |= constant_time_eq(token.as_bytes(), master.as_bytes());
        }
        !accepted
    }

    /// The token's provisioned role is authoritative with a master token: a viewer-role
    /// token's controller claim is coerced to viewer, so it cannot own a media graph, open a
    /// secondary display, or drive input.
    fn secure_effective_client_type(
        &self,
        client_type: Option<ClientType>,
        token: Option<&str>,
    ) -> Option<ClientType> {
        let authority = &self.options.authority;
        if authority.master_token().is_none() || client_type != Some(ClientType::Controller) {
            return client_type;
        }
        match token.and_then(|token| authority.lookup(token)) {
            Some(perms) if perms.role == ClientType::Controller => client_type,
            _ => Some(ClientType::Viewer),
        }
    }

    /// Hand a client's session to the socket of a hello naming its resume key. It keeps its
    /// peer id and its session; a socket of its still open (the cut reached the page alone)
    /// is closed without ending the session. Returns the peer, its partner, and the lines
    /// held for it; `None` where no session of this client is open to resume: none of its
    /// key, another role, display or token, or the partner gone.
    fn resume(
        state: &mut State,
        socket: &Arc<PeerSocket>,
        raddr: &str,
        host: Option<&str>,
        hello: &Hello,
        client_type: Option<ClientType>,
    ) -> Option<(String, String, Vec<String>)> {
        let key = hello.resume_key.as_deref()?;
        let peer_id = state
            .peers
            .values()
            .find(|peer| {
                !peer.is_server
                    && peer
                        .resume_key
                        .as_deref()
                        .is_some_and(|own| constant_time_eq(own.as_bytes(), key.as_bytes()))
            })?
            .peer_id
            .clone();
        let partner = state.sessions.get(&peer_id)?.clone();
        if !state.peers.get(&partner).is_some_and(|peer| peer.is_server) {
            return None;
        }
        let peer = state.peers.get(&peer_id)?;
        if peer.status != Status::Session
            || peer.client_type != client_type
            || peer.display_id != hello.display_id
            || peer.client_token != hello.client_token
        {
            return None;
        }
        if let Some(hold) = state.holds.remove(&peer_id) {
            hold.abort();
        }
        let peer = state.peers.get_mut(&peer_id)?;
        if !peer.socket.is_closed() {
            peer.socket.close(1000, "Resumed on a new connection.");
        }
        peer.socket = Arc::clone(socket);
        peer.raddr = raddr.to_owned();
        peer.host = host.map(str::to_owned);
        let held = std::mem::take(&mut peer.held);
        Some((peer_id, partner, held))
    }

    /// Validate a hello, evict what it supersedes, and register its peer, all under the lock.
    ///
    /// # Errors
    ///
    /// The close to answer with; nothing was registered.
    fn hello_peer(
        &self,
        text: &str,
        socket: &Arc<PeerSocket>,
        raddr: &str,
        host: Option<&str>,
        ceiling: Option<ClientType>,
    ) -> Result<String, Refusal> {
        let hello = parse_hello(text)?;
        let mut state = self.state();
        let mut client_type = hello.client_type;
        if !hello.is_server && self.secure_token_rejected(hello.client_token.as_deref()) {
            return Err(Refusal {
                code: 4001,
                reason: "Invalid authentication token".to_owned(),
                log: "missing or invalid token in secure mode".to_owned(),
            });
        }
        if hello.is_server && self.server_token_rejected(hello.server_token.as_deref()) {
            return Err(Refusal {
                code: 4001,
                reason: "Invalid authentication token".to_owned(),
                log: "a server peer that proved no claim to the role".to_owned(),
            });
        }
        if !hello.is_server {
            client_type =
                self.secure_effective_client_type(client_type, hello.client_token.as_deref());
            if ceiling == Some(ClientType::Viewer) && client_type == Some(ClientType::Controller) {
                client_type = Some(ClientType::Viewer);
            }
            if let Some((peer_id, partner, held)) =
                Self::resume(&mut state, socket, raddr, host, &hello, client_type)
            {
                drop(state);
                info!(target: "signaling", "Peer {peer_id} resumed its session from {raddr}");
                let sent = socket.send_text(format!("RESUMED {partner}")).is_ok()
                    && held.into_iter().all(|line| socket.send_text(line).is_ok());
                if !sent {
                    warn!(target: "signaling", "Peer {peer_id} lost its socket during its resume");
                }
                return Ok(peer_id);
            }
            self.admit_client(&mut state, &hello, client_type, raddr)?;
        }
        let peer_id = format!(
            "{}-{}",
            if hello.is_server { "server" } else { "client" },
            random_id()
        );
        let seq = state.next_seq;
        state.next_seq += 1;
        state.peers.insert(
            peer_id.clone(),
            Peer {
                peer_id: peer_id.clone(),
                socket: Arc::clone(socket),
                raddr: raddr.to_owned(),
                host: host.map(str::to_owned),
                is_server: hello.is_server,
                seq,
                client_type,
                client_slot: hello.client_slot,
                client_token: hello.client_token,
                status: Status::Idle,
                display_id: hello.display_id,
                display_position: hello.display_position,
                fullcolor_codecs: hello.fullcolor_codecs,
                tenbit_codecs: hello.tenbit_codecs,
                tab_id: hello.client_tab_id,
                cc_start_kbps: hello.cc_start_kbps,
                page_size: hello.page_size,
                resume_key: hello.resume_key,
                held: Vec::new(),
            },
        );
        if socket.send_text("HELLO".to_owned()).is_err() {
            state.peers.remove(&peer_id);
            return Err(Refusal {
                code: 1006,
                reason: String::new(),
                log: "the socket ended during the hello".to_owned(),
            });
        }
        info!(
            target: "signaling",
            "Registered peer {peer_id} at {raddr} ({})",
            client_type.map_or("server", ClientType::as_str)
        );
        Ok(peer_id)
    }

    /// The collision and gate rules a client that resumed nothing meets (module docstring).
    fn admit_client(
        &self,
        state: &mut State,
        hello: &Hello,
        client_type: Option<ClientType>,
        raddr: &str,
    ) -> Result<(), Refusal> {
        let display = hello.display_id.as_str();
        let tab_id = hello.client_tab_id.as_deref();
        let tab = |tab: Option<&str>| -> String { tab.unwrap_or("none").chars().take(8).collect() };
        let verdict = |reason: &str, log: String| Refusal {
            code: 4000,
            reason: reason.to_owned(),
            log,
        };
        const STORM: &str = "Session takeover loop detected; another page holds this session.";
        if !self.options.enable_sharing {
            let key = StormKey::Client(display.to_owned());
            let existing: Vec<String> = state
                .by_age()
                .iter()
                .filter(|peer| {
                    !peer.is_server
                        && peer.display_id == display
                        && Self::supersedes(client_type, peer)
                })
                .map(|peer| peer.peer_id.clone())
                .collect();
            if !existing.is_empty() && self.eviction_storm(state, &key) {
                return Err(verdict(STORM, format!("takeover storm, from {raddr}")));
            }
            for pid in existing {
                self.evict_peer(state, &pid, "Superseded by a new connection.", key.clone());
                info!(target: "signaling", "Evicting client {pid} for non-sharing reconnect from {raddr}");
            }
        } else {
            let slot = hello.client_slot;
            let allowed = slot == Some(1)
                || (slot == Some(2) && self.options.enable_player2)
                || (slot == Some(3) && self.options.enable_player3)
                || (slot == Some(4) && self.options.enable_player4);
            if slot != Some(-1) && !allowed {
                return Err(verdict(
                    "Invalid player id provided, check URL.",
                    format!("Invalid client slot provided {slot:?}"),
                ));
            }
            if let Some(slot) = slot.filter(|slot| *slot != -1) {
                // A page is the only one of its slot, as its hello names it. With a master
                // token the table says who a page is and which slots it drives, and every
                // page opened without #playerN names slot 1, so a page is the only one of its
                // token instead: the pages of a collaboration room, each with its own, stay.
                let by_token = self.options.authority.master_token().is_some();
                let key = if by_token {
                    StormKey::Token(display.to_owned(), hello.client_token.clone())
                } else {
                    StormKey::Slot(display.to_owned(), slot)
                };
                let colliding: Vec<String> = state
                    .by_age()
                    .iter()
                    .filter(|peer| {
                        (if by_token {
                            peer.client_token == hello.client_token
                        } else {
                            peer.client_slot == Some(slot)
                        }) && peer.display_id == display
                            && !self.beside(peer, client_type, tab_id)
                            && Self::supersedes(client_type, peer)
                    })
                    .map(|peer| peer.peer_id.clone())
                    .collect();
                if !colliding.is_empty() && self.eviction_storm(state, &key) {
                    // The token is a credential: the log names the page by its address alone.
                    return Err(if by_token {
                        verdict(
                            STORM,
                            format!("a page of the same token from {raddr}: takeover storm"),
                        )
                    } else {
                        verdict(
                            "Player slot takeover loop detected; another page holds this slot.",
                            format!("slot {slot} claim from {raddr}: takeover storm"),
                        )
                    });
                }
                for pid in colliding {
                    let reason = if by_token {
                        "Superseded by a new connection."
                    } else {
                        "Superseded by a new connection for this player slot."
                    };
                    let old_tab = tab(state
                        .peers
                        .get(&pid)
                        .and_then(|peer| peer.tab_id.as_deref()));
                    self.evict_peer(state, &pid, reason, key.clone());
                    info!(
                        target: "signaling",
                        "Evicting peer {pid} for reconnect from {raddr} (tab {old_tab}, the new page's {})",
                        tab(tab_id)
                    );
                }
            }
        }
        if !self.options.enable_shared && hello.client_strict_viewer {
            return Err(verdict(
                "Strict shared clients are not enabled.",
                format!("Strict shared clients are disabled; connection from {raddr}"),
            ));
        }
        let controller_of_display = |state: &State, skip_beside: bool| -> Option<String> {
            state
                .by_age()
                .iter()
                .find(|peer| {
                    peer.client_type == Some(ClientType::Controller)
                        && peer.display_id == display
                        && !(skip_beside && self.beside(peer, client_type, tab_id))
                })
                .map(|peer| peer.peer_id.clone())
        };
        let mut controller = controller_of_display(state, false);
        if client_type == Some(ClientType::Controller) {
            // A controller of another tab stays beside the newcomer; the one it supersedes is
            // its own page's, or any where either names no tab.
            match controller_of_display(state, true) {
                Some(rival) => controller = Some(rival),
                None => {
                    if let Some(beside) = &controller {
                        info!(
                            target: "signaling",
                            "Controller from {raddr} joins display {display} beside {beside} (tab {})",
                            tab(tab_id)
                        );
                    }
                    controller = None;
                }
            }
            if let Some(pid) = controller.take() {
                let key = StormKey::Controller(display.to_owned());
                if self.eviction_storm(state, &key) {
                    return Err(verdict(
                        STORM,
                        format!("controller from {raddr}: takeover storm"),
                    ));
                }
                self.evict_peer(
                    state,
                    &pid,
                    "Superseded by a new controller connection.",
                    key,
                );
                info!(target: "signaling", "Evicting controller {pid} for reconnect from {raddr}");
            }
        }
        if client_type == Some(ClientType::Viewer) {
            if let Some(pid) = controller.clone() {
                let dead = state
                    .peers
                    .get(&pid)
                    .is_some_and(|peer| peer.socket.is_closed());
                if dead && !state.holds.contains_key(&pid) {
                    Self::evict_dead_peer(state, &pid);
                    controller = None;
                    info!(target: "signaling", "Evicting dead controller {pid} ahead of viewer from {raddr}");
                }
            }
            if controller.is_none() && display != "primary" {
                return Err(verdict(
                    "No controller detected. A secondary display's viewer requires that display's controller.",
                    format!("No controller for display {display}; rejecting viewer from {raddr}"),
                ));
            }
        }
        Ok(())
    }

    /// Handle one text line from a registered peer. Returns whether the connection goes on.
    fn handle_text(&self, peer_id: &str, socket: &Arc<PeerSocket>, msg: &str) -> bool {
        let mut state = self.state();
        let Some(status) = state.peers.get(peer_id).map(|peer| peer.status.clone()) else {
            error!(target: "signaling", "Peer {peer_id} not found in the registry");
            return false;
        };
        match status {
            Status::Session => Self::relay_session(&mut state, peer_id, msg),
            Status::Room(room_id) => Self::relay_room(&state, peer_id, socket, &room_id, msg),
            Status::Idle if msg.starts_with("SESSION") => {
                Self::start_session(&mut state, peer_id, socket, msg)
            }
            Status::Idle if msg.starts_with("ROOM") => {
                Self::join_room(&mut state, peer_id, socket, msg)
            }
            Status::Idle => {
                debug!(target: "signaling", "Ignoring an unknown message from {peer_id}");
            }
        }
        true
    }

    /// Relay `<peer-id> <message>` between session partners, in either direction. What a cut
    /// partner cannot take waits for its resume, up to `HELD_LINES_MAX`.
    fn relay_session(state: &mut State, peer_id: &str, msg: &str) {
        let Some((other_id, body)) = split_word(msg) else {
            warn!(target: "signaling", "Malformed session message from {peer_id}");
            return;
        };
        if !state.peers.contains_key(other_id) {
            warn!(target: "signaling", "Peer {other_id} not found for session message relay");
            return;
        }
        let partners = state
            .sessions
            .get(peer_id)
            .is_some_and(|peer| peer == other_id)
            || state
                .sessions
                .get(other_id)
                .is_some_and(|peer| peer == peer_id);
        if !partners {
            warn!(target: "signaling", "Rejecting session relay {peer_id} -> {other_id}: not session partners");
            return;
        }
        let Some(other) = state.peers.get_mut(other_id) else {
            return;
        };
        if other.status != Status::Session {
            warn!(target: "signaling", "Rejecting session relay {peer_id} -> {other_id}: target not in a session");
            return;
        }
        debug!(target: "signaling", "{peer_id} -> {other_id}: {} bytes", body.len());
        let line = format!("{peer_id} {body}");
        if other.socket.send_text(line.clone()).is_err() && other.held.len() < HELD_LINES_MAX {
            other.held.push(line);
        }
    }

    fn relay_room(
        state: &State,
        peer_id: &str,
        socket: &Arc<PeerSocket>,
        room_id: &str,
        msg: &str,
    ) {
        let error = |text: String| {
            let _ = socket.send_text(format!("ERROR {text}"));
        };
        if !msg.starts_with("ROOM_PEER_MSG") {
            error("invalid msg, already in room".to_owned());
            return;
        }
        let parts = split_word(msg).and_then(|(_, rest)| split_word(rest));
        let Some((other_id, body)) = parts else {
            error("invalid ROOM_PEER_MSG format".to_owned());
            return;
        };
        let Some(other) = state.peers.get(other_id) else {
            error(format!("peer {other_id:?} not found"));
            return;
        };
        if other.status != Status::Room(room_id.to_owned()) {
            error(format!("peer {other_id:?} is not in the room"));
            return;
        }
        let _ = other
            .socket
            .send_text(format!("ROOM_PEER_MSG {peer_id} {body}"));
    }

    /// Pair a caller with the streaming server. Only the server is called: a session with
    /// another page would hand it this page's offer and overwrite its state.
    fn start_session(state: &mut State, peer_id: &str, socket: &Arc<PeerSocket>, msg: &str) {
        let Some((_, callee)) = split_word(msg) else {
            warn!(target: "signaling", "Malformed session message from {peer_id}");
            return;
        };
        let callee = callee.trim_end();
        let callee_id = if callee == "server" {
            state
                .by_age()
                .iter()
                .find(|peer| peer.is_server)
                .map_or_else(|| callee.to_owned(), |peer| peer.peer_id.clone())
        } else {
            callee.to_owned()
        };
        if !state
            .peers
            .get(&callee_id)
            .is_some_and(|peer| peer.is_server)
        {
            let _ = socket.send_text("ERROR peer server not found".to_owned());
            return;
        }
        let _ = socket.send_text(format!("SESSION_OK {callee_id}"));
        let Some(peer) = state.peers.get(peer_id) else {
            return;
        };
        let start = SessionStart {
            peer_id: peer_id.to_owned(),
            client_type: peer
                .client_type
                .map_or("None", ClientType::as_str)
                .to_owned(),
            display_id: peer.display_id.clone(),
            display_position: peer.display_position.clone(),
            client_token: peer.client_token.clone(),
            fullcolor_codecs: peer.fullcolor_codecs.clone(),
            tenbit_codecs: peer.tenbit_codecs.clone(),
        };
        info!(target: "signaling", "Session from {peer_id} ({}) to {callee_id}", peer.raddr);
        if let Some(callee) = state.peers.get_mut(&callee_id) {
            let _ = callee.socket.send_text(start.to_line());
            callee.status = Status::Session;
        }
        if let Some(peer) = state.peers.get_mut(peer_id) {
            peer.status = Status::Session;
        }
        state.sessions.insert(peer_id.to_owned(), callee_id);
    }

    fn join_room(state: &mut State, peer_id: &str, socket: &Arc<PeerSocket>, msg: &str) {
        let Some((_, room_id)) = split_word(msg) else {
            warn!(target: "signaling", "Malformed room message from {peer_id}");
            return;
        };
        if room_id == "session" || room_id.contains(char::is_whitespace) {
            let _ = socket.send_text(format!("ERROR invalid room id {room_id:?}"));
            return;
        }
        let members = state.rooms.entry(room_id.to_owned()).or_default();
        let others: Vec<String> = members.iter().cloned().collect();
        members.insert(peer_id.to_owned());
        let _ = socket.send_text(format!("ROOM_OK {}", others.join(" ")));
        if let Some(peer) = state.peers.get_mut(peer_id) {
            peer.status = Status::Room(room_id.to_owned());
        }
        for other in others {
            if let Some(peer) = state.peers.get(&other) {
                let _ = peer.socket.send_text(format!("ROOM_PEER_JOINED {peer_id}"));
            }
        }
    }

    /// Keep the session of a client whose socket was cut for `session_hold`, for a hello
    /// naming its resume key, and end it after. Returns whether it is held: a client in no
    /// session, one that named no key, and one that resumed on another socket since are not.
    fn hold_session(self: &Arc<PeerRegistry>, peer_id: &str, socket: &Arc<PeerSocket>) -> bool {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return false;
        };
        let mut state = self.state();
        let Some(peer) = state.peers.get(peer_id) else {
            return false;
        };
        if peer.socket.id() != socket.id()
            || peer.is_server
            || peer.status != Status::Session
            || peer.resume_key.is_none()
        {
            return false;
        }
        let registry = Arc::clone(self);
        let held_id = peer_id.to_owned();
        let held_socket = Arc::clone(socket);
        let hold = self.options.session_hold;
        let task = runtime.spawn(async move {
            tokio::time::sleep(hold).await;
            registry.state().holds.remove(&held_id);
            registry.remove_peer(&held_id, Some(&held_socket));
            registry.notify_client_presence();
        });
        state.holds.insert(peer_id.to_owned(), task);
        info!(
            target: "signaling",
            "Signaling socket of peer {peer_id} cut; its session is held {:.0}s for a resume.",
            hold.as_secs_f64()
        );
        true
    }

    /// Remove a peer and clean up its session, room, and socket.
    ///
    /// Every client, a primary controller included, only tells the server its session ended:
    /// the server socket is shared by every display's session and every viewer. A server peer
    /// leaving closes every client as going away (module docstring). `socket` is the one that
    /// ended, when the peer is removed for it: a peer that resumed on another stays.
    pub fn remove_peer(&self, peer_id: &str, socket: Option<&Arc<PeerSocket>>) {
        let mut state = self.state();
        if let Some(socket) = socket
            && state
                .peers
                .get(peer_id)
                .is_some_and(|peer| peer.socket.id() != socket.id())
        {
            return;
        }
        let Some(peer) = state.peers.get(peer_id) else {
            state.sessions.remove(peer_id);
            return;
        };
        let is_server = peer.is_server;
        let raddr = peer.raddr.clone();
        let client_type = peer.client_type;
        let peer_socket = Arc::clone(&peer.socket);
        Self::evict_dead_peer(&mut state, peer_id);
        if is_server {
            for other in state.peers.values() {
                if !other.is_server {
                    other.socket.close(1001, "Server going away.");
                }
            }
        } else {
            peer_socket.close(1000, "Connection closed");
            info!(
                target: "signaling",
                "Disconnected from peer {peer_id} at {raddr} ({})",
                client_type.map_or("unknown", ClientType::as_str)
            );
        }
    }
}

#[cfg(test)]
impl PeerRegistry {
    pub(super) fn peer_ids(&self) -> Vec<String> {
        self.state().peers.keys().cloned().collect()
    }

    pub(super) fn clear_peers(&self) {
        self.state().peers.clear();
    }

    pub(super) fn storm_identities(&self) -> usize {
        self.state().eviction_times.len()
    }

    pub(super) fn held_lines(&self, peer_id: &str) -> Vec<String> {
        self.state()
            .peers
            .get(peer_id)
            .map_or_else(Vec::new, |peer| peer.held.clone())
    }

    pub(super) fn session_partner(&self, peer_id: &str) -> Option<String> {
        self.state().sessions.get(peer_id).cloned()
    }

    pub(super) fn socket_of(&self, peer_id: &str) -> Option<u64> {
        self.state().peers.get(peer_id).map(|peer| peer.socket.id())
    }
}
