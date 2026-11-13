/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Known answers for the provider's primitives, each against the bytes its standard prints,
//! and the reuse of one keyed object across messages that a session relies on.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use rtc_crypto::{
    AeadAlgorithm, BlockCipherAlgorithm, CbcAlgorithm, CryptoError, HashAlgorithm, RTCCrypto,
    RTCCryptoProvider, StreamCipherAlgorithm,
};
use rtc_dtls::cipher_suite::CipherSuiteId;
use rtc_dtls::config::{ConfigBuilder, HandshakeConfig};
use rtc_dtls::crypto::Certificate;
use rtc_dtls::endpoint::{Endpoint, EndpointEvent};
use rtc_shared::TransportProtocol;
use rtc_stun::integrity::MessageIntegrity;
use rtc_stun::message::Message;

use super::{AwsLcCrypto, AwsLcProvider, fill_random, random};

fn hex(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .map(|byte| (byte as char).to_digit(16).expect("a hex digit") as u8)
        .collect();
    assert!(digits.len().is_multiple_of(2));
    digits
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| pair[0] << 4 | pair[1])
        .collect()
}

/// The four plaintext blocks NIST SP 800-38A uses in every example.
const SP_800_38A_PLAINTEXT: &str =
    "6bc1bee22e409f96e93d7e117393172a ae2d8a571e03ac9c9eb76fac45af8e51
     30c81c46a35ce411e5fbc1191a0a52ef f69f2445df4f9b17ad2b417be66c3710";

#[test]
fn the_provider_passes_the_rtc_crypto_conformance_suite() {
    let provider = AwsLcProvider::new();
    assert_eq!(provider.name(), "aws-lc");
    rtc_crypto::conformance::assert_provider(&provider);
}

#[test]
fn a_single_aes_block_matches_fips_197_and_the_key_derivation_of_rfc_3711() {
    // (key, block, encrypted): FIPS 197 C.1 and C.3, then RFC 3711 B.3's cipher key, cipher
    // salt, and first auth key block.
    let master = "E1F97A0D3E018BE0D64FA32C06DE4139";
    let cases = [
        (
            BlockCipherAlgorithm::Aes128,
            "000102030405060708090a0b0c0d0e0f",
            "00112233445566778899aabbccddeeff",
            "69c4e0d86a7b0430d8cdb78070b4c55a",
        ),
        (
            BlockCipherAlgorithm::Aes256,
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
            "00112233445566778899aabbccddeeff",
            "8ea2b7ca516745bfeafc49904b496089",
        ),
        (
            BlockCipherAlgorithm::Aes128,
            master,
            "0EC675AD498AFEEBB6960B3AABE60000",
            "C61E7A93744F39EE10734AFE3FF7A087",
        ),
        (
            BlockCipherAlgorithm::Aes128,
            master,
            "0EC675AD498AFEE9B6960B3AABE60000",
            "30CBBC08863D8C85D49DB34A9AE17AC6",
        ),
        (
            BlockCipherAlgorithm::Aes128,
            master,
            "0EC675AD498AFEEAB6960B3AABE60000",
            "CEBE321F6FF7716B6FD4AB49AF256A15",
        ),
    ];
    for (algorithm, key, block, expected) in cases {
        let mut block = hex(block);
        AwsLcCrypto
            .block_encrypt(algorithm, &hex(key), &mut block)
            .unwrap();
        assert_eq!(block, hex(expected));
    }
}

#[test]
fn aes_ctr_produces_the_srtp_keystream_of_rfc_3711_b2() {
    let mut cipher = AwsLcCrypto
        .new_stream_cipher(
            StreamCipherAlgorithm::Aes128Ctr,
            &hex("2B7E151628AED2A6ABF7158809CF4F3C"),
        )
        .unwrap();
    let iv = hex("F0F1F2F3F4F5F6F7F8F9FAFBFCFD0000");
    // Counters 0000 through FF01, so the low byte's carry is in the keystream.
    let mut keystream = vec![0u8; 16 * 0xFF02];
    cipher.apply_keystream(&iv, &mut keystream).unwrap();
    assert_eq!(
        keystream[..48],
        hex(
            "E03EAD0935C95E80E166B16DD92B4EB4 D23513162B02D0F72A43A2FE4A5F97AB
             41E95B3BB0A2E8DD477901E4FCA894C0"
        )
    );
    assert_eq!(
        keystream[16 * 0xFEFF..],
        hex(
            "EC8CDF7398607CB0F2D21675EA9EA1E4 362B7C3C6773516318A077D7FC5073AE
             6A2CC3787889374FBEB4C81B17BA6C44"
        )
    );

    // The same object under a second IV starts over from it (NIST SP 800-38A F.5.1), and a
    // length that is no whole number of blocks ends mid-block.
    let iv = hex("f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff");
    let expected = hex(
        "874d6191b620e3261bef6864990db6ce 9806f66b7970fdff8617187bb9fffdff
         5ae4df3edbd5d35e5b4f09020db03eab 1e031dda2fbe03d1792170a0f3009cee",
    );
    let mut data = hex(SP_800_38A_PLAINTEXT);
    cipher.apply_keystream(&iv, &mut data).unwrap();
    assert_eq!(data, expected);
    let mut data = hex(SP_800_38A_PLAINTEXT);
    cipher.apply_keystream(&iv, &mut data[..37]).unwrap();
    assert_eq!(data[..37], expected[..37]);
}

#[test]
fn aes_256_ctr_matches_nist_sp_800_38a_f5_5() {
    let mut cipher = AwsLcCrypto
        .new_stream_cipher(
            StreamCipherAlgorithm::Aes256Ctr,
            &hex("603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4"),
        )
        .unwrap();
    let mut data = hex(SP_800_38A_PLAINTEXT);
    cipher
        .apply_keystream(&hex("f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff"), &mut data)
        .unwrap();
    assert_eq!(
        data,
        hex(
            "601ec313775789a5b7a7f504bbf3d228 f443e3ca4d62b59aca84e990cacaf5c5
             2b0930daa23de94ce87017ba2d84988d dfc9c58db67aada613c2dd08457941a6"
        )
    );
}

/// SRTP's counter is the whole 128-bit block, so a carry out of the low 64 bits reaches the
/// high ones: the block after `0..0 ff..ff` is the encryption of `0..01 00..00`.
#[test]
fn aes_ctr_carries_the_counter_through_all_128_bits() {
    let key = hex("2B7E151628AED2A6ABF7158809CF4F3C");
    let mut iv = [0u8; 16];
    iv[8..].fill(0xFF);
    let mut keystream = [0u8; 32];
    AwsLcCrypto
        .new_stream_cipher(StreamCipherAlgorithm::Aes128Ctr, &key)
        .unwrap()
        .apply_keystream(&iv, &mut keystream)
        .unwrap();
    let mut next = [0u8; 16];
    next[7] = 1;
    AwsLcCrypto
        .block_encrypt(BlockCipherAlgorithm::Aes128, &key, &mut next)
        .unwrap();
    assert_eq!(keystream[16..], next);
}

#[test]
fn aes_256_cbc_matches_nist_sp_800_38a_f2_5_and_f2_6() {
    let key = hex("603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4");
    let iv = hex("000102030405060708090a0b0c0d0e0f");
    let plaintext = hex(SP_800_38A_PLAINTEXT);
    let ciphertext = hex(
        "f58c4c04d6e5f1ba779eabfb5f7bfbd6 9cfc4e967edb808d679f777bc6702c7d
         39f23369a9d9bacfa530e26304231461 b2eb05e2c39be9fcda6c19078c6a9d1b",
    );
    let mut cipher = AwsLcCrypto.new_cbc(CbcAlgorithm::Aes256Cbc, &key).unwrap();

    let mut blocks = plaintext.clone();
    cipher.encrypt_blocks(&iv, &mut blocks).unwrap();
    assert_eq!(blocks, ciphertext);
    cipher.decrypt_blocks(&iv, &mut blocks).unwrap();
    assert_eq!(blocks, plaintext);

    // One object across records: each call chains from the IV it is given and from nothing
    // an earlier call left, so the second half under the first half's last block completes
    // the same ciphertext.
    let mut blocks = plaintext.clone();
    let (first, second) = blocks.split_at_mut(32);
    cipher.encrypt_blocks(&iv, first).unwrap();
    cipher.encrypt_blocks(&ciphertext[16..32], second).unwrap();
    assert_eq!(blocks, ciphertext);
    let (first, second) = blocks.split_at_mut(32);
    cipher.decrypt_blocks(&ciphertext[16..32], second).unwrap();
    cipher.decrypt_blocks(&iv, first).unwrap();
    assert_eq!(blocks, plaintext);

    // No padding is added or expected: a partial block is refused.
    assert!(matches!(
        cipher.encrypt_blocks(&iv, &mut [0; 17]),
        Err(CryptoError::OutputTooSmall { .. })
    ));
}

/// One AES-CCM known answer: (key, nonce, associated data, plaintext, ciphertext, tag).
type CcmVector = (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
);

/// AES-128-CCM with the 12-byte nonce of the DTLS suites and a 16-byte tag, from Project
/// Wycheproof's `aes_ccm_test` (tests 51, 4, 11, and 1): RFC 3610's own vectors use 13-byte
/// nonces, which neither DTLS nor this provider takes. The last three have no associated
/// data, no message, and neither.
const CCM_VECTORS: [CcmVector; 4] = [
    (
        "2c9b9ff47d742c4ab224e9ca1ed57c4c",
        "917962caf3932441c259282f",
        "72175bdfdb4a23e97fdcbd263baf4316",
        "b542c2f3f81670ddf74f15184ab7de17e057cde9eef92babdb837500774c19",
        "320ae0c11e92d10d5bf5485c854b2d8f6318e33f16b520cffd35ada381c967",
        "a4866908e664ee140c6ae2b9d2ab8416",
    ),
    (
        "ffdf4228361ea1f8165852136b3480f7",
        "0e1666f2dc652f7708fb8f0d",
        "",
        "25b12e28ac0ef6ead0226a3b2288c800",
        "aaf596fa5b00aaac27700146aec932a9",
        "848b6735d32c96e4a0532bcdfaf33582",
    ),
    (
        "59ab7ec1c02bb206af5a9131f1134311",
        "5508f5cea197386986d92dbe",
        "a43d39f78a2e9a8a",
        "",
        "",
        "09ec70faae333537a7314929ddfb525b",
    ),
    (
        "bedcfb5a011ebc84600fcb296c15af0d",
        "438a547a94ea88dce46c6c85",
        "",
        "",
        "",
        "25d1a38495a7dea45bda049705627d10",
    ),
];

/// AES-128-CCM-8: NIST SP 800-38C example 3, and Wycheproof's `aes_ccm_test` test 366.
const CCM_8_VECTORS: [CcmVector; 2] = [
    (
        "404142434445464748494a4b4c4d4e4f",
        "101112131415161718191a1b",
        "000102030405060708090a0b0c0d0e0f10111213",
        "202122232425262728292a2b2c2d2e2f3031323334353637",
        "e3b201a9f5b71a7a9b1ceaeccd97e70b6176aad9a4428aa5",
        "484392fbc1b09951",
    ),
    (
        "c08339a6f80b84e201e3d6030cdb3f02",
        "1cbf2ca31330abe749db588b",
        "b535a847dfc962012d913a4076f58f9f",
        "4f9fd6ad1656cce99af7469960073a241569ce32dad558111b50306053a0b6",
        "c91d4c8bf7fdba49b87001fc3ec95f455ba32bc05ba336bc3d58f4ad08b5bc",
        "34d622fe4ba3cac5",
    ),
];

fn assert_ccm_vectors(algorithm: AeadAlgorithm, vectors: &[CcmVector]) {
    for (key, nonce, aad, plaintext, ciphertext, tag) in vectors {
        let (nonce, aad) = (hex(nonce), hex(aad));
        let mut cipher = AwsLcCrypto.new_aead(algorithm, &hex(key)).unwrap();
        let mut buffer = hex(plaintext);
        let mut sealed_tag = vec![0u8; cipher.tag_len()];
        cipher
            .seal_in_place(&nonce, &aad, &mut buffer, &mut sealed_tag)
            .unwrap();
        assert_eq!(buffer, hex(ciphertext));
        assert_eq!(sealed_tag, hex(tag));
        cipher
            .open_in_place(&nonce, &aad, &mut buffer, &sealed_tag)
            .unwrap();
        assert_eq!(buffer, hex(plaintext));
    }
}

#[test]
fn aes_128_ccm_matches_its_known_answers() {
    assert_ccm_vectors(AeadAlgorithm::Aes128Ccm, &CCM_VECTORS);
}

#[test]
fn aes_128_ccm_8_matches_its_known_answers() {
    assert_ccm_vectors(AeadAlgorithm::Aes128Ccm8, &CCM_8_VECTORS);
}

/// A DTLS session seals and opens every record on one cipher object, so nothing of a
/// message may reach the next: not its nonce, its direction, its lengths, nor the state a
/// rejected record leaves behind.
#[test]
fn one_ccm_object_keeps_no_state_between_messages() {
    let (key, nonce, aad, plaintext, ciphertext, tag) = CCM_VECTORS[0];
    let (nonce, aad, tag) = (hex(nonce), hex(aad), hex(tag));
    let mut cipher = AwsLcCrypto
        .new_aead(AeadAlgorithm::Aes128Ccm, &hex(key))
        .unwrap();
    let other_nonce = [9u8; 12];
    let mut scratch_tag = [0u8; 16];

    for _ in 0..2 {
        // A record under another nonce, with another length and no associated data.
        let mut other = vec![0x5A; 100];
        cipher
            .seal_in_place(&other_nonce, &[], &mut other, &mut scratch_tag)
            .unwrap();

        // A forged tag, forged associated data, and a forged ciphertext are each refused.
        let mut forged_tag = tag.clone();
        forged_tag[15] ^= 1;
        let mut buffer = hex(ciphertext);
        assert_eq!(
            cipher.open_in_place(&nonce, &aad, &mut buffer, &forged_tag),
            Err(CryptoError::AuthenticationFailed)
        );
        let mut buffer = hex(ciphertext);
        assert_eq!(
            cipher.open_in_place(&nonce, &aad[..15], &mut buffer, &tag),
            Err(CryptoError::AuthenticationFailed)
        );
        let mut buffer = hex(ciphertext);
        buffer[0] ^= 1;
        assert_eq!(
            cipher.open_in_place(&nonce, &aad, &mut buffer, &tag),
            Err(CryptoError::AuthenticationFailed)
        );

        // The known answer still opens and seals to the same bytes after all of that, and
        // with no associated data right after a rejected record that had some.
        let mut buffer = hex(ciphertext);
        cipher
            .open_in_place(&nonce, &aad, &mut buffer, &tag)
            .unwrap();
        assert_eq!(buffer, hex(plaintext));
        let mut buffer = hex(ciphertext);
        assert!(
            cipher
                .open_in_place(&nonce, &aad, &mut buffer, &scratch_tag)
                .is_err()
        );
        cipher
            .open_in_place(&other_nonce, &[], &mut other, &scratch_tag)
            .unwrap();
        assert_eq!(other, vec![0x5A; 100]);
        let mut buffer = hex(plaintext);
        cipher
            .seal_in_place(&nonce, &aad, &mut buffer, &mut scratch_tag)
            .unwrap();
        assert_eq!(buffer, hex(ciphertext));
        assert_eq!(scratch_tag.as_slice(), tag);
    }

    // The lengths the suites fix are enforced.
    assert!(matches!(
        cipher.seal_in_place(&[0; 13], &[], &mut [], &mut scratch_tag),
        Err(CryptoError::InvalidNonceLength { .. })
    ));
    assert!(matches!(
        cipher.open_in_place(&nonce, &[], &mut [], &[0; 8]),
        Err(CryptoError::InvalidTagLength { .. })
    ));
    assert!(matches!(
        AwsLcCrypto.new_aead(AeadAlgorithm::Aes128Ccm8, &[0; 32]),
        Err(CryptoError::InvalidKeyLength { .. })
    ));
}

#[test]
fn md5_matches_the_test_suite_of_rfc_1321() {
    let cases: [(&str, &str); 7] = [
        ("", "d41d8cd98f00b204e9800998ecf8427e"),
        ("a", "0cc175b9c0f1b6a831c399e269772661"),
        ("abc", "900150983cd24fb0d6963f7d28e17f72"),
        ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
        (
            "abcdefghijklmnopqrstuvwxyz",
            "c3fcd3d76192e4007dfb496cca67e13b",
        ),
        (
            "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
            "d174ab98d277d9f5a5611c2c9f419d9f",
        ),
        (
            "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
            "57edf4a22be3c955ac49da2e2107b67a",
        ),
    ];
    for (message, digest) in cases {
        assert_eq!(
            AwsLcCrypto
                .hash(HashAlgorithm::Md5, message.as_bytes())
                .unwrap(),
            hex(digest)
        );
    }
}

/// The long-term credential key is `MD5(username ":" realm ":" password)` (RFC 8489 section
/// 9.2.2), and the request RFC 5769 section 2.4 prints is authenticated with it: a TURN
/// server checks an allocation this way.
#[test]
fn the_stun_long_term_key_authenticates_the_request_of_rfc_5769() {
    let (username, realm, password) = ("マトリックス", "example.org", "TheMatrIX");
    let key = MessageIntegrity::long_term_integrity_key(
        username.to_owned(),
        realm.to_owned(),
        password.to_owned(),
        &AwsLcCrypto,
    )
    .unwrap();
    assert_eq!(key, hex("e8ca7ad59d5eb0518e312911d2dab2a9"));

    let request = hex("00 01 00 60 21 12 a4 42 78 ad 34 33 c6 ad 72 c0 29 da 41 2e
         00 06 00 12 e3 83 9e e3 83 88 e3 83 aa e3 83 83 e3 82 af e3 82 b9 00 00
         00 15 00 1c 66 2f 2f 34 39 39 6b 39 35 34 64 36 4f 4c 33 34 6f 4c 39 46
         53 54 76 79 36 34 73 41
         00 14 00 0b 65 78 61 6d 70 6c 65 2e 6f 72 67 00
         00 08 00 14 f6 70 24 65 6d d6 4a 3e 02 b8 e0 71 2e 85 c9 a2 8c a8 96 66");
    let mut message = Message::new();
    message.unmarshal_binary(&request).unwrap();
    MessageIntegrity::check(&mut message, &key, &AwsLcCrypto).unwrap();

    let wrong = MessageIntegrity::long_term_integrity_key(
        username.to_owned(),
        realm.to_owned(),
        "TheMatrIx".to_owned(),
        &AwsLcCrypto,
    )
    .unwrap();
    assert!(MessageIntegrity::check(&mut message, &wrong, &AwsLcCrypto).is_err());
}

#[test]
fn random_draws_are_fresh_and_fill_the_whole_buffer() {
    let (first, second): ([u8; 32], [u8; 32]) = (random(), random());
    assert_ne!(first, second);
    // A buffer left partly unwritten would keep a run of its zeros; 4096 random bytes hold
    // no 16-byte run of them.
    let mut buffer = [0u8; 4096];
    fill_random(&mut buffer);
    assert!(buffer.windows(16).all(|window| window != [0u8; 16]));
    fill_random(&mut []);
}

/// A handshake configuration offering `suite` alone, with the pre-shared key or the
/// certificate it authenticates with.
fn dtls_config(suite: CipherSuiteId, is_client: bool) -> Arc<HandshakeConfig> {
    let provider: Arc<dyn RTCCryptoProvider> = Arc::new(AwsLcProvider::new());
    let mut builder = ConfigBuilder::default()
        .with_crypto_provider(provider.clone())
        .with_cipher_suites(vec![suite])
        .with_insecure_skip_verify(true);
    let pre_shared = matches!(
        suite,
        CipherSuiteId::Tls_Psk_With_Aes_128_Ccm
            | CipherSuiteId::Tls_Psk_With_Aes_128_Ccm_8
            | CipherSuiteId::Tls_Psk_With_Aes_128_Gcm_Sha256
    );
    if pre_shared {
        builder = builder.with_psk(Some(Arc::new(|_| Ok(vec![0xAB, 0xCD, 0xEF]))));
        if is_client {
            builder = builder.with_psk_identity_hint(Some(b"selkies".to_vec()));
        }
    } else if !is_client {
        let certificate =
            Certificate::generate_self_signed(vec!["localhost".to_owned()], provider.crypto())
                .unwrap();
        builder = builder.with_certificates(vec![certificate]);
    }
    Arc::new(builder.build(is_client, None).unwrap())
}

/// Deliver everything `from` has queued to `to`, and return what `to` made of it.
fn deliver(from: &mut Endpoint, to: &mut Endpoint, from_addr: SocketAddr) -> Vec<EndpointEvent> {
    let mut events = Vec::new();
    while let Some(transmit) = from.poll_transmit() {
        events.extend(
            to.read(
                Instant::now(),
                from_addr,
                transmit.transport.ecn,
                transmit.message,
            )
            .unwrap(),
        );
    }
    events
}

/// Two `rtc-dtls` endpoints on this provider complete a handshake and exchange a record in
/// both directions under every record protection the crate has: the three suites Selkies
/// offers (AES-GCM, ChaCha20-Poly1305, AES-256-CBC with HMAC-SHA1), and the AES-CCM ones.
#[test]
fn a_dtls_handshake_and_a_record_each_way_succeed_under_every_suite() {
    let client_addr = SocketAddr::from(([127, 0, 0, 1], 4444));
    let server_addr = SocketAddr::from(([127, 0, 0, 1], 4445));
    for suite in [
        CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Gcm_Sha256,
        CipherSuiteId::Tls_Ecdhe_Ecdsa_With_ChaCha20_Poly1305_Sha256,
        CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_256_Cbc_Sha,
        CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Ccm,
        CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Ccm_8,
        CipherSuiteId::Tls_Psk_With_Aes_128_Ccm,
        CipherSuiteId::Tls_Psk_With_Aes_128_Ccm_8,
        CipherSuiteId::Tls_Psk_With_Aes_128_Gcm_Sha256,
    ] {
        let mut server = Endpoint::new(
            server_addr,
            TransportProtocol::UDP,
            Some(dtls_config(suite, false)),
        );
        let mut client = Endpoint::new(client_addr, TransportProtocol::UDP, None);
        client
            .connect(Instant::now(), server_addr, dtls_config(suite, true), None)
            .unwrap();
        let (mut client_done, mut server_done) = (false, false);
        for _ in 0..32 {
            server_done |= deliver(&mut client, &mut server, client_addr)
                .iter()
                .any(|event| matches!(event, EndpointEvent::HandshakeComplete));
            client_done |= deliver(&mut server, &mut client, server_addr)
                .iter()
                .any(|event| matches!(event, EndpointEvent::HandshakeComplete));
            if client_done && server_done {
                break;
            }
        }
        assert!(client_done && server_done, "{suite:?}: no handshake");

        for round in 0..3u8 {
            let message = vec![round; 1 + usize::from(round) * 700];
            client.write(Instant::now(), server_addr, &message).unwrap();
            let received = deliver(&mut client, &mut server, client_addr);
            assert!(
                received.iter().any(|event| matches!(
                    event,
                    EndpointEvent::ApplicationData(data) if data.as_ref() == message
                )),
                "{suite:?}: the server read no record {round}"
            );
            server.write(Instant::now(), client_addr, &message).unwrap();
            let received = deliver(&mut server, &mut client, server_addr);
            assert!(
                received.iter().any(|event| matches!(
                    event,
                    EndpointEvent::ApplicationData(data) if data.as_ref() == message
                )),
                "{suite:?}: the client read no record {round}"
            );
        }
    }
}

/// A client that offers no AES-GCM SRTP profile is given SRTP_AES128_CM_HMAC_SHA1_80 by a
/// server offering Selkies' three, and the keys both sides export from the handshake carry
/// RTP and RTCP each way: the profile's whole path, AES-CM keystream included, on this
/// provider.
#[test]
fn a_client_without_aes_gcm_srtp_gets_aes_cm_end_to_end() {
    use rtc_dtls::extension::extension_use_srtp::SrtpProtectionProfile;

    use crate::srtp::{KEYING_LABEL, SrtpProfile, SrtpSession};

    let client_addr = SocketAddr::from(([127, 0, 0, 1], 4444));
    let server_addr = SocketAddr::from(([127, 0, 0, 1], 4445));
    let suite = CipherSuiteId::Tls_Ecdhe_Ecdsa_With_Aes_128_Gcm_Sha256;
    let config = |is_client: bool, profiles: Vec<SrtpProtectionProfile>| {
        let provider: Arc<dyn RTCCryptoProvider> = Arc::new(AwsLcProvider::new());
        let mut builder = ConfigBuilder::default()
            .with_crypto_provider(provider.clone())
            .with_cipher_suites(vec![suite])
            .with_srtp_protection_profiles(profiles)
            .with_insecure_skip_verify(true);
        if !is_client {
            let certificate =
                Certificate::generate_self_signed(vec!["localhost".to_owned()], provider.crypto())
                    .unwrap();
            builder = builder.with_certificates(vec![certificate]);
        }
        Arc::new(builder.build(is_client, None).unwrap())
    };
    let mut server = Endpoint::new(
        server_addr,
        TransportProtocol::UDP,
        Some(config(
            false,
            vec![
                SrtpProtectionProfile::Srtp_Aead_Aes_256_Gcm,
                SrtpProtectionProfile::Srtp_Aead_Aes_128_Gcm,
                SrtpProtectionProfile::Srtp_Aes128_Cm_Hmac_Sha1_80,
            ],
        )),
    );
    let mut client = Endpoint::new(client_addr, TransportProtocol::UDP, None);
    let offered = vec![SrtpProtectionProfile::Srtp_Aes128_Cm_Hmac_Sha1_80];
    client
        .connect(Instant::now(), server_addr, config(true, offered), None)
        .unwrap();
    for _ in 0..32 {
        deliver(&mut client, &mut server, client_addr);
        deliver(&mut server, &mut client, server_addr);
    }

    let session = |endpoint: &mut Endpoint, peer: SocketAddr, is_client: bool| {
        let state = endpoint.get_connection_state(peer).expect("a connection");
        let profile = SrtpProfile::from_dtls(state.srtp_protection_profile()).expect("a profile");
        assert_eq!(profile, SrtpProfile::Aes128CmHmacSha1_80);
        let material = state
            .export_keying_material(KEYING_LABEL, &[], profile.keying_material_len())
            .unwrap();
        SrtpSession::from_keying_material(profile, material.as_ref(), is_client).unwrap()
    };
    let mut client_srtp = session(&mut client, server_addr, true);
    let mut server_srtp = session(&mut server, client_addr, false);

    let mut rtp = vec![0x80, 96, 0, 1, 0, 0, 0, 1, 0x12, 0x34, 0x56, 0x78];
    rtp.extend([0xA5; 1200]);
    let rtcp = [0x80, 200, 0, 1, 0x12, 0x34, 0x56, 0x78, 0, 0, 0, 0];
    for server_sends in [true, false] {
        let (from, to) = if server_sends {
            (&mut server_srtp, &mut client_srtp)
        } else {
            (&mut client_srtp, &mut server_srtp)
        };
        let sealed = from.tx.protect_rtp(&rtp).unwrap();
        assert_eq!(sealed.len(), rtp.len() + 10);
        assert_ne!(sealed[12..rtp.len()], rtp[12..]);
        assert_eq!(to.rx.unprotect_rtp(&sealed).unwrap().as_ref(), rtp);
        let sealed = from.tx.protect_rtcp(&rtcp).unwrap();
        assert_eq!(to.rx.unprotect_rtcp(&sealed).unwrap().as_ref(), rtcp);
    }
}
