/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Reader for the Ogg Opus stream an audio sink serves: pages are taken as they arrive
//! and split into packets, a packet continued across pages reassembled. The first packet
//! is the `OpusHead` the audio track is declared from; every later packet comes with the
//! page's granule position, the 48 kHz sample count at the end of the last packet the
//! page completes, which is the recorder's audio clock.

use std::collections::VecDeque;
use std::io::{self, Read};

/// What `OpusHead` declares; `mapping` holds the stream count, coupled count, and channel
/// table for channel mapping family 1 and is empty for family 0.
pub struct OpusHead {
    pub channels: u8,
    pub pre_skip: u16,
    pub input_sample_rate: u32,
    pub output_gain: i16,
    pub mapping_family: u8,
    pub mapping: Vec<u8>,
}

pub fn parse_opus_head(packet: &[u8]) -> Option<OpusHead> {
    if packet.len() < 19 || &packet[..8] != b"OpusHead" {
        return None;
    }
    let mapping_family = packet[18];
    let channels = packet[9];
    let mapping = if mapping_family == 0 {
        Vec::new()
    } else {
        packet.get(19..21 + channels as usize)?.to_vec()
    };
    Some(OpusHead {
        channels,
        pre_skip: u16::from_le_bytes([packet[10], packet[11]]),
        input_sample_rate: u32::from_le_bytes(packet[12..16].try_into().ok()?),
        output_gain: i16::from_le_bytes([packet[16], packet[17]]),
        mapping_family,
        mapping,
    })
}

/// Splits an Ogg stream into packets.
pub struct PageReader<R: Read> {
    inner: R,
    /// A packet the previous page did not complete.
    carry: Vec<u8>,
    pending: VecDeque<(Vec<u8>, Option<u64>)>,
}

impl<R: Read> PageReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            carry: Vec::new(),
            pending: VecDeque::new(),
        }
    }

    pub fn inner_mut(&mut self) -> &mut R {
        &mut self.inner
    }

    /// The next packet with the granule of the page it completes, `None` on the page's
    /// earlier packets; `Ok(None)` once the stream has ended.
    pub fn next_packet(&mut self) -> io::Result<Option<(Vec<u8>, Option<u64>)>> {
        while self.pending.is_empty() {
            if !self.read_page()? {
                return Ok(None);
            }
        }
        Ok(self.pending.pop_front())
    }

    fn read_exact_or_end(&mut self, buf: &mut [u8]) -> io::Result<bool> {
        let mut filled = 0;
        while filled < buf.len() {
            match self.inner.read(&mut buf[filled..]) {
                Ok(0) => return Ok(false),
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(true)
    }

    fn read_page(&mut self) -> io::Result<bool> {
        let mut header = [0u8; 27];
        if !self.read_exact_or_end(&mut header)? {
            return Ok(false);
        }
        if &header[..4] != b"OggS" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not an Ogg page",
            ));
        }
        let granule = u64::from_le_bytes(header[6..14].try_into().unwrap());
        let mut lacing = vec![0u8; header[26] as usize];
        if !self.read_exact_or_end(&mut lacing)? {
            return Ok(false);
        }
        let mut body = vec![0u8; lacing.iter().map(|&l| l as usize).sum()];
        if !self.read_exact_or_end(&mut body)? {
            return Ok(false);
        }
        let mut at = 0;
        let mut completed: Vec<Vec<u8>> = Vec::new();
        for &lace in &lacing {
            self.carry.extend_from_slice(&body[at..at + lace as usize]);
            at += lace as usize;
            if lace < 255 {
                completed.push(std::mem::take(&mut self.carry));
            }
        }
        let last = completed.len().saturating_sub(1);
        for (i, packet) in completed.into_iter().enumerate() {
            self.pending
                .push_back((packet, (i == last).then_some(granule)));
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(granule: u64, packets: &[&[u8]], continued_tail: bool) -> Vec<u8> {
        let mut lacing = Vec::new();
        let mut body = Vec::new();
        for (i, p) in packets.iter().enumerate() {
            lacing.extend(std::iter::repeat_n(255u8, p.len() / 255));
            if !(continued_tail && i == packets.len() - 1) {
                lacing.push((p.len() % 255) as u8);
            }
            body.extend_from_slice(p);
        }
        let mut out = b"OggS\0\0".to_vec();
        out.extend_from_slice(&granule.to_le_bytes());
        out.extend_from_slice(&[0u8; 12]);
        out.push(lacing.len() as u8);
        out.extend_from_slice(&lacing);
        out.extend_from_slice(&body);
        out
    }

    /// Packets come out in order, a page's granule on its last packet only, a packet cut
    /// across two pages whole, and a head parses into its fields.
    #[test]
    fn packets_and_granules() {
        let mut head = b"OpusHead".to_vec();
        head.extend_from_slice(&[1, 2, 0x38, 0x01, 0x80, 0xbb, 0, 0, 0, 0, 0]);
        let long = vec![7u8; 510];
        let mut stream = page(0, &[&head], false);
        stream.extend(page(1920, &[b"aa", b"bbb"], false));
        stream.extend(page(2880, &[&long], true));
        stream.extend(page(3840, &[&[8u8; 0][..]], false));
        let mut reader = PageReader::new(&stream[..]);
        let (first, g0) = reader.next_packet().unwrap().unwrap();
        let parsed = parse_opus_head(&first).expect("head");
        assert_eq!(
            (
                parsed.channels,
                parsed.pre_skip,
                parsed.input_sample_rate,
                parsed.mapping_family
            ),
            (2, 312, 48000, 0)
        );
        assert_eq!(g0, Some(0));
        assert_eq!(
            reader.next_packet().unwrap().unwrap(),
            (b"aa".to_vec(), None)
        );
        assert_eq!(
            reader.next_packet().unwrap().unwrap(),
            (b"bbb".to_vec(), Some(1920))
        );
        assert_eq!(reader.next_packet().unwrap().unwrap(), (long, Some(3840)));
        assert!(reader.next_packet().unwrap().is_none());
    }
}
