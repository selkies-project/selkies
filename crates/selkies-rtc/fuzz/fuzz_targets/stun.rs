/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A datagram as it reaches the ICE layer before any authentication, through the reads the
//! shared sockets and a connection make of it (`ice::stun`, `ice::mux`): the STUN test and
//! parse, the class and the USERNAME fragment a mux routes by, and every attribute a check or
//! its answer is read for. The input's first byte has the low bit authenticate the message
//! with the short-term credentials of a connection whose local fragment is `fuzzfrag` and whose
//! password is `fuzzpasswordfuzzpasswd` (`authenticate_request`, `verify_integrity`), the next
//! bit expect the remote fragment `peerfrag` too; the rest is the datagram.
//!
//! Nothing may panic, checking the integrity leaves the message as it was, and the success and
//! the error a request is answered with parse back to the request's transaction, the success
//! naming the address it was seen from and both checking under the local password.
#![no_main]

use std::net::SocketAddr;

use libfuzzer_sys::fuzz_target;
use selkies_rtc::ice::stun::{self, Class, attrs};

const LOCAL_UFRAG: &str = "fuzzfrag";
const LOCAL_PWD: &str = "fuzzpasswordfuzzpasswd";
const REMOTE_UFRAG: &str = "peerfrag";

fuzz_target!(|data: &[u8]| {
    let Some((&mode, datagram)) = data.split_first() else {
        return;
    };
    let _ = stun::is_stun(datagram);
    let Ok(mut message) = stun::parse(datagram) else {
        return;
    };
    let class = stun::class(&message);
    let _ = stun::is_binding(&message);
    let _ = stun::username_fragment(&message);
    let _ = stun::priority(&message);
    let _ = stun::tie_breaker(&message, attrs::ATTR_ICE_CONTROLLING);
    let _ = stun::tie_breaker(&message, attrs::ATTR_ICE_CONTROLLED);
    let _ = stun::use_candidate(&message);
    let _ = stun::mapped_address(&message);
    let _ = stun::error_code(&message);
    if mode & 1 != 0 {
        let remote = (mode & 2 != 0).then_some(REMOTE_UFRAG);
        let _ = stun::authenticate_request(&mut message, LOCAL_UFRAG, LOCAL_PWD, remote);
        if matches!(class, Class::Success | Class::Error) {
            let _ = stun::verify_integrity(&mut message, LOCAL_PWD);
        }
        assert_eq!(
            message.raw, datagram,
            "checking the integrity changed the message"
        );
    }
    if class != Class::Request {
        return;
    }
    let peer: SocketAddr = "203.0.113.9:4444".parse().expect("an address");
    let success = stun::binding_success(&message, peer, LOCAL_PWD).expect("a success is built");
    let mut success = stun::parse(&success).expect("the success parses");
    assert_eq!(stun::class(&success), Class::Success);
    assert_eq!(stun::mapped_address(&success), Some(peer));
    assert_eq!(success.transaction_id, message.transaction_id);
    assert!(stun::verify_integrity(&mut success, LOCAL_PWD));
    let (code, reason) = stun::ROLE_CONFLICT;
    let error = stun::binding_error(&message, code, reason, LOCAL_PWD).expect("an error is built");
    let mut error = stun::parse(&error).expect("the error parses");
    assert_eq!(stun::class(&error), Class::Error);
    assert_eq!(stun::error_code(&error), Some(code));
    assert_eq!(error.transaction_id, message.transaction_id);
    assert!(stun::verify_integrity(&mut error, LOCAL_PWD));
});
