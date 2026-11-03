/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What joins the RTP layer to a peer's socket: `PacketSink` is the protect-and-write path
//! the transport handle offers (`Transport::send_rtp`), and `PacedTransport` puts the
//! `Pacer` and the `TwccSender` in front of it as `SenderTransport`.
//!
//! A packet is assembled once, into the buffer the pacer holds, and encrypted when it leaves
//! the pacer rather than before it enters: a packet the pacer drops costs no cipher pass, and
//! the SRTP index stays in wire order for each SSRC, since the pacer keeps a class in order
//! and an SSRC rides one class.

use bytes::Bytes;

use super::pacer::{Class, Outgoing, Pacer, Verdict};
use super::sender::{Datagram, SenderTransport};
use super::twcc::TwccSender;

/// What SRTP protection adds to a packet at most (an AEAD-GCM tag), which a padding packet's
/// admission counts before protecting it.
pub const SRTP_PADDING_OVERHEAD: usize = 16;

/// The transport's send path could not take a packet.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct SinkError(pub String);

/// Protect and write one packet to the peer. Implemented over the transport handle's
/// `send_rtp`, `send_rtcp`, and `peer_heard_at` (as `clock::monotonic` seconds).
pub trait PacketSink {
    fn send_rtp(&mut self, packet: &[u8]) -> Result<(), SinkError>;
    fn send_rtcp(&mut self, compound: &[u8]) -> Result<(), SinkError>;
    /// One SCTP packet for the DTLS association.
    fn send_data(&mut self, packet: &[u8]) -> Result<(), SinkError>;
    fn peer_heard_at(&self) -> f64;
}

/// The pacer and the transport-wide state of one peer in front of its sink.
pub struct PacedTransport<S: PacketSink> {
    sink: S,
    pub(crate) twcc: TwccSender,
    pacer: Option<Pacer>,
    released: Vec<Outgoing>,
}

impl<S: PacketSink> PacedTransport<S> {
    /// A transport pacing at `encoder_bps`, or passing packets straight through without one.
    pub fn new(sink: S, encoder_bps: Option<u64>, now: f64) -> Self {
        Self {
            sink,
            twcc: TwccSender::new(now),
            pacer: encoder_bps.map(|bps| Pacer::new(bps, now)),
            released: Vec::with_capacity(256),
        }
    }

    pub fn sink(&self) -> &S {
        &self.sink
    }

    pub fn sink_mut(&mut self) -> &mut S {
        &mut self.sink
    }

    /// The transport-wide state, which `RtpSender::apply_pacer_events` also takes.
    pub fn twcc_state(&mut self) -> &mut TwccSender {
        &mut self.twcc
    }

    /// Release what credit covers to the sink; the seconds until the driver calls again,
    /// `None` with nothing queued. The driver then hands the pacer's events to the video
    /// sender (`RtpSender::apply_pacer_events`). A write that fails drops the queues, as a
    /// reference chain dies with them.
    pub fn drain(&mut self, now: f64) -> Option<f64> {
        let pacer = self.pacer.as_mut()?;
        let mut released = std::mem::take(&mut self.released);
        let mut next = pacer.drain(now, &mut released);
        let mut failed = false;
        for item in released.drain(..) {
            if failed {
                pacer.drop_tag(item.tag);
                continue;
            }
            let written = match item.class {
                Class::Data => self.sink.send_data(&item.data),
                _ => self.sink.send_rtp(&item.data),
            };
            if let Err(e) = written {
                log::warn!(target: "webrtc", "pacer: send failed: {e}");
                pacer.wire_failed(item.tag, now);
                failed = true;
                next = None;
            }
        }
        self.released = released;
        next
    }

    /// Send one SCTP packet through the pacer as the data class: ahead of queued video, behind
    /// audio and RTCP. The driver holds its senders while `Pacer::data_blocked`.
    pub fn send_data(&mut self, data: Bytes, now: f64) -> Result<(), SinkError> {
        match self.pacer.as_mut() {
            None => self.sink.send_data(&data),
            Some(pacer) => match pacer.send(data.clone(), Class::Data, None, now) {
                Verdict::SendNow => self.sink.send_data(&data),
                _ => Ok(()),
            },
        }
    }
}

impl<S: PacketSink> SenderTransport for PacedTransport<S> {
    fn send_rtp(
        &mut self,
        packet: Datagram<'_>,
        class: Class,
        twcc_seq: Option<u16>,
        now: f64,
    ) -> Verdict {
        let Some(pacer) = self.pacer.as_mut() else {
            if let Err(e) = self.sink.send_rtp(&packet.to_vec()) {
                log::debug!(target: "webrtc", "send failed: {e}");
            }
            return Verdict::SendNow;
        };
        let data = Bytes::from(packet.to_vec());
        let verdict = pacer.send(data.clone(), class, twcc_seq, now);
        if verdict == Verdict::SendNow
            && let Err(e) = self.sink.send_rtp(&data)
        {
            log::warn!(target: "webrtc", "pacer: send failed: {e}");
            pacer.wire_failed(twcc_seq, now);
        }
        verdict
    }

    fn send_rtcp(&mut self, compound: &[u8], _now: f64) {
        if let Err(e) = self.sink.send_rtcp(compound) {
            log::debug!(target: "webrtc", "RTCP send failed: {e}");
        }
    }

    fn twcc(&mut self) -> &mut TwccSender {
        &mut self.twcc
    }

    fn pacer(&mut self) -> Option<&mut Pacer> {
        self.pacer.as_mut()
    }

    fn peer_heard_at(&self) -> f64 {
        self.sink.peer_heard_at()
    }

    fn padding_room(&mut self, size: usize, now: f64) -> bool {
        self.pacer
            .as_mut()
            .is_some_and(|p| p.padding_room(size + SRTP_PADDING_OVERHEAD, now))
    }

    fn send_padding(&mut self, packet: Datagram<'_>, now: f64) {
        let Some(pacer) = self.pacer.as_mut() else {
            return;
        };
        pacer.take_padding(packet.len() + SRTP_PADDING_OVERHEAD);
        if let Err(e) = self.sink.send_rtp(&packet.to_vec()) {
            log::debug!(target: "webrtc", "padding send failed: {e}");
            pacer.wire_failed(None, now);
        }
    }
}
