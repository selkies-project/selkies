use byteorder::{BigEndian, ByteOrder};
use bytes::BytesMut;
use crypto::{AeadAlgorithm, AeadCipher, RTCCrypto, SecretVec};

use super::{Cipher, Kdf, crypto_error};
use crate::key_derivation::*;
use crate::protection_profile::ProtectionProfile;
use shared::{
    error::{Error, Result},
    marshal::*,
};

pub const CIPHER_AEAD_AES_GCM_AUTH_TAG_LEN: usize = 16;

const RTCP_ENCRYPTION_FLAG: u8 = 0x80;

/// Provider-backed AEAD cipher based on AES-GCM.
pub(crate) struct CipherAeadAesGcm {
    profile: ProtectionProfile,
    srtp_cipher: Box<dyn AeadCipher>,
    srtcp_cipher: Box<dyn AeadCipher>,
    srtp_session_salt: Vec<u8>,
    srtcp_session_salt: Vec<u8>,
}

impl Cipher for CipherAeadAesGcm {
    fn rtp_auth_tag_len(&self) -> usize {
        self.profile.rtp_auth_tag_len()
    }

    /// Get RTCP authenticated tag length.
    fn rtcp_auth_tag_len(&self) -> usize {
        self.profile.rtcp_auth_tag_len()
    }

    /// Get AEAD auth key length of the cipher.
    fn aead_auth_tag_len(&self) -> usize {
        self.profile.aead_auth_tag_len()
    }

    fn encrypt_rtp_in_place(
        &mut self,
        packet: &mut BytesMut,
        header: &rtp::Header,
        roc: u32,
    ) -> Result<()> {
        // Encrypt the payload region in place with the header region as AAD, and append the
        // detached tag.
        let header_len = header.marshal_size();
        let nonce = self.rtp_initialization_vector(header, roc);

        let (aad, plaintext) = packet.split_at_mut(header_len);
        let mut tag = [0; CIPHER_AEAD_AES_GCM_AUTH_TAG_LEN];
        self.srtp_cipher
            .seal_in_place(&nonce, aad, plaintext, &mut tag)
            .map_err(crypto_error)?;

        packet.extend_from_slice(&tag);
        Ok(())
    }

    fn decrypt_rtp(
        &mut self,
        ciphertext: &[u8],
        header: &rtp::Header,
        roc: u32,
    ) -> Result<BytesMut> {
        let tag_len = self.aead_auth_tag_len();
        if ciphertext.len() < tag_len {
            return Err(Error::ErrFailedToVerifyAuthTag);
        }

        let payload_offset = header.marshal_size();
        if ciphertext.len() < payload_offset + tag_len {
            // Too short to hold header + tag; the AEAD would reject it and the
            // slice split below would panic.
            return Err(Error::ErrFailedToVerifyAuthTag);
        }

        let nonce = self.rtp_initialization_vector(header, roc);

        let tag_offset = ciphertext.len() - tag_len;
        let tag = &ciphertext[tag_offset..];
        let mut writer = BytesMut::with_capacity(tag_offset);
        writer.extend_from_slice(&ciphertext[..tag_offset]);

        let (aad, encrypted_payload) = writer.split_at_mut(payload_offset);
        self.srtp_cipher
            .open_in_place(&nonce, aad, encrypted_payload, tag)
            .map_err(|_| Error::ErrFailedToVerifyAuthTag)?;
        Ok(writer)
    }

    fn encrypt_rtcp(
        &mut self,
        decrypted: &[u8],
        srtcp_index: usize,
        ssrc: u32,
    ) -> Result<BytesMut> {
        let iv = self.rtcp_initialization_vector(srtcp_index, ssrc);
        let aad = self.rtcp_additional_authenticated_data(decrypted, srtcp_index);

        let mut writer =
            BytesMut::with_capacity(decrypted.len() + self.aead_auth_tag_len() + SRTCP_INDEX_SIZE);
        writer.extend_from_slice(decrypted);

        let mut tag = [0; CIPHER_AEAD_AES_GCM_AUTH_TAG_LEN];
        self.srtcp_cipher
            .seal_in_place(&iv, &aad, &mut writer[8..], &mut tag)
            .map_err(crypto_error)?;

        writer.extend_from_slice(&tag);
        writer.extend_from_slice(&aad[8..]);

        Ok(writer)
    }

    fn decrypt_rtcp(
        &mut self,
        encrypted: &[u8],
        srtcp_index: usize,
        ssrc: u32,
    ) -> Result<BytesMut> {
        let tag_len = self.aead_auth_tag_len();
        if encrypted.len() < tag_len + SRTCP_INDEX_SIZE {
            return Err(Error::ErrFailedToVerifyAuthTag);
        }

        let nonce = self.rtcp_initialization_vector(srtcp_index, ssrc);
        let aad = self.rtcp_additional_authenticated_data(encrypted, srtcp_index);

        let tag_start = encrypted.len() - SRTCP_INDEX_SIZE - tag_len;
        if tag_start < 8 {
            // Too short to hold the SRTCP header + tag; the AEAD would reject it.
            return Err(Error::ErrFailedToVerifyAuthTag);
        }

        let tag = &encrypted[tag_start..tag_start + tag_len];
        let mut writer = BytesMut::with_capacity(tag_start);
        writer.extend_from_slice(&encrypted[..tag_start]);

        {
            let (_, encrypted_payload) = writer.split_at_mut(8);
            self.srtcp_cipher
                .open_in_place(&nonce, &aad, encrypted_payload, tag)
                .map_err(|_| Error::ErrFailedToVerifyAuthTag)?;
        }
        Ok(writer)
    }

    fn get_rtcp_index(&self, input: &[u8]) -> usize {
        let pos = input.len() - 4;
        let val = BigEndian::read_u32(&input[pos..]);

        (val & !((RTCP_ENCRYPTION_FLAG as u32) << 24)) as usize
    }
}

impl CipherAeadAesGcm {
    /// Create a new AEAD instance.
    pub(crate) fn new(
        profile: ProtectionProfile,
        master_key: &[u8],
        master_salt: &[u8],
        crypto: &dyn RTCCrypto,
    ) -> Result<CipherAeadAesGcm> {
        let (algorithm, kdf): (AeadAlgorithm, Kdf) = match profile {
            ProtectionProfile::AeadAes128Gcm => (AeadAlgorithm::Aes128Gcm, aes_cm_key_derivation),
            // AES_256_GCM must use AES_256_CM_PRF as per https://datatracker.ietf.org/doc/html/rfc7714#section-11
            ProtectionProfile::AeadAes256Gcm => {
                (AeadAlgorithm::Aes256Gcm, aes_256_cm_key_derivation)
            }
            _ => unreachable!(),
        };

        assert_eq!(
            profile.aead_auth_tag_len(),
            CIPHER_AEAD_AES_GCM_AUTH_TAG_LEN
        );
        assert_eq!(profile.salt_len(), master_salt.len());

        let build_cipher = |label: u8| -> Result<Box<dyn AeadCipher>> {
            let session_key = SecretVec::new(kdf(
                crypto,
                label,
                master_key,
                master_salt,
                0,
                master_key.len(),
            )?);
            let cipher = crypto
                .new_aead(algorithm, session_key.as_ref())
                .map_err(crypto_error)?;
            if cipher.tag_len() != CIPHER_AEAD_AES_GCM_AUTH_TAG_LEN {
                return Err(Error::Crypto(format!(
                    "SRTP AES-GCM requires a {}-byte tag, provider returned {}",
                    CIPHER_AEAD_AES_GCM_AUTH_TAG_LEN,
                    cipher.tag_len()
                )));
            }
            Ok(cipher)
        };

        let srtp_cipher = build_cipher(LABEL_SRTP_ENCRYPTION)?;
        let srtcp_cipher = build_cipher(LABEL_SRTCP_ENCRYPTION)?;

        let srtp_session_salt = kdf(
            crypto,
            LABEL_SRTP_SALT,
            master_key,
            master_salt,
            0,
            master_salt.len(),
        )?;

        let srtcp_session_salt = kdf(
            crypto,
            LABEL_SRTCP_SALT,
            master_key,
            master_salt,
            0,
            master_salt.len(),
        )?;

        Ok(CipherAeadAesGcm {
            profile,
            srtp_cipher,
            srtcp_cipher,
            srtp_session_salt,
            srtcp_session_salt,
        })
    }

    /// The 12-octet IV used by AES-GCM SRTP is formed by first concatenating
    /// 2 octets of zeroes, the 4-octet SSRC, the 4-octet rollover counter
    /// (ROC), and the 2-octet sequence number (SEQ).  The resulting 12-octet
    /// value is then XORed to the 12-octet salt to form the 12-octet IV.
    ///
    /// https://tools.ietf.org/html/rfc7714#section-8.1
    pub(crate) fn rtp_initialization_vector(&self, header: &rtp::Header, roc: u32) -> [u8; 12] {
        let mut iv = [0u8; 12];
        BigEndian::write_u32(&mut iv[2..], header.ssrc);
        BigEndian::write_u32(&mut iv[6..], roc);
        BigEndian::write_u16(&mut iv[10..], header.sequence_number);

        for (i, v) in iv.iter_mut().enumerate() {
            *v ^= self.srtp_session_salt[i];
        }

        iv
    }

    /// The 12-octet IV used by AES-GCM SRTCP is formed by first
    /// concatenating 2 octets of zeroes, the 4-octet SSRC identifier,
    /// 2 octets of zeroes, a single "0" bit, and the 31-bit SRTCP index.
    /// The resulting 12-octet value is then XORed to the 12-octet salt to
    /// form the 12-octet IV.
    ///
    /// https://tools.ietf.org/html/rfc7714#section-9.1
    pub(crate) fn rtcp_initialization_vector(&self, srtcp_index: usize, ssrc: u32) -> [u8; 12] {
        let mut iv = [0u8; 12];

        BigEndian::write_u32(&mut iv[2..], ssrc);
        BigEndian::write_u32(&mut iv[8..], srtcp_index as u32);

        for (i, v) in iv.iter_mut().enumerate() {
            *v ^= self.srtcp_session_salt[i];
        }

        iv
    }

    /// In an SRTCP packet, a 1-bit Encryption flag is prepended to the
    /// 31-bit SRTCP index to form a 32-bit value we shall call the
    /// "ESRTCP word"
    ///
    /// https://tools.ietf.org/html/rfc7714#section-17
    pub(crate) fn rtcp_additional_authenticated_data(
        &self,
        rtcp_packet: &[u8],
        srtcp_index: usize,
    ) -> [u8; 12] {
        let mut aad = [0u8; 12];

        aad[..8].copy_from_slice(&rtcp_packet[..8]);

        BigEndian::write_u32(&mut aad[8..], srtcp_index as u32);

        aad[8] |= RTCP_ENCRYPTION_FLAG;
        aad
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_aead_aes_gcm_128() {
        let profile = ProtectionProfile::AeadAes128Gcm;
        let master_key = vec![0u8; profile.key_len()];
        let master_salt = vec![0u8; 12];

        let provider = crypto::default_provider().unwrap();
        let mut cipher =
            CipherAeadAesGcm::new(profile, &master_key, &master_salt, provider.crypto()).unwrap();

        let header = rtp::Header {
            ssrc: 0x12345678,
            ..Default::default()
        };

        let payload = vec![0u8; 100];
        let encrypted = cipher.encrypt_rtp(&payload, &header, 0).unwrap();

        let decrypted = cipher.decrypt_rtp(&encrypted, &header, 0).unwrap();
        assert_eq!(&decrypted[..], &payload[..]);
    }

    /// Inputs long enough to pass the tag-length check but too short to hold
    /// header + tag must fail cleanly instead of panicking on a slice split.
    #[test]
    fn test_aead_aes_gcm_short_input_errors() {
        let profile = ProtectionProfile::AeadAes128Gcm;
        let master_key = vec![0u8; profile.key_len()];
        let master_salt = vec![0u8; 12];

        let provider = crypto::default_provider().unwrap();
        let mut cipher =
            CipherAeadAesGcm::new(profile, &master_key, &master_salt, provider.crypto()).unwrap();

        let header = rtp::Header {
            ssrc: 0x12345678,
            ..Default::default()
        };

        // 20 bytes: >= the 16-byte tag, but < 12-byte header + 16-byte tag.
        assert!(cipher.decrypt_rtp(&[0u8; 20], &header, 0).is_err());

        // 24 bytes: >= tag + 4-byte SRTCP index, but < 8-byte SRTCP header
        // + tag + index.
        assert!(cipher.decrypt_rtcp(&[0u8; 24], 0, 0x12345678).is_err());
    }

    #[test]
    fn test_aead_aes_gcm_256() {
        let profile = ProtectionProfile::AeadAes256Gcm;
        let master_key = vec![0u8; profile.key_len()];
        let master_salt = vec![0u8; 12];

        let provider = crypto::default_provider().unwrap();
        let mut cipher =
            CipherAeadAesGcm::new(profile, &master_key, &master_salt, provider.crypto()).unwrap();

        let header = rtp::Header {
            ssrc: 0x12345678,
            ..Default::default()
        };

        let payload = vec![0u8; 100];
        let encrypted = cipher.encrypt_rtp(&payload, &header, 0).unwrap();

        let decrypted = cipher.decrypt_rtp(&encrypted, &header, 0).unwrap();
        assert_eq!(&decrypted[..], &payload[..]);
    }
}
