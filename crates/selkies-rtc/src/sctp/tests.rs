/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Malformed or premature SCTP packets from a peer are dropped, and the six fixes hold.
//!
//! Each packet reaches the association the way the DTLS transport hands it over, on a
//! server-side association the test establishes with its own INIT and COOKIE ECHO. After a
//! hostile packet the association's state is unchanged and it still answers a HEARTBEAT; a
//! server that has no association yet (nothing but an INIT creates one) proves it is alive
//! by establishing one afterwards. Everything is read from the packets the association
//! sends: SACKs for what it received, DATA for what it retransmits.

use std::time::{Duration, Instant};

use bytes::{BufMut, Bytes, BytesMut};
use rtc_sctp::Side;

use super::association::{Sctp, SctpEvent, TransportState};
use super::channel::{ChannelState, DataChannelParameters, Message};

const TSN_HALF: u32 = 1 << 31;

/// An SCTP packet of `chunks` from port 5000 to port 5000 under `tag`.
fn packet(tag: u32, chunks: &[u8]) -> Bytes {
    let mut out = BytesMut::with_capacity(12 + chunks.len());
    out.put_u16(5000);
    out.put_u16(5000);
    out.put_u32(tag);
    out.put_u32(0);
    out.put_slice(chunks);
    let checksum = crc32c::crc32c(&out);
    out[8..12].copy_from_slice(&checksum.to_le_bytes());
    out.freeze()
}

/// One chunk of `kind` with `body`, padded to four bytes.
fn chunk(kind: u8, body: &[u8]) -> Vec<u8> {
    chunk_flagged(kind, 0, body)
}

fn chunk_flagged(kind: u8, flags: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len());
    out.push(kind);
    out.push(flags);
    out.extend_from_slice(&((4 + body.len()) as u16).to_be_bytes());
    out.extend_from_slice(body);
    out.resize(out.len() + (4 - body.len() % 4) % 4, 0);
    out
}

fn param(kind: u16, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&kind.to_be_bytes());
    out.extend_from_slice(&((4 + body.len()) as u16).to_be_bytes());
    out.extend_from_slice(body);
    out.resize(out.len() + (4 - body.len() % 4) % 4, 0);
    out
}

/// A DATA chunk: a whole message of one `x` byte on stream 0, sequence 0, PPID 51.
fn data(tsn: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&tsn.to_be_bytes());
    body.extend_from_slice(&0u16.to_be_bytes());
    body.extend_from_slice(&0u16.to_be_bytes());
    body.extend_from_slice(&51u32.to_be_bytes());
    body.push(b'x');
    chunk_flagged(0, 0x03, &body)
}

/// A whole-message DATA chunk with every field named.
fn whole(tsn: u32, stream: u16, seq: u16, ppid: u32, payload: &[u8], flags: u8) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&tsn.to_be_bytes());
    body.extend_from_slice(&stream.to_be_bytes());
    body.extend_from_slice(&seq.to_be_bytes());
    body.extend_from_slice(&ppid.to_be_bytes());
    body.extend_from_slice(payload);
    chunk_flagged(0, flags, &body)
}

fn forward(cumulative: u32) -> Vec<u8> {
    chunk(192, &cumulative.to_be_bytes())
}

fn init_chunk(params: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&1u32.to_be_bytes());
    body.extend_from_slice(&131_072u32.to_be_bytes());
    body.extend_from_slice(&16u16.to_be_bytes());
    body.extend_from_slice(&16u16.to_be_bytes());
    body.extend_from_slice(&1000u32.to_be_bytes());
    body.extend_from_slice(params);
    chunk(1, &body)
}

/// The INIT the test's peer sends: 16 streams, first TSN 1000, FORWARD TSN and RE-CONFIG supported.
fn init() -> Vec<u8> {
    init_chunk(&param(0x8008, &[192, 130]))
}

/// A parsed chunk: type, flags, body.
struct Chunk {
    kind: u8,
    flags: u8,
    body: Vec<u8>,
}

fn parse_packet(data: &[u8]) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut pos = 12;
    while pos + 4 <= data.len() {
        let kind = data[pos];
        let flags = data[pos + 1];
        let length = usize::from(u16::from_be_bytes([data[pos + 2], data[pos + 3]]));
        assert!(
            length >= 4 && pos + length <= data.len(),
            "chunk length {length}"
        );
        chunks.push(Chunk {
            kind,
            flags,
            body: data[pos + 4..pos + length].to_vec(),
        });
        pos += length + (4 - length % 4) % 4;
    }
    chunks
}

fn parse_params(body: &[u8]) -> Vec<(u16, Vec<u8>)> {
    let mut params = Vec::new();
    let mut pos = 0;
    while pos + 4 <= body.len() {
        let kind = u16::from_be_bytes([body[pos], body[pos + 1]]);
        let length = usize::from(u16::from_be_bytes([body[pos + 2], body[pos + 3]]));
        assert!(length >= 4, "parameter length {length}");
        params.push((kind, body[pos + 4..(pos + length).min(body.len())].to_vec()));
        pos += length + (4 - length % 4) % 4;
    }
    params
}

#[derive(Debug, PartialEq, Eq)]
struct Sack {
    cumulative: u32,
    gaps: Vec<(u16, u16)>,
    duplicates: Vec<u32>,
}

fn parse_sack(body: &[u8]) -> Sack {
    let cumulative = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
    let gap_count = usize::from(u16::from_be_bytes([body[8], body[9]]));
    let dup_count = usize::from(u16::from_be_bytes([body[10], body[11]]));
    let mut pos = 12;
    let mut gaps = Vec::new();
    for _ in 0..gap_count {
        gaps.push((
            u16::from_be_bytes([body[pos], body[pos + 1]]),
            u16::from_be_bytes([body[pos + 2], body[pos + 3]]),
        ));
        pos += 4;
    }
    let mut duplicates = Vec::new();
    for _ in 0..dup_count {
        duplicates.push(u32::from_be_bytes([
            body[pos],
            body[pos + 1],
            body[pos + 2],
            body[pos + 3],
        ]));
        pos += 4;
    }
    Sack {
        cumulative,
        gaps,
        duplicates,
    }
}

/// A DATA chunk the association sent: its TSN and PPID.
fn parse_data(body: &[u8]) -> (u32, u32) {
    (
        u32::from_be_bytes([body[0], body[1], body[2], body[3]]),
        u32::from_be_bytes([body[8], body[9], body[10], body[11]]),
    )
}

/// A server-side association under test and the clock it runs on.
struct Rig {
    sctp: Sctp,
    now: Instant,
    /// The association's verification tag, once an INIT ACK named it.
    tag: u32,
    sent: Vec<Bytes>,
}

impl Rig {
    fn new() -> Rig {
        Rig::bounded(super::MAX_REASSEMBLY_BYTES)
    }

    fn bounded(max_reassembly_bytes: u32) -> Rig {
        let now = Instant::now();
        Rig {
            sctp: Sctp::new_bounded(Side::Server, None, max_reassembly_bytes, now),
            now,
            tag: 1,
            sent: Vec::new(),
        }
    }

    /// Hand the association a packet and collect what it sends back.
    fn deliver(&mut self, raw: Bytes) -> Vec<Chunk> {
        self.sctp.handle_packet(raw, self.now);
        self.drain()
    }

    fn drain(&mut self) -> Vec<Chunk> {
        let mut chunks = Vec::new();
        while let Some(packet) = self.sctp.poll_transmit(self.now) {
            chunks.extend(parse_packet(&packet));
            self.sent.push(packet);
        }
        chunks
    }

    /// Advance the clock past the delayed-SACK timer and collect what that sends.
    fn tick(&mut self, by: Duration) -> Vec<Chunk> {
        self.now += by;
        self.sctp.handle_timeout(self.now);
        self.drain()
    }

    /// The last SACK among `chunks` and those a delayed-ack timer adds.
    fn last_sack(&mut self, mut chunks: Vec<Chunk>) -> Option<Sack> {
        chunks.extend(self.tick(Duration::from_millis(250)));
        chunks
            .iter()
            .rev()
            .find(|c| c.kind == 3)
            .map(|c| parse_sack(&c.body))
    }

    /// Establish the association: the peer's INIT, then its COOKIE ECHO of the INIT ACK.
    fn establish(&mut self) {
        let reply = self.deliver(packet(0, &init()));
        let init_ack = reply
            .iter()
            .find(|c| c.kind == 2)
            .expect("an INIT ACK answers the INIT");
        self.tag = u32::from_be_bytes([
            init_ack.body[0],
            init_ack.body[1],
            init_ack.body[2],
            init_ack.body[3],
        ]);
        let cookie = parse_params(&init_ack.body[16..])
            .into_iter()
            .find(|(kind, _)| *kind == 7)
            .map(|(_, body)| body)
            .expect("the INIT ACK carries a state cookie");
        let reply = self.deliver(packet(self.tag, &chunk(10, &cookie)));
        let kinds: Vec<u8> = reply.iter().map(|c| c.kind).collect();
        assert!(
            reply.iter().any(|c| c.kind == 11),
            "a COOKIE ACK answers the echo, not {kinds:?} (tag {}, cookie {} bytes, {:?})",
            self.tag,
            cookie.len(),
            self.sctp.state()
        );
        assert_eq!(self.sctp.state(), TransportState::Connected);
        while self.sctp.poll_event().is_some() {}
    }

    fn answers_heartbeat(&mut self) -> bool {
        let heartbeat = chunk(4, &param(1, b"live"));
        let reply = self.deliver(packet(self.tag, &heartbeat));
        reply.iter().any(|c| c.kind == 5)
    }

    fn events(&mut self) -> Vec<SctpEvent> {
        let mut events = Vec::new();
        while let Some(event) = self.sctp.poll_event() {
            events.push(event);
        }
        events
    }

    /// `n` one-byte messages sent on a negotiated channel, their TSNs in order.
    fn outstanding(&mut self, n: usize) -> Vec<u32> {
        let mut params = DataChannelParameters::new("input", true);
        params.negotiated = true;
        params.id = Some(0);
        let id = self
            .sctp
            .open_channel(params, self.now)
            .expect("channel opens");
        assert_eq!(self.sctp.channel_state(id), Some(ChannelState::Open));
        let before = self.sent.len();
        for _ in 0..n {
            self.sctp
                .send(id, Message::Text("x".into()), self.now)
                .expect("the send is taken");
        }
        let mut tsns: Vec<u32> = self
            .drain()
            .iter()
            .filter(|c| c.kind == 0)
            .map(|c| parse_data(&c.body).0)
            .collect();
        tsns.sort_unstable();
        assert_eq!(
            tsns.len(),
            n,
            "every message went out ({} packets)",
            self.sent.len() - before
        );
        tsns
    }

    fn sack(&self, cumulative: u32, gaps: &[(u16, u16)]) -> Bytes {
        let mut body = Vec::new();
        body.extend_from_slice(&cumulative.to_be_bytes());
        body.extend_from_slice(&131_072u32.to_be_bytes());
        body.extend_from_slice(&(gaps.len() as u16).to_be_bytes());
        body.extend_from_slice(&0u16.to_be_bytes());
        for (start, end) in gaps {
            body.extend_from_slice(&start.to_be_bytes());
            body.extend_from_slice(&end.to_be_bytes());
        }
        packet(self.tag, &chunk(3, &body))
    }
}

/// A hostile packet on an association not yet established is dropped, and an INIT after it
/// still establishes one.
fn dropped_before_init(raw: &[u8]) {
    let mut rig = Rig::new();
    let reply = rig.deliver(packet(1, raw));
    assert!(reply.is_empty(), "nothing answers a packet before the INIT");
    assert_eq!(rig.sctp.state(), TransportState::Connecting);
    rig.establish();
    assert!(rig.answers_heartbeat());
}

#[test]
fn a_data_chunk_shorter_than_its_header_is_dropped() {
    dropped_before_init(&chunk(0, b"abc"));
}

#[test]
fn an_init_parameter_claiming_zero_length_is_dropped_without_stalling() {
    let mut rig = Rig::new();
    let started = Instant::now();
    let reply = rig.deliver(packet(0, &init_chunk(&[0, 7, 0, 0])));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "no stall on the parameter"
    );
    assert_ne!(rig.sctp.state(), TransportState::Connected);
    if let Some(init_ack) = reply.iter().find(|c| c.kind == 2) {
        // The INIT was taken with the parameter ignored: its cookie still establishes.
        let tag = u32::from_be_bytes([
            init_ack.body[0],
            init_ack.body[1],
            init_ack.body[2],
            init_ack.body[3],
        ]);
        let cookie = parse_params(&init_ack.body[16..])
            .into_iter()
            .find(|(kind, _)| *kind == 7)
            .map(|(_, body)| body)
            .expect("a state cookie");
        rig.tag = tag;
        let reply = rig.deliver(packet(tag, &chunk(10, &cookie)));
        assert!(reply.iter().any(|c| c.kind == 11));
    } else {
        rig.establish();
    }
    assert!(rig.answers_heartbeat());
}

#[test]
fn data_before_init_named_the_peers_first_tsn_is_dropped() {
    dropped_before_init(&data(1000));
}

#[test]
fn forward_tsn_before_init_is_dropped() {
    dropped_before_init(&forward(1000));
}

#[test]
fn data_past_what_a_gap_ack_block_can_name_is_dropped() {
    let mut rig = Rig::new();
    rig.establish();
    let reply = rig.deliver(packet(rig.tag, &data(1000 + 70_000)));
    let sack = rig.last_sack(reply);
    assert!(
        sack.as_ref().is_none_or(|s| s.gaps.is_empty()),
        "SACK gaps {sack:?}"
    );
    assert_eq!(rig.sctp.state(), TransportState::Connected);
    assert!(rig.sctp.stream_ids().is_empty(), "nothing was buffered");
    assert!(rig.answers_heartbeat());
}

#[test]
fn a_reconfig_parameter_too_short_for_its_fields_is_dropped() {
    let mut rig = Rig::new();
    rig.establish();
    let mut short = Vec::new();
    short.extend_from_slice(&1u32.to_be_bytes());
    short.extend_from_slice(&0u32.to_be_bytes());
    short.extend_from_slice(&999u32.to_be_bytes());
    short.push(0);
    let reconfig = chunk(130, &param(13, &short));
    rig.deliver(packet(rig.tag, &reconfig));
    assert_eq!(rig.sctp.state(), TransportState::Connected);
    assert!(rig.answers_heartbeat());
}

#[test]
fn a_channels_label_and_a_string_message_not_utf8_are_taken() {
    let mut rig = Rig::new();
    rig.establish();
    let mut opening = vec![3u8, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0];
    opening.extend_from_slice(&[0xff, 0xcf]);
    let mut raw = whole(1000, 1, 0, 50, &opening, 0x03);
    raw.extend(whole(1001, 1, 1, 51, &[0xcf, 0x00, 0xff], 0x03));
    let reply = rig.deliver(packet(rig.tag, &raw));
    let acks: Vec<&Chunk> = reply
        .iter()
        .filter(|c| c.kind == 0 && parse_data(&c.body).1 == 50)
        .collect();
    assert_eq!(acks.len(), 1, "one DATA_CHANNEL_ACK");
    assert_eq!(acks[0].body[12..], [2]);
    let events = rig.events();
    assert!(events.iter().any(|e| matches!(e, SctpEvent::ChannelOpened(1, params) if params.label == "\u{fffd}\u{fffd}")), "{events:?}");
    assert!(events.iter().any(|e| matches!(e, SctpEvent::Message(1, Message::Text(text)) if text == "\u{fffd}\0\u{fffd}")), "{events:?}");
    assert_eq!(rig.sctp.state(), TransportState::Connected);
    assert!(rig.answers_heartbeat());
}

#[test]
fn a_sack_of_gap_blocks_spanning_millions_of_tsns_takes_no_time() {
    let mut rig = Rig::new();
    rig.establish();
    let tsns = rig.outstanding(8);
    let gaps = vec![(1u16, 65_535u16); 370];
    let sack = rig.sack(tsns[0].wrapping_sub(1), &gaps);
    let started = Instant::now();
    rig.deliver(sack);
    let took = started.elapsed();
    assert!(took < Duration::from_millis(200), "{took:?}");
    assert_eq!(rig.sctp.state(), TransportState::Connected);
    assert!(rig.answers_heartbeat());
}

/// Gap blocks acknowledge the chunks they name: a T3 expiry retransmits every other one.
fn gap_blocks_ack_what_they_name(gaps: &[(u16, u16)]) {
    let mut rig = Rig::new();
    rig.establish();
    let tsns = rig.outstanding(8);
    rig.deliver(rig.sack(tsns[0], gaps));
    assert!(rig.answers_heartbeat());
    let mut retransmitted: Vec<u32> = rig
        .tick(Duration::from_millis(1100))
        .iter()
        .filter(|c| c.kind == 0)
        .map(|c| parse_data(&c.body).0)
        .collect();
    retransmitted.sort_unstable();
    retransmitted.dedup();
    assert_eq!(
        retransmitted,
        vec![tsns[1], tsns[4], tsns[6], tsns[7]],
        "the chunks the gap blocks named are not retransmitted"
    );
}

/// Gap blocks strike the chunks missing before the highest they newly acknowledge: three
/// strikes fast-retransmit exactly those, the ones past the first SACK's highest untouched.
fn gap_blocks_strike_before_the_highest(gaps: &[(u16, u16)]) {
    let mut rig = Rig::new();
    rig.establish();
    let tsns = rig.outstanding(8);
    rig.deliver(rig.sack(tsns[0], gaps));
    let mut second = gaps.to_vec();
    second.push((6, 6));
    rig.deliver(rig.sack(tsns[0], &second));
    let mut third = gaps.to_vec();
    third.push((6, 7));
    let mut fast: Vec<u32> = rig
        .deliver(rig.sack(tsns[0], &third))
        .iter()
        .filter(|c| c.kind == 0)
        .map(|c| parse_data(&c.body).0)
        .collect();
    fast.sort_unstable();
    assert_eq!(
        fast,
        vec![tsns[1], tsns[4]],
        "fast retransmission of the struck chunks"
    );
    assert!(rig.answers_heartbeat());
}

#[test]
fn gap_blocks_ack_the_chunks_they_name_and_strike_those_before_the_highest() {
    for gaps in [
        vec![(2u16, 3u16), (5, 5)],
        vec![(5, 5), (2, 3), (3, 2), (2, 2), (3, 3)],
    ] {
        gap_blocks_ack_what_they_name(&gaps);
        gap_blocks_strike_before_the_highest(&gaps);
    }
}

/// First fragments of messages never finished, one per stream of `streams`.
fn fragments(rig: &mut Rig, streams: std::ops::Range<u16>, size: usize) -> Option<Sack> {
    let payload = vec![b'x'; size];
    let mut last = Vec::new();
    for (tsn, stream) in (1000u32..).zip(streams) {
        if rig.sctp.state() == TransportState::Closed {
            break;
        }
        let raw = whole(tsn, stream, 0, 51, &payload, 0x02);
        last = rig.deliver(packet(rig.tag, &raw));
    }
    rig.last_sack(last)
}

#[test]
fn unfinished_messages_over_many_streams_are_bounded_together() {
    let mut rig = Rig::bounded(64 * 1024);
    rig.establish();
    fragments(&mut rig, 0..16, 5000);
    assert_eq!(
        rig.sctp.state(),
        TransportState::Closed,
        "aborted past the bound"
    );
    assert!(
        rig.sent
            .iter()
            .flat_map(|p| parse_packet(p))
            .any(|c| c.kind == 6),
        "an ABORT went out"
    );
    assert!(rig.events().contains(&SctpEvent::Closed));
}

#[test]
fn and_below_the_bound_they_are_kept() {
    let mut rig = Rig::bounded(64 * 1024);
    rig.establish();
    fragments(&mut rig, 0..10, 1000);
    assert_eq!(rig.sctp.state(), TransportState::Connected);
    assert_eq!(rig.sctp.stream_ids(), (0..10).collect::<Vec<u16>>());
}

#[test]
fn data_on_streams_the_peer_never_opened_is_acknowledged_not_buffered() {
    let mut rig = Rig::bounded(64 * 1024);
    rig.establish();
    let sack = fragments(&mut rig, 100..140, 1000);
    assert_eq!(rig.sctp.state(), TransportState::Connected);
    assert!(
        rig.sctp.stream_ids().is_empty(),
        "{:?}",
        rig.sctp.stream_ids()
    );
    assert_eq!(
        sack.map(|s| s.cumulative),
        Some(1039),
        "every TSN acknowledged"
    );
}

#[test]
fn a_forward_tsn_exactly_half_past_the_cumulative_tsn_is_ignored() {
    let mut rig = Rig::new();
    rig.establish();
    let mut raw = data(1000);
    raw.extend(forward(1000 + TSN_HALF));
    raw.extend(data(1000));
    let reply = rig.deliver(packet(rig.tag, &raw));
    let sack = rig.last_sack(reply).expect("a SACK");
    assert_eq!(sack.cumulative, 1000);
    assert!(sack.duplicates.contains(&1000), "{sack:?}");
    assert!(rig.answers_heartbeat());
}

#[test]
fn a_forward_tsn_half_past_a_held_chunk_prunes_it_and_its_tsn_again_is_a_duplicate() {
    let mut rig = Rig::new();
    rig.establish();
    let mut raw = data(1000);
    raw.extend(data(1001));
    raw.extend(forward(1000 + TSN_HALF));
    raw.extend(data(1000));
    let started = Instant::now();
    let reply = rig.deliver(packet(rig.tag, &raw));
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "the jump costs nothing"
    );
    let sack = rig.last_sack(reply).expect("a SACK");
    assert_eq!(sack.cumulative, 1000 + TSN_HALF);
    assert!(sack.duplicates.contains(&1000), "{sack:?}");
    assert!(rig.answers_heartbeat());
}

#[test]
fn a_tsn_half_from_a_forward_tsns_cumulative_sent_twice_is_not_received_twice() {
    let mut rig = Rig::new();
    rig.establish();
    let unordered = whole(1024, 0, 4, 51, b"x", 0x07);
    let mut raw = forward(1024);
    raw.extend(forward(1024 + TSN_HALF));
    raw.extend(unordered.clone());
    raw.extend(unordered);
    let reply = rig.deliver(packet(rig.tag, &raw));
    let sack = rig.last_sack(reply).expect("a SACK");
    assert_eq!(sack.cumulative, 1024);
    assert_eq!(sack.duplicates, vec![1024, 1024], "{sack:?}");
    assert!(rig.answers_heartbeat());
}

fn wrap() -> Vec<u8> {
    let mut raw = forward(999 + TSN_HALF - 1);
    raw.extend(forward(0xFFFF_FFFD));
    raw
}

#[test]
fn gap_blocks_across_the_tsn_wrap_are_ascending_and_merged() {
    let mut rig = Rig::new();
    rig.establish();
    let mut raw = wrap();
    raw.extend(data(0xFFFF_FFFF));
    raw.extend(data(0));
    raw.extend(data(1));
    let reply = rig.deliver(packet(rig.tag, &raw));
    let sack = rig.last_sack(reply).expect("a SACK");
    assert_eq!(sack.cumulative, 0xFFFF_FFFD);
    assert_eq!(sack.gaps, vec![(2, 4)], "{sack:?}");
    assert!(rig.answers_heartbeat());
}

#[test]
fn a_forward_tsn_before_the_wrap_takes_the_tsns_received_past_it() {
    let mut rig = Rig::new();
    rig.establish();
    let mut raw = wrap();
    raw.extend(data(0xFFFF_FFFF));
    raw.extend(data(0));
    raw.extend(forward(0xFFFF_FFFE));
    let reply = rig.deliver(packet(rig.tag, &raw));
    let sack = rig.last_sack(reply).expect("a SACK");
    assert_eq!(sack.cumulative, 0);
    assert!(sack.gaps.is_empty(), "{sack:?}");
    assert!(rig.answers_heartbeat());
}

#[test]
fn in_window_data_is_still_received_and_acknowledged() {
    let mut rig = Rig::new();
    rig.establish();
    let reply = rig.deliver(packet(rig.tag, &data(1000)));
    let sack = rig.last_sack(reply).expect("a SACK");
    assert_eq!(sack.cumulative, 1000);
    assert!(rig.answers_heartbeat());
}

#[test]
fn a_hostile_init_while_the_association_runs_leaves_it_running() {
    let mut rig = Rig::new();
    rig.establish();
    let reply = rig.deliver(packet(0, &init()));
    assert!(!reply.iter().any(|c| c.kind == 2), "no second INIT ACK");
    assert_eq!(rig.sctp.state(), TransportState::Connected);
    assert!(rig.answers_heartbeat());
}

#[test]
fn a_message_past_the_peers_maximum_is_refused_not_sent() {
    let mut rig = Rig::new();
    rig.establish();
    let mut params = DataChannelParameters::new("input", true);
    params.negotiated = true;
    params.id = Some(0);
    let id = rig.sctp.open_channel(params, rig.now).unwrap();
    let big = Message::Binary(Bytes::from(vec![
        0u8;
        super::DEFAULT_REMOTE_MAX_MESSAGE_SIZE
            as usize
            + 1
    ]));
    assert!(matches!(
        rig.sctp.send(id, big, rig.now),
        Err(super::SendError::TooLarge { .. })
    ));
    assert!(
        rig.drain().iter().all(|c| c.kind != 0),
        "nothing of it went out"
    );
}

#[test]
fn a_channel_opened_before_the_association_sends_its_open_once_established() {
    let mut rig = Rig::new();
    let id = rig
        .sctp
        .open_channel(DataChannelParameters::new("input", true), rig.now)
        .unwrap();
    assert_eq!(id, 0, "a server opens even ids");
    assert_eq!(rig.sctp.channel_state(id), Some(ChannelState::Connecting));
    assert!(rig.drain().is_empty());
    rig.establish();
    let opens: Vec<Chunk> = rig
        .sent
        .iter()
        .flat_map(|p| parse_packet(p))
        .filter(|c| c.kind == 0 && parse_data(&c.body).1 == 50)
        .collect();
    assert_eq!(opens.len(), 1, "one DATA_CHANNEL_OPEN after the handshake");
    assert_eq!(opens[0].body[12], 3);
    let tsn = parse_data(&opens[0].body).0;
    rig.deliver(rig.sack(tsn, &[]));
    let ack = whole(1000, 0, 0, 50, &[2], 0x03);
    rig.deliver(packet(rig.tag, &ack));
    assert_eq!(rig.sctp.channel_state(id), Some(ChannelState::Open));
    assert!(
        rig.events()
            .contains(&SctpEvent::ChannelState(0, ChannelState::Open))
    );
}

#[test]
fn the_last_data_chunk_of_a_burst_carries_the_i_bit() {
    let mut rig = Rig::new();
    rig.establish();
    rig.outstanding(1);
    let flags: Vec<u8> = rig
        .sent
        .iter()
        .flat_map(|p| parse_packet(p))
        .filter(|c| c.kind == 0)
        .map(|c| c.flags)
        .collect();
    assert_eq!(flags.len(), 1);
    assert_eq!(
        flags[0] & 0x08,
        0x08,
        "the lone message asks for an immediate SACK"
    );
}

/// A client and a server association wired back to back, packets moved by the test.
struct Pair {
    client: Sctp,
    server: Sctp,
    now: Instant,
    /// Drop the next `n` packets the server sends carrying DATA.
    drop_server_data: usize,
}

impl Pair {
    fn new() -> Pair {
        let now = Instant::now();
        Pair {
            client: Sctp::new(Side::Client, None, now),
            server: Sctp::new(Side::Server, None, now),
            now,
            drop_server_data: 0,
        }
    }

    /// Move packets until neither side has any.
    fn settle(&mut self) {
        loop {
            let mut moved = false;
            while let Some(packet) = self.client.poll_transmit(self.now) {
                self.server.handle_packet(packet, self.now);
                moved = true;
            }
            while let Some(packet) = self.server.poll_transmit(self.now) {
                let carries_data = parse_packet(&packet).iter().any(|c| c.kind == 0);
                if carries_data && self.drop_server_data > 0 {
                    self.drop_server_data -= 1;
                } else {
                    self.client.handle_packet(packet, self.now);
                }
                moved = true;
            }
            if !moved {
                return;
            }
        }
    }

    /// Advance the clock by `by`, firing timers on both sides, and move what that sends.
    fn advance(&mut self, by: Duration) {
        self.now += by;
        self.client.handle_timeout(self.now);
        self.server.handle_timeout(self.now);
        self.settle();
    }

    fn established(&mut self) {
        self.settle();
        assert_eq!(self.client.state(), TransportState::Connected);
        assert_eq!(self.server.state(), TransportState::Connected);
        while self.client.poll_event().is_some() {}
        while self.server.poll_event().is_some() {}
    }
}

fn drain_events(sctp: &mut Sctp) -> Vec<SctpEvent> {
    let mut events = Vec::new();
    while let Some(event) = sctp.poll_event() {
        events.push(event);
    }
    events
}

#[test]
fn the_engines_two_channels_open_and_carry_messages_both_ways() {
    let mut pair = Pair::new();
    pair.established();
    let input = pair
        .server
        .open_channel(DataChannelParameters::new("input", true), pair.now)
        .unwrap();
    let pointer = pair
        .server
        .open_channel(DataChannelParameters::new("pointer", false), pair.now)
        .unwrap();
    assert_eq!((input, pointer), (0, 2), "a server's ids are even");
    pair.settle();
    let opened = drain_events(&mut pair.client);
    let labels: Vec<(u16, String, bool)> = opened
        .iter()
        .filter_map(|e| match e {
            SctpEvent::ChannelOpened(id, params) => {
                Some((*id, params.label.clone(), params.ordered))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        labels,
        vec![
            (0, "input".to_owned(), true),
            (2, "pointer".to_owned(), false)
        ]
    );
    assert_eq!(pair.server.channel_state(input), Some(ChannelState::Open));
    assert_eq!(pair.server.channel_state(pointer), Some(ChannelState::Open));

    pair.server
        .send(input, Message::Text("_gz,1".into()), pair.now)
        .unwrap();
    pair.client
        .send(
            pointer,
            Message::Binary(Bytes::from_static(b"m,1,2")),
            pair.now,
        )
        .unwrap();
    pair.client
        .send(input, Message::Text(String::new()), pair.now)
        .unwrap();
    pair.settle();
    assert!(
        drain_events(&mut pair.client)
            .contains(&SctpEvent::Message(input, Message::Text("_gz,1".into())))
    );
    let at_server = drain_events(&mut pair.server);
    assert!(at_server.contains(&SctpEvent::Message(
        pointer,
        Message::Binary(Bytes::from_static(b"m,1,2"))
    )));
    assert!(at_server.contains(&SctpEvent::Message(input, Message::Text(String::new()))));
}

#[test]
fn a_large_message_fragments_and_reassembles_and_buffered_amount_drains() {
    let mut pair = Pair::new();
    pair.established();
    let id = pair
        .server
        .open_channel(DataChannelParameters::new("input", true), pair.now)
        .unwrap();
    pair.settle();
    drain_events(&mut pair.server);
    drain_events(&mut pair.client);
    pair.server.set_buffered_amount_low_threshold(id, 16 * 1024);
    let body = Bytes::from(vec![7u8; 200 * 1024]);
    pair.server
        .send(id, Message::Binary(body.clone()), pair.now)
        .unwrap();
    assert!(pair.server.buffered_amount(id) >= 16 * 1024);
    for _ in 0..200 {
        pair.advance(Duration::from_millis(5));
    }
    assert_eq!(pair.server.buffered_amount(id), 0);
    assert!(drain_events(&mut pair.server).contains(&SctpEvent::BufferedAmountLow(id)));
    assert!(
        drain_events(&mut pair.client).contains(&SctpEvent::Message(id, Message::Binary(body)))
    );
}

#[test]
fn a_lost_lone_message_is_repaired_after_the_floor_not_a_second() {
    let mut pair = Pair::new();
    pair.established();
    let id = pair
        .server
        .open_channel(DataChannelParameters::new("input", true), pair.now)
        .unwrap();
    pair.settle();
    // A measured round trip brings the timer down from RTO.Initial to its floor.
    for _ in 0..3 {
        pair.server
            .send(id, Message::Text("warm".into()), pair.now)
            .unwrap();
        pair.advance(Duration::from_millis(30));
    }
    drain_events(&mut pair.client);
    pair.drop_server_data = 1;
    pair.server
        .send(id, Message::Text("cursor".into()), pair.now)
        .unwrap();
    pair.settle();
    let cursor = SctpEvent::Message(id, Message::Text("cursor".into()));
    assert!(
        !drain_events(&mut pair.client).contains(&cursor),
        "the first copy was lost"
    );
    pair.advance(Duration::from_millis(350));
    assert!(
        !drain_events(&mut pair.client).contains(&cursor),
        "not before the floor"
    );
    pair.advance(Duration::from_millis(60));
    assert!(
        drain_events(&mut pair.client).contains(&cursor),
        "repaired at 0.4 s"
    );
}

#[test]
fn a_retransmitted_init_is_answered_by_the_association_it_started() {
    let mut rig = Rig::new();
    let first = rig.deliver(packet(0, &init()));
    let second = rig.deliver(packet(0, &init()));
    let tag_of = |chunks: &[Chunk]| {
        chunks
            .iter()
            .find(|c| c.kind == 2)
            .map(|c| c.body[..4].to_vec())
    };
    assert!(tag_of(&first).is_some());
    assert_eq!(
        tag_of(&first),
        tag_of(&second),
        "the same association answers both"
    );
}

#[test]
fn a_sack_past_anything_sent_leaves_the_association_working() {
    let mut rig = Rig::new();
    rig.establish();
    let tsns = rig.outstanding(4);
    rig.deliver(rig.sack(tsns[0].wrapping_add(1000), &[]));
    assert_eq!(rig.sctp.state(), TransportState::Connected);
    rig.deliver(rig.sack(tsns[3], &[]));
    assert_eq!(
        rig.sctp.buffered_amount(0),
        0,
        "the real SACK still acknowledges"
    );
    assert!(rig.answers_heartbeat());
}

/// A small deterministic generator, so a failure replays.
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// The `sctp` fuzz target's shape: chunk bytes mutated from valid seeds, the checksum and the
/// tag made to match so each reaches the handlers, on associations established and not.
#[test]
fn mutated_packets_never_panic_the_association() {
    let mut rng = XorShift(0x5e1c_1e50_dead_beef);
    let seeds: Vec<Vec<u8>> = vec![
        init(),
        data(1000),
        whole(
            1001,
            1,
            0,
            50,
            &[3, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, b'x'],
            0x03,
        ),
        forward(5000),
        chunk(
            3,
            &[
                0, 0, 3, 232, 0, 2, 0, 0, 0, 1, 0, 0, 0, 2, 255, 255, 0, 3, 0, 4,
            ],
        ),
        chunk(4, &param(1, b"live")),
        chunk(
            130,
            &param(13, &[0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 3, 232, 0, 1]),
        ),
        chunk(130, &param(16, &[0, 0, 0, 1, 0, 0, 0, 1])),
        chunk(6, &[]),
        chunk(7, &[0, 0, 3, 232]),
        chunk(8, &[]),
        chunk(9, &param(1, &[0, 1])),
        chunk(11, &[]),
        chunk(14, &[]),
    ];
    let rounds = std::env::var("SELKIES_SCTP_MUTATION_ROUNDS")
        .ok()
        .and_then(|rounds| rounds.parse().ok())
        .unwrap_or(600);
    for round in 0..rounds {
        let mut rig = Rig::new();
        if round % 4 != 0 {
            rig.establish();
            if round % 8 == 1 {
                rig.outstanding(4);
            }
        }
        for _ in 0..24 {
            let mut body = seeds[rng.below(seeds.len())].clone();
            for _ in 0..rng.below(6) {
                match rng.below(4) {
                    0 if !body.is_empty() => {
                        let at = rng.below(body.len());
                        body[at] = rng.next() as u8;
                    }
                    1 if !body.is_empty() => body.truncate(rng.below(body.len()) + 1),
                    2 => body.extend_from_slice(&seeds[rng.below(seeds.len())]),
                    _ if body.len() > 2 => {
                        let at = rng.below(body.len() - 1);
                        body[at] = 0xff;
                        body[at + 1] = 0xff;
                    }
                    _ => {}
                }
            }
            let tag = if body.first() == Some(&1) { 0 } else { rig.tag };
            let _ = rig.deliver(packet(tag, &body));
            if rng.below(8) == 0 {
                let _ = rig.tick(Duration::from_millis(rng.below(1500) as u64));
            }
            let _ = rig.events();
        }
    }
}

#[test]
fn closing_a_channel_resets_its_stream_on_both_sides() {
    let mut pair = Pair::new();
    pair.established();
    let id = pair
        .server
        .open_channel(DataChannelParameters::new("input", true), pair.now)
        .unwrap();
    pair.settle();
    drain_events(&mut pair.server);
    drain_events(&mut pair.client);
    pair.server.close_channel(id, pair.now);
    assert_eq!(pair.server.channel_state(id), Some(ChannelState::Closing));
    assert!(matches!(
        pair.server.send(id, Message::Text("late".into()), pair.now),
        Err(super::SendError::NotOpen { .. })
    ));
    for _ in 0..4 {
        pair.advance(Duration::from_millis(50));
    }
    assert!(
        drain_events(&mut pair.client).contains(&SctpEvent::ChannelState(id, ChannelState::Closed))
    );
    assert!(
        drain_events(&mut pair.server).contains(&SctpEvent::ChannelState(id, ChannelState::Closed))
    );
    assert_eq!(pair.server.channel_state(id), None);
    assert_eq!(
        pair.client.state(),
        TransportState::Connected,
        "the association stays"
    );
}

#[test]
fn a_silent_path_outlasts_the_retries_of_a_stock_association() {
    let mut pair = Pair::new();
    pair.established();
    let id = pair
        .server
        .open_channel(DataChannelParameters::new("input", true), pair.now)
        .unwrap();
    pair.settle();
    drain_events(&mut pair.client);
    pair.server
        .send(id, Message::Text("after".into()), pair.now)
        .unwrap();
    // Thirty seconds of nothing reaching the client, as long as ICE consent holds a path.
    for _ in 0..300 {
        pair.now += Duration::from_millis(100);
        pair.server.handle_timeout(pair.now);
        while pair.server.poll_transmit(pair.now).is_some() {}
    }
    assert_eq!(
        pair.server.state(),
        TransportState::Connected,
        "DATA is not given up on alone"
    );
    for _ in 0..700 {
        pair.advance(Duration::from_millis(100));
    }
    let after = SctpEvent::Message(id, Message::Text("after".into()));
    assert!(
        drain_events(&mut pair.client).contains(&after),
        "repaired when the path returns"
    );
}

/// Input shares the association, so a lost repair is tried again within about a second however
/// long the outage ran, rather than at the minute RFC 9260's doubling reaches.
#[test]
fn a_t3_backoff_holds_a_repair_near_a_flat_retransmission() {
    let mut rig = Rig::new();
    rig.establish();
    let tsns = rig.outstanding(1);
    let mut repairs = 0;
    let mut waited = Duration::ZERO;
    let mut longest = Duration::ZERO;
    let mut since = Duration::ZERO;
    let step = Duration::from_millis(50);
    while waited < Duration::from_secs(60) {
        waited += step;
        since += step;
        if rig
            .tick(step)
            .iter()
            .any(|c| c.kind == 0 && parse_data(&c.body).0 == tsns[0])
        {
            longest = longest.max(since);
            since = Duration::ZERO;
            repairs += 1;
        }
    }
    assert!(
        repairs >= 50,
        "a minute's outage retries about once a second: {repairs}"
    );
    assert!(
        longest <= Duration::from_millis(1100),
        "the longest wait: {longest:?}"
    );
}
