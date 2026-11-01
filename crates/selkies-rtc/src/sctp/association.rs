/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The association and its data channels, sans-IO: SCTP packets in, SCTP packets out.
//!
//! `Sctp` drives one `rtc_sctp::Association` through its `Endpoint`, and keeps the data
//! channels on top of it: DCEP on each stream, the odd/even stream ids of a client and a
//! server, what a channel sends before the association is established, and the receive side
//! that turns reassembled messages into `Message`s. Nothing here reads a clock or a socket:
//! every call takes `now`, outgoing packets wait in `poll_transmit`, and what the engine has
//! to hear waits in `poll_event`.

use std::collections::{HashMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use log::{debug, warn};
use rtc_sctp::{
    Association, AssociationHandle, ClientConfig, DatagramEvent, Endpoint, EndpointConfig, Event,
    Payload, PayloadProtocolIdentifier, ReliabilityType, ServerConfig, Side, StreamEvent,
    TimerConfig, TransportConfig,
};
use rtc_shared::TransportProtocol;

use super::channel::{ChannelState, DataChannelParameters, Message};
use super::dcep::{self, DcepMessage};

/// What this side accepts in one data channel message, announced as `a=max-message-size`:
/// larger clipboard and file messages with less chunking, and the browsers' own ceiling.
pub const LOCAL_MAX_MESSAGE_SIZE: u32 = 262_144;

/// What a peer that announces no `a=max-message-size` is taken to receive: Chromium answers
/// legacy DTLS/SCTP m-lines without the attribute yet receives up to 256 KiB, and Firefox
/// and WebKit accept more. RFC 8841's 64 KiB fallback would cap clipboard and file messages
/// needlessly.
pub const DEFAULT_REMOTE_MAX_MESSAGE_SIZE: u32 = 262_144;

/// The receive window advertised to the peer. A browser's bulk upload is window-capped at
/// `rwnd / RTT` once past slow start, so a small window throttles any high-RTT path, TURN
/// relays especially; the cost is a larger worst-case reassembly buffer, bounded by
/// `MAX_REASSEMBLY_BYTES`.
pub const ADVERTISED_RWND: u32 = 4 * 1024 * 1024;

/// The ceiling on what the inbound streams buffer together before the association is aborted
/// (the fourth of the six fixes, `vendor/rtc-sctp`).
pub const MAX_REASSEMBLY_BYTES: u32 = 16 * 1024 * 1024;

/// The SCTP port both sides of a WebRTC association use.
pub const SCTP_PORT: u16 = 5000;

/// The peer's SCTP capabilities as its description announced them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SctpCapabilities {
    /// The largest message the peer receives, or 0 for any size.
    pub max_message_size: u32,
}

/// The state of the transport, as `RTCSctpTransport.state` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportState {
    /// Started, the handshake not yet complete.
    Connecting,
    /// Established: channels open and messages flow.
    Connected,
    /// Aborted or shut down; no event follows.
    Closed,
}

/// What the association tells the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SctpEvent {
    /// The association is established.
    Connected,
    /// The association ended, every channel with it.
    Closed,
    /// The peer opened a channel; it is open from this moment.
    ChannelOpened(u16, DataChannelParameters),
    /// A channel this side opened or closed changed state.
    ChannelState(u16, ChannelState),
    /// A message arrived on a channel.
    Message(u16, Message),
    /// A channel's buffered amount fell to its low threshold.
    BufferedAmountLow(u16),
}

/// Why a send was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    /// The channel is not open.
    #[error("data channel {id} is {state}")]
    NotOpen {
        /// The channel.
        id: u16,
        /// Its state.
        state: &'static str,
    },
    /// The message exceeds what the peer declared it receives; sending it would end the
    /// channel on the peer's side, as a browser's `RTCDataChannel.send()` refuses the same.
    #[error("cannot send {len} bytes: exceeds the peer's maximum message size of {max} bytes")]
    TooLarge {
        /// The message's length.
        len: usize,
        /// The peer's ceiling.
        max: usize,
    },
    /// No such channel.
    #[error("no data channel {0}")]
    Unknown(u16),
    /// The association refused the write.
    #[error("SCTP refused the write: {0}")]
    Transport(String),
}

/// Why a channel could not be opened.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OpenError {
    /// A negotiated channel needs an id of its own.
    #[error("ID must be in range 0-65534 if data channel is negotiated out-of-band")]
    NegotiatedWithoutId,
    /// The stream id is already a channel's.
    #[error("data channel with ID {0} already registered")]
    Taken(u16),
    /// The transport is closed.
    #[error("the SCTP transport is closed")]
    Closed,
}

struct Channel {
    params: DataChannelParameters,
    state: ChannelState,
    /// Bytes queued for the channel before the association was established.
    queued: usize,
    low_threshold: usize,
    /// Whether the buffered amount was last seen above the threshold.
    above_low: bool,
}

/// A send waiting for the association to be established.
struct Queued {
    stream_id: u16,
    ppid: PayloadProtocolIdentifier,
    data: Bytes,
    /// Whether the bytes count toward the channel's buffered amount (DCEP does not).
    counted: bool,
}

/// One SCTP association and the data channels riding it.
pub struct Sctp {
    side: Side,
    endpoint: Endpoint,
    handle: Option<AssociationHandle>,
    assoc: Option<Association>,
    remote_addr: SocketAddr,
    state: TransportState,
    remote_max_message_size: usize,
    channels: HashMap<u16, Channel>,
    next_channel_id: u16,
    queue: VecDeque<Queued>,
    events: VecDeque<SctpEvent>,
    transmit: VecDeque<Bytes>,
}

impl Sctp {
    /// Start an association: a client sends its INIT with the first `poll_transmit`, a server
    /// waits for the peer's.
    ///
    /// `remote_caps` is the peer's `a=max-message-size`, which bounds what one message may
    /// carry to it; a peer that announced none is taken at `DEFAULT_REMOTE_MAX_MESSAGE_SIZE`.
    pub fn new(side: Side, remote_caps: Option<SctpCapabilities>, now: Instant) -> Sctp {
        Sctp::new_bounded(side, remote_caps, MAX_REASSEMBLY_BYTES, now)
    }

    /// `new` with the reassembly ceiling the association aborts past.
    pub fn new_bounded(
        side: Side,
        remote_caps: Option<SctpCapabilities>,
        max_reassembly_bytes: u32,
        now: Instant,
    ) -> Sctp {
        let remote_max_message_size = match remote_caps {
            Some(caps) if caps.max_message_size > 0 => caps.max_message_size,
            _ => DEFAULT_REMOTE_MAX_MESSAGE_SIZE,
        };
        let transport = TransportConfig::default()
            .with_sctp_port(SCTP_PORT)
            .with_max_receive_buffer_size(ADVERTISED_RWND)
            .with_max_message_size(remote_max_message_size)
            .with_max_reassembly_bytes(max_reassembly_bytes)
            // A path can go quiet for as long as ICE consent holds: DATA is retried until ICE
            // or DTLS gives up on the peer, never given up on alone.
            .with_timer_config(TimerConfig {
                max_t3_rtx_retrans: usize::MAX,
                max_reconfig_retrans: usize::MAX,
                ..TimerConfig::default()
            });
        let local_addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, SCTP_PORT));
        let remote_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, SCTP_PORT));
        let server_config = side
            .is_server()
            .then(|| Arc::new(ServerConfig::new(transport.clone())));
        let mut endpoint = Endpoint::new(
            local_addr,
            TransportProtocol::UDP,
            Arc::new(EndpointConfig::new()),
            server_config,
        );
        let mut state = TransportState::Connecting;
        let mut handle = None;
        let mut assoc = None;
        if side.is_client() {
            match endpoint.connect(now, ClientConfig::new(transport), remote_addr) {
                Ok((connected_handle, connected)) => {
                    handle = Some(connected_handle);
                    assoc = Some(connected);
                }
                Err(err) => {
                    warn!(target: "webrtc", "SCTP association could not start: {err}");
                    state = TransportState::Closed;
                }
            }
        }
        let mut sctp = Sctp {
            side,
            endpoint,
            handle,
            assoc,
            remote_addr,
            state,
            remote_max_message_size: remote_max_message_size as usize,
            channels: HashMap::new(),
            // One side uses even ids, the other odd, so both may open channels at once.
            next_channel_id: if side.is_server() { 0 } else { 1 },
            queue: VecDeque::new(),
            events: VecDeque::new(),
            transmit: VecDeque::new(),
        };
        if side.is_client() {
            sctp.endpoint.reject_new_associations();
        }
        sctp.pump(now);
        sctp
    }

    /// Which side of the association this is.
    pub fn side(&self) -> Side {
        self.side
    }

    /// The transport's state.
    pub fn state(&self) -> TransportState {
        self.state
    }

    /// The largest message the peer receives, in bytes.
    pub fn remote_max_message_size(&self) -> usize {
        self.remote_max_message_size
    }

    /// The streams the association holds, inbound and outbound.
    pub fn stream_ids(&self) -> Vec<u16> {
        let mut ids = self
            .assoc
            .as_ref()
            .map(Association::stream_ids)
            .unwrap_or_default();
        ids.sort_unstable();
        ids
    }

    /// Hand one SCTP packet from the DTLS transport to the association.
    ///
    /// A packet that fails to parse, names the wrong verification tag, or carries a chunk the
    /// association's state refuses is dropped and the association kept (the second of the six
    /// fixes); an INIT under another tag while an association runs is dropped too, since the
    /// running one is the session's.
    pub fn handle_packet(&mut self, data: Bytes, now: Instant) {
        if self.state == TransportState::Closed {
            return;
        }
        match self.endpoint.handle(now, self.remote_addr, None, data) {
            Some((handle, DatagramEvent::NewAssociation(assoc))) => {
                self.handle = Some(handle);
                self.assoc = Some(assoc);
                // One association per transport: a retransmitted INIT reaches this one, any
                // other is refused without a slot in the endpoint.
                self.endpoint.reject_new_associations();
            }
            Some((_, DatagramEvent::AssociationEvent(event))) => {
                if let Some(assoc) = self.assoc.as_mut() {
                    assoc.handle_event(event);
                }
            }
            Some(_) | None => {}
        }
        self.pump(now);
    }

    /// When `handle_timeout` is next due.
    pub fn poll_timeout(&self) -> Option<Instant> {
        self.assoc.as_ref().and_then(Association::poll_timeout)
    }

    /// Fire the timers due at `now`.
    pub fn handle_timeout(&mut self, now: Instant) {
        if let Some(assoc) = self.assoc.as_mut() {
            assoc.handle_timeout(now);
        }
        self.pump(now);
    }

    /// The next SCTP packet for the DTLS transport, if any.
    pub fn poll_transmit(&mut self, now: Instant) -> Option<Bytes> {
        if self.transmit.is_empty() {
            self.gather(now);
        }
        self.transmit.pop_front()
    }

    /// The next event for the engine, if any.
    pub fn poll_event(&mut self) -> Option<SctpEvent> {
        self.events.pop_front()
    }

    /// Open a data channel: a negotiated one on its agreed id, any other with a
    /// `DATA_CHANNEL_OPEN` the peer acknowledges, sent once the association is established.
    pub fn open_channel(
        &mut self,
        params: DataChannelParameters,
        now: Instant,
    ) -> Result<u16, OpenError> {
        if self.state == TransportState::Closed {
            return Err(OpenError::Closed);
        }
        let id = match params.id {
            Some(id) if id > 65534 => return Err(OpenError::NegotiatedWithoutId),
            Some(id) => id,
            None if params.negotiated => return Err(OpenError::NegotiatedWithoutId),
            None => {
                let mut id = self.next_channel_id;
                while self.channels.contains_key(&id) {
                    id = id.wrapping_add(2);
                }
                id
            }
        };
        if self.channels.contains_key(&id) {
            return Err(OpenError::Taken(id));
        }
        let negotiated = params.negotiated;
        let established = self.state == TransportState::Connected;
        self.channels.insert(
            id,
            Channel {
                params: params.clone(),
                state: if negotiated && established {
                    ChannelState::Open
                } else {
                    ChannelState::Connecting
                },
                queued: 0,
                low_threshold: 0,
                above_low: false,
            },
        );
        if !negotiated {
            self.queue.push_back(Queued {
                stream_id: id,
                ppid: PayloadProtocolIdentifier::Dcep,
                data: dcep::encode_open(&params),
                counted: false,
            });
        }
        if established {
            self.flush(now);
        }
        Ok(id)
    }

    /// Send a message on an open channel.
    pub fn send(&mut self, id: u16, message: Message, now: Instant) -> Result<(), SendError> {
        let channel = self.channels.get(&id).ok_or(SendError::Unknown(id))?;
        if channel.state != ChannelState::Open {
            return Err(SendError::NotOpen {
                id,
                state: channel.state.as_str(),
            });
        }
        let (ppid, data) = match message {
            Message::Text(text) if text.is_empty() => (
                PayloadProtocolIdentifier::StringEmpty,
                Bytes::from_static(&[0]),
            ),
            Message::Text(text) => (PayloadProtocolIdentifier::String, Bytes::from(text)),
            Message::Binary(bytes) if bytes.is_empty() => (
                PayloadProtocolIdentifier::BinaryEmpty,
                Bytes::from_static(&[0]),
            ),
            Message::Binary(bytes) => (PayloadProtocolIdentifier::Binary, bytes),
        };
        if data.len() > self.remote_max_message_size {
            return Err(SendError::TooLarge {
                len: data.len(),
                max: self.remote_max_message_size,
            });
        }
        // Over its threshold before the association sends any of it, so a large message's
        // drain is seen whatever goes out at once.
        let len = data.len();
        if let Some(channel) = self.channels.get_mut(&id)
            && len > channel.low_threshold
        {
            channel.above_low = true;
        }
        self.write(id, ppid, data, now)?;
        self.note_buffered(id);
        Ok(())
    }

    /// Close a channel: its stream is reset once the association is established, else what
    /// it queued is dropped and it is closed at once.
    pub fn close_channel(&mut self, id: u16, now: Instant) {
        let Some(channel) = self.channels.get_mut(&id) else {
            return;
        };
        if matches!(channel.state, ChannelState::Closing | ChannelState::Closed) {
            return;
        }
        if self.state == TransportState::Connected {
            channel.state = ChannelState::Closing;
            self.events
                .push_back(SctpEvent::ChannelState(id, ChannelState::Closing));
            if let Some(assoc) = self.assoc.as_mut()
                && let Ok(mut stream) = assoc.stream(id)
                && let Err(err) = stream.close(now)
            {
                debug!(target: "webrtc", "SCTP stream {id} reset refused: {err}");
            }
            self.gather(now);
        } else {
            self.queue.retain(|queued| queued.stream_id != id);
            self.channels.remove(&id);
            self.events
                .push_back(SctpEvent::ChannelState(id, ChannelState::Closed));
        }
    }

    /// A channel's state, or `None` for an unknown one.
    pub fn channel_state(&self, id: u16) -> Option<ChannelState> {
        self.channels.get(&id).map(|channel| channel.state)
    }

    /// A channel's parameters, or `None` for an unknown one.
    pub fn channel_parameters(&self, id: u16) -> Option<&DataChannelParameters> {
        self.channels.get(&id).map(|channel| &channel.params)
    }

    /// Bytes queued on a channel and not yet transmitted, as W3C's `bufferedAmount` counts
    /// them. A bulk transfer waits on this against `DATA_CHANNEL_BULK_HIGH_WATER`, so bytes
    /// in flight do not count: they would cap it at the high water per round trip, whatever
    /// the congestion window. The association counts what waits per association, so with two
    /// channels sending at once this reads up to the other's share high, bounded by what this
    /// channel has unacknowledged.
    pub fn buffered_amount(&mut self, id: u16) -> usize {
        let queued = self.channels.get(&id).map_or(0, |channel| channel.queued);
        let Some(assoc) = self.assoc.as_mut() else {
            return queued;
        };
        let pending = assoc.pending_bytes();
        let unacknowledged = assoc
            .stream(id)
            .ok()
            .and_then(|stream| stream.buffered_amount().ok())
            .unwrap_or(0);
        queued + pending.min(unacknowledged)
    }

    /// Set the buffered amount at which `BufferedAmountLow` fires for a channel: when the
    /// amount falls from above it to it or below.
    pub fn set_buffered_amount_low_threshold(&mut self, id: u16, threshold: usize) {
        if let Some(channel) = self.channels.get_mut(&id) {
            channel.low_threshold = threshold;
        }
    }

    /// Raise `BufferedAmountLow` for each channel whose amount fell through its threshold.
    fn check_buffered_low(&mut self) {
        let waiting: Vec<u16> = self
            .channels
            .iter()
            .filter(|(_, channel)| channel.above_low)
            .map(|(id, _)| *id)
            .collect();
        for id in waiting {
            let amount = self.buffered_amount(id);
            if let Some(channel) = self.channels.get_mut(&id)
                && amount <= channel.low_threshold
            {
                channel.above_low = false;
                self.events.push_back(SctpEvent::BufferedAmountLow(id));
            }
        }
    }

    /// Note a send on a channel that may have lifted it over its threshold.
    fn note_buffered(&mut self, id: u16) {
        let amount = self.buffered_amount(id);
        if let Some(channel) = self.channels.get_mut(&id)
            && amount > channel.low_threshold
        {
            channel.above_low = true;
        }
    }

    /// Abort the association and close every channel.
    pub fn stop(&mut self, now: Instant) {
        if self.state == TransportState::Closed {
            return;
        }
        if let Some(assoc) = self.assoc.as_mut()
            && let Err(err) = assoc.abort()
        {
            debug!(target: "webrtc", "SCTP abort refused: {err}");
        }
        self.gather(now);
        self.close_all();
    }

    /// Drain the association's events, endpoint events, and state into this side's.
    fn pump(&mut self, now: Instant) {
        if self.state == TransportState::Closed {
            return;
        }
        let mut events = Vec::new();
        if let Some(assoc) = self.assoc.as_mut() {
            while let Some(event) = assoc.poll() {
                events.push(event);
            }
            while let Some(event) = assoc.poll_endpoint_event() {
                if let Some(handle) = self.handle {
                    self.endpoint.handle_event(handle, event);
                }
            }
        }
        for event in events {
            self.on_event(event, now);
            if self.state == TransportState::Closed {
                return;
            }
        }
        // A server's association reads closed until the peer's COOKIE ECHO: only one that got
        // past that, or a client's, has ended.
        let started = self.side.is_client() || self.state == TransportState::Connected;
        if started && self.assoc.as_ref().is_some_and(Association::is_closed) {
            self.gather(now);
            self.close_all();
        }
    }

    fn on_event(&mut self, event: Event, now: Instant) {
        match event {
            Event::Connected => {
                self.state = TransportState::Connected;
                self.events.push_back(SctpEvent::Connected);
                let negotiated: Vec<u16> = self
                    .channels
                    .iter()
                    .filter(|(_, channel)| {
                        channel.params.negotiated && channel.state != ChannelState::Open
                    })
                    .map(|(id, _)| *id)
                    .collect();
                for id in negotiated {
                    self.set_channel_state(id, ChannelState::Open);
                }
                self.flush(now);
            }
            Event::HandshakeFailed { reason } => {
                debug!(target: "webrtc", "SCTP handshake failed: {reason}");
                self.gather(now);
                self.close_all();
            }
            Event::AssociationLost { id, .. } => {
                if self.channels.contains_key(&id) {
                    self.channels.remove(&id);
                    self.events
                        .push_back(SctpEvent::ChannelState(id, ChannelState::Closed));
                }
            }
            Event::Stream(StreamEvent::Opened { id }) => {
                if let Some(assoc) = self.assoc.as_mut() {
                    while assoc.accept_stream().is_some() {}
                }
                // A negotiated channel the peer spoke on first: its stream takes its reliability.
                if let Some(params) = self.channels.get(&id).map(|c| c.params.clone()) {
                    self.set_reliability(id, &params);
                }
            }
            Event::Stream(StreamEvent::Readable { id }) => self.read_stream(id, now),
            Event::Stream(_) | Event::DatagramReceived => {}
            _ => {}
        }
    }

    /// Deliver every complete message a stream holds.
    fn read_stream(&mut self, id: u16, now: Instant) {
        loop {
            let Some(assoc) = self.assoc.as_mut() else {
                return;
            };
            let Ok(mut stream) = assoc.stream(id) else {
                return;
            };
            let Ok(Some(chunks)) = stream.read() else {
                return;
            };
            let payload = match chunks.to_payload(usize::MAX) {
                Ok(payload) => payload.freeze(),
                Err(_) => continue,
            };
            self.on_message(id, chunks.ppi, payload, now);
        }
    }

    fn on_message(&mut self, id: u16, ppid: PayloadProtocolIdentifier, data: Bytes, now: Instant) {
        match ppid {
            PayloadProtocolIdentifier::Dcep => match dcep::decode(id, &data) {
                Ok(DcepMessage::Open(params)) => {
                    if self.channels.contains_key(&id) {
                        // An OPEN reusing a live stream is a protocol violation, ignored.
                        debug!(target: "webrtc", "SCTP: ignoring DATA_CHANNEL_OPEN for existing stream {id}");
                        return;
                    }
                    self.channels.insert(
                        id,
                        Channel {
                            params: params.clone(),
                            state: ChannelState::Open,
                            queued: 0,
                            low_threshold: 0,
                            above_low: false,
                        },
                    );
                    self.set_reliability(id, &params);
                    if let Err(err) =
                        self.write(id, PayloadProtocolIdentifier::Dcep, dcep::encode_ack(), now)
                    {
                        debug!(target: "webrtc", "SCTP: DATA_CHANNEL_ACK on stream {id} failed: {err}");
                    }
                    self.events.push_back(SctpEvent::ChannelOpened(id, params));
                }
                Ok(DcepMessage::Ack) => {
                    if !self.channels.contains_key(&id) {
                        debug!(target: "webrtc", "SCTP: ignoring DATA_CHANNEL_ACK for unknown stream {id}");
                        return;
                    }
                    self.set_channel_state(id, ChannelState::Open);
                }
                Err(err) => debug!(target: "webrtc", "SCTP: dropping DCEP on stream {id}: {err}"),
            },
            PayloadProtocolIdentifier::String if self.channels.contains_key(&id) => {
                let text = String::from_utf8_lossy(&data).into_owned();
                self.events
                    .push_back(SctpEvent::Message(id, Message::Text(text)));
            }
            PayloadProtocolIdentifier::StringEmpty if self.channels.contains_key(&id) => {
                self.events
                    .push_back(SctpEvent::Message(id, Message::Text(String::new())));
            }
            PayloadProtocolIdentifier::Binary if self.channels.contains_key(&id) => {
                self.events
                    .push_back(SctpEvent::Message(id, Message::Binary(data)));
            }
            PayloadProtocolIdentifier::BinaryEmpty if self.channels.contains_key(&id) => {
                self.events
                    .push_back(SctpEvent::Message(id, Message::Binary(Bytes::new())));
            }
            _ => {}
        }
    }

    fn set_channel_state(&mut self, id: u16, state: ChannelState) {
        if let Some(channel) = self.channels.get_mut(&id)
            && channel.state != state
        {
            channel.state = state;
            self.events.push_back(SctpEvent::ChannelState(id, state));
        }
    }

    /// Open a channel's stream in the association, if it is not open yet, with its reliability.
    fn ensure_stream(&mut self, id: u16) -> Result<(), SendError> {
        let Some(assoc) = self.assoc.as_mut() else {
            return Err(SendError::Transport("no association".into()));
        };
        if assoc.stream(id).is_err() {
            assoc
                .open_stream(id, PayloadProtocolIdentifier::Binary)
                .map_err(|err| SendError::Transport(err.to_string()))?;
            if let Some(channel) = self.channels.get(&id) {
                let params = channel.params.clone();
                self.set_reliability(id, &params);
            }
        }
        Ok(())
    }

    fn set_reliability(&mut self, id: u16, params: &DataChannelParameters) {
        let (kind, value) = match (params.max_retransmits, params.max_packet_life_time) {
            (Some(retransmits), _) => (ReliabilityType::Rexmit, retransmits),
            (None, Some(lifetime)) => (ReliabilityType::Timed, lifetime),
            (None, None) => (ReliabilityType::Reliable, 0),
        };
        if let Some(assoc) = self.assoc.as_mut()
            && let Ok(mut stream) = assoc.stream(id)
        {
            let _ = stream.set_reliability_params(!params.ordered, kind, value);
        }
    }

    /// Write to a channel's stream now, or queue the write for the association's establishment.
    fn write(
        &mut self,
        id: u16,
        ppid: PayloadProtocolIdentifier,
        data: Bytes,
        now: Instant,
    ) -> Result<(), SendError> {
        if self.state != TransportState::Connected {
            if let Some(channel) = self.channels.get_mut(&id) {
                channel.queued += data.len();
            }
            self.queue.push_back(Queued {
                stream_id: id,
                ppid,
                data,
                counted: true,
            });
            return Ok(());
        }
        self.ensure_stream(id)?;
        let Some(assoc) = self.assoc.as_mut() else {
            return Err(SendError::Transport("no association".into()));
        };
        let mut stream = assoc
            .stream(id)
            .map_err(|err| SendError::Transport(err.to_string()))?;
        stream
            .write_chunk_with_ppi(now, &data, ppid)
            .map_err(|err| SendError::Transport(err.to_string()))?;
        self.gather(now);
        Ok(())
    }

    /// Hand what waited for the association to its streams.
    fn flush(&mut self, now: Instant) {
        while let Some(queued) = self.queue.pop_front() {
            if queued.counted
                && let Some(channel) = self.channels.get_mut(&queued.stream_id)
            {
                channel.queued = channel.queued.saturating_sub(queued.data.len());
            }
            let id = queued.stream_id;
            if let Err(err) = self.write(id, queued.ppid, queued.data, now) {
                debug!(target: "webrtc", "SCTP: a queued write on stream {id} failed: {err}");
            }
        }
    }

    /// Move the association's outgoing packets into the transmit queue.
    fn gather(&mut self, now: Instant) {
        if let Some(assoc) = self.assoc.as_mut() {
            while let Some(message) = assoc.poll_transmit(now) {
                if let Payload::RawEncode(packets) = message.message {
                    self.transmit.extend(packets);
                }
            }
        }
        self.check_buffered_low();
    }

    fn close_all(&mut self) {
        if self.state == TransportState::Closed {
            return;
        }
        self.state = TransportState::Closed;
        self.queue.clear();
        let ids: Vec<u16> = self.channels.keys().copied().collect();
        for id in ids {
            self.channels.remove(&id);
            self.events
                .push_back(SctpEvent::ChannelState(id, ChannelState::Closed));
        }
        self.events.push_back(SctpEvent::Closed);
    }
}
