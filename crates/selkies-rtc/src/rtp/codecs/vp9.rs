/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! RFC 9628 packetization of VP9 frames in the non-flexible mode a single-layer stream
//! takes: a 15-bit picture id on every packet, the inter-picture flag off on key frames,
//! which also open with the scalability structure (one spatial layer at the frame's size,
//! one temporal layer, each picture predicted from the one before), and the frame's first
//! and last packets marked.

use bytes::Bytes;

use super::{PACKET_MAX, PackError};
use crate::rtp::packet::Payload;

pub const FLAG_I: u8 = 0x80;
pub const FLAG_P: u8 = 0x40;
pub const FLAG_L: u8 = 0x20;
pub const FLAG_F: u8 = 0x10;
pub const FLAG_B: u8 = 0x08;
pub const FLAG_E: u8 = 0x04;
pub const FLAG_V: u8 = 0x02;
pub const FLAG_Z: u8 = 0x01;

const SYNC_CODE: (u32, u32, u32) = (0x49, 0x83, 0x42);

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl BitReader<'_> {
    fn u(&mut self, bits: usize) -> Option<u32> {
        let mut value = 0u32;
        for _ in 0..bits {
            let byte = *self.data.get(self.pos >> 3)?;
            value = (value << 1) | ((byte >> (7 - (self.pos & 7))) & 1) as u32;
            self.pos += 1;
        }
        Some(value)
    }
}

pub fn profile(frame: &[u8]) -> u8 {
    let b = frame[0];
    ((b >> 5) & 1) | (((b >> 4) & 1) << 1)
}

/// Read from the uncompressed header: not a shown existing frame, and `frame_type`
/// KEY_FRAME. Profile 3 carries a reserved bit ahead of both.
pub fn is_key(frame: &[u8]) -> bool {
    let Some(&b) = frame.first() else {
        return false;
    };
    if b >> 6 != 0b10 {
        return false;
    }
    if profile(frame) == 3 {
        b & 0x04 == 0 && b & 0x02 == 0
    } else {
        b & 0x08 == 0 && b & 0x04 == 0
    }
}

/// The frame size a key frame's uncompressed header states, or `None` for anything that
/// is not a parseable key frame.
pub fn key_frame_size(frame: &[u8]) -> Option<(u16, u16)> {
    if frame.len() < 10 || !is_key(frame) {
        return None;
    }
    let mut r = BitReader {
        data: frame,
        pos: 0,
    };
    r.u(2)?;
    let profile = r.u(1)? | (r.u(1)? << 1);
    if profile == 3 {
        r.u(1)?;
    }
    r.u(3)?;
    r.u(1)?;
    if (r.u(8)?, r.u(8)?, r.u(8)?) != SYNC_CODE {
        return None;
    }
    if profile >= 2 {
        r.u(1)?;
    }
    let color_space = r.u(3)?;
    if color_space != 7 {
        r.u(1)?;
        if profile == 1 || profile == 3 {
            r.u(3)?;
        }
    } else if profile == 1 || profile == 3 {
        r.u(1)?;
    }
    let width = r.u(16)? as u16 + 1;
    let height = r.u(16)? as u16 + 1;
    Some((width, height))
}

fn scalability_structure(frame: &[u8]) -> Vec<u8> {
    let size = key_frame_size(frame);
    // One spatial layer, its resolution when known, one picture group whose single picture
    // (temporal layer 0, an up-switch point) references the picture before it.
    let mut ss = vec![0x08 | if size.is_some() { 0x10 } else { 0 }];
    if let Some((w, h)) = size {
        ss.extend_from_slice(&w.to_be_bytes());
        ss.extend_from_slice(&h.to_be_bytes());
    }
    ss.extend_from_slice(&[1, 0x14, 1]);
    ss
}

/// The RTP payloads of one frame under `picture_id`.
pub fn packetize(frame: &Bytes, picture_id: u16, keyframe: bool) -> Vec<Payload> {
    let pid = (0x8000 | picture_id).to_be_bytes();
    let ss = if keyframe {
        scalability_structure(frame)
    } else {
        Vec::new()
    };
    let mut payloads = Vec::with_capacity(frame.len() / PACKET_MAX + 1);
    let length = frame.len();
    let mut pos = 0;
    let mut first = true;
    while pos < length || first {
        let mut flags = FLAG_I | if keyframe { 0 } else { FLAG_P };
        if first {
            flags |= FLAG_B | if keyframe { FLAG_V } else { 0 };
        }
        let mut descriptor = Vec::with_capacity(3 + ss.len());
        descriptor.push(flags);
        descriptor.extend_from_slice(&pid);
        if first {
            descriptor.extend_from_slice(&ss);
        }
        let size = (length - pos).min(PACKET_MAX - descriptor.len());
        let end = pos + size;
        if end >= length {
            descriptor[0] = flags | FLAG_E;
        }
        payloads.push(Payload::two(descriptor, frame.slice(pos..end)));
        pos = end;
        first = false;
    }
    payloads
}

/// The frame bytes behind one packet's descriptor.
pub fn depayload(payload: &Bytes) -> Result<Bytes, PackError> {
    const TRUNCATED: PackError = PackError("VP9 descriptor is truncated");
    let flags = *payload
        .first()
        .ok_or(PackError("VP9 descriptor is too short"))?;
    let mut pos = 1;
    let byte = |i: usize| payload.get(i).copied().ok_or(TRUNCATED);
    if flags & FLAG_I != 0 {
        pos += if byte(pos)? & 0x80 != 0 { 2 } else { 1 };
    }
    if flags & FLAG_L != 0 {
        pos += 1;
        if flags & FLAG_F == 0 {
            pos += 1;
        }
    }
    if flags & FLAG_F != 0 && flags & FLAG_P != 0 {
        loop {
            let more = byte(pos)? & 1;
            pos += 1;
            if more == 0 {
                break;
            }
        }
    }
    if flags & FLAG_V != 0 {
        let b = byte(pos)?;
        pos += 1;
        let layers = (b >> 5) as usize + 1;
        if b & 0x10 != 0 {
            pos += 4 * layers;
        }
        if b & 0x08 != 0 {
            let groups = byte(pos)? as usize;
            pos += 1;
            for _ in 0..groups {
                pos += 1 + ((byte(pos)? >> 2) & 3) as usize;
            }
        }
    }
    if pos > payload.len() {
        return Err(TRUNCATED);
    }
    Ok(payload.slice(pos..))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A profile-0 key frame's uncompressed header up to the frame size.
    pub(crate) fn key_header(width: u16, height: u16) -> Vec<u8> {
        let mut bits = String::from("10000010");
        for b in [0x49u8, 0x83, 0x42] {
            bits.push_str(&format!("{b:08b}"));
        }
        bits.push_str("0010");
        bits.push_str(&format!("{:016b}{:016b}", width - 1, height - 1));
        while bits.len() % 8 != 0 {
            bits.push('0');
        }
        (0..bits.len())
            .step_by(8)
            .map(|i| u8::from_str_radix(&bits[i..i + 8], 2).unwrap())
            .collect()
    }

    #[test]
    fn key_frames_are_read_from_the_header_and_packetized_with_the_structure() {
        let mut key = key_header(1280, 720);
        key.extend((0..256u32).map(|i| i as u8).cycle().take(256 * 12));
        let key = Bytes::from(key);
        let mut delta = vec![0x84u8];
        delta.extend((0..256u32).map(|i| i as u8).cycle().take(512));
        let delta = Bytes::from(delta);
        assert!(is_key(&key) && !is_key(&delta));
        assert_eq!(key_frame_size(&key), Some((1280, 720)));
        let payloads = packetize(&key, 1000, true);
        assert!(payloads.iter().all(|p| p.len() <= PACKET_MAX));
        let first = payloads[0].to_bytes();
        let last = payloads.last().unwrap().to_bytes();
        assert_eq!(first[0] & 0x88, 0x88);
        assert_ne!(first[0] & FLAG_V, 0);
        assert!(payloads.iter().all(|p| p.to_bytes()[0] & FLAG_P == 0));
        assert_ne!(last[0] & FLAG_E, 0);
        assert!(
            payloads[..payloads.len() - 1]
                .iter()
                .all(|p| p.to_bytes()[0] & FLAG_E == 0)
        );
        assert!(payloads.iter().all(|p| {
            let b = p.to_bytes();
            b[0] & FLAG_I != 0 && b[1] & 0x80 != 0
        }));
        let ss = &first[3..11];
        assert_eq!(ss[0], 0x18);
        assert_eq!(u16::from_be_bytes([ss[1], ss[2]]), 1280);
        assert_eq!(u16::from_be_bytes([ss[3], ss[4]]), 720);
        assert_eq!(&ss[5..8], &[1, 0x14, 1]);
        let back: Vec<u8> = payloads
            .iter()
            .flat_map(|p| depayload(&p.to_bytes()).unwrap().to_vec())
            .collect();
        assert_eq!(back, key.to_vec());
        let payloads2 = packetize(&delta, 1001, false);
        assert!(payloads2.iter().all(|p| p.to_bytes()[0] & FLAG_P != 0));
        assert_eq!(payloads2[0].to_bytes()[0] & FLAG_V, 0);
        let pid =
            ((payloads2[0].to_bytes()[1] & 0x7F) as u16) << 8 | payloads2[0].to_bytes()[2] as u16;
        assert_eq!(pid, 1001);
        let back: Vec<u8> = payloads2
            .iter()
            .flat_map(|p| depayload(&p.to_bytes()).unwrap().to_vec())
            .collect();
        assert_eq!(back, delta.to_vec());
    }
}
