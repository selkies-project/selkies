/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What the full-frame software sessions share: the planar picture a packed host frame is
//! converted into, at 8 or 10 bits, the constant quantizer that follows the session quality index, the
//! rate-control settings a live change is compared against, and the frames an encoder has
//! been handed but not yet answered.

use std::collections::VecDeque;

use super::frame_rate::FrameRate;
use super::software::{convert_to_yuv_mt, convert_to_yuv10_mt};
use super::{QP_HYSTERESIS_LIMIT, vbv_bits};
use crate::CaptureSettings;

/// A planar picture, 4:2:0 or 4:4:4, with tightly packed rows: 8-bit samples in `y`, `u`, and
/// `v`, or 10-bit ones in `y16`, `u16`, and `v16`, the planes of the other depth staying empty.
pub struct Planes {
    pub width: usize,
    pub height: usize,
    pub i444: bool,
    pub bit_depth: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub y16: Vec<u16>,
    pub u16: Vec<u16>,
    pub v16: Vec<u16>,
}

impl Planes {
    pub fn new(width: usize, height: usize, i444: bool, bit_depth: u32) -> Self {
        let (cw, ch) = if i444 {
            (width, height)
        } else {
            (width.div_ceil(2), height.div_ceil(2))
        };
        let (narrow, wide) = if bit_depth > 8 { (0, 1) } else { (1, 0) };
        Self {
            width,
            height,
            i444,
            bit_depth,
            y: vec![0; width * height * narrow],
            u: vec![0; cw * ch * narrow],
            v: vec![0; cw * ch * narrow],
            y16: vec![0; width * height * wide],
            u16: vec![0; cw * ch * wide],
            v16: vec![0; cw * ch * wide],
        }
    }

    pub fn chroma_width(&self) -> usize {
        if self.i444 {
            self.width
        } else {
            self.width.div_ceil(2)
        }
    }

    /// The planes as a library reads them, whichever depth they hold.
    pub fn pointers(&mut self) -> [*mut u8; 3] {
        if self.bit_depth > 8 {
            [
                self.y16.as_mut_ptr().cast(),
                self.u16.as_mut_ptr().cast(),
                self.v16.as_mut_ptr().cast(),
            ]
        } else {
            [
                self.y.as_mut_ptr(),
                self.u.as_mut_ptr(),
                self.v.as_mut_ptr(),
            ]
        }
    }

    /// The bytes a sample takes.
    pub fn sample_bytes(&self) -> usize {
        if self.bit_depth > 8 { 2 } else { 1 }
    }

    /// The bytes all three planes hold.
    pub fn byte_len(&self) -> usize {
        self.y.len()
            + self.u.len()
            + self.v.len()
            + 2 * (self.y16.len() + self.u16.len() + self.v16.len())
    }

    /// Convert a packed host frame (`stride` bytes per row, R,G,B,A when `rgba`, else B,G,R,A)
    /// into the planes across `threads` bands, with the BT.709 matrix at `full_range`, or BT.601
    /// where `bt601` names a codec whose bitstream can carry no other, which has no 10 bits.
    pub fn convert(
        &mut self,
        pixels: &[u8],
        stride: usize,
        rgba: bool,
        full_range: bool,
        bt601: bool,
        threads: usize,
    ) -> Result<(), String> {
        let cw = self.chroma_width();
        if self.bit_depth > 8 {
            check_host_frame(pixels, stride, self.width, self.height)?;
            convert_to_yuv10_mt(
                pixels,
                stride,
                self.width,
                self.height,
                rgba,
                self.i444,
                full_range,
                &mut self.y16,
                &mut self.u16,
                &mut self.v16,
                (self.width, cw),
                threads,
            );
            return Ok(());
        }
        convert_into(
            pixels,
            stride,
            self.width,
            self.height,
            rgba,
            self.i444,
            full_range,
            bt601,
            threads,
            &mut self.y,
            &mut self.u,
            &mut self.v,
            (self.width, cw),
        )
    }
}

/// Convert a packed host frame into planes a library owns, at `strides` (luma, chroma) bytes
/// per row, as `Planes::convert` does into its own.
#[allow(clippy::too_many_arguments)]
pub fn convert_into(
    pixels: &[u8],
    stride: usize,
    width: usize,
    height: usize,
    rgba: bool,
    i444: bool,
    full_range: bool,
    bt601: bool,
    threads: usize,
    y: &mut [u8],
    u: &mut [u8],
    v: &mut [u8],
    strides: (usize, usize),
) -> Result<(), String> {
    check_host_frame(pixels, stride, width, height)?;
    convert_to_yuv_mt(
        pixels,
        stride as u32,
        width,
        height,
        rgba,
        i444,
        full_range,
        bt601,
        y,
        u,
        v,
        strides,
        threads,
    )
    .map_err(|e| format!("rgb-to-yuv conversion failed: {e:?}"))
}

/// Whether `pixels` holds a `width` x `height` packed picture at `stride` bytes per row.
pub fn check_host_frame(
    pixels: &[u8],
    stride: usize,
    width: usize,
    height: usize,
) -> Result<(), String> {
    let row_bytes = width * 4;
    let needed = if height == 0 {
        0
    } else {
        stride.checked_mul(height - 1).ok_or("stride overflow")? + row_bytes
    };
    if stride < row_bytes || pixels.len() < needed {
        return Err("Input buffer too small".into());
    }
    Ok(())
}

/// The constant quantizer of a session, moved toward the one the quality index selects: a
/// decrease sharpens the picture and applies at once, an increase waits out
/// `QP_HYSTERESIS_LIMIT` consecutive requests so transient motion does not make quality blink.
/// The domain is the codec's own (`Codec::quantizer`).
pub struct Quality {
    pub current: u32,
    counter: u32,
}

impl Quality {
    pub fn new(quantizer: u32) -> Self {
        Self {
            current: quantizer,
            counter: 0,
        }
    }

    /// The quantizer to program now for `target`, once the hysteresis admits the change.
    pub fn update(&mut self, target: u32) -> Option<u32> {
        if target == self.current {
            self.counter = 0;
            return None;
        }
        if target > self.current {
            self.counter += 1;
            if self.counter <= QP_HYSTERESIS_LIMIT {
                return None;
            }
        }
        self.counter = 0;
        self.current = target;
        Some(target)
    }
}

/// The rate-control and frame-rate settings a session was configured with, so a live change
/// re-programs the encoder only when a value it reads actually moved.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RateSettings {
    pub cbr: bool,
    /// Whether a constant-quality session is capped at a peak of `bitrate_kbps` with a VBV of
    /// `vbv_multiplier` frames (`CaptureSettings::video_vbv_multiplier`), fixed at its open.
    pub capped: bool,
    pub bitrate_kbps: i32,
    pub vbv_multiplier: f64,
    pub keyframe_interval_s: f64,
    pub fps: FrameRate,
    pub min_qp: i32,
    pub max_qp: i32,
}

impl RateSettings {
    pub fn new(settings: &CaptureSettings) -> Self {
        Self {
            cbr: settings.video_cbr_mode,
            capped: !settings.video_cbr_mode && settings.video_vbv_multiplier > 0.0,
            bitrate_kbps: settings.video_bitrate_kbps,
            vbv_multiplier: settings.video_vbv_multiplier,
            keyframe_interval_s: settings.keyframe_interval_s,
            fps: FrameRate::of(settings.target_fps),
            min_qp: settings.video_min_qp,
            max_qp: settings.video_max_qp,
        }
    }

    /// The settings of a session that runs its constant quality uncapped whatever a caller asks,
    /// whose bitrate then moves nothing.
    pub fn uncapped(self) -> Self {
        Self {
            capped: false,
            ..self
        }
    }

    /// The settings as they stand now, if a rate or frame-rate value the encoder was programmed
    /// with changed: a bitrate or VBV multiplier in CBR and in a capped CRF, the frame rate in
    /// any mode.
    pub fn changed(&self, settings: &CaptureSettings) -> Option<Self> {
        let next = Self {
            cbr: self.cbr,
            capped: self.capped,
            min_qp: self.min_qp,
            max_qp: self.max_qp,
            ..Self::new(settings)
        };
        let rate_moved = (self.cbr || self.capped)
            && (next.bitrate_kbps != self.bitrate_kbps
                || next.vbv_multiplier != self.vbv_multiplier);
        (rate_moved || next.fps != self.fps).then_some(next)
    }

    /// The constant-rate target in bits per second.
    pub fn bps(&self) -> u64 {
        (self.bitrate_kbps.max(0) as u64) * 1000
    }

    /// The VBV buffer, in bits, the constant-rate target is held to.
    pub fn vbv(&self) -> u32 {
        vbv_bits(
            self.bps().min(u32::MAX as u64) as u32,
            self.fps.fps(),
            self.keyframe_interval_s,
            self.vbv_multiplier,
        )
    }
}

/// The frames handed to an encoder and not yet answered, by the timestamp each was submitted
/// with, so a packet names the frame it encodes rather than the one submitted alongside it.
/// An encoder that pipelines answers a frame several later, and one that drops a frame never
/// answers it at all, so taking a timestamp discards every submission it passed over.
#[derive(Default)]
pub struct Pending(VecDeque<(u64, u16)>);

impl Pending {
    /// Record that the frame with wire id `frame_id` went in at `pts`.
    pub fn push(&mut self, pts: u64, frame_id: u16) {
        self.0.push_back((pts, frame_id));
    }

    /// Drop the last submission, for a frame the encoder refused outright.
    pub fn undo(&mut self) {
        self.0.pop_back();
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The wire id submitted at `pts`, dropping it and everything older; `None` where no
    /// submission still held carries that timestamp.
    pub fn take(&mut self, pts: u64) -> Option<u16> {
        let at = self.0.iter().position(|&(p, _)| p == pts)?;
        let id = self.0[at].1;
        self.0.drain(..=at);
        Some(id)
    }
}

/// The encode threads a software session spreads across, between one and `most`: one less than
/// the host's cores, or one alone where `cpu_encode_threading` is off. The conversion ahead of
/// the encoder runs on as many.
fn threads_up_to(settings: &CaptureSettings, most: usize) -> i32 {
    if !settings.cpu_encode_threading {
        return 1;
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .saturating_sub(1)
        .clamp(1, most) as i32
}

/// The encode threads a full-frame H.265, VP8, VP9, or AV1 software session spreads across
/// (`threads_up_to` eight): x265's and kvazaar's wavefront, libvpx's token partitions and
/// rows, and SVT-AV1's logical processors.
pub fn encode_threads(settings: &CaptureSettings) -> i32 {
    threads_up_to(settings, 8)
}

/// The encode threads a full-frame H.264 software session spreads across (`threads_up_to`
/// four): x264's sliced threads, and OpenH264's threads over its four slices, more than which
/// would buy nothing.
pub fn h264_fullframe_threads(settings: &CaptureSettings) -> i32 {
    threads_up_to(settings, 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every software session's thread count follows `cpu_encode_threading`: one thread with it
    /// off, and with it on one less than the cores within the session's ceiling, one at least.
    #[test]
    fn cpu_encode_threading_off_leaves_every_session_one_thread() {
        let off = CaptureSettings {
            cpu_encode_threading: false,
            ..Default::default()
        };
        assert_eq!(encode_threads(&off), 1);
        assert_eq!(h264_fullframe_threads(&off), 1);
        let on = CaptureSettings::default();
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        assert_eq!(
            encode_threads(&on),
            cores.saturating_sub(1).clamp(1, 8) as i32
        );
        assert_eq!(
            h264_fullframe_threads(&on),
            cores.saturating_sub(1).clamp(1, 4) as i32
        );
    }

    #[test]
    fn a_quality_increase_applies_at_once_and_a_drop_waits() {
        let mut q = Quality::new(30);
        assert_eq!(q.update(30), None);
        assert_eq!(q.update(20), Some(20), "a lower quantizer applies at once");
        for _ in 0..QP_HYSTERESIS_LIMIT {
            assert_eq!(q.update(40), None);
        }
        assert_eq!(
            q.update(40),
            Some(40),
            "a higher one waits out the hysteresis"
        );
        assert_eq!(q.update(45), None);
        assert_eq!(
            q.update(40),
            None,
            "a request back at the current value resets the count"
        );
        assert_eq!(q.current, 40);
    }

    #[test]
    fn a_rate_change_is_noticed_only_where_the_encoder_reads_it() {
        let mut settings = CaptureSettings {
            target_fps: 30.0,
            video_bitrate_kbps: 4000,
            ..Default::default()
        };
        let rate = RateSettings::new(&settings);
        assert_eq!(rate.changed(&settings), None);
        settings.video_bitrate_kbps = 8000;
        assert_eq!(
            rate.changed(&settings),
            None,
            "a bitrate moves nothing in constant-quality mode"
        );
        settings.target_fps = 60.0;
        assert_eq!(
            rate.changed(&settings).map(|r| r.fps),
            Some(FrameRate { num: 60, den: 1 })
        );
        let ntsc = RateSettings::new(&CaptureSettings {
            target_fps: 60000.0 / 1001.0,
            ..settings.clone()
        });
        assert_eq!(
            ntsc.fps,
            FrameRate {
                num: 60000,
                den: 1001
            }
        );
        assert!(
            ntsc.changed(&settings).is_some(),
            "60 fps moves a session at 59.94"
        );
        settings.video_cbr_mode = true;
        let cbr = RateSettings::new(&settings);
        settings.video_bitrate_kbps = 9000;
        assert_eq!(cbr.changed(&settings).map(|r| r.bitrate_kbps), Some(9000));
        assert_eq!(cbr.bps(), 8_000_000);
        assert!(!cbr.capped);
    }

    /// A capped constant-quality session reads its peak as a constant rate reads its target; a
    /// session that cannot cap reads neither.
    #[test]
    fn a_capped_crf_session_notices_its_peak_move() {
        let mut settings = CaptureSettings {
            target_fps: 60.0,
            video_bitrate_kbps: 4000,
            video_vbv_multiplier: crate::encoders::CAPPED_VBV_FRAMES,
            ..Default::default()
        };
        let capped = RateSettings::new(&settings);
        assert!(capped.capped && !capped.cbr);
        let uncapped = capped.uncapped();
        settings.video_bitrate_kbps = 2000;
        assert_eq!(
            capped
                .changed(&settings)
                .map(|r| (r.bitrate_kbps, r.capped)),
            Some((2000, true))
        );
        assert_eq!(uncapped.changed(&settings), None);
        assert!(
            !RateSettings::new(&CaptureSettings {
                video_vbv_multiplier: 0.0,
                ..settings
            })
            .capped,
            "no buffer leaves the rate factor uncapped"
        );
    }

    /// A packet names the frame it encodes however deep the encoder pipelines, the frames it
    /// passed over go with it, and a timestamp never submitted names nothing.
    #[test]
    fn a_packet_names_the_frame_it_encodes() {
        let mut pending = Pending::default();
        assert!(pending.is_empty());
        for (pts, id) in [(0u64, 7u16), (1, 8), (2, 9)] {
            pending.push(pts, id);
        }
        assert_eq!(pending.take(99), None, "a timestamp never submitted");
        assert_eq!(
            pending.take(1),
            Some(8),
            "the encoder answered two frames in"
        );
        assert_eq!(
            pending.take(0),
            None,
            "the frame it passed over went with it"
        );
        assert_eq!(pending.take(2), Some(9));
        assert!(pending.is_empty());
        pending.push(3, 10);
        pending.undo();
        assert_eq!(pending.take(3), None, "a refused frame was never submitted");
    }

    #[test]
    fn a_frame_shorter_than_its_stride_says_so() {
        assert!(check_host_frame(&[0; 16 * 4 * 4], 64, 16, 4).is_ok());
        assert!(check_host_frame(&[0; 16 * 4 * 4 - 1], 64, 16, 4).is_err());
        assert!(
            check_host_frame(&[0; 16 * 4 * 4], 60, 16, 4).is_err(),
            "a stride shorter than a row"
        );
        let mut planes = Planes::new(4, 2, false, 8);
        assert_eq!((planes.u.len(), planes.chroma_width()), (2, 2));
        assert!(
            planes
                .convert(&[0; 4 * 4 * 2], 16, false, false, false, 1)
                .is_ok()
        );
        assert!(
            Planes::new(4, 2, true, 8)
                .convert(&[0; 4 * 4], 16, false, true, false, 1)
                .is_err()
        );
    }
}
