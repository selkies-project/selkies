/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The media a peer sends once SRTP has decrypted it, through what the peer task does with
//! each datagram (`on_incoming` in `crates/selkies/src/webrtc/peer.rs`). One the transport
//! takes for RTCP (`srtp::is_rtcp`) is parsed as a compound (`RtcpPacket::parse`), routed, and
//! handed to the uplink receivers, its transport-cc feedback joined against a send history
//! (`TwccSender::process_feedback`) and its REMB read (`unpack_remb_fci`). Any other is parsed
//! as RTP with every header extension Selkies negotiates (`RtpPacket::parse`,
//! `HeaderExtensionsMap::get`) and routed to the webcam or the microphone receiver, which
//! answers transport-cc, unwraps RTX, generates NACKs, depayloads each video codec, and
//! reassembles frames (`RtpReceiver::handle_rtp`). The webcam is SSRC `0x11111111` with its
//! RTX on `0x22222222` and the microphone `0x33333333`, on the payload types of
//! `default_video_codecs` and `default_audio_codecs`; another SSRC is learned from its payload
//! type as the router learns one.
//!
//! Nothing may panic, a parsed packet's payload lies inside its datagram, and every RTCP packet
//! the receivers answer with parses back. The input is a run of datagrams (`datagrams`).
#![no_main]

mod datagrams;

use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use selkies_rtc::rtp::codecs::{
    default_audio_codecs, default_header_extensions, default_video_codecs,
};
use selkies_rtc::rtp::params::{CodecParameters, ReceiveEncoding, ReceiveParameters};
use selkies_rtc::rtp::rtcp::{
    PsfbPacket, RTCP_PSFB_APP, RTCP_RTPFB_TWCC, RtpfbPacket, unpack_remb_fci,
};
use selkies_rtc::rtp::{
    HeaderExtensionsMap, RtcpPacket, RtpPacket, RtpReceiver, RtpRouter, TwccReceiver, TwccSender,
};
use selkies_rtc::srtp::is_rtcp;

const WEBCAM_SSRC: u32 = 0x1111_1111;
const WEBCAM_RTX_SSRC: u32 = 0x2222_2222;
const MICROPHONE_SSRC: u32 = 0x3333_3333;
const VIDEO_SSRC: u32 = 0x4444_4444;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Track {
    Video,
    Microphone,
    Webcam,
}

/// A started receiver of `kind` and the payload types it takes.
fn receiver(
    kind: &str,
    codecs: Vec<CodecParameters>,
    encoding: ReceiveEncoding,
) -> (RtpReceiver, Vec<u8>) {
    let payload_types = codecs.iter().map(|c| c.payload_type).collect();
    let mut receiver = RtpReceiver::new(kind);
    receiver.start(&ReceiveParameters {
        codecs,
        header_extensions: default_header_extensions(kind),
        encodings: vec![encoding],
        mux_id: None,
    });
    receiver.set_rtcp_ssrc(VIDEO_SSRC);
    (receiver, payload_types)
}

/// Every RTCP packet `receiver` has queued parses back.
fn drain(receiver: &mut RtpReceiver) {
    for compound in receiver.take_rtcp() {
        assert!(
            RtcpPacket::parse(&compound).is_ok(),
            "an RTCP answer that does not parse: {compound:02x?}"
        );
    }
}

fuzz_target!(|data: &[u8]| {
    let (mut webcam, webcam_pts) = receiver(
        "video",
        default_video_codecs(),
        ReceiveEncoding {
            ssrc: WEBCAM_SSRC,
            rtx_ssrc: Some(WEBCAM_RTX_SSRC),
        },
    );
    let (mut microphone, microphone_pts) = receiver(
        "audio",
        default_audio_codecs(),
        ReceiveEncoding {
            ssrc: MICROPHONE_SSRC,
            rtx_ssrc: None,
        },
    );
    let mut router = RtpRouter::new();
    router.register_receiver(Track::Webcam, &[WEBCAM_SSRC, WEBCAM_RTX_SSRC], &webcam_pts);
    router.register_receiver(Track::Microphone, &[MICROPHONE_SSRC], &microphone_pts);
    router.register_sender(Track::Video, VIDEO_SSRC, true);
    let mut map = HeaderExtensionsMap::new();
    map.configure(
        &[
            default_header_extensions("video"),
            default_header_extensions("audio"),
        ]
        .concat(),
    );
    let mut twcc_rx = TwccReceiver::new();
    let mut twcc_tx = TwccSender::new(0.0);
    for i in 0..512 {
        twcc_tx.next(1200, i as f64 * 0.001);
    }
    let mut now = 1.0;
    for datagram in datagrams::split(data) {
        now += 0.005;
        if is_rtcp(datagram) {
            let Ok(packets) = RtcpPacket::parse(datagram) else {
                continue;
            };
            for packet in &packets {
                let to = router.route_rtcp(packet);
                for track in to.receivers {
                    match track {
                        Track::Webcam => webcam.handle_rtcp(packet, now),
                        Track::Microphone => microphone.handle_rtcp(packet, now),
                        Track::Video => {}
                    }
                }
                match packet {
                    RtcpPacket::Rtpfb(RtpfbPacket { fmt, fci, .. }) if *fmt == RTCP_RTPFB_TWCC => {
                        let _ = twcc_tx.process_feedback(fci, now, 12_500.0);
                    }
                    RtcpPacket::Psfb(PsfbPacket { fmt, fci, .. }) if *fmt == RTCP_PSFB_APP => {
                        let _ = unpack_remb_fci(fci);
                    }
                    _ => {}
                }
            }
            continue;
        }
        let wire = Bytes::copy_from_slice(datagram);
        let Ok(packet) = RtpPacket::parse(&wire, &map) else {
            continue;
        };
        assert!(
            12 + 4 * packet.csrc.len() + packet.payload.len() + packet.padding_size as usize
                <= datagram.len(),
            "a payload past its datagram"
        );
        let receiver = match router.route_rtp(&packet) {
            Some(Track::Webcam) => &mut webcam,
            Some(Track::Microphone) => &mut microphone,
            _ => continue,
        };
        let _ = receiver.handle_rtp(packet, (now * 1000.0) as i64, now, &mut twcc_rx);
        drain(receiver);
    }
    for receiver in [&mut webcam, &mut microphone] {
        receiver.request_keyframe();
        drain(receiver);
        if let Some(report) = receiver.rtcp_report(now) {
            assert!(
                RtcpPacket::parse(&report).is_ok(),
                "a report that does not parse"
            );
        }
    }
});
