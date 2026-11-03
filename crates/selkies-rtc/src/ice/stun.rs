/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! STUN as the ICE layer reads and writes it: demultiplexing, short-term authentication, and
//! the binding request and responses a connectivity check is made of.
//!
//! Every datagram a peer sends reaches `is_stun` and, when it is one, `parse` before any
//! authentication, so both refuse anything malformed with an error rather than a panic: a
//! truncated attribute, a length that does not match, a fingerprint that does not check.
//! `authenticate_request` is the short-term credential rule of RFC 5389 10.1.2 as the fork
//! applies it: a request needs both USERNAME and MESSAGE-INTEGRITY, its USERNAME must open with
//! the local fragment, and its HMAC must be the local password's; one that fails any of these is
//! answered 400 and its source is never learned as a peer. The messages built here carry
//! MESSAGE-INTEGRITY and FINGERPRINT, in that order, as every check and answer on the wire does.

use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};

use rand::Rng;
use rtc_crypto::RTCCryptoProvider;
use rtc_ice::attributes::control::{AttrControlled, AttrControlling};
use rtc_ice::attributes::priority::PriorityAttr;
use rtc_ice::attributes::use_candidate::UseCandidateAttr;
use rtc_stun::attributes::{
    ATTR_FINGERPRINT, ATTR_ICE_CONTROLLED, ATTR_ICE_CONTROLLING, ATTR_MESSAGE_INTEGRITY,
    ATTR_USE_CANDIDATE, ATTR_USERNAME, AttrType,
};
use rtc_stun::error_code::{ErrorCode, ErrorCodeAttribute};
use rtc_stun::fingerprint::FINGERPRINT;
use rtc_stun::integrity::MessageIntegrity;
use rtc_stun::message::{
    BINDING_ERROR, BINDING_REQUEST, BINDING_SUCCESS, CLASS_ERROR_RESPONSE, CLASS_INDICATION,
    CLASS_REQUEST, CLASS_SUCCESS_RESPONSE, Getter, MAGIC_COOKIE, MESSAGE_HEADER_SIZE,
    METHOD_BINDING, Message, Setter, TransactionId,
};
use rtc_stun::textattrs::TextAttribute;
use rtc_stun::xoraddr::XorMappedAddress;

use super::IceError;

/// The STUN header length.
pub const HEADER_LENGTH: usize = MESSAGE_HEADER_SIZE;

/// The class of a STUN message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Request,
    Indication,
    Success,
    Error,
}

/// The one crypto provider every STUN, DTLS, and SRTP operation in this crate runs on.
pub fn crypto() -> Arc<dyn RTCCryptoProvider> {
    static PROVIDER: OnceLock<Arc<dyn RTCCryptoProvider>> = OnceLock::new();
    PROVIDER
        .get_or_init(|| {
            rtc_crypto::default_provider()
                .expect("the rtc crates are built with their ring provider")
        })
        .clone()
}

/// Whether `data` starts like a STUN message: zero top bits and the magic cookie.
pub fn is_stun(data: &[u8]) -> bool {
    data.len() >= HEADER_LENGTH
        && data[0] & 0xC0 == 0
        && u32::from_be_bytes([data[4], data[5], data[6], data[7]]) == MAGIC_COOKIE
}

/// Parse a STUN message, checking its FINGERPRINT when it carries one.
///
/// Returns an error for any malformed message: a length that does not match, a truncated
/// attribute, a fingerprint that does not check. MESSAGE-INTEGRITY is checked separately by
/// `authenticate_request`, since the key depends on which connection the message names.
pub fn parse(data: &[u8]) -> Result<Message, IceError> {
    if data.len() < HEADER_LENGTH {
        return Err(IceError::Stun(
            "STUN message length is less than 20 bytes".into(),
        ));
    }
    let length = u16::from_be_bytes([data[2], data[3]]) as usize;
    if data.len() != HEADER_LENGTH + length {
        return Err(IceError::Stun("STUN message length does not match".into()));
    }
    let mut message = Message {
        raw: data.to_vec(),
        ..Message::default()
    };
    message
        .decode()
        .map_err(|e| IceError::Stun(format!("malformed STUN message: {e}")))?;
    if message.contains(ATTR_FINGERPRINT) {
        FINGERPRINT
            .check(&message)
            .map_err(|_| IceError::Stun("STUN message fingerprint does not match".into()))?;
    }
    Ok(message)
}

/// The class of `message`.
pub fn class(message: &Message) -> Class {
    match message.typ.class {
        CLASS_REQUEST => Class::Request,
        CLASS_INDICATION => Class::Indication,
        CLASS_SUCCESS_RESPONSE => Class::Success,
        CLASS_ERROR_RESPONSE => Class::Error,
        _ => Class::Indication,
    }
}

/// Whether `message` is a BINDING of any class.
pub fn is_binding(message: &Message) -> bool {
    message.typ.method == METHOD_BINDING
}

/// The USERNAME attribute of `message`, when it carries a well-formed one.
pub fn username(message: &Message) -> Option<String> {
    TextAttribute::get_from_as(message, ATTR_USERNAME)
        .ok()
        .map(|a| a.text)
        .filter(|s| !s.is_empty())
}

/// The local username fragment an ICE check's USERNAME (`local:remote`) names.
pub fn username_fragment(message: &Message) -> Option<String> {
    username(message).map(|u| u.split(':').next().unwrap_or("").to_string())
}

/// Whether `message` carries attribute `attr`.
pub fn has(message: &Message, attr: AttrType) -> bool {
    message.contains(attr)
}

/// The PRIORITY attribute of `message`, when present and well-formed.
pub fn priority(message: &Message) -> Option<u32> {
    let mut attr = PriorityAttr::default();
    attr.get_from(message).ok().map(|_| attr.0)
}

/// The tie-breaker of an ICE-CONTROLLED or ICE-CONTROLLING attribute, when present.
pub fn tie_breaker(message: &Message, attr: AttrType) -> Option<u64> {
    if attr == ATTR_ICE_CONTROLLED {
        let mut a = AttrControlled::default();
        a.get_from(message).ok().map(|_| a.0)
    } else if attr == ATTR_ICE_CONTROLLING {
        let mut a = AttrControlling::default();
        a.get_from(message).ok().map(|_| a.0)
    } else {
        None
    }
}

/// Whether `message` nominates its pair (USE-CANDIDATE).
pub fn use_candidate(message: &Message) -> bool {
    message.contains(ATTR_USE_CANDIDATE)
}

/// The XOR-MAPPED-ADDRESS of a response, when present.
pub fn mapped_address(message: &Message) -> Option<SocketAddr> {
    let mut addr = XorMappedAddress::default();
    addr.get_from(message)
        .ok()
        .map(|_| SocketAddr::new(addr.ip, addr.port))
}

/// The ERROR-CODE of an error response, when present.
pub fn error_code(message: &Message) -> Option<u16> {
    let mut attr = ErrorCodeAttribute::default();
    attr.get_from(message).ok().map(|_| attr.code.0)
}

/// Why a request failed short-term authentication; every reason is answered 400.
#[derive(Debug, PartialEq, Eq)]
pub enum AuthError {
    /// No MESSAGE-INTEGRITY or no USERNAME.
    Missing,
    /// The USERNAME does not open with the local fragment, or does not name the peer.
    WrongUsername,
    /// The HMAC is not the local password's.
    BadIntegrity,
}

/// Authenticate an inbound binding request against the connection's short-term credentials.
///
/// `remote_ufrag`, once known, makes the whole USERNAME `local:remote` the expectation;
/// before that only the local prefix is checked, as a check may arrive before the answer that
/// names the peer. The integrity check needs `&mut` access since the library rewrites the
/// header length while hashing and restores it.
pub fn authenticate_request(
    message: &mut Message,
    local_ufrag: &str,
    local_pwd: &str,
    remote_ufrag: Option<&str>,
) -> Result<(), AuthError> {
    let name = username(message).ok_or(AuthError::Missing)?;
    if !message.contains(ATTR_MESSAGE_INTEGRITY) {
        return Err(AuthError::Missing);
    }
    if name.split(':').next().unwrap_or("") != local_ufrag {
        return Err(AuthError::WrongUsername);
    }
    MessageIntegrity::check(message, local_pwd.as_bytes(), crypto().crypto())
        .map_err(|_| AuthError::BadIntegrity)?;
    if let Some(remote) = remote_ufrag
        && name != format!("{local_ufrag}:{remote}")
    {
        return Err(AuthError::WrongUsername);
    }
    Ok(())
}

/// Verify a response's MESSAGE-INTEGRITY with `key`.
pub fn verify_integrity(message: &mut Message, key: &str) -> bool {
    MessageIntegrity::check(message, key.as_bytes(), crypto().crypto()).is_ok()
}

fn build(setters: &[Box<dyn Setter + '_>]) -> Result<Message, IceError> {
    let mut message = Message::new();
    message
        .build(setters)
        .map_err(|e| IceError::Stun(format!("cannot build STUN message: {e}")))?;
    Ok(message)
}

/// A binding success response to `request`, telling the peer the address it was seen from.
pub fn binding_success(
    request: &Message,
    mapped: SocketAddr,
    local_pwd: &str,
) -> Result<Vec<u8>, IceError> {
    let provider = crypto();
    let message = build(&[
        Box::new(request.transaction_id),
        Box::new(BINDING_SUCCESS),
        Box::new(XorMappedAddress {
            ip: mapped.ip(),
            port: mapped.port(),
        }),
        Box::new(MessageIntegrity::new_short_term_integrity_with_provider(
            local_pwd.to_string(),
            provider.crypto(),
        )),
        Box::new(FINGERPRINT),
    ])?;
    Ok(message.raw)
}

/// A binding error response to `request` carrying `code` and `reason`.
pub fn binding_error(
    request: &Message,
    code: u16,
    reason: &str,
    local_pwd: &str,
) -> Result<Vec<u8>, IceError> {
    let provider = crypto();
    let message = build(&[
        Box::new(request.transaction_id),
        Box::new(BINDING_ERROR),
        Box::new(ErrorCodeAttribute {
            code: ErrorCode(code),
            reason: reason.as_bytes().to_vec(),
        }),
        Box::new(MessageIntegrity::new_short_term_integrity_with_provider(
            local_pwd.to_string(),
            provider.crypto(),
        )),
        Box::new(FINGERPRINT),
    ])?;
    Ok(message.raw)
}

/// The role a binding request claims.
#[derive(Clone, Copy, Debug)]
pub enum Role {
    Controlling(u64),
    Controlled(u64),
}

/// A binding request as a connectivity or consent check: USERNAME `remote:local`, PRIORITY,
/// the role with its tie-breaker, USE-CANDIDATE when nominating, keyed by the peer's password.
pub fn binding_request(
    remote_ufrag: &str,
    local_ufrag: &str,
    priority: u32,
    role: Role,
    nominate: bool,
    remote_pwd: &str,
) -> Result<Message, IceError> {
    let provider = crypto();
    let mut setters: Vec<Box<dyn Setter + '_>> = vec![
        Box::new(BINDING_REQUEST),
        Box::new(TransactionId::new()),
        Box::new(TextAttribute::new(
            ATTR_USERNAME,
            format!("{remote_ufrag}:{local_ufrag}"),
        )),
        Box::new(PriorityAttr(priority)),
    ];
    match role {
        Role::Controlling(tb) => setters.push(Box::new(AttrControlling(tb))),
        Role::Controlled(tb) => setters.push(Box::new(AttrControlled(tb))),
    }
    if nominate {
        setters.push(Box::new(UseCandidateAttr));
    }
    setters.push(Box::new(
        MessageIntegrity::new_short_term_integrity_with_provider(
            remote_pwd.to_string(),
            provider.crypto(),
        ),
    ));
    setters.push(Box::new(FINGERPRINT));
    build(&setters)
}

/// A plain binding request, as a server-reflexive query to a STUN server sends.
pub fn plain_binding_request() -> Result<Message, IceError> {
    build(&[Box::new(BINDING_REQUEST), Box::new(TransactionId::new())])
}

/// Twelve random bytes as a transaction id.
pub fn random_transaction_id() -> TransactionId {
    TransactionId::new()
}

/// A random alphanumeric string of `length` characters, as ICE credentials are made.
pub fn random_string(length: usize) -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::rng();
    (0..length)
        .map(|_| CHARS[rng.random_range(0..CHARS.len())] as char)
        .collect()
}

/// Whether `value` satisfies 4*256 ice-char, as an ICE username fragment must.
pub fn valid_username(value: &str) -> bool {
    (4..=256).contains(&value.len()) && value.bytes().all(is_ice_char)
}

/// Whether `value` satisfies 22*256 ice-char, as an ICE password must.
pub fn valid_password(value: &str) -> bool {
    (22..=256).contains(&value.len()) && value.bytes().all(is_ice_char)
}

fn is_ice_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'+' || b == b'/'
}

/// The 400 reason phrase every failed authentication is answered with.
pub const BAD_REQUEST: (u16, &str) = (400, "Bad Request");
/// The 487 reason phrase a role conflict is answered with.
pub const ROLE_CONFLICT: (u16, &str) = (487, "Role Conflict");

/// Attribute type constants re-exported for the agents.
pub mod attrs {
    pub use rtc_stun::attributes::{ATTR_ICE_CONTROLLED, ATTR_ICE_CONTROLLING, ATTR_PRIORITY};
}

#[cfg(test)]
mod tests {
    //! Regression vectors for the parser every datagram a peer sends reaches first, as the
    //! `stun` fuzz target exercises it: nothing malformed may escape as a panic.
    use super::*;

    fn header(method_class: u16, length: u16) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&method_class.to_be_bytes());
        v.extend_from_slice(&length.to_be_bytes());
        v.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        v.extend_from_slice(&[7u8; 12]);
        v
    }

    #[test]
    fn truncated_attribute_is_refused_not_raised() {
        let mut attr = Vec::new();
        attr.extend_from_slice(&0x0024u16.to_be_bytes());
        attr.extend_from_slice(&2u16.to_be_bytes());
        attr.extend_from_slice(&[0, 1, 0, 0]);
        let mut data = header(0x0001, attr.len() as u16);
        data.extend_from_slice(&attr);
        assert!(is_stun(&data));
        let parsed = parse(&data);
        assert!(parsed.map(|m| priority(&m).is_none()).unwrap_or(true));
    }

    #[test]
    fn length_mismatch_and_short_messages_are_refused() {
        assert!(parse(&[0u8; 10]).is_err());
        let data = header(0x0001, 8);
        assert!(parse(&data).is_err());
        assert!(!is_stun(&[
            0x80, 0, 0, 0, 0x21, 0x12, 0xA4, 0x42, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
        ]));
    }

    #[test]
    fn hostile_attribute_lengths_do_not_panic() {
        for kind in [
            0x0006u16, 0x0008, 0x0009, 0x0020, 0x0024, 0x8028, 0x8029, 0x802A,
        ] {
            for len in [0u16, 1, 3, 4, 5, 7, 8, 19, 20, 21, 65535] {
                let body_len = (len as usize).min(64);
                let mut attr = Vec::new();
                attr.extend_from_slice(&kind.to_be_bytes());
                attr.extend_from_slice(&len.to_be_bytes());
                attr.extend_from_slice(&vec![0xAB; body_len]);
                while attr.len() % 4 != 0 {
                    attr.push(0);
                }
                let mut data = header(0x0001, attr.len() as u16);
                data.extend_from_slice(&attr);
                if let Ok(mut m) = parse(&data) {
                    let _ = username(&m);
                    let _ = priority(&m);
                    let _ = mapped_address(&m);
                    let _ = error_code(&m);
                    let _ = tie_breaker(&m, ATTR_ICE_CONTROLLED);
                    let _ =
                        authenticate_request(&mut m, "abcdefgh", "passwordpasswordpassword", None);
                }
            }
        }
    }

    /// What the `stun` fuzz target feeds the parser, deterministically: a valid check mutated
    /// byte by byte reaches the parser and the authenticator with no panic.
    #[test]
    fn mutated_checks_never_panic() {
        use rand::SeedableRng;
        let mut rng = rand::rngs::StdRng::seed_from_u64(0x5354_554E);
        let pwd = "serverpasswordserverpass";
        let valid =
            binding_request("serverfr", "clientfr", 7, Role::Controlling(9), true, pwd).unwrap();
        for _ in 0..50_000 {
            let mut data = valid.raw.clone();
            for _ in 0..rng.random_range(1..6) {
                let at = rng.random_range(0..data.len());
                data[at] = rng.random();
            }
            if rng.random_bool(0.3) {
                let keep = rng.random_range(0..=data.len());
                data.truncate(keep);
                if data.len() >= 4 {
                    let body = (data.len().saturating_sub(HEADER_LENGTH)) as u16;
                    data[2..4].copy_from_slice(&body.to_be_bytes());
                }
            }
            if !is_stun(&data) {
                continue;
            }
            if let Ok(mut m) = parse(&data) {
                let _ = username_fragment(&m);
                let _ = priority(&m);
                let _ = mapped_address(&m);
                let _ = error_code(&m);
                let _ = tie_breaker(&m, ATTR_ICE_CONTROLLING);
                let _ = authenticate_request(&mut m, "serverfr", pwd, None);
                let _ = binding_success(&m, "127.0.0.1:9".parse().unwrap(), pwd);
            }
        }
    }

    #[test]
    fn a_check_round_trips_and_authenticates() {
        let request = binding_request(
            "serverfr",
            "clientfr",
            12345,
            Role::Controlling(99),
            true,
            "serverpasswordserverpass",
        )
        .unwrap();
        let mut parsed = parse(&request.raw).unwrap();
        assert_eq!(class(&parsed), Class::Request);
        assert_eq!(username_fragment(&parsed).as_deref(), Some("serverfr"));
        assert_eq!(priority(&parsed), Some(12345));
        assert!(use_candidate(&parsed));
        assert_eq!(tie_breaker(&parsed, ATTR_ICE_CONTROLLING), Some(99));
        assert_eq!(
            authenticate_request(
                &mut parsed,
                "serverfr",
                "serverpasswordserverpass",
                Some("clientfr")
            ),
            Ok(())
        );
        assert_eq!(
            authenticate_request(&mut parsed, "serverfr", "wrongpasswordwrongpassword", None),
            Err(AuthError::BadIntegrity)
        );
        assert_eq!(
            authenticate_request(&mut parsed, "otherfrag", "serverpasswordserverpass", None),
            Err(AuthError::WrongUsername)
        );
        assert_eq!(
            authenticate_request(
                &mut parsed,
                "serverfr",
                "serverpasswordserverpass",
                Some("nobody")
            ),
            Err(AuthError::WrongUsername)
        );
        let peer: SocketAddr = "203.0.113.9:4444".parse().unwrap();
        let success =
            parse(&binding_success(&parsed, peer, "serverpasswordserverpass").unwrap()).unwrap();
        assert_eq!(class(&success), Class::Success);
        assert_eq!(mapped_address(&success), Some(peer));
        assert_eq!(success.transaction_id, parsed.transaction_id);
        let error = parse(
            &binding_error(&parsed, 487, "Role Conflict", "serverpasswordserverpass").unwrap(),
        )
        .unwrap();
        assert_eq!(class(&error), Class::Error);
        assert_eq!(error_code(&error), Some(487));
    }

    #[test]
    fn a_request_without_integrity_is_missing() {
        let request = plain_binding_request().unwrap();
        let mut parsed = parse(&request.raw).unwrap();
        assert_eq!(
            authenticate_request(&mut parsed, "serverfr", "serverpasswordserverpass", None),
            Err(AuthError::Missing)
        );
    }

    #[test]
    fn credentials_have_the_shared_socket_length() {
        let ufrag = random_string(super::super::LOCAL_UFRAG_LENGTH);
        let pwd = random_string(super::super::LOCAL_PWD_LENGTH);
        assert_eq!(ufrag.len(), 8);
        assert!(valid_username(&ufrag));
        assert!(valid_password(&pwd));
        assert!(!valid_username("abc"));
        assert!(!valid_password("short"));
    }
}
