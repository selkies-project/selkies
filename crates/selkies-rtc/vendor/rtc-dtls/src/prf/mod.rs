#[cfg(test)]
mod prf_test;

use std::fmt;

use crypto::{HashAlgorithm as CryptoHashAlgorithm, HmacAlgorithm, Mac, RTCCrypto};

use crate::cipher_suite::CipherSuiteHash;
use crate::content::ContentType;
use crate::curve::named_curve::*;
use crate::record_layer::record_layer_header::ProtocolVersion;
use shared::error::*;

pub(crate) const PRF_MASTER_SECRET_LABEL: &str = "master secret";
pub(crate) const PRF_EXTENDED_MASTER_SECRET_LABEL: &str = "extended master secret";
pub(crate) const PRF_KEY_EXPANSION_LABEL: &str = "key expansion";
pub(crate) const PRF_VERIFY_DATA_CLIENT_LABEL: &str = "client finished";
pub(crate) const PRF_VERIFY_DATA_SERVER_LABEL: &str = "server finished";

#[derive(PartialEq, Debug, Clone)]
pub(crate) struct EncryptionKeys {
    pub(crate) master_secret: Vec<u8>,
    pub(crate) client_mac_key: Vec<u8>,
    pub(crate) server_mac_key: Vec<u8>,
    pub(crate) client_write_key: Vec<u8>,
    pub(crate) server_write_key: Vec<u8>,
    pub(crate) client_write_iv: Vec<u8>,
    pub(crate) server_write_iv: Vec<u8>,
}

impl fmt::Display for EncryptionKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = "EncryptionKeys:\n".to_string();

        out += format!("- master_secret: {:?}\n", self.master_secret).as_str();
        out += format!("- client_mackey: {:?}\n", self.client_mac_key).as_str();
        out += format!("- server_mackey: {:?}\n", self.server_mac_key).as_str();
        out += format!("- client_write_key: {:?}\n", self.client_write_key).as_str();
        out += format!("- server_write_key: {:?}\n", self.server_write_key).as_str();
        out += format!("- client_write_iv: {:?}\n", self.client_write_iv).as_str();
        out += format!("- server_write_iv: {:?}\n", self.server_write_iv).as_str();

        write!(f, "{out}")
    }
}

// The premaster secret is formed as follows: if the PSK is N octets
// long, concatenate a uint16 with the value N, N zero octets, a second
// uint16 with the value N, and the PSK itself.
//
// https://tools.ietf.org/html/rfc4279#section-2
pub(crate) fn prf_psk_pre_master_secret(psk: &[u8]) -> Vec<u8> {
    let psk_len = psk.len();

    let mut out = vec![0u8; 2 + psk_len + 2];

    out.extend_from_slice(psk);
    let be = (psk_len as u16).to_be_bytes();
    out[..2].copy_from_slice(&be);
    out[2 + psk_len..2 + psk_len + 2].copy_from_slice(&be);

    out
}

pub(crate) fn prf_pre_master_secret(
    public_key: &[u8],
    keypair: &mut NamedCurveKeypair,
    curve: NamedCurve,
) -> Result<Vec<u8>> {
    if keypair.curve != curve {
        return Err(Error::ErrNamedCurveAndPrivateKeyMismatch);
    }
    keypair.complete(public_key)
}

//  This PRF with the SHA-256 hash function is used for all cipher suites
//  defined in this document and in TLS documents published prior to this
//  document when TLS 1.2 is negotiated.  New cipher suites MUST explicitly
//  specify a PRF and, in general, SHOULD use the TLS PRF with SHA-256 or a
//  stronger standard hash function.
//
//     P_hash(secret, seed) = HMAC_hash(secret, A(1) + seed) +
//                            HMAC_hash(secret, A(2) + seed) +
//                            HMAC_hash(secret, A(3) + seed) + ...
//
//  A() is defined as:
//
//     A(0) = seed
//     A(i) = HMAC_hash(secret, A(i-1))
//
//  P_hash can be iterated as many times as necessary to produce the
//  required quantity of data.  For example, if P_SHA256 is being used to
//  create 80 bytes of data, it will have to be iterated three times
//  (through A(3)), creating 96 bytes of output data; the last 16 bytes
//  of the final iteration will then be discarded, leaving 80 bytes of
//  output data.
//
// https://tools.ietf.org/html/rfc4346w
fn hmac_sha(
    crypto: &dyn RTCCrypto,
    h: CipherSuiteHash,
    key: &[u8],
    input: &[&[u8]],
) -> Result<Vec<u8>> {
    let algorithm = match h {
        CipherSuiteHash::Sha256 => HmacAlgorithm::Sha256,
    };
    let mut output = vec![0; algorithm.output_len()];
    crypto
        .new_hmac(algorithm, key)
        .and_then(|mut mac| mac.sign(input, &mut output))
        .map_err(|error| Error::Crypto(error.to_string()))?;
    Ok(output)
}

pub(crate) fn prf_p_hash(
    crypto: &dyn RTCCrypto,
    secret: &[u8],
    seed: &[u8],
    requested_length: usize,
    h: CipherSuiteHash,
) -> Result<Vec<u8>> {
    let mut last_round = seed.to_vec();
    let mut out = vec![];

    let iterations = ((requested_length as f64) / (h.size() as f64)).ceil() as usize;
    for _ in 0..iterations {
        last_round = hmac_sha(crypto, h, secret, &[&last_round])?;
        let with_secret = hmac_sha(crypto, h, secret, &[&last_round, seed])?;

        out.extend_from_slice(&with_secret);
    }

    Ok(out[..requested_length].to_vec())
}

pub(crate) fn prf_extended_master_secret(
    crypto: &dyn RTCCrypto,
    pre_master_secret: &[u8],
    session_hash: &[u8],
    h: CipherSuiteHash,
) -> Result<Vec<u8>> {
    let mut seed = PRF_EXTENDED_MASTER_SECRET_LABEL.as_bytes().to_vec();
    seed.extend_from_slice(session_hash);
    prf_p_hash(crypto, pre_master_secret, &seed, 48, h)
}

pub(crate) fn prf_master_secret(
    crypto: &dyn RTCCrypto,
    pre_master_secret: &[u8],
    client_random: &[u8],
    server_random: &[u8],
    h: CipherSuiteHash,
) -> Result<Vec<u8>> {
    let mut seed = PRF_MASTER_SECRET_LABEL.as_bytes().to_vec();
    seed.extend_from_slice(client_random);
    seed.extend_from_slice(server_random);
    prf_p_hash(crypto, pre_master_secret, &seed, 48, h)
}

pub(crate) struct EncryptionKeyLengths {
    pub(crate) mac: usize,
    pub(crate) key: usize,
    pub(crate) iv: usize,
}

pub(crate) fn prf_encryption_keys(
    crypto: &dyn RTCCrypto,
    master_secret: &[u8],
    client_random: &[u8],
    server_random: &[u8],
    lengths: EncryptionKeyLengths,
    h: CipherSuiteHash,
) -> Result<EncryptionKeys> {
    let mut seed = PRF_KEY_EXPANSION_LABEL.as_bytes().to_vec();
    seed.extend_from_slice(server_random);
    seed.extend_from_slice(client_random);

    let material = prf_p_hash(
        crypto,
        master_secret,
        &seed,
        (2 * lengths.mac) + (2 * lengths.key) + (2 * lengths.iv),
        h,
    )?;
    let mut key_material = &material[..];

    let client_mac_key = key_material[..lengths.mac].to_vec();
    key_material = &key_material[lengths.mac..];

    let server_mac_key = key_material[..lengths.mac].to_vec();
    key_material = &key_material[lengths.mac..];

    let client_write_key = key_material[..lengths.key].to_vec();
    key_material = &key_material[lengths.key..];

    let server_write_key = key_material[..lengths.key].to_vec();
    key_material = &key_material[lengths.key..];

    let client_write_iv = key_material[..lengths.iv].to_vec();
    key_material = &key_material[lengths.iv..];

    let server_write_iv = key_material[..lengths.iv].to_vec();

    Ok(EncryptionKeys {
        master_secret: master_secret.to_vec(),
        client_mac_key,
        server_mac_key,
        client_write_key,
        server_write_key,
        client_write_iv,
        server_write_iv,
    })
}

pub(crate) fn prf_verify_data(
    crypto: &dyn RTCCrypto,
    master_secret: &[u8],
    handshake_bodies: &[u8],
    label: &str,
    h: CipherSuiteHash,
) -> Result<Vec<u8>> {
    let result = match h {
        CipherSuiteHash::Sha256 => crypto
            .hash(CryptoHashAlgorithm::Sha256, handshake_bodies)
            .map_err(|error| Error::Crypto(error.to_string()))?,
    };
    let mut seed = label.as_bytes().to_vec();
    seed.extend_from_slice(&result);

    prf_p_hash(crypto, master_secret, &seed, 12, h)
}

pub(crate) fn prf_verify_data_client(
    crypto: &dyn RTCCrypto,
    master_secret: &[u8],
    handshake_bodies: &[u8],
    h: CipherSuiteHash,
) -> Result<Vec<u8>> {
    prf_verify_data(
        crypto,
        master_secret,
        handshake_bodies,
        PRF_VERIFY_DATA_CLIENT_LABEL,
        h,
    )
}

pub(crate) fn prf_verify_data_server(
    crypto: &dyn RTCCrypto,
    master_secret: &[u8],
    handshake_bodies: &[u8],
    h: CipherSuiteHash,
) -> Result<Vec<u8>> {
    prf_verify_data(
        crypto,
        master_secret,
        handshake_bodies,
        PRF_VERIFY_DATA_SERVER_LABEL,
        h,
    )
}

// compute the MAC using HMAC-SHA1
/// Computes the TLS 1.2 record MAC (RFC 5246 section 6.2.3.1) with an already-keyed MAC.
///
/// Takes `&mut dyn Mac` rather than a crypto handle plus raw key bytes so the caller keys once
/// per epoch. Re-deriving the HMAC key schedule per record measured ~2x on the CBC record path.
pub(crate) fn prf_mac(
    mac: &mut dyn Mac,
    epoch: u16,
    sequence_number: u64,
    content_type: ContentType,
    protocol_version: ProtocolVersion,
    payload: &[u8],
) -> Result<Vec<u8>> {
    let mut msg = vec![0u8; 13];
    msg[..2].copy_from_slice(&epoch.to_be_bytes());
    msg[2..8].copy_from_slice(&sequence_number.to_be_bytes()[2..]);
    msg[8] = content_type as u8;
    msg[9] = protocol_version.major;
    msg[10] = protocol_version.minor;
    msg[11..].copy_from_slice(&(payload.len() as u16).to_be_bytes());

    let mut output = vec![0; mac.output_len()];
    mac.sign(&[&msg, payload], &mut output)
        .map_err(|error| Error::Crypto(error.to_string()))?;
    Ok(output)
}
