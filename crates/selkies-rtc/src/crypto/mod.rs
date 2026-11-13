/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 * This file incorporates work covered by the following copyright and
 * permission notice:
 *
 *   Copyright (c) 2021 WebRTC.rs
 *
 *   Permission is hereby granted, free of charge, to any person obtaining a copy
 *   of this software and associated documentation files (the "Software"), to deal
 *   in the Software without restriction, including without limitation the rights
 *   to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 *   copies of the Software, and to permit persons to whom the Software is
 *   furnished to do so, subject to the following conditions:
 *
 *   The above copyright notice and this permission notice shall be included in all
 *   copies or substantial portions of the Software.
 *
 *   THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 *   IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 *   FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 *   AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 *   LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 *   OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 *   SOFTWARE.
 */

//! The crypto provider the WebRTC stack runs on: every primitive and every random byte from
//! AWS-LC.
//!
//! The `rtc-*` crates take their cryptography from an `rtc_crypto::RTCCryptoProvider` the
//! caller hands them. `rtc-crypto`'s own AWS-LC-RS provider computes AES-ECB, AES-CTR,
//! AES-CBC, AES-CCM, and MD5 with the RustCrypto crates and draws random bytes from `rand`,
//! so the stack is built without it and [`AwsLcProvider`] takes its place: one cryptographic
//! library in the binary, with its constant-time and hardware-accelerated AES everywhere.
//!
//! | Primitive | Route |
//! |---|---|
//! | SHA-256, HMAC-SHA1, HMAC-SHA256 | `aws_lc_rs::digest`, `aws_lc_rs::hmac` |
//! | AES-GCM, ChaCha20-Poly1305 | `aws_lc_rs::aead` |
//! | AES single block, AES-CTR, AES-256-CBC | `aws_lc_rs::cipher` ([`aes`]) |
//! | AES-128-CCM, AES-128-CCM-8, MD5 | `aws-lc-sys`, which `aws-lc-rs` wraps but exposes neither from ([`sys`]) |
//! | ECDH, signatures | `aws_lc_rs::agreement`, `aws_lc_rs::signature` |
//! | Random bytes | `aws_lc_rs::rand` ([`fill_random`]) |
//!
//! Every keyed object holds its expanded key or cipher context for its lifetime, so a packet
//! pays for no key schedule, allocation, or lock here: an SRTP or DTLS session creates its
//! ciphers once and reuses them per packet.
//!
//! [`fill_random`] is also the source of the security-relevant values this crate draws itself
//! (ICE credentials and tie-breakers, STUN transaction ids, initial RTP sequence numbers and
//! timestamps, SSRCs) and of the ones `rtc-sctp` draws (verification tags, initial TSNs, state
//! cookies). AWS-LC serves it from a per-thread DRBG, at about 0.3 microseconds a draw on an
//! x86-64 server core. DTLS draws once per record (the explicit nonce or the CBC IV), so a
//! data channel message pays that; SRTP draws nothing per packet.

mod aes;
mod sys;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{self, KeyPair};
use aws_lc_rs::{aead, agreement, digest, hmac};
use rtc_crypto::{
    ActiveKeyExchange, AeadAlgorithm, AeadCipher, BlockCipherAlgorithm, CbcAlgorithm, CbcCipher,
    CryptoAlgorithm, CryptoError, HashAlgorithm, HmacAlgorithm, KeyExchangeAlgorithm, Mac,
    PublicKey, PublicKeyEncoding, RTCCrypto, RTCCryptoProvider, RTCRandom, SecretVec,
    SignatureScheme, SigningKey, StreamCipher, StreamCipherAlgorithm, constant_time_eq,
};

/// The nonce length of every AEAD the provider offers.
const AEAD_NONCE_LEN: usize = 12;

/// The longest HMAC tag the provider produces (SHA-256).
const MAX_HMAC_LEN: usize = 32;

/// Fill `output` from AWS-LC's random generator.
///
/// # Panics
///
/// When AWS-LC cannot produce random bytes, which it treats as fatal itself: a value drawn
/// here guards a session, and none can be made without it.
pub fn fill_random(output: &mut [u8]) {
    aws_lc_rs::rand::fill(output).expect("AWS-LC produces random bytes");
}

/// `N` random bytes from [`fill_random`].
pub fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    fill_random(&mut bytes);
    bytes
}

/// The AWS-LC provider: [`AwsLcCrypto`] and [`AwsLcRandom`].
#[derive(Default)]
pub struct AwsLcProvider {
    crypto: AwsLcCrypto,
    random: AwsLcRandom,
}

impl AwsLcProvider {
    pub const fn new() -> Self {
        Self {
            crypto: AwsLcCrypto,
            random: AwsLcRandom,
        }
    }
}

impl RTCCryptoProvider for AwsLcProvider {
    fn name(&self) -> &'static str {
        "aws-lc"
    }

    fn crypto(&self) -> &dyn RTCCrypto {
        &self.crypto
    }

    fn random(&self) -> &dyn RTCRandom {
        &self.random
    }
}

/// AWS-LC's random generator as the stack's random source.
#[derive(Default)]
pub struct AwsLcRandom;

impl RTCRandom for AwsLcRandom {
    fn fill(&self, output: &mut [u8]) -> Result<(), CryptoError> {
        aws_lc_rs::rand::fill(output).map_err(|_| CryptoError::RandomnessFailed)
    }
}

/// The operations of the stack, each on AWS-LC.
#[derive(Default)]
pub struct AwsLcCrypto;

impl RTCCrypto for AwsLcCrypto {
    fn supports(&self, algorithm: CryptoAlgorithm) -> bool {
        matches!(
            algorithm,
            CryptoAlgorithm::Hash(HashAlgorithm::Md5 | HashAlgorithm::Sha256)
                | CryptoAlgorithm::Hmac(HmacAlgorithm::Sha1 | HmacAlgorithm::Sha256)
                | CryptoAlgorithm::Aead(
                    AeadAlgorithm::Aes128Gcm
                        | AeadAlgorithm::Aes256Gcm
                        | AeadAlgorithm::Aes128Ccm
                        | AeadAlgorithm::Aes128Ccm8
                        | AeadAlgorithm::ChaCha20Poly1305,
                )
                | CryptoAlgorithm::StreamCipher(
                    StreamCipherAlgorithm::Aes128Ctr | StreamCipherAlgorithm::Aes256Ctr,
                )
                | CryptoAlgorithm::BlockCipher(
                    BlockCipherAlgorithm::Aes128 | BlockCipherAlgorithm::Aes256,
                )
                | CryptoAlgorithm::Cbc(CbcAlgorithm::Aes256Cbc)
                | CryptoAlgorithm::KeyExchange(
                    KeyExchangeAlgorithm::P256
                        | KeyExchangeAlgorithm::P384
                        | KeyExchangeAlgorithm::X25519,
                )
                | CryptoAlgorithm::Signature(
                    SignatureScheme::Ed25519
                        | SignatureScheme::EcdsaP256Sha256
                        | SignatureScheme::EcdsaP384Sha256
                        | SignatureScheme::EcdsaP384Sha384
                        | SignatureScheme::RsaPkcs1Sha1
                        | SignatureScheme::RsaPkcs1Sha256
                        | SignatureScheme::RsaPkcs1Sha384
                        | SignatureScheme::RsaPkcs1Sha512,
                )
                | CryptoAlgorithm::SigningKeyGeneration(
                    SignatureScheme::Ed25519 | SignatureScheme::EcdsaP256Sha256,
                )
                | CryptoAlgorithm::SigningKeyImport(
                    SignatureScheme::Ed25519
                        | SignatureScheme::EcdsaP256Sha256
                        | SignatureScheme::RsaPkcs1Sha256,
                )
        )
    }

    fn hash(&self, algorithm: HashAlgorithm, data: &[u8]) -> Result<Vec<u8>, CryptoError> {
        match algorithm {
            HashAlgorithm::Md5 => Ok(sys::md5(data).to_vec()),
            HashAlgorithm::Sha256 => Ok(digest::digest(&digest::SHA256, data).as_ref().to_vec()),
            _ => Err(CryptoError::UnsupportedAlgorithm(CryptoAlgorithm::Hash(
                algorithm,
            ))),
        }
    }

    fn new_hmac(&self, algorithm: HmacAlgorithm, key: &[u8]) -> Result<Box<dyn Mac>, CryptoError> {
        let backend = match algorithm {
            HmacAlgorithm::Sha1 => hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY,
            HmacAlgorithm::Sha256 => hmac::HMAC_SHA256,
            _ => {
                return Err(CryptoError::UnsupportedAlgorithm(CryptoAlgorithm::Hmac(
                    algorithm,
                )));
            }
        };
        Ok(Box::new(Hmac {
            key: hmac::Key::new(backend, key),
            output_len: algorithm.output_len(),
        }))
    }

    fn block_encrypt(
        &self,
        algorithm: BlockCipherAlgorithm,
        key: &[u8],
        block: &mut [u8],
    ) -> Result<(), CryptoError> {
        aes::block_encrypt(algorithm, key, block)
    }

    fn new_stream_cipher(
        &self,
        algorithm: StreamCipherAlgorithm,
        key: &[u8],
    ) -> Result<Box<dyn StreamCipher>, CryptoError> {
        aes::new_ctr(algorithm, key)
    }

    fn new_aead(
        &self,
        algorithm: AeadAlgorithm,
        key: &[u8],
    ) -> Result<Box<dyn AeadCipher>, CryptoError> {
        match algorithm {
            AeadAlgorithm::Aes128Ccm => sys::Ccm::create(key, 16),
            AeadAlgorithm::Aes128Ccm8 => sys::Ccm::create(key, 8),
            AeadAlgorithm::Aes128Gcm => Aead::create(&aead::AES_128_GCM, key),
            AeadAlgorithm::Aes256Gcm => Aead::create(&aead::AES_256_GCM, key),
            AeadAlgorithm::ChaCha20Poly1305 => Aead::create(&aead::CHACHA20_POLY1305, key),
            _ => Err(CryptoError::UnsupportedAlgorithm(CryptoAlgorithm::Aead(
                algorithm,
            ))),
        }
    }

    fn new_cbc(
        &self,
        algorithm: CbcAlgorithm,
        key: &[u8],
    ) -> Result<Box<dyn CbcCipher>, CryptoError> {
        aes::new_cbc(algorithm, key)
    }

    fn start_key_exchange(
        &self,
        algorithm: KeyExchangeAlgorithm,
    ) -> Result<Box<dyn ActiveKeyExchange>, CryptoError> {
        KeyExchange::start(algorithm)
    }

    fn generate_signing_key(
        &self,
        scheme: SignatureScheme,
    ) -> Result<Arc<dyn SigningKey>, CryptoError> {
        PrivateKey::generate(scheme)
    }

    fn import_signing_key(
        &self,
        scheme: SignatureScheme,
        pkcs8_der: &[u8],
    ) -> Result<Arc<dyn SigningKey>, CryptoError> {
        PrivateKey::import(scheme, pkcs8_der)
    }

    fn verify_signature(
        &self,
        scheme: SignatureScheme,
        public_key: PublicKey<'_>,
        message: &[u8],
        signature: &[u8],
    ) -> Result<(), CryptoError> {
        verify_public_key_encoding(scheme, public_key.encoding)?;
        signature::UnparsedPublicKey::new(verification_algorithm(scheme)?, public_key.bytes)
            .verify(message, signature)
            .map_err(|_| CryptoError::InvalidSignature)
    }
}

fn check_key_len(expected: usize, actual: usize) -> Result<(), CryptoError> {
    if expected == actual {
        Ok(())
    } else {
        Err(CryptoError::InvalidKeyLength { expected, actual })
    }
}

fn check_nonce_len(expected: usize, actual: usize) -> Result<(), CryptoError> {
    if expected == actual {
        Ok(())
    } else {
        Err(CryptoError::InvalidNonceLength { expected, actual })
    }
}

fn check_tag_len(expected: usize, actual: usize) -> Result<(), CryptoError> {
    if expected == actual {
        Ok(())
    } else {
        Err(CryptoError::InvalidTagLength { expected, actual })
    }
}

/// A keyed HMAC.
///
/// `hmac::Key` holds the inner and outer pad states, so a message costs a copy of them and
/// the hash of its own bytes.
struct Hmac {
    key: hmac::Key,
    output_len: usize,
}

impl Mac for Hmac {
    fn output_len(&self) -> usize {
        self.output_len
    }

    fn sign(&mut self, input: &[&[u8]], output: &mut [u8]) -> Result<(), CryptoError> {
        check_tag_len(self.output_len, output.len())?;
        let mut context = hmac::Context::with_key(&self.key);
        for part in input {
            context.update(part);
        }
        output.copy_from_slice(context.sign().as_ref());
        Ok(())
    }

    fn verify(&mut self, input: &[&[u8]], expected: &[u8]) -> Result<(), CryptoError> {
        check_tag_len(self.output_len, expected.len())?;
        let mut actual = [0u8; MAX_HMAC_LEN];
        let actual = &mut actual[..self.output_len];
        self.sign(input, actual)?;
        if constant_time_eq(actual, expected) {
            Ok(())
        } else {
            Err(CryptoError::AuthenticationFailed)
        }
    }
}

/// AES-GCM or ChaCha20-Poly1305 under one key.
struct Aead {
    key: aead::LessSafeKey,
}

impl Aead {
    fn create(
        algorithm: &'static aead::Algorithm,
        key: &[u8],
    ) -> Result<Box<dyn AeadCipher>, CryptoError> {
        check_key_len(algorithm.key_len(), key.len())?;
        let key = aead::UnboundKey::new(algorithm, key)
            .map(aead::LessSafeKey::new)
            .map_err(|_| CryptoError::InvalidPrivateKey)?;
        Ok(Box::new(Self { key }))
    }
}

fn aead_nonce(nonce: &[u8]) -> Result<aead::Nonce, CryptoError> {
    aead::Nonce::try_assume_unique_for_key(nonce).map_err(|_| CryptoError::InvalidNonceLength {
        expected: AEAD_NONCE_LEN,
        actual: nonce.len(),
    })
}

impl AeadCipher for Aead {
    fn tag_len(&self) -> usize {
        16
    }

    fn seal_in_place(
        &mut self,
        nonce: &[u8],
        aad: &[u8],
        plaintext_and_ciphertext: &mut [u8],
        tag_out: &mut [u8],
    ) -> Result<(), CryptoError> {
        check_nonce_len(AEAD_NONCE_LEN, nonce.len())?;
        check_tag_len(self.tag_len(), tag_out.len())?;
        let tag = self
            .key
            .seal_in_place_separate_tag(
                aead_nonce(nonce)?,
                aead::Aad::from(aad),
                plaintext_and_ciphertext,
            )
            .map_err(|_| CryptoError::AuthenticationFailed)?;
        tag_out.copy_from_slice(tag.as_ref());
        Ok(())
    }

    fn open_in_place(
        &mut self,
        nonce: &[u8],
        aad: &[u8],
        ciphertext_and_plaintext: &mut [u8],
        tag: &[u8],
    ) -> Result<(), CryptoError> {
        check_nonce_len(AEAD_NONCE_LEN, nonce.len())?;
        check_tag_len(self.tag_len(), tag.len())?;
        self.key
            .open_in_place_separate_tag(
                aead_nonce(nonce)?,
                aead::Aad::from(aad),
                tag,
                ciphertext_and_plaintext,
            )
            .map(|_| ())
            .map_err(|_| CryptoError::AuthenticationFailed)
    }
}

/// An ephemeral ECDH key waiting for the peer's public key.
struct KeyExchange {
    algorithm: KeyExchangeAlgorithm,
    backend_algorithm: &'static agreement::Algorithm,
    private_key: agreement::PrivateKey,
    public_key: Vec<u8>,
}

impl KeyExchange {
    fn start(algorithm: KeyExchangeAlgorithm) -> Result<Box<dyn ActiveKeyExchange>, CryptoError> {
        let backend_algorithm = match algorithm {
            KeyExchangeAlgorithm::P256 => &agreement::ECDH_P256,
            KeyExchangeAlgorithm::P384 => &agreement::ECDH_P384,
            KeyExchangeAlgorithm::X25519 => &agreement::X25519,
            _ => {
                return Err(CryptoError::UnsupportedAlgorithm(
                    CryptoAlgorithm::KeyExchange(algorithm),
                ));
            }
        };
        let private_key = agreement::PrivateKey::generate(backend_algorithm)
            .map_err(|_| CryptoError::RandomnessFailed)?;
        let public_key = private_key
            .compute_public_key()
            .map_err(|_| CryptoError::Provider("key exchange public-key generation failed".into()))?
            .as_ref()
            .to_vec();
        Ok(Box::new(Self {
            algorithm,
            backend_algorithm,
            private_key,
            public_key,
        }))
    }
}

impl ActiveKeyExchange for KeyExchange {
    fn algorithm(&self) -> KeyExchangeAlgorithm {
        self.algorithm
    }

    fn public_key(&self) -> &[u8] {
        &self.public_key
    }

    fn complete(self: Box<Self>, peer_public_key: &[u8]) -> Result<SecretVec, CryptoError> {
        let peer = agreement::UnparsedPublicKey::new(self.backend_algorithm, peer_public_key);
        agreement::agree(
            &self.private_key,
            peer,
            CryptoError::InvalidPublicKey,
            |secret| Ok(SecretVec::new(secret.to_vec())),
        )
    }
}

enum PrivateKeyKind {
    Ed25519(signature::Ed25519KeyPair),
    EcdsaP256(signature::EcdsaKeyPair),
    Rsa(signature::RsaKeyPair),
}

/// A signing key with its PKCS#8 encoding kept for export.
struct PrivateKey {
    scheme: SignatureScheme,
    kind: PrivateKeyKind,
    public_key: Vec<u8>,
    public_key_encoding: PublicKeyEncoding,
    pkcs8_der: SecretVec,
}

impl PrivateKey {
    fn generate(scheme: SignatureScheme) -> Result<Arc<dyn SigningKey>, CryptoError> {
        let rng = SystemRandom::new();
        let pkcs8 = match scheme {
            SignatureScheme::Ed25519 => signature::Ed25519KeyPair::generate_pkcs8(&rng),
            SignatureScheme::EcdsaP256Sha256 => signature::EcdsaKeyPair::generate_pkcs8(
                &signature::ECDSA_P256_SHA256_ASN1_SIGNING,
                &rng,
            ),
            _ => {
                return Err(CryptoError::UnsupportedAlgorithm(
                    CryptoAlgorithm::SigningKeyGeneration(scheme),
                ));
            }
        }
        .map_err(|_| CryptoError::RandomnessFailed)?;
        Self::import(scheme, pkcs8.as_ref())
    }

    fn import(
        scheme: SignatureScheme,
        pkcs8_der: &[u8],
    ) -> Result<Arc<dyn SigningKey>, CryptoError> {
        let (kind, public_key, public_key_encoding) = match scheme {
            SignatureScheme::Ed25519 => {
                let key = signature::Ed25519KeyPair::from_pkcs8_maybe_unchecked(pkcs8_der)
                    .map_err(|_| CryptoError::InvalidPrivateKey)?;
                let public = key.public_key().as_ref().to_vec();
                (
                    PrivateKeyKind::Ed25519(key),
                    public,
                    PublicKeyEncoding::Ed25519Raw,
                )
            }
            SignatureScheme::EcdsaP256Sha256 => {
                let key = signature::EcdsaKeyPair::from_pkcs8(
                    &signature::ECDSA_P256_SHA256_ASN1_SIGNING,
                    pkcs8_der,
                )
                .map_err(|_| CryptoError::InvalidPrivateKey)?;
                let public = key.public_key().as_ref().to_vec();
                (
                    PrivateKeyKind::EcdsaP256(key),
                    public,
                    PublicKeyEncoding::EcUncompressedPoint,
                )
            }
            SignatureScheme::RsaPkcs1Sha256 => {
                let key = signature::RsaKeyPair::from_pkcs8(pkcs8_der)
                    .map_err(|_| CryptoError::InvalidPrivateKey)?;
                let public = key.public_key().as_ref().to_vec();
                (
                    PrivateKeyKind::Rsa(key),
                    public,
                    PublicKeyEncoding::RsaPkcs1Der,
                )
            }
            _ => {
                return Err(CryptoError::UnsupportedAlgorithm(
                    CryptoAlgorithm::SigningKeyImport(scheme),
                ));
            }
        };
        Ok(Arc::new(Self {
            scheme,
            kind,
            public_key,
            public_key_encoding,
            pkcs8_der: SecretVec::new(pkcs8_der.to_vec()),
        }))
    }
}

impl SigningKey for PrivateKey {
    fn supports(&self, scheme: SignatureScheme) -> bool {
        self.scheme == scheme
    }

    fn public_key(&self) -> PublicKey<'_> {
        PublicKey {
            encoding: self.public_key_encoding,
            bytes: &self.public_key,
        }
    }

    fn sign(&self, scheme: SignatureScheme, message: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if !self.supports(scheme) {
            return Err(CryptoError::UnsupportedAlgorithm(
                CryptoAlgorithm::Signature(scheme),
            ));
        }
        match &self.kind {
            PrivateKeyKind::Ed25519(key) => Ok(key.sign(message).as_ref().to_vec()),
            PrivateKeyKind::EcdsaP256(key) => key
                .sign(&SystemRandom::new(), message)
                .map(|signature| signature.as_ref().to_vec())
                .map_err(|_| CryptoError::Provider("signature generation failed".into())),
            PrivateKeyKind::Rsa(key) => {
                let mut signature = vec![0; key.public_modulus_len()];
                key.sign(
                    &signature::RSA_PKCS1_SHA256,
                    &SystemRandom::new(),
                    message,
                    &mut signature,
                )
                .map_err(|_| CryptoError::Provider("signature generation failed".into()))?;
                Ok(signature)
            }
        }
    }

    fn to_pkcs8_der(&self) -> Result<Option<SecretVec>, CryptoError> {
        Ok(Some(self.pkcs8_der.clone()))
    }
}

fn verification_algorithm(
    scheme: SignatureScheme,
) -> Result<&'static dyn signature::VerificationAlgorithm, CryptoError> {
    Ok(match scheme {
        SignatureScheme::Ed25519 => &signature::ED25519,
        SignatureScheme::EcdsaP256Sha256 => &signature::ECDSA_P256_SHA256_ASN1,
        SignatureScheme::EcdsaP384Sha256 => &signature::ECDSA_P384_SHA256_ASN1,
        SignatureScheme::EcdsaP384Sha384 => &signature::ECDSA_P384_SHA384_ASN1,
        SignatureScheme::RsaPkcs1Sha1 => &signature::RSA_PKCS1_1024_8192_SHA1_FOR_LEGACY_USE_ONLY,
        SignatureScheme::RsaPkcs1Sha256 => {
            &signature::RSA_PKCS1_1024_8192_SHA256_FOR_LEGACY_USE_ONLY
        }
        SignatureScheme::RsaPkcs1Sha384 => &signature::RSA_PKCS1_2048_8192_SHA384,
        SignatureScheme::RsaPkcs1Sha512 => {
            &signature::RSA_PKCS1_1024_8192_SHA512_FOR_LEGACY_USE_ONLY
        }
        _ => {
            return Err(CryptoError::UnsupportedAlgorithm(
                CryptoAlgorithm::Signature(scheme),
            ));
        }
    })
}

fn verify_public_key_encoding(
    scheme: SignatureScheme,
    encoding: PublicKeyEncoding,
) -> Result<(), CryptoError> {
    let valid = matches!(
        (scheme, encoding),
        (SignatureScheme::Ed25519, PublicKeyEncoding::Ed25519Raw)
            | (
                SignatureScheme::EcdsaP256Sha256
                    | SignatureScheme::EcdsaP384Sha256
                    | SignatureScheme::EcdsaP384Sha384,
                PublicKeyEncoding::EcUncompressedPoint
            )
            | (
                SignatureScheme::RsaPkcs1Sha1
                    | SignatureScheme::RsaPkcs1Sha256
                    | SignatureScheme::RsaPkcs1Sha384
                    | SignatureScheme::RsaPkcs1Sha512,
                PublicKeyEncoding::RsaPkcs1Der
            )
    );
    if valid {
        Ok(())
    } else {
        Err(CryptoError::InvalidPublicKey)
    }
}
