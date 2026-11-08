use crypto::{
    AeadAlgorithm, BlockCipherAlgorithm, CryptoAlgorithm, HmacAlgorithm, RTCCrypto,
    StreamCipherAlgorithm,
};
use shared::error::{Error, Result};

const AES_128_CM_REQUIREMENTS: &[CryptoAlgorithm] = &[
    CryptoAlgorithm::BlockCipher(BlockCipherAlgorithm::Aes128),
    CryptoAlgorithm::StreamCipher(StreamCipherAlgorithm::Aes128Ctr),
    CryptoAlgorithm::Hmac(HmacAlgorithm::Sha1),
];
const AES_256_CM_REQUIREMENTS: &[CryptoAlgorithm] = &[
    CryptoAlgorithm::BlockCipher(BlockCipherAlgorithm::Aes256),
    CryptoAlgorithm::StreamCipher(StreamCipherAlgorithm::Aes256Ctr),
    CryptoAlgorithm::Hmac(HmacAlgorithm::Sha1),
];
const AEAD_AES_128_GCM_REQUIREMENTS: &[CryptoAlgorithm] = &[
    CryptoAlgorithm::BlockCipher(BlockCipherAlgorithm::Aes128),
    CryptoAlgorithm::Aead(AeadAlgorithm::Aes128Gcm),
];
const AEAD_AES_256_GCM_REQUIREMENTS: &[CryptoAlgorithm] = &[
    CryptoAlgorithm::BlockCipher(BlockCipherAlgorithm::Aes256),
    CryptoAlgorithm::Aead(AeadAlgorithm::Aes256Gcm),
];

/// ProtectionProfile specifies Cipher and AuthTag details, similar to TLS cipher suite
#[derive(Default, Debug, Clone, Copy)]
#[repr(u8)]
#[non_exhaustive]
pub enum ProtectionProfile {
    #[default]
    /// `SRTP_AES128_CM_HMAC_SHA1_80`: AES-128 counter mode with an 80-bit HMAC-SHA1 tag.
    ///
    /// The profile every WebRTC implementation supports.
    Aes128CmHmacSha1_80 = 0x0001,
    /// `SRTP_AES128_CM_HMAC_SHA1_32`: as above with a truncated 32-bit tag, trading
    /// authentication strength for 6 bytes per packet.
    Aes128CmHmacSha1_32 = 0x0002,
    /// `SRTP_AES256_CM_HMAC_SHA1_80`: AES-256 counter mode with an 80-bit HMAC-SHA1 tag.
    Aes256CmHmacSha1_80 = 0x0003,
    /// `SRTP_AES256_CM_HMAC_SHA1_32`: AES-256 counter mode with a 32-bit HMAC-SHA1 tag.
    Aes256CmHmacSha1_32 = 0x0004,
    /// `SRTP_AEAD_AES_128_GCM`: AES-128 in GCM, which authenticates as part of encryption
    /// rather than with a separate HMAC.
    AeadAes128Gcm = 0x0007,
    /// `SRTP_AEAD_AES_256_GCM`: AES-256 in GCM.
    AeadAes256Gcm = 0x0008,
}

impl ProtectionProfile {
    /// Returns the provider operations required to construct this protection profile.
    #[must_use]
    pub const fn required_crypto_algorithms(self) -> &'static [CryptoAlgorithm] {
        match self {
            Self::Aes128CmHmacSha1_32 | Self::Aes128CmHmacSha1_80 => AES_128_CM_REQUIREMENTS,
            Self::Aes256CmHmacSha1_32 | Self::Aes256CmHmacSha1_80 => AES_256_CM_REQUIREMENTS,
            Self::AeadAes128Gcm => AEAD_AES_128_GCM_REQUIREMENTS,
            Self::AeadAes256Gcm => AEAD_AES_256_GCM_REQUIREMENTS,
        }
    }

    /// Validates that `crypto` implements every operation required by this profile.
    pub fn ensure_crypto_supported(self, crypto: &dyn RTCCrypto) -> Result<()> {
        for algorithm in self.required_crypto_algorithms() {
            if !crypto.supports(*algorithm) {
                return Err(Error::Crypto(format!(
                    "SRTP protection profile {self:?} requires unsupported algorithm {algorithm:?}"
                )));
            }
        }
        Ok(())
    }

    /// The master key length in bytes for this profile.
    pub fn key_len(&self) -> usize {
        match *self {
            ProtectionProfile::Aes128CmHmacSha1_32
            | ProtectionProfile::Aes128CmHmacSha1_80
            | ProtectionProfile::AeadAes128Gcm => 16,
            ProtectionProfile::Aes256CmHmacSha1_32 | ProtectionProfile::Aes256CmHmacSha1_80 => 32,
            ProtectionProfile::AeadAes256Gcm => 32,
        }
    }

    /// The master salt length in bytes for this profile.
    pub fn salt_len(&self) -> usize {
        match *self {
            ProtectionProfile::Aes128CmHmacSha1_32
            | ProtectionProfile::Aes128CmHmacSha1_80
            | ProtectionProfile::Aes256CmHmacSha1_32
            | ProtectionProfile::Aes256CmHmacSha1_80 => 14,
            ProtectionProfile::AeadAes128Gcm | ProtectionProfile::AeadAes256Gcm => 12,
        }
    }

    /// The authentication tag length appended to each SRTP packet, in bytes.
    pub fn rtp_auth_tag_len(&self) -> usize {
        match *self {
            ProtectionProfile::Aes128CmHmacSha1_80 | ProtectionProfile::Aes256CmHmacSha1_80 => 10,
            ProtectionProfile::Aes128CmHmacSha1_32 | ProtectionProfile::Aes256CmHmacSha1_32 => 4,
            ProtectionProfile::AeadAes128Gcm | ProtectionProfile::AeadAes256Gcm => 0,
        }
    }

    /// The authentication tag length appended to each SRTCP packet, in bytes.
    pub fn rtcp_auth_tag_len(&self) -> usize {
        match *self {
            ProtectionProfile::Aes128CmHmacSha1_80
            | ProtectionProfile::Aes128CmHmacSha1_32
            | ProtectionProfile::Aes256CmHmacSha1_80
            | ProtectionProfile::Aes256CmHmacSha1_32 => 10,
            ProtectionProfile::AeadAes128Gcm | ProtectionProfile::AeadAes256Gcm => 0,
        }
    }

    /// The AEAD tag length in bytes, for the GCM profiles; `0` for the HMAC-SHA1 ones.
    pub fn aead_auth_tag_len(&self) -> usize {
        match *self {
            ProtectionProfile::Aes128CmHmacSha1_80
            | ProtectionProfile::Aes128CmHmacSha1_32
            | ProtectionProfile::Aes256CmHmacSha1_80
            | ProtectionProfile::Aes256CmHmacSha1_32 => 0,
            ProtectionProfile::AeadAes128Gcm | ProtectionProfile::AeadAes256Gcm => 16,
        }
    }

    /// The HMAC authentication key length in bytes; `0` for the AEAD profiles, which derive
    /// authentication from the cipher itself.
    pub fn auth_key_len(&self) -> usize {
        match *self {
            ProtectionProfile::Aes128CmHmacSha1_80
            | ProtectionProfile::Aes128CmHmacSha1_32
            | ProtectionProfile::Aes256CmHmacSha1_80
            | ProtectionProfile::Aes256CmHmacSha1_32 => 20,
            ProtectionProfile::AeadAes128Gcm | ProtectionProfile::AeadAes256Gcm => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CapabilityCrypto {
        missing: Option<CryptoAlgorithm>,
    }

    impl RTCCrypto for CapabilityCrypto {
        fn supports(&self, algorithm: CryptoAlgorithm) -> bool {
            self.missing != Some(algorithm)
        }
    }

    const PROFILES: [ProtectionProfile; 6] = [
        ProtectionProfile::Aes128CmHmacSha1_80,
        ProtectionProfile::Aes128CmHmacSha1_32,
        ProtectionProfile::Aes256CmHmacSha1_80,
        ProtectionProfile::Aes256CmHmacSha1_32,
        ProtectionProfile::AeadAes128Gcm,
        ProtectionProfile::AeadAes256Gcm,
    ];

    #[test]
    fn complete_provider_supports_every_profile() {
        let crypto = CapabilityCrypto { missing: None };
        for profile in PROFILES {
            profile.ensure_crypto_supported(&crypto).unwrap();
        }
    }

    #[test]
    fn every_required_capability_is_enforced() {
        for profile in PROFILES {
            for algorithm in profile.required_crypto_algorithms() {
                let crypto = CapabilityCrypto {
                    missing: Some(*algorithm),
                };
                let error = profile.ensure_crypto_supported(&crypto).unwrap_err();
                let message = error.to_string();
                assert!(message.contains(&format!("{profile:?}")));
                assert!(message.contains(&format!("{algorithm:?}")));
            }
        }
    }
}
