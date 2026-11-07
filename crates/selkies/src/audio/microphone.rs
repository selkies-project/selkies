/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The microphone uplink of one connection: the client's Opus packets into the virtual
//! microphone.
//!
//! A [`MicrophoneUplink`] holds its connection's sound-server control, provisions the
//! `SelkiesVirtualMic` on the first packet (`AudioControl::ensure_virtual_microphone`, retried
//! five seconds after a failed attempt), opens a `pcmflux::AudioPlayback` into the `input` sink
//! on the first packet after that, and hands every packet to it: `write` for the WebSockets
//! uplink's bare Opus, `write_red` for the RED framing of the WebRTC uplink. The playback decodes
//! on the caller's thread and queues PCM for its own thread, never blocking on the sound server;
//! a write that fails tears the playback down so the next packet reopens a fresh one. At close
//! the playback is joined off the runtime, the `module-virtual-source` is unloaded only when this
//! connection loaded it (a reused source belongs to the other transport), and the control
//! connection is released.
//!
//! What the uplink does not decide is who may speak: the transport gates a viewer's packets and
//! tells the page `MICROPHONE_DISABLED`; the uplink only says, through [`MicWrite::Refused`],
//! when the settings or a missing control plane refuse the microphone.

use std::sync::Arc;
use std::time::{Duration, Instant};

use log::{debug, error, info, warn};
use pcmflux::{AudioPlayback, AudioPlaybackSettings};

use super::{AudioControl, AudioEngine};
use crate::settings::settings;

/// How long a failed virtual microphone provisioning waits before the next packet retries it.
const SETUP_RETRY: Duration = Duration::from_secs(5);

/// The playback the microphone packets go into: the `input` sink at the 24 kHz mono the client's
/// encoder produces (its worklet brings any capture rate to 24 kHz), buffered 40 ms.
pub fn microphone_playback_settings() -> AudioPlaybackSettings {
    AudioPlaybackSettings {
        device_name: Some(super::VIRTUAL_MIC_SINK.to_string()),
        sample_rate: 24000,
        channels: 1,
        latency_ms: 40,
        ..AudioPlaybackSettings::default()
    }
}

/// What became of one microphone packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicWrite {
    /// Queued for playback.
    Written,
    /// The microphone is off for this session: the settings lock it off, or no control plane
    /// could be reached. The transport tells the page `MICROPHONE_DISABLED` once.
    Refused,
    /// Dropped for now: provisioning not complete, an empty packet, or a playback that failed
    /// and reopens on the next packet.
    Skipped,
}

/// One connection's microphone path.
pub struct MicrophoneUplink {
    engine: Arc<AudioEngine>,
    peer: String,
    control: Option<AudioControl>,
    control_failed: bool,
    /// The `module-virtual-source` index and whether this connection loaded it.
    module: Option<(u32, bool)>,
    setup_done: bool,
    retry_at: Option<Instant>,
    playback: Option<Arc<AudioPlayback>>,
}

impl MicrophoneUplink {
    /// An uplink for the connection `peer` names in the log, forwarding into the capture sink
    /// `engine` derives from its device.
    pub fn new(engine: Arc<AudioEngine>, peer: &str) -> Self {
        MicrophoneUplink {
            engine,
            peer: peer.to_string(),
            control: None,
            control_failed: false,
            module: None,
            setup_done: false,
            retry_at: None,
            playback: None,
        }
    }

    /// Whether the settings let a microphone be forwarded at all: audio on, and the microphone
    /// not locked off. An unlocked default-off microphone is only the client's toggle, so a
    /// runtime enable needs no reconnect.
    pub fn enabled_by_settings() -> bool {
        let s = settings();
        let microphone = s.microphone_enabled();
        s.audio_enabled().on && (microphone.on || !microphone.locked)
    }

    /// Open the control plane at connect, when the settings allow a microphone. False when no
    /// control plane could be reached: packets are refused from then on.
    pub async fn open(&mut self) -> bool {
        if !Self::enabled_by_settings() {
            debug!(target: "audio", "Audio/microphone disabled in settings. Skipping PulseAudio setup.");
            return false;
        }
        let mut control = AudioControl::new("selkies-mic-handler");
        if control.open().await {
            let backend = control.backend().map(|b| b.as_str()).unwrap_or("none");
            info!(target: "audio", "Sound server control ready for the microphone ({backend}).");
            self.control = Some(control);
            true
        } else {
            error!(target: "audio", "Sound server control unavailable; microphone forwarding disabled.");
            self.control_failed = true;
            false
        }
    }

    /// Whether packets are refused: only a locked-off microphone refuses data, since an unlocked
    /// default-off is the client toggle and data means it is on; so does a control plane that
    /// could not be reached.
    pub fn refused(&self) -> bool {
        self.control_failed || !Self::enabled_by_settings()
    }

    /// The loaded `module-virtual-source` index and whether this connection owns it.
    pub fn module(&self) -> Option<(u32, bool)> {
        self.module
    }

    /// Play one bare Opus packet.
    pub async fn write(&mut self, packet: &[u8]) -> MicWrite {
        self.deliver(packet, |playback| playback.write(packet))
            .await
    }

    /// Play one RFC 2198 RED frame whose primary block's RTP timestamp is `primary_ts`.
    pub async fn write_red(&mut self, packet: &[u8], primary_ts: i64) -> MicWrite {
        self.deliver(packet, |playback| playback.write_red(packet, primary_ts))
            .await
    }

    async fn deliver(
        &mut self,
        packet: &[u8],
        play: impl FnOnce(&AudioPlayback) -> Result<(), pcmflux::Error>,
    ) -> MicWrite {
        if self.refused() {
            return MicWrite::Refused;
        }
        let Some(control) = self.control.as_mut() else {
            if !packet.is_empty() {
                warn!(target: "audio", "Sound server control not connected. Skipping microphone data.");
            }
            return MicWrite::Skipped;
        };
        if !self.setup_done {
            if self.retry_at.is_some_and(|at| Instant::now() < at) {
                return MicWrite::Skipped;
            }
            debug!(target: "audio", "Performing PulseAudio/PipeWire virtual microphone setup check...");
            let (index, owned) = control
                .ensure_virtual_microphone(
                    Some(self.engine.audio_device_name()),
                    self.engine.is_capturing(),
                )
                .await;
            match index {
                Some(index) => {
                    self.module = Some((index, owned));
                    self.setup_done = true;
                }
                None => self.retry_at = Some(Instant::now() + SETUP_RETRY),
            }
        }
        if !self.setup_done || packet.is_empty() {
            if !self.setup_done && !packet.is_empty() {
                warn!(target: "audio", "Mic setup not complete, skipping mic data.");
            }
            return MicWrite::Skipped;
        }
        if self.playback.is_none() {
            // Published only after a successful start, so a failed one is retried on the next
            // packet.
            let playback = Arc::new(AudioPlayback::new());
            let (starting, settings) = (playback.clone(), microphone_playback_settings());
            match tokio::task::spawn_blocking(move || starting.start(&settings)).await {
                Ok(Ok(())) => self.playback = Some(playback),
                Ok(Err(e)) => {
                    error!(target: "audio", "Rust mic playback error: {e}");
                    return MicWrite::Skipped;
                }
                Err(e) => {
                    error!(target: "audio", "Rust mic playback error: {e}");
                    return MicWrite::Skipped;
                }
            }
        }
        let Some(playback) = self.playback.as_ref() else {
            return MicWrite::Skipped;
        };
        match play(playback) {
            Ok(()) => MicWrite::Written,
            Err(e) => {
                error!(target: "audio", "Rust mic playback error: {e}");
                // Torn down so the next packet reopens a fresh stream.
                if let Some(dead) = self.playback.take() {
                    let _ = tokio::task::spawn_blocking(move || dead.stop()).await;
                }
                MicWrite::Skipped
            }
        }
    }

    /// Release the uplink: join the playback off the runtime, unload a module this connection
    /// loaded, and close the control connection.
    pub async fn close(&mut self) {
        if let Some(playback) = self.playback.take() {
            match tokio::task::spawn_blocking(move || playback.stop()).await {
                Ok(()) => debug!(target: "audio", "Stopped Rust mic playback for {}.", self.peer),
                Err(e) => {
                    error!(target: "audio", "Error stopping Rust mic playback for {}: {e}", self.peer)
                }
            }
        }
        if let Some(mut control) = self.control.take() {
            if let Some((index, true)) = self.module {
                info!(target: "audio", "Unloading PulseAudio module {index} for virtual mic (client: {}).", self.peer);
                control.unload_module(index).await;
            }
            control.close().await;
            debug!(target: "audio", "Closed sound server control connection for {}.", self.peer);
        }
        self.module = None;
        self.setup_done = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::install_test_settings;
    use crate::audio::{Framing, VIRTUAL_MIC_SOURCE_NAMES};

    #[test]
    fn the_playback_runs_at_the_rate_the_client_encodes() {
        let s = microphone_playback_settings();
        assert_eq!(s.device_name.as_deref(), Some("input"));
        assert_eq!((s.sample_rate, s.channels, s.latency_ms), (24000, 1, 40));
        assert!(s.validate().is_ok());
    }

    #[tokio::test]
    async fn the_uplink_provisions_the_virtual_microphone_and_plays_into_it() {
        install_test_settings();
        assert!(
            std::env::var_os("PULSE_SERVER").is_some()
                || std::env::var_os("XDG_RUNTIME_DIR").is_some(),
            "a sound server is required: set PULSE_SERVER"
        );
        let engine = AudioEngine::new(Framing::Headered);
        let mut uplink = MicrophoneUplink::new(engine, "test-peer");
        assert!(
            MicrophoneUplink::enabled_by_settings(),
            "the defaults leave the microphone to the client toggle"
        );
        assert!(
            uplink.open().await,
            "the control plane opens against the sandbox's sound server"
        );
        assert!(!uplink.refused());
        assert_eq!(
            uplink.write(&[]).await,
            MicWrite::Skipped,
            "an empty packet provisions and plays nothing"
        );
        let module = uplink
            .module()
            .expect("the virtual microphone is provisioned");
        let mut control = AudioControl::new("selkies-test-mic-check");
        let sources = control.sources().await;
        assert!(
            sources
                .iter()
                .any(|s| VIRTUAL_MIC_SOURCE_NAMES.contains(&s.name.as_str())),
            "the virtual source is listed"
        );
        // A packet of the stereo Opus silence frame: decodable, so it reaches the queue.
        assert_eq!(uplink.write(&[0xfc, 0xff, 0xfe]).await, MicWrite::Written);
        assert_eq!(
            uplink.write(&[0x00]).await,
            MicWrite::Written,
            "an undecodable packet is dropped, not an error"
        );
        uplink.close().await;
        assert_eq!(uplink.module(), None);
        if module.1 {
            let sources = control.sources().await;
            assert!(
                !sources
                    .iter()
                    .any(|s| VIRTUAL_MIC_SOURCE_NAMES.contains(&s.name.as_str())),
                "the module this uplink loaded is unloaded"
            );
        }
        control.close().await;
    }
}
