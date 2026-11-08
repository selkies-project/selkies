use byteorder::{BigEndian, ByteOrder};
use bytes::{BufMut, BytesMut};
use crypto::{
    HmacAlgorithm, Mac, RTCCrypto, SecretVec, StreamCipher, StreamCipherAlgorithm, constant_time_eq,
};
use rtcp::header::{HEADER_LENGTH, SSRC_LENGTH};
use shared::marshal::*;

use super::{Cipher, Kdf, crypto_error};
use crate::key_derivation::*;
use crate::protection_profile::*;
use shared::error::{Error, Result};

pub const CIPHER_AES_CM_HMAC_SHA1AUTH_TAG_LEN: usize = 10;

pub(crate) struct CipherAesCmHmacSha1 {
    profile: ProtectionProfile,
    srtp_session_salt: Vec<u8>,
    /// Pre-keyed HMAC-SHA1, built once here rather than per packet. Deriving the key schedule on
    /// every packet measured ~3x slower on the SRTP path; see `benches/README.md`.
    srtp_session_auth: Box<dyn Mac>,
    srtcp_session_salt: Vec<u8>,
    srtcp_session_auth: Box<dyn Mac>,
    srtp_cipher: Box<dyn StreamCipher>,
    srtcp_cipher: Box<dyn StreamCipher>,
}

impl CipherAesCmHmacSha1 {
    pub fn new(
        profile: ProtectionProfile,
        master_key: &[u8],
        master_salt: &[u8],
        crypto: &dyn RTCCrypto,
    ) -> Result<Self> {
        let (kdf, algorithm): (Kdf, StreamCipherAlgorithm) = match profile {
            ProtectionProfile::Aes128CmHmacSha1_32 | ProtectionProfile::Aes128CmHmacSha1_80 => {
                (aes_cm_key_derivation, StreamCipherAlgorithm::Aes128Ctr)
            }
            ProtectionProfile::Aes256CmHmacSha1_80 | ProtectionProfile::Aes256CmHmacSha1_32 => {
                (aes_256_cm_key_derivation, StreamCipherAlgorithm::Aes256Ctr)
            }
            _ => {
                return Err(Error::Other(String::from(
                    "no AES protection profile passed to CipherAesCmHmacSha1",
                )));
            }
        };
        let srtp_session_key = SecretVec::new(kdf(
            crypto,
            LABEL_SRTP_ENCRYPTION,
            master_key,
            master_salt,
            0,
            master_key.len(),
        )?);
        let srtcp_session_key = SecretVec::new(kdf(
            crypto,
            LABEL_SRTCP_ENCRYPTION,
            master_key,
            master_salt,
            0,
            master_key.len(),
        )?);

        let srtp_cipher = crypto
            .new_stream_cipher(algorithm, srtp_session_key.as_ref())
            .map_err(crypto_error)?;
        let srtcp_cipher = crypto
            .new_stream_cipher(algorithm, srtcp_session_key.as_ref())
            .map_err(crypto_error)?;
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
        let auth_key_len = profile.auth_key_len();
        let srtp_session_auth = SecretVec::new(kdf(
            crypto,
            LABEL_SRTP_AUTHENTICATION_TAG,
            master_key,
            master_salt,
            0,
            auth_key_len,
        )?);
        let srtcp_session_auth = SecretVec::new(kdf(
            crypto,
            LABEL_SRTCP_AUTHENTICATION_TAG,
            master_key,
            master_salt,
            0,
            auth_key_len,
        )?);

        // Key the MACs once per context. Everything above is per-context setup; the auth tag on
        // each packet then costs only the message pass.
        let srtp_session_auth = crypto
            .new_hmac(HmacAlgorithm::Sha1, srtp_session_auth.as_ref())
            .map_err(crypto_error)?;
        let srtcp_session_auth = crypto
            .new_hmac(HmacAlgorithm::Sha1, srtcp_session_auth.as_ref())
            .map_err(crypto_error)?;

        Ok(Self {
            profile,
            srtp_session_salt,
            srtp_session_auth,
            srtcp_session_salt,
            srtcp_session_auth,
            srtp_cipher,
            srtcp_cipher,
        })
    }

    /// Generate the SRTP HMAC-SHA1 authentication tag described by RFC 3711, section 4.2.
    ///
    /// Takes `&mut self` so the pre-keyed [`Mac`] can be used directly; the key schedule was
    /// derived once in [`new`](Self::new).
    fn generate_srtp_auth_tag(&mut self, buf: &[u8], roc: u32) -> Result<[u8; 20]> {
        let mut tag = [0; 20];
        self.srtp_session_auth
            .sign(&[buf, &roc.to_be_bytes()], &mut tag)
            .map_err(crypto_error)?;
        Ok(tag)
    }

    /// Generate the SRTCP HMAC-SHA1 authentication tag described by RFC 3711, section 4.2.
    fn generate_srtcp_auth_tag(&mut self, buf: &[u8]) -> Result<[u8; 20]> {
        let mut tag = [0; 20];
        self.srtcp_session_auth
            .sign(&[buf], &mut tag)
            .map_err(crypto_error)?;
        Ok(tag)
    }
}

impl Cipher for CipherAesCmHmacSha1 {
    /// Get RTP authenticated tag length.
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

    fn get_rtcp_index(&self, input: &[u8]) -> usize {
        let tail_offset = input.len() - (self.profile.rtcp_auth_tag_len() + SRTCP_INDEX_SIZE);
        (BigEndian::read_u32(&input[tail_offset..tail_offset + SRTCP_INDEX_SIZE]) & !(1 << 31))
            as usize
    }

    fn encrypt_rtp_in_place(
        &mut self,
        writer: &mut BytesMut,
        header: &rtp::Header,
        roc: u32,
    ) -> Result<()> {
        // Encrypt the payload
        let counter = generate_counter(
            header.sequence_number,
            roc,
            header.ssrc,
            &self.srtp_session_salt,
        );

        self.srtp_cipher
            .apply_keystream(&counter, &mut writer[header.marshal_size()..])
            .map_err(crypto_error)?;

        // Generate the auth tag.
        let full_auth_tag = self.generate_srtp_auth_tag(writer, roc)?;
        let auth_tag = &full_auth_tag[..self.rtp_auth_tag_len()];
        writer.extend_from_slice(auth_tag);

        Ok(())
    }

    fn decrypt_rtp(
        &mut self,
        encrypted: &[u8],
        header: &rtp::Header,
        roc: u32,
    ) -> Result<BytesMut> {
        let encrypted_len = encrypted.len();
        if encrypted_len < self.rtp_auth_tag_len() {
            return Err(Error::SrtpTooSmall(encrypted_len, self.rtp_auth_tag_len()));
        }

        let mut writer = BytesMut::with_capacity(encrypted_len - self.rtp_auth_tag_len());

        // Split the auth tag and the cipher text into two parts.
        let actual_tag = &encrypted[encrypted_len - self.rtp_auth_tag_len()..];
        let cipher_text = &encrypted[..encrypted_len - self.rtp_auth_tag_len()];

        // Generate the auth tag we expect to see from the ciphertext.
        let full_expected_tag = self.generate_srtp_auth_tag(cipher_text, roc)?;
        let expected_tag = &full_expected_tag[..self.rtp_auth_tag_len()];

        // See if the auth tag actually matches.
        // We use a constant time comparison to prevent timing attacks.
        if !constant_time_eq(actual_tag, expected_tag) {
            return Err(Error::RtpFailedToVerifyAuthTag);
        }

        // Write cipher_text to the destination buffer.
        writer.extend_from_slice(cipher_text);

        // Decrypt the ciphertext for the payload.
        let counter = generate_counter(
            header.sequence_number,
            roc,
            header.ssrc,
            &self.srtp_session_salt,
        );

        self.srtp_cipher
            .apply_keystream(&counter, &mut writer[header.marshal_size()..])
            .map_err(crypto_error)?;

        Ok(writer)
    }

    fn encrypt_rtcp(
        &mut self,
        decrypted: &[u8],
        srtcp_index: usize,
        ssrc: u32,
    ) -> Result<BytesMut> {
        let mut writer =
            BytesMut::with_capacity(decrypted.len() + SRTCP_INDEX_SIZE + self.rtcp_auth_tag_len());

        // Write the decrypted to the destination buffer.
        writer.extend_from_slice(decrypted);

        // Encrypt everything after header
        let counter = generate_counter(
            (srtcp_index & 0xFFFF) as u16,
            (srtcp_index >> 16) as u32,
            ssrc,
            &self.srtcp_session_salt,
        );

        self.srtcp_cipher
            .apply_keystream(&counter, &mut writer[HEADER_LENGTH + SSRC_LENGTH..])
            .map_err(crypto_error)?;

        // Add SRTCP index and set Encryption bit
        writer.put_u32(srtcp_index as u32 | (1u32 << 31));

        // Generate the auth tag.
        let full_auth_tag = self.generate_srtcp_auth_tag(&writer)?;
        let auth_tag = &full_auth_tag[..self.rtcp_auth_tag_len()];
        writer.extend_from_slice(auth_tag);

        Ok(writer)
    }

    fn decrypt_rtcp(
        &mut self,
        encrypted: &[u8],
        srtcp_index: usize,
        ssrc: u32,
    ) -> Result<BytesMut> {
        let encrypted_len = encrypted.len();
        if encrypted_len < self.rtcp_auth_tag_len() + SRTCP_INDEX_SIZE {
            return Err(Error::SrtcpTooSmall(
                encrypted_len,
                self.rtcp_auth_tag_len() + SRTCP_INDEX_SIZE,
            ));
        }

        let tail_offset = encrypted_len - (self.rtcp_auth_tag_len() + SRTCP_INDEX_SIZE);
        if tail_offset < 8 {
            return Err(Error::ErrTooShortRtcp);
        }

        let mut writer = BytesMut::with_capacity(tail_offset);

        writer.extend_from_slice(&encrypted[0..tail_offset]);

        let is_encrypted = encrypted[tail_offset] >> 7;
        if is_encrypted == 0 {
            return Ok(writer);
        }

        // Split the auth tag and the cipher text into two parts.
        let actual_tag = &encrypted[encrypted_len - self.rtcp_auth_tag_len()..];
        if actual_tag.len() != self.rtcp_auth_tag_len() {
            return Err(Error::RtcpInvalidLengthAuthTag(
                actual_tag.len(),
                self.rtcp_auth_tag_len(),
            ));
        }

        let cipher_text = &encrypted[..encrypted_len - self.rtcp_auth_tag_len()];

        // Generate the auth tag we expect to see from the ciphertext.
        let full_expected_tag = self.generate_srtcp_auth_tag(cipher_text)?;
        let expected_tag = &full_expected_tag[..self.rtcp_auth_tag_len()];

        // See if the auth tag actually matches.
        // We use a constant time comparison to prevent timing attacks.
        if !constant_time_eq(actual_tag, expected_tag) {
            return Err(Error::RtcpFailedToVerifyAuthTag);
        }

        let counter = generate_counter(
            (srtcp_index & 0xFFFF) as u16,
            (srtcp_index >> 16) as u32,
            ssrc,
            &self.srtcp_session_salt,
        );

        self.srtcp_cipher
            .apply_keystream(&counter, &mut writer[HEADER_LENGTH + SSRC_LENGTH..])
            .map_err(crypto_error)?;

        Ok(writer)
    }
}
