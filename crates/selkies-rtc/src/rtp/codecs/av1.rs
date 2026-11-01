/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! AV1 RTP payload format (AOM, v1.0) packetization of temporal units, and the reassembly
//! of a received frame's packets back into one.
//!
//! Packing: the temporal delimiter, tile lists, and padding go; every other OBU becomes an
//! element without its size field. Elements fill packets behind a one-byte aggregation
//! header (Z: the first element continues an OBU from the previous packet, Y: the last
//! continues in the next, W: the element count when three or fewer, N: the first packet
//! of a coded video sequence), a LEB128 length ahead of every element but a counted
//! packet's last, and an OBU larger than the room a packet has left is split across as
//! many packets as it needs. Elements are slices of the temporal unit; only the headers
//! and lengths are written.
//!
//! Assembly runs over a frame's packets in sequence order once the jitter buffer has them
//! all: fragments are joined across packets by the Y and Z flags, each OBU gets the size
//! field the elements travel without, and a temporal delimiter opens the unit, which is
//! then the low-overhead bitstream the virtual camera's decoder reads. A fragment whose
//! start was lost with a packet is dropped, as are the rest of that OBU's fragments.

use bytes::Bytes;

use super::{PACKET_MAX, PackError};
use crate::rtp::packet::Payload;

pub const OBU_SEQUENCE_HEADER: u8 = 1;
pub const OBU_TEMPORAL_DELIMITER: u8 = 2;
pub const OBU_FRAME_HEADER: u8 = 3;
pub const OBU_FRAME: u8 = 6;
pub const OBU_TILE_LIST: u8 = 8;
pub const OBU_PADDING: u8 = 15;

const AGGREGATION_HEADER_SIZE: usize = 1;
/// The smallest fragment worth starting in a packet's remaining room.
const MIN_FRAGMENT: usize = 16;

pub fn leb128(mut value: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(2);
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            out.push(byte | 0x80);
        } else {
            out.push(byte);
            return out;
        }
    }
}

fn leb128_len(value: usize) -> usize {
    let mut n = 1;
    let mut v = value >> 7;
    while v != 0 {
        n += 1;
        v >>= 7;
    }
    n
}

pub fn read_leb128(data: &[u8], mut pos: usize) -> Result<(usize, usize), PackError> {
    let mut value = 0usize;
    let mut shift = 0;
    loop {
        if pos >= data.len() || shift > 56 {
            return Err(PackError("LEB128 is truncated"));
        }
        let byte = data[pos];
        pos += 1;
        value |= ((byte & 0x7F) as usize) << shift;
        if byte & 0x80 == 0 {
            return Ok((value, pos));
        }
        shift += 7;
    }
}

/// One OBU of a temporal unit: its type, its header without the size flag (so the element
/// carries no size field), and its payload as a slice of the unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Obu {
    pub obu_type: u8,
    pub header: Vec<u8>,
    pub payload: Bytes,
}

/// The OBUs of a temporal unit.
pub fn obus(tu: &Bytes) -> Result<Vec<Obu>, PackError> {
    let mut out = Vec::with_capacity(4);
    let mut pos = 0;
    while pos < tu.len() {
        let header = tu[pos];
        let obu_type = (header >> 3) & 0x0F;
        let has_extension = header & 0x04 != 0;
        let has_size = header & 0x02 != 0;
        let mut start = pos + 1 + has_extension as usize;
        let size = if has_size {
            let (size, next) = read_leb128(tu, start)?;
            start = next;
            size
        } else {
            tu.len().saturating_sub(start)
        };
        let end = start + size;
        if end > tu.len() || start > tu.len() {
            return Err(PackError("OBU is truncated"));
        }
        let mut header_bytes = vec![header & !0x02];
        if has_extension {
            header_bytes.push(tu[pos + 1]);
        }
        out.push(Obu {
            obu_type,
            header: header_bytes,
            payload: tu.slice(start..end),
        });
        pos = end;
    }
    Ok(out)
}

/// Whether the unit opens with a key frame: its first frame header or frame OBU's
/// `frame_type`, read past the temporal delimiter and sequence header. A shown existing
/// frame, or a unit without a frame, is a delta.
pub fn is_key(obus: &[Obu]) -> bool {
    for obu in obus {
        if obu.obu_type == OBU_FRAME_HEADER || obu.obu_type == OBU_FRAME {
            let Some(&first) = obu.payload.first() else {
                return false;
            };
            return first & 0x80 == 0 && (first >> 5) & 0x03 == 0;
        }
    }
    false
}

/// The pieces of one element: its header bytes and payload slice, or a slice of a
/// fragmented OBU.
#[derive(Clone)]
struct Element {
    pieces: Vec<Bytes>,
}

impl Element {
    fn len(&self) -> usize {
        self.pieces.iter().map(|p| p.len()).sum()
    }

    /// The element's bytes from `start` for `take`, as slices of its pieces.
    fn slice(&self, start: usize, take: usize) -> Element {
        let mut pieces = Vec::with_capacity(2);
        let mut skip = start;
        let mut left = take;
        for piece in &self.pieces {
            if left == 0 {
                break;
            }
            if skip >= piece.len() {
                skip -= piece.len();
                continue;
            }
            let end = (skip + left).min(piece.len());
            pieces.push(piece.slice(skip..end));
            left -= end - skip;
            skip = 0;
        }
        Element { pieces }
    }
}

struct Packets {
    out: Vec<Payload>,
    current: Vec<Element>,
    used: usize,
    continues_previous: bool,
    keyframe: bool,
}

impl Packets {
    fn flush(&mut self, continues_next: bool) {
        let count = self.current.len();
        let w = if count <= 3 { count } else { 0 };
        let header = if self.continues_previous { 0x80 } else { 0 }
            | if continues_next { 0x40 } else { 0 }
            | (w << 4) as u8
            | if self.keyframe && self.out.is_empty() {
                0x08
            } else {
                0
            };
        let mut pieces: Vec<Bytes> = Vec::with_capacity(1 + 2 * count);
        let mut head = vec![header];
        for (index, element) in self.current.iter().enumerate() {
            if w == 0 || index < count - 1 {
                head.extend(leb128(element.len()));
            }
            // Written bytes gather in one piece until a frame slice interrupts them.
            for piece in &element.pieces {
                if piece.len() < 8 {
                    head.extend_from_slice(piece);
                } else {
                    if !head.is_empty() {
                        pieces.push(Bytes::from(std::mem::take(&mut head)));
                    }
                    pieces.push(piece.clone());
                }
            }
        }
        if !head.is_empty() {
            pieces.push(Bytes::from(head));
        }
        self.out.push(Payload::from_pieces(pieces));
        self.current.clear();
        self.used = AGGREGATION_HEADER_SIZE;
        self.continues_previous = continues_next;
    }

    fn room(&self) -> usize {
        PACKET_MAX.saturating_sub(self.used + leb128_len(PACKET_MAX))
    }
}

/// The RTP payloads of a temporal unit's OBUs.
pub fn packetize(obus: &[Obu], keyframe: bool) -> Vec<Payload> {
    let mut packets = Packets {
        out: Vec::with_capacity(4),
        current: Vec::with_capacity(4),
        used: AGGREGATION_HEADER_SIZE,
        continues_previous: false,
        keyframe,
    };
    for obu in obus {
        if matches!(
            obu.obu_type,
            OBU_TEMPORAL_DELIMITER | OBU_TILE_LIST | OBU_PADDING
        ) {
            continue;
        }
        let element = Element {
            pieces: vec![Bytes::from(obu.header.clone()), obu.payload.clone()],
        };
        let len = element.len();
        // Every element is costed with its length prefix; a counted packet's last element
        // drops it, which only leaves room to spare.
        let need = leb128_len(len) + len;
        if packets.used + need <= PACKET_MAX {
            packets.used += need;
            packets.current.push(element);
            continue;
        }
        let mut room = packets.room();
        if room < MIN_FRAGMENT {
            packets.flush(false);
            room = packets.room();
        }
        let mut pos = 0;
        while pos < len {
            let take = (len - pos).min(room);
            packets.current.push(element.slice(pos, take));
            packets.used += leb128_len(take) + take;
            pos += take;
            if pos < len {
                packets.flush(true);
                room = packets.room();
            }
        }
    }
    if !packets.current.is_empty() || packets.out.is_empty() {
        packets.flush(false);
    }
    packets.out
}

/// A packet's Z and Y flags and its OBU elements, sizes read off.
fn elements(payload: &[u8]) -> Result<(bool, bool, Vec<&[u8]>), PackError> {
    let header = payload[0];
    let w = ((header >> 4) & 3) as usize;
    let mut pos = 1;
    let mut out = Vec::new();
    while pos < payload.len() {
        let size = if w == 0 || out.len() < w - 1 {
            let (size, next) = read_leb128(payload, pos)?;
            pos = next;
            size
        } else {
            payload.len() - pos
        };
        if pos + size > payload.len() {
            return Err(PackError("OBU element is truncated"));
        }
        out.push(&payload[pos..pos + size]);
        pos += size;
    }
    Ok((header & 0x80 != 0, header & 0x40 != 0, out))
}

/// An OBU element with the size field the bitstream format wants.
fn sized_obu(obu: &[u8], out: &mut Vec<u8>) {
    let header = obu[0];
    let header_size = if header & 0x04 != 0 { 2 } else { 1 };
    if header & 0x02 != 0 || obu.len() < header_size {
        out.extend_from_slice(obu);
        return;
    }
    out.push(header | 0x02);
    out.extend_from_slice(&obu[1..header_size]);
    out.extend(leb128(obu.len() - header_size));
    out.extend_from_slice(&obu[header_size..]);
}

/// The temporal unit a frame's packets carry, in the low-overhead bitstream format: a
/// temporal delimiter, then every OBU element made whole across packets and given its
/// size field.
pub fn assemble(payloads: &[Bytes]) -> Bytes {
    let mut out = vec![0x12u8, 0x00];
    let mut pending: Vec<u8> = Vec::new();
    for payload in payloads {
        if payload.is_empty() {
            continue;
        }
        let Ok((continues, continued, elements)) = elements(payload) else {
            pending.clear();
            continue;
        };
        let count = elements.len();
        for (index, element) in elements.into_iter().enumerate() {
            let first = index == 0;
            let last = index + 1 == count;
            if first && continues {
                if pending.is_empty() {
                    // The OBU's start went with a lost packet; so does its rest.
                    continue;
                }
                pending.extend_from_slice(element);
            } else {
                pending.clear();
                pending.extend_from_slice(element);
            }
            if last && continued {
                continue;
            }
            if !pending.is_empty() {
                sized_obu(&pending, &mut out);
            }
            pending.clear();
        }
    }
    Bytes::from(out)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn obu(obu_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![(obu_type << 3) | 0x02];
        v.extend(leb128(payload.len()));
        v.extend_from_slice(payload);
        v
    }

    fn element(obu_bytes: &[u8]) -> Vec<u8> {
        let (_, pos) = read_leb128(obu_bytes, 1).unwrap();
        let mut v = vec![obu_bytes[0] & !0x02];
        v.extend_from_slice(&obu_bytes[pos..]);
        v
    }

    fn pattern(n: usize) -> Vec<u8> {
        (0..256u32).map(|i| i as u8).cycle().take(n).collect()
    }

    /// A packet's Z, Y, W, and N.
    type Flags = (bool, bool, usize, bool);

    /// Each packet's flags and the OBUs re-assembled across packets.
    fn walk(payloads: &[Payload]) -> (Vec<Flags>, Vec<Vec<u8>>) {
        let mut flags = Vec::new();
        let mut obus_out: Vec<Vec<u8>> = Vec::new();
        let mut pending: Vec<u8> = Vec::new();
        for p in payloads {
            let p = p.to_bytes();
            let (z, y, w, n) = (
                p[0] & 0x80 != 0,
                p[0] & 0x40 != 0,
                ((p[0] >> 4) & 3) as usize,
                p[0] & 0x08 != 0,
            );
            flags.push((z, y, w, n));
            let (_, _, els) = elements(&p).unwrap();
            if w != 0 {
                assert_eq!(els.len(), w);
            }
            for (i, el) in els.iter().enumerate() {
                if i == 0 && z {
                    pending.extend_from_slice(el);
                } else {
                    if !pending.is_empty() {
                        obus_out.push(std::mem::take(&mut pending));
                    }
                    pending = el.to_vec();
                }
                if i + 1 == els.len() && y {
                    continue;
                }
                obus_out.push(std::mem::take(&mut pending));
            }
        }
        if !pending.is_empty() {
            obus_out.push(pending);
        }
        (flags, obus_out)
    }

    #[test]
    fn leb128_round_trips() {
        for n in [0, 127, 128, 16383, 16384, 1 << 20] {
            assert_eq!(read_leb128(&leb128(n), 0).unwrap().0, n);
            assert_eq!(leb128(n).len(), leb128_len(n));
        }
    }

    #[test]
    fn a_key_unit_is_packed_within_the_spec_and_comes_back_whole() {
        let td = obu(2, &[]);
        let seq = obu(1, &[0x00, 0x00, 0x00, 0x04, 0x3e, 0x7f, 0xff, 0xe8]);
        let mut key_payload = vec![0x10u8];
        key_payload.extend(pattern(256 * 16));
        let key_frame = obu(6, &key_payload);
        let mut delta_payload = vec![0x30u8];
        delta_payload.extend(pattern(512));
        let delta_frame = obu(6, &delta_payload);
        let padding = obu(15, &[0; 10]);
        let tu_key =
            Bytes::from([td.clone(), seq.clone(), key_frame.clone(), padding.clone()].concat());
        let tu_delta = Bytes::from([td.clone(), delta_frame.clone()].concat());

        let parsed = obus(&tu_key).unwrap();
        assert_eq!(
            parsed.iter().map(|o| o.obu_type).collect::<Vec<_>>(),
            vec![2, 1, 6, 15]
        );
        assert!(parsed.iter().all(|o| o.header[0] & 0x02 == 0));
        assert!(is_key(&parsed) && !is_key(&obus(&tu_delta).unwrap()));

        let payloads = packetize(&parsed, true);
        assert!(payloads.iter().all(|p| p.len() <= PACKET_MAX));
        let (flags, rebuilt) = walk(&payloads);
        assert!(flags[0].3 && !flags[1..].iter().any(|f| f.3));
        assert!(!flags[0].0);
        assert!(flags.windows(2).all(|w| w[0].1 == w[1].0));
        assert!(!flags.last().unwrap().1);
        assert_eq!(rebuilt, vec![element(&seq), element(&key_frame)]);

        let payloads = packetize(&obus(&tu_delta).unwrap(), false);
        let (flags, rebuilt) = walk(&payloads);
        assert!(!flags[0].3);
        assert_eq!(rebuilt, vec![element(&delta_frame)]);

        let small = Bytes::from(
            [
                td.clone(),
                obu(5, &[1, 2]),
                obu(6, &[&[0x30u8][..], &[7u8; 40]].concat()),
            ]
            .concat(),
        );
        let payloads = packetize(&obus(&small).unwrap(), false);
        assert_eq!(payloads.len(), 1);
        let p = payloads[0].to_bytes();
        assert_eq!((p[0] >> 4) & 3, 2);
        assert_eq!(p[1], 3);
        assert_eq!(
            &p[p.len() - 41..],
            &[&[0x30u8][..], &[7u8; 40]].concat()[..]
        );

        let payloads: Vec<Bytes> = packetize(&parsed, true)
            .iter()
            .map(|p| p.to_bytes())
            .collect();
        assert_eq!(
            assemble(&payloads),
            Bytes::from([td.clone(), seq.clone(), key_frame.clone()].concat())
        );
        let payloads_delta: Vec<Bytes> = packetize(&obus(&tu_delta).unwrap(), false)
            .iter()
            .map(|p| p.to_bytes())
            .collect();
        assert_eq!(
            assemble(&payloads_delta),
            Bytes::from([td.clone(), delta_frame].concat())
        );
        assert!(payloads.len() > 2);
        assert_eq!(assemble(&payloads[1..]), Bytes::from(td.clone()));
        let seq_sized = obu(1, &[0x00, 0x00, 0x00, 0x04, 0x3e, 0x7f, 0xff, 0xe8]);
        let frame_sized = obu(6, &[&[0x10u8][..], &[0u8; 48]].concat());
        let mut sized = vec![0x20u8];
        sized.extend(leb128(seq_sized.len()));
        sized.extend_from_slice(&seq_sized);
        sized.extend_from_slice(&frame_sized);
        assert_eq!(
            assemble(&[Bytes::from(sized)]),
            Bytes::from([td, seq_sized, frame_sized].concat())
        );
    }

    #[test]
    fn fragments_reference_the_unit() {
        let mut key_payload = vec![0x10u8];
        key_payload.extend(pattern(5000));
        let tu = Bytes::from(obu(6, &key_payload));
        let payloads = packetize(&obus(&tu).unwrap(), true);
        assert!(payloads.len() >= 5);
        let copied: usize = payloads
            .iter()
            .flat_map(|p| p.pieces())
            .filter(|piece| piece.len() < 8)
            .map(|piece| piece.len())
            .sum();
        assert!(
            copied < 8 * payloads.len(),
            "only headers are written: {copied}"
        );
    }

    #[test]
    fn rtc_rtp_depacketizer_reads_our_packets() {
        use rtc_rtp::packetizer::Depacketizer;
        let seq = obu(1, &[0x00, 0x00, 0x00, 0x04, 0x3e, 0x7f, 0xff, 0xe8]);
        let mut key_payload = vec![0x10u8];
        key_payload.extend(pattern(3000));
        let frame = obu(6, &key_payload);
        let tu = Bytes::from([obu(2, &[]), seq.clone(), frame.clone()].concat());
        let payloads = packetize(&obus(&tu).unwrap(), true);
        let mut theirs = rtc_rtp::codec::av1::Av1Depacketizer::default();
        let mut got = Vec::new();
        for p in &payloads {
            got.extend_from_slice(&theirs.depacketize(&p.to_bytes()).unwrap());
        }
        // Pion's depacketizer hands back the OBUs with size fields, which is the unit
        // without its temporal delimiter.
        assert_eq!(got, [seq, frame].concat());
    }
}
