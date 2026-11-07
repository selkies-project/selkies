/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The WebRTC transport: the counterpart of `ws` over `selkies-rtc`.
//!
//! The service is one `WebRtcService` (`service`), with the layout passes in `layout`, the owner model
//! in `owner`, and what a page may change in `tunables`. `engine` runs the peers: their offers and
//! answers, the per-display media graphs, and the channels' gates. A `peer` is one connection's tasks:
//! ICE, DTLS, and SRTP under `selkies_rtc::transport`, RTP senders and receivers under one pacer, and
//! SCTP. A frame leaves its capture in `pipeline`, becomes an `rtp::EncodedFrame` in `media` with no
//! copy of its bytes, passes the display's `bridge`, and is handed to every peer by the display's
//! `relay`. `uplink` takes a peer's microphone and camera; `channel` writes its input channel. `host`
//! is what the integrator wires, `wiring` the input stack, clipboard, and gamepads over this
//! transport, and `routes` the signaling socket.
//!
//! Threading: tokio tasks throughout. Every peer owns its tasks and shares no lock with another
//! beyond short sections of the engine's table, so a frame being packed for one peer stalls neither
//! another peer's frames nor its input.

pub mod bridge;
pub mod channel;
pub mod engine;
pub mod host;
pub mod layout;
pub mod media;
pub mod owner;
pub mod peer;
pub mod pipeline;
pub mod relay;
pub mod routes;
pub mod service;
pub mod tunables;
pub mod uplink;
pub mod wiring;

#[cfg(test)]
mod tests;

pub use engine::{EngineHost, NoHost, RtcEngine};
pub use host::{DisplayPlane, PipelineFactory, PixelPipelines, ServerPlane, WebRtcDeps};
pub use routes::routes;
pub use service::{Timings, WebRtcService};
