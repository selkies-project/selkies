/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What a page may change of what its display streams, and the congestion loop that steers it.
//!
//! Every value a page sends passes the shared sanitizer, so no verb can bypass a locked or narrowed
//! range. Video keys apply to the sending display only, as each display's pipeline owns its running
//! values; audio and the clipboard policy are stream-global whichever display asserts them.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use log::{debug, info, warn};
use selkies_rtc::rtp::clock;
use selkies_rtc::rtp::pacer::CLEANUP_PACE_SHARE;
use selkies_rtc::rtp::twcc::TWCC_QUEUE_MS;
use serde_json::{Value as Json, json};

use crate::settings::{effective_cpu_encode_threading, settings, settings_mut};
use crate::webrtc::peer::Peer;
use crate::webrtc::service::{State, WebRtcService, bitrate_range};
use crate::ws::state::{DisplayState, display_settings, json_f64};
use crate::ws::steer::{LinkAllowance, SteerTick};

/// Seconds of the cleanup's rate a still screen's cleanup frame comes to at most (pixelflux's
/// `cleanup_cap`).
pub const CLEANUP_FRAME_S: f64 = 0.2;
/// What RTP, FlexFEC, and retransmissions add to such a frame on the wire: each peer's pacer takes
/// such a frame into its video queue whole (`Pacer::set_frame_floor`).
pub const CLEANUP_FRAME_OVERHEAD: f64 = 1.5;
/// Between congestion ticks.
const CONGESTION_TICK: Duration = Duration::from_secs(1);

/// The keys a page's `SETTINGS` may change.
const SETTINGS_ALLOWED: [&str; 19] = [
    "rate_control_mode",
    "video_crf",
    "video_bitrate",
    "audio_bitrate",
    "framerate",
    "use_cpu",
    "cpu_encode_threading",
    "enable_binary_clipboard",
    "manual_resolution",
    "manual_width",
    "manual_height",
    "force_aligned_resolution",
    "encoder",
    "video_fullcolor",
    "video_10bit",
    "video_streaming_mode",
    "use_paint_over_quality",
    "video_paintover_crf",
    "video_paintover_burst_frames",
];
/// The keys a pipeline applies live or by a restart.
const VIDEO_SETTINGS: [&str; 13] = [
    "rate_control_mode",
    "video_crf",
    "video_bitrate",
    "framerate",
    "use_cpu",
    "cpu_encode_threading",
    "encoder",
    "video_fullcolor",
    "video_10bit",
    "video_streaming_mode",
    "use_paint_over_quality",
    "video_paintover_crf",
    "video_paintover_burst_frames",
];

/// A display's settings value as `SETTINGS` carries it.
fn display_value(state: &State, display: &str, key: &str) -> Json {
    display_settings(state.displays.get(display), &state.seeds)
        .get(key)
        .cloned()
        .unwrap_or(Json::Null)
}

/// Store a sanitized value on a display's entry.
fn store_on_entry(entry: &mut DisplayState, key: &str, value: &Json) {
    let as_f64 = json_f64(value).unwrap_or(0.0);
    let as_bool = value.as_bool().unwrap_or(false);
    match key {
        "encoder" => entry.encoder = value.as_str().unwrap_or(&entry.encoder).to_string(),
        "framerate" => entry.framerate = as_f64,
        "video_crf" => entry.video_crf = as_f64 as i64,
        "video_bitrate" => entry.video_bitrate = as_f64,
        "video_fullcolor" => entry.video_fullcolor = as_bool,
        "video_10bit" => entry.video_10bit = as_bool,
        "video_streaming_mode" => entry.video_streaming_mode = as_bool,
        "use_cpu" => {
            entry.use_cpu = as_bool;
            entry.use_cpu_requested = Some(as_bool);
        }
        "cpu_encode_threading" => entry.cpu_encode_threading = as_bool,
        "use_paint_over_quality" => entry.use_paint_over_quality = as_bool,
        "video_paintover_crf" => entry.video_paintover_crf = as_f64 as i64,
        "video_paintover_burst_frames" => entry.video_paintover_burst_frames = as_f64 as i64,
        "rate_control_mode" => {
            entry.rate_control_mode = value
                .as_str()
                .unwrap_or(&entry.rate_control_mode)
                .to_string();
        }
        "force_aligned_resolution" => entry.force_aligned_resolution = as_bool,
        _ => {}
    }
}

fn sanitize(key: &str, value: &Json) -> Option<Json> {
    settings().sanitize_client_setting(key, Some(value), "webrtc")
}

/// Whether a sanitized value is what a display runs with; numbers compare by value, as a sanitizer
/// may answer an integer in a float's place.
fn same_value(a: &Json, b: &Json) -> bool {
    match (json_f64(a), json_f64(b)) {
        (Some(x), Some(y)) if !a.is_boolean() && !b.is_boolean() => x == y,
        _ => a == b,
    }
}

impl WebRtcService {
    /// Whether a display's software encoder threads: its client's request, the operator's value, or
    /// what its encoder defaults to. The request is the display's own, never seeded from another's.
    pub(crate) fn cpu_encode_threading_for(&self, display: &str) -> bool {
        let st = self.st.lock();
        let Some(d) = st.displays.get(display) else {
            return st.seeds.cpu_encode_threading;
        };
        effective_cpu_encode_threading(
            &d.encoder,
            d.cpu_encode_threading_requested,
            st.seeds.cpu_encode_threading,
            st.seeds.cpu_encode_threading_provided,
        )
    }

    /// Store one video setting as the display's current value and apply it to its live pipeline. The
    /// primary's encoder is written through to the settings, since transport services re-seed from
    /// them on a mode switch. The senders take a new codec ahead of the capture restart that produces
    /// it, so no frame of one codec goes out packed as another.
    pub(crate) async fn apply_display_setting(&self, display: &str, key: &str, value: Json) {
        if !VIDEO_SETTINGS.contains(&key) {
            return;
        }
        let (before, pipeline) = {
            let mut st = self.st.lock();
            let before = display_value(&st, display, key);
            if let Some(entry) = st.displays.get_mut(display) {
                store_on_entry(entry, key, &value);
            }
            if display == "primary" {
                let seeds = &mut st.seeds;
                match key {
                    "encoder" => {
                        seeds.encoder = value.as_str().unwrap_or(&seeds.encoder).to_string();
                    }
                    "video_bitrate" => {
                        seeds.video_bitrate = json_f64(&value).unwrap_or(seeds.video_bitrate);
                    }
                    "framerate" => seeds.framerate = json_f64(&value).unwrap_or(seeds.framerate),
                    "video_fullcolor" => {
                        seeds.video_fullcolor = value.as_bool().unwrap_or(seeds.video_fullcolor);
                    }
                    "video_10bit" => {
                        seeds.video_10bit = value.as_bool().unwrap_or(seeds.video_10bit);
                    }
                    _ => {}
                }
            }
            (before, st.pipelines.get(display).cloned())
        };
        if key == "encoder" {
            // Where nothing chose them, the threads follow the encoder, set ahead of the capture
            // restart the new encoder takes.
            let threading = self.cpu_encode_threading_for(display);
            if let Some(entry) = self.st.lock().displays.get_mut(display) {
                entry.cpu_encode_threading = threading;
            }
            if let Some(pipeline) = &pipeline {
                pipeline
                    .apply_setting("cpu_encode_threading", &json!(threading))
                    .await;
            }
            self.engine
                .switch_display_codec(display, value.as_str().unwrap_or("h264enc"))
                .await;
        }
        if let Some(pipeline) = &pipeline {
            pipeline.apply_setting(key, &value).await;
        }
        if key == "video_bitrate"
            && json_f64(&value).unwrap_or(0.0) < json_f64(&before).unwrap_or(0.0)
        {
            self.hold_to_ceiling(display).await;
        }
        if key == "framerate"
            && display == "primary"
            && !self.wayland()
            && let Some(pipeline) = &pipeline
        {
            self.deps
                .display
                .refresh_output_mode(pipeline.framerate())
                .await;
        }
        if key == "encoder" && display == "primary" {
            let mut s = settings_mut();
            s.set_str("encoder", value.as_str().unwrap_or("h264enc").to_string());
            s.encoder_client_set = true;
        }
    }

    /// Bring a display's stream down to a lowered bitrate setting at once, as its next congestion
    /// tick would: its pacers sized for the setting, the cleanup's link allowance held within their
    /// new pace, and the video budget and cleanup frame floor to both. Left to the tick, a ceiling
    /// lowered under motion lets up to a second of the old rate's overshoot into all three.
    async fn hold_to_ceiling(&self, display: &str) {
        let Some(pipeline) = self.pipeline(display) else {
            return;
        };
        let (lo, hi) = bitrate_range();
        let ceiling = {
            let st = self.st.lock();
            st.displays
                .get(display)
                .map_or(hi, |d| d.video_bitrate)
                .clamp(lo, hi)
        };
        let peers: Vec<(String, Peer)> = self
            .engine
            .peers
            .lock()
            .iter()
            .filter(|(_, p)| p.display_id == display)
            .map(|(id, p)| (id.clone(), p.peer.clone()))
            .collect();
        for (_, peer) in &peers {
            peer.set_encoder_bps((ceiling * 1000.0) as u64);
        }
        if !self.policy.lock().congestion_control || pipeline.rc_mode() != "cbr" {
            return;
        }
        let owners: Vec<&Peer> = {
            let owning: Vec<&Peer> = peers
                .iter()
                .filter(|(id, _)| self.peer_owns(id))
                .map(|(_, p)| p)
                .collect();
            if owning.is_empty() {
                peers.iter().map(|(_, p)| p).collect()
            } else {
                owning
            }
        };
        let mut paces = Vec::new();
        for peer in &owners {
            if let Some(pace) = peer.pace().await {
                paces.push(CLEANUP_PACE_SHARE * pace as f64);
            }
        }
        let pace = paces.into_iter().reduce(f64::min);
        let kbps = {
            let mut st = self.st.lock();
            match st.link_allowances.get_mut(display) {
                Some(allowance) => {
                    if let Some(told) = allowance.note(None, hi, pace) {
                        pipeline.set_link_rate(told);
                    }
                    allowance.kbps.unwrap_or(0) as f64
                }
                None => 0.0,
            }
        };
        self.engine.set_video_budget(
            display,
            Some(ceiling.max(kbps / LinkAllowance::SHARE) * 1000.0),
        );
        for peer in owners {
            peer.set_frame_floor(
                (kbps * 1000.0 / 8.0 * CLEANUP_FRAME_S * CLEANUP_FRAME_OVERHEAD) as usize,
            );
        }
    }

    /// A display's capture streams `encoder` in place of the one asked for: the setting follows, its
    /// senders switch codec, and every client hears of it.
    pub(crate) async fn encoder_demoted(&self, display: &str, encoder: &str) {
        {
            let mut st = self.st.lock();
            if let Some(entry) = st.displays.get_mut(display) {
                entry.encoder = encoder.to_string();
            }
            if display == "primary" {
                st.seeds.encoder = encoder.to_string();
            }
        }
        if display == "primary" {
            settings_mut().set_str("encoder", encoder.to_string());
        }
        self.engine.switch_display_codec(display, encoder).await;
        self.broadcast_server_settings();
    }

    /// Turn a format setting off for a display whose joining peer cannot decode it, and tell every
    /// client; a setting the operator holds stays, which leaves that peer without a picture. The
    /// payload advertises the primary's value, so it is taken out of the settings too: an operator's
    /// override left in it would come back from the client and turn the format on again.
    pub(crate) async fn format_declined(
        &self,
        display: &str,
        setting: &'static str,
        fmt: &str,
        label: &str,
    ) -> bool {
        let on = {
            let st = self.st.lock();
            display_value(&st, display, setting)
                .as_bool()
                .unwrap_or(false)
        };
        if !on {
            return true;
        }
        let locked = match setting {
            "video_fullcolor" => settings().video_fullcolor().locked,
            _ => settings().video_10bit().locked,
        };
        if locked {
            return false;
        }
        warn!(target: "webrtc", "A WebRTC peer of display '{display}' decodes no {fmt} of its codec; {label} is off.");
        self.apply_display_setting(display, setting, json!(false))
            .await;
        if display == "primary" {
            settings_mut().set_bool(setting, false);
        }
        self.broadcast_server_settings();
        true
    }

    /// Apply a client `SETTINGS` payload to the display that sent it. Video keys apply to the sending
    /// display only; audio and the clipboard policy are stream-global whichever display asserts them.
    /// A `scaling_dpi` in the primary's payload, or in any display's on Wayland, runs `handle_scaling`;
    /// a secondary's `displayPosition` may move it to any side of the primary after joining.
    pub async fn handle_update_settings(&self, payload: &Json, display: &str) {
        let Some(map) = payload.as_object() else {
            return;
        };
        if display != "primary" && !self.st.lock().displays.contains_key(display) {
            warn!(target: "webrtc", "Ignoring settings for unknown display '{display}' (not connected).");
            return;
        }
        if let (Some(layout), Some(input)) = (
            map.get("keyboardLayout").and_then(Json::as_str),
            self.input(),
        ) && !layout.is_empty()
        {
            input.apply_client_keyboard_layout(layout).await;
        }
        if let Some(dpi) = map.get("scaling_dpi").filter(|d| !d.is_null())
            && (display == "primary" || self.wayland())
        {
            match json_f64(dpi) {
                Some(dpi) => self.handle_scaling(dpi, display).await,
                None => {
                    warn!(target: "webrtc", "Ignoring malformed scaling_dpi in SETTINGS: {dpi}")
                }
            }
        }
        if let Some(scale) = map.get("displayScale").and_then(json_f64)
            && (0.05..=100.0).contains(&scale)
        {
            let changed = {
                let mut st = self.st.lock();
                match st.displays.get_mut(display) {
                    Some(entry) if entry.client_scale != Some(scale) => {
                        entry.client_scale = Some(scale);
                        true
                    }
                    _ => false,
                }
            };
            if changed {
                self.broadcast_display_config();
            }
        }
        if display != "primary"
            && let Some(position) = map.get("displayPosition").filter(|p| !p.is_null())
        {
            let position = position.as_str().unwrap_or("");
            if !["right", "left", "up", "down"].contains(&position) {
                warn!(target: "webrtc", "Ignoring invalid displayPosition from '{display}': {position:?}");
            } else {
                let moved = {
                    let mut st = self.st.lock();
                    match st.displays.get_mut(display) {
                        Some(entry) if entry.position != position => {
                            entry.position = position.to_string();
                            true
                        }
                        _ => false,
                    }
                };
                if moved {
                    self.reconfigure_displays().await;
                }
            }
        }
        let threading_before = self.cpu_encode_threading_for(display);
        for key in SETTINGS_ALLOWED {
            let Some(client_value) = map.get(key).filter(|v| !v.is_null()) else {
                continue;
            };
            if key == "cpu_encode_threading" {
                let requested = sanitize(key, client_value).and_then(|r| r.as_bool());
                if let Some(requested) = requested {
                    if let Some(entry) = self.st.lock().displays.get_mut(display) {
                        entry.cpu_encode_threading_requested = Some(requested);
                    }
                    let threading = self.cpu_encode_threading_for(display);
                    self.apply_display_setting(display, key, json!(threading))
                        .await;
                }
                continue;
            }
            if key == "rate_control_mode" && !settings().enable_rate_control().on {
                debug!(target: "webrtc", "Server has rate control disabled. Ignoring update for '{key}'.");
                continue;
            }
            if ["manual_resolution", "manual_width", "manual_height"].contains(&key)
                && display != "primary"
            {
                continue;
            }
            let Some(sanitized) = sanitize(key, client_value) else {
                continue;
            };
            let current = {
                let st = self.st.lock();
                match key {
                    "audio_bitrate" => json!(st.seeds.audio_bitrate),
                    "enable_binary_clipboard" => json!(st.enable_binary_clipboard),
                    "force_aligned_resolution" => st
                        .displays
                        .get(display)
                        .map_or(Json::Null, |d| json!(d.force_aligned_resolution)),
                    _ => display_value(&st, display, key),
                }
            };
            if same_value(&sanitized, &current) {
                continue;
            }
            match key {
                "audio_bitrate" => {
                    // Audio lives only on the primary pipeline.
                    let bps = json_f64(&sanitized).unwrap_or(0.0) as i64;
                    if let Some(pipeline) = self.pipeline("primary") {
                        pipeline.set_audio_bitrate(bps as i32);
                    }
                    self.st.lock().seeds.audio_bitrate = bps;
                }
                "enable_binary_clipboard" => {
                    let enabled = sanitized.as_bool().unwrap_or(false);
                    self.deps.clipboard.set_binary_enabled(enabled).await;
                    self.st.lock().enable_binary_clipboard = enabled;
                }
                _ if VIDEO_SETTINGS.contains(&key) => {
                    self.apply_display_setting(display, key, sanitized.clone())
                        .await;
                }
                "force_aligned_resolution" => {
                    if let Some(entry) = self.st.lock().displays.get_mut(display) {
                        store_on_entry(entry, key, &sanitized);
                    }
                }
                // Read by the resize paths and at startup.
                _ => {}
            }
            debug!(target: "webrtc", "Updated setting '{key}' for display '{display}' from {current} to {sanitized}");
        }
        if display == "primary" && self.cpu_encode_threading_for(display) != threading_before {
            // The page shows what the display runs with, and an encoder change can move it.
            self.broadcast_server_settings();
        }
        if display == "primary" {
            self.primary_sized.send_replace(true);
        }
    }

    /// Video bitrate change for the display whose page sent it.
    pub async fn handle_video_bitrate_change(&self, kbps: i64, display: &str) {
        self.apply_sanitized(display, "video_bitrate", &json!(kbps))
            .await;
    }

    /// Frame rate change for the display whose page sent it.
    pub async fn handle_fps_change(&self, fps: f64, display: &str) {
        self.apply_sanitized(display, "framerate", &json!(fps))
            .await;
    }

    /// CRF change for the display whose page sent it.
    pub async fn handle_crf_change(&self, crf: i64, display: &str) {
        self.apply_sanitized(display, "video_crf", &json!(crf))
            .await;
    }

    async fn apply_sanitized(&self, display: &str, key: &str, value: &Json) {
        let Some(sanitized) = sanitize(key, value) else {
            return;
        };
        let current = {
            let st = self.st.lock();
            display_value(&st, display, key)
        };
        if !same_value(&sanitized, &current) {
            self.apply_display_setting(display, key, sanitized).await;
        }
    }

    /// Audio bitrate change, in bps, sanitized like `SETTINGS`.
    pub async fn handle_audio_bitrate_change(&self, bps: i64) {
        let Some(sanitized) = sanitize("audio_bitrate", &json!(bps)) else {
            return;
        };
        let bps = json_f64(&sanitized).unwrap_or(0.0) as i64;
        if self.st.lock().seeds.audio_bitrate == bps {
            return;
        }
        if let Some(pipeline) = self.pipeline("primary") {
            pipeline.set_audio_bitrate(bps as i32);
        }
        self.st.lock().seeds.audio_bitrate = bps;
    }

    /// Rate-control switch for the display whose page sent it; honors the server's lock.
    pub async fn handle_rate_control_change(&self, mode: &str, display: &str) {
        if !settings().enable_rate_control().on {
            debug!(target: "webrtc", "Server has rate control disabled. Ignoring rate-control change.");
            return;
        }
        self.apply_display_setting(display, "rate_control_mode", json!(mode))
            .await;
    }

    /// GCC-style bitrate adaptation from transport-wide feedback: per display, follow the slowest of
    /// its peers' goodput estimates with headroom, back off on the first tick whose one-way delay
    /// shows a queue standing, or still building, `TWCC_QUEUE_MS` past the path's own, multiplicatively
    /// on two ticks of loss in a row, and hold there before recovering (`CongestionSteer`). One display's
    /// congested link never steers another's stream. The peers are the display's owner when it sent
    /// feedback, as over WebSockets the owner's acks steer: a viewer's or a controller's beside the
    /// owner on a slower link do not pull the owner's stream down; all of them otherwise. Only CBR has a
    /// target to steer. Each peer's own loss and queue set its sender's FlexFEC repairs (`Peer::tick`).
    ///
    /// The user-selected bitrate is the ceiling: control only backs off below it and recovers up to
    /// it. Damage-gated encoders are application-limited, so measured goodput may lift the target when
    /// it shows real headroom but never drags it down. The pacer rides the same tick but is configured
    /// for every peer before the feedback gate, since it must run on links that never send transport-cc.
    pub(crate) async fn congestion_control_loop(self: Arc<Self>, pacer_on: bool) {
        let (lo, hi) = bitrate_range();
        debug!(target: "webrtc", "Congestion control loop started (CBR only, range {lo}-{hi} kbps).");
        let mut interval = tokio::time::interval(CONGESTION_TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            if self.st.lock().shutdown {
                return;
            }
            self.congestion_tick(pacer_on, lo, hi).await;
        }
    }

    async fn congestion_tick(&self, pacer_on: bool, lo: f64, hi: f64) {
        #[derive(Default)]
        struct Bucket {
            goodputs: Vec<f64>,
            worst_loss: f64,
            queue_ms: Option<f64>,
            depth_ms: f64,
            sent_bps: f64,
            padding_bps: f64,
            rooms: Vec<f64>,
            paces: Vec<f64>,
            peers: Vec<Peer>,
        }
        let cc_on = self.policy.lock().congestion_control;
        let ticks = self.engine.peer_ticks().await;
        if let Some(metrics) = &self.deps.rtc_metrics {
            for (_, display, tick) in &ticks {
                if let Some(snapshot) = &tick.snapshot {
                    metrics.pacer_snapshot(display, snapshot);
                }
            }
            metrics.bridge_drops(&self.engine.bridge_drops());
        }
        let mut per_display: HashMap<String, Bucket> = HashMap::new();
        let mut by_owner: HashMap<String, Bucket> = HashMap::new();
        // The peers a display's padding goes to: its owner where it has one.
        let mut padded: HashMap<String, Vec<(String, Peer)>> = HashMap::new();
        let mut padded_owner: HashMap<String, Vec<(String, Peer)>> = HashMap::new();
        for (peer_id, display, tick) in ticks {
            let handle = self
                .engine
                .peers
                .lock()
                .get(&peer_id)
                .map(|p| p.peer.clone());
            let Some(handle) = handle else { continue };
            let owns = self.peer_owns(&peer_id);
            padded
                .entry(display.clone())
                .or_default()
                .push((peer_id.clone(), handle.clone()));
            if owns {
                padded_owner
                    .entry(display.clone())
                    .or_default()
                    .push((peer_id.clone(), handle.clone()));
            }
            if pacer_on {
                let ceiling = self
                    .st
                    .lock()
                    .displays
                    .get(&display)
                    .map_or(hi, |d| d.video_bitrate)
                    .clamp(lo, hi);
                handle.set_encoder_bps((ceiling * 1000.0) as u64);
            }
            let Some(window) = tick.window.clone() else {
                continue;
            };
            let standing = window
                .queue_ms
                .map(|q| q.max(window.queue_rising_ms.unwrap_or(0.0)));
            let note = |bucket: &mut Bucket| {
                if window.goodput_bps > 0 {
                    bucket.goodputs.push(window.goodput_bps as f64);
                }
                if let Some(room) = tick.room_bps {
                    bucket.rooms.push(room);
                }
                if let Some(pace) = tick.pace_bps {
                    bucket.paces.push(CLEANUP_PACE_SHARE * pace as f64);
                }
                bucket.peers.push(handle.clone());
                bucket.sent_bps = bucket.sent_bps.max(window.sent_bps as f64);
                bucket.padding_bps = bucket.padding_bps.max(window.padding_bps as f64);
                bucket.worst_loss = bucket.worst_loss.max(window.loss_fraction);
                if let Some(standing) = standing {
                    bucket.queue_ms = Some(bucket.queue_ms.unwrap_or(0.0).max(standing));
                    bucket.depth_ms = bucket.depth_ms.max(window.queue_depth_ms.unwrap_or(0.0));
                }
            };
            note(per_display.entry(display.clone()).or_default());
            if owns {
                note(by_owner.entry(display.clone()).or_default());
            }
        }
        per_display.extend(by_owner);
        padded.extend(padded_owner);
        if !cc_on {
            return;
        }
        let ticked: Vec<String> = per_display.keys().cloned().collect();
        for (did, bucket) in per_display {
            let Some(pipeline) = self.pipeline(&did) else {
                continue;
            };
            if pipeline.rc_mode() != "cbr" {
                self.engine.set_video_budget(&did, None);
                continue;
            }
            let Some(goodput) = bucket.goodputs.iter().copied().reduce(f64::min) else {
                continue;
            };
            let current = pipeline.video_bitrate();
            let ceiling = self
                .st
                .lock()
                .displays
                .get(&did)
                .map_or(hi, |d| d.video_bitrate)
                .clamp(lo, hi);
            let queue_s = bucket.queue_ms.map(|q| {
                if q > TWCC_QUEUE_MS {
                    q.max(bucket.depth_ms) / 1000.0
                } else {
                    0.0
                }
            });
            let now = clock::monotonic();
            let probed = self
                .st
                .lock()
                .path_probes
                .get(&did)
                .is_some_and(|p| p.padding);
            let (target, restored, queued) = {
                let mut st = self.st.lock();
                let steer = st.congestion.entry(did.clone()).or_default();
                let target = steer
                    .target(SteerTick {
                        current,
                        ceiling,
                        floor: lo,
                        goodput_bps: goodput,
                        loss: bucket.worst_loss,
                        now,
                        queue_s,
                        offered_bps: Some(bucket.sent_bps),
                        acked_offered_bps: None,
                        capacity_bps: None,
                        probed,
                    })
                    .round();
                // A target cut by a queue the path showed (the steer holds the capacity it
                // measured), not by loss alone on a path that never queued: only then do repairs
                // answer it.
                (target, steer.restored, steer.capacity_kbps.is_some())
            };
            // What the path carries, through each peer's pacer, sets the rate a still screen's
            // cleanup runs at. A pacer reads the path's capacity, not what cross traffic leaves of
            // it: a target backed off under the display's ceiling reads as no room.
            let mut room = bucket.rooms.iter().copied().reduce(f64::min);
            if target < ceiling.round() {
                room = Some(0.0);
            }
            let pace = bucket.paces.iter().copied().reduce(f64::min);
            let allowance = {
                let mut st = self.st.lock();
                let allowance = st.link_allowances.entry(did.clone()).or_default();
                let told = allowance.note(room, hi, pace);
                let kbps = allowance.kbps.unwrap_or(0) as f64;
                drop(st);
                if let Some(told) = told {
                    pipeline.set_link_rate(told);
                }
                kbps
            };
            // The budget runs to the room the allowance keeps its margin under: held to the
            // allowance itself, a cleanup's frames, a few percent past their rate, kept it full, and
            // each one it dropped started the cleanup over.
            self.engine.set_video_budget(
                &did,
                Some(target.max(allowance / LinkAllowance::SHARE) * 1000.0),
            );
            for peer in &bucket.peers {
                peer.set_frame_floor(
                    (allowance * 1000.0 / 8.0 * CLEANUP_FRAME_S * CLEANUP_FRAME_OVERHEAD) as usize,
                );
            }
            // Under a queue-measured cut repairs take no more than the rate steered to, so a path
            // that loses because it is full is not sent more than it was asked to carry; where loss
            // alone cut the target they are not bound, since the repairs a random loss asks for
            // crowd nothing and the goodput they carry holds the target up.
            for (_, peer) in padded.get(&did).map_or(&[][..], Vec::as_slice) {
                peer.set_repair_budget(queued.then_some(target * 1000.0), Some(goodput));
            }
            // A cut target over a still screen pads its stream to test the path. A trickle's
            // window reads no queue (too few feedback packets): padding starts on it, and the queue
            // the padded windows measure stops it.
            let content = (bucket.sent_bps - bucket.padding_bps).max(0.0);
            let pad = self.probe_rate(&did, target, ceiling, content, queue_s.unwrap_or(0.0), now);
            self.pad_display(
                &did,
                padded.get(&did).map_or(&[][..], Vec::as_slice),
                pad,
                now + 2.0 * CONGESTION_TICK.as_secs_f64(),
            );
            let held = self
                .st
                .lock()
                .rate_holds
                .entry(did.clone())
                .or_default()
                .note(target, now);
            if let Some(held) = held {
                self.engine.send_cc_rate(&did, held);
            }
            if target != current.round() {
                let why = if restored {
                    "the queue gone after a stall: the rate before it".to_string()
                } else {
                    format!(
                        "goodput {:.1} kbps, loss {:.1}%, queue {:.0} ms",
                        goodput / 1e3,
                        bucket.worst_loss * 100.0,
                        bucket.queue_ms.unwrap_or(0.0)
                    )
                };
                let line = format!(
                    "Congestion control[{did}]: video bitrate {current:.0} -> {target:.0} kbps ({why})"
                );
                if target < current || restored {
                    info!(target: "webrtc", "{line}");
                } else {
                    debug!(target: "webrtc", "{line}");
                }
                pipeline.set_video_bitrate(target).await;
            }
        }
        // A display that sent nothing this tick drained no feedback: an idle screen, which a cut
        // target pads from (its path idle, no queue to read) as it pads a trickle.
        let steered: Vec<String> = self.st.lock().congestion.keys().cloned().collect();
        for did in steered.into_iter().filter(|d| !ticked.contains(d)) {
            let Some(pipeline) = self.pipeline(&did).filter(|p| p.rc_mode() == "cbr") else {
                continue;
            };
            let ceiling = self
                .st
                .lock()
                .displays
                .get(&did)
                .map_or(hi, |d| d.video_bitrate)
                .clamp(lo, hi);
            let now = clock::monotonic();
            let pad = self.probe_rate(&did, pipeline.video_bitrate(), ceiling, 0.0, 0.0, now);
            self.pad_display(
                &did,
                padded.get(&did).map_or(&[][..], Vec::as_slice),
                pad,
                now + 2.0 * CONGESTION_TICK.as_secs_f64(),
            );
        }
    }

    /// The padding a display's `PathProbe` asks for after a tick that set `target` under `ceiling`
    /// (kbps), the stream itself having sent `content_bps` over a queue of `queue_s`.
    fn probe_rate(
        &self,
        display: &str,
        target: f64,
        ceiling: f64,
        content_bps: f64,
        queue_s: f64,
        now: f64,
    ) -> f64 {
        let mut st = self.st.lock();
        let held = st
            .congestion
            .get(display)
            .is_some_and(|s| now < s.hold_until);
        st.path_probes
            .entry(display.to_string())
            .or_default()
            .rate_bps(target, ceiling, Some(content_bps), Some(queue_s), held, now)
    }

    /// Pad a display's `peers` at `bps` until `until`, each through its own pacer, or stop padding
    /// where `bps` is 0 or no peer is left; a peer dropped from the list stops at once.
    pub(crate) fn pad_display(
        &self,
        display: &str,
        peers: &[(String, Peer)],
        bps: f64,
        until: f64,
    ) {
        let padding = bps > 0.0 && !peers.is_empty();
        let was = self.st.lock().padded.remove(display).unwrap_or_default();
        if padding != !was.is_empty() {
            if padding {
                debug!(target: "webrtc", "Congestion control[{display}]: probing the path at {:.0} kbps of padding (a still screen under a cut target)", bps / 1000.0);
            } else {
                debug!(target: "webrtc", "Congestion control[{display}]: probe ended");
            }
        }
        for (id, peer) in &was {
            if !padding || !peers.iter().any(|(p, _)| p == id) {
                peer.set_padding(0.0, 0.0);
            }
        }
        if padding {
            for (_, peer) in peers {
                peer.set_padding(bps, until);
            }
            self.st
                .lock()
                .padded
                .insert(display.to_string(), peers.to_vec());
        }
    }

    /// Drop what a display's congestion control learned about the path of the page it served, for
    /// a page taking it over, and stop its padding. The steered rate stays as the starting point.
    pub(crate) fn forget_path(&self, display: &str) {
        {
            let mut st = self.st.lock();
            st.congestion.remove(display);
            st.link_allowances.remove(display);
            st.path_probes.remove(display);
        }
        self.pad_display(display, &[], 0.0, 0.0);
    }
}
