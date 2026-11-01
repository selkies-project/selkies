/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 * This file incorporates work covered by the following copyright and
 * permission notice:
 *
 *   Copyright (c) Jeremy Lainé.
 *   All rights reserved.
 *
 *   Redistribution and use in source and binary forms, with or without
 *   modification, are permitted provided that the following conditions are met:
 *
 *       * Redistributions of source code must retain the above copyright notice,
 *       this list of conditions and the following disclaimer.
 *       * Redistributions in binary form must reproduce the above copyright notice,
 *       this list of conditions and the following disclaimer in the documentation
 *       and/or other materials provided with the distribution.
 *       * Neither the name of aiortc nor the names of its contributors may
 *       be used to endorse or promote products derived from this software without
 *       specific prior written permission.
 *
 *   THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
 *   ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
 *   WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
 *   DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
 *   FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
 *   DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
 *   SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
 *   CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
 *   OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
 *   OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
 */

//! RFC 7741 packetization of VP8 frames: every packet behind a payload descriptor with the
//! extension byte and a picture id (one byte under 128, else two with the M bit), the
//! partition-start bit on the frame's first packet.

use bytes::Bytes;

use super::{PACKET_MAX, PackError};
use crate::rtp::packet::Payload;

fn descriptor(partition_start: bool, picture_id: u16) -> Vec<u8> {
    let octet = if partition_start { 0x10 } else { 0 };
    let mut d = vec![0x80 | octet, 0x80];
    if picture_id < 128 {
        d.push(picture_id as u8);
    } else {
        d.extend_from_slice(&(0x8000 | picture_id).to_be_bytes());
    }
    d
}

/// The RTP payloads of one frame under `picture_id`.
pub fn packetize(frame: &Bytes, picture_id: u16) -> Vec<Payload> {
    let mut payloads = Vec::with_capacity(frame.len() / PACKET_MAX + 1);
    let mut pos = 0;
    let mut first = true;
    while pos < frame.len() {
        let descr = descriptor(first, picture_id);
        let size = (frame.len() - pos).min(PACKET_MAX - descr.len());
        payloads.push(Payload::two(descr, frame.slice(pos..pos + size)));
        first = false;
        pos += size;
    }
    payloads
}

/// The frame bytes behind one packet's descriptor.
pub fn depayload(payload: &Bytes) -> Result<Bytes, PackError> {
    if payload.is_empty() {
        return Err(PackError("VPX descriptor is too short"));
    }
    let octet = payload[0];
    let mut pos = 1;
    if octet & 0x80 != 0 {
        let ext = *payload
            .get(pos)
            .ok_or(PackError("VPX descriptor has truncated extended bits"))?;
        pos += 1;
        if ext & 0x80 != 0 {
            let b = *payload
                .get(pos)
                .ok_or(PackError("VPX descriptor has truncated PictureID"))?;
            pos += if b & 0x80 != 0 { 2 } else { 1 };
            if payload.len() < pos {
                return Err(PackError("VPX descriptor has truncated long PictureID"));
            }
        }
        if ext & 0x40 != 0 {
            pos += 1;
            if payload.len() < pos {
                return Err(PackError("VPX descriptor has truncated TL0PICIDX"));
            }
        }
        if ext & 0x30 != 0 {
            pos += 1;
            if payload.len() < pos {
                return Err(PackError("VPX descriptor has truncated T/K"));
            }
        }
    }
    Ok(payload.slice(pos..))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_round_trips_with_its_picture_id() {
        let frame = Bytes::from((0..3000u32).map(|i| i as u8).collect::<Vec<_>>());
        let payloads = packetize(&frame, 300);
        assert_eq!(payloads.len(), 3);
        let first = payloads[0].to_bytes();
        assert_eq!(&first[..4], &[0x90, 0x80, 0x81, 0x2c]);
        assert_eq!(payloads[1].to_bytes()[0], 0x80);
        assert!(payloads.iter().all(|p| p.len() <= PACKET_MAX));
        let back: Vec<u8> = payloads
            .iter()
            .flat_map(|p| depayload(&p.to_bytes()).unwrap().to_vec())
            .collect();
        assert_eq!(back, frame.to_vec());
        let short = packetize(&Bytes::from_static(&[1, 2, 3]), 5);
        assert_eq!(&short[0].to_bytes()[..], &[0x90, 0x80, 5, 1, 2, 3]);
    }

    #[test]
    fn rtc_rtp_payloader_agrees_byte_for_byte_with_a_picture_id() {
        use rtc_rtp::packetizer::Payloader;
        let frame = Bytes::from((0..3000u32).map(|i| i as u8).collect::<Vec<_>>());
        // Pion's payloader starts its picture id at 0 and keeps it private.
        let mut theirs = rtc_rtp::codec::vp8::Vp8Payloader::default();
        theirs.enable_picture_id = true;
        let theirs = theirs.payload(PACKET_MAX, &frame).unwrap();
        let ours: Vec<Bytes> = packetize(&frame, 0).iter().map(|p| p.to_bytes()).collect();
        assert_eq!(ours, theirs);
    }
}
