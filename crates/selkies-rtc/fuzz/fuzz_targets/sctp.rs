/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The SCTP packets a peer sends inside DTLS, into a server-side association the way the data
//! channels' transport hands them over (`Sctp::handle_packet`): the vendored `rtc-sctp`'s packet
//! and chunk parsers, then every chunk handler, DCEP, and the channels' bookkeeping.
//!
//! The first byte's low bit has the harness establish the association first with its own INIT
//! and COOKIE ECHO, and its next bit then opens a channel with four messages in flight, so a
//! SACK, a FORWARD TSN, or a RE-CONFIG meets state it acts on. The rest is a run of packets
//! (`datagrams`), each opening with a byte of tens of milliseconds the clock advances first, the
//! association's timers firing then. A packet's verification tag is replaced by the one the
//! association last named in an INIT ACK unless it is zero, as an INIT's is, and its checksum
//! is made to match, so inputs reach the handlers rather than stopping at those checks.
//!
//! Nothing may panic, a packet that unmarshals marshals to one that unmarshals again
//! (`rtc_sctp::fuzzing`), and every packet the association sends back is whole: a common
//! header, chunks that end inside it, and a CRC32c that checks. Its ports are the INIT's
//! reversed, whatever the INIT named, so they are not checked.
#![no_main]

mod datagrams;

use std::time::{Duration, Instant};

use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use selkies_rtc::sctp::{DataChannelParameters, Message, Sctp, Side};

const COMMON_HEADER: usize = 12;
const INIT: u8 = 1;
const INIT_ACK: u8 = 2;
const COOKIE_ECHO: u8 = 10;
const STATE_COOKIE: u16 = 7;

/// A packet from and to port 5000 under `tag`, its checksum set.
fn packet(tag: u32, chunks: &[u8]) -> Bytes {
    let mut out = Vec::with_capacity(COMMON_HEADER + chunks.len());
    out.extend_from_slice(&5000u16.to_be_bytes());
    out.extend_from_slice(&5000u16.to_be_bytes());
    out.extend_from_slice(&tag.to_be_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(chunks);
    seal(&mut out);
    out.into()
}

/// Set a packet's CRC32c (RFC 9260 appendix A), taken over the packet with the field zeroed.
fn seal(packet: &mut [u8]) {
    packet[8..12].fill(0);
    let checksum = crc32c::crc32c(packet);
    packet[8..12].copy_from_slice(&checksum.to_le_bytes());
}

/// One chunk of `kind` with `body`, padded to four bytes.
fn chunk(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![kind, 0];
    out.extend_from_slice(&((4 + body.len()) as u16).to_be_bytes());
    out.extend_from_slice(body);
    out.resize(out.len().next_multiple_of(4), 0);
    out
}

/// The chunks of a packet the association sent, as `(type, body)`, checking it is whole.
fn chunks(packet: &[u8]) -> Vec<(u8, &[u8])> {
    assert!(
        packet.len() >= COMMON_HEADER,
        "a packet shorter than its header"
    );
    let mut zeroed = packet.to_vec();
    seal(&mut zeroed);
    assert_eq!(zeroed[8..12], packet[8..12], "the checksum");
    let mut out = Vec::new();
    let mut pos = COMMON_HEADER;
    while pos < packet.len() {
        assert!(pos + 4 <= packet.len(), "a chunk header past the packet");
        let length = u16::from_be_bytes([packet[pos + 2], packet[pos + 3]]) as usize;
        assert!(
            length >= 4 && pos + length <= packet.len(),
            "a chunk of {length} bytes at {pos} in {}",
            packet.len()
        );
        out.push((packet[pos], &packet[pos + 4..pos + length]));
        pos += length.next_multiple_of(4);
    }
    out
}

/// A server-side association, its clock, and the tag of the last INIT ACK it sent.
struct Harness {
    sctp: Sctp,
    now: Instant,
    tag: u32,
}

impl Harness {
    /// Hand over a packet and check everything the association sends back.
    fn deliver(&mut self, packet: Bytes) -> Vec<Vec<u8>> {
        self.sctp.handle_packet(packet, self.now);
        self.drain()
    }

    fn drain(&mut self) -> Vec<Vec<u8>> {
        let mut sent = Vec::new();
        while let Some(packet) = self.sctp.poll_transmit(self.now) {
            for (kind, body) in chunks(&packet) {
                if kind == INIT_ACK && body.len() >= 4 {
                    self.tag = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
                }
            }
            sent.push(packet.to_vec());
        }
        while self.sctp.poll_event().is_some() {}
        sent
    }

    /// The peer's INIT (16 streams each way, first TSN 1000, FORWARD TSN and RE-CONFIG
    /// supported), then its COOKIE ECHO of the INIT ACK.
    fn establish(&mut self) {
        let mut init = Vec::new();
        init.extend_from_slice(&1u32.to_be_bytes());
        init.extend_from_slice(&131_072u32.to_be_bytes());
        init.extend_from_slice(&16u16.to_be_bytes());
        init.extend_from_slice(&16u16.to_be_bytes());
        init.extend_from_slice(&1000u32.to_be_bytes());
        init.extend_from_slice(&[0x80, 0x08, 0, 6, 192, 130, 0, 0]);
        let sent = self.deliver(packet(0, &chunk(INIT, &init)));
        let ack = sent
            .iter()
            .flat_map(|p| chunks(p).into_iter().map(|(k, b)| (k, b.to_vec())))
            .find(|(kind, _)| *kind == INIT_ACK)
            .map(|(_, body)| body)
            .expect("an INIT ACK answers the INIT");
        let mut pos = 16;
        let mut cookie = None;
        while pos + 4 <= ack.len() {
            let kind = u16::from_be_bytes([ack[pos], ack[pos + 1]]);
            let length = u16::from_be_bytes([ack[pos + 2], ack[pos + 3]]) as usize;
            if kind == STATE_COOKIE {
                cookie = Some(ack[pos + 4..pos + length].to_vec());
            }
            pos += length.max(4).next_multiple_of(4);
        }
        let cookie = cookie.expect("the INIT ACK carries a state cookie");
        self.deliver(packet(self.tag, &chunk(COOKIE_ECHO, &cookie)));
    }

    /// A negotiated channel on stream 0 with four messages sent and not yet acknowledged.
    fn in_flight(&mut self) {
        let mut parameters = DataChannelParameters::new("input", true);
        parameters.negotiated = true;
        parameters.id = Some(0);
        let id = self
            .sctp
            .open_channel(parameters, self.now)
            .expect("a negotiated channel opens");
        for _ in 0..4 {
            self.sctp
                .send(id, Message::Text("x".into()), self.now)
                .expect("the send is taken");
        }
        self.drain();
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    let now = Instant::now();
    let mut harness = Harness {
        sctp: Sctp::new(Side::Server, None, now),
        now,
        tag: 0,
    };
    if mode & 1 != 0 {
        harness.establish();
        if mode & 2 != 0 {
            harness.in_flight();
        }
    }
    for datagram in datagrams::split(rest) {
        let Some((&advance, raw)) = datagram.split_first() else {
            continue;
        };
        harness.now += Duration::from_millis(10 * advance as u64);
        harness.sctp.handle_timeout(harness.now);
        harness.drain();
        let mut raw = raw.to_vec();
        if raw.len() >= COMMON_HEADER {
            if raw[4..8] != [0; 4] {
                raw[4..8].copy_from_slice(&harness.tag.to_be_bytes());
            }
            seal(&mut raw);
        }
        if let Ok(wire) = rtc_sctp::fuzzing::packet_marshal(&raw) {
            assert!(
                rtc_sctp::fuzzing::packet_unmarshal(&wire).is_ok(),
                "a marshaled packet that does not unmarshal: {wire:02x?}"
            );
        }
        harness.deliver(raw.into());
    }
});
