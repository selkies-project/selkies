use super::*;
use shared::{
    error::{Error, Result},
    marshal::{MarshalSize, Unmarshal},
};

use bytes::BytesMut;

impl Context {
    /// Decrypts an SRTP packet whose header has already been parsed.
    ///
    /// Saves re-parsing when the caller needed the header to route the packet. The header must
    /// be the one belonging to `encrypted`.
    ///
    /// # Errors
    ///
    /// Fails if authentication fails, if the packet is a replay, or if it is too short to hold
    /// the profile's auth tag.
    pub fn decrypt_rtp_with_header(
        &mut self,
        encrypted: &[u8],
        header: &rtp::Header,
    ) -> Result<BytesMut> {
        let auth_tag_len = self.cipher.rtp_auth_tag_len();
        if encrypted.len() < header.marshal_size() + auth_tag_len {
            return Err(Error::ErrTooShortRtp);
        }

        let state = self.get_srtp_ssrc_state(header.ssrc);
        let (roc, diff, _) = state.next_rollover_count(header.sequence_number);
        if let Some(replay_detector) = &mut state.replay_detector
            && !replay_detector.check(header.sequence_number as u64)
        {
            return Err(Error::SrtpSsrcDuplicated(
                header.ssrc,
                header.sequence_number,
            ));
        }

        let dst = self.cipher.decrypt_rtp(encrypted, header, roc)?;
        {
            let state = self.get_srtp_ssrc_state(header.ssrc);
            if let Some(replay_detector) = &mut state.replay_detector {
                replay_detector.accept();
            }
            state.update_rollover_count(header.sequence_number, diff);
        }

        Ok(dst)
    }

    /// DecryptRTP decrypts a RTP packet with an encrypted payload
    pub fn decrypt_rtp(&mut self, encrypted: &[u8]) -> Result<BytesMut> {
        let mut buf = encrypted;
        let header = rtp::Header::unmarshal(&mut buf)?;
        self.decrypt_rtp_with_header(encrypted, &header)
    }

    /// Encrypts an RTP payload, using an already-parsed header.
    ///
    /// Saves re-parsing when the caller has just built the header. Returns the full protected
    /// packet, header included.
    ///
    /// # Errors
    ///
    /// Fails if the SRTP context has no key for this SSRC or the cipher rejects the input.
    pub fn encrypt_rtp_with_header(
        &mut self,
        plaintext: &[u8],
        header: &rtp::Header,
    ) -> Result<BytesMut> {
        let (roc, diff, ovf) = self
            .get_srtp_ssrc_state(header.ssrc)
            .next_rollover_count(header.sequence_number);
        if ovf {
            // ... when 2^48 SRTP packets or 2^31 SRTCP packets have been secured with the same key
            // (whichever occurs before), the key management MUST be called to provide new master key(s)
            // (previously stored and used keys MUST NOT be used again), or the session MUST be terminated.
            // https://www.rfc-editor.org/rfc/rfc3711#section-9.2
            return Err(Error::ErrExceededMaxPackets);
        }

        let dst = self.cipher.encrypt_rtp(plaintext, header, roc)?;

        self.get_srtp_ssrc_state(header.ssrc)
            .update_rollover_count(header.sequence_number, diff);

        Ok(dst)
    }

    /// Encrypts an RTP packet where it lies: `packet` grows by the profile's auth tag, without
    /// moving when its capacity has room for it.
    ///
    /// # Errors
    ///
    /// Fails if the packet's header does not parse, the SRTP context has no key for its SSRC,
    /// or the cipher rejects it.
    pub fn encrypt_rtp_in_place(&mut self, packet: &mut BytesMut) -> Result<()> {
        let header = rtp::Header::unmarshal(&mut &packet[..])?;
        let (roc, diff, ovf) = self
            .get_srtp_ssrc_state(header.ssrc)
            .next_rollover_count(header.sequence_number);
        if ovf {
            return Err(Error::ErrExceededMaxPackets);
        }

        self.cipher.encrypt_rtp_in_place(packet, &header, roc)?;

        self.get_srtp_ssrc_state(header.ssrc)
            .update_rollover_count(header.sequence_number, diff);

        Ok(())
    }

    /// EncryptRTP marshals and encrypts an RTP packet, writing to the dst buffer provided.
    /// If the dst buffer does not have the capacity to hold `len(plaintext) + 10` bytes, a new one will be allocated and returned.
    pub fn encrypt_rtp(&mut self, plaintext: &[u8]) -> Result<BytesMut> {
        let mut buf = plaintext;
        let header = rtp::Header::unmarshal(&mut buf)?;
        self.encrypt_rtp_with_header(plaintext, &header)
    }
}
