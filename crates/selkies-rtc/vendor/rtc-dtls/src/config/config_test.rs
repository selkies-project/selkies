use super::*;
use shared::error::Result;

struct IncompleteProvider;

impl crypto::RTCCryptoProvider for IncompleteProvider {
    fn name(&self) -> &'static str {
        "incomplete"
    }

    fn crypto(&self) -> &dyn crypto::RTCCrypto {
        self
    }

    fn random(&self) -> &dyn crypto::RTCRandom {
        self
    }
}

impl crypto::RTCCrypto for IncompleteProvider {
    fn supports(&self, _algorithm: crypto::CryptoAlgorithm) -> bool {
        false
    }
}

impl crypto::RTCRandom for IncompleteProvider {
    fn fill(&self, _output: &mut [u8]) -> std::result::Result<(), crypto::CryptoError> {
        Err(crypto::CryptoError::RandomnessFailed)
    }
}

#[derive(Debug)]
struct MockSigner;

impl crypto::SigningKey for MockSigner {
    fn supports(&self, _scheme: crypto::SignatureScheme) -> bool {
        true
    }

    fn public_key(&self) -> crypto::PublicKey<'_> {
        crypto::PublicKey {
            encoding: crypto::PublicKeyEncoding::SubjectPublicKeyInfoDer,
            bytes: &[],
        }
    }

    fn sign(
        &self,
        _scheme: crypto::SignatureScheme,
        _message: &[u8],
    ) -> std::result::Result<Vec<u8>, crypto::CryptoError> {
        Ok(vec![])
    }
}

#[test]
fn test_config_accepts_external_signing_key() -> Result<()> {
    let cert = Certificate {
        certificate: vec![],
        private_key: CryptoPrivateKey::from_signing_key(std::sync::Arc::new(MockSigner)),
    };

    let handshake = ConfigBuilder::default()
        .with_crypto_provider(crypto::default_provider().map_err(crypto_error)?)
        .with_certificates(vec![cert])
        .build(false, None)?;

    assert!(
        handshake.local_certificates[0]
            .private_key
            .signing_key
            .supports(crypto::SignatureScheme::EcdsaP256Sha256)
    );

    Ok(())
}

#[test]
fn test_config_rejects_incomplete_provider() {
    let result = ConfigBuilder::default()
        .with_crypto_provider(Arc::new(IncompleteProvider))
        .build(true, None);

    assert!(matches!(result, Err(Error::ErrNoAvailableCipherSuites)));
}

#[cfg(feature = "crypto-ring")]
#[test]
fn test_config_accepts_ring_provider() -> Result<()> {
    let handshake = ConfigBuilder::default()
        .with_crypto_provider(Arc::new(crypto::providers::RingProvider::new()))
        .build(true, None)?;

    assert_eq!(handshake.provider().name(), "ring");
    Ok(())
}

#[cfg(feature = "crypto-aws-lc-rs")]
#[test]
fn test_config_accepts_aws_lc_rs_provider() -> Result<()> {
    let handshake = ConfigBuilder::default()
        .with_crypto_provider(Arc::new(crypto::providers::AwsLcRsProvider::new()))
        .build(true, None)?;

    assert_eq!(handshake.provider().name(), "aws-lc-rs");
    Ok(())
}

/// A built handshake config must permit retransmissions.
///
/// `maximum_retransmit_number: 0` means the *first* firing of the retransmit timer takes the
/// handshake straight to `HandshakeState::Errored`, which surfaces as the thoroughly misleading
/// `ErrInvalidFsmTransition` — so a single reply slower than one `retransmit_interval` kills the
/// connection instead of being retried. `ConfigBuilder::build` shipped exactly that between P7
/// (which replaced `..Default::default()` with an explicit field list) and the fix, and it only
/// showed up as flaky DTLS failures on loaded CI machines.
#[test]
fn test_config_build_permits_retransmissions() -> Result<()> {
    let handshake = ConfigBuilder::default()
        .with_crypto_provider(crypto::default_provider().map_err(crypto_error)?)
        .build(true, None)?;

    assert!(
        handshake.maximum_retransmit_number > 0,
        "a handshake that cannot retransmit fails on the first slow reply"
    );
    assert_eq!(
        handshake.maximum_retransmit_number,
        HandshakeConfig::new(crypto::default_provider().map_err(crypto_error)?)
            .maximum_retransmit_number,
        "the builder must agree with `HandshakeConfig::new`, which it silently diverged from"
    );

    Ok(())
}

/// A client still presents one credential, so psk and certificates remain exclusive for it.
///
/// Only the server side was relaxed to hold both. The identity hint is set so the build gets past
/// the earlier `ErrPskAndIdentityMustBeSetForClient` check and reaches this one.
#[test]
fn test_client_rejects_psk_and_certificate_but_server_accepts_both() -> Result<()> {
    let provider = crypto::default_provider().map_err(crypto_error)?;
    let with_both = || {
        ConfigBuilder::default()
            .with_crypto_provider(provider.clone())
            .with_cipher_suites(vec![
                CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Gcm_Sha256,
                CipherSuiteId::Tls_Psk_With_Aes_128_Ccm_8,
            ])
            .with_psk(Some(Arc::new(|_| Ok(vec![0xab, 0xcd, 0xef]))))
            .with_psk_identity_hint(Some(b"rtc-dtls-test".to_vec()))
            .with_certificates(vec![
                Certificate::generate_self_signed(vec!["localhost".to_owned()], provider.crypto())
                    .expect("self-signed certificate"),
            ])
    };

    assert!(matches!(
        with_both().build(true, None),
        Err(Error::ErrPskAndCertificate)
    ));

    let server = with_both().build(false, None)?;
    assert_eq!(
        server.local_cipher_suites,
        vec![
            CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Gcm_Sha256,
            CipherSuiteId::Tls_Psk_With_Aes_128_Ccm_8,
        ],
        "the same credentials a client is rejected for let a server offer both families"
    );

    Ok(())
}
