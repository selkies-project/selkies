/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Software AV1 through SVT-AV1 at preset 11 in its real-time mode (`PRESET`), one frame in and one
//! packet out (two frames deep before 2.3), with rate control either constant-rate at the
//! session's bitrate and VBV or a constant quantizer. A new bitrate reaches the running
//! encoder with the next picture where the release takes one (`HAS_EVENTS`); a quality,
//! frame-rate, or VBV change re-opens it, as a bitrate change does on an earlier release.
//!
//! A constant-rate session on such a release names its references: it predicts every frame
//! from the one before in a flat structure and has the library store anchors on the schedule
//! of `ReferenceSlots`, its GOLDEN and ALTREF, so a frame a client lost is predicted past from
//! an anchor older than it, at `ANCHOR_PRESET`. Any other session leaves the references to the library, and a
//! frame a client lost costs the key frame the caller codes on the refusal, which SVT-AV1
//! before 2.0 codes only from a re-open as well.
//!
//! The blocks the ledger marks still and clean reach the library as the picture's
//! constant-block map (`constant_blocks`, `EbBufferHeaderType.constant_blocks`; the patches
//! under `patches/svtav1`): it copies only the other blocks, and those changed since its pooled
//! buffer last held the picture, into the buffers it codes from, bit for bit the stream of a
//! whole copy (the copy of a still 1080p frame took 0.011 ms on the calling thread where it took
//! 0.22, and at 2560x1600 0.025 where it took 0.52), and at a constant quantizer codes a 64x64
//! superblock of them as a zero-motion skip with no motion search, partition pass, candidate
//! search, or transform (a still 1080p frame took 1.08 ms on six logical processors where it
//! took 1.70, and at 2560x1600 1.88 where it took 2.86). A constant rate takes the copy alone:
//! its refresh of a still picture at a finer quantizer runs through the blocks a map would hold,
//! and holding them let its quantizer fall until a frame coded whole came out at many times
//! the budget.
//!
//! The session refines no blocks (`FrameEncoder::refines`): the library's region map sets a
//! segment's quantizer under a constant quantizer alone, its constant rate never setting up
//! segmentation, and the mode decision keeps the frame's lambda for every segment, so a still
//! block skips whatever its segment's quantizer. Half a still 720p picture mapped 150 quantizer
//! index steps finer than its qp 50 key frame came out at the map's 17 bytes a frame more, at
//! presets 4 to 11 (SVT-AV1 2.3).

use std::ffi::CString;
use std::ptr;
use std::sync::{Condvar, Mutex};

use codec_sys::svtav1::*;

use super::codec::{
    Codec, FRAME_ANCHOR, VIDEO_HEADER_LEN, VPX_QINDEX, av1_is_key, frame_type_from_key,
    push_video_header, vpx_level,
};
use super::reference::{Reference, ReferenceSlots, SlotPlan, SlotRefresh};
use super::session::{Pending, Planes, Quality, RateSettings, encode_threads};
use crate::CaptureSettings;

/// The lowest quantizer level the real-time mode runs at. Below three the library faults, and
/// below ten it codes every other frame of a scrolling text desktop with the lower third of
/// the picture wrong, at three times the bytes of level ten.
const RTC_MIN_LEVEL: u32 = 10;

/// The preset frames are coded at: in the real-time mode, a quarter less encode time than preset
/// 10 for more bytes at a fixed quantizer, which the quantizer table absorbs; the presets above
/// it are no faster.
const PRESET: i8 = 11;

/// The preset a frame predicting from an anchor alone is coded at. At preset 11 the library keeps
/// some blocks the picture changed since the anchor as they stand in the anchor, a stale copy
/// every client shows (test_slow_page's bar, 14 frames back, left where it was in a 64-pixel
/// block); preset 10 codes them. A session naming its references opens at it, since the library
/// takes a preset change only toward a faster one, and codes every other frame at `PRESET`.
const ANCHOR_PRESET: i8 = 10;

/// The preset a constant-rate session opens at, which codes a still screen's held key frame with
/// the screen-content mode on (`scm`): the real-time mode keeps the palette there and leaves it
/// off from preset 9 (intra block copy it leaves off throughout). 1080p text at quantizer 40 took
/// 229 kB against 296 at preset 10; the held key frame came out 284-327 kB at 49.2-50.4 dB against
/// 610-640 kB at 47.2-48.2 at preset 11, in about half a second. The session's first key frame
/// and every other frame keep their own presets: the library takes a picture's preset change
/// within the open one's and faster.
const KEY_PRESET: i8 = 8;

/// The highest constant-rate target the library takes, at open and live alike.
const MAX_BITRATE_BPS: u64 = 100_000_000;

/// The anchors a session that names its references has the library hold: GOLDEN and ALTREF, and
/// the ones they replaced until a picture releases them.
const MANAGED_REFS: u8 = 4;

/// SVT-AV1 before 2.0 codes a key frame asked for mid-stream only in its random-access quality
/// mode; a session on one re-opens the encoder for it, and the first frame answers.
const KEYFRAME_BY_REOPEN: bool = SVT_AV1_VERSION_MAJOR < 2;

/// SVT-AV1 before 1.5 drains an encoder on release even when it took no frame, and waits for a
/// packet that never comes; a session released before its first frame feeds it one.
const FEED_BEFORE_RELEASE: bool =
    SVT_AV1_VERSION_MAJOR < 1 || (SVT_AV1_VERSION_MAJOR == 1 && SVT_AV1_VERSION_MINOR < 5);

/// The handles alive, held while one is created or released and while the probe forks: before
/// 4.0 the library rebuilds a process-wide processor table in every handle it creates and frees
/// it in every one it releases, and from 4.1 handle setup takes process-wide locks that a child
/// forked meanwhile inherits held. The probe forks only while no handle is alive, since a live
/// session's worker threads hold that state as well.
pub(crate) static LIFECYCLE: Mutex<usize> = Mutex::new(0);

/// Notified at each handle released, for a probe waiting for none to be alive.
pub(crate) static RELEASED: Condvar = Condvar::new();

/// One SVT-AV1 session for one capture.
pub struct SvtAv1Encoder {
    handle: *mut EbComponentType,
    config: Box<EbSvtAv1EncConfiguration>,
    /// The input picture the library reads: the header and the plane pointers it carries.
    input: Box<(EbBufferHeaderType, EbSvtIOFormat)>,
    planes: Planes,
    threads: i32,
    quality: Quality,
    rate: RateSettings,
    omit_headers: bool,
    fresh: bool,
    /// The change the next picture carries to the running encoder.
    events: Events,
    /// The preset the running encoder codes the next picture at unless told otherwise.
    preset: i8,
    /// The buffers of a session that names its references, LAST the library's own.
    references: Option<ReferenceSlots>,
    /// The anchors the library holds, by the id each was stored under.
    anchors: Vec<u32>,
    last_reference: Reference,
    next_pts: u64,
    pending: Pending,
    /// The quantizer the next frame is held at whatever the rate control (`hold_quantizer`).
    held: Option<u32>,
    /// Whether the frame after a held key frame of a constant-rate session restates its target.
    restate_target: bool,
    /// The 16x16 blocks of the next picture that held still and are clean, one byte each in
    /// raster order, nonzero where constant (`constant_blocks`); empty where none is.
    constant: Vec<u8>,
    /// The quality index the rate control last coded a frame at, held frames aside.
    last_quality: Option<u32>,
}

/// The id an anchor is stored under: its timestamp, off the zero the library reserves.
fn anchor_id(pts: u64) -> u32 {
    (pts % u32::MAX as u64) as u32 + 1
}

unsafe impl Send for SvtAv1Encoder {}

fn error(what: &str, code: EbErrorType) -> String {
    format!("{what} (SVT-AV1 error {code:#x})")
}

impl Drop for SvtAv1Encoder {
    fn drop(&mut self) {
        self.close();
    }
}

impl SvtAv1Encoder {
    /// Open a session on host frames; `rgba` names the byte order they arrive in.
    pub fn new(settings: &CaptureSettings, rgba: bool) -> Result<Self, String> {
        let _ = rgba;
        let width = settings.width.max(1) as usize;
        let height = settings.height.max(1) as usize;
        let mut me = Self {
            handle: ptr::null_mut(),
            config: Box::new(unsafe { std::mem::zeroed() }),
            input: Box::new(unsafe { std::mem::zeroed() }),
            planes: Planes::new(
                width,
                height,
                false,
                if settings.video_bit_depth >= 10 {
                    10
                } else {
                    8
                },
            ),
            threads: encode_threads(settings),
            quality: Quality::new(Codec::Av1.quantizer(settings.video_crf)),
            rate: RateSettings::new(settings),
            omit_headers: settings.omit_stripe_headers,
            fresh: true,
            events: Events::default(),
            preset: PRESET,
            references: None,
            anchors: Vec::new(),
            last_reference: Reference::Untracked,
            next_pts: 0,
            pending: Pending::default(),
            held: None,
            restate_target: false,
            constant: Vec::new(),
            last_quality: None,
        };
        me.open()?;
        Ok(me)
    }

    /// The quantizer level the library takes for a quantizer index of AV1's domain.
    fn level(q: u32) -> u32 {
        vpx_level(Codec::Av1, q).max(RTC_MIN_LEVEL)
    }

    fn set(&mut self, name: &str, value: &str) -> Result<(), String> {
        let (n, v) = (CString::new(name).unwrap(), CString::new(value).unwrap());
        let code =
            unsafe { svt_av1_enc_parse_parameter(&mut *self.config, n.as_ptr(), v.as_ptr()) };
        if code != EB_ErrorNone {
            return Err(error(&format!("SVT-AV1 refused {name}={value}"), code));
        }
        Ok(())
    }

    /// Stand the encoder up with the live settings. The first frame it codes is a key frame.
    fn open(&mut self) -> Result<(), String> {
        self.close();
        let mut live = LIFECYCLE.lock().unwrap_or_else(|e| e.into_inner());
        let code = unsafe { init_handle(&mut self.handle, &mut *self.config) };
        if code != EB_ErrorNone || self.handle.is_null() {
            return Err(error("SVT-AV1 handed out no encoder handle", code));
        }
        *live += 1;
        let rate = self.rate;
        let bps = rate.bps();
        let tracks = HAS_EVENTS && rate.cbr;
        {
            let cfg = &mut *self.config;
            cfg.enc_mode = if tracks { KEY_PRESET } else { PRESET };
            self.preset = cfg.enc_mode;
            cfg.source_width = self.planes.width as u32;
            cfg.source_height = self.planes.height as u32;
            cfg.frame_rate_numerator = rate.fps.num;
            cfg.frame_rate_denominator = rate.fps.den;
            cfg.encoder_bit_depth = self.planes.bit_depth;
            cfg.encoder_color_format = EB_YUV420;
            cfg.color_primaries = EB_CICP_CP_BT_709;
            cfg.transfer_characteristics = 1;
            cfg.matrix_coefficients = 1;
            cfg.color_range = 0;
            cfg.intra_refresh_type = 2;
            if rate.cbr {
                let (lo, hi) = (
                    Codec::Av1.quantizer_bound(rate.min_qp),
                    Codec::Av1.quantizer_bound(rate.max_qp),
                );
                cfg.target_bit_rate = bps as u32;
                cfg.rate_control_mode = 2;
                cfg.max_qp_allowed = if hi > 0 { Self::level(hi) } else { 63 };
                cfg.min_qp_allowed = if lo > 0 {
                    Self::level(lo)
                } else {
                    RTC_MIN_LEVEL
                };
                // The library refuses a rate-control buffer shorter than 20 ms, which the
                // 1.5-frame VBV falls under above 75 fps.
                let vbv = (rate.vbv() as u64).max(bps / 50);
                cfg.maximum_buffer_size_ms = (vbv * 1000 / bps.max(1)) as i64;
            }
            if tracks {
                set_managed_refs(cfg, MANAGED_REFS);
            }
        }
        // `lp` is a level of parallelism, 0..=6, not a thread count.
        if HAS_RTC {
            self.set("rtc", "1")?;
        }
        self.set("pred-struct", "1")?;
        self.set("lookahead", "0")?;
        self.set("keyint", "-1")?;
        self.set("tile-columns", "0")?;
        self.set("tile-rows", "0")?;
        self.set("lp", &self.threads.min(6).to_string())?;
        if tracks {
            self.set("hierarchical-levels", "0")?;
            self.set("scm", "1")?;
        }
        if rate.cbr {
            self.set("rc", "2")?;
        } else {
            self.set("rc", "0")?;
            self.set("qp", &Self::level(self.quality.current).to_string())?;
        }
        let code = unsafe { svt_av1_enc_set_parameter(self.handle, &mut *self.config) };
        if code != EB_ErrorNone {
            return Err(error("SVT-AV1 refused the parameters", code));
        }
        let code = unsafe { svt_av1_enc_init(self.handle) };
        if code != EB_ErrorNone {
            return Err(error("SVT-AV1 refused the session", code));
        }
        let (header, io) = &mut *self.input;
        *header = unsafe { std::mem::zeroed() };
        header.size = std::mem::size_of::<EbBufferHeaderType>() as u32;
        header.p_buffer = io as *mut EbSvtIOFormat as *mut u8;
        let [y, u, v] = self.planes.pointers();
        io.luma = y;
        io.cb = u;
        io.cr = v;
        io.y_stride = self.planes.width as u32;
        io.cb_stride = self.planes.chroma_width() as u32;
        io.cr_stride = self.planes.chroma_width() as u32;
        header.n_alloc_len = self.planes.byte_len() as u32;
        self.fresh = true;
        self.events = Events::default();
        self.references = tracks.then(ReferenceSlots::new);
        self.anchors.clear();
        Ok(())
    }

    /// Tell the library the stream ended, drain what it still holds, and free the encoder.
    fn close(&mut self) {
        if self.handle.is_null() {
            return;
        }
        unsafe {
            if self.fresh && FEED_BEFORE_RELEASE {
                let header = &mut self.input.0;
                header.n_filled_len = header.n_alloc_len;
                header.flags = 0;
                header.pic_type = EB_AV1_KEY_PICTURE;
                self.fresh = svt_av1_enc_send_picture(self.handle, header) != EB_ErrorNone;
            }
            if !self.fresh {
                let mut last: EbBufferHeaderType = std::mem::zeroed();
                last.pic_type = EB_AV1_INVALID_PICTURE;
                last.flags = EB_BUFFERFLAG_EOS;
                svt_av1_enc_send_picture(self.handle, &mut last);
                loop {
                    let mut packet: *mut EbBufferHeaderType = ptr::null_mut();
                    if svt_av1_enc_get_packet(self.handle, &mut packet, 1) != EB_ErrorNone {
                        break;
                    }
                    let Some(header) = packet.as_ref() else { break };
                    let done = header.flags & EB_BUFFERFLAG_EOS != 0;
                    svt_av1_enc_release_out_buffer(&mut packet);
                    if done {
                        break;
                    }
                }
            }
            let mut live = LIFECYCLE.lock().unwrap_or_else(|e| e.into_inner());
            svt_av1_enc_deinit(self.handle);
            svt_av1_enc_deinit_handle(self.handle);
            *live -= 1;
            RELEASED.notify_all();
        }
        self.handle = ptr::null_mut();
    }

    pub fn codec(&self) -> Codec {
        Codec::Av1
    }

    /// The library behind the session, as the logs name it.
    pub fn library(&self) -> &'static str {
        "svt-av1"
    }

    pub fn is_fullcolor(&self) -> bool {
        false
    }

    /// The bits per sample the session codes.
    pub fn bit_depth(&self) -> u32 {
        self.planes.bit_depth
    }

    pub fn is_full_range(&self) -> bool {
        false
    }

    pub fn last_reference(&self) -> Reference {
        if self.references.is_some() {
            self.last_reference
        } else {
            Reference::Untracked
        }
    }

    /// Whether `hold_quantizer` holds a frame at its quantizer: at a constant rate where the
    /// release takes a new target with a picture. A constant-quantizer session is cleaned up
    /// through its own quality instead (`pipeline::decide_constant_quality`): a per-picture
    /// quantizer (`use_qp_file`) drops the library's key-frame and layer offsets from every
    /// picture it names, and a cleanup key frame held that way came out at 49.6 dB where the
    /// session's own key frame at the paint-over quality reached 61.0.
    pub fn holds_quantizer(&self) -> bool {
        self.rate.cbr && HAS_EVENTS
    }

    /// The quality index the rate control last coded a frame at, held frames aside.
    pub fn last_quality(&self) -> Option<u32> {
        self.last_quality
    }

    /// Leave frame `frame_id` and every frame after it out of the predictions where the session
    /// names its references; refused otherwise, so the caller codes a key frame.
    pub fn invalidate_reference(&mut self, frame_id: u16) -> bool {
        let Some(references) = self.references.as_mut() else {
            return false;
        };
        references.invalidate(frame_id);
        true
    }

    /// Every consumer holds frame `frame_id`, or where not `held` was sent it
    /// (`ReferenceSlots::acknowledge`), where the session names its references.
    pub fn acknowledge_reference(&mut self, frame_id: u16, held: bool) {
        if let Some(references) = self.references.as_mut() {
            references.acknowledge(frame_id, held);
        }
    }

    /// Encode the next frame at the quantizer the quality index `crf` selects, leaving the
    /// session's own quantizer for the frame after: the cleanup of a still screen, at a constant
    /// rate, where a held key picture is planned with a raised target (`holds_quantizer`). A
    /// constant-quantizer session codes the frame at its own quantizer.
    /// Mark the blocks of the next picture, in raster order, that held still and are clean
    /// (`BlockLedger::constant`): the library copies only the other blocks, and those changed
    /// since its buffer last held the picture, into the buffers it codes from
    /// (`EbBufferHeaderType.constant_blocks`), and at a constant quantizer codes a superblock
    /// of them as a zero-motion skip without analysis (`constant_blocks_skip`), which only a
    /// session that names no references takes, since only there does every frame predict from
    /// the one before (a loss is answered with a key frame). A constant rate
    /// leaves its mode decision to find the skips: its refresh of a still picture at a finer
    /// quantizer runs through the blocks a map would hold. An empty slice marks none.
    pub fn constant_blocks(&mut self, constant: &[bool]) {
        let blocks = self.planes.width.div_ceil(16) * self.planes.height.div_ceil(16);
        self.constant.clear();
        if constant.len() == blocks && constant.contains(&true) {
            self.constant.extend(constant.iter().map(|&c| u8::from(c)));
        }
    }

    pub fn hold_quantizer(&mut self, crf: u32) {
        self.held = Some(Codec::Av1.quantizer(crf as i32));
    }

    /// Apply a rate or frame-rate change: a new bitrate with the next picture where the library
    /// takes one live, since the VBV it holds spans the same time at any bitrate; anything else
    /// re-opens the encoder.
    pub fn reconfigure_rate(&mut self, settings: &CaptureSettings) -> Result<(), String> {
        let Some(rate) = self.rate.changed(settings) else {
            return Ok(());
        };
        let live = HAS_EVENTS
            && rate.fps == self.rate.fps
            && rate.vbv_multiplier == self.rate.vbv_multiplier
            && rate.bps() <= MAX_BITRATE_BPS;
        self.rate = rate;
        if !live {
            return self.open();
        }
        self.events.target_bit_rate = rate.bps() as u32;
        Ok(())
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
        let quality_changed = !self.rate.cbr
            && self
                .quality
                .update(Codec::Av1.quantizer(crf as i32))
                .is_some();
        if quality_changed || (force_idr && !self.fresh && KEYFRAME_BY_REOPEN) {
            self.open()?;
        }
        self.planes
            .convert(pixels, stride, rgba, false, false, self.threads as usize)?;
        let pts = self.next_pts;
        let mut key = force_idr || self.fresh;
        let mut plan = SlotPlan::KEY;
        if let Some(references) = &self.references {
            plan = references.plan(key);
            key = plan.predict_from == 0;
            let anchor = |slot: u8| {
                references
                    .slot(slot)
                    .map_or(0, |(_, held, _)| anchor_id(held))
            };
            let refreshed = |slot: u8| plan.refresh.refreshes(slot);
            let stored = if refreshed(SlotRefresh::GOLDEN | SlotRefresh::ALTREF) {
                anchor_id(references.next_pts())
            } else {
                0
            };
            let kept = [SlotRefresh::GOLDEN, SlotRefresh::ALTREF].map(|slot| {
                if refreshed(slot) {
                    stored
                } else {
                    anchor(slot)
                }
            });
            let from = if key || plan.predict_from == SlotRefresh::LAST {
                0
            } else {
                anchor(plan.predict_from)
            };
            let released = self
                .anchors
                .iter()
                .copied()
                .find(|a| !kept.contains(a) && *a != from);
            self.events.store = stored;
            self.events.predict_from = from;
            let preset = if from != 0 { ANCHOR_PRESET } else { PRESET };
            if preset != self.preset {
                self.events.preset = preset;
                self.preset = preset;
            }
            self.events.clear = if key { 0 } else { released.unwrap_or(0) };
        }
        {
            let header = &mut self.input.0;
            header.n_filled_len = header.n_alloc_len;
            header.pts = pts as i64;
            header.flags = 0;
            header.pic_type = if key {
                EB_AV1_KEY_PICTURE
            } else {
                EB_AV1_INVALID_PICTURE
            };
            header.constant_blocks = if key || self.held.is_some() || self.constant.is_empty() {
                ptr::null()
            } else {
                self.constant.as_ptr()
            };
            header.constant_blocks_skip = u8::from(!self.rate.cbr && self.references.is_none());
        }
        if self.rate.cbr && HAS_EVENTS {
            let held_key = key && self.held.is_some();
            // The held key frame at the open preset; any other at its own, as the references
            // planned it, or the anchor one where the session names none.
            let preset = if held_key {
                Some(KEY_PRESET)
            } else if self.references.is_none() {
                Some(ANCHOR_PRESET)
            } else {
                None
            };
            if let Some(preset) = preset
                && preset != self.preset
            {
                self.events.preset = preset;
                self.preset = preset;
            }
            if held_key || self.restate_target {
                // The rate control takes no quantizer: a held key frame's picture is planned
                // with a target that gives it `HELD_KEY_BUDGET_S` of the session's.
                let factor = if held_key {
                    super::HELD_KEY_BUDGET_S * self.rate.fps.fps()
                } else {
                    1.0
                };
                self.events.target_bit_rate =
                    (self.rate.bps() as f64 * factor.max(1.0)).min(MAX_BITRATE_BPS as f64) as u32;
            }
            self.restate_target = held_key;
        }
        let held = self.held.take().is_some();
        let sent = self.events;
        let code = unsafe { send_picture(self.handle, &mut self.input.0, sent) };
        self.constant.clear();
        if code != EB_ErrorNone {
            return Err(error("SVT-AV1 refused the frame", code));
        }
        self.events = Events::default();
        self.next_pts += 1;
        self.fresh = false;
        self.pending.push(pts, frame_number as u16);

        let mut output = Vec::new();
        loop {
            let mut packet: *mut EbBufferHeaderType = ptr::null_mut();
            let code = unsafe { svt_av1_enc_get_packet(self.handle, &mut packet, 0) };
            if code == EB_NoErrorEmptyQueue {
                break;
            }
            if code != EB_ErrorNone {
                return Err(error("SVT-AV1 handed out no packet", code));
            }
            let Some(p) = (unsafe { packet.as_ref() }) else {
                break;
            };
            let bytes =
                unsafe { std::slice::from_raw_parts(p.p_buffer, p.n_filled_len as usize) }.to_vec();
            let packet_pts = p.pts.max(0) as u64;
            if !held {
                self.last_quality =
                    Some(Codec::Av1.quality_index(VPX_QINDEX[(p.qp as usize).min(63)] as u32));
            }
            unsafe { svt_av1_enc_release_out_buffer(&mut packet) };
            let id = self.pending.take(packet_pts).unwrap_or(frame_number as u16);
            let is_key = av1_is_key(&bytes);
            let mut anchor = 0;
            if let Some(references) = self.references.as_mut() {
                if !is_key && references.shared_anchor(plan) {
                    anchor = FRAME_ANCHOR;
                }
                self.last_reference =
                    references.record(id, if is_key { SlotPlan::KEY } else { plan });
                if is_key {
                    self.anchors.clear();
                }
                self.anchors.retain(|&a| a != sent.clear);
                if sent.store != 0 {
                    self.anchors.push(sent.store);
                }
            }
            if !self.omit_headers {
                output.reserve(VIDEO_HEADER_LEN + bytes.len());
                push_video_header(
                    &mut output,
                    Codec::Av1,
                    frame_type_from_key(is_key) | anchor,
                    id,
                    0,
                    self.planes.width as u16,
                    self.planes.height as u16,
                    self.last_reference(),
                );
            }
            output.extend_from_slice(&bytes);
            if self.pending.is_empty() {
                break;
            }
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session let go before its first frame closes.
    #[test]
    fn a_session_let_go_before_its_first_frame_closes() {
        let (done, closed) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let settings = CaptureSettings {
                width: 64,
                height: 64,
                target_fps: 30.0,
                codec: Codec::Av1,
                ..Default::default()
            };
            drop(SvtAv1Encoder::new(&settings, false).expect("session"));
            let _ = done.send(());
        });
        assert!(
            closed
                .recv_timeout(std::time::Duration::from_secs(30))
                .is_ok(),
            "the release never returned"
        );
    }

    /// The blocks marked constant are copied no more and, at a constant quantizer, coded as skips
    /// without analysis: a still screen's stream comes out within a few bytes of the one the
    /// mode decision codes, the GLOBALMV skip costing a bit more than its own skip, and decodes
    /// to the same picture.
    #[test]
    fn constant_blocks_code_a_still_screen_as_skips() {
        use crate::webcam::decode::{Decoder as _, VideoDecoder};
        let (w, h) = (320usize, 240usize);
        let settings = CaptureSettings {
            width: w as i32,
            height: h as i32,
            target_fps: 30.0,
            codec: Codec::Av1,
            video_crf: 25,
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
        let mut plain = SvtAv1Encoder::new(&settings, false).expect("session");
        let mut mapped = SvtAv1Encoder::new(&settings, false).expect("session");
        let mut plain_dec = VideoDecoder::new(Codec::Av1).expect("decoder");
        let mut mapped_dec = VideoDecoder::new(Codec::Av1).expect("decoder");
        let blocks = w.div_ceil(16) * h.div_ceil(16);
        let (mut plain_bytes, mut mapped_bytes) = (0, 0);
        for n in 0..6u64 {
            let a = plain
                .encode_host(&bgra, w * 4, false, n, 25, n == 0)
                .expect("encode");
            mapped.constant_blocks(&vec![true; blocks]);
            let b = mapped
                .encode_host(&bgra, w * 4, false, n, 25, n == 0)
                .expect("encode");
            plain_bytes += a.len();
            mapped_bytes += b.len();
            assert!(plain_dec.decode(&a[VIDEO_HEADER_LEN..]).expect("decode"));
            assert!(mapped_dec.decode(&b[VIDEO_HEADER_LEN..]).expect("decode"));
        }
        assert!(
            mapped_bytes <= plain_bytes + 64,
            "{mapped_bytes} bytes with the map, {plain_bytes} without"
        );
        let (pa, pb) = (
            plain_dec.frame().expect("picture"),
            mapped_dec.frame().expect("picture"),
        );
        let distance =
            pa.y.chunks(pa.y_stride)
                .zip(pb.y.chunks(pb.y_stride))
                .take(h)
                .flat_map(|(ra, rb)| {
                    ra[..w]
                        .iter()
                        .zip(&rb[..w])
                        .map(|(&x, &y)| (x as f64 - y as f64).abs())
                })
                .sum::<f64>()
                / (w * h) as f64;
        assert!(
            distance < 0.5,
            "the pictures differ by {distance} luma levels on average"
        );
    }

    /// A box that changes every frame beside blocks marked constant, at a size no superblock
    /// divides: the library's partial copy merges the active blocks and the partial edge blocks
    /// into runs, clipping the chroma rows it copies to the planes, and at a constant quantizer
    /// the skip leaves the superblocks the box touches coded. Under a constant rate the copy runs
    /// alone. Either way the box and the still screen decode as close to the source as without
    /// the map.
    #[test]
    fn constant_blocks_leave_a_change_beside_them_coded() {
        use super::super::mixed_screen;
        for cbr in [false, true] {
            let settings = CaptureSettings {
                width: mixed_screen::W as i32,
                height: mixed_screen::H as i32,
                target_fps: 30.0,
                codec: Codec::Av1,
                video_crf: 25,
                video_cbr_mode: cbr,
                video_bitrate_kbps: 4000,
                ..Default::default()
            };
            let mut plain = SvtAv1Encoder::new(&settings, false).expect("session");
            let mut mapped = SvtAv1Encoder::new(&settings, false).expect("session");
            mixed_screen::assert_decodes_alike(
                Codec::Av1,
                if cbr {
                    "constant rate"
                } else {
                    "constant quantizer"
                },
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
    }

    /// The library is configured at the capture's rate as the fraction it names, a live change
    /// of rate included.
    #[test]
    fn the_frame_rate_reaches_the_library_as_its_fraction() {
        for (num, den) in [(60000u32, 1001u32), (120000, 1001), (144000, 1001), (60, 1)] {
            let fps = num as f64 / den as f64;
            let settings = CaptureSettings {
                width: 64,
                height: 64,
                target_fps: fps,
                codec: Codec::Av1,
                video_cbr_mode: true,
                video_bitrate_kbps: 2000,
                ..Default::default()
            };
            let mut enc = SvtAv1Encoder::new(&settings, false).expect("session");
            assert_eq!(
                (
                    enc.config.frame_rate_numerator,
                    enc.config.frame_rate_denominator
                ),
                (num, den)
            );
            enc.reconfigure_rate(&CaptureSettings {
                target_fps: 30.0,
                ..settings.clone()
            })
            .expect("30 fps");
            enc.reconfigure_rate(&settings).expect("back");
            assert_eq!(
                (
                    enc.config.frame_rate_numerator,
                    enc.config.frame_rate_denominator
                ),
                (num, den)
            );
        }
    }

    /// A constant-rate session that manages its presets opens at `KEY_PRESET` with the
    /// screen-content mode on, codes a still screen's held key frame there, and every other
    /// frame, its first key frame included, at its own.
    #[test]
    fn a_held_key_frame_is_coded_at_the_key_preset_and_no_other_frame_is() {
        if !HAS_EVENTS {
            return;
        }
        let settings = CaptureSettings {
            width: 64,
            height: 64,
            target_fps: 30.0,
            codec: Codec::Av1,
            video_cbr_mode: true,
            video_bitrate_kbps: 2000,
            ..Default::default()
        };
        let mut enc = SvtAv1Encoder::new(&settings, false).expect("session");
        assert_eq!(enc.config.enc_mode, KEY_PRESET);
        assert_eq!(enc.config.screen_content_mode, 1);
        let frame = vec![0x40u8; 64 * 64 * 4];
        enc.encode_host(&frame, 64 * 4, false, 0, 30, true)
            .expect("first frame");
        assert_ne!(enc.preset, KEY_PRESET, "the first key frame");
        enc.encode_host(&frame, 64 * 4, false, 1, 30, false)
            .expect("a frame");
        assert_ne!(enc.preset, KEY_PRESET);
        enc.hold_quantizer(10);
        enc.encode_host(&frame, 64 * 4, false, 2, 30, true)
            .expect("held key frame");
        assert_eq!(enc.preset, KEY_PRESET);
        enc.encode_host(&frame, 64 * 4, false, 3, 30, false)
            .expect("the frame after");
        assert_ne!(enc.preset, KEY_PRESET);
    }

    /// Sessions opened and closed on many threads at once each encode their frame.
    #[test]
    fn sessions_open_and_close_on_many_threads_at_once() {
        let settings = CaptureSettings {
            width: 64,
            height: 64,
            target_fps: 30.0,
            codec: Codec::Av1,
            ..Default::default()
        };
        let frame = vec![0u8; 64 * 64 * 4];
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    for n in 0..4 {
                        let mut enc = SvtAv1Encoder::new(&settings, false).expect("session");
                        enc.encode_host(&frame, 64 * 4, false, n, 30, true)
                            .expect("frame");
                    }
                });
            }
        });
    }
}
