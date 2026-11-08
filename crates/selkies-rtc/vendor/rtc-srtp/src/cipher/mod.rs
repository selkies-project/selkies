pub mod cipher_aead_aes_gcm;
pub mod cipher_aes_cm_hmac_sha1;

use bytes::BytesMut;
use crypto::{CryptoError, RTCCrypto};

use shared::error::{Error, Result};

type Kdf = fn(&dyn RTCCrypto, u8, &[u8], &[u8], usize, usize) -> Result<Vec<u8>>;

pub(crate) fn crypto_error(error: CryptoError) -> Error {
    Error::Crypto(error.to_string())
}

///NOTE: Auth tag and AEAD auth tag are placed at the different position in SRTCP
///
///In non-AEAD cipher, the authentication tag is placed *after* the ESRTCP word
///(Encrypted-flag and SRTCP index).
///
///> AES_128_CM_HMAC_SHA1_80
///> | RTCP Header | Encrypted payload |E| SRTCP Index | Auth tag |
///>                                   ^               |----------|
///>                                   |                ^
///>                                   |                authTagLen=10
///>                                   aeadAuthTagLen=0
///
///In AEAD cipher, the AEAD authentication tag is embedded in the ciphertext.
///It is *before* the ESRTCP word (Encrypted-flag and SRTCP index).
///
///> AEAD_AES_128_GCM
///> | RTCP Header | Encrypted payload | AEAD auth tag |E| SRTCP Index |
///>                                   |---------------|               ^
///>                                    ^                              authTagLen=0
///>                                    aeadAuthTagLen=16
///
///See https://tools.ietf.org/html/rfc7714 for the full specifications.
///
/// Cipher represents a implementation of one
/// of the SRTP Specific ciphers.
pub(crate) trait Cipher: Send {
    /// Get RTP authenticated tag length.
    fn rtp_auth_tag_len(&self) -> usize;

    /// Get RTCP authenticated tag length.
    fn rtcp_auth_tag_len(&self) -> usize;

    /// Get AEAD auth key length of the cipher.
    fn aead_auth_tag_len(&self) -> usize;

    /// Retrieved RTCP index.
    fn get_rtcp_index(&self, input: &[u8]) -> usize;

    /// Encrypt an RTP packet where it lies: `packet` holds the header and the payload, and
    /// grows by the auth tag, without moving when its capacity has room for it.
    fn encrypt_rtp_in_place(
        &mut self,
        packet: &mut BytesMut,
        header: &rtp::Header,
        roc: u32,
    ) -> Result<()>;

    /// Encrypt RTP payload.
    fn encrypt_rtp(&mut self, payload: &[u8], header: &rtp::Header, roc: u32) -> Result<BytesMut> {
        let mut writer = BytesMut::with_capacity(
            payload.len() + self.rtp_auth_tag_len() + self.aead_auth_tag_len(),
        );
        writer.extend_from_slice(payload);
        self.encrypt_rtp_in_place(&mut writer, header, roc)?;
        Ok(writer)
    }

    /// Decrypt RTP payload.
    fn decrypt_rtp(&mut self, payload: &[u8], header: &rtp::Header, roc: u32) -> Result<BytesMut>;

    /// Encrypt RTCP payload.
    fn encrypt_rtcp(&mut self, payload: &[u8], srtcp_index: usize, ssrc: u32) -> Result<BytesMut>;

    /// Decrypt RTCP payload.
    fn decrypt_rtcp(&mut self, payload: &[u8], srtcp_index: usize, ssrc: u32) -> Result<BytesMut>;
}
