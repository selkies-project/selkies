#![warn(rust_2018_idioms)]
#![warn(missing_docs)]
#![allow(dead_code)]

//! ICE for the Sans-I/O WebRTC stack.
//!
//! Interactive Connectivity Establishment ([RFC 8445], superseding [RFC 5245]) with the
//! extensions WebRTC uses: ICE-TCP candidates ([RFC 6544]), consent freshness
//! ([RFC 7675]), and Trickle ICE. ICE is what finds a path between two peers behind NATs:
//! it gathers candidate addresses, pairs local with remote, and probes each pair with STUN
//! connectivity checks until one succeeds.
//!
//! # Structure
//!
//! * [`agent`] — the Sans-I/O [`Agent`]: give it candidates and inbound
//!   datagrams, poll it for checks to send, state transitions, and the selected pair. It
//!   owns no sockets and no clock.
//! * [`candidate`] — the candidate types (host, server-reflexive, peer-reflexive, relay),
//!   their priorities, and SDP `a=candidate` parsing.
//! * [`state`] — connection and gathering states, and the checklist state machine.
//! * [`url`] — parsing `stun:`/`turn:` server URLs into something the agent can gather from.
//! * [`network_type`], [`tcp_type`] — UDP/TCP and active/passive/simultaneous-open.
//! * [`stats`] — per-candidate and per-pair counters, surfaced through `getStats`.
//! * [`mdns`] — mDNS candidate handling, for hiding private addresses.
//!
//! # Example
//!
//! ```
//! use rtc_ice::url::{ProtoType, SchemeType, Url};
//!
//! # fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let stun = Url::parse_url("stun:stun.l.google.com:19302")?;
//! assert_eq!(stun.scheme, SchemeType::Stun);
//! assert_eq!(stun.port, 19302);
//!
//! // TURN URLs may pin the transport used to reach the server.
//! let turn = Url::parse_url("turn:turn.example.com:3478?transport=tcp")?;
//! assert_eq!(turn.scheme, SchemeType::Turn);
//! assert_eq!(turn.proto, ProtoType::Tcp);
//! # Ok(())
//! # }
//! ```
//!
//! Most applications do not depend on this crate directly — the
//! [`rtc`](https://docs.rs/rtc) crate drives the agent as one layer of the peer-connection
//! pipeline.
//!
//! [RFC 8445]: https://datatracker.ietf.org/doc/html/rfc8445
//! [RFC 5245]: https://datatracker.ietf.org/doc/html/rfc5245
//! [RFC 6544]: https://datatracker.ietf.org/doc/html/rfc6544
//! [RFC 7675]: https://datatracker.ietf.org/doc/html/rfc7675

/// The Sans-I/O ICE agent: candidate pairing, connectivity checks, and nomination.
/// The crypto provider API.
///
/// Re-exported because this crate's public constructors take an
/// [`Arc<dyn RTCCryptoProvider>`](crypto::RTCCryptoProvider), which a caller must be able to name
/// without adding — and version-matching — a direct `rtc-crypto` dependency.
pub use crypto;

pub mod agent;
/// The ICE-specific STUN attributes carried in connectivity checks.
pub mod attributes;
/// Candidate types, priorities, and SDP `a=candidate` parsing.
pub mod candidate;
/// mDNS candidate handling, which hides private addresses behind `.local` names.
pub mod mdns;
/// UDP/TCP over IPv4/IPv6, as a candidate's transport.
pub mod network_type;
/// Random ICE credentials and identifiers.
pub mod rand;
/// Connection and gathering states.
pub mod state;
/// Per-candidate and per-pair counters, surfaced through `getStats`.
pub mod stats;
/// Active, passive and simultaneous-open, for ICE-TCP candidates.
pub mod tcp_type;
/// Parsing `stun:`/`turn:` server URLs.
pub mod url;

pub use agent::{
    Agent, Credentials, Event,
    agent_config::AgentConfig,
    agent_stats::{CandidatePairStats, CandidateStats},
};
