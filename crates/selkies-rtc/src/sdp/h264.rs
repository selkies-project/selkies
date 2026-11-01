/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The H.264 `profile-level-id` as RFC 6184 and the browsers read it.
//!
//! A profile is the `profile_idc` byte with a constraint-flag pattern, and two codecs are the
//! same H.264 when their profiles and packetization modes agree. A profile the table does
//! not model (High 4:4:4 `f4001f` of a full color stream among them) compares by its raw
//! id, since the sender payloads the encoder's bitstream as it is whatever the id says.

use super::Codec;

/// An H.264 profile RFC 6184's `profile-level-id` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H264Profile {
    ConstrainedBaseline,
    Baseline,
    Main,
    ConstrainedHigh,
    High,
    PredictiveHigh444,
}

/// An H.264 level, as `level_idc` tens; level 1b is `-1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct H264Level(pub i8);

const LEVELS: [i8; 17] = [
    10, 11, 12, 13, 20, 21, 22, 30, 31, 32, 40, 41, 42, 50, 51, 52, -1,
];

/// A `profile_iop` pattern: `1` and `0` bits must match, `x` bits are free.
struct BitPattern {
    mask: u8,
    value: u8,
}

impl BitPattern {
    const fn new(pattern: &[u8; 8]) -> BitPattern {
        let mut mask = 0u8;
        let mut value = 0u8;
        let mut i = 0;
        while i < 8 {
            let bit = 7 - i as u8;
            if pattern[i] != b'x' {
                mask |= 1 << bit;
            }
            if pattern[i] == b'1' {
                value |= 1 << bit;
            }
            i += 1;
        }
        BitPattern { mask, value }
    }

    fn matches(&self, profile_iop: u8) -> bool {
        profile_iop & self.mask == self.value
    }
}

const PROFILE_PATTERNS: [(u8, BitPattern, H264Profile); 9] = [
    (
        0x42,
        BitPattern::new(b"x1xx0000"),
        H264Profile::ConstrainedBaseline,
    ),
    (
        0x4D,
        BitPattern::new(b"1xxx0000"),
        H264Profile::ConstrainedBaseline,
    ),
    (
        0x58,
        BitPattern::new(b"11xx0000"),
        H264Profile::ConstrainedBaseline,
    ),
    (0x42, BitPattern::new(b"x0xx0000"), H264Profile::Baseline),
    (0x58, BitPattern::new(b"10xx0000"), H264Profile::Baseline),
    (0x4D, BitPattern::new(b"0x0x0000"), H264Profile::Main),
    (0x64, BitPattern::new(b"00000000"), H264Profile::High),
    (
        0x64,
        BitPattern::new(b"00001100"),
        H264Profile::ConstrainedHigh,
    ),
    (
        0xF4,
        BitPattern::new(b"00000000"),
        H264Profile::PredictiveHigh444,
    ),
];

/// Parse a six-digit hexadecimal `profile-level-id`.
///
/// Returns `None` for text that is not six hex digits, a level the table lacks, or a
/// profile no pattern models.
pub fn parse_profile_level_id(text: &str) -> Option<(H264Profile, H264Level)> {
    if text.len() != 6 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let profile_idc = u8::from_str_radix(&text[0..2], 16).ok()?;
    let profile_iop = u8::from_str_radix(&text[2..4], 16).ok()?;
    let level_idc = u8::from_str_radix(&text[4..6], 16).ok()?;
    let level = if level_idc == 11 {
        if profile_iop & 0x10 != 0 {
            H264Level(-1)
        } else {
            H264Level(11)
        }
    } else {
        let idc = i8::try_from(level_idc).ok()?;
        LEVELS.iter().copied().find(|l| *l == idc).map(H264Level)?
    };
    PROFILE_PATTERNS
        .iter()
        .find(|(idc, pattern, _)| *idc == profile_idc && pattern.matches(profile_iop))
        .map(|(_, _, profile)| (*profile, level))
}

/// Whether two codecs are the same codec for negotiation: the same MIME type and clock
/// rate, and for H.264 the same packetization mode and profile.
pub fn is_codec_compatible(a: &Codec, b: &Codec) -> bool {
    if !a.mime_type.eq_ignore_ascii_case(&b.mime_type) || a.clock_rate != b.clock_rate {
        return false;
    }
    if !a.mime_type.eq_ignore_ascii_case("video/h264") {
        return true;
    }
    let packetization = |c: &Codec| -> i64 {
        c.parameter("packetization-mode")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    if packetization(a) != packetization(b) {
        return false;
    }
    // The absence of a profile-level-id means constrained baseline 3.1, for older WebRTC.
    let raw = |c: &Codec| -> String {
        c.parameter("profile-level-id")
            .unwrap_or("42E01F")
            .to_ascii_lowercase()
    };
    match (
        parse_profile_level_id(&raw(a)),
        parse_profile_level_id(&raw(b)),
    ) {
        (Some((pa, _)), Some((pb, _))) => pa == pb,
        _ => raw(a) == raw(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_are_read_from_their_ids() {
        assert_eq!(
            parse_profile_level_id("42e01f"),
            Some((H264Profile::ConstrainedBaseline, H264Level(31)))
        );
        assert_eq!(
            parse_profile_level_id("42001f"),
            Some((H264Profile::Baseline, H264Level(31)))
        );
        assert_eq!(
            parse_profile_level_id("640c1f"),
            Some((H264Profile::ConstrainedHigh, H264Level(31)))
        );
        assert_eq!(
            parse_profile_level_id("f4001f"),
            Some((H264Profile::PredictiveHigh444, H264Level(31)))
        );
        assert_eq!(
            parse_profile_level_id("42100b"),
            Some((H264Profile::Baseline, H264Level(-1)))
        );
        assert_eq!(parse_profile_level_id("6e001f"), None);
        assert_eq!(parse_profile_level_id("zz"), None);
    }
}
