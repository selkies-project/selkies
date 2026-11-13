/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The two primitives `aws-lc-rs` does not expose, called in AWS-LC itself through
//! `aws-lc-sys`: AES-128-CCM for the DTLS CCM suites, and MD5 for the STUN long-term
//! credential key (RFC 8489 section 9.2.2).
//!
//! `aws-lc-sys` is the library `aws-lc-rs` links, at the version it resolves, so these calls
//! reach the same AWS-LC as every other primitive of the provider. This is the only module of
//! the provider with `unsafe` code.
//!
//! AWS-LC's one-call CCM (`EVP_aead_aes_128_ccm_*`) is fixed to the 13-byte nonces of
//! Bluetooth and Matter, and DTLS uses 12, so [`Ccm`] drives the `EVP_CIPHER` instead. That
//! cipher context holds the expanded key: [`Ccm::create`] keys it once, and a record only
//! re-arms it with its nonce and direction, with no allocation and no key schedule.

use std::ffi::{c_int, c_void};
use std::ptr::{self, NonNull};

use aws_lc_sys::{
    EVP_CIPHER_CTX, EVP_CIPHER_CTX_ctrl, EVP_CIPHER_CTX_free, EVP_CIPHER_CTX_new,
    EVP_CTRL_AEAD_GET_TAG, EVP_CTRL_AEAD_SET_IVLEN, EVP_CTRL_AEAD_SET_TAG, EVP_CipherInit_ex,
    EVP_CipherUpdate, EVP_aes_128_ccm, MD5,
};
use rtc_crypto::{AeadCipher, CryptoError};

use super::{check_key_len, check_nonce_len, check_tag_len};

/// The CCM nonce length of the DTLS suites (RFC 6655 section 3).
const CCM_NONCE_LEN: usize = 12;

/// A 12-byte nonce leaves a three-byte length field, so a message is under 2^24 bytes.
const CCM_MAX_MESSAGE_LEN: usize = (1 << 24) - 1;

/// The MD5 digest of `data`.
pub(super) fn md5(data: &[u8]) -> [u8; 16] {
    let mut digest = [0u8; 16];
    // SAFETY: `data` is readable for its length and `digest` is the 16 bytes MD5 writes.
    unsafe { MD5(data.as_ptr(), data.len(), digest.as_mut_ptr()) };
    digest
}

/// AES-128-CCM with a 12-byte nonce and a 16- or 8-byte tag, under one key.
pub(super) struct Ccm {
    /// The keyed context; each message sets its nonce and direction on it.
    context: NonNull<EVP_CIPHER_CTX>,
    tag_len: usize,
}

// SAFETY: the context is owned by this value alone, AWS-LC ties a cipher context to no
// thread, and every use goes through `&mut self`.
unsafe impl Send for Ccm {}

impl Drop for Ccm {
    fn drop(&mut self) {
        // SAFETY: `context` came from `EVP_CIPHER_CTX_new` and is freed once, here; freeing
        // it also clears the key.
        unsafe { EVP_CIPHER_CTX_free(self.context.as_ptr()) };
    }
}

fn failed(what: &str) -> CryptoError {
    CryptoError::Provider(format!("AES-CCM {what} failed"))
}

/// `Ok` when an AWS-LC call returned its success value of one.
fn check(result: c_int, what: &str) -> Result<(), CryptoError> {
    if result == 1 {
        Ok(())
    } else {
        Err(failed(what))
    }
}

/// A length as the `int` the `EVP_CIPHER` interface counts in.
fn c_len(len: usize) -> Result<c_int, CryptoError> {
    c_int::try_from(len).map_err(|_| failed("length"))
}

impl Ccm {
    /// A cipher for `key` whose tags are `tag_len` bytes.
    pub(super) fn create(key: &[u8], tag_len: usize) -> Result<Box<dyn AeadCipher>, CryptoError> {
        check_key_len(16, key.len())?;
        // SAFETY: no preconditions; a null return is handled.
        let context =
            NonNull::new(unsafe { EVP_CIPHER_CTX_new() }).ok_or_else(|| failed("context"))?;
        let cipher = Self { context, tag_len };
        let context = cipher.context.as_ptr();
        // The nonce and tag lengths shape the key's CCM state, so they are set before the key.
        // SAFETY: `context` is live; `key` is the 16 bytes AES-128 reads; the lengths are
        // passed by value with no pointer argument.
        unsafe {
            check(
                EVP_CipherInit_ex(
                    context,
                    EVP_aes_128_ccm(),
                    ptr::null_mut(),
                    ptr::null(),
                    ptr::null(),
                    1,
                ),
                "setup",
            )?;
            check(
                EVP_CIPHER_CTX_ctrl(
                    context,
                    EVP_CTRL_AEAD_SET_IVLEN,
                    c_len(CCM_NONCE_LEN)?,
                    ptr::null_mut(),
                ),
                "nonce length",
            )?;
            check(
                EVP_CIPHER_CTX_ctrl(
                    context,
                    EVP_CTRL_AEAD_SET_TAG,
                    c_len(tag_len)?,
                    ptr::null_mut(),
                ),
                "tag length",
            )?;
            check(
                EVP_CipherInit_ex(
                    context,
                    ptr::null(),
                    ptr::null_mut(),
                    key.as_ptr(),
                    ptr::null(),
                    -1,
                ),
                "key",
            )?;
        }
        Ok(Box::new(cipher))
    }

    /// Run one message through the context: its nonce and direction, the tag to check when
    /// opening, the message length CCM authenticates up front, the associated data, and the
    /// message itself in place.
    ///
    /// The associated data is passed even when empty, because that call is what starts the
    /// CBC-MAC afresh after a message that failed.
    fn process(
        &mut self,
        seal: bool,
        nonce: &[u8],
        aad: &[u8],
        in_out: &mut [u8],
        expected_tag: Option<&[u8]>,
    ) -> Result<(), CryptoError> {
        check_nonce_len(CCM_NONCE_LEN, nonce.len())?;
        if in_out.len() > CCM_MAX_MESSAGE_LEN {
            return Err(failed("length"));
        }
        let context = self.context.as_ptr();
        let message_len = c_len(in_out.len())?;
        let aad_len = c_len(aad.len())?;
        let mut written: c_int = 0;
        // SAFETY: `context` is live and keyed for 12-byte nonces and `tag_len`-byte tags;
        // `nonce` is 12 bytes; `expected_tag`, when present, is `tag_len` bytes, which AWS-LC
        // copies and does not write to; `aad` and `in_out` are valid for the lengths passed,
        // and CCM encrypts in place.
        unsafe {
            check(
                EVP_CipherInit_ex(
                    context,
                    ptr::null(),
                    ptr::null_mut(),
                    ptr::null(),
                    nonce.as_ptr(),
                    c_int::from(seal),
                ),
                "nonce",
            )?;
            if let Some(tag) = expected_tag {
                check(
                    EVP_CIPHER_CTX_ctrl(
                        context,
                        EVP_CTRL_AEAD_SET_TAG,
                        c_len(tag.len())?,
                        tag.as_ptr().cast_mut().cast::<c_void>(),
                    ),
                    "tag",
                )?;
            }
            check(
                EVP_CipherUpdate(
                    context,
                    ptr::null_mut(),
                    &raw mut written,
                    ptr::null(),
                    message_len,
                ),
                "length",
            )?;
            check(
                EVP_CipherUpdate(
                    context,
                    ptr::null_mut(),
                    &raw mut written,
                    aad.as_ptr(),
                    aad_len,
                ),
                "associated data",
            )?;
            if EVP_CipherUpdate(
                context,
                in_out.as_mut_ptr(),
                &raw mut written,
                in_out.as_ptr(),
                message_len,
            ) != 1
            {
                // Sealing a well-formed message cannot fail; opening fails on the tag.
                return Err(CryptoError::AuthenticationFailed);
            }
        }
        Ok(())
    }
}

impl AeadCipher for Ccm {
    fn tag_len(&self) -> usize {
        self.tag_len
    }

    fn seal_in_place(
        &mut self,
        nonce: &[u8],
        aad: &[u8],
        plaintext_and_ciphertext: &mut [u8],
        tag_out: &mut [u8],
    ) -> Result<(), CryptoError> {
        check_tag_len(self.tag_len, tag_out.len())?;
        self.process(true, nonce, aad, plaintext_and_ciphertext, None)?;
        // SAFETY: `context` is live and has just sealed a message; `tag_out` is the
        // `tag_len` bytes the context was set up to produce.
        check(
            unsafe {
                EVP_CIPHER_CTX_ctrl(
                    self.context.as_ptr(),
                    EVP_CTRL_AEAD_GET_TAG,
                    c_len(self.tag_len)?,
                    tag_out.as_mut_ptr().cast::<c_void>(),
                )
            },
            "tag",
        )
    }

    fn open_in_place(
        &mut self,
        nonce: &[u8],
        aad: &[u8],
        ciphertext_and_plaintext: &mut [u8],
        tag: &[u8],
    ) -> Result<(), CryptoError> {
        check_tag_len(self.tag_len, tag.len())?;
        self.process(false, nonce, aad, ciphertext_and_plaintext, Some(tag))
    }
}
