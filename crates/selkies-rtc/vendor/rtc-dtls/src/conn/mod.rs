//! The DTLS association.
//!
//! [`DTLSConn`](crate::conn::DTLSConn) joins the handshake state machine to the record layer for one peer: inbound
//! datagrams go in through [`read`](crate::conn::DTLSConn::read), application data comes out through
//! [`incoming_application_data`](crate::conn::DTLSConn::incoming_application_data), and whatever should go on
//! the wire is collected from [`outgoing_raw_packet`](crate::conn::DTLSConn::outgoing_raw_packet).
//!
//! Normally an application drives [`Endpoint`](crate::endpoint::Endpoint) instead, which owns one
//! of these per remote address.
#[cfg(test)]
mod conn_test;

use crate::alert::*;
use crate::application_data::*;
use crate::content::*;
use crate::curve::named_curve::NamedCurve;
use crate::extension::extension_use_srtp::*;
use crate::flight::flight0::*;
use crate::flight::flight1::*;
use crate::flight::flight5::*;
use crate::flight::flight6::*;
use crate::flight::*;
use crate::fragment_buffer::*;
use crate::handshake::handshake_cache::*;
use crate::handshake::handshake_header::HandshakeHeader;
use crate::handshake::*;
use crate::handshaker::*;
use crate::record_layer::record_layer_header::*;
use crate::record_layer::*;
use crate::state::*;
use std::collections::VecDeque;

use shared::{error::*, replay_detector::*};

use crate::config::HandshakeConfig;
use bytes::BytesMut;
use log::*;
use std::io::BufWriter;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) const INITIAL_TICKER_INTERVAL: Duration = Duration::from_secs(1);
pub(crate) const COOKIE_LENGTH: usize = 20;
pub(crate) const DEFAULT_NAMED_CURVE: NamedCurve = NamedCurve::X25519;
pub(crate) const INBOUND_BUFFER_SIZE: usize = 8192;
// Default replay protection window is specified by RFC 6347 Section 4.1.2.6
pub(crate) const DEFAULT_REPLAY_PROTECTION_WINDOW: usize = 64;
/// How many times a handshake flight may be retransmitted before the handshake fails.
///
/// Zero means "no retries at all": the first firing of the retransmit timer ends the handshake,
/// so any reply slower than one `retransmit_interval` is fatal. That is almost never what a
/// caller wants, and it is what `ConfigBuilder::build` accidentally used between P7 and this
/// constant being introduced.
pub(crate) const DEFAULT_MAXIMUM_RETRANSMIT_NUMBER: usize = 7;

pub(crate) static INVALID_KEYING_LABELS: &[&str] = &[
    "client finished",
    "server finished",
    "master secret",
    "key expansion",
];

// Conn represents a DTLS connection
/// One DTLS association: the handshake state machine plus the record layer around it.
pub struct DTLSConn {
    is_client: bool,
    maximum_transmission_unit: usize,
    pub(crate) maximum_retransmit_number: usize,
    replay_protection_window: usize,
    replay_detector: Vec<Box<dyn ReplayDetector>>,
    incoming_decrypted_packets: VecDeque<BytesMut>, // Decrypted Application Data or error, pull by calling `Read`
    incoming_encrypted_packets: VecDeque<Vec<u8>>,
    fragment_buffer: FragmentBuffer,
    pub(crate) cache: HandshakeCache, // caching of handshake messages for verifyData generation
    pub(crate) outgoing_packets: VecDeque<Packet>,
    outgoing_queued_packets: VecDeque<Packet>,
    outgoing_compacted_raw_packets: VecDeque<BytesMut>,

    pub(crate) state: State, // Internal state

    handshake_completed: bool,
    connection_closed_by_user: bool,
    // closeLock              sync.Mutex
    closed: bool, //  *closer.Closer
    /// The peer sent close_notify. Its reply has been queued; the connection is finished.
    pub(crate) closed_by_peer: bool,
    //handshakeLoopsFinished sync.WaitGroup

    //readDeadline  :deadline.Deadline,
    //writeDeadline :deadline.Deadline,

    //log logging.LeveledLogger
    /*
    reading               chan struct{}
    handshakeRecv         chan chan struct{}
    cancelHandshaker      func()
    cancelHandshakeReader func()
    */
    pub(crate) current_handshake_state: HandshakeState,
    pub(crate) current_retransmit_timer: Option<Instant>,
    pub(crate) current_retransmit_count: usize,

    pub(crate) current_flight: Box<dyn Flight>,
    pub(crate) flights: Option<Vec<Packet>>,
    pub(crate) handshake_config: Arc<HandshakeConfig>,
    pub(crate) retransmit: bool,
    pub(crate) handshake_rx: Option<()>,
    /// Whether the datagram most recently passed to [`Self::read`] carried a handshake record.
    /// Reset on every `read`, unlike the sticky `handshake_rx`, so a completed association can tell
    /// an RFC 6347 §4.2.4 last-flight repeat from ordinary post-handshake application data.
    pub(crate) received_handshake_in_last_datagram: bool,
}

impl DTLSConn {
    /// Creates a connection in the given role, ready to handshake.
    ///
    /// # Errors
    ///
    /// Fails if the configuration is invalid — no certificate for a server, or an unusable cipher
    /// suite list.
    pub fn new(
        handshake_config: Arc<HandshakeConfig>,
        is_client: bool,
        initial_state: Option<State>,
    ) -> Self {
        let (state, flight, initial_fsm_state) = if let Some(state) = initial_state {
            let flight = if is_client {
                Box::new(Flight5 {}) as Box<dyn Flight>
            } else {
                Box::new(Flight6 {}) as Box<dyn Flight>
            };

            (state, flight, HandshakeState::Finished)
        } else {
            let flight = if is_client {
                Box::new(Flight1 {}) as Box<dyn Flight>
            } else {
                Box::new(Flight0 {}) as Box<dyn Flight>
            };

            (
                State::new(handshake_config.crypto_provider.clone(), is_client),
                flight,
                HandshakeState::Preparing,
            )
        };

        Self {
            is_client,
            maximum_transmission_unit: handshake_config.maximum_transmission_unit,
            maximum_retransmit_number: handshake_config.maximum_retransmit_number,
            replay_protection_window: handshake_config.replay_protection_window,
            replay_detector: vec![],
            incoming_decrypted_packets: VecDeque::new(),
            incoming_encrypted_packets: VecDeque::new(),
            fragment_buffer: FragmentBuffer::new(),
            outgoing_packets: VecDeque::new(),
            outgoing_queued_packets: VecDeque::new(),
            outgoing_compacted_raw_packets: VecDeque::new(),

            cache: HandshakeCache::new(),
            state,
            handshake_completed: false,
            connection_closed_by_user: false,
            closed: false,
            closed_by_peer: false,

            current_handshake_state: initial_fsm_state,
            current_retransmit_timer: None,
            current_retransmit_count: 0,

            current_flight: flight,
            flights: None,
            handshake_config,
            retransmit: false,
            handshake_rx: None,
            received_handshake_in_last_datagram: false,
        }
    }

    // Read reads data from the connection.
    /// Takes the next decrypted application payload, if one is ready.
    pub fn incoming_application_data(&mut self) -> Option<BytesMut> {
        if !self.is_handshake_completed() {
            None
        } else {
            self.incoming_decrypted_packets.pop_front()
        }
    }

    /// Takes the next datagram the caller should send.
    pub fn outgoing_raw_packet(&mut self) -> Option<BytesMut> {
        if let Err(err) = self.handle_outgoing_packets() {
            warn!(
                "handle_outgoing_packets [{}] with error {}",
                srv_cli_str(self.is_client),
                err
            );
        }
        self.outgoing_compacted_raw_packets.pop_front()
    }

    // Write writes p to the DTLS connection
    /// Queues `p` as application data, encrypting it once the handshake has completed.
    ///
    /// # Errors
    ///
    /// Fails if the connection is closed or the handshake has not finished.
    pub fn write(&mut self, p: &[u8]) -> Result<()> {
        if self.is_connection_closed() {
            return Err(Error::ErrConnClosed);
        }

        let pkt = Packet {
            record: RecordLayer::new(
                PROTOCOL_VERSION1_2,
                self.get_local_epoch(),
                Content::ApplicationData(ApplicationData {
                    data: BytesMut::from(p),
                }),
            ),
            should_encrypt: true,
            reset_local_sequence_number: false,
        };

        if self.is_handshake_completed() {
            self.write_packets(vec![pkt]);
        } else {
            self.outgoing_queued_packets.push_back(pkt);
        }

        Ok(())
    }

    // Close closes the connection.
    /// Begins an orderly shutdown by queueing a `close_notify` alert.
    pub fn close(&mut self) {
        if !self.closed {
            self.closed = true;

            // Discard error from notify() to return non-error on the first user call of Close()
            // even if the underlying connection is already closed.
            self.notify(AlertLevel::Warning, AlertDescription::CloseNotify);
        }
    }

    /// connection_state returns basic DTLS details about the connection.
    /// Note that this replaced the `Export` function of v1.
    pub fn connection_state(&self) -> &State {
        &self.state
    }

    // selected_srtp_protection_profile returns the selected SRTPProtectionProfile
    pub(crate) fn selected_srtp_protection_profile(&self) -> SrtpProtectionProfile {
        self.state.srtp_protection_profile
    }

    pub(crate) fn notify(&mut self, level: AlertLevel, desc: AlertDescription) {
        self.write_packets(vec![Packet {
            record: RecordLayer::new(
                PROTOCOL_VERSION1_2,
                self.get_local_epoch(),
                Content::Alert(Alert {
                    alert_level: level,
                    alert_description: desc,
                }),
            ),
            should_encrypt: self.is_handshake_completed(),
            reset_local_sequence_number: false,
        }]);
    }

    pub(crate) fn write_packets(&mut self, pkts: Vec<Packet>) {
        for pkt in pkts {
            self.outgoing_packets.push_back(pkt);
        }
    }

    fn handle_outgoing_packets(&mut self) -> Result<()> {
        if self.is_handshake_completed() {
            while let Some(mut pkt) = self.outgoing_queued_packets.pop_front() {
                pkt.record.record_layer_header.epoch = self.get_local_epoch();
                self.write_packets(vec![pkt]);
            }
        }

        let mut raw_packets = vec![];
        while let Some(p) = self.outgoing_packets.pop_front() {
            if let Content::Handshake(h) = &p.record.content {
                let mut handshake_raw = vec![];
                {
                    let mut writer = BufWriter::<&mut Vec<u8>>::new(handshake_raw.as_mut());
                    p.record.marshal(&mut writer)?;
                }
                debug!(
                    "Send [handshake:{}] -> {} (epoch: {}, seq: {})",
                    srv_cli_str(self.is_client),
                    h.handshake_header.handshake_type,
                    p.record.record_layer_header.epoch,
                    h.handshake_header.message_sequence
                );
                self.cache.push(
                    handshake_raw[RECORD_LAYER_HEADER_SIZE..].to_vec(),
                    p.record.record_layer_header.epoch,
                    h.handshake_header.message_sequence,
                    h.handshake_header.handshake_type,
                    self.is_client,
                );

                let raw_handshake_packets = self.process_handshake_packet(&p, h)?;
                raw_packets.extend_from_slice(&raw_handshake_packets);
            } else {
                /*if let Content::Alert(a) = &p.record.content {
                    if a.alert_description == AlertDescription::CloseNotify {
                        closed = true;
                    }
                }*/

                let raw_packet = self.process_packet(p)?;
                raw_packets.push(raw_packet);
            }
        }

        if !raw_packets.is_empty() {
            let compacted_raw_packets =
                compact_raw_packets(&raw_packets, self.maximum_transmission_unit);

            for compacted_raw_packets in compacted_raw_packets {
                self.outgoing_compacted_raw_packets
                    .push_back(compacted_raw_packets);
            }
        }

        Ok(())
    }

    fn process_packet(&mut self, mut p: Packet) -> Result<Vec<u8>> {
        let epoch = p.record.record_layer_header.epoch as usize;
        let seq = {
            while self.state.local_sequence_number.len() <= epoch {
                self.state.local_sequence_number.push(0);
            }

            self.state.local_sequence_number[epoch] += 1;
            self.state.local_sequence_number[epoch] - 1
        };
        //debug!("{}: seq = {}", srv_cli_str(is_client), seq);

        if seq > MAX_SEQUENCE_NUMBER {
            // RFC 6347 Section 4.1.0
            // The implementation must either abandon an association or rehandshake
            // prior to allowing the sequence number to wrap.
            return Err(Error::ErrSequenceNumberOverflow);
        }
        p.record.record_layer_header.sequence_number = seq;

        // Marshal straight into the Vec: `BufWriter` would heap-allocate an
        // 8 KiB staging buffer for every outbound record.
        let mut raw_packet = vec![];
        p.record.marshal(&mut raw_packet)?;

        if p.should_encrypt
            && let Some(cipher_suite) = &mut self.state.cipher_suite
        {
            raw_packet = cipher_suite.encrypt(&p.record.record_layer_header, &raw_packet)?;
        }

        Ok(raw_packet)
    }

    fn process_handshake_packet(&mut self, p: &Packet, h: &Handshake) -> Result<Vec<Vec<u8>>> {
        let mut raw_packets = vec![];

        let handshake_fragments = DTLSConn::fragment_handshake(self.maximum_transmission_unit, h)?;

        let epoch = p.record.record_layer_header.epoch as usize;

        while self.state.local_sequence_number.len() <= epoch {
            self.state.local_sequence_number.push(0);
        }

        for handshake_fragment in &handshake_fragments {
            let seq = {
                self.state.local_sequence_number[epoch] += 1;
                self.state.local_sequence_number[epoch] - 1
            };
            //debug!("seq = {}", seq);
            if seq > MAX_SEQUENCE_NUMBER {
                return Err(Error::ErrSequenceNumberOverflow);
            }

            let record_layer_header = RecordLayerHeader {
                protocol_version: p.record.record_layer_header.protocol_version,
                content_type: p.record.record_layer_header.content_type,
                content_len: handshake_fragment.len() as u16,
                epoch: p.record.record_layer_header.epoch,
                sequence_number: seq,
            };

            let mut record_layer_header_bytes = vec![];
            {
                let mut writer = BufWriter::<&mut Vec<u8>>::new(record_layer_header_bytes.as_mut());
                record_layer_header.marshal(&mut writer)?;
            }

            //p.record.record_layer_header = record_layer_header;

            let mut raw_packet = vec![];
            raw_packet.extend_from_slice(&record_layer_header_bytes);
            raw_packet.extend_from_slice(handshake_fragment);
            if p.should_encrypt
                && let Some(cipher_suite) = &mut self.state.cipher_suite
            {
                raw_packet = cipher_suite.encrypt(&record_layer_header, &raw_packet)?;
            }

            raw_packets.push(raw_packet);
        }

        Ok(raw_packets)
    }

    fn fragment_handshake(maximum_transmission_unit: usize, h: &Handshake) -> Result<Vec<Vec<u8>>> {
        let mut content = vec![];
        {
            let mut writer = BufWriter::<&mut Vec<u8>>::new(content.as_mut());
            h.handshake_message.marshal(&mut writer)?;
        }

        let mut fragmented_handshakes = vec![];

        let mut content_fragments = split_bytes(&content, maximum_transmission_unit);
        if content_fragments.is_empty() {
            content_fragments = vec![vec![]];
        }

        let mut offset = 0;
        for content_fragment in &content_fragments {
            let content_fragment_len = content_fragment.len();

            let handshake_header_fragment = HandshakeHeader {
                handshake_type: h.handshake_header.handshake_type,
                length: h.handshake_header.length,
                message_sequence: h.handshake_header.message_sequence,
                fragment_offset: offset as u32,
                fragment_length: content_fragment_len as u32,
            };

            offset += content_fragment_len;

            let mut handshake_header_fragment_raw = vec![];
            {
                let mut writer =
                    BufWriter::<&mut Vec<u8>>::new(handshake_header_fragment_raw.as_mut());
                handshake_header_fragment.marshal(&mut writer)?;
            }

            let mut fragmented_handshake = vec![];
            fragmented_handshake.extend_from_slice(&handshake_header_fragment_raw);
            fragmented_handshake.extend_from_slice(content_fragment);

            fragmented_handshakes.push(fragmented_handshake);
        }

        Ok(fragmented_handshakes)
    }

    pub(crate) fn set_handshake_completed(&mut self) {
        self.handshake_completed = true;
    }

    pub(crate) fn is_handshake_completed(&self) -> bool {
        self.handshake_completed
    }

    /// Feeds one received datagram into the connection.
    ///
    /// Records whether this datagram carried a handshake record in
    /// `received_handshake_in_last_datagram`, which a completed association uses to tell an
    /// RFC 6347 §4.2.4 last-flight repeat from ordinary post-handshake application data.
    ///
    /// # Errors
    ///
    /// Fails if the record is malformed or fails authentication.
    pub fn read(&mut self, buf: &[u8]) -> Result<()> {
        // Per RFC 6347: buffer future-epoch packets only until Finished is received
        // (i.e. until handshake completes). After that, discard them.
        let enqueue = !self.is_handshake_completed();
        self.received_handshake_in_last_datagram = false;
        for pkt in unpack_datagram(buf)? {
            let (hs, alert, err) = self.handle_incoming_packet(pkt, enqueue);
            if let Some(alert) = alert {
                self.outgoing_packets.push_back(Packet {
                    record: RecordLayer::new(
                        PROTOCOL_VERSION1_2,
                        self.state.local_epoch,
                        Content::Alert(Alert {
                            alert_level: alert.alert_level,
                            alert_description: alert.alert_description,
                        }),
                    ),
                    should_encrypt: self.is_handshake_completed(),
                    reset_local_sequence_number: false,
                });

                if alert.alert_description == AlertDescription::CloseNotify {
                    // A close_notify is only queued here as the reply to the peer's own.
                    self.closed_by_peer = true;
                }
                if alert.alert_level == AlertLevel::Fatal
                    || alert.alert_description == AlertDescription::CloseNotify
                {
                    return Err(Error::ErrAlertFatalOrClose);
                }
            }

            if let Some(err) = err {
                return Err(err);
            }

            if hs {
                self.received_handshake_in_last_datagram = true;
                self.handshake_rx = Some(());
            }
        }

        Ok(())
    }

    pub(crate) fn handle_incoming_queued_packets(&mut self) -> Result<bool> {
        // Drain queued future-epoch packets once the cipher suite is initialized,
        // which may happen before handshake_completed (e.g. Finished arrived before
        // ChangeCipherSpec bumped remote_epoch, so Finished was queued).
        let cipher_ready = self
            .state
            .cipher_suite
            .as_ref()
            .is_some_and(|cs| cs.is_initialized());
        let mut is_handshake = false;
        if cipher_ready {
            while let Some(p) = self.incoming_encrypted_packets.pop_front() {
                let (hs, alert, err) = self.handle_incoming_packet(p, false); // don't re-enqueue
                if hs {
                    is_handshake = true;
                    self.handshake_rx = Some(());
                }
                if let Some(alert) = alert {
                    self.outgoing_packets.push_back(Packet {
                        record: RecordLayer::new(
                            PROTOCOL_VERSION1_2,
                            self.state.local_epoch,
                            Content::Alert(Alert {
                                alert_level: alert.alert_level,
                                alert_description: alert.alert_description,
                            }),
                        ),
                        should_encrypt: self.is_handshake_completed(),
                        reset_local_sequence_number: false,
                    });

                    if alert.alert_description == AlertDescription::CloseNotify {
                        // A close_notify is only queued here as the reply to the peer's own.
                        self.closed_by_peer = true;
                    }
                    if alert.alert_level == AlertLevel::Fatal
                        || alert.alert_description == AlertDescription::CloseNotify
                    {
                        return Err(Error::ErrAlertFatalOrClose);
                    }
                }

                if let Some(err) = err {
                    return Err(err);
                }
            }
        }

        Ok(is_handshake)
    }

    fn handle_incoming_packet(
        &mut self,
        mut pkt: Vec<u8>,
        enqueue: bool,
    ) -> (bool, Option<Alert>, Option<Error>) {
        // Parse the 13-byte header from the slice directly: `BufReader` would
        // heap-allocate an 8 KiB buffer for every inbound record.
        let mut reader = pkt.as_slice();
        let h = match RecordLayerHeader::unmarshal(&mut reader) {
            Ok(h) => h,
            Err(err) => {
                // Decode error must be silently discarded
                // [RFC6347 Section-4.1.2.7]
                debug!(
                    "{}: discarded broken packet: {}",
                    srv_cli_str(self.is_client),
                    err
                );
                return (false, None, None);
            }
        };

        // Validate epoch
        let epoch = self.state.remote_epoch;
        if h.epoch > epoch {
            if h.epoch > epoch + 1 {
                debug!(
                    "{}: discarded future packet (epoch: {}, seq: {})",
                    srv_cli_str(self.is_client),
                    h.epoch,
                    h.sequence_number,
                );
                return (false, None, None);
            }
            if enqueue {
                debug!(
                    "{}: received packet of next epoch, queuing packet",
                    srv_cli_str(self.is_client)
                );
                self.incoming_encrypted_packets.push_back(pkt);
            }
            return (false, None, None);
        }

        // Once the handshake has completed, a genuine peer only sends old-epoch handshake
        // records, to repeat its final flight (RFC 6347 4.2.4 retransmission relies on them).
        // Any other old-epoch record is plaintext that can only be forged or stale: acting on it
        // (an alert, or our fatal alert answering bad ApplicationData, ChangeCipherSpec or an
        // undecodable record) would let anyone spoofing the peer's address tear down the
        // association without keys. So it is silently discarded [RFC6347 Section-4.1.2.7].
        // Before the handshake completes, old epochs must be accepted [RFC6347 Section-4.1].
        if h.epoch < epoch
            && self.is_handshake_completed()
            && h.content_type != ContentType::Handshake
        {
            debug!(
                "{}: discarded {:?} record from old epoch {} (current: {}, seq: {})",
                srv_cli_str(self.is_client),
                h.content_type,
                h.epoch,
                epoch,
                h.sequence_number,
            );
            return (false, None, None);
        }

        // Anti-replay protection
        while self.replay_detector.len() <= h.epoch as usize {
            self.replay_detector
                .push(Box::new(SlidingWindowDetector::new(
                    self.replay_protection_window,
                    MAX_SEQUENCE_NUMBER,
                )));
        }

        let ok = self.replay_detector[h.epoch as usize].check(h.sequence_number);
        if !ok {
            debug!(
                "{}: discarded duplicated packet (epoch: {}, seq: {})",
                srv_cli_str(self.is_client),
                h.epoch,
                h.sequence_number,
            );
            return (false, None, None);
        }

        // Decrypt
        if h.epoch != 0 {
            let invalid_cipher_suite = {
                if let Some(cipher_suite) = &self.state.cipher_suite {
                    !cipher_suite.is_initialized()
                } else {
                    true
                }
            };
            if invalid_cipher_suite {
                if enqueue {
                    debug!(
                        "{}: handshake not finished, queuing packet",
                        srv_cli_str(self.is_client)
                    );
                    self.incoming_encrypted_packets.push_back(pkt);
                }
                return (false, None, None);
            }

            if let Some(cipher_suite) = &mut self.state.cipher_suite {
                pkt = match cipher_suite.decrypt(&pkt) {
                    Ok(pkt) => pkt,
                    Err(err) => {
                        debug!("{}: decrypt failed: {}", srv_cli_str(self.is_client), err);

                        // If we get an error for PSK we need to return an error.
                        if cipher_suite.is_psk() {
                            return (
                                false,
                                Some(Alert {
                                    alert_level: AlertLevel::Fatal,
                                    alert_description: AlertDescription::UnknownPskIdentity,
                                }),
                                None,
                            );
                        } else {
                            return (false, None, None);
                        }
                    }
                };
            }
        }

        let is_handshake = match self.fragment_buffer.push(&pkt) {
            Ok(is_handshake) => is_handshake,
            Err(err) => {
                // Decode error must be silently discarded
                // [RFC6347 Section-4.1.2.7]
                debug!(
                    "{}: defragment failed: {}",
                    srv_cli_str(self.is_client),
                    err
                );
                return (false, None, None);
            }
        };
        if is_handshake {
            self.replay_detector[h.epoch as usize].accept();
            while let Ok((out, epoch)) = self.fragment_buffer.pop() {
                //log::debug!("Extension Debug: out.len()={}", out.len());
                let mut reader = out.as_slice();
                let raw_handshake = match Handshake::unmarshal(&mut reader) {
                    Ok(rh) => {
                        debug!(
                            "Recv [handshake:{}] -> {} (epoch: {}, seq: {})",
                            srv_cli_str(self.is_client),
                            rh.handshake_header.handshake_type,
                            h.epoch,
                            rh.handshake_header.message_sequence
                        );
                        rh
                    }
                    Err(err) => {
                        debug!(
                            "{}: handshake parse failed: {}",
                            srv_cli_str(self.is_client),
                            err
                        );
                        continue;
                    }
                };

                self.cache.push(
                    out,
                    epoch,
                    raw_handshake.handshake_header.message_sequence,
                    raw_handshake.handshake_header.handshake_type,
                    !self.is_client,
                );
            }

            return (true, None, None);
        }

        let mut reader = pkt.as_slice();
        let r = match RecordLayer::unmarshal(&mut reader) {
            Ok(r) => r,
            Err(err) => {
                return (
                    false,
                    Some(Alert {
                        alert_level: AlertLevel::Fatal,
                        alert_description: AlertDescription::DecodeError,
                    }),
                    Some(err),
                );
            }
        };

        match r.content {
            Content::Alert(mut a) => {
                debug!("{}: <- {}", srv_cli_str(self.is_client), a);
                if a.alert_description == AlertDescription::CloseNotify {
                    // Respond with a close_notify [RFC5246 Section 7.2.1]
                    a = Alert {
                        alert_level: AlertLevel::Warning,
                        alert_description: AlertDescription::CloseNotify,
                    };
                }
                self.replay_detector[h.epoch as usize].accept();
                return (
                    false,
                    Some(a),
                    Some(Error::Other(format!("Error of Alert {a}"))),
                );
            }
            Content::ChangeCipherSpec(_) => {
                let invalid_cipher_suite = {
                    if let Some(cipher_suite) = &self.state.cipher_suite {
                        !cipher_suite.is_initialized()
                    } else {
                        true
                    }
                };

                if invalid_cipher_suite {
                    if enqueue {
                        debug!(
                            "{}: CipherSuite not initialized, queuing packet",
                            srv_cli_str(self.is_client)
                        );
                        self.incoming_encrypted_packets.push_back(pkt);
                    }
                    return (false, None, None);
                }

                let new_remote_epoch = h.epoch + 1;
                debug!(
                    "{}: <- ChangeCipherSpec (epoch: {})",
                    srv_cli_str(self.is_client),
                    new_remote_epoch
                );

                if epoch + 1 == new_remote_epoch {
                    self.state.remote_epoch = new_remote_epoch;
                    self.replay_detector[h.epoch as usize].accept();
                }
            }
            Content::ApplicationData(a) => {
                if h.epoch == 0 {
                    warn!(
                        "{}: <- Unexpected ApplicationData Message",
                        srv_cli_str(self.is_client),
                    );
                    return (
                        false,
                        Some(Alert {
                            alert_level: AlertLevel::Fatal,
                            alert_description: AlertDescription::UnexpectedMessage,
                        }),
                        Some(Error::ErrApplicationDataEpochZero),
                    );
                }

                self.replay_detector[h.epoch as usize].accept();

                self.incoming_decrypted_packets.push_back(a.data);
            }
            _ => {
                warn!(
                    "{}: <- Unexpected Handshake Message",
                    srv_cli_str(self.is_client),
                );
                return (
                    false,
                    Some(Alert {
                        alert_level: AlertLevel::Fatal,
                        alert_description: AlertDescription::UnexpectedMessage,
                    }),
                    Some(Error::ErrUnhandledContextType),
                );
            }
        };

        (false, None, None)
    }

    fn is_connection_closed(&self) -> bool {
        self.closed
    }

    pub(crate) fn set_local_epoch(&mut self, epoch: u16) {
        self.state.local_epoch = epoch;
    }

    pub(crate) fn get_local_epoch(&self) -> u16 {
        self.state.local_epoch
    }
}

fn compact_raw_packets(raw_packets: &[Vec<u8>], maximum_transmission_unit: usize) -> Vec<BytesMut> {
    let mut combined_raw_packets = vec![];
    let mut current_combined_raw_packet = BytesMut::new();

    for raw_packet in raw_packets {
        if !current_combined_raw_packet.is_empty()
            && current_combined_raw_packet.len() + raw_packet.len() >= maximum_transmission_unit
        {
            combined_raw_packets.push(current_combined_raw_packet);
            current_combined_raw_packet = BytesMut::new();
        }
        current_combined_raw_packet.extend_from_slice(raw_packet);
    }

    if !current_combined_raw_packet.is_empty() {
        combined_raw_packets.push(current_combined_raw_packet);
    }

    combined_raw_packets
}

fn split_bytes(bytes: &[u8], split_len: usize) -> Vec<Vec<u8>> {
    let mut splits = vec![];
    let num_bytes = bytes.len();
    for i in (0..num_bytes).step_by(split_len) {
        let mut j = i + split_len;
        if j > num_bytes {
            j = num_bytes;
        }

        splits.push(bytes[i..j].to_vec());
    }

    splits
}
