/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What a peer sends over signaling, as UTF-8 with replacement: a session description and the
//! candidates in it or trickled beside it.
//!
//! The text is parsed as a description (`SessionDescription::parse`), and one that parses is
//! printed (`to_sdp`) and parsed again, with the same sections of the same kinds and mids. It is
//! then taken as the answer to the offer a display session makes (`create_offer` in
//! `crates/selkies/src/webrtc/peer.rs`: the display's video and audio sent, the microphone and
//! the webcam received, the data channels), through `Session::set_remote_description`, and
//! every candidate it settles is lifted into the ICE layer's form as the peer task lifts it
//! (`Candidate::from_sdp`). Every line naming a candidate is parsed the way a trickled one is
//! (`IceCandidate::from_sdp`), and one that parses prints a line that parses back to it. Nothing
//! may panic.
#![no_main]

use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use selkies_rtc::ice::Candidate;
use selkies_rtc::sdp::codecs::{
    MediaKind, capabilities, codec_preferences, microphone_preferences, mime_for_encoder,
    webcam_preferences,
};
use selkies_rtc::sdp::{
    Direction, DtlsFingerprint, IceCandidate, IceParameters, LocalTransport, SdpType, Session,
    SessionDescription,
};

/// The video ladder an offer lists, in the server's order.
const LADDER: [&str; 5] = [
    "video/H264",
    "video/AV1",
    "video/VP8",
    "video/H265",
    "video/VP9",
];

/// A display session with its offer taken as its local description, built once and cloned for
/// each input.
fn offered() -> Session {
    let mut session = Session::new(false, 2);
    let video = session.add_transceiver(MediaKind::Video, Direction::SendOnly);
    let preferences = codec_preferences(
        &capabilities(MediaKind::Video),
        mime_for_encoder("x264enc"),
        &LADDER,
    )
    .expect("H.264 is offered");
    session.set_codec_preferences(video, preferences);
    session.add_transceiver(MediaKind::Audio, Direction::SendOnly);
    let microphone = session.add_transceiver(MediaKind::Audio, Direction::RecvOnly);
    if let Some(preferences) = microphone_preferences(&session.table(MediaKind::Audio), true) {
        session.set_codec_preferences(microphone, preferences);
    }
    let webcam = session.add_transceiver(MediaKind::Video, Direction::RecvOnly);
    session.set_codec_preferences(webcam, webcam_preferences(&session.table(MediaKind::Video)));
    session.add_data_section();
    let transport = LocalTransport {
        ice: IceParameters {
            username_fragment: Some("selkiesu".to_owned()),
            password: Some("selkiespassword0123456".to_owned()),
            ice_lite: false,
        },
        candidates: Vec::new(),
        candidates_complete: false,
        fingerprints: vec![DtlsFingerprint {
            algorithm: "sha-256".to_owned(),
            value: ["AB"; 32].join(":"),
        }],
    };
    let offer = session.create_offer(&transport).expect("an offer");
    session
        .set_local_description(SdpType::Offer, &offer)
        .expect("the offer is taken");
    session
}

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    if let Ok(parsed) = SessionDescription::parse(&text) {
        let printed = parsed.to_sdp();
        let again = SessionDescription::parse(&printed)
            .unwrap_or_else(|e| panic!("what was printed does not parse: {e}\n{printed}"));
        let sections = |d: &SessionDescription| {
            d.media
                .iter()
                .map(|m| (m.kind.clone(), m.mid.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(sections(&again), sections(&parsed), "{printed}");
    }
    static OFFERED: OnceLock<Session> = OnceLock::new();
    let mut session = OFFERED.get_or_init(offered).clone();
    if let Ok(negotiated) = session.set_remote_description(SdpType::Answer, &text) {
        for candidate in &negotiated.candidates {
            let _ = Candidate::from_sdp(&candidate.to_sdp());
        }
    }
    for line in text.lines() {
        let Some(value) = line.split_once("candidate:").map(|(_, v)| v) else {
            continue;
        };
        if let Ok(candidate) = IceCandidate::from_sdp(value) {
            let printed = candidate.to_sdp();
            let again = IceCandidate::from_sdp(&printed)
                .unwrap_or_else(|e| panic!("a printed candidate does not parse: {e}: {printed}"));
            assert_eq!(again, candidate, "{printed}");
            let _ = Candidate::from_sdp(&printed);
        }
    }
});
