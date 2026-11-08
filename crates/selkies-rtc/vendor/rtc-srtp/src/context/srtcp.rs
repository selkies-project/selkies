use super::*;
use crate::key_derivation::SRTCP_INDEX_SIZE;
use shared::{error::Result, marshal::Unmarshal};

use bytes::BytesMut;

impl Context {
    /// DecryptRTCP decrypts a RTCP packet with an encrypted payload
    pub fn decrypt_rtcp(&mut self, encrypted: &[u8]) -> Result<BytesMut> {
        // A received SRTCP packet must be at least the minimum valid size for the
        // negotiated profile: the RTCP header through the SSRC (read at bytes
        // 4..8), the trailing SRTCP index, and the auth tag that `get_rtcp_index`
        // and the cipher read from the end (the HMAC tag for AES-CM, the AEAD tag
        // for GCM). A shorter packet from the network would otherwise index out
        // of bounds and panic (a remotely triggerable DoS). This is self-
        // contained: it does not rely on the individual ciphers' own guards.
        let min_len = 8
            + SRTCP_INDEX_SIZE
            + self.cipher.rtcp_auth_tag_len()
            + self.cipher.aead_auth_tag_len();
        if encrypted.len() < min_len {
            return Err(Error::ErrTooShortRtcp);
        }

        let mut buf = encrypted;
        rtcp::Header::unmarshal(&mut buf)?;

        let index = self.cipher.get_rtcp_index(encrypted);
        let ssrc = u32::from_be_bytes([encrypted[4], encrypted[5], encrypted[6], encrypted[7]]);

        if let Some(replay_detector) = &mut self.get_srtcp_ssrc_state(ssrc).replay_detector
            && !replay_detector.check(index as u64)
        {
            return Err(Error::SrtcpSsrcDuplicated(ssrc, index));
        }

        let dst = self.cipher.decrypt_rtcp(encrypted, index, ssrc)?;

        if let Some(replay_detector) = &mut self.get_srtcp_ssrc_state(ssrc).replay_detector {
            replay_detector.accept();
        }

        Ok(dst)
    }

    /// EncryptRTCP marshals and encrypts an RTCP packet, writing to the dst buffer provided.
    /// If the dst buffer does not have the capacity to hold `len(plaintext) + 14` bytes, a new one will be allocated and returned.
    pub fn encrypt_rtcp(&mut self, decrypted: &[u8]) -> Result<BytesMut> {
        if decrypted.len() < 8 {
            return Err(Error::ErrTooShortRtcp);
        }

        let mut buf = decrypted;
        rtcp::Header::unmarshal(&mut buf)?;

        let ssrc = u32::from_be_bytes([decrypted[4], decrypted[5], decrypted[6], decrypted[7]]);

        let index = {
            let state = self.get_srtcp_ssrc_state(ssrc);
            state.srtcp_index += 1;
            if state.srtcp_index > MAX_SRTCP_INDEX {
                state.srtcp_index = 0;
            }
            state.srtcp_index
        };

        self.cipher.encrypt_rtcp(decrypted, index, ssrc)
    }
}
