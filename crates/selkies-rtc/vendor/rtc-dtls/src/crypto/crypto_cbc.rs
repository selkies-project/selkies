// AES-CBC (Cipher Block Chaining)
// First historic block cipher for AES.
// CBC mode is insecure and must not be used. It’s been progressively deprecated and
// removed from SSL libraries.
// Introduced with TLS 1.0 year 2002. Superseded by GCM in TLS 1.2 year 2008.
// Removed in TLS 1.3 year 2018.
// RFC 3268 year 2002 https://tools.ietf.org/html/rfc3268

use crypto::{CbcAlgorithm, CbcCipher, HmacAlgorithm, Mac, RTCCryptoProvider, constant_time_eq};
use std::io::Cursor;
use std::sync::Arc;

use crate::content::*;
use crate::crypto::{authentication_error, crypto_error};
use crate::prf::*;
use crate::record_layer::record_layer_header::*;
use shared::error::*;

/// AES-CBC encryption with a separate HMAC for DTLS records, holding the per-direction keys.
pub struct CryptoCbc {
    provider: Arc<dyn RTCCryptoProvider>,
    local_cipher: Box<dyn CbcCipher>,
    remote_cipher: Box<dyn CbcCipher>,
    /// Keyed once per epoch. Re-deriving the HMAC key schedule per record measured ~2x
    /// slower on this path; see `rtc-srtp/benches/README.md` for the equivalent SRTP data.
    write_mac: Box<dyn Mac>,
    read_mac: Box<dyn Mac>,
}

impl CryptoCbc {
    const BLOCK_SIZE: usize = 16;
    const MAC_SIZE: usize = 20;

    /// Builds the cipher from the local and remote keys and salts.
    pub fn new(
        provider: Arc<dyn RTCCryptoProvider>,
        local_key: &[u8],
        local_mac: &[u8],
        remote_key: &[u8],
        remote_mac: &[u8],
    ) -> Result<Self> {
        let local_cipher = provider
            .crypto()
            .new_cbc(CbcAlgorithm::Aes256Cbc, local_key)
            .map_err(crypto_error)?;
        let remote_cipher = provider
            .crypto()
            .new_cbc(CbcAlgorithm::Aes256Cbc, remote_key)
            .map_err(crypto_error)?;
        // Key the record MACs once per epoch, alongside the ciphers.
        let write_mac = provider
            .crypto()
            .new_hmac(HmacAlgorithm::Sha1, local_mac)
            .map_err(crypto_error)?;
        let read_mac = provider
            .crypto()
            .new_hmac(HmacAlgorithm::Sha1, remote_mac)
            .map_err(crypto_error)?;
        Ok(CryptoCbc {
            provider,
            local_cipher,
            write_mac,
            remote_cipher,
            read_mac,
        })
    }

    /// Protects one record, returning header plus ciphertext.
    ///
    /// # Errors
    ///
    /// Fails if the cipher rejects the input.
    pub fn encrypt(&mut self, pkt_rlh: &RecordLayerHeader, raw: &[u8]) -> Result<Vec<u8>> {
        let mut payload = raw[RECORD_LAYER_HEADER_SIZE..].to_vec();
        let raw = &raw[..RECORD_LAYER_HEADER_SIZE];

        // Generate + Append MAC
        let h = pkt_rlh;

        let mac = prf_mac(
            self.write_mac.as_mut(),
            h.epoch,
            h.sequence_number,
            h.content_type,
            h.protocol_version,
            &payload,
        )?;
        payload.extend_from_slice(&mac);

        let padding_len = Self::BLOCK_SIZE - (payload.len() % Self::BLOCK_SIZE);
        payload.resize(payload.len() + padding_len, (padding_len - 1) as u8);

        let mut iv = [0; Self::BLOCK_SIZE];
        self.provider.random().fill(&mut iv).map_err(crypto_error)?;
        self.local_cipher
            .encrypt_blocks(&iv, &mut payload)
            .map_err(crypto_error)?;

        // Prepend unencrypte header with encrypted payload
        let mut r = vec![];
        r.extend_from_slice(raw);
        r.extend_from_slice(&iv);
        r.extend_from_slice(&payload);

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

        if r.len() < RECORD_LAYER_HEADER_SIZE + Self::BLOCK_SIZE {
            return Err(Error::ErrInvalidPacketLength);
        }

        let body = &r[RECORD_LAYER_HEADER_SIZE..];
        let iv = &body[0..Self::BLOCK_SIZE];
        let body = &body[Self::BLOCK_SIZE..];

        if body.is_empty() || !body.len().is_multiple_of(Self::BLOCK_SIZE) {
            return Err(Error::ErrInvalidPacketLength);
        }

        let mut decrypted = body.to_vec();
        self.remote_cipher
            .decrypt_blocks(iv, &mut decrypted)
            .map_err(authentication_error)?;

        let padding_value = decrypted.last().copied().ok_or(Error::ErrInvalidMac)?;
        let padding_len = padding_value as usize + 1;
        if padding_len > decrypted.len() {
            return Err(Error::ErrInvalidMac);
        }
        let padding_start = decrypted.len() - padding_len;
        let expected_padding = vec![padding_value; padding_len];
        let padding_valid = constant_time_eq(&decrypted[padding_start..], &expected_padding);
        decrypted.truncate(padding_start);

        if decrypted.len() < Self::MAC_SIZE {
            return Err(Error::ErrInvalidMac);
        }

        let recv_mac = &decrypted[decrypted.len() - Self::MAC_SIZE..];
        let decrypted = &decrypted[0..decrypted.len() - Self::MAC_SIZE];
        let mac = prf_mac(
            self.read_mac.as_mut(),
            h.epoch,
            h.sequence_number,
            h.content_type,
            h.protocol_version,
            decrypted,
        )?;

        if !padding_valid || !constant_time_eq(recv_mac, &mac) {
            return Err(Error::ErrInvalidMac);
        }

        let mut d = Vec::with_capacity(RECORD_LAYER_HEADER_SIZE + decrypted.len());
        d.extend_from_slice(&r[..RECORD_LAYER_HEADER_SIZE]);
        d.extend_from_slice(decrypted);

        Ok(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cipher_pair() -> (CryptoCbc, CryptoCbc) {
        let provider = crypto::default_provider().unwrap();
        let local_key = [0x11; 32];
        let remote_key = [0x22; 32];
        let local_mac = [0x33; 20];
        let remote_mac = [0x44; 20];
        let sender = CryptoCbc::new(
            provider.clone(),
            &local_key,
            &local_mac,
            &remote_key,
            &remote_mac,
        )
        .unwrap();
        let receiver =
            CryptoCbc::new(provider, &remote_key, &remote_mac, &local_key, &local_mac).unwrap();
        (sender, receiver)
    }

    fn record(payload: &[u8]) -> (RecordLayerHeader, Vec<u8>) {
        let header = RecordLayerHeader {
            content_type: ContentType::ApplicationData,
            protocol_version: PROTOCOL_VERSION1_2,
            epoch: 1,
            sequence_number: 7,
            content_len: payload.len() as u16,
        };
        let mut raw = Vec::new();
        header.marshal(&mut raw).unwrap();
        raw.extend_from_slice(payload);
        (header, raw)
    }

    #[test]
    fn roundtrip_and_authentication_failures() {
        let (mut sender, mut receiver) = cipher_pair();
        let (header, raw) = record(b"CBC record payload");
        let encrypted = sender.encrypt(&header, &raw).unwrap();
        assert_eq!(
            receiver.decrypt(&encrypted).unwrap()[RECORD_LAYER_HEADER_SIZE..],
            raw[RECORD_LAYER_HEADER_SIZE..]
        );

        let mut wrong_mac = encrypted.clone();
        wrong_mac[RECORD_LAYER_HEADER_SIZE] ^= 1;
        assert_eq!(receiver.decrypt(&wrong_mac), Err(Error::ErrInvalidMac));

        let mut bad_padding = encrypted.clone();
        *bad_padding.last_mut().unwrap() ^= 1;
        assert_eq!(receiver.decrypt(&bad_padding), Err(Error::ErrInvalidMac));

        let mut truncated = encrypted;
        truncated.pop();
        assert_eq!(
            receiver.decrypt(&truncated),
            Err(Error::ErrInvalidPacketLength)
        );
    }
}
