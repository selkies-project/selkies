use std::io::Cursor;
use std::sync::Arc;

use crypto::{AeadAlgorithm, AeadCipher, RTCCryptoProvider};

use super::*;
use crate::content::*;
use crate::record_layer::record_layer_header::*; // what about Aes256Gcm?

const CRYPTO_CHACHA20_TAG_LENGTH: usize = 16;
const CRYPTO_CHACHA20_NONCE_LENGTH: usize = 12;

// State needed to handle encrypted input/output
/// ChaCha20-Poly1305 authenticated encryption for DTLS records, holding the per-direction keys.
pub struct CryptoChaCha20 {
    local_cc: Box<dyn AeadCipher>,
    remote_cc: Box<dyn AeadCipher>,
    local_write_iv: Vec<u8>,
    remote_write_iv: Vec<u8>,
}

fn noncegen(nonce: &mut [u8], epoch: u16, seqnum: u64) {
    let epoch: u64 = epoch.into();
    let seqnum = (seqnum & 0xFFFFFFFFFFFF) | (epoch << 48);
    for i in 0..8 {
        nonce[i + 4] ^= ((seqnum >> (8 * (7 - i))) & 0xFF) as u8;
    }
}

impl CryptoChaCha20 {
    /// Builds the cipher from the local and remote keys and salts.
    pub fn new(
        provider: Arc<dyn RTCCryptoProvider>,
        local_key: &[u8],
        local_write_iv: &[u8],
        remote_key: &[u8],
        remote_write_iv: &[u8],
    ) -> Result<Self> {
        let local_cc = provider
            .crypto()
            .new_aead(AeadAlgorithm::ChaCha20Poly1305, local_key)
            .map_err(crypto_error)?;
        let remote_cc = provider
            .crypto()
            .new_aead(AeadAlgorithm::ChaCha20Poly1305, remote_key)
            .map_err(crypto_error)?;
        Ok(CryptoChaCha20 {
            local_cc,
            local_write_iv: local_write_iv.to_vec(),
            remote_cc,
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

        let mut nonce = [0u8; CRYPTO_CHACHA20_NONCE_LENGTH];
        nonce[..CRYPTO_CHACHA20_NONCE_LENGTH]
            .copy_from_slice(&self.local_write_iv[..CRYPTO_CHACHA20_NONCE_LENGTH]);

        noncegen(&mut nonce[..], pkt_rlh.epoch, pkt_rlh.sequence_number);
        let additional_data = generate_aead_additional_data(pkt_rlh, payload.len());

        let mut buffer: Vec<u8> = Vec::new();
        buffer.extend_from_slice(payload);

        let mut tag = [0; CRYPTO_CHACHA20_TAG_LENGTH];
        self.local_cc
            .seal_in_place(&nonce, &additional_data, &mut buffer, &mut tag)
            .map_err(crypto_error)?;

        let mut r = Vec::with_capacity(raw.len() + buffer.len() + tag.len());
        r.extend_from_slice(raw);
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

        let mut nonce = [0; CRYPTO_CHACHA20_NONCE_LENGTH];
        nonce.copy_from_slice(&self.remote_write_iv[..]);

        noncegen(&mut nonce[..], h.epoch, h.sequence_number);
        let out = &r[RECORD_LAYER_HEADER_SIZE..];
        if out.len() < CRYPTO_CHACHA20_TAG_LENGTH {
            return Err(Error::ErrInvalidMac);
        }

        let additional_data =
            generate_aead_additional_data(&h, out.len() - CRYPTO_CHACHA20_TAG_LENGTH);

        let tag_start = out.len() - CRYPTO_CHACHA20_TAG_LENGTH;
        let mut buffer = out[..tag_start].to_vec();

        self.remote_cc
            .open_in_place(&nonce, &additional_data, &mut buffer, &out[tag_start..])
            .map_err(authentication_error)?;

        let mut d = Vec::with_capacity(RECORD_LAYER_HEADER_SIZE + buffer.len());
        d.extend_from_slice(&r[..RECORD_LAYER_HEADER_SIZE]);
        d.extend_from_slice(&buffer);

        Ok(d)
    }
}
