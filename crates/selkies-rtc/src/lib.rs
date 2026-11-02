/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The Selkies WebRTC stack over the `rtc-*` sans-IO crates.
//!
//! Each module owns one layer: `sctp` the SCTP association
//! and the data channels over it, `sdp` the session descriptions Selkies builds and reads,
//! `signaling` the JSON protocol a page speaks over its WebSocket before and beside the
//! peer connection, `ice`, `dtls`, and `srtp` the path under them, and `transport` the handle
//! that runs those three for one peer. `ice` and `transport` are the only modules that own
//! sockets and tasks; for the others a datagram path, a WebSocket, and a clock are handed in
//! by the server crate that mounts the stack.

pub mod dtls;
pub mod ice;
pub mod rtp;
pub mod sctp;
pub mod sdp;
pub mod signaling;
pub mod srtp;
pub mod transport;
