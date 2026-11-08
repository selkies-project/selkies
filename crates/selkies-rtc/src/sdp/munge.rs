/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The rewrites an offer takes before it reaches the page, and the 4:4:4 and 10-bit
//! decisions a peer's hello settles before the offer is built.
//!
//! `munge_sdp` injects a 125 ms `rtx-time`, `sps-pps-idr-in-keyframe=1` for H.264 and
//! H.265, the Opus `ptime`, and the video bandwidth ceilings. The codec rewrites describe
//! the display's own stream, so they reach its video section and no other: the recvonly
//! webcam section is the browser's to encode, and a profile it has no encoder for makes it
//! reject that section and stop the camera's sender. Full color is a 4:4:4 bitstream, so
//! the display's H.264 `profile-level-id` becomes High 4:4:4 (`f4001f`) rather than
//! handing the decoder a 4:2:0 baseline profile that cannot match what it receives; a
//! session known to encode on OpenH264 is excluded, since it emits limited-range 4:2:0 and
//! a 4:4:4 profile makes decoders misread its color range. A full color VP9 display offers
//! profile 1; a 10-bit display offers its codec's 10-bit profile: VP9 profile 2, or 3 at
//! 4:4:4, H.265 Main 10 in place of Main, H.264 High 10 (`6e001f`); AV1's main profile
//! carries both depths.
//!
//! `settle_fullcolor` and `settle_ten_bit` are the pure parts of the engine's settlement before an
//! offer: whether the offer may describe 4:4:4 or 10 bits to a peer whose hello named what it
//! decodes, and what the engine has to do about it.

use std::fmt::Write as _;

/// The codec whose 4:4:4 profile an encoder's stream carries when full color is on; the
/// others encode 4:2:0 whatever is asked.
pub fn fullcolor_codec(encoder: &str) -> Option<&'static str> {
    match encoder {
        "h264enc" => Some("h264"),
        "h265enc" => Some("h265"),
        "vp9enc" => Some("vp9"),
        _ => None,
    }
}

/// The codec whose 10-bit profile an encoder's stream carries when 10-bit is on; the others
/// encode 8 bits whatever is asked.
pub fn ten_bit_codec(encoder: &str) -> Option<&'static str> {
    match encoder {
        "h264enc" => Some("h264"),
        "h265enc" => Some("h265"),
        "vp9enc" => Some("vp9"),
        "av1enc" => Some("av1"),
        _ => None,
    }
}

/// What a peer's hello settles about a display's 4:4:4 or 10 bits before the offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settlement {
    /// The offer describes the format: the peer decodes it, or named nothing.
    Keep,
    /// The format is off for this offer: the host carries no such format for the encoder,
    /// so the stream is the plain one whatever is asked, and the setting stays.
    HostLacks,
    /// The peer decodes no such format: the display should turn it off before the offer; where the
    /// setting is held, the offer keeps it and the peer paints nothing of the stream.
    Decline,
}

/// Whether the offer to a peer may describe 4:4:4.
///
/// `fullcolor_codecs` is what the peer's hello said it decodes at 4:4:4; `host_fullcolor`
/// is `AppSettings.encoder_fullcolor(encoder, use_cpu)`, `Some(false)` where this host
/// carries no 4:4:4 for the encoder.
pub fn settle_fullcolor(
    encoder: &str,
    fullcolor_codecs: &[String],
    host_fullcolor: Option<bool>,
) -> Settlement {
    let Some(codec) = fullcolor_codec(encoder) else {
        return Settlement::Keep;
    };
    if fullcolor_codecs.iter().any(|c| c == codec) {
        return Settlement::Keep;
    }
    if host_fullcolor == Some(false) {
        return Settlement::HostLacks;
    }
    Settlement::Decline
}

/// Whether the offer to a peer may describe 10 bits.
///
/// `tenbit_codecs` is what the peer's hello said it decodes at 10 bits, a codec name for
/// 4:2:0 and the name with `444` after it for 4:4:4, `None` for a peer that said nothing
/// and is taken at its word; `host_ten_bit` is `AppSettings.encoder_ten_bit(encoder,
/// use_cpu, fullcolor)`. `HostLacks` here also covers an encoder with no 10-bit profile.
pub fn settle_ten_bit(
    encoder: &str,
    fullcolor: bool,
    tenbit_codecs: Option<&[String]>,
    host_ten_bit: Option<bool>,
) -> Settlement {
    let Some(codec) = ten_bit_codec(encoder) else {
        return Settlement::HostLacks;
    };
    if host_ten_bit == Some(false) {
        return Settlement::HostLacks;
    }
    let Some(tenbit_codecs) = tenbit_codecs else {
        return Settlement::Keep;
    };
    let wanted = if fullcolor {
        format!("{codec}444")
    } else {
        codec.to_owned()
    };
    if tenbit_codecs.contains(&wanted) {
        Settlement::Keep
    } else {
        Settlement::Decline
    }
}

/// What the display's stream is, for the offer's rewrites.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamFormat {
    /// The display's encoder: `h264enc`, `vp9enc`, ...
    pub encoder: String,
    /// Whether the display emits a 4:4:4 bitstream.
    pub fullcolor: bool,
    /// Whether the display emits a 10-bit bitstream.
    pub ten_bit: bool,
    /// Whether the display's H.264 is OpenH264's, which emits limited-range 4:2:0 whatever
    /// the profile says: `software_video_path && software_encoders()["h264"] == "openh264"`.
    pub openh264: bool,
    /// The Opus frame duration pcmflux emits, in milliseconds (`audio_frame_duration_ms`).
    pub opus_frame_ms: f64,
}

fn is_h264(encoder: &str) -> bool {
    encoder.contains("h264") || encoder.contains("x264")
}

fn is_h265(encoder: &str) -> bool {
    encoder.contains("h265") || encoder.contains("x265")
}

/// The sections of an SDP text, each starting at its `m=` line; the first is the session part.
fn sections(sdp: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in sdp.split_inclusive('\n') {
        if line.starts_with("m=") && !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
        current.push_str(line);
    }
    out.push(current);
    out
}

/// Replace every `profile-level-id=` six-digit value in `section` with `profile`.
fn rewrite_profile_level_id(section: &str, profile: &str) -> String {
    let mut out = String::with_capacity(section.len());
    let mut rest = section;
    while let Some(i) = rest.find("profile-level-id=") {
        let start = i + "profile-level-id=".len();
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let hex_len = tail
            .bytes()
            .take(6)
            .take_while(u8::is_ascii_hexdigit)
            .count();
        if hex_len == 6 {
            out.push_str(profile);
            rest = &tail[6..];
        } else {
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

/// Replace `key=old` with `key=new` where `old` stands as a whole value.
fn rewrite_value(section: &str, key: &str, old: &str, new: &str) -> String {
    let needle = format!("{key}={old}");
    let mut out = String::with_capacity(section.len());
    let mut rest = section;
    while let Some(i) = rest.find(&needle) {
        let end = i + needle.len();
        let bounded = rest[end..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
        out.push_str(&rest[..i]);
        if bounded {
            out.push_str(&format!("{key}={new}"));
        } else {
            out.push_str(&needle);
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Set every `rtx-time=<n>` to 125, or add one after each `apt=<n>` where none exists.
fn inject_rtx_time(sdp: &str) -> String {
    if !sdp.contains("rtx-time") {
        let mut out = String::with_capacity(sdp.len() + 64);
        let mut rest = sdp;
        while let Some(i) = rest.find("apt=") {
            let digits = rest[i + 4..].bytes().take_while(u8::is_ascii_digit).count();
            let end = i + 4 + digits;
            out.push_str(&rest[..end]);
            if digits > 0 {
                out.push_str(";rtx-time=125");
            }
            rest = &rest[end..];
        }
        out.push_str(rest);
        return out;
    }
    if sdp.contains("rtx-time=125") {
        return sdp.to_owned();
    }
    let mut out = String::with_capacity(sdp.len());
    let mut rest = sdp;
    while let Some(i) = rest.find("rtx-time=") {
        let start = i + "rtx-time=".len();
        let digits = rest[start..].bytes().take_while(u8::is_ascii_digit).count();
        out.push_str(&rest[..start]);
        out.push_str("125");
        rest = &rest[start + digits..];
    }
    out.push_str(rest);
    out
}

/// Rewrite the local offer for the display's stream (module docstring).
pub fn munge_sdp(sdp: &str, format: &StreamFormat) -> String {
    let encoder = format.encoder.as_str();
    let mut text = inject_rtx_time(sdp);
    let mut parts = sections(&text);
    for section in parts.iter_mut() {
        if !section.starts_with("m=video") || section.contains("a=recvonly") {
            continue;
        }
        let mut rewritten = section.clone();
        if is_h264(encoder) || is_h265(encoder) {
            if rewritten.contains("sps-pps-idr-in-keyframe") {
                rewritten = rewrite_sps_pps(&rewritten);
            } else {
                rewritten = rewritten.replace(
                    "packetization-mode=",
                    "sps-pps-idr-in-keyframe=1;packetization-mode=",
                );
            }
            if is_h264(encoder) && format.fullcolor && !format.openh264 {
                rewritten = rewrite_profile_level_id(&rewritten, "f4001f");
            }
        }
        if encoder.contains("vp9") && (format.fullcolor || format.ten_bit) {
            let profile = u8::from(format.fullcolor) + 2 * u8::from(format.ten_bit);
            rewritten = rewrite_value(&rewritten, "profile-id", "0", &profile.to_string());
        }
        if encoder.contains("h265") && format.ten_bit && !format.fullcolor {
            rewritten = rewrite_value(&rewritten, "profile-id", "1", "2");
        }
        if is_h264(encoder) && format.ten_bit && !format.fullcolor {
            rewritten = rewrite_profile_level_id(&rewritten, "6e001f");
        }
        *section = rewritten;
    }
    text = parts.concat();
    if text.to_ascii_lowercase().contains("opus/") {
        // A 2.5 ms frame advertises 3; pcmflux keeps the real frame.
        let ptime = (format.opus_frame_ms + 0.5) as i64;
        let line = format!("a=ptime:{ptime}");
        if !text.contains(&line) {
            // a=ptime is media-level: in the audio section, not after the video sprop lines,
            // and an audio-less offer skips it.
            let mut parts = sections(&text);
            if let Some(audio) = parts.iter_mut().find(|s| s.starts_with("m=audio")) {
                let mut lines: Vec<&str> = audio.split("\r\n").collect();
                let owned: Vec<String> = {
                    lines.insert(1, &line);
                    lines.iter().map(|l| (*l).to_owned()).collect()
                };
                *audio = owned.join("\r\n");
            }
            text = parts.concat();
        }
    }
    munge_video_bandwidth(&text)
}

fn rewrite_sps_pps(section: &str) -> String {
    let mut out = String::with_capacity(section.len());
    let mut rest = section;
    let key = "sps-pps-idr-in-keyframe=";
    while let Some(i) = rest.find(key) {
        let start = i + key.len();
        let digits = rest[start..].bytes().take_while(u8::is_ascii_digit).count();
        out.push_str(&rest[..start]);
        out.push('1');
        rest = &rest[start + digits..];
    }
    out.push_str(rest);
    out
}

/// The ceiling on a display's video section, in kbit/s: high enough never to bind.
const DISPLAY_CEILING_KBPS: u32 = 300_000;

/// The ceiling on the webcam's video section, in kbit/s: the 2.5 Mbit/s the WebSockets
/// uplink encodes the same camera at (`webcam-capture.ts`).
const WEBCAM_CEILING_KBPS: u32 = 2_500;

/// Set the bandwidth ceiling of every video section.
///
/// A generous `b=AS` on a display's section keeps the browser's REMB from throttling a
/// high-bitrate desktop stream (a cap hint, not a target), and `x-google-max-bitrate`
/// mirrors it on the Chrome receive side. The one section this side only receives is the
/// client's webcam, where the same two lines bound what the browser sends: there they carry
/// the rate the WebSockets uplink encodes the camera at, rather than letting the camera
/// take as much of the client's upload as the browser's estimate allows. Both are scoped to
/// each video section: the `b=AS` check looks only inside it, and the line goes after the
/// section's own `c=`, or right after the `m=video` line when the section inherits the
/// session-level `c=`. The x-google hints go on the fmtp of every video payload type except
/// RTX; a codec without an fmtp gets one carrying just the hints.
pub fn munge_video_bandwidth(sdp: &str) -> String {
    let lines: Vec<&str> = sdp.split("\r\n").collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 8);
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if !line.starts_with("m=video") {
            out.push(line.to_owned());
            i += 1;
            continue;
        }
        let mut section: Vec<String> = vec![line.to_owned()];
        i += 1;
        while i < lines.len() && !lines[i].starts_with("m=") {
            section.push(lines[i].to_owned());
            i += 1;
        }
        let kbps = if section.iter().any(|s| s == "a=recvonly") {
            WEBCAM_CEILING_KBPS
        } else {
            DISPLAY_CEILING_KBPS
        };
        let xgoogle = format!("x-google-max-bitrate={kbps};x-google-min-bitrate=0");
        if !section.iter().any(|s| s.starts_with("b=AS:")) {
            let insert_at = section
                .iter()
                .position(|s| s.starts_with("c="))
                .map_or(1, |c| c + 1);
            section.insert(insert_at.min(section.len()), format!("b=AS:{kbps}"));
        }
        let video_pts: Vec<String> = section
            .iter()
            .filter_map(|s| {
                let rest = s.strip_prefix("a=rtpmap:")?;
                let (pt, encoding) = rest.split_once(char::is_whitespace)?;
                (!encoding.trim().to_ascii_lowercase().starts_with("rtx/")).then(|| pt.to_owned())
            })
            .collect();
        for pt in video_pts {
            let fmtp_prefix = format!("a=fmtp:{pt} ");
            if let Some(index) = section.iter().position(|s| s.starts_with(&fmtp_prefix)) {
                if !section[index].contains("x-google-max-bitrate") {
                    let rest = section[index][fmtp_prefix.len()..].to_owned();
                    section[index] = format!("{fmtp_prefix}{xgoogle};{rest}");
                }
            } else {
                let rtpmap_prefix = format!("a=rtpmap:{pt} ");
                if let Some(index) = section.iter().position(|s| s.starts_with(&rtpmap_prefix)) {
                    section.insert(index + 1, format!("{fmtp_prefix}{xgoogle}"));
                }
            }
        }
        out.extend(section);
    }
    let mut text = String::with_capacity(sdp.len() + 256);
    for (n, line) in out.iter().enumerate() {
        if n > 0 {
            text.push_str("\r\n");
        }
        let _ = write!(text, "{line}");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settlements_follow_the_hello_and_the_host() {
        assert_eq!(settle_fullcolor("av1enc", &[], None), Settlement::Keep);
        assert_eq!(
            settle_fullcolor("h264enc", &["h264".to_owned()], None),
            Settlement::Keep
        );
        assert_eq!(
            settle_fullcolor("h264enc", &[], Some(false)),
            Settlement::HostLacks
        );
        assert_eq!(
            settle_fullcolor("vp9enc", &["h264".to_owned()], Some(true)),
            Settlement::Decline
        );
        assert_eq!(
            settle_ten_bit("vp8enc", false, None, None),
            Settlement::HostLacks
        );
        assert_eq!(
            settle_ten_bit("vp9enc", false, None, None),
            Settlement::Keep
        );
        assert_eq!(
            settle_ten_bit("vp9enc", true, Some(&["vp9444".to_owned()]), None),
            Settlement::Keep
        );
        assert_eq!(
            settle_ten_bit("vp9enc", true, Some(&["vp9".to_owned()]), None),
            Settlement::Decline
        );
        assert_eq!(
            settle_ten_bit("av1enc", false, Some(&[]), Some(false)),
            Settlement::HostLacks
        );
    }

    #[test]
    fn rtx_time_is_injected_or_corrected() {
        assert_eq!(
            inject_rtx_time("a=fmtp:98 apt=97\r\n"),
            "a=fmtp:98 apt=97;rtx-time=125\r\n"
        );
        assert_eq!(
            inject_rtx_time("a=fmtp:98 apt=97;rtx-time=3000\r\n"),
            "a=fmtp:98 apt=97;rtx-time=125\r\n"
        );
    }

    #[test]
    fn whole_values_alone_are_rewritten() {
        assert_eq!(
            rewrite_value("a=fmtp:1 profile-id=0\r\n", "profile-id", "0", "3"),
            "a=fmtp:1 profile-id=3\r\n"
        );
        assert_eq!(
            rewrite_value("a=fmtp:1 profile-id=01\r\n", "profile-id", "0", "3"),
            "a=fmtp:1 profile-id=01\r\n"
        );
    }
}
