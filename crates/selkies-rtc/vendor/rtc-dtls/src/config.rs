//! Handshake configuration.
//!
//! [`ConfigBuilder`](crate::config::ConfigBuilder) is what a caller supplies: certificates, the client/server role, which cipher
//! suites and curves to offer, the SRTP protection profiles to negotiate through `use_srtp`,
//! and how strictly to require the extended master secret
//! ([`ExtendedMasterSecretType`](crate::config::ExtendedMasterSecretType)).
//!
//! WebRTC authenticates peers by comparing the certificate fingerprint against the one
//! signalled in SDP, not against a CA chain — so certificates here are normally self-signed
//! (see [`gen_self_signed_root_cert`](crate::config::gen_self_signed_root_cert)) and the check is implemented by supplying a
//! [`VerifyPeerCertificateFn`](crate::config::VerifyPeerCertificateFn).
//!
//! [`HandshakeConfig`](crate::config::HandshakeConfig) is the resolved form the handshake
//! actually runs with, produced by [`ConfigBuilder::build`](crate::config::ConfigBuilder::build).

#[cfg(test)]
mod config_test;

use crate::cipher_suite::*;
use crate::conn::{
    DEFAULT_MAXIMUM_RETRANSMIT_NUMBER, DEFAULT_REPLAY_PROTECTION_WINDOW, INITIAL_TICKER_INTERVAL,
};
use crate::crypto::*;
use crate::curve::named_curve::NamedCurve;
use crate::extension::extension_use_srtp::SrtpProtectionProfile;
use crate::signature_hash_algorithm::{
    SignatureHashAlgorithm, SignatureScheme, parse_signature_schemes,
};
use crypto::RTCCryptoProvider;
use log::warn;
use shared::error::*;
use std::collections::HashMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::CertificateDer;
use rustls::server::danger::ClientCertVerifier;

/// Explicit rustls/webpki backend used only for CA-chain and hostname verification.
///
/// This policy adapter is separate from [`RTCCryptoProvider`]. Applications authenticating with
/// SDP fingerprints do not need it, while applications enabling WebPKI validation can select its
/// backend without changing their primitive RTC provider.
#[derive(Clone)]
pub struct RustlsVerifierAdapter {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl RustlsVerifierAdapter {
    /// Wraps a rustls crypto provider for WebPKI verification.
    #[must_use]
    pub fn new(provider: Arc<rustls::crypto::CryptoProvider>) -> Self {
        Self { provider }
    }

    /// Uses rustls's ring verification backend.
    #[cfg(feature = "crypto-ring")]
    #[must_use]
    pub fn ring() -> Self {
        Self::new(Arc::new(rustls::crypto::ring::default_provider()))
    }

    /// Uses rustls's AWS-LC-RS verification backend.
    #[cfg(feature = "crypto-aws-lc-rs")]
    #[must_use]
    pub fn aws_lc_rs() -> Self {
        Self::new(Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
    }
}

fn default_verifier_adapter() -> Option<RustlsVerifierAdapter> {
    #[cfg(feature = "crypto-ring")]
    {
        Some(RustlsVerifierAdapter::ring())
    }
    #[cfg(all(not(feature = "crypto-ring"), feature = "crypto-aws-lc-rs"))]
    {
        Some(RustlsVerifierAdapter::aws_lc_rs())
    }
    #[cfg(not(any(feature = "crypto-ring", feature = "crypto-aws-lc-rs")))]
    {
        None
    }
}

/// Builds the default server-certificate verifier, with an explicit provider where we have one.
///
/// # Errors
///
/// Fails if the root store holds no usable trust anchors.
fn server_cert_verifier(
    roots: std::sync::Arc<rustls::RootCertStore>,
    adapter: &RustlsVerifierAdapter,
) -> Result<std::sync::Arc<dyn ServerCertVerifier>> {
    let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(
        roots,
        adapter.provider.clone(),
    )
    .build()
    .map_err(|err| Error::Other(format!("rustls server cert verifier: {err}")))?;
    Ok(verifier)
}

/// Config is used to configure a DTLS client or server.
/// After a Config is passed to a DTLS function it must not be modified.
#[derive(Clone)]
pub struct ConfigBuilder {
    crypto_provider: Option<Arc<dyn RTCCryptoProvider>>,
    certificates: Vec<Certificate>,
    cipher_suites: Vec<CipherSuiteId>,
    signature_schemes: Vec<SignatureScheme>,
    srtp_protection_profiles: Vec<SrtpProtectionProfile>,
    client_auth: ClientAuthType,
    extended_master_secret: ExtendedMasterSecretType,
    flight_interval: Duration,
    psk: Option<PskCallback>,
    psk_identity_hint: Option<Vec<u8>>,
    insecure_skip_verify: bool,
    insecure_hashes: bool,
    insecure_verification: bool,
    verify_peer_certificate: Option<VerifyPeerCertificateFn>,
    roots_cas: rustls::RootCertStore,
    client_cas: rustls::RootCertStore,
    verifier_adapter: Option<RustlsVerifierAdapter>,
    server_name: String,
    mtu: usize,
    replay_protection_window: usize,
}

impl Default for ConfigBuilder {
    fn default() -> Self {
        Self {
            crypto_provider: None,
            certificates: vec![],
            cipher_suites: vec![],
            signature_schemes: vec![],
            srtp_protection_profiles: vec![],
            client_auth: ClientAuthType::default(),
            extended_master_secret: ExtendedMasterSecretType::default(),
            flight_interval: Duration::default(),
            psk: None,
            psk_identity_hint: None,
            insecure_skip_verify: false,
            insecure_hashes: false,
            insecure_verification: false,
            verify_peer_certificate: None,
            roots_cas: rustls::RootCertStore::empty(),
            client_cas: rustls::RootCertStore::empty(),
            verifier_adapter: default_verifier_adapter(),
            server_name: String::default(),
            mtu: 0,
            replay_protection_window: 0,
        }
    }
}

impl ConfigBuilder {
    /// Selects the cryptography and CSPRNG implementation for this DTLS association.
    ///
    /// The provider is resolved while building the handshake configuration and is reused for the
    /// entire handshake and record lifetime. No global registration is required.
    pub fn with_crypto_provider(mut self, provider: Arc<dyn RTCCryptoProvider>) -> Self {
        self.crypto_provider = Some(provider);
        self
    }

    /// certificates contains certificate chain to present to the other side of the connection.
    /// Server MUST set this if psk is nil, and MAY set it alongside a psk to serve both families
    /// Client SHOULD set this so CertificateRequests can be handled, and MUST NOT set it with a psk
    pub fn with_certificates(mut self, certificates: Vec<Certificate>) -> Self {
        self.certificates = certificates;
        self
    }

    /// cipher_suites is a list of supported cipher suites.
    /// If cipher_suites is nil, a default list is used
    pub fn with_cipher_suites(mut self, cipher_suites: Vec<CipherSuiteId>) -> Self {
        self.cipher_suites = cipher_suites;
        self
    }

    /// signature_schemes contains the signature and hash schemes that the peer requests to verify.
    pub fn with_signature_schemes(mut self, signature_schemes: Vec<SignatureScheme>) -> Self {
        self.signature_schemes = signature_schemes;
        self
    }

    /// srtp_protection_profiles are the supported protection profiles
    /// Clients will send this via use_srtp and assert that the server properly responds
    /// Servers will assert that clients send one of these profiles and will respond as needed
    pub fn with_srtp_protection_profiles(
        mut self,
        srtp_protection_profiles: Vec<SrtpProtectionProfile>,
    ) -> Self {
        self.srtp_protection_profiles = srtp_protection_profiles;
        self
    }

    /// client_auth determines the server's policy for
    /// TLS Client Authentication. The default is NoClientCert.
    pub fn with_client_auth(mut self, client_auth: ClientAuthType) -> Self {
        self.client_auth = client_auth;
        self
    }

    /// extended_master_secret determines if the "Extended Master Secret" extension
    /// should be disabled, requested, or required (default requested).
    pub fn with_extended_master_secret(
        mut self,
        extended_master_secret: ExtendedMasterSecretType,
    ) -> Self {
        self.extended_master_secret = extended_master_secret;
        self
    }

    /// flight_interval controls how often we send outbound handshake messages
    /// defaults to time.Second
    pub fn with_flight_interval(mut self, flight_interval: Duration) -> Self {
        self.flight_interval = flight_interval;
        self
    }

    /// psk sets the pre-shared key used by this DTLS connection
    /// A client with a psk offers only psk cipher_suites; a server that also holds certificates
    /// offers both families and lets the negotiated suite decide which credential it uses
    pub fn with_psk(mut self, psk: Option<PskCallback>) -> Self {
        self.psk = psk;
        self
    }

    /// psk_identity_hint sets the pre-shared key hint
    pub fn with_psk_identity_hint(mut self, psk_identity_hint: Option<Vec<u8>>) -> Self {
        self.psk_identity_hint = psk_identity_hint;
        self
    }

    /// insecure_skip_verify controls whether a client verifies the
    /// server's certificate chain and host name.
    /// If insecure_skip_verify is true, TLS accepts any certificate
    /// presented by the server and any host name in that certificate.
    /// In this mode, TLS is susceptible to man-in-the-middle attacks.
    /// This should be used only for testing.
    pub fn with_insecure_skip_verify(mut self, insecure_skip_verify: bool) -> Self {
        self.insecure_skip_verify = insecure_skip_verify;
        self
    }

    /// insecure_hashes allows the use of hashing algorithms that are known
    /// to be vulnerable.
    pub fn with_insecure_hashes(mut self, insecure_hashes: bool) -> Self {
        self.insecure_hashes = insecure_hashes;
        self
    }

    /// insecure_verification allows the use of verification algorithms that are
    /// known to be vulnerable or deprecated
    pub fn with_insecure_verification(mut self, insecure_verification: bool) -> Self {
        self.insecure_verification = insecure_verification;
        self
    }

    /// VerifyPeerCertificate, if not nil, is called after normal
    /// certificate verification by either a client or server. It
    /// receives the certificate provided by the peer and also a flag
    /// that tells if normal verification has succeeded. If it returns a
    /// non-nil error, the handshake is aborted and that error results.
    ///
    /// If normal verification fails then the handshake will abort before
    /// considering this callback. If normal verification is disabled by
    /// setting insecure_skip_verify, or (for a server) when client_auth is
    /// RequestClientCert or RequireAnyClientCert, then this callback will
    /// be considered but the verifiedChains will always be nil.
    pub fn with_verify_peer_certificate(
        mut self,
        verify_peer_certificate: Option<VerifyPeerCertificateFn>,
    ) -> Self {
        self.verify_peer_certificate = verify_peer_certificate;
        self
    }

    /// roots_cas defines the set of root certificate authorities
    /// that one peer uses when verifying the other peer's certificates.
    /// If RootCAs is nil, TLS uses the host's root CA set.
    /// Used by Client to verify server's certificate
    pub fn with_roots_cas(mut self, roots_cas: rustls::RootCertStore) -> Self {
        self.roots_cas = roots_cas;
        self
    }

    /// client_cas defines the set of root certificate authorities
    /// that servers use if required to verify a client certificate
    /// by the policy in client_auth.
    /// Used by Server to verify client's certificate
    pub fn with_client_cas(mut self, client_cas: rustls::RootCertStore) -> Self {
        self.client_cas = client_cas;
        self
    }

    /// Selects the rustls/webpki adapter used for optional CA-chain and hostname verification.
    pub fn with_rustls_verifier_adapter(mut self, adapter: RustlsVerifierAdapter) -> Self {
        self.verifier_adapter = Some(adapter);
        self
    }

    /// server_name is used to verify the hostname on the returned
    /// certificates unless insecure_skip_verify is given.
    pub fn with_server_name(mut self, server_name: String) -> Self {
        self.server_name = server_name;
        self
    }

    /// mtu is the length at which handshake messages will be fragmented to
    /// fit within the maximum transmission unit (default is 1200 bytes)
    pub fn with_mtu(mut self, mtu: usize) -> Self {
        self.mtu = mtu;
        self
    }

    /// replay_protection_window is the size of the replay attack protection window.
    /// Duplication of the sequence number is checked in this window size.
    /// Packet with sequence number older than this value compared to the latest
    /// accepted packet will be discarded. (default is 64)
    pub fn with_replay_protection_window(mut self, replay_protection_window: usize) -> Self {
        self.replay_protection_window = replay_protection_window;
        self
    }
}

pub(crate) const DEFAULT_MTU: usize = 1200; // bytes

/// PSKCallback is called once we have the remote's psk_identity_hint.
/// If the remote provided none it will be nil
pub(crate) type PskCallback = Arc<dyn (Fn(&[u8]) -> Result<Vec<u8>>) + Send + Sync>;

/// ClientAuthType declares the policy the server will follow for
/// TLS Client Authentication.
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClientAuthType {
    #[default]
    /// `NO_CLIENT_CERT` (`0`).
    NoClientCert = 0,
    /// `REQUEST_CLIENT_CERT` (`1`).
    RequestClientCert = 1,
    /// `REQUIRE_ANY_CLIENT_CERT` (`2`).
    RequireAnyClientCert = 2,
    /// `VERIFY_CLIENT_CERT_IF_GIVEN` (`3`).
    VerifyClientCertIfGiven = 3,
    /// `REQUIRE_AND_VERIFY_CLIENT_CERT` (`4`).
    RequireAndVerifyClientCert = 4,
}

// ExtendedMasterSecretType declares the policy the client and server
// will follow for the Extended Master Secret extension
#[derive(Debug, Default, PartialEq, Eq, Copy, Clone)]
/// How strictly to require the extended master secret extension ([RFC 7627]).
pub enum ExtendedMasterSecretType {
    #[default]
    /// `REQUEST` (`0`).
    Request = 0,
    /// `REQUIRE` (`1`).
    Require = 1,
    /// `DISABLE` (`2`).
    Disable = 2,
}

impl ConfigBuilder {
    fn offered_cipher_suites(&self, is_client: bool) -> Result<Vec<Box<dyn CipherSuite>>> {
        // a client offers one family or the other, while a server can only offer them if
        // it actually holds a certificate.
        let exclude_certificate = if is_client {
            self.psk.is_some()
        } else {
            self.certificates.is_empty()
        };

        parse_cipher_suites(&self.cipher_suites, self.psk.is_none(), exclude_certificate)
    }

    fn validate(&self, is_client: bool) -> Result<()> {
        if is_client && self.psk.is_some() && self.psk_identity_hint.is_none() {
            return Err(Error::ErrPskAndIdentityMustBeSetForClient);
        }

        if !is_client && self.psk.is_none() && self.certificates.is_empty() {
            return Err(Error::ErrServerMustHaveCertificate);
        }

        // A server may hold both and let each handshake pick
        if is_client && !self.certificates.is_empty() && self.psk.is_some() {
            return Err(Error::ErrPskAndCertificate);
        }

        if self.psk_identity_hint.is_some() && self.psk.is_none() {
            return Err(Error::ErrIdentityNoPsk);
        }

        self.offered_cipher_suites(is_client)?;

        Ok(())
    }

    /// build handshake config
    pub fn build(
        mut self,
        is_client: bool,
        remote_addr: Option<SocketAddr>,
    ) -> Result<HandshakeConfig> {
        // The caller supplies the provider; this crate never resolves a default. An application
        // that wants the feature-selected built-in passes `crypto::default_provider()?`.
        let crypto_provider = self.crypto_provider.take().ok_or_else(|| {
            Error::Crypto(
                "no crypto provider configured: call ConfigBuilder::with_crypto_provider"
                    .to_owned(),
            )
        })?;
        self.validate(is_client)?;

        // Each id is paired with the suite's own `is_psk`, so the checks below can tell the two
        // families apart without a second definition of which suites are psk.
        let mut local_cipher_suites: Vec<(CipherSuiteId, bool)> = self
            .offered_cipher_suites(is_client)?
            .iter()
            .map(|cs| (cs.id(), cs.is_psk()))
            .filter(|(id, _)| id.supported_by(crypto_provider.crypto()))
            .collect();
        if local_cipher_suites.is_empty() {
            return Err(Error::ErrNoAvailableCipherSuites);
        }

        let sigs: Vec<u16> = self.signature_schemes.iter().map(|x| *x as u16).collect();
        let local_signature_schemes: Vec<_> = parse_signature_schemes(&sigs, self.insecure_hashes)?
            .into_iter()
            .filter(|algorithm| {
                algorithm.crypto_scheme().is_ok_and(|scheme| {
                    crypto_provider
                        .crypto()
                        .supports(crypto::CryptoAlgorithm::Signature(scheme))
                })
            })
            .collect();

        let has_non_psk_suites = local_cipher_suites.iter().any(|(_, is_psk)| !is_psk);
        if has_non_psk_suites && local_signature_schemes.is_empty() {
            return Err(Error::ErrNoAvailableSignatureSchemes);
        }

        let local_named_curves: Vec<_> = [NamedCurve::P256, NamedCurve::X25519, NamedCurve::P384]
            .into_iter()
            .filter(|curve| {
                curve.crypto_algorithm().is_ok_and(|algorithm| {
                    crypto_provider
                        .crypto()
                        .supports(crypto::CryptoAlgorithm::KeyExchange(algorithm))
                })
            })
            .collect();
        if has_non_psk_suites && local_named_curves.is_empty() {
            return Err(Error::ErrNoAvailableCipherSuites);
        }

        if !is_client && has_non_psk_suites {
            let signing_key = &self.certificates[0].private_key.signing_key;
            local_cipher_suites.retain(|(id, is_psk)| {
                *is_psk
                    || local_signature_schemes.iter().any(|algorithm| {
                        let signature_family_matches =
                            match id {
                                CipherSuiteId::Tls_Ecdhe_Rsa_With_Aes_128_Gcm_Sha256
                                | CipherSuiteId::Tls_Ecdhe_Rsa_With_Aes_256_Cbc_Sha
                                | CipherSuiteId::Tls_Ecdhe_Rsa_With_ChaCha20_Poly1305_Sha256 => {
                                    algorithm.signature
                                        == crate::signature_hash_algorithm::SignatureAlgorithm::Rsa
                                }
                                _ => algorithm.signature
                                    == crate::signature_hash_algorithm::SignatureAlgorithm::Ecdsa,
                            };
                        signature_family_matches
                            && algorithm
                                .crypto_scheme()
                                .is_ok_and(|scheme| signing_key.supports(scheme))
                    })
            });
            // A mixed config keeps its psk suites either way, so being left with only those
            // means the certificate can sign none of the suites offered alongside them.
            let only_psk_suites_left = local_cipher_suites.iter().all(|(_, is_psk)| *is_psk);
            if only_psk_suites_left {
                return Err(Error::ErrNoAvailableCipherSuites);
            }
        }

        let local_cipher_suites: Vec<CipherSuiteId> =
            local_cipher_suites.into_iter().map(|(id, _)| id).collect();

        let retransmit_interval = if self.flight_interval != Duration::from_secs(0) {
            self.flight_interval
        } else {
            INITIAL_TICKER_INTERVAL
        };

        let maximum_transmission_unit = if self.mtu == 0 { DEFAULT_MTU } else { self.mtu };

        // `..Default::default()` used to supply this; when P7 replaced that with an explicit
        // field list it wrote `0`, which means *no retransmits are permitted at all*. The very
        // first firing of the retransmit timer then takes the handshake straight to `Errored`
        // (see `handshaker::handshake_timeout`), so a single reply arriving later than one
        // `retransmit_interval` kills the handshake instead of being retried.
        //
        // That is exactly what a loaded CI machine produces: under `-Cinstrument-coverage` on a
        // two-vCPU runner, a round trip between two in-process peers can easily exceed the 1 s
        // interval, and the connection dies with the misleading `ErrInvalidFsmTransition`.
        let maximum_retransmit_number = DEFAULT_MAXIMUM_RETRANSMIT_NUMBER;

        let replay_protection_window = if self.replay_protection_window == 0 {
            DEFAULT_REPLAY_PROTECTION_WINDOW
        } else {
            self.replay_protection_window
        };

        let mut server_name = self.server_name.clone();

        // Use host from conn address when server_name is not provided
        if is_client && server_name.is_empty() {
            if let Some(remote_addr) = remote_addr {
                server_name = remote_addr.ip().to_string();
            } else {
                warn!(
                    "conn.remote_addr is empty, please set explicitly server_name in Config! Use default \"localhost\" as server_name now"
                );
                "localhost".clone_into(&mut server_name);
            }
        }

        let server_cert_verifier = if self.insecure_skip_verify {
            None
        } else {
            let adapter = self.verifier_adapter.as_ref().ok_or_else(|| {
                Error::Crypto("CA-chain verification requires a RustlsVerifierAdapter".to_owned())
            })?;
            let roots = if self.roots_cas.is_empty() {
                gen_self_signed_root_cert()
            } else {
                self.roots_cas.clone()
            };
            Some(server_cert_verifier(Arc::new(roots), adapter)?)
        };

        let client_cert_verifier = if self.client_auth as u8
            >= ClientAuthType::VerifyClientCertIfGiven as u8
        {
            let adapter = self.verifier_adapter.as_ref().ok_or_else(|| {
                Error::Crypto(
                    "client-certificate verification requires a RustlsVerifierAdapter".to_owned(),
                )
            })?;
            Some(
                rustls::server::WebPkiClientVerifier::builder_with_provider(
                    Arc::new(self.client_cas.clone()),
                    adapter.provider.clone(),
                )
                .build()
                .map_err(|err| Error::Other(format!("rustls client cert verifier: {err}")))?
                    as Arc<dyn ClientCertVerifier>,
            )
        } else {
            None
        };

        // Fields not derived from the builder come from `HandshakeConfig::new`, which also
        // supplies `crypto_provider`.
        Ok(HandshakeConfig {
            crypto_provider,
            local_psk_callback: self.psk.take(),
            local_psk_identity_hint: self.psk_identity_hint.take(),
            local_cipher_suites,
            local_named_curves,
            local_signature_schemes,
            extended_master_secret: self.extended_master_secret,
            local_srtp_protection_profiles: self.srtp_protection_profiles,
            server_name,
            client_auth: self.client_auth,
            local_certificates: self.certificates,
            name_to_certificate: Default::default(),
            insecure_skip_verify: self.insecure_skip_verify,
            insecure_verification: self.insecure_verification,
            verify_peer_certificate: self.verify_peer_certificate.take(),
            roots_cas: self.roots_cas,
            server_cert_verifier,
            client_cert_verifier,
            retransmit_interval,
            initial_epoch: 0,
            maximum_transmission_unit,
            maximum_retransmit_number,
            replay_protection_window,
        })
    }
}

/// A callback that decides whether a peer's certificate chain is acceptable.
///
/// WebRTC verifies the fingerprint from SDP instead of a CA chain, so this is where that check
/// goes.
pub type VerifyPeerCertificateFn =
    Arc<dyn (Fn(&[Vec<u8>], &[CertificateDer<'static>]) -> Result<()>) + Send + Sync>;

/// Generates a self-signed certificate, as WebRTC endpoints use.
pub fn gen_self_signed_root_cert() -> rustls::RootCertStore {
    #[cfg(any(feature = "crypto-ring", feature = "crypto-aws-lc-rs"))]
    {
        let mut certs = rustls::RootCertStore::empty();
        certs
            .add(
                rcgen::generate_simple_self_signed(vec![])
                    .unwrap()
                    .cert
                    .der()
                    .to_owned(),
            )
            .unwrap();
        certs
    }
    #[cfg(not(any(feature = "crypto-ring", feature = "crypto-aws-lc-rs")))]
    {
        rustls::RootCertStore::empty()
    }
}

#[derive(Clone)]
/// The resolved configuration a handshake runs with, produced by [`ConfigBuilder::build`].
pub struct HandshakeConfig {
    pub(crate) crypto_provider: Arc<dyn RTCCryptoProvider>,
    pub(crate) local_psk_callback: Option<PskCallback>,
    pub(crate) local_psk_identity_hint: Option<Vec<u8>>,
    pub(crate) local_cipher_suites: Vec<CipherSuiteId>, // Available CipherSuites
    pub(crate) local_named_curves: Vec<NamedCurve>,
    pub(crate) local_signature_schemes: Vec<SignatureHashAlgorithm>, // Available signature schemes
    pub(crate) extended_master_secret: ExtendedMasterSecretType, // Policy for the Extended Master Support extension
    pub(crate) local_srtp_protection_profiles: Vec<SrtpProtectionProfile>, // Available SRTPProtectionProfiles, if empty no SRTP support
    pub(crate) server_name: String,
    pub(crate) client_auth: ClientAuthType, // If we are a client should we request a client certificate
    pub(crate) local_certificates: Vec<Certificate>,
    pub(crate) name_to_certificate: HashMap<String, Certificate>,
    pub(crate) insecure_skip_verify: bool,
    pub(crate) insecure_verification: bool,
    pub(crate) verify_peer_certificate: Option<VerifyPeerCertificateFn>,
    pub(crate) roots_cas: rustls::RootCertStore,
    pub(crate) server_cert_verifier: Option<Arc<dyn ServerCertVerifier>>,
    pub(crate) client_cert_verifier: Option<Arc<dyn ClientCertVerifier>>,
    pub(crate) retransmit_interval: Duration,
    pub(crate) initial_epoch: u16,
    pub(crate) maximum_transmission_unit: usize,
    pub(crate) maximum_retransmit_number: usize,
    pub(crate) replay_protection_window: usize,
}

impl fmt::Debug for HandshakeConfig {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("HandshakeConfig<T>")
            .field("crypto_provider", &self.crypto_provider.name())
            .field("local_psk_identity_hint", &self.local_psk_identity_hint)
            .field("local_cipher_suites", &self.local_cipher_suites)
            .field("local_named_curves", &self.local_named_curves)
            .field("local_signature_schemes", &self.local_signature_schemes)
            .field("extended_master_secret", &self.extended_master_secret)
            .field(
                "local_srtp_protection_profiles",
                &self.local_srtp_protection_profiles,
            )
            .field("server_name", &self.server_name)
            .field("client_auth", &self.client_auth)
            .field("local_certificates", &self.local_certificates)
            .field("name_to_certificate", &self.name_to_certificate)
            .field("insecure_skip_verify", &self.insecure_skip_verify)
            .field("insecure_verification", &self.insecure_verification)
            .field("roots_cas", &self.roots_cas)
            .field("retransmit_interval", &self.retransmit_interval)
            .field("initial_epoch", &self.initial_epoch)
            .field("maximum_transmission_unit", &self.maximum_transmission_unit)
            .field("maximum_retransmit_number", &self.maximum_retransmit_number)
            .field("replay_protection_window", &self.replay_protection_window)
            .finish()
    }
}

impl HandshakeConfig {
    pub(crate) fn new(crypto_provider: Arc<dyn RTCCryptoProvider>) -> Self {
        Self {
            crypto_provider,
            local_psk_callback: None,
            local_psk_identity_hint: None,
            local_cipher_suites: vec![],
            local_named_curves: vec![],
            local_signature_schemes: vec![],
            extended_master_secret: ExtendedMasterSecretType::Disable,
            local_srtp_protection_profiles: vec![],
            server_name: String::new(),
            client_auth: ClientAuthType::NoClientCert,
            local_certificates: vec![],
            name_to_certificate: HashMap::new(),
            insecure_skip_verify: false,
            insecure_verification: false,
            verify_peer_certificate: None,
            roots_cas: rustls::RootCertStore::empty(),
            server_cert_verifier: default_verifier_adapter().and_then(|adapter| {
                server_cert_verifier(Arc::new(gen_self_signed_root_cert()), &adapter).ok()
            }),
            client_cert_verifier: None,
            retransmit_interval: std::time::Duration::from_secs(0),
            initial_epoch: 0,
            maximum_transmission_unit: DEFAULT_MTU,
            maximum_retransmit_number: DEFAULT_MAXIMUM_RETRANSMIT_NUMBER,
            replay_protection_window: DEFAULT_REPLAY_PROTECTION_WINDOW,
        }
    }

    pub(crate) fn provider(&self) -> &Arc<dyn RTCCryptoProvider> {
        &self.crypto_provider
    }

    pub(crate) fn get_certificate(&self, server_name: &str) -> Result<Certificate> {
        if self.local_certificates.is_empty() {
            return Err(Error::ErrNoCertificates);
        }

        if self.local_certificates.len() == 1 {
            // There's only one choice, so no point doing any work.
            return Ok(self.local_certificates[0].clone());
        }

        if server_name.is_empty() {
            return Ok(self.local_certificates[0].clone());
        }

        let lower = server_name.to_lowercase();
        let name = lower.trim_end_matches('.');

        if let Some(cert) = self.name_to_certificate.get(name) {
            return Ok(cert.clone());
        }

        // try replacing labels in the name with wildcards until we get a
        // match.
        let mut labels: Vec<&str> = name.split_terminator('.').collect();
        for i in 0..labels.len() {
            labels[i] = "*";
            let candidate = labels.join(".");
            if let Some(cert) = self.name_to_certificate.get(&candidate) {
                return Ok(cert.clone());
            }
        }

        // If nothing matches, return the first certificate.
        Ok(self.local_certificates[0].clone())
    }
}
