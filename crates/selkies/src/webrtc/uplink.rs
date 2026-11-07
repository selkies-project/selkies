/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A peer's microphone and camera: what its receive-only sections carry into the desktop.
//!
//! The encoded payload goes straight into pcmflux or pixelflux, undecoded here. RED (loss
//! resilience) is gated by `audio_redundancy`: when on, the offer takes it and pcmflux de-frames
//! and loss-recovers each payload before decoding, the RTP timestamp anchoring the redundant
//! blocks' offsets; when off the section offers plain Opus and packets are decoded directly.
//!
//! Only a controller or a live collaborator speaks into the desktop mixer, the verdict read per
//! packet so a handoff takes effect without renegotiation. The first packet opens the playback off
//! the task, dropping packets until it is ready; the shared virtual microphone is provisioned first
//! so apps recording the default source hear this one, and a provisioning failure does not block
//! playback. A playback that errors (the sound server restarted mid-run) drops the chunk and is torn
//! down for the next packet to reopen: swallowing it would leave this peer's mic silent for good.
//! Playback is per peer: a closing peer never silences the others.
//!
//! The camera's first frame brings the shared camera up off the task; frames until then are dropped
//! and the decoder's first key-frame request becomes a PLI, so the stream starts clean. The start
//! latch is released only on success, so a camera that cannot start is not retried per frame.

use std::sync::Arc;
use std::time::Instant;

use log::{error, info};
use pcmflux::AudioPlayback;
use selkies_rtc::rtp::codecs::Codec;
use selkies_rtc::rtp::receiver::ReceivedFrame;
use selkies_rtc::signaling::ClientType;
use tokio::sync::mpsc;

use crate::audio::microphone_playback_settings;
use crate::core::Role;
use crate::webcam::{VirtualWebcam, codec_by_name, shared_webcam, webcam_uplink_allowed};
use crate::webrtc::engine::RtcEngine;
use crate::webrtc::peer::Uplink;

/// Packets a microphone's playback task holds; a stall past it drops them.
const MIC_QUEUE: usize = 64;
/// Floor between a camera's key-frame requests.
const CAMERA_KEYFRAME_FLOOR_S: f64 = 0.25;

/// One peer's microphone: the playback task behind a queue.
#[derive(Default)]
pub struct MicState {
    queue: Option<mpsc::Sender<(Vec<u8>, Option<i64>)>>,
    denied_logged: bool,
}

impl MicState {
    /// Retire the peer's playback.
    pub fn stop(self) {
        drop(self.queue);
    }
}

/// One peer's camera sink.
#[derive(Default)]
pub struct WebcamState {
    starting: bool,
    denied_logged: bool,
    codec: Option<String>,
    last_pli: Option<Instant>,
}

impl WebcamState {
    /// Retire the peer's sink; the shared camera outlives peers.
    pub fn close(self) {}
}

/// Route one frame of an uplink, on the peer's media task.
pub fn on_frame(engine: &Arc<RtcEngine>, peer: &str, uplink: Uplink, frame: ReceivedFrame) {
    match uplink {
        Uplink::Microphone => on_mic(engine, peer, frame),
        Uplink::Webcam => on_webcam(engine, peer, frame),
    }
}

fn on_mic(engine: &Arc<RtcEngine>, peer: &str, frame: ReceivedFrame) {
    if frame.data.is_empty() {
        return;
    }
    let red = frame.codec == Codec::Red;
    let timestamp = red.then_some(frame.rtp_timestamp as i64);
    let (client_type, token) = {
        let peers = engine.peers.lock();
        let Some(entry) = peers.get(peer) else { return };
        (entry.client_type, entry.client_token.clone())
    };
    let allowed =
        client_type != ClientType::Viewer || engine.viewer_is_collaborator(token.as_deref());
    let queue = {
        let mut peers = engine.peers.lock();
        let Some(state) = peers.get_mut(peer).and_then(|p| p.mic.as_mut()) else {
            return;
        };
        if !allowed {
            if !state.denied_logged {
                state.denied_logged = true;
                info!(target: "webrtc", "Dropping microphone audio from a view-only peer (no m/k authority).");
            }
            return;
        }
        state
            .queue
            .get_or_insert_with(|| spawn_playback(engine.clone()))
            .clone()
    };
    let _ = queue.try_send((frame.data.to_vec(), timestamp));
}

/// A peer's playback task: opens on the first packet and reopens after a failure.
fn spawn_playback(engine: Arc<RtcEngine>) -> mpsc::Sender<(Vec<u8>, Option<i64>)> {
    let (tx, mut rx) = mpsc::channel::<(Vec<u8>, Option<i64>)>(MIC_QUEUE);
    tokio::spawn(async move {
        let mut playback: Option<Arc<AudioPlayback>> = None;
        while let Some((packet, timestamp)) = rx.recv().await {
            if playback.is_none() {
                engine.host().provision_virtual_mic().await;
                let opened = tokio::task::spawn_blocking(|| {
                    let playback = Arc::new(AudioPlayback::new());
                    playback
                        .start(&microphone_playback_settings())
                        .map(|()| playback)
                })
                .await;
                match opened {
                    Ok(Ok(opened)) => playback = Some(opened),
                    Ok(Err(e)) => {
                        error!(target: "webrtc", "WebRTC mic playback start failed: {e}");
                        continue;
                    }
                    Err(e) => {
                        error!(target: "webrtc", "WebRTC mic playback start failed: {e}");
                        continue;
                    }
                }
            }
            let Some(current) = playback.clone() else {
                continue;
            };
            let written = match timestamp {
                Some(ts) => current.write_red(&packet, ts),
                None => current.write(&packet),
            };
            if let Err(e) = written {
                error!(target: "webrtc", "WebRTC mic playback write failed: {e}");
                playback = None;
                tokio::task::spawn_blocking(move || {
                    current.stop();
                });
            }
        }
        if let Some(playback) = playback {
            tokio::task::spawn_blocking(move || {
                playback.stop();
            });
        }
    });
    tx
}

fn codec_name(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "h264",
        Codec::H265 => "h265",
        Codec::Vp8 => "vp8",
        Codec::Vp9 => "vp9",
        Codec::Av1 => "av1",
        _ => "",
    }
}

fn on_webcam(engine: &Arc<RtcEngine>, peer: &str, frame: ReceivedFrame) {
    let (client_type, token) = {
        let peers = engine.peers.lock();
        let Some(entry) = peers.get(peer) else { return };
        (entry.client_type, entry.client_token.clone())
    };
    let role = match client_type {
        ClientType::Controller => Role::Controller,
        _ if engine.viewer_is_collaborator(token.as_deref()) => Role::Collaborator,
        _ => Role::Viewer,
    };
    let webcam = shared_webcam();
    let allowed = webcam_uplink_allowed(role);
    let name = codec_name(frame.codec);
    let codec = codec_by_name(name);
    {
        let mut peers = engine.peers.lock();
        let Some(state) = peers.get_mut(peer).and_then(|p| p.webcam.as_mut()) else {
            return;
        };
        if !allowed {
            if !state.denied_logged {
                state.denied_logged = true;
                info!(target: "webrtc", "Dropping webcam video from a peer without webcam authority.");
            }
            return;
        }
        let Some(codec) = codec.filter(|_| !frame.data.is_empty()) else {
            return;
        };
        if state.codec.as_deref() != Some(name) {
            state.codec = Some(name.to_string());
            info!(target: "webcam", "Webcam uplink carries {name}.");
        }
        if webcam.needs_ensure(Some(codec)) && !state.starting {
            state.starting = true;
            let (engine, peer, webcam) = (engine.clone(), peer.to_string(), webcam.clone());
            tokio::spawn(async move {
                if webcam.ensure(Some(codec)).await.is_some()
                    && let Some(state) = engine
                        .peers
                        .lock()
                        .get_mut(&peer)
                        .and_then(|p| p.webcam.as_mut())
                {
                    state.starting = false;
                }
            });
        }
    }
    let Some(codec) = codec else { return };
    if webcam.needs_ensure(Some(codec)) && webcam.camera().is_none() {
        return;
    }
    if VirtualWebcam::keyframe_wanted(webcam.push(&frame.data, codec, false, 0, false)) {
        let mut peers = engine.peers.lock();
        let Some(entry) = peers.get_mut(peer) else {
            return;
        };
        let handle = entry.peer.clone();
        let Some(state) = entry.webcam.as_mut() else {
            return;
        };
        let due = state
            .last_pli
            .is_none_or(|at| at.elapsed().as_secs_f64() >= CAMERA_KEYFRAME_FLOOR_S);
        if due {
            state.last_pli = Some(Instant::now());
            handle.request_uplink_keyframe();
        }
    }
}
