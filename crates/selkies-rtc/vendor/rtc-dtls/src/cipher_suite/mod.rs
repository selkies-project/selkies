/// Shared implementation for the AES-128-CCM suites.
pub mod cipher_suite_aes_128_ccm;
/// Shared implementation for the AES-128-GCM-SHA256 suites.
pub mod cipher_suite_aes_128_gcm_sha256;
/// Shared implementation for the AES-256-CBC-SHA suites.
pub mod cipher_suite_aes_256_cbc_sha;
/// Shared implementation for the ChaCha20-Poly1305-SHA256 suites.
pub mod cipher_suite_chacha20_poly1305_sha256;
/// An ECDHE-ECDSA suite with AES-128, the pairing WebRTC normally negotiates.
/// An ECDHE-ECDSA suite with AES-128-CCM and a truncated 8-byte tag.
pub mod cipher_suite_tls_ecdhe_ecdsa_with_aes_128_ccm;
/// An ECDHE-ECDSA suite with AES-128-CCM and a full 16-byte tag.
pub mod cipher_suite_tls_ecdhe_ecdsa_with_aes_128_ccm8;
/// `TLS_PSK_WITH_AES_128_CCM`, for pre-shared-key handshakes.
pub mod cipher_suite_tls_psk_with_aes_128_ccm;
/// `TLS_PSK_WITH_AES_128_CCM_8`, with a truncated 8-byte tag.
pub mod cipher_suite_tls_psk_with_aes_128_ccm8;
/// `TLS_PSK_WITH_AES_128_GCM_SHA256`, for pre-shared-key handshakes.
pub mod cipher_suite_tls_psk_with_aes_128_gcm_sha256;

use crypto::RTCCryptoProvider;
use crypto::{
    AeadAlgorithm, CbcAlgorithm, CryptoAlgorithm, HashAlgorithm, HmacAlgorithm, RTCCrypto,
};
use std::fmt;
use std::sync::Arc;

use super::client_certificate_type::*;
use super::record_layer::record_layer_header::*;
use shared::error::*;

use cipher_suite_aes_128_gcm_sha256::*;
use cipher_suite_aes_256_cbc_sha::*;
use cipher_suite_chacha20_poly1305_sha256::*;
use cipher_suite_tls_ecdhe_ecdsa_with_aes_128_ccm::*;
use cipher_suite_tls_ecdhe_ecdsa_with_aes_128_ccm8::*;
use cipher_suite_tls_psk_with_aes_128_ccm::*;
use cipher_suite_tls_psk_with_aes_128_ccm8::*;
use cipher_suite_tls_psk_with_aes_128_gcm_sha256::*;

// CipherSuiteID is an ID for our supported CipherSuites
// Supported Cipher Suites
#[allow(non_camel_case_types)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
/// The cipher suites this crate can negotiate, by their IANA code points.
#[non_exhaustive]
pub enum CipherSuiteId {
    // AES-128-CCM
    /// `TLS_ECDHE_ECDSA_WITH_AES_128_CCM` (`0xc0ac`).
    Tls_Ecdhe_Ecdsa_With_Aes_128_Ccm = 0xc0ac,
    /// `TLS_ECDHE_ECDSA_WITH_AES_128_CCM_8` (`0xc0ae`).
    Tls_Ecdhe_Ecdsa_With_Aes_128_Ccm_8 = 0xc0ae,

    // AES-128-GCM-SHA256
    /// `TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256` (`0xc02b`).
    Tls_Ecdhe_Ecdsa_With_Aes_128_Gcm_Sha256 = 0xc02b,
    /// `TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256` (`0xc02f`).
    Tls_Ecdhe_Rsa_With_Aes_128_Gcm_Sha256 = 0xc02f,

    // AES-256-CBC-SHA
    /// `TLS_ECDHE_ECDSA_WITH_AES_256_CBC_SHA` (`0xc00a`).
    Tls_Ecdhe_Ecdsa_With_Aes_256_Cbc_Sha = 0xc00a,
    /// `TLS_ECDHE_RSA_WITH_AES_256_CBC_SHA` (`0xc014`).
    Tls_Ecdhe_Rsa_With_Aes_256_Cbc_Sha = 0xc014,

    /// `TLS_PSK_WITH_AES_128_CCM` (`0xc0a4`).
    Tls_Psk_With_Aes_128_Ccm = 0xc0a4,
    /// `TLS_PSK_WITH_AES_128_CCM_8` (`0xc0a8`).
    Tls_Psk_With_Aes_128_Ccm_8 = 0xc0a8,
    /// `TLS_PSK_WITH_AES_128_GCM_SHA256` (`0x00a8`).
    Tls_Psk_With_Aes_128_Gcm_Sha256 = 0x00a8,

    // CHACHA20_POLY1305_SHA256
    /// `TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256` (`0xcca8`).
    Tls_Ecdhe_Rsa_With_ChaCha20_Poly1305_Sha256 = 0xcca8,
    /// `TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256` (`0xcca9`).
    Tls_Ecdhe_Ecdsa_With_ChaCha20_Poly1305_Sha256 = 0xcca9,

    /// A code point this crate does not implement.
    Unsupported,
}

impl fmt::Display for CipherSuiteId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Ccm => {
                write!(f, "TLS_ECDHE_ECDSA_WITH_AES_128_CCM")
            }
            CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Ccm_8 => {
                write!(f, "TLS_ECDHE_ECDSA_WITH_AES_128_CCM_8")
            }
            CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Gcm_Sha256 => {
                write!(f, "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256")
            }
            CipherSuiteId::Tls_Ecdhe_Rsa_With_Aes_128_Gcm_Sha256 => {
                write!(f, "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256")
            }
            CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_256_Cbc_Sha => {
                write!(f, "TLS_ECDHE_ECDSA_WITH_AES_256_CBC_SHA")
            }
            CipherSuiteId::Tls_Ecdhe_Rsa_With_Aes_256_Cbc_Sha => {
                write!(f, "TLS_ECDHE_RSA_WITH_AES_256_CBC_SHA")
            }
            CipherSuiteId::Tls_Psk_With_Aes_128_Ccm => write!(f, "TLS_PSK_WITH_AES_128_CCM"),
            CipherSuiteId::Tls_Psk_With_Aes_128_Ccm_8 => write!(f, "TLS_PSK_WITH_AES_128_CCM_8"),
            CipherSuiteId::Tls_Psk_With_Aes_128_Gcm_Sha256 => {
                write!(f, "TLS_PSK_WITH_AES_128_GCM_SHA256")
            }
            CipherSuiteId::Tls_Ecdhe_Rsa_With_ChaCha20_Poly1305_Sha256 => {
                write!(f, "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256")
            }
            CipherSuiteId::Tls_Ecdhe_Ecdsa_With_ChaCha20_Poly1305_Sha256 => {
                write!(f, "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256")
            }

            _ => write!(f, "Unsupported CipherSuiteID"),
        }
    }
}

impl From<u16> for CipherSuiteId {
    fn from(val: u16) -> Self {
        match val {
            // AES-128-CCM
            0xc0ac => CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Ccm,
            0xc0ae => CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Ccm_8,

            // AES-128-GCM-SHA256
            0xc02b => CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Gcm_Sha256,
            0xc02f => CipherSuiteId::Tls_Ecdhe_Rsa_With_Aes_128_Gcm_Sha256,

            // AES-256-CBC-SHA
            0xc00a => CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_256_Cbc_Sha,
            0xc014 => CipherSuiteId::Tls_Ecdhe_Rsa_With_Aes_256_Cbc_Sha,

            0xc0a4 => CipherSuiteId::Tls_Psk_With_Aes_128_Ccm,
            0xc0a8 => CipherSuiteId::Tls_Psk_With_Aes_128_Ccm_8,
            0x00a8 => CipherSuiteId::Tls_Psk_With_Aes_128_Gcm_Sha256,

            // CHACHA20_POLY1305_SHA256
            0xcca8 => CipherSuiteId::Tls_Ecdhe_Rsa_With_ChaCha20_Poly1305_Sha256,
            0xcca9 => CipherSuiteId::Tls_Ecdhe_Ecdsa_With_ChaCha20_Poly1305_Sha256,

            _ => CipherSuiteId::Unsupported,
        }
    }
}

impl From<&str> for CipherSuiteId {
    fn from(val: &str) -> Self {
        match val {
            "TLS_ECDHE_ECDSA_WITH_AES_128_CCM" => CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Ccm,
            "TLS_ECDHE_ECDSA_WITH_AES_128_CCM_8" => {
                CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Ccm_8
            }
            "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256" => {
                CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Gcm_Sha256
            }
            "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256" => {
                CipherSuiteId::Tls_Ecdhe_Rsa_With_Aes_128_Gcm_Sha256
            }
            "TLS_ECDHE_ECDSA_WITH_AES_256_CBC_SHA" => {
                CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_256_Cbc_Sha
            }
            "TLS_ECDHE_RSA_WITH_AES_256_CBC_SHA" => {
                CipherSuiteId::Tls_Ecdhe_Rsa_With_Aes_256_Cbc_Sha
            }
            "TLS_PSK_WITH_AES_128_CCM" => CipherSuiteId::Tls_Psk_With_Aes_128_Ccm,
            "TLS_PSK_WITH_AES_128_CCM_8" => CipherSuiteId::Tls_Psk_With_Aes_128_Ccm_8,
            "TLS_PSK_WITH_AES_128_GCM_SHA256" => CipherSuiteId::Tls_Psk_With_Aes_128_Gcm_Sha256,
            "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256" => {
                CipherSuiteId::Tls_Ecdhe_Rsa_With_ChaCha20_Poly1305_Sha256
            }
            "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256" => {
                CipherSuiteId::Tls_Ecdhe_Ecdsa_With_ChaCha20_Poly1305_Sha256
            }
            _ => CipherSuiteId::Unsupported,
        }
    }
}

impl CipherSuiteId {
    pub(crate) fn supported_by(self, crypto: &dyn RTCCrypto) -> bool {
        let record_algorithm = match self {
            Self::Tls_Ecdhe_Ecdsa_With_Aes_128_Ccm | Self::Tls_Psk_With_Aes_128_Ccm => {
                CryptoAlgorithm::Aead(AeadAlgorithm::Aes128Ccm)
            }
            Self::Tls_Ecdhe_Ecdsa_With_Aes_128_Ccm_8 | Self::Tls_Psk_With_Aes_128_Ccm_8 => {
                CryptoAlgorithm::Aead(AeadAlgorithm::Aes128Ccm8)
            }
            Self::Tls_Ecdhe_Ecdsa_With_Aes_128_Gcm_Sha256
            | Self::Tls_Ecdhe_Rsa_With_Aes_128_Gcm_Sha256
            | Self::Tls_Psk_With_Aes_128_Gcm_Sha256 => {
                CryptoAlgorithm::Aead(AeadAlgorithm::Aes128Gcm)
            }
            Self::Tls_Ecdhe_Ecdsa_With_Aes_256_Cbc_Sha
            | Self::Tls_Ecdhe_Rsa_With_Aes_256_Cbc_Sha => {
                CryptoAlgorithm::Cbc(CbcAlgorithm::Aes256Cbc)
            }
            Self::Tls_Ecdhe_Ecdsa_With_ChaCha20_Poly1305_Sha256
            | Self::Tls_Ecdhe_Rsa_With_ChaCha20_Poly1305_Sha256 => {
                CryptoAlgorithm::Aead(AeadAlgorithm::ChaCha20Poly1305)
            }
            Self::Unsupported => return false,
        };
        crypto.supports(CryptoAlgorithm::Hash(HashAlgorithm::Sha256))
            && crypto.supports(CryptoAlgorithm::Hmac(HmacAlgorithm::Sha256))
            && crypto.supports(record_algorithm)
            && (!matches!(record_algorithm, CryptoAlgorithm::Cbc(_))
                || crypto.supports(CryptoAlgorithm::Hmac(HmacAlgorithm::Sha1)))
    }
}

#[derive(Copy, Clone, Debug)]
/// The hash a suite uses in its PRF and `Finished` computation.
#[non_exhaustive]
pub enum CipherSuiteHash {
    /// SHA-256.
    Sha256,
}

impl CipherSuiteHash {
    pub(crate) fn size(&self) -> usize {
        match *self {
            CipherSuiteHash::Sha256 => 32,
        }
    }
}

/// A negotiated cipher suite: its identity, and the record encryption it performs once keys
/// are installed.
pub trait CipherSuite: Send {
    /// The suite's IANA name.
    fn to_string(&self) -> String;
    /// The suite's code point.
    fn id(&self) -> CipherSuiteId;
    /// The certificate type this suite requires of a peer.
    fn certificate_type(&self) -> ClientCertificateType;
    /// The hash used for the PRF and `Finished`.
    fn hash_func(&self) -> CipherSuiteHash;
    /// Whether this suite authenticates with a pre-shared key rather than certificates.
    fn is_psk(&self) -> bool;
    /// Whether keys have been installed, so records can be protected.
    fn is_initialized(&self) -> bool;

    // Generate the internal encryption state
    /// Installs the keying material derived from the handshake.
    ///
    /// # Errors
    ///
    /// Fails if the key or salt lengths do not match what this suite expects.
    fn init(
        &mut self,
        provider: Arc<dyn RTCCryptoProvider>,
        master_secret: &[u8],
        client_random: &[u8],
        server_random: &[u8],
        is_client: bool,
    ) -> Result<()>;

    /// Protects one record, returning the encrypted record including its header.
    ///
    /// # Errors
    ///
    /// Fails if keys are not installed, or the cipher rejects the input.
    fn encrypt(&mut self, pkt_rlh: &RecordLayerHeader, raw: &[u8]) -> Result<Vec<u8>>;
    /// Unprotects one record.
    ///
    /// # Errors
    ///
    /// Fails if authentication fails, or the record is malformed.
    fn decrypt(&mut self, input: &[u8]) -> Result<Vec<u8>>;
}

// Taken from https://www.iana.org/assignments/tls-parameters/tls-parameters.xml
// A cipher_suite is a specific combination of key agreement, cipher and MAC
// function.
/// Builds the [`CipherSuite`] implementation for `id`.
///
/// # Errors
///
/// Fails if the id is not one this crate implements.
pub fn cipher_suite_for_id(id: CipherSuiteId) -> Result<Box<dyn CipherSuite>> {
    match id {
        CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Ccm => {
            Ok(Box::new(new_cipher_suite_tls_ecdhe_ecdsa_with_aes_128_ccm()))
        }
        CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Ccm_8 => Ok(Box::new(
            new_cipher_suite_tls_ecdhe_ecdsa_with_aes_128_ccm8(),
        )),
        CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Gcm_Sha256 => {
            Ok(Box::new(CipherSuiteAes128GcmSha256::new(false)))
        }
        CipherSuiteId::Tls_Ecdhe_Rsa_With_Aes_128_Gcm_Sha256 => {
            Ok(Box::new(CipherSuiteAes128GcmSha256::new(true)))
        }
        CipherSuiteId::Tls_Ecdhe_Rsa_With_Aes_256_Cbc_Sha => {
            Ok(Box::new(CipherSuiteAes256CbcSha::new(true)))
        }
        CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_256_Cbc_Sha => {
            Ok(Box::new(CipherSuiteAes256CbcSha::new(false)))
        }
        CipherSuiteId::Tls_Psk_With_Aes_128_Ccm => {
            Ok(Box::new(new_cipher_suite_tls_psk_with_aes_128_ccm()))
        }
        CipherSuiteId::Tls_Psk_With_Aes_128_Ccm_8 => {
            Ok(Box::new(new_cipher_suite_tls_psk_with_aes_128_ccm8()))
        }
        CipherSuiteId::Tls_Psk_With_Aes_128_Gcm_Sha256 => {
            Ok(Box::<CipherSuiteTlsPskWithAes128GcmSha256>::default())
        }
        CipherSuiteId::Tls_Ecdhe_Rsa_With_ChaCha20_Poly1305_Sha256 => {
            Ok(Box::new(CipherSuiteChaCha20Poly1305Sha256::new(true)))
        }
        CipherSuiteId::Tls_Ecdhe_Ecdsa_With_ChaCha20_Poly1305_Sha256 => {
            Ok(Box::new(CipherSuiteChaCha20Poly1305Sha256::new(false)))
        }

        _ => Err(Error::ErrInvalidCipherSuite),
    }
}

// CipherSuites we support in order of preference
pub(crate) fn default_cipher_suites() -> Vec<Box<dyn CipherSuite>> {
    vec![
        Box::new(CipherSuiteAes128GcmSha256::new(false)),
        Box::new(CipherSuiteAes256CbcSha::new(false)),
        Box::new(CipherSuiteAes128GcmSha256::new(true)),
        Box::new(CipherSuiteAes256CbcSha::new(true)),
        Box::new(CipherSuiteChaCha20Poly1305Sha256::new(false)),
    ]
}

fn all_cipher_suites() -> Vec<Box<dyn CipherSuite>> {
    vec![
        Box::new(new_cipher_suite_tls_ecdhe_ecdsa_with_aes_128_ccm()),
        Box::new(new_cipher_suite_tls_ecdhe_ecdsa_with_aes_128_ccm8()),
        Box::new(CipherSuiteAes128GcmSha256::new(false)),
        Box::new(CipherSuiteAes128GcmSha256::new(true)),
        Box::new(CipherSuiteAes256CbcSha::new(false)),
        Box::new(CipherSuiteAes256CbcSha::new(true)),
        Box::new(new_cipher_suite_tls_psk_with_aes_128_ccm()),
        Box::new(new_cipher_suite_tls_psk_with_aes_128_ccm8()),
        Box::<CipherSuiteTlsPskWithAes128GcmSha256>::default(),
        Box::new(CipherSuiteChaCha20Poly1305Sha256::new(false)),
        Box::new(CipherSuiteChaCha20Poly1305Sha256::new(true)),
    ]
}

fn cipher_suites_for_ids(ids: &[CipherSuiteId]) -> Result<Vec<Box<dyn CipherSuite>>> {
    let mut cipher_suites = vec![];
    for id in ids {
        cipher_suites.push(cipher_suite_for_id(*id)?);
    }
    Ok(cipher_suites)
}

pub(crate) fn parse_cipher_suites(
    user_selected_suites: &[CipherSuiteId],
    exclude_psk: bool,
    exclude_non_psk: bool,
) -> Result<Vec<Box<dyn CipherSuite>>> {
    let cipher_suites = if !user_selected_suites.is_empty() {
        cipher_suites_for_ids(user_selected_suites)?
    } else {
        default_cipher_suites()
    };

    let filtered_cipher_suites: Vec<Box<dyn CipherSuite>> = cipher_suites
        .into_iter()
        .filter(|c| !((exclude_psk && c.is_psk()) || (exclude_non_psk && !c.is_psk())))
        .collect();

    if filtered_cipher_suites.is_empty() {
        Err(Error::ErrNoAvailableCipherSuites)
    } else {
        Ok(filtered_cipher_suites)
    }
}
