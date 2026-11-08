use crate::{option::*, protection_profile::*};
use shared::error::{Error, Result};

/// RFC 5764 exporter label used to derive SRTP master keys and salts from DTLS.
pub const LABEL_EXTRACTOR_DTLS_SRTP: &str = "EXTRACTOR-dtls_srtp";

/// SessionKeys bundles the keys required to setup an SRTP session
#[derive(Default, Debug, Clone)]
pub struct SessionKeys {
    /// The master key used to protect outbound packets.
    pub local_master_key: Vec<u8>,
    /// The master salt used to protect outbound packets.
    pub local_master_salt: Vec<u8>,
    /// The master key used to unprotect inbound packets.
    pub remote_master_key: Vec<u8>,
    /// The master salt used to unprotect inbound packets.
    pub remote_master_salt: Vec<u8>,
}

/// Config is used to configure a session.
/// The top-level integration exports keying material from DTLS and installs it here, or callers
/// can directly pass the keys themselves.
/// After a Config is passed to a session it must not be modified.
#[derive(Default)]
pub struct Config {
    /// The master keys and salts for both directions.
    pub keys: SessionKeys,
    /// The negotiated protection profile, which fixes the cipher and tag lengths.
    pub profile: ProtectionProfile,
    //LoggerFactory: logging.LoggerFactory
    /// List of local/remote context options.
    /// ReplayProtection is enabled on remote context by default.
    /// Default replay protection window size is 64.
    pub local_rtp_options: Option<ContextOption>,
    /// Replay-protection options for the inbound RTP context. Enabled by default.
    pub remote_rtp_options: Option<ContextOption>,

    /// Replay-protection options for the outbound RTCP context.
    pub local_rtcp_options: Option<ContextOption>,
    /// Replay-protection options for the inbound RTCP context. Enabled by default.
    pub remote_rtcp_options: Option<ContextOption>,
}

impl Config {
    /// Returns the exact number of DTLS exporter bytes required by this profile.
    #[must_use]
    pub fn keying_material_len(&self) -> usize {
        let key_len = self.profile.key_len();
        let salt_len = self.profile.salt_len();
        (key_len * 2) + (salt_len * 2)
    }

    /// Splits DTLS-SRTP exporter output into local and remote master keys and salts according to
    /// RFC 5764.
    ///
    /// # Errors
    ///
    /// Returns an error unless `keying_material` has exactly [`Self::keying_material_len`] bytes.
    pub fn set_session_keys_from_keying_material(
        &mut self,
        keying_material: &[u8],
        is_client: bool,
    ) -> Result<()> {
        let expected = self.keying_material_len();
        if keying_material.len() != expected {
            return Err(Error::Other(format!(
                "invalid DTLS-SRTP keying material length: expected {expected}, got {}",
                keying_material.len()
            )));
        }

        let key_len = self.profile.key_len();
        let salt_len = self.profile.salt_len();

        let mut offset = 0;
        let client_write_key = keying_material[offset..offset + key_len].to_vec();
        offset += key_len;

        let server_write_key = keying_material[offset..offset + key_len].to_vec();
        offset += key_len;

        let client_write_salt = keying_material[offset..offset + salt_len].to_vec();
        offset += salt_len;

        let server_write_salt = keying_material[offset..offset + salt_len].to_vec();

        if is_client {
            self.keys.local_master_key = client_write_key;
            self.keys.local_master_salt = client_write_salt;
            self.keys.remote_master_key = server_write_key;
            self.keys.remote_master_salt = server_write_salt;
        } else {
            self.keys.local_master_key = server_write_key;
            self.keys.local_master_salt = server_write_salt;
            self.keys.remote_master_key = client_write_key;
            self.keys.remote_master_salt = client_write_salt;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn material(len: usize) -> Vec<u8> {
        (0..len).map(|value| value as u8).collect()
    }

    #[test]
    fn rejects_keying_material_with_the_wrong_length() {
        let mut config = Config {
            profile: ProtectionProfile::Aes128CmHmacSha1_80,
            ..Default::default()
        };
        let expected = config.keying_material_len();

        for actual in [expected - 1, expected + 1] {
            let error = config
                .set_session_keys_from_keying_material(&material(actual), true)
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains(&format!("expected {expected}, got {actual}"))
            );
        }
    }

    #[test]
    fn assigns_client_and_server_material_by_role() -> Result<()> {
        let mut client = Config {
            profile: ProtectionProfile::Aes128CmHmacSha1_80,
            ..Default::default()
        };
        let bytes = material(client.keying_material_len());
        client.set_session_keys_from_keying_material(&bytes, true)?;

        let mut server = Config {
            profile: client.profile,
            ..Default::default()
        };
        server.set_session_keys_from_keying_material(&bytes, false)?;

        assert_eq!(client.keys.local_master_key, server.keys.remote_master_key);
        assert_eq!(
            client.keys.local_master_salt,
            server.keys.remote_master_salt
        );
        assert_eq!(client.keys.remote_master_key, server.keys.local_master_key);
        assert_eq!(
            client.keys.remote_master_salt,
            server.keys.local_master_salt
        );
        Ok(())
    }
}
