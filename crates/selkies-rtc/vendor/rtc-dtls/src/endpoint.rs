//! The Sans-I/O DTLS endpoint.
//!
//! An [`Endpoint`](crate::endpoint::Endpoint) multiplexes several DTLS associations by remote address. Feed it inbound
//! datagrams, poll it for the datagrams to send and for [`EndpointEvent`](crate::endpoint::EndpointEvent)s, and drive its timers
//! with `handle_timeout`/`poll_timeout`. It owns no sockets and reads no clock.
//!
//! [`EndpointEvent::HandshakeComplete`](crate::endpoint::EndpointEvent::HandshakeComplete) is the signal an application waits for: from that point
//! application data can be written, and the SRTP keying material can be exported from the
//! completed handshake.
use crate::conn::DTLSConn;
use shared::error::{Error, Result};
use shared::{EcnCodepoint, TransportContext};
use shared::{TransportMessage, TransportProtocol};

use crate::config::HandshakeConfig;
use crate::state::State;
use bytes::BytesMut;
use std::collections::hash_map::Keys;
use std::collections::{HashMap, VecDeque, hash_map::Entry::Vacant};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

/// What the endpoint reports to its caller.
#[non_exhaustive]
pub enum EndpointEvent {
    /// The handshake finished; application data may now be sent, and SRTP keys can be exported.
    HandshakeComplete,
    /// Decrypted application data arrived.
    ApplicationData(BytesMut),
    /// The peer closed the connection with a `close_notify` alert (RFC 5246 §7.2.1). The
    /// matching `close_notify` reply is queued for [`Endpoint::poll_transmit`], and the connection
    /// has been removed.
    PeerClosed,
}

/// The main entry point to the library
///
/// This object performs no I/O whatsoever. Instead, it generates a stream of packets to send via
/// `poll_transmit`, and consumes incoming packets and connections-generated events via `handle` and
/// `handle_event`.
pub struct Endpoint {
    local_addr: SocketAddr,
    transport_protocol: TransportProtocol,
    transmits: VecDeque<TransportMessage<BytesMut>>,
    connections: HashMap<SocketAddr, DTLSConn>,
    server_config: Option<Arc<HandshakeConfig>>,
}

impl Endpoint {
    /// Create a new endpoint
    ///
    /// Returns `Err` if the configuration is invalid.
    pub fn new(
        local_addr: SocketAddr,
        protocol: TransportProtocol,
        server_config: Option<Arc<HandshakeConfig>>,
    ) -> Self {
        Self {
            local_addr,
            transport_protocol: protocol,
            transmits: VecDeque::new(),
            connections: HashMap::new(),
            server_config,
        }
    }

    /// Replace the server configuration, affecting new incoming associations only
    pub fn set_server_config(&mut self, server_config: Option<Arc<HandshakeConfig>>) {
        self.server_config = server_config;
    }

    /// Get the next packet to transmit
    #[must_use]
    pub fn poll_transmit(&mut self) -> Option<TransportMessage<BytesMut>> {
        self.transmits.pop_front()
    }

    /// Get keys of Connections
    pub fn get_connections_keys(&self) -> Keys<'_, SocketAddr, DTLSConn> {
        self.connections.keys()
    }

    /// Get Connection State
    pub fn get_connection_state(&self, remote: SocketAddr) -> Option<&State> {
        if let Some(conn) = self.connections.get(&remote) {
            Some(conn.connection_state())
        } else {
            None
        }
    }

    /// Initiate an Association
    pub fn connect(
        &mut self,
        now: Instant,
        remote: SocketAddr,
        client_config: Arc<HandshakeConfig>,
        initial_state: Option<State>,
    ) -> Result<()> {
        if remote.port() == 0 {
            return Err(Error::InvalidRemoteAddress(remote));
        }

        if let Vacant(e) = self.connections.entry(remote) {
            let mut conn = DTLSConn::new(client_config, true, initial_state);
            conn.handshake(now)?;

            while let Some(payload) = conn.outgoing_raw_packet() {
                self.transmits.push_back(TransportMessage {
                    now,
                    transport: TransportContext {
                        local_addr: self.local_addr,
                        peer_addr: remote,
                        ecn: None,
                        transport_protocol: self.transport_protocol,
                    },
                    message: payload,
                });
            }

            e.insert(conn);
        }

        Ok(())
    }

    /// Process stop remote
    ///
    /// `now` stamps the close_notify this queues. Like [`Self::connect`], the instant is a
    /// parameter rather than something the endpoint samples: it owns no sockets and reads no
    /// clock.
    pub fn stop(&mut self, now: Instant, remote: SocketAddr) -> Option<DTLSConn> {
        if let Some(conn) = self.connections.get_mut(&remote) {
            conn.close();
            while let Some(payload) = conn.outgoing_raw_packet() {
                self.transmits.push_back(TransportMessage {
                    now,
                    transport: TransportContext {
                        local_addr: self.local_addr,
                        peer_addr: remote,
                        ecn: None,
                        transport_protocol: self.transport_protocol,
                    },
                    message: payload,
                });
            }
        }
        self.connections.remove(&remote)
    }

    /// Process close
    ///
    /// `now` stamps the close_notify queued for every live association.
    pub fn close(&mut self, now: Instant) -> Result<()> {
        for (remote_addr, conn) in self.connections.iter_mut() {
            conn.close();
            while let Some(payload) = conn.outgoing_raw_packet() {
                self.transmits.push_back(TransportMessage {
                    now,
                    transport: TransportContext {
                        local_addr: self.local_addr,
                        peer_addr: *remote_addr,
                        ecn: None,
                        transport_protocol: self.transport_protocol,
                    },
                    message: payload,
                });
            }
        }
        self.connections.clear();

        Ok(())
    }

    /// Feeds one datagram to `conn` and advances its handshake, recording a completed handshake
    /// in `messages`. Draining application data and transmits is left to the caller, which does it
    /// whether or not this fails.
    fn read_into(
        conn: &mut DTLSConn,
        now: Instant,
        data: &[u8],
        messages: &mut Vec<EndpointEvent>,
    ) -> Result<()> {
        let is_handshake_completed_before = conn.is_handshake_completed();
        conn.read(data)?;
        if !conn.is_handshake_completed() {
            conn.handshake(now)?;
            // Drain any queued future-epoch packets (e.g. Finished that arrived
            // before ChangeCipherSpec bumped remote_epoch). If draining sets
            // handshake_rx, run handshake() again so the FSM can advance.
            let is_handshake = conn.handle_incoming_queued_packets()?;
            if is_handshake && !conn.is_handshake_completed() {
                conn.handshake(now)?;
            }
        } else if conn.received_handshake_in_last_datagram {
            // RFC 6347 4.2.4: this datagram carried a handshake record although the association
            // is already established, so the peer is repeating its own final flight — ours was
            // lost and has to be sent again. The gate is this datagram's handshake record, not
            // the sticky `handshake_rx` flag, so ordinary post-handshake application data can
            // never provoke a spurious flight (certificate included, for a client). The
            // association is complete, so the FSM above is skipped, and `send()` never arms
            // `current_retransmit_timer` on the completed path, so `handle_timeout` cannot fire
            // for it either. `handshake_timeout`'s Finished branch regenerates the flight — with
            // fresh record sequence numbers, since a verbatim replay would be dropped by the
            // peer's replay window (4.1.2.6) — under a retransmission budget, so an
            // unauthenticated repeat cannot amplify without limit. Clear `handshake_rx` first so
            // that retransmission re-sends the buffered flight instead of re-parsing the whole
            // transcript in `finish()`.
            conn.handshake_rx = None;
            conn.handshake_timeout(now)?;
        }
        if !is_handshake_completed_before && conn.is_handshake_completed() {
            messages.push(EndpointEvent::HandshakeComplete)
        }
        Ok(())
    }

    /// Process an incoming UDP datagram
    pub fn read(
        &mut self,
        now: Instant,
        remote: SocketAddr,
        ecn: Option<EcnCodepoint>,
        data: BytesMut,
    ) -> Result<Vec<EndpointEvent>> {
        if let Vacant(e) = self.connections.entry(remote) {
            if let Some(server_config) = &self.server_config {
                let handshake_config = server_config.clone();
                let conn = DTLSConn::new(handshake_config, false, None);
                e.insert(conn);
            } else {
                return Err(Error::NoServerConfig);
            }
        }

        // Handle packet on existing association, if any
        let mut messages = vec![];
        if let Some(conn) = self.connections.get_mut(&remote) {
            let result = Self::read_into(conn, now, &data, &mut messages);
            // Whatever the outcome, deliver the data that arrived and send what the connection
            // queued: an alert in reply to the peer's (its close_notify, or a fatal alert) has to
            // go out even though this read is ending in an error.
            while let Some(message) = conn.incoming_application_data() {
                messages.push(EndpointEvent::ApplicationData(message));
            }
            while let Some(payload) = conn.outgoing_raw_packet() {
                self.transmits.push_back(TransportMessage {
                    now,
                    transport: TransportContext {
                        local_addr: self.local_addr,
                        peer_addr: remote,
                        ecn,
                        transport_protocol: self.transport_protocol,
                    },
                    message: payload,
                });
            }
            // A peer's close_notify ends the connection gracefully: report it rather than the
            // error the connection returns for it.
            if conn.closed_by_peer {
                self.connections.remove(&remote);
                messages.push(EndpointEvent::PeerClosed);
                return Ok(messages);
            }
            result?;
        }

        Ok(messages)
    }

    /// Queues application data for `remote`.
    ///
    /// # Errors
    ///
    /// Fails if there is no association with `remote`, or its handshake has not completed.
    pub fn write(&mut self, now: Instant, remote: SocketAddr, data: &[u8]) -> Result<()> {
        if let Some(conn) = self.connections.get_mut(&remote) {
            conn.write(data)?;
            while let Some(payload) = conn.outgoing_raw_packet() {
                self.transmits.push_back(TransportMessage {
                    now,
                    transport: TransportContext {
                        local_addr: self.local_addr,
                        peer_addr: remote,
                        ecn: None,
                        transport_protocol: self.transport_protocol,
                    },
                    message: payload,
                });
            }
            Ok(())
        } else {
            Err(Error::InvalidRemoteAddress(remote))
        }
    }

    /// Advances `remote`'s association to `now`, driving handshake retransmissions.
    ///
    /// # Errors
    ///
    /// Fails if the handshake has exhausted its retransmissions.
    pub fn handle_timeout(&mut self, remote: SocketAddr, now: Instant) -> Result<()> {
        if let Some(conn) = self.connections.get_mut(&remote) {
            if let Some(current_retransmit_timer) = &conn.current_retransmit_timer
                && now >= *current_retransmit_timer
            {
                if conn.current_retransmit_timer.take().is_some() && !conn.is_handshake_completed()
                {
                    conn.handshake_timeout(now)?;
                }
                while let Some(payload) = conn.outgoing_raw_packet() {
                    self.transmits.push_back(TransportMessage {
                        now,
                        transport: TransportContext {
                            local_addr: self.local_addr,
                            peer_addr: remote,
                            ecn: None,
                            transport_protocol: self.transport_protocol,
                        },
                        message: payload,
                    });
                }
            }
            Ok(())
        } else {
            Err(Error::InvalidRemoteAddress(remote))
        }
    }

    /// When `remote`'s association next needs [`Self::handle_timeout`].
    pub fn poll_timeout(&self, remote: &SocketAddr) -> Option<Instant> {
        if let Some(conn) = self.connections.get(remote) {
            conn.current_retransmit_timer
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cipher_suite::CipherSuiteId;
    use crate::config::ConfigBuilder;
    use crate::conn::DEFAULT_MAXIMUM_RETRANSMIT_NUMBER;
    use crate::crypto::Certificate;
    use crypto::{CryptoError, RTCCrypto, RTCCryptoProvider, RTCRandom};

    fn client_addr() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 4444))
    }

    fn server_addr() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 4445))
    }

    struct FailingRandom;

    impl RTCRandom for FailingRandom {
        fn fill(&self, _output: &mut [u8]) -> std::result::Result<(), CryptoError> {
            Err(CryptoError::RandomnessFailed)
        }
    }

    struct FailingRandomProvider {
        provider: Arc<dyn RTCCryptoProvider>,
    }

    impl RTCCryptoProvider for FailingRandomProvider {
        fn name(&self) -> &'static str {
            "failing-random"
        }

        fn crypto(&self) -> &dyn RTCCrypto {
            self.provider.crypto()
        }

        fn random(&self) -> &dyn RTCRandom {
            &FailingRandom
        }
    }

    /// Builds a config offering `suites`, with whichever credentials they need — pass both
    /// families to get a server holding a psk callback and certificates both.
    fn config(
        provider: Arc<dyn RTCCryptoProvider>,
        is_client: bool,
        suites: &[CipherSuiteId],
    ) -> Result<Arc<HandshakeConfig>> {
        let mut builder = ConfigBuilder::default()
            .with_crypto_provider(provider.clone())
            .with_cipher_suites(suites.to_vec())
            .with_insecure_skip_verify(true);
        let is_psk = |suite: &CipherSuiteId| {
            matches!(
                suite,
                CipherSuiteId::Tls_Psk_With_Aes_128_Ccm
                    | CipherSuiteId::Tls_Psk_With_Aes_128_Ccm_8
                    | CipherSuiteId::Tls_Psk_With_Aes_128_Gcm_Sha256
            )
        };
        if suites.iter().any(is_psk) {
            builder = builder.with_psk(Some(Arc::new(|_| Ok(vec![0xab, 0xcd, 0xef]))));
            if is_client {
                builder = builder.with_psk_identity_hint(Some(b"rtc-dtls-test".to_vec()));
            }
        }
        if !is_client && suites.iter().any(|suite| !is_psk(suite)) {
            builder = builder.with_certificates(vec![Certificate::generate_self_signed(
                vec!["localhost".to_owned()],
                provider.crypto(),
            )?]);
        }
        Ok(Arc::new(builder.build(is_client, None)?))
    }

    fn transfer(
        source: &mut Endpoint,
        destination: &mut Endpoint,
        source_addr: SocketAddr,
    ) -> Result<Vec<EndpointEvent>> {
        let mut events = Vec::new();
        while let Some(transmit) = source.poll_transmit() {
            events.extend(destination.read(
                Instant::now(),
                source_addr,
                transmit.transport.ecn,
                transmit.message,
            )?);
        }
        Ok(events)
    }

    fn handshake_and_exchange(
        client_provider: Arc<dyn RTCCryptoProvider>,
        server_provider: Arc<dyn RTCCryptoProvider>,
        client_suite: CipherSuiteId,
        server_suites: &[CipherSuiteId],
    ) -> Result<()> {
        let server_config = config(server_provider, false, server_suites)?;
        let mut server = Endpoint::new(server_addr(), TransportProtocol::UDP, Some(server_config));
        exchange_with_server(&mut server, client_provider, client_addr(), client_suite)
    }

    /// Drives one client through a handshake and a record exchange against an existing server,
    /// so several clients can be served by the one endpoint and configuration.
    fn exchange_with_server(
        server: &mut Endpoint,
        client_provider: Arc<dyn RTCCryptoProvider>,
        client_addr: SocketAddr,
        client_suite: CipherSuiteId,
    ) -> Result<()> {
        let client_config = config(client_provider, true, &[client_suite])?;
        let mut client = Endpoint::new(client_addr, TransportProtocol::UDP, None);
        client.connect(Instant::now(), server_addr(), client_config, None)?;

        let mut client_complete = false;
        let mut server_complete = false;
        for _ in 0..32 {
            for event in transfer(&mut client, server, client_addr)? {
                server_complete |= matches!(event, EndpointEvent::HandshakeComplete);
            }
            for event in transfer(server, &mut client, server_addr())? {
                client_complete |= matches!(event, EndpointEvent::HandshakeComplete);
            }
            if client_complete && server_complete {
                break;
            }
        }
        assert!(
            client_complete && server_complete,
            "DTLS handshake did not complete for a {client_suite:?} client"
        );

        client.write(Instant::now(), server_addr(), b"provider-backed DTLS")?;
        let transmit = client
            .poll_transmit()
            .expect("application write produces a DTLS record");
        let replay = transmit.message.clone();
        let events = server.read(
            Instant::now(),
            client_addr,
            transmit.transport.ecn,
            transmit.message,
        )?;
        assert!(events.into_iter().any(|event| matches!(
            event,
            EndpointEvent::ApplicationData(data) if data.as_ref() == b"provider-backed DTLS"
        )));
        assert!(
            server
                .read(Instant::now(), client_addr, None, replay)?
                .is_empty()
        );
        Ok(())
    }

    /// A plaintext (epoch 0) record, as anyone who can spoof the peer's address can send: it
    /// needs no keys.
    fn plaintext_record_datagram(content_type: u8, body: &[u8], sequence_number: u64) -> BytesMut {
        use crate::content::ContentType;
        use crate::record_layer::record_layer_header::{PROTOCOL_VERSION1_2, RecordLayerHeader};

        let header = RecordLayerHeader {
            content_type: ContentType::Alert,
            protocol_version: PROTOCOL_VERSION1_2,
            epoch: 0,
            sequence_number,
            content_len: body.len() as u16,
        };
        let mut raw = vec![];
        header.marshal(&mut raw).expect("a record header marshals");
        // `ContentType` folds unknown values into `Invalid`, so the type byte is written raw.
        raw[0] = content_type;
        raw.extend_from_slice(body);
        BytesMut::from(&raw[..])
    }

    #[cfg(feature = "crypto-ring")]
    #[test]
    fn ring_provider_completes_handshake_and_record_exchange() -> Result<()> {
        let provider: Arc<dyn RTCCryptoProvider> = Arc::new(crypto::providers::RingProvider::new());
        handshake_and_exchange(
            provider.clone(),
            provider,
            CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Gcm_Sha256,
            &[CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Gcm_Sha256],
        )
    }

    #[cfg(feature = "crypto-aws-lc-rs")]
    #[test]
    fn aws_lc_rs_provider_completes_handshake_and_record_exchange() -> Result<()> {
        let provider: Arc<dyn RTCCryptoProvider> =
            Arc::new(crypto::providers::AwsLcRsProvider::new());
        handshake_and_exchange(
            provider.clone(),
            provider,
            CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Gcm_Sha256,
            &[CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Gcm_Sha256],
        )
    }

    #[cfg(all(feature = "crypto-ring", feature = "crypto-aws-lc-rs"))]
    #[test]
    fn ring_and_aws_lc_rs_complete_cross_provider_handshakes() -> Result<()> {
        let ring: Arc<dyn RTCCryptoProvider> = Arc::new(crypto::providers::RingProvider::new());
        let aws: Arc<dyn RTCCryptoProvider> = Arc::new(crypto::providers::AwsLcRsProvider::new());
        for suite in [
            CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Gcm_Sha256,
            CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Ccm,
            CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Ccm_8,
            CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_256_Cbc_Sha,
            CipherSuiteId::Tls_Ecdhe_Ecdsa_With_ChaCha20_Poly1305_Sha256,
            CipherSuiteId::Tls_Psk_With_Aes_128_Gcm_Sha256,
            CipherSuiteId::Tls_Psk_With_Aes_128_Ccm,
            CipherSuiteId::Tls_Psk_With_Aes_128_Ccm_8,
        ] {
            handshake_and_exchange(ring.clone(), aws.clone(), suite, &[suite])?;
            handshake_and_exchange(aws.clone(), ring.clone(), suite, &[suite])?;
        }
        Ok(())
    }

    #[test]
    fn failing_random_provider_aborts_client_hello_cleanly() -> Result<()> {
        let base = crypto::default_provider().map_err(|error| Error::Crypto(error.to_string()))?;
        let provider: Arc<dyn RTCCryptoProvider> =
            Arc::new(FailingRandomProvider { provider: base });
        let config = config(
            provider,
            true,
            &[CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Gcm_Sha256],
        )?;
        let mut endpoint = Endpoint::new(client_addr(), TransportProtocol::UDP, None);

        let result = endpoint.connect(Instant::now(), server_addr(), config, None);
        assert!(matches!(result, Err(Error::Crypto(_))));
        Ok(())
    }

    #[cfg(feature = "crypto-ring")]
    #[test]
    fn one_server_config_serves_psk_and_certificate_clients() -> Result<()> {
        let provider: Arc<dyn RTCCryptoProvider> = Arc::new(crypto::providers::RingProvider::new());
        let certificate_and_psk_suites = [
            CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Gcm_Sha256,
            CipherSuiteId::Tls_Psk_With_Aes_128_Ccm_8,
        ];

        let server_config = config(provider.clone(), false, &certificate_and_psk_suites)?;
        let mut server = Endpoint::new(server_addr(), TransportProtocol::UDP, Some(server_config));

        for (index, client_suite) in certificate_and_psk_suites.into_iter().enumerate() {
            let client_addr = SocketAddr::from(([127, 0, 0, 1], 4446 + index as u16));
            exchange_with_server(&mut server, provider.clone(), client_addr, client_suite)?;
        }

        Ok(())
    }

    /// Runs a full handshake to completion and returns both endpoints, drained of any pending
    /// transmit so a later assertion sees only newly generated output.
    #[cfg(feature = "crypto-ring")]
    fn completed_client_and_server() -> Result<(Endpoint, Endpoint)> {
        let provider: Arc<dyn RTCCryptoProvider> = Arc::new(crypto::providers::RingProvider::new());
        let suites = [CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Gcm_Sha256];
        let client_config = config(provider.clone(), true, &suites)?;
        let server_config = config(provider, false, &suites)?;
        let mut client = Endpoint::new(client_addr(), TransportProtocol::UDP, None);
        let mut server = Endpoint::new(server_addr(), TransportProtocol::UDP, Some(server_config));
        client.connect(Instant::now(), server_addr(), client_config, None)?;

        let mut client_complete = false;
        let mut server_complete = false;
        for _ in 0..32 {
            for event in transfer(&mut client, &mut server, client_addr())? {
                server_complete |= matches!(event, EndpointEvent::HandshakeComplete);
            }
            for event in transfer(&mut server, &mut client, server_addr())? {
                client_complete |= matches!(event, EndpointEvent::HandshakeComplete);
            }
            if client_complete && server_complete {
                break;
            }
        }
        assert!(
            client_complete && server_complete,
            "the handshake completed for both sides"
        );
        while client.poll_transmit().is_some() {}
        while server.poll_transmit().is_some() {}
        Ok((client, server))
    }

    /// A 29-byte epoch-0 record whose content type marks it a handshake and whose body is a
    /// well-formed fragment header over a junk payload: [`crate::fragment_buffer::FragmentBuffer`]
    /// classifies the datagram as a handshake record — arming the RFC 6347 4.2.4 retransmission
    /// trigger — although nothing in it is authenticated (epoch 0 carries no MAC).
    fn junk_epoch_zero_handshake_datagram(sequence_number: u64) -> BytesMut {
        use crate::content::ContentType;
        use crate::record_layer::record_layer_header::{PROTOCOL_VERSION1_2, RecordLayerHeader};

        let header = RecordLayerHeader {
            content_type: ContentType::Handshake,
            protocol_version: PROTOCOL_VERSION1_2,
            epoch: 0,
            sequence_number,
            content_len: 16,
        };
        let mut raw = vec![];
        header.marshal(&mut raw).expect("a record header marshals");
        raw.extend_from_slice(&[
            0xff, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00,
            0x00, 0x00,
        ]);
        BytesMut::from(&raw[..])
    }

    /// The `(epoch, record sequence number)` of every record in `datagram`, read from the plaintext
    /// record headers (the epoch-1 Finished body is encrypted, but its header is not).
    fn record_sequence_numbers(datagram: &[u8]) -> Vec<(u16, u64)> {
        use crate::record_layer::record_layer_header::RecordLayerHeader;
        use crate::record_layer::unpack_datagram;

        unpack_datagram(datagram)
            .expect("a datagram splits into records")
            .into_iter()
            .map(|record| {
                let mut reader = record.as_slice();
                let header =
                    RecordLayerHeader::unmarshal(&mut reader).expect("each record has a header");
                (header.epoch, header.sequence_number)
            })
            .collect()
    }

    /// Asserts every record in `retransmit` carries a sequence number strictly greater than any
    /// record of the same epoch in `original` — i.e. the flight was regenerated, not replayed
    /// verbatim (a replay would be dropped by the peer's replay window, RFC 6347 4.1.2.6).
    fn assert_fresh_sequence_numbers(original: &[BytesMut], retransmit: &[BytesMut]) {
        use std::collections::HashMap;

        let by_epoch = |flight: &[BytesMut]| {
            let mut per_epoch: HashMap<u16, Vec<u64>> = HashMap::new();
            for datagram in flight {
                for (epoch, sequence_number) in record_sequence_numbers(datagram) {
                    per_epoch.entry(epoch).or_default().push(sequence_number);
                }
            }
            per_epoch
        };

        let original = by_epoch(original);
        let retransmit = by_epoch(retransmit);
        assert!(
            retransmit.contains_key(&1),
            "the retransmitted flight carries the encrypted Finished (epoch 1)"
        );
        for (epoch, retransmit_seqs) in &retransmit {
            if let Some(original_seqs) = original.get(epoch) {
                let max_original = original_seqs.iter().copied().max().unwrap_or(0);
                let min_retransmit = retransmit_seqs.iter().copied().min().unwrap_or(0);
                assert!(
                    min_retransmit > max_original,
                    "epoch {epoch}: retransmitted record seq {min_retransmit} must be fresh, not a replay of {max_original}"
                );
            }
        }
    }

    /// RFC 6347 4.2.4: the sender of the last flight has to retransmit it when the peer repeats its
    /// own final flight, because it cannot know its flight arrived. Here the server's last flight
    /// (ChangeCipherSpec + Finished) is lost in transit, the client retransmits Flight 5 on its own
    /// timer, and the server answers with a fresh flight (fresh record sequence numbers, not a
    /// replay) so the client completes.
    #[cfg(feature = "crypto-ring")]
    #[test]
    fn a_completed_server_retransmits_a_fresh_last_flight_so_the_client_completes() -> Result<()> {
        let provider: Arc<dyn RTCCryptoProvider> = Arc::new(crypto::providers::RingProvider::new());
        let suites = [CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Gcm_Sha256];
        let client_config = config(provider.clone(), true, &suites)?;
        let server_config = config(provider, false, &suites)?;
        let mut client = Endpoint::new(client_addr(), TransportProtocol::UDP, None);
        let mut server = Endpoint::new(server_addr(), TransportProtocol::UDP, Some(server_config));
        client.connect(Instant::now(), server_addr(), client_config, None)?;

        // Drive until the server completes. Its last flight is queued at that point, not yet taken.
        let mut server_complete = false;
        for _ in 0..32 {
            for event in transfer(&mut client, &mut server, client_addr())? {
                server_complete |= matches!(event, EndpointEvent::HandshakeComplete);
            }
            if server_complete {
                break;
            }
            transfer(&mut server, &mut client, server_addr())?;
        }
        assert!(server_complete, "the server side never completed");

        // Lose it: capture the server's queued last flight (to compare sequence numbers) but
        // deliver none of it.
        let mut original_last_flight = vec![];
        while let Some(transmit) = server.poll_transmit() {
            original_last_flight.push(transmit.message);
        }
        assert!(
            !original_last_flight.is_empty(),
            "the server had a last flight to lose"
        );

        // The client is still waiting for that flight; its retransmit timer fires and it repeats
        // Flight 5.
        let deadline = client
            .poll_timeout(&server_addr())
            .expect("a client waiting for the last flight arms a retransmit timer");
        client.handle_timeout(server_addr(), deadline)?;
        let mut client_repeat = vec![];
        while let Some(transmit) = client.poll_transmit() {
            client_repeat.push(transmit);
        }
        assert!(
            !client_repeat.is_empty(),
            "the client retransmitted its own flight"
        );

        // RFC 6347 4.2.4 requires the completed server to answer with its last flight again.
        for transmit in client_repeat {
            server.read(
                deadline,
                client_addr(),
                transmit.transport.ecn,
                transmit.message,
            )?;
        }
        let mut retransmit = vec![];
        while let Some(transmit) = server.poll_transmit() {
            retransmit.push(transmit.message);
        }
        assert!(
            !retransmit.is_empty(),
            "a completed server has to retransmit its last flight when the peer repeats its own"
        );
        assert_fresh_sequence_numbers(&original_last_flight, &retransmit);

        // Feed the fresh flight back; the client must now actually complete.
        let mut client_complete = false;
        for datagram in retransmit {
            for event in client.read(deadline, server_addr(), None, datagram)? {
                client_complete |= matches!(event, EndpointEvent::HandshakeComplete);
            }
        }
        assert!(
            client_complete,
            "the client completes once the retransmitted last flight arrives"
        );
        Ok(())
    }

    /// RFC 6347 4.2.4 for the client role: a completed client repeats its own last flight (Flight 5)
    /// when the peer repeats its final flight. Flight 5 carries the client certificate, so it spans
    /// epoch-0 records (certificate/key-exchange/verify/ChangeCipherSpec) plus the epoch-1 Finished,
    /// and each repeat carries fresh record sequence numbers.
    #[cfg(feature = "crypto-ring")]
    #[test]
    fn a_completed_client_retransmits_its_full_last_flight_with_fresh_record_sequence_numbers()
    -> Result<()> {
        let (mut client, _server) = completed_client_and_server()?;

        client.read(
            Instant::now(),
            server_addr(),
            None,
            junk_epoch_zero_handshake_datagram(20_000),
        )?;
        let mut first = vec![];
        while let Some(transmit) = client.poll_transmit() {
            first.push(transmit.message);
        }
        assert!(!first.is_empty(), "the client retransmits its last flight");
        let epochs: std::collections::HashSet<u16> = first
            .iter()
            .flat_map(|datagram| record_sequence_numbers(datagram))
            .map(|(epoch, _)| epoch)
            .collect();
        assert!(
            epochs.contains(&0) && epochs.contains(&1),
            "the retransmit is the full Flight 5 (epoch-0 records plus the epoch-1 Finished), not a bare answer"
        );

        client.read(
            Instant::now(),
            server_addr(),
            None,
            junk_epoch_zero_handshake_datagram(20_001),
        )?;
        let mut second = vec![];
        while let Some(transmit) = client.poll_transmit() {
            second.push(transmit.message);
        }
        assert!(
            !second.is_empty(),
            "the client retransmits again on the next repeat"
        );
        assert_fresh_sequence_numbers(&first, &second);
        Ok(())
    }

    /// A completed client fed a stream of unauthenticated epoch-0 handshake records must answer at
    /// most `maximum_retransmit_number` times and then fall silent (closing the RFC 6347 4.2.4
    /// retransmission amplification), and must never error out of `read` — an `ErrInvalidFsmTransition`
    /// escaping here would tear down the established association.
    #[cfg(feature = "crypto-ring")]
    #[test]
    fn a_completed_client_neither_amplifies_nor_tears_down_when_flooded_with_handshake_records()
    -> Result<()> {
        let (mut client, _server) = completed_client_and_server()?;

        let floods = DEFAULT_MAXIMUM_RETRANSMIT_NUMBER + 5;
        let mut answered = 0usize;
        let mut silent_after_budget = true;
        for index in 0..floods {
            let datagram = junk_epoch_zero_handshake_datagram(10_000 + index as u64);
            let result = client.read(Instant::now(), server_addr(), None, datagram);
            assert!(
                result.is_ok(),
                "read {index} of a junk handshake record must not tear down an established association"
            );
            let mut produced = false;
            while client.poll_transmit().is_some() {
                produced = true;
            }
            if produced {
                answered += 1;
            }
            if index >= DEFAULT_MAXIMUM_RETRANSMIT_NUMBER {
                silent_after_budget &= !produced;
            }
        }
        assert!(answered > 0, "the retransmission path is reachable at all");
        assert!(
            answered <= DEFAULT_MAXIMUM_RETRANSMIT_NUMBER,
            "a completed client answered {answered} floods but the budget is {DEFAULT_MAXIMUM_RETRANSMIT_NUMBER}"
        );
        assert!(
            silent_after_budget,
            "the retransmit budget must silence the flood once it is spent"
        );
        Ok(())
    }

    /// The last-flight retransmission must be gated on this datagram carrying a handshake record,
    /// not on the persisted `handshake_rx` flag. Application data arriving after the handshake —
    /// even with `handshake_rx` left set — must be decrypted and answered with nothing, never with a
    /// spurious flight.
    #[cfg(feature = "crypto-ring")]
    #[test]
    fn application_data_does_not_retrigger_a_completed_client_even_with_a_stale_handshake_flag()
    -> Result<()> {
        let (mut client, mut server) = completed_client_and_server()?;

        // Simulate the leak finding 3 describes: a `handshake_rx` left set from an earlier datagram.
        client
            .connections
            .get_mut(&server_addr())
            .expect("the client holds the association")
            .handshake_rx = Some(());

        server.write(
            Instant::now(),
            client_addr(),
            b"post-handshake application data",
        )?;
        let events = transfer(&mut server, &mut client, server_addr())?;
        assert!(
            events.iter().any(|event| matches!(
                event,
                EndpointEvent::ApplicationData(data) if data.as_ref() == b"post-handshake application data"
            )),
            "the completed client still decrypts application data"
        );
        assert!(
            client.poll_transmit().is_none(),
            "application data must not make a completed client emit a handshake flight"
        );
        Ok(())
    }

    /// A peer's close_notify (RFC 5246 §7.2.1) ends the connection gracefully: data that arrived
    /// before it is still delivered, the close is reported as an event rather than an error, the
    /// matching close_notify reply is sent, and the connection is dropped (rtc#255).
    #[cfg(feature = "crypto-ring")]
    #[test]
    fn a_peer_close_notify_is_answered_and_reported() -> Result<()> {
        use crate::content::ContentType;

        let (mut client, mut server) = completed_client_and_server()?;

        client.write(Instant::now(), server_addr(), b"before close")?;
        client.close(Instant::now())?;
        let events = transfer(&mut client, &mut server, client_addr())?;

        assert!(
            matches!(
                events.as_slice(),
                [EndpointEvent::ApplicationData(data), EndpointEvent::PeerClosed]
                    if data.as_ref() == b"before close"
            ),
            "expected the data, then PeerClosed"
        );
        assert!(
            server.connections.is_empty(),
            "the closed connection is removed"
        );

        let reply = server
            .poll_transmit()
            .expect("the close_notify reply is sent");
        assert_eq!(
            ContentType::Alert as u8,
            reply.message[0],
            "the reply is an alert record"
        );
        assert!(server.poll_transmit().is_none());
        Ok(())
    }

    /// Once the handshake has completed, a genuine peer sends nothing but handshake records
    /// (repeating its final flight) in epoch 0, so any other plaintext record can only be forged
    /// or stale. None of them may close the connection, fail the read or provoke an alert:
    /// otherwise anyone able to spoof the peer's address could tear down an authenticated
    /// association with one unauthenticated datagram (RFC 6347 §4.1.2.7: invalid records are
    /// silently discarded).
    #[cfg(feature = "crypto-ring")]
    #[test]
    fn a_plaintext_record_cannot_close_an_established_connection() -> Result<()> {
        use crate::alert::{AlertDescription, AlertLevel};

        let (mut client, mut server) = completed_client_and_server()?;

        let close_notify = [
            AlertLevel::Warning as u8,
            AlertDescription::CloseNotify as u8,
        ];
        let fatal = [
            AlertLevel::Fatal as u8,
            AlertDescription::HandshakeFailure as u8,
        ];
        let forgeries: [(&str, u8, &[u8]); 7] = [
            ("close_notify alert", 21, &close_notify),
            ("fatal alert", 21, &fatal),
            ("truncated alert", 21, &[AlertLevel::Fatal as u8]),
            ("application data", 23, b"forged"),
            ("change_cipher_spec", 20, &[0x01]),
            ("malformed change_cipher_spec", 20, &[0x02]),
            ("unknown content type", 99, b"forged"),
        ];
        for (sequence_number, (name, content_type, body)) in (1_000_000..).zip(forgeries) {
            let forged = plaintext_record_datagram(content_type, body, sequence_number);
            let events = server.read(Instant::now(), client_addr(), None, forged)?;
            assert!(
                events.is_empty(),
                "a forged {name} was reported as {} event(s)",
                events.len()
            );
            assert!(
                server.connections.contains_key(&client_addr()),
                "a forged {name} removed the connection"
            );
            assert!(
                server.poll_transmit().is_none(),
                "a forged {name} was answered"
            );
        }

        // and the connection still carries data both ways
        client.write(Instant::now(), server_addr(), b"still here")?;
        let events = transfer(&mut client, &mut server, client_addr())?;
        assert!(
            events.iter().any(|event| matches!(
                event,
                EndpointEvent::ApplicationData(data) if data.as_ref() == b"still here"
            )),
            "the connection stopped carrying data after a forged record"
        );
        server.write(Instant::now(), client_addr(), b"and back")?;
        let events = transfer(&mut server, &mut client, server_addr())?;
        assert!(
            events.iter().any(|event| matches!(
                event,
                EndpointEvent::ApplicationData(data) if data.as_ref() == b"and back"
            )),
            "the connection stopped carrying data back after a forged record"
        );
        Ok(())
    }
}
