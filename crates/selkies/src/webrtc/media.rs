/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A pixelflux frame as the RTP layer takes it, with no copy of its bytes.
//!
//! RTP senders live across capture restarts, so the display keeps its own monotonic 90 kHz clock
//! (`VideoClock`): each frame's timestamp is its capture instant, as pixelflux stamped it on
//! `CLOCK_MONOTONIC`, from an anchor scoped to the display rather than from `frame_id`, a `u16`
//! that wraps, restarts at 0 on every capture start, and changes its implied step on a live
//! frame-rate raise, all backward jumps on a live sender. The capture instant rather than the
//! delivery keeps the encode's varying duration out of the RTP clock, so a receiver mapping it
//! through the sender reports reads capture times; a path that stamps no capture falls back to
//! the delivery. Ties bump one tick, so the clock is strictly increasing.
//!
//! The stripe header (`STRIPE_HEADER_LEN`) is pixelflux's wire framing for the WebSockets path:
//! a `0x04` type byte for a video frame, then a byte whose high nibble is the codec and whose
//! low nibble says whether the frame is a key frame (`0x01`) and an anchor (`FRAME_ANCHOR`).

use bytes::Bytes;
use pixelflux::sink::{Frame, Reference};
use selkies_rtc::rtp::codecs::Codec as RtpCodec;
use selkies_rtc::rtp::{EncodedFrame, FrameTiming, clock};

use crate::capture::frame_bytes;
use crate::display::capture::FRAME_ANCHOR;

/// Bytes of the header pixelflux puts in front of a frame's bitstream.
pub const STRIPE_HEADER_LEN: usize = 12;

/// The codec a video header's high nibble names.
fn wire_codec(id: u8) -> Option<RtpCodec> {
    match id {
        1 => Some(RtpCodec::H264),
        2 => Some(RtpCodec::Vp8),
        3 => Some(RtpCodec::Vp9),
        4 => Some(RtpCodec::Av1),
        5 => Some(RtpCodec::H265),
        _ => None,
    }
}

/// `(frame id, frame it predicts from)` as the sender's dependency descriptor reads it: `None`
/// from an encoder that cannot say, `(id, None)` for a frame decoding on its own.
pub fn dependency(frame_id: i32, reference: Reference) -> Option<(u16, Option<u16>)> {
    let id = (frame_id & 0xFFFF) as u16;
    match reference {
        Reference::Untracked => None,
        Reference::None => Some((id, None)),
        Reference::Frame(r) => Some((id, Some(r))),
    }
}

/// A display's 90 kHz clock, which survives its capture's restarts (module docblock).
#[derive(Debug, Default)]
pub struct VideoClock {
    anchor_ns: Option<i64>,
    last: Option<u64>,
}

impl VideoClock {
    /// The RTP timestamp of a frame captured at `capture_ns`, 0 or less where the path stamps
    /// no capture, read at `now_ns` instead; returns the instant it read too.
    pub fn stamp(&mut self, capture_ns: i64, now_ns: i64) -> (u32, i64) {
        let captured = if capture_ns > 0 { capture_ns } else { now_ns };
        let anchor = *self.anchor_ns.get_or_insert(captured);
        let mut pts = ((captured - anchor).max(0) as i128 * 9 / 100_000) as u64;
        if let Some(last) = self.last
            && pts <= last
        {
            pts = last + 1;
        }
        self.last = Some(pts);
        (pts as u32, captured)
    }
}

/// One pixelflux frame as an `EncodedFrame`, or `None` for one with no bitstream behind its
/// header. Slices the encoder's own buffer. A frame with no `0x04` header is taken for a key frame
/// of the codec the sender is running, as a header the RTP layer cannot read says nothing to gate
/// on.
pub fn encoded_frame(frame: &Frame, clock_state: &mut VideoClock) -> Option<EncodedFrame> {
    let data = frame.as_bytes();
    if data.len() <= STRIPE_HEADER_LEN {
        return None;
    }
    let video = data[0] == 0x04;
    let keyframe = !video || data[1] & 0x0F == 0x01;
    let (timestamp, captured) = clock_state.stamp(frame.timing.capture_ns, clock::monotonic_ns());
    let codec = if video {
        wire_codec(data[1] >> 4)
    } else {
        None
    };
    let anchor = video && data[1] & FRAME_ANCHOR != 0;
    let bytes: Bytes = frame_bytes(frame).slice(STRIPE_HEADER_LEN..);
    Some(EncodedFrame {
        data: bytes,
        timestamp,
        keyframe,
        timing: Some(FrameTiming {
            capture_ns: captured,
            encode_start_ns: frame.timing.encode_start_ns,
            encode_end_ns: frame.timing.encode_end_ns,
        }),
        dependency: dependency(frame.frame_id, frame.reference),
        codec,
        anchor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_is_strictly_increasing_at_90khz_from_the_first_capture() {
        let mut c = VideoClock::default();
        assert_eq!(c.stamp(1_000_000_000, 5).0, 0);
        assert_eq!(c.stamp(1_000_000_000 + 1_000_000_000 / 60, 5).0, 1499);
        assert_eq!(c.stamp(1_000_000_000 + 1_000_000_000 / 60, 5).0, 1500);
        assert_eq!(c.stamp(1_000_000_000, 5).0, 1501);
    }

    #[test]
    fn an_unstamped_capture_reads_the_delivery() {
        let mut c = VideoClock::default();
        assert_eq!(c.stamp(0, 2_000_000_000), (0, 2_000_000_000));
        assert_eq!(c.stamp(-1, 3_000_000_000), (90_000, 3_000_000_000));
    }

    /// A capture's frame tagged with its stripe type and codec bytes.
    fn stripe(type_byte: u8, codec_byte: u8) -> Frame {
        let mut data = vec![type_byte, codec_byte, 0, 1, 0, 0, 0x02, 0xD0, 0, 0, 0, 0];
        data.extend_from_slice(&[0u8; 16]);
        Frame {
            data: std::sync::Arc::new(data),
            codec: pixelflux::encoders::Codec::H264,
            stripe_y_start: 0,
            stripe_height: 720,
            frame_id: 1,
            timing: pixelflux::sink::FrameTiming::default(),
            reference: Reference::Untracked,
        }
    }

    #[test]
    fn each_video_frame_carries_the_codec_its_header_names_a_jpeg_stripe_none() {
        let mut clock = VideoClock::default();
        let codec = |type_byte: u8, id: u8, clock: &mut VideoClock| {
            encoded_frame(&stripe(type_byte, (id << 4) | 0x01), clock)
                .unwrap()
                .codec
        };
        assert_eq!(codec(0x04, 0, &mut clock), None);
        assert_eq!(codec(0x04, 1, &mut clock), Some(RtpCodec::H264));
        assert_eq!(codec(0x04, 2, &mut clock), Some(RtpCodec::Vp8));
        assert_eq!(codec(0x04, 3, &mut clock), Some(RtpCodec::Vp9));
        assert_eq!(codec(0x04, 4, &mut clock), Some(RtpCodec::Av1));
        assert_eq!(codec(0x04, 5, &mut clock), Some(RtpCodec::H265));
        assert_eq!(codec(0x03, 0, &mut clock), None);
    }

    #[test]
    fn a_frame_is_sliced_past_its_header_and_read_from_it() {
        let mut clock = VideoClock::default();
        let key = encoded_frame(&stripe(0x04, 0x11), &mut clock).unwrap();
        assert!(key.keyframe);
        assert_eq!(key.data.len(), 16, "the header is off, the bytes shared");
        let delta = encoded_frame(&stripe(0x04, 0x10), &mut clock).unwrap();
        assert!(!delta.keyframe);
        let anchor = encoded_frame(&stripe(0x04, 0x10 | FRAME_ANCHOR), &mut clock).unwrap();
        assert!(anchor.anchor && !anchor.keyframe);
        let bare = Frame {
            data: std::sync::Arc::new(vec![0x04; STRIPE_HEADER_LEN]),
            ..stripe(0x04, 0x11)
        };
        assert!(
            encoded_frame(&bare, &mut clock).is_none(),
            "no bitstream behind the header"
        );
    }

    #[test]
    fn dependency_maps_each_reference() {
        assert_eq!(dependency(5, Reference::Untracked), None);
        assert_eq!(dependency(5, Reference::None), Some((5, None)));
        assert_eq!(
            dependency(70_000, Reference::Frame(3)),
            Some((4464, Some(3)))
        );
    }
}
