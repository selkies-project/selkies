/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The sound-server control plane and the pcmflux data planes both transports share.
//!
//! Three parts, each in its own file:
//!
//! - [`AudioControl`] (`control.rs`, `pulse.rs`): the few one-shot control operations around
//!   the pcmflux data planes. Make sure the null sink whose monitor the server captures exists,
//!   resolve the capture source, bring up the `SelkiesVirtualMic` (the `input` null sink plus
//!   `module-virtual-source`) for microphone forwarding, set the system defaults, move a strayed
//!   pcmflux source-output back onto its source, and list the virtual microphone's recorders for
//!   capture demand. The operations run in-process over libpulse on a dedicated thread with a
//!   mainloop of its own; the `pactl` subprocess backend engages only when that connection
//!   cannot be made, and says so in one log line. The never-cancel discipline is the thread's
//!   contract: a pending operation is
//!   never canceled, a timeout abandons the connection instead and leaves the operation to end
//!   through the disconnect, and a caller that gives up (its future dropped) leaves the thread
//!   to finish the operation on its own, so the connection stays usable.
//! - [`AudioEngine`] (`engine.rs`): one engine for both transports owning the
//!   `pcmflux::AudioCapture`. Its sink hands every `AudioFrame` into a tokio broadcast as an
//!   [`AudioPacket`] whose bytes share the frame's buffer (never copied), stamped on a continuous
//!   48 kHz clock across capture restarts. It computes the Opus+RED gate from the clients'
//!   reports, applies the bitrate verb live, watches the capture's health and restarts a worker
//!   that died after its start handshake, and brings the capture in line with the session's
//!   start policy. The audio lock serializes start, stop, and re-gating, apart from any other
//!   lock, so a start the sound server is slow to answer holds up the audio alone.
//! - [`MicrophoneUplink`] (`microphone.rs`): the per-connection microphone path, provisioning
//!   the virtual microphone through the control plane and playing the client's Opus packets into
//!   the `input` sink through `pcmflux::AudioPlayback` (`write`, or `write_red` for the RED
//!   framing of the WebRTC uplink).
//!
//! The capture sink is created with the session's channel count in the speaker positions pcmflux
//! opens a surround capture with (`SURROUND_SINK_MAPS`), so the sound server has nothing to remix
//! between them: applications see a 5.1 or 7.1 device rather than a stereo one whose two channels
//! the capture would only spread over six or eight.
//!
//! The whole stack gates with pcmflux under Linux, where the sound server it drives runs; off
//! Linux `stub.rs` answers the surface the transports and the routes reach, every call reporting
//! that no backend is wired, so a client's audio and microphone messages keep their wire shape
//! and are answered with the refusals a Linux session without a sound server serves. The Windows
//! audio milestone lands in the place that file holds.

#[cfg(target_os = "linux")]
mod control;
#[cfg(target_os = "linux")]
mod engine;
#[cfg(target_os = "linux")]
mod microphone;
#[cfg(target_os = "linux")]
mod pulse;
#[cfg(not(target_os = "linux"))]
mod stub;

#[cfg(target_os = "linux")]
use pcmflux::AudioCaptureSettings;

#[cfg(target_os = "linux")]
pub use control::{
    AudioControl, AudioControlError, BackendKind, Connector, ControlBackend, PactlRunner,
    PulseNode, ensure_capture_sink, parse_pactl_list,
};
#[cfg(target_os = "linux")]
pub use engine::{
    AUDIO_RED_DISTANCE, AudioEngine, AudioGuard, AudioPacket, ClientId, Framing, HEALTH_INTERVAL,
    HealthCheck, HealthMonitor, PtsClock, RED_SETTLE, RESTART_FLOOR, RedCapability,
};
#[cfg(target_os = "linux")]
pub use microphone::{MicWrite, MicrophoneUplink, microphone_playback_settings};
#[cfg(target_os = "linux")]
pub use pulse::PulseBackend;
#[cfg(not(target_os = "linux"))]
pub use stub::{
    AudioControl, AudioEngine, AudioPacket, Framing, MicWrite, MicrophoneUplink,
    ensure_capture_sink,
};

/// The sink whose monitor the server captures when `audio_device_name` names none.
pub const DEFAULT_CAPTURE_SINK: &str = "output";
/// The recordable virtual source applications record the client's microphone from.
pub const VIRTUAL_MIC_SOURCE: &str = "SelkiesVirtualMic";
/// The null sink the microphone uplink plays into; its monitor is the virtual source's master.
pub const VIRTUAL_MIC_SINK: &str = "input";
/// The virtual source's master: the monitor of `VIRTUAL_MIC_SINK`.
pub const VIRTUAL_MIC_MASTER: &str = "input.monitor";
/// PipeWire's default sink when no card exists; its monitor is the capture of last resort.
pub const PIPEWIRE_NULL_MONITOR: &str = "auto_null.monitor";
/// The names the virtual source appears under: PipeWire prepends `output.` to a virtual source.
pub const VIRTUAL_MIC_SOURCE_NAMES: [&str; 2] = [VIRTUAL_MIC_SOURCE, "output.SelkiesVirtualMic"];
/// The `application.name` pcmflux's record stream carries.
pub const PCMFLUX_APP_NAME: &str = "pcmflux";

/// The speaker positions pcmflux opens a surround capture with, which the sink it records is
/// given too, by channel count.
pub const SURROUND_SINK_MAPS: [(i32, &str); 2] = [
    (
        6,
        "front-left,front-right,front-center,lfe,rear-left,rear-right",
    ),
    (
        8,
        "front-left,front-right,front-center,lfe,rear-left,rear-right,side-left,side-right",
    ),
];

/// The channel map a surround capture sink is created with; `None` for any other count, which
/// leaves the server's default layout.
pub fn surround_sink_map(channels: i32) -> Option<&'static str> {
    SURROUND_SINK_MAPS
        .iter()
        .find(|(count, _)| *count == channels)
        .map(|(_, map)| *map)
}

/// The sink whose monitor `audio_device_name` names; `output` when unset.
pub fn capture_sink_name(audio_device_name: Option<&str>) -> String {
    let trimmed = audio_device_name.unwrap_or("").trim();
    let name = trimmed.split(".monitor").next().unwrap_or("");
    if name.is_empty() {
        DEFAULT_CAPTURE_SINK.to_string()
    } else {
        name.to_string()
    }
}

/// The session's capture channel count (`audio_channels`), which the capture sink is created
/// with; 2 for a value below one.
pub fn capture_channels() -> i32 {
    let channels = crate::settings::settings().audio_channels();
    if channels < 1 {
        2
    } else {
        channels.min(i32::MAX as i64) as i32
    }
}

/// pcmflux settings for an Opus capture of the session's sink: 48 kHz, VBR, digital silence
/// gated so it is sent as nothing, and the sound server's fragments kept no larger than one
/// frame, so the frame duration is the latency floor.
///
/// What a listener hears is unchanged by the gate: it fires only on frames that are already all
/// zeros, so a WebRTC receiver extrapolating across the gap extrapolates silence. Its concealment
/// counter rises for the same reason, which is why that counter says nothing about loss on a
/// quiet desktop.
#[cfg(target_os = "linux")]
pub fn opus_capture_settings(
    audio_device_name: Option<&str>,
    channels: i32,
    bitrate: i32,
    frame_ms: f64,
) -> AudioCaptureSettings {
    AudioCaptureSettings {
        device_name: audio_device_name
            .filter(|name| !name.is_empty())
            .map(str::to_string),
        sample_rate: 48000,
        channels,
        opus_bitrate: bitrate,
        frame_duration_ms: frame_ms,
        use_vbr: true,
        use_silence_gate: true,
        latency_ms: frame_ms.min(10.0) as i32,
        ..AudioCaptureSettings::default()
    }
}

/// The settings the audio module's own tests install; they exercise the Linux stack.
#[cfg(target_os = "linux")]
#[cfg(test)]
pub(crate) fn install_test_settings() {
    crate::settings::install(crate::settings::Settings::parse(&[], &|_| None));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_sink_name_derives_the_sink_from_a_monitor_name() {
        assert_eq!(capture_sink_name(Some("output.monitor")), "output");
        assert_eq!(capture_sink_name(None), "output");
        assert_eq!(capture_sink_name(Some(" speakers ")), "speakers");
        assert_eq!(capture_sink_name(Some(".monitor")), "output");
    }

    /// The pcmflux settings are a property of the Linux capture.
    #[cfg(target_os = "linux")]
    #[test]
    fn opus_capture_settings_pin_the_latency_floor_to_the_frame() {
        let s = opus_capture_settings(Some("output.monitor"), 2, 128000, 10.0);
        assert_eq!(s.device_name.as_deref(), Some("output.monitor"));
        assert_eq!(s.sample_rate, 48000);
        assert!(s.use_vbr && s.use_silence_gate);
        assert_eq!(s.latency_ms, 10);
        assert_eq!(opus_capture_settings(None, 2, 128000, 2.5).latency_ms, 2);
        assert_eq!(
            opus_capture_settings(Some(""), 2, 128000, 20.0).device_name,
            None
        );
        assert_eq!(opus_capture_settings(None, 2, 128000, 20.0).latency_ms, 10);
        assert!(s.validate().is_ok());
    }

    #[test]
    fn surround_sink_maps_name_pcmflux_positions() {
        assert_eq!(
            surround_sink_map(6),
            Some("front-left,front-right,front-center,lfe,rear-left,rear-right")
        );
        assert!(
            surround_sink_map(8)
                .unwrap()
                .ends_with("side-left,side-right")
        );
        assert_eq!(surround_sink_map(2), None);
    }
}
