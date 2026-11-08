use std::sync::Arc;
#[cfg(feature = "crypto-ring")]
use std::sync::atomic::{AtomicUsize, Ordering};

#[cfg(feature = "crypto-ring")]
use crypto::{
    AeadAlgorithm, AeadCipher, BlockCipherAlgorithm, HmacAlgorithm, StreamCipher,
    StreamCipherAlgorithm,
};
use crypto::{CryptoAlgorithm, CryptoError, RTCCrypto, RTCCryptoProvider, RTCRandom};
use rtc_srtp::context::Context;
use rtc_srtp::option::{srtcp_replay_protection, srtp_replay_protection};
use rtc_srtp::protection_profile::ProtectionProfile;
use shared::error::Result;
use shared::marshal::Marshal;

const PROFILES: [ProtectionProfile; 6] = [
    ProtectionProfile::Aes128CmHmacSha1_80,
    ProtectionProfile::Aes128CmHmacSha1_32,
    ProtectionProfile::Aes256CmHmacSha1_80,
    ProtectionProfile::Aes256CmHmacSha1_32,
    ProtectionProfile::AeadAes128Gcm,
    ProtectionProfile::AeadAes256Gcm,
];

fn providers() -> Vec<Arc<dyn RTCCryptoProvider>> {
    #[cfg(all(feature = "crypto-ring", feature = "crypto-aws-lc-rs"))]
    return vec![
        Arc::new(crypto::providers::RingProvider::new()),
        Arc::new(crypto::providers::AwsLcRsProvider::new()),
    ];
    #[cfg(all(feature = "crypto-ring", not(feature = "crypto-aws-lc-rs")))]
    return vec![Arc::new(crypto::providers::RingProvider::new())];
    #[cfg(all(not(feature = "crypto-ring"), feature = "crypto-aws-lc-rs"))]
    return vec![Arc::new(crypto::providers::AwsLcRsProvider::new())];
    #[cfg(not(any(feature = "crypto-ring", feature = "crypto-aws-lc-rs")))]
    Vec::new()
}

struct IncompleteCrypto;

impl RTCCrypto for IncompleteCrypto {
    fn supports(&self, _algorithm: CryptoAlgorithm) -> bool {
        false
    }
}

struct IncompleteRandom;

impl RTCRandom for IncompleteRandom {
    fn fill(&self, _output: &mut [u8]) -> std::result::Result<(), CryptoError> {
        Err(CryptoError::RandomnessFailed)
    }
}

struct IncompleteProvider {
    crypto: IncompleteCrypto,
    random: IncompleteRandom,
}

impl RTCCryptoProvider for IncompleteProvider {
    fn name(&self) -> &'static str {
        "incomplete"
    }

    fn crypto(&self) -> &dyn RTCCrypto {
        &self.crypto
    }

    fn random(&self) -> &dyn RTCRandom {
        &self.random
    }
}

#[test]
fn explicit_incomplete_provider_returns_actionable_capability_error() {
    let profile = ProtectionProfile::Aes128CmHmacSha1_80;
    let (key, salt) = key_material(profile);
    let provider = IncompleteProvider {
        crypto: IncompleteCrypto,
        random: IncompleteRandom,
    };
    let error = Context::new(&key, &salt, profile, None, None, provider.crypto())
        .err()
        .expect("an incomplete provider must be rejected");
    let message = error.to_string();
    assert!(message.contains("Aes128CmHmacSha1_80"));
    assert!(message.contains("BlockCipher(Aes128)"));
}

fn key_material(profile: ProtectionProfile) -> (Vec<u8>, Vec<u8>) {
    (
        (0..profile.key_len()).map(|index| index as u8).collect(),
        (0..profile.salt_len())
            .map(|index| 0x80 | index as u8)
            .collect(),
    )
}

fn context(
    profile: ProtectionProfile,
    provider: Arc<dyn RTCCryptoProvider>,
    replay: bool,
) -> Result<Context> {
    let (key, salt) = key_material(profile);
    Context::new(
        &key,
        &salt,
        profile,
        replay.then(|| srtp_replay_protection(64)),
        replay.then(|| srtcp_replay_protection(64)),
        provider.crypto(),
    )
}

fn rtp_packet(sequence_number: u16) -> Result<Vec<u8>> {
    Ok(rtp::Packet {
        header: rtp::Header {
            version: 2,
            sequence_number,
            timestamp: 0x1234_5678,
            ssrc: 0x1122_3344,
            ..Default::default()
        },
        payload: vec![0x41; 48].into(),
    }
    .marshal()?
    .to_vec())
}

fn rtcp_packet() -> [u8; 8] {
    [0x80, 200, 0, 1, 0x11, 0x22, 0x33, 0x44]
}

#[test]
fn every_profile_round_trips_with_every_enabled_provider() -> Result<()> {
    for provider in providers() {
        for profile in PROFILES {
            let mut sender = context(profile, provider.clone(), false)?;
            let mut receiver = context(profile, provider.clone(), false)?;

            let rtp = rtp_packet(7)?;
            let protected_rtp = sender.encrypt_rtp(&rtp)?;
            assert_eq!(receiver.decrypt_rtp(&protected_rtp)?.as_ref(), rtp);

            if profile.aead_auth_tag_len() == 0 {
                let mut wrong_tag = protected_rtp.to_vec();
                let last = wrong_tag.len() - 1;
                wrong_tag[last] ^= 1;
                assert!(
                    context(profile, provider.clone(), false)?
                        .decrypt_rtp(&wrong_tag)
                        .is_err()
                );
            }

            let rtcp = rtcp_packet();
            let protected_rtcp = sender.encrypt_rtcp(&rtcp)?;
            assert_eq!(receiver.decrypt_rtcp(&protected_rtcp)?.as_ref(), rtcp);
            if profile.aead_auth_tag_len() == 0 {
                let mut wrong_tag = protected_rtcp.to_vec();
                let last = wrong_tag.len() - 1;
                wrong_tag[last] ^= 1;
                assert!(
                    context(profile, provider.clone(), false)?
                        .decrypt_rtcp(&wrong_tag)
                        .is_err()
                );
            }
        }
    }
    Ok(())
}

#[test]
fn aead_profiles_reject_wrong_aad_tag_rollover_and_replay() -> Result<()> {
    for provider in providers() {
        for profile in [
            ProtectionProfile::AeadAes128Gcm,
            ProtectionProfile::AeadAes256Gcm,
        ] {
            let mut sender = context(profile, provider.clone(), false)?;
            let first = rtp_packet(u16::MAX)?;
            let first_protected = sender.encrypt_rtp(&first)?;
            let wrapped = rtp_packet(0)?;
            let wrapped_protected = sender.encrypt_rtp(&wrapped)?;

            let mut receiver = context(profile, provider.clone(), true)?;
            assert_eq!(receiver.decrypt_rtp(&first_protected)?.as_ref(), first);
            assert_eq!(receiver.decrypt_rtp(&wrapped_protected)?.as_ref(), wrapped);
            assert!(receiver.decrypt_rtp(&wrapped_protected).is_err());

            let mut wrong_rollover = context(profile, provider.clone(), false)?;
            assert!(wrong_rollover.decrypt_rtp(&wrapped_protected).is_err());

            let mut wrong_aad = first_protected.to_vec();
            wrong_aad[8] ^= 1;
            assert!(
                context(profile, provider.clone(), false)?
                    .decrypt_rtp(&wrong_aad)
                    .is_err()
            );

            let mut wrong_tag = first_protected.to_vec();
            let last = wrong_tag.len() - 1;
            wrong_tag[last] ^= 1;
            assert!(
                context(profile, provider.clone(), false)?
                    .decrypt_rtp(&wrong_tag)
                    .is_err()
            );

            let rtcp = rtcp_packet();
            let mut rtcp_sender = context(profile, provider.clone(), false)?;
            let protected_rtcp = rtcp_sender.encrypt_rtcp(&rtcp)?;
            let mut rtcp_receiver = context(profile, provider.clone(), true)?;
            assert_eq!(rtcp_receiver.decrypt_rtcp(&protected_rtcp)?.as_ref(), rtcp);
            assert!(rtcp_receiver.decrypt_rtcp(&protected_rtcp).is_err());

            let mut wrong_rtcp_aad = protected_rtcp.to_vec();
            wrong_rtcp_aad[4] ^= 1;
            assert!(
                context(profile, provider.clone(), false)?
                    .decrypt_rtcp(&wrong_rtcp_aad)
                    .is_err()
            );

            let mut wrong_rtcp_tag = protected_rtcp.to_vec();
            let tag_byte = wrong_rtcp_tag.len() - 5;
            wrong_rtcp_tag[tag_byte] ^= 1;
            assert!(
                context(profile, provider.clone(), false)?
                    .decrypt_rtcp(&wrong_rtcp_tag)
                    .is_err()
            );
        }
    }
    Ok(())
}

#[cfg(all(feature = "crypto-ring", feature = "crypto-aws-lc-rs"))]
#[test]
fn providers_produce_identical_packets_and_interoperate() -> Result<()> {
    let ring: Arc<dyn RTCCryptoProvider> = Arc::new(crypto::providers::RingProvider::new());
    let aws: Arc<dyn RTCCryptoProvider> = Arc::new(crypto::providers::AwsLcRsProvider::new());

    for profile in PROFILES {
        let rtp = rtp_packet(42)?;
        let mut ring_sender = context(profile, ring.clone(), false)?;
        let mut aws_sender = context(profile, aws.clone(), false)?;
        let ring_packet = ring_sender.encrypt_rtp(&rtp)?;
        let aws_packet = aws_sender.encrypt_rtp(&rtp)?;
        assert_eq!(ring_packet, aws_packet);

        let mut ring_receiver = context(profile, ring.clone(), false)?;
        let mut aws_receiver = context(profile, aws.clone(), false)?;
        assert_eq!(ring_receiver.decrypt_rtp(&aws_packet)?.as_ref(), rtp);
        assert_eq!(aws_receiver.decrypt_rtp(&ring_packet)?.as_ref(), rtp);
    }
    Ok(())
}

#[cfg(feature = "crypto-ring")]
struct CountingCrypto {
    inner: crypto::providers::RingCrypto,
    stream_constructions: AtomicUsize,
    aead_constructions: AtomicUsize,
    mac_constructions: AtomicUsize,
}

#[cfg(feature = "crypto-ring")]
impl RTCCrypto for CountingCrypto {
    fn supports(&self, algorithm: CryptoAlgorithm) -> bool {
        self.inner.supports(algorithm)
    }

    fn block_encrypt(
        &self,
        algorithm: BlockCipherAlgorithm,
        key: &[u8],
        block: &mut [u8],
    ) -> std::result::Result<(), CryptoError> {
        self.inner.block_encrypt(algorithm, key, block)
    }

    fn new_stream_cipher(
        &self,
        algorithm: StreamCipherAlgorithm,
        key: &[u8],
    ) -> std::result::Result<Box<dyn StreamCipher>, CryptoError> {
        self.stream_constructions.fetch_add(1, Ordering::Relaxed);
        self.inner.new_stream_cipher(algorithm, key)
    }

    fn new_aead(
        &self,
        algorithm: AeadAlgorithm,
        key: &[u8],
    ) -> std::result::Result<Box<dyn AeadCipher>, CryptoError> {
        self.aead_constructions.fetch_add(1, Ordering::Relaxed);
        self.inner.new_aead(algorithm, key)
    }

    fn new_hmac(
        &self,
        algorithm: HmacAlgorithm,
        key: &[u8],
    ) -> std::result::Result<Box<dyn crypto::Mac>, CryptoError> {
        self.mac_constructions.fetch_add(1, Ordering::Relaxed);
        self.inner.new_hmac(algorithm, key)
    }
}

#[cfg(feature = "crypto-ring")]
struct CountingProvider {
    crypto: CountingCrypto,
    random: crypto::providers::RingRandom,
}

#[cfg(feature = "crypto-ring")]
impl CountingProvider {
    fn new() -> Self {
        Self {
            crypto: CountingCrypto {
                inner: crypto::providers::RingCrypto,
                stream_constructions: AtomicUsize::new(0),
                aead_constructions: AtomicUsize::new(0),
                mac_constructions: AtomicUsize::new(0),
            },
            random: crypto::providers::RingRandom,
        }
    }
}

#[cfg(feature = "crypto-ring")]
impl RTCCryptoProvider for CountingProvider {
    fn name(&self) -> &'static str {
        "counting-ring"
    }

    fn crypto(&self) -> &dyn RTCCrypto {
        &self.crypto
    }

    fn random(&self) -> &dyn RTCRandom {
        &self.random
    }
}

#[cfg(feature = "crypto-ring")]
#[test]
fn keyed_ciphers_are_constructed_once_per_context_not_per_packet() -> Result<()> {
    let stream_provider = Arc::new(CountingProvider::new());
    let mut stream_context = context(
        ProtectionProfile::Aes128CmHmacSha1_80,
        stream_provider.clone(),
        false,
    )?;
    assert_eq!(
        stream_provider
            .crypto
            .stream_constructions
            .load(Ordering::Relaxed),
        2
    );
    // Two MACs (SRTP and SRTCP), keyed once alongside the ciphers.
    assert_eq!(
        stream_provider
            .crypto
            .mac_constructions
            .load(Ordering::Relaxed),
        2
    );
    for sequence_number in 0..4 {
        stream_context.encrypt_rtp(&rtp_packet(sequence_number)?)?;
    }
    assert_eq!(
        stream_provider
            .crypto
            .stream_constructions
            .load(Ordering::Relaxed),
        2
    );
    assert_eq!(
        stream_provider
            .crypto
            .mac_constructions
            .load(Ordering::Relaxed),
        2,
        "MACs must be keyed once per context, not per packet"
    );

    let aead_provider = Arc::new(CountingProvider::new());
    let mut aead_context = context(
        ProtectionProfile::AeadAes128Gcm,
        aead_provider.clone(),
        false,
    )?;
    assert_eq!(
        aead_provider
            .crypto
            .aead_constructions
            .load(Ordering::Relaxed),
        2
    );
    for sequence_number in 0..4 {
        aead_context.encrypt_rtp(&rtp_packet(sequence_number)?)?;
    }
    assert_eq!(
        aead_provider
            .crypto
            .aead_constructions
            .load(Ordering::Relaxed),
        2
    );
    Ok(())
}
