#![warn(rust_2018_idioms)]
#![warn(missing_docs)]
#![allow(dead_code)]

//! SRTP and SRTCP for the Sans-I/O WebRTC stack.
//!
//! The Secure Real-time Transport Protocol ([RFC 3711]) as WebRTC keys it: protection
//! profiles negotiated through DTLS-SRTP ([RFC 5764]), with keying material exported from
//! the DTLS handshake rather than signalled.
//!
//! # Structure
//!
//! * [`context`] — [`Context`](context::Context), the encrypt/decrypt state for one
//!   direction: `encrypt_rtp`/`decrypt_rtp` and the RTCP equivalents, plus the replay
//!   protection and rollover-counter tracking the RFC requires.
//! * [`protection_profile`] — the negotiable profiles (AES-128-CM-SHA1-80,
//!   AEAD-AES-128-GCM, and friends) and their key/salt lengths.
//! * [`config`], [`option`] — how a context is built, including replay-window sizing.
//!
//! # Example
//!
//! A profile is negotiated through DTLS-SRTP, and it fixes the key, salt and tag sizes the
//! context will use:
//!
//! ```
//! use rtc_srtp::protection_profile::ProtectionProfile;
//!
//! let profile = ProtectionProfile::Aes128CmHmacSha1_80;
//! assert_eq!(profile.key_len(), 16); // AES-128
//! assert_eq!(profile.salt_len(), 14);
//! assert_eq!(profile.rtp_auth_tag_len(), 10); // 80-bit tag
//!
//! // The AEAD profiles authenticate inside the cipher, so they carry no HMAC key.
//! assert_eq!(ProtectionProfile::AeadAes128Gcm.auth_key_len(), 0);
//! ```
//!
//! Most applications do not depend on this crate directly — the
//! [`rtc`](https://docs.rs/rtc) crate creates the contexts from the DTLS handshake and
//! applies them to media as one layer of the peer-connection pipeline.
//! Applications constructing contexts directly can select cryptography explicitly with
//! [`context::Context::new`]; [`context::Context::new`] retains default-provider
//! compatibility.
//!
//! [RFC 3711]: https://datatracker.ietf.org/doc/html/rfc3711
//! [RFC 5764]: https://datatracker.ietf.org/doc/html/rfc5764

mod cipher;

/// The crypto provider API.
///
/// Re-exported because this crate's public constructors take an
/// [`Arc<dyn RTCCryptoProvider>`](crypto::RTCCryptoProvider), which a caller must be able to name
/// without adding — and version-matching — a direct `rtc-crypto` dependency.
pub use crypto;

/// Session configuration: keys, protection profile, and replay-protection options.
pub mod config;
/// The encrypt/decrypt state for one SRTP/SRTCP session.
pub mod context;
mod key_derivation;
/// Per-context options, currently the replay-detector factory.
pub mod option;
/// The DTLS-SRTP protection profiles and their key, salt and tag lengths.
pub mod protection_profile;
