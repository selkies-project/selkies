// AES-CCM (Counter with CBC-MAC)
// Alternative to GCM mode.
// Available in OpenSSL as of TLS 1.3 (2018), but disabled by default.
// Two AES computations per block, thus expected to be somewhat slower than AES-GCM.
// RFC 6655 year 2012 https://tools.ietf.org/html/rfc6655
// Much lower adoption, probably because it came after GCM and offer no significant benefit.

// https://github.com/RustCrypto/AEADs
// https://docs.rs/ccm/0.3.0/ccm/ Or https://crates.io/crates/aes-ccm?

use std::io::Cursor;
use std::sync::Arc;

use crypto::{AeadAlgorithm, AeadCipher, RTCCryptoProvider};

use super::*;
use crate::content::*;
use crate::record_layer::record_layer_header::*;
use shared::error::*;

const CRYPTO_CCM_NONCE_LENGTH: usize = 12;

#[derive(Clone)]
/// The authentication tag length a CCM suite uses.
pub enum CryptoCcmTagLen {
    /// An 8-byte tag, as the `_CCM_8` suites use.
    CryptoCcm8TagLength,
    /// The full 16-byte tag.
    CryptoCcmTagLength,
}

/// AES-CCM authenticated encryption for DTLS records, holding the per-direction keys.
pub struct CryptoCcm {
    local_ccm: Box<dyn AeadCipher>,
    remote_ccm: Box<dyn AeadCipher>,
    local_write_iv: Vec<u8>,
    remote_write_iv: Vec<u8>,
}

impl CryptoCcm {
    /// Builds the cipher from the local and remote keys and salts.
    pub fn new(
        provider: Arc<dyn RTCCryptoProvider>,
        tag_len: &CryptoCcmTagLen,
        local_key: &[u8],
        local_write_iv: &[u8],
        remote_key: &[u8],
        remote_write_iv: &[u8],
    ) -> Result<Self> {
        let algorithm = match tag_len {
            CryptoCcmTagLen::CryptoCcmTagLength => AeadAlgorithm::Aes128Ccm,
            CryptoCcmTagLen::CryptoCcm8TagLength => AeadAlgorithm::Aes128Ccm8,
        };
        let local_ccm = provider
            .crypto()
            .new_aead(algorithm, local_key)
            .map_err(crypto_error)?;
        let remote_ccm = provider
            .crypto()
            .new_aead(algorithm, remote_key)
            .map_err(crypto_error)?;
        Ok(CryptoCcm {
            local_ccm,
            local_write_iv: local_write_iv.to_vec(),
            remote_ccm,
            remote_write_iv: remote_write_iv.to_vec(),
        })
    }

    /// Protects one record, returning header plus ciphertext.
    ///
    /// # Errors
    ///
    /// Fails if the cipher rejects the input.
    pub fn encrypt(&mut self, pkt_rlh: &RecordLayerHeader, raw: &[u8]) -> Result<Vec<u8>> {
        let payload = &raw[RECORD_LAYER_HEADER_SIZE..];
        let raw = &raw[..RECORD_LAYER_HEADER_SIZE];

        // The explicit part of the nonce is the record's epoch and sequence number, which no
        // other record under this key carries (RFC 5288 section 3, RFC 6655 section 3).
        let explicit = explicit_nonce(pkt_rlh);
        let nonce: [u8; CRYPTO_CCM_NONCE_LENGTH] = std::array::from_fn(|i| {
            if i < 4 {
                self.local_write_iv[i]
            } else {
                explicit[i - 4]
            }
        });

        let additional_data = generate_aead_additional_data(pkt_rlh, payload.len());

        let mut buffer = payload.to_vec();
        let mut tag = vec![0; self.local_ccm.tag_len()];
        self.local_ccm
            .seal_in_place(&nonce, &additional_data, &mut buffer, &mut tag)
            .map_err(crypto_error)?;

        let mut r = Vec::with_capacity(raw.len() + 8 + buffer.len() + tag.len());

        r.extend_from_slice(raw);
        r.extend_from_slice(&nonce[4..]);
        r.extend_from_slice(&buffer);
        r.extend_from_slice(&tag);

        // Update recordLayer size to include explicit nonce
        let r_len = (r.len() - RECORD_LAYER_HEADER_SIZE) as u16;
        r[RECORD_LAYER_HEADER_SIZE - 2..RECORD_LAYER_HEADER_SIZE]
            .copy_from_slice(&r_len.to_be_bytes());

        Ok(r)
    }

    /// Unprotects one record.
    ///
    /// # Errors
    ///
    /// Fails if authentication fails or the record is too short.
    pub fn decrypt(&mut self, r: &[u8]) -> Result<Vec<u8>> {
        let mut reader = Cursor::new(r);
        let h = RecordLayerHeader::unmarshal(&mut reader)?;
        if h.content_type == ContentType::ChangeCipherSpec {
            // Nothing to encrypt with ChangeCipherSpec
            return Ok(r.to_vec());
        }

        if r.len() <= (RECORD_LAYER_HEADER_SIZE + 8) {
            return Err(Error::ErrNotEnoughRoomForNonce);
        }

        let explicit = &r[RECORD_LAYER_HEADER_SIZE..RECORD_LAYER_HEADER_SIZE + 8];
        let nonce: [u8; CRYPTO_CCM_NONCE_LENGTH] = std::array::from_fn(|i| {
            if i < 4 {
                self.remote_write_iv[i]
            } else {
                explicit[i - 4]
            }
        });

        let out = &r[RECORD_LAYER_HEADER_SIZE + 8..];

        let tag_len = self.remote_ccm.tag_len();
        if out.len() < tag_len {
            return Err(Error::ErrInvalidMac);
        }
        let tag_start = out.len() - tag_len;
        let additional_data = generate_aead_additional_data(&h, tag_start);
        let mut buffer = out[..tag_start].to_vec();
        self.remote_ccm
            .open_in_place(&nonce, &additional_data, &mut buffer, &out[tag_start..])
            .map_err(authentication_error)?;

        let mut d = Vec::with_capacity(RECORD_LAYER_HEADER_SIZE + buffer.len());
        d.extend_from_slice(&r[..RECORD_LAYER_HEADER_SIZE]);
        d.extend_from_slice(&buffer);

        Ok(d)
    }
}
