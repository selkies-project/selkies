#![warn(rust_2018_idioms)]
#![warn(missing_docs)]
#![allow(dead_code)]

//! TURN for the Sans-I/O WebRTC stack.
//!
//! Traversal Using Relays around NAT ([RFC 5766]) with IPv6 support ([RFC 6156]). TURN is
//! ICE's fallback: when no direct path between two peers can be found, each relays its
//! media through a server, which allocates a public address on their behalf.
//!
//! # Structure
//!
//! * [`client`] — the Sans-I/O client: allocate a relayed address, create permissions,
//!   bind channels, and send or receive through the allocation. It owns no sockets.
//! * [`proto`] — the TURN-specific STUN attributes and methods (`ALLOCATE`,
//!   `CREATE-PERMISSION`, `CHANNEL-BIND`, `XOR-RELAYED-ADDRESS`, ChannelData framing),
//!   built on [`rtc-stun`].
//!
//! # Example
//!
//! A client is configured with the server to allocate from and the long-term credentials to
//! authenticate with; driving it is then a matter of feeding it datagrams and polling for
//! [`Event`](client::Event)s:
//!
//! ```
//! use rtc_turn::client::ClientConfig;
//! use shared::TransportProtocol;
//!
//! let config = ClientConfig {
//!     turn_serv_addr: "turn.example.com:3478".to_owned(),
//!     local_addr: "0.0.0.0:0".parse().unwrap(),
//!     transport_protocol: TransportProtocol::UDP,
//!     username: "user".to_owned(),
//!     password: "pass".to_owned(),
//!     realm: "example.com".to_owned(),
//!     stun_serv_addr: String::new(), // optional: only for Binding requests
//!     software: String::new(),
//!     rto_in_ms: 0, // 0 selects the default retransmission timeout
//!     allocation_refresh_interval_cap: None,
//! };
//! assert_eq!(config.turn_serv_addr, "turn.example.com:3478");
//! ```
//!
//! Most applications do not depend on this crate directly — [`rtc-ice`] gathers relay
//! candidates through it, and the [`rtc`](https://docs.rs/rtc) crate drives that.
//!
//! [RFC 5766]: https://datatracker.ietf.org/doc/html/rfc5766
//! [RFC 6156]: https://datatracker.ietf.org/doc/html/rfc6156
//! [`rtc-stun`]: https://docs.rs/rtc-stun
//! [`rtc-ice`]: https://docs.rs/rtc-ice

/// The Sans-I/O TURN client: allocate a relayed address and send through it.
/// The crypto provider API.
///
/// Re-exported because this crate's public constructors take an
/// [`Arc<dyn RTCCryptoProvider>`](crypto::RTCCryptoProvider), which a caller must be able to name
/// without adding — and version-matching — a direct `rtc-crypto` dependency.
pub use crypto;

pub mod client;
/// The TURN-specific STUN attributes, methods and ChannelData framing.
pub mod proto;
