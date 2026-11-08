/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! DTLS over the selected ICE pair: the certificate the SDP layer fingerprints, the handshake
//! in either role, and the keying material SRTP is derived from.
//!
//! A `Certificate` is self-signed ECDSA P-256, and its fingerprints are published as SHA-256,
//! SHA-384, and SHA-512 colon-separated upper-case hex, the forms a browser accepts in
//! `a=fingerprint`. A peer is authenticated by comparing the certificate it presents against the
//! fingerprints its SDP carried, never by a CA chain: there must be at least one fingerprint
//! with a supported algorithm, and every supported one must match. The check runs inside the
//! handshake, so a mismatch ends it with a `bad_certificate` alert, and once more on the
//! completed state. As the DTLS server this side requires a client certificate, as WebRTC does.
//!
//! The SRTP profiles negotiated through `use_srtp`, most preferred first, are AES-256-GCM,
//! AES-128-GCM, and AES-128-CM with an 80-bit HMAC-SHA1 tag (RFC 5764, RFC 7714), and the
//! keying material is the RFC 5764 export, `2 * (key + salt)` bytes under the label
//! `EXTRACTOR-dtls_srtp`. The cipher suites offered are ECDHE-ECDSA with AES-128-GCM,
//! ChaCha20-Poly1305, and AES-256-CBC-SHA.
//!
//! `Dtls` is sans-IO over `rtc_dtls::endpoint::Endpoint`: the task feeds it the datagrams that
//! arrive on the pair, drives its retransmission timer, and sends what it queues. The endpoint
//! keys associations by a remote address, and a WebRTC session has exactly one association
//! however the pair moves, so the association lives under one fixed key and its output always
//! goes to whatever pair is selected.

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use aws_lc_rs::digest::{SHA256, SHA384, SHA512, digest};
use bytes::BytesMut;
use log::debug;
use rtc_dtls::cipher_suite::CipherSuiteId;
use rtc_dtls::config::{ClientAuthType, ConfigBuilder, HandshakeConfig};
use rtc_dtls::crypto::Certificate as DtlsCertificate;
use rtc_dtls::endpoint::{Endpoint, EndpointEvent};
use rtc_dtls::extension::extension_use_srtp::SrtpProtectionProfile;
use rtc_shared::TransportProtocol;

use crate::ice::stun::crypto;
use crate::srtp::{KEYING_LABEL, SrtpProfile};

/// The one key the peer's association lives under inside the endpoint.
const PEER_KEY: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 1);
const LOCAL_KEY: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 2);

/// The fingerprint algorithms published and accepted, in the order published.
pub const FINGERPRINT_ALGORITHMS: [&str; 3] = ["sha-256", "sha-384", "sha-512"];

/// The SRTP protection profiles offered, most preferred first.
pub const SRTP_PROFILES: [SrtpProtectionProfile; 3] = [
    SrtpProtectionProfile::Srtp_Aead_Aes_256_Gcm,
    SrtpProtectionProfile::Srtp_Aead_Aes_128_Gcm,
    SrtpProtectionProfile::Srtp_Aes128_Cm_Hmac_Sha1_80,
];

/// What DTLS fails on.
#[derive(Debug, thiserror::Error)]
pub enum DtlsError {
    #[error("certificate: {0}")]
    Certificate(String),
    #[error("DTLS handshake failed ({0})")]
    Handshake(String),
    #[error("DTLS is not connected")]
    NotConnected,
    #[error("{0}")]
    Other(String),
}

/// A certificate's hash under one algorithm, as `a=fingerprint` carries it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    /// The hash function name, `sha-256` for instance.
    pub algorithm: String,
    /// Upper-case hex octets separated by colons.
    pub value: String,
}

/// The fingerprint of a DER certificate under `algorithm`, or `None` for an unsupported one.
pub fn certificate_digest(der: &[u8], algorithm: &str) -> Option<String> {
    let digest: Vec<u8> = match algorithm.to_ascii_lowercase().as_str() {
        "sha-256" => digest(&SHA256, der).as_ref().to_vec(),
        "sha-384" => digest(&SHA384, der).as_ref().to_vec(),
        "sha-512" => digest(&SHA512, der).as_ref().to_vec(),
        _ => return None,
    };
    Some(
        digest
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(":"),
    )
}

/// The self-signed certificate one DTLS transport presents.
pub struct Certificate {
    inner: DtlsCertificate,
    der: Vec<u8>,
}

impl Certificate {
    /// Generate a self-signed ECDSA P-256 certificate.
    pub fn generate() -> Result<Certificate, DtlsError> {
        let provider = crypto();
        let inner = DtlsCertificate::generate_self_signed(Vec::<String>::new(), provider.crypto())
            .map_err(|e| DtlsError::Certificate(e.to_string()))?;
        let der = inner
            .certificate
            .first()
            .map(|c| c.as_ref().to_vec())
            .ok_or_else(|| DtlsError::Certificate("no certificate generated".into()))?;
        Ok(Certificate { inner, der })
    }

    /// The certificate's DER encoding.
    pub fn der(&self) -> &[u8] {
        &self.der
    }

    /// One fingerprint per published algorithm.
    pub fn fingerprints(&self) -> Vec<Fingerprint> {
        FINGERPRINT_ALGORITHMS
            .iter()
            .filter_map(|algorithm| {
                certificate_digest(&self.der, algorithm).map(|value| Fingerprint {
                    algorithm: algorithm.to_string(),
                    value,
                })
            })
            .collect()
    }
}

/// The DTLS role a side takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtlsRole {
    /// Decided by the caller from the ICE role: controlling is the server.
    Auto,
    Client,
    Server,
}

impl DtlsRole {
    /// The role as `RTCDtlsParameters.role` spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            DtlsRole::Auto => "auto",
            DtlsRole::Client => "client",
            DtlsRole::Server => "server",
        }
    }
}

/// The DTLS parameters one side signals.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtlsParameters {
    pub fingerprints: Vec<Fingerprint>,
    pub role: DtlsRole,
}

/// The state of a DTLS transport, as `RTCDtlsTransport.state` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtlsState {
    New,
    Connecting,
    Connected,
    Closed,
    Failed,
}

impl DtlsState {
    /// The state as the W3C API spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            DtlsState::New => "new",
            DtlsState::Connecting => "connecting",
            DtlsState::Connected => "connected",
            DtlsState::Closed => "closed",
            DtlsState::Failed => "failed",
        }
    }
}

/// What the DTLS transport reports to its task.
#[derive(Debug)]
pub enum DtlsEvent {
    /// The handshake completed with `profile` negotiated; SRTP is keyed from the material.
    Connected {
        profile: SrtpProfile,
        keying_material: Vec<u8>,
        is_client: bool,
    },
    /// Application data: an SCTP packet.
    Data(BytesMut),
    /// The peer closed the association.
    Closed,
    /// The handshake failed, or the association broke.
    Failed(String),
}

/// Whether every supported fingerprint in `expected` matches `der`, and at least one does.
pub fn fingerprints_match(der: &[u8], expected: &[Fingerprint]) -> bool {
    let mut supported = 0;
    let mut valid = 0;
    for fingerprint in expected {
        if let Some(digest) = certificate_digest(der, &fingerprint.algorithm) {
            supported += 1;
            if fingerprint.value.to_ascii_uppercase() == digest {
                valid += 1;
            }
        }
    }
    supported > 0 && valid == supported
}

/// One DTLS association over the selected pair.
pub struct Dtls {
    certificate: Arc<Certificate>,
    endpoint: Option<Endpoint>,
    state: DtlsState,
    is_client: bool,
    expected: Vec<Fingerprint>,
    events: VecDeque<DtlsEvent>,
}

impl Dtls {
    /// A transport presenting `certificate`, idle until `start`.
    pub fn new(certificate: Arc<Certificate>) -> Dtls {
        Dtls {
            certificate,
            endpoint: None,
            state: DtlsState::New,
            is_client: false,
            expected: Vec::new(),
            events: VecDeque::new(),
        }
    }

    /// The transport's state.
    pub fn state(&self) -> DtlsState {
        self.state
    }

    /// Whether `start` was called.
    pub fn started(&self) -> bool {
        self.endpoint.is_some()
    }

    /// The local parameters: the certificate's fingerprints and `Auto`.
    pub fn local_parameters(&self) -> DtlsParameters {
        DtlsParameters {
            fingerprints: self.certificate.fingerprints(),
            role: DtlsRole::Auto,
        }
    }

    fn build_config(&self, is_client: bool) -> Result<Arc<HandshakeConfig>, DtlsError> {
        let expected = self.expected.clone();
        let mut builder = ConfigBuilder::default()
            .with_crypto_provider(crypto())
            .with_certificates(vec![self.certificate.inner.clone()])
            .with_cipher_suites(vec![
                CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Gcm_Sha256,
                CipherSuiteId::Tls_Ecdhe_Ecdsa_With_ChaCha20_Poly1305_Sha256,
                CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_256_Cbc_Sha,
            ])
            .with_srtp_protection_profiles(SRTP_PROFILES.to_vec())
            .with_insecure_skip_verify(true)
            .with_verify_peer_certificate(Some(Arc::new(move |certs: &[Vec<u8>], _chains| {
                match certs.first() {
                    Some(der) if fingerprints_match(der, &expected) => Ok(()),
                    _ => Err(rtc_shared::error::Error::Other(
                        "fingerprint mismatch".into(),
                    )),
                }
            })));
        if !is_client {
            builder = builder.with_client_auth(ClientAuthType::RequireAnyClientCert);
        }
        builder
            .build(is_client, Some(PEER_KEY))
            .map(Arc::new)
            .map_err(|e| DtlsError::Other(format!("DTLS configuration: {e}")))
    }

    /// Start the handshake as client or server against a peer expected to present one of
    /// `remote.fingerprints`.
    ///
    /// # Errors
    /// The remote parameters carry no fingerprint, or the configuration cannot be built.
    pub fn start(
        &mut self,
        now: Instant,
        is_client: bool,
        remote: &DtlsParameters,
    ) -> Result<(), DtlsError> {
        if self.endpoint.is_some() {
            return Ok(());
        }
        if remote.fingerprints.is_empty() {
            return Err(DtlsError::Handshake("no remote fingerprint".into()));
        }
        self.expected = remote.fingerprints.clone();
        self.is_client = is_client;
        let config = self.build_config(is_client)?;
        let mut endpoint = Endpoint::new(
            LOCAL_KEY,
            TransportProtocol::UDP,
            if is_client {
                None
            } else {
                Some(config.clone())
            },
        );
        if is_client {
            endpoint
                .connect(now, PEER_KEY, config, None)
                .map_err(|e| DtlsError::Handshake(e.to_string()))?;
        }
        self.endpoint = Some(endpoint);
        self.set_state(DtlsState::Connecting);
        Ok(())
    }

    fn set_state(&mut self, state: DtlsState) {
        if self.state != state {
            debug!(target: "webrtc.dtls", "DTLS {} -> {}", self.state.as_str(), state.as_str());
            self.state = state;
        }
    }

    /// Feed one DTLS datagram from the pair.
    pub fn handle_datagram(&mut self, now: Instant, data: BytesMut) {
        let Some(endpoint) = self.endpoint.as_mut() else {
            return;
        };
        if matches!(self.state, DtlsState::Closed | DtlsState::Failed) {
            return;
        }
        match endpoint.read(now, PEER_KEY, None, data) {
            Ok(events) => {
                for event in events {
                    self.on_event(event);
                }
            }
            Err(e) => {
                if self.state == DtlsState::Connecting {
                    self.fail(format!("error {e}"));
                } else {
                    debug!(target: "webrtc.dtls", "DTLS record refused: {e}");
                }
            }
        }
    }

    fn on_event(&mut self, event: EndpointEvent) {
        match event {
            EndpointEvent::HandshakeComplete => self.complete(),
            EndpointEvent::ApplicationData(data) => self.events.push_back(DtlsEvent::Data(data)),
            EndpointEvent::PeerClosed => {
                debug!(target: "webrtc.dtls", "DTLS shutdown by remote party");
                self.set_state(DtlsState::Closed);
                self.events.push_back(DtlsEvent::Closed);
            }
            _ => {}
        }
    }

    fn complete(&mut self) {
        let Some(state) = self
            .endpoint
            .as_ref()
            .and_then(|e| e.get_connection_state(PEER_KEY))
        else {
            self.fail("connection state missing".into());
            return;
        };
        let der = state.peer_certificates.first().cloned().unwrap_or_default();
        if !fingerprints_match(&der, &self.expected) {
            self.fail("fingerprint mismatch".into());
            return;
        }
        let Some(profile) = SrtpProfile::from_dtls(state.srtp_protection_profile()) else {
            self.fail("no SRTP profile negotiated".into());
            return;
        };
        let material =
            match state.export_keying_material(KEYING_LABEL, &[], profile.keying_material_len()) {
                Ok(m) => m.into_bytes(),
                Err(e) => {
                    self.fail(format!("keying material: {e}"));
                    return;
                }
            };
        let is_client = state.is_client();
        debug!(target: "webrtc.dtls", "DTLS handshake negotiated {}", profile.name());
        self.set_state(DtlsState::Connected);
        self.events.push_back(DtlsEvent::Connected {
            profile,
            keying_material: material,
            is_client,
        });
    }

    fn fail(&mut self, reason: String) {
        debug!(target: "webrtc.dtls", "DTLS handshake failed ({reason})");
        self.set_state(DtlsState::Failed);
        self.events.push_back(DtlsEvent::Failed(reason));
    }

    /// Queue application data (an SCTP packet) for the peer.
    ///
    /// # Errors
    /// `NotConnected` before the handshake completes.
    pub fn write(&mut self, now: Instant, data: &[u8]) -> Result<(), DtlsError> {
        if self.state != DtlsState::Connected {
            return Err(DtlsError::NotConnected);
        }
        let endpoint = self.endpoint.as_mut().ok_or(DtlsError::NotConnected)?;
        endpoint
            .write(now, PEER_KEY, data)
            .map_err(|e| DtlsError::Other(e.to_string()))
    }

    /// The next datagram to send on the pair.
    pub fn poll_transmit(&mut self) -> Option<BytesMut> {
        self.endpoint.as_mut()?.poll_transmit().map(|t| t.message)
    }

    /// The next event.
    pub fn poll_event(&mut self) -> Option<DtlsEvent> {
        self.events.pop_front()
    }

    /// When the handshake's retransmission timer is next due.
    pub fn poll_timeout(&self) -> Option<Instant> {
        if self.state != DtlsState::Connecting {
            return None;
        }
        self.endpoint.as_ref()?.poll_timeout(&PEER_KEY)
    }

    /// Advance the handshake's retransmission timer.
    pub fn handle_timeout(&mut self, now: Instant) {
        let Some(endpoint) = self.endpoint.as_mut() else {
            return;
        };
        if self.state != DtlsState::Connecting {
            return;
        }
        if let Err(e) = endpoint.handle_timeout(PEER_KEY, now) {
            self.fail(format!("timeout {e}"));
        }
    }

    /// Send `close_notify` and end the association.
    pub fn close(&mut self, now: Instant) {
        if let Some(endpoint) = self.endpoint.as_mut()
            && matches!(self.state, DtlsState::Connecting | DtlsState::Connected)
        {
            let _ = endpoint.close(now);
            debug!(target: "webrtc.dtls", "DTLS shutdown complete");
        }
        if self.state != DtlsState::Failed {
            self.set_state(DtlsState::Closed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_are_colon_hex_in_three_algorithms() {
        let certificate = Certificate::generate().unwrap();
        let fingerprints = certificate.fingerprints();
        assert_eq!(fingerprints.len(), 3);
        assert_eq!(fingerprints[0].algorithm, "sha-256");
        assert_eq!(fingerprints[0].value.len(), 32 * 3 - 1);
        assert!(
            fingerprints[0]
                .value
                .bytes()
                .all(|b| b == b':' || b.is_ascii_hexdigit())
        );
        assert!(fingerprints_match(certificate.der(), &fingerprints));
        let other = Certificate::generate().unwrap();
        assert!(!fingerprints_match(other.der(), &fingerprints));
        assert!(!fingerprints_match(
            certificate.der(),
            &[Fingerprint {
                algorithm: "md5".into(),
                value: "00".into()
            }]
        ));
        let mixed = vec![
            fingerprints[0].clone(),
            Fingerprint {
                algorithm: "sha-384".into(),
                value: "AA".into(),
            },
        ];
        assert!(!fingerprints_match(certificate.der(), &mixed));
    }

    /// Two transports complete a handshake in memory and exchange application records, each
    /// datagram delivered whole and without a later one.
    #[test]
    fn records_are_delivered_as_they_arrive_and_close_notify_closes() {
        let now = Instant::now();
        let server_cert = Arc::new(Certificate::generate().unwrap());
        let client_cert = Arc::new(Certificate::generate().unwrap());
        let mut server = Dtls::new(server_cert.clone());
        let mut client = Dtls::new(client_cert.clone());
        server
            .start(now, false, &client.local_parameters())
            .unwrap();
        client.start(now, true, &server.local_parameters()).unwrap();
        let mut keys = None;
        for _ in 0..40 {
            while let Some(d) = client.poll_transmit() {
                server.handle_datagram(now, d);
            }
            while let Some(d) = server.poll_transmit() {
                client.handle_datagram(now, d);
            }
            while let Some(e) = server.poll_event() {
                if let DtlsEvent::Connected {
                    profile,
                    keying_material,
                    is_client,
                } = e
                {
                    assert!(!is_client);
                    keys = Some((profile, keying_material));
                }
            }
            while let Some(e) = client.poll_event() {
                if let DtlsEvent::Failed(r) = e {
                    panic!("client failed: {r}");
                }
            }
            if server.state() == DtlsState::Connected && client.state() == DtlsState::Connected {
                break;
            }
        }
        let (profile, material) = keys.expect("the server completed and exported keys");
        assert_eq!(profile, SrtpProfile::AeadAes256Gcm);
        assert_eq!(material.len(), profile.keying_material_len());

        client.write(now, b"synthetic_input_0").unwrap();
        client.write(now, b"synthetic_input_1").unwrap();
        let mut bundled = BytesMut::new();
        while let Some(d) = client.poll_transmit() {
            bundled.extend_from_slice(&d);
        }
        server.handle_datagram(now, bundled);
        let mut delivered = Vec::new();
        while let Some(e) = server.poll_event() {
            if let DtlsEvent::Data(d) = e {
                delivered.push(d.to_vec());
            }
        }
        assert_eq!(
            delivered,
            vec![b"synthetic_input_0".to_vec(), b"synthetic_input_1".to_vec()]
        );

        client.write(now, b"synthetic_input_2").unwrap();
        let record = client.poll_transmit().unwrap();
        server.handle_datagram(now, record.clone());
        let mut third = Vec::new();
        while let Some(e) = server.poll_event() {
            if let DtlsEvent::Data(d) = e {
                third.push(d.to_vec());
            }
        }
        assert_eq!(third, vec![b"synthetic_input_2".to_vec()]);
        server.handle_datagram(now, record);
        assert!(
            server.poll_event().is_none(),
            "a replayed record yields nothing"
        );

        client.close(now);
        while let Some(d) = client.poll_transmit() {
            server.handle_datagram(now, d);
        }
        let mut closed = false;
        while let Some(e) = server.poll_event() {
            closed |= matches!(e, DtlsEvent::Closed);
        }
        assert!(closed && server.state() == DtlsState::Closed);
    }

    #[test]
    fn a_wrong_fingerprint_fails_the_handshake() {
        let now = Instant::now();
        let server_cert = Arc::new(Certificate::generate().unwrap());
        let client_cert = Arc::new(Certificate::generate().unwrap());
        let impostor = Certificate::generate().unwrap();
        let mut server = Dtls::new(server_cert);
        let mut client = Dtls::new(client_cert.clone());
        server
            .start(
                now,
                false,
                &DtlsParameters {
                    fingerprints: impostor.fingerprints(),
                    role: DtlsRole::Client,
                },
            )
            .unwrap();
        client
            .start(now, true, &Dtls::new(client_cert).local_parameters())
            .unwrap_or(());
        let mut failed = false;
        for _ in 0..40 {
            while let Some(d) = client.poll_transmit() {
                server.handle_datagram(now, d);
            }
            while let Some(d) = server.poll_transmit() {
                client.handle_datagram(now, d);
            }
            while let Some(e) = server.poll_event() {
                failed |= matches!(e, DtlsEvent::Failed(_));
            }
            while client.poll_event().is_some() {}
            if failed || server.state() == DtlsState::Failed {
                break;
            }
        }
        assert!(failed || server.state() == DtlsState::Failed);
    }
}
