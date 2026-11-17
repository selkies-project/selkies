/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What the WebRTC transport does for every frame, packet and connectivity check, timed for
//! CodSpeed: video packetized into RTP, a packet serialized and parsed with the header
//! extensions Selkies negotiates, a peer's compound RTCP parsed, FlexFEC repairs built, SRTP
//! protecting and unprotecting a packet on the AWS-LC provider, a session description parsed
//! and printed, and a STUN binding check built and read. Every input is fixed (no random
//! transaction id, picture id or payload), so each run measures the same path, and every path
//! is checked to succeed once before it is timed: a bench that measured an error's early return
//! would read as a speedup.
//!
//! `cargo bench -p selkies-rtc` runs them under Criterion locally; CI runs them under CodSpeed's
//! instruction counting (`cargo codspeed`), which keys its history on the names below.

use std::hint::black_box;

use bytes::{Bytes, BytesMut};
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use selkies_rtc::ice::stun;
use selkies_rtc::rtp::codecs::{default_header_extensions, h264, vp8};
use selkies_rtc::rtp::fec::{RepairHeader, build_flexfec_03};
use selkies_rtc::rtp::{HeaderExtensionsMap, RtcpPacket, RtpPacket};
use selkies_rtc::sdp::SessionDescription;
use selkies_rtc::srtp::{SrtpProfile, SrtpSession};

/// A deterministic byte pattern standing in for coded video.
fn pattern(len: usize, seed: u8) -> Vec<u8> {
    let mut state = u32::from(seed) | 0x1234_5600;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state.to_le_bytes()[0]
        })
        .collect()
}

/// A NAL unit of `len` bytes with the header byte `header` (no start code).
fn nal(header: u8, len: usize, seed: u8) -> Bytes {
    let mut out = pattern(len, seed);
    out[0] = header;
    Bytes::from(out)
}

fn video_map() -> HeaderExtensionsMap {
    let mut map = HeaderExtensionsMap::new();
    map.configure(&default_header_extensions("video"));
    map
}

/// A full video packet with the extensions every packet of a stream carries.
fn video_packet() -> RtpPacket {
    let mut packet = RtpPacket::new(102, 4321, 90_000, 0x1111_1111);
    packet.marker = true;
    packet.extensions.mid = Some("0".into());
    packet.extensions.abs_send_time = Some(0x0012_3456);
    packet.extensions.transport_sequence_number = Some(777);
    packet.payload = Bytes::from(pattern(1180, 1)).into();
    packet
}

fn rtp(c: &mut Criterion) {
    let map = video_map();
    let packet = video_packet();
    let wire = Bytes::from(packet.serialize(&map));
    assert!(RtpPacket::parse(&wire, &map).is_ok());
    c.bench_function("rtp_serialize_video_packet", |b| {
        b.iter(|| black_box(&packet).serialize(black_box(&map)));
    });
    c.bench_function("rtp_parse_video_packet", |b| {
        b.iter(|| RtpPacket::parse(black_box(&wire), black_box(&map)));
    });
}

fn rtcp(c: &mut Criterion) {
    // The fuzz corpus's compound: what a browser sends back for a stream (SR, RR, SDES and
    // feedback), datagram length-prefixed as the rtp target reads it.
    let seed = include_bytes!("../fuzz/seeds/rtp/rtcp-compound");
    let len = usize::from(u16::from_be_bytes([seed[0], seed[1]]));
    let compound = &seed[2..(2 + len).min(seed.len())];
    assert!(RtcpPacket::parse(compound).is_ok_and(|packets| !packets.is_empty()));
    c.bench_function("rtcp_parse_compound", |b| {
        b.iter(|| RtcpPacket::parse(black_box(compound)));
    });
}

fn h264_packetize(c: &mut Criterion) {
    // An IDR access unit: SPS, PPS and a 120 KB slice, which goes out as FU-A fragments.
    let keyframe = vec![nal(0x67, 24, 2), nal(0x68, 6, 3), nal(0x65, 120_000, 4)];
    let delta = vec![nal(0x41, 4_000, 5)];
    // Eight small slices, which aggregate into STAP-A packets.
    let small: Vec<Bytes> = (0..8).map(|i| nal(0x41, 300, 10 + i)).collect();
    for (name, nals) in [
        ("h264_packetize_keyframe_120k", &keyframe),
        ("h264_packetize_delta_4k", &delta),
        ("h264_packetize_stap_a_8x300", &small),
    ] {
        assert!(!h264::packetize(nals).is_empty());
        c.bench_function(name, |b| b.iter(|| h264::packetize(black_box(nals))));
    }
    let fragments: Vec<Bytes> = h264::packetize(&keyframe)
        .into_iter()
        .map(|payload| Bytes::from(payload.pieces().concat()))
        .collect();
    assert!(fragments.iter().all(|f| h264::depayload(f).is_ok()));
    c.bench_function("h264_depayload_keyframe_120k", |b| {
        b.iter(|| {
            for fragment in &fragments {
                let _ = black_box(h264::depayload(black_box(fragment)));
            }
        });
    });
}

fn vp8_packetize(c: &mut Criterion) {
    let frame = Bytes::from(pattern(60_000, 6));
    // A picture id past 127 takes the two-byte form, as every stream's soon does.
    assert!(!vp8::packetize(&frame, 1000).is_empty());
    c.bench_function("vp8_packetize_60k", |b| {
        b.iter(|| vp8::packetize(black_box(&frame), black_box(1000)));
    });
}

fn flexfec(c: &mut Criterion) {
    let map = video_map();
    let media: Vec<Vec<u8>> = (0..10u16)
        .map(|i| {
            let mut packet = video_packet();
            packet.sequence_number = 5000 + i;
            packet.payload = Bytes::from(pattern(1180, 20 + i as u8)).into();
            packet.serialize(&map)
        })
        .collect();
    let header = RepairHeader {
        first_sequence_number: 5000,
        protected_ssrc: 0x1111_1111,
        payload_type: 118,
        sequence_number: 1,
        timestamp: 90_000,
        ssrc: 0x4444_4444,
    };
    assert!(!build_flexfec_03(&media, &header, &[]).is_empty());
    c.bench_function("flexfec_repair_over_10", |b| {
        b.iter(|| build_flexfec_03(black_box(&media), black_box(&header), &[]));
    });
}

fn srtp(c: &mut Criterion) {
    let profile = SrtpProfile::AeadAes128Gcm;
    let material: Vec<u8> = (0..profile.keying_material_len())
        .map(|i| i as u8)
        .collect();
    let plain = video_packet().serialize(&video_map());
    let mut sender = SrtpSession::from_keying_material(profile, &material, false).unwrap();
    let protected = sender.tx.protect_rtp(&plain).unwrap().freeze();
    {
        let mut check = SrtpSession::from_keying_material(profile, &material, true).unwrap();
        assert_eq!(
            check.rx.unprotect_rtp(&protected).unwrap().as_ref(),
            &plain[..]
        );
    }
    c.bench_function("srtp_protect_in_place_aes128gcm", |b| {
        b.iter_batched(
            || {
                let mut packet = BytesMut::with_capacity(plain.len() + 16);
                packet.extend_from_slice(&plain);
                packet
            },
            |mut packet| {
                sender.tx.protect_rtp_in_place(&mut packet).unwrap();
                packet
            },
            BatchSize::SmallInput,
        );
    });
    // A fresh receiving context for each packet: one context's replay window refuses the same
    // packet twice, which would time the refusal.
    c.bench_function("srtp_unprotect_aes128gcm", |b| {
        b.iter_batched(
            || SrtpSession::from_keying_material(profile, &material, true).unwrap(),
            |mut receiver| receiver.rx.unprotect_rtp(black_box(&protected)).unwrap(),
            BatchSize::SmallInput,
        );
    });
}

fn sdp(c: &mut Criterion) {
    let answer = include_str!("../fuzz/seeds/sdp/answer.sdp");
    let parsed = SessionDescription::parse(answer).unwrap();
    c.bench_function("sdp_parse_browser_answer", |b| {
        b.iter(|| SessionDescription::parse(black_box(answer)));
    });
    c.bench_function("sdp_print_browser_answer", |b| {
        b.iter(|| black_box(&parsed).to_sdp());
    });
}

fn stun_checks(c: &mut Criterion) {
    let (local, remote, pwd) = ("selkiesu", "peerfrag", "peerpassword0123456789ab");
    let role = stun::Role::Controlling(0x0102_0304_0506_0708);
    let request = stun::binding_request(remote, local, 1_853_817_087, role, true, pwd).unwrap();
    let wire = request.raw;
    assert!(stun::parse(&wire).is_ok());
    assert!(stun::verify_integrity(
        &mut stun::parse(&wire).unwrap(),
        pwd
    ));
    c.bench_function("stun_binding_request_build", |b| {
        b.iter(|| stun::binding_request(remote, local, 1_853_817_087, role, true, pwd));
    });
    c.bench_function("stun_parse_binding_request", |b| {
        b.iter(|| stun::parse(black_box(&wire)));
    });
    c.bench_function("stun_parse_and_verify_binding_request", |b| {
        b.iter(|| {
            let mut message = stun::parse(black_box(&wire)).unwrap();
            stun::verify_integrity(&mut message, pwd)
        });
    });
}

criterion_group!(
    benches,
    rtp,
    rtcp,
    h264_packetize,
    vp8_packetize,
    flexfec,
    srtp,
    sdp,
    stun_checks
);
criterion_main!(benches);
