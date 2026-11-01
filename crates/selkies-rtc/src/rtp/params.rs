/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The negotiated parameters a sender or receiver is started with: the codecs by payload
//! type, the header extensions by id, the mid, and the RTCP cname. The SDP layer fills
//! these from the answer; the defaults the offer carries live in `codecs`.

use std::collections::BTreeMap;

/// One negotiated codec of a media section.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodecParameters {
    /// The MIME type as the SDP spells it, such as `video/H264` or `audio/opus`.
    pub mime_type: String,
    pub clock_rate: u32,
    /// Audio channels, 0 for video.
    pub channels: u8,
    pub payload_type: u8,
    /// The `a=fmtp` parameters; a flag parameter maps to an empty string.
    pub parameters: BTreeMap<String, String>,
    /// The `a=rtcp-fb` entries as `(type, parameter)`.
    pub rtcp_feedback: Vec<(String, String)>,
}

impl CodecParameters {
    /// A codec with its MIME type, clock rate, and payload type and nothing else.
    pub fn new(mime_type: &str, clock_rate: u32, payload_type: u8) -> Self {
        Self {
            mime_type: mime_type.to_string(),
            clock_rate,
            payload_type,
            ..Self::default()
        }
    }

    /// The codec name after the slash, lower case.
    pub fn name(&self) -> String {
        self.mime_type
            .split_once('/')
            .map(|(_, n)| n)
            .unwrap_or(&self.mime_type)
            .to_ascii_lowercase()
    }

    /// The MIME type lower case, which every comparison uses.
    pub fn mime_lower(&self) -> String {
        self.mime_type.to_ascii_lowercase()
    }

    pub fn is_rtx(&self) -> bool {
        self.name() == "rtx"
    }

    pub fn is_red(&self) -> bool {
        self.name() == "red"
    }

    pub fn is_flexfec(&self) -> bool {
        self.mime_lower() == "video/flexfec-03"
    }

    /// The `apt` of an RTX codec, where it names one.
    pub fn apt(&self) -> Option<u8> {
        self.parameters.get("apt").and_then(|v| v.parse().ok())
    }
}

/// One negotiated RTP header extension.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeaderExtensionParameters {
    pub id: u8,
    pub uri: String,
}

impl HeaderExtensionParameters {
    pub fn new(id: u8, uri: &str) -> Self {
        Self {
            id,
            uri: uri.to_string(),
        }
    }
}

/// What a sender is started with.
#[derive(Clone, Debug, Default)]
pub struct SendParameters {
    pub codecs: Vec<CodecParameters>,
    pub header_extensions: Vec<HeaderExtensionParameters>,
    /// The media section's mid, written as the `mid` extension.
    pub mux_id: Option<String>,
    /// The RTCP cname sent in SDES.
    pub cname: Option<String>,
}

/// One received encoding: a media SSRC and the RTX SSRC paired with it.
#[derive(Clone, Debug, Default)]
pub struct ReceiveEncoding {
    pub ssrc: u32,
    pub rtx_ssrc: Option<u32>,
}

/// What a receiver is started with.
#[derive(Clone, Debug, Default)]
pub struct ReceiveParameters {
    pub codecs: Vec<CodecParameters>,
    pub header_extensions: Vec<HeaderExtensionParameters>,
    pub encodings: Vec<ReceiveEncoding>,
    pub mux_id: Option<String>,
}
