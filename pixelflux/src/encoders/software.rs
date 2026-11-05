/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! CPU-based striped encoder: H.264 through the build's software encoder — libx264 with the
//! `gpl` feature, Cisco OpenH264 without it (`software_encoder`) — and turbojpeg for JPEG.
//!
//! Frames are split into horizontal stripes processed in parallel via rayon. Each stripe is
//! independently hashed against the previous frame for change detection, and only dirty stripes
//! are encoded. The H.264 path maintains per-stripe encoder state across frames for
//! inter-prediction, and both libraries emit the same per-stripe wire framing; the JPEG path is
//! stateless.

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
use yuv::{BufferStoreMut, YuvConversionMode, YuvPlanarImageMut, YuvRange, YuvStandardMatrix};

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
/// **Why the band split exists.** Color conversion is a non-trivial slice of per-frame CPU. The
/// striped path already parallelizes it for free — each stripe converts on its own rayon worker —
/// but a single full-frame consumer (the whole-frame x264 stripe, a full-frame OpenH264
/// instance, a whole-frame session of another codec) would otherwise convert its entire image on one thread and
/// stall the frame there. Splitting into horizontal bands hands that lone conversion the same
/// multi-threading the striped path enjoys. The cut is horizontal because YUV planes are
/// row-major, so a horizontal boundary yields contiguous, non-overlapping plane sub-slices with no
/// per-row seam bookkeeping.
///
/// 1. **Plane strides**: `strides` gives the Y and chroma row pitches of the output planes
///    (tightly packed for the stripe buffers, padded for an AVFrame); the chroma planes are
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

    let band_h = ((height / bands.max(1)) & !1).max(2);
    if bands <= 1 || height <= band_h {
        return convert_band(src, y_buf, u_buf, v_buf, height);
    }

    let uv_rows = |rows: usize| if i444 { rows } else { rows / 2 };
    let mut jobs = Vec::new();
    let (mut src_rest, mut y_rest, mut u_rest, mut v_rest) = (src, y_buf, u_buf, v_buf);
    let mut row = 0;
    while row < height {
        let h = if height - row < band_h + 2 {
            height - row
        } else {
            band_h
        };
        let (src_band, s_next) = src_rest.split_at(h * src_stride as usize);
        let (y_band, y_next) = y_rest.split_at_mut(h * y_stride);
        let (u_band, u_next) = u_rest.split_at_mut(uv_rows(h) * uv_stride);
        let (v_band, v_next) = v_rest.split_at_mut(uv_rows(h) * uv_stride);
        src_rest = s_next;
        y_rest = y_next;
        u_rest = u_next;
        v_rest = v_next;
        row += h;
        jobs.push((src_band, y_band, u_band, v_band, h));
    }
    jobs.into_par_iter()
        .map(|(src_band, y_band, u_band, v_band, h)| {
            convert_band(src_band, y_band, u_band, v_band, h)
        })
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

    let band_h = ((height / bands.max(1)) & !1).max(2);
    if bands <= 1 || height <= band_h {
        convert_band(src, y_buf, u_buf, v_buf, height);
        return;
    }
    let uv_rows = |rows: usize| if i444 { rows } else { rows.div_ceil(2) };
    let mut jobs = Vec::new();
    let (mut src_rest, mut y_rest, mut u_rest, mut v_rest) = (src, y_buf, u_buf, v_buf);
    let mut row = 0;
    while row < height {
        let h = if height - row < band_h + 2 {
            height - row
        } else {
            band_h
        };
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
/// not thread-safe yet the striped path opens encoders concurrently from many stripe workers.
///
/// libx264 mutates process-global state inside `x264_encoder_open`/`x264_encoder_close`, so two
/// stripe encoders opening at once — or two capture instances sharing one process — can race that
/// state and corrupt the heap. The lock is deliberately held only around open and close, never
/// around `x264_encoder_encode`, so serializing setup costs nothing in the hot per-stripe encode
/// path where the real parallelism lives.
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

/// The adaptive-quantization strength x264 sessions open at: the least that keeps it on, so
/// per-macroblock offsets apply (`H264EncoderWrapper::refine_blocks`), with no quantizer moved
/// by the picture's own variance.
#[cfg(feature = "gpl")]
const AQ_STRENGTH: f32 = 1e-4;

/// One long-lived libx264 session for a stripe, holding the raw `x264_t` handle alongside a
/// mirror of its live parameters so the encoder can be retuned per frame instead of rebuilt.
///
/// Rebuilding an x264 encoder is expensive and forces a fresh IDR, so a stripe keeps its instance
/// across frames and only nudges CRF, bitrate, VBV, and frame rate live; the tracked `current_*`
/// fields are that mirror, letting a reconfigure skip the FFI call whenever nothing actually changed.
/// `is_i444` (4:4:4 vs 4:2:0) is baked into the encoder's color space at open, so a change to it is
/// one of the few things that forces a full rebuild; `is_cbr` records which rate-control mode was
/// chosen at open and gates which of the live reconfigures apply. The manual `Send` impl exists only
/// because a raw pointer is not `Send` by default and the handle must move onto the rayon stripe
/// workers; `Drop` closes it under the global open/close lock for the same reason that lock exists.
#[cfg(feature = "gpl")]
pub struct H264EncoderWrapper {
    encoder: *mut x264_sys::x264_t,
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
    /// The quantizer the next frame is held at whatever the rate control (`hold_quantizer`).
    held_qp: Option<i32>,
    /// The mean quantizer the rate control last coded a frame's rows at, held frames aside.
    last_qp: Option<u32>,
    /// The live rate factor, which x264 reports a frame's mean quantizer against (`last_qp`).
    rf_constant: f64,
    /// The quantizer offset of each macroblock of the next frame, empty where it has none
    /// (`refine_blocks`).
    quant_offsets: Vec<f32>,
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
            unsafe { x264_sys::x264_encoder_close(self.encoder) };
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
    /// 6. **Output**: repeated headers (SPS/PPS before each keyframe) and Annex-B framing, with
    ///    x264's own logging silenced.
    /// 7. **References**: a decoded picture buffer of `REFERENCE_FRAMES` (`i_dpb_size`), declaring
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
            let mut param: x264_sys::x264_param_t = std::mem::zeroed();
            let preset = CString::new("ultrafast").unwrap();
            let tune = CString::new("zerolatency").unwrap();

            if x264_sys::x264_param_default_preset(&mut param, preset.as_ptr(), tune.as_ptr()) < 0 {
                return None;
            }

            let frame_rate = x264_frame_rate(fps);
            param.i_width = width;
            param.i_height = height;
            param.i_fps_num = frame_rate.num;
            param.i_fps_den = frame_rate.den;
            param.i_keyint_max = x264_sys::X264_KEYINT_MAX_INFINITE as i32;
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
                param.rc.i_rc_method = x264_sys::X264_RC_ABR as i32;
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
                param.rc.i_rc_method = x264_sys::X264_RC_CRF as i32;
                param.rc.f_rf_constant = crf as f32;
                if vbv_kbit > 0 {
                    param.rc.i_vbv_max_bitrate = bitrate_kbps.saturating_abs();
                    param.rc.i_vbv_buffer_size = vbv_kbit;
                    param.rc.i_qp_max = 51 + qp_offset(bit_depth);
                }
            }
            let depth_flag = if bit_depth > 8 {
                x264_sys::X264_CSP_HIGH_DEPTH
            } else {
                0
            };
            param.i_csp = (if is_i444 {
                x264_sys::X264_CSP_I444
            } else {
                x264_sys::X264_CSP_I420
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
            x264_sys::x264_param_apply_profile(&mut param, profile.as_ptr());
            // A frame's per-macroblock offsets (`refine_blocks`) reach the quantizer only under
            // adaptive quantization, which x264 turns off at strength 0, so it runs at one that
            // moves no macroblock's quantizer by itself.
            param.rc.i_aq_mode = x264_sys::X264_AQ_VARIANCE as i32;
            param.rc.f_aq_strength = AQ_STRENGTH;

            param.i_threads = threads;
            param.b_repeat_headers = 1;
            param.b_annexb = 1;
            param.i_log_level = x264_sys::X264_LOG_NONE;

            let encoder = {
                let _guard = X264_OPEN_CLOSE_LOCK
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                x264_sys::x264_encoder_open(&mut param)
            };
            if encoder.is_null() {
                None
            } else {
                x264_sys::x264_encoder_parameters(encoder, &mut param);
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
                    held_qp: None,
                    last_qp: None,
                    rf_constant: param.rc.f_rf_constant as f64,
                    quant_offsets: Vec::new(),
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
            let mut param: x264_sys::x264_param_t = std::mem::zeroed();
            x264_sys::x264_encoder_parameters(self.encoder, &mut param);
            param.rc.f_rf_constant = new_crf as f32;
            if x264_sys::x264_encoder_reconfig(self.encoder, &mut param) == 0 {
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
            let mut param: x264_sys::x264_param_t = std::mem::zeroed();
            x264_sys::x264_encoder_parameters(self.encoder, &mut param);
            param.rc.i_bitrate = bk;
            if vbv_applies {
                param.rc.i_vbv_max_bitrate = bk;
                param.rc.i_vbv_buffer_size = vbv_kbit.max(1);
            }
            if x264_sys::x264_encoder_reconfig(self.encoder, &mut param) == 0 {
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
            let mut param: x264_sys::x264_param_t = std::mem::zeroed();
            x264_sys::x264_encoder_parameters(self.encoder, &mut param);
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

    /// Put the session's own rate and VBV buffer back after a held key frame.
    fn restore_rate(&mut self) {
        unsafe {
            let mut param: x264_sys::x264_param_t = std::mem::zeroed();
            x264_sys::x264_encoder_parameters(self.encoder, &mut param);
            param.rc.i_bitrate = self.current_bitrate;
            param.rc.i_vbv_max_bitrate = self.current_bitrate;
            param.rc.i_vbv_buffer_size = self.current_vbv.max(1);
            x264_sys::x264_encoder_reconfig(self.encoder, &mut param);
        }
    }

    /// Leave frame `frame_id` and every frame after it out of the predictions. False when x264
    /// refuses, and the caller codes a key frame instead.
    pub fn invalidate_reference(&mut self, frame_id: u16) -> bool {
        match self.references.invalidate(frame_id) {
            Invalidation::Forget(pts) => unsafe {
                x264_sys::x264_encoder_invalidate_reference(self.encoder, pts as i64) == 0
            },
            Invalidation::KeyFrame | Invalidation::Ignored => true,
        }
    }

    /// Encode one YUV frame into H.264 and frame it for the wire, reporting whether the
    /// encoder actually emitted a bitstream this call.
    ///
    /// The boolean return is load-bearing: `x264_encoder_encode` can legitimately produce nothing on
    /// a given call, and the caller must forward a stripe only when real bytes exist — never an empty
    /// or header-only packet. Framing is conditional because the transport needs the pipeline's small
    /// wire header to route the stripe, while `omit_headers` consumers take the bare Annex-B
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
        y_start: u16,
        force_idr: bool,
        omit_headers: bool,
        output_buf: &mut Vec<u8>,
    ) -> bool {
        let key_budget =
            self.is_cbr && force_idr && self.held_qp.is_some() && self.open_for_held_key();
        let coded = self.encode_picture(
            y,
            u,
            v,
            y_stride,
            u_stride,
            v_stride,
            frame_id,
            y_start,
            force_idr,
            omit_headers,
            output_buf,
        );
        if key_budget {
            self.restore_rate();
        }
        coded
    }

    /// `encode_with_headers` past the choice of session.
    #[allow(clippy::too_many_arguments)]
    fn encode_picture(
        &mut self,
        y: &[u8],
        u: &[u8],
        v: &[u8],
        y_stride: i32,
        u_stride: i32,
        v_stride: i32,
        frame_id: u16,
        y_start: u16,
        force_idr: bool,
        omit_headers: bool,
        output_buf: &mut Vec<u8>,
    ) -> bool {
        unsafe {
            let mut pic_in: x264_sys::x264_picture_t = std::mem::zeroed();
            x264_sys::x264_picture_init(&mut pic_in);

            pic_in.img.i_csp = (if self.is_i444 {
                x264_sys::X264_CSP_I444
            } else {
                x264_sys::X264_CSP_I420
            } | if self.bit_depth > 8 {
                x264_sys::X264_CSP_HIGH_DEPTH
            } else {
                0
            }) as i32;
            pic_in.img.i_plane = 3;
            pic_in.img.plane[0] = y.as_ptr() as *mut u8;
            pic_in.img.plane[1] = u.as_ptr() as *mut u8;
            pic_in.img.plane[2] = v.as_ptr() as *mut u8;
            pic_in.img.i_stride[0] = y_stride;
            pic_in.img.i_stride[1] = u_stride;
            pic_in.img.i_stride[2] = v_stride;
            pic_in.i_pts = self.references.next_pts() as i64;
            pic_in.i_type = if force_idr || !self.references.has_reference() {
                x264_sys::X264_TYPE_IDR
            } else {
                x264_sys::X264_TYPE_AUTO
            } as i32;
            let held = self.held_qp.take();
            let offset = qp_offset(self.bit_depth);
            pic_in.i_qpplus1 = held.map_or(x264_sys::X264_QP_AUTO as i32, |q| q + offset + 1);
            // x264 reads the offsets as the frame is submitted, within this call.
            if !self.quant_offsets.is_empty() {
                pic_in.prop.quant_offsets = self.quant_offsets.as_mut_ptr();
            }

            let mut pic_out: x264_sys::x264_picture_t = std::mem::zeroed();
            let mut nals: *mut x264_sys::x264_nal_t = ptr::null_mut();
            let mut i_nals: i32 = 0;

            let frame_size = x264_sys::x264_encoder_encode(
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
                let frame_type = if pic_out.i_type == x264_sys::X264_TYPE_IDR as i32 {
                    FRAME_KEY
                } else if pic_out.i_type == x264_sys::X264_TYPE_I as i32 {
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
                        y_start,
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

/// Everything one horizontal stripe must remember between frames: its reused buffers, its own
/// live encoder, and the motion / paint-over / damage bookkeeping that drives its send decision.
///
/// The frame is striped so independent screen regions can encode in parallel and an unchanged region
/// can be skipped on its own, and that only works if each stripe carries its *own* cross-frame
/// history. So one instance lives per stripe for the whole session and nothing per-stripe is rebuilt
/// or recomputed from scratch each frame:
/// - **Reused buffers**: `y_buf` / `u_buf` / `v_buf` hold the stripe's YUV planes and `packet_buf`
///   the encoded output, grown in place rather than reallocated per frame.
/// - **Encoder**: `h264_encoder` is the stripe's software H.264 instance — libx264 in a `gpl`
///   build, OpenH264 otherwise — reused until its geometry (or, for x264, chroma format) changes.
/// - **Cleanup / recovery**: `no_motion_frame_count` counts consecutive static frames,
///   `paint_over_sent` records that the region was cleaned up since it last changed,
///   `h264_burst_frames_remaining` tracks a post-cleanup or recovery streaming burst (held at the
///   paint-over quantizer where `burst_held`), and `dirty_run`, `change_mass`, `unclean_frames`,
///   and `motion` (the share of recent frames in motion) are what `pipeline::cleanup_due` weighs
///   to pick the cleanup's moment and kind; `clean_quality` keeps a constant-quality session at
///   the paint-over quality from a cleanup until the region changes again; `rc_bytes` (the bytes of
///   the stripe's last frame under its rate control), `rc_debt` (the bytes a cleanup's rate control
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
    /// The planes of a 10-bit stripe, as 16-bit samples.
    #[cfg(feature = "gpl")]
    pub yuv16: [Vec<u16>; 3],
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

/// The bits per sample the striped H.264 path codes `settings` at: 10 where they ask for it
/// and the build's libx264 opens such a session, else 8.
pub fn stripe_bit_depth(settings: &CaptureSettings) -> u32 {
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

/// No stripe is shorter than a macroblock row.
const MIN_STRIPE_HEIGHT: i32 = 64;
/// The frames the stripes' cleanups of one still screen are spread over (`encode_cpu`).
const CLEANUP_STAGGER_FRAMES: usize = 4;
/// The least of a constant rate a stripe refining a still screen through x264's rate control is
/// given, in kbit/s: on shares of 62 to 100 x264 coded slice headers alone until each cleanup
/// ended (`pipeline::EMPTY_S`), seven of eight stripes of a page left at 14 dB at 0.5 Mbit/s,
/// where 250 refined every stripe at 30 and 60 fps.
const REFINE_STRIPE_KBPS: i32 = 250;
/// How fast the smoothed count of budget-carrying stripes follows the frame's.
const CARRY_RISE: f32 = 0.3;
const CARRY_FALL: f32 = 0.05;

/// Whether the striped software path holds a stripe's cleanup at the quantizer asked for:
/// libx264 does under either rate control, and OpenH264 moves its quantizer only at a constant
/// quality (`Openh264Encoder::update_qp`), so a stripe that holds none is cleaned up through its
/// rate control (`encode_cpu`), never by a key frame, which that would starve.
pub fn stripes_hold_quantizer(settings: &CaptureSettings) -> bool {
    cfg!(feature = "gpl") || !settings.video_cbr_mode
}

/// Whether a frame that holds still codes a stripe at the paint-over quality instead of the
/// session's (`encode_cpu`): at a constant quality, a stripe whose cleanup falls due, whose burst
/// is held, or which keeps the quality it was cleaned up at. A constant rate holds none there.
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

/// One frame of a stripe's `ledger`, the stripe at rows `y` to `y + height`: the rectangles of
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

/// The software encoder's per-frame entry point: split the frame into horizontal stripes,
/// decide per stripe whether it needs sending, and encode only those as JPEG or H.264 (libx264
/// or OpenH264, by build) across the rayon pool.
///
/// Two pressures drive the design: CPU H.264/JPEG is expensive, so the frame is cut into
/// parallel stripes; bandwidth is precious, so unchanged stripes are skipped. Each stripe is
/// independently hashed against the previous frame for change detection, and only dirty stripes
/// are encoded. The H.264 path maintains per-stripe encoder state across frames for
/// inter-prediction; the JPEG path is stateless.
///
/// # Arguments
///
/// * `stripes` - Persistent per-stripe state vector (resized as needed, encoder state preserved).
/// * `raw_pixels` - Packed BGRA/RGBA pixel buffer (`width * height * 4` bytes).
/// * `width` - Frame width in pixels.
/// * `height` - Frame height in pixels.
/// * `damage_rects` - Where the source says the frame changed: what marks a stripe dirty without
///   `hash_damage` (Wayland), and with it where the change lies within each stripe the hash
///   finds dirty, for its block ledger (empty where the source cannot say).
/// * `settings` - Capture settings (quality, mode, rate control, etc.).
/// * `frame_counter` - Current frame number (wrapping `u16`).
/// * `use_gpu` - `true` when the source is RGBA (GLES readback); `false` for BGRA (X11 host).
/// * `hash_damage` - `true` for X11 stripe-hash change detection; `false` when damage rects
///   are provided.
/// * `requested_idr` - Force a keyframe on every stripe (client join / reset).
///
/// # Returns
///
/// Vec of [`EncodedStripe`] — empty when nothing changed.
/// repainting a stalled region at full quality, and letting a freshly-joined or reset client recover
/// a clean picture. Persistent `StripeState` is what makes both affordable: encoders and buffers
/// survive across frames instead of being rebuilt, and the motion/paint-over history the decision
/// needs lives right beside them. The per-stripe decision mirrors `decide_hw_fullframe`'s policy for
/// the hardware full-frame encoders; it is kept as separate code here because the striped path also
/// chooses JPEG-vs-H.264 and derives its own damage.
///
/// 1. **Stripe count**: the core count, held to `MAX_STRIPES` because the client decodes one
///    picture per stripe, but collapses to a single full-frame stripe when H.264 full-frame is
///    requested or the frame is shorter than the 64-row minimum, and no stripe is thinner than 64 rows —
///    below that the per-stripe encoder and thread overhead outweighs the parallelism and the tiny
///    H.264 slices compress poorly. The persistent `stripes` vector is resized to match, preserving
///    per-stripe state across frames.
/// 2. **Idle fast path**: a frame on which no stripe can emit anything (no damage / clean
///    hashes, no cleanup due, no burst, no recovery IDR, not streaming) only advances the
///    per-stripe cleanup bookkeeping inline and returns without dispatching the stripe
///    fan-out, so a static capture never wakes the rayon pool.
/// 3. **Dirty map**: with external compositor damage (`hash_damage == false`) each `damage_rects`
///    rectangle marks every stripe whose row range it overlaps. With `hash_damage == true` (X11,
///    which has no compositor damage) per-stripe content hashing drives dirtiness instead, in
///    streaming H.264 too, since the cleanup reads a still stripe from its content.
/// 4. **Per-stripe cleanup** (`pipeline::cleanup_due`, one region per stripe): a stripe that
///    changed is cleaned up once it has held still for `paint_over_trigger`, or has kept
///    changing only a little for four times that, at the paint-over JPEG quality or, for H.264,
///    at the paint-over quality: under CBR through libx264's rate control, the stripe sent until it
///    codes at the paint-over quantizer or finer in a small frame (`pipeline::convergence`), or its
///    frames have been slice headers alone for `pipeline::EMPTY_S`, with no key frame, and where
///    the stripe's encoder reports no quantizer (OpenH264) as a refresh and burst at its rate
///    control's quality; under CRF as the stripe's rate factor until it changes again, as main's
///    paint-over was, as a key frame after a run of changes and a refresh otherwise, then a burst.
///    The stripes whose cleanups fall due on one
///    frame are spread over `CLEANUP_STAGGER_FRAMES` frames, so a screen going still costs no
///    one frame the whole screen's cleanup. A stripe is otherwise sent when it is dirty, while
///    its burst runs, or when streaming mode is on, at the base quality; a newly dirty frame
///    cancels its burst.
/// 5. **Recovery IDR** (`requested_idr`): forces a send on every stripe even when static so a
///    reconnecting client can resume. A scheduled one (`pipeline::periodic_idr_due`) waits, per
///    stripe, for the stripe to change (`StripeState::scheduled_key`): a stripe's change has no
///    known extent, so any counts as motion. For H.264 it forces an IDR at the stripe's own quality and
///    arms the burst (held at the paint-over quantizer at a constant quality, as in
///    `pipeline::decide_hw_fullframe`); for JPEG, where every stripe is already intra, it resends
///    a painted-over stripe at the paint-over quality already on screen so a joining viewer does
///    not see a downgrade.
/// 6. **Encoding**:
///    - **JPEG**: source byte order is RGBA on the GPU readback path and BGRA on
///      X11; each worker thread reuses its thread-local TurboJPEG compressor. Header-less output
///      hands the compressed buffer straight through; otherwise a 6-byte stripe header (`0x03` tag,
///      a reserved byte, frame number, y-start) is prepended to match the H.264 path's native
///      framing so the transport can forward the buffer without re-framing.
///    - **H.264**: the stripe's encoder is reused unless the width, height, or
///      (x264) chroma format changed, in which case it is rebuilt and an IDR forced; otherwise CRF
///      and rate are reconfigured live. With libx264, ARGB is converted to YUV here (a conversion
///      failure skips the stripe rather than encoding garbage) and an 8-byte fixed header (frame
///      number, y-start, width, height) is emitted; OpenH264 converts and frames inside
///      `encode_stripe_argb` with the same header layout, and encodes a 4:4:4 request 4:2:0 (said
///      once per process). The live CBR budget is recomputed here from the bitrate/fps so it
///      rescales with live changes.
/// 7. **Dispatch**: a single full-frame stripe runs inline (sequential — empirically faster than a
///    one-element rayon job) with one fewer encode thread than the available cores, clamped to
///    `[1, 4]` (x264 with a single-band color conversion; OpenH264 adds four slices and a four-band
///    conversion of its own). The slice threads keep the in-frame encode latency inside the frame
///    budget at high resolutions; the cap is four because `zerolatency` makes x264 slice-threaded
///    and more than four slices trips decode glitches in some Chromium builds, and the minus-one
///    leaves headroom for the capture thread. Multiple stripes instead run across the rayon pool
///    with a single encode thread and one conversion band each, since the parallelism there
///    already comes from encoding the stripes concurrently. Without `cpu_encode_threading` the
///    full frame takes one thread and the stripes are coded one after another
///    (`stripes_at_once`).
#[allow(clippy::too_many_arguments)]
pub fn encode_cpu(
    stripes: &mut Vec<StripeState>,
    carrying: &mut f32,
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
    let codec = settings.codec;
    let scheduled = codec.is_video() && crate::pipeline::periodic_idr_due(settings, frame_counter);
    let n_processing_stripes = stripe_count(height, codec, settings.video_fullframe);

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
    let keys = codec.is_video() && holds && !converges;
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
                    keys,
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
                    keys,
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
    let bit_depth = stripe_bit_depth(settings);
    let video_streaming = settings.video_streaming_mode;
    let jpeg_q = settings.jpeg_quality;
    let paint_q = settings.paint_over_jpeg_quality;
    let target_fps = settings.target_fps;
    let omit_headers = settings.omit_stripe_headers;
    let damage_block_threshold = settings.damage_block_threshold;
    let damage_block_duration = settings.damage_block_duration as i32;
    let video_cbr = settings.video_cbr_mode;
    // The requested rate is a whole-screen budget, and CRF needs no division
    // at all (a per-quality target). OpenH264 sizes its own buffer, so only
    // x264 reads the VBV share.
    #[cfg_attr(not(feature = "gpl"), allow(unused_variables))]
    let (video_bitrate, video_vbv) = stripe_rate_control(settings, *carrying, n_processing_stripes);
    #[cfg(feature = "gpl")]
    let cleanup_vbv = cleanup_vbv_kbit(settings, video_bitrate).max(video_vbv);
    // A full-frame x264 stripe runs the H.264 thread policy (`h264_fullframe_threads`), as a
    // full-frame OpenH264 instance does internally; one stripe of several runs one thread.
    #[cfg(feature = "gpl")]
    let h264_threads = if n_processing_stripes == 1 {
        crate::encoders::session::h264_fullframe_threads(settings)
    } else {
        1
    };
    // A frame of x264's slices, one a thread, that refines nothing.
    #[cfg(feature = "gpl")]
    let empty_bytes = crate::encoders::codec::VIDEO_HEADER_LEN
        + crate::pipeline::EMPTY_SLICE_BYTES * h264_threads as usize;
    #[cfg(not(feature = "gpl"))]
    let empty_bytes = 0;
    #[cfg(feature = "gpl")]
    let csc_bands = 1;
    if video && video_fullcolor && !crate::encoders::software_fullcolor(Codec::H264) {
        static FULLCOLOR_LOGGED: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
        if !FULLCOLOR_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            eprintln!(
                "[software] 4:4:4 full-color requested; OpenH264 is 4:2:0-only, encoding 4:2:0."
            );
        }
    }

    // A still screen reaches every stripe's cleanup on the same frame; spread them over
    // `CLEANUP_STAGGER_FRAMES` frames so no one frame carries the whole screen's. Under a constant
    // rate x264's stripes refining share one budget, so no more refine at once than it feeds at
    // `REFINE_STRIPE_KBPS` a stripe; the rest wait their turn (`refine_slots`).
    let per_frame = stripes.len().div_ceil(CLEANUP_STAGGER_FRAMES).max(1);
    let refine_slots = if converges {
        refine_slots(settings, stripes.len())
    } else {
        stripes.len()
    };
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
                keys,
            ) {
                return true;
            }
            granted += 1;
            granted <= per_frame && refining + granted <= refine_slots
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
            keys,
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
                    let y_size = width_usize * actual_height;
                    let uv_size = if video_fullcolor { y_size } else { y_size / 4 };
                    let y_stride = width_usize as i32;
                    let uv_stride =
                        (if video_fullcolor { width_usize } else { width_usize / 2 }) as i32;
                    let wide = bit_depth > 8;
                    let conversion_result = if wide {
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
                    if enc.encode_with_headers(
                        y,
                        u,
                        v,
                        y_stride * scale,
                        uv_stride * scale,
                        uv_stride * scale,
                        frame_counter,
                        y_start as u16,
                        force_idr,
                        omit_headers,
                        &mut stripe_state.packet_buf,
                    ) {
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
                            let budget = crate::pipeline::frame_budget(video_bitrate, target_fps)
                                * actual_height as f64
                                / height.max(1) as f64;
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
                    stripe_state.h264_encoder = Openh264Encoder::new_stripe(
                        settings,
                        width_usize,
                        actual_height,
                        quality_or_crf,
                        video_bitrate,
                        n_processing_stripes == 1,
                    );
                    if stripe_state.h264_encoder.is_none() {
                        // Once per process: a geometry OpenH264 refuses (wider than
                        // 3840, say) would otherwise log on every stripe of every frame.
                        static INIT_FAILED_LOGGED: std::sync::atomic::AtomicBool =
                            std::sync::atomic::AtomicBool::new(false);
                        if !INIT_FAILED_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                            eprintln!(
                                "[software] OpenH264 init failed for a {}x{} stripe; no software H.264 for it",
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

                let coded = stripe_state.h264_encoder.as_mut()?.encode_stripe_argb(
                    stripe_bytes,
                    width_usize * 4,
                    frame_counter as u64,
                    y_start as u16,
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
                        eprintln!("[software] OpenH264 encode failed for stripe at y={y_start}: {e}");
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
    let encoded: Vec<EncodedStripe> = if !stripes_at_once(settings, n_processing_stripes) {
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
    };
    // Follow motion spreading out quickly and narrowing slowly: the budget is
    // better spent late than overshot the moment a screen goes still again.
    let sent = encoded.len() as f32;
    let alpha = if sent > *carrying {
        CARRY_RISE
    } else {
        CARRY_FALL
    };
    *carrying += (sent - *carrying) * alpha;
    encoded
}

/// Whether a frame's `n` stripes are coded at once across the rayon pool, or one after another on
/// the encode thread: one stripe, or a session without `cpu_encode_threading`, takes no other.
fn stripes_at_once(settings: &CaptureSettings, n: usize) -> bool {
    n > 1 && settings.cpu_encode_threading
}

/// How many horizontal stripes a frame of `height` is split into, which is the choice of how
/// much encode parallelism to spend on it.
///
/// A full-frame session is one contiguous stream and so a single stripe; otherwise the frame
/// fans out across cores, held to `MAX_STRIPES` and bounded so no stripe is shorter than a
/// macroblock row. Both the encoder and the settings line report from here, so what is logged is
/// what is encoded.
/// Leave frame `frame_id` and every frame after it out of every stripe's predictions. False
/// when an encoder cannot, and the caller codes a key frame instead.
pub fn invalidate_reference(stripes: &mut [StripeState], frame_id: u16) -> bool {
    cfg_if::cfg_if! {
        if #[cfg(feature = "gpl")] {
            // Every stripe is its own stream, so each one is told; a refusal from any of
            // them still leaves the rest to forget the frame.
            let mut forgotten = true;
            for enc in stripes.iter_mut().filter_map(|s| s.h264_encoder.as_mut()) {
                forgotten &= enc.invalidate_reference(frame_id);
            }
            forgotten
        } else {
            // OpenH264 predicts past a loss only from a long-term reference every consumer
            // holds, which only a full-frame instance keeps; a stripe of several codes a key
            // frame.
            let mut forgotten = true;
            for enc in stripes.iter_mut().filter_map(|s| s.h264_encoder.as_mut()) {
                forgotten &= enc.invalidate_reference(frame_id);
            }
            forgotten
        }
    }
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

/// Most stripes a frame is cut into, whatever the encoder host's core count.
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

pub fn stripe_count(height: i32, codec: Codec, fullframe: bool) -> usize {
    if !codec.stripes() || (codec.is_video() && fullframe) || height < MIN_STRIPE_HEIGHT {
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

/// Frames of the target a constant-rate x264 stripe is given as its buffer while its cleanup
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

/// The buffer, in kbit, of a stripe at `bitrate_kbps` while it is cleaned up
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

/// How many stripes refine a still screen through their rate control at once: x264's, as many as
/// the session's rate gives `REFINE_STRIPE_KBPS` each, one at least; OpenH264's, all of them,
/// since held to that count a GPL-off build left the page at 25 and 19 dB at 8 and 60 fps
/// 0.5 Mbit/s, against 42 with every stripe refining.
fn refine_slots(settings: &CaptureSettings, n_stripes: usize) -> usize {
    let all = n_stripes.max(1);
    if !cfg!(feature = "gpl") {
        return all;
    }
    ((settings.video_bitrate_kbps / REFINE_STRIPE_KBPS).max(1) as usize).min(all)
}

/// Split the configured rate budget across the stripes carrying it, returning the
/// `(bitrate_kbps, vbv_kbit)` each stripe's encoder is programmed with: the CBR target, or the
/// peak of a CRF a positive `video_vbv_multiplier` caps. An uncapped CRF gets a buffer of 0.
///
/// Every stripe runs its own encoder and rate control is per instance, metered against the
/// declared frame rate rather than against the frames that stripe was actually sent. So the
/// screen's rate is one stripe's rate times the number of stripes that carry motion, and the
/// budget is divided by that number — not by the stripe count, which on a screen where one
/// corner moves would spend a fraction of what was configured. The divisor is the smoothed
/// count so it changes on the scale of a moving average and not every frame: a rate that
/// swings frame to frame leaves the encoder chasing it and delivers less than either rate would.
fn stripe_rate_control(settings: &CaptureSettings, carrying: f32, n_stripes: usize) -> (i32, i32) {
    let divisor = (carrying.round().max(1.0) as usize).min(n_stripes.max(1)) as i32;
    let bitrate = (settings.video_bitrate_kbps / divisor).max(1);
    if !settings.video_cbr_mode && settings.video_vbv_multiplier <= 0.0 {
        return (bitrate, 0);
    }
    let vbv = (crate::encoders::vbv_bits(
        (bitrate as u32).saturating_mul(1000),
        settings.target_fps,
        settings.keyframe_interval_s,
        settings.video_vbv_multiplier,
    ) / 1000)
        .max(1) as i32;
    (bitrate, vbv)
}

/// Divide `height` into `n` contiguous stripes as `(y_start, stripe_height)`, with the split
/// rule differing by codec because only H.264 constrains stripe height.
///
/// - **JPEG**: JPEG has no vertical subsampling, so stripes may be any height; the heights differ
///   by at most one row — the first `remainder` stripes take one extra each — and every row of
///   the frame is covered.
/// - **Video**: 4:2:0 pairs chroma rows vertically, so every stripe height is forced even and the
///   remainder is handed out two rows at a time. The deliberate cost is that a single trailing
///   odd row may be left uncovered — preferable to an odd-height stripe the encoder cannot
///   represent.
fn compute_stripe_geometries(height: usize, n: usize, codec: Codec) -> Vec<(usize, usize)> {
    let mut geoms = Vec::with_capacity(n);
    let mut current_y = 0;
    if !codec.is_video() {
        let base_h = height / n;
        let remainder = height - base_h * n;
        for i in 0..n {
            let s_h = base_h + if i < remainder { 1 } else { 0 };
            geoms.push((current_y, s_h));
            current_y += s_h;
        }
    } else {
        let base_h = (height / n) & !1;
        let remainder = height - base_h * n;
        let stripes_with_extra = remainder / 2;
        for i in 0..n {
            let s_h = base_h + if i < stripes_with_extra { 2 } else { 0 };
            geoms.push((current_y, s_h));
            current_y += s_h;
        }
    }
    geoms
}

#[cfg(test)]
mod stripe_count_tests {
    use super::{Codec, MAX_STRIPES, MIN_STRIPE_HEIGHT, stripe_count};

    /// The count is what the client decodes, so a host with many cores cannot raise it: a 4K
    /// frame has room for thirty-three stripes at the minimum height and still gets at most
    /// `MAX_STRIPES`, on the striped video codec as much as on JPEG.
    #[test]
    fn a_tall_frame_is_not_cut_into_one_stripe_per_core() {
        for codec in [Codec::Jpeg, Codec::H264] {
            let n = stripe_count(2160, codec, false);
            assert!(
                n <= MAX_STRIPES,
                "{codec:?} cut a 4K frame into {n} stripes"
            );
            assert!(n >= 1, "{codec:?} cut a 4K frame into none");
        }
    }

    /// A stripe still never falls below the minimum height, so a short frame is cut by its own
    /// height rather than by the ceiling.
    #[test]
    fn a_short_frame_is_cut_by_its_height() {
        assert_eq!(stripe_count(MIN_STRIPE_HEIGHT - 1, Codec::Jpeg, false), 1);
        assert!(stripe_count(MIN_STRIPE_HEIGHT * 2, Codec::Jpeg, false) <= 2);
    }

    /// A full-frame video session carries one picture, and a codec that does not stripe never
    /// gets more than one whatever the height.
    #[test]
    fn a_whole_frame_is_one_stripe() {
        assert_eq!(stripe_count(2160, Codec::H264, true), 1);
        assert_eq!(stripe_count(2160, Codec::Av1, false), 1);
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

    /// The configured bitrate is a budget for the screen, not for each stripe: every stripe
    /// runs its own rate control, so what reaches an encoder is the budget over the number of
    /// stripes carrying motion. Dividing by the stripe count instead spends a fraction of the
    /// configured rate whenever only part of the screen moves, and dividing by nothing at all
    /// spends a multiple of it whenever the whole screen does.
    #[test]
    fn cbr_budget_is_split_across_the_stripes_carrying_it() {
        use crate::CaptureSettings;
        for &kbps in &[500i32, 4000, 8000, 20000] {
            let settings = CaptureSettings {
                video_cbr_mode: true,
                video_bitrate_kbps: kbps,
                ..Default::default()
            };
            for &n in &[1usize, 2, 4, 12, 64] {
                let (all, _) = super::stripe_rate_control(&settings, n as f32, n);
                let total = all * n as i32;
                assert!(
                    total <= kbps && kbps - total < n as i32,
                    "{n} stripes at {all} kbps must sum to the configured {kbps}"
                );
                let (one, _) = super::stripe_rate_control(&settings, 1.0, n);
                assert_eq!(one, kbps, "a lone moving stripe carries the whole budget");
                let (over, _) = super::stripe_rate_control(&settings, n as f32 * 4.0, n);
                assert_eq!(
                    over, all,
                    "the divisor never exceeds the stripes that exist"
                );
                let (under, _) = super::stripe_rate_control(&settings, 0.0, n);
                assert_eq!(under, kbps, "and never falls below one");
            }
            let (vbv_one, whole) = super::stripe_rate_control(&settings, 1.0, 8);
            let (_, share) = super::stripe_rate_control(&settings, 8.0, 8);
            assert_eq!(vbv_one, kbps);
            assert!(
                (share * 8 - whole).abs() <= 9,
                "each stripe's buffer is its share of the whole-screen one: {share}x8 vs {whole}"
            );
        }
    }

    /// x264's stripes clean up through the rate control as many at a time as the session's rate
    /// gives `REFINE_STRIPE_KBPS` each, one at least and never more than there are: two of eight
    /// at 0.5 Mbit/s, all eight from 2 Mbit/s. OpenH264's all clean up at once.
    #[test]
    fn stripes_refine_as_many_at_once_as_the_rate_feeds() {
        use crate::CaptureSettings;
        let slots = |kbps| {
            super::refine_slots(
                &CaptureSettings {
                    video_cbr_mode: true,
                    video_bitrate_kbps: kbps,
                    ..Default::default()
                },
                8,
            )
        };
        if cfg!(feature = "gpl") {
            assert_eq!(slots(100), 1);
            assert_eq!(slots(500), 2);
            assert_eq!(slots(1000), 4);
        } else {
            assert_eq!(slots(100), 8);
            assert_eq!(slots(500), 8);
        }
        assert_eq!(slots(2000), 8);
        assert_eq!(slots(20000), 8);
    }

    /// A CRF stripe is capped only where a VBV multiplier asks for it, with the whole-screen
    /// buffer that multiplier names at the configured rate as its peak; CBR always has one.
    #[test]
    fn crf_stripes_take_a_vbv_only_where_a_multiplier_asks() {
        use crate::CaptureSettings;
        let crf = |mult| CaptureSettings {
            video_bitrate_kbps: 8000,
            video_vbv_multiplier: mult,
            target_fps: 30.0,
            ..Default::default()
        };
        assert_eq!(super::stripe_rate_control(&crf(0.0), 1.0, 4), (8000, 0));
        assert_eq!(super::stripe_rate_control(&crf(12.0), 1.0, 4), (8000, 3200));
        assert_eq!(super::stripe_rate_control(&crf(12.0), 4.0, 4), (2000, 800));
        let cbr = CaptureSettings {
            video_cbr_mode: true,
            ..crf(0.0)
        };
        assert!(super::stripe_rate_control(&cbr, 1.0, 4).1 > 0);
    }

    /// The divisor follows the screen rather than the configuration: full-screen motion moves
    /// it to the stripe count within a few frames, and it comes back down when the motion
    /// stops. A divisor recomputed per frame would swing between those two ends every frame,
    /// which leaves the encoder chasing a square wave and delivering less than either rate.
    #[test]
    fn the_budget_divisor_follows_motion_and_is_smoothed() {
        use crate::CaptureSettings;
        let (w, h) = (64, 512);
        let settings = CaptureSettings {
            width: w,
            height: h,
            codec: Codec::Jpeg,
            jpeg_quality: 40,
            use_paint_over_quality: false,
            ..Default::default()
        };
        let full = [smithay::utils::Rectangle::new((0, 0).into(), (w, h).into())];
        let stripes_n = super::stripe_count(h, settings.codec, settings.video_fullframe);
        if stripes_n < 2 {
            return;
        }
        let mut stripes = Vec::new();
        let mut carrying = 1.0f32;
        for frame in 0..40u16 {
            let shade = 40u8.wrapping_add(frame.wrapping_mul(7) as u8);
            let px = vec![shade; (w * h * 4) as usize];
            super::encode_cpu(
                &mut stripes,
                &mut carrying,
                &px,
                w,
                h,
                &full,
                &settings,
                frame,
                false,
                false,
                false,
            );
        }
        assert!(
            carrying > stripes_n as f32 * 0.75,
            "full-screen motion must move the divisor toward the {stripes_n} stripes it uses, \
             not leave it at {carrying}"
        );
        let moved = carrying;
        // Motion that narrows to one corner narrows the divisor with it, so the budget
        // follows the stripes that are actually spending it. A frame with no motion at all
        // encodes nothing and carries nothing, so it leaves the divisor where it was.
        let band = [smithay::utils::Rectangle::new(
            (0, 0).into(),
            (w, 64).into(),
        )];
        for frame in 40..120u16 {
            let shade = 40u8.wrapping_add(frame.wrapping_mul(11) as u8);
            let mut px = vec![200u8; (w * h * 4) as usize];
            for byte in px.iter_mut().take((w * 64 * 4) as usize) {
                *byte = shade;
            }
            super::encode_cpu(
                &mut stripes,
                &mut carrying,
                &px,
                w,
                h,
                &band,
                &settings,
                frame,
                false,
                false,
                false,
            );
        }
        assert!(
            carrying < moved * 0.5,
            "motion in one stripe must bring the divisor back down: {carrying} vs {moved}"
        );
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
            video_fullframe: true,
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
        let (mut stripes, mut carrying) = (Vec::new(), 1.0f32);
        let mut dec = Decoder::new().expect("decoder");
        let (mut sent, mut skipped_before_last, mut last) = (0usize, false, None);
        for i in 0..900u16 {
            let px = if i < 10 { &flat } else { &page };
            let out = super::encode_cpu(
                &mut stripes,
                &mut carrying,
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

    /// A scheduled key frame waits, stripe by stripe, for the stripe to change: a still screen
    /// sends nothing through its intervals, and the first change after one keys the stripes it
    /// touches and no others, once.
    #[test]
    fn a_stripes_scheduled_key_frame_waits_for_its_change() {
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
        let n = super::stripe_count(h, settings.codec, settings.video_fullframe);
        if n < 2 {
            return;
        }
        let mut stripes = Vec::new();
        let mut carrying = 1.0f32;
        let px: Vec<u8> = (0..(w * h * 4) as usize).map(|i| (i % 251) as u8).collect();
        let mut encode = |pixels: &[u8], frame: u16| {
            super::encode_cpu(
                &mut stripes,
                &mut carrying,
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
        assert_eq!(
            encode(&px, 1).len(),
            n,
            "every stripe is sent on the first frame"
        );
        assert!(
            (2..200).all(|frame| encode(&px, frame).is_empty()),
            "a still screen sends nothing through its intervals"
        );
        let mut moved = px.clone();
        for b in moved.iter_mut().take((w * 8 * 4) as usize) {
            *b = b.wrapping_add(97);
        }
        let key = encode(&moved, 200);
        assert_eq!(key.len(), 1, "the change sends its stripe alone");
        assert_eq!(key[0].stripe_y_start, 0);
        assert_eq!(key[0].data[1], 0x11, "as the key frame owed");
        for b in moved.iter_mut().take((w * 8 * 4) as usize) {
            *b = b.wrapping_add(31);
        }
        let next = encode(&moved, 201);
        assert_eq!(next.len(), 1);
        assert_ne!(next[0].data[1], 0x11, "once");
    }

    /// Without `gpl` the striped H.264 path runs one OpenH264 instance per stripe and speaks
    /// the x264 stripes' protocol: the first frame emits every stripe as an IDR whose wire header
    /// carries that stripe's y-start and geometry, each stripe is an independently decodable
    /// stream (a decoder fed only that stripe's bytes yields a picture of the stripe's size), a
    /// static follow-up frame sends nothing, and motion confined to the top rows re-sends only
    /// the top stripe, as a delta frame.
    #[cfg(not(feature = "gpl"))]
    #[test]
    fn openh264_stripes_are_independent_streams() {
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
        let n = super::stripe_count(h, settings.codec, settings.video_fullframe);
        if n < 2 {
            return;
        }
        let mut stripes = Vec::new();
        let mut carrying = 1.0f32;
        let px: Vec<u8> = (0..(w * h * 4) as usize).map(|i| (i % 251) as u8).collect();
        let first = super::encode_cpu(
            &mut stripes,
            &mut carrying,
            &px,
            w,
            h,
            &[],
            &settings,
            0,
            false,
            true,
            false,
        );
        assert_eq!(first.len(), n, "every stripe is sent on the first frame");
        for (stripe, (y, sh)) in
            first
                .iter()
                .zip(compute_stripe_geometries(h as usize, n, Codec::H264))
        {
            let d = &stripe.data;
            assert_eq!(d[0], 0x04, "H.264 stripe tag");
            assert_eq!(d[1], 0x11, "first frame of a stripe is an H.264 key frame");
            assert_eq!(u16::from_be_bytes([d[2], d[3]]), 0, "frame number");
            assert_eq!(u16::from_be_bytes([d[4], d[5]]) as usize, y, "y-start");
            assert_eq!(u16::from_be_bytes([d[6], d[7]]) as i32, w, "width");
            assert_eq!(
                u16::from_be_bytes([d[8], d[9]]) as usize,
                sh,
                "stripe height"
            );
            assert_eq!(
                (
                    stripe.stripe_y_start as usize,
                    stripe.stripe_height as usize
                ),
                (y, sh)
            );
            let mut dec = Decoder::new().expect("decoder");
            let img = dec
                .decode(&d[crate::encoders::codec::VIDEO_HEADER_LEN..])
                .expect("decode")
                .expect("an IDR decodes on its own");
            assert_eq!(
                img.dimensions(),
                (w as usize, sh),
                "each stripe is its own stream"
            );
        }
        let quiet = super::encode_cpu(
            &mut stripes,
            &mut carrying,
            &px,
            w,
            h,
            &[],
            &settings,
            1,
            false,
            true,
            false,
        );
        assert!(quiet.is_empty(), "a static frame sends nothing");
        let mut moved = px.clone();
        for b in moved.iter_mut().take((w * 8 * 4) as usize) {
            *b = b.wrapping_add(97);
        }
        let top = super::encode_cpu(
            &mut stripes,
            &mut carrying,
            &moved,
            w,
            h,
            &[],
            &settings,
            2,
            false,
            true,
            false,
        );
        assert_eq!(
            top.len(),
            1,
            "motion in the top rows re-sends the top stripe alone"
        );
        assert_eq!(top[0].stripe_y_start, 0);
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
        let mut carrying = 1.0f32;
        let full = [smithay::utils::Rectangle::new((0, 0).into(), (w, h).into())];
        let dirty = super::encode_cpu(
            &mut stripes,
            &mut carrying,
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
                &mut carrying,
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
        let mut carrying = 1.0f32;
        let first = super::encode_cpu(
            &mut stripes,
            &mut carrying,
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
                &mut carrying,
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
            &mut carrying,
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
        let mut carrying = 1.0f32;
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
                &mut carrying,
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

    /// H.264 geometry yields even, contiguous stripe heights that cover the whole frame
    /// except at most one trailing odd row, across a range of heights and stripe counts.
    #[test]
    fn h264_stripes_even_and_within_bounds() {
        for &h in &[64usize, 720, 721, 1080, 1081] {
            for &n in &[1usize, 2, 8] {
                let g = compute_stripe_geometries(h, n, Codec::H264);
                assert_eq!(g.len(), n);
                for &(_, sh) in &g {
                    assert_eq!(
                        sh % 2,
                        0,
                        "H.264 stripe heights must be even h={} n={}",
                        h,
                        n
                    );
                }
                assert_contiguous(&g);
                assert!(covered(&g) <= h);
                assert!(
                    h - covered(&g) <= 1,
                    "at most one trailing odd row uncovered"
                );
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
                    0,
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

    /// Encode `FRAMES` scrolling-text luma frames through the x264 stripe encoder at the
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
                    0,
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
                &y, &u, &v, w as i32, cw as i32, cw as i32, 0, 0, true, true, &mut out
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
                let mut param: x264_sys::x264_param_t = std::mem::zeroed();
                x264_sys::x264_encoder_parameters(enc.encoder, &mut param);
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
    /// and a constant rate: the full-frame session is one stripe of the same machinery.
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
                0,
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
                0,
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
                0,
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
                0,
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
                0,
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
            0,
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
                0,
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
                    0,
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
