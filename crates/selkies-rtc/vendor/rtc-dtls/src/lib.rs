#![warn(rust_2018_idioms)]
#![warn(missing_docs)]
#![allow(dead_code)]

//! DTLS 1.2 for the Sans-I/O WebRTC stack.
//!
//! An implementation of Datagram Transport Layer Security ([RFC 6347]) with the extensions
//! WebRTC requires: DTLS-SRTP key export ([RFC 5764]), extended master secret
//! ([RFC 7627]), and elliptic-curve cipher suites ([RFC 4492], [RFC 5289]). It secures the
//! media and data-channel path: SRTP keying material comes out of the DTLS handshake, and
//! SCTP data channels run over the DTLS association itself.
//!
//! # Structure
//!
//! * [`endpoint`] — the Sans-I/O entry point: feed it datagrams, poll it for the datagrams
//!   it wants to send and the events it produces. No sockets, no timers of its own.
//! * [`config`] — certificates, cipher-suite and curve preferences, the client/server role,
//!   and the SRTP protection profiles to negotiate.
//! * [`handshake`], [`flight`], [`state`] — the handshake message types and the flight state
//!   machine that drives them, including retransmission.
//! * [`cipher_suite`], [`crypto`], [`curve`], [`signature_hash_algorithm`] — the
//!   cryptographic primitives and the negotiated-suite abstraction.
//! * [`extension`] — the ClientHello/ServerHello extensions, including `use_srtp` and SNI.
//! * [`alert`], [`content`], [`record_layer`] — the record layer and its content types.
//!
//! # Example
//!
//! A WebRTC handshake is configured with a self-signed certificate and the SRTP profiles to
//! negotiate through `use_srtp`; the keys for those profiles are then exported from the
//! completed handshake rather than signalled:
//!
//! ```
//! use rtc_dtls::config::{ConfigBuilder, ExtendedMasterSecretType};
//! use rtc_dtls::extension::extension_use_srtp::SrtpProtectionProfile;
//!
//! let builder = ConfigBuilder::default()
//!     .with_srtp_protection_profiles(vec![
//!         SrtpProtectionProfile::Srtp_Aead_Aes_128_Gcm,
//!         SrtpProtectionProfile::Srtp_Aes128_Cm_Hmac_Sha1_80,
//!     ])
//!     .with_extended_master_secret(ExtendedMasterSecretType::Require);
//! # let _ = builder;
//! ```
//!
//! Most applications do not depend on this crate directly — the
//! [`rtc`](https://docs.rs/rtc) crate drives it as one layer of the peer-connection
//! pipeline.
//!
//! [RFC 6347]: https://datatracker.ietf.org/doc/html/rfc6347
//! [RFC 5764]: https://datatracker.ietf.org/doc/html/rfc5764
//! [RFC 7627]: https://datatracker.ietf.org/doc/html/rfc7627
//! [RFC 4492]: https://datatracker.ietf.org/doc/html/rfc4492
//! [RFC 5289]: https://datatracker.ietf.org/doc/html/rfc5289

/// The crypto provider API.
///
/// Re-exported because this crate's public constructors take an `Arc<dyn RTCCryptoProvider>`,
/// which a caller must be able to name without adding — and version-matching — a direct
/// `rtc-crypto` dependency. Named `crypto_provider` here because this crate already has its own
/// [`crypto`] module for DTLS record ciphers and certificates.
pub use ::crypto as crypto_provider;

/// Alert records: fatal errors and the orderly `close_notify`.
pub mod alert;
/// Application data records — the payload DTLS carries once the handshake completes.
pub mod application_data;
/// The ChangeCipherSpec record, which switches a side over to the negotiated keys.
pub mod change_cipher_spec;
/// The negotiable cipher suites and the [`CipherSuite`] trait they
/// implement.
pub mod cipher_suite;
/// Certificate types a server may request from a client.
pub mod client_certificate_type;
/// The compression-methods field. DTLS in WebRTC always negotiates null compression.
pub mod compression_methods;
/// Handshake configuration: certificates, roles, cipher-suite and SRTP profile preferences.
pub mod config;
/// Connection state shared across the handshake and record layers.
pub mod conn;
/// Record content types: handshake, alert, change-cipher-spec and application data.
pub mod content;
/// Cryptographic primitives: the AEAD and CBC ciphers, certificates and signatures.
pub mod crypto;
/// Elliptic curves and the key-exchange values exchanged over them.
pub mod curve;
/// The Sans-I/O entry point: feed it datagrams, poll it for output and events.
pub mod endpoint;
/// ClientHello and ServerHello extensions, including `use_srtp` and SNI.
pub mod extension;
/// The flight state machine, which drives the handshake and its retransmissions.
pub mod flight;
/// Reassembly of handshake messages fragmented across datagrams.
pub mod fragment_buffer;
/// The handshake message types and the cache that hashes them for `Finished`.
pub mod handshake;
/// Handshake orchestration: state, roles and the verification callbacks.
pub mod handshaker;
/// The pseudo-random function that expands the master secret into keys.
pub mod prf;
/// The record layer: framing, sequence numbers and epochs.
pub mod record_layer;
/// Signature and hash algorithm pairs, as negotiated for certificate verification.
pub mod signature_hash_algorithm;
/// The negotiated connection state: keys, sequence numbers and peer identity.
pub mod state;

use cipher_suite::*;
use extension::extension_use_srtp::SrtpProtectionProfile;

pub(crate) fn find_matching_srtp_profile(
    a: &[SrtpProtectionProfile],
    b: &[SrtpProtectionProfile],
) -> Result<SrtpProtectionProfile, ()> {
    for a_profile in a {
        for b_profile in b {
            if a_profile == b_profile {
                return Ok(*a_profile);
            }
        }
    }
    Err(())
}

pub(crate) fn find_matching_cipher_suite(
    a: &[CipherSuiteId],
    b: &[CipherSuiteId],
) -> Result<CipherSuiteId, ()> {
    for a_suite in a {
        for b_suite in b {
            if a_suite == b_suite {
                return Ok(*a_suite);
            }
        }
    }
    Err(())
}
