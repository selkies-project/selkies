/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! ICE for the Selkies WebRTC transport: the topology decided once, the sockets it binds,
//! and the agent each peer runs on them.
//!
//! The topology is decided at startup and never per session (`config::IceTopology`):
//! `open_muxes` binds the shared UDP and TCP ports the settings name, failing the service on
//! a port in use, and every peer's agent reads the muxes, the ICE-lite choice, the port
//! range, the public addresses, and the STUN/TURN servers from the `IceConfig` the topology
//! hands it, never from the settings directly. Sessions on a shared socket are told apart by
//! their ICE username fragment alone, which is why a local fragment is eight characters
//! (`LOCAL_UFRAG_LENGTH`) rather than the four ICE requires.
//!
//! The pieces:
//!
//! - `mux`: one UDP port and one TCP port shared by every session, demultiplexed by the ICE
//!   handshake itself (a learned peer address, else the STUN USERNAME of a request, else a
//!   registered transaction id), with ICE-TCP per RFC 6544 in RFC 4571 frames.
//! - `lite`: the ICE-lite agent the server runs when `webrtc_ice_lite` is on. It answers the
//!   peer's connectivity checks and sends none of its own; the peer's nomination alone selects
//!   the pair; consent freshness (RFC 7675) is the only request it ever sends. It is Selkies
//!   code because the `lite` flag of `rtc_ice` 0.21 only stops the periodic checks: a lite
//!   agent there still answers a nomination with a triggered check of its own and no response,
//!   selects a pair only once that check succeeds, and can switch to the controlling role on a
//!   tie-breaker, all of which RFC 8445 appendix A forbids (`agent::tests`).
//! - `agent`: one surface over the lite agent and the full `rtc_ice::Agent` (the server as a
//!   full, controlling agent when ICE-lite is off), so the driver in `connection` is the same
//!   for both.
//! - `connection`: the per-peer driver that owns the local points (a mux attachment or a socket
//!   per host address, a passive TCP candidate per address, a TURN relay), authenticates every
//!   STUN request before the agent sees it, gathers candidates, and hands authenticated
//!   non-STUN datagrams up to DTLS and SRTP.
//! - `gather`, `candidate`, `stun`, `turn`, `mdns`, `config`: host-address filtering and socket
//!   binding, the candidate type and its SDP form, STUN message helpers, the TURN relay, the
//!   resolver of a browser's `.local` candidates, and the URI and setting parsers.
//! - `rtc_config`: the deployment's STUN and TURN servers and their credentials, from the
//!   prioritized sources (Cloudflare, a config file, a TURN REST API, long-term and HMAC
//!   credentials) to the `IceServer` list a peer is configured with.
//!
//! The specification is the vendored Python fork under `src/selkies/ice/` and
//! `src/selkies/webrtc/rtcicetransport.py`.

pub mod agent;
pub mod candidate;
pub mod config;
pub mod connection;
pub mod gather;
pub mod lite;
pub mod mdns;
pub mod mux;
pub mod rtc_config;
pub mod stun;
#[cfg(test)]
mod tests;
pub mod turn;

use std::net::SocketAddr;
use std::time::Instant;

use bytes::BytesMut;

pub use agent::{AgentEvent, IceState};
pub use candidate::{Candidate, CandidateType, TcpType};
pub use config::{IceConfig, IceMuxes, IceServer, IceTopology, open_muxes, parse_port_range};
pub use connection::{ConnectionEvent, IceConnection, IceParameters};
pub use mux::{TcpMux, UdpMux};

/// How many characters a local username fragment has: shared sockets tell sessions apart by
/// this fragment alone, so it is longer than the four characters ICE requires.
pub const LOCAL_UFRAG_LENGTH: usize = 8;
/// How many characters a local ICE password has.
pub const LOCAL_PWD_LENGTH: usize = 22;

/// Seconds between consent checks on the selected pair (RFC 7675), jittered by 0.8 to 1.2.
pub const CONSENT_INTERVAL: f64 = 5.0;
/// A consent check's retransmissions, 0.5, 1, and 2 seconds apart, let an answer come back
/// from behind a standing queue; consent lapses only after `CONSENT_TIMEOUT` without one.
pub const CONSENT_RETRANSMISSIONS: u32 = 2;
/// Seconds without an answered consent check before the connection is over.
pub const CONSENT_TIMEOUT: f64 = 30.0;
/// Seconds a connection waits for a path only the peer can open: its nomination of an
/// ICE-lite agent, its connection to a passive TCP candidate.
pub const INBOUND_CHECK_TIMEOUT: f64 = 30.0;
/// Remote candidates a connection takes, however many the peer offers: a browser offers a
/// handful, and each costs a pairing and a check list sort.
pub const MAX_REMOTE_CANDIDATES: usize = 64;

/// The transport a local point or a datagram rides.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Protocol {
    Udp,
    Tcp,
}

impl Protocol {
    /// The ICE transport name, as a candidate's SDP spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::Udp => "udp",
            Protocol::Tcp => "tcp",
        }
    }
}

impl From<Protocol> for rtc_shared::TransportProtocol {
    fn from(p: Protocol) -> Self {
        match p {
            Protocol::Udp => rtc_shared::TransportProtocol::UDP,
            Protocol::Tcp => rtc_shared::TransportProtocol::TCP,
        }
    }
}

/// One datagram as a socket reader hands it to a peer: the bound local address it arrived
/// on, the peer's address, the transport, the instant it was read, and the bytes.
#[derive(Debug)]
pub struct Packet {
    pub now: Instant,
    pub local: SocketAddr,
    pub peer: SocketAddr,
    pub protocol: Protocol,
    pub data: BytesMut,
}

/// What a local point delivers to its peer's task.
#[derive(Debug)]
pub enum Inbound {
    /// A datagram from the network.
    Packet(Packet),
    /// An ICE-TCP stream from `peer` on the listener at `local` closed.
    TcpStreamLost { local: SocketAddr, peer: SocketAddr },
    /// The shared socket at `local` closed, ending every connection that rode it.
    MuxClosed { local: SocketAddr },
    /// An mDNS lookup settled: `address` is what `candidate.host` resolved to, if anything.
    Resolved {
        candidate: Candidate,
        address: Option<std::net::IpAddr>,
    },
}

/// A datagram a point is asked to send: from the bound local address `local` to `peer`.
#[derive(Debug)]
pub struct Outbound {
    pub local: SocketAddr,
    pub peer: SocketAddr,
    pub protocol: Protocol,
    pub data: Vec<u8>,
}

/// What the ICE layer refuses or fails on.
#[derive(Debug, thiserror::Error)]
pub enum IceError {
    #[error("{0}")]
    Config(String),
    #[error("ICE negotiation failed: {0}")]
    Failed(String),
    #[error("not connected")]
    NotConnected,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Stun(String),
}
