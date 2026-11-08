// AES-GCM (Galois Counter Mode)
// The most widely used block cipher worldwide.
// Mandatory as of TLS 1.2 (2008) and used by default by most clients.
// RFC 5288 year 2008 https://tools.ietf.org/html/rfc5288

use std::io::Cursor;
use std::sync::Arc;

use crypto::{AeadAlgorithm, AeadCipher, RTCCryptoProvider};

use super::*;
use crate::content::*;
use crate::record_layer::record_layer_header::*;
use shared::error::*;

const CRYPTO_GCM_TAG_LENGTH: usize = 16;
const CRYPTO_GCM_NONCE_LENGTH: usize = 12;

/// AES-GCM authenticated encryption for DTLS records, holding the per-direction keys.
pub struct CryptoGcm {
    provider: Arc<dyn RTCCryptoProvider>,
    local_gcm: Box<dyn AeadCipher>,
    remote_gcm: Box<dyn AeadCipher>,
    local_write_iv: Vec<u8>,
    remote_write_iv: Vec<u8>,
}

impl CryptoGcm {
    /// Builds the cipher from the local and remote keys and salts.
    pub fn new(
        provider: Arc<dyn RTCCryptoProvider>,
        local_key: &[u8],
        local_write_iv: &[u8],
        remote_key: &[u8],
        remote_write_iv: &[u8],
    ) -> Result<Self> {
        let local_gcm = provider
            .crypto()
            .new_aead(AeadAlgorithm::Aes128Gcm, local_key)
            .map_err(crypto_error)?;
        let remote_gcm = provider
            .crypto()
            .new_aead(AeadAlgorithm::Aes128Gcm, remote_key)
            .map_err(crypto_error)?;
        Ok(CryptoGcm {
            provider,
            local_gcm,
            local_write_iv: local_write_iv.to_vec(),
            remote_gcm,
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

        let mut nonce = [0u8; CRYPTO_GCM_NONCE_LENGTH];
        nonce[..4].copy_from_slice(&self.local_write_iv[..4]);
        self.provider
            .random()
            .fill(&mut nonce[4..])
            .map_err(crypto_error)?;

        let additional_data = generate_aead_additional_data(pkt_rlh, payload.len());

        // Assemble header + explicit nonce + payload once, then encrypt the
        // payload region in place with a detached tag: one allocation and one
        // payload copy instead of the former staging Vec + full re-copy.
        let mut r = Vec::with_capacity(
            RECORD_LAYER_HEADER_SIZE + 8 + payload.len() + CRYPTO_GCM_TAG_LENGTH,
        );
        r.extend_from_slice(raw);
        r.extend_from_slice(&nonce[4..]);
        r.extend_from_slice(payload);

        let mut tag = [0; CRYPTO_GCM_TAG_LENGTH];
        self.local_gcm
            .seal_in_place(
                &nonce,
                &additional_data,
                &mut r[RECORD_LAYER_HEADER_SIZE + 8..],
                &mut tag,
            )
            .map_err(crypto_error)?;
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

        let mut nonce = [0u8; CRYPTO_GCM_NONCE_LENGTH];
        nonce[..4].copy_from_slice(&self.remote_write_iv[..4]);
        nonce[4..].copy_from_slice(&r[RECORD_LAYER_HEADER_SIZE..RECORD_LAYER_HEADER_SIZE + 8]);

        let out = &r[RECORD_LAYER_HEADER_SIZE + 8..];
        if out.len() < CRYPTO_GCM_TAG_LENGTH {
            // Too short to hold the auth tag; the AEAD would reject it.
            return Err(Error::Other(
                "DTLS AES-GCM record too short for tag".to_string(),
            ));
        }
        let tag_start = out.len() - CRYPTO_GCM_TAG_LENGTH;

        let additional_data = generate_aead_additional_data(&h, tag_start);

        let mut d = Vec::with_capacity(RECORD_LAYER_HEADER_SIZE + tag_start);
        d.extend_from_slice(&r[..RECORD_LAYER_HEADER_SIZE]);
        d.extend_from_slice(&out[..tag_start]);
        self.remote_gcm
            .open_in_place(
                &nonce,
                &additional_data,
                &mut d[RECORD_LAYER_HEADER_SIZE..],
                &out[tag_start..],
            )
            .map_err(authentication_error)?;

        Ok(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record_layer::record_layer_header::PROTOCOL_VERSION1_2;

    fn make_record(payload: &[u8]) -> (RecordLayerHeader, Vec<u8>) {
        let header = RecordLayerHeader {
            content_type: ContentType::ApplicationData,
            protocol_version: PROTOCOL_VERSION1_2,
            epoch: 1,
            sequence_number: 1,
            content_len: payload.len() as u16,
        };
        let mut raw = Vec::with_capacity(RECORD_LAYER_HEADER_SIZE + payload.len());
        header.marshal(&mut raw).unwrap();
        raw.extend_from_slice(payload);
        (header, raw)
    }

    #[test]
    fn test_crypto_gcm_roundtrip() {
        let local_key = [0x11u8; 16];
        let local_iv = [0x22u8; 4];
        let remote_key = [0x33u8; 16];
        let remote_iv = [0x44u8; 4];
        let provider = crypto::default_provider().unwrap();
        let mut sender = CryptoGcm::new(
            provider.clone(),
            &local_key,
            &local_iv,
            &remote_key,
            &remote_iv,
        )
        .unwrap();
        let mut receiver =
            CryptoGcm::new(provider, &remote_key, &remote_iv, &local_key, &local_iv).unwrap();

        let payload = b"application data!";
        let (header, raw) = make_record(payload);

        let encrypted = sender.encrypt(&header, &raw).unwrap();
        assert_eq!(
            encrypted.len(),
            RECORD_LAYER_HEADER_SIZE + 8 + payload.len() + CRYPTO_GCM_TAG_LENGTH,
            "header + explicit nonce + ciphertext + tag"
        );
        assert_ne!(
            &encrypted[RECORD_LAYER_HEADER_SIZE + 8..RECORD_LAYER_HEADER_SIZE + 8 + payload.len()],
            &payload[..],
            "payload must not be in the clear"
        );

        let decrypted = receiver.decrypt(&encrypted).unwrap();
        // The wire header is passed through as-is; its length field was
        // patched by encrypt to include the explicit nonce and tag.
        assert_eq!(
            &decrypted[..RECORD_LAYER_HEADER_SIZE - 2],
            &raw[..RECORD_LAYER_HEADER_SIZE - 2]
        );
        assert_eq!(&decrypted[RECORD_LAYER_HEADER_SIZE..], &payload[..]);
    }

    /// A record long enough to hold the explicit nonce but too short for the
    /// auth tag must fail cleanly instead of panicking.
    #[test]
    fn test_crypto_gcm_decrypt_too_short_for_tag() {
        let key = [0x11u8; 16];
        let iv = [0x22u8; 4];
        let mut cg =
            CryptoGcm::new(crypto::default_provider().unwrap(), &key, &iv, &key, &iv).unwrap();

        let (_, mut raw) = make_record(&[0u8; 0]);
        // 8-byte explicit nonce plus 10 bytes: less than the 16-byte tag.
        raw.extend_from_slice(&[0u8; 8 + 10]);

        assert!(cg.decrypt(&raw).is_err());
    }

    /// Tampered ciphertext must fail authentication.
    #[test]
    fn test_crypto_gcm_decrypt_rejects_tampering() {
        let key = [0x11u8; 16];
        let iv = [0x22u8; 4];
        let mut cg =
            CryptoGcm::new(crypto::default_provider().unwrap(), &key, &iv, &key, &iv).unwrap();

        let (header, raw) = make_record(b"payload");
        let mut encrypted = cg.encrypt(&header, &raw).unwrap();
        let last = encrypted.len() - 1;
        encrypted[last] ^= 0xff;

        assert!(cg.decrypt(&encrypted).is_err());
    }
}
