/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The server's audio surface where the Linux session stack does not run: every name the
//! transports and the routes reach, answering that no backend is wired for this OS the way a
//! refused Linux start reports its own -- a start that does not start, a control plane that
//! does not open, a microphone whose packets the page is told are refused -- so the wire keeps
//! its shape and a client learns the absence rather than a hang. The Windows audio milestone
//! lands in the place this file holds.

use std::sync::Arc;

use bytes::Bytes;
use log::info;
use tokio::sync::broadcast;

use crate::settings::settings;

/// How the capture frames its packets: the transport's choice, the same wire contract the Linux
/// engine reads. No capture runs off Linux to frame one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    /// pcmflux's two-byte header and RED framing on the wire, gated on the clients (WebSockets).
    Headered,
    /// Bare Opus packets for an RTP packetizer that applies RED itself (WebRTC).
    Raw,
}

/// One encoded frame as the broadcast carries it, the same shape the Linux engine's carries
/// and the fan-out and the suites read; off Linux nothing is ever carried.
#[derive(Clone, Debug)]
pub struct AudioPacket {
    /// The frame's bytes.
    pub data: Bytes,
    /// pcmflux's running sample count before this frame.
    pub pts: u64,
    /// The frame's start on the engine's continuous 48 kHz clock.
    pub rtp_pts: u64,
    /// Channels the frame's Opus carries.
    pub channels: u8,
    /// A surround capture's stereo fold.
    pub companion: bool,
}

/// What became of one microphone packet; off Linux every packet is `Refused`, which the
/// transport tells the page once as `MICROPHONE_DISABLED`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicWrite {
    /// Queued for playback.
    Written,
    /// The microphone is off for this session.
    Refused,
    /// Dropped for now.
    Skipped,
}

/// The shared audio engine where no capture runs: a start reports the absence a Linux start
/// refused by the settings does, and the broadcast stays silent, so a subscriber parks until
/// the process ends exactly as an idle Linux engine's does.
pub struct AudioEngine {
    framing: Framing,
    device_name: String,
    tx: broadcast::Sender<AudioPacket>,
}

impl AudioEngine {
    /// An engine framing its packets for one transport, reading the configured capture device
    /// so the virtual-microphone provisioning names what the settings named. Nothing runs until
    /// a start, and no start starts anything.
    pub fn new(framing: Framing) -> Arc<Self> {
        let device_name = settings().audio_device_name().to_string();
        let (tx, _) = broadcast::channel(1);
        Arc::new(AudioEngine {
            framing,
            device_name,
            tx,
        })
    }

    /// The transport's framing.
    pub fn framing(&self) -> Framing {
        self.framing
    }

    /// The configured capture device (`audio_device_name`), as the microphone provisioning
    /// derives the capture sink from it.
    pub fn audio_device_name(&self) -> &str {
        &self.device_name
    }

    /// Start; always reports that no capture runs, one line telling the story, so a client
    /// unmuting is served the same refusal a settings-disabled session serves it.
    pub async fn start(self: &Arc<Self>) -> bool {
        info!(target: "audio", "Audio capture unavailable: no audio backend is wired for this OS.");
        false
    }

    /// Stop; nothing runs, so the call reports that it stopped nothing.
    pub async fn stop(self: &Arc<Self>) -> bool {
        false
    }

    /// No capture runs off Linux.
    pub fn is_capturing(&self) -> bool {
        false
    }

    /// No capture runs to retarget, so the bitrate is held for a start that does not come.
    pub fn set_bitrate(&self, _bps: i32) {}

    /// No capture runs off Linux to hand a stereo companion.
    pub async fn set_stereo_companion(self: &Arc<Self>, _wanted: bool) {}

    /// A receiver of every packet from now on; none ever arrives.
    pub fn subscribe(&self) -> broadcast::Receiver<AudioPacket> {
        self.tx.subscribe()
    }

    /// A page joined; nothing runs to gate on it.
    pub fn client_joined(self: &Arc<Self>, _id: u64) {}

    /// A page said what it decodes; nothing runs to gate on it.
    pub fn client_settings(&self, _id: u64, _audio_redundancy: bool) {}

    /// A page left; nothing runs to stop on its leaving.
    pub async fn client_left(self: &Arc<Self>, _id: u64, _ends_capture: bool) {}

    /// The start policy decides what a session starts with; nothing starts.
    pub async fn apply_initial_policy(
        self: &Arc<Self>,
        _display_id: &str,
        _viewer: bool,
        _others_listen: bool,
    ) {
    }
}

/// The sound-server control plane where no sound server runs: `open` never opens, so every
/// caller that reaches for one is told the connection could not be had -- the same answer the
/// Linux control plane gives a server it cannot reach -- and the provisioning around it reports
/// that nothing was loaded.
pub struct AudioControl;

impl AudioControl {
    /// A control plane named for its client; it never opens.
    pub fn new(_client_name: &str) -> Self {
        AudioControl
    }

    /// Never opens: no sound server runs off Linux, one line saying so.
    pub async fn open(&mut self) -> bool {
        info!(target: "audio", "Sound server control unavailable: no audio backend is wired for this OS.");
        false
    }

    /// Nothing was opened, so there is nothing to close.
    pub async fn close(&mut self) {}

    /// No sound server runs to load a module on, so no microphone is loaded and none owned.
    pub async fn ensure_virtual_microphone(
        &mut self,
        _audio_device_name: Option<&str>,
        _capturing: bool,
    ) -> (Option<u32>, bool) {
        (None, false)
    }

    /// No module was ever loaded, so there is nothing to unload; the call reports that nothing
    /// was taken down.
    pub async fn unload_module(&mut self, _index: u32) -> bool {
        false
    }

    /// No sound server runs to list the recorders of.
    pub async fn recorders(&mut self, _source_names: &[&str]) -> Vec<String> {
        Vec::new()
    }
}

/// Provision the sink a recording captures; off Linux no sound server runs to provision one, so
/// the recording proceeds video-only, the same shape a Linux recording whose capture does not
/// start takes.
pub async fn ensure_capture_sink(_audio_device_name: Option<&str>, _client_name: &str) -> bool {
    false
}

/// One connection's microphone path where no playback runs: `open` reports that no control
/// plane could be reached, so every packet is refused and the page is told once,
/// `MICROPHONE_DISABLED` -- the answer the Linux path serves a sound server it could not reach.
pub struct MicrophoneUplink;

impl MicrophoneUplink {
    /// An uplink for the connection `peer` names in the log; nothing runs off Linux.
    pub fn new(_engine: Arc<AudioEngine>, _peer: &str) -> Self {
        MicrophoneUplink
    }

    /// Reports that no control plane could be reached.
    pub async fn open(&mut self) -> bool {
        false
    }

    /// Refused: this OS offers nowhere to play the packet.
    pub async fn write(&mut self, _packet: &[u8]) -> MicWrite {
        MicWrite::Refused
    }

    /// Nothing was opened, so there is nothing to close.
    pub async fn close(&mut self) {}
}
