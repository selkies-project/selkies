use crate::util::{AssociationIdGenerator, RandomAssociationIdGenerator};

use crate::TimerConfig;
use std::fmt;
use std::sync::Arc;

/// MTU for inbound packet (from DTLS)
pub(crate) const RECEIVE_MTU: usize = 8192;
/// Initial MTU for outgoing packets (to DTLS): bounds the assembled SCTP
/// packet (common header + bundled chunks). Each SCTP packet becomes one
/// DTLS record carried in one UDP datagram, so the wire size is roughly
/// `INITIAL_MTU` + ~37 bytes of DTLS record overhead + 48 bytes of IPv6/UDP
/// headers. 1191 keeps that within the 1280-byte IPv6 minimum MTU advertised
/// by common tunnel paths (WireGuard, Tailscale); the previous 1228
/// overflowed it (~1313 wire bytes), and because retransmissions re-bundle
/// to the same oversized packet, a dropped flight stalled forever with no
/// surfaced error. 1191 is the exact TURN-relayed IPv6 budget
/// (1280 - 40 IPv6 - 8 UDP - 4 TURN ChannelData - 37 DTLS), the derivation
/// from pion/sctp#476, adopted by webrtc-rs/webrtc#807 (webrtc v0.17.2).
pub(crate) const INITIAL_MTU: u32 = 1191;
pub(crate) const INITIAL_RECV_BUF_SIZE: u32 = 1024 * 1024;
pub(crate) const COMMON_HEADER_SIZE: u32 = 12;
pub(crate) const DATA_CHUNK_HEADER_SIZE: u32 = 16;
/// Floor for [`max_payload_size_for_mtu`]: the smallest representable padded
/// DATA packet — the 12-byte common header, the 16-byte DATA chunk header, and
/// a 1..=4-byte payload padded to the 4-byte chunk boundary.
pub const MIN_DATA_PACKET_MTU: u32 = COMMON_HEADER_SIZE + DATA_CHUNK_HEADER_SIZE + 4;

/// Derive an [`EndpointConfig::max_payload_size`] value from an outbound
/// DATA-packet budget: the largest DATA packet (common header plus bundled
/// DATA chunks) associations may emit, in bytes. Control packets (INIT,
/// INIT ACK, SACK, RECONFIG, FORWARD TSN, shutdown) marshal without
/// consulting this value.
///
/// This is the inverse of the default derivation: `mtu` minus the two fixed
/// headers, rounded down to the SCTP 4-byte chunk-padding boundary, so a
/// single maximum-size DATA chunk always marshals to at most `mtu` bytes —
/// exactly as the default budget does for `INITIAL_MTU` (1191, the
/// TURN-relayed IPv6 minimum-MTU budget). Budgets below
/// [`MIN_DATA_PACKET_MTU`] cannot satisfy that promise and are raised to it
/// with a warning.
pub fn max_payload_size_for_mtu(mtu: u32) -> u32 {
    if mtu < MIN_DATA_PACKET_MTU {
        log::warn!(
            "sctp mtu {mtu} is below the smallest representable DATA packet; raising to \
             {MIN_DATA_PACKET_MTU} bytes"
        );
    }
    let mtu = mtu.max(MIN_DATA_PACKET_MTU);
    (mtu - (COMMON_HEADER_SIZE + DATA_CHUNK_HEADER_SIZE)) & !3
}
pub(crate) const DEFAULT_MAX_MESSAGE_SIZE: u32 = 262144;
/// Ceiling on what the inbound streams buffer together before the association is aborted: a
/// memory backstop, not per-message policing. Under burst loss an ordered stream legitimately
/// buffers several complete messages behind one head-of-line gap while retransmits fill it,
/// so the bound is generous; only a peer that ignores both the advertised window and the
/// message size reaches it.
pub(crate) const DEFAULT_MAX_REASSEMBLY_BYTES: u32 = 16 * 1024 * 1024;

/// Config collects the arguments to create_association construction into
/// a single structure
#[derive(Debug, Clone)]
pub struct TransportConfig {
    sctp_port: u16,
    max_receive_buffer_size: u32,
    max_message_size: u32,
    max_reassembly_bytes: u32,
    max_num_outbound_streams: u16,
    max_num_inbound_streams: u16,
    timer_config: TimerConfig,
}

impl Default for TransportConfig {
    fn default() -> Self {
        TransportConfig {
            sctp_port: 5000,
            max_receive_buffer_size: INITIAL_RECV_BUF_SIZE,
            max_message_size: DEFAULT_MAX_MESSAGE_SIZE,
            max_reassembly_bytes: DEFAULT_MAX_REASSEMBLY_BYTES,
            max_num_outbound_streams: u16::MAX,
            max_num_inbound_streams: u16::MAX,
            timer_config: TimerConfig::default(),
        }
    }
}

impl TransportConfig {
    /// Sets the SCTP port. WebRTC always uses 5000.
    pub fn with_sctp_port(mut self, value: u16) -> Self {
        self.sctp_port = value;
        self
    }

    /// Sets the advertised receive window (a_rwnd), bounding how much unacknowledged data a peer
    /// may have in flight toward this endpoint.
    pub fn with_max_receive_buffer_size(mut self, value: u32) -> Self {
        self.max_receive_buffer_size = value;
        self
    }

    /// Sets the largest message this endpoint will accept.
    pub fn with_max_message_size(mut self, value: u32) -> Self {
        self.max_message_size = value;
        self
    }

    /// Sets the ceiling on what every inbound stream buffers together before the association
    /// is aborted (see [`DEFAULT_MAX_REASSEMBLY_BYTES`]).
    pub fn with_max_reassembly_bytes(mut self, value: u32) -> Self {
        self.max_reassembly_bytes = value;
        self
    }

    /// Sets how many outbound streams to request during the handshake.
    pub fn with_max_num_outbound_streams(mut self, value: u16) -> Self {
        self.max_num_outbound_streams = value;
        self
    }

    /// Sets how many inbound streams this endpoint will accept.
    pub fn with_max_num_inbound_streams(mut self, value: u16) -> Self {
        self.max_num_inbound_streams = value;
        self
    }

    /// Overrides the retransmission limits; see [`TimerConfig`].
    pub fn with_timer_config(mut self, value: TimerConfig) -> Self {
        self.timer_config = value;
        self
    }

    /// The configured SCTP port.
    pub fn sctp_port(&self) -> u16 {
        self.sctp_port
    }

    /// The configured receive window in bytes.
    pub fn max_receive_buffer_size(&self) -> u32 {
        self.max_receive_buffer_size
    }

    /// The configured maximum message size in bytes.
    pub fn max_message_size(&self) -> u32 {
        self.max_message_size
    }

    /// The configured reassembly ceiling in bytes.
    pub fn max_reassembly_bytes(&self) -> u32 {
        self.max_reassembly_bytes
    }

    /// The configured outbound stream count.
    pub fn max_num_outbound_streams(&self) -> u16 {
        self.max_num_outbound_streams
    }

    /// The configured inbound stream count.
    pub fn max_num_inbound_streams(&self) -> u16 {
        self.max_num_inbound_streams
    }

    /// The configured retransmission limits.
    pub fn timer_config(&self) -> TimerConfig {
        self.timer_config
    }
}

/// Global configuration for the endpoint, affecting all associations
///
/// Default values should be suitable for most internet applications.
#[derive(Clone)]
pub struct EndpointConfig {
    pub(crate) max_payload_size: u32,

    /// AID generator factory
    ///
    /// Create a aid generator for local aid in Endpoint struct
    pub(crate) aid_generator_factory:
        Arc<dyn (Fn() -> Box<dyn AssociationIdGenerator + Send>) + Send + Sync>,
}

impl Default for EndpointConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl EndpointConfig {
    /// Create a default configuration
    pub fn new() -> Self {
        let aid_factory: fn() -> Box<dyn AssociationIdGenerator + Send> =
            || Box::<RandomAssociationIdGenerator>::default();
        Self {
            max_payload_size: max_payload_size_for_mtu(INITIAL_MTU),
            aid_generator_factory: Arc::new(aid_factory),
        }
    }

    /// Supply a custom Association ID generator factory
    ///
    /// Called once by each `Endpoint` constructed from this configuration to obtain the AID
    /// generator which will be used to generate the AIDs used for incoming packets on all
    /// associations involving that  `Endpoint`. A custom AID generator allows applications to embed
    /// information in local association IDs, e.g. to support stateless packet-level load balancers.
    ///
    /// `EndpointConfig::new()` applies a default random AID generator factory. This functions
    /// accepts any customized AID generator to reset AID generator factory that implements
    /// the `AssociationIdGenerator` trait.
    pub fn aid_generator<
        F: Fn() -> Box<dyn AssociationIdGenerator + Send> + Send + Sync + 'static,
    >(
        &mut self,
        factory: F,
    ) -> &mut Self {
        self.aid_generator_factory = Arc::new(factory);
        self
    }

    /// Maximum payload size accepted from peers.
    ///
    /// The default is suitable for typical internet applications. Applications which expect to run
    /// on networks supporting Ethernet jumbo frames or similar should set this appropriately.
    pub fn max_payload_size(&mut self, value: u32) -> &mut Self {
        self.max_payload_size = value;
        self
    }

    /// Get the current value of `max_payload_size`
    ///
    /// While most parameters don't need to be readable, this must be exposed to allow higher-level
    /// layers to determine how large a receive buffer to allocate to
    /// support an externally-defined `EndpointConfig`.
    ///
    /// While `get_` accessors are typically unidiomatic in Rust, we favor concision for setters,
    /// which will be used far more heavily.
    pub fn get_max_payload_size(&self) -> u32 {
        self.max_payload_size
    }
}

impl fmt::Debug for EndpointConfig {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("EndpointConfig")
            .field("max_payload_size", &self.max_payload_size)
            .field("aid_generator_factory", &"[ elided ]")
            .finish()
    }
}

/// Parameters governing incoming associations
///
/// Default values should be suitable for most internet applications.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Transport configuration to use for incoming associations
    pub transport: Arc<TransportConfig>,

    /// Maximum number of concurrent associations
    pub(crate) concurrent_associations: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            transport: Arc::new(TransportConfig::default()),
            concurrent_associations: 100_000,
        }
    }
}

impl ServerConfig {
    /// Create a default configuration with a particular handshake token key
    pub fn new(transport: TransportConfig) -> Self {
        ServerConfig {
            transport: Arc::new(transport),
            concurrent_associations: 100_000,
        }
    }
}

/// Configuration for outgoing associations
///
/// Default values should be suitable for most internet applications.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Transport configuration to use
    pub transport: Arc<TransportConfig>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            transport: Arc::new(TransportConfig::default()),
        }
    }
}

impl ClientConfig {
    /// Create a default configuration with a particular cryptographic configuration
    pub fn new(transport: TransportConfig) -> Self {
        ClientConfig {
            transport: Arc::new(transport),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_max_payload_size_fits_ipv6_min_mtu() {
        // INITIAL_MTU bounds the assembled SCTP packet. The default is the
        // exact TURN-relayed IPv6 budget (pion/sctp#476, webrtc-rs/webrtc#807):
        // the 1280-byte IPv6 minimum MTU (RFC 8200) minus IPv6, UDP, and TURN
        // ChannelData headers and DTLS record overhead.
        assert_eq!(INITIAL_MTU, 1280 - 40 - 8 - 4 - 37);
        let config = EndpointConfig::default();
        assert_eq!(
            config.get_max_payload_size(),
            (INITIAL_MTU - (COMMON_HEADER_SIZE + DATA_CHUNK_HEADER_SIZE)) & !3
        );
        // A single maximum-size DATA chunk must marshal within INITIAL_MTU:
        // common header + the chunk header and payload, padded to 4 bytes.
        let padded_chunk =
            (DATA_CHUNK_HEADER_SIZE + config.get_max_payload_size()).next_multiple_of(4);
        assert!(COMMON_HEADER_SIZE + padded_chunk <= INITIAL_MTU);
    }

    #[test]
    fn max_payload_size_for_mtu_derives_the_payload_budget() {
        // `max_payload_size_for_mtu(INITIAL_MTU)` must reproduce the default
        // payload budget exactly.
        assert_eq!(
            max_payload_size_for_mtu(INITIAL_MTU),
            EndpointConfig::default().get_max_payload_size()
        );

        // Rounded down to the 4-byte padding boundary: the last MTU of one
        // padding window and the first of the next straddle a 4-byte step.
        assert_eq!(max_payload_size_for_mtu(1203), 1172);
        assert_eq!(max_payload_size_for_mtu(1204), 1176);

        // Values below MIN_DATA_PACKET_MTU behave exactly as the 32-byte
        // floor: the smallest representable padded DATA packet, a 4-byte
        // payload budget.
        for mtu in [0, 31, 32] {
            assert_eq!(max_payload_size_for_mtu(mtu), 4, "mtu {mtu}");
        }

        // Upper edge: the derivation must not overflow.
        assert_eq!(
            max_payload_size_for_mtu(u32::MAX),
            (u32::MAX - (COMMON_HEADER_SIZE + DATA_CHUNK_HEADER_SIZE)) & !3
        );
    }
}
