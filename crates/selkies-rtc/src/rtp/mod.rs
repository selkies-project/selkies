/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The RTP layer: pre-encoded frames in, paced RTP datagrams out, and the receive side of
//! the microphone and webcam uplinks.
//!
//! Nothing here does I/O or keeps a clock: every call takes `now`, `CLOCK_MONOTONIC`
//! seconds (`clock::monotonic`), and every datagram leaves through a trait the transport
//! implements. The pieces, each in its module:
//!
//! - `packet`, `rtcp`: the wire formats, with the header extensions Selkies writes and the
//!   dependency descriptor.
//! - `codecs`: the packetizers for the frames pixelflux and pcmflux have already encoded
//!   (H.264, H.265, VP8, VP9, AV1, Opus, Opus RED) and the depacketizers of the uplinks,
//!   with the H.264 skip-picture stand-in.
//! - `history`, `fec`: the retransmission history and the FlexFEC repair builder.
//! - `twcc`: transport-wide congestion control accounting on both sides.
//! - `pacer`: the strict-priority token bucket a transport paces its datagrams with.
//! - `sender`: `RtpSender`, which numbers, describes, protects, and paces a stream and
//!   answers its RTCP.
//! - `receiver`, `rate`: `RtpReceiver` for the uplink tracks, with the REMB estimator.
//! - `router`: which stream of a bundled transport a packet belongs to.
//! - `transport`: `PacketSink`, the protect-and-write path a peer's transport offers, and
//!   `PacedTransport`, the pacer and the transport-wide state in front of it.
//!
//! Wiring, as the integrator does it: one `PacedTransport` per peer, over a `PacketSink` on
//! its `Transport`; an `RtpSender` per track pushed frames with `push_frame`, and a
//! `RtpReceiver` per uplink, with an `RtpRouter` telling their packets apart. A timer calls
//! `PacedTransport::drain` when the delay it last returned elapses and after any send that
//! queued, then hands `Pacer::take_events` to the video sender
//! (`RtpSender::apply_pacer_events`); another calls `RtpSender::send_report` and
//! `RtpReceiver::rtcp_report` at the interval they return, and a control loop feeds
//! `TwccSender::take_window` to `RtpSender::steer_fec`.

pub mod clock;
pub mod codecs;
pub mod fec;
pub mod history;
pub mod pacer;
pub mod packet;
pub mod params;
pub mod rate;
pub mod receiver;
pub mod router;
pub mod rtcp;
pub mod sender;
pub mod transport;
pub mod twcc;

pub use pacer::{Class, Pacer};
pub use packet::{HeaderExtensions, HeaderExtensionsMap, Payload, RtpPacket};
pub use params::{CodecParameters, HeaderExtensionParameters, ReceiveParameters, SendParameters};
pub use receiver::{ReceivedFrame, RtpReceiver};
pub use router::RtpRouter;
pub use rtcp::RtcpPacket;
pub use sender::{
    EncodedFrame, FrameTiming, Kind, NoHooks, RtpSender, SenderHooks, SenderTransport,
};
pub use transport::{PacedTransport, PacketSink};
pub use twcc::{TwccReceiver, TwccSender};
