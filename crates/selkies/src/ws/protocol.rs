/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The wire the data plane speaks: the control verbs, the binary opcodes, the gzip wrapping
//! of large control text, the token a handshake presents, the authority lists shared with
//! the WebRTC gate, and the `SETTINGS` payload as the server parses it.
//!
//! Text frames carry the control verbs. Binary frames are typed by their first byte:
//! `0x01` Opus audio in pcmflux's native framing, `0x03` JPEG and `0x04` video stripes as
//! pixelflux emits them (header and all), `0x02` a client's microphone Opus packet, `0x06` a
//! webcam frame for the virtual camera, `0x05` gzip-wrapped control text, and `0x07` padding
//! toward a page that sent `_pad,1`, which drops it unread (`PathProbe`). A client that
//! sends `_gz,1` can inflate gzip: control text at or above `WS_GZIP_MIN_BYTES` then goes out
//! as `0x05` frames, small latency-critical messages stay raw, and the server echoes the verb
//! so the client gzips its own large sends, which the handler inflates back into text before
//! dispatch, bounded by the per-message ceiling.

use std::io::Read;

use bytes::Bytes;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde_json::Value as Json;

use crate::settings::WS_MAX_MESSAGE_BYTES;

/// Opcodes of the binary frames, by their first byte.
pub const OPCODE_AUDIO: u8 = 0x01;
pub const OPCODE_MIC: u8 = 0x02;
pub const OPCODE_JPEG: u8 = 0x03;
pub const OPCODE_VIDEO: u8 = 0x04;
pub const OPCODE_GZIP: u8 = 0x05;
pub const OPCODE_WEBCAM: u8 = 0x06;
pub const OPCODE_PADDING: u8 = 0x07;

/// Bytes of a `0x06` webcam frame ahead of its payload: opcode, codec, flags.
pub const WEBCAM_HEADER_LEN: usize = 3;

/// Below this, gzip does not pay for its CPU and latency-critical small messages must stay
/// raw; matches the WebRTC data-channel threshold.
pub const WS_GZIP_MIN_BYTES: usize = 512;
/// From this size gzip runs on a blocking thread: level-6 compression of a multi-megabyte
/// clipboard would stall the runtime's workers for tens of milliseconds.
pub const WS_GZIP_OFFLOAD_BYTES: usize = 512 * 1024;

/// What a real-time frame may find queued in front of it: a bulk chunk is admitted only
/// below this.
pub const WS_BULK_BACKLOG_BYTES: usize = 16 * 1024;
pub const WS_BULK_BACKLOG_POLL_S: f64 = 0.002;
/// How long a bulk sender waits for this host's own queue to drain before writing anyway.
pub const BULK_DRAIN_TIMEOUT_S: f64 = 15.0;

/// Secure mode: the messages only the input-authority holder may send, the clipboard reads
/// among them; checked on the first comma-separated token.
pub const SECURE_INPUT_PREFIXES: &[&str] = &[
    "kd",
    "ku",
    "kh",
    "kr",
    "m",
    "m2",
    "co",
    "cmd",
    "sf",
    "sfe",
    "cws",
    "cbs",
    "cwd",
    "cbd",
    "cwe",
    "cbe",
    "cw",
    "cb",
    "cr",
    "REQUEST_CLIPBOARD",
];

/// A frame rate as a log line states it: to a hundredth, without trailing zeros, so
/// 60000/1001 reads 59.94 and 60 reads 60.
pub fn fps_label(fps: f64) -> String {
    let rounded = (fps * 100.0).round() / 100.0;
    if rounded.fract() == 0.0 {
        format!("{}", rounded as i64)
    } else {
        let text = format!("{rounded:.2}");
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// A `0x05` frame: the opcode, then a gzip container of `text` at level 6.
pub fn gzip_frame(text: &[u8]) -> Bytes {
    let mut out = Vec::with_capacity(text.len() / 4 + 32);
    out.push(OPCODE_GZIP);
    let mut encoder = GzEncoder::new(out, Compression::new(6));
    // Writing into a Vec cannot fail.
    let _ = std::io::Write::write_all(&mut encoder, text);
    Bytes::from(encoder.finish().unwrap_or_default())
}

/// Inflate a client gzip payload, bounded by the shared message ceiling: the inflated bytes
/// stand in for a raw text message, which could never exceed `WS_MAX_MESSAGE_BYTES`, so an
/// unbounded inflate would let one small frame balloon into process memory.
///
/// Errors name a payload inflating past the cap, a truncated stream, and bytes that are not
/// UTF-8.
pub fn inflate_gz_bounded(payload: &[u8]) -> Result<String, String> {
    let mut decoder = GzDecoder::new(payload);
    let mut inflated = Vec::new();
    let limit = WS_MAX_MESSAGE_BYTES as u64 + 1;
    (&mut decoder)
        .take(limit)
        .read_to_end(&mut inflated)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                "truncated gzip stream".to_string()
            } else {
                format!("undecodable gzip stream: {e}")
            }
        })?;
    if inflated.len() > WS_MAX_MESSAGE_BYTES {
        return Err(format!(
            "inflates past the {WS_MAX_MESSAGE_BYTES}-byte per-message ceiling"
        ));
    }
    String::from_utf8(inflated).map_err(|_| "inflated bytes are not UTF-8".to_string())
}

/// A `SETTINGS,{json}` payload as the server reads it: every key the client may send, typed,
/// absent where the payload lacks it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClientSettings {
    pub framerate: Option<f64>,
    pub video_crf: Option<i64>,
    pub encoder: Option<String>,
    pub video_fullcolor: Option<bool>,
    pub video_10bit: Option<bool>,
    pub video_streaming_mode: Option<bool>,
    pub manual_resolution: Option<bool>,
    pub manual_width: Option<i64>,
    pub manual_height: Option<i64>,
    pub audio_bitrate: Option<i64>,
    pub initial_client_width: Option<i64>,
    pub initial_client_height: Option<i64>,
    pub jpeg_quality: Option<i64>,
    pub paint_over_jpeg_quality: Option<i64>,
    pub use_cpu: Option<bool>,
    pub cpu_encode_threading: Option<bool>,
    pub video_paintover_crf: Option<i64>,
    pub video_paintover_burst_frames: Option<i64>,
    pub use_paint_over_quality: Option<bool>,
    pub scaling_dpi: Option<i64>,
    pub enable_binary_clipboard: Option<bool>,
    /// `primary` where the payload names none.
    pub display_id: String,
    pub display_position: Option<String>,
    pub display_scale: Option<f64>,
    pub rate_control_mode: Option<String>,
    pub video_bitrate: Option<f64>,
    pub force_aligned_resolution: Option<bool>,
    /// Whether the page decodes Opus+RED.
    pub audio_redundancy: Option<bool>,
    /// The browser tab the page runs in, at most 64 characters; empty becomes `None`.
    pub tab_id: Option<String>,
    pub keyboard_layout: Option<String>,
    /// Whether `encoder` is a fallback the page took on its own rather than its user's pick.
    pub encoder_fallback: Option<bool>,
    /// The keys the user just picked, at most 64, which is what applies from a page beside
    /// the display's owner.
    pub picked: Vec<String>,
    /// The rate congestion control last held the display at for this page, raw.
    pub cc_start_kbps: Option<Json>,
}

/// Why a `SETTINGS` payload could not be parsed.
#[derive(Debug, PartialEq)]
pub enum SettingsParseError {
    /// Not a JSON object.
    Json(String),
    /// A key carried a value its type cannot hold.
    Value(String),
}

impl std::fmt::Display for SettingsParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SettingsParseError::Json(e) => write!(f, "SETTINGS JSON decode error: {e}"),
            SettingsParseError::Value(e) => write!(f, "Error processing SETTINGS: {e}"),
        }
    }
}

/// A JSON value as a float: numbers, numeric strings, bools; `None` for null.
fn json_number(value: Option<&Json>, key: &str) -> Result<Option<f64>, SettingsParseError> {
    match value {
        None | Some(Json::Null) => Ok(None),
        Some(Json::Number(n)) => Ok(n.as_f64()),
        Some(Json::Bool(b)) => Ok(Some(if *b { 1.0 } else { 0.0 })),
        Some(Json::String(s)) => s.trim().parse::<f64>().map(Some).map_err(|_| {
            SettingsParseError::Value(format!(
                "could not convert string to float: '{s}' for {key}"
            ))
        }),
        Some(other) => Err(SettingsParseError::Value(format!(
            "{key} is not a number: {other}"
        ))),
    }
}

impl ClientSettings {
    /// Parse one payload; absent keys stay `None`, `displayId` defaults to `primary`, a
    /// float where an int belongs truncates rather than failing the whole payload.
    pub fn parse(payload: &str) -> Result<ClientSettings, SettingsParseError> {
        let data: Json =
            serde_json::from_str(payload).map_err(|e| SettingsParseError::Json(e.to_string()))?;
        let Json::Object(map) = data else {
            return Err(SettingsParseError::Value("payload is not an object".into()));
        };
        let get_int = |key: &str| -> Result<Option<i64>, SettingsParseError> {
            Ok(json_number(map.get(key), key)?.map(|v| v as i64))
        };
        let get_number = |key: &str| -> Result<Option<f64>, SettingsParseError> {
            json_number(map.get(key), key)
        };
        let get_bool = |key: &str| -> Option<bool> {
            match map.get(key) {
                None | Some(Json::Null) => None,
                Some(Json::Bool(b)) => Some(*b),
                Some(Json::String(s)) => Some(s.to_lowercase() == "true"),
                Some(other) => Some(other.to_string().to_lowercase() == "true"),
            }
        };
        let get_str = |key: &str| -> Option<String> {
            match map.get(key) {
                None | Some(Json::Null) => None,
                Some(Json::String(s)) => Some(s.clone()),
                Some(Json::Bool(b)) => Some(if *b { "True".into() } else { "False".into() }),
                Some(other) => Some(other.to_string()),
            }
        };
        let tab_id = get_str("tabId")
            .map(|t| t.chars().take(64).collect::<String>())
            .filter(|t| !t.is_empty());
        let picked = match map.get("picked") {
            Some(Json::Array(items)) => items
                .iter()
                .filter_map(|k| k.as_str().map(str::to_string))
                .take(64)
                .collect(),
            _ => Vec::new(),
        };
        Ok(ClientSettings {
            framerate: get_number("framerate")?,
            video_crf: get_int("video_crf")?,
            encoder: get_str("encoder"),
            video_fullcolor: get_bool("video_fullcolor"),
            video_10bit: get_bool("video_10bit"),
            video_streaming_mode: get_bool("video_streaming_mode"),
            manual_resolution: get_bool("manual_resolution"),
            manual_width: get_int("manual_width")?,
            manual_height: get_int("manual_height")?,
            audio_bitrate: get_int("audio_bitrate")?,
            initial_client_width: get_int("initialClientWidth")?,
            initial_client_height: get_int("initialClientHeight")?,
            jpeg_quality: get_int("jpeg_quality")?,
            paint_over_jpeg_quality: get_int("paint_over_jpeg_quality")?,
            use_cpu: get_bool("use_cpu"),
            cpu_encode_threading: get_bool("cpu_encode_threading"),
            video_paintover_crf: get_int("video_paintover_crf")?,
            video_paintover_burst_frames: get_int("video_paintover_burst_frames")?,
            use_paint_over_quality: get_bool("use_paint_over_quality"),
            scaling_dpi: get_int("scaling_dpi")?,
            enable_binary_clipboard: get_bool("enable_binary_clipboard"),
            display_id: get_str("displayId")
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| "primary".into()),
            display_position: get_str("displayPosition"),
            display_scale: get_number("displayScale")?,
            rate_control_mode: get_str("rate_control_mode"),
            video_bitrate: get_number("video_bitrate")?,
            force_aligned_resolution: get_bool("force_aligned_resolution"),
            audio_redundancy: get_bool("audioRedundancy"),
            tab_id,
            keyboard_layout: get_str("keyboardLayout"),
            encoder_fallback: get_bool("encoderFallback"),
            picked,
            cc_start_kbps: map.get("ccStartKbps").cloned(),
        })
    }

    /// The value of one `STREAM_SETTINGS` key as the payload carries it, `None` where absent.
    pub fn stream_value(&self, key: &str) -> Option<Json> {
        match key {
            "encoder" => self.encoder.clone().map(Json::from),
            "framerate" => self.framerate.map(Json::from),
            "video_crf" => self.video_crf.map(Json::from),
            "video_fullcolor" => self.video_fullcolor.map(Json::from),
            "video_10bit" => self.video_10bit.map(Json::from),
            "video_streaming_mode" => self.video_streaming_mode.map(Json::from),
            "jpeg_quality" => self.jpeg_quality.map(Json::from),
            "paint_over_jpeg_quality" => self.paint_over_jpeg_quality.map(Json::from),
            "use_paint_over_quality" => self.use_paint_over_quality.map(Json::from),
            "video_paintover_crf" => self.video_paintover_crf.map(Json::from),
            "video_paintover_burst_frames" => self.video_paintover_burst_frames.map(Json::from),
            "video_bitrate" => self.video_bitrate.map(Json::from),
            "rate_control_mode" => self.rate_control_mode.clone().map(Json::from),
            "use_cpu" => self.use_cpu.map(Json::from),
            "cpu_encode_threading" => self.cpu_encode_threading.map(Json::from),
            "audio_bitrate" => self.audio_bitrate.map(Json::from),
            _ => None,
        }
    }

    /// Whether the payload carries `key`, a stream setting.
    pub fn carries(&self, key: &str) -> bool {
        self.stream_value(key).is_some()
    }

    /// Take a stream setting out of the payload.
    pub fn clear_stream(&mut self, key: &str) {
        match key {
            "encoder" => self.encoder = None,
            "framerate" => self.framerate = None,
            "video_crf" => self.video_crf = None,
            "video_fullcolor" => self.video_fullcolor = None,
            "video_10bit" => self.video_10bit = None,
            "video_streaming_mode" => self.video_streaming_mode = None,
            "jpeg_quality" => self.jpeg_quality = None,
            "paint_over_jpeg_quality" => self.paint_over_jpeg_quality = None,
            "use_paint_over_quality" => self.use_paint_over_quality = None,
            "video_paintover_crf" => self.video_paintover_crf = None,
            "video_paintover_burst_frames" => self.video_paintover_burst_frames = None,
            "video_bitrate" => self.video_bitrate = None,
            "rate_control_mode" => self.rate_control_mode = None,
            "use_cpu" => self.use_cpu = None,
            "cpu_encode_threading" => self.cpu_encode_threading = None,
            "audio_bitrate" => self.audio_bitrate = None,
            _ => {}
        }
    }

    /// A payload carrying only the stream settings in `keys` (with their values from
    /// `self`), the display id, and nothing else: what a pick beside the owner applies.
    pub fn picks_only(&self, keys: &[&str], display_id: &str) -> ClientSettings {
        let mut out = ClientSettings {
            display_id: display_id.to_string(),
            ..Default::default()
        };
        for key in keys {
            match *key {
                "encoder" => out.encoder = self.encoder.clone(),
                "framerate" => out.framerate = self.framerate,
                "video_crf" => out.video_crf = self.video_crf,
                "video_fullcolor" => out.video_fullcolor = self.video_fullcolor,
                "video_10bit" => out.video_10bit = self.video_10bit,
                "video_streaming_mode" => out.video_streaming_mode = self.video_streaming_mode,
                "jpeg_quality" => out.jpeg_quality = self.jpeg_quality,
                "paint_over_jpeg_quality" => {
                    out.paint_over_jpeg_quality = self.paint_over_jpeg_quality
                }
                "use_paint_over_quality" => {
                    out.use_paint_over_quality = self.use_paint_over_quality
                }
                "video_paintover_crf" => out.video_paintover_crf = self.video_paintover_crf,
                "video_paintover_burst_frames" => {
                    out.video_paintover_burst_frames = self.video_paintover_burst_frames
                }
                "video_bitrate" => out.video_bitrate = self.video_bitrate,
                "rate_control_mode" => out.rate_control_mode = self.rate_control_mode.clone(),
                "use_cpu" => out.use_cpu = self.use_cpu,
                "cpu_encode_threading" => out.cpu_encode_threading = self.cpu_encode_threading,
                "audio_bitrate" => out.audio_bitrate = self.audio_bitrate,
                _ => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn gzip_round_trips_and_is_bounded() {
        let text = "SETTINGS,".to_string() + &"x".repeat(100_000);
        let frame = gzip_frame(text.as_bytes());
        assert_eq!(frame[0], OPCODE_GZIP);
        assert_eq!(inflate_gz_bounded(&frame[1..]).unwrap(), text);
        assert!(
            inflate_gz_bounded(&frame[1..frame.len() - 5])
                .unwrap_err()
                .contains("truncated")
        );
        let huge = vec![b'a'; WS_MAX_MESSAGE_BYTES + 1];
        let err = inflate_gz_bounded(&gzip_frame(&huge)[1..]).unwrap_err();
        assert!(err.contains("per-message ceiling"), "{err}");
        assert!(inflate_gz_bounded(b"not gzip").is_err());
    }

    #[test]
    fn settings_payload_parses_as_the_python_did() {
        let parsed = ClientSettings::parse(
            r#"{"framerate": "29.7", "video_crf": "33.9", "encoder": "h264enc", "use_cpu": "True",
                "displayId": "display2", "tabId": "", "picked": ["encoder", 3, "framerate"],
                "video_bitrate": 8000, "displayScale": 1.5, "ccStartKbps": "3000", "audioRedundancy": true}"#,
        )
        .unwrap();
        assert_eq!(parsed.framerate, Some(29.7));
        assert_eq!(parsed.video_crf, Some(33));
        assert_eq!(parsed.use_cpu, Some(true));
        assert_eq!(parsed.display_id, "display2");
        assert_eq!(parsed.tab_id, None);
        assert_eq!(parsed.picked, vec!["encoder", "framerate"]);
        assert_eq!(parsed.video_bitrate, Some(8000.0));
        assert_eq!(parsed.cc_start_kbps, Some(json!("3000")));
        assert_eq!(parsed.audio_redundancy, Some(true));
        assert_eq!(ClientSettings::parse("{}").unwrap().display_id, "primary");
        assert_eq!(
            ClientSettings::parse(r#"{"picked": "encoder"}"#)
                .unwrap()
                .picked,
            Vec::<String>::new()
        );
        assert!(matches!(
            ClientSettings::parse("nope"),
            Err(SettingsParseError::Json(_))
        ));
        assert!(matches!(
            ClientSettings::parse(r#"{"video_crf": "abc"}"#),
            Err(SettingsParseError::Value(_))
        ));
    }

    #[test]
    fn fps_labels() {
        assert_eq!(fps_label(60.0), "60");
        assert_eq!(fps_label(60000.0 / 1001.0), "59.94");
        assert_eq!(fps_label(29.5), "29.5");
    }
}
