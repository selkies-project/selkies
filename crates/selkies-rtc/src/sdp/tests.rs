/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A remote SDP is parsed in time linear in its length, and the offer's codec rewrites
//! reach the display's own video section and no other.
//!
//! The parse runs on the task every session shares, on what a page sends over signaling: a
//! media section at the caps on codecs, feedback lines, and SSRCs parses at once, one past
//! any cap is refused, a description far past anything a browser sends is refused as fast,
//! and one longer than its bound is refused before parsing. The offer bundles the display's
//! sendonly video with the recvonly webcam section the browser encodes; full color rewrites
//! the H.264 profile to High 4:4:4 and VP9 to profile 1, H.264 or H.265 get
//! `sps-pps-idr-in-keyframe`, all of it describing the stream the server sends, while the
//! webcam section keeps the profiles it offered. A surround session offers `multiopus`
//! ahead of stereo RED and Opus on the audio it sends, and the microphone's section offers
//! only what its sink decodes.

use std::time::{Duration, Instant};

use super::codecs::{
    MediaKind, capabilities, codec_preferences, microphone_preferences, mime_for_encoder,
    webcam_preferences,
};
use super::munge::{StreamFormat, munge_sdp};
use super::{
    Direction, DtlsFingerprint, IceParameters, LocalTransport, MAX_CODECS, MAX_REMOTE_SDP,
    MAX_RTCP_FB, MAX_SSRCS, SdpError, SdpType, Session, SessionDescription,
};

fn head() -> String {
    let fingerprint = vec!["AA"; 32].join(":");
    format!(
        "v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0\r\n\
         a=fingerprint:sha-256 {fingerprint}\r\na=setup:active\r\n\
         a=ice-ufrag:abcd\r\na=ice-pwd:abcdefghijklmnopqrstuv\r\n"
    )
}

fn section(codecs: usize, feedback: usize, ssrcs: usize) -> String {
    let mut lines = vec![
        "m=video 9 UDP/TLS/RTP/SAVPF 96".to_owned(),
        "c=IN IP4 0.0.0.0".to_owned(),
        "a=mid:0".to_owned(),
        "a=rtcp-mux".to_owned(),
        "a=recvonly".to_owned(),
    ];
    lines.extend(std::iter::repeat_n(
        "a=rtpmap:96 VP8/90000".to_owned(),
        codecs,
    ));
    lines.extend(std::iter::repeat_n("a=rtcp-fb:* nack".to_owned(), feedback));
    lines.extend((0..ssrcs).map(|i| format!("a=ssrc:{} cname:x", 1000 + i)));
    format!("{}{}\r\n", head(), lines.join("\r\n"))
}

fn parse(text: &str) -> (Result<SessionDescription, SdpError>, Duration) {
    let started = Instant::now();
    let outcome = SessionDescription::parse(text);
    (outcome, started.elapsed())
}

#[test]
fn a_section_at_every_cap_parses_at_once() {
    let (outcome, took) = parse(&section(MAX_CODECS, MAX_RTCP_FB, MAX_SSRCS));
    assert!(outcome.is_ok(), "{outcome:?}");
    assert!(took < Duration::from_secs(1), "{took:?}");
}

#[test]
fn one_past_a_cap_is_refused() {
    for (label, args) in [
        ("codecs", (MAX_CODECS + 1, 0, 0)),
        ("feedback lines", (1, MAX_RTCP_FB + 1, 0)),
        ("SSRCs", (1, 0, MAX_SSRCS + 1)),
    ] {
        let (outcome, _) = parse(&section(args.0, args.1, args.2));
        assert!(
            matches!(outcome, Err(SdpError::Bound(_, bound)) if bound == label),
            "{label}: {outcome:?}"
        );
    }
}

#[test]
fn a_description_of_thousands_is_refused_at_once() {
    let big = section(4000, 4000, 4000);
    let (outcome, took) = parse(&big);
    assert!(matches!(outcome, Err(SdpError::Bound(..))), "{outcome:?}");
    assert!(took < Duration::from_millis(500), "{took:?}");
}

#[test]
fn an_answer_past_its_bound_is_refused_before_anything_else() {
    let text = format!("v=0\r\n{}", "a=x\r\n".repeat(MAX_REMOTE_SDP / 5));
    let (outcome, took) = parse(&text);
    assert_eq!(outcome, Err(SdpError::TooLong));
    assert!(took < Duration::from_millis(50), "{took:?}");
    assert!(SdpError::TooLong.to_string().contains("longer than"));
}

#[test]
fn the_corpus_answer_parses_and_prints_back() {
    let text = include_str!("../../../../tests/fuzz/corpus/sdp/answer.sdp");
    let parsed = SessionDescription::parse(text).expect("the corpus answer parses");
    assert!(!parsed.media.is_empty());
    let printed = parsed.to_sdp();
    assert!(printed.starts_with("v=0\r\n"));
    let again = SessionDescription::parse(&printed).expect("what was printed parses");
    assert_eq!(again.media.len(), parsed.media.len());
    for (a, b) in again.media.iter().zip(&parsed.media) {
        assert_eq!(a.kind, b.kind);
        assert_eq!(a.mid, b.mid);
        assert_eq!(a.codecs.len(), b.codecs.len());
        assert_eq!(a.direction, b.direction);
    }
}

#[test]
fn malformed_attributes_are_skipped_not_the_offer() {
    let text = format!(
        "{}m=video 9 UDP/TLS/RTP/SAVPF 96\r\nc=IN IP4 0.0.0.0\r\na=mid:0\r\na=rtcp-mux\r\n\
         a=sendrecv\r\na=rtpmap:96 VP8/90000\r\na=extmap\r\na=extmap:x urn:y\r\n\
         a=extmap:2/recvonly/foo urn:z\r\na=candidate:bogus\r\na=rtcp:nope\r\n\
         a=fmtp:97 apt=96\r\na=rtcp-fb:96 nack pli\r\n",
        head()
    );
    let parsed = SessionDescription::parse(&text).expect("parses");
    let media = &parsed.media[0];
    assert_eq!(media.header_extensions.len(), 1);
    assert_eq!(media.header_extensions[0].id, 2);
    assert!(media.ice_candidates.is_empty());
    assert_eq!(media.codecs[0].rtcp_feedback.len(), 1);
    assert_eq!(
        media.codecs[0].rtcp_feedback[0].parameter.as_deref(),
        Some("pli")
    );
    assert_eq!(media.direction, Some(Direction::SendRecv));
}

#[test]
fn forbidden_payload_types_refuse_the_offer() {
    let text = format!(
        "{}m=video 9 UDP/TLS/RTP/SAVPF 72\r\nc=IN IP4 0.0.0.0\r\na=mid:0\r\na=rtcp-mux\r\n",
        head()
    );
    assert_eq!(
        SessionDescription::parse(&text),
        Err(SdpError::PayloadTypeForbidden(72))
    );
    let text = format!(
        "{}m=video 9 UDP/TLS/RTP/SAVPF 300\r\nc=IN IP4 0.0.0.0\r\na=mid:0\r\na=rtcp-mux\r\n",
        head()
    );
    assert_eq!(
        SessionDescription::parse(&text),
        Err(SdpError::PayloadTypeRange)
    );
}

fn transport() -> LocalTransport {
    LocalTransport {
        ice: IceParameters {
            username_fragment: Some("ufrag123".to_owned()),
            password: Some("pwd456789012345678901234".to_owned()),
            ice_lite: false,
        },
        candidates: Vec::new(),
        candidates_complete: false,
        fingerprints: vec![DtlsFingerprint {
            algorithm: "sha-256".to_owned(),
            value: vec!["AB"; 32].join(":"),
        }],
    }
}

/// The offer of a display session: the display's video and audio sent, the webcam and the
/// microphone received, the data channels; `encoder` first in the video preferences.
fn offer(encoder: &str, audio_channels: u8) -> (Session, String) {
    let mut session = Session::new(false, audio_channels);
    let video = session.add_transceiver(MediaKind::Video, Direction::SendRecv);
    let audio = session.add_transceiver(MediaKind::Audio, Direction::SendRecv);
    let mic = session.add_transceiver(MediaKind::Audio, Direction::RecvOnly);
    let webcam = session.add_transceiver(MediaKind::Video, Direction::RecvOnly);
    let _ = audio;
    if let Some(wanted) = microphone_preferences(&session.table(MediaKind::Audio), true) {
        session.set_codec_preferences(mic, wanted);
    }
    session.set_codec_preferences(webcam, webcam_preferences(&session.table(MediaKind::Video)));
    let ladder = [
        "video/H264",
        "video/AV1",
        "video/VP8",
        "video/H265",
        "video/VP9",
    ];
    let preferences = codec_preferences(
        &capabilities(MediaKind::Video),
        mime_for_encoder(encoder),
        &ladder,
    )
    .expect("the codec is offered");
    session.set_codec_preferences(video, preferences);
    session.add_data_section();
    let sdp = session.create_offer(&transport()).expect("an offer");
    session
        .set_local_description(SdpType::Offer, &sdp)
        .expect("the offer is taken");
    (session, sdp)
}

/// The `m=<kind>` sections carrying `a=<direction>`.
fn sections<'a>(sdp: &'a str, kind: &str, direction: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut start = None;
    let bytes: Vec<(usize, &str)> = sdp.match_indices("\r\n").collect();
    let mut positions: Vec<usize> = vec![0];
    positions.extend(bytes.iter().map(|(i, _)| i + 2));
    for &pos in &positions {
        if sdp[pos..].starts_with("m=") {
            if let Some(s) = start {
                out.push(&sdp[s..pos]);
            }
            start = Some(pos);
        }
    }
    if let Some(s) = start {
        out.push(&sdp[s..]);
    }
    out.into_iter()
        .filter(|s| s.starts_with(&format!("m={kind}")) && s.contains(&format!("a={direction}")))
        .collect()
}

fn profiles(section: &str) -> Vec<String> {
    section
        .match_indices("profile-level-id=")
        .map(|(i, _)| section[i + 17..i + 23].to_ascii_lowercase())
        .collect()
}

fn lines_of(section: &str) -> Vec<&str> {
    section.split("\r\n").filter(|l| !l.is_empty()).collect()
}

/// The fmtp parameters of the section's VP9 payload types, without the x-google hints.
fn vp9_fmtp(section: &str) -> Vec<String> {
    let lines = lines_of(section);
    let pts: Vec<&str> = lines
        .iter()
        .filter_map(|l| l.strip_prefix("a=rtpmap:"))
        .filter(|l| l.contains(" VP9/"))
        .map(|l| l.split(' ').next().unwrap_or(""))
        .collect();
    pts.iter()
        .filter_map(|pt| {
            let prefix = format!("a=fmtp:{pt} ");
            lines
                .iter()
                .find_map(|l| l.strip_prefix(&prefix))
                .map(|params| {
                    params
                        .split(';')
                        .filter(|p| !p.starts_with("x-google-"))
                        .collect::<Vec<_>>()
                        .join(";")
                })
        })
        .collect()
}

fn format(encoder: &str, fullcolor: bool, ten_bit: bool) -> StreamFormat {
    StreamFormat {
        encoder: encoder.to_owned(),
        fullcolor,
        ten_bit,
        openh264: false,
        opus_frame_ms: 10.0,
    }
}

fn dynamic(section: &str) -> std::collections::BTreeSet<u32> {
    lines_of(section)
        .iter()
        .filter_map(|l| l.strip_prefix("a=rtpmap:"))
        .filter_map(|l| l.split(' ').next()?.parse().ok())
        .collect()
}

#[test]
fn the_offers_codec_rewrites_reach_the_displays_section_alone() {
    let (_, raw) = offer("h264enc", 2);
    let display = sections(&raw, "video", "sendrecv");
    let webcam = sections(&raw, "video", "recvonly");
    assert_eq!((display.len(), webcam.len()), (1, 1), "{raw}");
    assert!(!profiles(display[0]).is_empty() && !profiles(webcam[0]).is_empty());
    assert_eq!(vp9_fmtp(webcam[0]), vec!["profile-id=0"]);
    assert!(!raw.contains("sps-pps-idr-in-keyframe"));
    assert!(raw.contains("a=group:BUNDLE 0 1 2 3 4"));
    assert!(raw.contains("a=sctp-port:5000") && raw.contains("a=max-message-size:262144"));

    let full = munge_sdp(&raw, &format("h264enc", true, false));
    let out_display = sections(&full, "video", "sendrecv")[0];
    let out_webcam = sections(&full, "video", "recvonly")[0];
    let display_profiles: std::collections::HashSet<String> =
        profiles(out_display).into_iter().collect();
    assert_eq!(
        display_profiles,
        std::collections::HashSet::from(["f4001f".to_owned()])
    );
    assert_eq!(profiles(out_webcam), profiles(webcam[0]));
    assert!(out_display.contains("sps-pps-idr-in-keyframe=1"));
    assert!(!out_webcam.contains("sps-pps-idr-in-keyframe"));
    assert!(out_display.contains("b=AS:300000") && out_webcam.contains("b=AS:300000"));
    assert!(full.contains("rtx-time=125"));

    let plain = munge_sdp(&raw, &format("h264enc", false, false));
    assert_eq!(
        profiles(sections(&plain, "video", "sendrecv")[0]),
        profiles(display[0])
    );

    let vp9 = munge_sdp(&raw, &format("vp9enc", true, false));
    let out_display = sections(&vp9, "video", "sendrecv")[0];
    let out_webcam = sections(&vp9, "video", "recvonly")[0];
    assert_eq!(vp9_fmtp(out_display), vec!["profile-id=1"]);
    assert_eq!(vp9_fmtp(out_webcam), vec!["profile-id=0"]);
    assert_eq!(profiles(out_display), profiles(display[0]));

    let ten = munge_sdp(&raw, &format("vp9enc", true, true));
    assert_eq!(
        vp9_fmtp(sections(&ten, "video", "sendrecv")[0]),
        vec!["profile-id=3"]
    );
    let ten = munge_sdp(&raw, &format("h264enc", false, true));
    let display_profiles: std::collections::HashSet<String> =
        profiles(sections(&ten, "video", "sendrecv")[0])
            .into_iter()
            .collect();
    assert_eq!(
        display_profiles,
        std::collections::HashSet::from(["6e001f".to_owned()])
    );

    let hevc = munge_sdp(&raw, &format("h265enc", true, false));
    let out_display = sections(&hevc, "video", "sendrecv")[0];
    assert!(out_display.contains("sps-pps-idr-in-keyframe=1"));
    assert_eq!(profiles(out_display), profiles(display[0]));

    let audio_out = sections(&full, "audio", "sendrecv");
    let mic_out = sections(&full, "audio", "recvonly");
    assert_eq!((audio_out.len(), mic_out.len()), (1, 1));
    assert!(audio_out[0].contains("a=ptime:10"));
    assert!(!mic_out[0].contains("a=ptime:"));
    assert_eq!(
        lines_of(audio_out[0])[1],
        "a=ptime:10",
        "ptime right after the m= line"
    );

    // One BUNDLE group, so a payload type stands for one codec with one set of parameters.
    let mut shared = dynamic(out_display);
    shared.extend(dynamic(audio_out[0]));
    let own = [dynamic(out_webcam), dynamic(mic_out[0])];
    assert!(
        own.iter().all(|o| !o.is_empty() && o.is_disjoint(&shared)),
        "{own:?} vs {shared:?}"
    );
    assert!(own[0].is_disjoint(&own[1]));
    let mic_lines = lines_of(mic_out[0]);
    let red = mic_lines
        .iter()
        .find_map(|l| l.strip_prefix("a=rtpmap:").filter(|l| l.contains(" red/")))
        .map(|l| l.split(' ').next().unwrap_or(""))
        .expect("the mic offers RED");
    let opus = mic_lines
        .iter()
        .find_map(|l| l.strip_prefix("a=rtpmap:").filter(|l| l.contains(" opus/")))
        .map(|l| l.split(' ').next().unwrap_or(""))
        .expect("the mic offers Opus");
    assert!(
        mic_lines.contains(&format!("a=fmtp:{red} {opus}/{opus}").as_str()),
        "{mic_lines:?}"
    );
}

#[test]
fn a_surround_session_offers_multiopus_first_on_the_audio_it_sends() {
    let (_, offer) = offer("h264enc", 6);
    let sent = sections(&offer, "audio", "sendrecv");
    let mic = sections(&offer, "audio", "recvonly");
    assert_eq!((sent.len(), mic.len()), (1, 1));
    let names = |section: &str| -> Vec<String> {
        let lines = lines_of(section);
        let fmt: Vec<&str> = lines[0].split(' ').skip(3).collect();
        fmt.iter()
            .map(|pt| {
                lines
                    .iter()
                    .find_map(|l| l.strip_prefix(&format!("a=rtpmap:{pt} ")))
                    .and_then(|l| l.split('/').next())
                    .unwrap_or("")
                    .to_owned()
            })
            .collect()
    };
    let sent_names = names(sent[0]);
    assert_eq!(&sent_names[..3], ["multiopus", "red", "opus"]);
    let video_pts = dynamic(sections(&offer, "video", "sendrecv")[0]);
    let multiopus_pt: u32 = lines_of(sent[0])[0]
        .split(' ')
        .nth(3)
        .unwrap()
        .parse()
        .unwrap();
    assert!(!video_pts.contains(&multiopus_pt));
    let mic_names = names(mic[0]);
    assert_eq!(&mic_names[..2], ["red", "opus"]);
    assert!(!mic_names.iter().any(|n| n == "multiopus"));
}

/// An answer the way the engines write one: every section's codecs but multiopus, the
/// server's sections reversed, `a=setup:active`.
fn engine_answer(offer: &str, declined: &[&str]) -> String {
    let parsed = SessionDescription::parse(offer).expect("the offer parses");
    let mut answer = parsed.clone();
    answer.kind = Some(SdpType::Answer);
    for media in &mut answer.media {
        media
            .codecs
            .retain(|c| !declined.iter().any(|d| c.mime_type.eq_ignore_ascii_case(d)));
        let kept: Vec<u8> = media.codecs.iter().map(|c| c.payload_type).collect();
        media
            .codecs
            .retain(|c| !c.is_rtx() || c.apt().is_some_and(|apt| kept.contains(&apt)));
        if media.is_rtp() {
            media.fmt = media
                .codecs
                .iter()
                .map(|c| c.payload_type.to_string())
                .collect();
        }
        media.direction = media.direction.map(Direction::reverse);
        media.ssrc.clear();
        media.ssrc_group.clear();
        media.ice.username_fragment = Some("peerfrag".to_owned());
        media.ice.password = Some("peerpassword0123456789ab".to_owned());
        if let Some(dtls) = media.dtls.as_mut() {
            dtls.role = super::DtlsRole::Client;
        }
    }
    answer.to_sdp()
}

#[test]
fn an_answer_settles_the_codecs_the_peer_took_in_the_offers_order() {
    let (mut session, offer) = offer("vp9enc", 2);
    let answer = engine_answer(&offer, &["video/VP9", "video/multiopus"]);
    let negotiated = session
        .set_remote_description(SdpType::Answer, &answer)
        .expect("the answer is taken");
    assert_eq!(negotiated.dtls_role, Some(super::DtlsRole::Server));
    assert!(negotiated.sctp);
    assert_eq!(
        session.sctp.as_ref().and_then(|s| s.remote_port),
        Some(5000)
    );
    let display = &session.transceivers[0];
    let answered = display.answered_mimes();
    assert_eq!(answered[0], "video/H264", "{answered:?}");
    assert!(!answered.iter().any(|m| m == "video/VP9"));
    assert_eq!(display.current_direction, Some(Direction::SendRecv));
    let audio = &session.transceivers[1];
    assert_eq!(audio.answered_mimes()[0], "audio/red");
    assert_eq!(
        session.signaling_state(),
        super::offer::SignalingState::Stable
    );
}

#[test]
fn an_answer_out_of_turn_or_without_credentials_is_refused() {
    let mut session = Session::new(false, 2);
    session.add_transceiver(MediaKind::Video, Direction::SendRecv);
    let text = format!(
        "{}m=video 9 UDP/TLS/RTP/SAVPF 96\r\nc=IN IP4 0.0.0.0\r\na=mid:0\r\na=rtcp-mux\r\na=sendrecv\r\na=rtpmap:96 VP8/90000\r\n",
        head()
    );
    assert!(matches!(
        session.set_remote_description(SdpType::Answer, &text),
        Err(SdpError::State(_))
    ));
    let sdp = session.create_offer(&transport()).unwrap();
    session.set_local_description(SdpType::Offer, &sdp).unwrap();
    let no_credentials =
        engine_answer(&sdp, &[]).replace("a=ice-pwd:peerpassword0123456789ab\r\n", "");
    assert!(matches!(
        session.set_remote_description(SdpType::Answer, &no_credentials),
        Err(SdpError::Invalid(_))
    ));
}

#[test]
fn an_ice_lite_session_says_so_in_its_offer() {
    let mut session = Session::new(true, 2);
    session.add_transceiver(MediaKind::Video, Direction::SendOnly);
    let sdp = session.create_offer(&transport()).unwrap();
    assert!(sdp.contains("\r\na=ice-lite\r\n"), "{sdp}");
    session.set_local_description(SdpType::Offer, &sdp).unwrap();
    assert_eq!(
        session.ice_controlling(),
        Some(false),
        "an ICE-lite offerer is controlled"
    );
}

/// `tests/fuzz/fuzz_sdp.py`'s shape: the corpus answer mutated, parsed, and what parses printed.
#[test]
fn mutated_descriptions_never_panic_the_parse() {
    let seed = include_str!("../../../../tests/fuzz/corpus/sdp/answer.sdp").as_bytes();
    let mut state = 0x1234_5678_9abc_def0u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..3000 {
        let mut bytes = seed.to_vec();
        for _ in 0..(next() % 12) {
            let at = (next() % bytes.len() as u64) as usize;
            match next() % 4 {
                0 => bytes[at] = (next() % 128) as u8,
                1 => {
                    bytes.remove(at);
                }
                2 => bytes.truncate(at.max(1)),
                _ => bytes[at] = b' ',
            }
            if bytes.is_empty() {
                break;
            }
        }
        let text = String::from_utf8_lossy(&bytes);
        if let Ok(parsed) = SessionDescription::parse(&text) {
            let _ = parsed.to_sdp();
        }
    }
}

#[test]
fn the_offer_names_the_ladder_the_groups_and_the_extensions_of_a_display() {
    let (session, raw) = offer("vp9enc", 2);
    let display = sections(&raw, "video", "sendrecv")[0];
    let lines = lines_of(display);
    let fmt: Vec<&str> = lines[0].split(' ').skip(3).collect();
    let name_of = |pt: &str| -> String {
        lines
            .iter()
            .find_map(|l| l.strip_prefix(&format!("a=rtpmap:{pt} ")))
            .and_then(|l| l.split('/').next())
            .unwrap_or("")
            .to_owned()
    };
    let names: Vec<String> = fmt.iter().map(|pt| name_of(pt)).collect();
    assert_eq!(names[0], "VP9", "{names:?}");
    assert_eq!(names[1], "rtx", "its RTX right behind it: {names:?}");
    assert_eq!(names[2], "H264", "then the ladder: {names:?}");
    assert_eq!(
        names.last().map(String::as_str),
        Some("flexfec-03"),
        "{names:?}"
    );
    let video = &session.transceivers[0];
    assert!(display.contains(&format!(
        "a=ssrc-group:FID {} {}",
        video.ssrc, video.rtx_ssrc
    )));
    assert!(display.contains(&format!(
        "a=ssrc-group:FEC-FR {} {}",
        video.ssrc, video.fec_ssrc
    )));
    assert!(display.contains("a=imageattr:* send [x=[16:8192],y=[16:8192]] recv *"));
    for (id, uri) in [
        (
            5,
            "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01",
        ),
        (8, super::codecs::DEPENDENCY_DESCRIPTOR_URI),
        (9, super::codecs::ABS_CAPTURE_TIME_URI),
    ] {
        assert!(display.contains(&format!("a=extmap:{id} {uri}")), "{uri}");
    }
    assert!(raw.contains("a=setup:actpass"));
    assert!(raw.contains("a=fingerprint:sha-256 AB:"));
    assert!(raw.contains("a=ice-ufrag:ufrag123"));
    assert!(raw.contains("a=msid-semantic:WMS *"));
    let webcam = sections(&raw, "video", "recvonly")[0];
    assert!(
        !webcam.contains("flexfec-03"),
        "the camera's section is its sink's alone"
    );
}
