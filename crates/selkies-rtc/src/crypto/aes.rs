/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Unauthenticated AES for the provider, through `aws_lc_rs::cipher`: the single block SRTP
//! derives its session keys with, the counter mode of the AES-CM SRTP profiles, and the CBC
//! of the DTLS CBC suite.
//!
//! An `aws_lc_rs` cipher key is the expanded AES key, and encrypting with it builds nothing:
//! the IV travels on the stack. [`AesCtr`] and [`AesCbc`] therefore hold their keys from
//! construction, once per SRTP or DTLS session, and a packet pays for its own blocks alone.
//! [`block_encrypt`] takes the raw key on every call, as the provider interface has it, and
//! expands it each time; only SRTP's key derivation calls it, a few blocks per session.

use aws_lc_rs::cipher::{
    AES_128, AES_256, Algorithm, DecryptingKey, DecryptionContext, EncryptingKey,
    EncryptionContext, UnboundCipherKey,
};
use aws_lc_rs::iv::FixedLength;
use rtc_crypto::{
    BlockCipherAlgorithm, CbcAlgorithm, CbcCipher, CryptoAlgorithm, CryptoError, StreamCipher,
    StreamCipherAlgorithm,
};

use super::{check_key_len, check_nonce_len};

/// The AES block length, which is also the length of a CTR or CBC IV.
const BLOCK_LEN: usize = 16;

/// The expanded key for `key`, which must be the `expected` bytes the algorithm takes.
fn cipher_key(
    algorithm: &'static Algorithm,
    expected: usize,
    key: &[u8],
) -> Result<UnboundCipherKey, CryptoError> {
    check_key_len(expected, key.len())?;
    UnboundCipherKey::new(algorithm, key).map_err(|_| CryptoError::InvalidKeyLength {
        expected,
        actual: key.len(),
    })
}

/// `iv` as the context an `aws_lc_rs` cipher takes.
fn iv_bytes(iv: &[u8]) -> Result<[u8; BLOCK_LEN], CryptoError> {
    check_nonce_len(BLOCK_LEN, iv.len())?;
    let mut bytes = [0u8; BLOCK_LEN];
    bytes.copy_from_slice(iv);
    Ok(bytes)
}

fn failed(what: &str) -> CryptoError {
    CryptoError::Provider(format!("{what} failed"))
}

/// Encrypt the one block in `block` in place under `key`.
pub(super) fn block_encrypt(
    algorithm: BlockCipherAlgorithm,
    key: &[u8],
    block: &mut [u8],
) -> Result<(), CryptoError> {
    if block.len() != BLOCK_LEN {
        return Err(CryptoError::OutputTooSmall {
            required: BLOCK_LEN,
            actual: block.len(),
        });
    }
    let (backend, key_len) = match algorithm {
        BlockCipherAlgorithm::Aes128 => (&AES_128, 16),
        BlockCipherAlgorithm::Aes256 => (&AES_256, 32),
        _ => {
            return Err(CryptoError::UnsupportedAlgorithm(
                CryptoAlgorithm::BlockCipher(algorithm),
            ));
        }
    };
    EncryptingKey::ecb(cipher_key(backend, key_len, key)?)
        .and_then(|key| key.less_safe_encrypt(block, EncryptionContext::None))
        .map(|_| ())
        .map_err(|_| failed("AES block encryption"))
}

/// AES in counter mode for `key`.
pub(super) fn new_ctr(
    algorithm: StreamCipherAlgorithm,
    key: &[u8],
) -> Result<Box<dyn StreamCipher>, CryptoError> {
    let (backend, key_len) = match algorithm {
        StreamCipherAlgorithm::Aes128Ctr => (&AES_128, 16),
        StreamCipherAlgorithm::Aes256Ctr => (&AES_256, 32),
        _ => {
            return Err(CryptoError::UnsupportedAlgorithm(
                CryptoAlgorithm::StreamCipher(algorithm),
            ));
        }
    };
    let key = EncryptingKey::ctr(cipher_key(backend, key_len, key)?)
        .map_err(|_| failed("AES-CTR key"))?;
    Ok(Box::new(AesCtr { key }))
}

/// AES-256-CBC for `key`.
pub(super) fn new_cbc(
    algorithm: CbcAlgorithm,
    key: &[u8],
) -> Result<Box<dyn CbcCipher>, CryptoError> {
    match algorithm {
        CbcAlgorithm::Aes256Cbc => {}
        _ => {
            return Err(CryptoError::UnsupportedAlgorithm(CryptoAlgorithm::Cbc(
                algorithm,
            )));
        }
    }
    Ok(Box::new(AesCbc {
        encrypting: EncryptingKey::cbc(cipher_key(&AES_256, 32, key)?)
            .map_err(|_| failed("AES-CBC key"))?,
        decrypting: DecryptingKey::cbc(cipher_key(&AES_256, 32, key)?)
            .map_err(|_| failed("AES-CBC key"))?,
    }))
}

/// AES counter mode with a 128-bit big-endian counter, which is SRTP's AES-CM (RFC 3711
/// section 4.1.1): the IV is the first counter block.
struct AesCtr {
    key: EncryptingKey,
}

impl StreamCipher for AesCtr {
    fn apply_keystream(&mut self, iv: &[u8], data: &mut [u8]) -> Result<(), CryptoError> {
        let context = EncryptionContext::Iv128(FixedLength::from(iv_bytes(iv)?));
        self.key
            .less_safe_encrypt(data, context)
            .map(|_| ())
            .map_err(|_| failed("AES-CTR"))
    }
}

/// AES-256-CBC over whole blocks with no padding: DTLS pads its records itself.
struct AesCbc {
    encrypting: EncryptingKey,
    decrypting: DecryptingKey,
}

/// Refuse anything but a positive whole number of blocks.
fn check_blocks(blocks: &[u8]) -> Result<(), CryptoError> {
    if blocks.is_empty() || !blocks.len().is_multiple_of(BLOCK_LEN) {
        return Err(CryptoError::OutputTooSmall {
            required: blocks.len().next_multiple_of(BLOCK_LEN).max(BLOCK_LEN),
            actual: blocks.len(),
        });
    }
    Ok(())
}

impl CbcCipher for AesCbc {
    fn block_len(&self) -> usize {
        BLOCK_LEN
    }

    fn encrypt_blocks(&mut self, iv: &[u8], blocks: &mut [u8]) -> Result<(), CryptoError> {
        let context = EncryptionContext::Iv128(FixedLength::from(iv_bytes(iv)?));
        check_blocks(blocks)?;
        self.encrypting
            .less_safe_encrypt(blocks, context)
            .map(|_| ())
            .map_err(|_| failed("AES-CBC encryption"))
    }

    fn decrypt_blocks(&mut self, iv: &[u8], blocks: &mut [u8]) -> Result<(), CryptoError> {
        let context = DecryptionContext::Iv128(FixedLength::from(iv_bytes(iv)?));
        check_blocks(blocks)?;
        self.decrypting
            .decrypt(blocks, context)
            .map(|_| ())
            .map_err(|_| failed("AES-CBC decryption"))
    }
}
