//! SCTP for the Sans-I/O WebRTC stack.
//!
//! The Stream Control Transmission Protocol ([RFC 4960]) with the extensions WebRTC data
//! channels need: partial reliability ([RFC 3758]) and stream reset / reconfiguration
//! ([RFC 6525]). In WebRTC, SCTP runs *over* DTLS rather than over IP, and carries the data
//! channels described by [`rtc-datachannel`].
//!
//! This is a fully deterministic implementation of the protocol logic. It contains no
//! networking code and reads no clock of its own: you feed it datagrams and time, and poll it
//! for the datagrams and events it produces. That is what makes it testable without a network
//! and reusable under any executor.
//!
//! # Structure
//!
//! * [`Endpoint`] — the protocol state for one socket. It holds configuration and dispatches
//!   inbound datagrams to the right association.
//! * [`Association`] — the bulk of the logic for a single
//!   association: handshake, congestion control, retransmission, and its streams.
//! * [`Stream`] — one stream's reads, writes and reliability
//!   settings.
//! * [`Chunks`] — a reassembled inbound message, delivered once every fragment has arrived.
//!
//! # Example
//!
//! Configuration is plain data, and the association is driven entirely by the caller — feed it
//! datagrams and time, poll it for output:
//!
//! ```
//! use rtc_sctp::{EndpointConfig, TransportConfig};
//! use std::sync::Arc;
//!
//! let transport = TransportConfig::default()
//!     .with_max_message_size(65_536)
//!     .with_max_num_outbound_streams(1024);
//!
//! let endpoint_config = Arc::new(EndpointConfig::new());
//! assert_eq!(transport.max_message_size(), 65_536);
//! # let _ = endpoint_config;
//! ```
//!
//! Most applications do not depend on this crate directly — the [`rtc`](https://docs.rs/rtc)
//! crate drives it as one layer of the peer-connection pipeline and exposes data channels,
//! and [`webrtc`](https://docs.rs/webrtc) wraps that in an async API.
//!
//! [RFC 4960]: https://datatracker.ietf.org/doc/html/rfc4960
//! [RFC 3758]: https://datatracker.ietf.org/doc/html/rfc3758
//! [RFC 6525]: https://datatracker.ietf.org/doc/html/rfc6525
//! [`rtc-datachannel`]: https://docs.rs/rtc-datachannel

#![warn(rust_2018_idioms)]
#![warn(missing_docs)]
#![allow(dead_code)]
#![allow(clippy::bool_to_int_with_if)]

use bytes::Bytes;
use std::{fmt, ops};

mod association;
pub use crate::association::{
    Association, AssociationError, Event,
    stats::AssociationStats,
    stream::{ReliabilityType, Stream, StreamEvent, StreamId, StreamState},
    timer::TimerConfig,
};

pub(crate) mod chunk;
pub use crate::chunk::{
    ErrorCauseCode,
    chunk_payload_data::{ChunkPayloadData, PayloadProtocolIdentifier},
};

mod config;
pub use crate::config::{
    ClientConfig, EndpointConfig, MIN_DATA_PACKET_MTU, ServerConfig, TransportConfig,
    max_payload_size_for_mtu,
};

mod endpoint;
pub use crate::endpoint::{AssociationHandle, ConnectError, DatagramEvent, Endpoint};

mod packet;

mod shared;
pub use crate::shared::{AssociationEvent, AssociationId, EndpointEvent};

pub(crate) mod param;

pub(crate) mod queue;
pub use crate::queue::reassembly_queue::{Chunk, Chunks};

pub(crate) mod util;

/// Entry points for fuzz targets and benchmarks.
///
/// Thin wrappers that drive one encode or decode step over a raw byte slice, so a fuzzer or
/// a benchmark can reach the packet codec without setting up an association. Gated behind
/// `cfg(fuzzing)` or the `bench` feature; not part of the supported API.
#[cfg(any(fuzzing, feature = "bench"))]
pub mod fuzzing;

/// Whether an endpoint was the initiator of an association
#[derive(Default, Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Side {
    /// The initiator of an association
    #[default]
    Client = 0,
    /// The acceptor of an association
    Server = 1,
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match *self {
            Side::Client => "Client",
            Side::Server => "Server",
        };
        write!(f, "{}", s)
    }
}

impl Side {
    #[inline]
    /// Shorthand for `self == Side::Client`
    pub fn is_client(self) -> bool {
        self == Side::Client
    }

    #[inline]
    /// Shorthand for `self == Side::Server`
    pub fn is_server(self) -> bool {
        self == Side::Server
    }
}

impl ops::Not for Side {
    type Output = Side;
    fn not(self) -> Side {
        match self {
            Side::Client => Side::Server,
            Side::Server => Side::Client,
        }
    }
}

use crate::packet::PartialDecode;

/// Payload in Incoming/outgoing Transmit
#[derive(Debug)]
#[non_exhaustive]
pub enum Payload {
    /// An inbound packet whose header has been decoded but whose chunks have not.
    PartialDecode(PartialDecode),
    /// Outbound packets, already encoded and ready to hand to the transport.
    RawEncode(Vec<Bytes>),
}
