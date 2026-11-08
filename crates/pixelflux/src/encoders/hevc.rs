/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Software HEVC: x265 with the `gpl` feature, kvazaar without it, each at the fastest setting
//! its library offers in real time (ultrafast, wavefront threads, no lookahead, no reordering)
//! with the parameter sets repeated on every key frame. x265 carries 4:4:4 at full range like
//! x264; kvazaar encodes 4:2:0. Neither library lets a session steer what a frame predicts
//! from, so the session names no reference and refuses an invalidation, and the caller codes
//! a key frame instead.
//!
//! x265 encodes a picture laid out as its own frame where it lies (`x265_param_input_layout`,
//! `copy-pic=0`; the patches under `patches/x265`), two pictures converted into in turn since
//! it reads one through the encode it is passed to; the blocks the ledger marks still and clean
//! reach it as its constant-block map (`constant_blocks`, `x265_picture.constantBlocks`), coded
//! as skips without analysis and keeping their lowres intra estimates; and its per-block
//! quantizer offsets apply without adaptive quantization's variance pass (`qp-offsets`). A
//! still 1080p screen took 2.8 ms a frame on eight threads where it took 5.7, and 2560x1600
//! 4.6 where it took 10.8. kvazaar converts into the picture it allocates and encodes from.

use std::ffi::CString;

use super::codec::{Codec, VIDEO_HEADER_LEN, h265_frame_type, push_video_header};
use super::reference::Reference;
use super::session::{Pending, Planes, Quality, RateSettings, encode_threads};
use crate::CaptureSettings;

/// One software HEVC session for one capture.
pub struct HevcEncoder {
    backend: Backend,
    planes: Planes,
    threads: i32,
    quality: Quality,
    rate: RateSettings,
    omit_headers: bool,
    /// Whether the next frame is the first after an open, which the encoder codes as a key
    /// frame on its own.
    fresh: bool,
    next_pts: u64,
    pending: Pending,
    /// The quantizer the next frame is held at whatever the rate control (`hold_quantizer`).
    held: Option<u32>,
    /// The blocks the next frame of the rate control's refines (`refine_blocks`), and the
    /// quantizer offset of each 16x16 block of every frame, 0 where it refines nothing.
    refinement: Option<crate::ledger::Refinement>,
    offsets: Vec<f32>,
    /// The 16x16 blocks of the next frame that held still and are clean, one byte each in
    /// raster order, nonzero where constant (`constant_blocks`); empty where none is.
    constant: Vec<u8>,
    /// The quantizer the rate control last coded a frame at, held frames aside.
    last_quality: Option<u32>,
    /// The bytes of that frame.
    last_bytes: Option<usize>,
}

unsafe impl Send for HevcEncoder {}

/// `HevcEncoder::reach_finer`: the predicted frames of either library at a quantizer leave the
/// motion's coarse blocks as they are. x265's (4.2) cleanup of 1080p text ended at 45.0-46.3 dB
/// at the paint-over quantizer and at 49.7-52.4 six steps finer at 1 and 2 Mbit/s, where its key
/// frame at the paint-over quantizer decodes at 50.0, on a fifth to three fifths more bytes over
/// a few seconds more (twelve came to 50.4-55.9 over up to 29 s); kvazaar's at 8 fps 0.5 and 1
/// Mbit/s at 44.4-45.0 against 47.4-47.9, its rate control settling about the paint-over
/// quantizer at 2 Mbit/s whatever the cleanup asks (44.8-46.8 dB either way).
const HEVC_REACH_FINER: u32 = 6;

/// Whether the build's software HEVC encoder codes 10 bits: x265 where it was built with its
/// 10-bit encoder linked in.
pub fn ten_bit() -> bool {
    Backend::ten_bit()
}

/// What a backend answered for one submitted picture, with the quantizer it was coded at.
struct Coded {
    bytes: Vec<u8>,
    pts: u64,
    qp: u32,
    /// The bytes the rate control spent on the frame, the filler it pads a constant rate with
    /// included, which `bytes` leaves out.
    spent: usize,
}

impl HevcEncoder {
    /// Open a session on host frames; `rgba` names the byte order they arrive in.
    pub fn new(settings: &CaptureSettings, rgba: bool) -> Result<Self, String> {
        let _ = rgba;
        let fullcolor = settings.video_fullcolor && Backend::FULLCOLOR;
        let bit_depth = if settings.video_bit_depth >= 10 && Backend::ten_bit() {
            10
        } else {
            8
        };
        let threads = encode_threads(settings);
        let rate = RateSettings::new(settings);
        let quality = Quality::new(Codec::H265.quantizer(settings.video_crf));
        let width = settings.width.max(1) as usize;
        let height = settings.height.max(1) as usize;
        let backend = Backend::open(
            width,
            height,
            threads,
            fullcolor,
            bit_depth,
            rate,
            quality.current,
        )?;
        Ok(Self {
            backend,
            planes: Planes::new(width, height, fullcolor, bit_depth),
            threads,
            quality,
            rate,
            omit_headers: settings.omit_stripe_headers,
            fresh: true,
            next_pts: 0,
            pending: Pending::default(),
            held: None,
            refinement: None,
            offsets: Vec::new(),
            constant: Vec::new(),
            last_quality: None,
            last_bytes: None,
        })
    }

    pub fn codec(&self) -> Codec {
        Codec::H265
    }

    /// The library behind the session, as the logs name it.
    pub fn library(&self) -> &'static str {
        Backend::LIBRARY
    }

    pub fn is_fullcolor(&self) -> bool {
        self.planes.i444
    }

    /// The bits per sample the session codes.
    pub fn bit_depth(&self) -> u32 {
        self.planes.bit_depth
    }

    /// The software 4:4:4 of x264's kind signals full range.
    pub fn is_full_range(&self) -> bool {
        self.planes.i444
    }

    pub fn last_reference(&self) -> Reference {
        Reference::Untracked
    }

    /// Whether `hold_quantizer` holds a frame at its quantizer: x265 at a constant rate factor,
    /// kvazaar never. At a constant rate x265's rate control moves a forced quantizer back to
    /// what its buffer affords (a key frame held at 18 came out 23.2 kB at 26.6 dB at 8 Mbit/s
    /// 1080p), and a fresh session whose buffer is the key frame's budget plans it from a
    /// fraction of that buffer (102.5 kB at 34 dB where the budget was 250 kB and the picture
    /// before it 53.9 dB), so a constant-rate session holds none.
    pub fn holds_quantizer(&self) -> bool {
        Backend::HOLDS_QUANTIZER && !self.rate.cbr
    }

    /// The quantizer steps finer than the paint-over one at which a constant rate's frames come
    /// to the paint-over quality (`FrameEncoder::reach_finer`): `HEVC_REACH_FINER`.
    pub fn reach_finer(&self) -> u32 {
        if self.rate.cbr { HEVC_REACH_FINER } else { 0 }
    }

    /// Whether the session refines blocks of a frame of its rate control's (`refine_blocks`):
    /// x265, through its per-block quantizer offsets, and kvazaar, through its map of a delta a
    /// 64x64 tree block, the finest of the blocks it holds.
    pub fn refines(&self) -> bool {
        Backend::REFINES
    }

    /// Quality index steps finer than the paint-over one a refined block is coded at
    /// (`FrameEncoder::refines`): the library's (`Backend::REFINE_FINER`).
    pub fn refine_finer(&self) -> u32 {
        Backend::REFINE_FINER
    }

    /// Code the blocks `refinement` names in the next frame of the rate control's at its target
    /// quality (`ledger`): each 16x16 block it refines at the quantizer steps from the rate
    /// control's last to the target, off the frame's own.
    pub fn refine_blocks(&mut self, refinement: &crate::ledger::Refinement) {
        self.refinement = self.refines().then(|| refinement.clone());
    }

    /// Mark the blocks of the next frame, in raster order, that held still and are clean
    /// (`BlockLedger::constant`), which x265 codes as skips without analysis
    /// (`x265_picture.constantBlocks`) on a frame of its rate control's; kvazaar takes no such
    /// map. An empty slice marks none.
    pub fn constant_blocks(&mut self, constant: &[bool]) {
        let blocks = self.planes.width.div_ceil(16) * self.planes.height.div_ceil(16);
        self.constant.clear();
        if Backend::CONSTANT_BLOCKS && constant.len() == blocks && constant.contains(&true) {
            self.constant.extend(constant.iter().map(|&c| u8::from(c)));
        }
    }

    /// The quality index (HEVC's quantizer) the rate control last coded a frame at, held frames
    /// aside.
    pub fn last_quality(&self) -> Option<u32> {
        self.last_quality
    }

    /// The bytes the rate control spent on its last frame, held frames aside: its filler counted,
    /// as the rate control counts it, though the frame goes without it (`Coded::spent`).
    pub fn last_size(&self) -> Option<usize> {
        self.last_bytes
    }

    /// The library keeps its reference lists to itself: refused, so the caller codes a key frame.
    pub fn invalidate_reference(&mut self, _frame_id: u16) -> bool {
        false
    }

    /// Open a fresh encoder with the live settings, whose first frame is a key frame.
    fn reopen(&mut self) -> Result<(), String> {
        self.backend = Backend::open(
            self.planes.width,
            self.planes.height,
            self.threads,
            self.planes.i444,
            self.planes.bit_depth,
            self.rate,
            self.quality.current,
        )?;
        self.fresh = true;
        Ok(())
    }

    /// Encode the next frame at the quantizer the quality index `crf` selects, leaving the
    /// session's own quality for the frame after: the cleanup of a still screen, at a constant
    /// rate factor, where x265 takes it as the frame's forced quantizer. A constant-rate session
    /// codes the frame under its rate control (`holds_quantizer`). kvazaar takes no quantizer
    /// but the one it opens at, and a re-open for the frame would need another to come back,
    /// each a key frame, so it codes the frame at its own.
    pub fn hold_quantizer(&mut self, crf: u32) {
        if !self.rate.cbr {
            self.held = Some(Codec::H265.quantizer(crf as i32));
        }
    }

    /// Re-open the encoder when a rate or frame-rate setting changed: neither library's rate
    /// control takes a new target while it runs.
    pub fn reconfigure_rate(&mut self, settings: &CaptureSettings) -> Result<(), String> {
        let Some(rate) = self.rate.changed(settings) else {
            return Ok(());
        };
        self.rate = rate;
        self.reopen()
    }

    /// Encode one packed host frame at the quality index `crf`, as a key frame when `force_idr`.
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
            && self
                .quality
                .update(Codec::H265.quantizer(crf as i32))
                .is_some()
        {
            self.reopen()?;
        }
        let held = self.held.take();
        if force_idr && !self.fresh && !Backend::KEY_ON_REQUEST {
            self.reopen()?;
        }
        let key = force_idr || self.fresh;
        let full_range = self.is_full_range();
        let pts = self.next_pts;
        let refinement = self.refinement.take().filter(|_| held.is_none() && !key);
        let offset = refinement.as_ref().map_or(0.0, |r| {
            let q = |index: i32| Codec::H265.quantizer(index) as f32;
            (q(r.target) - q(r.from)).min(0.0)
        });
        let cols = self.planes.width.div_ceil(16);
        let rows = self.planes.height.div_ceil(16);
        self.offsets.clear();
        self.offsets.extend((0..rows).flat_map(|row| {
            let refinement = refinement.as_ref();
            (0..cols).map(move |col| {
                if refinement.is_some_and(|r| r.plan.refines(col, row)) {
                    offset
                } else {
                    0.0
                }
            })
        }));
        let constant = (!key && held.is_none() && !self.constant.is_empty())
            .then_some(self.constant.as_mut_slice());
        let coded = self.backend.encode(
            &mut self.planes,
            pixels,
            stride,
            rgba,
            full_range,
            self.threads as usize,
            pts,
            key,
            held,
            Some(self.offsets.as_mut_slice()),
            constant,
        );
        self.constant.clear();
        let coded = coded?;
        self.next_pts += 1;
        self.fresh = false;
        self.pending.push(pts, frame_number as u16);
        let Some(coded) = coded else {
            return Ok(Vec::new());
        };
        if held.is_none() {
            self.last_quality = Some(coded.qp);
            self.last_bytes = Some(coded.spent);
        }
        let id = self.pending.take(coded.pts).unwrap_or(frame_number as u16);
        let mut output = Vec::with_capacity(VIDEO_HEADER_LEN + coded.bytes.len());
        if !self.omit_headers {
            push_video_header(
                &mut output,
                Codec::H265,
                h265_frame_type(&coded.bytes),
                id,
                0,
                self.planes.width as u16,
                self.planes.height as u16,
                Reference::Untracked,
            );
        }
        output.extend_from_slice(&coded.bytes);
        Ok(output)
    }
}

/// The CBR quantizer bounds in HEVC's domain, zero where the settings name none.
fn qp_bounds(rate: RateSettings) -> (u32, u32) {
    (
        Codec::H265.quantizer_bound(rate.min_qp),
        Codec::H265.quantizer_bound(rate.max_qp),
    )
}

#[cfg(not(feature = "gpl"))]
use kvazaar::Backend;
#[cfg(feature = "gpl")]
use x265::Backend;

#[cfg(feature = "gpl")]
mod x265 {
    //! x265 through its versioned API table, configured by name so the session reads no field
    //! of a parameter structure that moves between releases.
    use std::ffi::c_int;
    use std::ptr;

    use codec_sys::x265::*;

    use super::*;

    /// HEVC's filler data NAL unit type (FD_NUT).
    const FILLER_NAL: u32 = 38;

    pub struct Backend {
        api: *const x265_api,
        params: *mut x265_param,
        encoder: *mut x265_encoder,
        /// How the encoder lays out a source picture (`x265_param_input_layout`), which
        /// `pictures` follow so x265 encodes them where they lie (`copy-pic=0`).
        layout: x265_input_layout,
        /// Two pictures, three planes each, converted into in turn: x265 reads a picture through
        /// the encode it is passed to, and the other holds the frame before while it is read.
        /// Each plane is 64-byte aligned within its buffer at `aligned`.
        pictures: [[Vec<u64>; 3]; 2],
        aligned: [[usize; 3]; 2],
        current: usize,
    }

    impl Drop for Backend {
        fn drop(&mut self) {
            unsafe {
                let api = &*self.api;
                if !self.encoder.is_null() {
                    (api.encoder_close.unwrap())(self.encoder);
                }
                if !self.params.is_null() {
                    (api.param_free.unwrap())(self.params);
                }
            }
        }
    }

    impl Backend {
        pub const LIBRARY: &'static str = "x265";
        pub const FULLCOLOR: bool = true;

        /// Whether the linked x265 carries a 10-bit encoder beside its 8-bit one, which is
        /// a property of how the library was built.
        pub fn ten_bit() -> bool {
            !unsafe { x265_api_get(10) }.is_null()
        }

        pub const KEY_ON_REQUEST: bool = true;
        pub const HOLDS_QUANTIZER: bool = true;
        pub const REFINES: bool = true;
        pub const CONSTANT_BLOCKS: bool = true;
        /// Quality index steps finer than the paint-over one x265 codes a refined block at
        /// (`FrameEncoder::refines`). Its ultrafast analysis settles many a still block on its merge
        /// candidate before the offset's quantizer counts, so refining takes 1080p text beside a video
        /// less far than its cleanup of the still screen does, and every step finer gains: at 8 Mbit/s
        /// from 31.1 dB to 34.2, 35.5, and 36.3 at 0, 3, and 6 steps finer, against 46.4 still; at a
        /// constant quality from 37.7 to 40.7, 41.6, and 42.7, against 49.6; the video losing 0.3 dB.
        pub const REFINE_FINER: u32 = 6;

        /// The frame rate the library was configured at, as `(num, den)`.
        #[cfg(test)]
        pub fn frame_rate(&self) -> (u32, u32) {
            unsafe { ((*self.params).fpsNum, (*self.params).fpsDenom) }
        }

        /// The smallest coding unit the library was configured with, in pixels.
        #[cfg(test)]
        pub fn min_cu_size(&self) -> u32 {
            unsafe { (*self.params).minCUSize }
        }

        fn set(&self, name: &str, value: &str) -> Result<(), String> {
            let (n, v) = (CString::new(name).unwrap(), CString::new(value).unwrap());
            match unsafe { ((*self.api).param_parse.unwrap())(self.params, n.as_ptr(), v.as_ptr()) }
            {
                0 => Ok(()),
                X265_PARAM_BAD_NAME => Err(format!("x265 knows no option {name}")),
                _ => Err(format!("x265 refused {name}={value}")),
            }
        }

        /// The rate-control options of `rate` at quantizer `q`: a constant rate held to its
        /// VBV with the quantizer capped at 51, since the library's default ceiling admits
        /// out-of-spec quantizers that only force skips on a VBV underflow; else a constant
        /// rate factor.
        fn rate_options(rate: RateSettings, q: u32) -> Vec<(&'static str, String)> {
            let mut options = Vec::new();
            if rate.cbr {
                let kbps = (rate.bps() / 1000).max(1);
                let vbv_kbit = (rate.vbv() / 1000).max(1);
                options.push(("bitrate", kbps.to_string()));
                options.push(("vbv-maxrate", kbps.to_string()));
                options.push(("vbv-bufsize", vbv_kbit.to_string()));
                if rate.vbv() > 1000 {
                    options.push(("vbv-init", vbv_kbit.to_string()));
                }
                options.push(("strict-cbr", "1".into()));
                let (lo, hi) = qp_bounds(rate);
                if lo > 0 {
                    options.push(("qpmin", lo.to_string()));
                }
                options.push(("qpmax", if hi > 0 { hi } else { 51 }.to_string()));
            } else {
                options.push(("crf", q.to_string()));
            }
            options
        }

        pub fn open(
            width: usize,
            height: usize,
            threads: i32,
            i444: bool,
            bit_depth: u32,
            rate: RateSettings,
            q: u32,
        ) -> Result<Self, String> {
            let api = unsafe { x265_api_get(bit_depth as c_int) };
            if api.is_null() {
                return Err(format!("x265 carries no {bit_depth}-bit encoder"));
            }
            let params = unsafe { ((*api).param_alloc.unwrap())() };
            if params.is_null() {
                return Err("x265 allocated no parameters".into());
            }
            let mut me = Self {
                api,
                params,
                encoder: ptr::null_mut(),
                layout: unsafe { std::mem::zeroed() },
                pictures: Default::default(),
                aligned: [[0; 3]; 2],
                current: 0,
            };
            let (preset, tune) = (c"ultrafast", c"zerolatency");
            if unsafe {
                ((*api).param_default_preset.unwrap())(params, preset.as_ptr(), tune.as_ptr())
            } < 0
            {
                return Err("x265 refused the ultrafast zerolatency preset".into());
            }
            if width < 16 || height < 16 {
                return Err(format!(
                    "x265 encodes no picture as small as {width}x{height}"
                ));
            }
            let mut options: Vec<(&str, String)> = vec![
                ("log-level", "none".into()),
                ("input-res", format!("{width}x{height}")),
                ("input-csp", if i444 { "i444" } else { "i420" }.into()),
                ("fps", format!("{}/{}", rate.fps.num, rate.fps.den)),
                ("range", if i444 { "full" } else { "limited" }.into()),
                ("colorprim", "bt709".into()),
                ("transfer", "bt709".into()),
                ("colormatrix", "bt709".into()),
                ("keyint", "-1".into()),
                ("scenecut", "0".into()),
                ("repeat-headers", "1".into()),
                ("annexb", "1".into()),
                ("aud", "0".into()),
                ("rc-lookahead", "0".into()),
                ("bframes", "0".into()),
                ("frame-threads", "1".into()),
                ("pools", threads.to_string()),
                ("wpp", "1".into()),
            ];
            if width < 32 || height < 32 {
                options.push(("ctu", "16".into()));
            } else if width < 64 || height < 64 {
                options.push(("ctu", "32".into()));
            }
            // ultrafast's 16x16 coding units cannot follow text: a 1080p page's key frame at
            // quantizer 18 came out at 750 kB and 49.3 dB with them, at 395 kB and 53.5 dB with 8x8
            // ones, and a still screen's cleanup at 60 fps 8 Mbit/s ended at 47.6 dB against 53.1
            // on 58% fewer bytes. A 1080p scroll codes as fast, one at 4K a tenth slower.
            options.push(("min-cu-size", "8".into()));
            // A frame's per-block offsets (`refine_blocks`) reach the quantizer only under
            // adaptive quantization; qp-offsets keeps it at strength 0, where its variance pass
            // over every block is skipped and the offsets apply as given.
            options.push(("aq-mode", "1".into()));
            options.push(("aq-strength", "0".into()));
            options.push(("qp-offsets", "1".into()));
            options.push(("copy-pic", "0".into()));
            options.extend(Self::rate_options(rate, q));
            for (name, value) in &options {
                me.set(name, value)?;
            }
            let profile = match (i444, bit_depth) {
                (true, 10) => Some(c"main444-10"),
                (true, _) => Some(c"main444-8"),
                (false, 10) => Some(c"main10"),
                (false, _) => None,
            };
            if let Some(profile) = profile
                && unsafe { ((*api).param_apply_profile.unwrap())(params, profile.as_ptr()) } < 0
            {
                return Err(format!(
                    "x265 refused the {} profile",
                    profile.to_string_lossy()
                ));
            }
            me.encoder = unsafe { ((*api).encoder_open.unwrap())(params) };
            if me.encoder.is_null() {
                return Err("x265 refused the session".into());
            }
            // The layout follows the parameters as the open encoder holds them.
            let mut opened: x265_param = unsafe { std::mem::zeroed() };
            unsafe { ((*api).encoder_parameters.unwrap())(me.encoder, &mut opened) };
            if unsafe { ((*api).param_input_layout.unwrap())(&opened, &mut me.layout) } < 0 {
                return Err("x265 named no input layout".into());
            }
            let align = (me.layout.align.max(1) as usize).max(8);
            for (picture, aligned) in me.pictures.iter_mut().zip(me.aligned.iter_mut()) {
                for p in 0..3 {
                    let size = me.layout.size[p].max(0) as usize;
                    picture[p] = vec![0u64; (size + align).div_ceil(8)];
                    let start = picture[p].as_ptr() as usize;
                    aligned[p] = start.next_multiple_of(align) - start;
                }
            }
            Ok(me)
        }

        /// The first pixel of plane `p` of picture `picture`, and the bytes from it to the
        /// buffer's end.
        fn plane(&mut self, picture: usize, p: usize) -> (*mut u8, usize) {
            let offset = self.aligned[picture][p] + self.layout.offset[p].max(0) as usize;
            let buf = &mut self.pictures[picture][p];
            let len = buf.len() * 8 - offset;
            (unsafe { buf.as_mut_ptr().cast::<u8>().add(offset) }, len)
        }

        #[allow(clippy::too_many_arguments)]
        /// Convert the host frame into the next in-place picture and encode it there.
        /// `constant` marks the 16x16 blocks x265 codes as skips without analysis.
        #[allow(clippy::too_many_arguments)]
        pub fn encode(
            &mut self,
            planes: &mut Planes,
            pixels: &[u8],
            stride: usize,
            rgba: bool,
            full_range: bool,
            threads: usize,
            pts: u64,
            key: bool,
            held: Option<u32>,
            offsets: Option<&mut [f32]>,
            constant: Option<&mut [u8]>,
        ) -> Result<Option<Coded>, String> {
            let picture = self.current;
            self.current ^= 1;
            let (w, h) = (planes.width, planes.height);
            let ch = if planes.i444 { h } else { h.div_ceil(2) };
            let strides = (
                self.layout.stride[0] as usize,
                self.layout.stride[1] as usize,
            );
            let [(y, ylen), (u, ulen), (v, vlen)] = [
                self.plane(picture, 0),
                self.plane(picture, 1),
                self.plane(picture, 2),
            ];
            super::super::session::check_host_frame(pixels, stride, w, h)?;
            if planes.bit_depth > 8 {
                let (ys, cs) = (strides.0 / 2, strides.1 / 2);
                // SAFETY: each plane buffer is x265's own layout for this picture, whose offset of
                // the first pixel is a whole sample, and holds ys * h and cs * ch samples past it.
                let (y16, u16, v16) = unsafe {
                    (
                        std::slice::from_raw_parts_mut(y.cast::<u16>(), (ys * h).min(ylen / 2)),
                        std::slice::from_raw_parts_mut(u.cast::<u16>(), (cs * ch).min(ulen / 2)),
                        std::slice::from_raw_parts_mut(v.cast::<u16>(), (cs * ch).min(vlen / 2)),
                    )
                };
                crate::encoders::software::convert_to_yuv10_mt(
                    pixels,
                    stride,
                    w,
                    h,
                    rgba,
                    planes.i444,
                    full_range,
                    y16,
                    u16,
                    v16,
                    (ys, cs),
                    threads,
                );
            } else {
                // SAFETY: as above, with byte samples.
                let (y8, u8s, v8) = unsafe {
                    (
                        std::slice::from_raw_parts_mut(y, (strides.0 * h).min(ylen)),
                        std::slice::from_raw_parts_mut(u, (strides.1 * ch).min(ulen)),
                        std::slice::from_raw_parts_mut(v, (strides.1 * ch).min(vlen)),
                    )
                };
                super::super::session::convert_into(
                    pixels,
                    stride,
                    w,
                    h,
                    rgba,
                    planes.i444,
                    full_range,
                    false,
                    threads,
                    y8,
                    u8s,
                    v8,
                    strides,
                )?;
            }
            let api = unsafe { &*self.api };
            let mut pic: x265_picture = unsafe { std::mem::zeroed() };
            let mut out: [x265_picture; 2] = unsafe { std::mem::zeroed() };
            unsafe {
                (api.picture_init.unwrap())(self.params, &mut pic);
                for o in &mut out {
                    (api.picture_init.unwrap())(self.params, o);
                }
            }
            // Written per index: x265 4 declares a fourth plane for alpha, x265 3 does not.
            pic.planes[0] = y.cast();
            pic.planes[1] = u.cast();
            pic.planes[2] = v.cast();
            pic.stride[0] = self.layout.stride[0];
            pic.stride[1] = self.layout.stride[1];
            pic.stride[2] = self.layout.stride[2];
            if let Some(constant) = constant {
                pic.constantBlocks = constant.as_mut_ptr();
            }
            pic.pts = pts as i64;
            pic.bitDepth = planes.bit_depth as c_int;
            pic.sliceType = if key {
                X265_TYPE_IDR as c_int
            } else {
                X265_TYPE_AUTO as c_int
            };
            pic.forceqp = held.map_or(0, |q| q as c_int + 1);
            // x265 copies the offsets as the frame is submitted, within this call, into a buffer
            // the frame object it recycles has only where its first frame came with offsets: a
            // session hands them with every frame, zero where it refines nothing, or a recycled
            // frame without one is written through a null pointer.
            if let Some(offsets) = offsets {
                pic.quantOffsets = offsets.as_mut_ptr();
            }
            let mut nals: *mut x265_nal = ptr::null_mut();
            let mut count: u32 = 0;
            let ret = unsafe {
                encoder_encode(
                    api,
                    self.encoder,
                    &mut nals,
                    &mut count,
                    &mut pic,
                    out.as_mut_ptr(),
                )
            };
            if ret < 0 {
                return Err("x265 refused the frame".into());
            }
            if count == 0 {
                return Ok(None);
            }
            // A constant rate pads every frame short of its budget with filler data (`strict-cbr`),
            // which tells a decoder nothing: 44% of a still screen's stream at 60 fps and 54% at
            // 8 fps at 0.5 Mbit/s at 1080p. It stays off the wire.
            let mut bytes = Vec::new();
            let mut spent = 0;
            for nal in unsafe { std::slice::from_raw_parts(nals, count as usize) } {
                spent += nal.sizeBytes as usize;
                if nal.type_ != FILLER_NAL {
                    bytes.extend_from_slice(unsafe {
                        std::slice::from_raw_parts(nal.payload, nal.sizeBytes as usize)
                    });
                }
            }
            Ok(Some(Coded {
                bytes,
                pts: out[0].pts.max(0) as u64,
                qp: out[0].frameData.qp.round().clamp(0.0, 51.0) as u32,
                spent,
            }))
        }
    }
}

#[cfg(not(feature = "gpl"))]
mod kvazaar {
    //! kvazaar through its API table. The library takes no per-frame key-frame request and no
    //! live rate change, so both re-open the encoder.
    use std::ptr;

    use codec_sys::kvazaar::*;

    use super::*;

    pub struct Backend {
        api: *const kvz_api,
        config: *mut kvz_config,
        encoder: *mut kvz_encoder,
    }

    impl Drop for Backend {
        fn drop(&mut self) {
            unsafe {
                let api = &*self.api;
                if !self.encoder.is_null() {
                    (api.encoder_close.unwrap())(self.encoder);
                }
                if !self.config.is_null() {
                    (api.config_destroy.unwrap())(self.config);
                }
            }
        }
    }

    impl Backend {
        pub const LIBRARY: &'static str = "kvazaar";
        pub const FULLCOLOR: bool = false;

        /// kvazaar's sample depth is fixed when it is built, and the build here is 8-bit.
        pub fn ten_bit() -> bool {
            false
        }
        pub const KEY_ON_REQUEST: bool = false;
        pub const HOLDS_QUANTIZER: bool = false;
        pub const REFINES: bool = true;
        pub const CONSTANT_BLOCKS: bool = false;
        /// Quality index steps finer than the paint-over one kvazaar codes a refined block at
        /// (`FrameEncoder::refines`): 1080p text beside a video went from 38.5 dB to 43.2, 45.8,
        /// and 48.0 at a constant quality at 0, 3, and 6 steps finer, where kvazaar's cleanup of
        /// the still screen reaches 44.3. At a constant rate the refined tree blocks take the
        /// frame's quantizer off its rate control, which spends what they save elsewhere: 31.8
        /// dB with the map and without at 8 Mbit/s.
        pub const REFINE_FINER: u32 = 0;

        /// The frame rate the library was configured at, as `(num, den)`.
        #[cfg(test)]
        pub fn frame_rate(&self) -> (u32, u32) {
            unsafe {
                (
                    (*self.config).framerate_num as u32,
                    (*self.config).framerate_denom as u32,
                )
            }
        }

        fn set(&self, name: &str, value: &str) -> Result<(), String> {
            let (n, v) = (CString::new(name).unwrap(), CString::new(value).unwrap());
            if unsafe { ((*self.api).config_parse.unwrap())(self.config, n.as_ptr(), v.as_ptr()) }
                == 0
            {
                return Err(format!("kvazaar refused {name}={value}"));
            }
            Ok(())
        }

        pub fn open(
            width: usize,
            height: usize,
            threads: i32,
            i444: bool,
            bit_depth: u32,
            rate: RateSettings,
            q: u32,
        ) -> Result<Self, String> {
            let _ = (i444, bit_depth);
            if !width.is_multiple_of(8) || !height.is_multiple_of(8) {
                return Err(format!(
                    "kvazaar takes only pictures a multiple of eight, not {width}x{height}"
                ));
            }
            let api = unsafe { kvz_api_get(8) };
            if api.is_null() {
                return Err("kvazaar carries no 8-bit encoder".into());
            }
            let config = unsafe { ((*api).config_alloc.unwrap())() };
            if config.is_null() {
                return Err("kvazaar allocated no configuration".into());
            }
            let me = Self {
                api,
                config,
                encoder: ptr::null_mut(),
            };
            unsafe {
                if ((*api).config_init.unwrap())(config) == 0 {
                    return Err("kvazaar refused to initialize its configuration".into());
                }
                let cfg = &mut *config;
                cfg.width = width as i32;
                cfg.height = height as i32;
                cfg.framerate_num = rate.fps.num as i32;
                cfg.framerate_denom = rate.fps.den as i32;
                cfg.vui.fullrange = 0;
                cfg.vui.colorprim = 1;
                cfg.vui.transfer = 1;
                cfg.vui.colormatrix = 1;
            }
            let mut options: Vec<(&str, String)> = vec![
                ("preset", "ultrafast".into()),
                ("gop", "0".into()),
                ("period", "0".into()),
                ("vps-period", "0".into()),
                ("threads", threads.to_string()),
                ("owf", "0".into()),
                ("wpp", "1".into()),
            ];
            if rate.cbr {
                options.push(("bitrate", rate.bps().to_string()));
                options.push(("rc-algorithm", "oba".into()));
            } else {
                options.push(("qp", q.to_string()));
            }
            for (name, value) in &options {
                me.set(name, value)?;
            }
            let mut me = me;
            me.encoder = unsafe { ((*api).encoder_open.unwrap())(config) };
            if me.encoder.is_null() {
                return Err("kvazaar refused the session".into());
            }
            Ok(me)
        }

        #[allow(clippy::too_many_arguments)]
        pub fn encode(
            &mut self,
            planes: &mut Planes,
            pixels: &[u8],
            stride: usize,
            rgba: bool,
            full_range: bool,
            threads: usize,
            pts: u64,
            _key: bool,
            _held: Option<u32>,
            offsets: Option<&mut [f32]>,
            _constant: Option<&mut [u8]>,
        ) -> Result<Option<Coded>, String> {
            let api = unsafe { &*self.api };
            let pic =
                unsafe { (api.picture_alloc.unwrap())(planes.width as i32, planes.height as i32) };
            if pic.is_null() {
                return Err("kvazaar allocated no picture".into());
            }
            // kvazaar decides at the key frame whether the stream carries a quantizer delta per
            // coding unit, from whether that picture has a map, so every picture carries one,
            // 0 where nothing is refined; it frees the map with the picture.
            if let Some(offsets) = offsets {
                let (cols16, cols, rows) = (
                    planes.width.div_ceil(16),
                    planes.width.div_ceil(64),
                    planes.height.div_ceil(64),
                );
                let rows16 = planes.height.div_ceil(16);
                let map = unsafe { libc::malloc(cols * rows) as *mut i8 };
                if !map.is_null() {
                    for row in 0..rows {
                        for col in 0..cols {
                            let mut dqp = 0f32;
                            for r in row * 4..(row * 4 + 4).min(rows16) {
                                for c in col * 4..(col * 4 + 4).min(cols16) {
                                    dqp = dqp.min(offsets[r * cols16 + c]);
                                }
                            }
                            unsafe { *map.add(row * cols + col) = dqp.round() as i8 };
                        }
                    }
                    unsafe {
                        (*pic).roi.width = cols as i32;
                        (*pic).roi.height = rows as i32;
                        (*pic).roi.roi_array = map;
                    }
                }
            }
            let result = unsafe {
                let p = &mut *pic;
                let luma_stride = p.stride as usize;
                let chroma_stride = luma_stride / 2;
                let (w, h) = (planes.width, planes.height);
                let y = std::slice::from_raw_parts_mut(p.y, luma_stride * h);
                let u = std::slice::from_raw_parts_mut(p.u, chroma_stride * h.div_ceil(2));
                let v = std::slice::from_raw_parts_mut(p.v, chroma_stride * h.div_ceil(2));
                p.pts = pts as i64;
                super::super::session::convert_into(
                    pixels,
                    stride,
                    w,
                    h,
                    rgba,
                    false,
                    full_range,
                    false,
                    threads,
                    y,
                    u,
                    v,
                    (luma_stride, chroma_stride),
                )
            };
            if let Err(e) = result {
                unsafe { (api.picture_free.unwrap())(pic) };
                return Err(e);
            }
            let mut data: *mut kvz_data_chunk = ptr::null_mut();
            let mut len: u32 = 0;
            let mut recon: *mut kvz_picture = ptr::null_mut();
            let mut info: kvz_frame_info = unsafe { std::mem::zeroed() };
            let ok = unsafe {
                (api.encoder_encode.unwrap())(
                    self.encoder,
                    pic,
                    &mut data,
                    &mut len,
                    &mut recon,
                    ptr::null_mut(),
                    &mut info,
                )
            };
            let mut coded = None;
            if ok != 0 && !data.is_null() {
                let mut bytes = Vec::with_capacity(len as usize);
                let mut chunk = data;
                while !chunk.is_null() {
                    let c = unsafe { &*chunk };
                    bytes.extend_from_slice(&c.data[..c.len as usize]);
                    chunk = c.next;
                }
                let out_pts = if recon.is_null() {
                    pts
                } else {
                    unsafe { (*recon).pts.max(0) as u64 }
                };
                coded = Some(Coded {
                    spent: bytes.len(),
                    bytes,
                    pts: out_pts,
                    qp: info.qp.clamp(0, 51) as u32,
                });
            }
            unsafe {
                (api.picture_free.unwrap())(pic);
                (api.picture_free.unwrap())(recon);
                (api.chunk_free.unwrap())(data);
            }
            if ok == 0 {
                return Err("kvazaar refused the frame".into());
            }
            Ok(coded)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session's first frame is a key frame, a forced one mid-stream is too, and the ones
    /// between are deltas that name no reference.
    #[test]
    fn key_frames_come_on_request_and_deltas_name_no_reference() {
        let settings = CaptureSettings {
            width: 64,
            height: 48,
            target_fps: 30.0,
            codec: Codec::H265,
            video_crf: 25,
            ..Default::default()
        };
        let mut enc = HevcEncoder::new(&settings, false).expect("session");
        assert_eq!(enc.library(), Backend::LIBRARY);
        let bgra = vec![128u8; 64 * 48 * 4];
        let mut kinds = Vec::new();
        for t in 0..4u64 {
            let out = enc
                .encode_host(&bgra, 64 * 4, false, t, 25, t == 2)
                .expect("encode");
            kinds.push(super::super::codec::parse_video_type(out[1]).map(|(_, k)| k));
            assert_eq!(enc.last_reference(), Reference::Untracked);
        }
        assert_eq!(
            kinds,
            [
                Some(super::super::codec::FRAME_KEY),
                Some(super::super::codec::FRAME_DELTA),
                Some(super::super::codec::FRAME_KEY),
                Some(super::super::codec::FRAME_DELTA)
            ]
        );
        assert!(!enc.invalidate_reference(1));
    }

    /// The blocks marked constant are coded as skips without analysis: a still screen's frame
    /// comes out smaller with the map than without, where x265 takes one, at 8 bits and, where
    /// the build carries the encoder, at 10, each picture converted into x265's own layout.
    #[cfg(feature = "gpl")]
    #[test]
    fn constant_blocks_code_a_still_frame_as_skips() {
        for depth in [8, 10] {
            if depth == 10 && !ten_bit() {
                continue;
            }
            constant_blocks_skip_at(depth);
        }
    }

    #[cfg(feature = "gpl")]
    fn constant_blocks_skip_at(depth: u32) {
        let (w, h) = (320usize, 240usize);
        let settings = CaptureSettings {
            width: w as i32,
            height: h as i32,
            target_fps: 30.0,
            codec: Codec::H265,
            video_crf: 25,
            video_bit_depth: depth as i32,
            ..Default::default()
        };
        let mut bgra = vec![0u8; w * h * 4];
        for (i, px) in bgra.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, y) = (i % w, i / w);
            let text = (x / 7 + y / 11) % 3 == 0 && x % 7 < 5 && y % 11 < 8;
            px.copy_from_slice(if text {
                &[30, 30, 30, 255]
            } else {
                &[235, 235, 235, 255]
            });
        }
        let mut plain = HevcEncoder::new(&settings, false).expect("session");
        let mut mapped = HevcEncoder::new(&settings, false).expect("session");
        let blocks = w.div_ceil(16) * h.div_ceil(16);
        let (mut plain_bytes, mut mapped_bytes) = (0, 0);
        for n in 0..6u64 {
            plain_bytes = plain
                .encode_host(&bgra, w * 4, false, n, 25, n == 0)
                .expect("encode")
                .len();
            mapped.constant_blocks(&vec![true; blocks]);
            mapped_bytes = mapped
                .encode_host(&bgra, w * 4, false, n, 25, n == 0)
                .expect("encode")
                .len();
        }
        assert_eq!(mapped.bit_depth(), depth);
        assert!(
            mapped_bytes < plain_bytes,
            "{depth} bits: {mapped_bytes} bytes with the map, {plain_bytes} without"
        );
    }

    /// A box that changes every frame beside blocks marked constant, at a size no coding unit
    /// divides: the coding units the box touches and the partial ones along the edges are
    /// analyzed as before, the rest skipped, and the box and the still screen decode as close to
    /// the source as without the map, each picture converted in place into x265's own layout.
    #[cfg(feature = "gpl")]
    #[test]
    fn constant_blocks_leave_a_change_beside_them_coded() {
        use super::super::mixed_screen;
        let settings = CaptureSettings {
            width: mixed_screen::W as i32,
            height: mixed_screen::H as i32,
            target_fps: 30.0,
            codec: Codec::H265,
            video_crf: 25,
            ..Default::default()
        };
        let mut plain = HevcEncoder::new(&settings, false).expect("session");
        let mut mapped = HevcEncoder::new(&settings, false).expect("session");
        mixed_screen::assert_decodes_alike(
            Codec::H265,
            "x265",
            |src, n| {
                plain
                    .encode_host(src, mixed_screen::W * 4, false, n, 25, n == 0)
                    .expect("encode")
            },
            |src, n, map| {
                mapped.constant_blocks(map);
                mapped
                    .encode_host(src, mixed_screen::W * 4, false, n, 25, n == 0)
                    .expect("encode")
            },
        );
    }

    /// A constant-rate session's frames come to the paint-over quality six quantizer steps finer
    /// than the paint-over quantizer, a constant quality's at it.
    #[test]
    fn a_constant_rate_cleanup_reaches_six_steps_finer() {
        let mut settings = CaptureSettings {
            width: 64,
            height: 48,
            target_fps: 30.0,
            codec: Codec::H265,
            video_crf: 25,
            ..Default::default()
        };
        assert_eq!(
            HevcEncoder::new(&settings, false)
                .expect("session")
                .reach_finer(),
            0
        );
        settings.video_cbr_mode = true;
        settings.video_bitrate_kbps = 500;
        assert_eq!(
            HevcEncoder::new(&settings, false)
                .expect("session")
                .reach_finer(),
            6
        );
    }

    /// An x265 session codes with 8x8 coding units, finer than its preset's 16x16, at every size
    /// down to the smallest picture it codes.
    #[cfg(feature = "gpl")]
    #[test]
    fn x265_codes_with_8x8_coding_units() {
        for (width, height) in [(1920, 1080), (64, 48), (16, 16)] {
            let settings = CaptureSettings {
                width,
                height,
                target_fps: 30.0,
                codec: Codec::H265,
                video_crf: 25,
                ..Default::default()
            };
            let enc = HevcEncoder::new(&settings, false).expect("session");
            assert_eq!(enc.backend.min_cu_size(), 8, "{width}x{height}");
        }
    }

    /// A constant rate's still frames go without the filler x265 pads them to their budget with,
    /// which the cleanup still reads as spent: the frames on the wire come to a fraction of it.
    #[cfg(feature = "gpl")]
    #[test]
    fn a_constant_rates_filler_stays_off_the_wire() {
        let settings = CaptureSettings {
            width: 320,
            height: 240,
            target_fps: 30.0,
            codec: Codec::H265,
            video_cbr_mode: true,
            video_bitrate_kbps: 1000,
            ..Default::default()
        };
        let mut enc = HevcEncoder::new(&settings, false).expect("session");
        let bgra: Vec<u8> = (0..320 * 240 * 4).map(|i| ((i * 7) % 251) as u8).collect();
        let (mut sent, mut spent) = (0, 0);
        for t in 0..60u64 {
            let out = enc
                .encode_host(&bgra, 320 * 4, false, t, 25, false)
                .expect("encode");
            if t >= 30 && !out.is_empty() {
                let payload = &out[VIDEO_HEADER_LEN..];
                let mut at = 0;
                while let Some(i) = payload[at..].windows(3).position(|w| w == [0, 0, 1]) {
                    at += i + 3;
                    assert_ne!((payload[at] >> 1) & 0x3f, 38, "frame {t} carries filler");
                }
                sent += payload.len();
                spent += enc.last_size().unwrap_or(0);
            }
        }
        assert!(spent > 2 * sent, "{sent} bytes sent of {spent} spent");
    }

    /// The library is configured at the capture's rate as the fraction it names.
    #[test]
    fn the_frame_rate_reaches_the_library_as_its_fraction() {
        for (num, den) in [(60000u32, 1001u32), (120000, 1001), (144000, 1001), (60, 1)] {
            let settings = CaptureSettings {
                width: 64,
                height: 48,
                target_fps: num as f64 / den as f64,
                codec: Codec::H265,
                video_cbr_mode: true,
                video_bitrate_kbps: 2000,
                ..Default::default()
            };
            let enc = HevcEncoder::new(&settings, false).expect("session");
            assert_eq!(enc.backend.frame_rate(), (num, den), "{}", Backend::LIBRARY);
        }
    }
}
