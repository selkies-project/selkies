/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The pixelflux glue both transports share: one `DisplayCapture` per display, holding a
//! `pixelflux::ScreenCapture` whose frames arrive on pixelflux's deliver thread through a
//! `FrameSink` and fan out over a tokio broadcast channel to every relay that subscribed,
//! each stripe's bytes shared with the encoder's buffer and never copied.
//!
//! The channel never blocks the deliver thread: a subscriber that falls behind loses the
//! oldest frames from its own view of the channel and learns it did (`Lagged`), which is
//! what a per-client relay uses to resynchronize, so one slow client can never back
//! pressure the shared pipeline. The capture's live controls (bitrate, link rate, frame
//! rate, tunables, reference reports) pass straight through to pixelflux, and what the
//! capture runs on is read from pixelflux's report (`stream_info`) rather than inferred.

use std::sync::Arc;

use bytes::Bytes;
use log::debug;
use pixelflux::sink::{Frame, FrameSink};
use pixelflux::{CaptureSettings, Error, LiveTunables, ScreenCapture, StreamInfo, StreamStats};
use tokio::sync::broadcast;

use crate::core::DisplayId;

/// Every stripe of one encoded frame, in capture order, as pixelflux delivered it.
pub struct FrameBatch {
    pub frames: Vec<Frame>,
}

impl FrameBatch {
    /// The frame id every stripe of the batch carries.
    pub fn frame_id(&self) -> Option<i32> {
        self.frames.first().map(|f| f.frame_id)
    }

    /// The total encoded bytes of the batch.
    pub fn len(&self) -> usize {
        self.frames.iter().map(|f| f.data.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

/// The encoder's buffer behind a stripe, as the owner a `Bytes` borrows from.
struct SharedBuffer(Arc<Vec<u8>>);

impl AsRef<[u8]> for SharedBuffer {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// A stripe's bytes as a `Bytes` sharing the encoder's buffer, for a socket write.
pub fn frame_bytes(frame: &Frame) -> Bytes {
    Bytes::from_owner(SharedBuffer(frame.data.clone()))
}

/// Frames the channel holds for a subscriber before it is told it lagged: about a second at
/// 60 fps, more than any relay's own budget, so lagging is decided by the relay's policy and
/// never by the channel.
const CHANNEL_DEPTH: usize = 64;

/// The sink pixelflux calls on its deliver thread: one `send` per batch, never blocking.
struct Fanout {
    tx: broadcast::Sender<Arc<FrameBatch>>,
}

impl FrameSink for Fanout {
    fn frames(&self, frames: Vec<Frame>) {
        // No receiver is an ordinary moment (a capture outliving its last client); the
        // batch is dropped with it.
        let _ = self.tx.send(Arc::new(FrameBatch { frames }));
    }
}

/// One display's capture: the pixelflux handle and the channel its frames fan out on.
pub struct DisplayCapture {
    display_id: DisplayId,
    capture: Arc<ScreenCapture>,
    tx: broadcast::Sender<Arc<FrameBatch>>,
}

impl DisplayCapture {
    /// A capture for `display_id`, not yet started.
    pub fn new(display_id: DisplayId) -> Self {
        let (tx, _) = broadcast::channel(CHANNEL_DEPTH);
        Self {
            display_id,
            capture: Arc::new(ScreenCapture::new()),
            tx,
        }
    }

    pub fn display_id(&self) -> &str {
        &self.display_id
    }

    /// Start (or reconfigure in place, where pixelflux can) with `settings`; frames flow to
    /// every subscriber from here on.
    pub fn start(&self, settings: CaptureSettings) -> Result<(), Error> {
        debug!(target: "display", "Starting capture of {}: {}x{} {} at {} fps",
            self.display_id, settings.width, settings.height, settings.codec.name(), settings.target_fps);
        let sink: Arc<dyn FrameSink> = Arc::new(Fanout {
            tx: self.tx.clone(),
        });
        self.capture.start_capture(sink, settings)
    }

    /// Stop the capture; subscribers see no more frames but keep their receivers.
    pub fn stop(&self) -> Result<(), Error> {
        debug!(target: "display", "Capture stopping for {}", self.display_id);
        self.capture.stop_capture()
    }

    /// A receiver of every batch from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<FrameBatch>> {
        self.tx.subscribe()
    }

    /// How many relays receive this capture.
    pub fn receivers(&self) -> usize {
        self.tx.receiver_count()
    }

    pub fn is_capturing(&self) -> bool {
        self.capture.is_capturing()
    }

    /// The pixelflux handle, for the controls not wrapped here (injection, outputs).
    pub fn inner(&self) -> &Arc<ScreenCapture> {
        &self.capture
    }

    pub fn request_idr_frame(&self) -> Result<(), Error> {
        self.capture.request_idr_frame()
    }

    /// A frame a client could not decode: taken out of the encoder's references, so the
    /// stream predicts past it without a key frame (AGENTS.md invariant).
    pub fn invalidate_reference(&self, frame_id: u16, withheld: bool) -> Result<(), Error> {
        self.capture.invalidate_reference(frame_id, withheld)
    }

    pub fn acknowledge_reference(&self, frame_id: u16, held: bool) -> Result<(), Error> {
        self.capture.acknowledge_reference(frame_id, held)
    }

    pub fn update_video_bitrate(&self, kbps: i32) -> Result<(), Error> {
        self.capture.update_video_bitrate(kbps)
    }

    pub fn update_link_rate(&self, kbps: i32) -> Result<(), Error> {
        self.capture.update_link_rate(kbps)
    }

    pub fn update_framerate(&self, fps: f64) -> Result<(), Error> {
        self.capture.update_framerate(fps)
    }

    pub fn update_vbv_multiplier(&self, multiplier: f64) -> Result<(), Error> {
        self.capture.update_vbv_multiplier(multiplier)
    }

    pub fn update_tunables(&self, settings: &CaptureSettings) -> Result<(), Error> {
        self.capture.update_tunables(settings)
    }

    /// The live tunables a settings value carries, for a reconfigure without a restart.
    pub fn live_tunables(settings: &CaptureSettings) -> LiveTunables {
        LiveTunables::from_settings(settings)
    }

    pub fn update_capture_region(
        &self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> Result<(), Error> {
        self.capture.update_capture_region(x, y, width, height)
    }

    pub fn stream_info(&self) -> Option<StreamInfo> {
        self.capture.stream_info()
    }

    pub fn stream_stats(&self) -> Option<StreamStats> {
        self.capture.stream_stats()
    }

    pub fn active_codec(&self) -> Option<String> {
        self.capture.active_codec().ok().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pixelflux::encoders::Codec;
    use pixelflux::sink::{FrameTiming, Reference};

    #[test]
    fn fanout_shares_the_buffer_and_tells_a_lagging_subscriber() {
        let cap = DisplayCapture::new("primary".into());
        let mut rx = cap.subscribe();
        let sink = Fanout { tx: cap.tx.clone() };
        let data = Arc::new(vec![1u8, 2, 3]);
        sink.frames(vec![Frame {
            data: data.clone(),
            codec: Codec::H264,
            stripe_y_start: 0,
            stripe_height: 0,
            frame_id: 7,
            timing: FrameTiming::default(),
            reference: Reference::None,
        }]);
        let batch = rx.try_recv().unwrap();
        assert_eq!(batch.frame_id(), Some(7));
        let bytes = frame_bytes(&batch.frames[0]);
        assert_eq!(bytes.as_ptr(), data.as_ptr());
        for i in 0..(CHANNEL_DEPTH + 2) {
            sink.frames(vec![Frame {
                data: data.clone(),
                codec: Codec::H264,
                stripe_y_start: 0,
                stripe_height: 0,
                frame_id: i as i32,
                timing: FrameTiming::default(),
                reference: Reference::None,
            }]);
        }
        assert!(matches!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Lagged(_))
        ));
    }
}
