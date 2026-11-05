/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Software H.264 encoder built on Cisco's OpenH264: the software H.264 encoder of a
//! build without the `gpl` feature, where it stands in for libx264 behind the same
//! software path (`encode_cpu`), one instance for the whole frame, and emits the same
//! wire framing, so a client cannot tell which library a GPL-free build encodes with. Cisco's BSD license
//! is what lets it ship redistributably; the price of admission is being 4:2:0-only
//! (a 4:4:4 request is encoded 4:2:0) and Annex-B.
//!
//! Keyframes are driven externally — an effectively infinite intra period plus
//! on-demand `force_intra_frame`, never a fixed cadence — so a mostly-static screen
//! spends no bitrate on keyframes nothing requested, matching the NVENC/x264
//! strict-GOP streaming behavior. Host ARGB is converted to I420 with the same
//! BT.709 limited-range path the x264 encoder uses, then fed to OpenH264 as borrowed planes.

use crate::CaptureSettings;
use crate::encoders::QP_HYSTERESIS_LIMIT;
use crate::encoders::codec::{
    Codec, FRAME_ANCHOR, FRAME_DELTA, FRAME_INTRA, FRAME_KEY, VIDEO_HEADER_LEN, push_video_header,
};
use crate::encoders::oh264_refs::{LongTermRefs, Recovery};
use crate::encoders::reference::Reference;
use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, QpRange,
    RateControlMode, UsageType, VuiConfig,
};
use openh264::formats::YUVSlices;

/// The frames OpenH264 lets pass between its long-term markings. Each marking waits on the
/// confirmation of the one before, and a loss is predicted past from the newest confirmed, so a
/// short period keeps that one near: with a slow page dark 2 s in every 10, the frames predicting
/// past its losses over WebSockets reached 10 frames back (median) at 2, against 50 at
/// OpenH264's default of 30.
const LTR_MARK_PERIOD: u32 = 2;

/// Suppresses OpenH264's built-in periodic IDR so the GOP is effectively infinite and a
/// keyframe costs bitrate only when something genuinely needs one.
///
/// A wall-clock keyframe cadence spikes bitrate and dents quality on a screen that is not changing,
/// so recovery keyframes are driven entirely by the pipeline's force-IDR decision
/// (`force_intra_frame`) instead. The period is simply set so large the encoder effectively never
/// reaches it within a session.
const INFINITE_INTRA_PERIOD: u32 = 300_000;

/// A deliberately huge bitrate value (100 Mbps, still well under H.264 level 5.2's cap) used
/// wherever rate control must be kept from becoming the binding constraint. Two such places:
///
/// 1. **CRF/CQP rate budget**: passed as the target bitrate so the pinned QP — not the bitrate —
///    always dominates rate control and the constant QP is what actually shapes each frame.
/// 2. **Live CBR headroom**: the max bitrate a live CBR change may lift the session to.
///    Construction pins OpenH264's max bitrate to the *starting* target (EncoderConfig exposes no
///    separate max), which would otherwise reject any later raise above it; lifting the max to this
///    ceiling first is what lets `set_live_bitrate` move the target freely.
const BITRATE_CEILING_BPS: u32 = 100_000_000;

/// One session's worth of OpenH264 state, kept alive between frames because
/// the encoder's reference chain and rate-control state must persist across the whole session —
/// and the I420 plane buffers are reused every frame to keep the per-frame hot path free of
/// allocation.
///
/// Holds the live `Encoder`, the fixed dimensions, and the reusable I420 plane buffers
/// (`y_buf` / `u_buf` / `v_buf`) that each frame's RGB-to-YUV conversion writes into before
/// hand-off. `threads`, `slices`, and `csc_bands` are the parallelism policy fixed at open: the
/// instance encodes with several threads over four slices and converts color in four bands (one
/// thread and one band without `cpu_encode_threading`, the four slices kept). `is_cbr` selects the rate-control mode: `true` is
/// CBR (bitrate-mode RC driving a target bitrate), `false` is CRF/CQP (the same bitrate-mode RC but
/// with the QP pinned to a single value). `omit_stripe_headers` drops the wire header for
/// bare Annex-B output.
///
/// The live rate-control state is mirrored so a change can be detected, rolled back, or carried
/// across a rebuild: `current_bitrate_bps` is the currently-accepted CBR target, `current_fps` the
/// accepted frame rate, and `current_qp` the pinned CRF/CQP quantizer with `qp_hysteresis_counter`
/// gating increases. `settings` is the construction configuration, kept because a QP change is
/// applied by rebuilding the encoder from it (see `update_qp`).
pub struct Openh264Encoder {
    encoder: Encoder,
    settings: CaptureSettings,
    width: usize,
    height: usize,
    threads: u16,
    slices: u32,
    csc_bands: usize,
    y_buf: Vec<u8>,
    u_buf: Vec<u8>,
    v_buf: Vec<u8>,
    current_bitrate_bps: i32,
    current_fps: f32,
    current_qp: i32,
    qp_hysteresis_counter: u32,
    is_cbr: bool,
    omit_stripe_headers: bool,
    /// The long-term references, read off the encoder's own output.
    refs: LongTermRefs,
    last_reference: Reference,
}

impl Openh264Encoder {
    /// Build an OpenH264 encoder from the capture settings (their width, height, CRF, and
    /// bitrate), or `None` on init failure: `with_size` at the settings' own.
    pub fn new(settings: &CaptureSettings) -> Option<Self> {
        Self::with_size(
            settings,
            settings.width.max(2) as usize,
            settings.height.max(2) as usize,
            settings.video_crf,
            settings.video_bitrate_kbps,
        )
    }

    /// Build the encoder of the software H.264 session (`encode_cpu`), or `None` on init
    /// failure. Like the NVENC/VAAPI encoders, this one self-prepends the wire header on the
    /// buffer it returns.
    ///
    /// `width` x `height` is the frame's geometry, `crf` its constant quantizer, and
    /// `bitrate_kbps` its CBR budget. The instance encodes with `h264_fullframe_threads` threads
    /// over four fixed slices (client decoders slice-parallelize too) and converts color in four
    /// bands, so a frame never serializes on one core, unless `cpu_encode_threading` is off, which
    /// leaves it one thread and one band.
    ///
    /// 1. **Geometry**: width and height are floored to even values (`& !1`, min 2) because 4:2:0
    ///    chroma subsampling requires it. A 4:4:4 request is encoded 4:2:0 regardless: OpenH264 is
    ///    4:2:0-only (the caller reports that once, via `software_fullcolor`).
    ///
    /// 2. **CRF floor**: `crf` is clamped to `[1, 51]`. The floor is 1, not 0, because a zero
    ///    anywhere in the QP pair makes OpenH264's `ParamValidation` discard the *entire* range and
    ///    substitute its screen-content defaults `[26, 35]`; explicit non-zero values are honored
    ///    (the library clips the min up to its own floor of 12).
    ///
    /// 3. **Base config** (shared by both rate modes): camera usage with long-term references
    ///    (`refs`), low complexity, an effectively infinite intra period, a VUI declaring the conversion
    ///    (BT.709 primaries, transfer, and matrix at limited range; without it a WebRTC receiver
    ///    infers the range from the SDP profile and can display the picture visibly darker), and
    ///    the thread count. Frame skip is **enabled** so the rate controller can actually hold
    ///    the target bitrate — skip-less bitrate control is only approximate. A skipped frame is not encoded
    ///    and the next P-frame references the last *encoded* frame, so the reference chain stays
    ///    intact. Adaptive quantization and background detection are set explicitly to `false` to
    ///    silence the init warnings, and scene-change detection is off, so no IDR comes unasked.
    ///    The internal stderr trace follows the capture's
    ///    `debug_logging` flag (otherwise quiet).
    ///
    /// 4. **Rate control**:
    ///    - **CBR** (`video_cbr_mode`): bitrate-mode RC targeting `bitrate_kbps`, with the QP range
    ///      made explicit (`video_min_qp`, and `video_max_qp` defaulting the max to 51). The
    ///      explicit range dodges the same zero-substitution as the CRF floor, which would
    ///      otherwise cap the RC at QP 35 where hard content overshoots any target.
    ///    - **CRF/CQP** (default): the same bitrate-mode RC but with an oversized bitrate budget
    ///      (`BITRATE_CEILING_BPS`) and the QP pinned to a single value (`min == max == crf`), so
    ///      the QP clamp always dominates and every frame uses the constant QP — matching NVENC
    ///      CONSTQP / VAAPI CQP. Bitrate-mode with a pinned QP is what actually holds the QP
    ///      constant: RC_OFF ignores the QP range and RC_QUALITY rejects a `min == max` range.
    ///
    /// The instance is then primed for its four fixed slices via `enable_slices`.
    pub fn with_size(
        settings: &CaptureSettings,
        width: usize,
        height: usize,
        crf: i32,
        bitrate_kbps: i32,
    ) -> Option<Self> {
        let width = width.max(2) & !1;
        let height = height.max(2) & !1;
        let bps = (bitrate_kbps.max(1) as u32).saturating_mul(1000);
        let fps = Self::clamp_fps(settings.target_fps);
        let crf = crf.clamp(1, 51);
        let threads = super::session::h264_fullframe_threads(settings) as u16;
        let slices = 4;

        let encoder = Self::build_encoder(settings, fps, bps, crf as u8, threads)?;

        let (cw, ch) = (width / 2, height / 2);
        let mut me = Self {
            encoder,
            settings: settings.clone(),
            width,
            height,
            threads,
            slices,
            // The conversion takes a band a thread; one alone converts in one.
            csc_bands: if threads > 1 { slices as usize } else { 1 },
            y_buf: vec![0u8; width * height],
            u_buf: vec![0u8; cw * ch],
            v_buf: vec![0u8; cw * ch],
            current_bitrate_bps: bps as i32,
            current_fps: fps,
            current_qp: crf,
            qp_hysteresis_counter: 0,
            is_cbr: settings.video_cbr_mode,
            omit_stripe_headers: settings.omit_stripe_headers,
            refs: LongTermRefs::new(),
            last_reference: Reference::Untracked,
        };
        me.enable_slices();
        Some(me)
    }

    /// Encoded width in pixels (the request floored to even).
    pub fn width(&self) -> usize {
        self.width
    }

    /// Encoded height in pixels (the request floored to even).
    pub fn height(&self) -> usize {
        self.height
    }

    /// The frame rate OpenH264 is configured with: the requested rate, or 30 when the capture
    /// asks for less than 1 fps (the library rejects a sub-1 rate).
    fn clamp_fps(target_fps: f64) -> f32 {
        if target_fps < 1.0 {
            30.0
        } else {
            target_fps as f32
        }
    }

    /// Build one configured OpenH264 encoder, as described by `with_size`'s steps 3 and 4.
    /// Separate from `with_size` because `update_qp` rebuilds the encoder to move the pinned
    /// CRF/CQP quantizer, and that rebuild has to reproduce the session's configuration exactly
    /// apart from the QP — hence the live `fps` / `bitrate_bps` / `crf` arguments rather than
    /// reading the construction settings for values a running session can have retuned.
    ///
    /// The usage is the camera one: OpenH264 predicts past a loss from a long-term reference only
    /// there (its screen-content strategy with long-term references ignores the recovery
    /// request), and it codes a scrolling document as well (41.4 against 41.3 dB moving, 57.1 both
    /// still, 1080p at 8 Mbit/s).
    fn build_encoder(
        settings: &CaptureSettings,
        fps: f32,
        bitrate_bps: u32,
        crf: u8,
        threads: u16,
    ) -> Option<Encoder> {
        let base = EncoderConfig::new()
            .max_frame_rate(FrameRate::from_hz(fps))
            .usage_type(UsageType::CameraVideoRealTime)
            .long_term_reference(true)
            .scene_change_detect(false)
            .complexity(Complexity::Low)
            .skip_frames(true)
            .adaptive_quantization(false)
            .background_detection(false)
            .debug(settings.debug_logging)
            .intra_frame_period(IntraFramePeriod::from_num_frames(INFINITE_INTRA_PERIOD))
            .vui(VuiConfig::bt709())
            .num_threads(threads);
        let config = if settings.video_cbr_mode {
            let min_qp = settings.video_min_qp.clamp(1, 51) as u8;
            let max_qp = if settings.video_max_qp > 0 {
                settings.video_max_qp.clamp(min_qp as i32, 51) as u8
            } else {
                51
            };
            base.bitrate(BitRate::from_bps(bitrate_bps))
                .rate_control_mode(RateControlMode::Bitrate)
                .qp(QpRange::new(min_qp, max_qp))
        } else {
            base.bitrate(BitRate::from_bps(BITRATE_CEILING_BPS))
                .rate_control_mode(RateControlMode::Bitrate)
                .qp(QpRange::new(crf, crf))
        };

        Encoder::with_api_config(OpenH264API::from_source(), config).ok()
    }

    /// Give every frame of a multi-threaded instance `slices` fixed slices, so its encoder
    /// threads and the client's decoder can parallelize within the frame; more than four slices
    /// upsets some Chromium decoders, so the count is pinned to four (`SM_FIXEDSLCNUM_SLICE`).
    /// The same re-init shortens the long-term marking period (`LTR_MARK_PERIOD`), which the crate's
    /// config fixes at 30.
    ///
    /// The `openh264` crate initializes the underlying encoder lazily on the *first* encode and
    /// exposes no fixed slicing through its config, so the routine primes the encoder with one
    /// throwaway frame, patches the now-live `SEncParamExt` parameter set via `set_option`, and
    /// lets the next encode re-init from it. It is best-effort: any `get_option` / `set_option`
    /// failure returns early, leaving a working single-slice encoder. Because the throwaway frame
    /// consumed the initial IDR, a final `force_intra_frame` guarantees the first *delivered* frame
    /// is still an IDR.
    fn enable_slices(&mut self) {
        let slices = YUVSlices::new(
            (&self.y_buf, &self.u_buf, &self.v_buf),
            (self.width, self.height),
            (self.width, self.width / 2, self.width / 2),
        );
        if self.encoder.encode(&slices).is_err() {
            return;
        }
        unsafe {
            let mut params: openh264_sys2::SEncParamExt = std::mem::zeroed();
            let raw = self.encoder.raw_api();
            if raw.get_option(
                openh264_sys2::ENCODER_OPTION_SVC_ENCODE_PARAM_EXT,
                &mut params as *mut _ as *mut std::ffi::c_void,
            ) != 0
            {
                return;
            }
            params.sSpatialLayers[0].sSliceArgument.uiSliceMode =
                openh264_sys2::SM_FIXEDSLCNUM_SLICE;
            params.sSpatialLayers[0].sSliceArgument.uiSliceNum = self.slices;
            params.iLtrMarkPeriod = LTR_MARK_PERIOD;
            let ret = raw.set_option(
                openh264_sys2::ENCODER_OPTION_SVC_ENCODE_PARAM_EXT,
                &mut params as *mut _ as *mut std::ffi::c_void,
            );
            if ret != 0 {
                eprintln!("[openh264] stream-param re-init rejected ({ret}); staying single-slice");
            }
        }
        self.encoder.force_intra_frame();
    }

    /// Apply a live bitrate (kbps) and/or framerate change in place, so the session keeps its
    /// reference chain and infinite GOP instead of taking the visible reset a full rebuild would
    /// force on the viewer.
    ///
    /// OpenH264 honors both through `SetOption`, so the reference chain and the infinite GOP are
    /// preserved and the change takes effect mid-stream — this is what makes the web UI's bitrate
    /// slider work, like NVENC and x264. The bitrate is applied only in **CBR** mode, and only when
    /// it actually differs from the current target; the CRF/CQP quantizer moves through `update_qp`
    /// instead. A framerate of at least 1 fps that differs from the live one is pushed via the
    /// frame-rate option and mirrored in `current_fps`, so a later `update_qp` rebuild keeps the
    /// live rate. Both are self-gating, so the software path's per-frame call is free when
    /// nothing changed.
    pub fn reconfigure_rate(&mut self, bitrate_kbps: i32, fps: f64) {
        if self.is_cbr {
            let bps = bitrate_kbps.max(1).saturating_mul(1000);
            if bps != self.current_bitrate_bps {
                self.set_live_bitrate(bps);
            }
        }
        if fps >= 1.0 && fps as f32 != self.current_fps {
            let mut rate = fps as f32;
            unsafe {
                self.encoder.raw_api().set_option(
                    openh264_sys2::ENCODER_OPTION_FRAME_RATE,
                    std::ptr::addr_of_mut!(rate).cast(),
                );
            }
            self.current_fps = rate;
        }
    }

    /// Move the CRF/CQP quantizer to `target_qp` — the quality slider and the paint-over refresh
    /// both land here, so a session started in CRF mode retunes like the NVENC / VA-API / x264 ones
    /// rather than staying pinned to its construction QP for its whole life.
    ///
    /// OpenH264 exposes no live-QP setter: `SetOption(SVC_ENCODE_PARAM_EXT)` carries a QP range, but
    /// its in-place adjust path never copies `iMinQp` / `iMaxQp` onto the running parameter set, so
    /// only a rebuilt encoder actually encodes at a new quantizer. The rebuild is therefore the
    /// mechanism, and its cost — a fresh reference chain, a new parameter set, and an IDR — is what
    /// the call is gated to avoid paying needlessly, in the same order as the VA-API CQP path:
    ///
    /// 1. **CBR**: no-op — the bitrate target governs quality there and the QP range is a clamp.
    /// 2. **Unchanged QP**: reset the hysteresis counter and return, which is what makes a
    ///    per-frame call from the pipeline free.
    /// 3. **QP decrease** (higher quality, e.g. a paint-over refresh): rebuild immediately.
    /// 4. **QP increase** (lower quality, e.g. sustained motion): only rebuild once
    ///    `QP_HYSTERESIS_LIMIT` consecutive frames have asked for it, so transient motion does not
    ///    make quality blink.
    ///
    /// A rebuild that fails to initialize leaves the running encoder untouched, so the session
    /// continues at its current quantizer instead of dying on a quality change. True where it
    /// rebuilt, so the next frame is a key frame.
    pub fn update_qp(&mut self, target_qp: u32) -> bool {
        if self.is_cbr {
            return false;
        }
        let qp = (target_qp as i32).clamp(1, 51);
        if qp == self.current_qp {
            self.qp_hysteresis_counter = 0;
            return false;
        }
        if qp > self.current_qp {
            self.qp_hysteresis_counter += 1;
            if self.qp_hysteresis_counter <= QP_HYSTERESIS_LIMIT {
                return false;
            }
        }
        self.qp_hysteresis_counter = 0;
        match Self::build_encoder(
            &self.settings,
            self.current_fps,
            self.current_bitrate_bps.max(1) as u32,
            qp as u8,
            self.threads,
        ) {
            Some(encoder) => {
                self.encoder = encoder;
                self.current_qp = qp;
                self.enable_slices();
                true
            }
            None => {
                eprintln!(
                    "[openh264] rebuild for QP {qp} failed; staying at QP {}",
                    self.current_qp
                );
                false
            }
        }
    }

    /// Move the live CBR target to `bps` — but raise the max-bitrate ceiling first, because
    /// OpenH264 refuses any target above the max that construction pinned to the session's *starting*
    /// bitrate.
    ///
    /// Construction pins OpenH264's max bitrate to the starting target (EncoderConfig exposes no
    /// separate max), and `SetOption` rejects — after partially applying — any target above that
    /// max. So the ceiling is first lifted to `BITRATE_CEILING_BPS` for both the overall stream
    /// (`SPATIAL_LAYER_ALL`) and layer 0 (verification reads the per-layer value); if raising the
    /// ceiling fails, the previous rate is kept and the change aborts. The target is then set on the
    /// whole stream: on success `current_bitrate_bps` is updated; on failure the already-mutated
    /// target is re-set back to `current_bitrate_bps` so the encoder stays in sync with the tracked
    /// value.
    fn set_live_bitrate(&mut self, bps: i32) {
        for layer in [
            openh264_sys2::SPATIAL_LAYER_ALL,
            openh264_sys2::SPATIAL_LAYER_0,
        ] {
            let ret = self.set_bitrate_option(
                openh264_sys2::ENCODER_OPTION_MAX_BITRATE,
                layer,
                BITRATE_CEILING_BPS as i32,
            );
            if ret != 0 {
                eprintln!(
                    "[openh264] live bitrate change to {bps} bps failed raising the max-bitrate ceiling \
                     (code {ret}); keeping {} bps",
                    self.current_bitrate_bps
                );
                return;
            }
        }
        let ret = self.set_bitrate_option(
            openh264_sys2::ENCODER_OPTION_BITRATE,
            openh264_sys2::SPATIAL_LAYER_ALL,
            bps,
        );
        if ret == 0 {
            self.current_bitrate_bps = bps;
        } else {
            self.set_bitrate_option(
                openh264_sys2::ENCODER_OPTION_BITRATE,
                openh264_sys2::SPATIAL_LAYER_ALL,
                self.current_bitrate_bps,
            );
            eprintln!(
                "[openh264] live bitrate change to {bps} bps failed (code {ret}); keeping {} bps",
                self.current_bitrate_bps
            );
        }
    }

    /// Marshal one `SBitrateInfo` FFI struct and issue a single `SetOption` (BITRATE or
    /// MAX_BITRATE) for the given layer, returning the raw return code (0 on success). Exists so the
    /// ceiling-raise and the target-set in `set_live_bitrate` share one unsafe marshalling path
    /// rather than open-coding the same `zeroed()`-and-cast dance twice.
    fn set_bitrate_option(
        &mut self,
        option: openh264_sys2::ENCODER_OPTION,
        layer: openh264_sys2::LAYER_NUM,
        bps: i32,
    ) -> i32 {
        let mut info: openh264_sys2::SBitrateInfo = unsafe { std::mem::zeroed() };
        info.iLayer = layer;
        info.iBitrate = bps;
        unsafe {
            self.encoder
                .raw_api()
                .set_option(option, std::ptr::addr_of_mut!(info).cast())
        }
    }

    /// Encode one host frame and return it framed exactly like x264's and the hardware
    /// encoders' frames, so the client demuxes
    /// this software path with the same code and cannot tell which library encoded it. Returns an
    /// empty buffer when the frame produced no payload.
    ///
    /// 1. **Keyframe**: when `force_idr` is set, `force_intra_frame` is called so this frame is
    ///    emitted as an IDR.
    /// 2. **Color conversion**: `argb` (with `stride` bytes per row, `height` rows) is converted
    ///    to I420 into the reusable Y/U/V plane buffers, across `csc_bands` threads. `rgba_input`
    ///    selects the source byte order — `false` is B,G,R,A (X11 XShm), `true` is R,G,B,A (Wayland
    ///    GL readback).
    /// 3. **Encode**: the planes are wrapped as borrowed `YUVSlices` and encoded.
    /// 4. **Framing**: unless `omit_stripe_headers` is set, the wire header is prepended, its
    ///    frame kind read from the *actually encoded* picture type rather than from `force_idr`,
    ///    because the encoder may re-type a frame and the header must label a real decode entry
    ///    point, not the request. With `omit_stripe_headers` the output is bare Annex-B.
    /// 5. **Empty payload**: if the encoder produced no bitstream (e.g. a skipped frame), an empty
    ///    vec is returned rather than a lone header — a header with no Annex-B behind it would be a
    ///    malformed frame to the client, and the pipeline reads empty as "nothing to send".
    pub fn encode_host_argb(
        &mut self,
        argb: &[u8],
        stride: usize,
        frame_number: u64,
        force_idr: bool,
        rgba_input: bool,
    ) -> Result<Vec<u8>, String> {
        if force_idr {
            self.encoder.force_intra_frame();
        }
        let cw = self.width / 2;
        crate::encoders::software::convert_to_yuv_mt(
            argb,
            stride as u32,
            self.width,
            self.height,
            rgba_input,
            false,
            false,
            false,
            &mut self.y_buf,
            &mut self.u_buf,
            &mut self.v_buf,
            (self.width, cw),
            self.csc_bands,
        )
        .map_err(|e| format!("rgb-to-yuv420 failed: {e:?}"))?;

        let slices = YUVSlices::new(
            (&self.y_buf, &self.u_buf, &self.v_buf),
            (self.width, self.height),
            (self.width, cw, cw),
        );
        match self.encoder.encode(&slices) {
            Ok(bitstream) => {
                let frame_type = match bitstream.frame_type() {
                    FrameType::IDR => FRAME_KEY,
                    FrameType::I => FRAME_INTRA,
                    _ => FRAME_DELTA,
                };
                let mut body = Vec::new();
                bitstream.write_vec(&mut body);
                if body.is_empty() {
                    return Ok(Vec::new());
                }
                let (reference, anchor) = self.refs.record(frame_number as u16, &body);
                self.last_reference = reference;
                if self.omit_stripe_headers {
                    return Ok(body);
                }
                let mut out = Vec::with_capacity(VIDEO_HEADER_LEN + body.len());
                push_video_header(
                    &mut out,
                    Codec::H264,
                    frame_type | if anchor { FRAME_ANCHOR } else { 0 },
                    frame_number as u16,
                    0,
                    self.width as u16,
                    self.height as u16,
                    reference,
                );
                out.extend_from_slice(&body);
                Ok(out)
            }
            Err(e) => Err(format!("openh264 encode failed: {e:?}")),
        }
    }

    /// The frame the last encoded frame predicted from, tracked through the long-term references.
    pub fn last_reference(&self) -> Reference {
        self.last_reference
    }

    /// Every consumer holds frame `frame_id`, or where not `held` was sent it: a held frame that
    /// marked a long-term reference is confirmed to the encoder, which then takes it for a
    /// recovery and marks the next.
    pub fn acknowledge_reference(&mut self, frame_id: u16, held: bool) {
        if let Some((idr_pic_id, frame_num)) = self.refs.acknowledge(frame_id, held) {
            self.feedback(openh264_sys2::LTR_MARKING_SUCCESS, idr_pic_id, frame_num);
        }
    }

    /// A consumer lost frame `frame_id`: the next frame predicts from the confirmed long-term
    /// reference where one precedes the loss. False where none does, and the caller codes a key
    /// frame instead.
    pub fn invalidate_reference(&mut self, frame_id: u16) -> bool {
        let (recovery, failed) = self.refs.invalidate(frame_id);
        for (idr_pic_id, frame_num) in failed {
            self.feedback(openh264_sys2::LTR_MARKING_FAILED, idr_pic_id, frame_num);
        }
        match recovery {
            Recovery::Ignored => true,
            Recovery::KeyFrame => false,
            Recovery::Request {
                idr_pic_id,
                last_correct,
            } => {
                let mut request: openh264_sys2::SLTRRecoverRequest = unsafe { std::mem::zeroed() };
                request.uiFeedbackType = openh264_sys2::LTR_RECOVERY_REQUEST as u32;
                request.uiIDRPicId = idr_pic_id;
                request.iLastCorrectFrameNum = last_correct;
                // -1: the decoder's own frame is unknown, which OpenH264 takes as a loss at once.
                request.iCurrentFrameNum = -1;
                let ret = unsafe {
                    self.encoder.raw_api().set_option(
                        openh264_sys2::ENCODER_LTR_RECOVERY_REQUEST,
                        std::ptr::addr_of_mut!(request).cast(),
                    )
                };
                ret == 0
            }
        }
    }

    fn feedback(
        &mut self,
        kind: openh264_sys2::KEY_FRAME_REQUEST_TYPE,
        idr_pic_id: u32,
        frame_num: i32,
    ) {
        let mut fb: openh264_sys2::SLTRMarkingFeedback = unsafe { std::mem::zeroed() };
        fb.uiFeedbackType = kind as u32;
        fb.uiIDRPicId = idr_pic_id;
        fb.iLTRFrameNum = frame_num;
        unsafe {
            self.encoder.raw_api().set_option(
                openh264_sys2::ENCODER_LTR_MARKING_FEEDBACK,
                std::ptr::addr_of_mut!(fb).cast(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a busy, motion-carrying test frame (high-entropy per-pixel content) so the
    /// rate controller produces non-trivial P-frames. Fine under CBR (the RC raises QP to fit) but
    /// pathological for a pinned low QP, so the CRF test uses the compressible `gradient_frame`.
    fn busy_frame(w: usize, h: usize, t: usize) -> Vec<u8> {
        let mut f = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                let v = (((x + t) * 7 + (y + t) * 13) & 0xFF) as u8;
                f[i] = v;
                f[i + 1] = v.wrapping_mul(3);
                f[i + 2] = v.wrapping_add((x + t) as u8);
                f[i + 3] = 255;
            }
        }
        f
    }

    /// Build a smooth diagonal-gradient test frame (moderate, realistic screen-like detail):
    /// compressible enough not to overflow at a low pinned QP, yet detailed enough that the QP
    /// visibly scales the output size.
    fn gradient_frame(w: usize, h: usize, t: usize) -> Vec<u8> {
        let mut f = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                let v = ((x * 3 + y * 5 + t * 4) & 0xFF) as u8;
                f[i] = v;
                f[i + 1] = v.wrapping_add(40);
                f[i + 2] = v.wrapping_add(80);
                f[i + 3] = 255;
            }
        }
        f
    }

    /// The mean luma distance between two decoded pictures.
    fn luma_apart(
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

    /// Two consumers share a full-frame session: one takes every frame, the other misses runs and
    /// then every frame predicting from one it lacks, and once it has room reports the first frame
    /// it missed. Every frame both hold is acknowledged, so OpenH264 confirms the long-term
    /// references it marks; the frames marking them are anchors from the first marking on, each
    /// predicting from a frame both hold; the frame after each report predicts from a confirmed
    /// one the one behind holds and is an anchor too; the one behind decodes every frame it is
    /// sent as the other does (OpenH264's decoder, Firefox's for WebRTC H.264 without FFmpeg),
    /// and no frame after the first is a key frame. The runs reach both long-term indices.
    #[test]
    fn a_consumer_that_misses_frames_is_predicted_past_from_a_long_term_reference() {
        use crate::webcam::decode::{Decoder as _, VideoDecoder};
        let (w, h) = (320usize, 192usize);
        let s = CaptureSettings {
            width: w as i32,
            height: h as i32,
            target_fps: 30.0,
            video_cbr_mode: true,
            video_bitrate_kbps: 4000,
            ..Default::default()
        };
        let mut enc = Openh264Encoder::new(&s).expect("openh264 init");
        let (mut whole, mut part) = (
            VideoDecoder::new(Codec::H264).unwrap(),
            VideoDecoder::new(Codec::H264).unwrap(),
        );
        // (frames it has no room for, the frame its report goes ahead of)
        let runs = [
            (50..=60, 63u16),
            (101..=104, 106),
            (140..=150, 152),
            (190..=191, 194),
        ];
        let mut holds = std::collections::HashMap::new();
        let mut first_missed = None;
        let mut recovered = Vec::new();
        let mut anchors = Vec::new();
        for i in 0..=230u16 {
            if runs.iter().any(|r| r.1 == i) {
                let first = first_missed.take().expect("a missing run");
                assert!(
                    enc.invalidate_reference(first),
                    "{first} reported lost before {i}"
                );
            }
            let out = enc
                .encode_host_argb(
                    &gradient_frame(w, h, i as usize),
                    w * 4,
                    i as u64,
                    i == 0,
                    false,
                )
                .expect("encode");
            if out.is_empty() {
                continue;
            }
            assert_eq!(
                out[1] & 0x0f & !FRAME_ANCHOR == FRAME_KEY,
                i == 0,
                "frame {i} kind"
            );
            let reference = enc.last_reference();
            let can = match reference {
                Reference::None => true,
                Reference::Frame(r) => holds.get(&r).copied().unwrap_or(false),
                Reference::Untracked => false,
            };
            let sent = can && !runs.iter().any(|r| r.0.contains(&i));
            if out[1] & FRAME_ANCHOR != 0 {
                assert!(
                    can,
                    "anchor {i} predicts from {reference:?}, which the one behind lacks"
                );
                anchors.push(i);
            }
            let bits = &out[VIDEO_HEADER_LEN..];
            assert!(whole.decode(bits).unwrap_or(false), "frame {i}");
            if runs.iter().any(|r| r.1 == i) {
                assert!(
                    sent,
                    "frame {i} after the report predicts from {reference:?}"
                );
                assert!(out[1] & FRAME_ANCHOR != 0, "frame {i} is an anchor");
                recovered.push((i, reference));
            }
            if sent {
                assert!(part.decode(bits).unwrap_or(false), "frame {i} sent");
                let off = luma_apart(&whole.frame().unwrap(), &part.frame().unwrap());
                assert!(off < 0.5, "frame {i} sent is {off:.3} off");
                enc.acknowledge_reference(i, true);
            } else if first_missed.is_none() {
                first_missed = Some(i);
            }
            holds.insert(i, sent);
        }
        println!("recoveries (frame, predicting from): {recovered:?}; anchors {anchors:?}");
        assert!(
            anchors.iter().any(|&a| a < 50),
            "no marking anchor before the first loss: {anchors:?}"
        );
        assert_eq!(recovered.len(), runs.len(), "{recovered:?}");
        let distinct: std::collections::HashSet<i32> =
            recovered.iter().map(|r| r.1.frame_id()).collect();
        assert!(
            distinct.len() >= 2,
            "recoveries from one long-term frame only: {recovered:?}"
        );
    }

    /// The stream declares the matrix its input was converted with: BT.709 at limited range.
    /// A full-frame session encodes on the H.264 thread policy over four slices, converting in a
    /// band a thread; without `cpu_encode_threading` on one thread and in one band, the four
    /// slices kept for the client's decoder.
    #[test]
    fn cpu_encode_threading_off_converts_in_one_band() {
        let off = CaptureSettings {
            width: 320,
            height: 192,
            target_fps: 30.0,
            cpu_encode_threading: false,
            ..Default::default()
        };
        let enc = Openh264Encoder::new(&off).expect("openh264 init");
        assert_eq!((enc.threads, enc.slices, enc.csc_bands), (1, 4, 1));
        let on = CaptureSettings {
            cpu_encode_threading: true,
            ..off
        };
        let threads = super::super::session::h264_fullframe_threads(&on) as u16;
        let enc = Openh264Encoder::new(&on).expect("openh264 init");
        assert_eq!((enc.threads, enc.slices), (threads, 4));
        assert_eq!(enc.csc_bands, if threads > 1 { 4 } else { 1 });
    }

    #[test]
    fn declares_the_conversion_matrix() {
        use crate::webcam::decode::{ColorTags, Decoder, VideoDecoder};
        let s = CaptureSettings {
            width: 128,
            height: 96,
            target_fps: 30.0,
            ..Default::default()
        };
        let mut enc = Openh264Encoder::new(&s).expect("openh264 init");
        let idr = enc
            .encode_host_argb(&busy_frame(128, 96, 0), 128 * 4, 0, true, false)
            .expect("encode");
        let mut dec = VideoDecoder::new(Codec::H264).expect("decoder");
        assert!(dec.decode(&idr[VIDEO_HEADER_LEN..]).expect("decode"));
        assert_eq!(dec.color_tags(), Some(ColorTags::BT709_LIMITED));
    }

    /// The stream bounds reordering at zero at a constant quantizer and a constant rate.
    #[test]
    fn bounds_reordering_at_zero() {
        use crate::encoders::sps::fixtures::assert_no_reorder;
        for cbr in [false, true] {
            let s = CaptureSettings {
                width: 320,
                height: 192,
                target_fps: 30.0,
                video_cbr_mode: cbr,
                video_bitrate_kbps: 2000,
                ..Default::default()
            };
            let mut enc = Openh264Encoder::new(&s).expect("openh264 init");
            let idr = enc
                .encode_host_argb(&busy_frame(320, 192, 0), 320 * 4, 0, true, false)
                .expect("encode");
            assert_no_reorder(&idr[VIDEO_HEADER_LEN..], &format!("OpenH264 cbr {cbr}"));
        }
    }

    /// The color chart, handed to the encoder as host ARGB, decodes back to the color that was
    /// painted when the BT.709 the stream declares is inverted — the check a client's
    /// presentation path performs on every frame, here with no browser in the way.
    #[test]
    fn paints_the_chart_it_converts() {
        use crate::encoders::chroma_siting::{BT709, chart_bgra, chart_error};
        use crate::webcam::decode::{Decoder, VideoDecoder};
        let (w, h) = (256usize, 128usize);
        let s = CaptureSettings {
            width: w as i32,
            height: h as i32,
            target_fps: 30.0,
            video_crf: 20,
            ..Default::default()
        };
        let mut enc = Openh264Encoder::new(&s).expect("openh264 init");
        let idr = enc
            .encode_host_argb(&chart_bgra(w, h), w * 4, 0, true, false)
            .expect("encode");
        let mut dec = VideoDecoder::new(Codec::H264).expect("decoder");
        assert!(dec.decode(&idr[VIDEO_HEADER_LEN..]).expect("decode"));
        let worst = chart_error(&dec.frame().expect("frame"), BT709);
        println!("[chart] OpenH264: worst |dRGB| {worst:.1}");
        assert!(
            worst <= 12.0,
            "the OpenH264 path paints {worst:.1} off the chart"
        );
    }

    /// A forced first frame is emitted as a typed IDR with a valid wire header and Annex-B
    /// payload; re-encoding identical content (no scene change) yields a typed delta (P) frame
    /// whose header carries the passed frame number.
    #[test]
    fn encodes_annexb_idr_then_p() {
        let s = CaptureSettings {
            width: 128,
            height: 96,
            video_bitrate_kbps: 2000,
            target_fps: 30.0,
            ..Default::default()
        };
        let mut enc = Openh264Encoder::new(&s).expect("openh264 init");
        let stride = 128 * 4;
        let idr = enc
            .encode_host_argb(&busy_frame(128, 96, 0), stride, 0, true, false)
            .expect("encode idr");
        assert!(idr.len() > 10, "IDR frame should produce output");
        assert_eq!(idr[0], 0x04);
        assert_eq!(
            idr[1], 0x11,
            "forced first frame must be typed an H.264 key frame"
        );
        assert_eq!(&idr[2..6], &[0, 0, 0, 0], "frame_id 0 and y_start 0");
        assert_eq!(&idr[6..10], &[0, 128, 0, 96], "width/height big-endian");
        assert!(
            idr[VIDEO_HEADER_LEN..].starts_with(&[0, 0, 0, 1])
                || idr[VIDEO_HEADER_LEN..].starts_with(&[0, 0, 1]),
            "payload must be Annex-B (start code prefixed)"
        );
        let p = enc
            .encode_host_argb(&busy_frame(128, 96, 0), stride, 7, false, false)
            .expect("encode p");
        assert!(p.len() > 10, "second frame should produce output");
        assert_eq!(
            p[1], 0x10,
            "unforced static second frame must be typed an H.264 delta"
        );
        assert_eq!(&p[2..4], &[0, 7], "frame_id must come from frame_number");
    }

    /// With `omit_stripe_headers` set, the output is bare Annex-B (start-code prefixed) with
    /// no wire header.
    #[test]
    fn omit_stripe_headers_yields_bare_annexb() {
        let s = CaptureSettings {
            width: 128,
            height: 96,
            video_bitrate_kbps: 2000,
            target_fps: 30.0,
            omit_stripe_headers: true,
            ..Default::default()
        };
        let mut enc = Openh264Encoder::new(&s).expect("openh264 init");
        let out = enc
            .encode_host_argb(&busy_frame(128, 96, 0), 128 * 4, 0, true, false)
            .expect("encode");
        assert!(
            out.starts_with(&[0, 0, 0, 1]) || out.starts_with(&[0, 0, 1]),
            "omit_stripe_headers output must be bare Annex-B"
        );
    }

    /// In CBR mode a lower target bitrate compresses harder: summed output over a busy
    /// sequence at 200 kbps is smaller than at 8000 kbps.
    #[test]
    fn lower_bitrate_yields_smaller_output() {
        let (w, h, stride) = (256usize, 192usize, 256 * 4);
        let encode_run = |kbps: i32| -> usize {
            let s = CaptureSettings {
                width: w as i32,
                height: h as i32,
                video_cbr_mode: true,
                video_bitrate_kbps: kbps,
                target_fps: 30.0,
                ..Default::default()
            };
            let mut e = Openh264Encoder::new(&s).unwrap();
            let _ = e
                .encode_host_argb(&busy_frame(w, h, 0), stride, 0, true, false)
                .unwrap();
            (1..24)
                .map(|t| {
                    e.encode_host_argb(&busy_frame(w, h, t), stride, t as u64, false, false)
                        .unwrap()
                        .len()
                })
                .sum()
        };
        let high = encode_run(8000);
        let low = encode_run(200);
        assert!(
            low < high,
            "lower target bitrate should compress harder (low={low}, high={high})"
        );
    }

    /// Dropping the CBR target live via `reconfigure_rate` (as the web UI slider does)
    /// shrinks subsequent output versus the initial higher-bitrate run.
    #[test]
    fn live_reconfigure_rate_takes_effect() {
        let (w, h, stride) = (256usize, 192usize, 256 * 4);
        let s = CaptureSettings {
            width: w as i32,
            height: h as i32,
            video_cbr_mode: true,
            video_bitrate_kbps: 8000,
            target_fps: 30.0,
            ..Default::default()
        };
        let mut e = Openh264Encoder::new(&s).unwrap();
        let _ = e
            .encode_host_argb(&busy_frame(w, h, 0), stride, 0, true, false)
            .unwrap();
        let high: usize = (1..24)
            .map(|t| {
                e.encode_host_argb(&busy_frame(w, h, t), stride, t as u64, false, false)
                    .unwrap()
                    .len()
            })
            .sum();
        e.reconfigure_rate(200, 30.0);
        let low: usize = (24..48)
            .map(|t| {
                e.encode_host_argb(&busy_frame(w, h, t), stride, t as u64, false, false)
                    .unwrap()
                    .len()
            })
            .sum();
        assert!(
            low < high,
            "live bitrate drop should shrink output (low={low}, high={high})"
        );
    }

    /// Raising the CBR target live above the session's initial bitrate is accepted (not
    /// rejected) — which requires lifting OpenH264's max-bitrate ceiling that construction pins to
    /// the starting target — and grows subsequent output well beyond the initial low-bitrate run.
    #[test]
    fn live_reconfigure_rate_raise_takes_effect() {
        let (w, h, stride) = (256usize, 192usize, 256 * 4);
        let s = CaptureSettings {
            width: w as i32,
            height: h as i32,
            video_cbr_mode: true,
            video_bitrate_kbps: 300,
            target_fps: 30.0,
            ..Default::default()
        };
        let mut e = Openh264Encoder::new(&s).unwrap();
        let _ = e
            .encode_host_argb(&busy_frame(w, h, 0), stride, 0, true, false)
            .unwrap();
        let low: usize = (1..24)
            .map(|t| {
                e.encode_host_argb(&busy_frame(w, h, t), stride, t as u64, false, false)
                    .unwrap()
                    .len()
            })
            .sum();
        e.reconfigure_rate(8000, 30.0);
        assert_eq!(
            e.current_bitrate_bps, 8_000_000,
            "live raise must be accepted, not rejected"
        );
        let high: usize = (24..48)
            .map(|t| {
                e.encode_host_argb(&busy_frame(w, h, t), stride, t as u64, false, false)
                    .unwrap()
                    .len()
            })
            .sum();
        assert!(
            high > low * 3 / 2,
            "live bitrate raise should grow output (low={low}, high={high})"
        );
    }

    /// In CRF/CQP mode (bitrate-mode RC with a pinned `min == max` QP), a higher `video_crf`
    /// — a higher constant QP, stronger compression — produces smaller output than a lower CRF,
    /// confirming the constant QP is actually applied. Uses the compressible gradient frame so a
    /// low pinned QP does not overflow.
    #[test]
    fn crf_mode_qp_scales_output() {
        let (w, h, stride) = (256usize, 192usize, 256 * 4);
        let run = |crf: i32| -> usize {
            let s = CaptureSettings {
                width: w as i32,
                height: h as i32,
                video_cbr_mode: false,
                video_crf: crf,
                target_fps: 30.0,
                ..Default::default()
            };
            let mut e = Openh264Encoder::new(&s).unwrap();
            let mut total = e
                .encode_host_argb(&gradient_frame(w, h, 0), stride, 0, true, false)
                .unwrap()
                .len();
            total += (1..24)
                .map(|t| {
                    e.encode_host_argb(&gradient_frame(w, h, t), stride, t as u64, false, false)
                        .unwrap()
                        .len()
                })
                .sum::<usize>();
            total
        };
        let high_quality = run(18);
        let low_quality = run(40);
        assert!(
            low_quality < high_quality,
            "higher CRF must compress harder (crf40={low_quality}, crf18={high_quality})"
        );
    }

    /// A live CRF change through `update_qp` really moves the quantizer: a drop to a lower QP
    /// (better quality) applies at once and grows subsequent output, while an immediate request to
    /// go back up is held by the hysteresis instead of rebuilding on a transient.
    ///
    /// The two windows are equal-length runs of keyframes: the rebuild `update_qp` performs forces
    /// an IDR of its own, and a window of P frames would compare motion residuals — nearly free on
    /// a translating gradient — instead of the quantizer. Measuring intra-coded pictures on both
    /// sides makes the size difference attributable to the QP alone, by a margin far wider than
    /// frame-to-frame noise.
    #[test]
    fn live_update_qp_takes_effect() {
        let (w, h, stride) = (256usize, 192usize, 256 * 4);
        let s = CaptureSettings {
            width: w as i32,
            height: h as i32,
            video_cbr_mode: false,
            video_crf: 40,
            target_fps: 30.0,
            ..Default::default()
        };
        let mut e = Openh264Encoder::new(&s).unwrap();
        let idr_run = |e: &mut Openh264Encoder, range: std::ops::Range<usize>| -> usize {
            range
                .map(|t| {
                    e.encode_host_argb(&gradient_frame(w, h, t), stride, t as u64, true, false)
                        .unwrap()
                        .len()
                })
                .sum()
        };
        let coarse = idr_run(&mut e, 0..8);
        e.update_qp(18);
        assert_eq!(e.current_qp, 18, "a QP decrease must apply immediately");
        let fine = idr_run(&mut e, 8..16);
        assert!(
            fine > coarse * 2,
            "a lower live QP should grow output well beyond the coarse run (qp18={fine}, qp40={coarse})"
        );
        e.update_qp(40);
        assert_eq!(
            e.current_qp, 18,
            "a single QP increase must wait out the hysteresis"
        );
    }

    /// Both input byte orders encode valid, wire-headered Annex-B: the Wayland GLES readback
    /// delivers RGBA and X11 delivers BGRA, so `encode_host_argb` is exercised with `rgba_input`
    /// both true and false (color correctness is verified end-to-end, not here).
    #[test]
    fn rgba_and_bgra_inputs_both_encode() {
        let (w, h, stride) = (128usize, 96usize, 128 * 4);
        let s = CaptureSettings {
            width: w as i32,
            height: h as i32,
            video_bitrate_kbps: 2000,
            target_fps: 30.0,
            ..Default::default()
        };
        let frame = busy_frame(w, h, 0);
        for rgba in [false, true] {
            let mut e = Openh264Encoder::new(&s).unwrap();
            let out = e
                .encode_host_argb(&frame, stride, 0, true, rgba)
                .expect("encode");
            assert!(out.len() > 10, "rgba_input={rgba} must produce output");
            assert_eq!(
                out[0], 0x04,
                "rgba_input={rgba} output must carry the wire header"
            );
            assert!(
                out[VIDEO_HEADER_LEN..].starts_with(&[0, 0, 0, 1])
                    || out[VIDEO_HEADER_LEN..].starts_with(&[0, 0, 1]),
                "rgba_input={rgba} payload must be Annex-B"
            );
        }
    }
}

#[cfg(test)]
mod slice_tests {
    use super::*;

    /// Number of Annex-B start codes in an encoded buffer, header included.
    fn count_nals(out: &[u8]) -> usize {
        let mut nals = 0;
        let mut i = 0;
        while i + 3 < out.len() {
            if &out[i..i + 3] == b"\x00\x00\x01" {
                nals += 1;
                i += 3;
            } else {
                i += 1;
            }
        }
        nals
    }

    /// A single IDR frame carries at least six NALs (SPS + PPS + four IDR slices),
    /// confirming the fixed four-slice configuration primed by `enable_slices`.
    #[test]
    fn four_slices_per_frame() {
        let s = CaptureSettings {
            width: 640,
            height: 480,
            codec: Codec::H264,
            video_bitrate_kbps: 2000,
            video_cbr_mode: true,
            ..Default::default()
        };
        let mut enc = Openh264Encoder::new(&s).expect("encoder");
        let frame = vec![0x80u8; 640 * 480 * 4];
        let out = enc
            .encode_host_argb(&frame, 640 * 4, 0, true, false)
            .expect("encode");
        let nals = count_nals(&out);
        assert!(nals >= 6, "expected 4-slice IDR (>=6 NALs), got {nals}");
    }

    /// Build a deterministic per-frame noise image — the worst case for rate control
    /// (incompressible, and screen-mode scene-change detection fires on every frame).
    fn noise_frame(w: usize, h: usize, seed: u32) -> Vec<u8> {
        let mut x = seed.wrapping_mul(2654435761).max(1);
        let mut v = vec![0u8; w * h * 4];
        for px in v.as_chunks_mut::<4>().0 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            px[0] = x as u8;
            px[1] = (x >> 8) as u8;
            px[2] = (x >> 16) as u8;
            px[3] = 255;
        }
        v
    }

    /// Diagnostic that the CBR rate controller responds to its target on incompressible
    /// noise, and that QP pinning works.
    ///
    /// Runs 60 noise frames each at CBR 4 Mbps / 500 kbps and at CRF QP51 / QP25, printing
    /// bytes-per-frame and IDR counts, then asserts two invariants:
    ///
    /// 1. **CBR responds to its target**: 500 kbps must emit far fewer bytes/frame than 4 Mbps.
    ///    This guards the explicit QP range set in `new` — a zero anywhere in the configured QP pair
    ///    makes OpenH264 substitute its `[26, 35]` screen defaults, under which noise emits the same
    ///    bytes at any target.
    /// 2. **QP pinning**: QP51 must compress far harder than QP25.
    ///
    /// The RC still cannot reach the QP51 floor on this content: screen mode force-enables
    /// scene-change detection (so every noise frame becomes an IDR) and `RcCalculateIdrQp` clamps
    /// IDR QP to `<= 40` regardless of the session max — an upstream RC ceiling, not a config
    /// defect.
    #[test]
    fn cbr_rc_diagnostic_on_noise() {
        let (w, h) = (1280usize, 720usize);
        let stride = w * 4;
        let run = |cbr: bool, kbps: i32, crf: i32| -> (usize, usize) {
            let s = CaptureSettings {
                width: w as i32,
                height: h as i32,
                video_cbr_mode: cbr,
                video_bitrate_kbps: kbps,
                video_crf: crf,
                target_fps: 60.0,
                ..Default::default()
            };
            let mut e = Openh264Encoder::new(&s).unwrap();
            let mut total = 0usize;
            let mut idrs = 0usize;
            for t in 0..60u64 {
                let out = e
                    .encode_host_argb(&noise_frame(w, h, t as u32), stride, t, t == 0, false)
                    .unwrap();
                total += out.len();
                for i in 0..out.len().saturating_sub(4) {
                    if out[i] == 0 && out[i + 1] == 0 && out[i + 2] == 1 && (out[i + 3] & 0x1F) == 5
                    {
                        idrs += 1;
                        break;
                    }
                }
            }
            (total / 60, idrs)
        };
        let (cbr4m, idr4m) = run(true, 4000, 25);
        let (cbr500k, _) = run(true, 500, 25);
        let (qp51, _) = run(false, 1, 51);
        let (qp25, _) = run(false, 1, 25);
        println!(
            "noise 720p60 bytes/frame: CBR-4Mbps={cbr4m} (IDR frames {idr4m}/60) \
             CBR-500kbps={cbr500k} QP51-floor={qp51} QP25={qp25}"
        );
        assert!(
            cbr500k * 4 < cbr4m * 3,
            "CBR-on-noise no longer responds to its target: 500kbps={cbr500k} B/f vs 4Mbps={cbr4m} B/f \
             (QP51 floor {qp51}, QP25 {qp25})"
        );
        assert!(
            qp51 * 4 < qp25,
            "QP pinning sanity: 51 must compress far harder than 25"
        );
    }

    /// The four-slice re-init in `enable_slices` preserves the CBR rate-control
    /// parameters: after construction the live `SEncParamExt` still shows the target bitrate, frame
    /// rate, QP headroom (max 51), and enabled frame-skip intact.
    #[test]
    fn four_slice_reinit_preserves_rc_params() {
        let (w, h) = (1280usize, 720usize);
        let s = CaptureSettings {
            width: w as i32,
            height: h as i32,
            video_cbr_mode: true,
            video_bitrate_kbps: 4000,
            target_fps: 60.0,
            ..Default::default()
        };
        let mut e = Openh264Encoder::new(&s).unwrap();
        unsafe {
            let mut p: openh264_sys2::SEncParamExt = std::mem::zeroed();
            let ret = e.encoder.raw_api().get_option(
                openh264_sys2::ENCODER_OPTION_SVC_ENCODE_PARAM_EXT,
                &mut p as *mut _ as *mut std::ffi::c_void,
            );
            assert_eq!(ret, 0, "GetOption(SVC_ENCODE_PARAM_EXT)");
            println!(
                "post-init params: iRCMode={} iTargetBitrate={} iMaxBitrate={} fMaxFrameRate={} \
                 iMaxQp={} iMinQp={} bEnableFrameSkip={} uiIntraPeriod={} layer0(target={} max={})",
                p.iRCMode,
                p.iTargetBitrate,
                p.iMaxBitrate,
                p.fMaxFrameRate,
                p.iMaxQp,
                p.iMinQp,
                p.bEnableFrameSkip,
                p.uiIntraPeriod,
                p.sSpatialLayers[0].iSpatialBitrate,
                p.sSpatialLayers[0].iMaxSpatialBitrate,
            );
            assert_eq!(
                p.iTargetBitrate, 4_000_000,
                "target bitrate survives the 4-slice re-init"
            );
            assert!(
                p.fMaxFrameRate > 1.0,
                "frame rate survives the 4-slice re-init"
            );
            assert_eq!(p.iMaxQp, 51, "RC QP headroom survives the 4-slice re-init");
            assert!(
                p.bEnableFrameSkip,
                "CBR enables frame skip so the bitrate is actually held"
            );
        }
    }
}

#[cfg(test)]
mod rebuild_cost {
    use super::*;

    /// Measure that constructing the encoder at 1080p is cheap — on the order of
    /// milliseconds.
    ///
    /// OpenH264 has no in-place resolution change (its `SetOption(SVC_ENCODE_PARAM_EXT)`
    /// re-initializes the encoder core internally), so a resolution change costs a full rebuild;
    /// this test measures that construction cost. It times both the `new` construction (`init_ms`)
    /// and the first-frame encode (`first_ms`) at 1920x1080, prints both, and asserts the
    /// fastest of up to five constructions stays under a second: a rebuild gone pathological is
    /// slow every time, while a loaded host stretches single ones past that.
    #[test]
    fn construction_cost_is_milliseconds() {
        let s = CaptureSettings {
            width: 1920,
            height: 1080,
            codec: Codec::H264,
            video_bitrate_kbps: 8000,
            ..Default::default()
        };
        let mut init_ms = f64::INFINITY;
        let mut built = None;
        for _ in 0..5 {
            let t = std::time::Instant::now();
            built = Some(Openh264Encoder::new(&s).expect("init"));
            init_ms = init_ms.min(t.elapsed().as_secs_f64() * 1000.0);
            if init_ms < 1000.0 {
                break;
            }
        }
        let mut e = built.expect("init");
        let frame = vec![128u8; 1920 * 1080 * 4];
        let t = std::time::Instant::now();
        let out = e
            .encode_host_argb(&frame, 1920 * 4, 0, true, false)
            .expect("encode");
        let first_ms = t.elapsed().as_secs_f64() * 1000.0;
        assert!(!out.is_empty());
        println!("openh264 1080p init={init_ms:.1}ms first_frame={first_ms:.1}ms");
        // The bound is the one the check is for: a rebuild gone pathological takes seconds,
        // so a second is the line. A tighter one fails on host contention alone -- 260ms on a
        // loaded builder -- which reports nothing about the encoder and costs a red suite.
        assert!(
            init_ms < 1000.0,
            "OpenH264 init unexpectedly slow: {init_ms}ms"
        );
    }
}
