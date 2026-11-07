/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! SRTP and SRTCP keyed from the DTLS handshake (RFC 5764).
//!
//! The negotiated profile fixes the key and salt lengths; the DTLS exporter yields
//! `client_key | server_key | client_salt | server_salt`, and the DTLS server protects with the
//! server half and unprotects with the client half, the client the other way round. The
//! receive contexts run a 1024-packet replay window for RTP and RTCP, wide enough that a burst
//! reordered behind a standing queue is not rejected; the send contexts check no replay, since a
//! retransmission under its original sequence number is a legitimate send. Every SSRC gets its
//! own rollover counter inside the contexts, so one pair of contexts serves every stream the
//! transport bundles.

use bytes::BytesMut;
use rtc_dtls::extension::extension_use_srtp::SrtpProtectionProfile;
use rtc_srtp::config::Config;
use rtc_srtp::context::Context;
use rtc_srtp::option::{srtcp_replay_protection, srtp_replay_protection};
use rtc_srtp::protection_profile::ProtectionProfile;

use crate::ice::stun::crypto;

/// The RFC 5764 exporter label.
pub const KEYING_LABEL: &str = rtc_srtp::config::LABEL_EXTRACTOR_DTLS_SRTP;
/// The replay window of the receive contexts.
pub const RX_REPLAY_WINDOW: usize = 1024;

/// What SRTP fails on.
#[derive(Debug, thiserror::Error)]
pub enum SrtpError {
    #[error("SRTP keying: {0}")]
    Keying(String),
    #[error("SRTP protect failed: {0}")]
    Protect(String),
    #[error("SRTP unprotect failed: {0}")]
    Unprotect(String),
}

/// The protection profiles this transport negotiates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SrtpProfile {
    AeadAes256Gcm,
    AeadAes128Gcm,
    Aes128CmHmacSha1_80,
}

impl SrtpProfile {
    /// The profile DTLS negotiated, or `None` for one this transport does not run.
    pub fn from_dtls(profile: SrtpProtectionProfile) -> Option<SrtpProfile> {
        match profile {
            SrtpProtectionProfile::Srtp_Aead_Aes_256_Gcm => Some(SrtpProfile::AeadAes256Gcm),
            SrtpProtectionProfile::Srtp_Aead_Aes_128_Gcm => Some(SrtpProfile::AeadAes128Gcm),
            SrtpProtectionProfile::Srtp_Aes128_Cm_Hmac_Sha1_80 => {
                Some(SrtpProfile::Aes128CmHmacSha1_80)
            }
            _ => None,
        }
    }

    fn rtc(self) -> ProtectionProfile {
        match self {
            SrtpProfile::AeadAes256Gcm => ProtectionProfile::AeadAes256Gcm,
            SrtpProfile::AeadAes128Gcm => ProtectionProfile::AeadAes128Gcm,
            SrtpProfile::Aes128CmHmacSha1_80 => ProtectionProfile::Aes128CmHmacSha1_80,
        }
    }

    /// The profile's name as `use_srtp` spells it.
    pub fn name(self) -> &'static str {
        match self {
            SrtpProfile::AeadAes256Gcm => "SRTP_AEAD_AES_256_GCM",
            SrtpProfile::AeadAes128Gcm => "SRTP_AEAD_AES_128_GCM",
            SrtpProfile::Aes128CmHmacSha1_80 => "SRTP_AES128_CM_SHA1_80",
        }
    }

    /// The master key length.
    pub fn key_len(self) -> usize {
        self.rtc().key_len()
    }

    /// The master salt length.
    pub fn salt_len(self) -> usize {
        self.rtc().salt_len()
    }

    /// The exporter bytes the profile needs: two keys and two salts.
    pub fn keying_material_len(self) -> usize {
        2 * (self.key_len() + self.salt_len())
    }

    /// The bytes a protected RTP packet grows by.
    pub fn rtp_tag_len(self) -> usize {
        let profile = self.rtc();
        profile.rtp_auth_tag_len() + profile.aead_auth_tag_len()
    }
}

/// Whether `data`, known to be RTP or RTCP, is RTCP: its packet type is in 192-223.
pub fn is_rtcp(data: &[u8]) -> bool {
    rtc_shared::util::is_rtcp(data)
}

/// One direction's contexts: protect and unprotect are never mixed on one context.
pub struct SrtpContext {
    inner: Context,
}

impl SrtpContext {
    /// Protect one RTP packet; the result is the packet plus its tag.
    pub fn protect_rtp(&mut self, packet: &[u8]) -> Result<BytesMut, SrtpError> {
        self.inner
            .encrypt_rtp(packet)
            .map_err(|e| SrtpError::Protect(e.to_string()))
    }

    /// Protect one RTCP packet.
    pub fn protect_rtcp(&mut self, packet: &[u8]) -> Result<BytesMut, SrtpError> {
        self.inner
            .encrypt_rtcp(packet)
            .map_err(|e| SrtpError::Protect(e.to_string()))
    }

    /// Unprotect one SRTP packet.
    pub fn unprotect_rtp(&mut self, packet: &[u8]) -> Result<BytesMut, SrtpError> {
        self.inner
            .decrypt_rtp(packet)
            .map_err(|e| SrtpError::Unprotect(e.to_string()))
    }

    /// Unprotect one SRTCP packet.
    pub fn unprotect_rtcp(&mut self, packet: &[u8]) -> Result<BytesMut, SrtpError> {
        self.inner
            .decrypt_rtcp(packet)
            .map_err(|e| SrtpError::Unprotect(e.to_string()))
    }
}

/// The two directions of one DTLS-SRTP session.
pub struct SrtpSession {
    pub profile: SrtpProfile,
    pub rx: SrtpContext,
    pub tx: SrtpContext,
}

impl SrtpSession {
    /// Key both directions from the DTLS exporter output, which side this is deciding which
    /// half protects.
    ///
    /// # Errors
    /// The material has the wrong length for `profile`, or the crypto provider lacks the
    /// profile.
    pub fn from_keying_material(
        profile: SrtpProfile,
        material: &[u8],
        is_client: bool,
    ) -> Result<SrtpSession, SrtpError> {
        let mut config = Config {
            profile: profile.rtc(),
            ..Config::default()
        };
        config
            .set_session_keys_from_keying_material(material, is_client)
            .map_err(|e| SrtpError::Keying(e.to_string()))?;
        let provider = crypto();
        let tx = Context::new(
            &config.keys.local_master_key,
            &config.keys.local_master_salt,
            profile.rtc(),
            None,
            None,
            provider.crypto(),
        )
        .map_err(|e| SrtpError::Keying(e.to_string()))?;
        let rx = Context::new(
            &config.keys.remote_master_key,
            &config.keys.remote_master_salt,
            profile.rtc(),
            Some(srtp_replay_protection(RX_REPLAY_WINDOW)),
            Some(srtcp_replay_protection(RX_REPLAY_WINDOW)),
            provider.crypto(),
        )
        .map_err(|e| SrtpError::Keying(e.to_string()))?;
        Ok(SrtpSession {
            profile,
            rx: SrtpContext { inner: rx },
            tx: SrtpContext { inner: tx },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rtp_packet(seq: u16, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0x80, 96, 0, 0, 0, 0, 0, 1, 0x12, 0x34, 0x56, 0x78];
        p[2..4].copy_from_slice(&seq.to_be_bytes());
        p.extend_from_slice(payload);
        p
    }

    /// What one send costs in `rtc_srtp`, whose copy of the packet is the one a media send pays.
    #[test]
    #[ignore = "prints a measurement to quote; run with --release"]
    fn bench_protect_rtp() {
        for profile in [
            SrtpProfile::AeadAes256Gcm,
            SrtpProfile::AeadAes128Gcm,
            SrtpProfile::Aes128CmHmacSha1_80,
        ] {
            let material: Vec<u8> = (0..profile.keying_material_len())
                .map(|i| i as u8)
                .collect();
            let mut session = SrtpSession::from_keying_material(profile, &material, false).unwrap();
            for size in [200usize, 1200] {
                let mut packet = rtp_packet(0, &vec![0xA5; size]);
                let count: u32 = 200_000;
                let started = std::time::Instant::now();
                for seq in 0..count {
                    packet[2..4].copy_from_slice(&(seq as u16).to_be_bytes());
                    std::hint::black_box(session.tx.protect_rtp(&packet).unwrap());
                }
                let per_packet = started.elapsed() / count;
                println!("{} {size} bytes: {per_packet:?} per packet", profile.name());
            }
        }
    }

    #[test]
    fn both_directions_derive_from_one_export_in_every_profile() {
        for profile in [
            SrtpProfile::AeadAes256Gcm,
            SrtpProfile::AeadAes128Gcm,
            SrtpProfile::Aes128CmHmacSha1_80,
        ] {
            let material: Vec<u8> = (0..profile.keying_material_len())
                .map(|i| i as u8)
                .collect();
            let mut server = SrtpSession::from_keying_material(profile, &material, false).unwrap();
            let mut client = SrtpSession::from_keying_material(profile, &material, true).unwrap();
            let packet = rtp_packet(7, b"frame");
            let protected = server.tx.protect_rtp(&packet).unwrap();
            assert_eq!(protected.len(), packet.len() + profile.rtp_tag_len());
            assert_eq!(
                client.rx.unprotect_rtp(&protected).unwrap().as_ref(),
                &packet[..]
            );
            assert!(
                client.rx.unprotect_rtp(&protected).is_err(),
                "a replay is refused"
            );
            let rtcp = vec![0x80, 200, 0, 1, 0, 0, 0, 1, 0, 0, 0, 0];
            assert!(is_rtcp(&rtcp) && !is_rtcp(&packet));
            let protected = client.tx.protect_rtcp(&rtcp).unwrap();
            assert_eq!(
                server.rx.unprotect_rtcp(&protected).unwrap().as_ref(),
                &rtcp[..]
            );
            assert!(server.rx.unprotect_rtp(&packet).is_err());
        }
        assert!(
            SrtpSession::from_keying_material(SrtpProfile::AeadAes128Gcm, &[0u8; 10], true)
                .is_err()
        );
    }
}
