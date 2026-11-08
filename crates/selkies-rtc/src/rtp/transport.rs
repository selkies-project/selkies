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
    /// One RTP packet, its buffer handed over with room for the SRTP tag.
    fn send_rtp(&mut self, packet: Bytes) -> Result<(), SinkError>;
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
                _ => self.sink.send_rtp(item.data),
            };
            if let Err(e) = written {
                log::warn!(target: "webrtc", "pacer: send failed: {e}");
                pacer.wire_failed(item.tag, now);
                failed = true;
                next = None;
            } else if let Some(seq) = item.tag {
                self.twcc.released(seq, now);
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
            match self.sink.send_rtp(packet.to_bytes(SRTP_PADDING_OVERHEAD)) {
                Ok(()) => {
                    if let Some(seq) = twcc_seq {
                        self.twcc.released(seq, now);
                    }
                }
                Err(e) => log::debug!(target: "webrtc", "send failed: {e}"),
            }
            return Verdict::SendNow;
        };
        let data = packet.to_bytes(SRTP_PADDING_OVERHEAD);
        let verdict = pacer.send(data.clone(), class, twcc_seq, now);
        if verdict == Verdict::SendNow {
            match self.sink.send_rtp(data) {
                Ok(()) => {
                    if let Some(seq) = twcc_seq {
                        self.twcc.released(seq, now);
                    }
                }
                Err(e) => {
                    log::warn!(target: "webrtc", "pacer: send failed: {e}");
                    pacer.wire_failed(twcc_seq, now);
                }
            }
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

    fn send_padding(&mut self, packet: Datagram<'_>, twcc_seq: u16, now: f64) {
        let Some(pacer) = self.pacer.as_mut() else {
            return;
        };
        pacer.take_padding(packet.len() + SRTP_PADDING_OVERHEAD);
        match self.sink.send_rtp(packet.to_bytes(SRTP_PADDING_OVERHEAD)) {
            Ok(()) => self.twcc.released(twcc_seq, now),
            Err(e) => {
                log::debug!(target: "webrtc", "padding send failed: {e}");
                pacer.wire_failed(None, now);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtp::rtcp::pack_twcc_fci;

    #[derive(Default)]
    struct TestSink {
        rtp: Vec<Vec<u8>>,
    }

    impl PacketSink for TestSink {
        fn send_rtp(&mut self, packet: Bytes) -> Result<(), SinkError> {
            self.rtp.push(packet.to_vec());
            Ok(())
        }

        fn send_rtcp(&mut self, _compound: &[u8]) -> Result<(), SinkError> {
            Ok(())
        }

        fn send_data(&mut self, _packet: &[u8]) -> Result<(), SinkError> {
            Ok(())
        }

        fn peer_heard_at(&self) -> f64 {
            0.0
        }
    }

    fn send(transport: &mut PacedTransport<TestSink>, class: Class, seq: u16, now: f64) -> Verdict {
        let data = vec![0x80; 100];
        let datagram = Datagram {
            header: &data,
            payload: &[],
        };
        transport.send_rtp(datagram, class, Some(seq), now)
    }

    #[test]
    fn queued_video_gap_is_pending_when_bypassed_audio_is_acknowledged() {
        let mut transport = PacedTransport::new(TestSink::default(), Some(100_000), 0.0);
        transport.pacer().unwrap().set_credit(0.0, 0.0);
        let video = transport.twcc_state().next(1200, 0.0);
        assert_eq!(
            send(&mut transport, Class::Video, video, 0.0),
            Verdict::Queued
        );
        let audio = transport.twcc_state().next(100, 0.001);
        assert_eq!(
            send(&mut transport, Class::Rtcp, audio, 0.001),
            Verdict::SendNow
        );
        assert_eq!(transport.sink().rtp.len(), 1);
        assert_eq!(transport.pacer().unwrap().bytes_queued(), 100);

        transport.twcc_state().process_feedback(
            &pack_twcc_fci(video, &[None, Some(10.0)], 0),
            0.1,
            0.0,
        );
        let window = transport.twcc_state().take_window(0.1).unwrap();
        assert_eq!(
            (window.received, window.lost),
            (1, 0),
            "the acknowledged audio bypassed video that has not reached the socket"
        );

        transport.pacer().unwrap().set_credit(1000.0, 0.1);
        assert_eq!(transport.drain(0.1), None);
        assert_eq!(transport.sink().rtp.len(), 2);
        transport
            .twcc_state()
            .process_feedback(&pack_twcc_fci(video, &[Some(20.0)], 0), 0.2, 0.0);
        let late = transport.twcc_state().take_window(0.2).unwrap();
        assert_eq!((late.received, late.lost), (1, 0));
    }

    #[test]
    fn released_video_gap_is_loss_when_later_audio_is_acknowledged() {
        let mut transport = PacedTransport::new(TestSink::default(), Some(100_000), 0.0);
        transport.pacer().unwrap().set_credit(10_000.0, 0.0);
        let video = transport.twcc_state().next(1200, 0.0);
        send(&mut transport, Class::Video, video, 0.0);
        let audio = transport.twcc_state().next(100, 0.001);
        send(&mut transport, Class::Rtcp, audio, 0.001);
        assert_eq!(transport.sink().rtp.len(), 2);

        transport.twcc_state().process_feedback(
            &pack_twcc_fci(video, &[None, Some(10.0)], 0),
            0.1,
            0.0,
        );
        let window = transport.twcc_state().take_window(0.1).unwrap();
        assert_eq!((window.received, window.lost), (1, 1));
    }

    #[test]
    fn release_ordered_ack_resolves_a_pending_gap_and_a_late_positive_retracts_it() {
        let mut transport = PacedTransport::new(TestSink::default(), Some(100_000), 0.0);
        transport.pacer().unwrap().set_credit(0.0, 0.0);
        let video = transport.twcc_state().next(1200, 0.0);
        assert_eq!(
            send(&mut transport, Class::Video, video, 0.0),
            Verdict::Queued
        );
        let audio = transport.twcc_state().next(100, 0.001);
        send(&mut transport, Class::Rtcp, audio, 0.001);
        let bypass_feedback = pack_twcc_fci(video, &[None, Some(10.0)], 0);
        transport
            .twcc_state()
            .process_feedback(&bypass_feedback, 0.1, 0.0);
        transport.twcc_state().take_window(0.1).unwrap();

        transport.pacer().unwrap().set_credit(1000.0, 0.1);
        transport.drain(0.1);
        assert!(
            transport
                .twcc_state()
                .process_feedback(&bypass_feedback, 0.2, 0.0)
                .is_none(),
            "the audio's retained release position still precedes the video"
        );
        assert!(transport.twcc_state().take_window(0.2).is_none());

        transport.pacer().unwrap().set_credit(1000.0, 0.2);
        let later_video = transport.twcc_state().next(1200, 0.2);
        assert_eq!(
            send(&mut transport, Class::Video, later_video, 0.2),
            Verdict::SendNow
        );
        transport.twcc_state().process_feedback(
            &pack_twcc_fci(video, &[None, Some(10.0), Some(30.0)], 0),
            0.3,
            0.0,
        );
        let loss = transport.twcc_state().take_window(0.3).unwrap();
        assert_eq!((loss.received, loss.lost), (1, 1));

        transport
            .twcc_state()
            .process_feedback(&pack_twcc_fci(video, &[Some(40.0)], 0), 0.4, 0.0);
        let retracted = transport.twcc_state().take_window(0.4).unwrap();
        assert_eq!((retracted.received, retracted.lost), (1, 0));
    }

    fn all_missing(base: u16) -> Vec<u8> {
        let mut fci = base.to_be_bytes().to_vec();
        fci.extend_from_slice(&1u16.to_be_bytes());
        fci.extend_from_slice(&[0; 4]);
        fci.extend_from_slice(&1u16.to_be_bytes());
        fci
    }

    #[test]
    fn unpaced_media_is_marked_released() {
        let mut transport = PacedTransport::new(TestSink::default(), None, 0.0);
        let video = transport.twcc_state().next(1200, 0.0);
        assert_eq!(
            send(&mut transport, Class::Video, video, 0.0),
            Verdict::SendNow
        );
        transport
            .twcc_state()
            .process_feedback(&all_missing(video), 0.1, 0.0);
        let window = transport.twcc_state().take_window(0.1).unwrap();
        assert_eq!((window.received, window.lost), (0, 1));
    }

    #[test]
    fn padding_is_marked_released_without_changing_its_accounting() {
        let mut transport = PacedTransport::new(TestSink::default(), Some(100_000), 0.0);
        let padding = transport.twcc_state().next_padding(100, 0.0);
        let data = vec![0x80; 100];
        let datagram = Datagram {
            header: &data,
            payload: &[],
        };
        transport.send_padding(datagram, padding, 0.0);
        transport
            .twcc_state()
            .process_feedback(&all_missing(padding), 0.1, 0.0);
        let window = transport.twcc_state().take_window(0.1).unwrap();
        assert_eq!((window.received, window.lost), (0, 1));
        assert!(window.padding_bps > 0);
    }
}
