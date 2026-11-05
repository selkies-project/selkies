/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Software VP8 and VP9 through libvpx, with every frame's references under the session's
//! control.
//!
//! One session type serves both codecs. libvpx runs at the lowest latency it offers in real
//! time -- one frame in, one packet out, no lag, no alternate-reference lookahead -- with screen
//! tuning on VP9, and its rate control is either constant-rate at the session's bitrate and VBV
//! or a quantizer pinned between an equal minimum and maximum, which is how a quality change
//! reaches a running session without a key frame. A VP9 session runs in libvpx's flexible mode,
//! where the codec's eight buffers are addressed by slot, so a frame a client lost leaves the
//! predictions the way a decoded picture buffer allows; a VP8 session steers its three buffers
//! with the per-frame reference and refresh flags. What a lost frame would leave behind besides
//! its picture is kept out too: VP8's entropy probabilities persist across frames unless a
//! frame declines to update them, so no frame does, and a VP9 decoder takes the previous
//! frame's motion vectors and probability contexts whatever the references say, which only the
//! codec's error-resilient mode switches off. VP8 runs error-resilient too, since its
//! constant-rate control otherwise refreshes GOLDEN on a schedule of its own, whatever a frame's
//! flags say.

use std::ffi::{CStr, c_int, c_long, c_void};
use std::ptr;

use codec_sys::vpx::*;

use super::codec::{
    Codec, FRAME_ANCHOR, VIDEO_HEADER_LEN, frame_type_from_key, push_video_header, vp8_is_key,
    vp9_is_key, vpx_level,
};
use super::reference::{Reference, ReferenceSlots, SlotPlan, SlotRefresh};
use super::session::{Pending, Planes, Quality, RateSettings, encode_threads};
use crate::CaptureSettings;

/// The rate target, in kbit/s, a pinned-quantizer session names and never reaches.
const RATE_CEILING_KBPS: u32 = 100_000;

/// The temporal layers a VP9 session declares. Every frame is coded in the base layer; the
/// second exists because libvpx sets up and re-targets its per-layer rate control, which the
/// flexible reference mode runs on, only for a session with more than one layer.
const VP9_LAYERS: u32 = 2;
/// The libvpx quantizer levels a session leaves to the library's own bounds.
const VP8_DEFAULT_MIN_LEVEL: u32 = 4;
/// libvpx's `aq_mode` for the cyclic refresh.
const CYCLIC_REFRESH_AQ: c_int = 3;
/// The squared error under which libvpx's VP8 skips a macroblock as unchanged (its static
/// threshold), so a still region is left as it is rather than coded again every frame: at 0.25
/// Mbit/s a 1080p texture under a moving box took 3.0 MB for its 3 s of motion against 5.8, at
/// 42.0 dB against 41.4, and 304 kB still against 1185. Scrolling text codes the same.
const VP8_STATIC_THRESHOLD: c_int = 100;
/// VP8's token partitions, eight, the most it has: a client decoding in software spreads a frame
/// across them, as across H.264's slices, where one leaves it on one thread. They cost nothing
/// measurable: at 1080p text, a game frame and dense text code within 0.1% of the bytes and
/// 0.02 dB of one partition while scrolling, and within 2% still.
const VP8_TOKEN_PARTITIONS: c_int = VP8_EIGHT_TOKENPARTITION as c_int;
/// The frame budget a macroblock, in bits, from which VP8 sweeps its refresh in bands
/// (`VpxEncoder::band_size`): three times what a frame that skips every macroblock costs.
const BAND_BITS_PER_MB: f64 = 1.0;
/// The finest VP8 quantizer indices, at and under which a held band is coded with no loop filter
/// (`band_roi`), as libvpx itself filters a frame there at level 1 at most. libvpx starts a
/// frame's level from the last frame's and keeps it while a strip across the middle of the
/// picture shows no gain, which a band elsewhere leaves as it was: the bands above the middle of
/// a sweep after motion kept the level its coarse frames left (46), and 1080p text swept at
/// index 2 ended at 42.7 dB at 8 fps 2 Mbit/s against 50.6 unfiltered, at 15 fps 43.6 against
/// 50.6, at 30 and 60 fps level. At coarser indices libvpx's level stands, unfiltered coming out
/// 0.2 to 0.4 dB short at 24 and 73. A refresh held whole shows the strip, and libvpx's level
/// came out 0.3 to 0.8 dB finer than none at index 3.
const VP8_UNFILTERED_QINDEX: u32 = 16;

/// Quality index steps finer than the paint-over one VP8 codes a refined block at
/// (`FrameEncoder::refines`): none, since a VP8 session's paint-over quantizer already sits at
/// the floor its bounds keep (`VP8_DEFAULT_MIN_LEVEL`), and 1080p text beside a video came out
/// the same at 0, 3, and 6 steps finer (39.7 dB at 2 Mbit/s and 42.6 at 8 against 29.6 and 33.5
/// unrefined, where VP8's cleanup of the still screen reaches 42.0). VP9 refines no block: in
/// its screen mode a block whose source has not changed reads as static and is skipped
/// whatever its segment's quantizer, and refined, the same text came to 25.9 and 35.4 dB
/// against 25.6 and 34.9, where VP9's cleanup of the still screen reaches 37.7 and 51.1.
pub const REFINE_FINER: u32 = 0;

/// Whether the loaded libvpx codes VP9 4:4:4 in the flexible mode the sessions run: before 1.13
/// its layer machinery re-sizes every frame at 4:2:0, so a profile 1 session writes headers
/// over pictures no decoder takes.
pub fn encodes_444() -> bool {
    (unsafe { vpx_codec_version() }) >= (1 << 16) | (13 << 8)
}

/// Whether the loaded libvpx was built with VP9's high bit depth, which its 10-bit profiles
/// need.
pub fn encodes_ten_bit() -> bool {
    (unsafe { vpx_codec_get_caps(vpx_codec_vp9_cx()) })
        & VPX_CODEC_CAP_HIGHBITDEPTH as vpx_codec_caps_t
        != 0
}

/// One libvpx session for one capture.
pub struct VpxEncoder {
    codec: Codec,
    ctx: vpx_codec_ctx_t,
    cfg: vpx_codec_enc_cfg_t,
    planes: Planes,
    threads: u32,
    quality: Quality,
    rate: RateSettings,
    omit_headers: bool,
    references: ReferenceSlots,
    last_reference: Reference,
    pending: Pending,
    /// The slot plan of the frame being encoded, recorded once its packet returns.
    plan: SlotPlan,
    /// The quality index the next frame is held at whatever the rate control (`hold_quantizer`).
    held: Option<u32>,
    /// The share of the picture, from and to in raster order, the held quantizer covers, the
    /// rest left as it is (`hold_quantizer`, VP8 only); `None` for the whole picture.
    held_band: Option<(f64, f64)>,
    /// The bytes of the last held frame, 0 before one (`band_size`).
    held_bytes: usize,
    /// One segment per macroblock for a held band (`band_roi`): 1 inside it, 0 outside.
    band_map: Vec<u8>,
    /// The blocks the next frame of the rate control's refines (`refine_blocks`).
    refinement: Option<crate::ledger::Refinement>,
    /// The quality index the rate control last coded a frame at, held frames aside.
    last_quality: Option<u32>,
    /// The bytes of the last frame the rate control coded, held frames aside.
    last_bytes: Option<usize>,
    /// The size and quality index of the last held key frame of a constant-rate session
    /// (`super::held_key_start`).
    held_key: Option<(usize, u32)>,
    /// The bytes the next frame, a still screen's cleanup, may come to (`cap`, VP9).
    cap: Option<usize>,
    /// Whether the rate control runs at a cleanup frame's rate, to be put back.
    raised: bool,
}

unsafe impl Send for VpxEncoder {}

impl Drop for VpxEncoder {
    fn drop(&mut self) {
        unsafe { vpx_codec_destroy(&mut self.ctx) };
    }
}

/// The session's last error, with libvpx's detail where it has one. The context is handed
/// over mutable because libvpx before 1.14 declares the parameter so, though it only reads it.
fn error(ctx: &vpx_codec_ctx_t, what: &str) -> String {
    unsafe {
        let detail = vpx_codec_error_detail(ptr::from_ref(ctx).cast_mut());
        let text = CStr::from_ptr(vpx_codec_err_to_string(ctx.err)).to_string_lossy();
        if detail.is_null() {
            format!("{what}: {text}")
        } else {
            format!(
                "{what}: {text} ({})",
                CStr::from_ptr(detail).to_string_lossy()
            )
        }
    }
}

impl VpxEncoder {
    /// Open a VP8 or VP9 session on host frames in the byte order `rgba` names.
    pub fn new(settings: &CaptureSettings, codec: Codec, rgba: bool) -> Result<Self, String> {
        if !matches!(codec, Codec::Vp8 | Codec::Vp9) {
            return Err(format!("libvpx encodes no {}", codec.display()));
        }
        let _ = rgba;
        let fullcolor = codec == Codec::Vp9 && settings.video_fullcolor && encodes_444();
        let ten_bit = codec == Codec::Vp9 && settings.video_bit_depth >= 10 && encodes_ten_bit();
        let threads = encode_threads(settings) as u32;
        let rate = RateSettings::new(settings);
        let quality = Quality::new(codec.quantizer(settings.video_crf));
        let iface = unsafe {
            if codec == Codec::Vp8 {
                vpx_codec_vp8_cx()
            } else {
                vpx_codec_vp9_cx()
            }
        };
        let mut cfg: vpx_codec_enc_cfg_t = unsafe { std::mem::zeroed() };
        if unsafe { vpx_codec_enc_config_default(iface, &mut cfg, 0) } != VPX_CODEC_OK {
            return Err("libvpx refused its default configuration".into());
        }
        cfg.g_w = settings.width.max(1) as u32;
        cfg.g_h = settings.height.max(1) as u32;
        cfg.g_timebase = vpx_rational {
            num: rate.fps.den as i32,
            den: rate.fps.num as i32,
        };
        cfg.g_threads = threads.min(64);
        cfg.g_lag_in_frames = 0;
        cfg.g_pass = VPX_RC_ONE_PASS;
        cfg.g_error_resilient = VPX_ERROR_RESILIENT_DEFAULT;
        cfg.g_profile = fullcolor as u32 + 2 * ten_bit as u32;
        cfg.g_bit_depth = if ten_bit { VPX_BITS_10 } else { VPX_BITS_8 };
        cfg.g_input_bit_depth = if ten_bit { 10 } else { 8 };
        cfg.rc_dropframe_thresh = 0;
        cfg.rc_2pass_vbr_bias_pct = 50;
        cfg.rc_2pass_vbr_minsection_pct = 100;
        cfg.rc_2pass_vbr_maxsection_pct = 100;
        cfg.kf_max_dist = c_int::MAX as u32;
        if codec == Codec::Vp9 {
            cfg.ss_number_layers = 1;
            cfg.ts_number_layers = VP9_LAYERS;
            cfg.ts_rate_decimator[..VP9_LAYERS as usize].fill(1);
            cfg.ts_periodicity = 1;
            cfg.temporal_layering_mode = VP9E_TEMPORAL_LAYERING_MODE_BYPASS as c_int;
        }
        let mut me = Self {
            codec,
            ctx: unsafe { std::mem::zeroed() },
            cfg,
            planes: Planes::new(
                settings.width.max(1) as usize,
                settings.height.max(1) as usize,
                fullcolor,
                if ten_bit { 10 } else { 8 },
            ),
            threads,
            quality,
            rate,
            omit_headers: settings.omit_stripe_headers,
            references: if codec == Codec::Vp9 {
                ReferenceSlots::vp9()
            } else {
                ReferenceSlots::new()
            },
            last_reference: Reference::Untracked,
            pending: Pending::default(),
            plan: SlotPlan::KEY,
            held: None,
            held_band: None,
            held_bytes: 0,
            band_map: Vec::new(),
            refinement: None,
            last_quality: None,
            last_bytes: None,
            cap: None,
            raised: false,
            held_key: None,
        };
        let q = me.quality.current;
        me.program_rate(rate, q);
        let res = unsafe {
            vpx_codec_enc_init_ver(
                &mut me.ctx,
                iface,
                &me.cfg,
                if ten_bit {
                    VPX_CODEC_USE_HIGHBITDEPTH as vpx_codec_flags_t
                } else {
                    0
                },
                VPX_ENCODER_ABI_VERSION as c_int,
            )
        };
        if res != VPX_CODEC_OK {
            return Err(error(&me.ctx, "libvpx refused the session"));
        }
        me.control(VP8E_SET_CPUUSED, if codec == Codec::Vp9 { 8 } else { 16 })?;
        me.control(VP8E_SET_ENABLEAUTOALTREF, 0)?;
        me.control(
            VP8E_SET_STATIC_THRESHOLD,
            if codec == Codec::Vp8 {
                VP8_STATIC_THRESHOLD
            } else {
                0
            },
        )?;
        me.control(VP8E_SET_MAX_INTRA_BITRATE_PCT, 0)?;
        if codec == Codec::Vp8 {
            me.control(VP8E_SET_NOISE_SENSITIVITY, 0)?;
            me.control(VP8E_SET_TOKEN_PARTITIONS, VP8_TOKEN_PARTITIONS)?;
        } else {
            // Column threading is per tile and VP9's narrowest tile is 256 pixels, so the
            // width sets how many columns the encode can spread across.
            let tile_columns = (settings.width / 256).max(1).ilog2().min(6);
            me.control(VP9E_SET_TILE_COLUMNS, tile_columns as c_int)?;
            me.control(VP9E_SET_FRAME_PARALLEL_DECODING, 0)?;
            me.control(VP9E_SET_COLOR_SPACE, VPX_CS_BT_709 as c_int)?;
            me.control(VP9E_SET_COLOR_RANGE, VPX_CR_STUDIO_RANGE as c_int)?;
            me.control(VP9E_SET_TARGET_LEVEL, 255)?;
            me.control(VP9E_SET_ROW_MT, 1)?;
            me.control(VP9E_SET_TUNE_CONTENT, VP9E_CONTENT_SCREEN as c_int)?;
            me.control(VP9E_SET_SVC, 1)?;
            me.program_layer()?;
            me.program_refresh()?;
        }
        Ok(me)
    }

    fn control(&mut self, id: vp8e_enc_control_id, value: c_int) -> Result<(), String> {
        if unsafe { vpx_codec_control_(&mut self.ctx, id as c_int, value) } != VPX_CODEC_OK {
            return Err(error(&self.ctx, &format!("libvpx refused control {id}")));
        }
        Ok(())
    }

    /// A constant-rate VP9 session refines a still screen through libvpx's cyclic refresh, a
    /// share of the blocks a frame within the budget. Without it the rate control codes the
    /// whole screen finer in single frames: at 2 Mbit/s at 1080p up to 44 budgets each, a
    /// third over the rate for 30 s, for a picture 4.6 dB coarser. A pinned quantizer takes
    /// none, since the refresh codes its blocks at another.
    fn program_refresh(&mut self) -> Result<(), String> {
        if self.codec != Codec::Vp9 {
            return Ok(());
        }
        let mode = if self.rate.cbr { CYCLIC_REFRESH_AQ } else { 0 };
        self.control(VP9E_SET_AQ_MODE, mode)
    }

    /// The layers of a VP9 session, whose quantizer bounds libvpx reads from here rather
    /// than from the configuration once the flexible mode is on.
    fn program_layer(&mut self) -> Result<(), String> {
        let mut layer: vpx_svc_parameters = unsafe { std::mem::zeroed() };
        for i in 0..VP9_LAYERS as usize {
            layer.max_quantizers[i] = self.cfg.rc_max_quantizer as c_int;
            layer.min_quantizers[i] = self.cfg.rc_min_quantizer as c_int;
            layer.speed_per_layer[i] = 8;
        }
        layer.scaling_factor_num[0] = 1;
        layer.scaling_factor_den[0] = 1;
        layer.temporal_layering_mode = VP9E_TEMPORAL_LAYERING_MODE_BYPASS as c_int;
        let res = unsafe {
            vpx_codec_control_(
                &mut self.ctx,
                VP9E_SET_SVC_PARAMETERS as c_int,
                &mut layer as *mut vpx_svc_parameters as *mut c_void,
            )
        };
        if res != VPX_CODEC_OK {
            return Err(error(&self.ctx, "libvpx refused the layer parameters"));
        }
        Ok(())
    }

    /// Write `rate` and the quantizer `q` (the codec's own domain) into the configuration:
    /// a constant rate holds the target and VBV with the quantizer bounds the settings name,
    /// a pinned quantizer names a rate it never reaches with both bounds at the quantizer.
    fn program_rate(&mut self, rate: RateSettings, q: u32) {
        let cfg = &mut self.cfg;
        cfg.g_timebase = vpx_rational {
            num: rate.fps.den as i32,
            den: rate.fps.num as i32,
        };
        cfg.rc_end_usage = VPX_CBR;
        if rate.cbr {
            let kbps = (rate.bps() / 1000).clamp(1, u32::MAX as u64) as u32;
            cfg.rc_target_bitrate = kbps;
            let (lo, hi) = (
                self.codec.quantizer_bound(rate.min_qp),
                self.codec.quantizer_bound(rate.max_qp),
            );
            cfg.rc_min_quantizer = if lo > 0 {
                vpx_level(self.codec, lo)
            } else if self.codec == Codec::Vp8 {
                VP8_DEFAULT_MIN_LEVEL
            } else {
                0
            };
            cfg.rc_max_quantizer = if hi > 0 {
                vpx_level(self.codec, hi)
            } else {
                63
            };
            let ms = (rate.vbv() as u64 * 1000 / rate.bps().max(1)) as u32;
            cfg.rc_buf_sz = ms;
            cfg.rc_buf_initial_sz = ms;
            cfg.rc_buf_optimal_sz = ms * 5 / 6;
        } else {
            cfg.rc_target_bitrate = RATE_CEILING_KBPS;
            cfg.rc_min_quantizer = vpx_level(self.codec, q);
            cfg.rc_max_quantizer = vpx_level(self.codec, q);
        }
        cfg.ss_target_bitrate[0] = cfg.rc_target_bitrate;
        cfg.ts_target_bitrate[..VP9_LAYERS as usize].fill(cfg.rc_target_bitrate);
        cfg.layer_target_bitrate[..VP9_LAYERS as usize].fill(cfg.rc_target_bitrate);
    }

    /// Apply the configuration to the running session.
    fn reconfigure(&mut self) -> Result<(), String> {
        if unsafe { vpx_codec_enc_config_set(&mut self.ctx, &self.cfg) } != VPX_CODEC_OK {
            return Err(error(&self.ctx, "libvpx refused the new configuration"));
        }
        if self.codec == Codec::Vp9 {
            self.program_layer()?;
        }
        Ok(())
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// The library behind the session, as the logs name it.
    pub fn library(&self) -> &'static str {
        "libvpx"
    }

    /// Whether the session carries 4:4:4 (VP9 profile 1, or 3 at 10 bits).
    pub fn is_fullcolor(&self) -> bool {
        self.planes.i444
    }

    /// The bits per sample the session codes: 10 in VP9 profiles 2 and 3.
    pub fn bit_depth(&self) -> u32 {
        self.planes.bit_depth
    }

    /// VP9 keeps the limited range of its 4:2:0 in profile 1, so the decoder hint a client
    /// sends for it stays true; VP8 has no other.
    pub fn is_full_range(&self) -> bool {
        false
    }

    /// The frame the last encoded frame predicted from.
    pub fn last_reference(&self) -> Reference {
        self.last_reference
    }

    /// Leave frame `frame_id` and every frame after it out of the predictions; the next frame
    /// predicts from an earlier buffer, or is a key frame when none is left.
    pub fn invalidate_reference(&mut self, frame_id: u16) -> bool {
        self.references.invalidate(frame_id);
        true
    }

    /// Every consumer holds frame `frame_id`, or where not `held` was sent it
    /// (`ReferenceSlots::acknowledge`).
    pub fn acknowledge_reference(&mut self, frame_id: u16, held: bool) {
        self.references.acknowledge(frame_id, held);
    }

    /// The quality index the rate control last coded a frame at, held frames aside.
    pub fn last_quality(&self) -> Option<u32> {
        self.last_quality
    }

    /// Whether a still screen's cleanup frame is coded at the rate its cap runs at (`cap`): VP9
    /// at a constant rate, whose cleanup runs through its rate control, which takes a new target
    /// live.
    pub fn follows_cap(&self) -> bool {
        self.codec == Codec::Vp9 && self.rate.cbr
    }

    /// Code the next frame, a still screen's cleanup, at the rate that spends `bytes` in
    /// `pipeline::CLEANUP_FRAME_S`, its buffer as long in time as the session's; the frame after
    /// goes back to the session's rate. At 60 fps 0.5 Mbit/s VP9 refined 1080p text by 1 dB a
    /// second at its own rate (32.5 dB 8 s after the stop), and to 55 dB in 8 s at a 2 Mbit/s
    /// path's.
    pub fn cap(&mut self, bytes: usize) {
        if self.follows_cap() {
            self.cap = Some(bytes);
        }
    }

    /// The bytes of the last frame the rate control coded, held frames aside, where a
    /// constant-rate cleanup runs through that rate control (`holds_quantizer`).
    pub fn last_size(&self) -> Option<usize> {
        self.last_bytes.filter(|_| !self.holds_quantizer())
    }

    /// Whether the cleanup of a still screen holds a frame at its quantizer: at a constant
    /// quantizer, and at a constant rate for VP8, whose rate control leaves a still screen as
    /// it is (a screen of text at 2 Mbit/s at 1080p stayed at 33.6 dB, where the held
    /// refresh reached 42.8). VP9's refines one, so its constant-rate cleanup runs through
    /// the rate control instead: 43.6 dB in 2.5 s with no frame over 75 kB, where the held
    /// refresh was one of 347 kB.
    pub fn holds_quantizer(&self) -> bool {
        !self.rate.cbr || self.codec == Codec::Vp8
    }

    /// Encode the next frame at the quantizer the quality index `crf` selects, whatever the rate
    /// control, and put the session's own rate control back for the frame after: the cleanup of a
    /// still screen. A held key frame of a constant-rate session starts at the quantizer the
    /// last one says fits `HELD_KEY_BUDGET_S` of the target (`held_key_start`), and one that
    /// comes out past it is coded again, as a key frame, at the coarser quantizer
    /// `held_key_retry` picks.
    /// `band`, the share of the picture from and to in raster order, confines the quantizer to
    /// the macroblocks it covers where the session holds a band (`band_size`), and the rest of
    /// the frame is left as the reference has it.
    pub fn hold_quantizer(&mut self, crf: u32, band: Option<(f64, f64)>) {
        self.held = Some(crf);
        self.held_band = band.filter(|_| self.band_size().is_some());
    }

    /// The bytes of the last held frame (0 before one), where the session holds a band of a
    /// frame at a quantizer: VP8, through libvpx's region-of-interest map (`band_roi`), at a rate
    /// that leaves a band room in a frame. A VP8 frame codes every macroblock's mode even where
    /// it skips them all, about a third of a bit each at 720p and 1080p, so below
    /// `BAND_BITS_PER_MB` of frame budget a band a frame is mostly that and the sweep cannot keep
    /// the rate: 1080p text at 0.25 Mbit/s (half a bit) ended at 29.5 dB in bands against 42.0
    /// held whole, where 720p at 0.25 (1.2 bits) ended at 41.5 against 42.0 with the worst wait
    /// at the target 5.0 s against 18.5. The refresh is held whole below it.
    pub fn band_size(&self) -> Option<usize> {
        let blocks = f64::from(self.cfg.g_w.div_ceil(16) * self.cfg.g_h.div_ceil(16));
        let fps = f64::from(self.rate.fps.num) / f64::from(self.rate.fps.den.max(1));
        let budget_bits = self.rate.bps() as f64 / fps.max(1.0);
        (self.codec == Codec::Vp8 && budget_bits >= BAND_BITS_PER_MB * blocks)
            .then_some(self.held_bytes)
    }

    /// Hand libvpx the region map of a held band (`band`, from and to in raster order of the
    /// macroblocks, as NVENC's delta map counts them), or clear it with `None`. The band is
    /// coded at the frame's own quantizer, the one held, with the static threshold the session
    /// keeps (`VP8_STATIC_THRESHOLD`) and, where `unfiltered`, no loop filter
    /// (`VP8_UNFILTERED_QINDEX`); every other macroblock is skipped, a static threshold
    /// no prediction error reaches making it a copy of the reference, so the frame carries the
    /// band and nothing else. VP8's segments take a quantizer delta of 63 at most, so the band
    /// is not held finer than a coarse rest the way NVENC's map holds it: the rest is skipped
    /// outright, which no quantizer of its own would do better.
    fn band_roi(&mut self, band: Option<(f64, f64)>, unfiltered: bool) -> Result<(), String> {
        let cols = self.cfg.g_w.div_ceil(16);
        let rows = self.cfg.g_h.div_ceil(16);
        let mut roi: vpx_roi_map_t = unsafe { std::mem::zeroed() };
        roi.rows = rows;
        roi.cols = cols;
        if let Some((from, to)) = band {
            let blocks = (rows * cols) as usize;
            let first = ((from.clamp(0.0, 1.0) * blocks as f64).floor() as usize).min(blocks);
            let last = ((to.clamp(0.0, 1.0) * blocks as f64).ceil() as usize).clamp(first, blocks);
            self.band_map.clear();
            self.band_map.resize(blocks, 0);
            self.band_map[first..last].fill(1);
            roi.roi_map = self.band_map.as_mut_ptr();
            roi.delta_q[0] = 63;
            // No loop filter either, which would still touch a skipped block's edges.
            roi.delta_lf[0] = -63;
            if unfiltered {
                roi.delta_lf[1] = -63;
            }
            roi.static_threshold[0] = u32::MAX >> 1;
            roi.static_threshold[1] = VP8_STATIC_THRESHOLD as u32;
        }
        let res = unsafe {
            vpx_codec_control_(
                &mut self.ctx,
                VP8E_SET_ROI_MAP as c_int,
                &mut roi as *mut vpx_roi_map_t as *mut c_void,
            )
        };
        if res != VPX_CODEC_OK {
            return Err(error(&self.ctx, "libvpx refused the band's region map"));
        }
        Ok(())
    }

    /// Code the blocks `refinement` names in the next frame of the rate control's at its target
    /// quality (`ledger`), through libvpx's region-of-interest map (`refine_roi`).
    pub fn refine_blocks(&mut self, refinement: &crate::ledger::Refinement) {
        self.refinement = Some(refinement.clone());
    }

    /// Hand libvpx the region map of `refinement`, or clear it with `None`: segment 1 the blocks
    /// it refines, coded the steps of libvpx's quantizer between the rate control's last and the
    /// target finer than the frame (which libvpx translates to its internal index), segment 0
    /// the rest at the frame's own. VP8 maps its 16x16 macroblocks and skips a refined block
    /// with no static threshold, since an unchanged block under the session's own
    /// (`VP8_STATIC_THRESHOLD`) would be copied rather than coded finer; VP9 maps its 8x8
    /// blocks, and runs the map in place of its cyclic refresh for the frame.
    fn refine_roi(&mut self, refinement: Option<&crate::ledger::Refinement>) -> Result<(), String> {
        let unit = if self.codec == Codec::Vp8 { 16 } else { 8 };
        let cols = self.cfg.g_w.div_ceil(unit);
        let rows = self.cfg.g_h.div_ceil(unit);
        let mut roi: vpx_roi_map_t = unsafe { std::mem::zeroed() };
        roi.rows = rows;
        roi.cols = cols;
        roi.ref_frame = [-1; 8];
        if let Some(r) = refinement {
            let step = |index: i32| self.codec.quantizer(index) as i32;
            let delta = (step(r.target) - step(r.from)).clamp(-63, 0);
            let u = unit as i32;
            self.band_map.clear();
            self.band_map.extend((0..rows).flat_map(|row| {
                (0..cols).map(move |col| {
                    u8::from(r.plan.refines_area(col as i32 * u, row as i32 * u, u, u))
                })
            }));
            roi.roi_map = self.band_map.as_mut_ptr();
            roi.delta_q[1] = delta;
            roi.static_threshold[0] = VP8_STATIC_THRESHOLD as u32;
        }
        let control = if self.codec == Codec::Vp8 {
            VP8E_SET_ROI_MAP
        } else {
            VP9E_SET_ROI_MAP
        };
        let res = unsafe {
            vpx_codec_control_(
                &mut self.ctx,
                control as c_int,
                &mut roi as *mut vpx_roi_map_t as *mut c_void,
            )
        };
        if res != VPX_CODEC_OK {
            return Err(error(
                &self.ctx,
                "libvpx refused the refinement's region map",
            ));
        }
        Ok(())
    }

    /// Pin the configuration's quantizer bounds to the one the quality index `crf` selects.
    fn pin_quantizer(&mut self, crf: u32) -> Result<(), String> {
        let level = vpx_level(self.codec, self.codec.quantizer(crf as i32));
        self.cfg.rc_min_quantizer = level;
        self.cfg.rc_max_quantizer = level;
        self.reconfigure()
    }

    /// Re-program the rate control when a rate or frame-rate setting changed, live.
    pub fn reconfigure_rate(&mut self, settings: &CaptureSettings) -> Result<(), String> {
        let Some(rate) = self.rate.changed(settings) else {
            return Ok(());
        };
        self.rate = rate;
        self.program_rate(rate, self.quality.current);
        self.reconfigure()?;
        self.program_refresh()
    }

    /// Encode one packed host frame at the quality index `crf`, as a key frame when
    /// `force_idr` or when no reference is left to predict from.
    pub fn encode_host(
        &mut self,
        pixels: &[u8],
        stride: usize,
        rgba: bool,
        frame_number: u64,
        crf: u32,
        force_idr: bool,
    ) -> Result<Vec<u8>, String> {
        if !self.rate.cbr
            && let Some(q) = self.quality.update(self.codec.quantizer(crf as i32))
        {
            self.program_rate(self.rate, q);
            self.reconfigure()?;
        }
        if let Some(bytes) = self.cap.take() {
            let kbps = ((bytes as f64 * 8.0 / crate::pipeline::CLEANUP_FRAME_S / 1000.0) as u32)
                .max((self.rate.bps() / 1000) as u32);
            if kbps != self.cfg.rc_target_bitrate {
                self.cfg.rc_target_bitrate = kbps;
                self.cfg.ss_target_bitrate[0] = kbps;
                self.cfg.ts_target_bitrate[..VP9_LAYERS as usize].fill(kbps);
                self.cfg.layer_target_bitrate[..VP9_LAYERS as usize].fill(kbps);
                self.reconfigure()?;
            }
            self.raised = true;
        } else if self.raised {
            self.program_rate(self.rate, self.quality.current);
            self.reconfigure()?;
            self.raised = false;
        }
        let capped = force_idr && self.rate.cbr;
        let cap = (self.rate.bps() as f64 / 8.0 * super::HELD_KEY_BUDGET_S) as usize;
        // A key frame skips nothing, so a band is held on a predicted one alone, and a key frame
        // asked for where a band was due goes at the rate control's quality rather than whole at
        // the band's quantizer.
        if force_idr && self.held_band.is_some() {
            self.held = None;
        }
        let held = self.held.take().map(|crf| {
            if capped {
                super::held_key_start(crf, self.held_key, cap)
            } else {
                crf
            }
        });
        if let Some(crf) = held {
            self.pin_quantizer(crf)?;
        }
        let band = self
            .held_band
            .take()
            .filter(|_| held.is_some() && !force_idr);
        if band.is_some() {
            let unfiltered =
                held.is_some_and(|crf| self.codec.quantizer(crf as i32) <= VP8_UNFILTERED_QINDEX);
            self.band_roi(band, unfiltered)?;
        }
        let refinement = self
            .refinement
            .take()
            .filter(|_| held.is_none() && !force_idr);
        if let Some(r) = &refinement {
            self.refine_roi(Some(r))?;
        }
        let quality = self.last_quality;
        let mut result = self.encode_frame(pixels, stride, rgba, frame_number, force_idr);
        if band.is_some() {
            self.band_roi(None, false)?;
        }
        if refinement.is_some() {
            self.refine_roi(None)?;
        }
        let mut coded_at = held;
        let retry = match (held, &result) {
            (Some(crf), Ok(coded)) if capped => super::held_key_retry(crf, coded.len(), cap),
            _ => None,
        };
        if let Some(coarser) = retry {
            self.pin_quantizer(coarser)?;
            result = self.encode_frame(pixels, stride, rgba, frame_number, true);
            coded_at = Some(coarser);
        }
        if let (true, Some(crf), Ok(coded)) = (capped, coded_at, &result) {
            self.held_key = Some((coded.len(), crf));
        }
        if held.is_some() {
            if let Ok(coded) = &result {
                self.held_bytes = coded.len();
            }
            self.last_quality = quality;
            self.program_rate(self.rate, self.quality.current);
            self.reconfigure()?;
        } else if let Ok(coded) = &result {
            self.last_bytes = Some(coded.len());
        }
        result
    }

    /// Encode one packed host frame at the quantizer the configuration holds.
    fn encode_frame(
        &mut self,
        pixels: &[u8],
        stride: usize,
        rgba: bool,
        frame_number: u64,
        force_idr: bool,
    ) -> Result<Vec<u8>, String> {
        let bt601 = self.codec == Codec::Vp8;
        self.planes
            .convert(pixels, stride, rgba, false, bt601, self.threads as usize)?;

        let key = force_idr || !self.references.has_reference();
        let pts = self.references.next_pts();
        let mut flags: c_long = if key { VPX_EFLAG_FORCE_KF as c_long } else { 0 };
        self.plan = self.references.plan(key);
        match self.codec {
            Codec::Vp9 => {
                let mut refs: vpx_svc_ref_frame_config = unsafe { std::mem::zeroed() };
                refs.update_buffer_slot[0] = self.plan.refresh.0 as c_int;
                if !key {
                    let slot = self.plan.predict_from.trailing_zeros() as c_int;
                    refs.lst_fb_idx[0] = slot;
                    refs.gld_fb_idx[0] = slot;
                    refs.alt_fb_idx[0] = slot;
                    refs.reference_last[0] = 1;
                }
                let res = unsafe {
                    vpx_codec_control_(
                        &mut self.ctx,
                        VP9E_SET_SVC_REF_FRAME_CONFIG as c_int,
                        &mut refs as *mut vpx_svc_ref_frame_config as *mut c_void,
                    )
                };
                if res != VPX_CODEC_OK {
                    return Err(error(
                        &self.ctx,
                        "libvpx refused the reference configuration",
                    ));
                }
            }
            _ => {
                flags |= VP8_EFLAG_NO_UPD_ENTROPY as c_long;
                if !key {
                    for (buffer, no_ref, no_upd, force) in [
                        (
                            SlotRefresh::LAST,
                            VP8_EFLAG_NO_REF_LAST,
                            VP8_EFLAG_NO_UPD_LAST,
                            0,
                        ),
                        (
                            SlotRefresh::GOLDEN,
                            VP8_EFLAG_NO_REF_GF,
                            VP8_EFLAG_NO_UPD_GF,
                            VP8_EFLAG_FORCE_GF,
                        ),
                        (
                            SlotRefresh::ALTREF,
                            VP8_EFLAG_NO_REF_ARF,
                            VP8_EFLAG_NO_UPD_ARF,
                            VP8_EFLAG_FORCE_ARF,
                        ),
                    ] {
                        if self.plan.predict_from != buffer {
                            flags |= no_ref as c_long;
                        }
                        if self.plan.refresh.refreshes(buffer) {
                            flags |= force as c_long;
                        } else {
                            flags |= no_upd as c_long;
                        }
                    }
                }
            }
        }

        let mut img: vpx_image = unsafe { std::mem::zeroed() };
        let fmt = match (self.planes.i444, self.planes.bit_depth > 8) {
            (true, true) => VPX_IMG_FMT_I44416,
            (true, false) => VPX_IMG_FMT_I444,
            (false, true) => VPX_IMG_FMT_I42016,
            (false, false) => VPX_IMG_FMT_I420,
        };
        let [y, u, v] = self.planes.pointers();
        unsafe {
            vpx_img_wrap(
                &mut img,
                fmt,
                self.planes.width as u32,
                self.planes.height as u32,
                1,
                y,
            );
        }
        let bytes = self.planes.sample_bytes();
        img.planes[0] = y;
        img.planes[1] = u;
        img.planes[2] = v;
        img.stride[0] = (self.planes.width * bytes) as c_int;
        img.stride[1] = (self.planes.chroma_width() * bytes) as c_int;
        img.stride[2] = (self.planes.chroma_width() * bytes) as c_int;
        img.bit_depth = self.planes.bit_depth;
        img.range = VPX_CR_STUDIO_RANGE;
        self.pending.push(pts, frame_number as u16);
        let res = unsafe {
            vpx_codec_encode(
                &mut self.ctx,
                &img,
                pts as i64,
                1,
                flags,
                VPX_DL_REALTIME as u64,
            )
        };
        if res != VPX_CODEC_OK {
            self.pending.undo();
            return Err(error(&self.ctx, "libvpx refused the frame"));
        }

        let mut output = Vec::new();
        let mut iter: vpx_codec_iter_t = ptr::null_mut();
        loop {
            let pkt = unsafe { vpx_codec_get_cx_data(&mut self.ctx, &mut iter) };
            if pkt.is_null() {
                break;
            }
            let pkt = unsafe { &*pkt };
            if pkt.kind != VPX_CODEC_CX_FRAME_PKT {
                continue;
            }
            let frame = unsafe { pkt.data.frame };
            let bytes = unsafe { std::slice::from_raw_parts(frame.buf as *const u8, frame.sz) };
            let is_key = if self.codec == Codec::Vp8 {
                vp8_is_key(bytes)
            } else {
                vp9_is_key(bytes)
            };
            let id = self
                .pending
                .take(frame.pts as u64)
                .unwrap_or(frame_number as u16);
            let plan = if is_key {
                self.references.plan(true)
            } else {
                self.plan
            };
            let anchor = if self.references.shared_anchor(plan) {
                FRAME_ANCHOR
            } else {
                0
            };
            self.last_reference = self.references.record(id, plan);
            if !self.omit_headers {
                output.reserve(VIDEO_HEADER_LEN + bytes.len());
                push_video_header(
                    &mut output,
                    self.codec,
                    frame_type_from_key(is_key) | anchor,
                    id,
                    0,
                    self.planes.width as u16,
                    self.planes.height as u16,
                    self.last_reference,
                );
            }
            output.extend_from_slice(bytes);
        }
        if !output.is_empty() {
            let mut q: c_int = 0;
            let res = unsafe {
                vpx_codec_control_(
                    &mut self.ctx,
                    VP8E_GET_LAST_QUANTIZER as c_int,
                    &mut q as *mut c_int,
                )
            };
            self.last_quality =
                (res == VPX_CODEC_OK).then(|| self.codec.quality_index(q.max(0) as u32));
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoders::codec::{FRAME_DELTA, FRAME_KEY, parse_video_type};
    use crate::webcam::decode::{Decoder as _, VideoDecoder};

    const W: usize = 320;
    const H: usize = 240;

    fn settings(codec: Codec) -> CaptureSettings {
        CaptureSettings {
            width: W as i32,
            height: H as i32,
            target_fps: 30.0,
            codec,
            video_crf: 25,
            use_cpu: true,
            ..Default::default()
        }
    }

    /// A frame whose luma spells its index in blocks, so a decoded picture names the frame it is.
    fn frame(t: usize) -> Vec<u8> {
        let mut f = vec![0u8; W * H * 4];
        let (bx, by) = ((t * 9) % (W - 40), (t * 5) % (H - 30));
        for y in 0..H {
            for x in 0..W {
                let i = (y * W + x) * 4;
                let g = ((x * 255) / W) as u8;
                let cell = (x / 8 + y / 12) % 3 == 0 && x % 8 < 6 && y % 12 < 9;
                let (b, gr, r) = if x >= bx && x < bx + 40 && y >= by && y < by + 30 {
                    (40, 220, 250)
                } else if cell {
                    (30, 30, 30)
                } else {
                    (g, 200 - g / 2, 120)
                };
                f[i..i + 4].copy_from_slice(&[b, gr, r, 255]);
            }
        }
        f
    }

    fn luma_distance(
        a: &crate::webcam::convert::I420View<'_>,
        b: &crate::webcam::convert::I420View<'_>,
    ) -> f64 {
        a.y.chunks(a.y_stride)
            .zip(b.y.chunks(b.y_stride))
            .take(a.height)
            .flat_map(|(ra, rb)| {
                ra[..a.width]
                    .iter()
                    .zip(&rb[..a.width])
                    .map(|(&x, &y)| (x as f64 - y as f64).abs())
            })
            .sum::<f64>()
            / (a.width * a.height) as f64
    }

    /// A frame a client lost is left out of the predictions: the next frame names the newest
    /// frame before it, and a decoder that never saw the lost frames decodes it exactly as one
    /// that saw everything. VP9 reaches back through its recent frames, VP8 through its anchors.
    /// Both codecs count time in frames of the capture's rate, as the fraction it names, a live
    /// change of rate included.
    #[test]
    fn the_time_base_is_one_frame_of_the_rate() {
        for codec in [Codec::Vp8, Codec::Vp9] {
            for (num, den) in [(60000i32, 1001i32), (120000, 1001), (144000, 1001), (60, 1)] {
                let fps = num as f64 / den as f64;
                let mut enc = VpxEncoder::new(
                    &CaptureSettings {
                        target_fps: fps,
                        ..settings(codec)
                    },
                    codec,
                    false,
                )
                .expect("session");
                assert_eq!(
                    (enc.cfg.g_timebase.num, enc.cfg.g_timebase.den),
                    (den, num),
                    "{codec:?}"
                );
                enc.reconfigure_rate(&CaptureSettings {
                    target_fps: 30.0,
                    ..settings(codec)
                })
                .expect("30 fps");
                assert_eq!(
                    (enc.cfg.g_timebase.num, enc.cfg.g_timebase.den),
                    (1, 30),
                    "{codec:?}"
                );
            }
        }
    }

    #[test]
    fn a_lost_frame_is_predicted_past() {
        for codec in [Codec::Vp8, Codec::Vp9] {
            let s = settings(codec);
            let mut enc = VpxEncoder::new(&s, codec, false).expect("session");
            let mut frames = Vec::new();
            for t in 0..8u64 {
                let out = enc
                    .encode_host(&frame(t as usize), W * 4, false, t, 25, t == 0)
                    .expect("encode");
                assert_eq!(
                    parse_video_type(out[1]).map(|(_, k)| k),
                    Some(if t == 0 { FRAME_KEY } else { FRAME_DELTA }),
                    "{codec:?} {t}"
                );
                assert_eq!(
                    enc.last_reference(),
                    if t == 0 {
                        Reference::None
                    } else {
                        Reference::Frame(t as u16 - 1)
                    },
                    "{codec:?} {t}"
                );
                frames.push(out);
            }
            // Frame 5 is reported lost once 6 and 7 have gone out: VP9 predicts from 4 through
            // its slots, VP8 from its golden anchor, which still holds the key frame.
            assert!(enc.invalidate_reference(5));
            let out = enc
                .encode_host(&frame(8), W * 4, false, 8, 25, false)
                .expect("encode");
            let anchor = if codec == Codec::Vp9 { 4 } else { 0 };
            assert_eq!(enc.last_reference(), Reference::Frame(anchor), "{codec:?}");
            assert_eq!(
                parse_video_type(out[1]).map(|(_, k)| k),
                Some(FRAME_DELTA),
                "{codec:?}"
            );
            frames.push(out);
            frames.push(
                enc.encode_host(&frame(9), W * 4, false, 9, 25, false)
                    .expect("encode"),
            );
            assert_eq!(enc.last_reference(), Reference::Frame(8), "{codec:?}");
            let (mut whole, mut lossy) = (
                VideoDecoder::new(codec).unwrap(),
                VideoDecoder::new(codec).unwrap(),
            );
            for (i, f) in frames.iter().enumerate() {
                assert!(
                    whole.decode(&f[VIDEO_HEADER_LEN..]).expect("decode"),
                    "{codec:?} frame {i}"
                );
                if !(5..8).contains(&i) {
                    assert!(
                        lossy
                            .decode(&f[VIDEO_HEADER_LEN..])
                            .expect("decode without 5-7"),
                        "{codec:?} frame {i}"
                    );
                }
            }
            let apart = luma_distance(&whole.frame().unwrap(), &lossy.frame().unwrap());
            assert!(
                apart == 0.0,
                "{codec:?}: the decoder that lost frames 5-7 shows frame 9 {apart:.2} off the one that saw them"
            );
        }
    }

    /// A loss ten frames deep, past the last eight frames, is predicted past from an anchor, and
    /// a decoder that never saw the lost frames decodes the frames after it exactly.
    #[test]
    fn a_loss_ten_frames_deep_is_predicted_past() {
        for codec in [Codec::Vp8, Codec::Vp9] {
            let s = settings(codec);
            let mut enc = VpxEncoder::new(&s, codec, false).expect("session");
            let mut frames = Vec::new();
            for t in 0..30u64 {
                frames.push(
                    enc.encode_host(&frame(t as usize), W * 4, false, t, 25, t == 0)
                        .expect("encode"),
                );
            }
            assert!(enc.invalidate_reference(20));
            let out = enc
                .encode_host(&frame(30), W * 4, false, 30, 25, false)
                .expect("encode");
            assert_eq!(
                parse_video_type(out[1]).map(|(_, k)| k),
                Some(FRAME_DELTA),
                "{codec:?}"
            );
            let anchor = if codec == Codec::Vp9 { 16 } else { 12 };
            assert_eq!(enc.last_reference(), Reference::Frame(anchor), "{codec:?}");
            frames.push(out);
            frames.push(
                enc.encode_host(&frame(31), W * 4, false, 31, 25, false)
                    .expect("encode"),
            );
            assert_eq!(enc.last_reference(), Reference::Frame(30), "{codec:?}");
            let (mut whole, mut lossy) = (
                VideoDecoder::new(codec).unwrap(),
                VideoDecoder::new(codec).unwrap(),
            );
            for (i, f) in frames.iter().enumerate() {
                assert!(
                    whole.decode(&f[VIDEO_HEADER_LEN..]).expect("decode"),
                    "{codec:?} frame {i}"
                );
                if !(20..30).contains(&i) {
                    assert!(
                        lossy
                            .decode(&f[VIDEO_HEADER_LEN..])
                            .expect("decode without 20-29"),
                        "{codec:?} frame {i}"
                    );
                }
            }
            let apart = luma_distance(&whole.frame().unwrap(), &lossy.frame().unwrap());
            assert!(
                apart == 0.0,
                "{codec:?}: the decoder that lost frames 20-29 shows frame 31 {apart:.2} off the one that saw them"
            );
        }
    }

    /// A loss no buffer reaches past costs a key frame, and the count restarts there.
    #[test]
    fn a_loss_past_the_window_costs_a_key_frame() {
        for codec in [Codec::Vp8, Codec::Vp9] {
            let s = settings(codec);
            let mut enc = VpxEncoder::new(&s, codec, false).expect("session");
            for t in 0..40u64 {
                enc.encode_host(&frame(t as usize), W * 4, false, t, 25, t == 0)
                    .expect("encode");
            }
            assert!(enc.invalidate_reference(1));
            let out = enc
                .encode_host(&frame(40), W * 4, false, 40, 25, false)
                .expect("encode");
            assert_eq!(
                parse_video_type(out[1]).map(|(_, k)| k),
                Some(FRAME_KEY),
                "{codec:?}"
            );
            assert_eq!(enc.last_reference(), Reference::None);
            enc.encode_host(&frame(41), W * 4, false, 41, 25, false)
                .expect("encode");
            assert_eq!(enc.last_reference(), Reference::Frame(40), "{codec:?}");
        }
    }

    /// The quantizer index read back from a VP9 frame's header is the one libvpx says it coded
    /// the frame at, for a key frame and a predicted one of every profile the build codes.
    #[test]
    fn a_vp9_header_names_its_quantizer_index() {
        use crate::encoders::codec::vp9_base_q_idx;
        let depths: &[i32] = if encodes_ten_bit() { &[8, 10] } else { &[8] };
        for (&depth, fullcolor) in depths.iter().flat_map(|d| [(d, false), (d, true)]) {
            let mut s = settings(Codec::Vp9);
            s.video_bit_depth = depth;
            s.video_fullcolor = fullcolor;
            let mut enc = VpxEncoder::new(&s, Codec::Vp9, fullcolor).expect("session");
            for (n, crf, key) in [(0, 40, true), (1, 15, false), (2, 28, false)] {
                let coded = enc
                    .encode_host(&frame(n), W * 4, false, n as u64, crf, key)
                    .unwrap();
                let read = vp9_base_q_idx(&coded[VIDEO_HEADER_LEN..]);
                assert_eq!(
                    read.map(|q| Codec::Vp9.quality_index(q)),
                    enc.last_quality(),
                    "{depth}-bit fullcolor {fullcolor} frame {n}: {read:?}"
                );
            }
        }
    }

    /// A quality change reaches a running session without a key frame, and a rate change too.
    /// The decoded picture's luma, `W` x `H`.
    fn luma(dec: &VideoDecoder) -> Vec<u8> {
        let f = dec.frame().expect("a decoded picture");
        f.y.chunks(f.y_stride)
            .take(H)
            .flat_map(|row| row[..W].iter().copied())
            .collect()
    }

    /// The mean luma difference of `a` and `b` over `rows`.
    fn rows_distance(a: &[u8], b: &[u8], rows: std::ops::Range<usize>) -> f64 {
        let n = rows.len() * W;
        a[rows.start * W..rows.end * W]
            .iter()
            .zip(&b[rows.start * W..rows.end * W])
            .map(|(&x, &y)| (x as f64 - y as f64).abs())
            .sum::<f64>()
            / n as f64
    }

    /// A VP8 refresh held in a band codes the band's macroblocks alone at the held quantizer and
    /// leaves every other one as the reference has it: a quarter of the picture costs a fraction
    /// of the whole refresh, the band decodes as the whole refresh's does, and the rest exactly
    /// as the frame before.
    #[test]
    fn a_vp8_band_codes_its_macroblocks_alone() {
        let mut s = settings(Codec::Vp8);
        s.video_cbr_mode = true;
        s.video_bitrate_kbps = 30;
        let still = frame(0);
        let run = |band: Option<(f64, f64)>| {
            let mut enc = VpxEncoder::new(&s, Codec::Vp8, false).expect("session");
            let mut dec = VideoDecoder::new(Codec::Vp8).unwrap();
            for t in 0..6u64 {
                let f = enc
                    .encode_host(&still, W * 4, false, t, 25, t == 0)
                    .unwrap();
                assert!(dec.decode(&f[VIDEO_HEADER_LEN..]).unwrap());
            }
            let before = luma(&dec);
            assert_eq!(enc.band_size(), Some(0), "no frame held yet");
            enc.hold_quantizer(5, band);
            let held = enc.encode_host(&still, W * 4, false, 6, 25, false).unwrap();
            assert_eq!(parse_video_type(held[1]).map(|(_, k)| k), Some(FRAME_DELTA));
            assert!(dec.decode(&held[VIDEO_HEADER_LEN..]).unwrap());
            assert_eq!(enc.band_size(), Some(held.len()));
            // The frame after goes back to the rate control, with no band left on it.
            let next = enc.encode_host(&still, W * 4, false, 7, 25, false).unwrap();
            assert!(dec.decode(&next[VIDEO_HEADER_LEN..]).unwrap());
            (held.len(), before, luma(&dec))
        };
        let (whole, before, refreshed) = run(None);
        // 300 macroblocks of 20 a row: the first quarter is rows 0 to 2 and part of row 3.
        let (banded, band_before, band_after) = run(Some((0.0, 0.25)));
        assert!(
            banded * 2 < whole,
            "a quarter of the picture cost {banded} bytes against {whole} for the whole"
        );
        assert_eq!(
            before, band_before,
            "both runs reach the same picture before the hold"
        );
        let changed = rows_distance(&before, &refreshed, 0..48);
        assert!(
            changed > 0.5,
            "the held quantizer refines the picture ({changed:.2})"
        );
        let band = rows_distance(&band_after, &refreshed, 0..48);
        assert!(
            band < changed / 4.0,
            "the band decodes as the whole refresh's ({band:.2})"
        );
        let rest = rows_distance(&band_after, &band_before, 64..H);
        assert!(rest < 0.01, "the rest is left as it was ({rest:.3})");
        // Half a bit of budget a macroblock (5 kbps at 300 macroblocks and 30 frames a second)
        // is mostly the frame's own modes, so no band is offered and the refresh is held whole.
        s.video_bitrate_kbps = 5;
        let starved = VpxEncoder::new(&s, Codec::Vp8, false).expect("session");
        assert_eq!(starved.band_size(), None);
    }

    /// A VP8 band held at a fine quantizer after coarse motion decodes as close to the picture as
    /// the quantizer codes it: it is not loop-filtered at the level the motion's frames left.
    #[test]
    fn a_vp8_band_after_coarse_motion_is_as_sharp_as_its_quantizer() {
        let mut s = settings(Codec::Vp8);
        s.video_cbr_mode = true;
        s.video_bitrate_kbps = 30;
        // The picture scrolled a few rows a frame.
        let scrolled = |t: usize| {
            let f = frame(0);
            let rows = W * 4;
            let shift = (t * 5) % H;
            [&f[shift * rows..], &f[..shift * rows]].concat()
        };
        let still = scrolled(12);
        // Its luma as VP8 codes it: BT.601, limited range.
        let source: Vec<u8> = still
            .chunks(4)
            .map(|p| {
                let (b, g, r) = (p[0] as f64, p[1] as f64, p[2] as f64);
                (16.0 + (65.481 * r + 128.553 * g + 24.966 * b) / 255.0).round() as u8
            })
            .collect();
        let mut enc = VpxEncoder::new(&s, Codec::Vp8, false).expect("session");
        let mut dec = VideoDecoder::new(Codec::Vp8).unwrap();
        for t in 0..12u64 {
            let f = enc
                .encode_host(&scrolled(t as usize), W * 4, false, t, 25, t == 0)
                .unwrap();
            assert!(dec.decode(&f[VIDEO_HEADER_LEN..]).unwrap());
        }
        // The top quarter, away from the middle of the picture.
        enc.hold_quantizer(5, Some((0.0, 0.25)));
        let held = enc
            .encode_host(&still, W * 4, false, 12, 25, false)
            .unwrap();
        assert!(dec.decode(&held[VIDEO_HEADER_LEN..]).unwrap());
        let off = rows_distance(&luma(&dec), &source, 0..48);
        // A key frame of the picture at the same quantizer, from a session with no motion behind it.
        let mut fresh = VpxEncoder::new(&settings(Codec::Vp8), Codec::Vp8, false).expect("session");
        let mut fresh_dec = VideoDecoder::new(Codec::Vp8).unwrap();
        fresh.hold_quantizer(5, None);
        let key = fresh
            .encode_host(&still, W * 4, false, 0, 25, true)
            .unwrap();
        assert!(fresh_dec.decode(&key[VIDEO_HEADER_LEN..]).unwrap());
        let floor = rows_distance(&luma(&fresh_dec), &source, 0..48);
        assert!(
            off < floor + 0.2,
            "the band decodes {off:.2} off the picture, a key frame at its quantizer {floor:.2}"
        );
    }

    /// A VP9 cleanup frame capped at `bytes` is coded at the rate that spends them in the
    /// cleanup's frame time, the frame after at the session's again; VP8, which holds its cleanup
    /// at a quantizer, keeps its rate.
    #[test]
    fn a_vp9_cleanup_frame_runs_at_its_caps_rate_and_the_next_at_the_sessions() {
        for codec in [Codec::Vp8, Codec::Vp9] {
            let mut s = settings(codec);
            s.video_cbr_mode = true;
            s.video_bitrate_kbps = 500;
            let mut enc = VpxEncoder::new(&s, codec, false).expect("session");
            enc.encode_host(&frame(0), W * 4, false, 0, 25, true)
                .unwrap();
            enc.cap(400_000);
            enc.encode_host(&frame(0), W * 4, false, 1, 25, false)
                .unwrap();
            let raised = enc.cfg.rc_target_bitrate;
            enc.encode_host(&frame(0), W * 4, false, 2, 25, false)
                .unwrap();
            let after = enc.cfg.rc_target_bitrate;
            if codec == Codec::Vp9 {
                assert_eq!((raised, after), (16_000, 500));
            } else {
                assert_eq!((raised, after), (500, 500), "{codec:?}");
            }
        }
    }

    #[test]
    fn quality_and_rate_move_without_a_key_frame() {
        for codec in [Codec::Vp8, Codec::Vp9] {
            let mut s = settings(codec);
            s.video_crf = 40;
            let mut enc = VpxEncoder::new(&s, codec, false).expect("session");
            let coarse = enc
                .encode_host(&frame(0), W * 4, false, 0, 40, true)
                .unwrap();
            let fine = enc
                .encode_host(&frame(1), W * 4, false, 1, 15, false)
                .unwrap();
            assert_eq!(enc.quality.current, codec.quantizer(15));
            assert_eq!(
                parse_video_type(fine[1]).map(|(_, k)| k),
                Some(FRAME_DELTA),
                "{codec:?}"
            );
            let again = enc
                .encode_host(&frame(2), W * 4, false, 2, 15, false)
                .unwrap();
            assert!(
                again.len() + fine.len() > coarse.len() / 2,
                "{codec:?}: the finer quantizer spends more"
            );
            s.target_fps = 15.0;
            enc.reconfigure_rate(&s).expect("rate");
            let after = enc
                .encode_host(&frame(3), W * 4, false, 3, 15, false)
                .unwrap();
            assert_eq!(
                parse_video_type(after[1]).map(|(_, k)| k),
                Some(FRAME_DELTA),
                "{codec:?}"
            );
            let mut dec = VideoDecoder::new(codec).unwrap();
            for f in [&coarse, &fine, &again, &after] {
                assert!(dec.decode(&f[VIDEO_HEADER_LEN..]).unwrap(), "{codec:?}");
            }
        }
    }
}
