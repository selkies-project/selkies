/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The CPU encode path: JPEG through turbojpeg in horizontal stripes, and H.264 through the
//! build's software encoder — libx264 with the `gpl` feature, Cisco OpenH264 without it
//! (`software_encoder`) — as one whole-frame session.
//!
//! JPEG frames are split into stripes coded in parallel via rayon; each stripe is hashed against
//! the previous frame for change detection, and only dirty stripes are sent, statelessly. H.264
//! is one region covering the frame, its encoder kept across frames for inter-prediction, and
//! both libraries emit the same wire framing. Both run the same per-region send and cleanup
//! decision (`encode_cpu`).

use super::codec::{Codec, push_jpeg_header};
#[cfg(feature = "gpl")]
use super::codec::{
    FRAME_DELTA, FRAME_INTRA, FRAME_KEY, h264_dpb_frames, h264_level, h264_reference_level,
    push_video_header,
};
#[cfg(feature = "gpl")]
use super::frame_rate::FrameRate;
use super::reference::Reference;
#[cfg(feature = "gpl")]
use super::reference::{Invalidation, ReferenceWindow};
#[cfg(feature = "gpl")]
use super::sps::{WideFrameNum, h264_frame_num_range};
use crate::CaptureSettings;
use crate::ledger::{BlockLedger, RefinePace};
use crate::pipeline::{Cleanup, Damage};
use rayon::prelude::*;
use smithay::utils::{Physical, Rectangle};
#[cfg(feature = "gpl")]
use std::ffi::CString;
#[cfg(feature = "gpl")]
use std::ptr;
use std::sync::Arc;
use yuv::{
    BufferStoreMut, YuvBiPlanarImageMut, YuvConversionMode, YuvPlanarImageMut, YuvRange,
    YuvStandardMatrix,
};

/// Upper bound on the horizontal stripes the CPU encoder splits a frame into, so the
/// persistent per-stripe state vector can be reserved to a fixed capacity once, up front.
///
/// With the vector reserved to this size at startup, the per-frame resize to the actual stripe count
/// stays a cheap in-place adjustment that preserves each stripe's reused encoder and scratch buffers,
/// rather than a reallocation that would churn them whenever the count changes.
pub const MAX_STRIPE_CAPACITY: usize = 64;

/// Convert a packed BGRA/RGBA buffer to planar YUV (4:2:0 or 4:4:4) for the software H.264
/// encoders, spreading the conversion across up to `bands` threads so it never bottlenecks a frame.
///
/// **Why the band split exists.** Color conversion is a non-trivial slice of per-frame CPU, and a
/// whole-frame session (OpenH264, or a session of another codec) would otherwise convert its
/// entire image on one thread and stall the frame there. Splitting into horizontal bands spreads
/// that conversion over the rayon pool. The cut is horizontal because YUV planes are row-major, so
/// a horizontal boundary yields contiguous, non-overlapping plane sub-slices with no per-row seam
/// bookkeeping.
///
/// 1. **Plane strides**: `strides` gives the Y and chroma row pitches of the output planes
///    (tightly packed for the session's own buffers, padded for an AVFrame); the chroma planes are
///    `width` wide for 4:4:4 (`i444 == true`) or `width / 2` for 4:2:0. `rgba_input` selects
///    the source byte order and `i444` the subsampling; `full_range` selects the signal range,
///    **Full** for the 4:4:4 stream x264 and x265 declare and **Limited** for everything else.
///    The matrix is **BT.709**, whose primaries and transfer the sRGB desktop source already
///    carries, unless `bt601` marks a codec whose bitstream can name no other matrix (VP8).
///    Every encoder declares the matrix it was fed. All four `yuv` crate routines run in the
///    **Fast** conversion mode.
/// 2. **Band split**: `band_h` is `height / bands` floored to an even number and at least 2 rows
///    (a band under 2 rows is not worth a task). Keeping band boundaries even ensures a 4:2:0
///    chroma pair never straddles a seam. When `bands <= 1` or the whole image fits one band, the
///    conversion runs on the calling thread in place.
/// 3. **Parallel bands**: otherwise `src` and the three output planes are carved into contiguous
///    per-band sub-slices (chroma rows scaled by `uv_rows` — full height for 4:4:4, half for
///    4:2:0) and converted on the rayon pool, whose workers already exist, so a frame spawns no
///    thread. The final band absorbs any leftover rows, taking all remaining rows whenever fewer
///    than `band_h + 2` are left. The first error wins.
#[allow(clippy::too_many_arguments)]
pub(crate) fn convert_to_yuv_mt(
    src: &[u8],
    src_stride: u32,
    width: usize,
    height: usize,
    rgba_input: bool,
    i444: bool,
    full_range: bool,
    bt601: bool,
    y_buf: &mut [u8],
    u_buf: &mut [u8],
    v_buf: &mut [u8],
    strides: (usize, usize),
    bands: usize,
) -> Result<(), yuv::YuvError> {
    let (y_stride, uv_stride) = strides;
    let range = if full_range {
        YuvRange::Full
    } else {
        YuvRange::Limited
    };
    let matrix = if bt601 {
        YuvStandardMatrix::Bt601
    } else {
        YuvStandardMatrix::Bt709
    };

    let convert_band = |src_band: &[u8], y: &mut [u8], u: &mut [u8], v: &mut [u8], h: usize| {
        let mut img = YuvPlanarImageMut {
            y_plane: BufferStoreMut::Borrowed(y),
            y_stride: y_stride as u32,
            u_plane: BufferStoreMut::Borrowed(u),
            u_stride: uv_stride as u32,
            v_plane: BufferStoreMut::Borrowed(v),
            v_stride: uv_stride as u32,
            width: width as u32,
            height: h as u32,
        };
        match (i444, rgba_input) {
            (true, true) => yuv::rgba_to_yuv444(
                &mut img,
                src_band,
                src_stride,
                range,
                matrix,
                YuvConversionMode::Fast,
            ),
            (true, false) => yuv::bgra_to_yuv444(
                &mut img,
                src_band,
                src_stride,
                range,
                matrix,
                YuvConversionMode::Fast,
            ),
            (false, true) => yuv::rgba_to_yuv420(
                &mut img,
                src_band,
                src_stride,
                range,
                matrix,
                YuvConversionMode::Fast,
            ),
            (false, false) => yuv::bgra_to_yuv420(
                &mut img,
                src_band,
                src_stride,
                range,
                matrix,
                YuvConversionMode::Fast,
            ),
        }
    };

    let heights = band_heights(height, bands);
    if heights.len() <= 1 {
        return convert_band(src, y_buf, u_buf, v_buf, height);
    }

    let uv_rows = |rows: usize| if i444 { rows } else { rows / 2 };
    let mut jobs = Vec::new();
    let (mut src_rest, mut y_rest, mut u_rest, mut v_rest) = (src, y_buf, u_buf, v_buf);
    for h in heights {
        let (src_band, s_next) = src_rest.split_at(h * src_stride as usize);
        let (y_band, y_next) = y_rest.split_at_mut(h * y_stride);
        let (u_band, u_next) = u_rest.split_at_mut(uv_rows(h) * uv_stride);
        let (v_band, v_next) = v_rest.split_at_mut(uv_rows(h) * uv_stride);
        src_rest = s_next;
        y_rest = y_next;
        u_rest = u_next;
        v_rest = v_next;
        jobs.push((src_band, y_band, u_band, v_band, h));
    }
    jobs.into_par_iter()
        .map(|(src_band, y_band, u_band, v_band, h)| {
            convert_band(src_band, y_band, u_band, v_band, h)
        })
        .collect::<Result<Vec<()>, _>>()
        .map(|_| ())
}

/// The rows of each band a conversion of `height` rows splits into for up to `bands` threads
/// (`convert_to_yuv_mt`): `height / bands` floored to an even number and at least 2, the last band
/// taking the rest wherever fewer than a band and 2 rows would be left after it. One band where
/// `bands <= 1` or the image fits one.
fn band_heights(height: usize, bands: usize) -> Vec<usize> {
    let band_h = ((height / bands.max(1)) & !1).max(2);
    if bands <= 1 || height <= band_h {
        return vec![height];
    }
    let mut heights = Vec::with_capacity(bands + 1);
    let mut row = 0;
    while row < height {
        let h = if height - row < band_h + 2 {
            height - row
        } else {
            band_h
        };
        heights.push(h);
        row += h;
    }
    heights
}

/// Convert a packed buffer, B,G,R,A or with `rgba_input` R,G,B,A, to 8-bit 4:2:0 BT.709 limited
/// range as NV12, a luma plane and an interleaved chroma one `strides` bytes a row, the picture
/// x264 encodes in place (`Nv12Planes`), in the bands `convert_to_yuv_mt` splits it into. Its
/// samples are the ones `convert_to_yuv_mt` writes to I420, the chroma interleaved.
#[allow(clippy::too_many_arguments)]
pub(crate) fn convert_to_nv12_mt(
    src: &[u8],
    src_stride: u32,
    width: usize,
    height: usize,
    rgba_input: bool,
    y_buf: &mut [u8],
    uv_buf: &mut [u8],
    strides: (usize, usize),
    bands: usize,
) -> Result<(), yuv::YuvError> {
    let (y_stride, uv_stride) = strides;
    let convert_band = |src_band: &[u8], y: &mut [u8], uv: &mut [u8], h: usize| {
        let mut img = YuvBiPlanarImageMut {
            y_plane: BufferStoreMut::Borrowed(y),
            y_stride: y_stride as u32,
            uv_plane: BufferStoreMut::Borrowed(uv),
            uv_stride: uv_stride as u32,
            width: width as u32,
            height: h as u32,
        };
        let convert = if rgba_input {
            yuv::rgba_to_yuv_nv12
        } else {
            yuv::bgra_to_yuv_nv12
        };
        convert(
            &mut img,
            src_band,
            src_stride,
            YuvRange::Limited,
            YuvStandardMatrix::Bt709,
            YuvConversionMode::Fast,
        )
    };
    let heights = band_heights(height, bands);
    if heights.len() <= 1 {
        return convert_band(src, y_buf, uv_buf, height);
    }
    let mut jobs = Vec::with_capacity(heights.len());
    let (mut src_rest, mut y_rest, mut uv_rest) = (src, y_buf, uv_buf);
    for h in heights {
        let (src_band, s_next) = src_rest.split_at(h * src_stride as usize);
        let (y_band, y_next) = y_rest.split_at_mut(h * y_stride);
        let (uv_band, uv_next) = uv_rest.split_at_mut(h / 2 * uv_stride);
        src_rest = s_next;
        y_rest = y_next;
        uv_rest = uv_next;
        jobs.push((src_band, y_band, uv_band, h));
    }
    jobs.into_par_iter()
        .map(|(src_band, y_band, uv_band, h)| convert_band(src_band, y_band, uv_band, h))
        .collect::<Result<Vec<()>, _>>()
        .map(|_| ())
}

/// The fixed-point scale of the 10-bit conversion's coefficients.
const YUV10_SHIFT: u32 = 14;

/// The BT.709 coefficients that take 8-bit R, G, B to 10-bit Y, Cb, Cr at `full_range` or
/// limited range, scaled by `1 << YUV10_SHIFT`: a row each for Y, Cb, and Cr, then the luma
/// offset. The chroma offset is the 10-bit midpoint at either range.
fn yuv10_coefficients(full_range: bool) -> ([[i32; 3]; 3], i32) {
    const KR: f64 = 0.2126;
    const KB: f64 = 0.0722;
    const KG: f64 = 1.0 - KR - KB;
    let (luma, chroma, offset) = if full_range {
        (1023.0 / 255.0, 1023.0 / 255.0, 0)
    } else {
        (876.0 / 255.0, 896.0 / 255.0, 64)
    };
    let fixed = |k: f64| (k * (1 << YUV10_SHIFT) as f64).round() as i32;
    let (cb, cr) = (chroma / (2.0 * (1.0 - KB)), chroma / (2.0 * (1.0 - KR)));
    (
        [
            [fixed(luma * KR), fixed(luma * KG), fixed(luma * KB)],
            [fixed(-cb * KR), fixed(-cb * KG), fixed(cb * (1.0 - KB))],
            [fixed(cr * (1.0 - KR)), fixed(-cr * KG), fixed(-cr * KB)],
        ],
        offset,
    )
}

/// Convert a packed BGRA/RGBA buffer to planar 10-bit YUV (4:2:0 or 4:4:4) in 16-bit samples,
/// for the software encoders' 10-bit sessions, across up to `bands` threads as
/// `convert_to_yuv_mt` splits them.
///
/// The 10-bit samples are computed from the 8-bit source directly rather than widened from an
/// 8-bit conversion, so the two bits the encoder gains carry the precision an 8-bit Y, Cb, Cr
/// rounds away. The matrix is BT.709 at `full_range` or limited range, and a 4:2:0 chroma
/// sample is the conversion of the mean of its 2x2 block, which sites it at the block's
/// center as the 8-bit conversion does. `strides` are in samples.
#[allow(clippy::too_many_arguments)]
pub(crate) fn convert_to_yuv10_mt(
    src: &[u8],
    src_stride: usize,
    width: usize,
    height: usize,
    rgba_input: bool,
    i444: bool,
    full_range: bool,
    y_buf: &mut [u16],
    u_buf: &mut [u16],
    v_buf: &mut [u16],
    strides: (usize, usize),
    bands: usize,
) {
    let (y_stride, uv_stride) = strides;
    let (k, y_offset) = yuv10_coefficients(full_range);
    let (ri, bi) = if rgba_input { (0, 2) } else { (2, 0) };
    let half = 1 << (YUV10_SHIFT - 1);
    let luma = move |r: i32, g: i32, b: i32| {
        ((k[0][0] * r + k[0][1] * g + k[0][2] * b + half) >> YUV10_SHIFT) + y_offset
    };
    let chroma = move |row: &[i32; 3], r: i32, g: i32, b: i32, shift: u32| {
        (((row[0] * r + row[1] * g + row[2] * b + (1 << (shift - 1))) >> shift) + 512)
            .clamp(0, 1023) as u16
    };

    let convert_band = move |src: &[u8], y: &mut [u16], u: &mut [u16], v: &mut [u16], h: usize| {
        for row in 0..h {
            let line = &src[row * src_stride..row * src_stride + width * 4];
            let out = &mut y[row * y_stride..row * y_stride + width];
            for (px, y) in line.as_chunks::<4>().0.iter().zip(out.iter_mut()) {
                *y = luma(px[ri] as i32, px[1] as i32, px[bi] as i32) as u16;
            }
        }
        if i444 {
            for row in 0..h {
                let line = &src[row * src_stride..row * src_stride + width * 4];
                let cb = &mut u[row * uv_stride..row * uv_stride + width];
                let cr = &mut v[row * uv_stride..row * uv_stride + width];
                for ((px, cb), cr) in line
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .zip(cb.iter_mut())
                    .zip(cr.iter_mut())
                {
                    let (r, g, b) = (px[ri] as i32, px[1] as i32, px[bi] as i32);
                    *cb = chroma(&k[1], r, g, b, YUV10_SHIFT);
                    *cr = chroma(&k[2], r, g, b, YUV10_SHIFT);
                }
            }
            return;
        }
        let cw = width.div_ceil(2);
        for row in 0..h.div_ceil(2) {
            let top = &src[2 * row * src_stride..2 * row * src_stride + width * 4];
            let below = (2 * row + 1).min(h - 1);
            let bottom = &src[below * src_stride..below * src_stride + width * 4];
            let cb = &mut u[row * uv_stride..row * uv_stride + cw];
            let cr = &mut v[row * uv_stride..row * uv_stride + cw];
            for (col, (cb, cr)) in cb.iter_mut().zip(cr.iter_mut()).enumerate() {
                let left = 8 * col;
                let right = (left + 4).min(width * 4 - 4);
                let sum = |i: usize| {
                    top[left + i] as i32
                        + top[right + i] as i32
                        + bottom[left + i] as i32
                        + bottom[right + i] as i32
                };
                let (r, g, b) = (sum(ri), sum(1), sum(bi));
                *cb = chroma(&k[1], r, g, b, YUV10_SHIFT + 2);
                *cr = chroma(&k[2], r, g, b, YUV10_SHIFT + 2);
            }
        }
    };

    let heights = band_heights(height, bands);
    if heights.len() <= 1 {
        convert_band(src, y_buf, u_buf, v_buf, height);
        return;
    }
    let uv_rows = |rows: usize| if i444 { rows } else { rows.div_ceil(2) };
    let mut jobs = Vec::new();
    let (mut src_rest, mut y_rest, mut u_rest, mut v_rest) = (src, y_buf, u_buf, v_buf);
    let mut row = 0;
    for h in heights {
        let last = row + h >= height;
        let take = |len: usize, rows: usize, stride: usize| if last { len } else { rows * stride };
        let (src_band, s_next) = src_rest.split_at(take(src_rest.len(), h, src_stride));
        let (y_band, y_next) = y_rest.split_at_mut(take(y_rest.len(), h, y_stride));
        let (u_band, u_next) = u_rest.split_at_mut(take(u_rest.len(), uv_rows(h), uv_stride));
        let (v_band, v_next) = v_rest.split_at_mut(take(v_rest.len(), uv_rows(h), uv_stride));
        src_rest = s_next;
        y_rest = y_next;
        u_rest = u_next;
        v_rest = v_next;
        row += h;
        jobs.push((src_band, y_band, u_band, v_band, h));
    }
    jobs.into_par_iter()
        .for_each(|(src_band, y_band, u_band, v_band, h)| {
            convert_band(src_band, y_band, u_band, v_band, h)
        });
}

thread_local! {
    /// Reused libjpeg-turbo compressor kept per worker thread to avoid paying a
    /// `tjInitCompress`/`tjDestroy` round trip for every stripe of every frame.
    ///
    /// The striped JPEG path compresses one stripe per rayon worker, so the compressor is
    /// thread-local rather than shared: each worker creates its own lazily on first use and then
    /// holds it for the process lifetime. Making it thread-local also sidesteps the locking a shared
    /// compressor would otherwise need across the parallel stripe encoders.
    static JPEG_COMPRESSOR: std::cell::RefCell<Option<turbojpeg::Compressor>> =
        const { std::cell::RefCell::new(None) };
}

/// Process-global lock that serializes libx264 encoder open/close, because those calls are
/// not thread-safe yet several captures in one process open their sessions concurrently.
///
/// libx264 mutates process-global state inside `x264_encoder_open`/`x264_encoder_close`, so two
/// capture instances sharing one process can race that state and corrupt the heap. The lock is
/// deliberately held only around open and close, never around `x264_encoder_encode`, so
/// serializing setup costs nothing in the hot per-frame encode path.
#[cfg(feature = "gpl")]
static X264_OPEN_CLOSE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// What libx264 adds to a quantizer above 8 bits: its quantizer bounds, a frame's forced
/// quantizer, and the one it reports are all on a scale that starts six below zero for every
/// bit past eight, while the rate factor is not. Added going in and taken off coming out, so a
/// session's quantizers mean what they do at 8 bits whatever its depth.
#[cfg(feature = "gpl")]
fn qp_offset(bit_depth: u32) -> i32 {
    6 * (bit_depth as i32 - 8)
}

/// The rate an x264 session is opened at: the capture's, or 30 frames per second where it names
/// less than one.
#[cfg(feature = "gpl")]
fn x264_frame_rate(fps: f64) -> FrameRate {
    FrameRate::of(if fps < 1.0 { 30.0 } else { fps })
}

/// One long-lived libx264 session for a frame, holding the raw `x264_t` handle alongside a
/// mirror of its live parameters so the encoder can be retuned per frame instead of rebuilt.
///
/// Rebuilding an x264 encoder is expensive and forces a fresh IDR, so a session keeps its instance
/// across frames and only nudges CRF, bitrate, VBV, and frame rate live; the tracked `current_*`
/// fields are that mirror, letting a reconfigure skip the FFI call whenever nothing actually changed.
/// `is_i444` (4:4:4 vs 4:2:0) is baked into the encoder's color space at open, so a change to it is
/// one of the few things that forces a full rebuild; `is_cbr` records which rate-control mode was
/// chosen at open and gates which of the live reconfigures apply. The manual `Send` impl exists only
/// because a raw pointer is not `Send` by default and the handle must move onto the encode
/// thread; `Drop` closes it under the global open/close lock for the same reason that lock exists.
#[cfg(feature = "gpl")]
pub struct H264EncoderWrapper {
    encoder: *mut codec_sys::x264::x264_t,
    pub width: i32,
    pub height: i32,
    current_crf: i32,
    pub is_i444: bool,
    /// The bits per sample the session was opened at, baked in as the chroma format is.
    pub bit_depth: u32,
    is_cbr: bool,
    current_bitrate: i32,
    current_vbv: i32,
    current_fps: FrameRate,
    /// Open-time parameters retained so a frame-rate change can reopen the session: x264's live
    /// reconfigure cannot alter the frame rate, and CBR/VBV budgets are derived from it.
    threads: i32,
    min_qp: i32,
    max_qp: i32,
    /// The frames the decoder holds, so a lost one can be left out of the predictions and
    /// each frame can name what it predicts from.
    references: ReferenceWindow,
    /// The most frames the decoded picture buffer keeps, 0 for the level's
    /// (`video_reference_frames`), kept for a reopen.
    reference_frames: u32,
    last_reference: Reference,
    /// The stream's `frame_num`, widened from x264's sixteen values to 4096.
    frame_num: WideFrameNum,
    /// The floor the rate control's quantizer stands at now (`reconfigure_floor`), on the
    /// H.264 scale; 0 is x264's own.
    current_min_qp: i32,
    /// The quantizer the next frame is held at whatever the rate control (`hold_quantizer`).
    held_qp: Option<i32>,
    /// The mean quantizer the rate control last coded a frame's rows at, held frames aside.
    last_qp: Option<u32>,
    /// The live rate factor, which x264 reports a frame's mean quantizer against (`last_qp`).
    rf_constant: f64,
    /// The quantizer offset of each macroblock of the next frame, empty where it has none
    /// (`refine_blocks`).
    quant_offsets: Vec<f32>,
    /// The next frame's macroblocks that did not change (`constant_blocks`), as x264's
    /// `mb_info` flags; empty where the source cannot say.
    mb_info: Vec<u8>,
    /// The picture x264 still reads after its last encode (`held_input`).
    held_input: Option<*const u8>,
}

/// The planes of a picture handed to x264: 4:2:0 or 4:4:4 planes of either depth, each its
/// stride in bytes, or an 8-bit 4:2:0 picture as NV12.
#[cfg(feature = "gpl")]
enum Input<'a> {
    Planar {
        planes: [&'a [u8]; 3],
        strides: [i32; 3],
    },
    Nv12(&'a Nv12Planes),
}

#[cfg(feature = "gpl")]
unsafe impl Send for H264EncoderWrapper {}

#[cfg(feature = "gpl")]
impl Drop for H264EncoderWrapper {
    fn drop(&mut self) {
        if !self.encoder.is_null() {
            let _guard = X264_OPEN_CLOSE_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            unsafe { codec_sys::x264::x264_encoder_close(self.encoder) };
            self.encoder = ptr::null_mut();
        }
    }
}

#[cfg(feature = "gpl")]
impl H264EncoderWrapper {
    /// Open an x264 encoder tuned for real-time screen streaming, or `None` on failure.
    ///
    /// **Why this configuration.** These frames are captured live and must ship immediately, so the
    /// encoder is optimized for latency over compression ratio: the `ultrafast` preset keeps encode
    /// time under the frame budget, and `zerolatency` bars the frame reordering and lookahead
    /// buffering that would otherwise add pipeline delay. Everything below then bends x264 toward the
    /// pipeline's own keyframe and color model instead of its broadcast-oriented defaults.
    ///
    /// 1. **Preset/tune**: starts from the `ultrafast` preset with the `zerolatency` tune, then
    ///    overrides resolution, frame rate (floored to 30 fps when under 1), and thread count.
    /// 2. **Infinite GOP**: `i_keyint_max` is set to x264's infinite sentinel and adaptive scene-cut
    ///    is disabled (`i_scenecut_threshold = 0`), so the encoder never injects an unrequested IDR
    ///    on a scene change — keyframes are purely on-demand via the forced-IDR path, matching the
    ///    strict infinite-GOP model.
    /// 3. **Rate control**:
    ///    - **CBR** (`cbr_mode`): ABR targeting `bitrate_kbps` with a VBV cap pinned to the same
    ///      value (buffer `vbv_kbit`, precomputed by the caller from the frame-time multiplier
    ///      policy) and filler disabled. `max_qp` is the legibility floor (caps how ugly a
    ///      rate-starved frame gets) and `min_qp` the waste ceiling (stops over-spending on easy
    ///      content); both are clamped to 51, and the ceiling is 51 when unset because x264's
    ///      own default admits out-of-spec quantizers above it that exist only to force skips
    ///      when the VBV underflows, which leaves rows of the picture frozen on old content. A
    ///      budget the content cannot meet overshoots instead, as NVENC and libvpx do.
    ///    - **CRF** (default): constant-quality with `f_rf_constant = crf`. A positive
    ///      `vbv_kbit` caps it with a VBV of that buffer at a peak of `bitrate_kbps` (x264's
    ///      capped CRF): the rate factor still picks the quantizer and the VBV bounds the size
    ///      of a frame, a key frame above all, under CBR's ceiling of 51, so content the cap
    ///      starves overshoots it rather than freezes. `vbv_kbit == 0` leaves it uncapped.
    /// 4. **Color**: I444 at full range or I420 at limited range, a VUI declaring that range
    ///    with the BT.709 primaries, transfer, and matrix the sRGB source and the conversion
    ///    carry, and the matching `high444` / `baseline` profile.
    /// 5. **Coding tools**: CABAC and the 8x8 transform are disabled, matching the low-latency
    ///    baseline profile — CAVLC entropy coding with no 8x8 DCT — for minimal encode cost.
    ///    Motion search is the uneven multi-hexagon one over 64 pixels in place of `ultrafast`'s
    ///    diamond over 16, which misses a scroll or a moved window wherever its starting
    ///    prediction is poor (a slice's first row, the edge of a still region) and codes the
    ///    shifted pixels as new: at 2560x1600 it took a scrolling full-screen change from 5.0 to
    ///    3.5 Mbit/s at the same PSNR for 0.14 ms more of encode, and a quarter of the screen from
    ///    5.0 to 4.4 for 0.09 ms, the search running only on the macroblocks `mb_info` leaves.
    /// 6. **Output**: repeated headers (SPS/PPS before each keyframe) and Annex-B framing, with
    ///    x264's own logging silenced.
    /// 7. **References**: a decoded picture buffer of `REFERENCE_FRAMES` (`i_dpb_size`), or of
    ///    `reference_frames` where the caller caps it (`video_reference_frames`), declaring
    ///    the lowest level up to 5.2 that holds them (`h264_reference_level`: 5.0 at 1080p, where
    ///    4.2 holds four), so `invalidate_reference` can leave a frame a client lost out of the
    ///    predictions with earlier frames still there to predict from, for a report up to eight
    ///    frames late (133 ms at 60 fps). Motion search keeps its single reference.
    /// 8. **`frame_num`**: x264 counts it in sixteen values for that buffer, and a loss covering
    ///    the frame where it wraps costs a key frame (`ReferenceWindow`), so the stream carries it
    ///    a byte wider (`WideFrameNum`): 4096 values, one such frame in 68 s at 60 fps.
    ///
    /// The `x264_encoder_open` call is serialized under `X264_OPEN_CLOSE_LOCK` because it mutates
    /// libx264 global state.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        width: i32,
        height: i32,
        crf: i32,
        is_i444: bool,
        fps: f64,
        threads: i32,
        cbr_mode: bool,
        bitrate_kbps: i32,
        vbv_kbit: i32,
        min_qp: i32,
        max_qp: i32,
    ) -> Option<Self> {
        Self::open(
            width,
            height,
            crf,
            is_i444,
            8,
            fps,
            threads,
            cbr_mode,
            bitrate_kbps,
            vbv_kbit,
            min_qp,
            max_qp,
            0,
            None,
        )
    }

    /// `new` at `bit_depth` bits per sample: 10 opens a High 10 session, or High 4:4:4
    /// Predictive at 10 bits, which reads its planes as 16-bit samples; its decoded picture
    /// buffer keeps at most `reference_frames`, 0 for the level's (`video_reference_frames`).
    #[allow(clippy::too_many_arguments)]
    pub fn with_depth(
        width: i32,
        height: i32,
        crf: i32,
        is_i444: bool,
        bit_depth: u32,
        fps: f64,
        threads: i32,
        cbr_mode: bool,
        bitrate_kbps: i32,
        vbv_kbit: i32,
        min_qp: i32,
        max_qp: i32,
        reference_frames: u32,
    ) -> Option<Self> {
        Self::open(
            width,
            height,
            crf,
            is_i444,
            bit_depth,
            fps,
            threads,
            cbr_mode,
            bitrate_kbps,
            vbv_kbit,
            min_qp,
            max_qp,
            reference_frames,
            None,
        )
    }

    /// Whether the linked libx264 opens a 10-bit session, which is a property of how the
    /// library was built, asked once of a session too small to cost anything.
    pub fn ten_bit() -> bool {
        static TEN_BIT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *TEN_BIT.get_or_init(|| {
            Self::with_depth(64, 64, 25, false, 10, 30.0, 1, false, 0, 0, 0, 0, 0).is_some()
        })
    }

    /// `new`, with a constant-rate session's first frame given a budget of its own where `key`
    /// names one, `(budget_kbit, level_idc)`: the rate is that budget a frame and the VBV buffer
    /// that budget, full, until `restore_rate`, and the level is pinned to the one the session it
    /// replaces declared, which the raised rate would otherwise lift.
    #[allow(clippy::too_many_arguments)]
    fn open(
        width: i32,
        height: i32,
        crf: i32,
        is_i444: bool,
        bit_depth: u32,
        fps: f64,
        threads: i32,
        cbr_mode: bool,
        bitrate_kbps: i32,
        vbv_kbit: i32,
        min_qp: i32,
        max_qp: i32,
        reference_frames: u32,
        key: Option<(i32, i32)>,
    ) -> Option<Self> {
        unsafe {
            let mut param: codec_sys::x264::x264_param_t = std::mem::zeroed();
            let preset = CString::new("ultrafast").unwrap();
            let tune = CString::new("zerolatency").unwrap();

            if codec_sys::x264::x264_param_default_preset(
                &mut param,
                preset.as_ptr(),
                tune.as_ptr(),
            ) < 0
            {
                return None;
            }

            let frame_rate = x264_frame_rate(fps);
            param.i_width = width;
            param.i_height = height;
            param.i_fps_num = frame_rate.num;
            param.i_fps_den = frame_rate.den;
            param.i_keyint_max = codec_sys::x264::X264_KEYINT_MAX_INFINITE as i32;
            param.i_scenecut_threshold = 0;
            let bitrate_bps = if cbr_mode {
                bitrate_kbps.saturating_abs() as u64 * 1000
            } else {
                0
            };
            let dpb = h264_dpb_frames(
                h264_reference_level(
                    h264_level(width as u32, height as u32, frame_rate.ceil(), bitrate_bps),
                    width as u32,
                    height as u32,
                ),
                width as u32,
                height as u32,
            );
            let dpb = match reference_frames {
                0 => dpb,
                n => dpb.min(n),
            };
            param.i_dpb_size = dpb as i32;
            if cbr_mode {
                let bk = bitrate_kbps.saturating_abs();
                param.rc.i_rc_method = codec_sys::x264::X264_RC_ABR as i32;
                param.rc.i_bitrate = bk;
                param.rc.i_vbv_max_bitrate = bk;
                param.rc.i_vbv_buffer_size = vbv_kbit.max(1);
                if let Some((budget, level)) = key {
                    let rate = ((budget as f64 * frame_rate.fps()) as i32).max(bk);
                    param.rc.i_bitrate = rate;
                    param.rc.i_vbv_max_bitrate = rate;
                    param.rc.i_vbv_buffer_size = budget.max(1);
                    param.rc.f_vbv_buffer_init = 1.0;
                    param.i_level_idc = level;
                }
                param.rc.b_filler = 0;
                if min_qp > 0 {
                    param.rc.i_qp_min = min_qp.min(51) + qp_offset(bit_depth);
                }
                param.rc.i_qp_max =
                    if max_qp > 0 { max_qp.min(51) } else { 51 } + qp_offset(bit_depth);
            } else {
                param.rc.i_rc_method = codec_sys::x264::X264_RC_CRF as i32;
                param.rc.f_rf_constant = crf as f32;
                if vbv_kbit > 0 {
                    param.rc.i_vbv_max_bitrate = bitrate_kbps.saturating_abs();
                    param.rc.i_vbv_buffer_size = vbv_kbit;
                    param.rc.i_qp_max = 51 + qp_offset(bit_depth);
                }
            }
            let depth_flag = if bit_depth > 8 {
                codec_sys::x264::X264_CSP_HIGH_DEPTH
            } else {
                0
            };
            param.i_csp = (if is_i444 {
                codec_sys::x264::X264_CSP_I444
            } else {
                codec_sys::x264::X264_CSP_I420
            } | depth_flag) as i32;
            param.i_bitdepth = bit_depth as i32;
            param.vui.b_fullrange = if is_i444 { 1 } else { 0 };
            param.vui.i_colorprim = 1;
            param.vui.i_transfer = 1;
            param.vui.i_colmatrix = 1;

            let profile = CString::new(match (is_i444, bit_depth > 8) {
                (true, _) => "high444",
                (false, true) => "high10",
                (false, false) => "baseline",
            })
            .unwrap();
            codec_sys::x264::x264_param_apply_profile(&mut param, profile.as_ptr());
            // A patched x264 applies the per-block offsets as given (`patches/x264`); an
            // unpatched one weighs them against its adaptive quantization.
            #[cfg(x264_qp_offsets)]
            {
                param.rc.b_qp_offsets = 1;
            }
            param.analyse.b_mb_info = 1;
            param.analyse.i_me_method = codec_sys::x264::X264_ME_UMH as i32;
            param.analyse.i_me_range = 64;

            param.i_threads = threads;
            param.b_repeat_headers = 1;
            param.b_annexb = 1;
            param.i_log_level = codec_sys::x264::X264_LOG_NONE;

            let encoder = {
                let _guard = X264_OPEN_CLOSE_LOCK
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                codec_sys::x264::x264_encoder_open(&mut param)
            };
            if encoder.is_null() {
                None
            } else {
                codec_sys::x264::x264_encoder_parameters(encoder, &mut param);
                Some(Self {
                    encoder,
                    width,
                    height,
                    current_crf: crf,
                    is_i444,
                    bit_depth,
                    is_cbr: cbr_mode,
                    current_bitrate: bitrate_kbps.saturating_abs(),
                    current_vbv: vbv_kbit,
                    current_fps: frame_rate,
                    threads,
                    min_qp,
                    max_qp,
                    references: ReferenceWindow::new(dpb),
                    reference_frames,
                    last_reference: Reference::Untracked,
                    frame_num: WideFrameNum::default(),
                    current_min_qp: min_qp,
                    held_qp: None,
                    last_qp: None,
                    rf_constant: param.rc.f_rf_constant as f64,
                    quant_offsets: Vec::new(),
                    mb_info: Vec::new(),
                    held_input: None,
                })
            }
        }
    }

    /// Retune the constant-quality CRF on the running encoder, so a quality change costs a
    /// parameter push rather than tearing down and rebuilding the session (a rebuild would force an
    /// IDR and drop encoder state).
    ///
    /// It is a no-op in CBR mode, where rate is bitrate-controlled and CRF simply does not apply, and
    /// a no-op when the value is unchanged — the tracked `current_crf` is what makes that cheap
    /// early-out possible. Otherwise it reads the encoder's live parameters, overwrites
    /// `f_rf_constant`, and pushes the change via `x264_encoder_reconfig`, advancing the tracked CRF
    /// only once the reconfig has actually succeeded so the mirror never drifts from the encoder.
    pub fn reconfigure_crf(&mut self, new_crf: i32) {
        if self.is_cbr || self.current_crf == new_crf {
            return;
        }
        unsafe {
            let mut param: codec_sys::x264::x264_param_t = std::mem::zeroed();
            codec_sys::x264::x264_encoder_parameters(self.encoder, &mut param);
            param.rc.f_rf_constant = new_crf as f32;
            if codec_sys::x264::x264_encoder_reconfig(self.encoder, &mut param) == 0 {
                self.current_crf = new_crf;
                self.rf_constant = new_crf as f64;
            }
        }
    }

    /// Retune bitrate/VBV (CBR, or a capped CRF) and/or frame rate to match the live settings,
    /// structured to be called unconditionally every frame so the caller need not track what
    /// changed itself.
    ///
    /// Because `encode_cpu` fires it on every frame, it first computes the would-be values and bails
    /// before touching the encoder when neither the bitrate/VBV nor the frame rate differs from
    /// what is live — that self-gating keeps a per-frame call nearly free.
    ///
    /// A frame-rate change reopens the encoder rather than reconfiguring it: `x264_encoder_reconfig`
    /// does not apply `i_fps_*`, and the CBR/VBV per-frame budget is `bitrate / fps`, so a session
    /// left at its old rate ships roughly half the configured bitrate once fps halves. The reopen
    /// carries the new bitrate/VBV too, and a fresh session emits an IDR on its first frame; a failed
    /// reopen keeps the working session instead of nulling the handle. Capping a CRF session or
    /// lifting its cap reopens it too, since x264 retunes a VBV in place only while one is on. A
    /// bitrate/VBV-only change otherwise stays a live `x264_encoder_reconfig`, and the tracked
    /// mirror advances only on success so it cannot drift from the encoder's real state.
    pub fn reconfigure_rate(&mut self, bitrate_kbps: i32, vbv_kbit: i32, fps: f64) {
        let bk = bitrate_kbps.saturating_abs();
        let new_fps = x264_frame_rate(fps);
        let vbv_applies = self.is_cbr || vbv_kbit > 0;
        let rate_changed =
            vbv_applies && (self.current_bitrate != bk || self.current_vbv != vbv_kbit);
        let reopen = self.current_fps != new_fps
            || (!self.is_cbr && (self.current_vbv > 0) != (vbv_kbit > 0));
        if !rate_changed && !reopen {
            return;
        }
        if reopen {
            if let Some(fresh) = H264EncoderWrapper::with_depth(
                self.width,
                self.height,
                self.current_crf,
                self.is_i444,
                self.bit_depth,
                new_fps.fps(),
                self.threads,
                self.is_cbr,
                bk,
                vbv_kbit,
                self.min_qp,
                self.max_qp,
                self.reference_frames,
            ) {
                *self = fresh;
            }
            return;
        }
        unsafe {
            let mut param: codec_sys::x264::x264_param_t = std::mem::zeroed();
            codec_sys::x264::x264_encoder_parameters(self.encoder, &mut param);
            param.rc.i_bitrate = bk;
            if vbv_applies {
                param.rc.i_vbv_max_bitrate = bk;
                param.rc.i_vbv_buffer_size = vbv_kbit.max(1);
            }
            if codec_sys::x264::x264_encoder_reconfig(self.encoder, &mut param) == 0 {
                self.current_bitrate = bk;
                self.current_vbv = vbv_kbit;
            }
        }
    }

    /// The frame the last encoded frame predicted from.
    pub fn last_reference(&self) -> Reference {
        self.last_reference
    }

    /// The mean quantizer the rate control last coded a frame's rows at, held frames aside.
    pub fn last_qp(&self) -> Option<u32> {
        self.last_qp
    }

    /// Move the floor of a constant-rate session's quantizer live, `min_qp` on the H.264 scale
    /// and 0 for x264's own, through `x264_encoder_reconfig` with the bounds the carried patch
    /// lets it take. A floor at the paint-over quality while the screen moves holds the rate
    /// control from spending a generous budget on coding motion finer than the still picture
    /// it leaves (a 1440p scroll at 30 Mbit/s: 22.3 Mbit/s at 57 dB against 13.5 at 46 dB),
    /// lifted while a still screen is cleaned up, which then reaches what the rate control
    /// gives it; the blocks a moving frame refines code below the floor, as the patch keeps
    /// the caller's offsets. An x264 without the patches keeps the bounds it opened with.
    pub fn reconfigure_floor(&mut self, min_qp: i32) {
        if !self.is_cbr || min_qp == self.current_min_qp {
            return;
        }
        unsafe {
            let mut param: codec_sys::x264::x264_param_t = std::mem::zeroed();
            codec_sys::x264::x264_encoder_parameters(self.encoder, &mut param);
            param.rc.i_qp_min = if min_qp > 0 {
                min_qp.min(51) + qp_offset(self.bit_depth)
            } else {
                0
            };
            if codec_sys::x264::x264_encoder_reconfig(self.encoder, &mut param) == 0 {
                self.current_min_qp = min_qp;
            }
        }
    }

    /// Encode the next frame at quantizer `qp` whatever the rate control, leaving the session's
    /// own for the frame after: x264's per-picture forced quantizer.
    ///
    /// Under a constant rate x264's row-level VBV control moves a forced quantizer back toward
    /// the rate control's own for the frame, so a held key frame there, a fresh start for the
    /// decoder anyway, starts a fresh session (`open_for_held_key`) whose rate control plans that
    /// frame with `HELD_KEY_BUDGET_S` of the target, which also bounds its size, and takes the
    /// session's own rate back for the frame after.
    pub fn hold_quantizer(&mut self, qp: i32) {
        self.held_qp = Some(qp.clamp(0, 51));
    }

    /// Replace the session with one whose first frame, the held key frame about to be encoded,
    /// has the budget of `HELD_KEY_BUDGET_S` of the target; true where it opened, and
    /// `restore_rate` is owed after the frame.
    fn open_for_held_key(&mut self) -> bool {
        let level = unsafe {
            let mut param: codec_sys::x264::x264_param_t = std::mem::zeroed();
            codec_sys::x264::x264_encoder_parameters(self.encoder, &mut param);
            param.i_level_idc
        };
        let budget = (self.current_bitrate as f64 * super::HELD_KEY_BUDGET_S).round() as i32;
        let fresh = Self::open(
            self.width,
            self.height,
            self.current_crf,
            self.is_i444,
            self.bit_depth,
            self.current_fps.fps(),
            self.threads,
            self.is_cbr,
            self.current_bitrate,
            self.current_vbv,
            self.min_qp,
            self.max_qp,
            self.reference_frames,
            Some((budget, level)),
        );
        let Some(mut fresh) = fresh else { return false };
        fresh.held_qp = self.held_qp;
        *self = fresh;
        true
    }

    /// Code the macroblocks of the next frame that `plan` refines `offset` quantizer steps off
    /// the frame's own, and the rest at it: the ledger's refinement of blocks that held still
    /// while others move (`ledger`). The plan's blocks are the macroblocks of this session's
    /// picture.
    pub fn refine_blocks(&mut self, plan: &crate::ledger::BlockPlan, offset: f32) {
        let cols = (self.width as usize).div_ceil(16);
        let rows = (self.height as usize).div_ceil(16);
        self.quant_offsets.clear();
        self.quant_offsets.extend((0..rows).flat_map(|row| {
            (0..cols).map(move |col| if plan.refines(col, row) { offset } else { 0.0 })
        }));
    }

    /// Mark the macroblocks of the next frame that did not change since the frame before
    /// (`unchanged`, in raster order), which x264 codes as skips without analysing them where
    /// its reference holds them at the frame's quantizer or finer.
    pub fn constant_blocks(&mut self, unchanged: impl Iterator<Item = bool>) {
        self.mb_info.clear();
        self.mb_info.extend(unchanged.map(|c| {
            if c {
                codec_sys::x264::X264_MBINFO_CONSTANT as u8
            } else {
                0
            }
        }));
    }

    /// Put the session's own rate and VBV buffer back after a held key frame.
    fn restore_rate(&mut self) {
        unsafe {
            let mut param: codec_sys::x264::x264_param_t = std::mem::zeroed();
            codec_sys::x264::x264_encoder_parameters(self.encoder, &mut param);
            param.rc.i_bitrate = self.current_bitrate;
            param.rc.i_vbv_max_bitrate = self.current_bitrate;
            param.rc.i_vbv_buffer_size = self.current_vbv.max(1);
            codec_sys::x264::x264_encoder_reconfig(self.encoder, &mut param);
        }
    }

    /// Leave frame `frame_id` and every frame after it out of the predictions. False when x264
    /// refuses, and the caller codes a key frame instead.
    pub fn invalidate_reference(&mut self, frame_id: u16) -> bool {
        match self.references.invalidate(frame_id) {
            Invalidation::Forget(pts) => unsafe {
                codec_sys::x264::x264_encoder_invalidate_reference(self.encoder, pts as i64) == 0
            },
            Invalidation::KeyFrame | Invalidation::Ignored => true,
        }
    }

    /// Encode one YUV frame into H.264 and frame it for the wire, reporting whether the
    /// encoder actually emitted a bitstream this call.
    ///
    /// The boolean return is load-bearing: `x264_encoder_encode` can legitimately produce nothing on
    /// a given call, and the caller must forward a frame only when real bytes exist — never an empty
    /// or header-only packet. Framing is conditional because the transport needs the pipeline's small
    /// wire header to route the frame, while `omit_headers` consumers take the bare Annex-B
    /// elementary stream.
    ///
    /// 1. **Picture setup**: wraps the borrowed Y/U/V planes and their strides in an
    ///    `x264_picture_t` with the encoder's CSP, stamps the presentation timestamp with the
    ///    session's count of encoded frames (the one an invalidation names), and requests an IDR
    ///    when `force_idr` is set or no reference is left to predict from (otherwise
    ///    `X264_TYPE_AUTO`).
    /// 2. **Encode**: calls `x264_encoder_encode`; a non-positive returned size means no frame was
    ///    emitted this call, so the function returns `false` without writing output.
    /// 3. **Framing**: `output_buf` is cleared and refilled. Unless `omit_headers` is set, the wire
    ///    header is prepended with a frame kind read from the *actual* output picture type rather
    ///    than from `force_idr`, because the encoder may not honor a keyframe request and the
    ///    client keys its decode-recovery on the kind it truly received. With `omit_headers` the
    ///    output is bare Annex-B.
    /// 4. **Payload**: every NAL payload is appended to `output_buf` after the optional header,
    ///    its `frame_num` widened (`WideFrameNum`), so the bytes past the wire header are always a
    ///    contiguous Annex-B access unit.
    #[allow(clippy::too_many_arguments)]
    pub fn encode_with_headers(
        &mut self,
        y: &[u8],
        u: &[u8],
        v: &[u8],
        y_stride: i32,
        u_stride: i32,
        v_stride: i32,
        frame_id: u16,
        force_idr: bool,
        omit_headers: bool,
        output_buf: &mut Vec<u8>,
    ) -> bool {
        self.encode_input(
            Input::Planar {
                planes: [y, u, v],
                strides: [y_stride, u_stride, v_stride],
            },
            frame_id,
            force_idr,
            omit_headers,
            output_buf,
        )
    }

    /// `encode_with_headers` for an 8-bit 4:2:0 session's NV12 picture, which x264 encodes in
    /// place where it is laid out for it: x264 reads it through the next encode, which takes it
    /// as the previous input, so it is left unchanged until that encode returns or until
    /// `release_input` (`held_input`).
    pub fn encode_nv12_with_headers(
        &mut self,
        planes: &Nv12Planes,
        frame_id: u16,
        force_idr: bool,
        omit_headers: bool,
        output_buf: &mut Vec<u8>,
    ) -> bool {
        self.encode_input(
            Input::Nv12(planes),
            frame_id,
            force_idr,
            omit_headers,
            output_buf,
        )
    }

    /// The picture x264 still reads, as the previous input, after its last encode returned: the
    /// [`Nv12Planes::id`] of the last one it was handed in place, `None` where the last encode
    /// copied its picture.
    pub fn held_input(&self) -> Option<*const u8> {
        self.held_input
    }

    /// Stop x264 reading the picture it holds (`held_input`), so its buffers can change.
    pub fn release_input(&mut self) {
        // Only a patched x264 holds an input (`x264_in_place`); an unpatched one copied it.
        #[cfg(x264_in_place)]
        unsafe {
            codec_sys::x264::x264_encoder_release_input(self.encoder)
        };
        self.held_input = None;
    }

    fn encode_input(
        &mut self,
        input: Input<'_>,
        frame_id: u16,
        force_idr: bool,
        omit_headers: bool,
        output_buf: &mut Vec<u8>,
    ) -> bool {
        let key_budget =
            self.is_cbr && force_idr && self.held_qp.is_some() && self.open_for_held_key();
        let coded = self.encode_picture(input, frame_id, force_idr, omit_headers, output_buf);
        if key_budget {
            self.restore_rate();
        }
        coded
    }

    /// `encode_input` past the choice of session.
    fn encode_picture(
        &mut self,
        input: Input<'_>,
        frame_id: u16,
        force_idr: bool,
        omit_headers: bool,
        output_buf: &mut Vec<u8>,
    ) -> bool {
        unsafe {
            let mut pic_in: codec_sys::x264::x264_picture_t = std::mem::zeroed();
            codec_sys::x264::x264_picture_init(&mut pic_in);

            self.held_input = None;
            match input {
                Input::Planar { planes, strides } => {
                    pic_in.img.i_csp = (if self.is_i444 {
                        codec_sys::x264::X264_CSP_I444
                    } else {
                        codec_sys::x264::X264_CSP_I420
                    } | if self.bit_depth > 8 {
                        codec_sys::x264::X264_CSP_HIGH_DEPTH
                    } else {
                        0
                    }) as i32;
                    pic_in.img.i_plane = 3;
                    for p in 0..3 {
                        pic_in.img.plane[p] = planes[p].as_ptr() as *mut u8;
                        pic_in.img.i_stride[p] = strides[p];
                    }
                }
                Input::Nv12(nv12) => {
                    // A picture is laid out in place only where the linked x264 named a layout.
                    #[cfg(x264_in_place)]
                    let in_place = if nv12.in_place {
                        codec_sys::x264::X264_CSP_IN_PLACE
                    } else {
                        0
                    };
                    #[cfg(not(x264_in_place))]
                    let in_place = 0;
                    pic_in.img.i_csp = (codec_sys::x264::X264_CSP_NV12 | in_place) as i32;
                    pic_in.img.i_plane = 2;
                    let (y, uv) = nv12.planes();
                    pic_in.img.plane[0] = y.as_ptr() as *mut u8;
                    pic_in.img.plane[1] = uv.as_ptr() as *mut u8;
                    pic_in.img.i_stride[0] = nv12.stride[0] as i32;
                    pic_in.img.i_stride[1] = nv12.stride[1] as i32;
                    self.held_input = nv12.in_place.then(|| nv12.id());
                }
            }
            pic_in.i_pts = self.references.next_pts() as i64;
            pic_in.i_type = if force_idr || !self.references.has_reference() {
                codec_sys::x264::X264_TYPE_IDR
            } else {
                codec_sys::x264::X264_TYPE_AUTO
            } as i32;
            let held = self.held_qp.take();
            let offset = qp_offset(self.bit_depth);
            pic_in.i_qpplus1 =
                held.map_or(codec_sys::x264::X264_QP_AUTO as i32, |q| q + offset + 1);
            // x264 reads the offsets as the frame is submitted, within this call.
            if !self.quant_offsets.is_empty() {
                pic_in.prop.quant_offsets = self.quant_offsets.as_mut_ptr();
            }
            let blocks = (self.width as usize).div_ceil(16) * (self.height as usize).div_ceil(16);
            if self.mb_info.len() == blocks {
                pic_in.prop.mb_info = self.mb_info.as_mut_ptr();
            }

            let mut pic_out: codec_sys::x264::x264_picture_t = std::mem::zeroed();
            let mut nals: *mut codec_sys::x264::x264_nal_t = ptr::null_mut();
            let mut i_nals: i32 = 0;

            let frame_size = codec_sys::x264::x264_encoder_encode(
                self.encoder,
                &mut nals,
                &mut i_nals,
                &mut pic_in,
                &mut pic_out,
            );
            self.quant_offsets.clear();

            if frame_size > 0 {
                // x264 says the quantizer it planned the frame at, while its row-level control
                // codes the rows that would overrun the buffer coarser, up to 51: on a still screen
                // the plan falls to the paint-over quantizer and below with the rows still coarse.
                // `f_crf_avg` is the rate factor plus the rows' mean less the pre-VBV quantizer,
                // which the plan stands in for (never under it, so the mean never reads finer).
                if held.is_none() {
                    self.last_qp = (pic_out.i_qpplus1 > 0).then(|| {
                        let planned = (pic_out.i_qpplus1 - 1) as f64;
                        let coded = planned + pic_out.prop.f_crf_avg - self.rf_constant;
                        (coded.round() as i32 - offset).clamp(0, 51) as u32
                    });
                }
                let frame_type = if pic_out.i_type == codec_sys::x264::X264_TYPE_IDR as i32 {
                    FRAME_KEY
                } else if pic_out.i_type == codec_sys::x264::X264_TYPE_I as i32 {
                    FRAME_INTRA
                } else {
                    FRAME_DELTA
                };
                self.last_reference = self.references.record(frame_id, frame_type != FRAME_DELTA);
                output_buf.clear();
                output_buf.reserve(super::codec::VIDEO_HEADER_LEN + frame_size as usize);
                if !omit_headers {
                    push_video_header(
                        output_buf,
                        Codec::H264,
                        frame_type,
                        frame_id,
                        0,
                        self.width as u16,
                        self.height as u16,
                        self.last_reference,
                    );
                }

                let nal_slice = std::slice::from_raw_parts(nals, i_nals as usize);
                let mut widened = true;
                for nal in nal_slice {
                    let payload = std::slice::from_raw_parts(nal.p_payload, nal.i_payload as usize);
                    widened &= self.frame_num.push(payload, output_buf);
                }
                if !widened {
                    // Its slices no longer match the set they went out under, so the next frame
                    // is a key frame.
                    self.references.reset();
                }
                if frame_type == FRAME_KEY {
                    let stream = &output_buf[if omit_headers {
                        0
                    } else {
                        super::codec::VIDEO_HEADER_LEN
                    }..];
                    self.references
                        .set_frame_num_range(h264_frame_num_range(stream).unwrap_or(0));
                }
                return true;
            }
        }
        false
    }
}

/// Everything one region of the frame must remember between frames: its reused buffers, its own
/// live encoder, and the motion / paint-over / damage bookkeeping that drives its send decision.
///
/// A region is a JPEG stripe, the whole frame of a software H.264 session, or the frame of a
/// full-frame session (`pipeline::decide_hw_fullframe`). JPEG stripes encode in parallel and an
/// unchanged one is skipped on its own, which only works if each carries its *own* cross-frame
/// history. So one instance lives per region for the whole session and nothing in it is rebuilt or
/// recomputed from scratch each frame:
/// - **Reused buffers**: `nv12` holds an 8-bit 4:2:0 x264 frame's pictures, `y_buf` / `u_buf` /
///   `v_buf` and `yuv16` another frame's YUV planes, and `packet_buf` the encoded output, grown
///   in place rather than reallocated per frame.
/// - **Encoder**: `h264_encoder` is the software H.264 session — libx264 in a `gpl` build,
///   OpenH264 otherwise — reused until its geometry (or, for x264, chroma format) changes.
/// - **Cleanup / recovery**: `no_motion_frame_count` counts consecutive static frames,
///   `paint_over_sent` records that the region was cleaned up since it last changed,
///   `h264_burst_frames_remaining` tracks a post-cleanup or recovery streaming burst (held at the
///   paint-over quantizer where `burst_held`), and `dirty_run`, `change_mass`, `unclean_frames`,
///   and `motion` (the share of recent frames in motion) are what `pipeline::cleanup_due` weighs
///   to pick the cleanup's moment and kind; `clean_quality` keeps a constant-quality session at
///   the paint-over quality from a cleanup until the region changes again; `rc_bytes` (the bytes of
///   the region's last frame under its rate control), `rc_debt` (the bytes a cleanup's rate control
///   has spent past its target and not yet repaid), `idle_frames` (a constant-rate cleanup's run
///   of small frames short of the paint-over quality, of empty ones for x264), `same_frames` (of
///   frames the size of the one before), `finest` and `finest_frames` (the finest quality index
///   its rate control has coded at and the frames since), and `over_frames` (its run of frames over
///   `pipeline::OVERSHOOT_BUDGETS` and no finer than `finest`) tell a cleanup that runs through
///   the rate control when it has converged, stalled, or been pinned at its coarsest quantizer,
///   `fine_frames` counts its run of frames at the paint-over quantizer or finer, `measured`,
///   `measuring`, `level_checks`, and
///   `measure_in` are the last measurement of the picture it is refining, whether the frame before
///   was measured, the measurements in a row that found it level, and the frames to the next
///   (`pipeline::plateau`), `settled` records
///   that such a cleanup ran its course under a rate control it cannot hold a quantizer
///   under, so no other starts before the region moves, and `sweep` (the next band's start
///   and the last band's size, as shares of the picture) carries the band sweep a
///   full-frame refresh falls back to (`pipeline::decide_hw_fullframe`), `sweep_frames` its
///   bands so far, `sweep_coarser` the steps they are coded coarser than the refresh,
///   `sweep_held` whether the last frame was one of them, and `sweep_cost` the bytes the whole
///   picture would come to at the refresh's quantizer, by its bands so far (`sweep_first`, by its
///   first); `cleanup_debt` is the
///   bytes a cleanup's frames, the rate control's and the bands, came to past the cleanup's
///   budget a frame, which it waits out (`cleanup_paced`: the last frame was one of them).
/// - **Content-hash damage** (only for sources without external damage, i.e. X11): `last_hash` is
///   the previous frame's content hash, `consecutive_changes` counts changed frames toward the
///   damage-block threshold, and `in_damage_block` / `damage_block_frames_remaining` drive the
///   sustained-motion damage block managed by `content_dirty`.
#[derive(Default)]
pub struct StripeState {
    pub no_motion_frame_count: u32,
    pub paint_over_sent: bool,
    pub dirty_run: u32,
    pub change_mass: f32,
    /// Whether the region moved (`Damage::is_motion`) since its last cleanup.
    pub moved: bool,
    /// Whether a scheduled key frame fell due that waits for the region's next motion
    /// (`StripeState::scheduled_key`).
    pub key_owed: bool,
    pub unclean_frames: u32,
    /// Whether the last cleanup was a key frame (`pipeline::lost_frame` repeats its kind).
    pub cleanup_keyed: bool,
    pub motion: f32,
    pub burst_held: bool,
    pub clean_quality: bool,
    pub rc_bytes: usize,
    pub rc_debt: f64,
    pub idle_frames: u32,
    pub over_frames: u32,
    pub same_frames: u32,
    pub finest: Option<u32>,
    pub finest_frames: u32,
    pub settled: bool,
    pub fine_frames: u32,
    pub measured: Option<f32>,
    pub measuring: bool,
    pub level_checks: u32,
    pub measure_in: u32,
    pub sweep: Option<(f64, f64)>,
    pub sweep_frames: u32,
    pub sweep_coarser: u32,
    pub sweep_cost: f64,
    pub sweep_first: f64,
    pub sweep_held: bool,
    pub cleanup_debt: f64,
    pub cleanup_paced: bool,
    #[cfg(feature = "gpl")]
    pub h264_encoder: Option<H264EncoderWrapper>,
    #[cfg(not(feature = "gpl"))]
    pub h264_encoder: Option<crate::encoders::oh264::Openh264Encoder>,
    pub h264_burst_frames_remaining: i32,
    #[cfg(feature = "gpl")]
    pub y_buf: Vec<u8>,
    #[cfg(feature = "gpl")]
    pub u_buf: Vec<u8>,
    #[cfg(feature = "gpl")]
    pub v_buf: Vec<u8>,
    /// The planes of a 10-bit frame, as 16-bit samples.
    #[cfg(feature = "gpl")]
    pub yuv16: [Vec<u16>; 3],
    /// The two pictures an 8-bit 4:2:0 x264 session converts its frames into in turn, x264
    /// reading the one before through the next encode (`H264EncoderWrapper::held_input`).
    #[cfg(feature = "gpl")]
    pub nv12: Vec<Nv12Planes>,
    pub packet_buf: Vec<u8>,
    pub last_hash: u64,
    pub consecutive_changes: u32,
    pub in_damage_block: bool,
    pub damage_block_frames_remaining: i32,
    /// Whether the last `content_dirty` hashed the region and found it changed: a damage block's
    /// other frames read dirty without a hash.
    pub hash_changed: bool,
    /// The region's blocks, which held still and which are clean (`ledger`), for refining the
    /// ones that held still while others move.
    pub ledger: BlockLedger,
    /// How many of them a frame refines (`ledger::RefinePace`).
    pub refine: RefinePace,
}

/// The bits per sample the software H.264 session codes `settings` at: 10 where they ask for it
/// and the build's libx264 opens such a session, else 8.
pub fn h264_bit_depth(settings: &CaptureSettings) -> u32 {
    #[cfg(feature = "gpl")]
    if settings.codec == Codec::H264
        && settings.video_bit_depth >= 10
        && H264EncoderWrapper::ten_bit()
    {
        return 10;
    }
    let _ = settings;
    8
}

/// Fast, non-cryptographic 64-bit content hash used only for in-memory change detection.
///
/// Uses xxh3: a SIMD-friendly hash that processes 64-byte blocks with parallel lanes,
/// delivering near memory-bandwidth throughput. The value is never persisted or sent on the
/// wire, so only the property that identical bytes hash identically matters. A collision
/// between two distinct stripes is ~2^-64, and the next real content change or a requested
/// keyframe repaints any missed update anyway.
fn fast_hash(bytes: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64_with_seed(bytes, 0)
}

impl StripeState {
    /// Stand in for the compositor damage that X11 capture does not provide: hash this stripe
    /// to decide whether it changed since last frame, and once it is clearly in motion, stop
    /// re-hashing it every frame by committing to a sustained-motion "damage block".
    ///
    /// The hash is not free, and a region that changes every frame would otherwise be re-hashed
    /// forever while always reporting dirty anyway. So after `threshold` consecutive changes the
    /// stripe enters a damage block that just reports dirty for `duration` frames and re-hashes only
    /// on its last two frames, to decide whether to extend the block or let it lapse — trading a
    /// little extra sending for far fewer hashes on exactly the regions that need them least:
    ///
    /// 1. **Inside a damage block**: the stripe is treated as dirty without re-hashing, and the
    ///    block's remaining-frame counter is decremented. The stripe is hashed on the block's last
    ///    two frames: if they differ the region is still moving and the block is renewed for
    ///    another `duration` frames, otherwise the block exits and the change counter resets, so
    ///    motion that stopped inside a block is read as stopped when that block ends, not a block
    ///    later. This keeps a continuously-moving region streaming for `duration` frames per
    ///    re-check rather than hashing every frame.
    /// 2. **Outside a block**: the stripe is hashed and compared to the previous frame. A change
    ///    increments `consecutive_changes`, and reaching `threshold` consecutive changes opens a new
    ///    damage block; an unchanged frame resets the counter to zero.
    ///
    /// Returns `true` whenever the stripe is considered dirty (always true while inside a block).
    pub fn content_dirty(&mut self, bytes: &[u8], threshold: u32, duration: i32) -> bool {
        self.dirty_by(|| fast_hash(bytes), threshold, duration)
    }

    /// `content_dirty` for a stripe hashed elsewhere (on the GPU, where the frame was uploaded):
    /// the same state, read off `hash`.
    pub fn hash_dirty(&mut self, hash: u64, threshold: u32, duration: i32) -> bool {
        self.dirty_by(|| hash, threshold, duration)
    }

    fn dirty_by(&mut self, fast_hash: impl FnOnce() -> u64, threshold: u32, duration: i32) -> bool {
        self.hash_changed = false;
        if self.in_damage_block {
            self.damage_block_frames_remaining -= 1;
            if self.damage_block_frames_remaining == 1 {
                self.last_hash = fast_hash();
            } else if self.damage_block_frames_remaining <= 0 {
                let h = fast_hash();
                if h != self.last_hash {
                    self.hash_changed = true;
                    self.damage_block_frames_remaining = duration;
                } else {
                    self.in_damage_block = false;
                    self.consecutive_changes = 0;
                }
                self.last_hash = h;
            }
            return true;
        }
        let h = fast_hash();
        let changed = h != self.last_hash;
        self.last_hash = h;
        self.hash_changed = changed;
        if changed {
            self.consecutive_changes += 1;
            if self.consecutive_changes >= threshold {
                self.in_damage_block = true;
                self.damage_block_frames_remaining = duration;
            }
        } else {
            self.consecutive_changes = 0;
        }
        changed
    }
}

/// One encoded stripe: the compressed bytes plus geometry and identity metadata.
///
/// The consumer can place and attribute the stripe even when the payload has no header. In
/// `omit_headers` mode the per-stripe wire header is stripped from the bytes; the struct fields
/// carry that information out-of-band.
///
/// # Fields
///
/// * `data` - Compressed payload (JPEG, or the video codec's bitstream). `Arc`-shared so every
///   delivery-layer consumer can retain the frame without copying the bytes.
/// * `codec` - The codec of the payload.
/// * `stripe_y_start` - Y pixel coordinate of the stripe's top edge within the frame.
/// * `stripe_height` - Height of the stripe in pixels.
/// * `frame_id` - Frame sequence number this stripe belongs to.
/// * `reference` - The frame this stripe predicts from.
pub struct EncodedStripe {
    pub data: Arc<Vec<u8>>,
    pub codec: Codec,
    pub stripe_y_start: i32,
    pub stripe_height: i32,
    pub frame_id: i32,
    pub timing: FrameTiming,
    pub reference: Reference,
}

impl EncodedStripe {
    /// The encoded bytes, shared with the encoder's buffer.
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    /// The codec tag the wire carries: the codec id.
    pub fn data_type(&self) -> i32 {
        self.codec.data_type()
    }

    /// The frame this one predicts from: its id, -1 for a frame that decodes on its own,
    /// -2 where the encoder does not track its references.
    pub fn reference_frame_id(&self) -> i32 {
        self.reference.frame_id()
    }
}

/// When a frame was captured and when its encode began and ended, as CLOCK_MONOTONIC
/// nanoseconds, so a consumer can attribute a frame's age to the host rather than the
/// network or the decoder. An encoder leaves them zero; the capture that ran it stamps
/// every stripe of the frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameTiming {
    pub capture_ns: i64,
    pub encode_start_ns: i64,
    pub encode_end_ns: i64,
}

impl FrameTiming {
    /// Stamps the stripes of one frame, captured at `capture_ns` and encoded from
    /// `encode_start_ns` until now.
    pub fn stamp(stripes: &mut [EncodedStripe], capture_ns: i64, encode_start_ns: i64) {
        let timing = FrameTiming {
            capture_ns,
            encode_start_ns,
            encode_end_ns: crate::wayland::host::now_ns(),
        };
        for stripe in stripes {
            stripe.timing = timing;
        }
    }
}

/// No JPEG stripe is shorter than this many rows.
const MIN_STRIPE_HEIGHT: i32 = 64;
/// The frames the JPEG stripes' cleanups of one still screen are spread over (`encode_cpu`).
const CLEANUP_STAGGER_FRAMES: usize = 4;

/// Whether the software H.264 session holds its cleanup at the quantizer asked for: libx264
/// does under either rate control, and OpenH264 moves its quantizer only at a constant quality
/// (`Openh264Encoder::update_qp`), so a session that holds none is cleaned up through its rate
/// control (`encode_cpu`), never by a key frame, which that would starve.
pub fn stripes_hold_quantizer(settings: &CaptureSettings) -> bool {
    cfg!(feature = "gpl") || !settings.video_cbr_mode
}

/// Whether a frame that holds still codes the software H.264 session at the paint-over quality
/// instead of its own (`encode_cpu`): at a constant quality, where its cleanup falls due, its
/// burst is held, or it keeps the quality it was cleaned up at. A constant rate holds none there.
pub fn stripes_held_still(stripes: &[StripeState], settings: &CaptureSettings) -> bool {
    !settings.video_cbr_mode
        && settings.use_paint_over_quality
        && settings.video_paintover_crf < settings.video_crf
        && stripes.iter().any(|st| {
            st.clean_quality
                || (st.burst_held && st.h264_burst_frames_remaining > 0)
                || crate::pipeline::cleanup_pending(
                    st,
                    settings.paint_over_trigger(),
                    settings.paint_over_trigger(),
                    true,
                    true,
                )
        })
}

/// One frame of a region's `ledger`, the region at rows `y` to `y + height`: the rectangles of
/// `changed` that reach it, in its own coordinates, read with whether it changed (`dirty`) and
/// whether its hash saw it change (`observed`, `BlockLedger::frame_reported`).
fn ledger_frame(
    ledger: &mut BlockLedger,
    changed: &[Rectangle<i32, Physical>],
    (y, height): (i32, i32),
    dirty: bool,
    observed: bool,
) {
    let local: Vec<Rectangle<i32, Physical>> = changed
        .iter()
        .filter(|r| r.loc.y < y + height && r.loc.y + r.size.h > y)
        .map(|r| Rectangle::new((r.loc.x, r.loc.y - y).into(), r.size))
        .collect();
    ledger.frame_reported(&local, dirty, observed);
}

/// The CPU path's per-frame entry point: decide per region whether it needs sending, and encode
/// those as JPEG stripes across the rayon pool or as one H.264 frame through the build's software
/// encoder (libx264 or OpenH264).
///
/// JPEG is cut into parallel stripes so its CPU cost spreads over the cores and an unchanged
/// stripe is skipped on its own; H.264 is one region covering the frame, its encoder kept across
/// frames for inter-prediction. Persistent `StripeState` keeps encoders, buffers, and the
/// motion/paint-over history the decision needs alive across frames. The per-region decision
/// mirrors `decide_hw_fullframe`'s policy for the full-frame sessions; it is kept as separate code
/// because this path also derives its own damage and cleans H.264 up through its software rate
/// control.
///
/// # Arguments
///
/// * `stripes` - Persistent per-region state vector (resized as needed, encoder state preserved).
/// * `raw_pixels` - Packed BGRA/RGBA pixel buffer (`width * height * 4` bytes).
/// * `width` - Frame width in pixels.
/// * `height` - Frame height in pixels.
/// * `damage_rects` - Where the source says the frame changed: what marks a region dirty without
///   `hash_damage` (Wayland), and with it where the change lies within each region the hash
///   finds dirty, for its block ledger (empty where the source cannot say).
/// * `settings` - Capture settings (quality, mode, rate control, etc.).
/// * `frame_counter` - Current frame number (wrapping `u16`).
/// * `use_gpu` - `true` when the source is RGBA (GLES readback); `false` for BGRA (X11 host).
/// * `hash_damage` - `true` for X11 content-hash change detection; `false` when damage rects
///   are provided.
/// * `requested_idr` - Force a keyframe (client join / reset).
///
/// # Returns
///
/// Vec of [`EncodedStripe`] — empty when nothing changed.
///
/// 1. **Regions**: JPEG takes the core count of stripes, held to `MAX_STRIPES` because the client
///    decodes one picture per stripe, with no stripe thinner than 64 rows (`stripe_count`); H.264
///    takes one region covering the frame. The persistent `stripes` vector is resized to match,
///    preserving per-region state across frames.
/// 2. **Idle fast path**: a frame on which no region can emit anything (no damage / clean
///    hashes, no cleanup due, no burst, no recovery IDR, not streaming) only advances the
///    per-region cleanup bookkeeping inline and returns without dispatching the stripe
///    fan-out, so a static capture never wakes the rayon pool.
/// 3. **Dirty map**: with external compositor damage (`hash_damage == false`) each `damage_rects`
///    rectangle marks every region whose row range it overlaps. With `hash_damage == true` (X11,
///    which has no compositor damage) per-region content hashing drives dirtiness instead, in
///    streaming H.264 too, since the cleanup reads a still region from its content.
/// 4. **Per-region cleanup** (`pipeline::cleanup_due`): a region that changed is cleaned up once
///    it has held still for `paint_over_trigger`, or has kept changing only a little for four
///    times that, at the paint-over JPEG quality or, for H.264, at the paint-over quality: under
///    CBR through libx264's rate control, the frame sent until it codes at the paint-over
///    quantizer or finer in a small frame (`pipeline::convergence`), or its frames have been slice
///    headers alone for `pipeline::EMPTY_S`, with no key frame, and where the encoder reports no
///    quantizer (OpenH264) as a refresh and burst at its rate control's quality; under CRF as the
///    session's rate factor until it changes again, as main's paint-over was, a refresh and
///    then a burst, with no key frame either, as `pipeline::decide_constant_quality` keeps it:
///    the frames held at the rate factor refine the picture past what a key frame at it codes
///    (x264 at CRF 24 and paint-over 16 on a scrolled 1440p page, 51.3 dB after the refresh,
///    48.3 after the 2.7 MB key frame that followed it). The JPEG stripes whose
///    cleanups fall due on one frame are spread over `CLEANUP_STAGGER_FRAMES` frames, so a screen
///    going still costs no one frame the whole screen's cleanup. A region is otherwise sent when
///    it is dirty, while its burst runs, or when streaming mode is on, at the base quality; a
///    newly dirty frame cancels its burst. A constant-rate x264 session is floored at the
///    paint-over quantizer while it moves and the floor lifted for its cleanup
///    (`H264EncoderWrapper::reconfigure_floor`), unless the caller names a floor of its own
///    (`video_min_qp`): the rate control otherwise spends a generous budget coding motion finer
///    than the still picture it leaves (a 1440p scroll at 30 Mbit/s, 22.3 Mbit/s at 57 dB
///    against 13.5 at 46 dB), where the still screen reaches what the rate control gives it
///    either way, and the blocks a moving frame refines code below the floor.
/// 5. **Recovery IDR** (`requested_idr`): forces a send even when static so a reconnecting
///    client can resume. A scheduled one (`pipeline::periodic_idr_due`) waits for the region to
///    change (`StripeState::scheduled_key`): a hashed change has no known extent, so any counts as
///    motion. For H.264 it forces an IDR at the session's own quality and arms the burst (held at
///    the paint-over quantizer at a constant quality, as in `pipeline::decide_hw_fullframe`); for
///    JPEG, where every stripe is already intra, it resends a painted-over stripe at the
///    paint-over quality already on screen so a joining viewer does not see a downgrade.
/// 6. **Encoding**:
///    - **JPEG**: source byte order is RGBA on the GPU readback path and BGRA on
///      X11; each worker thread reuses its thread-local TurboJPEG compressor. Header-less output
///      hands the compressed buffer straight through; otherwise a 6-byte stripe header (`0x03` tag,
///      a reserved byte, frame number, y-start) is prepended so the transport can forward the
///      buffer without re-framing.
///    - **H.264**: the encoder is reused unless the width, height, or (x264) chroma format or
///      depth changed, in which case it is rebuilt and an IDR forced; otherwise CRF and rate are
///      reconfigured live. With libx264, ARGB is converted to YUV here (a conversion failure skips
///      the frame rather than encoding garbage) and the video header is emitted; an 8-bit 4:2:0
///      frame is converted to NV12 laid out as x264's own frames, which x264 encodes where it
///      lies instead of copying it, into the session's two pictures in turn, since x264 reads the
///      one before through the next encode; OpenH264 converts
///      and frames inside `encode_host_argb` with the same header layout, and encodes a 4:4:4
///      request 4:2:0 (said once per process). The live CBR budget is recomputed here from the
///      bitrate/fps (`rate_control`) so it rescales with live changes.
/// 7. **Dispatch**: H.264 runs inline with `h264_fullframe_threads` encode threads, its color
///    conversion split into as many bands (x264: a whole 2560x1600 frame took 0.98 ms on one
///    thread and 0.62 on four; OpenH264 adds four slices and a four-band conversion of its own),
///    or none where the capture hands over the planes ([`encode_cpu_from`]). The slice threads keep the in-frame encode latency inside the frame budget at high
///    resolutions; the cap is four because `zerolatency` makes x264 slice-threaded and more than
///    four slices trips decode glitches in some Chromium builds, and the minus-one leaves
///    headroom for the capture thread. JPEG stripes instead run across the rayon pool, or one
///    after another without `cpu_encode_threading` (`stripes_at_once`).
#[allow(clippy::too_many_arguments)]
pub fn encode_cpu(
    stripes: &mut Vec<StripeState>,
    raw_pixels: &[u8],
    width: i32,
    height: i32,
    damage_rects: &[Rectangle<i32, Physical>],
    settings: &CaptureSettings,
    frame_counter: u16,
    use_gpu: bool,
    hash_damage: bool,
    requested_idr: bool,
) -> Vec<EncodedStripe> {
    encode_cpu_from(
        stripes,
        raw_pixels,
        None,
        width,
        height,
        damage_rects,
        true,
        settings,
        frame_counter,
        use_gpu,
        hash_damage,
        requested_idr,
    )
}

/// A frame's 8-bit 4:2:0 picture as NV12, a luma plane and an interleaved chroma one, laid out
/// as an x264 session encodes it in place (`x264_param_input_layout`): each plane `stride` bytes
/// a row within a buffer padded and aligned as x264's own frames are, which x264 reads without
/// copying it and writes the padding of. Tightly packed where the build has no x264, whose
/// OpenH264 session takes no such picture.
pub struct Nv12Planes {
    buf: Vec<u8>,
    start: [usize; 2],
    pub stride: [usize; 2],
    pub width: usize,
    pub height: usize,
    #[cfg(feature = "gpl")]
    in_place: bool,
}

impl Nv12Planes {
    /// Black planes for a `width` x `height` picture, both even.
    pub fn new(width: usize, height: usize) -> Self {
        let layout = x264_input_layout(width, height);
        #[cfg(feature = "gpl")]
        let in_place = layout.is_some();
        let layout = layout.unwrap_or(InputLayout {
            stride: [width, width],
            offset: [0, 0],
            size: [width * height, width * height / 2],
        });
        const ALIGN: usize = 64;
        let luma = layout.size[0].next_multiple_of(ALIGN);
        let mut buf = vec![128u8; ALIGN + luma + layout.size[1]];
        let base = buf.as_ptr().align_offset(ALIGN);
        buf[base..base + layout.size[0]].fill(16);
        Self {
            buf,
            start: [base + layout.offset[0], base + luma + layout.offset[1]],
            stride: layout.stride,
            width,
            height,
            #[cfg(feature = "gpl")]
            in_place,
        }
    }

    /// The luma and chroma planes from their first pixel, `stride` bytes a row for the picture's
    /// rows.
    pub fn planes(&self) -> (&[u8], &[u8]) {
        let (y, uv) = self.extents();
        (&self.buf[y], &self.buf[uv])
    }

    /// [`Self::planes`], to write.
    pub fn planes_mut(&mut self) -> (&mut [u8], &mut [u8]) {
        let (y, uv) = self.extents();
        let (head, tail) = self.buf.split_at_mut(uv.start);
        (&mut head[y], &mut tail[..uv.len()])
    }

    /// The address of the first luma pixel, which names the picture to the session holding it
    /// (`H264EncoderWrapper::held_input`).
    pub fn id(&self) -> *const u8 {
        self.buf[self.start[0]..].as_ptr()
    }

    fn extents(&self) -> (std::ops::Range<usize>, std::ops::Range<usize>) {
        let y = self.start[0]..self.start[0] + self.stride[0] * self.height;
        let uv = self.start[1]..self.start[1] + self.stride[1] * self.height / 2;
        (y, uv)
    }
}

/// Where each plane of an NV12 picture lies in its buffer: `stride` bytes a row, the first pixel
/// `offset` bytes in, the buffer `size` bytes.
struct InputLayout {
    stride: [usize; 2],
    offset: [usize; 2],
    size: [usize; 2],
}

/// The planes an 8-bit 4:2:0 x264 session of `width` x `height` encodes in place, which the
/// library lays out for the CPU it runs on; `None` where the linked x264 is not patched to.
#[cfg(all(feature = "gpl", x264_in_place))]
fn x264_input_layout(width: usize, height: usize) -> Option<InputLayout> {
    unsafe {
        let mut param: codec_sys::x264::x264_param_t = std::mem::zeroed();
        codec_sys::x264::x264_param_default(&mut param);
        param.i_width = width as i32;
        param.i_height = height as i32;
        param.i_csp = codec_sys::x264::X264_CSP_I420 as i32;
        param.i_bitdepth = 8;
        let (mut stride, mut offset, mut size) = ([0i32; 2], [0i32; 2], [0i64; 2]);
        if codec_sys::x264::x264_param_input_layout(
            &param,
            stride.as_mut_ptr(),
            offset.as_mut_ptr(),
            size.as_mut_ptr(),
        ) < 0
        {
            return None;
        }
        Some(InputLayout {
            stride: stride.map(|v| v as usize),
            offset: offset.map(|v| v as usize),
            size: size.map(|v| v as usize),
        })
    }
}

#[cfg(not(all(feature = "gpl", x264_in_place)))]
fn x264_input_layout(_width: usize, _height: usize) -> Option<InputLayout> {
    None
}

/// [`encode_cpu`] with the frame's picture already converted (`planes`): an 8-bit 4:2:0 x264
/// session encodes it in place and leaves `raw_pixels` unread, which a capture converting straight
/// from the screen's memory never wrote. x264 reads the picture through the next encode
/// ([`held_input`]).
///
/// `current` says whether `damage_rects` holds every change of this frame against the one
/// before, as a compositor's damage or this frame's own hash does. Only then does x264 code the
/// blocks they leave out as skips without analysis (`H264EncoderWrapper::constant_blocks`):
/// rectangles read from the frame before's hash (a Turbo frame's) leave out a block that held
/// still there and changes in this frame, which a skip would show as it was.
#[allow(clippy::too_many_arguments)]
pub fn encode_cpu_from(
    stripes: &mut Vec<StripeState>,
    raw_pixels: &[u8],
    planes: Option<&Nv12Planes>,
    width: i32,
    height: i32,
    damage_rects: &[Rectangle<i32, Physical>],
    current: bool,
    settings: &CaptureSettings,
    frame_counter: u16,
    use_gpu: bool,
    hash_damage: bool,
    requested_idr: bool,
) -> Vec<EncodedStripe> {
    #[cfg(not(feature = "gpl"))]
    let _ = planes;
    let codec = settings.codec;
    let scheduled = codec.is_video() && crate::pipeline::periodic_idr_due(settings, frame_counter);
    let n_processing_stripes = stripe_count(height, codec);

    if stripes.len() != n_processing_stripes {
        stripes.resize_with(n_processing_stripes, StripeState::default);
    }

    let stripe_geometries = compute_stripe_geometries(height as usize, n_processing_stripes, codec);

    // Idle fast path: a static frame must still advance every stripe's paint-over countdown,
    // but nothing else — so when no stripe can emit anything this frame, do that bookkeeping
    // inline and return before the rayon fan-out. Waking the whole worker pool 60x/s for
    // no-op stripes is the dominant idle cost (tens of percent of a core), dwarfing the real
    // per-frame work. "Static" is known up front for damage-authoritative sources (Wayland:
    // empty damage list); hash-damage sources (X11) instead take a sequential early-exit
    // hash scan, probing the most-recently-dirty stripe first so live content bails out
    // after a single stripe hash. A clean scan performs exactly the state transitions
    // `content_dirty` would (hash unchanged, change streak reset), so the damage-block
    // machinery observes no difference.
    let coded_quality = |st: &StripeState| -> Option<u32> {
        cfg_if::cfg_if! {
            if #[cfg(feature = "gpl")] {
                st.h264_encoder.as_ref().and_then(H264EncoderWrapper::last_qp)
            } else {
                let _ = st;
                None
            }
        }
    };
    let holds = stripes_hold_quantizer(settings);
    // A constant-rate stripe is cleaned up through its rate control: x264 refines a still screen
    // within its buffer, and OpenH264, which moves its quantizer only by a rebuild, past its frame
    // skip, a skipped frame telling nothing of the picture (`rc_bytes` 0).
    let converges = codec.is_video() && settings.video_cbr_mode;
    let motion_floor = if settings.video_min_qp > 0 || !converges {
        settings.video_min_qp
    } else {
        settings.video_paintover_crf.max(0)
    };
    let paint_over_armed = |st: &StripeState| {
        if !codec.is_video() {
            return settings.use_paint_over_quality
                && settings.paint_over_jpeg_quality > settings.jpeg_quality;
        }
        crate::pipeline::paint_over_improves(
            settings,
            crate::pipeline::EncoderQuality {
                last: coded_quality(st),
                bytes: None,
                holds,
                reopens: false,
                keys: true,
                band: None,
                measures: false,
                psnr: None,
                refit: None,
                follows_cap: false,
                finer: 0,
                ends_on_key: false,
            },
        )
    };
    let trigger_frames = settings.paint_over_trigger();
    // A stripe's change is read from its hash, of no known extent, so a caret reads as motion:
    // its cleanup waits the trigger, through the rate control or not.
    let start_frames = trigger_frames;
    let idle_candidate = damage_rects.is_empty()
        && !requested_idr
        && !(codec.is_video() && settings.video_streaming_mode);
    if idle_candidate {
        let no_pending_send = |st: &StripeState| {
            (!codec.is_video() || st.h264_burst_frames_remaining <= 0)
                && !crate::pipeline::cleanup_pending(
                    st,
                    trigger_frames,
                    start_frames,
                    paint_over_armed(st),
                    false,
                )
        };
        let quiescent = if !hash_damage {
            stripes.iter().all(no_pending_send)
        } else {
            let width_bytes = width as usize * 4;
            let hint = stripes
                .iter()
                .enumerate()
                .min_by_key(|(_, st)| st.no_motion_frame_count)
                .map(|(i, _)| i)
                .unwrap_or(0);
            let clean = |i: usize| {
                let st = &stripes[i];
                if !no_pending_send(st) || st.in_damage_block {
                    return false;
                }
                let (y, h) = stripe_geometries[i];
                let bytes = &raw_pixels[y * width_bytes..(y + h) * width_bytes];
                fast_hash(bytes) == st.last_hash
            };
            clean(hint) && (0..stripes.len()).filter(|&i| i != hint).all(clean)
        };
        if quiescent {
            for st in stripes.iter_mut() {
                let armed = paint_over_armed(st);
                crate::pipeline::cleanup_due(
                    st,
                    trigger_frames,
                    start_frames,
                    armed,
                    true,
                    false,
                    Damage::None,
                );
                st.consecutive_changes = 0;
                st.key_owed |= scheduled;
                st.ledger.frame([]);
            }
            return Vec::new();
        }
    }
    let mut stripe_is_dirty = vec![false; n_processing_stripes];
    if !damage_rects.is_empty() {
        for rect in damage_rects {
            let r_y_start = rect.loc.y.max(0) as usize;
            let r_y_end = (rect.loc.y + rect.size.h).min(height) as usize;
            if r_y_start < r_y_end {
                for (i, &(s_y, s_h)) in stripe_geometries.iter().enumerate() {
                    let s_end = s_y + s_h;
                    if r_y_start < s_end && r_y_end > s_y {
                        stripe_is_dirty[i] = true;
                    }
                }
            }
        }
    }

    let width_usize = width as usize;
    let video = codec.is_video();
    let video_crf = settings.video_crf;
    let video_po_crf = settings.video_paintover_crf;
    let video_burst = settings.video_paintover_burst_frames;
    let video_fullcolor = settings.video_fullcolor;
    #[cfg(feature = "gpl")]
    let bit_depth = h264_bit_depth(settings);
    let video_streaming = settings.video_streaming_mode;
    let jpeg_q = settings.jpeg_quality;
    let paint_q = settings.paint_over_jpeg_quality;
    let target_fps = settings.target_fps;
    let omit_headers = settings.omit_stripe_headers;
    let damage_block_threshold = settings.damage_block_threshold;
    let damage_block_duration = settings.damage_block_duration as i32;
    let video_cbr = settings.video_cbr_mode;
    // OpenH264 sizes its own buffer, so only x264 reads the VBV.
    #[cfg_attr(not(feature = "gpl"), allow(unused_variables))]
    let (video_bitrate, video_vbv) = rate_control(settings);
    #[cfg(feature = "gpl")]
    let cleanup_vbv = cleanup_vbv_kbit(settings, video_bitrate).max(video_vbv);
    // x264 runs the H.264 thread policy, as OpenH264 does internally.
    #[cfg(feature = "gpl")]
    let h264_threads = crate::encoders::session::h264_fullframe_threads(settings);
    // A frame of x264's slices, one a thread, that refines nothing.
    #[cfg(feature = "gpl")]
    let empty_bytes = crate::encoders::codec::VIDEO_HEADER_LEN
        + crate::pipeline::EMPTY_SLICE_BYTES * h264_threads as usize;
    #[cfg(not(feature = "gpl"))]
    let empty_bytes = 0;
    #[cfg(feature = "gpl")]
    let csc_bands = h264_threads.max(1) as usize;
    if video && video_fullcolor && !crate::encoders::software_fullcolor(Codec::H264) {
        static FULLCOLOR_LOGGED: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
        if !FULLCOLOR_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            eprintln!(
                "[software] 4:4:4 full-color requested; OpenH264 is 4:2:0-only, encoding 4:2:0."
            );
        }
    }

    // A still screen reaches every JPEG stripe's cleanup on the same frame; spread them over
    // `CLEANUP_STAGGER_FRAMES` frames so no one frame carries the whole screen's. An H.264 session
    // refining through its rate control takes no other cleanup until that one ends.
    let per_frame = stripes.len().div_ceil(CLEANUP_STAGGER_FRAMES).max(1);
    let refining = stripes
        .iter()
        .filter(|st| converges && st.h264_burst_frames_remaining > 0)
        .count();
    let mut granted = 0;
    let cleanup_allowed: Vec<bool> = stripes
        .iter()
        .map(|st| {
            if !crate::pipeline::cleanup_pending(
                st,
                trigger_frames,
                start_frames,
                paint_over_armed(st),
                false,
            ) {
                return true;
            }
            granted += 1;
            granted <= per_frame && refining + granted <= stripes.len()
        })
        .collect();

    let stripe_body = |(i, stripe_state): (usize, &mut StripeState)| -> Option<EncodedStripe> {
        if i >= stripe_geometries.len() {
            return None;
        }
        let (y_start, actual_height) = stripe_geometries[i];
        let start_idx = y_start * width_usize * 4;
        let end_idx = start_idx + (actual_height * width_usize * 4);
        let stripe_bytes = &raw_pixels[start_idx..end_idx];

        let is_dirty = if !hash_damage {
            stripe_is_dirty[i]
        } else {
            stripe_state.content_dirty(stripe_bytes, damage_block_threshold, damage_block_duration)
        };
        stripe_state.ledger.fit(width, actual_height as i32);
        let observed = if hash_damage {
            stripe_state.hash_changed
        } else {
            is_dirty
        };
        ledger_frame(
            &mut stripe_state.ledger,
            damage_rects,
            (y_start as i32, actual_height as i32),
            is_dirty,
            observed,
        );
        let armed = paint_over_armed(stripe_state);
        let cleanup = crate::pipeline::cleanup_due(
            stripe_state,
            trigger_frames,
            start_frames,
            armed,
            cleanup_allowed[i],
            false,
            if is_dirty {
                Damage::Unknown
            } else {
                Damage::None
            },
        );
        let force_idr_all = requested_idr || stripe_state.scheduled_key(scheduled, is_dirty);
        let mut send_this_stripe = is_dirty || cleanup != Cleanup::None || force_idr_all;
        let mut quality_or_crf = if !video { jpeg_q } else { video_crf };
        let mut force_idr = video && (force_idr_all || cleanup == Cleanup::Key);
        if force_idr {
            stripe_state.key_owed = false;
        }
        // A key frame asked for codes a clean stripe again at its rate control's quality: it is due
        // its cleanup again, as after a loss, a trigger period on (`pipeline::decide_hw_fullframe`).
        if requested_idr && video && cleanup == Cleanup::None {
            stripe_state.lost_frame();
        }
        let mut hold = None;
        let quality = crate::pipeline::EncoderQuality {
            last: coded_quality(stripe_state),
            bytes: None,
            holds,
            reopens: false,
            keys: true,
            band: None,
            measures: false,
            psnr: None,
            refit: None,
            follows_cap: false,
            finer: 0,
            ends_on_key: false,
        };
        let refresh_crf = crate::pipeline::held_refresh_quality(settings, quality) as i32;
        if cleanup != Cleanup::None {
            if !video {
                quality_or_crf = paint_q;
            } else if cleanup == Cleanup::Key {
                hold = Some(video_po_crf);
            } else if holds && !converges {
                hold = Some(refresh_crf);
            }
        } else if force_idr_all && !is_dirty && !video && armed && stripe_state.paint_over_sent {
            quality_or_crf = paint_q;
        }
        if converges
            && stripe_state.h264_burst_frames_remaining > 0
            && !is_dirty
            && cleanup == Cleanup::None
        {
            let budget = crate::pipeline::frame_budget(video_bitrate, target_fps);
            let bytes = (stripe_state.rc_bytes > 0).then_some(stripe_state.rc_bytes);
            stripe_state.idle_frames = if bytes.is_some_and(|b| b <= empty_bytes) {
                stripe_state.idle_frames.saturating_add(1)
            } else {
                0
            };
            // A run of a trigger period: after a key frame coded coarser, x264 codes the frames that
            // follow at a fine quantizer while skipping most of the picture, so one small one says
            // nothing of the picture.
            stripe_state.fine_frames = if crate::pipeline::convergence(
                quality.last,
                bytes,
                settings.video_paintover_crf.max(0) as u32,
                budget,
            ) == crate::pipeline::Convergence::Converged
            {
                stripe_state.fine_frames.saturating_add(1)
            } else {
                0
            };
            if stripe_state.fine_frames >= trigger_frames {
                stripe_state.ledger.cleaned();
            }
            if stripe_state.fine_frames >= trigger_frames
                || stripe_state.idle_frames as f64 >= crate::pipeline::EMPTY_S * target_fps
            {
                stripe_state.h264_burst_frames_remaining = 0;
            }
        }
        if is_dirty {
            stripe_state.h264_burst_frames_remaining = 0;
        } else if video
            && converges
            && (cleanup != Cleanup::None || (force_idr && video_burst > 0 && armed))
        {
            // Neither encoder has a refresh after its rate control: it refines for as long as it
            // gains, x264 at 0.5 Mbit/s at 8 fps 1.7 dB a second still at 10 s, and OpenH264 at
            // 1080p first waits out seconds of frame skip after the frame that changed the page.
            stripe_state.h264_burst_frames_remaining = crate::pipeline::refine_frames(settings);
            stripe_state.burst_held = false;
            stripe_state.idle_frames = 0;
            stripe_state.fine_frames = 0;
        } else if video && (force_idr || cleanup != Cleanup::None) && video_burst > 0 {
            stripe_state.h264_burst_frames_remaining = video_burst;
            stripe_state.burst_held =
                holds && (cleanup != Cleanup::None || (armed && !settings.video_cbr_mode));
        } else if video && stripe_state.h264_burst_frames_remaining > 0 {
            stripe_state.h264_burst_frames_remaining -= 1;
            send_this_stripe = true;
            if stripe_state.burst_held && armed {
                hold = Some(refresh_crf);
            }
        }
        // A constant-quality stripe is cleaned up through its session quality, as main's
        // paint-over was (`pipeline::decide_constant_quality`): x264's rate factor or
        // OpenH264's rebuild at the held index, where a key frame takes the library's intra
        // offset. It stays there until the stripe changes again; a requested key frame is
        // coded at the session's own quality.
        if video && !video_cbr {
            if is_dirty || !armed {
                stripe_state.clean_quality = false;
            }
            if cleanup != Cleanup::None {
                stripe_state.clean_quality = true;
            }
            if stripe_state.clean_quality && hold.is_none() && !force_idr_all {
                hold = Some(video_po_crf);
            }
        }
        if video && video_streaming {
            send_this_stripe = true;
        }

        if send_this_stripe {
            if !video {
                let pixel_format = if use_gpu {
                    turbojpeg::PixelFormat::RGBA
                } else {
                    turbojpeg::PixelFormat::BGRA
                };
                let img = turbojpeg::Image {
                    pixels: stripe_bytes,
                    width: width_usize,
                    pitch: width_usize * 4,
                    height: actual_height,
                    format: pixel_format,
                };
                JPEG_COMPRESSOR.with(|cell| -> Option<EncodedStripe> {
                    let mut slot = cell.borrow_mut();
                    if slot.is_none() {
                        *slot = Some(turbojpeg::Compressor::new().ok()?);
                    }
                    let compressor = slot.as_mut().unwrap();
                    compressor.set_quality(quality_or_crf).ok()?;
                    let jpeg = compressor.compress_to_vec(img).ok()?;
                    let data = if omit_headers {
                        jpeg
                    } else {
                        stripe_state.packet_buf.clear();
                        push_jpeg_header(
                            &mut stripe_state.packet_buf,
                            frame_counter,
                            y_start as u16,
                        );
                        stripe_state.packet_buf.extend_from_slice(&jpeg);
                        std::mem::take(&mut stripe_state.packet_buf)
                    };
                    Some(EncodedStripe {
                        data: Arc::new(data),
                        codec: Codec::Jpeg,
                        stripe_y_start: y_start as i32,
                        stripe_height: actual_height as i32,
                        frame_id: frame_counter as i32,
                        timing: FrameTiming::default(),
                        reference: Reference::Untracked,
                    })
                })
            } else {
                cfg_if::cfg_if! {
                    if #[cfg(feature = "gpl")] {
                let needs_reinit = if let Some(ref enc) = stripe_state.h264_encoder {
                    enc.width != width_usize as i32
                        || enc.height != actual_height as i32
                        || enc.is_i444 != video_fullcolor
                        || enc.bit_depth != bit_depth
                } else {
                    true
                };

                // A constant rate holds the frame at its quantizer (`hold_quantizer`); a
                // constant quality codes it at that rate factor.
                let x264_crf = if video_cbr { quality_or_crf } else { hold.unwrap_or(quality_or_crf) };
                if needs_reinit {
                    stripe_state.h264_encoder = H264EncoderWrapper::with_depth(
                        width_usize as i32,
                        actual_height as i32,
                        x264_crf,
                        video_fullcolor,
                        bit_depth,
                        target_fps,
                        h264_threads,
                        video_cbr,
                        video_bitrate,
                        video_vbv,
                        settings.video_min_qp,
                        settings.video_max_qp,
                        settings.video_reference_frames,
                    );
                    force_idr = true;
                } else if let Some(ref mut enc) = stripe_state.h264_encoder {
                    enc.reconfigure_crf(x264_crf);
                    let cleaning = converges && stripe_state.h264_burst_frames_remaining > 0;
                    let vbv = if cleaning { cleanup_vbv } else { video_vbv };
                    enc.reconfigure_rate(video_bitrate, vbv, target_fps);
                    enc.reconfigure_floor(if cleaning { settings.video_min_qp } else { motion_floor });
                }

                // A stripe moving at the session's quality refines the blocks of it that held
                // still and are not clean, at the paint-over quality: a quantizer offset from
                // the rate control's last, or from the session's rate factor.
                let refines = video
                    && armed
                    && is_dirty
                    && cleanup == Cleanup::None
                    && hold.is_none()
                    && !force_idr;
                let plan = refines
                    .then(|| {
                        let blocks = stripe_state.ledger.blocks();
                        let most = stripe_state.refine.most(blocks, video_cbr);
                        stripe_state.ledger.plan(trigger_frames, most, usize::MAX)
                    })
                    .flatten();
                if let Some(ref mut enc) = stripe_state.h264_encoder {
                    if let Some(q) = hold.filter(|_| video_cbr) {
                        enc.hold_quantizer(q);
                    }
                    let refinement = plan.map(|plan| {
                        let refinement = crate::ledger::Refinement {
                            plan,
                            target: video_po_crf - crate::ledger::REFINE_FINER as i32,
                            reach: u32::MAX,
                            from: if video_cbr {
                                enc.last_qp().map_or(video_crf, |q| q as i32)
                            } else {
                                video_crf
                            },
                        };
                        enc.refine_blocks(&refinement.plan, refinement.offset() as f32);
                        refinement
                    });
                    if hash_damage || !current {
                        enc.constant_blocks(std::iter::empty());
                    } else {
                        enc.constant_blocks(stripe_state.ledger.unchanged());
                    }
                    let y_size = width_usize * actual_height;
                    let uv_size = if video_fullcolor { y_size } else { y_size / 4 };
                    let y_stride = width_usize as i32;
                    let uv_stride =
                        (if video_fullcolor { width_usize } else { width_usize / 2 }) as i32;
                    let wide = bit_depth > 8;
                    let nv12 = !wide && !video_fullcolor;
                    let planes = planes.filter(|p| {
                        nv12 && y_start == 0 && p.width == width_usize && p.height == actual_height
                    });
                    // x264 reads the picture it encoded last through this encode, so the frame
                    // is converted into the session's other one.
                    let own = if nv12 && planes.is_none() {
                        let held = enc.held_input();
                        let buffers = &mut stripe_state.nv12;
                        buffers.retain(|p| p.width == width_usize && p.height == actual_height);
                        while buffers.len() < 2 {
                            buffers.push(Nv12Planes::new(width_usize, actual_height));
                        }
                        Some(usize::from(held == Some(buffers[0].id())))
                    } else {
                        None
                    };
                    let conversion_result = if planes.is_some() {
                        Ok(())
                    } else if let Some(i) = own {
                        let target = &mut stripe_state.nv12[i];
                        let strides = (target.stride[0], target.stride[1]);
                        let (y, uv) = target.planes_mut();
                        convert_to_nv12_mt(
                            stripe_bytes,
                            (width_usize * 4) as u32,
                            width_usize,
                            actual_height,
                            use_gpu,
                            y,
                            uv,
                            strides,
                            csc_bands,
                        )
                    } else if wide {
                        let [y16, u16, v16] = &mut stripe_state.yuv16;
                        y16.resize(y_size, 0);
                        u16.resize(uv_size, 0);
                        v16.resize(uv_size, 0);
                        convert_to_yuv10_mt(
                            stripe_bytes,
                            width_usize * 4,
                            width_usize,
                            actual_height,
                            use_gpu,
                            video_fullcolor,
                            video_fullcolor,
                            y16,
                            u16,
                            v16,
                            (y_stride as usize, uv_stride as usize),
                            csc_bands,
                        );
                        Ok(())
                    } else {
                        if stripe_state.y_buf.len() != y_size {
                            stripe_state.y_buf.resize(y_size, 0);
                        }
                        if stripe_state.u_buf.len() != uv_size {
                            stripe_state.u_buf.resize(uv_size, 0);
                        }
                        if stripe_state.v_buf.len() != uv_size {
                            stripe_state.v_buf.resize(uv_size, 0);
                        }
                        convert_to_yuv_mt(
                            stripe_bytes,
                            (width_usize * 4) as u32,
                            width_usize,
                            actual_height,
                            use_gpu,
                            video_fullcolor,
                            video_fullcolor,
                            false,
                            &mut stripe_state.y_buf,
                            &mut stripe_state.u_buf,
                            &mut stripe_state.v_buf,
                            (y_stride as usize, uv_stride as usize),
                            csc_bands,
                        )
                    };

                    if let Err(e) = conversion_result {
                        eprintln!(
                            "[software] YUV conversion failed for {}x{} stripe: {:?}; skipping",
                            width_usize, actual_height, e
                        );
                        return None;
                    }

                    let bytes = |plane: &[u16]| unsafe {
                        std::slice::from_raw_parts(plane.as_ptr().cast::<u8>(), plane.len() * 2)
                    };
                    let coded = if let Some(p) = planes.or(own.map(|i| &stripe_state.nv12[i])) {
                        enc.encode_nv12_with_headers(
                            p,
                            frame_counter,
                            force_idr,
                            omit_headers,
                            &mut stripe_state.packet_buf,
                        )
                    } else {
                        let (y, u, v, scale) = if wide {
                            let [y16, u16, v16] = &stripe_state.yuv16;
                            (bytes(y16), bytes(u16), bytes(v16), 2)
                        } else {
                            (
                                stripe_state.y_buf.as_slice(),
                                stripe_state.u_buf.as_slice(),
                                stripe_state.v_buf.as_slice(),
                                1,
                            )
                        };
                        enc.encode_with_headers(
                            y,
                            u,
                            v,
                            y_stride * scale,
                            uv_stride * scale,
                            uv_stride * scale,
                            frame_counter,
                            force_idr,
                            omit_headers,
                            &mut stripe_state.packet_buf,
                        )
                    };
                    if coded {
                        if hold.is_none() || !video_cbr {
                            stripe_state.rc_bytes = stripe_state.packet_buf.len();
                        }
                        if let Some(refinement) = &refinement {
                            let coded = if video_cbr {
                                enc.last_qp().map_or(i32::MAX, |q| q as i32)
                            } else {
                                video_crf
                            };
                            stripe_state.ledger.refinement_coded(refinement, coded);
                        }
                        if video_cbr
                            && hold.is_none()
                            && !force_idr
                            && let Some(coded) = enc.last_qp().map(|q| q as i32)
                        {
                            let budget = crate::pipeline::frame_budget(video_bitrate, target_fps);
                            let blocks = stripe_state.ledger.blocks();
                            stripe_state.refine.coded(
                                refinement.is_some(),
                                coded,
                                stripe_state.packet_buf.len(),
                                budget,
                                blocks,
                            );
                        }
                        if hold.is_some_and(|q| q <= video_po_crf) {
                            stripe_state.ledger.cleaned();
                        } else if force_idr {
                            stripe_state.ledger.coarsened();
                        }
                        Some(EncodedStripe {
                            data: Arc::new(std::mem::take(&mut stripe_state.packet_buf)),
                            codec: Codec::H264,
                            stripe_y_start: y_start as i32,
                            stripe_height: actual_height as i32,
                            frame_id: frame_counter as i32,
                            timing: FrameTiming::default(),
                            reference: enc.last_reference(),
                        })
                    } else {
                        None
                    }
                } else {
                    None
                }
                    } else {
                use crate::encoders::oh264::Openh264Encoder;
                let needs_reinit = stripe_state.h264_encoder.as_ref().is_none_or(|enc| {
                    enc.width() != width_usize || enc.height() != actual_height
                });
                if needs_reinit {
                    stripe_state.h264_encoder = Openh264Encoder::with_size(
                        settings,
                        width_usize,
                        actual_height,
                        quality_or_crf,
                        video_bitrate,
                    );
                    if stripe_state.h264_encoder.is_none() {
                        // Once per process: a geometry OpenH264 refuses (wider than
                        // 3840, say) would otherwise log on every stripe of every frame.
                        static INIT_FAILED_LOGGED: std::sync::atomic::AtomicBool =
                            std::sync::atomic::AtomicBool::new(false);
                        if !INIT_FAILED_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                            eprintln!(
                                "[software] OpenH264 init failed for a {}x{} frame; no software H.264 for it",
                                width_usize, actual_height
                            );
                        }
                    }
                    force_idr = true;
                } else if let Some(ref mut enc) = stripe_state.h264_encoder {
                    // OpenH264 moves its quantizer only by a rebuild, which opens on a key
                    // frame, so a held quantizer is that key frame, and the stripe's key-frame
                    // cleanup with it; a still stripe keeps the quantizer it was cleaned at,
                    // since moving it back would rebuild a picture nothing changed in.
                    if let Some(q) = hold {
                        if enc.update_qp(q.max(0) as u32) {
                            stripe_state.change_mass = 0.0;
                        }
                    } else if is_dirty {
                        enc.update_qp(quality_or_crf.max(0) as u32);
                    }
                    enc.reconfigure_rate(video_bitrate, target_fps);
                }

                let coded = stripe_state.h264_encoder.as_mut()?.encode_host_argb(
                    stripe_bytes,
                    width_usize * 4,
                    frame_counter as u64,
                    force_idr,
                    use_gpu,
                );
                stripe_state.rc_bytes = coded.as_ref().map_or(0, Vec::len);
                let enc = stripe_state.h264_encoder.as_mut()?;
                match coded {
                    Ok(data) if !data.is_empty() => Some(EncodedStripe {
                        data: Arc::new(data),
                        codec: Codec::H264,
                        stripe_y_start: y_start as i32,
                        stripe_height: actual_height as i32,
                        frame_id: frame_counter as i32,
                        timing: FrameTiming::default(),
                        reference: enc.last_reference(),
                    }),
                    Ok(_) => None,
                    Err(e) => {
                        eprintln!("[software] OpenH264 encode failed: {e}");
                        None
                    }
                }
                    }
                }
            }
        } else {
            None
        }
    };
    if !stripes_at_once(settings, n_processing_stripes) {
        stripes
            .iter_mut()
            .enumerate()
            .filter_map(&stripe_body)
            .collect()
    } else {
        stripes
            .par_iter_mut()
            .enumerate()
            .filter_map(&stripe_body)
            .collect()
    }
}

/// Whether a frame's `n` JPEG stripes are coded at once across the rayon pool, or one after
/// another on the encode thread: one region, or a session without `cpu_encode_threading`, takes
/// no other.
fn stripes_at_once(settings: &CaptureSettings, n: usize) -> bool {
    n > 1 && settings.cpu_encode_threading
}

/// Leave frame `frame_id` and every frame after it out of the software H.264 session's
/// predictions: x264 through its reference window, OpenH264 from a long-term reference every
/// consumer holds. False when the encoder cannot, and the caller codes a key frame instead.
pub fn invalidate_reference(stripes: &mut [StripeState], frame_id: u16) -> bool {
    let mut forgotten = true;
    for enc in stripes.iter_mut().filter_map(|s| s.h264_encoder.as_mut()) {
        forgotten &= enc.invalidate_reference(frame_id);
    }
    forgotten
}

/// The picture the software H.264 session still reads after its last encode returned
/// (`H264EncoderWrapper::held_input`), which its owner leaves unchanged: `None` for any other
/// session.
pub fn held_input(stripes: &[StripeState]) -> Option<*const u8> {
    #[cfg(feature = "gpl")]
    return stripes
        .iter()
        .find_map(|s| s.h264_encoder.as_ref()?.held_input());
    #[cfg(not(feature = "gpl"))]
    {
        let _ = stripes;
        None
    }
}

/// Stop the software H.264 session reading the picture it holds ([`held_input`]).
pub fn release_input(stripes: &mut [StripeState]) {
    #[cfg(feature = "gpl")]
    for enc in stripes.iter_mut().filter_map(|s| s.h264_encoder.as_mut()) {
        enc.release_input();
    }
    #[cfg(not(feature = "gpl"))]
    let _ = stripes;
}

/// Every consumer holds frame `frame_id`, or where not `held` was sent it: OpenH264 confirms the
/// long-term reference it marked there. x264 keeps none.
pub fn acknowledge_reference(stripes: &mut [StripeState], frame_id: u16, held: bool) {
    #[cfg(not(feature = "gpl"))]
    for enc in stripes.iter_mut().filter_map(|s| s.h264_encoder.as_mut()) {
        enc.acknowledge_reference(frame_id, held);
    }
    #[cfg(feature = "gpl")]
    let _ = (stripes, frame_id, held);
}

/// Most stripes a JPEG frame is cut into, whatever the encoder host's core count.
///
/// A stripe is a picture of its own on the wire, and the client decodes one per stripe: the
/// count is spent on the viewer's machine, which is not the one it was derived from. Measured
/// decoding a 1080p frame split N ways, WebKit -- the engine a phone or tablet runs -- loses
/// throughput with every stripe added (18.9 fps whole, 9.4 at eight, 3.8 at thirty-two), while
/// Chromium gains up to four and holds through twelve. Eight is where both sit near their best
/// and where the JPEG payload is smallest, so a host with the cores to cut thirty-two no longer
/// hands a tablet a frame it cannot assemble. One stripe is one encode job, so this bounds the
/// encoder's own parallelism with it.
const MAX_STRIPES: usize = 8;

/// How many horizontal regions a frame of `height` is split into, which is the choice of how
/// much encode parallelism to spend on it.
///
/// A video codec streams whole frames, one region; JPEG fans out across cores, held to
/// `MAX_STRIPES` and bounded so no stripe is shorter than `MIN_STRIPE_HEIGHT` rows. Both the
/// encoder and the settings line report from here, so what is logged is what is encoded.
pub fn stripe_count(height: i32, codec: Codec) -> usize {
    if !codec.stripes() || height < MIN_STRIPE_HEIGHT {
        return 1;
    }
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    cores
        .min(MAX_STRIPES)
        .min((height / MIN_STRIPE_HEIGHT) as usize)
        .max(1)
}

/// Frames of the target a constant-rate x264 session is given as its buffer while its cleanup
/// runs through the rate control, or `CLEANUP_FRAME_S` of its rate where that is more: in the
/// session's buffer of a frame and a half x264's row-level control prices a still screen's whole
/// residual against the buffer and coarsens the rows it names a fine quantizer for, so a screen
/// of text at 2 Mbit/s at 1080p sat at 31 dB while the quantizer x264 planned read 19; in four
/// it reached 46 dB in five seconds with no frame over a budget and a half. A row finer than its
/// reference is priced at its whole intra cost against a slice's share of the buffer: at 60 fps
/// four frames held a 1080p page at 0.5 Mbit/s at quantizer 30 and 25-35 dB, or at 51 after a
/// scroll, where 0.2 s reached 55.7 dB in 30 s.
#[cfg(feature = "gpl")]
const CLEANUP_VBV_FRAMES: f64 = 4.0;

/// The buffer, in kbit, of an x264 session at `bitrate_kbps` while it is cleaned up
/// (`CLEANUP_VBV_FRAMES`).
#[cfg(feature = "gpl")]
fn cleanup_vbv_kbit(settings: &CaptureSettings, bitrate_kbps: i32) -> i32 {
    let frames = crate::encoders::vbv_bits(
        (bitrate_kbps as u32).saturating_mul(1000),
        settings.target_fps,
        0.0,
        CLEANUP_VBV_FRAMES,
    ) / 1000;
    let seconds = (bitrate_kbps.max(0) as f64 * crate::pipeline::CLEANUP_FRAME_S).round() as u32;
    frames.max(seconds).max(1) as i32
}

/// Frames of the target a constant-rate x264 session is given as its buffer where the caller
/// names no `video_vbv_multiplier`. x264's row-level control prices each row against a slice's
/// share of the buffer, so in the frame and a half the other encoders take it coded a moving
/// 1080p window on a 2560x1600 screen at half its target: 1.95 Mbit/s of 4 at 30.4 dB, 2.96 in
/// four frames, 3.52 at 34.6 dB in eight, 3.68 in fifteen, with no latency lost on a LAN (6.2 ms
/// to draw and decode at 1.5 frames, 5.5 at 8). Eight frames hold a frame's worst case to 133 ms
/// of the rate at 60 fps, where fifteen added queue on a slow link for 4% more of the target.
const CBR_VBV_FRAMES: f64 = 8.0;

/// The `(bitrate_kbps, vbv_kbit)` the software H.264 encoder is programmed with: the CBR
/// target, with a buffer of `CBR_VBV_FRAMES` unless `video_vbv_multiplier` names one, or the
/// peak of a CRF a positive `video_vbv_multiplier` caps. An uncapped CRF gets a buffer of 0.
fn rate_control(settings: &CaptureSettings) -> (i32, i32) {
    let bitrate = settings.video_bitrate_kbps.max(1);
    if !settings.video_cbr_mode && settings.video_vbv_multiplier <= 0.0 {
        return (bitrate, 0);
    }
    let vbv = (crate::encoders::vbv_bits(
        (bitrate as u32).saturating_mul(1000),
        settings.target_fps,
        settings.keyframe_interval_s,
        if settings.video_cbr_mode && settings.video_vbv_multiplier <= 0.0 {
            CBR_VBV_FRAMES
        } else {
            settings.video_vbv_multiplier
        },
    ) / 1000)
        .max(1) as i32;
    (bitrate, vbv)
}

/// Divide `height` into `n` contiguous regions as `(y_start, height)`, by codec.
///
/// - **JPEG**: JPEG has no vertical subsampling, so stripes may be any height; the heights differ
///   by at most one row — the first `remainder` stripes take one extra each — and every row of
///   the frame is covered.
/// - **Video**: one region, its height forced even since 4:2:0 pairs chroma rows vertically. The
///   deliberate cost is that a single trailing odd row may be left uncovered — preferable to an
///   odd-height picture the encoder cannot represent.
fn compute_stripe_geometries(height: usize, n: usize, codec: Codec) -> Vec<(usize, usize)> {
    if codec.is_video() {
        return vec![(0, height & !1)];
    }
    let mut geoms = Vec::with_capacity(n);
    let mut current_y = 0;
    let base_h = height / n;
    let remainder = height - base_h * n;
    for i in 0..n {
        let s_h = base_h + if i < remainder { 1 } else { 0 };
        geoms.push((current_y, s_h));
        current_y += s_h;
    }
    geoms
}

#[cfg(test)]
mod stripe_count_tests {
    use super::{Codec, MAX_STRIPES, MIN_STRIPE_HEIGHT, stripe_count};

    /// The count is what the client decodes, so a host with many cores cannot raise it: a 4K
    /// frame has room for thirty-three stripes at the minimum height and still gets at most
    /// `MAX_STRIPES`.
    #[test]
    fn a_tall_frame_is_not_cut_into_one_stripe_per_core() {
        let n = stripe_count(2160, Codec::Jpeg);
        assert!(n <= MAX_STRIPES, "a 4K frame cut into {n} stripes");
        assert!(n >= 1, "a 4K frame cut into none");
    }

    /// A stripe still never falls below the minimum height, so a short frame is cut by its own
    /// height rather than by the ceiling.
    #[test]
    fn a_short_frame_is_cut_by_its_height() {
        assert_eq!(stripe_count(MIN_STRIPE_HEIGHT - 1, Codec::Jpeg), 1);
        assert!(stripe_count(MIN_STRIPE_HEIGHT * 2, Codec::Jpeg) <= 2);
    }

    /// A video codec carries one picture a frame whatever the height.
    #[test]
    fn a_video_frame_is_one_region() {
        for codec in Codec::VIDEO {
            assert_eq!(stripe_count(2160, codec), 1, "{codec:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    /// Stripes are coded at once only where there are several and `cpu_encode_threading` is on.
    #[test]
    fn stripes_are_coded_at_once_only_with_cpu_encode_threading() {
        use super::stripes_at_once;
        use crate::CaptureSettings;
        let on = CaptureSettings::default();
        let off = CaptureSettings {
            cpu_encode_threading: false,
            ..Default::default()
        };
        assert!(stripes_at_once(&on, 8));
        assert!(!stripes_at_once(&on, 1));
        assert!(!stripes_at_once(&off, 8));
        assert!(!stripes_at_once(&off, 1));
    }

    /// A CRF session is capped only where a VBV multiplier asks for it, with the buffer that
    /// multiplier names at the configured rate as its peak; CBR always has one.
    #[test]
    fn a_crf_session_takes_a_vbv_only_where_a_multiplier_asks() {
        use crate::CaptureSettings;
        let crf = |mult| CaptureSettings {
            video_bitrate_kbps: 8000,
            video_vbv_multiplier: mult,
            target_fps: 30.0,
            ..Default::default()
        };
        assert_eq!(super::rate_control(&crf(0.0)), (8000, 0));
        assert_eq!(super::rate_control(&crf(12.0)), (8000, 3200));
        let cbr = CaptureSettings {
            video_cbr_mode: true,
            ..crf(0.0)
        };
        assert!(super::rate_control(&cbr).1 > 0);
    }

    use super::{Codec, StripeState, compute_stripe_geometries};

    /// Without `gpl` a constant-rate stripe is cleaned up through OpenH264's rate control past the
    /// frames it skips: a page coded far over the target is followed by seconds of skipped frames,
    /// which say nothing of the picture, and the cleanup refines once they end, where a burst of
    /// a few frames ran out inside them and left the picture as the page's own frame had it.
    #[cfg(not(feature = "gpl"))]
    #[test]
    fn openh264_cleans_up_past_the_frames_it_skips() {
        use crate::CaptureSettings;
        use openh264::decoder::Decoder;
        use openh264::formats::YUVSource;
        let (w, h) = (640, 352);
        let settings = CaptureSettings {
            width: w,
            height: h,
            codec: Codec::H264,
            target_fps: 30.0,
            video_cbr_mode: true,
            video_bitrate_kbps: 150,
            video_streaming_mode: false,
            omit_stripe_headers: true,
            ..Default::default()
        };
        let flat = vec![128u8; (w * h * 4) as usize];
        let page: Vec<u8> = (0..(w * h) as usize)
            .flat_map(|i| {
                let v = if ((i * 2654435761usize) >> 7).is_multiple_of(3) {
                    230
                } else {
                    20
                };
                [v, v, v, 255]
            })
            .collect();
        let mut stripes = Vec::new();
        let mut dec = Decoder::new().expect("decoder");
        let (mut sent, mut skipped_before_last, mut last) = (0usize, false, None);
        for i in 0..900u16 {
            let px = if i < 10 { &flat } else { &page };
            let out = super::encode_cpu(
                &mut stripes,
                px,
                w,
                h,
                &[],
                &settings,
                i,
                false,
                true,
                i == 0,
            );
            for stripe in &out {
                if i >= 10 {
                    sent += 1;
                }
                if let Ok(Some(img)) = dec.decode(&stripe.data) {
                    let stride = img.strides().0;
                    last = Some(
                        (0..h as usize)
                            .flat_map(|r| img.y()[r * stride..r * stride + w as usize].to_vec())
                            .collect::<Vec<u8>>(),
                    );
                }
            }
            if i > 10
                && out.is_empty()
                && stripes
                    .first()
                    .is_some_and(|st| st.h264_burst_frames_remaining > 0)
            {
                skipped_before_last = true;
            }
        }
        let luma: Vec<u8> = page.chunks(4).map(|p| p[0]).collect();
        let picture = last.expect("a picture");
        let mse = picture
            .iter()
            .zip(&luma)
            .map(|(&a, &b)| {
                let d = (a as f64 - 16.0) * 255.0 / 219.0 - b as f64;
                d * d
            })
            .sum::<f64>()
            / luma.len() as f64;
        let psnr = 10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10();
        println!(
            "frames sent after the page: {sent}, skipped inside the cleanup: {skipped_before_last}, {psnr:.1} dB"
        );
        assert!(
            skipped_before_last,
            "the rate control skipped frames inside the cleanup"
        );
        assert!(
            sent > 20,
            "the cleanup refined past the skipped frames: {sent} frames"
        );
        assert!(psnr > 30.0, "{psnr:.1} dB");
    }

    /// A scheduled key frame waits for the software H.264 session to change: a still screen
    /// sends nothing through its intervals, and the first change after one keys the frame, once.
    #[test]
    fn a_scheduled_key_frame_waits_for_the_change() {
        use crate::CaptureSettings;
        let (w, h) = (128, 512);
        let settings = CaptureSettings {
            width: w,
            height: h,
            codec: Codec::H264,
            video_crf: 25,
            use_paint_over_quality: false,
            video_streaming_mode: false,
            keyframe_interval_s: 1.0,
            target_fps: 60.0,
            ..Default::default()
        };
        let mut stripes = Vec::new();
        let px: Vec<u8> = (0..(w * h * 4) as usize).map(|i| (i % 251) as u8).collect();
        let mut encode = |pixels: &[u8], frame: u16| {
            super::encode_cpu(
                &mut stripes,
                pixels,
                w,
                h,
                &[],
                &settings,
                frame,
                false,
                true,
                false,
            )
        };
        assert_eq!(encode(&px, 1).len(), 1, "the first frame is sent");
        assert!(
            (2..200).all(|frame| encode(&px, frame).is_empty()),
            "a still screen sends nothing through its intervals"
        );
        let mut moved = px.clone();
        for b in moved.iter_mut().take((w * 8 * 4) as usize) {
            *b = b.wrapping_add(97);
        }
        let key = encode(&moved, 200);
        assert_eq!(key.len(), 1, "the change sends the frame");
        assert_eq!(key[0].data[1], 0x11, "as the key frame owed");
        for b in moved.iter_mut().take((w * 8 * 4) as usize) {
            *b = b.wrapping_add(31);
        }
        let next = encode(&moved, 201);
        assert_eq!(next.len(), 1);
        assert_ne!(next[0].data[1], 0x11, "once");
    }

    /// Without `gpl` the software H.264 session is one OpenH264 instance for the whole frame,
    /// framed as every video session is: the first frame is an IDR whose wire header carries
    /// y-start 0 and the frame's geometry and decodes on its own to a picture of the frame's
    /// size, a static follow-up sends nothing, and motion confined to the top rows re-sends the
    /// frame as a delta.
    #[cfg(not(feature = "gpl"))]
    #[test]
    fn openh264_codes_the_whole_frame() {
        use crate::CaptureSettings;
        use openh264::decoder::Decoder;
        use openh264::formats::YUVSource;
        let (w, h) = (128, 512);
        let settings = CaptureSettings {
            width: w,
            height: h,
            codec: Codec::H264,
            video_crf: 25,
            use_paint_over_quality: false,
            video_streaming_mode: false,
            ..Default::default()
        };
        let mut stripes = Vec::new();
        let px: Vec<u8> = (0..(w * h * 4) as usize).map(|i| (i % 251) as u8).collect();
        let mut encode = |pixels: &[u8], frame: u16| {
            super::encode_cpu(
                &mut stripes,
                pixels,
                w,
                h,
                &[],
                &settings,
                frame,
                false,
                true,
                false,
            )
        };
        let first = encode(&px, 0);
        assert_eq!(first.len(), 1, "the first frame is sent whole");
        let d = &first[0].data;
        assert_eq!(d[0], 0x04, "video tag");
        assert_eq!(d[1], 0x11, "the first frame is an H.264 key frame");
        assert_eq!(u16::from_be_bytes([d[2], d[3]]), 0, "frame number");
        assert_eq!(u16::from_be_bytes([d[4], d[5]]), 0, "y-start");
        assert_eq!(u16::from_be_bytes([d[6], d[7]]) as i32, w, "width");
        assert_eq!(u16::from_be_bytes([d[8], d[9]]) as i32, h, "height");
        assert_eq!((first[0].stripe_y_start, first[0].stripe_height), (0, h));
        let mut dec = Decoder::new().expect("decoder");
        let img = dec
            .decode(&d[crate::encoders::codec::VIDEO_HEADER_LEN..])
            .expect("decode")
            .expect("an IDR decodes on its own");
        assert_eq!(img.dimensions(), (w as usize, h as usize));
        assert!(encode(&px, 1).is_empty(), "a static frame sends nothing");
        let mut moved = px.clone();
        for b in moved.iter_mut().take((w * 8 * 4) as usize) {
            *b = b.wrapping_add(97);
        }
        let top = encode(&moved, 2);
        assert_eq!(top.len(), 1, "motion in the top rows re-sends the frame");
        assert_eq!(
            top[0].data[1], 0x10,
            "an unforced follow-up is an H.264 delta frame"
        );
        assert_eq!(
            u16::from_be_bytes([top[0].data[2], top[0].data[3]]),
            2,
            "frame number"
        );
    }

    /// A stripe hashed elsewhere reads as one hashed here: the same changes, damage blocks and
    /// their renewal for the same sequence of hashes, whatever produced them.
    #[test]
    fn hash_dirty_reads_as_content_dirty() {
        let frames: Vec<Vec<u8>> = [1u8, 1, 2, 1, 1, 1, 1, 1, 3, 4, 5, 6, 7, 7, 7, 7, 7, 7]
            .iter()
            .map(|&v| vec![v; 256])
            .collect();
        let (mut here, mut there) = (StripeState::default(), StripeState::default());
        for frame in &frames {
            assert_eq!(
                here.content_dirty(frame, 2, 3),
                there.hash_dirty(super::fast_hash(frame), 2, 3)
            );
            assert_eq!(here.in_damage_block, there.in_damage_block);
        }
    }

    /// With `threshold = 2` and `duration = 3`, a first change reads dirty and two consecutive
    /// changes open a damage block that holds dirty for three frames without re-hashing; once content
    /// has gone static, the end-of-block re-hash exits the block and the stripe reads clean again.
    #[test]
    fn content_dirty_detects_change_and_damage_block() {
        let mut st = StripeState::default();
        let a = vec![1u8; 256];
        let b = vec![2u8; 256];
        assert!(st.content_dirty(&a, 2, 3));
        assert!(!st.content_dirty(&a, 2, 3));
        assert!(st.content_dirty(&b, 2, 3));
        assert!(st.content_dirty(&a, 2, 3));
        assert!(st.in_damage_block);
        assert!(st.content_dirty(&a, 2, 3));
        assert!(st.content_dirty(&a, 2, 3));
        assert!(st.content_dirty(&a, 2, 3));
        assert!(!st.in_damage_block);
        assert!(!st.content_dirty(&a, 2, 3));

        // Motion through a whole block renews it; motion that stops inside one ends with it.
        let frames: Vec<Vec<u8>> = (0..12u8).map(|i| vec![i; 256]).collect();
        let mut st = StripeState::default();
        for f in &frames {
            assert!(st.content_dirty(f, 2, 3));
        }
        assert!(
            st.in_damage_block,
            "a region moving every frame stays in its block"
        );
        let still = frames.last().unwrap();
        let dirty: Vec<bool> = (0..6).map(|_| st.content_dirty(still, 2, 3)).collect();
        assert_eq!(
            dirty,
            [true, true, false, false, false, false],
            "clean once the block that saw it stop ends"
        );
    }

    /// With compositor damage as the authority (Wayland), a clean frame must still advance the
    /// paint-over countdown and fire the repaint at the trigger, and once every stripe has
    /// latched (`paint_over_sent`) further clean frames must produce nothing — that quiescent
    /// tail is the idle fast path, which skips the stripe fan-out entirely.
    #[test]
    fn clean_frames_countdown_fire_paintover_then_go_quiescent() {
        use crate::CaptureSettings;
        let (w, h) = (64, 128);
        let pixels = vec![128u8; (w * h * 4) as usize];
        let settings = CaptureSettings {
            width: w,
            height: h,
            codec: Codec::Jpeg,
            jpeg_quality: 60,
            paint_over_jpeg_quality: 90,
            use_paint_over_quality: true,
            paint_over_trigger_frames: 5,
            ..Default::default()
        };
        let mut stripes = Vec::new();
        let full = [smithay::utils::Rectangle::new((0, 0).into(), (w, h).into())];
        let dirty = super::encode_cpu(
            &mut stripes,
            &pixels,
            w,
            h,
            &full,
            &settings,
            0,
            false,
            false,
            false,
        );
        assert!(!dirty.is_empty(), "damaged frame must encode");

        let mut painted = Vec::new();
        for frame in 1..=20u16 {
            let out = super::encode_cpu(
                &mut stripes,
                &pixels,
                w,
                h,
                &[],
                &settings,
                frame,
                false,
                false,
                false,
            );
            painted.extend(out.iter().map(|s| (frame, s.stripe_y_start)));
        }
        let per_frame = stripes.len().div_ceil(super::CLEANUP_STAGGER_FRAMES).max(1);
        assert_eq!(
            painted.len(),
            stripes.len(),
            "each stripe is painted over exactly once: {painted:?}"
        );
        assert_eq!(
            painted[0].0, settings.paint_over_trigger_frames as u16,
            "starting at the trigger"
        );
        for (n, (frame, _)) in painted.iter().enumerate() {
            assert_eq!(
                *frame as usize,
                settings.paint_over_trigger_frames as usize + n / per_frame,
                "{per_frame} a frame"
            );
        }
        assert!(
            stripes.iter().all(|st| st.paint_over_sent),
            "all stripes latched after the repaint"
        );
    }

    /// Hash-damage sources (X11) take the sequential-scan fast path: static frames advance
    /// the countdown and fire the paint-over exactly once, the quiescent tail emits nothing,
    /// and a subsequent content change is still detected and encoded (streak state reset by
    /// the fast path must not swallow the wake-up).
    #[test]
    fn hash_scan_idles_after_paintover_and_wakes_on_change() {
        use crate::CaptureSettings;
        let (w, h) = (64, 128);
        let static_px = vec![128u8; (w * h * 4) as usize];
        let changed_px = vec![200u8; (w * h * 4) as usize];
        let settings = CaptureSettings {
            width: w,
            height: h,
            codec: Codec::Jpeg,
            jpeg_quality: 60,
            paint_over_jpeg_quality: 90,
            use_paint_over_quality: true,
            paint_over_trigger_frames: 5,
            damage_block_threshold: 10,
            damage_block_duration: 10,
            ..Default::default()
        };
        let mut stripes = Vec::new();
        let first = super::encode_cpu(
            &mut stripes,
            &static_px,
            w,
            h,
            &[],
            &settings,
            0,
            false,
            true,
            false,
        );
        assert!(
            !first.is_empty(),
            "first frame hashes as changed and encodes"
        );

        let mut painted = Vec::new();
        for frame in 1..=20u16 {
            let out = super::encode_cpu(
                &mut stripes,
                &static_px,
                w,
                h,
                &[],
                &settings,
                frame,
                false,
                true,
                false,
            );
            painted.extend(out.iter().map(|s| (frame, s.stripe_y_start)));
        }
        assert_eq!(
            painted.len(),
            stripes.len(),
            "each stripe is painted over exactly once while static: {painted:?}"
        );
        assert_eq!(painted[0].0, settings.paint_over_trigger_frames as u16);

        let woke = super::encode_cpu(
            &mut stripes,
            &changed_px,
            w,
            h,
            &[],
            &settings,
            21,
            false,
            true,
            false,
        );
        assert!(!woke.is_empty(), "content change after idle must encode");
    }

    /// A caret changing one stripe every fourth frame keeps it from ever holding still for the
    /// trigger, yet that stripe is cleaned up once the low-motion window passes, and the stripes
    /// around it are cleaned up at the trigger as if it were not there.
    #[test]
    fn a_blinking_caret_starves_no_stripe_of_its_cleanup() {
        use crate::CaptureSettings;
        let (w, h) = (64i32, 256i32);
        let settings = CaptureSettings {
            width: w,
            height: h,
            codec: Codec::Jpeg,
            jpeg_quality: 40,
            paint_over_jpeg_quality: 90,
            use_paint_over_quality: true,
            paint_over_trigger_frames: 5,
            damage_block_threshold: 10,
            damage_block_duration: 10,
            ..Default::default()
        };
        let mut stripes = Vec::new();
        let frame = |caret: bool| {
            let mut px = vec![128u8; (w * h * 4) as usize];
            if caret {
                px[(w * 4 * 3) as usize..(w * 4 * 3) as usize + 8].fill(0);
            }
            px
        };
        // The luma DC quantizer of a stripe's JPEG: 3 at the paint-over quality, 20 at the base one.
        let dc_quant = |data: &[u8]| {
            let at = data
                .windows(2)
                .position(|m| m == [0xFF, 0xDB])
                .expect("a quantization table");
            data[at + 5]
        };
        let mut cleaned = std::collections::HashMap::new();
        for n in 0..60u16 {
            let out = super::encode_cpu(
                &mut stripes,
                &frame((n / 4) % 2 == 0),
                w,
                h,
                &[],
                &settings,
                n,
                false,
                true,
                false,
            );
            for s in &out {
                if dc_quant(&s.data) < 8 {
                    cleaned.entry(s.stripe_y_start).or_insert(n);
                }
            }
        }
        let rows = stripes.len();
        assert!(rows > 1, "the frame is striped");
        let caret_stripe = 0;
        let others: Vec<u16> = cleaned
            .iter()
            .filter(|(y, _)| **y != caret_stripe)
            .map(|(_, n)| *n)
            .collect();
        assert_eq!(
            others.len(),
            rows - 1,
            "every still stripe is cleaned up: {cleaned:?}"
        );
        assert!(
            others
                .iter()
                .all(|&n| n <= 5 + super::CLEANUP_STAGGER_FRAMES as u16),
            "at the trigger: {cleaned:?}"
        );
        let at = *cleaned
            .get(&caret_stripe)
            .expect("the caret's stripe is cleaned up too");
        assert!(
            (19..=23).contains(&at),
            "once the low-motion window passes: {at}"
        );
    }

    /// Total rows covered by a geometry — the sum of all stripe heights.
    fn covered(geoms: &[(usize, usize)]) -> usize {
        geoms.iter().map(|&(_, h)| h).sum()
    }

    /// Assert the stripes tile the frame with no gaps or overlap: each stripe's `y_start`
    /// equals the running sum of the preceding heights.
    fn assert_contiguous(geoms: &[(usize, usize)]) {
        let mut y = 0;
        for &(sy, sh) in geoms {
            assert_eq!(sy, y, "stripes must be contiguous");
            y += sh;
        }
    }

    /// JPEG geometry covers the full frame height with contiguous stripes, across a range of
    /// heights (odd ones included) and stripe counts.
    #[test]
    fn jpeg_covers_every_row_including_odd() {
        for &h in &[1usize, 63, 720, 721, 1079, 1080, 1081] {
            for &n in &[1usize, 2, 3, 8, 16] {
                let g = compute_stripe_geometries(h, n, Codec::Jpeg);
                assert_eq!(g.len(), n);
                assert_eq!(
                    covered(&g),
                    h,
                    "JPEG must cover full height h={} n={}",
                    h,
                    n
                );
                assert_contiguous(&g);
            }
        }
    }

    /// Video geometry is one region of even height covering the whole frame except at most one
    /// trailing odd row, whatever the count asked for.
    #[test]
    fn a_video_frame_is_one_even_region() {
        for &h in &[64usize, 720, 721, 1080, 1081] {
            for &n in &[1usize, 2, 8] {
                let g = compute_stripe_geometries(h, n, Codec::H264);
                assert_eq!(g, vec![(0, h & !1)], "h={h} n={n}");
            }
        }
    }

    /// The host convert sites chroma at the center of the block on both its paths, the
    /// single-threaded one and the banded one, and a band boundary never splits a chroma pair.
    #[test]
    fn the_host_convert_sites_chroma_at_the_block_center() {
        use super::convert_to_yuv_mt;
        let (w, h) = (64usize, 64usize);
        let bgra = crate::encoders::chroma_siting::bgra(w, h);
        for bands in [1usize, 4, 7] {
            let (mut yp, mut up, mut vp) =
                (vec![0u8; w * h], vec![0u8; w * h / 4], vec![0u8; w * h / 4]);
            convert_to_yuv_mt(
                &bgra,
                (w * 4) as u32,
                w,
                h,
                false,
                false,
                false,
                false,
                &mut yp,
                &mut up,
                &mut vp,
                (w, w / 2),
                bands,
            )
            .expect("convert");
            let worst = up
                .iter()
                .zip(&vp)
                .map(|(&u, &v)| (f64::from(u) - 128.0).hypot(f64::from(v) - 128.0))
                .fold(0.0f64, f64::max);
            assert!(
                worst <= 2.0,
                "{bands} bands: chroma sits {worst:.1} off neutral"
            );
        }
    }

    /// The JPEG stripes a WebSockets session sends by default hand BGRA to libjpeg-turbo, which
    /// subsamples chroma itself, so the tile is held to the same neutral chroma there.
    #[test]
    fn the_jpeg_path_sites_chroma_at_the_block_center() {
        use super::Codec;
        use crate::webcam::decode::new_decoder;
        let (w, h) = (64usize, 64usize);
        let bgra = crate::encoders::chroma_siting::bgra(w, h);
        let mut comp = turbojpeg::Compressor::new().expect("turbojpeg compressor");
        comp.set_quality(90).expect("quality");
        let img = turbojpeg::Image {
            pixels: &bgra[..],
            width: w,
            pitch: w * 4,
            height: h,
            format: turbojpeg::PixelFormat::BGRA,
        };
        let jpeg = comp.compress_to_vec(img).expect("compress");
        let mut dec = new_decoder(Codec::Jpeg).expect("jpeg decoder");
        assert!(
            dec.decode(&jpeg).expect("decode"),
            "the stripe decoded nothing"
        );
        let worst = crate::encoders::chroma_siting::worst(&dec.frame().expect("frame"));
        assert!(worst <= 4.0, "JPEG chroma sits {worst:.1} off neutral");
    }

    /// The 10-bit conversion against the BT.709 arithmetic it implements, at both ranges and
    /// both chroma formats: every sample within one code value, a 4:2:0 chroma sample being
    /// the conversion of its block's mean, and the same picture whatever the band count.
    #[test]
    fn ten_bit_conversion_matches_bt709() {
        use super::convert_to_yuv10_mt;
        let (w, h) = (34usize, 18usize);
        let mut bgra = vec![0u8; w * h * 4];
        let mut seed = 0x2545_f491u32;
        for px in bgra.as_chunks_mut::<4>().0 {
            for c in px.iter_mut().take(3) {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                *c = (seed >> 11) as u8;
            }
            px[3] = 255;
        }
        let reference = |r: f64, g: f64, b: f64, full: bool| {
            let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            let (cb, cr) = ((b - y) / 1.8556, (r - y) / 1.5748);
            if full {
                (
                    y * 1023.0 / 255.0,
                    512.0 + cb * 1023.0 / 255.0,
                    512.0 + cr * 1023.0 / 255.0,
                )
            } else {
                (
                    64.0 + y * 876.0 / 255.0,
                    512.0 + cb * 896.0 / 255.0,
                    512.0 + cr * 896.0 / 255.0,
                )
            }
        };
        let rgb = |x: usize, y: usize| {
            let p = &bgra[(y * w + x) * 4..];
            (p[2] as f64, p[1] as f64, p[0] as f64)
        };
        for full in [false, true] {
            for i444 in [false, true] {
                let (cw, ch) = if i444 { (w, h) } else { (w / 2, h / 2) };
                let convert = |bands: usize| {
                    let (mut y, mut u, mut v) =
                        (vec![0u16; w * h], vec![0u16; cw * ch], vec![0u16; cw * ch]);
                    convert_to_yuv10_mt(
                        &bgra,
                        w * 4,
                        w,
                        h,
                        false,
                        i444,
                        full,
                        &mut y,
                        &mut u,
                        &mut v,
                        (w, cw),
                        bands,
                    );
                    (y, u, v)
                };
                let (y, u, v) = convert(1);
                assert_eq!(
                    (y.clone(), u.clone(), v.clone()),
                    convert(4),
                    "bands change nothing"
                );
                for row in 0..h {
                    for col in 0..w {
                        let (r, g, b) = rgb(col, row);
                        let want = reference(r, g, b, full).0;
                        assert!((y[row * w + col] as f64 - want).abs() <= 1.0, "luma");
                    }
                }
                for row in 0..ch {
                    for col in 0..cw {
                        let (r, g, b) = if i444 {
                            rgb(col, row)
                        } else {
                            let mut sum = (0.0, 0.0, 0.0);
                            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                                let p = rgb(2 * col + dx, 2 * row + dy);
                                sum = (sum.0 + p.0 / 4.0, sum.1 + p.1 / 4.0, sum.2 + p.2 / 4.0);
                            }
                            sum
                        };
                        let (_, cb, cr) = reference(r, g, b, full);
                        assert!((u[row * cw + col] as f64 - cb).abs() <= 1.0, "cb");
                        assert!((v[row * cw + col] as f64 - cr).abs() <= 1.0, "cr");
                    }
                }
            }
        }
    }

    /// A 10-bit x264 session takes and reports quantizers on the scale an 8-bit one does: the
    /// rate factor's first frame reads the same quantizer at either depth, and a key frame held
    /// at a coarse quantizer comes out as small.
    #[cfg(feature = "gpl")]
    #[test]
    fn x264_quantizers_keep_their_scale_at_ten_bits() {
        use super::H264EncoderWrapper;
        if !H264EncoderWrapper::ten_bit() {
            println!("this libx264 was built for 8 bits alone; nothing to check");
            return;
        }
        let (w, h) = (640usize, 360usize);
        let mut seed = 7u32;
        let mut noise = |n: usize, amplitude: u32, base: u32| -> Vec<u32> {
            (0..n)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    base + seed % amplitude
                })
                .collect()
        };
        let (luma, chroma) = (noise(w * h, 60, 90), noise(w * h / 4, 20, 118));
        let mut seen = Vec::new();
        for depth in [8u32, 10] {
            let plane = |samples: &[u32]| -> Vec<u8> {
                if depth == 10 {
                    samples
                        .iter()
                        .flat_map(|s| ((s * 4) as u16).to_le_bytes())
                        .collect()
                } else {
                    samples.iter().map(|s| *s as u8).collect()
                }
            };
            let (y, c) = (plane(&luma), plane(&chroma));
            let bytes = (depth as usize).div_ceil(8);
            let mut enc = H264EncoderWrapper::with_depth(
                w as i32, h as i32, 25, false, depth, 30.0, 1, false, 0, 0, 0, 0, 0,
            )
            .expect("x264 init");
            let encode = |enc: &mut H264EncoderWrapper, id: u16| {
                let mut out = Vec::new();
                assert!(enc.encode_with_headers(
                    &y,
                    &c,
                    &c,
                    (w * bytes) as i32,
                    (w / 2 * bytes) as i32,
                    (w / 2 * bytes) as i32,
                    id,
                    true,
                    true,
                    &mut out,
                ));
                out.len()
            };
            encode(&mut enc, 0);
            let rate_factor_qp = enc.last_qp().expect("a quantizer");
            enc.hold_quantizer(45);
            seen.push((rate_factor_qp, encode(&mut enc, 1)));
        }
        let ((qp8, held8), (qp10, held10)) = (seen[0], seen[1]);
        assert!(
            qp8.abs_diff(qp10) <= 2,
            "rate factor quantizers {qp8} and {qp10}"
        );
        assert!(
            held10 < 2 * held8,
            "held key frames of {held8} and {held10} bytes"
        );
    }

    /// A 10-bit x264 session declares High 10, or High 4:4:4 Predictive, and codes the
    /// 16-bit planes it is handed.
    #[cfg(feature = "gpl")]
    #[test]
    fn x264_codes_ten_bits() {
        use super::H264EncoderWrapper;
        if !H264EncoderWrapper::ten_bit() {
            println!("this libx264 was built for 8 bits alone; nothing to check");
            return;
        }
        let (w, h) = (64usize, 64usize);
        for (i444, profile) in [(false, 110u8), (true, 244u8)] {
            let mut enc = H264EncoderWrapper::with_depth(
                w as i32, h as i32, 25, i444, 10, 30.0, 1, false, 0, 0, 0, 0, 0,
            )
            .expect("x264 init");
            assert_eq!(enc.bit_depth, 10);
            let (cw, ch) = if i444 { (w, h) } else { (w / 2, h / 2) };
            let planes = [
                vec![400u16; w * h],
                vec![512u16; cw * ch],
                vec![600u16; cw * ch],
            ];
            let bytes = |p: &[u16]| unsafe {
                std::slice::from_raw_parts(p.as_ptr().cast::<u8>(), p.len() * 2).to_vec()
            };
            let (y, u, v) = (bytes(&planes[0]), bytes(&planes[1]), bytes(&planes[2]));
            let mut out = Vec::new();
            assert!(enc.encode_with_headers(
                &y,
                &u,
                &v,
                (w * 2) as i32,
                (cw * 2) as i32,
                (cw * 2) as i32,
                0,
                true,
                true,
                &mut out,
            ));
            let sps = crate::encoders::codec::annexb_nals(&out)
                .find(|n| n[0] & 0x1f == 7)
                .expect("a sequence parameter set");
            assert_eq!(sps[1], profile, "the profile declared");
        }
    }

    /// The NV12 conversion x264 encodes in place writes the samples the I420 one does, its
    /// chroma interleaved, from either byte order, at any band count and into padded rows.
    #[test]
    fn the_nv12_convert_writes_the_i420_samples_interleaved() {
        use super::{Nv12Planes, convert_to_nv12_mt, convert_to_yuv_mt};
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        for (w, h) in [(64usize, 64usize), (130, 66), (1920, 1080)] {
            let pixels: Vec<u8> = (0..w * h * 4)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    seed as u8
                })
                .collect();
            for rgba in [false, true] {
                let (mut yp, mut up, mut vp) =
                    (vec![0u8; w * h], vec![0u8; w * h / 4], vec![0u8; w * h / 4]);
                convert_to_yuv_mt(
                    &pixels,
                    (w * 4) as u32,
                    w,
                    h,
                    rgba,
                    false,
                    false,
                    false,
                    &mut yp,
                    &mut up,
                    &mut vp,
                    (w, w / 2),
                    1,
                )
                .expect("I420");
                for bands in [1usize, 4, 7] {
                    let mut nv12 = Nv12Planes::new(w, h);
                    let strides = (nv12.stride[0], nv12.stride[1]);
                    let (y, uv) = nv12.planes_mut();
                    convert_to_nv12_mt(&pixels, (w * 4) as u32, w, h, rgba, y, uv, strides, bands)
                        .expect("NV12");
                    let (y, uv) = nv12.planes();
                    for row in 0..h {
                        assert_eq!(
                            &y[row * strides.0..][..w],
                            &yp[row * w..][..w],
                            "{w}x{h} rgba {rgba}: luma row {row}"
                        );
                    }
                    for row in 0..h / 2 {
                        let line = &uv[row * strides.1..][..w];
                        let (u, v) = (&up[row * w / 2..][..w / 2], &vp[row * w / 2..][..w / 2]);
                        assert!(
                            line.chunks(2)
                                .zip(u.iter().zip(v))
                                .all(|(c, (u, v))| c == [*u, *v]),
                            "{w}x{h} rgba {rgba}, {bands} bands: chroma row {row}"
                        );
                    }
                }
            }
        }
    }

    /// An NV12 picture laid out as x264 asks is encoded where it lies, reading through the next
    /// encode the picture before (`held_input`), and codes the stream the same planes copied
    /// as I420 code, across a release.
    #[cfg(feature = "gpl")]
    #[test]
    #[cfg(x264_in_place)]
    fn x264_encodes_an_nv12_picture_in_place_as_it_codes_a_copy() {
        use super::{H264EncoderWrapper, Nv12Planes, convert_to_nv12_mt, convert_to_yuv_mt};
        let (w, h) = (200usize, 120usize);
        let frames: Vec<Vec<u8>> = (0..6)
            .map(|f| {
                (0..w * h * 4)
                    .map(|i| {
                        let (x, y) = ((i / 4) % w, (i / 4) / w);
                        ((x + 3 * f) * 7 + y * 3 + (i % 4) * 50) as u8
                    })
                    .collect()
            })
            .collect();
        let open = || {
            H264EncoderWrapper::new(w as i32, h as i32, 25, false, 30.0, 4, false, 0, 0, 0, 0)
                .expect("x264 init")
        };
        let (mut copied, mut in_place) = (open(), open());
        let mut pictures = [Nv12Planes::new(w, h), Nv12Planes::new(w, h)];
        assert!(
            pictures[0].in_place,
            "the linked libx264 lays out a picture to encode in place"
        );
        let layout = super::x264_input_layout(w, h).expect("layout");
        let align = codec_sys::x264::X264_IN_PLACE_ALIGN as usize;
        for p in &pictures {
            let (y, uv) = p.planes();
            assert!(
                (y.as_ptr().addr() - layout.offset[0]).is_multiple_of(align)
                    && (uv.as_ptr().addr() - layout.offset[1]).is_multiple_of(align),
                "each plane lies its layout offset into a buffer aligned as x264 asks"
            );
        }
        let (mut a, mut b) = (Vec::new(), Vec::new());
        for (n, bgra) in frames.iter().enumerate() {
            let (mut yp, mut up, mut vp) =
                (vec![0u8; w * h], vec![0u8; w * h / 4], vec![0u8; w * h / 4]);
            convert_to_yuv_mt(
                bgra,
                (w * 4) as u32,
                w,
                h,
                false,
                false,
                false,
                false,
                &mut yp,
                &mut up,
                &mut vp,
                (w, w / 2),
                1,
            )
            .unwrap();
            let mut out = Vec::new();
            assert!(copied.encode_with_headers(
                &yp,
                &up,
                &vp,
                w as i32,
                (w / 2) as i32,
                (w / 2) as i32,
                n as u16,
                false,
                true,
                &mut out,
            ));
            a.extend_from_slice(&out);

            let mb_w = w.div_ceil(16) * 16;
            let picture = &mut pictures[n % 2];
            let strides = (picture.stride[0], picture.stride[1]);
            let (y, uv) = picture.planes_mut();
            convert_to_nv12_mt(bgra, (w * 4) as u32, w, h, false, y, uv, strides, 2).unwrap();
            y[mb_w] = !y[w - 1];
            if n == 4 {
                in_place.release_input();
                assert_eq!(in_place.held_input(), None);
            }
            let picture = &pictures[n % 2];
            assert!(in_place.encode_nv12_with_headers(picture, n as u16, false, true, &mut out));
            b.extend_from_slice(&out);
            assert_eq!(in_place.held_input(), Some(picture.id()));
            let (y, _) = picture.planes();
            assert_eq!(
                y[mb_w],
                y[w - 1],
                "x264 padded the picture where it lies, past its last macroblock"
            );
        }
        assert!(a == b, "the stream the copy codes");
    }
}

#[cfg(test)]
mod qp_bound_sweep {
    //! Invariants under test: the CBR QP clamp reaches libx264/OpenH264 (a max clamp must
    //! raise worst-case fidelity on rate-starved text at the cost of bitrate overshoot;
    //! a min clamp must cut spend on over-budgeted content) and defaults (0) leave the
    //! encoders' own behavior untouched. Each encoder is swept separately: the OpenH264
    //! sweep runs in every build (the crate is a dev-dependency), the x264 one needs `gpl`.
    #[cfg(feature = "gpl")]
    use super::H264EncoderWrapper;
    use crate::CaptureSettings;
    use crate::encoders::Codec;
    use crate::encoders::oh264::Openh264Encoder;
    use openh264::decoder::Decoder;
    use openh264::formats::YUVSource;

    const W: usize = 1280;
    const H: usize = 720;
    const FRAMES: usize = 60;

    /// Build a scrolling terminal-like luma frame: an 8x12 glyph grid seeded by an LCG and
    /// scrolled 4 px per frame — the worst case for screen-share rate control, with dense
    /// high-contrast detail (~40% lit pixels per glyph row) under full-frame motion.
    fn text_luma(frame: usize) -> Vec<u8> {
        let mut y = vec![18u8; W * H];
        let scroll = frame * 4;
        for row in 0..H {
            let srow = row + scroll;
            let cell_y = srow / 12;
            let in_glyph_y = srow % 12;
            if in_glyph_y >= 10 {
                continue;
            }
            for col in 0..W {
                let cell_x = col / 8;
                let in_glyph_x = col % 8;
                if in_glyph_x >= 7 {
                    continue;
                }
                let mut s = (cell_x as u32)
                    .wrapping_mul(2654435761)
                    .wrapping_add((cell_y as u32).wrapping_mul(40503))
                    .wrapping_add(1);
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                if (s >> ((in_glyph_y * 3 + in_glyph_x) % 29)) & 1 == 1 {
                    y[row * W + col] = 224;
                }
            }
        }
        y
    }

    /// Encode `FRAMES` scrolling-text luma frames through the x264 session at the
    /// given rate-control settings (constant gray chroma), returning each frame's raw bitstream.
    #[cfg(feature = "gpl")]
    fn encode_x264(cbr: bool, kbps: i32, crf: i32, min_qp: i32, max_qp: i32) -> Vec<Vec<u8>> {
        let mut enc = H264EncoderWrapper::new(
            W as i32, H as i32, crf, false, 60.0, 4, cbr, kbps, 50, min_qp, max_qp,
        )
        .expect("x264 init");
        let u = vec![128u8; (W / 2) * (H / 2)];
        let v = vec![128u8; (W / 2) * (H / 2)];
        (0..FRAMES)
            .map(|i| {
                let y = text_luma(i);
                let mut out = Vec::new();
                enc.encode_with_headers(
                    &y,
                    &u,
                    &v,
                    W as i32,
                    (W / 2) as i32,
                    (W / 2) as i32,
                    i as u16,
                    i == 0,
                    true,
                    &mut out,
                );
                out
            })
            .collect()
    }

    /// The x264 stream declares the signal its input was converted with: the BT.709 matrix in
    /// both chroma formats, at full range for I444 and limited for I420.
    #[cfg(feature = "gpl")]
    #[test]
    fn x264_declares_the_conversion_matrix() {
        use crate::encoders::codec::annexb_nals;
        use crate::encoders::sps::read_color;
        use crate::webcam::decode::{ColorTags, Decoder as _, VideoDecoder};
        for (i444, want) in [
            (false, ColorTags::BT709_LIMITED),
            (true, ColorTags::BT709_FULL),
        ] {
            let (w, h) = (128usize, 96usize);
            let mut enc =
                H264EncoderWrapper::new(w as i32, h as i32, 25, i444, 30.0, 1, false, 0, 0, 0, 0)
                    .expect("x264 init");
            let (cw, ch) = if i444 { (w, h) } else { (w / 2, h / 2) };
            let y = vec![90u8; w * h];
            let u = vec![128u8; cw * ch];
            let v = vec![160u8; cw * ch];
            let mut out = Vec::new();
            assert!(enc.encode_with_headers(
                &y, &u, &v, w as i32, cw as i32, cw as i32, 0, true, true, &mut out
            ));
            let sps = annexb_nals(&out)
                .find(|n| n[0] & 0x1f == 7)
                .expect("an SPS");
            let declared = read_color(sps).map(|s| ColorTags {
                matrix: s.matrix,
                full_range: s.full_range,
            });
            assert_eq!(declared, Some(want), "i444={i444}");
            if !i444 {
                let mut dec = VideoDecoder::new(Codec::H264).expect("decoder");
                assert!(dec.decode(&out).expect("decode"));
                assert_eq!(dec.color_tags(), Some(want));
            }
        }
    }

    /// x264 is opened at the capture's rate as the fraction it names, its SPS declares that
    /// rate, and a session moved to another rate reopens at it.
    #[cfg(feature = "gpl")]
    #[test]
    fn x264_takes_the_frame_rate_as_its_fraction() {
        use crate::encoders::frame_rate::FrameRate;
        use crate::encoders::sps::h264_timing;
        let (w, h) = (64usize, 64usize);
        let (y, uv) = (vec![90u8; w * h], vec![128u8; w * h / 4]);
        for (num, den) in [(60000u32, 1001u32), (120000, 1001), (144000, 1001), (60, 1)] {
            let mut enc = H264EncoderWrapper::new(
                w as i32,
                h as i32,
                25,
                false,
                num as f64 / den as f64,
                1,
                true,
                4000,
                100,
                0,
                0,
            )
            .expect("x264 init");
            let param = unsafe {
                let mut param: codec_sys::x264::x264_param_t = std::mem::zeroed();
                codec_sys::x264::x264_encoder_parameters(enc.encoder, &mut param);
                param
            };
            assert_eq!((param.i_fps_num, param.i_fps_den), (num, den));
            let mut out = Vec::new();
            assert!(enc.encode_with_headers(
                &y,
                &uv,
                &uv,
                w as i32,
                (w / 2) as i32,
                (w / 2) as i32,
                0,
                true,
                true,
                &mut out
            ));
            assert_eq!(
                h264_timing(&out),
                Some((den, 2 * num)),
                "{num}/{den}: the SPS declares the rate"
            );
        }
        let mut enc = H264EncoderWrapper::new(
            w as i32, h as i32, 25, false, 60.0, 1, true, 4000, 100, 0, 0,
        )
        .expect("x264 init");
        enc.reconfigure_rate(4000, 100, 60000.0 / 1001.0);
        assert_eq!(
            enc.current_fps,
            FrameRate {
                num: 60000,
                den: 1001
            },
            "60 fps moved to 59.94"
        );
    }

    /// The color chart, converted by the host path and encoded by x264, decodes back to the
    /// color that was painted when the BT.709 the stream declares is inverted — the check a
    /// client's presentation path performs on every frame, here with no browser in the way.
    #[cfg(feature = "gpl")]
    #[test]
    fn x264_paints_the_chart_it_converts() {
        use super::convert_to_yuv_mt;
        use crate::encoders::chroma_siting::{BT709, chart_bgra, chart_error};
        use crate::webcam::decode::{Decoder as _, VideoDecoder};
        let (w, h) = (256usize, 128usize);
        let bgra = chart_bgra(w, h);
        let (mut y, mut u, mut v) = (vec![0u8; w * h], vec![0u8; w * h / 4], vec![0u8; w * h / 4]);
        convert_to_yuv_mt(
            &bgra,
            (w * 4) as u32,
            w,
            h,
            false,
            false,
            false,
            false,
            &mut y,
            &mut u,
            &mut v,
            (w, w / 2),
            4,
        )
        .expect("convert");
        let mut enc =
            H264EncoderWrapper::new(w as i32, h as i32, 20, false, 30.0, 1, false, 0, 0, 0, 0)
                .expect("x264 init");
        let mut out = Vec::new();
        assert!(enc.encode_with_headers(
            &y,
            &u,
            &v,
            w as i32,
            (w / 2) as i32,
            (w / 2) as i32,
            0,
            true,
            true,
            &mut out
        ));
        let mut dec = VideoDecoder::new(Codec::H264).expect("decoder");
        assert!(dec.decode(&out).expect("decode"));
        let worst = chart_error(&dec.frame().expect("frame"), BT709);
        println!("[chart] x264: worst |dRGB| {worst:.1}");
        assert!(
            worst <= 12.0,
            "the software H.264 path paints {worst:.1} off the chart"
        );
    }

    /// Every x264 session bounds reordering at zero, 4:2:0 and 4:4:4, at a constant quantizer
    /// and a constant rate.
    #[cfg(feature = "gpl")]
    #[test]
    fn x264_bounds_reordering_at_zero() {
        use crate::encoders::sps::fixtures::assert_no_reorder;
        for (fullcolor, cbr) in [(false, false), (false, true), (true, false), (true, true)] {
            let mut enc = H264EncoderWrapper::new(
                W as i32, H as i32, 20, fullcolor, 60.0, 4, cbr, 8000, 0, 0, 0,
            )
            .expect("x264 init");
            let chroma = if fullcolor { W * H } else { W * H / 4 };
            let (u, v) = (vec![128u8; chroma], vec![128u8; chroma]);
            let stride = if fullcolor { W } else { W / 2 } as i32;
            let mut out = Vec::new();
            assert!(enc.encode_with_headers(
                &text_luma(0),
                &u,
                &v,
                W as i32,
                stride,
                stride,
                0,
                true,
                true,
                &mut out
            ));
            assert_no_reorder(&out, &format!("x264 fullcolor {fullcolor} cbr {cbr}"));
        }
    }

    /// A CRF session given a VBV codes its key frame within the buffer, where the same session
    /// without one codes it at whatever size the rate factor asks.
    #[cfg(feature = "gpl")]
    #[test]
    fn x264_crf_bounds_a_key_frame_by_its_vbv() {
        const VBV_KBIT: i32 = 1500;
        let key = |vbv_kbit: i32| {
            let mut enc = H264EncoderWrapper::new(
                W as i32, H as i32, 12, false, 60.0, 4, false, 8000, vbv_kbit, 0, 0,
            )
            .expect("x264 init");
            let (u, v) = (vec![128u8; W * H / 4], vec![128u8; W * H / 4]);
            let mut out = Vec::new();
            assert!(enc.encode_with_headers(
                &text_luma(0),
                &u,
                &v,
                W as i32,
                (W / 2) as i32,
                (W / 2) as i32,
                0,
                true,
                true,
                &mut out
            ));
            out.len() * 8
        };
        let (uncapped, capped) = (key(0), key(VBV_KBIT));
        assert!(
            capped <= VBV_KBIT as usize * 1000,
            "a {capped}-bit key frame overflows the {VBV_KBIT} kbit buffer"
        );
        assert!(
            uncapped > 2 * capped,
            "the uncapped key frame ({uncapped} bits) is not the rate factor's own"
        );
    }

    /// A capped CRF session its content starves overshoots the buffer rather than code past
    /// quantizer 51, where x264 forces skips that leave rows frozen on old content.
    #[cfg(feature = "gpl")]
    #[test]
    fn x264_capped_crf_codes_no_quantizer_past_51() {
        let mut enc =
            H264EncoderWrapper::new(W as i32, H as i32, 23, false, 60.0, 4, false, 300, 10, 0, 0)
                .expect("x264 init");
        let (u, v) = (vec![128u8; W * H / 4], vec![128u8; W * H / 4]);
        let mut worst = 0;
        for i in 0..FRAMES {
            let mut out = Vec::new();
            enc.encode_with_headers(
                &text_luma(i),
                &u,
                &v,
                W as i32,
                (W / 2) as i32,
                (W / 2) as i32,
                i as u16,
                i == 0,
                true,
                &mut out,
            );
            worst = worst.max(enc.last_qp().unwrap_or(0));
        }
        assert!(worst <= 51, "a starved capped CRF coded quantizer {worst}");
    }

    /// A constant-rate session on a still screen of text after a scroll says the quantizer its
    /// rows were coded at: at 1 Mbit/s at 720p x264 plans its frames at 17 while its row-level
    /// control holds every row at 51, coding nothing, so a frame said to be that fine holds a
    /// picture that fine.
    #[cfg(feature = "gpl")]
    #[test]
    fn x264_says_the_quantizer_a_still_screens_rows_were_coded_at() {
        const KBPS: i32 = 1000;
        const SCROLL: usize = 30;
        const STILL: usize = 300;
        const PAINT_QP: u32 = 18;
        let mut enc = H264EncoderWrapper::new(
            W as i32,
            H as i32,
            25,
            false,
            60.0,
            4,
            true,
            KBPS,
            KBPS * 4 / 60,
            0,
            0,
        )
        .expect("x264 init");
        let (u, v) = (vec![128u8; W * H / 4], vec![128u8; W * H / 4]);
        let still = text_luma(SCROLL);
        let mut dec = Decoder::new().expect("decoder");
        let mut picture = vec![0u8; W * H];
        let mut misread = Vec::new();
        for i in 0..SCROLL + STILL {
            let y = if i < SCROLL {
                text_luma(i)
            } else {
                still.clone()
            };
            let mut out = Vec::new();
            enc.encode_with_headers(
                &y,
                &u,
                &v,
                W as i32,
                (W / 2) as i32,
                (W / 2) as i32,
                i as u16,
                i == 0,
                true,
                &mut out,
            );
            if let Some(img) = dec.decode(&out).expect("decode") {
                let stride = img.strides().0;
                for r in 0..H {
                    picture[r * W..r * W + W].copy_from_slice(&img.y()[r * stride..r * stride + W]);
                }
            }
            let psnr = mean_psnr(std::slice::from_ref(&picture), std::slice::from_ref(&still));
            if i >= SCROLL && enc.last_qp().is_some_and(|q| q <= PAINT_QP) && psnr < 40.0 {
                misread.push((i, enc.last_qp(), out.len(), psnr as i32));
            }
        }
        assert!(
            misread.is_empty(),
            "frames said to be at the paint-over quantizer: {:?}",
            &misread[..misread.len().min(8)]
        );
    }

    /// A constant-rate session refines a still screen of text after a scroll in a cleanup's
    /// buffer at 60 fps: in four frames x264's row-level control prices every row at its whole
    /// intra cost against a slice's share and codes none, leaving 720p at 1 Mbit/s at 12 dB,
    /// where in 0.2 s it reaches 39 dB in ten seconds.
    #[cfg(feature = "gpl")]
    #[test]
    fn x264_refines_a_still_screen_after_a_scroll_in_a_cleanups_buffer() {
        const KBPS: i32 = 1000;
        const SCROLL: usize = 30;
        const STILL: usize = 600;
        let settings = CaptureSettings {
            target_fps: 60.0,
            ..Default::default()
        };
        let session_vbv =
            (crate::encoders::vbv_bits(KBPS as u32 * 1000, 60.0, 0.0, 0.0) / 1000) as i32;
        let mut enc = H264EncoderWrapper::new(
            W as i32,
            H as i32,
            25,
            false,
            60.0,
            4,
            true,
            KBPS,
            session_vbv,
            0,
            0,
        )
        .expect("x264 init");
        let (u, v) = (vec![128u8; W * H / 4], vec![128u8; W * H / 4]);
        let still = text_luma(SCROLL);
        let mut dec = Decoder::new().expect("decoder");
        let mut picture = vec![0u8; W * H];
        for i in 0..SCROLL + STILL {
            if i == SCROLL {
                enc.reconfigure_rate(KBPS, super::cleanup_vbv_kbit(&settings, KBPS), 60.0);
            }
            let y = if i < SCROLL {
                text_luma(i)
            } else {
                still.clone()
            };
            let mut out = Vec::new();
            enc.encode_with_headers(
                &y,
                &u,
                &v,
                W as i32,
                (W / 2) as i32,
                (W / 2) as i32,
                i as u16,
                i == 0,
                true,
                &mut out,
            );
            if let Some(img) = dec.decode(&out).expect("decode") {
                let stride = img.strides().0;
                for r in 0..H {
                    picture[r * W..r * W + W].copy_from_slice(&img.y()[r * stride..r * stride + W]);
                }
            }
        }
        let psnr = mean_psnr(std::slice::from_ref(&picture), std::slice::from_ref(&still));
        assert!(psnr > 35.0, "a still screen refined to {psnr:.1} dB");
    }

    /// A CRF session capped or uncapped live codes its next key frame by the new setting.
    #[cfg(feature = "gpl")]
    #[test]
    fn x264_crf_takes_a_cap_set_or_lifted_live() {
        const VBV_KBIT: i32 = 1500;
        let (u, v) = (vec![128u8; W * H / 4], vec![128u8; W * H / 4]);
        let mut enc =
            H264EncoderWrapper::new(W as i32, H as i32, 12, false, 60.0, 4, false, 8000, 0, 0, 0)
                .expect("x264 init");
        let key = |enc: &mut H264EncoderWrapper| {
            let mut out = Vec::new();
            assert!(enc.encode_with_headers(
                &text_luma(0),
                &u,
                &v,
                W as i32,
                (W / 2) as i32,
                (W / 2) as i32,
                0,
                true,
                true,
                &mut out
            ));
            out.len() * 8
        };
        let uncapped = key(&mut enc);
        enc.reconfigure_rate(8000, VBV_KBIT, 60.0);
        let capped = key(&mut enc);
        enc.reconfigure_rate(8000, 0, 60.0);
        let lifted = key(&mut enc);
        assert!(
            capped <= VBV_KBIT as usize * 1000,
            "a {capped}-bit key frame after capping overflows the {VBV_KBIT} kbit buffer ({uncapped} bits before)"
        );
        assert!(
            lifted > 2 * capped,
            "the key frame after lifting the cap ({lifted} bits) is still capped"
        );
    }

    /// A frame a client lost is left out of the predictions: the next frame predicts from the
    /// newest frame before it and names it, a decoder that never saw the lost frames decodes it
    /// as one that saw everything does, and a loss the window no longer covers becomes a key
    /// frame. The stream declares the decoded picture buffer this needs.
    #[cfg(feature = "gpl")]
    #[test]
    fn x264_predicts_past_a_lost_frame() {
        use crate::encoders::codec::{FRAME_DELTA, FRAME_KEY, h264_frame_type};
        use crate::encoders::reference::{REFERENCE_FRAMES, Reference};
        use crate::encoders::sps::h264_max_num_ref_frames;
        use crate::webcam::decode::{Decoder as _, VideoDecoder};
        let (u, v) = (vec![128u8; W * H / 4], vec![128u8; W * H / 4]);
        let mut enc =
            H264EncoderWrapper::new(W as i32, H as i32, 20, false, 60.0, 4, false, 0, 0, 0, 0)
                .expect("x264 init");
        let encode = |enc: &mut H264EncoderWrapper, i: usize| {
            let y = text_luma(i);
            let mut out = Vec::new();
            assert!(enc.encode_with_headers(
                &y,
                &u,
                &v,
                W as i32,
                (W / 2) as i32,
                (W / 2) as i32,
                i as u16,
                i == 0,
                true,
                &mut out
            ));
            (out, enc.last_reference())
        };
        let mut frames: Vec<Vec<u8>> = Vec::new();
        for i in 0..8 {
            let (out, reference) = encode(&mut enc, i);
            assert_eq!(
                reference,
                if i == 0 {
                    Reference::None
                } else {
                    Reference::Frame(i as u16 - 1)
                }
            );
            frames.push(out);
        }
        assert_eq!(
            h264_max_num_ref_frames(&frames[0]),
            Some(REFERENCE_FRAMES),
            "the SPS declares the DPB"
        );
        // Frame 5 is reported lost once 6 and 7 have gone out.
        assert!(enc.invalidate_reference(5));
        let (out, reference) = encode(&mut enc, 8);
        assert_eq!(reference, Reference::Frame(4));
        assert_eq!(h264_frame_type(&out), FRAME_DELTA);
        frames.push(out);
        let (out, reference) = encode(&mut enc, 9);
        assert_eq!(reference, Reference::Frame(8));
        frames.push(out);
        let (mut whole, mut lossy) = (
            VideoDecoder::new(Codec::H264).unwrap(),
            VideoDecoder::new(Codec::H264).unwrap(),
        );
        for (i, f) in frames.iter().enumerate() {
            assert!(whole.decode(f).expect("decode"), "frame {i}");
            if !(5..8).contains(&i) {
                assert!(lossy.decode(f).expect("decode without 5-7"), "frame {i}");
            }
        }
        let apart = luma_distance(&whole.frame().unwrap(), &lossy.frame().unwrap());
        assert!(
            apart < 0.5,
            "the decoder that lost frames 5-7 shows frame 9 {apart:.2} off the one that saw them"
        );
        let source = text_luma(9);
        let off = lossy
            .frame()
            .unwrap()
            .y
            .chunks(lossy.frame().unwrap().y_stride)
            .take(H)
            .zip(source.chunks(W))
            .flat_map(|(row, src)| {
                row[..W]
                    .iter()
                    .zip(src)
                    .map(|(&a, &b)| (a as f64 - b as f64).abs())
            })
            .sum::<f64>()
            / (W * H) as f64;
        assert!(
            off < 6.0,
            "frame 9 decoded without frames 5-7 is {off:.2} off the picture painted"
        );
        for i in 10..20 {
            frames.push(encode(&mut enc, i).0);
        }
        // Frame 9 has left an 8-frame window holding 12..19, so every reference predicts
        // through it: the next frame is a key frame.
        assert!(enc.invalidate_reference(9));
        let (out, reference) = encode(&mut enc, 20);
        assert_eq!(reference, Reference::None);
        assert_eq!(h264_frame_type(&out), FRAME_KEY);
    }

    /// A session told to keep one reference frame (`video_reference_frames`) declares a decoded
    /// picture buffer of one, and a loss of the frame before the last is a key frame.
    #[cfg(feature = "gpl")]
    #[test]
    fn x264_keeps_the_reference_frames_it_is_given() {
        use crate::encoders::codec::{FRAME_KEY, h264_frame_type};
        use crate::encoders::sps::h264_max_num_ref_frames;
        let (u, v) = (vec![128u8; W * H / 4], vec![128u8; W * H / 4]);
        let mut enc = H264EncoderWrapper::with_depth(
            W as i32, H as i32, 20, false, 8, 60.0, 1, false, 0, 0, 0, 0, 1,
        )
        .expect("x264 init");
        let mut frames = Vec::new();
        for i in 0..4usize {
            let mut out = Vec::new();
            assert!(enc.encode_with_headers(
                &text_luma(i),
                &u,
                &v,
                W as i32,
                (W / 2) as i32,
                (W / 2) as i32,
                i as u16,
                i == 0,
                true,
                &mut out
            ));
            frames.push(out);
        }
        assert_eq!(h264_max_num_ref_frames(&frames[0]), Some(1));
        assert!(enc.invalidate_reference(3));
        let mut out = Vec::new();
        assert!(enc.encode_with_headers(
            &text_luma(4),
            &u,
            &v,
            W as i32,
            (W / 2) as i32,
            (W / 2) as i32,
            4,
            false,
            true,
            &mut out
        ));
        assert_eq!(h264_frame_type(&out), FRAME_KEY, "nothing older is held");
    }

    /// x264 counts `frame_num` in sixteen values and the stream in 4096 (`WideFrameNum`): a loss
    /// covering frame 16, where x264's own count wraps, is predicted past, and one covering frame
    /// 4096 is answered with a key frame, since past a gap across the stream's wrap FFmpeg's
    /// decoder drops about a range of pictures.
    #[test]
    #[cfg(feature = "gpl")]
    fn x264_answers_a_loss_at_the_frame_num_wrap_with_a_key_frame() {
        use crate::encoders::codec::{FRAME_DELTA, FRAME_KEY, h264_frame_type};
        use crate::encoders::reference::Reference;
        use crate::encoders::sps::h264_frame_num_range;
        use crate::webcam::decode::{Decoder as _, VideoDecoder};
        let (w, h) = (160usize, 96usize);
        let (u, v) = (vec![128u8; w * h / 4], vec![128u8; w * h / 4]);
        let mut enc =
            H264EncoderWrapper::new(w as i32, h as i32, 20, false, 60.0, 1, false, 0, 0, 0, 0)
                .expect("x264 init");
        let encode = |enc: &mut H264EncoderWrapper, i: usize| {
            let y: Vec<u8> = (0..w * h)
                .map(|p| ((p % w + p / w + 3 * i) * 5 % 256) as u8)
                .collect();
            let mut out = Vec::new();
            assert!(enc.encode_with_headers(
                &y,
                &u,
                &v,
                w as i32,
                (w / 2) as i32,
                (w / 2) as i32,
                i as u16,
                i == 0,
                true,
                &mut out
            ));
            (out, enc.last_reference())
        };
        let (mut whole, mut lossy) = (
            VideoDecoder::new(Codec::H264).unwrap(),
            VideoDecoder::new(Codec::H264).unwrap(),
        );
        let (first, _) = encode(&mut enc, 0);
        let range = h264_frame_num_range(&first).expect("the key frame carries the SPS") as usize;
        assert_eq!(range, 4096, "x264's sixteen values, a byte wider");
        assert!(whole.decode(&first).expect("decode") && lossy.decode(&first).expect("decode"));
        for i in 1..=18 {
            let (out, _) = encode(&mut enc, i);
            assert!(whole.decode(&out).expect("decode"), "frame {i}");
            if i < 16 {
                assert!(lossy.decode(&out).expect("decode"), "frame {i}");
            }
        }
        assert!(enc.invalidate_reference(16), "x264's wrap frame is lost");
        let (out, reference) = encode(&mut enc, 19);
        assert_eq!(reference, Reference::Frame(15));
        assert_eq!(h264_frame_type(&out), FRAME_DELTA);
        assert!(whole.decode(&out).expect("decode"));
        assert!(lossy.decode(&out).expect("decode past x264's wrap"));
        let apart = luma_distance(&whole.frame().unwrap(), &lossy.frame().unwrap());
        assert!(
            apart < 0.5,
            "the decoder that lost frames 16-18 shows frame 19 {apart:.2} off the one that saw them"
        );
        for i in 20..=range {
            let (out, _) = encode(&mut enc, i);
            if i < range {
                assert!(lossy.decode(&out).expect("decode"), "frame {i}");
            }
        }
        assert!(
            enc.invalidate_reference(range as u16),
            "the stream's wrap frame is reported lost"
        );
        let (out, reference) = encode(&mut enc, range + 1);
        assert_eq!(reference, Reference::None);
        assert_eq!(h264_frame_type(&out), FRAME_KEY);
        assert!(
            lossy.decode(&out).expect("decode past the wrap"),
            "the decoder that never saw the wrap frame shows the next one"
        );
        assert_eq!(
            encode(&mut enc, range + 2).1,
            Reference::Frame(range as u16 + 1)
        );
        assert!(
            enc.invalidate_reference(range as u16 + 2),
            "the count restarted at the key frame"
        );
        assert_eq!(
            encode(&mut enc, range + 3).1,
            Reference::Frame(range as u16 + 1)
        );
    }

    /// Mean absolute luma difference between two decoded pictures.
    #[cfg(feature = "gpl")]
    fn luma_distance(
        a: &crate::webcam::convert::I420View<'_>,
        b: &crate::webcam::convert::I420View<'_>,
    ) -> f64 {
        let rows =
            a.y.chunks(a.y_stride)
                .zip(b.y.chunks(b.y_stride))
                .take(a.height);
        rows.flat_map(|(ra, rb)| {
            ra[..a.width]
                .iter()
                .zip(&rb[..a.width])
                .map(|(&x, &y)| (x as f64 - y as f64).abs())
        })
        .sum::<f64>()
            / (a.width * a.height) as f64
    }

    /// Encode the same scrolling-text sequence through the OpenH264 full-frame encoder (luma
    /// broadcast to a gray BGRA frame), returning each frame's bitstream for comparison with the
    /// x264 run.
    fn encode_oh264(cbr: bool, kbps: i32, crf: i32, min_qp: i32, max_qp: i32) -> Vec<Vec<u8>> {
        let s = CaptureSettings {
            width: W as i32,
            height: H as i32,
            target_fps: 60.0,
            codec: Codec::H264,
            video_cbr_mode: cbr,
            video_bitrate_kbps: kbps,
            video_crf: crf,
            video_min_qp: min_qp,
            video_max_qp: max_qp,
            ..Default::default()
        };
        let mut enc = Openh264Encoder::new(&s).expect("oh264 init");
        (0..FRAMES)
            .map(|i| {
                let y = text_luma(i);
                let mut bgra = vec![255u8; W * H * 4];
                for (px, &l) in bgra.as_chunks_mut::<4>().0.iter_mut().zip(y.iter()) {
                    px[0] = l;
                    px[1] = l;
                    px[2] = l;
                }
                enc.encode_host_argb(&bgra, W * 4, i as u64, i == 0, false)
                    .expect("oh264 encode")
            })
            .collect()
    }

    /// Decode a sequence of H.264 frames back to tightly-packed luma planes (dropping empty
    /// frames and the decoded chroma) for PSNR comparison.
    fn decode_luma(frames: &[Vec<u8>]) -> Vec<Vec<u8>> {
        let mut dec = Decoder::new().expect("decoder");
        let mut out = Vec::new();
        for f in frames {
            if f.is_empty() {
                continue;
            }
            if let Ok(Some(img)) = dec.decode(f) {
                let (w, h) = img.dimensions();
                let stride = img.strides().0;
                let mut y = vec![0u8; w * h];
                for r in 0..h {
                    y[r * w..r * w + w].copy_from_slice(&img.y()[r * stride..r * stride + w]);
                }
                out.push(y);
            }
        }
        out
    }

    /// Mean per-frame luma PSNR (dB) between two decoded sequences, treating a zero-MSE frame
    /// as 99 dB.
    fn mean_psnr(a: &[Vec<u8>], b: &[Vec<u8>]) -> f64 {
        let n = a.len().min(b.len());
        let mut acc = 0.0;
        for i in 0..n {
            let mse: f64 = a[i]
                .iter()
                .zip(b[i].iter())
                .map(|(&x, &y)| {
                    let d = x as f64 - y as f64;
                    d * d
                })
                .sum::<f64>()
                / a[i].len() as f64;
            acc += if mse <= 0.0 {
                99.0
            } else {
                10.0 * (255.0f64 * 255.0 / mse).log10()
            };
        }
        acc / n.max(1) as f64
    }

    /// Average encoded bitrate (kbps) of a frame sequence, assuming 60 fps playback.
    fn kbps(frames: &[Vec<u8>]) -> f64 {
        frames.iter().map(|f| f.len()).sum::<usize>() as f64 * 8.0 * 60.0 / FRAMES as f64 / 1000.0
    }

    /// Diagnostic that the CBR QP clamp is actually plumbed through to x264, printing a
    /// bitrate/PSNR table on scrolling text and asserting the effect.
    ///
    /// Encodes worst-case scrolling text at 2 Mbps CBR across a sweep of `max_qp` values (plus a
    /// separate `min_qp` sweep on an over-provisioned 12 Mbps budget), measuring luma PSNR against a
    /// near-lossless CRF-12 reference from the same encoder so color-conversion differences cancel
    /// out. Capping `max_qp` at 30 on rate-starved content must lift fidelity by more than 0.5 dB
    /// over the unclamped run — proving the clamp reaches the encoder rather than being silently
    /// dropped (paid for in bitrate overshoot).
    #[cfg(feature = "gpl")]
    #[test]
    fn cbr_qp_bound_sweep_x264() {
        let reference = decode_luma(&encode_x264(false, 0, 12, 0, 0));

        println!("scrolling-text 720p60 @ 2 Mbps CBR, x264 (PSNR vs own CRF-12 decode):");
        let mut rows = Vec::new();
        for &max_qp in &[0i32, 45, 40, 35, 30] {
            let x = encode_x264(true, 2000, 25, 0, max_qp);
            let psnr = mean_psnr(&decode_luma(&x), &reference);
            println!(
                "  max_qp {:>2}: {:>8.1} kbps / {:>5.2} dB",
                max_qp,
                kbps(&x),
                psnr
            );
            rows.push((max_qp, psnr));
        }
        println!("scrolling-text 720p60 @ 12 Mbps CBR, x264 min-QP sweep:");
        for &min_qp in &[0i32, 10, 15] {
            let x = encode_x264(true, 12000, 25, min_qp, 0);
            let psnr = mean_psnr(&decode_luma(&x), &reference);
            println!(
                "  min_qp {:>2}: {:>8.1} kbps / {:>5.2} dB",
                min_qp,
                kbps(&x),
                psnr
            );
        }

        let base = rows[0].1;
        let capped = rows.last().unwrap().1;
        assert!(
            capped > base + 0.5,
            "x264 max-QP clamp had no effect: {capped:.2} vs {base:.2} dB"
        );
    }

    /// The OpenH264 counterpart of [`cbr_qp_bound_sweep_x264`]: the software H.264 sweep of a
    /// build without the `gpl` feature, where OpenH264 is the encoder behind every stripe.
    ///
    /// Same scrolling-text workload and 2 Mbps CBR `max_qp` sweep, with luma PSNR measured against
    /// this encoder's own near-lossless QP-12 reference. Capping `max_qp` at 30 must lift fidelity
    /// by more than 0.5 dB over the unclamped run.
    #[test]
    fn cbr_qp_bound_sweep_openh264() {
        let reference = decode_luma(&encode_oh264(false, 0, 12, 0, 0));

        println!("scrolling-text 720p60 @ 2 Mbps CBR, oh264 (PSNR vs own QP-12 decode):");
        let mut rows = Vec::new();
        for &max_qp in &[0i32, 45, 40, 35, 30] {
            let o = encode_oh264(true, 2000, 25, 0, max_qp);
            let psnr = mean_psnr(&decode_luma(&o), &reference);
            println!(
                "  max_qp {:>2}: {:>8.1} kbps / {:>5.2} dB",
                max_qp,
                kbps(&o),
                psnr
            );
            rows.push((max_qp, psnr));
        }

        let base = rows[0].1;
        let capped = rows.last().unwrap().1;
        assert!(
            capped > base + 0.5,
            "oh264 max-QP clamp had no effect: {capped:.2} vs {base:.2} dB"
        );
    }

    /// After a live frame-rate change the CBR stream still tracks the configured bitrate.
    ///
    /// x264's per-frame CBR budget is `bitrate / fps`, so halving the frame rate at a fixed kbps
    /// budget must roughly double each encoded frame while the per-second bitrate holds. This encodes
    /// incompressible full-frame noise at 20 Mbps CBR (content the rate controller cannot undershoot,
    /// so per-frame size sits at the budget; a budget the noise cannot meet within the quantizer
    /// ceiling would read the encoder's shortfall instead), measures the mean encoded frame size at 60 fps, drops
    /// to 30 fps through `reconfigure_rate`, and requires the per-frame size to roughly double — so
    /// the per-second bitrate is preserved rather than collapsing to half, which is what the pre-fix
    /// path did by leaving the session budgeting for 60 fps (`x264_encoder_reconfig` never applies a
    /// frame-rate change). Single-threaded so the rate-control measurement is deterministic; warmup
    /// frames are discarded so the ABR controller and the post-reopen IDR do not skew the mean.
    #[cfg(feature = "gpl")]
    #[test]
    fn cbr_bitrate_tracks_configured_rate_after_fps_change() {
        const TARGET_KBPS: i32 = 20000;
        const WARMUP: usize = 24;
        const MEASURED: usize = 96;
        let u = vec![128u8; (W / 2) * (H / 2)];
        let v = vec![128u8; (W / 2) * (H / 2)];
        let vbv = |fps: f64| {
            (crate::encoders::vbv_bits((TARGET_KBPS as u32) * 1000, fps, 0.0, 0.0) / 1000).max(1)
                as i32
        };
        // A fresh incompressible luma plane every frame, so inter-prediction cannot cheapen a
        // frame and CBR must spend its whole per-frame budget: the per-frame size then reads the
        // budget directly, which is exactly what a frame-rate change is supposed to move.
        let noise_luma = |frame: usize| -> Vec<u8> {
            let mut y = vec![0u8; W * H];
            let mut s = (frame as u32).wrapping_mul(2654435761).wrapping_add(1);
            for p in y.iter_mut() {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                *p = (s >> 24) as u8;
            }
            y
        };

        let mut enc = H264EncoderWrapper::new(
            W as i32,
            H as i32,
            25,
            false,
            60.0,
            1,
            true,
            TARGET_KBPS,
            vbv(60.0),
            0,
            0,
        )
        .expect("x264 init");

        let measure = |enc: &mut H264EncoderWrapper, start: usize| -> f64 {
            let mut bytes = 0usize;
            let mut counted = 0usize;
            for i in start..start + WARMUP + MEASURED {
                let y = noise_luma(i);
                let mut out = Vec::new();
                enc.encode_with_headers(
                    &y,
                    &u,
                    &v,
                    W as i32,
                    (W / 2) as i32,
                    (W / 2) as i32,
                    i as u16,
                    i == start,
                    true,
                    &mut out,
                );
                if i >= start + WARMUP && !out.is_empty() {
                    bytes += out.len();
                    counted += 1;
                }
            }
            bytes as f64 / counted.max(1) as f64
        };

        let per_frame_60 = measure(&mut enc, 0);
        enc.reconfigure_rate(TARGET_KBPS, vbv(30.0), 30.0);
        let per_frame_30 = measure(&mut enc, 1000);

        let eff_60 = per_frame_60 * 8.0 * 60.0 / 1000.0;
        let eff_30 = per_frame_30 * 8.0 * 30.0 / 1000.0;
        println!(
            "x264 CBR {TARGET_KBPS} kbps: 60fps {eff_60:.0} kbps ({per_frame_60:.0} B/frame), 30fps {eff_30:.0} kbps ({per_frame_30:.0} B/frame)"
        );

        let ratio = per_frame_30 / per_frame_60.max(1.0);
        assert!(
            (1.5..2.6).contains(&ratio),
            "30fps per-frame size {per_frame_30:.0} B vs 60fps {per_frame_60:.0} B (ratio {ratio:.2}); halving fps should roughly double each frame"
        );
        assert!(
            eff_30 > eff_60 * 0.75,
            "30fps effective bitrate {eff_30:.0} kbps collapsed from the 60fps {eff_60:.0} kbps; fps change did not re-budget the bitrate"
        );
    }
}
