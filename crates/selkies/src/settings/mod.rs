/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Centralized configuration schema and resolution for the Selkies server.
//!
//! Every setting lives in one declarative table (`table::DEFINITIONS`, the definition of
//! record, which `docs/settings.md` is rendered from); `Settings::parse` resolves each entry
//! with precedence CLI flag > `SELKIES_<NAME>` environment variable > fallback variable(s) >
//! built-in default. CLI and environment names derive from `name`: `my_setting` becomes
//! `--my-setting` (and `--my_setting`) and `SELKIES_MY_SETTING`. This module also owns
//! everything both transports must agree on about settings: the client-facing settings
//! payload (`client_payload`), the shared per-setting sanitizer for client-proposed values
//! (`sanitize_client_setting`), the encoder ladder and the host's encoder backends, and the
//! WebSocket message size ceiling.
//!
//! Override value syntax, by setting type:
//!
//! - List/enum (`SELKIES_ENCODER="jpeg,h264enc"`): the first item is the default, the full
//!   list the allowed options; a single value locks the choice. Invalid items are dropped;
//!   an entirely invalid override keeps the full built-in menu and default.
//! - Bool (case-insensitive): `"true"`/`"1"` is on, anything else off, a bare flag
//!   (`--public`) is on; a `"|locked"` suffix (`"true|locked"`) forbids the client changing
//!   it.
//! - Range: `"8-240"` restricts the allowed span (initial value the built-in default,
//!   clamped in); a bare value `"60"` keeps the built-in span and makes it the initial
//!   value, widening the span if it falls outside; `"60,8-240"` sets both; a degenerate
//!   span `"60-60"` locks the setting.
//! - An override set to `""` means the built-in default; list types keep their explicit
//!   `""`/`"none"` = disable semantics.

pub mod table;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};

use log::{error, info, warn};
use serde::Serialize;
use serde_json::{Map, Value as Json, json};

pub use table::DEFINITIONS;

/// The type of a setting, which decides how its text is parsed and published.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Bool,
    Enum,
    List,
    Int,
    Float,
    Str,
    Range,
}

/// One entry of the settings table.
#[derive(Debug)]
pub struct SettingDef {
    pub name: &'static str,
    pub kind: Kind,
    /// The built-in default as text, parsed like an override.
    pub default: &'static str,
    pub help: &'static str,
    /// Fallback environment variables, earlier entries winning.
    pub env_vars: &'static [&'static str],
    /// The menu of an enum or list.
    pub allowed: &'static [&'static str],
    /// A numeric enum's span of values accepted beyond its curated stops.
    pub value_range: Option<(f64, f64)>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// A range's initial value.
    pub default_value: Option<f64>,
    /// A secret: never published to clients.
    pub sensitive: bool,
    /// Listener, filesystem, and lifecycle settings a browser has no use for.
    pub client_excluded: bool,
}

/// A number that keeps whether it was integral, for a range bound.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Num {
    Int(i64),
    Float(f64),
}

impl Num {
    /// Parse a range number: integral text stays an int, anything else a float; `inf` and
    /// `nan` are refused, since no span can carry them.
    pub fn parse(text: &str) -> Result<Num, String> {
        let value: f64 = text
            .trim()
            .parse()
            .map_err(|_| format!("{text:?} is not a number"))?;
        if !value.is_finite() {
            return Err(format!("{text:?} is not a finite number"));
        }
        Ok(Num::of(value))
    }

    /// The number, keeping whether it is integral.
    pub fn of(value: f64) -> Num {
        if value.fract() == 0.0 && value.abs() < 9.0e15 {
            Num::Int(value as i64)
        } else {
            Num::Float(value)
        }
    }

    pub fn as_f64(self) -> f64 {
        match self {
            Num::Int(i) => i as f64,
            Num::Float(f) => f,
        }
    }

    fn min(self, other: Num) -> Num {
        if self.as_f64() <= other.as_f64() {
            self
        } else {
            other
        }
    }

    fn max(self, other: Num) -> Num {
        if self.as_f64() >= other.as_f64() {
            self
        } else {
            other
        }
    }
}

impl fmt::Display for Num {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Num::Int(i) => write!(f, "{i}"),
            Num::Float(v) => write!(f, "{v}"),
        }
    }
}

impl Serialize for Num {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Num::Int(i) => s.serialize_i64(*i),
            Num::Float(v) => s.serialize_f64(*v),
        }
    }
}

/// A bool setting: its value and whether the operator locked it against the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoolSetting {
    pub on: bool,
    pub locked: bool,
}

/// A setting's resolved value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Bool(BoolSetting),
    /// An enum's choice or a string setting.
    Str(String),
    List(Vec<String>),
    Int(i64),
    Float(f64),
    /// A range's span, low then high.
    Range(Num, Num),
}

impl Value {
    /// The value as the client payload carries it.
    fn to_json(&self) -> Json {
        match self {
            Value::Bool(b) => json!(b.on),
            Value::Str(s) => json!(s),
            Value::List(l) => json!(l),
            Value::Int(i) => json!(i),
            Value::Float(f) => json!(f),
            Value::Range(lo, hi) => json!([lo, hi]),
        }
    }
}

/// Resolve a configured value's truth: `true` or `1`, case-insensitively, ahead of any
/// `|locked` suffix. Everything else is false; an empty value keeps `default`.
pub fn parse_bool(value: Option<&str>, default: bool) -> bool {
    match value.map(str::trim) {
        None | Some("") => default,
        Some(text) => matches!(
            text.split('|')
                .next()
                .unwrap_or("")
                .trim()
                .to_lowercase()
                .as_str(),
            "true" | "1"
        ),
    }
}

/// The 32 MiB above which proxies and WebSocket stacks in the field degrade or reject
/// frames, and so the most the server emits in one frame.
pub const WS_MESSAGE_SIZE_HARD_CAP: usize = 32 * 1024 * 1024;
/// The message size ceiling in both directions: enforced on receive, advertised to clients
/// so multipart chunk sizing fills the frame, and the bound on inflating a gzip frame.
pub const WS_MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

/// Encoders the WebRTC pipeline can produce: the full-frame codecs its RTP stack
/// packetizes; the jpeg framing is a websockets-stream concept.
pub const WEBRTC_ENCODER_CHOICES: &[&str] = &["h264enc", "h265enc", "vp8enc", "vp9enc", "av1enc"];
/// Spellings base images still ship in `SELKIES_ENCODER`; both mean full-frame H.264.
const ENCODER_ALIASES: &[(&str, &str)] = &[("x264enc", "h264enc"), ("openh264enc", "h264enc")];
/// The pixelflux codec each encoder name selects.
pub const ENCODER_CODECS: &[(&str, &str)] = &[
    ("jpeg", "jpeg"),
    ("h264enc", "h264"),
    ("h265enc", "h265"),
    ("vp8enc", "vp8"),
    ("vp9enc", "vp9"),
    ("av1enc", "av1"),
];
/// The name each pixelflux codec goes by in logs and notices.
pub const CODEC_LABELS: &[(&str, &str)] = &[
    ("jpeg", "JPEG"),
    ("h264", "H.264"),
    ("h265", "H.265"),
    ("vp8", "VP8"),
    ("vp9", "VP9"),
    ("av1", "AV1"),
];
/// Encoders with no hardware path: selecting one implies software encoding.
pub const CPU_ONLY_ENCODERS: &[&str] = &["jpeg"];
/// What a display streams with, as `SETTINGS` carries it: a controller beside the display's
/// owner changes these by its user's pick alone.
pub const STREAM_SETTINGS: &[&str] = &[
    "encoder",
    "framerate",
    "video_crf",
    "video_fullcolor",
    "video_10bit",
    "video_streaming_mode",
    "jpeg_quality",
    "paint_over_jpeg_quality",
    "use_paint_over_quality",
    "video_paintover_crf",
    "video_paintover_burst_frames",
    "video_bitrate",
    "rate_control_mode",
    "use_cpu",
    "cpu_encode_threading",
    "audio_bitrate",
];
/// The fallback ladder, one order on both transports: the full-frame codecs by the measured
/// time per frame of their software encoders, and JPEG last.
pub const ENCODER_LADDER: &[&str] = &["h264enc", "av1enc", "vp8enc", "h265enc", "vp9enc", "jpeg"];
/// The full-frame codecs by compression efficiency, the order a host with an engine for
/// them takes them in.
pub const HARDWARE_LADDER: &[&str] = &["av1enc", "h265enc", "vp9enc", "h264enc", "vp8enc"];
/// `CaptureSettings.encode_node_index` where no setting names a node: pixelflux resolves it
/// through `auto_gpu` as the capture starts.
pub const AUTO_ENCODE_NODE: i32 = -2;
/// Non-bool settings a client may not change once an operator set them.
const OPERATOR_LOCKED_WHEN_OVERRIDDEN: &[&str] = &["scaling_dpi"];
/// Pipelines a session starts on or off by policy, each named by a `*_on_start` setting.
pub const START_STATE_PIPELINES: &[&str] = &["video", "audio", "microphone", "webcam", "gamepad"];
/// Bounds for client numerics without declared ones; the min is not 0 because -1 sentinels
/// must not be clamped up.
const INT_SETTING_DEFAULT_MAX: f64 = 1_000_000.0;
const INT_SETTING_DEFAULT_MIN: f64 = -1_000_000.0;

/// The full-frame encoder that streams `codec`; H.264 for a codec nothing streams.
pub fn encoder_for_codec(codec: &str) -> &'static str {
    ENCODER_CODECS
        .iter()
        .find(|(_, streamed)| *streamed == codec)
        .map(|(enc, _)| *enc)
        .unwrap_or("h264enc")
}

/// The pixelflux codec name an encoder selects; an unknown name selects H.264, the codec
/// every path serves.
pub fn codec_for_encoder(encoder: &str) -> &'static str {
    let canonical = canonical_encoder(encoder);
    ENCODER_CODECS
        .iter()
        .find(|(enc, _)| *enc == canonical)
        .map(|(_, codec)| *codec)
        .unwrap_or("h264")
}

/// Map an encoder alias onto the encoder that serves it; any other value comes back
/// trimmed and unchanged, so the caller's own validation decides what it means.
pub fn canonical_encoder(name: &str) -> String {
    let text = name.trim();
    let key = text.to_lowercase();
    ENCODER_ALIASES
        .iter()
        .find(|(alias, _)| *alias == key)
        .map(|(_, enc)| enc.to_string())
        .unwrap_or_else(|| text.to_string())
}

/// The label a codec goes by in logs and notices.
pub fn codec_label(codec: &str) -> &'static str {
    CODEC_LABELS
        .iter()
        .find(|(c, _)| *c == codec)
        .map(|(_, l)| *l)
        .unwrap_or("H.264")
}

/// The software encoder of each codec this pixelflux build carries, by codec name.
pub fn software_encoders() -> HashMap<&'static str, &'static str> {
    use pixelflux::encoders::{Codec, software_encoder};
    Codec::VIDEO
        .into_iter()
        .filter_map(|codec| software_encoder(codec).map(|enc| (codec.name(), enc.library)))
        .collect()
}

/// The codecs whose software encoder takes a `video_fullcolor` session as 4:4:4.
pub fn software_fullcolor() -> Vec<&'static str> {
    use pixelflux::encoders::{Codec, software_fullcolor};
    Codec::VIDEO
        .into_iter()
        .filter(|&codec| software_fullcolor(codec))
        .map(|codec| codec.name())
        .collect()
}

/// The formats each software encoder codes its codec in, as `chroma-depth` names.
pub fn software_formats() -> HashMap<&'static str, Vec<String>> {
    use pixelflux::encoders::{Codec, software_encoder, software_formats};
    Codec::VIDEO
        .into_iter()
        .filter(|&codec| software_encoder(codec).is_some())
        .map(|codec| (codec.name(), software_formats(codec).names()))
        .collect()
}

/// What the GPU behind a render node encodes, read once per node.
#[derive(Clone, Debug, Default)]
pub struct HardwareProbe {
    /// The backend serving each codec (`nvenc`, `vaapi`, `tegra`, `v4l2`).
    pub encoders: HashMap<&'static str, &'static str>,
    /// The codecs the node encodes 4:4:4.
    pub fullcolor: Vec<&'static str>,
    /// The formats the node encodes each codec in.
    pub formats: HashMap<&'static str, Vec<String>>,
}

/// Probe the hardware encoders of a render node (`AUTO_ENCODE_NODE` for pixelflux's pick).
pub fn hardware_probe(encode_node_index: i32, auto_gpu: &str) -> HardwareProbe {
    let mut probe = HardwareProbe::default();
    for (codec, backend, formats) in pixelflux::hardware_encoders(encode_node_index, auto_gpu) {
        probe.encoders.insert(codec.name(), backend);
        if formats.fullcolor {
            probe.fullcolor.push(codec.name());
        }
        probe.formats.insert(codec.name(), formats.names());
    }
    probe
}

/// Whether a session with this encoder is known to encode video on the CPU: a video encoder
/// encodes in software when forced; JPEG is no video.
pub fn software_video_path(encoder: &str, use_cpu: bool) -> bool {
    canonical_encoder(encoder) != "jpeg" && use_cpu
}

/// Derive whether a display's software encoder threads within a frame: a client's own
/// request wins, then an operator's value; unset, a VP8 session runs on one thread and any
/// other threaded, since libvpx's VP8 row threads cost screen content up to four times the
/// bits at the same quantizer.
pub fn effective_cpu_encode_threading(
    encoder: &str,
    requested: Option<bool>,
    default: bool,
    provided: bool,
) -> bool {
    if let Some(r) = requested {
        return r;
    }
    if provided {
        return default;
    }
    canonical_encoder(encoder) != "vp8enc"
}

/// Derive the software-encode flag for an encoder choice: a CPU-only encoder implies it,
/// any other honors the client's own request, else the server default.
pub fn effective_use_cpu(encoder: &str, requested: Option<bool>, default: bool) -> bool {
    if CPU_ONLY_ENCODERS.contains(&encoder) {
        return true;
    }
    requested.unwrap_or(default)
}

/// The directory the interposer sockets live in: the operator's, else the session's private
/// runtime directory, else `/tmp`.
pub fn socket_dir(configured: &str) -> String {
    if !configured.is_empty() {
        return configured.to_string();
    }
    std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/tmp".to_string())
}

/// Parse the `gpu_id` setting: `None` for empty or invalid (no explicit pick), `-1` for the
/// explicit software-encode request, or a device index.
pub fn parse_gpu_id(value: &str) -> Option<i32> {
    let gid: i32 = value.trim().parse().ok()?;
    (gid >= -1).then_some(gid)
}

/// Parse a DRI render-node path like `/dev/dri/renderD128` into an index (128 is 0), or `-1`
/// for an invalid, malformed, or empty path, which disables hardware encoding.
pub fn parse_dri_node_to_index(node_path: &str) -> i32 {
    if node_path.is_empty() || !node_path.starts_with("/dev/dri/renderD") {
        if !node_path.is_empty() {
            warn!(target: "display", "Invalid DRI node format: '{node_path}'. Expected '/dev/dri/renderD...'. VA-API will be disabled.");
        }
        return -1;
    }
    let number = node_path.rsplit("renderD").next().unwrap_or("");
    match number.parse::<i32>() {
        Ok(n) if n >= 128 => n - 128,
        Ok(n) => {
            warn!(target: "display", "Parsed DRI node number {n} from '{node_path}' is less than 128. Invalid.");
            -1
        }
        Err(e) => {
            warn!(target: "display", "Could not parse DRI node '{node_path}': {e}. VA-API will be disabled.");
            -1
        }
    }
}

/// The backend of one side (hardware or software) of a codec, for the client payload.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct CodecBackends {
    pub hardware: Option<String>,
    pub software: Option<String>,
    pub fullcolor: Sides<Option<bool>>,
    pub ten_bit: Sides<Option<TenBit>>,
}

/// A value for each side a codec may run on.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Sides<T> {
    pub hardware: T,
    pub software: T,
}

/// Whether a side takes a `video_10bit` session at 10 bits, by chroma.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct TenBit {
    #[serde(rename = "420")]
    pub chroma_420: bool,
    #[serde(rename = "444")]
    pub chroma_444: bool,
}

/// The resolved settings of this server process.
#[derive(Debug)]
pub struct Settings {
    values: HashMap<&'static str, Value>,
    overridden: HashSet<&'static str>,
    /// Each enum's and list's menu after the operator's narrowing.
    allowed: HashMap<&'static str, Vec<String>>,
    /// Each range's initial value after the override.
    default_value: HashMap<&'static str, Num>,
    /// The hardware probe of the encode node, once `resolve_encoder_backends` ran.
    hardware: Option<HardwareProbe>,
    operator_encoder_allowed: Option<Vec<String>>,
    operator_encoder_value: Option<String>,
    pre_webrtc_encoder: Option<String>,
    webrtc_encoder_fallback: Option<String>,
    /// A client asserted the encoder during the webrtc leg, so the stash does not apply.
    pub encoder_client_set: bool,
    /// Command-line tokens no setting took, warned about by name.
    pub unknown_args: Vec<String>,
}

fn definition(name: &str) -> Option<&'static SettingDef> {
    DEFINITIONS.iter().find(|d| d.name == name)
}

/// Split a comma-separated list into its non-empty trimmed items.
fn items(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// The command line's raw value for each setting: `--name value`, `--name=value`, the
/// underscore spelling, a unique prefix as argparse takes one, and a bare bool flag as
/// `true`. Tokens no setting takes are returned for the warning.
fn scan_args(args: &[String]) -> (HashMap<&'static str, String>, Vec<String>) {
    let mut found = HashMap::new();
    let mut unknown = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let token = &args[i];
        i += 1;
        if !token.starts_with("--") {
            unknown.push(token.clone());
            continue;
        }
        let (flag, inline) = match token.split_once('=') {
            Some((f, v)) => (f, Some(v.to_string())),
            None => (token.as_str(), None),
        };
        let spelled = flag.trim_start_matches("--").replace('-', "_");
        let exact = DEFINITIONS.iter().find(|d| d.name == spelled);
        let def = exact.or_else(|| {
            let candidates: Vec<&SettingDef> = DEFINITIONS
                .iter()
                .filter(|d| d.name.starts_with(&spelled))
                .collect();
            (candidates.len() == 1).then(|| candidates[0])
        });
        let Some(def) = def else {
            unknown.push(token.clone());
            continue;
        };
        let value = match inline {
            Some(v) => v,
            None if def.kind == Kind::Bool => {
                if i < args.len() && !args[i].starts_with('-') {
                    i += 1;
                    args[i - 1].clone()
                } else {
                    "true".to_string()
                }
            }
            None => {
                if i < args.len() {
                    i += 1;
                    args[i - 1].clone()
                } else {
                    unknown.push(token.clone());
                    continue;
                }
            }
        };
        found.insert(def.name, value);
    }
    (found, unknown)
}

impl Settings {
    /// Resolve every setting from the command line and the environment.
    ///
    /// Unrecognized arguments are tolerated so a wrapper can pass its own through, which
    /// also means a misspelled flag is accepted and ignored rather than rejected; the caller
    /// warns about each by name (`unknown_args`), since a setting that never took its value
    /// looks the same as one left at its default.
    pub fn parse(args: &[String], env: &dyn Fn(&str) -> Option<String>) -> Settings {
        let (cli, unknown) = scan_args(args);
        let mut s = Settings {
            values: HashMap::new(),
            overridden: HashSet::new(),
            allowed: DEFINITIONS
                .iter()
                .filter(|d| !d.allowed.is_empty())
                .map(|d| (d.name, d.allowed.iter().map(|a| a.to_string()).collect()))
                .collect(),
            default_value: DEFINITIONS
                .iter()
                .filter_map(|d| d.default_value.map(|v| (d.name, Num::of(v))))
                .collect(),
            hardware: None,
            operator_encoder_allowed: None,
            operator_encoder_value: None,
            pre_webrtc_encoder: None,
            webrtc_encoder_fallback: None,
            encoder_client_set: false,
            unknown_args: unknown,
        };
        for def in DEFINITIONS {
            let std_env = env(&format!("SELKIES_{}", def.name.to_uppercase()));
            let fallback = def.env_vars.iter().find_map(|v| env(v));
            let cli_val = cli.get(def.name).cloned();
            let mut is_override = cli_val.is_some() || std_env.is_some() || fallback.is_some();
            let mut raw = cli_val
                .or(std_env)
                .or(fallback)
                .unwrap_or_else(|| def.default.to_string());
            if is_override && def.kind != Kind::List && raw.trim().is_empty() {
                is_override = false;
                raw = def.default.to_string();
            }
            let value = match s.parse_value(def, &raw, &mut is_override) {
                Ok(v) => v,
                Err(e) => {
                    error!(target: "settings", "Could not parse setting '{}' with value '{raw}'. Using default. Error: {e}", def.name);
                    s.parse_value(def, def.default, &mut false)
                        .unwrap_or_else(|_| Value::Str(def.default.to_string()))
                }
            };
            if is_override {
                s.overridden.insert(def.name);
            }
            s.values.insert(def.name, value);
        }
        s.resolve_manual_resolution();
        s.post_process();
        s
    }

    /// Parse one setting's text by its type, narrowing menus and spans as an override does.
    fn parse_value(
        &mut self,
        def: &'static SettingDef,
        raw: &str,
        is_override: &mut bool,
    ) -> Result<Value, String> {
        match def.kind {
            Kind::Bool => {
                let locked = raw
                    .trim()
                    .to_lowercase()
                    .split('|')
                    .skip(1)
                    .any(|part| part.trim() == "locked");
                Ok(Value::Bool(BoolSetting {
                    on: parse_bool(Some(raw), false),
                    locked,
                }))
            }
            Kind::Enum | Kind::List => {
                let master: Vec<String> = def.allowed.iter().map(|a| a.to_string()).collect();
                if !*is_override {
                    return Ok(if def.kind == Kind::Enum {
                        Value::Str(def.default.to_string())
                    } else {
                        Value::List(items(def.default))
                    });
                }
                if def.kind == Kind::List
                    && matches!(raw.trim().to_lowercase().as_str(), "" | "none")
                {
                    self.allowed.insert(def.name, Vec::new());
                    return Ok(Value::List(Vec::new()));
                }
                let mut user: Vec<String> = items(raw);
                if def.name == "encoder" {
                    user = user.iter().map(|i| canonical_encoder(i)).collect();
                }
                let user: Vec<String> = user
                    .into_iter()
                    .map(|item| {
                        master
                            .iter()
                            .find(|m| m.to_lowercase() == item.to_lowercase())
                            .cloned()
                            .unwrap_or(item)
                    })
                    .collect();
                let valid: Vec<String> = user
                    .iter()
                    .filter(|i| master.contains(i))
                    .cloned()
                    .collect();
                if def.kind == Kind::Enum
                    && let Some((lo, hi)) = def.value_range
                    && user.len() == 1
                    && let Ok(n) = user[0].parse::<f64>()
                    && lo <= n
                    && n <= hi
                {
                    // '128000.0' must not reach int consumers.
                    let text = if n == n.trunc() {
                        format!("{}", n as i64)
                    } else {
                        user[0].clone()
                    };
                    return Ok(Value::Str(text));
                }
                if !valid.is_empty() {
                    self.allowed.insert(def.name, valid.clone());
                    return Ok(if def.kind == Kind::Enum {
                        Value::Str(valid[0].clone())
                    } else {
                        Value::List(valid)
                    });
                }
                *is_override = false;
                if !user.is_empty() {
                    warn!(target: "settings", "Invalid value(s) '{raw}' for {}; keeping the full allowed set with the system default.", def.name);
                }
                let defaults: Vec<String> = items(def.default)
                    .into_iter()
                    .filter(|i| master.contains(i))
                    .collect();
                Ok(if def.kind == Kind::Enum {
                    Value::Str(
                        defaults
                            .first()
                            .cloned()
                            .unwrap_or_else(|| def.default.to_string()),
                    )
                } else {
                    Value::List(defaults)
                })
            }
            Kind::Int | Kind::Float => {
                let text = raw.trim();
                let mut n: f64 = if def.kind == Kind::Int {
                    text.parse::<i64>().map_err(|e| e.to_string())? as f64
                } else {
                    text.parse::<f64>().map_err(|e| e.to_string())?
                };
                let orig = n;
                if let Some(lo) = def.min {
                    n = n.max(lo);
                }
                if let Some(hi) = def.max {
                    n = n.min(hi);
                }
                if n != orig {
                    warn!(target: "settings", "Setting '{}' value {} out of range [{},{}], clamped to {}",
                        def.name, Num::of(orig), def.min.map(|v| Num::of(v).to_string()).unwrap_or("None".into()),
                        def.max.map(|v| Num::of(v).to_string()).unwrap_or("None".into()), Num::of(n));
                }
                Ok(if def.kind == Kind::Int {
                    Value::Int(n as i64)
                } else {
                    Value::Float(n)
                })
            }
            Kind::Str => Ok(Value::Str(raw.to_string())),
            Kind::Range => {
                let mut span: Option<(Num, Num)> = None;
                let mut initial: Option<Num> = None;
                for token in items(raw) {
                    match split_span(&token) {
                        Some((a, b)) => span = Some((Num::parse(a)?, Num::parse(b)?)),
                        None => initial = Some(Num::parse(&token)?),
                    }
                }
                if span.is_none() && initial.is_none() {
                    return Err("no span or value given".into());
                }
                match span {
                    None => {
                        let initial = initial.unwrap();
                        let (def_lo, def_hi) = default_span(def)?;
                        self.default_value.insert(def.name, initial);
                        Ok(Value::Range(def_lo.min(initial), def_hi.max(initial)))
                    }
                    Some((a, b)) => {
                        let (lo, hi) = (a.min(b), a.max(b));
                        if let Some(initial) = initial {
                            let clamped = lo.max(initial.min(hi));
                            if clamped != initial {
                                warn!(target: "settings", "Setting '{}' initial value {initial} outside span {lo}-{hi}, clamped to {clamped}", def.name);
                            }
                            self.default_value.insert(def.name, clamped);
                        } else if let Some(current) = self.default_value.get(def.name).copied() {
                            self.default_value.insert(def.name, lo.max(current.min(hi)));
                        }
                        Ok(Value::Range(lo, hi))
                    }
                }
            }
        }
    }

    /// Manual-resolution mode follows a positive manual width or height override, or an
    /// explicit `manual_resolution`: 0 is both the built-in default and the no-manual-width
    /// sentinel, so a templated launcher emitting `--manual-width 0` must not lock the display
    /// to 1024x768.
    fn resolve_manual_resolution(&mut self) {
        let width_overridden =
            self.overridden.contains("manual_width") && self.int_value("manual_width") > 0;
        let height_overridden =
            self.overridden.contains("manual_height") && self.int_value("manual_height") > 0;
        let manual = self.bool_setting("manual_resolution").on;
        if width_overridden || height_overridden || manual {
            info!(target: "settings", "A manual resolution setting was activated; locking to manual mode.");
            self.values.insert(
                "manual_resolution",
                Value::Bool(BoolSetting {
                    on: true,
                    locked: true,
                }),
            );
            if self.int_value("manual_width") <= 0 {
                self.values.insert("manual_width", Value::Int(1024));
                info!(target: "settings", "Manual width not set or invalid, defaulting to 1024.");
            }
            if self.int_value("manual_height") <= 0 {
                self.values.insert("manual_height", Value::Int(768));
                info!(target: "settings", "Manual height not set or invalid, defaulting to 768.");
            }
        }
    }

    /// Normalize and cross-check settings whose meaning spans several entries: the subfolder
    /// stored the one way that composes (empty, or a leading slash and no trailing one), the
    /// transport mode lowercased, CRF alone where rate control is locked off, the paint-over
    /// and thread defaults, microphone requiring audio, the clipboard and start policies
    /// normalized, the TURN username defaulted, and a public listener as the both-family
    /// wildcard address.
    fn post_process(&mut self) {
        let subfolder = self
            .str_value("subfolder")
            .trim()
            .trim_matches('/')
            .to_string();
        self.set_str(
            "subfolder",
            if subfolder.is_empty() {
                String::new()
            } else {
                format!("/{subfolder}")
            },
        );

        let mode = self.str_value("mode").trim().to_lowercase();
        if mode != "websockets" && mode != "webrtc" {
            warn!(target: "settings", "Invalid mode value {:?}; using 'websockets'.", self.str_value("mode"));
            self.set_str("mode", "websockets".into());
        } else {
            self.set_str("mode", mode);
        }

        self.apply_webrtc_encoder_filter();

        if !self.bool_setting("enable_rate_control").on {
            if self.overridden.contains("rate_control_mode")
                && self.str_value("rate_control_mode") != "crf"
            {
                warn!(target: "settings", "Ignoring rate_control_mode={}: enable_rate_control is false, so the encoder runs CRF.", self.str_value("rate_control_mode"));
            }
            self.set_str("rate_control_mode", "crf".into());
            self.allowed.insert("rate_control_mode", vec!["crf".into()]);
        }
        self.resolve_paint_over_default();
        self.resolve_cpu_encode_threading_default();

        if !self.bool_setting("audio_enabled").on && self.bool_setting("microphone_enabled").on {
            warn!(target: "settings", "Microphone support requires audio to be enabled. Disabling microphone support.");
            let locked = self.bool_setting("microphone_enabled").locked;
            self.values.insert(
                "microphone_enabled",
                Value::Bool(BoolSetting { on: false, locked }),
            );
        }

        let clip = self
            .str_value("enable_clipboard")
            .split('|')
            .next()
            .unwrap_or("")
            .trim()
            .to_lowercase();
        let clip = match clip.as_str() {
            "1" => "true",
            "0" => "false",
            mode => mode,
        };
        if !matches!(clip, "true" | "false" | "in" | "out") {
            warn!(target: "settings", "Invalid enable_clipboard value {:?}; using 'true'.", self.str_value("enable_clipboard"));
            self.set_str("enable_clipboard", "true".into());
        } else {
            self.set_str("enable_clipboard", clip.into());
        }

        for name in ["microphone_on_start", "webcam_on_start"] {
            let raw = self.str_value(name).to_string();
            let mode = raw.split('|').next().unwrap_or("").trim().to_lowercase();
            // The bool spellings these settings once took are still read.
            let mode = match mode.as_str() {
                "1" => "true".to_string(),
                "0" => "false".to_string(),
                _ => mode,
            };
            if !matches!(mode.as_str(), "true" | "false" | "demand") {
                warn!(target: "settings", "Invalid {name} value {raw:?}; using 'false'.");
                self.set_str(name, "false".into());
            } else {
                self.set_str(name, mode);
            }
        }

        if self.str_value("turn_rest_username").is_empty() {
            self.set_str("turn_rest_username", "selkies".into());
        }
        if self.bool_setting("public").on {
            self.set_str("addr", "0.0.0.0,::".into());
        }
    }

    /// Whether a setting was given on the command line or in the environment; one left to
    /// its default, or overridden to the empty string, reads as not provided.
    pub fn was_provided(&self, name: &str) -> bool {
        self.overridden.contains(name)
    }

    /// The setting's resolved value.
    pub fn value(&self, name: &str) -> Option<&Value> {
        self.values.get(name)
    }

    /// Replace a string or enum setting's value (a transport switch, a client's encoder pick).
    pub fn set_str(&mut self, name: &'static str, value: String) {
        self.values.insert(name, Value::Str(value));
    }

    /// Replace a bool setting's value, keeping its lock.
    pub fn set_bool(&mut self, name: &'static str, on: bool) {
        let locked = self.bool_setting(name).locked;
        self.values
            .insert(name, Value::Bool(BoolSetting { on, locked }));
    }

    pub(crate) fn bool_setting(&self, name: &str) -> BoolSetting {
        match self.values.get(name) {
            Some(Value::Bool(b)) => *b,
            _ => BoolSetting {
                on: false,
                locked: false,
            },
        }
    }

    pub(crate) fn str_value(&self, name: &str) -> &str {
        match self.values.get(name) {
            Some(Value::Str(s)) => s,
            _ => "",
        }
    }

    pub(crate) fn list_value(&self, name: &str) -> &[String] {
        match self.values.get(name) {
            Some(Value::List(l)) => l,
            _ => &[],
        }
    }

    pub(crate) fn int_value(&self, name: &str) -> i64 {
        match self.values.get(name) {
            Some(Value::Int(i)) => *i,
            Some(Value::Float(f)) => *f as i64,
            _ => 0,
        }
    }

    pub(crate) fn float_value(&self, name: &str) -> f64 {
        match self.values.get(name) {
            Some(Value::Float(f)) => *f,
            Some(Value::Int(i)) => *i as f64,
            _ => 0.0,
        }
    }

    pub(crate) fn range_value(&self, name: &str) -> (Num, Num) {
        match self.values.get(name) {
            Some(Value::Range(lo, hi)) => (*lo, *hi),
            _ => (Num::Int(0), Num::Int(0)),
        }
    }

    /// A range's initial value: the operator's, else the table's.
    pub fn range_default(&self, name: &str) -> Option<Num> {
        self.default_value.get(name).copied()
    }

    /// The values an enum or list setting allows after the operator's narrowing, first the
    /// default; empty for a name that is neither.
    pub fn enum_allowed(&self, name: &str) -> Vec<String> {
        self.allowed.get(name).cloned().unwrap_or_default()
    }

    /// The DRI render-node index hardware encoders open: `encode_dri` names a node, else
    /// `gpu_id` picks one, else `AUTO_ENCODE_NODE`. `None` where no session encodes on
    /// hardware: `gpu_id` -1, an unusable `encode_dri`, or software encoding locked on.
    pub fn encode_node_index(&self) -> Option<i32> {
        let use_cpu = self.bool_setting("use_cpu");
        if use_cpu.on && use_cpu.locked {
            return None;
        }
        let node = self.str_value("encode_dri");
        if !node.is_empty() {
            let index = parse_dri_node_to_index(node);
            return (index >= 0).then_some(index);
        }
        match parse_gpu_id(self.str_value("gpu_id")) {
            None => Some(AUTO_ENCODE_NODE),
            Some(gid) if gid < 0 => None,
            Some(gid) => Some(gid),
        }
    }

    /// The backends that serve each video codec on this host, by codec name, or `None`
    /// before `resolve_encoder_backends` ran, so no consumer hides a choice on a guess.
    pub fn encoder_backends(&self) -> Option<HashMap<&'static str, CodecBackends>> {
        let hardware = self.hardware.as_ref()?;
        let software = software_encoders();
        let sw_fullcolor = software_fullcolor();
        let sw_formats = software_formats();
        let ten_bit = |backend: Option<&str>,
                       formats: &HashMap<&'static str, Vec<String>>,
                       codec: &str|
         -> Option<TenBit> {
            backend?;
            let listed = formats.get(codec).cloned().unwrap_or_default();
            Some(TenBit {
                chroma_420: listed.iter().any(|f| f == "420-10"),
                chroma_444: listed.iter().any(|f| f == "444-10"),
            })
        };
        Some(
            CODEC_LABELS
                .iter()
                .map(|(codec, _)| *codec)
                .filter(|codec| *codec != "jpeg")
                .map(|codec| {
                    let hw = hardware.encoders.get(codec).copied();
                    let sw = software.get(codec).copied();
                    (
                        codec,
                        CodecBackends {
                            hardware: hw.map(str::to_string),
                            software: sw.map(str::to_string),
                            fullcolor: Sides {
                                hardware: hw.map(|_| hardware.fullcolor.contains(&codec)),
                                software: sw.map(|_| sw_fullcolor.contains(&codec)),
                            },
                            ten_bit: Sides {
                                hardware: ten_bit(hw, &hardware.formats, codec),
                                software: ten_bit(sw, &sw_formats, codec),
                            },
                        },
                    )
                })
                .collect(),
        )
    }

    /// Whether a `video_fullcolor` session on this encoder streams 4:4:4 from this host: by
    /// the encode node's engine where the codec has one and software is not forced, else by
    /// the build's software encoder. A VA-API engine refuses a 4:4:4 it lacks rather than
    /// streaming 4:2:0, so that session is the software encoder's too.
    pub fn encoder_fullcolor(&self, encoder: &str, use_cpu: bool) -> Option<bool> {
        let backends = self.encoder_backends()?;
        let served = backends.get(codec_for_encoder(&canonical_encoder(encoder)))?;
        if use_cpu || served.hardware.is_none() {
            return served.fullcolor.software;
        }
        if served.fullcolor.hardware == Some(false)
            && served.hardware.as_deref() == Some("vaapi")
            && served.fullcolor.software == Some(true)
        {
            return Some(true);
        }
        served.fullcolor.hardware
    }

    /// Whether a `video_10bit` session on this encoder streams 10 bits from this host; each
    /// side answers at the chroma it would run.
    pub fn encoder_ten_bit(&self, encoder: &str, use_cpu: bool, fullcolor: bool) -> Option<bool> {
        let backends = self.encoder_backends()?;
        let served = backends.get(codec_for_encoder(&canonical_encoder(encoder)))?;
        let carried = |table: &Option<TenBit>, side_fullcolor: Option<bool>| -> Option<bool> {
            let t = table.as_ref()?;
            Some(if fullcolor && side_fullcolor == Some(true) {
                t.chroma_444
            } else {
                t.chroma_420
            })
        };
        let software = carried(&served.ten_bit.software, served.fullcolor.software);
        let hardware = carried(&served.ten_bit.hardware, served.fullcolor.hardware);
        if use_cpu || served.hardware.is_none() {
            return software;
        }
        if fullcolor
            && served.fullcolor.hardware == Some(false)
            && served.fullcolor.software == Some(true)
        {
            return software;
        }
        match hardware {
            Some(true) => Some(true),
            _ => software.filter(|&s| s).or(hardware),
        }
    }

    /// Whether a session on this encoder comes up on the codec it names rather than
    /// demoting; true for every encoder while the hardware side is unknown.
    pub fn encoder_served(&self, encoder: &str) -> bool {
        let Some(backends) = self.encoder_backends() else {
            return true;
        };
        let encoder = canonical_encoder(encoder);
        if CPU_ONLY_ENCODERS.contains(&encoder.as_str()) {
            return true;
        }
        backends
            .get(codec_for_encoder(&encoder))
            .map(|s| s.software.is_some() || s.hardware.is_some())
            .unwrap_or(false)
    }

    /// Where an encoder sits on the fallback ladder, lowest taken first.
    pub fn encoder_rung(&self, encoder: &str) -> (usize, usize) {
        let group = if encoder == "jpeg" {
            2
        } else {
            let hw = self.encoder_backends().and_then(|b| {
                b.get(codec_for_encoder(encoder))
                    .and_then(|s| s.hardware.clone())
            });
            if hw.is_some() { 0 } else { 1 }
        };
        let order = if group == 0 {
            HARDWARE_LADDER
        } else {
            ENCODER_LADDER
        };
        (
            group,
            order
                .iter()
                .position(|e| *e == encoder)
                .unwrap_or(order.len()),
        )
    }

    /// Learn once, at startup, which encoders this host serves, and narrow the encoder menu
    /// to them: an operator's pick that is not served falls back to the lowest served rung
    /// with a warning, and a menu narrowed to nothing reverts to every served encoder.
    pub fn resolve_encoder_backends(&mut self) {
        let auto_gpu = self.str_value("auto_gpu").to_string();
        self.hardware = Some(match self.encode_node_index() {
            None => HardwareProbe::default(),
            Some(node) => hardware_probe(node, &auto_gpu),
        });
        let operator_allowed = self
            .operator_encoder_allowed
            .clone()
            .unwrap_or_else(|| self.enum_allowed("encoder"));
        let mut served: Vec<String> = operator_allowed
            .iter()
            .filter(|e| self.encoder_served(e))
            .cloned()
            .collect();
        let dropped: Vec<String> = operator_allowed
            .iter()
            .filter(|e| !served.contains(e))
            .cloned()
            .collect();
        if served.is_empty() {
            served = ENCODER_CODECS
                .iter()
                .map(|(e, _)| e.to_string())
                .filter(|e| self.encoder_served(e))
                .collect();
            warn!(target: "settings", "No encoder of the configured menu ({}) is served on this host; offering {}.", operator_allowed.join(", "), served.join(", "));
        } else if !dropped.is_empty() {
            info!(target: "settings", "Encoders not served on this host are left off the menu: {}", dropped.join(", "));
        }
        self.operator_encoder_allowed = Some(served.clone());
        let value = self
            .operator_encoder_value
            .clone()
            .unwrap_or_else(|| self.str_value("encoder").to_string());
        if !served.contains(&value) {
            let fallback = served
                .iter()
                .min_by_key(|e| self.encoder_rung(e))
                .cloned()
                .unwrap_or_else(|| "h264enc".into());
            warn!(target: "settings", "Encoder {value:?} is not served on this host; using {fallback:?}.");
            self.operator_encoder_value = Some(fallback.clone());
            if self.str_value("encoder") == value {
                self.set_str("encoder", fallback);
            }
        }
        self.allowed.insert("encoder", served);
        self.apply_webrtc_encoder_filter();
    }

    /// Bring the `encoder` knob, the published menu and the value, in line with the
    /// transport: in webrtc mode both are brought into the WebRTC-producible subset of the
    /// operator's menu and a choice that cannot stream falls back with a warning; a switch
    /// back to websockets restores the operator's menu and value, and a client's
    /// websockets-only pick stashed by the clamp where the client asserted nothing newer.
    pub fn apply_webrtc_encoder_filter(&mut self) {
        if self.operator_encoder_allowed.is_none() {
            self.operator_encoder_allowed = Some(self.enum_allowed("encoder"));
            self.operator_encoder_value = Some(self.str_value("encoder").to_string());
        }
        let operator_allowed = self.operator_encoder_allowed.clone().unwrap();
        if self.str_value("mode") != "webrtc" {
            self.allowed.insert("encoder", operator_allowed.clone());
            if !operator_allowed.contains(&self.str_value("encoder").to_string()) {
                let v = self.operator_encoder_value.clone().unwrap_or_default();
                self.set_str("encoder", v);
            }
            if let Some(stash) = self.pre_webrtc_encoder.take() {
                if !self.encoder_client_set
                    && Some(self.str_value("encoder").to_string()) == self.webrtc_encoder_fallback
                    && operator_allowed.contains(&stash)
                {
                    self.set_str("encoder", stash);
                }
                self.webrtc_encoder_fallback = None;
            }
            return;
        }
        let mut allowed: Vec<String> = operator_allowed
            .iter()
            .filter(|e| WEBRTC_ENCODER_CHOICES.contains(&e.as_str()))
            .cloned()
            .collect();
        if allowed.is_empty() {
            allowed = WEBRTC_ENCODER_CHOICES
                .iter()
                .filter(|e| self.encoder_served(e))
                .map(|e| e.to_string())
                .collect();
        }
        self.allowed.insert("encoder", allowed.clone());
        let current = self.str_value("encoder").to_string();
        if !allowed.contains(&current) {
            let fallback = allowed
                .iter()
                .min_by_key(|e| self.encoder_rung(e))
                .cloned()
                .unwrap_or_else(|| "h264enc".into());
            if self.was_provided("encoder") {
                warn!(target: "settings", "Encoder {current:?} is not available for WebRTC ({}); using {fallback:?}.", allowed.join(", "));
            }
            self.pre_webrtc_encoder = Some(current);
            self.webrtc_encoder_fallback = Some(fallback.clone());
            self.encoder_client_set = false;
            self.set_str("encoder", fallback);
        } else {
            self.pre_webrtc_encoder = None;
            self.webrtc_encoder_fallback = None;
        }
    }

    /// Default paint-over off while Turbo drives a video encoder and on otherwise, JPEG
    /// included; an operator's value wins. Called again on a transport switch.
    pub fn resolve_paint_over_default(&mut self) {
        if self.was_provided("use_paint_over_quality") {
            return;
        }
        let on = canonical_encoder(self.str_value("encoder")) == "jpeg"
            || !self.bool_setting("video_streaming_mode").on;
        self.set_bool("use_paint_over_quality", on);
    }

    /// Default the software encode threads off for a VP8 session and on for any other, so
    /// the payload a page first takes already says what the session encoder runs with.
    pub fn resolve_cpu_encode_threading_default(&mut self) {
        if self.was_provided("cpu_encode_threading") {
            return;
        }
        let on = effective_cpu_encode_threading(self.str_value("encoder"), None, true, false);
        self.set_bool("cpu_encode_threading", on);
    }

    /// Whether a connecting page starts with `pipeline` on: a shared viewer and a second
    /// display page keep the built-in start state whatever the settings say, and the
    /// microphone and webcam policies' `demand` starts nothing at connect.
    pub fn pipeline_starts_on(&self, pipeline: &str, display_id: &str, viewer: bool) -> bool {
        if viewer || display_id != "primary" {
            return matches!(pipeline, "video" | "audio" | "gamepad");
        }
        let name = format!("{pipeline}_on_start");
        match self.values.get(name.as_str()) {
            Some(Value::Str(s)) => s == "true",
            Some(Value::Bool(b)) => b.on,
            _ => false,
        }
    }

    /// The `allowed` list a client is told about for an enum, with the server's resolved
    /// value merged in when it sits off the curated stops but inside the value range, so
    /// every dashboard's control lands on the value the server runs. Numeric lists stay sorted.
    fn published_enum_allowed(&self, def: &SettingDef, value: &str) -> Vec<String> {
        let allowed = self.enum_allowed(def.name);
        if allowed.is_empty() || allowed.iter().any(|a| a == value) {
            return allowed;
        }
        let Some((lo, hi)) = def.value_range else {
            return allowed;
        };
        let Ok(numeric) = value.parse::<f64>() else {
            return allowed;
        };
        let Ok(numbers) = allowed
            .iter()
            .map(|a| a.parse::<f64>())
            .collect::<Result<Vec<_>, _>>()
        else {
            return allowed;
        };
        if !(lo <= numeric && numeric <= hi) {
            return allowed;
        }
        let mut merged: Vec<(f64, String)> = numbers.into_iter().zip(allowed).collect();
        merged.push((numeric, value.to_string()));
        merged.sort_by(|a, b| a.0.total_cmp(&b.0));
        merged.into_iter().map(|(_, item)| item).collect()
    }

    /// The client-facing settings snapshot shared by both transports: every setting that is
    /// neither server-local nor sensitive with its `value`, `locked` and `overridden` flags
    /// and its enum or range metadata, plus the clipboard gates derived from
    /// `enable_clipboard`, the start and on-demand booleans of the microphone and webcam
    /// policies, `encoder_backends` once the startup probe ran, and `apps_local_repo`.
    pub fn client_payload(&self) -> Map<String, Json> {
        let mut out = Map::new();
        for def in DEFINITIONS {
            if def.client_excluded || def.sensitive {
                continue;
            }
            let Some(value) = self.values.get(def.name) else {
                continue;
            };
            let mut entry = Map::new();
            match value {
                Value::Bool(b) => {
                    entry.insert("value".into(), json!(b.on));
                    entry.insert("locked".into(), json!(b.locked));
                }
                other => {
                    entry.insert("value".into(), other.to_json());
                }
            }
            let overridden = self.overridden.contains(def.name);
            entry.insert("overridden".into(), json!(overridden));
            if OPERATOR_LOCKED_WHEN_OVERRIDDEN.contains(&def.name) && overridden {
                entry.insert("locked".into(), json!(true));
            }
            match def.kind {
                Kind::Range => {
                    if let Value::Range(lo, hi) = value {
                        entry.insert("min".into(), json!(lo));
                        entry.insert("max".into(), json!(hi));
                    }
                    if let Some(d) = self.default_value.get(def.name) {
                        entry.insert("default".into(), json!(d));
                    }
                }
                Kind::Enum => {
                    if self.allowed.contains_key(def.name) {
                        let v = match value {
                            Value::Str(s) => s.clone(),
                            other => other.to_json().to_string(),
                        };
                        entry.insert(
                            "allowed".into(),
                            json!(self.published_enum_allowed(def, &v)),
                        );
                    }
                }
                Kind::List => {
                    if let Some(a) = self.allowed.get(def.name) {
                        entry.insert("allowed".into(), json!(a));
                    }
                }
                _ => {}
            }
            out.insert(def.name.to_string(), Json::Object(entry));
        }
        let clip = self.str_value("enable_clipboard");
        out.insert(
            "clipboard_enabled".into(),
            json!({"value": clip != "false"}),
        );
        out.insert(
            "clipboard_in_enabled".into(),
            json!({"value": clip == "true" || clip == "in"}),
        );
        out.insert(
            "clipboard_out_enabled".into(),
            json!({"value": clip == "true" || clip == "out"}),
        );
        for pipeline in ["microphone", "webcam"] {
            let mode = self.str_value(&format!("{pipeline}_on_start")).to_string();
            if let Some(Json::Object(entry)) = out.get_mut(&format!("{pipeline}_on_start")) {
                entry.insert("value".into(), json!(mode == "true"));
            }
            out.insert(
                format!("{pipeline}_on_demand"),
                json!({"value": mode == "demand"}),
            );
        }
        if let Some(backends) = self.encoder_backends() {
            out.insert("encoder_backends".into(), json!({"value": backends}));
        }
        out.insert(
            "apps_local_repo".into(),
            json!({"value": !self.apps_repo_path().is_empty()}),
        );
        out
    }

    /// The proot-apps local repository as an absolute path, or empty for the remote one.
    pub fn apps_repo_path(&self) -> String {
        let folder = self.str_value("apps_repo_folder").trim();
        if folder.is_empty() {
            return String::new();
        }
        let expanded = crate::core::expand_user(folder);
        std::path::absolute(&expanded)
            .map(|p| p.components().collect::<std::path::PathBuf>())
            .unwrap_or(expanded)
            .to_string_lossy()
            .into_owned()
    }

    /// Clamp or validate one client-provided setting against the server's limits, the
    /// sanitizer shared by both transports: ranges clamp into the span (integral values stay
    /// ints), ints and floats into declared bounds, locked bools keep the server value, an
    /// enum outside `allowed` falls back to the server's own value, never to `allowed[0]`.
    /// A `None` client value resolves to the server value or default for the type; an
    /// unknown name yields `None`.
    pub fn sanitize_client_setting(
        &self,
        name: &str,
        client_value: Option<&Json>,
        target: &str,
    ) -> Option<Json> {
        let def = definition(name)?;
        let server = self.values.get(name)?;
        let Some(client) = client_value else {
            return Some(match (def.kind, server) {
                (Kind::Range, Value::Range(lo, hi)) => {
                    if lo == hi {
                        json!(lo)
                    } else {
                        self.default_value
                            .get(name)
                            .map(|d| json!(d))
                            .unwrap_or(Json::Null)
                    }
                }
                (Kind::Bool, Value::Bool(b)) => json!(b.on),
                (_, other) => other.to_json(),
            });
        };
        let as_text = |v: &Json| -> String {
            match v {
                Json::String(s) => s.clone(),
                Json::Bool(b) => {
                    if *b {
                        "True".into()
                    } else {
                        "False".into()
                    }
                }
                other => other.to_string(),
            }
        };
        let as_number = |v: &Json| -> Option<f64> {
            match v {
                Json::Number(n) => n.as_f64(),
                Json::String(s) => s.trim().parse().ok(),
                Json::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
                _ => None,
            }
        };
        let fallback = || -> Json {
            self.default_value
                .get(name)
                .map(|d| json!(d))
                .unwrap_or_else(|| match def.kind {
                    Kind::Int => def.default.parse::<i64>().map_or(Json::Null, |n| json!(n)),
                    Kind::Float => def.default.parse::<f64>().map_or(Json::Null, |n| json!(n)),
                    _ => json!(def.default),
                })
        };
        match (def.kind, server) {
            (Kind::Range, Value::Range(lo, hi)) => {
                let Some(n) = as_number(client).filter(|n| n.is_finite()) else {
                    return Some(fallback());
                };
                let numeric = Num::of(n);
                let sanitized = lo.max(numeric.min(*hi));
                if sanitized != numeric {
                    warn!(target: target, "Client value for '{name}' ({}) was clamped to {sanitized} (server range: {lo}-{hi}).", as_text(client));
                }
                Some(json!(sanitized))
            }
            (Kind::Enum, Value::Str(server_value)) => {
                let allowed = self.enum_allowed(name);
                let mut text = as_text(client);
                if name == "encoder" {
                    text = canonical_encoder(&text);
                }
                if allowed.contains(&text) {
                    return Some(json!(text));
                }
                let server_default = if server_value.is_empty() {
                    def.default.to_string()
                } else {
                    server_value.clone()
                };
                if text == server_default {
                    return Some(json!(server_default));
                }
                warn!(target: target, "Client value for '{name}' ('{text}') is not in the allowed list {allowed:?}. Using server default '{server_default}'.");
                Some(json!(server_default))
            }
            (Kind::Int | Kind::Float, _) => {
                let Some(n) = as_number(client).filter(|n| n.is_finite()) else {
                    return Some(fallback());
                };
                let sanitized = if def.kind == Kind::Int { n.trunc() } else { n };
                let lo = def.min.unwrap_or(INT_SETTING_DEFAULT_MIN);
                let hi = def.max.unwrap_or(INT_SETTING_DEFAULT_MAX);
                let clamped = sanitized.clamp(lo, hi);
                if clamped != sanitized {
                    warn!(target: target, "Client value for '{name}' ({}) was clamped to {} (bounds: {}-{}).", as_text(client), Num::of(clamped), Num::of(lo), Num::of(hi));
                }
                Some(if def.kind == Kind::Int {
                    json!(clamped as i64)
                } else {
                    json!(clamped)
                })
            }
            (Kind::Bool, Value::Bool(b)) => {
                let client_bool = matches!(as_text(client).to_lowercase().as_str(), "true" | "1");
                if b.locked {
                    if client_bool != b.on {
                        warn!(target: target, "Client tried to change locked setting '{name}' to '{client_bool}'. Request ignored, using server value '{}'.", b.on);
                    }
                    return Some(json!(b.on));
                }
                Some(json!(client_bool))
            }
            _ => Some(client.clone()),
        }
    }

    /// The `--help` text: every setting with its flags, environment names, and help.
    pub fn help_text(epilog: &str) -> String {
        let mut out = String::from(
            "usage: selkies [options]\n\nSelkies WebSocket Streaming Server\n\noptions:\n",
        );
        for def in DEFINITIONS {
            let mut flags = format!("--{}", def.name.replace('_', "-"));
            if def.name.contains('_') {
                flags.push_str(&format!(", --{}", def.name));
            }
            let mut env = format!("Env: SELKIES_{}", def.name.to_uppercase());
            if !def.env_vars.is_empty() {
                env.push_str(&format!(" (or {})", def.env_vars.join(", ")));
            }
            out.push_str(&format!("  {flags}\n      {} ({env})\n", def.help));
        }
        out.push('\n');
        out.push_str(epilog);
        out.push('\n');
        out
    }
}

/// Split `a-b` where both sides are numbers; a lone negative number is not a span.
fn split_span(token: &str) -> Option<(&str, &str)> {
    let bytes = token.as_bytes();
    for (i, &b) in bytes.iter().enumerate().skip(1) {
        if b == b'-' {
            let (a, rest) = token.split_at(i);
            let b_part = &rest[1..];
            if a.trim().parse::<f64>().is_ok() && b_part.trim().parse::<f64>().is_ok() {
                return Some((a.trim(), b_part.trim()));
            }
        }
    }
    None
}

/// A range setting's built-in span.
fn default_span(def: &SettingDef) -> Result<(Num, Num), String> {
    let (a, b) =
        split_span(def.default).ok_or_else(|| format!("default {:?} is no span", def.default))?;
    let (lo, hi) = (Num::parse(a)?, Num::parse(b)?);
    Ok((lo.min(hi), lo.max(hi)))
}

static GLOBAL: OnceLock<RwLock<Settings>> = OnceLock::new();

/// Install the process's settings; a second call is ignored.
pub fn install(settings: Settings) {
    let _ = GLOBAL.set(RwLock::new(settings));
}

/// The process's settings for reading.
pub fn settings() -> RwLockReadGuard<'static, Settings> {
    GLOBAL.get().expect("settings installed").read().unwrap()
}

/// The process's settings for a change (a transport switch, a client's pick).
pub fn settings_mut() -> RwLockWriteGuard<'static, Settings> {
    GLOBAL.get().expect("settings installed").write().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str], env: &[(&str, &str)]) -> Settings {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let env_map: HashMap<String, String> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Settings::parse(&args, &|k| env_map.get(k).cloned())
    }

    #[test]
    fn defaults_resolve() {
        let s = parse(&[], &[]);
        assert_eq!(s.port(), 8080);
        assert_eq!(s.addr(), "localhost");
        assert!(!s.public().on);
        assert_eq!(s.framerate(), (Num::Int(8), Num::Int(240)));
        assert_eq!(s.range_default("framerate"), Some(Num::Int(60)));
        assert_eq!(s.mode(), "websockets");
        assert!(!s.was_provided("port"));
    }

    #[test]
    fn precedence_cli_over_env_over_fallback() {
        let s = parse(
            &["--port", "9000"],
            &[("SELKIES_PORT", "9100"), ("CUSTOM_WS_PORT", "9200")],
        );
        assert_eq!(s.port(), 9000);
        let s = parse(&[], &[("SELKIES_PORT", "9100"), ("CUSTOM_WS_PORT", "9200")]);
        assert_eq!(s.port(), 9100);
        let s = parse(&[], &[("CUSTOM_WS_PORT", "9200")]);
        assert_eq!(s.port(), 9200);
        assert!(s.was_provided("port"));
    }

    #[test]
    fn bool_flag_bare_locked_and_underscore_spelling() {
        let s = parse(&["--public"], &[]);
        assert!(s.public().on);
        assert_eq!(s.addr(), "0.0.0.0,::");
        let s = parse(&["--enable_resize=false|locked"], &[]);
        assert_eq!(
            s.enable_resize(),
            BoolSetting {
                on: false,
                locked: true
            }
        );
        let s = parse(&[], &[("SELKIES_ENABLE_RESIZE", "TRUE")]);
        assert!(s.enable_resize().on);
    }

    #[test]
    fn empty_override_means_default_and_unknown_args_kept() {
        let s = parse(&["--bogus-flag", "1", "--port="], &[]);
        assert_eq!(s.port(), 8080);
        assert!(!s.was_provided("port"));
        assert_eq!(
            s.unknown_args,
            vec!["--bogus-flag".to_string(), "1".to_string()]
        );
    }

    #[test]
    fn range_syntax() {
        let s = parse(&["--framerate", "30"], &[]);
        assert_eq!(s.framerate(), (Num::Int(8), Num::Int(240)));
        assert_eq!(s.range_default("framerate"), Some(Num::Int(30)));
        let s = parse(&["--framerate", "300"], &[]);
        assert_eq!(s.framerate(), (Num::Int(8), Num::Int(300)));
        let s = parse(&["--framerate", "60,10-120"], &[]);
        assert_eq!(s.framerate(), (Num::Int(10), Num::Int(120)));
        let s = parse(&["--framerate", "500,10-120"], &[]);
        assert_eq!(s.range_default("framerate"), Some(Num::Int(120)));
        let s = parse(&["--framerate", "60-60"], &[]);
        assert_eq!(s.framerate(), (Num::Int(60), Num::Int(60)));
    }

    #[test]
    fn enum_narrowing_and_invalid_override() {
        let s = parse(&["--encoder", "JPEG,h264enc"], &[]);
        assert_eq!(s.encoder(), "jpeg");
        assert_eq!(s.enum_allowed("encoder"), vec!["jpeg", "h264enc"]);
        let s = parse(&["--encoder", "x264enc"], &[]);
        assert_eq!(s.encoder(), "h264enc");
        let s = parse(&["--encoder", "bogus"], &[]);
        assert_eq!(s.encoder(), "h264enc");
        assert!(!s.was_provided("encoder"));
        assert!(s.enum_allowed("encoder").len() > 2);
    }

    /// An operator or a stored client setting naming an encoder off the menu, such as
    /// `h264enc-striped`, gets the server's encoder.
    #[test]
    fn an_encoder_off_the_menu_falls_back_to_the_server_value() {
        let s = parse(&["--encoder", "h264enc-striped"], &[]);
        assert_eq!(s.encoder(), "h264enc");
        assert!(!s.was_provided("encoder"));
        assert_eq!(
            s.sanitize_client_setting("encoder", Some(&json!("h264enc-striped")), "ws"),
            Some(json!("h264enc"))
        );
    }

    #[test]
    fn numeric_enum_in_range_value_keeps_menu() {
        let s = parse(&["--audio-bitrate", "6000"], &[]);
        assert_eq!(s.audio_bitrate(), "6000");
        let payload = s.client_payload();
        let allowed = payload["audio_bitrate"]["allowed"].as_array().unwrap();
        assert!(allowed.iter().any(|v| v == "6000"));
        assert!(allowed.len() > 1);
    }

    #[test]
    fn list_none_disables() {
        let s = parse(&["--file-transfers", "none"], &[]);
        assert!(s.file_transfers().is_empty());
        assert!(s.was_provided("file_transfers"));
    }

    #[test]
    fn manual_resolution_follows_a_positive_width() {
        let s = parse(&["--manual-width", "0"], &[]);
        assert!(!s.manual_resolution().on);
        let s = parse(&["--manual-width", "1280"], &[]);
        assert_eq!(
            s.manual_resolution(),
            BoolSetting {
                on: true,
                locked: true
            }
        );
        assert_eq!(s.manual_height(), 768);
    }

    #[test]
    fn post_processing_rules() {
        let s = parse(
            &[
                "--subfolder",
                "desk/",
                "--mode",
                "WebRTC",
                "--enable-rate-control=false",
                "--rate-control-mode",
                "cbr",
            ],
            &[],
        );
        assert_eq!(s.subfolder(), "/desk");
        assert_eq!(s.mode(), "webrtc");
        assert_eq!(s.rate_control_mode(), "crf");
        assert_eq!(s.enum_allowed("rate_control_mode"), vec!["crf"]);
        assert!(!s.enum_allowed("encoder").iter().any(|e| e == "jpeg"));
        let s = parse(&["--audio-enabled=false", "--microphone-enabled"], &[]);
        assert!(!s.microphone_enabled().on);
        let s = parse(&["--microphone-on-start", "1"], &[]);
        assert_eq!(s.microphone_on_start(), "true");
    }

    #[test]
    fn clipboard_accepts_boolean_spellings() {
        for (value, expected) in [
            ("0", "false"),
            ("1", "true"),
            ("0|locked", "false"),
            (" False ", "false"),
            ("out", "out"),
        ] {
            let s = parse(&[], &[("SELKIES_ENABLE_CLIPBOARD", value)]);
            assert_eq!(s.enable_clipboard(), expected, "value: {value:?}");
        }
    }

    #[test]
    fn sanitizer_clamps_and_respects_locks() {
        let s = parse(&["--enable-resize=true|locked"], &[]);
        assert_eq!(
            s.sanitize_client_setting("framerate", Some(&json!(1000)), "ws"),
            Some(json!(240))
        );
        assert_eq!(
            s.sanitize_client_setting("framerate", Some(&json!(59.94)), "ws"),
            Some(json!(59.94))
        );
        assert_eq!(
            s.sanitize_client_setting("enable_resize", Some(&json!(false)), "ws"),
            Some(json!(true))
        );
        assert_eq!(
            s.sanitize_client_setting("encoder", Some(&json!("bogus")), "ws"),
            Some(json!("h264enc"))
        );
        assert_eq!(
            s.sanitize_client_setting("framerate", None, "ws"),
            Some(json!(60))
        );
        assert_eq!(
            s.sanitize_client_setting("no_such_setting", None, "ws"),
            None
        );
    }

    #[test]
    fn payload_shape() {
        let s = parse(&[], &[]);
        let p = s.client_payload();
        assert!(p.get("port").is_none());
        assert_eq!(p["framerate"]["min"], json!(8));
        assert_eq!(p["framerate"]["default"], json!(60));
        assert_eq!(p["enable_resize"]["locked"], json!(false));
        assert_eq!(p["clipboard_enabled"]["value"], json!(true));
        assert_eq!(p["microphone_on_demand"]["value"], json!(false));
        assert!(p.get("encoder_backends").is_none());
    }
}
