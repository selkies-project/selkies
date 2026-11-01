/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What the data plane remembers: each display's tunables and owner, the session's seeds for
//! later displays, and the tables of who is connected, beside whom, and paused.
//!
//! The state is split by who reads it and how often, and that split is the lock order:
//! `ServerState` (control-plane sections) may be held while taking a display's
//! `DisplayLink`, which may be held while taking a relay's own lock, which may be held while
//! taking `Registry`, the table every frame reads; nothing runs in the other direction, and
//! no guard is held across an await. So a frame's fan-out never waits on a settings pass, and
//! a backpressure check reaches the relay and the encoder without touching the control state.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;

use parking_lot::Mutex;
use pixelflux::CaptureSettings;
use serde_json::{Map, Value as Json, json};
use tokio::task::JoinHandle;

use crate::core::{DisplayId, PRIMARY_DISPLAY};
use crate::display::Layouts;
use crate::display::capture::CommonFrames;
use crate::files::pacing::TransferPacer;
use crate::settings::{
    self, Num, STREAM_SETTINGS, effective_cpu_encode_threading, effective_use_cpu, settings,
};
use crate::stats::StreamWatch;
use crate::ws::client::{Client, ClientId};
use crate::ws::host::CaptureHandle;
use crate::ws::link::DisplayLink;
use crate::ws::protocol::ClientSettings;
use crate::ws::relay::VideoRelay;
use crate::ws::uplink::Gauge;

/// A setting's value as the sanitizer answers for no client value: the operator's, else the
/// built-in default.
fn initial(name: &str) -> Json {
    settings()
        .sanitize_client_setting(name, None, "ws")
        .unwrap_or(Json::Null)
}

fn initial_i64(name: &str, fallback: i64) -> i64 {
    json_i64(&initial(name)).unwrap_or(fallback)
}

fn initial_bool(name: &str) -> bool {
    initial(name).as_bool().unwrap_or(false)
}

/// A sanitized number or numeric string as an integer.
pub fn json_i64(value: &Json) -> Option<i64> {
    json_f64(value).map(|v| v as i64)
}

/// A sanitized number or numeric string as a float.
pub fn json_f64(value: &Json) -> Option<f64> {
    match value {
        Json::Number(n) => n.as_f64(),
        Json::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// The session's values a later display seeds from; the primary's picks move them.
#[derive(Clone, Debug, PartialEq)]
pub struct Seeds {
    pub encoder: String,
    pub framerate: f64,
    pub video_crf: i64,
    pub video_bitrate: f64,
    pub video_fullcolor: bool,
    pub video_10bit: bool,
    pub video_streaming_mode: bool,
    pub jpeg_quality: i64,
    pub paint_over_jpeg_quality: i64,
    pub use_cpu: bool,
    pub cpu_encode_threading: bool,
    pub cpu_encode_threading_provided: bool,
    pub use_paint_over_quality: bool,
    pub video_paintover_crf: i64,
    pub video_paintover_burst_frames: i64,
    /// `cbr` or `crf`.
    pub rc_mode: String,
    pub audio_bitrate: i64,
    pub display_width: i32,
    pub display_height: i32,
}

impl Seeds {
    /// The seeds the settings start a session with: a manual resolution, even-masked, where
    /// one is configured, else 1024x768, for a capture that starts before any client sized
    /// the display.
    pub fn from_settings() -> Seeds {
        let s = settings();
        let (mut w, mut h) = (1024, 768);
        if s.manual_resolution().on && s.manual_width() > 0 && s.manual_height() > 0 {
            w = (s.manual_width() & !1) as i32;
            h = (s.manual_height() & !1) as i32;
        }
        let rc_mode = if s.enable_rate_control().on {
            s.rate_control_mode().to_string()
        } else {
            "crf".to_string()
        };
        let encoder = s.encoder().to_string();
        let audio_bitrate = s
            .audio_bitrate()
            .parse::<f64>()
            .map(|v| v as i64)
            .unwrap_or(128_000);
        let provided = s.was_provided("cpu_encode_threading");
        drop(s);
        Seeds {
            encoder,
            framerate: json_f64(&initial("framerate")).unwrap_or(60.0),
            video_crf: initial_i64("video_crf", 25),
            video_bitrate: json_f64(&initial("video_bitrate")).unwrap_or(8000.0),
            video_fullcolor: initial_bool("video_fullcolor"),
            video_10bit: initial_bool("video_10bit"),
            video_streaming_mode: initial_bool("video_streaming_mode"),
            jpeg_quality: initial_i64("jpeg_quality", 75),
            paint_over_jpeg_quality: initial_i64("paint_over_jpeg_quality", 95),
            use_cpu: initial_bool("use_cpu"),
            cpu_encode_threading: initial_bool("cpu_encode_threading"),
            cpu_encode_threading_provided: provided,
            use_paint_over_quality: initial_bool("use_paint_over_quality"),
            video_paintover_crf: initial_i64("video_paintover_crf", 18),
            video_paintover_burst_frames: initial_i64("video_paintover_burst_frames", 5),
            rc_mode,
            audio_bitrate,
            display_width: w,
            display_height: h,
        }
    }

    /// Whether a display on `encoder` threads its software encode: the client's request, the
    /// operator's value, or what the encoder defaults to.
    pub fn threading_for(&self, encoder: &str, requested: Option<bool>) -> bool {
        effective_cpu_encode_threading(
            encoder,
            requested,
            self.cpu_encode_threading,
            self.cpu_encode_threading_provided,
        )
    }
}

/// One display's entry: what it streams with and how its page sized it. The link state lives
/// beside it in `Registry::links`.
#[derive(Clone, Debug)]
pub struct DisplayState {
    pub tab_id: Option<String>,
    pub decode_pace: Option<f64>,
    pub width: i32,
    pub height: i32,
    pub position: String,
    pub video_active: bool,
    pub encoder: String,
    pub encoder_fallback: bool,
    pub framerate: f64,
    pub video_crf: i64,
    pub video_fullcolor: bool,
    pub video_10bit: bool,
    pub video_streaming_mode: bool,
    pub jpeg_quality: i64,
    pub paint_over_jpeg_quality: i64,
    pub use_cpu: bool,
    pub use_cpu_requested: Option<bool>,
    pub cpu_encode_threading: bool,
    pub cpu_encode_threading_requested: Option<bool>,
    pub video_paintover_crf: i64,
    pub video_paintover_burst_frames: i64,
    pub use_paint_over_quality: bool,
    pub rate_control_mode: String,
    pub video_bitrate: f64,
    pub force_aligned_resolution: bool,
    /// The density in the sanitizer's normalized form (an enum, so text).
    pub scaling_dpi: Option<String>,
    pub scale: f64,
    pub client_scale: Option<f64>,
    pub client_stream_box: Option<(f64, f64, f64, f64)>,
    pub client_stream_box_at: f64,
    pub stop_signaled: bool,
}

/// The fields whose change reaches a running capture, compared across one `SETTINGS`.
#[derive(Clone, Debug, PartialEq)]
pub struct VideoParams {
    pub restart: (String, bool, bool, bool, bool, String),
    pub scale: f64,
    pub live: (f64, i64, bool, i64, i64, i64, i64, bool, f64),
}

impl DisplayState {
    /// A fresh entry seeded from the session.
    pub fn new(
        seeds: &Seeds,
        tab_id: Option<String>,
        video_active: bool,
        scaling_dpi: Option<String>,
    ) -> DisplayState {
        DisplayState {
            tab_id,
            decode_pace: None,
            width: 0,
            height: 0,
            position: "right".into(),
            video_active,
            encoder: seeds.encoder.clone(),
            encoder_fallback: false,
            framerate: seeds.framerate,
            video_crf: seeds.video_crf,
            video_fullcolor: seeds.video_fullcolor,
            video_10bit: seeds.video_10bit,
            video_streaming_mode: seeds.video_streaming_mode,
            jpeg_quality: seeds.jpeg_quality,
            paint_over_jpeg_quality: seeds.paint_over_jpeg_quality,
            use_cpu: effective_use_cpu(&seeds.encoder, None, seeds.use_cpu),
            use_cpu_requested: None,
            cpu_encode_threading: seeds.threading_for(&seeds.encoder, None),
            cpu_encode_threading_requested: None,
            video_paintover_crf: seeds.video_paintover_crf,
            video_paintover_burst_frames: seeds.video_paintover_burst_frames,
            use_paint_over_quality: seeds.use_paint_over_quality,
            rate_control_mode: seeds.rc_mode.clone(),
            video_bitrate: seeds.video_bitrate,
            force_aligned_resolution: settings().force_aligned_resolution().on,
            scaling_dpi,
            scale: 1.0,
            client_scale: None,
            client_stream_box: None,
            client_stream_box_at: 0.0,
            stop_signaled: false,
        }
    }

    /// The rate the capture runs at: the rate the client chose, held to what its decoder
    /// keeps up with (`DECODE_PACE`).
    pub fn capture_fps(&self) -> f64 {
        match self.decode_pace {
            Some(pace) if pace > 0.0 => self.framerate.min(pace),
            _ => self.framerate,
        }
    }

    pub fn video_params(&self) -> VideoParams {
        VideoParams {
            restart: (
                self.encoder.clone(),
                self.use_cpu,
                self.cpu_encode_threading,
                self.video_fullcolor,
                self.video_10bit,
                self.rate_control_mode.clone(),
            ),
            scale: self.scale,
            live: (
                self.framerate,
                self.video_crf,
                self.video_streaming_mode,
                self.jpeg_quality,
                self.paint_over_jpeg_quality,
                self.video_paintover_crf,
                self.video_paintover_burst_frames,
                self.use_paint_over_quality,
                self.video_bitrate,
            ),
        }
    }
}

/// What a display streams with, keyed as `SETTINGS` carries it (`_display_settings`): the
/// `use_cpu` the display asks for rather than the one a CPU-only encoder implies, and the
/// session's audio bitrate; empty of the display's keys where it has no entry.
pub fn display_settings(state: Option<&DisplayState>, seeds: &Seeds) -> Map<String, Json> {
    let mut values = Map::new();
    if let Some(d) = state {
        values.insert("encoder".into(), json!(d.encoder));
        values.insert("framerate".into(), json!(Num::of(d.framerate)));
        values.insert("video_crf".into(), json!(d.video_crf));
        values.insert("video_fullcolor".into(), json!(d.video_fullcolor));
        values.insert("video_10bit".into(), json!(d.video_10bit));
        values.insert("video_streaming_mode".into(), json!(d.video_streaming_mode));
        values.insert("jpeg_quality".into(), json!(d.jpeg_quality));
        values.insert(
            "paint_over_jpeg_quality".into(),
            json!(d.paint_over_jpeg_quality),
        );
        values.insert(
            "use_paint_over_quality".into(),
            json!(d.use_paint_over_quality),
        );
        values.insert("video_paintover_crf".into(), json!(d.video_paintover_crf));
        values.insert(
            "video_paintover_burst_frames".into(),
            json!(d.video_paintover_burst_frames),
        );
        values.insert("video_bitrate".into(), json!(Num::of(d.video_bitrate)));
        values.insert("rate_control_mode".into(), json!(d.rate_control_mode));
        values.insert("cpu_encode_threading".into(), json!(d.cpu_encode_threading));
        values.insert(
            "use_cpu".into(),
            json!(d.use_cpu_requested.unwrap_or(seeds.use_cpu)),
        );
    } else {
        values.insert("use_cpu".into(), json!(seeds.use_cpu));
    }
    values.insert("audio_bitrate".into(), json!(seeds.audio_bitrate));
    debug_assert!(values.keys().all(|k| STREAM_SETTINGS.contains(&k.as_str())));
    values
}

/// `payload` with what a display streams with in place of the boot values
/// (`with_live_stream_settings`): a range carries its value as `default` (its `value` is the
/// range), a string list as a string; keys the payload lacks and null values are left alone.
/// A page that picked none of these keeps and shows the server's, and a page whose storage was
/// cleared sends no picks, so it must be told the display's current values.
pub fn with_live_stream_settings(payload: &mut Map<String, Json>, live: &Map<String, Json>) {
    for (key, value) in live {
        if value.is_null() {
            continue;
        }
        let Some(Json::Object(entry)) = payload.get_mut(key) else {
            continue;
        };
        if entry.contains_key("min") {
            entry.insert("default".into(), value.clone());
            continue;
        }
        let stringly = entry
            .get("allowed")
            .and_then(Json::as_array)
            .and_then(|a| a.first())
            .is_some_and(Json::is_string);
        let carried = match value {
            Json::Bool(_) | Json::String(_) => value.clone(),
            other if stringly => Json::String(other.to_string()),
            other => other.clone(),
        };
        entry.insert("value".into(), carried);
    }
}

/// A page's later `SETTINGS` without the stream settings it repeats from its last one: a page
/// beside its owner may have changed them since. An encoder whose fallback mark changed counts
/// as changed.
pub fn settings_changed_by(last: Option<&ClientSettings>, new: &ClientSettings) -> ClientSettings {
    let mut out = new.clone();
    let fallback_moved = new.encoder_fallback != last.and_then(|l| l.encoder_fallback);
    for key in STREAM_SETTINGS {
        let repeated = new.stream_value(key) == last.and_then(|l| l.stream_value(key));
        if repeated && !(*key == "encoder" && fallback_moved) {
            out.clear_stream(key);
        }
    }
    out
}

/// A running capture and what it is configured with: the object a live change updates, what
/// sizes new relays, and what a layout-following reconfigure re-pushes.
pub struct CaptureInstance {
    pub settings: CaptureSettings,
}

/// The table every frame reads (module docs).
#[derive(Default)]
pub struct Registry {
    /// Every connected data socket, in connection order.
    pub clients: BTreeMap<ClientId, Arc<Client>>,
    /// Each display's owning socket, which may have just gone (the reconnect grace).
    pub owners: HashMap<DisplayId, ClientId>,
    pub links: HashMap<DisplayId, Arc<Mutex<DisplayLink>>>,
    /// Controllers beside the primary's owner, oldest first, with their tab.
    pub co_controllers: Vec<(ClientId, Option<String>)>,
    /// Sockets that sent `STOP_VIDEO`: out of the primary's fan-out until `START_VIDEO`.
    pub video_paused: HashSet<ClientId>,
    /// Paused sockets with a deferred rejoin pending, which count as consumers.
    pub waking: HashSet<ClientId>,
    /// Each display's capture, running or starting: live requests reach either.
    pub captures: HashMap<DisplayId, Arc<dyn CaptureHandle>>,
    /// Displays whose capture has finished starting.
    pub running: HashSet<DisplayId>,
    /// The bitrate each display's capture is configured with, which sizes new relays.
    pub capture_kbps: HashMap<DisplayId, i64>,
    /// Display to its relays; the entry's presence marks the capture as delivering.
    pub relays: HashMap<DisplayId, HashMap<ClientId, Arc<VideoRelay>>>,
    pub common: HashMap<(DisplayId, bool), CommonFrames<ClientId>>,
    pub repair_times: HashMap<(ClientId, &'static str), f64>,
    pub watches: HashMap<DisplayId, Arc<StreamWatch>>,
    pub framed: HashSet<DisplayId>,
    pub last_lost_frame: HashMap<DisplayId, f64>,
    /// When the invalidation line was last logged, and how many it stands for since.
    pub invalidation_log: HashMap<DisplayId, (f64, u64)>,
    /// The audio each controller beside the owner asked for; `session_audio` is the owner's.
    pub beside_audio: HashMap<ClientId, bool>,
    pub session_audio: Option<bool>,
    pub stats_subscribers: HashSet<ClientId>,
    pub stats_sending: HashSet<ClientId>,
    /// Each socket's bulk allowance (`ClipboardHost::bulk_window`).
    pub bulk: HashMap<ClientId, Arc<tokio::sync::Mutex<(Gauge, TransferPacer)>>>,
}

impl Registry {
    /// The sockets owning a display other than the primary.
    pub fn secondary_owners(&self) -> HashSet<ClientId> {
        self.owners
            .iter()
            .filter(|(d, _)| d.as_str() != PRIMARY_DISPLAY)
            .map(|(_, c)| *c)
            .collect()
    }

    pub fn is_beside(&self, client: ClientId) -> bool {
        self.co_controllers.iter().any(|(c, _)| *c == client)
    }

    pub fn owns(&self, client: ClientId, display_id: &str) -> bool {
        self.owners.get(display_id) == Some(&client)
    }

    /// Whether the audio fan-out serves a page: a controller beside the primary's owner by its
    /// own `START_AUDIO`, every other page by the owner's.
    pub fn hears_audio(&self, client: ClientId) -> bool {
        if self.is_beside(client) {
            return self.beside_audio.get(&client).copied().unwrap_or(false);
        }
        self.session_audio
            .unwrap_or_else(|| settings().pipeline_starts_on("audio", PRIMARY_DISPLAY, false))
    }

    /// Sockets the audio fan-out serves, minus `exclude`.
    pub fn audio_listeners(&self, exclude: Option<ClientId>) -> Vec<ClientId> {
        let secondary = self.secondary_owners();
        self.clients
            .keys()
            .copied()
            .filter(|c| !secondary.contains(c) && Some(*c) != exclude)
            .collect()
    }

    /// Sockets still consuming the primary broadcast: every client but the secondary owners
    /// and the paused, a paused one with a rejoin pending counting as active, since it keeps
    /// the capture alive under a waking viewer.
    pub fn active_primary_consumers(&self, exclude: Option<ClientId>) -> Vec<ClientId> {
        let secondary = self.secondary_owners();
        self.clients
            .keys()
            .copied()
            .filter(|c| {
                !secondary.contains(c)
                    && (!self.video_paused.contains(c) || self.waking.contains(c))
                    && Some(*c) != exclude
            })
            .collect()
    }

    pub fn snapshot(&self) -> Vec<Arc<Client>> {
        self.clients.values().cloned().collect()
    }
}

/// The control plane's tables.
pub struct ServerState {
    pub seeds: Seeds,
    pub displays: HashMap<DisplayId, DisplayState>,
    pub layouts: Layouts,
    pub instances: HashMap<DisplayId, CaptureInstance>,
    /// Each display's settings while its start is awaited, registered once it runs.
    pub starting: HashMap<DisplayId, CaptureSettings>,
    /// One capture per display for the server's lifetime, so a restart keeps the backend warm.
    pub persistent: HashMap<DisplayId, Arc<dyn CaptureHandle>>,
    pub fanouts: HashMap<DisplayId, JoinHandle<()>>,
    pub backpressure_tasks: HashMap<DisplayId, JoinHandle<()>>,
    pub capture_cursor: bool,
    pub binary_clipboard: bool,
    pub page_settings: HashMap<ClientId, ClientSettings>,
    pub page_hidden_at: HashMap<ClientId, Option<f64>>,
    pub page_shown_at: HashMap<ClientId, f64>,
    /// When each controller tab's user last acted through it; by tab, so a reconnect keeps it.
    pub tab_used_at: HashMap<String, f64>,
    pub owner_settle_pending: bool,
    pub last_start_video: HashMap<ClientId, f64>,
    pub deferred_rejoins: HashMap<ClientId, JoinHandle<()>>,
    pub last_keyframe_request: HashMap<DisplayId, f64>,
    pub last_keyframe_log: HashMap<DisplayId, f64>,
    pub keyframe_log_suppressed: HashMap<DisplayId, u64>,
    /// The reconnect debounce: each address's last connection, oldest first.
    pub last_connections: VecDeque<(String, f64)>,
    pub last_cursor: Option<Json>,
    pub host_output_capacity: Option<i64>,
    pub audio_started: bool,
    pub monitor_started: bool,
}

impl ServerState {
    pub fn new() -> ServerState {
        ServerState {
            seeds: Seeds::from_settings(),
            displays: HashMap::new(),
            layouts: Layouts::new(),
            instances: HashMap::new(),
            starting: HashMap::new(),
            persistent: HashMap::new(),
            fanouts: HashMap::new(),
            backpressure_tasks: HashMap::new(),
            capture_cursor: false,
            binary_clipboard: settings().enable_binary_clipboard().on,
            page_settings: HashMap::new(),
            page_hidden_at: HashMap::new(),
            page_shown_at: HashMap::new(),
            tab_used_at: HashMap::new(),
            owner_settle_pending: false,
            last_start_video: HashMap::new(),
            deferred_rejoins: HashMap::new(),
            last_keyframe_request: HashMap::new(),
            last_keyframe_log: HashMap::new(),
            keyframe_log_suppressed: HashMap::new(),
            last_connections: VecDeque::new(),
            last_cursor: None,
            host_output_capacity: None,
            audio_started: false,
            monitor_started: false,
        }
    }

    /// The stream settings of one display for its page: the display's, where it has an entry.
    pub fn display_settings(&self, display_id: &str) -> Map<String, Json> {
        display_settings(self.displays.get(display_id), &self.seeds)
    }

    /// The display ids, the primary first.
    pub fn display_ids(&self) -> Vec<DisplayId> {
        let mut ids: Vec<DisplayId> = self.displays.keys().cloned().collect();
        ids.sort_by_key(|d| (d != PRIMARY_DISPLAY, d.clone()));
        ids
    }
}

impl Default for ServerState {
    fn default() -> Self {
        Self::new()
    }
}

/// One setting through the shared sanitizer, under this plane's logger.
pub fn sanitize(name: &str, value: Option<&Json>) -> Option<Json> {
    settings::settings().sanitize_client_setting(name, value, "ws")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ws::testkit::settings_installed;

    #[test]
    fn a_later_settings_applies_what_it_changed() {
        let first = ClientSettings::parse(r#"{"encoder":"jpeg","video_crf":30,"manual_resolution":false,"initialClientWidth":1280}"#).unwrap();
        assert_eq!(
            settings_changed_by(None, &first).encoder.as_deref(),
            Some("jpeg")
        );
        let again = ClientSettings::parse(r#"{"encoder":"jpeg","video_crf":35,"manual_resolution":false,"initialClientWidth":1280}"#).unwrap();
        let applied = settings_changed_by(Some(&first), &again);
        assert_eq!(
            applied.encoder, None,
            "leaves out the stream settings it repeats"
        );
        assert_eq!(applied.video_crf, Some(35));
        assert_eq!(
            applied.manual_resolution,
            Some(false),
            "keeps what is not a stream setting"
        );
        assert_eq!(applied.initial_client_width, Some(1280));
        let marked =
            ClientSettings::parse(r#"{"encoder":"jpeg","video_crf":35,"encoderFallback":true}"#)
                .unwrap();
        assert_eq!(
            settings_changed_by(Some(&again), &marked)
                .encoder
                .as_deref(),
            Some("jpeg"),
            "an encoder whose fallback mark changed is a change"
        );
    }

    #[test]
    fn the_live_payload_carries_what_a_display_streams_with() {
        settings_installed();
        let boot = settings().client_payload();
        let mut live = settings().client_payload();
        let picks = json!({"framerate": 30, "rate_control_mode": "crf", "audio_bitrate": 128000, "use_cpu": true,
                           "video_crf": null, "not_a_setting": 1});
        with_live_stream_settings(&mut live, picks.as_object().unwrap());
        assert_eq!(
            live["framerate"]["default"],
            json!(30),
            "a range carries the live value as its default"
        );
        assert_eq!(live["framerate"]["min"], boot["framerate"]["min"]);
        assert_eq!(live["framerate"]["max"], boot["framerate"]["max"]);
        assert_eq!(live["rate_control_mode"]["value"], json!("crf"));
        assert_eq!(
            live["audio_bitrate"]["value"],
            json!("128000"),
            "a list of strings carries a number as a string"
        );
        assert_eq!(live["use_cpu"]["value"], json!(true), "a bool stays a bool");
        assert_eq!(
            live["video_crf"], boot["video_crf"],
            "a value not known yet keeps the boot entry"
        );
        assert!(!live.contains_key("not_a_setting"));
    }

    #[test]
    fn a_displays_settings_are_what_it_streams_with() {
        settings_installed();
        let mut seeds = Seeds::from_settings();
        seeds.use_cpu = false;
        seeds.audio_bitrate = 128000;
        let mut state = DisplayState::new(&seeds, None, true, None);
        state.encoder = "jpeg".into();
        state.framerate = 60.0;
        state.video_crf = 25;
        state.use_cpu = true;
        let values = display_settings(Some(&state), &seeds);
        assert_eq!(values["encoder"], json!("jpeg"));
        assert_eq!(values["framerate"], json!(60));
        assert_eq!(values["video_crf"], json!(25));
        assert_eq!(
            values["use_cpu"],
            json!(false),
            "the use_cpu it asks for, not the one JPEG implies"
        );
        assert_eq!(values["audio_bitrate"], json!(128000));
        assert!(
            !display_settings(None, &seeds).contains_key("video_bitrate"),
            "the keys it lacks are left out"
        );
    }

    #[test]
    fn the_decode_pace_holds_the_rate_chosen() {
        settings_installed();
        let seeds = Seeds::from_settings();
        let mut state = DisplayState::new(&seeds, None, true, None);
        state.framerate = 60.0;
        assert_eq!(state.capture_fps(), 60.0);
        state.decode_pace = Some(41.0);
        assert_eq!(state.capture_fps(), 41.0);
        state.framerate = 30.0;
        assert_eq!(
            state.capture_fps(),
            30.0,
            "a chosen rate under the pace is taken as chosen"
        );
    }
}
