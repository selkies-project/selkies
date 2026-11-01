use crate::association::{
    state::{AckMode, AckState, AssociationState},
    stats::AssociationStats,
};
use crate::chunk::chunk_header::CHUNK_HEADER_SIZE;
use crate::chunk::{
    Chunk, ErrorCause, ErrorCauseUnrecognizedChunkType, USER_INITIATED_ABORT,
    chunk_abort::ChunkAbort,
    chunk_cookie_ack::ChunkCookieAck,
    chunk_cookie_echo::ChunkCookieEcho,
    chunk_error::ChunkError,
    chunk_forward_tsn::ChunkForwardTsn,
    chunk_forward_tsn::ChunkForwardTsnStream,
    chunk_heartbeat::ChunkHeartbeat,
    chunk_heartbeat_ack::ChunkHeartbeatAck,
    chunk_init::ChunkInit,
    chunk_init::ChunkInitAck,
    chunk_payload_data::ChunkPayloadData,
    chunk_payload_data::{MessageId, MessageReliability, PayloadProtocolIdentifier},
    chunk_reconfig::ChunkReconfig,
    chunk_selective_ack::ChunkSelectiveAck,
    chunk_shutdown::ChunkShutdown,
    chunk_shutdown_ack::ChunkShutdownAck,
    chunk_shutdown_complete::ChunkShutdownComplete,
    chunk_type::CT_FORWARD_TSN,
};
use crate::config::{COMMON_HEADER_SIZE, DATA_CHUNK_HEADER_SIZE, ServerConfig, TransportConfig};
use crate::packet::{CommonHeader, Packet};
use crate::param::{
    Param,
    param_heartbeat_info::ParamHeartbeatInfo,
    param_outgoing_reset_request::ParamOutgoingResetRequest,
    param_reconfig_response::{ParamReconfigResponse, ReconfigResult},
    param_state_cookie::ParamStateCookie,
    param_supported_extensions::ParamSupportedExtensions,
};
use crate::queue::{
    payload_queue::PayloadQueue,
    pending_queue::{PendingQueue, ResetMarker},
};
use crate::shared::{AssociationEventInner, AssociationId, EndpointEvent, EndpointEventInner};
use crate::util::{constant_time_eq, sna16lt, sna32gt, sna32gte, sna32lt, sna32lte};
use crate::{AssociationEvent, Payload, Side};
use shared::error::{Error, Result};
use shared::{TransportContext, TransportMessage, TransportProtocol};
use stream::{Stream, StreamEvent, StreamId, StreamState};
use timer::{ACK_INTERVAL, RtoManager, Timer, TimerTable};

use crate::association::stream::RecvSendState;
use bytes::{Bytes, BytesMut};
use log::{debug, error, trace, warn};
use rand::random;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;

pub(crate) mod state;
pub(crate) mod stats;
pub(crate) mod stream;
pub(crate) mod timer;

#[cfg(test)]
mod association_test;

/// Reasons why an association might be lost
#[derive(Debug, Error, Clone, PartialEq)]
#[non_exhaustive]
pub enum AssociationError {
    /// Handshake failed
    #[error("handshake failed due to {0}")]
    HandshakeFailed(String),
    /// The peer violated the QUIC specification as understood by this implementation
    #[error("transport error")]
    TransportError,
    /// The peer's QUIC stack aborted the association automatically
    #[error("aborted by peer")]
    AssociationClosed,
    /// The peer closed the association
    #[error("closed by peer")]
    ApplicationClosed,
    /// The peer is unable to continue processing this association, usually due to having restarted
    #[error("reset by peer")]
    Reset,
    /// Communication with the peer has lapsed for longer than the negotiated idle timeout
    ///
    /// If neither side is sending keep-alives, an association will time out after a long enough idle
    /// period even if the peer is still reachable
    #[error("timed out")]
    TimedOut,
    /// The local application closed the association
    #[error("closed")]
    LocallyClosed,
}

/// Events of interest to the application
#[derive(Debug)]
#[non_exhaustive]
pub enum Event {
    /// Handshake was failed
    HandshakeFailed {
        /// Reason that the association was closed
        reason: AssociationError,
    },

    /// The association was successfully established
    Connected,
    /// The association was lost
    ///
    /// Emitted if the peer closes the association or an error is encountered.
    AssociationLost {
        /// Reason that the association was closed
        reason: AssociationError,
        /// The stream the loss was reported against.
        id: StreamId,
    },
    /// Stream events
    Stream(StreamEvent),
    /// One or more application datagrams have been received
    DatagramReceived,
}

/// Selection is stable even after fragments leave either queue.
#[derive(Debug, Copy, Clone)]
struct MessageToAbandon {
    id: MessageId,
    // TSN of an inflight candidate; None when selected from pending.
    // This permits a direct lookup for whole messages. Fragmented messages
    // and pending selections use the index keyed by `id`.
    sent_tsn: Option<u32>,
}

///Association represents an SCTP association
//13.2.  Parameters Necessary per Association (i.e., the TCB)
//Peer : Tag value to be sent in every packet and is received
//Verification: in the INIT or INIT ACK chunk.
//Tag :
//
//My : Tag expected in every inbound packet and sent in the
//Verification: INIT or INIT ACK chunk.
//
//Tag :
//State : A state variable indicating what state the association
// : is in, i.e., COOKIE-WAIT, COOKIE-ECHOED, ESTABLISHED,
// : SHUTDOWN-PENDING, SHUTDOWN-SENT, SHUTDOWN-RECEIVED,
// : SHUTDOWN-ACK-SENT.
//
// No Closed state is illustrated since if a
// association is Closed its TCB SHOULD be removed.
pub struct Association {
    side: Side,
    state: AssociationState,
    handshake_completed: bool,
    max_message_size: u32,
    inflight_queue_length: usize,
    will_send_shutdown: bool,
    bytes_received: usize,
    bytes_sent: usize,

    pub(crate) peer_verification_tag: u32,
    my_verification_tag: u32,
    pub(crate) my_next_tsn: u32,
    peer_last_tsn: u32,
    // for RTT measurement
    min_tsn2measure_rtt: u32,
    will_send_forward_tsn: bool,
    will_retransmit_fast: bool,
    will_retransmit_reconfig: bool,
    /// True only while the in-flight queue may still hold chunks flagged for
    /// T3-rtx retransmission (set when the timer marks them, cleared once the
    /// scan has re-sent them all). Lets `gather_outbound` skip the O(in-flight)
    /// retransmit scan entirely in the steady state, where nothing is ever
    /// marked — that scan was the single hottest function in the send profile.
    t3_retransmit_pending: bool,

    will_send_shutdown_ack: bool,
    will_send_shutdown_complete: bool,

    // Reconfig
    my_next_rsn: u32,
    reconfigs: HashMap<u32, ChunkReconfig>,
    reconfig_requests: HashMap<u32, ParamOutgoingResetRequest>,

    // Non-RFC internal data
    remote_addr: SocketAddr,
    local_addr: SocketAddr,
    transport_protocol: TransportProtocol,

    pub(crate) source_port: u16,
    pub(crate) destination_port: u16,
    my_max_num_inbound_streams: u16,
    my_max_num_outbound_streams: u16,
    my_cookie: Option<ParamStateCookie>,

    pub(crate) payload_queue: PayloadQueue,
    inflight_queue: PayloadQueue,
    pending_queue: PendingQueue,
    control_queue: VecDeque<Packet>,
    stream_queue: VecDeque<u16>,

    pub(crate) mtu: u32,
    // max DATA chunk payload size
    max_payload_size: u32,
    cumulative_tsn_ack_point: u32,
    advanced_peer_tsn_ack_point: u32,
    use_forward_tsn: bool,
    /// Max stream-sequence-number per *ordered* stream among abandoned chunks
    /// currently in the forward-TSN window `(cumulative_tsn_ack_point,
    /// advanced_peer_tsn_ack_point]`. Maintained incrementally as chunks are
    /// abandoned (the two RFC 3758 C2 loops) so that `create_forward_tsn` is
    /// O(streams) instead of rescanning the whole in-flight window — which, for
    /// PR-SCTP data channels, ran ~1000 hashmap probes per FORWARD-TSN and was
    /// ~9% of send CPU in profiles. Unordered chunks are omitted: the receiver
    /// ignores the per-stream list for them (it advances by `new_cumulative_tsn`
    /// alone), so reporting them was pure waste.
    fwd_tsn_stream_map: FxHashMap<u16, u16>,

    pub(crate) rto_mgr: RtoManager,
    timers: TimerTable,

    // Congestion control parameters
    max_receive_buffer_size: u32,
    max_reassembly_bytes: u32,
    /// Streams the peer's INIT or INIT ACK opened for its sends; 0 before it named a count.
    peer_num_outbound_streams: u16,
    // my congestion window size
    pub(crate) cwnd: u32,
    // calculated peer's receiver windows size
    rwnd: u32,
    // slow start threshold
    pub(crate) ssthresh: u32,
    partial_bytes_acked: u32,
    pub(crate) in_fast_recovery: bool,
    fast_recover_exit_point: u32,

    // Chunks stored for retransmission
    stored_init: Option<ChunkInit>,
    stored_cookie_echo: Option<ChunkCookieEcho>,
    next_stream_generation: u64,
    next_message_id: u64,
    /// Per-chunk lookups on the receive path; SIDs are bounded by the
    /// negotiated stream count, so the faster non-SipHash hasher is safe.
    pub(crate) streams: FxHashMap<StreamId, StreamState>,

    events: VecDeque<Event>,
    endpoint_events: VecDeque<EndpointEventInner>,
    error: Option<AssociationError>,

    // per inbound packet context
    delayed_ack_triggered: bool,
    immediate_ack_triggered: bool,

    pub(crate) stats: AssociationStats,
    ack_state: AckState,

    // for testing
    pub(crate) ack_mode: AckMode,
}

impl Default for Association {
    fn default() -> Self {
        Association {
            side: Side::default(),
            state: AssociationState::default(),
            handshake_completed: false,
            max_message_size: 0,
            inflight_queue_length: 0,
            will_send_shutdown: false,
            bytes_received: 0,
            bytes_sent: 0,

            peer_verification_tag: 0,
            my_verification_tag: 0,
            my_next_tsn: 0,
            peer_last_tsn: 0,
            // for RTT measurement
            min_tsn2measure_rtt: 0,
            will_send_forward_tsn: false,
            will_retransmit_fast: false,
            will_retransmit_reconfig: false,
            t3_retransmit_pending: false,

            will_send_shutdown_ack: false,
            will_send_shutdown_complete: false,

            // Reconfig
            my_next_rsn: 0,
            reconfigs: HashMap::default(),
            reconfig_requests: HashMap::default(),

            // Non-RFC internal data
            remote_addr: SocketAddr::from_str("0.0.0.0:0").unwrap(),
            local_addr: SocketAddr::from_str("0.0.0.0:0").unwrap(),
            transport_protocol: TransportProtocol::UDP,

            source_port: 0,
            destination_port: 0,
            my_max_num_inbound_streams: 0,
            my_max_num_outbound_streams: 0,
            my_cookie: None,

            payload_queue: PayloadQueue::default(),
            inflight_queue: PayloadQueue::default(),
            pending_queue: PendingQueue::default(),
            control_queue: VecDeque::default(),
            stream_queue: VecDeque::default(),

            mtu: 0,
            // max DATA chunk payload size
            max_payload_size: 0,
            cumulative_tsn_ack_point: 0,
            advanced_peer_tsn_ack_point: 0,
            use_forward_tsn: false,
            fwd_tsn_stream_map: FxHashMap::default(),

            rto_mgr: RtoManager::default(),
            timers: TimerTable::default(),

            // Congestion control parameters
            max_receive_buffer_size: 0,
            max_reassembly_bytes: 0,
            peer_num_outbound_streams: 0,
            // my congestion window size
            cwnd: 0,
            // calculated peer's receiver windows size
            rwnd: 0,
            // slow start threshold
            ssthresh: 0,
            partial_bytes_acked: 0,
            in_fast_recovery: false,
            fast_recover_exit_point: 0,

            // Chunks stored for retransmission
            stored_init: None,
            stored_cookie_echo: None,
            next_stream_generation: 0,
            next_message_id: 0,
            streams: FxHashMap::default(),

            events: VecDeque::default(),
            endpoint_events: VecDeque::default(),
            error: None,

            // per inbound packet context
            delayed_ack_triggered: false,
            immediate_ack_triggered: false,

            stats: AssociationStats::default(),
            ack_state: AckState::default(),

            // for testing
            ack_mode: AckMode::default(),
        }
    }
}

impl Association {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        server_config: Option<Arc<ServerConfig>>,
        config: Arc<TransportConfig>,
        max_payload_size: u32,
        local_aid: AssociationId,
        remote_addr: SocketAddr,
        local_addr: SocketAddr,
        protocol: TransportProtocol,
        now: Instant,
    ) -> Self {
        let side = if server_config.is_some() {
            Side::Server
        } else {
            Side::Client
        };

        // It's a bit strange, but we're going backwards from the calculation in
        // config.rs to get max_payload_size from INITIAL_MTU.
        let mtu = max_payload_size.saturating_add(COMMON_HEADER_SIZE + DATA_CHUNK_HEADER_SIZE);

        // RFC 4960 Sec 7.2.1
        // The initial cwnd before DATA transmission or after a sufficiently
        // long idle period MUST be set to min(4*MTU, max (2*MTU, 4380bytes)).
        // Written in total form: the previous `(2 * mtu).clamp(4380, 4 * mtu)`
        // panicked for effective MTUs below ~1095 (`Ord::clamp` with
        // min > max) and overflowed near `u32::MAX`, both reachable through
        // `EndpointConfig::max_payload_size` and MTU-derived payload budgets.
        let cwnd = mtu.saturating_mul(2).max(4380).min(mtu.saturating_mul(4));
        // RFC 4960 requires an unpredictable initial TSN. SCTP remains usable without an RTC
        // crypto provider, so this deliberately uses `rand`'s thread-local CSPRNG.
        let mut tsn = random::<u32>();
        if tsn == 0 {
            tsn += 1;
        }

        let mut this = Association {
            side,
            handshake_completed: false,
            max_receive_buffer_size: config.max_receive_buffer_size(),
            max_reassembly_bytes: config.max_reassembly_bytes(),
            max_message_size: config.max_message_size(),
            my_max_num_outbound_streams: config.max_num_outbound_streams(),
            my_max_num_inbound_streams: config.max_num_inbound_streams(),
            max_payload_size,

            rto_mgr: RtoManager::new(),
            timers: TimerTable::new(config.timer_config()),

            mtu,
            cwnd,
            remote_addr,
            local_addr,
            transport_protocol: protocol,

            my_verification_tag: local_aid,
            my_next_tsn: tsn,
            my_next_rsn: tsn,
            min_tsn2measure_rtt: tsn,
            cumulative_tsn_ack_point: tsn - 1,
            advanced_peer_tsn_ack_point: tsn - 1,
            error: None,

            ..Default::default()
        };

        if side.is_client() {
            let mut init = ChunkInit {
                initial_tsn: this.my_next_tsn,
                num_outbound_streams: this.my_max_num_outbound_streams,
                num_inbound_streams: this.my_max_num_inbound_streams,
                initiate_tag: this.my_verification_tag,
                advertised_receiver_window_credit: this.max_receive_buffer_size,
                ..Default::default()
            };
            init.set_supported_extensions();

            this.set_state(AssociationState::CookieWait);
            this.stored_init = Some(init);
            let _ = this.send_init();
            this.timers
                .start(Timer::T1Init, now, this.rto_mgr.get_rto());
        }

        this
    }

    /// Returns application-facing event
    ///
    /// Associations should be polled for events after:
    /// - a call was made to `handle_event`
    /// - a call was made to `handle_timeout`
    #[must_use]
    pub fn poll(&mut self) -> Option<Event> {
        if let Some(x) = self.events.pop_front() {
            return Some(x);
        }

        /*TODO: if let Some(event) = self.streams.poll() {
            return Some(Event::Stream(event));
        }*/

        if let Some(err) = self.error.take() {
            return Some(Event::HandshakeFailed { reason: err });
        }

        None
    }

    /// Return endpoint-facing event
    #[must_use]
    pub fn poll_endpoint_event(&mut self) -> Option<EndpointEvent> {
        self.endpoint_events.pop_front().map(EndpointEvent)
    }

    /// Returns the next time at which `handle_timeout` should be called
    ///
    /// The value returned may change after:
    /// - the application performed some I/O on the association
    /// - a call was made to `handle_transmit`
    /// - a call to `poll_transmit` returned `Some`
    /// - a call was made to `handle_timeout`
    #[must_use]
    pub fn poll_timeout(&self) -> Option<Instant> {
        self.timers.next_timeout()
    }

    /// Returns packets to transmit
    ///
    /// Associations should be polled for transmit after:
    /// - the application performed some I/O on the Association
    /// - a call was made to `handle_event`
    /// - a call was made to `handle_timeout`
    #[must_use]
    pub fn poll_transmit(&mut self, now: Instant) -> Option<TransportMessage<Payload>> {
        let (contents, _) = self.gather_outbound(now);
        if contents.is_empty() {
            None
        } else {
            trace!(
                "[{}] sending {} bytes (total {} datagrams)",
                self.side,
                contents.iter().fold(0, |l, c| l + c.len()),
                contents.len()
            );
            Some(TransportMessage {
                now,
                transport: TransportContext {
                    local_addr: self.local_addr,
                    peer_addr: self.remote_addr,
                    ecn: None,
                    transport_protocol: Default::default(),
                },
                message: Payload::RawEncode(contents),
            })
        }
    }

    /// Process timer expirations
    ///
    /// Executes protocol logic, potentially preparing signals (including application `Event`s,
    /// `EndpointEvent`s and outgoing datagrams) that should be extracted through the relevant
    /// methods.
    ///
    /// It is most efficient to call this immediately after the system clock reaches the latest
    /// `Instant` that was output by `poll_timeout`; however spurious extra calls will simply
    /// no-op and therefore are safe.
    pub fn handle_timeout(&mut self, now: Instant) {
        for &timer in &Timer::VALUES {
            let (expired, failure, n_rtos) = self.timers.is_expired(timer, now);
            if !expired {
                continue;
            }
            self.timers.set(timer, None);
            //trace!("{:?} timeout", timer);

            if timer == Timer::Ack {
                self.on_ack_timeout();
            } else if failure {
                self.on_retransmission_failure(timer);
            } else {
                self.on_retransmission_timeout(timer, n_rtos, now);
                if self.state() == AssociationState::Closed {
                    break;
                }
                self.timers.start(timer, now, self.rto_mgr.get_rto());
            }
        }
    }

    /// Process `AssociationEvent`s generated by the associated `Endpoint`
    ///
    /// Will execute protocol logic upon receipt of an association event, in turn preparing signals
    /// (including application `Event`s, `EndpointEvent`s and outgoing datagrams) that should be
    /// extracted through the relevant methods.
    pub fn handle_event(&mut self, event: AssociationEvent) {
        match event.0 {
            AssociationEventInner::Datagram(transmit) => {
                // If this packet could initiate a migration and we're a client or a server that
                // forbids migration, drop the datagram. This could be relaxed to heuristically
                // permit NAT-rebinding-like migration.
                /*TODO:if remote != self.remote && self.server_config.as_ref().map_or(true, |x| !x.migration)
                {
                    trace!("discarding packet from unrecognized peer {}", remote);
                    return;
                }*/

                if let Payload::PartialDecode(partial_decode) = transmit.message {
                    debug!(
                        "[{}] recving {} bytes",
                        self.side,
                        COMMON_HEADER_SIZE as usize + partial_decode.remaining.len()
                    );

                    let pkt = match partial_decode.finish() {
                        Ok(p) => p,
                        Err(err) => {
                            warn!("[{}] unable to parse SCTP packet {}", self.side, err);
                            return;
                        }
                    };

                    if let Err(err) = self.handle_inbound(pkt, transmit.now) {
                        error!("handle_inbound got err: {}", err);
                        let _ = self.close(AssociationError::TransportError);
                    }
                } else {
                    trace!("discarding invalid partial_decode");
                }
            } //TODO:
        }
    }

    /// Returns Association statistics
    pub fn stats(&self) -> AssociationStats {
        self.stats
    }

    /// Whether the Association is in the process of being established
    ///
    /// If this returns `false`, the Association may be either established or closed, signaled by the
    /// emission of a `Connected` or `AssociationLost` message respectively.
    pub fn is_handshaking(&self) -> bool {
        !self.handshake_completed
    }

    /// The number of streams negotiated for this association: the smaller of the inbound and
    /// outbound counts the two endpoints agreed on in INIT/INIT ACK.
    ///
    /// `None` while the association is still handshaking. Before the peer's INIT (or INIT ACK) has
    /// been processed, the two counts still hold this endpoint's *configured* limits rather than
    /// anything agreed, so reporting them would overstate what the association can carry.
    ///
    /// This is what [RFC 8831] data channels are limited by, and what W3C
    /// `RTCSctpTransport.maxChannels` reports.
    ///
    /// [RFC 8831]: https://datatracker.ietf.org/doc/html/rfc8831
    pub fn negotiated_max_streams(&self) -> Option<u16> {
        if self.is_handshaking() {
            None
        } else {
            Some(
                self.my_max_num_inbound_streams
                    .min(self.my_max_num_outbound_streams),
            )
        }
    }

    /// Whether the Association is closed
    ///
    /// Closed Associations cannot transport any further data. An association becomes closed when
    /// either peer application intentionally closes it, or when either transport layer detects an
    /// error such as a time-out or certificate validation failure.
    ///
    /// A `AssociationLost` event is emitted with details when the association becomes closed.
    pub fn is_closed(&self) -> bool {
        self.state == AssociationState::Closed
    }

    /// Whether there is no longer any need to keep the association around
    ///
    /// Closed associations become drained after a brief timeout to absorb any remaining in-flight
    /// packets from the peer. All drained associations have been closed.
    pub fn is_drained(&self) -> bool {
        self.state.is_drained()
    }

    /// Look up whether we're the client or server of this Association
    pub fn side(&self) -> Side {
        self.side
    }

    /// The latest socket address for this Association's peer
    pub fn remote_addr(&self) -> SocketAddr {
        self.remote_addr
    }

    /// Smoothed round-trip time, or zero before the first RTT measurement.
    /// Retransmission timeout backoff does not change this estimate.
    pub fn rtt(&self) -> Duration {
        Duration::from_millis(self.rto_mgr.srtt)
    }

    /// The local IP address which was used when the peer established
    /// the association
    ///
    /// This can be different from the address the endpoint is bound to, in case
    /// the endpoint is bound to a wildcard address like `0.0.0.0` or `::`.
    ///
    /// This will return `None` for clients.
    ///
    /// Retrieving the local IP address is currently supported on the following
    /// platforms:
    /// - Linux
    ///
    /// On all non-supported platforms the local IP address will not be available,
    /// and the method will return `None`.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Shutdown initiates the shutdown sequence. The method blocks until the
    /// shutdown sequence is completed and the association is closed, or until the
    /// passed context is done, in which case the context's error is returned.
    pub fn shutdown(&mut self) -> Result<()> {
        debug!("[{}] closing association..", self.side);

        let state = self.state();
        if state != AssociationState::Established {
            return Err(Error::ErrShutdownNonEstablished);
        }

        // Attempt a graceful shutdown.
        self.set_state(AssociationState::ShutdownPending);

        if self.inflight_queue_length == 0 {
            // No more outstanding, send shutdown.
            self.will_send_shutdown = true;
            self.awake_write_loop();
            self.set_state(AssociationState::ShutdownSent);
        }

        self.endpoint_events.push_back(EndpointEventInner::Drained);

        Ok(())
    }

    /// Aborts the association (RFC 9260 §9.1): queues an ABORT chunk and closes the association.
    ///
    /// Unlike [`shutdown`](Self::shutdown) this needs no round trip, which is what closing a
    /// WebRTC peer connection calls for: W3C `close()` tears the association down "by sending an
    /// SCTP ABORT chunk". The ABORT carries a "User-Initiated Abort" error cause (RFC 9260
    /// §3.3.10.12), which a peer takes as an intentional close rather than an error.
    ///
    /// The ABORT is sent by the next [`poll_transmit`](Self::poll_transmit). Anything not handed to
    /// `poll_transmit` before this call is discarded, so drain it first if it should still go out.
    /// A no-op on a closed association.
    pub fn abort(&mut self) -> Result<()> {
        if self.state() == AssociationState::Closed {
            return Ok(());
        }
        let abort = ChunkAbort {
            error_causes: vec![ErrorCause {
                code: USER_INITIATED_ABORT,
                ..Default::default()
            }],
        };
        let packet = self.create_packet(vec![Box::new(abort)]);
        self.control_queue.push_back(packet);
        self.close(AssociationError::LocallyClosed)
    }

    /// Close ends the SCTP Association and cleans up any state
    pub fn close(&mut self, reason: AssociationError) -> Result<()> {
        if self.state() != AssociationState::Closed {
            self.set_state(AssociationState::Closed);

            debug!("[{}] closing association..", self.side);

            self.close_all_timers();

            for si in self.streams.keys().cloned().collect::<Vec<u16>>() {
                self.unregister_stream(si, reason.clone());
            }

            debug!("[{}] association closed", self.side);
            debug!(
                "[{}] stats nDATAs (in) : {}",
                self.side,
                self.stats.get_num_datas()
            );
            debug!(
                "[{}] stats nSACKs (in) : {}",
                self.side,
                self.stats.get_num_sacks()
            );
            debug!(
                "[{}] stats nT3Timeouts : {}",
                self.side,
                self.stats.get_num_t3timeouts()
            );
            debug!(
                "[{}] stats nAckTimeouts: {}",
                self.side,
                self.stats.get_num_ack_timeouts()
            );
            debug!(
                "[{}] stats nFastRetrans: {}",
                self.side,
                self.stats.get_num_fast_retrans()
            );
        }

        Ok(())
    }

    /// open_stream opens a stream
    pub fn open_stream(
        &mut self,
        stream_identifier: StreamId,
        default_payload_type: PayloadProtocolIdentifier,
    ) -> Result<Stream<'_>> {
        if self.streams.contains_key(&stream_identifier) {
            return Err(Error::ErrStreamAlreadyExist);
        }

        if let Some(s) = self.create_stream(stream_identifier, false, default_payload_type) {
            Ok(s)
        } else {
            Err(Error::ErrStreamCreateFailed)
        }
    }

    /// accept_stream accepts a stream
    pub fn accept_stream(&mut self) -> Option<Stream<'_>> {
        self.stream_queue
            .pop_front()
            .map(move |stream_identifier| Stream {
                stream_identifier,
                association: self,
            })
    }

    /// stream returns a stream
    pub fn stream(&mut self, stream_identifier: StreamId) -> Result<Stream<'_>> {
        if !self.streams.contains_key(&stream_identifier) {
            Err(Error::ErrStreamNotExisted)
        } else {
            Ok(Stream {
                stream_identifier,
                association: self,
            })
        }
    }

    /// The identifiers of every stream currently open on this association.
    pub fn stream_ids(&self) -> Vec<StreamId> {
        self.streams.keys().cloned().collect()
    }

    /// bytes_sent returns the number of bytes sent
    pub(crate) fn bytes_sent(&self) -> usize {
        self.bytes_sent
    }

    /// bytes_received returns the number of bytes received
    pub(crate) fn bytes_received(&self) -> usize {
        self.bytes_received
    }

    /// max_message_size returns the maximum message size you can send.
    pub(crate) fn max_message_size(&self) -> u32 {
        self.max_message_size
    }

    /// set_max_message_size sets the maximum message size you can send.
    pub(crate) fn set_max_message_size(&mut self, max_message_size: u32) {
        self.max_message_size = max_message_size;
    }

    /// unregister_stream un-registers a stream from the association
    /// The caller should hold the association write lock.
    fn unregister_stream(&mut self, stream_identifier: StreamId, reason: AssociationError) {
        if let Some(mut s) = self.streams.remove(&stream_identifier) {
            debug!("[{}] unregister_stream {}", self.side, stream_identifier);
            self.events.push_back(Event::AssociationLost {
                reason,
                id: stream_identifier,
            });
            s.state = RecvSendState::Closed;
        }
    }

    /// Re-run any reset request that was answered In Progress because
    /// `stream_identifier` still had data the application had not collected
    /// Called when that stream drains.
    ///
    /// The reply is queued like any other control packet; a request naming
    /// several streams is retried as a whole and simply defers again if another
    /// of its streams is still readable.
    pub(crate) fn retry_deferred_resets(&mut self, stream_identifier: StreamId) {
        let pending: Vec<ParamOutgoingResetRequest> = self
            .reconfig_requests
            .values()
            .filter(|p| p.stream_identifiers.contains(&stream_identifier))
            .cloned()
            .collect();
        if pending.is_empty() {
            return;
        }

        let mut reply = vec![];
        for p in pending {
            if let Err(err) = self.reset_streams_if_any(&p, true, &mut reply) {
                debug!("[{}] retry of deferred reset failed: {:?}", self.side, err);
            }
        }
        for packet in reply {
            self.control_queue.push_back(packet);
        }
        self.awake_write_loop();
    }

    /// set_state atomically sets the state of the Association.
    fn set_state(&mut self, new_state: AssociationState) {
        if new_state != self.state {
            debug!(
                "[{}] state change: '{}' => '{}'",
                self.side, self.state, new_state,
            );
        }
        self.state = new_state;
    }

    /// state atomically returns the state of the Association.
    pub(crate) fn state(&self) -> AssociationState {
        self.state
    }

    /// caller must hold self.lock
    fn send_init(&mut self) -> Result<()> {
        if let Some(stored_init) = &self.stored_init {
            debug!("[{}] sending INIT", self.side);

            self.source_port = 5000; // Spec??
            self.destination_port = 5000; // Spec??

            let outbound = Packet {
                common_header: CommonHeader {
                    source_port: self.source_port,
                    destination_port: self.destination_port,
                    verification_tag: self.peer_verification_tag,
                },
                chunks: vec![Box::new(stored_init.clone())],
            };

            self.control_queue.push_back(outbound);
            self.awake_write_loop();

            Ok(())
        } else {
            Err(Error::ErrInitNotStoredToSend)
        }
    }

    /// caller must hold self.lock
    fn send_cookie_echo(&mut self) -> Result<()> {
        if let Some(stored_cookie_echo) = &self.stored_cookie_echo {
            debug!("[{}] sending COOKIE-ECHO", self.side);

            let outbound = Packet {
                common_header: CommonHeader {
                    source_port: self.source_port,
                    destination_port: self.destination_port,
                    verification_tag: self.peer_verification_tag,
                },
                chunks: vec![Box::new(stored_cookie_echo.clone())],
            };

            self.control_queue.push_back(outbound);
            self.awake_write_loop();

            Ok(())
        } else {
            Err(Error::ErrCookieEchoNotStoredToSend)
        }
    }

    /// handle_inbound parses incoming raw packets
    fn handle_inbound(&mut self, p: Packet, now: Instant) -> Result<()> {
        if let Err(err) = p.check_packet() {
            warn!("[{}] failed validating packet {}", self.side, err);
            return Ok(());
        }

        self.handle_chunk_start();

        for c in &p.chunks {
            if let Err(err) = self.handle_chunk(&p, c, now) {
                // Only an ABORT ends the association: a malformed or premature chunk of the
                // peer's is dropped, the rest of the packet and the association kept.
                if matches!(err, Error::ErrAbortChunk(_)) {
                    return Err(err);
                }
                debug!("[{}] dropping chunk: {}", self.side, err);
            }
        }

        self.handle_chunk_end(now);

        Ok(())
    }

    fn handle_chunk_start(&mut self) {
        self.delayed_ack_triggered = false;
        self.immediate_ack_triggered = false;
    }

    fn handle_chunk_end(&mut self, now: Instant) {
        if self.immediate_ack_triggered {
            self.ack_state = AckState::Immediate;
            self.timers.stop(Timer::Ack);
            self.awake_write_loop();
        } else if self.delayed_ack_triggered {
            // Will send delayed ack in the next ack timeout
            self.ack_state = AckState::Delay;
            self.timers.start(Timer::Ack, now, ACK_INTERVAL);
        }
    }

    #[allow(clippy::borrowed_box)]
    fn handle_chunk(&mut self, p: &Packet, chunk: &Box<dyn Chunk>, now: Instant) -> Result<()> {
        chunk.check()?;
        let chunk_any = chunk.as_any();
        let packets = if let Some(c) = chunk_any.downcast_ref::<ChunkInit>() {
            if c.is_ack {
                self.handle_init_ack(p, c, now)?
            } else {
                self.handle_init(p, c)?
            }
        } else if let Some(c) = chunk_any.downcast_ref::<ChunkAbort>() {
            let mut err_str = String::new();
            for e in &c.error_causes {
                if matches!(e.code, USER_INITIATED_ABORT) {
                    debug!("User initiated abort received");
                    let _ = self.close(AssociationError::Reset);
                    return Ok(());
                }
                err_str += &format!("({})", e);
            }
            return Err(Error::ErrAbortChunk(err_str));
        } else if let Some(c) = chunk_any.downcast_ref::<ChunkError>() {
            // An ERROR ends an association still being established; one about an established
            // association is the peer's complaint, noted and the association kept.
            let mut err_str = String::new();
            for e in &c.error_causes {
                err_str += &format!("({})", e);
            }
            if matches!(
                self.state(),
                AssociationState::CookieWait | AssociationState::CookieEchoed
            ) {
                return Err(Error::ErrAbortChunk(err_str));
            }
            debug!("[{}] ERROR chunk from the peer: {}", self.side, err_str);
            vec![]
        } else if let Some(c) = chunk_any.downcast_ref::<ChunkHeartbeat>() {
            self.handle_heartbeat(c)?
        } else if let Some(c) = chunk_any.downcast_ref::<ChunkCookieEcho>() {
            self.handle_cookie_echo(c)?
        } else if chunk_any.downcast_ref::<ChunkCookieAck>().is_some() {
            self.handle_cookie_ack()?
        } else if let Some(c) = chunk_any.downcast_ref::<ChunkPayloadData>() {
            self.handle_data(c)?
        } else if let Some(c) = chunk_any.downcast_ref::<ChunkSelectiveAck>() {
            self.handle_sack(c, now)?
        } else if let Some(c) = chunk_any.downcast_ref::<ChunkReconfig>() {
            self.handle_reconfig(c)?
        } else if let Some(c) = chunk_any.downcast_ref::<ChunkForwardTsn>() {
            self.handle_forward_tsn(c)?
        } else if let Some(c) = chunk_any.downcast_ref::<ChunkShutdown>() {
            self.handle_shutdown(c)?
        } else if let Some(c) = chunk_any.downcast_ref::<ChunkShutdownAck>() {
            self.handle_shutdown_ack(c)?
        } else if let Some(c) = chunk_any.downcast_ref::<ChunkShutdownComplete>() {
            self.handle_shutdown_complete(c)?
        } else {
            return Err(Error::ErrChunkTypeUnhandled);
        };

        if !packets.is_empty() {
            let mut buf: VecDeque<_> = packets.into_iter().collect();
            self.control_queue.append(&mut buf);
            self.awake_write_loop();
        }

        Ok(())
    }

    fn handle_init(&mut self, p: &Packet, i: &ChunkInit) -> Result<Vec<Packet>> {
        let state = self.state();
        debug!("[{}] chunkInit received in state '{}'", self.side, state);

        // https://tools.ietf.org/html/rfc4960#section-5.2.1
        // Upon receipt of an INIT in the COOKIE-WAIT state, an endpoint MUST
        // respond with an INIT ACK using the same parameters it sent in its
        // original INIT chunk (including its Initiate Tag, unchanged).  When
        // responding, the endpoint MUST send the INIT ACK back to the same
        // address that the original INIT (sent by this endpoint) was sent.

        if state != AssociationState::Closed
            && state != AssociationState::CookieWait
            && state != AssociationState::CookieEchoed
        {
            // 5.2.2.  Unexpected INIT in States Other than CLOSED, COOKIE-ECHOED,
            //        COOKIE-WAIT, and SHUTDOWN-ACK-SENT
            return Err(Error::ErrHandleInitState);
        }

        // Should we be setting any of these permanently until we've ACKed further?
        self.my_max_num_inbound_streams =
            std::cmp::min(i.num_inbound_streams, self.my_max_num_inbound_streams);
        self.my_max_num_outbound_streams =
            std::cmp::min(i.num_outbound_streams, self.my_max_num_outbound_streams);
        self.peer_num_outbound_streams = i.num_outbound_streams;
        self.peer_verification_tag = i.initiate_tag;
        self.source_port = p.common_header.destination_port;
        self.destination_port = p.common_header.source_port;

        // 13.2 This is the last TSN received in sequence.  This value
        // is set initially by taking the peer's initial TSN,
        // received in the INIT or INIT ACK chunk, and
        // subtracting one from it.
        self.peer_last_tsn = if i.initial_tsn == 0 {
            u32::MAX
        } else {
            i.initial_tsn - 1
        };

        // Adopt the peer's advertised receive window and seed ssthresh from it,
        // mirroring handle_init_ack (and Pion's shared init path). RFC 4960
        // §7.2.1 (Slow-Start) permits initialising ssthresh to the advertised
        // receiver window; without this the answerer keeps ssthresh at its
        // initial 0, so cwnd never starts below it, slow-start never runs, and
        // cwnd only grows linearly under congestion avoidance (§7.2.2).
        self.rwnd = i.advertised_receiver_window_credit;
        debug!("[{}] initial rwnd={}", self.side, self.rwnd);
        self.ssthresh = self.rwnd;

        for param in &i.params {
            if let Some(v) = param.as_any().downcast_ref::<ParamSupportedExtensions>() {
                for t in &v.chunk_types {
                    if *t == CT_FORWARD_TSN {
                        debug!("[{}] use ForwardTSN (on init)", self.side);
                        self.use_forward_tsn = true;
                    }
                }
            }
        }
        if !self.use_forward_tsn {
            warn!("[{}] not using ForwardTSN (on init)", self.side);
        }

        let mut outbound = Packet {
            common_header: CommonHeader {
                verification_tag: self.peer_verification_tag,
                source_port: self.source_port,
                destination_port: self.destination_port,
            },
            chunks: vec![],
        };

        let mut init_ack = ChunkInit {
            is_ack: true,
            initial_tsn: self.my_next_tsn,
            num_outbound_streams: self.my_max_num_outbound_streams,
            num_inbound_streams: self.my_max_num_inbound_streams,
            initiate_tag: self.my_verification_tag,
            advertised_receiver_window_credit: self.max_receive_buffer_size,
            ..Default::default()
        };

        if self.my_cookie.is_none() {
            self.my_cookie = Some(ParamStateCookie::new());
        }

        if let Some(my_cookie) = &self.my_cookie {
            init_ack.params = vec![Box::new(my_cookie.clone())];
        }

        init_ack.set_supported_extensions();

        outbound.chunks = vec![Box::new(init_ack)];

        Ok(vec![outbound])
    }

    fn handle_init_ack(
        &mut self,
        p: &Packet,
        i: &ChunkInitAck,
        now: Instant,
    ) -> Result<Vec<Packet>> {
        let state = self.state();
        debug!("[{}] chunkInitAck received in state '{}'", self.side, state);
        if state != AssociationState::CookieWait {
            // RFC 4960
            // 5.2.3.  Unexpected INIT ACK
            //   If an INIT ACK is received by an endpoint in any state other than the
            //   COOKIE-WAIT state, the endpoint should discard the INIT ACK chunk.
            //   An unexpected INIT ACK usually indicates the processing of an old or
            //   duplicated INIT chunk.
            return Ok(vec![]);
        }

        self.my_max_num_inbound_streams =
            std::cmp::min(i.num_inbound_streams, self.my_max_num_inbound_streams);
        self.my_max_num_outbound_streams =
            std::cmp::min(i.num_outbound_streams, self.my_max_num_outbound_streams);
        self.peer_num_outbound_streams = i.num_outbound_streams;
        self.peer_verification_tag = i.initiate_tag;
        self.peer_last_tsn = if i.initial_tsn == 0 {
            u32::MAX
        } else {
            i.initial_tsn - 1
        };
        if self.source_port != p.common_header.destination_port
            || self.destination_port != p.common_header.source_port
        {
            warn!("[{}] handle_init_ack: port mismatch", self.side);
            return Ok(vec![]);
        }

        self.rwnd = i.advertised_receiver_window_credit;
        debug!("[{}] initial rwnd={}", self.side, self.rwnd);

        // RFC 4690 Sec 7.2.1
        //  o  The initial value of ssthresh MAY be arbitrarily high (for
        //     example, implementations MAY use the size of the receiver
        //     advertised window).
        self.ssthresh = self.rwnd;
        trace!(
            "[{}] updated cwnd={} ssthresh={} inflight={} (INI)",
            self.side,
            self.cwnd,
            self.ssthresh,
            self.inflight_queue.get_num_bytes()
        );

        self.timers.stop(Timer::T1Init);
        self.stored_init = None;

        let mut cookie_param = None;
        for param in &i.params {
            if let Some(v) = param.as_any().downcast_ref::<ParamStateCookie>() {
                cookie_param = Some(v);
            } else if let Some(v) = param.as_any().downcast_ref::<ParamSupportedExtensions>() {
                for t in &v.chunk_types {
                    if *t == CT_FORWARD_TSN {
                        debug!("[{}] use ForwardTSN (on initAck)", self.side);
                        self.use_forward_tsn = true;
                    }
                }
            }
        }
        if !self.use_forward_tsn {
            warn!("[{}] not using ForwardTSN (on initAck)", self.side);
        }

        if let Some(v) = cookie_param {
            self.stored_cookie_echo = Some(ChunkCookieEcho {
                cookie: v.cookie.clone(),
            });

            self.send_cookie_echo()?;

            self.timers
                .start(Timer::T1Cookie, now, self.rto_mgr.get_rto());

            self.set_state(AssociationState::CookieEchoed);

            Ok(vec![])
        } else {
            Err(Error::ErrInitAckNoCookie)
        }
    }

    fn handle_heartbeat(&self, c: &ChunkHeartbeat) -> Result<Vec<Packet>> {
        trace!("[{}] chunkHeartbeat", self.side);
        if let Some(p) = c.params.first() {
            if let Some(hbi) = p.as_any().downcast_ref::<ParamHeartbeatInfo>() {
                return Ok(vec![Packet {
                    common_header: CommonHeader {
                        verification_tag: self.peer_verification_tag,
                        source_port: self.source_port,
                        destination_port: self.destination_port,
                    },
                    chunks: vec![Box::new(ChunkHeartbeatAck {
                        params: vec![Box::new(ParamHeartbeatInfo {
                            heartbeat_information: hbi.heartbeat_information.clone(),
                        })],
                    })],
                }]);
            } else {
                warn!(
                    "[{}] failed to handle Heartbeat, no ParamHeartbeatInfo",
                    self.side,
                );
            }
        }

        Ok(vec![])
    }

    fn handle_cookie_echo(&mut self, c: &ChunkCookieEcho) -> Result<Vec<Packet>> {
        let state = self.state();
        debug!("[{}] COOKIE-ECHO received in state '{}'", self.side, state);

        if let Some(my_cookie) = &self.my_cookie {
            match state {
                AssociationState::Established => {
                    if !constant_time_eq(&my_cookie.cookie, &c.cookie) {
                        return Ok(vec![]);
                    }
                }
                AssociationState::Closed
                | AssociationState::CookieWait
                | AssociationState::CookieEchoed => {
                    if !constant_time_eq(&my_cookie.cookie, &c.cookie) {
                        return Ok(vec![]);
                    }

                    self.timers.stop(Timer::T1Init);
                    self.stored_init = None;

                    self.timers.stop(Timer::T1Cookie);
                    self.stored_cookie_echo = None;

                    self.events.push_back(Event::Connected);
                    self.set_state(AssociationState::Established);
                    self.handshake_completed = true;
                }
                _ => return Ok(vec![]),
            };
        } else {
            debug!("[{}] COOKIE-ECHO received before initialization", self.side);
            return Ok(vec![]);
        }

        Ok(vec![Packet {
            common_header: CommonHeader {
                verification_tag: self.peer_verification_tag,
                source_port: self.source_port,
                destination_port: self.destination_port,
            },
            chunks: vec![Box::new(ChunkCookieAck {})],
        }])
    }

    fn handle_cookie_ack(&mut self) -> Result<Vec<Packet>> {
        let state = self.state();
        debug!("[{}] COOKIE-ACK received in state '{}'", self.side, state);
        if state != AssociationState::CookieEchoed {
            // RFC 4960
            // 5.2.5.  Handle Duplicate COOKIE-ACK.
            //   At any state other than COOKIE-ECHOED, an endpoint should silently
            //   discard a received COOKIE ACK chunk.
            return Ok(vec![]);
        }

        self.timers.stop(Timer::T1Cookie);
        self.stored_cookie_echo = None;

        self.events.push_back(Event::Connected);
        self.set_state(AssociationState::Established);
        self.handshake_completed = true;

        Ok(vec![])
    }

    fn handle_data(&mut self, d: &ChunkPayloadData) -> Result<Vec<Packet>> {
        debug!(
            "[{}] DATA: tsn={} peer_last_tsn={} immediateSack={} len={}, unordered={}",
            self.side,
            d.tsn,
            self.peer_last_tsn,
            d.immediate_sack,
            d.user_data.len(),
            d.unordered,
        );
        self.stats.inc_datas();

        // A gap ack block reaches only 65535 TSNs past the cumulative one, so a TSN farther
        // ahead can never be acknowledged: dropped unbuffered, with a SACK still going out.
        if sna32gt(d.tsn, self.peer_last_tsn)
            && d.tsn.wrapping_sub(self.peer_last_tsn) > u32::from(u16::MAX)
        {
            return self.handle_peer_last_tsn_and_acknowledgement(d.immediate_sack);
        }

        let can_push = self.payload_queue.can_push(d, self.peer_last_tsn);
        let mut stream_handle_data = false;
        if can_push && !self.inbound_stream_open(d.stream_identifier) {
            // A stream the peer never opened is acknowledged and its data discarded (RFC 4960
            // 6.5): the TSN is held without its payload until the cumulative ack passes it.
            let mut unopened = d.clone();
            unopened.user_data = Bytes::new();
            self.payload_queue.push(unopened, self.peer_last_tsn);
        } else if can_push {
            if self.get_or_create_stream(d.stream_identifier).is_some() {
                if self.get_my_receiver_window_credit() > 0 {
                    // Pass the new chunk to stream level as soon as it arrives
                    self.payload_queue.push(d.clone(), self.peer_last_tsn);
                    stream_handle_data = true;
                } else {
                    // Receive buffer is full. Two kinds of chunk are still worth taking,
                    // because refusing them is not something the application can relieve by
                    // draining.
                    //
                    // The first fills a gap below the highest TSN already queued.
                    let fills_gap = self
                        .payload_queue
                        .get_last_tsn_received()
                        .is_some_and(|last_tsn| sna32lt(d.tsn, *last_tsn));

                    // The second is the next in-sequence chunk when nothing is readable at
                    // all, and it is what keeps a full buffer from becoming permanent. Bytes
                    // are released only when the application reads; `is_readable()` is
                    // head-of-line blocked by an incomplete chunk set; and the chunk that
                    // would complete that set is exactly `peer_last_tsn + 1`. Dropping it
                    // leaves the window at zero with no way to reopen — every retransmission
                    // is refused for the same reason, forever.
                    // See <https://github.com/webrtc-rs/webrtc/issues/822>.
                    //
                    // Guarded on nothing being readable so this stays an escape from deadlock
                    // rather than a licence to overrun the bound. A merely slow receiver has
                    // data to take and must be made to take it. Without the guard, the chunk
                    // accepted here is `peer_last_tsn + 1` — precisely the one that advances
                    // the cumulative ack — so every chunk would qualify and
                    // `max_receive_buffer_size` would stop bounding anything.
                    let unblocks_reassembly =
                        d.tsn == self.peer_last_tsn.wrapping_add(1) && !self.has_readable_data();

                    if fills_gap || unblocks_reassembly {
                        debug!(
                            "[{}] receive buffer full, but accepted {} with tsn={} ssn={}",
                            self.side,
                            if fills_gap {
                                "as this is a missing chunk"
                            } else {
                                "as the in-sequence chunk that unblocks reassembly"
                            },
                            d.tsn,
                            d.stream_sequence_number
                        );
                        self.payload_queue.push(d.clone(), self.peer_last_tsn);
                        stream_handle_data = true;
                    } else {
                        debug!(
                            "[{}] receive buffer full. dropping DATA with tsn={} ssn={}",
                            self.side, d.tsn, d.stream_sequence_number
                        );
                    }
                }
            } else {
                // silently discard the data. (sender will retry on T3-rtx timeout)
                debug!("[{}] discard {}", self.side, d.stream_sequence_number);
                return Ok(vec![]);
            }
        }

        // A duplicate is acknowledged at once and named, so the peer can tell its retransmission
        // was spurious.
        let duplicate = !can_push;
        if duplicate {
            self.payload_queue.note_duplicate(d.tsn);
        }
        let immediate_sack = d.immediate_sack || duplicate;

        if stream_handle_data && let Some(s) = self.streams.get_mut(&d.stream_identifier) {
            self.events.push_back(Event::DatagramReceived);
            if s.handle_data(d) && s.reassembly_queue.is_readable() {
                self.events.push_back(Event::Stream(StreamEvent::Readable {
                    id: s.stream_identifier,
                }));
            }
        }

        if stream_handle_data && self.reassembly_bytes() > self.max_reassembly_bytes as usize {
            // The peer ignored the advertised window and the message size, on one stream or
            // spread over many: abort rather than buffer without bound.
            warn!(
                "[{}] aborting: reassembly exceeded {} bytes",
                self.side, self.max_reassembly_bytes
            );
            self.abort()?;
            return Ok(vec![]);
        }

        self.handle_peer_last_tsn_and_acknowledgement(immediate_sack)
    }

    fn handle_sack(&mut self, d: &ChunkSelectiveAck, now: Instant) -> Result<Vec<Packet>> {
        trace!(
            "[{}] {}, SACK: cumTSN={} a_rwnd={}",
            self.side,
            self.cumulative_tsn_ack_point,
            d.cumulative_tsn_ack,
            d.advertised_receiver_window_credit
        );
        let state = self.state();
        if state != AssociationState::Established
            && state != AssociationState::ShutdownPending
            && state != AssociationState::ShutdownReceived
        {
            return Ok(vec![]);
        }

        self.stats.inc_sacks();

        if sna32gt(self.cumulative_tsn_ack_point, d.cumulative_tsn_ack) {
            // RFC 4960 sec 6.2.1.  Processing a Received SACK
            // D)
            //   i) If Cumulative TSN Ack is less than the Cumulative TSN Ack
            //      Point, then drop the SACK.  Since Cumulative TSN Ack is
            //      monotonically increasing, a SACK whose Cumulative TSN Ack is
            //      less than the Cumulative TSN Ack Point indicates an out-of-
            //      order SACK.

            debug!(
                "[{}] SACK Cumulative ACK {} is older than ACK point {}",
                self.side, d.cumulative_tsn_ack, self.cumulative_tsn_ack_point
            );

            return Ok(vec![]);
        }

        // A cumulative TSN past anything sent acknowledges nothing real: dropped whole, before
        // the chunks below it are popped and the ack point left behind them for good.
        if !sna32lt(d.cumulative_tsn_ack, self.my_next_tsn) {
            debug!(
                "[{}] SACK Cumulative ACK {} is past the next TSN {}",
                self.side, d.cumulative_tsn_ack, self.my_next_tsn
            );
            return Ok(vec![]);
        }

        // Process selective ack
        let (bytes_acked_per_stream, total_bytes_acked, htna) =
            self.process_selective_ack(d, now)?;

        let mut cum_tsn_ack_point_advanced = false;
        if sna32lt(self.cumulative_tsn_ack_point, d.cumulative_tsn_ack) {
            trace!(
                "[{}] SACK: cumTSN advanced: {} -> {}",
                self.side, self.cumulative_tsn_ack_point, d.cumulative_tsn_ack
            );

            self.cumulative_tsn_ack_point = d.cumulative_tsn_ack;
            cum_tsn_ack_point_advanced = true;
            self.on_cumulative_tsn_ack_point_advanced(total_bytes_acked, now);
        }

        self.release_stream_buffers(&bytes_acked_per_stream);

        // New rwnd value
        // RFC 4960 sec 6.2.1.  Processing a Received SACK
        // D)
        //   ii) Set rwnd equal to the newly received a_rwnd minus the number
        //       of bytes still outstanding after processing the Cumulative
        //       TSN Ack and the Gap Ack Blocks.

        // acknowledged payload bytes were already released from the in-flight queue
        let bytes_outstanding = self.inflight_queue.get_num_bytes() as u32;
        if bytes_outstanding >= d.advertised_receiver_window_credit {
            self.rwnd = 0;
        } else {
            self.rwnd = d.advertised_receiver_window_credit - bytes_outstanding;
        }

        self.process_fast_retransmission(d.cumulative_tsn_ack, htna, cum_tsn_ack_point_advanced)?;

        if self.use_forward_tsn {
            // RFC 3758 Sec 3.5 C1
            if sna32lt(
                self.advanced_peer_tsn_ack_point,
                self.cumulative_tsn_ack_point,
            ) {
                self.advanced_peer_tsn_ack_point = self.cumulative_tsn_ack_point;
                // Window reset: everything previously tracked is now at/below
                // the cumulative ack point, so start the stream map fresh.
                self.fwd_tsn_stream_map.clear();
            }

            self.advance_peer_ack_point();
            self.awake_write_loop();
        }

        self.postprocess_sack(state, cum_tsn_ack_point_advanced, now);

        Ok(vec![])
    }

    fn handle_reconfig(&mut self, c: &ChunkReconfig) -> Result<Vec<Packet>> {
        trace!("[{}] handle_reconfig", self.side);

        let mut pp = vec![];

        if let Some(param_a) = &c.param_a {
            self.handle_reconfig_param(param_a, &mut pp)?;
        }

        if let Some(param_b) = &c.param_b {
            self.handle_reconfig_param(param_b, &mut pp)?;
        }

        Ok(pp)
    }

    fn handle_forward_tsn(&mut self, c: &ChunkForwardTsn) -> Result<Vec<Packet>> {
        trace!("[{}] FwdTSN: {}", self.side, c);

        if !self.use_forward_tsn {
            warn!("[{}] received FwdTSN but not enabled", self.side);
            // Return an error chunk
            let cerr = ChunkError {
                error_causes: vec![ErrorCauseUnrecognizedChunkType::default()],
            };

            let outbound = Packet {
                common_header: CommonHeader {
                    verification_tag: self.peer_verification_tag,
                    source_port: self.source_port,
                    destination_port: self.destination_port,
                },
                chunks: vec![Box::new(cerr)],
            };
            return Ok(vec![outbound]);
        }

        // From RFC 3758 Sec 3.6:
        //   Note, if the "New Cumulative TSN" value carried in the arrived
        //   FORWARD TSN chunk is found to be behind or at the current cumulative
        //   TSN point, the data receiver MUST treat this FORWARD TSN as out-of-
        //   date and MUST NOT update its Cumulative TSN.  The receiver SHOULD
        //   send a SACK to its peer (the sender of the FORWARD TSN) since such a
        //   duplicate may indicate the previous SACK was lost in the network.

        trace!(
            "[{}] should send ack? newCumTSN={} peer_last_tsn={}",
            self.side, c.new_cumulative_tsn, self.peer_last_tsn
        );
        if !sna32gt(c.new_cumulative_tsn, self.peer_last_tsn) {
            trace!("[{}] sending ack on Forward TSN", self.side);
            self.ack_state = AckState::Immediate;
            self.timers.stop(Timer::Ack);
            self.awake_write_loop();
            return Ok(vec![]);
        }

        // From RFC 3758 Sec 3.6:
        //   the receiver MUST perform the same TSN handling, including duplicate
        //   detection, gap detection, SACK generation, cumulative TSN
        //   advancement, etc. as defined in RFC 2960 [2]---with the following
        //   exceptions and additions.

        //   When a FORWARD TSN chunk arrives, the data receiver MUST first update
        //   its cumulative TSN point to the value carried in the FORWARD TSN
        //   chunk,

        // Advance peer_last_tsn, pruning what the queue holds at or below it: each chunk held
        // is visited once, never every TSN a jump of up to 2^31 - 1 spans.
        self.payload_queue.prune_through(c.new_cumulative_tsn);
        self.peer_last_tsn = c.new_cumulative_tsn;

        // Report new peer_last_tsn value and abandoned largest SSN value to
        // corresponding streams so that the abandoned chunks can be removed
        // from the reassemblyQueue.
        for forwarded in &c.streams {
            if let Some(s) = self.streams.get_mut(&forwarded.identifier) {
                s.handle_forward_tsn_for_ordered(forwarded.sequence);
                if s.reassembly_queue.is_readable() {
                    self.events.push_back(Event::Stream(StreamEvent::Readable {
                        id: s.stream_identifier,
                    }));
                }
            }
        }

        // TSN may be forwarded for unordered chunks. ForwardTSN chunk does not
        // report which stream identifier it skipped for unordered chunks.
        // Therefore, we need to broadcast this event to all existing streams for
        // unordered chunks.
        for s in self.streams.values_mut() {
            s.handle_forward_tsn_for_unordered(c.new_cumulative_tsn);
            if s.reassembly_queue.is_readable() {
                self.events.push_back(Event::Stream(StreamEvent::Readable {
                    id: s.stream_identifier,
                }));
            }
        }

        self.handle_peer_last_tsn_and_acknowledgement(false)
    }

    fn handle_shutdown(&mut self, _: &ChunkShutdown) -> Result<Vec<Packet>> {
        let state = self.state();

        if state == AssociationState::Established {
            if !self.inflight_queue.is_empty() {
                self.set_state(AssociationState::ShutdownReceived);
            } else {
                // No more outstanding, send shutdown ack.
                self.will_send_shutdown_ack = true;
                self.set_state(AssociationState::ShutdownAckSent);

                self.awake_write_loop();
            }
        } else if state == AssociationState::ShutdownSent {
            // self.cumulative_tsn_ack_point = c.cumulative_tsn_ack

            self.will_send_shutdown_ack = true;
            self.set_state(AssociationState::ShutdownAckSent);

            self.awake_write_loop();
        }

        Ok(vec![])
    }

    fn handle_shutdown_ack(&mut self, _: &ChunkShutdownAck) -> Result<Vec<Packet>> {
        let state = self.state();
        if state == AssociationState::ShutdownSent || state == AssociationState::ShutdownAckSent {
            self.timers.stop(Timer::T2Shutdown);
            self.will_send_shutdown_complete = true;

            self.awake_write_loop();
        }

        Ok(vec![])
    }

    fn handle_shutdown_complete(&mut self, _: &ChunkShutdownComplete) -> Result<Vec<Packet>> {
        let state = self.state();
        if state == AssociationState::ShutdownAckSent {
            self.timers.stop(Timer::T2Shutdown);
            self.close(AssociationError::AssociationClosed)?;
        }

        Ok(vec![])
    }

    /// A common routine for handle_data and handle_forward_tsn routines
    fn handle_peer_last_tsn_and_acknowledgement(
        &mut self,
        sack_immediately: bool,
    ) -> Result<Vec<Packet>> {
        let mut reply = vec![];

        // Try to advance peer_last_tsn

        // From RFC 3758 Sec 3.6:
        //   .. and then MUST further advance its cumulative TSN point locally
        //   if possible
        // Meaning, if peer_last_tsn+1 points to a chunk that is received,
        // advance peer_last_tsn until peer_last_tsn+1 points to unreceived chunk.
        //debug!("[{}] peer_last_tsn = {}", self.side, self.peer_last_tsn);
        while self
            .payload_queue
            .pop(self.peer_last_tsn.wrapping_add(1))
            .is_some()
        {
            self.peer_last_tsn = self.peer_last_tsn.wrapping_add(1);
            //debug!("[{}] peer_last_tsn = {}", self.side, self.peer_last_tsn);

            let rst_reqs: Vec<ParamOutgoingResetRequest> =
                self.reconfig_requests.values().cloned().collect();
            for rst_req in rst_reqs {
                self.reset_streams_if_any(&rst_req, false, &mut reply)?;
            }
        }

        let has_packet_loss = !self.payload_queue.is_empty();
        if has_packet_loss {
            trace!(
                "[{}] packetloss: {}",
                self.side,
                self.payload_queue
                    .get_gap_ack_blocks_string(self.peer_last_tsn)
            );
        }

        if (self.ack_state != AckState::Immediate
            && !sack_immediately
            && !has_packet_loss
            && self.ack_mode == AckMode::Normal)
            || self.ack_mode == AckMode::AlwaysDelay
        {
            if self.ack_state == AckState::Idle {
                self.delayed_ack_triggered = true;
            } else {
                self.immediate_ack_triggered = true;
            }
        } else {
            self.immediate_ack_triggered = true;
        }

        Ok(reply)
    }

    #[allow(clippy::borrowed_box)]
    fn handle_reconfig_param(
        &mut self,
        raw: &Box<dyn Param>,
        reply: &mut Vec<Packet>,
    ) -> Result<()> {
        if let Some(p) = raw.as_any().downcast_ref::<ParamOutgoingResetRequest>() {
            self.reconfig_requests
                .insert(p.reconfig_request_sequence_number, p.clone());
            self.reset_streams_if_any(p, true, reply)?;
            Ok(())
        } else if let Some(p) = raw.as_any().downcast_ref::<ParamReconfigResponse>() {
            self.reconfigs.remove(&p.reconfig_response_sequence_number);
            if self.reconfigs.is_empty() {
                self.timers.stop(Timer::Reconfig);
            }
            Ok(())
        } else {
            Err(Error::ErrParameterType)
        }
    }

    fn process_selective_ack(
        &mut self,
        d: &ChunkSelectiveAck,
        now: Instant,
    ) -> Result<(FxHashMap<u16, i64>, i64, u32)> {
        let mut bytes_acked_per_stream = FxHashMap::default();
        let mut total_bytes_acked = 0;
        let mut newly_acked_data = false;
        // Abandoned DATA can precede the earliest outstanding TSN. RFC 9260
        // 6.3.2 R3 applies to that outstanding DATA, including gap acknowledgments.
        let earliest_outstanding = self.inflight_queue.sorted.iter().copied().find(|tsn| {
            self.inflight_queue
                .get(*tsn)
                .is_some_and(ChunkPayloadData::is_outstanding)
        });

        // New ack point, so pop all ACKed packets from inflight_queue
        // We add 1 because the "currentAckPoint" has already been popped from the inflight queue
        // For the first SACK we take care of this by setting the ackpoint to cumAck - 1
        let mut i = self.cumulative_tsn_ack_point.wrapping_add(1);
        //log::debug!("[{}] i={} d={}", self.name, i, d.cumulative_tsn_ack);
        while sna32lte(i, d.cumulative_tsn_ack) {
            if let Some(mut c) = self.inflight_queue.pop(i) {
                if c.acknowledge() {
                    newly_acked_data |= c.nsent > 0;

                    let n_bytes_acked = c.user_data.len() as i64;
                    total_bytes_acked += n_bytes_acked;
                    // A late SACK for a retired stream must not release bytes
                    // belonging to a new channel that reused its SID.
                    if self.is_current_stream_data(&c) {
                        *bytes_acked_per_stream
                            .entry(c.stream_identifier)
                            .or_default() += n_bytes_acked;
                    }

                    // RFC 4960 sec 6.3.1.  RTO Calculation
                    //   C4)  When data is in flight and when allowed by rule C5 below, a new
                    //        RTT measurement MUST be made each round trip.  Furthermore, new
                    //        RTT measurements SHOULD be made no more than once per round trip
                    //        for a given destination transport address.
                    //   C5)  Karn's algorithm: RTT measurements MUST NOT be made using
                    //        packets that were retransmitted (and thus for which it is
                    //        ambiguous whether the reply was for the first instance of the
                    //        chunk or for a later instance)
                    if !c.abandoned() && c.nsent == 1 && sna32gte(c.tsn, self.min_tsn2measure_rtt) {
                        self.min_tsn2measure_rtt = self.my_next_tsn;
                        if let Some(since) = &c.since {
                            let rtt = now.duration_since(*since);
                            let srtt = self.rto_mgr.set_new_rtt(rtt.as_millis() as u64);
                            trace!(
                                "[{}] SACK: measured-rtt={} srtt={} new-rto={}",
                                self.side,
                                rtt.as_millis(),
                                srtt,
                                self.rto_mgr.get_rto()
                            );
                        } else {
                            error!("[{}] invalid c.since", self.side);
                        }
                    }
                }

                if self.in_fast_recovery && c.tsn == self.fast_recover_exit_point {
                    debug!("[{}] exit fast-recovery", self.side);
                    self.in_fast_recovery = false;
                }
            } else {
                return Err(Error::ErrInflightQueueTsnPop);
            }

            i = i.wrapping_add(1);
        }

        let mut htna = d.cumulative_tsn_ack;

        // The gap blocks as merged ranges of offsets from the cumulative TSN, and each chunk
        // in flight looked up in them, never each offset they span: a packet of blocks names
        // millions, and only what was sent can be acknowledged.
        let mut ranges: Vec<(u16, u16)> = d
            .gap_ack_blocks
            .iter()
            .filter(|g| g.start <= g.end)
            .map(|g| (g.start, g.end))
            .collect();
        ranges.sort_unstable();
        let mut merged: Vec<(u16, u16)> = Vec::with_capacity(ranges.len());
        for (start, end) in ranges {
            match merged.last_mut() {
                Some(last) if u32::from(start) <= u32::from(last.1) + 1 => last.1 = last.1.max(end),
                _ => merged.push((start, end)),
            }
        }
        let highest_seen = merged
            .last()
            .map(|(_, end)| d.cumulative_tsn_ack.wrapping_add(u32::from(*end)));
        let gap_acked = |tsn: u32| -> bool {
            let offset = tsn.wrapping_sub(d.cumulative_tsn_ack);
            if offset > u32::from(u16::MAX) {
                return false;
            }
            let offset = offset as u16;
            let i = merged.partition_point(|(start, _)| *start <= offset);
            i > 0 && offset <= merged[i - 1].1
        };

        // Record peer acknowledgments independently of local payload release.
        let in_flight: Vec<u32> = self.inflight_queue.sorted.iter().copied().collect();
        if let Some(highest_seen) = highest_seen {
            for tsn in in_flight {
                if sna32gt(tsn, highest_seen) {
                    break;
                }
                if !gap_acked(tsn) {
                    continue;
                }

                let newly_acknowledged = self.inflight_queue.acknowledge(tsn);
                let n_bytes_acked = if newly_acknowledged {
                    self.inflight_queue.release_payload(tsn) as i64
                } else {
                    0
                };

                if let Some(c) = self.inflight_queue.get(tsn) {
                    if newly_acknowledged {
                        newly_acked_data |= c.nsent > 0;
                        total_bytes_acked += n_bytes_acked;
                        if self.is_current_stream_data(c) {
                            *bytes_acked_per_stream
                                .entry(c.stream_identifier)
                                .or_default() += n_bytes_acked;
                        }

                        trace!("[{}] tsn={} has been sacked", self.side, c.tsn);

                        if !c.abandoned() && c.nsent == 1 {
                            self.min_tsn2measure_rtt = self.my_next_tsn;
                            if let Some(since) = &c.since {
                                let rtt = now.duration_since(*since);
                                let srtt = self.rto_mgr.set_new_rtt(rtt.as_millis() as u64);
                                trace!(
                                    "[{}] SACK: measured-rtt={} srtt={} new-rto={}",
                                    self.side,
                                    rtt.as_millis(),
                                    srtt,
                                    self.rto_mgr.get_rto()
                                );
                            } else {
                                error!("[{}] invalid c.since", self.side);
                            }
                        }

                        if sna32lt(htna, tsn) {
                            htna = tsn;
                        }
                    }
                }
            }
        }

        // RFC 9260 8.1-8.2: an acknowledgment of new DATA clears the error
        // counter, including a late SACK after local abandonment. Payload
        // release does not acknowledge DATA; duplicates and never-sent TSNs
        // assigned only for FORWARD TSN do not count as newly acknowledged DATA.
        if newly_acked_data {
            self.timers.reset_retrans(Timer::T3RTX);
        }
        if earliest_outstanding.is_some_and(|tsn| {
            sna32lte(tsn, d.cumulative_tsn_ack)
                || self.inflight_queue.get(tsn).is_some_and(|c| c.acknowledged)
        }) {
            // Postprocessing restarts T3 with the current RTO if DATA/FORWARD
            // TSN remains outstanding.
            self.timers.set(Timer::T3RTX, None);
        }
        Ok((bytes_acked_per_stream, total_bytes_acked, htna))
    }

    fn on_cumulative_tsn_ack_point_advanced(&mut self, total_bytes_acked: i64, now: Instant) {
        // RFC 4096, sec 6.3.2.  Retransmission Timer Rules
        //   R2)  Whenever all outstanding data sent to an address have been
        //        acknowledged, turn off the T3-rtx timer of that address.
        if self.inflight_queue.is_empty() {
            trace!(
                "[{}] SACK: no more packet in-flight (pending={})",
                self.side,
                self.pending_queue.len()
            );
            self.timers.stop(Timer::T3RTX);
        } else {
            trace!("[{}] T3-rtx timer start (pt2)", self.side);
            self.timers
                .start_if_idle(Timer::T3RTX, now, self.rto_mgr.get_rto());
        }

        // Update congestion control parameters
        if self.cwnd <= self.ssthresh {
            // RFC 4096, sec 7.2.1.  Slow-Start
            //   o  When cwnd is less than or equal to ssthresh, an SCTP endpoint MUST
            //		use the slow-start algorithm to increase cwnd only if the current
            //      congestion window is being fully utilized, an incoming SACK
            //      advances the Cumulative TSN Ack Point, and the data sender is not
            //      in Fast Recovery.  Only when these three conditions are met can
            //      the cwnd be increased; otherwise, the cwnd MUST not be increased.
            //		If these conditions are met, then cwnd MUST be increased by, at
            //      most, the lesser of 1) the total size of the previously
            //      outstanding DATA chunk(s) acknowledged, and 2) the destination's
            //      path MTU.
            if !self.in_fast_recovery && !self.pending_queue.is_empty() {
                self.cwnd += std::cmp::min(total_bytes_acked as u32, self.cwnd); // TCP way
                // self.cwnd += min32(uint32(total_bytes_acked), self.mtu) // SCTP way (slow)
                trace!(
                    "[{}] updated cwnd={} ssthresh={} acked={} (SS)",
                    self.side, self.cwnd, self.ssthresh, total_bytes_acked
                );
            } else {
                trace!(
                    "[{}] cwnd did not grow: cwnd={} ssthresh={} acked={} FR={} pending={}",
                    self.side,
                    self.cwnd,
                    self.ssthresh,
                    total_bytes_acked,
                    self.in_fast_recovery,
                    self.pending_queue.len()
                );
            }
        } else {
            // RFC 4096, sec 7.2.2.  Congestion Avoidance
            //   o  Whenever cwnd is greater than ssthresh, upon each SACK arrival
            //      that advances the Cumulative TSN Ack Point, increase
            //      partial_bytes_acked by the total number of bytes of all new chunks
            //      acknowledged in that SACK including chunks acknowledged by the new
            //      Cumulative TSN Ack and by Gap Ack Blocks.
            self.partial_bytes_acked += total_bytes_acked as u32;

            //   o  When partial_bytes_acked is equal to or greater than cwnd and
            //      before the arrival of the SACK the sender had cwnd or more bytes
            //      of data outstanding (i.e., before arrival of the SACK, flight size
            //      was greater than or equal to cwnd), increase cwnd by MTU, and
            //      reset partial_bytes_acked to (partial_bytes_acked - cwnd).
            if self.partial_bytes_acked >= self.cwnd && !self.pending_queue.is_empty() {
                self.partial_bytes_acked -= self.cwnd;
                self.cwnd += self.mtu;
                trace!(
                    "[{}] updated cwnd={} ssthresh={} acked={} (CA)",
                    self.side, self.cwnd, self.ssthresh, total_bytes_acked
                );
            }
        }
    }

    fn process_fast_retransmission(
        &mut self,
        cum_tsn_ack_point: u32,
        htna: u32,
        cum_tsn_ack_point_advanced: bool,
    ) -> Result<()> {
        // HTNA algorithm - RFC 4960 Sec 7.2.4
        // Increment missIndicator of each chunks that the SACK reported missing
        // when either of the following is met:
        // a)  Not in fast-recovery
        //     miss indications are incremented only for missing TSNs prior to the
        //     highest TSN newly acknowledged in the SACK.
        // b)  In fast-recovery AND the Cumulative TSN Ack Point advanced
        //     the miss indications are incremented for all TSNs reported missing
        //     in the SACK.
        if !self.in_fast_recovery || cum_tsn_ack_point_advanced {
            let max_tsn = if !self.in_fast_recovery {
                // a) increment only for missing TSNs prior to the HTNA
                htna
            } else {
                // b) increment for all TSNs reported missing
                cum_tsn_ack_point.wrapping_add(self.inflight_queue.len() as u32 + 1)
            };

            let mut tsn = cum_tsn_ack_point.wrapping_add(1);
            while sna32lt(tsn, max_tsn) {
                if let Some(c) = self.inflight_queue.get_mut(tsn) {
                    if c.is_outstanding() && c.miss_indicator < 3 {
                        c.miss_indicator += 1;
                        if c.miss_indicator == 3 && !self.in_fast_recovery {
                            // 2)  If not in Fast Recovery, adjust the ssthresh and cwnd of the
                            //     destination address(es) to which the missing DATA chunks were
                            //     last sent, according to the formula described in Section 7.2.3.
                            self.in_fast_recovery = true;
                            self.fast_recover_exit_point = htna;
                            self.ssthresh = std::cmp::max(self.cwnd / 2, 4 * self.mtu);
                            self.cwnd = self.ssthresh;
                            self.partial_bytes_acked = 0;
                            self.will_retransmit_fast = true;

                            trace!(
                                "[{}] updated cwnd={} ssthresh={} inflight={} (FR)",
                                self.side,
                                self.cwnd,
                                self.ssthresh,
                                self.inflight_queue.get_num_bytes()
                            );
                        }
                    }
                } else {
                    return Err(Error::ErrTsnRequestNotExist);
                }

                tsn = tsn.wrapping_add(1);
            }
        }

        if self.in_fast_recovery && cum_tsn_ack_point_advanced {
            self.will_retransmit_fast = true;
        }

        Ok(())
    }

    /// The caller must hold the lock. This method was only added because the
    /// linter was complaining about the "cognitive complexity" of handle_sack.
    fn postprocess_sack(
        &mut self,
        state: AssociationState,
        mut should_awake_write_loop: bool,
        now: Instant,
    ) {
        if !self.inflight_queue.is_empty() {
            // Start timer. (noop if already started)
            trace!("[{}] T3-rtx timer start (pt3)", self.side);
            self.timers
                .start_if_idle(Timer::T3RTX, now, self.rto_mgr.get_rto());
        } else if state == AssociationState::ShutdownPending {
            // No more outstanding, send shutdown.
            should_awake_write_loop = true;
            self.will_send_shutdown = true;
            self.set_state(AssociationState::ShutdownSent);
        } else if state == AssociationState::ShutdownReceived {
            // No more outstanding, send shutdown ack.
            should_awake_write_loop = true;
            self.will_send_shutdown_ack = true;
            self.set_state(AssociationState::ShutdownAckSent);
        }

        if should_awake_write_loop {
            self.awake_write_loop();
        }
    }

    /// Streams named by `p` that still hold a complete message the application
    /// has not collected, so a reset naming them cannot be performed yet.
    ///
    /// Only *readable* data counts: an incomplete fragment set can never be
    /// completed once the peer has reset, so it is discardable and must not
    /// hold the reset open forever.
    fn streams_awaiting_drain(&self, p: &ParamOutgoingResetRequest) -> Vec<StreamId> {
        p.stream_identifiers
            .iter()
            .copied()
            .filter(|id| {
                self.streams
                    .get(id)
                    .is_some_and(|s| s.reassembly_queue.is_readable())
            })
            .collect()
    }

    fn reset_streams_if_any(
        &mut self,
        p: &ParamOutgoingResetRequest,
        respond: bool,
        reply: &mut Vec<Packet>,
    ) -> Result<()> {
        let mut result = ReconfigResult::SuccessPerformed;
        let mut sis_to_reset = vec![];

        if sna32lte(p.sender_last_tsn, self.peer_last_tsn) {
            debug!(
                "[{}] resetStream(): senderLastTSN={} <= peer_last_tsn={}",
                self.side, p.sender_last_tsn, self.peer_last_tsn
            );

            let draining = self.streams_awaiting_drain(p);

            for id in &p.stream_identifiers {
                if draining.contains(id) {
                    continue;
                }
                if self.streams.contains_key(id) {
                    if respond {
                        sis_to_reset.push(*id);
                    }
                    self.unregister_stream(*id, AssociationError::Reset);
                }
            }

            if draining.is_empty() {
                self.reconfig_requests
                    .remove(&p.reconfig_request_sequence_number);
            } else {
                debug!(
                    "[{}] resetStream(): deferring reset of {:?}, application has not drained",
                    self.side, draining
                );
                result = ReconfigResult::InProgress;
            }
        } else {
            debug!(
                "[{}] resetStream(): senderLastTSN={} > peer_last_tsn={}",
                self.side, p.sender_last_tsn, self.peer_last_tsn
            );
            result = ReconfigResult::InProgress;
        }

        // Answer incoming reset requests with the same reset request, but with
        // reconfig_response_sequence_number.
        if !sis_to_reset.is_empty() {
            let rsn = self.generate_next_rsn();
            let tsn = self.my_next_tsn.wrapping_sub(1);

            let c = ChunkReconfig {
                param_a: Some(Box::new(ParamOutgoingResetRequest {
                    reconfig_request_sequence_number: rsn,
                    reconfig_response_sequence_number: p.reconfig_request_sequence_number,
                    sender_last_tsn: tsn,
                    stream_identifiers: sis_to_reset,
                })),
                ..Default::default()
            };

            self.reconfigs.insert(rsn, c.clone()); // store in the map for retransmission

            let p = self.create_packet(vec![Box::new(c)]);
            reply.push(p);
        }

        let packet = self.create_packet(vec![Box::new(ChunkReconfig {
            param_a: Some(Box::new(ParamReconfigResponse {
                reconfig_response_sequence_number: p.reconfig_request_sequence_number,
                result,
            })),
            param_b: None,
        })]);

        debug!("[{}] RESET RESPONSE: {}", self.side, packet);

        reply.push(packet);

        Ok(())
    }

    /// create_packet wraps chunks in a packet.
    /// The caller should hold the read lock.
    pub(crate) fn create_packet(&self, chunks: Vec<Box<dyn Chunk>>) -> Packet {
        Packet {
            common_header: CommonHeader {
                verification_tag: self.peer_verification_tag,
                source_port: self.source_port,
                destination_port: self.destination_port,
            },
            chunks,
        }
    }

    /// Marshal a single control chunk into one SCTP packet, bypassing the
    /// `Vec<Box<dyn Chunk>>` + `Packet` allocations of `create_packet(..).marshal()`.
    /// Feeds the borrowed chunk straight to the shared framing path
    /// ([`Packet::write_framed`]). Used on the hot SACK / FORWARD-TSN send path
    /// (a SACK is emitted roughly every 1-2 inbound DATA chunks). The caller holds
    /// the lock.
    fn marshal_control_chunk(&self, chunk: &dyn Chunk) -> Result<Bytes> {
        let common_header = CommonHeader {
            verification_tag: self.peer_verification_tag,
            source_port: self.source_port,
            destination_port: self.destination_port,
        };
        // common header + chunk header + value + up to 3 bytes of trailing padding.
        let mut buf = BytesMut::with_capacity(
            COMMON_HEADER_SIZE as usize + CHUNK_HEADER_SIZE + chunk.value_length() + 3,
        );
        Packet::write_framed(&common_header, std::iter::once(chunk), &mut buf)?;
        Ok(buf.freeze())
    }

    /// create_stream creates a stream. The caller should hold the lock and check no stream exists for this id.
    fn create_stream(
        &mut self,
        stream_identifier: StreamId,
        accept: bool,
        default_payload_type: PayloadProtocolIdentifier,
    ) -> Option<Stream<'_>> {
        let mut s = StreamState::new(
            self.side,
            stream_identifier,
            self.max_payload_size,
            default_payload_type,
        );

        s.generation = self.next_stream_generation;
        self.next_stream_generation = self.next_stream_generation.wrapping_add(1);

        if accept {
            self.stream_queue.push_back(stream_identifier);
            self.events.push_back(Event::Stream(StreamEvent::Opened {
                id: stream_identifier,
            }));
        }

        self.streams.insert(stream_identifier, s);

        Some(Stream {
            stream_identifier,
            association: self,
        })
    }

    /// get_or_create_stream gets or creates a stream. The caller should hold the lock.
    fn get_or_create_stream(&mut self, stream_identifier: StreamId) -> Option<Stream<'_>> {
        if self.streams.contains_key(&stream_identifier) {
            Some(Stream {
                stream_identifier,
                association: self,
            })
        } else {
            self.create_stream(
                stream_identifier,
                true,
                PayloadProtocolIdentifier::default(),
            )
        }
    }

    /// Whether any stream currently holds a complete message the application could read.
    ///
    /// Distinguishes a receiver that is merely slow — it has data to take, and back-pressure
    /// should make it take it — from one wedged behind an incomplete chunk set, which cannot
    /// drain anything however attentive it is. Only the second justifies accepting a chunk
    /// into a full receive buffer.
    fn has_readable_data(&self) -> bool {
        self.streams
            .values()
            .any(|s| s.reassembly_queue.is_readable())
    }

    pub(crate) fn get_my_receiver_window_credit(&self) -> u32 {
        let bytes_queued = u32::try_from(self.reassembly_bytes()).unwrap_or(u32::MAX);
        self.max_receive_buffer_size.saturating_sub(bytes_queued)
    }

    /// What every inbound stream buffers together, delivered and undelivered alike.
    fn reassembly_bytes(&self) -> usize {
        self.streams
            .values()
            .map(|s| s.get_num_bytes_in_reassembly_queue())
            .sum()
    }

    /// Whether the peer's INIT or INIT ACK opened `stream_identifier` for its sends; any,
    /// before it named a count.
    fn inbound_stream_open(&self, stream_identifier: StreamId) -> bool {
        self.peer_num_outbound_streams == 0 || stream_identifier < self.peer_num_outbound_streams
    }

    /// gather_outbound gathers outgoing packets. The returned bool value set to
    /// false means the association should be closed down after the final send.
    fn gather_outbound(&mut self, now: Instant) -> (Vec<Bytes>, bool) {
        let mut raw_packets = vec![];

        if !self.control_queue.is_empty() {
            for p in self.control_queue.drain(..) {
                if let Ok(raw) = p.marshal() {
                    raw_packets.push(raw);
                } else {
                    warn!("[{}] failed to serialize a control packet", self.side);
                    continue;
                }
            }
        }

        let state = self.state();
        match state {
            AssociationState::Established => {
                raw_packets = self.gather_data_packets_to_retransmit(raw_packets, now);
                if self.state() == AssociationState::Closed {
                    return (vec![], false);
                }
                raw_packets = self.gather_outbound_data_and_reconfig_packets(raw_packets, now);
                if self.state() == AssociationState::Closed {
                    return (vec![], false);
                }
                raw_packets = self.gather_outbound_fast_retransmission_packets(raw_packets, now);
                if self.state() == AssociationState::Closed {
                    return (vec![], false);
                }
                raw_packets = self.gather_outbound_sack_packets(raw_packets);
                raw_packets = self.gather_outbound_forward_tsn_packets(raw_packets);
                (raw_packets, true)
            }
            AssociationState::ShutdownPending
            | AssociationState::ShutdownSent
            | AssociationState::ShutdownReceived => {
                raw_packets = self.gather_data_packets_to_retransmit(raw_packets, now);
                if self.state() == AssociationState::Closed {
                    return (vec![], false);
                }
                raw_packets = self.gather_outbound_fast_retransmission_packets(raw_packets, now);
                if self.state() == AssociationState::Closed {
                    return (vec![], false);
                }
                raw_packets = self.gather_outbound_sack_packets(raw_packets);
                raw_packets = self.gather_outbound_forward_tsn_packets(raw_packets);
                self.gather_outbound_shutdown_packets(raw_packets, now)
            }
            AssociationState::ShutdownAckSent => {
                self.gather_outbound_shutdown_packets(raw_packets, now)
            }
            _ => (raw_packets, true),
        }
    }

    fn gather_data_packets_to_retransmit(
        &mut self,
        mut raw_packets: Vec<Bytes>,
        now: Instant,
    ) -> Vec<Bytes> {
        // Nothing is ever flagged for T3-rtx in the steady state, so skip the
        // full in-flight scan unless the T3-rtx timer has actually marked chunks.
        if self.t3_retransmit_pending
            && let Err(err) = self.get_data_packets_to_retransmit(now, &mut raw_packets)
        {
            self.fail_message_queue(err);
            raw_packets.clear();
        }
        raw_packets
    }

    fn gather_outbound_data_and_reconfig_packets(
        &mut self,
        mut raw_packets: Vec<Bytes>,
        now: Instant,
    ) -> Vec<Bytes> {
        // Pop unsent data chunks from the pending queue to send as much as
        // cwnd and rwnd allow.
        let (chunks, sis_to_reset) = self.pop_pending_data_chunks_to_send(now);
        if self.state() == AssociationState::Closed {
            return vec![];
        }
        if !chunks.is_empty() {
            // Start timer. (noop if already started)
            trace!("[{}] T3-rtx timer start (pt1)", self.side);
            self.timers
                .start_if_idle(Timer::T3RTX, now, self.rto_mgr.get_rto());

            self.bundle_data_chunks_into_packets(chunks, &mut raw_packets);
        }

        if !sis_to_reset.is_empty() || self.will_retransmit_reconfig {
            if self.will_retransmit_reconfig {
                self.will_retransmit_reconfig = false;
                debug!(
                    "[{}] retransmit {} RECONFIG chunk(s)",
                    self.side,
                    self.reconfigs.len()
                );
                for c in self.reconfigs.values() {
                    let p = self.create_packet(vec![Box::new(c.clone())]);
                    if let Ok(raw) = p.marshal() {
                        raw_packets.push(raw);
                    } else {
                        warn!(
                            "[{}] failed to serialize a RECONFIG packet to be retransmitted",
                            self.side,
                        );
                    }
                }
            }

            if !sis_to_reset.is_empty() {
                let rsn = self.generate_next_rsn();
                let tsn = self.my_next_tsn.wrapping_sub(1);
                debug!(
                    "[{}] sending RECONFIG: rsn={} tsn={} streams={:?}",
                    self.side,
                    rsn,
                    self.my_next_tsn.wrapping_sub(1),
                    sis_to_reset
                );

                let c = ChunkReconfig {
                    param_a: Some(Box::new(ParamOutgoingResetRequest {
                        reconfig_request_sequence_number: rsn,
                        sender_last_tsn: tsn,
                        stream_identifiers: sis_to_reset,
                        ..Default::default()
                    })),
                    ..Default::default()
                };
                self.reconfigs.insert(rsn, c.clone()); // store in the map for retransmission

                let p = self.create_packet(vec![Box::new(c)]);
                if let Ok(raw) = p.marshal() {
                    raw_packets.push(raw);
                } else {
                    warn!(
                        "[{}] failed to serialize a RECONFIG packet to be transmitted",
                        self.side
                    );
                }
            }

            if !self.reconfigs.is_empty() {
                self.timers
                    .start(Timer::Reconfig, now, self.rto_mgr.get_rto());
            }
        }

        raw_packets
    }

    fn gather_outbound_fast_retransmission_packets(
        &mut self,
        mut raw_packets: Vec<Bytes>,
        now: Instant,
    ) -> Vec<Bytes> {
        if self.will_retransmit_fast {
            self.will_retransmit_fast = false;

            let mut to_fast_retrans: Vec<ChunkPayloadData> = vec![];
            let mut fast_retrans_size = COMMON_HEADER_SIZE;

            let mut i = 0;
            loop {
                let tsn = self.cumulative_tsn_ack_point.wrapping_add(i + 1);
                if let Some(c) = self.inflight_queue.get(tsn) {
                    if !c.is_fast_retransmit_candidate() {
                        i += 1;
                        continue;
                    }

                    // Only the retry being considered needs a policy check.
                    // Retire an exhausted candidate's whole message first.
                    match self.abandon_retransmit_candidate(tsn, now) {
                        Ok(true) => {
                            to_fast_retrans.retain(|chunk| {
                                self.inflight_queue
                                    .get(chunk.tsn)
                                    .is_some_and(|c| !c.abandoned())
                            });
                            fast_retrans_size = COMMON_HEADER_SIZE
                                + to_fast_retrans
                                    .iter()
                                    .map(|c| {
                                        (DATA_CHUNK_HEADER_SIZE + c.user_data.len() as u32)
                                            .next_multiple_of(4)
                                    })
                                    .sum::<u32>();
                            i += 1;
                            continue;
                        }
                        Ok(false) => {}
                        Err(err) => {
                            self.fail_message_queue(err);
                            return vec![];
                        }
                    }
                    let Some(c) = self.inflight_queue.get(tsn) else {
                        break;
                    };

                    // RFC 4960 Sec 7.2.4 Fast Retransmit on Gap Reports
                    //  3)  Determine how many of the earliest (i.e., lowest TSN) DATA chunks
                    //      marked for retransmission will fit into a single packet, subject
                    //      to constraint of the path MTU of the destination transport
                    //      address to which the packet is being sent.  Call this value K.
                    //      Retransmit those K DATA chunks in a single packet.  When a Fast
                    //      Retransmit is being performed, the sender SHOULD ignore the value
                    //      of cwnd and SHOULD NOT delay retransmission for this single
                    //		packet.

                    // Padded wire size, the same accounting
                    // bundle_data_chunks_into_packets uses for bundle
                    // decisions.
                    let data_chunk_size =
                        (DATA_CHUNK_HEADER_SIZE + c.user_data.len() as u32).next_multiple_of(4);
                    if self.mtu < fast_retrans_size + data_chunk_size {
                        break;
                    }

                    fast_retrans_size += data_chunk_size;
                } else {
                    break; // end of pending data
                }

                if let Some(c) = self.inflight_queue.get(tsn) {
                    to_fast_retrans.push(c.clone());
                }
                i += 1;
            }

            for chunk in &mut to_fast_retrans {
                if let Some(c) = self.inflight_queue.get_mut(chunk.tsn) {
                    c.nsent += 1;
                    chunk.nsent = c.nsent;
                    self.stats.inc_fast_retrans();
                    trace!(
                        "[{}] fast-retransmit: tsn={} sent={} htna={}",
                        self.side, c.tsn, c.nsent, self.fast_recover_exit_point
                    );
                }
            }
            self.bundle_data_chunks_into_packets(to_fast_retrans, &mut raw_packets);
        }

        raw_packets
    }

    fn gather_outbound_sack_packets(&mut self, mut raw_packets: Vec<Bytes>) -> Vec<Bytes> {
        if self.ack_state == AckState::Immediate {
            self.ack_state = AckState::Idle;
            let sack = self.create_selective_ack_chunk();
            debug!("[{}] sending SACK: {}", self.side, sack);
            if let Ok(raw) = self.marshal_control_chunk(&sack) {
                raw_packets.push(raw);
            } else {
                warn!("[{}] failed to serialize a SACK packet", self.side);
            }
        }

        raw_packets
    }

    fn gather_outbound_forward_tsn_packets(&mut self, mut raw_packets: Vec<Bytes>) -> Vec<Bytes> {
        /*log::debug!(
            "[{}] gatherOutboundForwardTSNPackets {}",
            self.name,
            self.will_send_forward_tsn
        );*/
        if self.will_send_forward_tsn {
            self.will_send_forward_tsn = false;
            if sna32gt(
                self.advanced_peer_tsn_ack_point,
                self.cumulative_tsn_ack_point,
            ) {
                let fwd_tsn = self.create_forward_tsn();
                if let Ok(raw) = self.marshal_control_chunk(&fwd_tsn) {
                    raw_packets.push(raw);
                } else {
                    warn!("[{}] failed to serialize a Forward TSN packet", self.side);
                }
            }
        }

        raw_packets
    }

    fn gather_outbound_shutdown_packets(
        &mut self,
        mut raw_packets: Vec<Bytes>,
        now: Instant,
    ) -> (Vec<Bytes>, bool) {
        let mut ok = true;

        if self.will_send_shutdown {
            self.will_send_shutdown = false;

            let shutdown = ChunkShutdown {
                cumulative_tsn_ack: self.cumulative_tsn_ack_point,
            };

            if let Ok(raw) = self.create_packet(vec![Box::new(shutdown)]).marshal() {
                self.timers
                    .start(Timer::T2Shutdown, now, self.rto_mgr.get_rto());
                raw_packets.push(raw);
            } else {
                warn!("[{}] failed to serialize a Shutdown packet", self.side);
            }
        } else if self.will_send_shutdown_ack {
            self.will_send_shutdown_ack = false;

            let shutdown_ack = ChunkShutdownAck {};

            if let Ok(raw) = self.create_packet(vec![Box::new(shutdown_ack)]).marshal() {
                self.timers
                    .start(Timer::T2Shutdown, now, self.rto_mgr.get_rto());
                raw_packets.push(raw);
            } else {
                warn!("[{}] failed to serialize a ShutdownAck packet", self.side);
            }
        } else if self.will_send_shutdown_complete {
            self.will_send_shutdown_complete = false;

            let shutdown_complete = ChunkShutdownComplete {};

            if let Ok(raw) = self
                .create_packet(vec![Box::new(shutdown_complete)])
                .marshal()
            {
                raw_packets.push(raw);
                ok = false;
            } else {
                warn!(
                    "[{}] failed to serialize a ShutdownComplete packet",
                    self.side
                );
            }
        }

        (raw_packets, ok)
    }

    /// get_data_packets_to_retransmit is called when T3-rtx is timed out and retransmit outstanding data chunks
    /// that are not acked or abandoned yet.
    fn get_data_packets_to_retransmit(
        &mut self,
        now: Instant,
        raw_packets: &mut Vec<Bytes>,
    ) -> Result<()> {
        let awnd = std::cmp::min(self.cwnd, self.rwnd);
        let mut chunks: Vec<ChunkPayloadData> = vec![];
        let mut bytes_to_send = 0;
        let mut done = false;
        let mut i = 0;
        // Assume we will re-send every flagged chunk; flip back on if we stop
        // early (a marked chunk that doesn't fit awnd, or a zero-window probe)
        // so the next gather_outbound still scans.
        let mut chunks_remaining = false;
        while !done {
            let tsn = self.cumulative_tsn_ack_point.wrapping_add(i + 1);
            if let Some(c) = self.inflight_queue.get(tsn) {
                if !c.retransmit {
                    i += 1;
                    continue;
                }

                // A T3-marked candidate can expire while waiting for the send
                // window. Recheck it now (RFC 3758 TR4), without a window scan.
                if self.abandon_retransmit_candidate(tsn, now)? {
                    chunks.retain(|chunk| {
                        self.inflight_queue
                            .get(chunk.tsn)
                            .is_some_and(|c| !c.abandoned())
                    });
                    bytes_to_send = chunks.iter().map(|c| c.user_data.len()).sum();
                    i += 1;
                    continue;
                }
                let Some(c) = self.inflight_queue.get(tsn) else {
                    break;
                };

                if i == 0 && self.rwnd < c.user_data.len() as u32 {
                    // Send it as a zero window probe
                    done = true;
                    chunks_remaining = true;
                } else if bytes_to_send + c.user_data.len() > awnd as usize {
                    chunks_remaining = true;
                    break;
                }

                bytes_to_send += c.user_data.len();
            } else {
                break; // end of pending data
            }

            if let Some(c) = self.inflight_queue.get(tsn) {
                chunks.push(c.clone());
            }
            i += 1;
        }

        // Cleared once the whole in-flight window has been rescanned with nothing
        // left flagged; kept set while awnd/zero-window left chunks behind.
        self.t3_retransmit_pending = chunks_remaining;

        // A later candidate can abandon an earlier fragment already selected
        // into this batch. Commit attempts only after whole-message retirement.
        for chunk in &mut chunks {
            if let Some(c) = self.inflight_queue.get_mut(chunk.tsn) {
                c.retransmit = false;
                c.nsent += 1;
                chunk.nsent = c.nsent;
                trace!(
                    "[{}] retransmitting tsn={} ssn={} sent={}",
                    self.side, c.tsn, c.stream_sequence_number, c.nsent
                );
            }
        }
        self.bundle_data_chunks_into_packets(chunks, raw_packets);
        Ok(())
    }

    fn release_stream_buffers(&mut self, bytes_acked_per_stream: &FxHashMap<u16, i64>) {
        for (si, n_bytes_acked) in bytes_acked_per_stream {
            if *n_bytes_acked > 0 {
                // Report the exact bytes released for this stream (acknowledged OR
                // abandoned — both funnel through bytes_acked_per_stream) so upper
                // layers can decrement their own send-buffer accounting. Unlike the
                // edge-triggered BufferedAmountLow advisory below, this fires on
                // every release with the byte delta.
                self.events
                    .push_back(Event::Stream(StreamEvent::BufferedAmountReleased {
                        id: *si,
                        n_bytes: *n_bytes_acked as usize,
                    }));
            }
            if let Some(s) = self.streams.get_mut(si)
                && s.on_buffer_released(*n_bytes_acked)
            {
                trace!("StreamEvent::BufferedAmountLow");
                self.events
                    .push_back(Event::Stream(StreamEvent::BufferedAmountLow { id: *si }))
            }
        }
    }

    /// RFC 3758 section 3.5 C2-C4. Retain the stream map until the peer catches up.
    fn advance_peer_ack_point(&mut self) {
        let mut tsn = self.advanced_peer_tsn_ack_point.wrapping_add(1);
        while let Some(c) = self.inflight_queue.get(tsn) {
            if !c.abandoned() {
                break;
            }
            let (unordered, si, ssn) = (c.unordered, c.stream_identifier, c.stream_sequence_number);
            self.advanced_peer_tsn_ack_point = tsn;
            self.note_abandoned_for_forward_tsn(unordered, si, ssn);
            tsn = tsn.wrapping_add(1);
        }
        if sna32gt(
            self.advanced_peer_tsn_ack_point,
            self.cumulative_tsn_ack_point,
        ) {
            self.will_send_forward_tsn = true;
        } else {
            self.fwd_tsn_stream_map.clear();
        }
    }

    fn timed_data_expired(&self, c: &ChunkPayloadData, now: Instant) -> bool {
        self.use_forward_tsn
            && c.reliability
                .deadline()
                .is_some_and(|deadline| now >= deadline)
    }

    fn is_current_stream_data(&self, c: &ChunkPayloadData) -> bool {
        self.streams
            .get(&c.stream_identifier)
            .is_some_and(|s| s.generation == c.stream_generation)
    }

    /// A positive deadline can expire at any time. Timed(0) and Rexmit limits
    /// instead apply when this particular DATA is considered for retransmission.
    fn retransmission_policy_exhausted(
        &self,
        c: &ChunkPayloadData,
        now: Instant,
        is_candidate: bool,
    ) -> bool {
        if !self.use_forward_tsn {
            return false;
        }
        match c.reliability {
            MessageReliability::Reliable => false,
            MessageReliability::Timed { deadline } => {
                now >= deadline && (Some(deadline) != c.created_at || is_candidate)
            }
            // RFC 7496 3.1: the limit excludes the first transmission and counts
            // both fast and timer-based retries. nsent includes that first send.
            MessageReliability::Rexmit { max_retransmits } => {
                is_candidate && c.nsent > max_retransmits
            }
        }
    }

    fn abandon_retransmit_candidate(&mut self, tsn: u32, now: Instant) -> Result<bool> {
        let c = self
            .inflight_queue
            .get(tsn)
            .ok_or_else(|| Error::OtherSctpErr("missing retransmission candidate".into()))?;
        if !self.retransmission_policy_exhausted(c, now, true) {
            return Ok(false);
        }
        let message = MessageToAbandon {
            id: c.message_id.ok_or_else(|| {
                Error::OtherSctpErr("outbound DATA has no message identity".into())
            })?,
            sent_tsn: Some(tsn),
        };
        if self.abandon_message(message)? {
            self.on_messages_abandoned(now);
        }
        Ok(true)
    }

    /// Select identities without changing ACK, retry or buffer state. Only a
    /// fragment selected for recovery can exhaust that message's retry budget.
    fn unretransmittable_messages(
        &self,
        now: Instant,
        is_candidate: impl Fn(&ChunkPayloadData) -> bool,
    ) -> Result<Vec<MessageToAbandon>> {
        let mut messages = vec![];
        let mut selected = FxHashSet::default();
        for &tsn in &self.inflight_queue.sorted {
            let c = self
                .inflight_queue
                .get(tsn)
                .ok_or_else(|| Error::OtherSctpErr("missing inflight message fragment".into()))?;
            if c.is_outstanding()
                && self.retransmission_policy_exhausted(c, now, is_candidate(c))
                && let Some(id) = c.message_id
                && selected.insert(id)
            {
                messages.push(MessageToAbandon {
                    id,
                    sent_tsn: Some(tsn),
                });
            }
        }
        Ok(messages)
    }

    /// RFC 3758 A2/A3: abandon all fragments by identity, including Gap-ACKed
    /// siblings and an unsent tail. Each payload is released at most once.
    /// FORWARD TSN and timers are updated after the complete operation.
    fn abandon_message(&mut self, message: MessageToAbandon) -> Result<bool> {
        let whole_tsn = message.sent_tsn.filter(|&tsn| {
            self.inflight_queue.get(tsn).is_some_and(|c| {
                c.message_id == Some(message.id) && c.beginning_fragment && c.ending_fragment
            })
        });
        let mut inflight = whole_tsn.map_or_else(
            || self.inflight_queue.message_tsns(message.id),
            |tsn| vec![tsn],
        );
        let mut released_per_stream = FxHashMap::default();
        let mut changed = false;
        let tail = self.pending_queue.drain_message(message.id)?;
        let assign_tsns = !inflight.is_empty()
            || tail
                .first()
                .is_some_and(|c| !c.beginning_fragment || !c.unordered);
        for mut c in tail {
            if assign_tsns {
                c.tsn = self.generate_next_tsn();
                inflight.push(c.tsn);
                self.inflight_queue.push_no_check(c);
            } else {
                if self.is_current_stream_data(&c) {
                    *released_per_stream.entry(c.stream_identifier).or_default() +=
                        c.user_data.len() as i64;
                }
                changed = true;
            }
        }
        for tsn in inflight {
            if let Some(c) = self.inflight_queue.get_mut(tsn) {
                changed |= !c.abandoned();
                c.mark_abandoned();
                let (si, generation) = (c.stream_identifier, c.stream_generation);
                let released = self.inflight_queue.release_payload(tsn) as i64;
                if released > 0
                    && self
                        .streams
                        .get(&si)
                        .is_some_and(|s| s.generation == generation)
                {
                    *released_per_stream.entry(si).or_default() += released;
                }
            }
        }
        self.release_stream_buffers(&released_per_stream);
        Ok(changed)
    }

    fn fail_message_queue(&mut self, err: Error) {
        error!("[{}] inconsistent message queue: {err}", self.side);
        if let Err(err) = self.close(AssociationError::TransportError) {
            error!("[{}] failed to close association: {err}", self.side);
        }
        self.control_queue.clear();
        self.endpoint_events.push_back(EndpointEventInner::Drained);
    }

    fn on_messages_abandoned(&mut self, now: Instant) {
        self.advance_peer_ack_point();
        // RFC 3758 C5: recovery must continue even with no DATA left to retry.
        if sna32gt(
            self.advanced_peer_tsn_ack_point,
            self.cumulative_tsn_ack_point,
        ) {
            self.timers
                .start_if_idle(Timer::T3RTX, now, self.rto_mgr.get_rto());
        }
    }

    fn abandon_unretransmittable_messages(
        &mut self,
        now: Instant,
        is_candidate: impl Fn(&ChunkPayloadData) -> bool,
    ) -> Result<()> {
        let mut changed = false;
        for message in self.unretransmittable_messages(now, is_candidate)? {
            changed |= self.abandon_message(message)?;
        }
        if changed {
            self.on_messages_abandoned(now);
        }
        Ok(())
    }

    /// An unsent unordered message can be dropped without assigning TSNs.
    /// A selected fragment tail instead needs the whole-message abandonment path.
    /// Timed(0) permits first transmissions until an actual retry is considered.
    fn pending_message_expired(&self, c: &ChunkPayloadData, now: Instant) -> bool {
        (!c.beginning_fragment || c.unordered)
            && c.reliability.deadline() != c.created_at
            && self.timed_data_expired(c, now)
    }

    fn pending_message_to_abandon(&self) -> Result<MessageToAbandon> {
        Ok(MessageToAbandon {
            id: self
                .pending_queue
                .peek()
                .and_then(|c| c.message_id)
                .ok_or_else(|| {
                    Error::OtherSctpErr("outbound DATA has no message identity".into())
                })?,
            sent_tsn: None,
        })
    }

    /// pop_pending_data_chunks_to_send pops chunks from the pending queues as many as
    /// the cwnd and rwnd allows to send.
    fn pop_pending_data_chunks_to_send(
        &mut self,
        now: Instant,
    ) -> (Vec<ChunkPayloadData>, Vec<u16>) {
        let mut chunks = vec![];
        let mut sis_to_reset = vec![]; // stream identifiers to reset
        if !self.pending_queue.is_empty() {
            // RFC 4960 sec 6.1.  Transmission of DATA Chunks
            //   A) At any given time, the data sender MUST NOT transmit new data to
            //      any destination transport address if its peer's rwnd indicates
            //      that the peer has no buffer space (i.e., rwnd is 0; see Section
            //      6.2.1).  However, regardless of the value of rwnd (including if it
            //      is 0), the data sender can always have one DATA chunk in flight to
            //      the receiver if allowed by cwnd (see rule B, below).

            loop {
                let Some(c) = self.pending_queue.peek() else {
                    if let Some(reset) = self.pending_queue.pop_ready_reset() {
                        sis_to_reset.push(reset.stream_identifier);
                        continue;
                    }
                    break;
                };
                let (beginning_fragment, unordered, data_len) =
                    (c.beginning_fragment, c.unordered, c.user_data.len());

                if data_len == 0 {
                    self.fail_message_queue(Error::OtherSctpErr(
                        "outbound DATA has no user data".into(),
                    ));
                    return (vec![], vec![]);
                }

                // RFC 3758 timed reliability: a message whose lifetime has run
                // out must be abandoned even if it was never transmitted. Doing
                // this here, ahead of the window checks and before a TSN is
                // allocated, means there is no gap for the peer to reconcile and
                // no ForwardTSN to send.
                if self.pending_message_expired(c, now) {
                    match self
                        .pending_message_to_abandon()
                        .and_then(|message| self.abandon_message(message))
                    {
                        Ok(true) => self.on_messages_abandoned(now),
                        Ok(false) => {}
                        Err(err) => {
                            self.fail_message_queue(err);
                            return (vec![], vec![]);
                        }
                    }
                    continue;
                }

                if self.inflight_queue.get_num_bytes() + data_len > self.cwnd as usize {
                    break; // would exceeds cwnd
                }

                if data_len > self.rwnd as usize {
                    break; // no more rwnd
                }

                if let Some(chunk) = self.move_pending_data_chunk_to_inflight_queue(
                    beginning_fragment,
                    unordered,
                    now,
                ) {
                    self.rwnd -= data_len as u32;
                    chunks.push(chunk);
                } else {
                    self.fail_message_queue(Error::OtherSctpErr(
                        "failed to pop pending DATA".into(),
                    ));
                    return (vec![], vec![]);
                }
            }

            // the data sender can always have one DATA chunk in flight to the receiver
            if chunks.is_empty() && self.inflight_queue.is_empty() {
                // Send zero window probe
                if let Some(c) = self.pending_queue.peek() {
                    let (beginning_fragment, unordered) = (c.beginning_fragment, c.unordered);

                    if let Some(chunk) = self.move_pending_data_chunk_to_inflight_queue(
                        beginning_fragment,
                        unordered,
                        now,
                    ) {
                        chunks.push(chunk);
                    } else {
                        self.fail_message_queue(Error::OtherSctpErr(
                            "failed to pop pending DATA".into(),
                        ));
                        return (vec![], vec![]);
                    }
                }
            }
        }

        // RFC 7053: the I-bit asks the receiver to acknowledge at once when nothing follows
        // this burst until its SACK: once the send made us cwnd-limited, so cwnd reopens
        // without waiting a delayed-ack interval, and when it emptied the queue, so a lone
        // message's retransmission timer is set from the path's round trip rather than the
        // peer's ack delay.
        if let Some(last) = chunks.last_mut()
            && (self.pending_queue.is_empty()
                || self.inflight_queue.get_num_bytes() + self.max_payload_size as usize
                    >= self.cwnd as usize)
        {
            last.immediate_sack = true;
            if let Some(held) = self.inflight_queue.get_mut(last.tsn) {
                held.immediate_sack = true;
            }
        }

        (chunks, sis_to_reset)
    }

    /// bundle_data_chunks_into_packets packs DATA chunks into packets. It tries to bundle
    /// DATA chunks into a packet so long as the resulting packet size does not exceed
    /// the path MTU.
    fn bundle_data_chunks_into_packets(
        &self,
        chunks: Vec<ChunkPayloadData>,
        raw_packets: &mut Vec<Bytes>,
    ) {
        // RFC 4960 sec 6.1.  Transmission of DATA Chunks
        //   Multiple DATA chunks committed for transmission MAY be bundled in a
        //   single packet.  Furthermore, DATA chunks being retransmitted MAY be
        //   bundled with new DATA chunks, as long as the resulting packet size
        //   does not exceed the path MTU.
        //
        // Marshal each bundle straight into `raw_packets` from the borrowed
        // chunks: no intermediate `Vec<Packet>`, no `Box<dyn Chunk>` per chunk.
        // The chunks are already retained in the in-flight queue, so this send
        // copy is throwaway.
        if chunks.is_empty() {
            return;
        }
        let common_header = CommonHeader {
            verification_tag: self.peer_verification_tag,
            source_port: self.source_port,
            destination_port: self.destination_port,
        };

        // First pass: split the chunks into MTU-bounded datagrams and total up
        // their marshalled (4-byte-padded) length. The whole burst is then
        // written into ONE buffer and `split_to` hands out each datagram as a
        // zero-copy `Bytes` view sharing that single allocation — one malloc per
        // burst instead of one per packet on the hot send path. The bundle
        // boundaries are computed once here and reused below, so the MTU-split
        // rule lives in exactly one place.
        let hdr = COMMON_HEADER_SIZE as usize;
        let mut bundles: Vec<(usize, usize)> = Vec::new();
        let mut total_len = 0usize;
        let mut bundle_start = 0;
        let mut bundle_len = hdr;
        for (i, chunk) in chunks.iter().enumerate() {
            // Marshalled chunk size: header + payload, padded up to the SCTP
            // 4-byte boundary. Bundle decisions must use this wire size —
            // deciding on raw payload sizes admitted bundles that marshalled
            // past the MTU (e.g. payloads of 1147 + 16 pass a payload-only
            // check at an MTU of 1191 but serialize to a 1208-byte packet).
            let wire =
                (DATA_CHUNK_HEADER_SIZE as usize + chunk.user_data.len()).next_multiple_of(4);
            // Close the current bundle before a chunk whose padded wire size
            // would push the datagram past the MTU.
            if bundle_len + wire > self.mtu as usize && i > bundle_start {
                bundles.push((bundle_start, i));
                total_len += bundle_len;
                bundle_start = i;
                bundle_len = hdr;
            }
            bundle_len += wire;
        }
        bundles.push((bundle_start, chunks.len()));
        total_len += bundle_len;

        // Second pass: marshal each datagram into the shared buffer.
        let mut buf = BytesMut::with_capacity(total_len);
        for (start, end) in bundles {
            match Packet::write_framed(
                &common_header,
                chunks[start..end].iter().map(|c| c as &dyn Chunk),
                &mut buf,
            ) {
                Ok(_) => {
                    let plen = buf.len();
                    raw_packets.push(buf.split_to(plen).freeze());
                }
                Err(_) => {
                    warn!("[{}] failed to serialize a DATA packet", self.side);
                    buf.clear();
                }
            }
        }
    }

    /// generate_next_tsn returns the my_next_tsn and increases it. The caller should hold the lock.
    fn generate_next_tsn(&mut self) -> u32 {
        let tsn = self.my_next_tsn;
        self.my_next_tsn = self.my_next_tsn.wrapping_add(1);
        tsn
    }

    /// generate_next_rsn returns the my_next_rsn and increases it. The caller should hold the lock.
    fn generate_next_rsn(&mut self) -> u32 {
        let rsn = self.my_next_rsn;
        self.my_next_rsn += 1;
        rsn
    }

    fn create_selective_ack_chunk(&mut self) -> ChunkSelectiveAck {
        ChunkSelectiveAck {
            cumulative_tsn_ack: self.peer_last_tsn,
            advertised_receiver_window_credit: self.get_my_receiver_window_credit(),
            gap_ack_blocks: self.payload_queue.get_gap_ack_blocks(self.peer_last_tsn),
            duplicate_tsn: self.payload_queue.pop_duplicates(),
        }
    }

    /// Record an abandoned chunk into the forward-TSN stream map (RFC 3758 C4),
    /// called from the two C2 loops as `advanced_peer_tsn_ack_point` advances
    /// over each newly-abandoned in-flight chunk. Only *ordered* streams are
    /// tracked: the receiver ignores the per-stream SSN list for unordered
    /// chunks (it advances purely by `new_cumulative_tsn`). Keeps the greatest
    /// SSN seen per stream, which is the value RFC 3758 C4 requires to report.
    ///
    /// A stream's entry may briefly outlive the chunk that set it (until the
    /// window closes and the map is cleared), so a stale SSN can be re-reported;
    /// that is safe because the receiver only advances a stream forward and the
    /// SSN always corresponds to a really-abandoned chunk. The `u16` SSN compare
    /// (`sna16lt`) is sound because the window span is bounded by rwnd — a
    /// stream cannot lap the full 65536-sequence space before the window closes.
    fn note_abandoned_for_forward_tsn(&mut self, unordered: bool, si: u16, ssn: u16) {
        if unordered {
            return;
        }
        self.fwd_tsn_stream_map
            .entry(si)
            .and_modify(|cur| {
                if sna16lt(*cur, ssn) {
                    *cur = ssn;
                }
            })
            .or_insert(ssn);
    }

    /// Test-only observer for the incremental forward-TSN stream map, so the
    /// endpoint tests can assert it is cleared once the forward-TSN window
    /// closes (the C1/C3 clear paths, which the unit tests can't reach).
    #[cfg(test)]
    pub(crate) fn fwd_tsn_stream_map_is_empty(&self) -> bool {
        self.fwd_tsn_stream_map.is_empty()
    }

    /// create_forward_tsn generates ForwardTSN chunk.
    /// This method will be be called if use_forward_tsn is set to false.
    fn create_forward_tsn(&self) -> ChunkForwardTsn {
        // RFC 3758 Sec 3.5 C4: report, once per ordered stream, the greatest
        // stream-sequence-number among abandoned chunks in the forward-TSN
        // window. This is maintained incrementally in `fwd_tsn_stream_map` (see
        // its declaration and the two C2 loops that feed it), so we no longer
        // rescan `(cumulative_tsn_ack_point, advanced_peer_tsn_ack_point]` with
        // a per-TSN hashmap probe on every FORWARD-TSN — that scan was O(rwnd)
        // (~1000 probes/call for a 1 MB window) and dominated the send profile.
        let mut fwd_tsn = ChunkForwardTsn {
            new_cumulative_tsn: self.advanced_peer_tsn_ack_point,
            streams: Vec::with_capacity(self.fwd_tsn_stream_map.len()),
        };
        for (si, ssn) in &self.fwd_tsn_stream_map {
            fwd_tsn.streams.push(ChunkForwardTsnStream {
                identifier: *si,
                sequence: *ssn,
            });
        }
        // `trace!` evaluates its arguments lazily, so the stream list is only
        // formatted when trace logging is enabled -- no per-FORWARD-TSN string
        // allocation on the hot send path (this fires often for PR-SCTP data
        // channels, which is exactly where it was showing up in profiles).
        trace!(
            "[{}] building fwd_tsn: newCumulativeTSN={} cumTSN={} streams={:?}",
            self.side, fwd_tsn.new_cumulative_tsn, self.cumulative_tsn_ack_point, fwd_tsn.streams
        );

        fwd_tsn
    }

    /// Move the chunk peeked with self.pending_queue.peek() to the inflight_queue.
    fn move_pending_data_chunk_to_inflight_queue(
        &mut self,
        beginning_fragment: bool,
        unordered: bool,
        now: Instant,
    ) -> Option<ChunkPayloadData> {
        if let Some(mut c) = self.pending_queue.pop(beginning_fragment, unordered) {
            // Assign TSN
            c.tsn = self.generate_next_tsn();

            // RTT baseline only; maxPacketLifeTime measures from `created_at`.
            c.since = Some(now);
            c.nsent = 1; // being sent for the first time

            trace!(
                "[{}] sending ppi={} tsn={} ssn={} sent={} len={} ({},{})",
                self.side,
                c.payload_type as u32,
                c.tsn,
                c.stream_sequence_number,
                c.nsent,
                c.user_data.len(),
                c.beginning_fragment,
                c.ending_fragment
            );

            self.inflight_queue.push_no_check(c.clone());

            Some(c)
        } else {
            error!("[{}] failed to pop from pending queue", self.side);
            None
        }
    }

    /// Queues an outgoing stream-reset marker (RFC 6525). Its retransmission
    /// timer starts when the RE-CONFIG request is emitted, not when queued.
    pub(crate) fn send_reset_request(
        &mut self,
        _now: Instant,
        stream_identifier: StreamId,
    ) -> Result<()> {
        let state = self.state();
        if state != AssociationState::Established {
            return Err(Error::ErrResetPacketInStateNotExist);
        }

        self.pending_queue
            .push_reset(ResetMarker { stream_identifier });
        self.awake_write_loop();

        Ok(())
    }

    /// Allocate an identity before consuming input or changing stream state.
    pub(crate) fn allocate_message_id(&mut self) -> Result<MessageId> {
        let state = self.state();
        if state != AssociationState::Established {
            return Err(Error::ErrPayloadDataStateNotExist);
        }

        let id = MessageId::new(self.next_message_id)
            .ok_or_else(|| Error::OtherSctpErr("message identity exhausted".into()))?;
        // MessageId::new has already checked that this increment cannot overflow.
        self.next_message_id += 1;
        Ok(id)
    }

    /// send_payload_data sends the data chunks with their allocated identity.
    ///
    /// The queueing instant is already recorded on each chunk by `Stream::packetize`, so this
    /// does not need one of its own.
    pub(crate) fn send_payload_data(&mut self, id: MessageId, mut chunks: Vec<ChunkPayloadData>) {
        for c in &mut chunks {
            c.message_id = Some(id);
        }
        self.pending_queue.push_message(chunks);

        self.awake_write_loop();
    }

    /// User payload bytes queued and not yet handed to a packet, over every stream: what a
    /// channel's `bufferedAmount` means to W3C ("not yet transmitted"), as against the stream's
    /// count, which holds bytes until they are acknowledged.
    pub fn pending_bytes(&self) -> usize {
        self.pending_queue.get_num_bytes()
    }

    /// buffered_amount returns total amount (in bytes) of currently buffered user data.
    /// This is used only by testing.
    pub(crate) fn buffered_amount(&self) -> usize {
        self.pending_queue.get_num_bytes() + self.inflight_queue.get_num_bytes()
    }

    fn awake_write_loop(&self) {
        // No Op on Purpose
    }

    fn close_all_timers(&mut self) {
        // Close all retransmission & ack timers
        for timer in Timer::VALUES {
            self.timers.stop(timer);
        }
    }

    fn on_ack_timeout(&mut self) {
        trace!(
            "[{}] ack timed out (ack_state: {})",
            self.side, self.ack_state
        );
        self.stats.inc_ack_timeouts();
        self.ack_state = AckState::Immediate;
        self.awake_write_loop();
    }

    fn on_retransmission_timeout(&mut self, timer_id: Timer, n_rtos: usize, now: Instant) {
        match timer_id {
            Timer::T1Init => {
                if let Err(err) = self.send_init() {
                    debug!(
                        "[{}] failed to retransmit init (n_rtos={}): {:?}",
                        self.side, n_rtos, err
                    );
                }
            }

            Timer::T1Cookie => {
                if let Err(err) = self.send_cookie_echo() {
                    debug!(
                        "[{}] failed to retransmit cookie-echo (n_rtos={}): {:?}",
                        self.side, n_rtos, err
                    );
                }
            }

            Timer::T2Shutdown => {
                debug!(
                    "[{}] retransmission of shutdown timeout (n_rtos={})",
                    self.side, n_rtos
                );
                let state = self.state();
                match state {
                    AssociationState::ShutdownSent => {
                        self.will_send_shutdown = true;
                        self.awake_write_loop();
                    }
                    AssociationState::ShutdownAckSent => {
                        self.will_send_shutdown_ack = true;
                        self.awake_write_loop();
                    }
                    _ => {}
                }
            }

            Timer::T3RTX => {
                self.stats.inc_t3timeouts();
                self.rto_mgr.backoff();
                // Refresh before C2 or mark_all_to_retrasmit reads abandoned().
                if let Err(err) =
                    self.abandon_unretransmittable_messages(now, ChunkPayloadData::is_outstanding)
                {
                    self.fail_message_queue(err);
                    return;
                }

                // RFC 4960 sec 6.3.3
                //  E1)  For the destination address for which the timer expires, adjust
                //       its ssthresh with rules defined in Section 7.2.3 and set the
                //       cwnd <- MTU.
                // RFC 4960 sec 7.2.3
                //   When the T3-rtx timer expires on an address, SCTP should perform slow
                //   start by:
                //      ssthresh = max(cwnd/2, 4*MTU)
                //      cwnd = 1*MTU

                self.ssthresh = std::cmp::max(self.cwnd / 2, 4 * self.mtu);
                self.cwnd = self.mtu;
                trace!(
                    "[{}] updated cwnd={} ssthresh={} inflight={} (RTO)",
                    self.side,
                    self.cwnd,
                    self.ssthresh,
                    self.inflight_queue.get_num_bytes()
                );

                // RFC 3758 sec 3.5
                //  A5) Any time the T3-rtx timer expires, on any destination, the sender
                //  SHOULD try to advance the "Advanced.Peer.Ack.Point" by following
                //  the procedures outlined in C2 - C5.
                if self.use_forward_tsn {
                    self.advance_peer_ack_point();
                }

                debug!(
                    "[{}] T3-rtx timed out: n_rtos={} cwnd={} ssthresh={}",
                    self.side, n_rtos, self.cwnd, self.ssthresh
                );

                self.inflight_queue.mark_all_to_retrasmit();
                self.t3_retransmit_pending = true;
                self.awake_write_loop();
            }

            Timer::Reconfig => {
                self.will_retransmit_reconfig = true;
                self.awake_write_loop();
            }

            _ => {}
        }
    }

    fn on_retransmission_failure(&mut self, id: Timer) {
        match id {
            Timer::T1Init => {
                error!("[{}] retransmission failure: T1-init", self.side);
                self.error = Some(AssociationError::HandshakeFailed(
                    Error::ErrHandshakeInitAck.to_string(),
                ));
            }

            Timer::T1Cookie => {
                error!("[{}] retransmission failure: T1-cookie", self.side);
                self.error = Some(AssociationError::HandshakeFailed(
                    Error::ErrHandshakeCookieEcho.to_string(),
                ));
            }

            Timer::T2Shutdown => {
                error!("[{}] retransmission failure: T2-shutdown", self.side);
            }

            Timer::T3RTX => {
                // T3-rtx timer will not fail by design
                // Justifications:
                //  * ICE would fail if the connectivity is lost
                //  * WebRTC spec is not clear how this incident should be reported to ULP
                error!("[{}] retransmission failure: T3-rtx (DATA)", self.side);
            }

            _ => {}
        }
    }

    /// Whether no timers are running
    #[cfg(test)]
    pub(crate) fn is_idle(&self) -> bool {
        Timer::VALUES
            .iter()
            //.filter(|&&t| t != Timer::KeepAlive && t != Timer::PushNewCid)
            .filter_map(|&t| Some((t, self.timers.get(t)?)))
            .min_by_key(|&(_, time)| time)
            //.map_or(true, |(timer, _)| timer == Timer::Idle)
            .is_none()
    }
}
