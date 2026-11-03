/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The sender's contracts, each driven through the real `RtpSender`: a transport recording
//! what it is handed stands in for the socket, and the clock is the test's.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use super::super::codecs;
use super::super::fec::RepairHeader;
use super::super::fec::tests::recover;
use super::super::packet::HeaderExtensions;
use super::super::packet::{
    ABS_CAPTURE_TIME_URI, ABS_SEND_TIME_URI, DEPENDENCY_DESCRIPTOR_URI, MID_URI, PLAYOUT_DELAY_URI,
    TRANSMISSION_OFFSET_URI, TRANSPORT_CC_URI, VIDEO_TIMING_URI,
};
use super::super::params::HeaderExtensionParameters;
use super::super::rtcp::{PsfbPacket, RtpfbPacket};
use super::*;

/// A transport recording what it was handed; its refusals and backlog are the test's.
struct FakeTransport {
    twcc: TwccSender,
    pacer: Option<Pacer>,
    /// Every packet passed on to the wire.
    rtp: Vec<Vec<u8>>,
    /// The transport-wide number of every send attempted, refused or not.
    attempts: Vec<Option<u16>>,
    rtcp: Vec<Vec<u8>>,
    refusals: VecDeque<bool>,
    heard_at: f64,
    backlog: (usize, f64, f64),
    real_backlog: bool,
    room: Option<usize>,
    /// How many padding packets the pacer admits, and those it did.
    pad_allow: usize,
    padded: Vec<Vec<u8>>,
}

impl FakeTransport {
    fn new(now: f64) -> Self {
        Self {
            twcc: TwccSender::new(now),
            pacer: None,
            rtp: Vec::new(),
            attempts: Vec::new(),
            rtcp: Vec::new(),
            refusals: VecDeque::new(),
            heard_at: now,
            backlog: (0, 0.0, f64::INFINITY),
            real_backlog: false,
            room: None,
            pad_allow: 0,
            padded: Vec::new(),
        }
    }

    fn media(&self, pt: u8) -> Vec<Vec<u8>> {
        self.rtp
            .iter()
            .filter(|d| d[1] & 0x7F == pt)
            .cloned()
            .collect()
    }

    fn seqs(&self) -> Vec<u16> {
        self.rtp
            .iter()
            .map(|d| u16::from_be_bytes([d[2], d[3]]))
            .collect()
    }
}

impl SenderTransport for FakeTransport {
    fn send_rtp(
        &mut self,
        packet: Datagram<'_>,
        class: Class,
        twcc_seq: Option<u16>,
        now: f64,
    ) -> Verdict {
        self.attempts.push(twcc_seq);
        if self.refusals.pop_front() == Some(true) {
            return Verdict::Dropped;
        }
        let data = packet.to_vec();
        let verdict = match self.pacer.as_mut() {
            Some(pacer) => pacer.send(Bytes::from(data.clone()), class, twcc_seq, now),
            None => Verdict::SendNow,
        };
        if matches!(verdict, Verdict::SendNow | Verdict::Queued) {
            self.rtp.push(data);
        }
        verdict
    }

    fn send_rtcp(&mut self, compound: &[u8], _now: f64) {
        self.rtcp.push(compound.to_vec());
    }

    fn twcc(&mut self) -> &mut TwccSender {
        &mut self.twcc
    }

    fn pacer(&mut self) -> Option<&mut Pacer> {
        self.pacer.as_mut()
    }

    fn peer_heard_at(&self) -> f64 {
        self.heard_at
    }

    fn video_backlog(&mut self, now: f64) -> (usize, f64, f64) {
        if self.real_backlog {
            let (frames, wait) = self
                .pacer
                .as_ref()
                .map_or((0, 0.0), |p| (p.frames_pending(), p.drain_s()));
            return (
                frames,
                wait + self.twcc.path_queue_s(),
                now - self.twcc.lost_at(),
            );
        }
        self.backlog
    }

    fn video_room(&mut self) -> Option<usize> {
        self.room
    }

    fn padding_room(&mut self, _size: usize, _now: f64) -> bool {
        let admitted = self.pad_allow > 0;
        self.pad_allow = self.pad_allow.saturating_sub(1);
        admitted
    }

    fn send_padding(&mut self, packet: Datagram<'_>, _now: f64) {
        self.padded.push(packet.to_vec());
    }
}

#[derive(Default)]
struct Shared {
    events: Vec<String>,
    log_sent: bool,
    selective: Option<bool>,
    taken: bool,
    resync_wired: bool,
}

/// Hooks recording every call as a line the checks read.
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Shared>>);

impl Recorder {
    fn log(&self, line: String) {
        self.0.lock().unwrap().events.push(line);
    }

    fn events(&self) -> Vec<String> {
        self.0.lock().unwrap().events.clone()
    }

    fn clear(&self) {
        self.0.lock().unwrap().events.clear();
    }

    fn set_taken(&self, taken: bool) {
        self.0.lock().unwrap().taken = taken;
    }
}

impl SenderHooks for Recorder {
    fn on_frame_sent(&mut self, captured_ns: i64, size: usize) {
        if self.0.lock().unwrap().log_sent {
            self.log(format!("sent {captured_ns} {size}"));
        }
    }

    fn selective(&self) -> Option<bool> {
        self.0.lock().unwrap().selective
    }

    fn on_resync(&mut self, frame_id: u16, reach: bool) -> Option<bool> {
        let (wired, taken) = {
            let shared = self.0.lock().unwrap();
            (shared.resync_wired, shared.taken)
        };
        if !wired {
            return None;
        }
        if !reach && !taken {
            return Some(false);
        }
        self.log(format!("resync {frame_id} {reach}"));
        Some(true)
    }

    fn on_frame_out(&mut self, frame_id: u16, keyframe: bool) {
        self.log(format!("out {frame_id} {keyframe}"));
    }

    fn on_frame_held(&mut self, frame_id: u16, keyframe: bool) {
        self.log(format!("held {frame_id} {keyframe}"));
    }

    fn lost_frame(&mut self, frame_id: u16) {
        self.log(format!("lost_frame {frame_id}"));
    }

    fn pli(&mut self) {
        self.log("pli".to_string());
    }
}

fn extensions(dd: bool) -> Vec<HeaderExtensionParameters> {
    let mut uris = vec![
        MID_URI,
        ABS_SEND_TIME_URI,
        TRANSPORT_CC_URI,
        PLAYOUT_DELAY_URI,
        VIDEO_TIMING_URI,
    ];
    if dd {
        uris.push(DEPENDENCY_DESCRIPTOR_URI);
    }
    uris.iter()
        .enumerate()
        .map(|(i, uri)| HeaderExtensionParameters::new(i as u8 + 1, uri))
        .collect()
}

fn parameters(mime: &str, dd: bool) -> SendParameters {
    SendParameters {
        codecs: vec![CodecParameters::new(mime, 90000, 96)],
        header_extensions: extensions(dd),
        mux_id: Some("0".into()),
        cname: Some("cname".into()),
    }
}

struct Harness {
    s: RtpSender,
    tr: FakeTransport,
    rec: Recorder,
    clock: f64,
}

impl Harness {
    fn new(dd: bool, selective: Option<bool>, taken: bool, mime: &str) -> Self {
        let clock = 1000.0;
        let rec = Recorder::default();
        {
            let mut shared = rec.0.lock().unwrap();
            shared.selective = selective;
            shared.taken = taken;
            shared.resync_wired = true;
        }
        let mut s = RtpSender::new(Kind::Video, Box::new(rec.clone()));
        s.start(&parameters(mime, dd)).unwrap();
        for seq in 1..5 {
            s.history.add(payload_packet(seq), 0.0, Some(seq));
        }
        Self {
            s,
            tr: FakeTransport::new(clock),
            rec,
            clock,
        }
    }

    fn selective() -> Self {
        Self::new(true, Some(true), true, "video/H264")
    }

    /// What the RTP path does with a frame: leave it out, describe it, and hand it over.
    fn send(
        &mut self,
        frame_id: u16,
        reference: Option<u16>,
        tag: u16,
        key: bool,
        anchor: bool,
    ) -> Option<(u16, Option<u16>)> {
        if !self
            .s
            .forward(frame_id, reference, key, anchor, &mut self.tr, self.clock)
        {
            self.s.frame_left = None;
            return None;
        }
        let described = self.s.describe(frame_id, reference, key);
        if described.is_none() {
            self.s.frame_left = None;
            self.s.undecodable(frame_id, reference);
            return None;
        }
        self.s.in_flight.push_back((tag, frame_id));
        described
    }

    /// `(frames, wait)` of the peer's backlog, as far behind as the checks need it.
    fn behind(&mut self, frames: usize, wait: f64) {
        self.tr.backlog.0 = frames;
        self.tr.backlog.1 = wait;
    }

    fn on_wire(&mut self, tag: u16, frame_id: Option<u16>) {
        self.s.note_frame_end(
            tag,
            FrameEnd {
                keyframe: false,
                timing: None,
                captured_ns: 0,
                size: 0,
                frame_id,
            },
        );
        self.s.frame_left(tag, self.clock);
    }

    fn resync(&mut self) -> bool {
        self.s.resync_held(&mut self.tr, self.clock)
    }

    fn numbered(&self) -> HashSet<u16> {
        self.s.frame_numbers.keys().collect()
    }

    fn hold(&mut self, frames: &[u16]) {
        for frame in frames {
            self.s.held.insert(*frame, ());
        }
    }

    fn unheld(
        &mut self,
        frame_id: u16,
        reference: Option<u16>,
        keyframe: bool,
        packets: &[(u16, u16)],
    ) {
        self.s.unheld.push_back(Unheld {
            frame_id,
            reference,
            keyframe,
            sent_at: self.clock,
            packets: packets.to_vec(),
        });
    }

    fn nack(&mut self, lost: &[u16]) {
        let packet = RtcpPacket::Rtpfb(RtpfbPacket::nack(1, 2, lost.to_vec()));
        self.s.handle_rtcp(&packet, &mut self.tr, self.clock);
    }

    fn fid(&self, frame: u16) -> String {
        format!("resync {frame} true")
    }
}

fn payload_packet(seq: u16) -> RtpPacket {
    let mut p = RtpPacket::new(96, seq, 0, 0);
    p.payload = Payload::from(&b"x"[..]);
    p
}

// ---- test_video_timing_legs

#[test]
fn video_timing_legs_from_the_capture() {
    const MS: i64 = 1_000_000;
    let capture = 5_000 * MS;
    let stamped = |start: i64, end: i64| FrameTiming {
        capture_ns: capture,
        encode_start_ns: capture + start * MS,
        encode_end_ns: capture + end * MS,
    };
    assert_eq!(
        video_timing_legs(Some(stamped(2, 9)), capture + 12 * MS, 99),
        (1, 2, 9, 12, 12, 0, 0)
    );
    assert_eq!(video_timing_legs(None, capture, 7), (1, 0, 0, 7, 7, 0, 0));
    assert_eq!(
        video_timing_legs(Some(FrameTiming::default()), capture, 7),
        (1, 0, 0, 7, 7, 0, 0)
    );
    assert_eq!(
        video_timing_legs(Some(stamped(70_000, 80_000)), capture + 90_000 * MS, 0),
        (1, 0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF, 0, 0)
    );
    let legs = video_timing_legs(Some(stamped(-1, 0)), capture, 0);
    assert_eq!(
        (legs.1, legs.2),
        (0, 0),
        "a stamp before the capture reads as zero"
    );
}

// ---- test_rtp_color_space

#[test]
fn color_space_per_codec_on_every_packet_of_a_key_frame() {
    assert_eq!(rtp_color_space(Codec::Vp8), Some((1, 1, 6, 1)));
    assert_eq!(rtp_color_space(Codec::Vp9), Some((1, 1, 1, 1)));
    for codec in [Codec::H264, Codec::H265, Codec::Av1] {
        assert_eq!(rtp_color_space(codec), None);
    }
    let mut params = parameters("video/VP8", false);
    params
        .header_extensions
        .push(HeaderExtensionParameters::new(
            7,
            super::super::packet::COLOR_SPACE_URI,
        ));
    let mut s = RtpSender::new(Kind::Video, Box::new(NoHooks));
    s.start(&params).unwrap();
    let mut tr = FakeTransport::new(1000.0);
    let data = Bytes::from(vec![0x10u8; 3000]);
    s.push_frame(
        EncodedFrame {
            data: data.clone(),
            keyframe: true,
            ..Default::default()
        },
        &mut tr,
        1000.0,
    );
    s.push_frame(
        EncodedFrame {
            data: Bytes::from(vec![0x11u8; 3000]),
            timestamp: 3000,
            ..Default::default()
        },
        &mut tr,
        1000.1,
    );
    let map = s.header_extensions().clone();
    let parsed: Vec<RtpPacket> = tr
        .rtp
        .iter()
        .map(|d| RtpPacket::parse(&Bytes::from(d.clone()), &map).unwrap())
        .collect();
    assert_eq!(parsed.len(), 6);
    assert!(
        parsed[..3]
            .iter()
            .all(|p| p.extensions.color_space == Some((1, 1, 6, 1)))
    );
    assert!(
        parsed[3..]
            .iter()
            .all(|p| p.extensions.color_space.is_none())
    );
}

// ---- test_rtp_capture_clock

fn h264_frame() -> Bytes {
    let mut data = vec![0, 0, 0, 1, 0x65];
    data.extend_from_slice(&[0x88; 40]);
    Bytes::from(data)
}

fn ntp_seconds(ntp: u64) -> f64 {
    ntp as f64 / (1u64 << 32) as f64
}

#[test]
fn sender_reports_map_a_frame_to_its_capture_and_abs_capture_time_rides_as_paced() {
    const MS: i64 = 1_000_000;
    let mut params = parameters("video/H264", false);
    params
        .header_extensions
        .push(HeaderExtensionParameters::new(9, ABS_CAPTURE_TIME_URI));
    let mut s = RtpSender::new(Kind::Video, Box::new(NoHooks));
    s.start(&params).unwrap();
    let start_ns = 2_000_000 * MS;
    let mut tr = FakeTransport::new(start_ns as f64 / 1e9);
    tr.heard_at = 1e12;
    let mut captures = std::collections::HashMap::new();
    let mut reports = Vec::new();
    let mut next_report = start_ns as f64 / 1e9 + 0.5;
    // 60 fps for 2.2 s; each frame reaches the sender 25 ms after its capture, as an encode
    // and a hop onto the loop would deliver it.
    for i in 0..132i64 {
        let capture = start_ns + i * 16_666_667;
        let at = (capture + 25 * MS) as f64 / 1e9;
        while next_report < at {
            reports.push(s.rtcp_report(next_report));
            next_report += 0.7;
        }
        let pts = ((capture - start_ns) * 9 / 100_000) as u32;
        captures.insert(pts, capture);
        s.push_frame(
            EncodedFrame {
                data: h264_frame(),
                timestamp: pts,
                keyframe: i == 0,
                timing: Some(FrameTiming {
                    capture_ns: capture,
                    encode_start_ns: capture + MS,
                    encode_end_ns: capture + 20 * MS,
                }),
                ..Default::default()
            },
            &mut tr,
            at,
        );
    }
    reports.push(s.rtcp_report(next_report));
    let map = s.header_extensions().clone();
    let packets: Vec<RtpPacket> = tr
        .rtp
        .iter()
        .map(|d| RtpPacket::parse(&Bytes::from(d.clone()), &map).unwrap())
        .collect();
    let origin = packets[0].timestamp;
    let parsed: Vec<(u64, u32)> = reports
        .iter()
        .filter_map(|c| match &RtcpPacket::parse(c).unwrap()[0] {
            RtcpPacket::Sr { sender_info, .. } if sender_info.ntp_timestamp != 0 => {
                Some((sender_info.ntp_timestamp, sender_info.rtp_timestamp))
            }
            _ => None,
        })
        .collect();
    assert!(parsed.len() >= 2, "{}", parsed.len());
    let mut worst = 0.0f64;
    for (ntp, rtp) in &parsed {
        for packet in &packets {
            let offset = rtp.wrapping_sub(packet.timestamp) as i32 as f64;
            let estimate = ntp_seconds(*ntp) - offset / 90000.0;
            let capture = captures[&packet.timestamp.wrapping_sub(origin)];
            worst = worst.max((estimate - ntp_seconds(ntp_from_monotonic_ns(capture))).abs());
        }
    }
    assert!(worst < 0.001, "worst {} ms", worst * 1000.0);
    let stamped: Vec<&RtpPacket> = packets
        .iter()
        .filter(|p| p.extensions.abs_capture_time.is_some())
        .collect();
    assert!((2..=4).contains(&stamped.len()), "{}", stamped.len());
    assert_eq!(
        stamped[0].timestamp, packets[0].timestamp,
        "the key frame carries it"
    );
    let error = stamped
        .iter()
        .map(|p| {
            let capture = captures[&p.timestamp.wrapping_sub(origin)];
            (ntp_seconds(p.extensions.abs_capture_time.unwrap())
                - ntp_seconds(ntp_from_monotonic_ns(capture)))
            .abs()
        })
        .fold(0.0, f64::max);
    assert!(error < 0.001, "{error}");
    let mut firsts = HashSet::new();
    for p in &packets {
        let first = firsts.insert(p.timestamp);
        assert!(
            first || p.extensions.abs_capture_time.is_none(),
            "only a frame's first packet carries it"
        );
    }
}

#[test]
fn a_peer_that_did_not_negotiate_abs_capture_time_is_sent_none() {
    let mut s = RtpSender::new(Kind::Video, Box::new(NoHooks));
    s.start(&parameters("video/H264", false)).unwrap();
    let mut tr = FakeTransport::new(1000.0);
    s.push_frame(
        EncodedFrame {
            data: h264_frame(),
            keyframe: true,
            ..Default::default()
        },
        &mut tr,
        1000.0,
    );
    let mut map = HeaderExtensionsMap::new();
    map.configure(&[HeaderExtensionParameters::new(9, ABS_CAPTURE_TIME_URI)]);
    assert!(!tr.rtp.is_empty());
    for d in &tr.rtp {
        assert_eq!(
            RtpPacket::parse(&Bytes::from(d.clone()), &map)
                .unwrap()
                .extensions
                .abs_capture_time,
            None
        );
    }
}

// ---- test_rtcp_xr

#[test]
fn a_reference_time_is_answered_beside_every_sender_report() {
    let mut s = RtpSender::new(Kind::Video, Box::new(NoHooks));
    s.start(&parameters("video/H264", false)).unwrap();
    let mut tr = FakeTransport::new(1000.0);
    s.send_report(&mut tr, 1000.0);
    assert!(
        !RtcpPacket::parse(&tr.rtcp[0])
            .unwrap()
            .iter()
            .any(|p| matches!(p, RtcpPacket::Xr(_))),
        "no DLRR before a reference time"
    );
    let ntp = 0xE9C2_1234_5678_9ABCu64;
    let rrtr = |ntp| {
        RtcpPacket::Xr(XrPacket {
            ssrc: 1,
            rrtr: Some(ntp),
            dlrr: vec![],
        })
    };
    s.handle_rtcp(&rrtr(ntp), &mut tr, 1000.0);
    let arrived = (1000.0f64 * 1e9) as i64;
    let report = s.dlrr_report(arrived + 250_000_000);
    assert_eq!(report.dlrr.len(), 1);
    let (ssrc, lrr, delay) = report.dlrr[0];
    assert_eq!((ssrc, lrr), (1, 0x1234_5678));
    assert!((delay as i64 - 16384).abs() <= 2, "{delay}");
    assert_eq!(report.ssrc, s.ssrc());
    s.handle_rtcp(&rrtr(ntp + (1 << 32)), &mut tr, 1000.0);
    assert_eq!(
        s.dlrr_report(arrived).dlrr.len(),
        1,
        "a newer one replaces the reporter's last"
    );
    for i in 0..3 {
        s.send_report(&mut tr, 1001.0 + i as f64);
    }
    for compound in &tr.rtcp[1..] {
        let packets = RtcpPacket::parse(compound).unwrap();
        assert!(matches!(packets[0], RtcpPacket::Sr { .. }));
        assert!(matches!(packets[1], RtcpPacket::Sdes { .. }));
        match &packets[2] {
            RtcpPacket::Xr(xr) => assert_eq!(xr.dlrr[0].1, 0x1235_5678),
            other => panic!("{other:?}"),
        }
    }
    for ssrc in 0..2 * RRTR_REPORTERS_MAX as u32 {
        s.handle_rtcp(
            &RtcpPacket::Xr(XrPacket {
                ssrc,
                rrtr: Some(1),
                dlrr: vec![],
            }),
            &mut tr,
            1005.0,
        );
    }
    assert_eq!(
        s.dlrr_report(arrived).dlrr.len(),
        RRTR_REPORTERS_MAX,
        "reporters are bounded"
    );
    s.stop(&mut tr, 1006.0);
    assert!(
        matches!(&RtcpPacket::parse(tr.rtcp.last().unwrap()).unwrap()[0], RtcpPacket::Bye { sources } if sources == &vec![s.ssrc()])
    );
}

// ---- test_reference_invalidation: numbering

#[test]
fn frames_are_numbered_and_measured_back() {
    let mut h = Harness::selective();
    let s = &mut h.s;
    s.frame_number = 65534;
    assert_eq!(s.describe(100, None, true), Some((65534, None)));
    assert_eq!(s.describe(101, Some(100), false), Some((65535, Some(1))));
    assert_eq!(s.describe(102, Some(100), false), Some((0, Some(2))));
    assert_eq!(
        s.describe(103, Some(77), false),
        None,
        "one this sender never sent"
    );
    assert_eq!(s.describe(104, None, true), Some((1, None)));
    assert_eq!(
        s.describe(105, Some(102), false),
        None,
        "a key frame forgets the frames before it"
    );
    for fid in 200..206 + FRAME_NUMBERS_MEMORY as u16 {
        s.describe(fid, (fid > 200).then(|| fid - 1), fid == 200);
    }
    let newest = 205 + FRAME_NUMBERS_MEMORY as u16;
    let number = s.frame_number;
    assert_eq!(s.frame_numbers.len(), FRAME_NUMBERS_MEMORY);
    assert_eq!(s.describe(newest + 1, Some(205), false), None);
    assert_eq!(
        s.describe(newest + 2, Some(newest), false),
        Some((number, Some(1)))
    );
}

// ---- test_reference_invalidation: the answer to a NACK

#[test]
fn a_second_nack_names_the_frame_lost_once() {
    let mut h = Harness::new(true, None, true, "video/H264");
    h.s.history = RtpHistory::new();
    for (seq, frame) in [(1, 10), (2, 10), (3, 11)] {
        h.s.history.add(payload_packet(seq), 0.0, Some(frame));
    }
    h.nack(&[2, 3]);
    assert_eq!(h.tr.seqs(), vec![2, 2, 3, 3], "each packet twice");
    assert!(h.rec.events().is_empty());
    h.nack(&[2]);
    assert_eq!(h.tr.seqs(), vec![2, 2, 3, 3, 2], "and once more");
    assert_eq!(h.rec.events(), vec!["lost_frame 10"]);
    h.nack(&[1, 3]);
    assert_eq!(h.rec.events(), vec!["lost_frame 10", "lost_frame 11"]);
    assert_eq!(h.tr.seqs(), vec![2, 2, 3, 3, 2, 1, 1, 3]);
    h.nack(&[99, 3]);
    assert_eq!(h.rec.events().last().unwrap(), "pli");
    assert_eq!(
        h.tr.seqs().len(),
        9,
        "a NACK past the history still repairs the rest"
    );
    h.nack(&[2, 1]);
    assert_eq!(
        h.rec.events(),
        vec!["lost_frame 10", "lost_frame 11", "pli"],
        "a frame is named lost once"
    );
    assert_eq!(h.tr.seqs()[9..], [2, 1]);
    h.s.history
        .age_lost(10, 2.0 * super::super::packet::RTP_HISTORY_S);
    h.s.history.add(payload_packet(5), 0.0, Some(10));
    h.s.history.nacked(5);
    h.nack(&[5]);
    assert_eq!(
        h.rec.events().last().unwrap(),
        "lost_frame 10",
        "past the horizon it can be named again"
    );

    h.s.rtt = Some(0.05);
    let now = h.clock;
    h.s.history.add(payload_packet(6), now - 1.0, Some(12));
    h.nack(&[6]);
    h.nack(&[6]);
    assert!(
        !h.rec.events().contains(&"lost_frame 12".to_string()),
        "before the repair had a round trip"
    );
    assert_eq!(h.tr.seqs()[h.tr.seqs().len() - 3..], [6, 6, 6]);
    h.s.history.age_repair(6, 0.1);
    h.s.history.repaired(6, now);
    h.nack(&[6]);
    assert!(
        !h.rec.events().contains(&"lost_frame 12".to_string()),
        "nor before a FlexFEC repair had"
    );
    h.s.history.age_repair(6, 0.1);
    h.nack(&[6]);
    assert_eq!(h.rec.events().last().unwrap(), "lost_frame 12");
}

#[test]
fn a_retransmission_is_wrapped_as_rtx_under_its_own_transport_number() {
    let mut params = parameters("video/H264", false);
    let mut rtx = CodecParameters::new("video/rtx", 90000, 97);
    rtx.parameters.insert("apt".into(), "96".into());
    params.codecs.push(rtx);
    let mut s = RtpSender::new(Kind::Video, Box::new(NoHooks));
    s.start(&params).unwrap();
    s.set_ssrcs(11, 22, 33);
    let mut tr = FakeTransport::new(1000.0);
    s.push_frame(
        EncodedFrame {
            data: h264_frame(),
            keyframe: true,
            ..Default::default()
        },
        &mut tr,
        1000.0,
    );
    let map = s.header_extensions().clone();
    let original = RtpPacket::parse(&Bytes::from(tr.rtp[0].clone()), &map).unwrap();
    s.handle_rtcp(
        &RtcpPacket::Rtpfb(RtpfbPacket::nack(1, 11, vec![original.sequence_number])),
        &mut tr,
        1000.1,
    );
    let copies: Vec<RtpPacket> = tr
        .media(97)
        .iter()
        .map(|d| RtpPacket::parse(&Bytes::from(d.clone()), &map).unwrap())
        .collect();
    assert_eq!(copies.len(), 2, "a first repair goes out twice");
    assert_eq!(copies[0].ssrc, 22);
    assert_eq!(
        copies[1].sequence_number,
        copies[0].sequence_number.wrapping_add(1)
    );
    let unwrapped = super::super::packet::unwrap_rtx(&copies[0], 96, 11);
    assert_eq!(unwrapped.sequence_number, original.sequence_number);
    assert_eq!(unwrapped.payload.to_bytes(), original.payload.to_bytes());
    let numbers: HashSet<Option<u16>> = [&original, &copies[0], &copies[1]]
        .iter()
        .map(|p| p.extensions.transport_sequence_number)
        .collect();
    assert_eq!(numbers.len(), 3, "each is a new packet on the wire");
    assert_eq!(s.packet_count(), 1, "the report counts media alone");
}

// ---- test_webrtc_padding: the sender's side

#[test]
fn padding_resends_the_newest_large_packets_on_the_rtx_stream() {
    let source = |seq: u16| {
        let mut p = RtpPacket::new(96, seq, 0, 11);
        p.payload = Payload::from(vec![seq as u8; 1100]);
        p
    };
    let mut s = RtpSender::new(Kind::Video, Box::new(NoHooks));
    s.start(&parameters("video/H264", false)).unwrap();
    s.set_ssrcs(11, 22, 33);
    let mut tr = FakeTransport::new(1000.0);
    tr.pad_allow = 3;
    assert_eq!(
        s.send_padding(100_000, &mut tr, 1000.0),
        0,
        "nothing pads before a packet was sent"
    );
    s.keep_padding_source(&payload_packet(999));
    assert!(
        s.padding_sources.is_empty(),
        "a small packet is no padding source"
    );
    for seq in 0..(PADDING_SOURCES as u16 + 2) {
        s.keep_padding_source(&source(1000 + seq));
    }
    assert!(
        s.padding_sources.len() == PADDING_SOURCES && s.padding_sources[0].sequence_number == 1002,
        "it keeps only the newest packets to pad with"
    );
    assert!(
        s.send_padding(100_000, &mut tr, 1000.0) == 0 && tr.twcc.window().padding_sent == 0,
        "nothing pads without RTX negotiated"
    );

    s.rtx_payload_type = Some(97);
    let got = s.send_padding(100_000, &mut tr, 1000.0);
    let map = s.header_extensions().clone();
    let packets: Vec<RtpPacket> = tr
        .padded
        .iter()
        .map(|d| RtpPacket::parse(&Bytes::from(d.clone()), &map).unwrap())
        .collect();
    assert!(
        packets.len() == 3
            && packets.iter().all(|p| p.payload_type == 97 && p.ssrc == 22)
            && got == tr.padded.iter().map(Vec::len).sum::<usize>(),
        "with RTX it resends its packets on the RTX stream while the pacer admits them: {} {got}",
        packets.len()
    );
    let originals: Vec<u16> = packets
        .iter()
        .map(|p| u16::from_be_bytes([p.payload.to_bytes()[0], p.payload.to_bytes()[1]]))
        .collect();
    assert_eq!(
        originals,
        vec![1002, 1003, 1004],
        "each carrying its original sequence number first"
    );
    assert!(
        packets
            .iter()
            .all(|p| p.extensions.transport_sequence_number.is_some())
            && tr.twcc.window().padding_sent == tr.twcc.window().bytes_sent
            && tr.twcc.window().padding_sent > 0,
        "numbered only once admitted, each as padding"
    );
    tr.pad_allow = 100;
    let got = s.send_padding(2500, &mut tr, 1000.0);
    assert!(
        (2500..2500 + 1300).contains(&got),
        "and stops at its budget: {got}"
    );
    s.set_enabled(false);
    tr.pad_allow = 100;
    assert_eq!(
        s.send_padding(2500, &mut tr, 1000.0),
        0,
        "a paused page's sender pads nothing"
    );
}

// ---- test_webrtc_gop_reset: the sender's repairs

#[test]
fn an_abandoned_gop_is_gone_from_the_repairs() {
    let mut h = Harness::new(true, None, true, "video/H264");
    h.nack(&[1, 2]);
    assert_eq!(h.tr.attempts.len(), 4, "a first NACK's each sent twice");
    assert!((1..6).all(|s| !h.s.history.abandoned(s)));
    h.tr.refusals = VecDeque::from([false, true, false]);
    h.tr.attempts.clear();
    h.nack(&[1, 2, 3, 4]);
    assert_eq!(
        h.tr.attempts.len(),
        2,
        "a batch stops at the repair the pacer refused"
    );
    assert!((1..5).all(|s| h.s.history.abandoned(s)));
    assert_eq!(h.rec.events(), vec!["lost_frame 1"]);
    assert_ne!(
        h.tr.attempts[0], h.tr.attempts[1],
        "a repair goes out under its own number"
    );
    h.tr.attempts.clear();
    h.rec.clear();
    h.nack(&[3, 4]);
    assert!(
        h.tr.attempts.is_empty() && h.rec.events().is_empty(),
        "a late NACK for an abandoned packet is ignored"
    );
    for seq in [6, 7] {
        h.s.history.add(payload_packet(seq), 0.0, Some(seq));
    }
    h.nack(&[4, 7]);
    assert_eq!(h.tr.attempts.len(), 2, "a packet sent after is repaired");
    assert!(h.rec.events().is_empty());
    for seq in [40000, 40001] {
        h.s.history.add(payload_packet(seq), 0.0, Some(seq));
    }
    h.tr.attempts.clear();
    h.nack(&[40001]);
    assert_eq!(
        h.tr.attempts.len(),
        2,
        "half the numbering after an abandonment"
    );
    h.rec.clear();
    h.nack(&[99]);
    assert_eq!(h.rec.events(), vec!["pli"]);
}

// ---- test_webrtc_resync

#[test]
fn a_pacer_cut_is_answered_without_a_key_frame() {
    let mut h = Harness::selective();
    h.send(1, None, 10, true, false);
    h.send(2, Some(1), 20, false, false);
    h.send(3, Some(2), 30, false, false);
    h.send(4, Some(3), 40, false, false);
    h.on_wire(20, None);
    assert_eq!(
        h.s.in_flight,
        VecDeque::from([(30, 3), (40, 4)]),
        "frames whose last packet left"
    );
    let now = h.clock;
    assert!(
        h.s.pacer_resync(Some(25), &mut h.tr, now),
        "a reset cutting frames 3 and 4"
    );
    assert_eq!(h.rec.events(), vec![h.fid(3)]);
    assert_eq!(h.numbered(), HashSet::from([1, 2]));
    assert!(
        (1..5).all(|q| h.s.history.abandoned(q)),
        "the repairs of what was sent before it end"
    );
    assert_eq!(h.send(5, Some(4), 50, false, false), None);
    assert_eq!(h.send(6, Some(5), 60, false, false), None);
    assert_eq!(
        h.rec.events(),
        vec![h.fid(3)],
        "held back, the report not repeated, no key frame"
    );
    let resumed = h.send(7, Some(2), 70, false, false);
    assert_eq!(
        resumed,
        Some((4, Some(3))),
        "its descriptor points back past the two cut"
    );
    assert!(h.s.resync_since.is_none());

    let mut h = Harness::selective();
    h.send(1, None, 10, true, false);
    h.send(2, Some(1), 20, false, false);
    let now = h.clock;
    h.s.pacer_resync(Some(15), &mut h.tr, now);
    h.clock += RESYNC_S + 0.01;
    h.send(3, Some(2), 30, false, false);
    assert!(
        h.rec.events().contains(&"pli".to_string()) && h.s.resync_since.is_none(),
        "never predicted past"
    );

    let mut h = Harness::new(false, Some(true), true, "video/H264");
    h.s.in_flight.push_back((10, 1));
    let now = h.clock;
    assert!(
        !h.s.pacer_resync(Some(5), &mut h.tr, now),
        "without the descriptor"
    );
    assert!(h.rec.events().is_empty());
    let mut h = Harness::selective();
    let now = h.clock;
    assert!(
        !h.s.pacer_resync(Some(5), &mut h.tr, now),
        "with nothing described in flight"
    );
    let mut h = Harness::new(true, Some(false), true, "video/H264");
    h.send(1, None, 10, true, false);
    h.send(2, Some(1), 20, false, false);
    let now = h.clock;
    assert!(
        !h.s.pacer_resync(Some(15), &mut h.tr, now),
        "for the display's owner"
    );
    assert!(h.rec.events().is_empty());
}

#[test]
fn a_real_pacer_cut_reaches_the_sender_through_the_verdict() {
    let mut h = Harness::selective();
    let mut pacer = Pacer::new(8_000_000, h.clock);
    pacer.set_credit(0.0, h.clock);
    h.tr.pacer = Some(pacer);
    h.tr.real_backlog = true;
    h.tr.heard_at = 1e12;
    let frame = |id: u16, reference, key, size: usize| {
        let mut data = vec![0, 0, 0, 1, if key { 0x65 } else { 0x41 }];
        data.resize(5 + size, 0x88);
        EncodedFrame {
            data: Bytes::from(data),
            timestamp: id as u32 * 3000,
            keyframe: key,
            dependency: Some((id, reference)),
            ..Default::default()
        }
    };
    let now = h.clock;
    h.s.push_frame(frame(1, None, true, 20_000), &mut h.tr, now);
    // Past the 120 ms budget at the pace: the key frame sized the floor, so this overflows it.
    h.s.push_frame(frame(2, Some(1), false, 1_000_000), &mut h.tr, now);
    assert_eq!(h.s.resyncs, 1);
    let pacer = h.tr.pacer.as_ref().unwrap();
    assert_eq!(pacer.stats().resyncs, 1, "{:?}", pacer.snapshot());
    assert_eq!(
        pacer.stats().keyreqs,
        0,
        "no key frame on the shared stream"
    );
    assert!(!pacer.gop_dead());
    assert!(
        h.s.frame_ends
            .iter()
            .all(|(tag, _)| h.tr.twcc.history_seqs().contains(tag))
    );
    assert!(h.rec.events().contains(&h.fid(1)), "{:?}", h.rec.events());
    let now = h.clock;
    let room = h.tr.pacer.as_ref().unwrap().video_room();
    assert!(room > 0);
    let sent = h.tr.rtp.len();
    h.s.push_frame(frame(3, None, true, 2_000), &mut h.tr, now);
    assert!(h.tr.rtp.len() > sent, "video flows on");
}

#[test]
fn a_peer_without_room_is_left_frames_and_resynced() {
    let mut h = Harness::selective();
    h.send(1, None, 10, true, false);
    assert!(h.send(2, Some(1), 20, false, false).is_some());
    h.behind(RESYNC_ROOM_FRAMES, 0.0);
    let reach = RESYNC_REACH_FRAMES as u16;
    let out: Vec<_> = (3..3 + reach - 1)
        .map(|n| h.send(n, Some(n - 1), 10 * n, false, false))
        .collect();
    assert!(out.iter().all(Option::is_none));
    assert_eq!(h.s.resync_first, Some(3));
    assert!(h.rec.events().is_empty(), "not told while it has no room");
    h.send(3 + reach - 1, Some(3 + reach - 2), 0, false, false);
    assert_eq!(h.rec.events(), vec![h.fid(3)], "at the reach it is told");
    let branch = h.send(20, Some(2), 200, false, false);
    assert_eq!(
        branch,
        Some((2, Some(1))),
        "the frame predicting past the run goes out"
    );
    assert!(h.s.resync_since.is_none());
    h.behind(0, RESYNC_LAG_S);
    assert_eq!(
        h.send(21, Some(20), 210, false, false),
        None,
        "a pacer that far behind has no room either"
    );
    assert_eq!(h.s.resync_first, Some(21));

    let mut h = Harness::new(true, Some(false), true, "video/H264");
    h.send(1, None, 10, true, false);
    h.behind(RESYNC_FAR_FRAMES, 5.0);
    assert!(
        (2..12).all(|n| h.send(n, Some(n - 1), 10 * n, false, false).is_some()),
        "the owner is sent every frame"
    );
    assert!(h.rec.events().is_empty());
}

#[test]
fn a_frame_num_wrap_is_never_left_out() {
    let wrap = FRAME_NUM_WRAP as u16;
    let mut h = Harness::selective();
    h.send(1, None, 10, true, false);
    for n in 2..wrap - 1 {
        h.send(n, Some(n - 1), 10 * n, false, false);
    }
    h.behind(RESYNC_ROOM_FRAMES, 0.0);
    let n = wrap - 1;
    h.send(n, Some(n - 1), 10 * n, false, false);
    assert_eq!(
        h.rec.events(),
        vec![h.fid(n)],
        "a run opened just ahead of it is reported at once"
    );
    assert!(h.send(n + 1, Some(n - 1), 0, false, false).is_some());
    assert!(h.send(n + 2, Some(n + 1), 0, false, false).is_some());

    let mut h = Harness::new(true, Some(true), true, "video/AV1");
    h.send(1, None, 10, true, false);
    for n in 2..wrap + 1 {
        h.send(n, Some(n - 1), 10 * n, false, false);
    }
    h.behind(RESYNC_ROOM_FRAMES, 0.0);
    assert_eq!(
        h.send(wrap + 1, Some(wrap), 0, false, false),
        None,
        "a codec without frame_num"
    );
    assert!(h.rec.events().is_empty());
}

#[test]
fn a_peer_far_behind_is_answered_once_it_has_room() {
    let reach = RESYNC_REACH_FRAMES as u16;
    let mut h = Harness::new(true, Some(true), false, "video/H264");
    h.send(1, None, 10, true, false);
    h.behind(RESYNC_FAR_FRAMES, 0.0);
    for n in 2..4 + reach {
        h.send(n, Some(n - 1), 10 * n, false, false);
    }
    assert!(h.rec.events().is_empty(), "not answered at the reach");
    h.behind(0, 0.0);
    let n = 4 + reach;
    h.send(n, Some(n - 1), 0, false, false);
    assert!(
        h.rec.events().is_empty(),
        "a run past the reach waits while its requests are spent"
    );
    h.rec.set_taken(true);
    h.send(n + 1, Some(n), 0, false, false);
    assert_eq!(h.rec.events(), vec!["resync 2 false"]);

    for mime in ["video/AV1", "video/H264"] {
        let mut h = Harness::new(true, Some(true), false, mime);
        h.send(1, None, 10, true, false);
        h.send(2, Some(1), 20, false, true);
        h.behind(RESYNC_FAR_FRAMES, 0.0);
        for n in 3..5 + reach {
            h.send(n, Some(n - 1), 10 * n, false, false);
        }
        h.behind(0, 0.0);
        h.send(5 + reach, Some(4 + reach), 0, false, false);
        assert_eq!(
            h.rec.events(),
            vec![h.fid(3)],
            "where anchors run, within reach: {mime}"
        );
    }

    let mut h = Harness::selective();
    h.send(1, None, 10, true, false);
    h.tr.backlog = (0, 2.0 * RESYNC_LAG_S, RESYNC_LOSS_S / 2.0);
    for n in 2..3 + reach {
        h.send(n, Some(n - 1), 10 * n, false, false);
    }
    assert!(
        h.rec.events().is_empty(),
        "a path losing packets behind a standing queue"
    );
    let mut h = Harness::selective();
    h.send(1, None, 10, true, false);
    h.tr.backlog = (0, 0.0, RESYNC_LOSS_S / 2.0);
    assert!(
        h.send(2, Some(1), 20, false, false).is_some(),
        "loss with no queue takes no room away"
    );
}

#[test]
fn anchors_go_without_room_and_end_a_run() {
    let reach = RESYNC_REACH_FRAMES as u16;
    let mut h = Harness::selective();
    h.send(1, None, 10, true, false);
    h.send(2, Some(1), 20, false, false);
    h.behind(RESYNC_ROOM_FRAMES, 0.0);
    assert!(h.send(3, Some(2), 30, false, true).is_some() && h.s.resync_since.is_none());
    let out: Vec<_> = (4..4 + reach - 1)
        .map(|n| h.send(n, Some(n - 1), 10 * n, false, false))
        .collect();
    assert!(
        out.iter().all(Option::is_none) && h.s.resync_first == Some(4) && h.rec.events().is_empty()
    );
    assert!(
        h.send(20, Some(3), 200, false, true).is_some() && h.s.resync_since.is_none(),
        "the next anchor ends the run"
    );
    h.send(21, Some(20), 210, false, false);
    h.behind(RESYNC_FAR_FRAMES, 0.0);
    assert_eq!(
        h.send(30, Some(3), 300, false, true),
        None,
        "a peer too far behind is not sent one"
    );
    assert_eq!(h.s.resync_first, Some(30));
    h.behind(0, 0.0);
    h.send(31, Some(30), 310, false, false);
    assert_eq!(h.rec.events(), vec![h.fid(30)]);
    h.on_wire(0, None);
    h.rec.clear();
    h.on_wire(0, Some(20));
    assert_eq!(
        h.rec.events(),
        vec!["out 20 false"],
        "a frame whose last packet leaves"
    );
    h.s.frame_numbers.clear();
    for n in 100..170 {
        h.s.describe(n, (n != 100).then(|| n - 1), n == 100);
    }
    assert!(
        h.s.describe(200, Some(100), false).is_some(),
        "numbered for longer than the recent ones"
    );
}

#[test]
fn an_anchored_run_waits_longer_for_a_key_frame() {
    let mut h = Harness::selective();
    h.send(1, None, 10, true, false);
    h.behind(RESYNC_FAR_FRAMES, 0.0);
    assert_eq!(
        h.send(2, Some(1), 20, false, false),
        None,
        "left out where far behind"
    );
    assert!(h.s.resync_since.is_some());
    let mut h = Harness::selective();
    h.send(1, None, 10, true, false);
    h.send(2, Some(1), 20, false, false);
    h.behind(RESYNC_ROOM_FRAMES, 0.0);
    h.send(3, Some(2), 30, false, true);
    h.send(4, Some(3), 40, false, false);
    h.behind(RESYNC_FAR_FRAMES, 0.0);
    h.send(5, Some(4), 50, false, false);
    h.clock += RESYNC_S + 0.5;
    h.send(6, Some(5), 60, false, false);
    assert!(!h.rec.events().contains(&"pli".to_string()));
    h.clock += RESYNC_ANCHORED_S;
    h.send(7, Some(6), 70, false, false);
    assert!(h.rec.events().contains(&"pli".to_string()));
}

#[test]
fn a_loss_past_repair_is_resynced_from_the_newest_frame_held() {
    let frames = |h: &mut Harness| {
        for n in 1..6 {
            h.send(n, (n != 1).then(|| n - 1), 10 * n, n == 1, n == 3);
        }
    };
    let mut h = Harness::selective();
    frames(&mut h);
    assert!(
        !h.resync(),
        "holding no frame, with no key frame on its way"
    );
    h.s.stall_since = None;
    h.hold(&[1, 2, 3]);
    let t = h.clock;
    h.tr.twcc.set_link_times(t - RESYNC_DARK_S - 0.01, t);
    assert!(
        h.resync() && h.rec.events().is_empty() && h.s.stall_since.is_none(),
        "a dark link"
    );
    h.tr.twcc
        .set_link_times(t - RESYNC_DARK_S - 0.01, t - RESYNC_DARK_S - 0.01);
    assert!(h.resync(), "sent nothing since its last report");
    assert_eq!(h.rec.events(), vec![h.fid(4)]);
    assert!(!h.s.frame_numbers.contains(&4));
    h.tr.twcc.set_link_times(f64::INFINITY, 0.0);
    assert_eq!(
        h.send(6, Some(5), 60, false, false),
        None,
        "the frames after it are held back"
    );
    assert!(
        h.resync() && h.rec.events() == vec![h.fid(4)],
        "answered by the run"
    );
    let mut starved = false;
    for _ in 0..3 {
        h.clock += RESYNC_ANCHORED_S / 2.0;
        starved = h.resync();
    }
    assert!(starved, "requests while it holds nothing new are no stall");
    h.s.held_at = h.clock;
    h.clock += RESYNC_ANCHORED_S / 2.0;
    h.resync();
    h.s.held_at = h.clock;
    h.clock += RESYNC_ANCHORED_S / 2.0 + 0.1;
    assert!(
        !h.resync(),
        "a stall past RESYNC_ANCHORED_S while it holds what it is sent"
    );

    let mut h = Harness::selective();
    frames(&mut h);
    h.hold(&[1, 2, 3]);
    h.resync();
    h.hold(&[5]);
    h.s.close_run();
    h.rec.clear();
    assert!(
        h.resync() && h.rec.events().is_empty(),
        "a frame held after a resync let it go is passed over"
    );

    let mut h = Harness::selective();
    h.send(1, None, 10, true, false);
    h.send(2, Some(1), 20, false, true);
    h.s.held.clear();
    h.s.key_sent_at = Some(h.clock);
    h.s.rtt = Some(0.8);
    h.clock += 1.5;
    assert!(
        h.resync(),
        "a key frame on its way is waited for a round trip past its queue"
    );
    h.clock += 0.2;
    assert!(!h.resync(), "and asked again past that");

    let mut h = Harness::selective();
    h.send(1, None, 10, true, false);
    h.send(2, Some(1), 20, false, true);
    for (n, reference) in [(3, 2), (4, 3), (5, 1), (6, 5)] {
        h.send(n, Some(reference), 10 * n, false, false);
        h.unheld(n, Some(reference), false, &[]);
    }
    h.hold(&[1, 2]);
    assert!(h.resync());
    assert_eq!(
        h.rec.events(),
        vec![h.fid(5)],
        "the newest predicting from none of the others"
    );

    let mut h = Harness::selective();
    h.send(1, None, 10, true, false);
    h.send(2, Some(1), 20, false, true);
    h.send(3, None, 30, true, false);
    assert!(h.s.anchored, "a key frame leaves the stream anchored");
    let mut o = Harness::new(true, Some(false), true, "video/H264");
    o.send(1, None, 10, true, false);
    o.send(2, Some(1), 20, false, true);
    o.hold(&[1]);
    assert!(!o.resync(), "the display's owner always is left one");
}

#[test]
fn a_frame_is_held_once_reported_and_its_reference_is() {
    let mut h = Harness::new(true, None, true, "video/H264");
    h.unheld(1, None, true, &[(100, 1), (101, 2)]);
    h.unheld(2, Some(1), false, &[(102, 3)]);
    h.unheld(3, Some(2), false, &[(103, 4), (104, 5)]);
    let t = h.clock;
    for seq in [100, 101, 103, 104] {
        h.tr.twcc.mark_arrived(seq, t);
    }
    h.s.confirm(&mut h.tr, t);
    assert_eq!(h.rec.events(), vec!["held 1 true"]);
    assert_eq!(
        h.s.unheld.iter().map(|u| u.frame_id).collect::<Vec<_>>(),
        vec![2, 3]
    );
    h.s.rtx_twcc.insert(3, 110);
    h.tr.twcc.mark_arrived(110, t);
    h.s.confirm(&mut h.tr, t);
    assert_eq!(
        h.rec.events(),
        vec!["held 1 true", "held 2 false", "held 3 false"]
    );
    assert!(
        h.s.unheld.is_empty(),
        "a retransmission received holds the packet it repairs"
    );
    h.unheld(4, Some(3), false, &[(120, 6)]);
    h.clock += HELD_S + 0.01;
    let t = h.clock;
    h.tr.twcc.mark_arrived(120, t);
    h.s.confirm(&mut h.tr, t);
    assert!(
        h.rec.events().last().unwrap() == "held 3 false" && h.s.unheld.is_empty(),
        "never past HELD_S"
    );

    let mut h = Harness::new(true, None, true, "video/H264");
    h.unheld(1, None, true, &[(1000, 1)]);
    for n in 2..200 {
        h.unheld(n, Some(1), false, &[(1000 + n, n)]);
    }
    let t = h.clock;
    for seq in 1000..1200 {
        h.tr.twcc.mark_arrived(seq, t);
    }
    h.s.confirm(&mut h.tr, t);
    assert_eq!(
        h.rec.events().len(),
        199,
        "predicting from the key frame, however many were held since"
    );
}

#[test]
fn a_peer_without_the_descriptor_is_served_in_order() {
    assert!(
        Harness::new(false, None, true, "video/H264")
            .s
            .numbers_frames()
    );
    assert!(
        Harness::new(true, None, true, "video/VP8")
            .s
            .numbers_frames()
    );
    assert!(
        !Harness::new(false, None, true, "video/VP8")
            .s
            .numbers_frames()
    );
    let reach = RESYNC_REACH_FRAMES as u16;
    let mut h = Harness::new(false, Some(true), true, "video/H264");
    h.send(1, None, 10, true, false);
    h.send(2, Some(1), 20, false, false);
    h.behind(RESYNC_ROOM_FRAMES, 0.0);
    let out: Vec<_> = (3..3 + reach)
        .map(|n| h.send(n, Some(n - 1), 10 * n, false, false))
        .collect();
    assert!(out.iter().all(Option::is_none));
    assert_eq!(h.rec.events(), vec![h.fid(3)]);
    assert_eq!(h.send(30, Some(2), 300, false, false), Some((2, Some(1))));
    h.s.anchored = true;
    h.hold(&[1, 2]);
    assert!(!h.resync(), "its receiver decodes nothing past the hole");

    let mut h = Harness::new(false, None, true, "video/H264");
    h.unheld(1, None, true, &[(100, 1)]);
    h.unheld(2, Some(1), false, &[(101, 2)]);
    h.unheld(3, Some(1), false, &[(102, 3)]);
    let t = h.clock;
    h.tr.twcc.mark_arrived(100, t);
    h.tr.twcc.mark_arrived(102, t);
    h.s.confirm(&mut h.tr, t);
    assert_eq!(
        h.rec.events(),
        vec!["held 1 true"],
        "only once it holds every frame sent before"
    );
    h.s.unheld.push_back(Unheld {
        frame_id: 4,
        reference: Some(1),
        keyframe: false,
        sent_at: t + HELD_S,
        packets: vec![(103, 4)],
    });
    h.clock += HELD_S + 0.01;
    let t = h.clock;
    h.tr.twcc.mark_arrived(103, t);
    h.s.confirm(&mut h.tr, t);
    assert_eq!(h.rec.events(), vec!["held 1 true"]);
    assert_eq!(
        h.s.unheld.iter().map(|u| u.frame_id).collect::<Vec<_>>(),
        vec![4]
    );
    assert!(h.s.unheld_gap);
}

#[test]
fn a_nack_whose_repairs_would_overflow_the_queue_is_past_repair() {
    let mut h = Harness::selective();
    h.tr.room = Some(7);
    assert!(h.s.past_repair(&[1, 2, 3, 4], &mut h.tr));
    assert!(!h.s.past_repair(&[1, 2, 3], &mut h.tr));
    let mut o = Harness::new(true, Some(false), true, "video/H264");
    o.tr.room = Some(0);
    assert!(
        !o.s.past_repair(&[1, 2, 3, 4], &mut o.tr),
        "the owner's is repaired"
    );
}

// ---- test_still_request

struct Still(Harness);

impl Still {
    fn new() -> Self {
        let mut h = Harness::new(true, None, true, "video/H264");
        h.s.rtt = Some(0.02);
        h.rec.0.lock().unwrap().log_sent = true;
        h.s.frame_handed = Some(40);
        h.s.frame_left = Some((40, h.clock - 0.4));
        let t = h.clock;
        h.tr.twcc.seed_sent(41, 1200, t);
        Self(h)
    }

    /// Whether a request `after` seconds past the last one is answered with a key frame.
    fn ask(&mut self, fmt: u8, after: f64) -> bool {
        let h = &mut self.0;
        h.s.key_asked_at = h.clock - after;
        let before = h.rec.events().len();
        let packet = RtcpPacket::Psfb(PsfbPacket {
            fmt,
            ssrc: 1,
            media_ssrc: 2,
            fci: vec![],
        });
        h.s.handle_rtcp(&packet, &mut h.tr, h.clock);
        h.rec.events().len() > before
    }

    fn pli(&mut self) -> bool {
        self.ask(RTCP_PSFB_PLI, 0.4)
    }

    /// Frame `seq`, its encode begun at `encoded`, leaves now.
    fn left(&mut self, seq: u16, keyframe: bool, encoded: f64) {
        let h = &mut self.0;
        h.s.frame_handed = Some(seq);
        h.s.note_frame_end(
            seq,
            FrameEnd {
                keyframe,
                timing: Some(FrameTiming {
                    capture_ns: 1,
                    encode_start_ns: (encoded * 1e9) as i64,
                    encode_end_ns: 0,
                }),
                captured_ns: 5,
                size: 100,
                frame_id: None,
            },
        );
        h.s.frame_left(seq, h.clock);
    }
}

#[test]
fn a_receivers_wait_on_a_still_screen_costs_no_key_frame() {
    let mut s = Still::new();
    let t = s.0.clock;
    assert!(!s.pli(), "400 ms after an acknowledged frame");
    assert!(
        !s.ask(RTCP_PSFB_PLI, 0.375) && !s.ask(RTCP_PSFB_PLI, 0.375),
        "nor its repeats"
    );
    assert!(!s.ask(RTCP_PSFB_FIR, 0.4), "a FIR is the same wait");
    assert!(
        s.ask(RTCP_PSFB_PLI, STILL_REQUEST_S - 0.13),
        "sooner is a failed decode"
    );
    s.0.s.frame_left = Some((40, t - STILL_REQUEST_S + 0.05));
    assert!(s.pli(), "close behind the frame");
    s.0.s.rtt = Some(0.3);
    s.0.s.frame_left = Some((40, t - 0.4));
    assert!(s.pli(), "the round trip is part of the wait");
    s.0.s.frame_left = Some((40, t - 0.6));
    assert!(!s.pli(), "past it the request is the wait again");
    s.0.s.rtt = None;
    assert!(!s.pli(), "an unmeasured round trip counts as none");
    s.0.s.frame_handed = Some(42);
    assert!(s.pli(), "the newest frame still in the pacer");
    s.0.s.frame_handed = Some(41);
    s.0.s.frame_left = Some((41, t - 0.4));
    assert!(s.pli(), "a last packet the peer never acknowledged");
    s.0.s.frame_handed = Some(40);
    s.0.s.frame_left = Some((40, t - 0.4));
    s.0.s.lost_at = Some(t - 1.0);
    assert!(s.pli(), "after the peer lost a frame past repair");
    s.left(43, false, t - 1.1);
    s.0.s.frame_left = Some((43, t - 0.4));
    assert!(
        s.0.s.lost_at.is_some() && s.pli(),
        "a frame coded before the loss"
    );
    s.left(44, false, t - 0.99);
    s.0.s.frame_left = Some((44, t - 0.4));
    assert!(
        s.0.s.lost_at.is_none() && !s.pli(),
        "one coded after it predicts past the loss"
    );
    s.0.s.lost_at = Some(t);
    s.left(45, true, 0.0);
    assert!(s.0.s.lost_at.is_none(), "a key frame on the wire");
    s.0.s.lost_at = Some(t);
    s.left(47, false, 0.0);
    assert!(
        s.0.s.lost_at.is_some(),
        "a frame with no encode instant never clears a loss"
    );
    assert!(
        s.0.rec
            .events()
            .iter()
            .filter(|e| e.starts_with("sent"))
            .all(|e| e == "sent 5 100")
    );
    assert_eq!(s.0.s.frame_left, Some((47, t)));

    let mut s = Still::new();
    s.0.s.set_enabled(false);
    let t = s.0.clock;
    s.0.s.push_frame(
        EncodedFrame {
            data: h264_frame(),
            ..Default::default()
        },
        &mut s.0.tr,
        t,
    );
    assert_eq!(
        s.0.s.frame_left, None,
        "a frame this peer is not sent leaves none on the wire"
    );
    let mut s = Still::new();
    s.0.tr.heard_at = t - PEER_SILENCE_S - 1.0;
    s.0.s.push_frame(
        EncodedFrame {
            data: h264_frame(),
            ..Default::default()
        },
        &mut s.0.tr,
        t,
    );
    assert_eq!(s.0.s.frame_left, None, "nor one to a peer gone silent");
    assert!(s.0.tr.rtp.is_empty());
    s.0.tr.heard_at = t + 1.0;
    s.0.s.push_frame(
        EncodedFrame {
            data: h264_frame(),
            ..Default::default()
        },
        &mut s.0.tr,
        t + 1.0,
    );
    assert!(
        s.0.rec.events().contains(&"pli".to_string()),
        "its first packet back asks for a key frame"
    );
    assert!(!s.0.tr.rtp.is_empty());
    assert!(
        s.0.tr.twcc.acked(100) && !s.0.tr.twcc.acked(41),
        "a packet awaiting feedback is unacknowledged"
    );
}

// ---- test_flexfec_normalization and the density steer of test_flexfec_repair

const URIS: [&str; 7] = [
    MID_URI,
    ABS_SEND_TIME_URI,
    TRANSPORT_CC_URI,
    PLAYOUT_DELAY_URI,
    VIDEO_TIMING_URI,
    DEPENDENCY_DESCRIPTOR_URI,
    TRANSMISSION_OFFSET_URI,
];

fn configured(extended: bool) -> HeaderExtensionsMap {
    let mut map = HeaderExtensionsMap::new();
    let offset = if extended { 20 } else { 0 };
    map.configure(
        &URIS
            .iter()
            .enumerate()
            .map(|(i, uri)| HeaderExtensionParameters::new(i as u8 + 1 + offset, uri))
            .collect::<Vec<_>>(),
    );
    map
}

/// libwebrtc's recovery input, from the structured fields rather than the byte walk.
fn receiver_packet(packet: &RtpPacket, map: &HeaderExtensionsMap) -> Vec<u8> {
    let mut normalized = packet.clone();
    let e = &mut normalized.extensions;
    e.abs_send_time = e.abs_send_time.map(|_| 0);
    e.transport_sequence_number = e.transport_sequence_number.map(|_| 0);
    e.video_timing = e.video_timing.map(|t| (t.0, t.1, t.2, t.3, 0, 0, 0));
    normalized.serialize(map)
}

fn raw_packet(profile: u16, extensions: &[u8]) -> Vec<u8> {
    let mut out = vec![0xB2, 96];
    out.extend_from_slice(&100u16.to_be_bytes());
    out.extend_from_slice(&123456u32.to_be_bytes());
    out.extend_from_slice(&0x1234u32.to_be_bytes());
    out.extend_from_slice(&19u32.to_be_bytes());
    out.extend_from_slice(&23u32.to_be_bytes());
    out.extend_from_slice(&profile.to_be_bytes());
    out.extend_from_slice(&((extensions.len() / 4) as u16).to_be_bytes());
    out.extend_from_slice(extensions);
    out.extend_from_slice(b"media\x00\x00\x00\x04");
    out
}

fn zeroed(data: &[u8], spans: &[(usize, usize)]) -> Vec<u8> {
    let mut out = data.to_vec();
    for (start, stop) in spans {
        out[*start..*stop].fill(0);
    }
    out
}

#[test]
fn every_single_loss_comes_back_as_the_receivers_copy() {
    for extended in [false, true] {
        let map = configured(extended);
        for size in [2u16, 3, 10] {
            let mut wire = Vec::new();
            let mut expected = Vec::new();
            for i in 0..size {
                let mut packet = RtpPacket::new(96, 100 + i, 123456, 0x1234);
                let mut payload = vec![0x7c, 0x01];
                payload.extend(std::iter::repeat_n(0x55 + i as u8, 128));
                packet.payload = Payload::from(payload);
                packet.extensions = HeaderExtensions {
                    mid: Some("0".into()),
                    abs_send_time: Some(0x314159 + i as u32),
                    transport_sequence_number: Some(200 + i),
                    playout_delay: Some((0, 0)),
                    dependency_descriptor: Some(Bytes::from_static(b"\x01\x00\x08")),
                    video_timing: (i == size - 1).then_some((1, 3, 5, 7, 2, 11, 13)),
                    ..Default::default()
                };
                if extended {
                    packet.csrc = vec![19, 23];
                }
                wire.push(packet.serialize(&map));
                expected.push(receiver_packet(&packet, &map));
            }
            let protected: Vec<Vec<u8>> = wire.iter().map(|d| map.for_fec(d)).collect();
            assert_eq!(protected, expected, "extended {extended} size {size}");
            assert_ne!(wire.last(), protected.last());
            let header = RepairHeader {
                first_sequence_number: 100,
                protected_ssrc: 0x1234,
                payload_type: 118,
                sequence_number: 900,
                timestamp: 123456,
                ssrc: 8,
            };
            let fec = build_flexfec_03(&protected, &header, &[]);
            for missing in 0..size {
                let mut known: HashMap<u16, Vec<u8>> = expected
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i as u16 != missing)
                    .map(|(i, d)| (100 + i as u16, d.clone()))
                    .collect();
                let got = recover(std::slice::from_ref(&fec), &mut known);
                assert_eq!(got.get(&(100 + missing)), Some(&expected[missing as usize]));
            }
            let timing =
                RtpPacket::parse(&Bytes::from(wire.last().unwrap().clone()), &map).unwrap();
            assert_eq!(
                timing.extensions.video_timing,
                Some((1, 3, 5, 7, 2, 11, 13)),
                "the packet sent keeps it whole"
            );
        }
    }
}

#[test]
fn the_walk_stops_where_libwebrtcs_parser_stops() {
    let one = raw_packet(0xBEDE, b"\x00\x22\x31\x41\x59\x71\x12\x34\x90\xab\x00\x00");
    assert_eq!(
        configured(false).for_fec(&one),
        zeroed(&one, &[(26, 29), (30, 32)])
    );
    assert_eq!(&one[one.len() - 9..], b"media\x00\x00\x00\x04");
    let two = raw_packet(
        0x100F,
        b"\x00\x16\x03\x31\x41\x59\x1b\x03\x12\x34\x56\x09\x00\x00\x00\x00",
    );
    assert_eq!(
        configured(true).for_fec(&two),
        zeroed(&two, &[(27, 30), (32, 35)])
    );
    let map = configured(false);
    let mut plain = RtpPacket::new(0, 0, 0, 0);
    plain.payload = Payload::from(&b"media"[..]);
    let truncated = raw_packet(0xBEDE, b"\x22\x31\x41\x59");
    let unchanged: Vec<Vec<u8>> = vec![
        plain.serialize(&HeaderExtensionsMap::new()),
        raw_packet(0xBEDE, b"\x90\xab\x00\x00"),
        raw_packet(0x4321, b"\x22\x31\x41\x59"),
        raw_packet(0xBEDE, b"\xf0\x22\x31\x41"),
        raw_packet(0xBEDE, b"\x01\x00\x22\x31\x41\x59\x00\x00"),
        raw_packet(0xBEDE, b""),
        b"\x80".to_vec(),
        [&b"\x90"[..], &[0u8; 11]].concat(),
        truncated[..27].to_vec(),
    ];
    for data in &unchanged {
        assert_eq!(&map.for_fec(data), data, "{data:?}");
    }
    let overrun = raw_packet(0xBEDE, b"\x22\x31\x41\x59\x3c\x00\x00\x00");
    assert_eq!(map.for_fec(&overrun), zeroed(&overrun, &[(25, 28)]));
    let orphan = raw_packet(0x1000, b"\x16\x03\x31\x41\x59\x00\x00\x16");
    assert_eq!(
        configured(true).for_fec(&orphan),
        zeroed(&orphan, &[(26, 29)])
    );
}

/// A frame the H.264 packer cuts into `packets` fragments.
fn fragmented(packets: usize) -> Bytes {
    let mut data = vec![0, 0, 0, 1, 0x65];
    data.extend((0..packets * 1198 - 100).map(|i| (i % 251) as u8 + 1));
    Bytes::from(data)
}

#[test]
fn the_real_sender_protects_what_the_receiver_recovers() {
    for fec_enabled in [true, false] {
        let mut params = parameters("video/H264", true);
        if fec_enabled {
            params
                .codecs
                .push(CodecParameters::new("video/flexfec-03", 90000, 118));
        }
        let mut s = RtpSender::new(Kind::Video, Box::new(NoHooks));
        s.start(&params).unwrap();
        s.set_ssrcs(0x1234, 2, 3);
        // Every group repaired, as a path losing packets with no queue has it.
        s.fec_full = fec_enabled;
        let mut tr = FakeTransport::new(1000.0);
        const MS: i64 = 1_000_000;
        let now_ns = (1000.0f64 * 1e9) as i64;
        let frame = EncodedFrame {
            data: fragmented(2),
            keyframe: true,
            timing: Some(FrameTiming {
                capture_ns: now_ns - 7 * MS,
                encode_start_ns: now_ns - 4 * MS,
                encode_end_ns: now_ns - 2 * MS,
            }),
            dependency: Some((5, None)),
            ..Default::default()
        };
        s.push_frame(frame, &mut tr, 1000.0);
        let map = s.header_extensions().clone();
        let media: Vec<RtpPacket> = tr
            .media(96)
            .iter()
            .map(|d| RtpPacket::parse(&Bytes::from(d.clone()), &map).unwrap())
            .collect();
        assert_eq!(media.len(), 2);
        let last = &media[1].extensions;
        assert_eq!(last.video_timing, Some((1, 3, 5, 7, 7, 0, 0)));
        assert!(last.transport_sequence_number.is_some());
        for packet in &media {
            let stored = s.history().get(packet.sequence_number).expect("kept");
            assert_eq!(
                stored.serialize(&map),
                packet.serialize(&map),
                "the history holds them as sent"
            );
        }
        let repairs = tr.media(118);
        assert_eq!(repairs.len(), fec_enabled as usize);
        if fec_enabled {
            let mut known = HashMap::new();
            known.insert(media[1].sequence_number, receiver_packet(&media[1], &map));
            let recovered = recover(&repairs, &mut known);
            let back = &recovered[&media[0].sequence_number];
            assert_eq!(back, &receiver_packet(&media[0], &map));
            let parsed = RtpPacket::parse(&Bytes::from(back.clone()), &map).unwrap();
            assert_eq!(parsed.payload.to_bytes(), media[0].payload.to_bytes());
            assert_eq!(&repairs[0][8..12], &3u32.to_be_bytes(), "on its own SSRC");
            assert_eq!(&repairs[0][24..28], &0x1234u32.to_be_bytes());
        }
    }
}

#[test]
fn a_frame_is_interleaved_into_groups_and_its_repairs_reach_the_history() {
    let mut params = parameters("video/H264", false);
    params
        .codecs
        .push(CodecParameters::new("video/flexfec-03", 90000, 118));
    let mut s = RtpSender::new(Kind::Video, Box::new(NoHooks));
    s.start(&params).unwrap();
    s.set_ssrcs(0x1234, 2, 3);
    s.fec_full = true;
    s.fec_repair_packets = 2;
    let mut tr = FakeTransport::new(1000.0);
    s.push_frame(
        EncodedFrame {
            data: fragmented(25),
            keyframe: true,
            ..Default::default()
        },
        &mut tr,
        1000.0,
    );
    let order: Vec<u8> = tr.rtp.iter().map(|d| d[1] & 0x7F).collect();
    assert_eq!(order.iter().filter(|p| **p == 96).count(), 25);
    assert_eq!(
        order.iter().filter(|p| **p == 118).count(),
        6,
        "10 + 10 + 5, two repairs each"
    );
    assert_eq!(
        &order[..12],
        &[96, 96, 96, 96, 96, 96, 96, 96, 96, 96, 118, 118],
        "repairs follow their group"
    );
    let repairs = tr.media(118);
    let masks: Vec<u16> = repairs
        .iter()
        .map(|f| u16::from_be_bytes([f[30], f[31]]) & 0x7FFF)
        .collect();
    assert_eq!(masks[..2], [0b101010101000000, 0b010101010100000]);
    let bases: Vec<u16> = repairs
        .iter()
        .map(|f| u16::from_be_bytes([f[28], f[29]]))
        .collect();
    assert_eq!(bases[2], bases[0].wrapping_add(10));
    let seqs: Vec<u16> = repairs
        .iter()
        .map(|f| u16::from_be_bytes([f[2], f[3]]))
        .collect();
    assert!(
        seqs.windows(2).all(|w| w[1] == w[0].wrapping_add(1)),
        "the repair stream numbers its own"
    );
    assert!(
        tr.rtp
            .iter()
            .filter(|d| d[1] & 0x7F == 118)
            .all(|d| d.len() <= 12 + 20 + 1300)
    );
}

#[test]
fn the_repair_density_follows_the_loss_and_the_queue() {
    let mut s = RtpSender::new(Kind::Video, Box::new(NoHooks));
    let mut steps = Vec::new();
    for loss in [
        0.0, 0.0, 0.03, 0.03, 0.03, 0.03, 0.15, 0.15, 0.15, 0.15, 0.15, 0.0, 0.0, 0.0, 0.0, 0.0,
        0.0, 0.0, 0.0, 0.0, 0.0,
    ] {
        s.steer_fec(loss, false);
        steps.push(s.fec_repair_packets);
    }
    assert!(
        steps[..2] == [1, 1]
            && steps[2..6].contains(&2)
            && steps[10] == 3
            && *steps.last().unwrap() == 1,
        "{steps:?}"
    );

    let repairs = |density: usize, groups: &[usize], full: bool| -> Vec<usize> {
        let mut s = RtpSender::new(Kind::Video, Box::new(NoHooks));
        s.fec_repair_packets = density;
        s.fec_full = full;
        groups.iter().map(|n| s.fec_repairs(*n)).collect()
    };
    for (density, total) in [(1, 10), (2, 20), (3, 30)] {
        assert_eq!(
            repairs(density, &[1; 100], false).iter().sum::<usize>(),
            total
        );
    }
    assert_eq!(repairs(2, &[1; 100], true).iter().sum::<usize>(), 100);
    assert_eq!(repairs(2, &[10; 5], false), vec![2; 5]);
    assert_eq!(repairs(2, &[10; 5], true), vec![2; 5]);
    let groups = [2, 1, 10, 1, 2, 2];
    let mixed = repairs(3, &groups, false);
    assert!(
        mixed.iter().zip(&groups).all(|(r, n)| r <= n) && mixed.iter().sum::<usize>() == 5,
        "{mixed:?}"
    );

    let repaired = |windows: &[(f64, bool)]| -> Vec<bool> {
        let mut s = RtpSender::new(Kind::Video, Box::new(NoHooks));
        windows
            .iter()
            .map(|(l, q)| {
                s.steer_fec(*l, *q);
                s.fec_full
            })
            .collect()
    };
    let clear = repaired(&[(0.01, false); 8]);
    assert!(
        !clear[..3].iter().any(|v| *v) && clear[4..].iter().all(|v| *v),
        "{clear:?}"
    );
    let pattern = [
        (0.0, true),
        (0.05, true),
        (0.05, false),
        (0.02, false),
        (0.0, false),
        (0.0, false),
    ];
    assert!(
        !repaired(&pattern.repeat(3)).iter().any(|v| *v),
        "a queue's overflow never does"
    );
    let mut windows = vec![(0.01, false); 6];
    windows.extend([(0.01, true); 2]);
    windows.extend([(0.0, false); 14]);
    let queued = repaired(&windows);
    assert!(
        queued[4..6].iter().all(|v| *v)
            && !queued[6..8].iter().any(|v| *v)
            && !queued.last().unwrap(),
        "{queued:?}"
    );
}

// ---- test_h264_skip: the sender's stand-in

fn annex_b(nals: &[&[u8]]) -> Bytes {
    let mut out = Vec::new();
    for nal in nals {
        out.extend_from_slice(h264::START_CODE);
        out.extend_from_slice(nal);
    }
    Bytes::from(out)
}

#[test]
fn a_frame_left_out_goes_as_its_stand_in() {
    use super::super::codecs::h264_skip::tests::{p_slice, pps, sps};
    let delta = annex_b(&[&p_slice(0, 2, 1, 0, 5)]);
    let mut h = Harness::new(false, Some(true), true, "video/H264");
    assert!(
        h.s.stand_in(&delta).is_none(),
        "a peer sent no key frame yet gets none"
    );
    h.s.learn_parameter_sets(&annex_b(&[&sps(2), &pps(1, 0), &[0x65, 0x88]]));
    let out = h.s.stand_in(&delta).expect("a stand-in");
    assert_eq!(h.s.stand_ins, 1);
    let mut expected = h264_skip::SkipPictures::new();
    expected.learn([sps(2).as_slice(), pps(1, 0).as_slice()]);
    let want = expected
        .stand_in([p_slice(0, 2, 1, 0, 5).as_slice()])
        .unwrap();
    let got: Vec<u8> = out
        .iter()
        .flat_map(|p| h264::depayload(&p.to_bytes()).unwrap().to_vec())
        .collect();
    assert!(
        got.ends_with(&want[0]),
        "packed as any frame, it depacks to the stand-in"
    );
    let mut vp8 = Harness::new(false, Some(true), true, "video/VP8");
    vp8.s
        .learn_parameter_sets(&annex_b(&[&sps(2), &pps(1, 0), &[0x65, 0x88]]));
    assert!(
        vp8.s.stand_in(&delta).is_none(),
        "a VP8 peer gets none: it names its pictures"
    );

    // Through the whole path: the frame's timestamp, no frame counted, the next predicting past.
    let mut h = Harness::new(false, Some(true), true, "video/H264");
    h.tr.heard_at = 1e12;
    let t = h.clock;
    h.s.push_frame(
        EncodedFrame {
            data: annex_b(&[&sps(2), &pps(1, 0), &p_slice(0, 2, 1, 0, 5)]),
            keyframe: true,
            dependency: Some((1, None)),
            ..Default::default()
        },
        &mut h.tr,
        t,
    );
    let sent = h.tr.rtp.len();
    h.behind(RESYNC_ROOM_FRAMES, 0.0);
    h.s.push_frame(
        EncodedFrame {
            data: delta.clone(),
            timestamp: 3000,
            dependency: Some((2, Some(1))),
            ..Default::default()
        },
        &mut h.tr,
        t + 0.016,
    );
    assert_eq!(h.s.stand_ins, 1);
    assert_eq!(
        h.tr.rtp.len(),
        sent + 1,
        "the stand-in goes in place of the frame"
    );
    assert!(!h.s.frame_numbers.contains(&2), "not numbered");
    assert_eq!(h.s.frame_left, None);
}

// ---- codec switch and the paced transport

#[test]
fn a_switch_sends_as_the_negotiated_codec_and_drops_what_was_coded_before() {
    let mut params = parameters("video/H264", false);
    params
        .codecs
        .push(CodecParameters::new("video/VP8", 90000, 98));
    let mut s = RtpSender::new(Kind::Video, Box::new(NoHooks));
    assert!(
        s.switch_codec("video/VP8"),
        "before the start it names the codec it starts on"
    );
    s.start(&params).unwrap();
    let mut tr = FakeTransport::new(1000.0);
    s.push_frame(
        EncodedFrame {
            data: Bytes::from_static(&[0x10, 1, 2]),
            codec: Some(Codec::Vp8),
            ..Default::default()
        },
        &mut tr,
        1000.0,
    );
    assert_eq!(tr.media(98).len(), 1);
    assert!(s.switch_codec("video/h264"));
    s.push_frame(
        EncodedFrame {
            data: Bytes::from_static(&[0x10, 1, 2]),
            codec: Some(Codec::Vp8),
            ..Default::default()
        },
        &mut tr,
        1000.1,
    );
    assert_eq!(
        tr.rtp.len(),
        1,
        "a frame coded before the switch is not this codec's to pack"
    );
    s.push_frame(
        EncodedFrame {
            data: h264_frame(),
            codec: Some(Codec::H264),
            ..Default::default()
        },
        &mut tr,
        1000.2,
    );
    assert_eq!(tr.media(96).len(), 1);
    assert!(
        !s.switch_codec("video/AV1"),
        "one the peer never took drops the frames"
    );
    s.push_frame(
        EncodedFrame {
            data: h264_frame(),
            ..Default::default()
        },
        &mut tr,
        1000.3,
    );
    assert_eq!(tr.rtp.len(), 2);
    let mut only_aux = RtpSender::new(Kind::Video, Box::new(NoHooks));
    let aux = SendParameters {
        codecs: vec![CodecParameters::new("video/rtx", 90000, 97)],
        ..Default::default()
    };
    assert!(
        only_aux.start(&aux).is_err(),
        "auxiliary entries alone send nothing"
    );
}

#[test]
fn audio_goes_as_it_is_and_red_stamps_its_blocks() {
    let mut params = SendParameters {
        codecs: vec![CodecParameters::new("audio/opus", 48000, 96)],
        header_extensions: super::super::codecs::default_header_extensions("audio"),
        mux_id: Some("1".into()),
        cname: None,
    };
    let mut s = RtpSender::new(Kind::Audio, Box::new(NoHooks));
    s.start(&params).unwrap();
    let mut tr = FakeTransport::new(1000.0);
    s.push_frame(
        EncodedFrame {
            data: Bytes::from_static(b"opus"),
            timestamp: 960,
            ..Default::default()
        },
        &mut tr,
        1000.0,
    );
    let map = s.header_extensions().clone();
    let p = RtpPacket::parse(&Bytes::from(tr.rtp[0].clone()), &map).unwrap();
    assert_eq!(p.payload.to_bytes(), Bytes::from_static(b"opus"));
    assert!(p.marker, "the frame's only packet");
    assert_eq!(p.extensions.mid.as_deref(), Some("1"));
    assert!(p.extensions.video_timing.is_none());
    let mut red = CodecParameters::new("audio/red", 48000, 63);
    red.parameters.insert("96/96".into(), String::new());
    params.codecs = vec![red];
    let mut s = RtpSender::new(Kind::Audio, Box::new(NoHooks));
    s.set_red_distance(Some(1));
    s.start(&params).unwrap();
    let mut tr = FakeTransport::new(1000.0);
    for i in 0..2 {
        s.push_frame(
            EncodedFrame {
                data: Bytes::from_static(b"opus"),
                timestamp: 960 * i,
                ..Default::default()
            },
            &mut tr,
            1000.0,
        );
    }
    let p = RtpPacket::parse(
        &Bytes::from(tr.rtp[1].clone()),
        &s.header_extensions().clone(),
    )
    .unwrap();
    assert_eq!(
        p.payload.to_bytes()[0],
        0x80 | 96,
        "a redundant block behind the F bit"
    );
}

#[derive(Default)]
struct Sink {
    rtp: Vec<Vec<u8>>,
    rtcp: Vec<Vec<u8>>,
    fail: bool,
}

impl super::super::transport::PacketSink for Sink {
    fn send_data(&mut self, _packet: &[u8]) -> Result<(), super::super::transport::SinkError> {
        Ok(())
    }

    fn send_rtp(&mut self, packet: &[u8]) -> Result<(), super::super::transport::SinkError> {
        if self.fail {
            return Err(super::super::transport::SinkError("wire gone".into()));
        }
        self.rtp.push(packet.to_vec());
        Ok(())
    }

    fn send_rtcp(&mut self, compound: &[u8]) -> Result<(), super::super::transport::SinkError> {
        self.rtcp.push(compound.to_vec());
        Ok(())
    }

    fn peer_heard_at(&self) -> f64 {
        1e12
    }
}

#[test]
fn the_paced_transport_lets_a_frame_out_at_the_pace_and_reports_it_on_the_wire() {
    use super::super::transport::PacedTransport;
    let rec = Recorder::default();
    rec.0.lock().unwrap().log_sent = true;
    let mut s = RtpSender::new(Kind::Video, Box::new(rec.clone()));
    s.start(&parameters("video/H264", true)).unwrap();
    let mut now = 1000.0;
    let mut tr = PacedTransport::new(Sink::default(), Some(8_000_000), now);
    let timing = FrameTiming {
        capture_ns: 77,
        encode_start_ns: 78,
        encode_end_ns: 79,
    };
    now += 0.01;
    s.push_frame(
        EncodedFrame {
            data: fragmented(30),
            keyframe: true,
            timing: Some(timing),
            dependency: Some((1, None)),
            ..Default::default()
        },
        &mut tr,
        now,
    );
    let straight = tr.sink().rtp.len();
    assert!(
        straight > 0 && straight < 30,
        "{straight} of 30 within the burst"
    );
    assert!(
        !rec.events().iter().any(|e| e.starts_with("sent")),
        "not on the wire yet"
    );
    let mut guard = 0;
    while let Some(wait) = tr.drain(now) {
        let events = tr.pacer().unwrap().take_events();
        s.apply_pacer_events(events, &mut tr.twcc);
        now += wait;
        guard += 1;
        assert!(guard < 1000);
    }
    let events = tr.pacer().unwrap().take_events();
    s.apply_pacer_events(events, &mut tr.twcc);
    assert_eq!(tr.sink().rtp.len(), 30);
    assert_eq!(
        rec.events(),
        vec![
            "out 1 true".to_string(),
            format!("sent 77 {}", s.octet_count())
        ]
    );
    let seqs: Vec<u16> = tr
        .sink()
        .rtp
        .iter()
        .map(|d| u16::from_be_bytes([d[2], d[3]]))
        .collect();
    assert!(
        seqs.windows(2).all(|w| w[1] == w[0].wrapping_add(1)),
        "in order"
    );
    s.send_report(&mut tr, now);
    assert_eq!(tr.sink().rtcp.len(), 1);
    tr.sink_mut().fail = true;
    now += 1.0;
    s.push_frame(
        EncodedFrame {
            data: fragmented(30),
            timestamp: 3000,
            dependency: Some((2, Some(1))),
            ..Default::default()
        },
        &mut tr,
        now,
    );
    tr.drain(now + 0.1);
    assert!(
        tr.pacer().unwrap().gop_dead(),
        "a write that fails abandons the GOP"
    );
    assert_eq!(tr.pacer().unwrap().bytes_queued(), 0);
}

// ---- test_twcc_interval_delivery: the route, and test_webrtc_frame_end: nothing pacing

#[test]
fn routed_feedback_reaches_the_aggregate_measurement() {
    use super::super::router::RtpRouter;
    use super::super::rtcp::pack_twcc_fci;
    let mut s = RtpSender::new(Kind::Video, Box::new(NoHooks));
    s.start(&parameters("video/H264", false)).unwrap();
    let mut router = RtpRouter::new();
    router.register_sender("video", s.ssrc(), true);
    router.register_sender("audio", 5, false);
    let mut tr = FakeTransport::new(10.0);
    for i in 0..400u16 {
        tr.twcc.seed_sent(i, 1000, 10.0 + i as f64 * 0.00125);
    }
    let times: Vec<Option<f64>> = (0..400)
        .map(|i| Some(1000.0 + (i / 20) as f64 * 25.0 + (i % 20) as f64 * 0.25))
        .collect();
    for i in (0..400).step_by(20) {
        let fci = pack_twcc_fci(i as u16, &times[i..i + 20], 0);
        let wire = RtcpPacket::Rtpfb(RtpfbPacket::twcc(1, 0, fci)).serialize();
        for packet in RtcpPacket::parse(&wire).unwrap() {
            assert_eq!(router.route_rtcp(&packet).senders, vec!["video"]);
            s.handle_rtcp(&packet, &mut tr, 11.0);
        }
    }
    let window = tr.twcc.take_window(11.0).unwrap();
    assert_eq!((window.received, window.goodput_bps), (400, 6_653_465));
}

#[test]
fn a_transport_that_paces_nothing_reports_the_frame_once_written() {
    use super::super::transport::PacedTransport;
    let rec = Recorder::default();
    rec.0.lock().unwrap().log_sent = true;
    let mut s = RtpSender::new(Kind::Video, Box::new(rec.clone()));
    s.start(&parameters("video/H264", true)).unwrap();
    let mut tr = PacedTransport::new(Sink::default(), None, 1000.0);
    let frame = EncodedFrame {
        data: fragmented(3),
        keyframe: true,
        timing: Some(FrameTiming {
            capture_ns: 77,
            encode_start_ns: 78,
            encode_end_ns: 79,
        }),
        dependency: Some((1, None)),
        ..Default::default()
    };
    s.push_frame(frame, &mut tr, 1000.0);
    assert_eq!(tr.sink().rtp.len(), 3);
    assert_eq!(
        rec.events(),
        vec![
            "out 1 true".to_string(),
            format!("sent 77 {}", s.octet_count())
        ]
    );
    assert!(s.frame_ends.is_empty());
}

// ---- what a peer can send

/// Random and mutated bytes against everything that parses what a peer sends: the crate's
/// fuzz targets (`crates/selkies-rtc/fuzz`) run these same paths under coverage, this keeps
/// them from panicking in the unit tier.
#[test]
fn nothing_a_peer_sends_panics() {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 32) as u32
    };
    let map = configured(false);
    let mut h = Harness::selective();
    h.s.start(&parameters("video/H264", true)).unwrap();
    let mut rx = super::super::receiver::RtpReceiver::new("video");
    rx.start(&super::super::params::ReceiveParameters {
        codecs: [
            "video/H264",
            "video/H265",
            "video/VP8",
            "video/VP9",
            "video/AV1",
        ]
        .iter()
        .enumerate()
        .map(|(i, mime)| CodecParameters::new(mime, 90000, 96 + i as u8))
        .collect(),
        ..Default::default()
    });
    let mut twcc = super::super::twcc::TwccReceiver::new();
    let seeds: Vec<Vec<u8>> = vec![
        raw_packet(0xBEDE, b"\x00\x22\x31\x41\x59\x71\x12\x34\x90\xab\x00\x00"),
        raw_packet(0x1000, b"\x16\x03\x31\x41\x59\x00\x00\x16"),
        RtcpPacket::Rtpfb(RtpfbPacket::nack(1, 2, vec![1, 2, 40])).serialize(),
        RtcpPacket::Rtpfb(RtpfbPacket::twcc(
            1,
            0,
            crate::rtp::rtcp::pack_twcc_fci(0, &[Some(1.0), None, Some(9.0)], 0),
        ))
        .serialize(),
        RtcpPacket::Xr(XrPacket {
            ssrc: 1,
            rrtr: Some(9),
            dlrr: vec![(1, 2, 3)],
        })
        .serialize(),
        RtcpPacket::Sdes {
            chunks: vec![crate::rtp::rtcp::SourceInfo {
                ssrc: 1,
                items: vec![(1, b"x".to_vec())],
            }],
        }
        .serialize(),
    ];
    let mut codec = 0;
    for round in 0..40_000u32 {
        let mut data = seeds[round as usize % seeds.len()].clone();
        if round % 3 == 0 {
            data = (0..next() % 80).map(|_| next() as u8).collect();
        } else {
            for _ in 0..1 + next() % 4 {
                let at = next() as usize % data.len().max(1);
                if let Some(b) = data.get_mut(at) {
                    *b = next() as u8;
                }
            }
            data.truncate(next() as usize % (data.len() + 1));
        }
        let t = 1000.0 + round as f64 * 0.001;
        if let Ok(packets) = RtcpPacket::parse(&data) {
            for packet in &packets {
                h.s.handle_rtcp(packet, &mut h.tr, t);
                rx.handle_rtcp(packet, t);
            }
        }
        h.tr.twcc.process_feedback(&data, t, 12_500.0);
        let bytes = Bytes::from(data);
        if let Ok(mut packet) = RtpPacket::parse(&bytes, &map) {
            codec = (codec + 1) % 5;
            packet.payload_type = 96 + codec;
            rx.handle_rtp(packet, round as i64, t, &mut twcc);
        }
        for kind in [Codec::H264, Codec::H265, Codec::Vp8, Codec::Vp9, Codec::Av1] {
            let _ = codecs::depayload(kind, &bytes);
        }
        let _ = codecs::assemble(Codec::Av1, std::slice::from_ref(&bytes));
        let mut skips = h264_skip::SkipPictures::new();
        skips.learn([&bytes[..]]);
        let _ = skips.stand_in([&bytes[..]]);
        let mut packer =
            Packetizer::for_codec(&CodecParameters::new("video/AV1", 90000, 96), None).unwrap();
        let _ = packer.pack(&bytes, 0);
        for mime in ["video/H264", "video/H265", "video/VP9"] {
            let mut packer =
                Packetizer::for_codec(&CodecParameters::new(mime, 90000, 96), None).unwrap();
            let _ = packer.pack(&bytes, 0);
        }
    }
}

// ---- the figure the port exists for

/// A sink that encrypts each datagram under SRTP and, where it has a socket, writes it there:
/// what `Transport::send_rtp` does for each packet.
struct BenchSink {
    srtp: Option<crate::srtp::SrtpSession>,
    socket: Option<std::net::UdpSocket>,
}

impl super::super::transport::PacketSink for BenchSink {
    fn send_rtp(&mut self, packet: &[u8]) -> Result<(), super::super::transport::SinkError> {
        if let Some(srtp) = self.srtp.as_mut() {
            let protected = srtp.tx.protect_rtp(packet).expect("protects");
            if let Some(socket) = &self.socket {
                // A full socket buffer is the wire's loss, not the benchmark's concern.
                let _ = socket.send(&protected);
            }
        }
        Ok(())
    }

    fn send_rtcp(&mut self, _compound: &[u8]) -> Result<(), super::super::transport::SinkError> {
        Ok(())
    }

    fn send_data(&mut self, _packet: &[u8]) -> Result<(), super::super::transport::SinkError> {
        Ok(())
    }

    fn peer_heard_at(&self) -> f64 {
        1e12
    }
}

/// One row of the benchmark: frames through the real sender and `PacedTransport`, timed from
/// `push_frame` to its return. The pacer's rate covers the frame, so every packet takes the
/// fast path a stream under its pace takes.
fn bench_row(label: &str, density: usize, pacer: bool, sink: BenchSink) {
    use super::super::transport::PacedTransport;
    let mut frame = vec![0, 0, 0, 1, 0x41];
    // No start code in it: one NAL, cut into FU-A fragments, as a real slice is.
    frame.extend((0..150_000u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8 | 1));
    let frame = Bytes::from(frame);
    let mut params = SendParameters {
        codecs: codecs::default_video_codecs(),
        header_extensions: codecs::default_header_extensions("video"),
        mux_id: Some("0".into()),
        cname: Some("bench".into()),
    };
    params
        .codecs
        .retain(|c| (density > 0 && c.is_flexfec()) || c.mime_lower() == "video/h264");
    params.codecs.truncate(if density > 0 { 4 } else { 3 });
    let mut s = RtpSender::new(Kind::Video, Box::new(NoHooks));
    s.start(&params).unwrap();
    s.fec_full = density > 0;
    s.fec_repair_packets = density.max(1);
    let mut now = 1000.0;
    let bps = pacer.then_some(2_000_000_000);
    let mut tr = PacedTransport::new(sink, bps, now);
    const FRAMES: u32 = 2000;
    let mut samples = Vec::with_capacity(FRAMES as usize);
    for i in 0..FRAMES {
        let encoded = EncodedFrame {
            data: frame.clone(),
            timestamp: i * 1500,
            keyframe: i == 0,
            timing: Some(FrameTiming {
                capture_ns: 1,
                encode_start_ns: 2,
                encode_end_ns: 3,
            }),
            dependency: Some((i as u16, (i > 0).then(|| (i - 1) as u16))),
            ..Default::default()
        };
        now += 1.0 / 60.0;
        let started = std::time::Instant::now();
        s.push_frame(encoded, &mut tr, now);
        samples.push(started.elapsed().as_nanos() as f64 / 1e3);
        let queued = tr.pacer().map_or(0, |p| p.bytes_queued());
        assert_eq!(queued, 0, "the rate covers the frame: nothing queues");
    }
    samples.sort_by(|a, b| a.total_cmp(b));
    let media = s.packet_count() as f64 / FRAMES as f64;
    let datagrams = media
        + if density > 0 {
            (media / 10.0).ceil() * density as f64
        } else {
            0.0
        };
    let median = samples[samples.len() / 2];
    println!(
        "{label:>42}: {datagrams:>4.0} datagrams, frame median {median:>7.1} us, p99 {:>7.1} us, {:>5.2} us/datagram",
        samples[samples.len() * 99 / 100],
        median / datagrams
    );
}

/// A 150 KB H.264 frame into RTP with the dependency descriptor, every negotiated extension,
/// and FlexFEC repairs, then with SRTP and a socket behind it. Prints figures to quote: `cargo
/// test --release rtp_bench -- --ignored --nocapture`.
#[test]
#[ignore = "a measurement, in a release build"]
fn rtp_bench_150kb_frame() {
    use crate::srtp::{SrtpProfile, SrtpSession};
    let srtp = || {
        let profile = SrtpProfile::AeadAes128Gcm;
        let material = vec![7u8; profile.keying_material_len()];
        Some(SrtpSession::from_keying_material(profile, &material, false).expect("keys"))
    };
    let nothing = || BenchSink {
        srtp: None,
        socket: None,
    };
    bench_row("no FEC, no SRTP, no pacer", 0, false, nothing());
    for density in 1..4 {
        bench_row(
            &format!("{density} repair(s) per group, no SRTP, no pacer"),
            density,
            false,
            nothing(),
        );
    }
    bench_row("2 repairs, pacer fast path, no SRTP", 2, true, nothing());
    bench_row(
        "2 repairs, pacer, AES-128-GCM",
        2,
        true,
        BenchSink {
            srtp: srtp(),
            socket: None,
        },
    );
    let drain = std::net::UdpSocket::bind("127.0.0.1:0").expect("binds");
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("binds");
    socket
        .connect(drain.local_addr().unwrap())
        .expect("connects");
    socket.set_nonblocking(true).expect("nonblocking");
    bench_row(
        "2 repairs, pacer, AES-128-GCM, UDP loopback",
        2,
        true,
        BenchSink {
            srtp: srtp(),
            socket: Some(socket),
        },
    );
}

/// A key frame of `size` bytes whose last packet left under `tag` now, unacknowledged unless
/// `acked`.
fn key_on_wire(h: &mut Harness, tag: u16, size: usize, acked: bool) {
    if !acked {
        let at = h.clock;
        h.tr.twcc.seed_sent(tag, 1200, at);
    }
    h.s.note_frame_end(
        tag,
        FrameEnd {
            keyframe: true,
            timing: None,
            captured_ns: 0,
            size,
            frame_id: None,
        },
    );
    h.s.frame_left(tag, h.clock);
}

fn plis(h: &Harness) -> usize {
    h.rec.events().iter().filter(|e| *e == "pli").count()
}

fn pli(h: &mut Harness) {
    let packet = RtcpPacket::Psfb(PsfbPacket {
        fmt: RTCP_PSFB_PLI,
        ssrc: 1,
        media_ssrc: 2,
        fci: vec![],
    });
    h.s.handle_rtcp(&packet, &mut h.tr, h.clock);
}

#[test]
fn repairs_and_key_frame_requests_wait_on_a_full_path() {
    const GONE: u16 = 9999;
    let mut h = Harness::new(true, None, true, "video/H264");
    h.s.history = RtpHistory::new();
    for seq in 1..=20u16 {
        let mut p = RtpPacket::new(96, seq, 0, 0);
        p.payload = Payload::from(&[b'x'; 1000][..]);
        h.s.history.add(p, 0.0, Some(10 + (seq - 1) / 5));
    }
    h.s.rtt = Some(0.1);
    let now = h.clock;

    assert!(
        h.s.repair_room(10_000_000, false, now),
        "unmeasured, a repair of any size fits"
    );
    for _ in 0..3 {
        h.nack(&[GONE]);
    }
    assert_eq!(
        plis(&h),
        3,
        "unmeasured, each NACK naming a packet gone asks for a key frame, as before"
    );

    h.s.set_repair_budget(Some(800_000.0), Some(1_000_000.0));
    let room = 800_000.0 * REPAIR_SHARE / 8.0 * REPAIR_WINDOW_S;
    assert!(
        h.s.repair_room(room as usize, false, now) && !h.s.repair_room(1, false, now),
        "measured, the budget holds REPAIR_WINDOW_S of REPAIR_SHARE of the steered rate"
    );
    h.s.repair_at -= 0.1;
    let tick = room * 0.1 / REPAIR_WINDOW_S;
    assert!(
        h.s.repair_room((tick * 0.9) as usize, false, now)
            && !h.s.repair_room((tick * 0.2) as usize, false, now),
        "and refills at that rate"
    );

    h.tr.rtp.clear();
    h.s.repair_bytes = 4500.0;
    h.s.repair_at = now;
    h.nack(&[1, 2, 3, 4]);
    assert_eq!(
        h.tr.seqs(),
        vec![1, 1, 2, 2],
        "a NACK's retransmissions stop at the budget, a first repair counting twice (it is sent twice)"
    );
    h.s.repair_bytes = room;
    h.nack(&[3, 4]);
    assert_eq!(
        h.tr.seqs()[4..],
        [3, 4, 4],
        "and resume once it has room again; a packet the budget refused counted its NACK, so the next sends it once"
    );
    assert!(
        h.rec.events().contains(&"lost_frame 10".to_string()),
        "and names its frame lost"
    );

    h.rec.clear();
    h.s.repair_bytes = room;
    h.s.key_requested = None;
    h.nack(&[GONE]);
    h.nack(&[GONE]);
    assert_eq!(
        plis(&h),
        1,
        "measured, a NACK naming a packet gone asks once, and the next waits on the key frame asked"
    );
    h.s.key_requested = h.s.key_requested.map(|t| t - KEY_ASK_S - 0.001);
    h.nack(&[GONE]);
    assert_eq!(
        plis(&h),
        2,
        "past KEY_ASK_S with no key frame out, it asks again"
    );

    key_on_wire(&mut h, 40, 70_000, false);
    h.nack(&[GONE]);
    assert_eq!(
        plis(&h),
        2,
        "a key frame on the wire answers it: no request"
    );
    let travel = 0.1 + KEY_MARGIN_S + 70_000.0 * 8.0 / 1_000_000.0;
    h.s.key_wire = Some((40, now - travel + 0.05, 70_000));
    h.nack(&[GONE]);
    assert_eq!(
        plis(&h),
        2,
        "until its bytes' time at what the path delivers, the round trip and KEY_MARGIN_S"
    );
    h.s.key_wire = Some((40, now - travel - 0.05, 70_000));
    h.nack(&[GONE]);
    assert_eq!(plis(&h), 3, "past that it asks again");

    key_on_wire(&mut h, 41, 70_000, true);
    h.nack(&[GONE]);
    assert_eq!(
        plis(&h),
        4,
        "a key frame the peer acknowledged answers nothing new: it asks"
    );

    key_on_wire(&mut h, 42, 70_000, false);
    pli(&mut h);
    assert_eq!(
        plis(&h),
        4,
        "a peer's PLI waits on a key frame on its way too (it repeats while one crosses a slow path)"
    );

    key_on_wire(&mut h, 43, 70_000, false);
    h.s.pacer_resync(None, &mut h.tr, now);
    h.nack(&[GONE]);
    assert!(
        plis(&h) == 5 && h.s.key_wire.is_none(),
        "a pacer reset drops the key frame on its way: the next request asks"
    );
    key_on_wire(&mut h, 44, 70_000, false);
    h.s.delivery_bps = None;
    pli(&mut h);
    assert_eq!(
        plis(&h),
        6,
        "unmeasured, a peer's PLI is answered whatever is on its way, as before"
    );

    h.s.set_repair_budget(Some(800_000.0), Some(1_000_000.0));
    h.s.repair_bytes = room * (FEC_RESERVE + 0.01);
    h.s.repair_at = now;
    assert!(
        !h.s.repair_room((room * 0.02) as usize, true, now)
            && h.s.repair_room((room * 0.02) as usize, false, now),
        "FlexFEC leaves FEC_RESERVE of the budget to retransmissions"
    );

    h.s.set_repair_budget(None, None);
    assert!(
        h.s.repair_room(10_000_000, false, now),
        "a budget lifted lets any repair through again"
    );
}
