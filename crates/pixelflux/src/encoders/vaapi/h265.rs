/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The HEVC arm: the sequence, picture, and slice parameters, the packed VPS, SPS, PPS, and slice
//! headers, and the reference picture set behind them. Main profile, Main 10 at 10 bits, or the
//! range extensions' Main 4:4:4 and Main 4:4:4 10 for a 4:4:4 session, with the coding tools
//! and block sizes the driver reports, one reference per slice, and a decoded picture buffer of
//! the frames the level admits: every slice header carries an explicit short-term reference
//! picture set naming the frames the decoder keeps, with the newest one the client still has
//! as the one used. Where the driver says a predicted slice has to carry both reference lists
//! (`VA_PREDICTION_DIRECTION_BI_NOT_EMPTY`, Intel's low-power encoder), a predicted frame is a B
//! slice whose two lists name that same frame, which codes what a P slice would: such an
//! encoder fails a P slice on some generations and takes it on others.

use va_sys::*;

use super::super::bits::{BitWriter, nal_unit};
use super::super::codec::{h265_level, h265_tier};
use super::{Buffers, Frame, Negotiated};

const LOG2_MAX_POC_LSB_MINUS4: u32 = 8;
/// The quantizer the PPS names for a constant-rate session, and the base of every slice's
/// delta.
const PIC_INIT_QP_CBR: u32 = 30;
const PIC_INIT_QP_CQP: u32 = 26;

/// The quantizer a constant-rate slice names where the rate control codes its own in every
/// coding unit as a delta from the slice's (Intel's full encoder): the middle of the range
/// at `bit_depth`, from which a delta within the syntax's -(26 + QpBdOffsetY / 2) to
/// 25 + QpBdOffsetY / 2 reaches every quantizer. From 30, a unit the rate control takes under
/// 4 codes a delta past that range, which a strict decoder refuses and a lenient one decodes
/// as garbage.
fn centered_qp(bit_depth: u32) -> u32 {
    26 - 3 * (bit_depth - 8)
}

/// The finest quantizer Intel's low-power HEVC encoder codes: the driver clips the slice's to
/// 10..51 when it programs VDENC and looks its rate tables up from 10, and its rate control floors
/// its own minimum at 10. A slice naming a finer one is coded at this one regardless, so the
/// decoder dequantizes it too fine and the picture comes out washed out, the more so the finer
/// the slice names (31 dB at 9 and 16 dB at 2 against 59 dB at 10, Arc A750).
pub(super) const INTEL_LOW_POWER_MIN_QP: u32 = 10;

/// The coding tools and block sizes a session runs with: what the driver reports, or the
/// defaults of `Arm::new` where it reports none.
#[derive(Clone, Copy)]
struct Tools {
    ctu_size: u32,
    min_cb_size: u32,
    log2_min_tb_minus2: u32,
    log2_diff_max_min_tb: u32,
    max_th_depth_inter: u32,
    max_th_depth_intra: u32,
    amp: bool,
    sao: bool,
    temporal_mvp: bool,
    pcm: bool,
    transform_skip: bool,
    cu_qp_delta: bool,
}

pub(super) struct Arm {
    profile: VAProfile,
    tools: Tools,
    level_idc: u32,
    tier: u32,
    ctb_width: u32,
    ctb_height: u32,
    slices: (u32, u32),
    dpb: u32,
    pic_init_qp: u32,
    cu_qp_delta: bool,
    /// The depth of the coding units that each carry a quantizer delta, 0 for one per tree block.
    cu_qp_delta_depth: u32,
    /// The surface alignment, the smallest coding block but where `new` says otherwise.
    align: u32,
    /// Whether a predicted frame is coded as a B slice with the one reference in both lists.
    low_delay_b: bool,
    sequence: VAEncSequenceParameterBufferHEVC,
    headers: Vec<u8>,
}

impl Arm {
    /// The arm on `profile`, with the driver's feature and block-size attributes where it
    /// reports them. Intel's full encoder (`unit_qp_deltas`), which reports no block sizes, codes
    /// blocks from 8x8 to 32x32 (Skylake's, as FFmpeg's defaults say) on surfaces aligned to 16.
    pub(super) fn new(
        profile: VAProfile,
        features: Option<u32>,
        block_sizes: Option<u32>,
        prediction: Option<u32>,
        unit_qp_deltas: bool,
    ) -> Self {
        let mut tools = Tools {
            ctu_size: 32,
            min_cb_size: 16,
            log2_min_tb_minus2: 0,
            log2_diff_max_min_tb: 3,
            max_th_depth_inter: 3,
            max_th_depth_intra: 3,
            amp: true,
            sao: false,
            temporal_mvp: false,
            pcm: false,
            transform_skip: false,
            cu_qp_delta: true,
        };
        let mut align = tools.min_cb_size;
        if unit_qp_deltas && block_sizes.is_none() {
            tools.min_cb_size = 8;
        }
        if let Some(value) = features {
            let f = VAConfigAttribValEncHEVCFeatures { value };
            unsafe {
                tools.amp = f.bits.amp() != 0;
                tools.sao = f.bits.sao() != 0;
                tools.temporal_mvp = f.bits.temporal_mvp() != 0;
                tools.pcm = f.bits.pcm() != 0;
                tools.transform_skip = f.bits.transform_skip() != 0;
                tools.cu_qp_delta = f.bits.cu_qp_delta() != 0;
            }
        }
        if let Some(value) = block_sizes {
            let b = VAConfigAttribValEncHEVCBlockSizes { value };
            unsafe {
                tools.ctu_size = 1 << (b.bits.log2_max_coding_tree_block_size_minus3() + 3);
                tools.min_cb_size = 1 << (b.bits.log2_min_luma_coding_block_size_minus3() + 3);
                tools.log2_min_tb_minus2 = b.bits.log2_min_luma_transform_block_size_minus2();
                tools.log2_diff_max_min_tb = b.bits.log2_max_luma_transform_block_size_minus2()
                    - b.bits.log2_min_luma_transform_block_size_minus2();
                tools.max_th_depth_inter = b.bits.max_max_transform_hierarchy_depth_inter();
                tools.max_th_depth_intra = b.bits.max_max_transform_hierarchy_depth_intra();
            }
            align = tools.min_cb_size;
        }
        Self {
            profile,
            tools,
            level_idc: 0,
            tier: 0,
            ctb_width: 0,
            ctb_height: 0,
            slices: (1, 1),
            dpb: 1,
            pic_init_qp: PIC_INIT_QP_CBR,
            cu_qp_delta: false,
            cu_qp_delta_depth: 0,
            align,
            low_delay_b: prediction.is_some_and(|p| p & VA_PREDICTION_DIRECTION_BI_NOT_EMPTY != 0),
            sequence: unsafe { std::mem::zeroed() },
            headers: Vec::new(),
        }
    }

    /// The surface alignment: the smallest coding block, or 16 on Intel's full encoder.
    pub(super) fn alignment(&self) -> (u32, u32) {
        (self.align, self.align)
    }

    pub(super) fn ctu_size(&self) -> u32 {
        self.tools.ctu_size
    }

    pub(super) fn slices(&self) -> (u32, u32) {
        self.slices
    }

    /// The `general_level_idc` the sequence names.
    pub(super) fn level(&self) -> u32 {
        self.level_idc
    }

    /// The profile a session declares: the range extensions for 4:4:4, else Main 10 or Main.
    fn profile_idc(n: &Negotiated) -> u32 {
        match (n.fullcolor, n.bit_depth) {
            (true, _) => 4,
            (false, 10) => 2,
            (false, _) => 1,
        }
    }

    fn profile_tier_level(&self, w: &mut BitWriter, n: &Negotiated) {
        let (fullcolor, bit_depth) = (n.fullcolor, n.bit_depth);
        let profile_idc = Self::profile_idc(n);
        w.u(2, 0);
        w.u(1, self.tier as u64);
        w.u(5, profile_idc as u64);
        let mut compat = [false; 32];
        compat[profile_idc as usize] = true;
        if compat[1] {
            compat[2] = true;
        }
        if compat[3] {
            compat[1] = true;
            compat[2] = true;
        }
        for flag in compat {
            w.flag(flag);
        }
        w.flag(true);
        w.flag(false);
        w.flag(true);
        w.flag(true);
        if profile_idc == 4 {
            w.flag(true);
            w.flag(true);
            w.flag(bit_depth == 8);
            w.flag(!fullcolor);
            w.flag(!fullcolor);
            w.flag(false);
            w.flag(false);
            w.flag(false);
            w.flag(true);
            w.u(34, 0);
        } else {
            w.u(43, 0);
        }
        w.flag(false);
        w.u(8, self.level_idc as u64);
    }

    /// The stream's sequence: level and tier from the ladder, the level never below the one
    /// the decoded picture buffer was sized for, the picture in coding tree blocks, and the
    /// VPS, SPS, and PPS the key frames carry.
    pub(super) fn configure(
        &mut self,
        n: &Negotiated,
        surface_width: u32,
        surface_height: u32,
        slices: (u32, u32),
    ) {
        let t = self.tools;
        self.ctb_width = surface_width.div_ceil(t.ctu_size);
        self.ctb_height = surface_height.div_ceil(t.ctu_size);
        self.slices = slices;
        self.dpb = n.dpb;
        self.level_idc = h265_level(
            n.width,
            n.height,
            n.fps.ceil(),
            n.bits_per_second as u64,
            true,
        )
        .max(n.dpb_level);
        self.tier = h265_tier(self.level_idc);
        self.pic_init_qp = if n.rc_mode == VA_RC_CQP {
            PIC_INIT_QP_CQP
        } else if n.unit_qp_deltas {
            centered_qp(n.bit_depth)
        } else {
            PIC_INIT_QP_CBR
        };
        self.cu_qp_delta = n.rc_mode != VA_RC_CQP && t.cu_qp_delta;
        let log2_min_cb = t.min_cb_size.trailing_zeros();
        let log2_diff_cb = t.ctu_size.trailing_zeros() - log2_min_cb;
        // A frame's units share the quantizer of Intel's full encoder's rate control, so one delta
        // per tree block carries it, as FFmpeg codes there.
        self.cu_qp_delta_depth = if n.unit_qp_deltas { 0 } else { log2_diff_cb };
        let chroma_shift = if n.fullcolor { 0 } else { 1 };
        let conformance = surface_width != n.width || surface_height != n.height;

        let mut w = BitWriter::new();
        w.u(4, 0);
        w.flag(true);
        w.flag(true);
        w.u(6, 0);
        w.u(3, 0);
        w.flag(true);
        w.u(16, 0xffff);
        self.profile_tier_level(&mut w, n);
        w.flag(false);
        w.ue(self.dpb);
        w.ue(0);
        w.ue(0);
        w.u(6, 0);
        w.ue(0);
        w.flag(true);
        w.u(32, n.fps.den as u64);
        w.u(32, n.fps.num as u64);
        w.flag(true);
        w.ue(0);
        w.ue(0);
        w.flag(false);
        w.trailing_bits();
        let vps = nal_unit(&[0x40, 0x01], w.as_bytes());

        let mut w = BitWriter::new();
        w.u(4, 0);
        w.u(3, 0);
        w.flag(true);
        self.profile_tier_level(&mut w, n);
        w.ue(0);
        w.ue(if n.fullcolor { 3 } else { 1 });
        if n.fullcolor {
            w.flag(false);
        }
        w.ue(surface_width);
        w.ue(surface_height);
        w.flag(conformance);
        if conformance {
            w.ue(0);
            w.ue((surface_width - n.width) >> chroma_shift);
            w.ue(0);
            w.ue((surface_height - n.height) >> chroma_shift);
        }
        w.ue(n.bit_depth - 8);
        w.ue(n.bit_depth - 8);
        w.ue(LOG2_MAX_POC_LSB_MINUS4);
        w.flag(false);
        w.ue(self.dpb);
        w.ue(0);
        w.ue(0);
        w.ue(log2_min_cb - 3);
        w.ue(log2_diff_cb);
        w.ue(t.log2_min_tb_minus2);
        w.ue(t.log2_diff_max_min_tb);
        w.ue(t.max_th_depth_inter);
        w.ue(t.max_th_depth_intra);
        w.flag(false);
        w.flag(t.amp);
        w.flag(t.sao);
        w.flag(t.pcm);
        if t.pcm {
            w.u(4, 0);
            w.u(4, 0);
            w.ue(0);
            w.ue(0);
            w.flag(false);
        }
        w.ue(0);
        w.flag(false);
        w.flag(t.temporal_mvp);
        w.flag(false);
        w.flag(true);
        w.flag(false);
        w.flag(false);
        w.flag(true);
        w.u(3, 5);
        w.flag(false);
        w.flag(true);
        w.u(8, 1);
        w.u(8, 1);
        w.u(8, 1);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(true);
        w.u(32, n.fps.den as u64);
        w.u(32, n.fps.num as u64);
        w.flag(true);
        w.ue(0);
        w.flag(false);
        w.flag(true);
        w.flag(false);
        w.flag(true);
        w.flag(true);
        w.ue(0);
        w.ue(0);
        w.ue(0);
        w.ue(15);
        w.ue(15);
        w.flag(false);
        w.trailing_bits();
        let sps = nal_unit(&[0x42, 0x01], w.as_bytes());

        let mut w = BitWriter::new();
        w.ue(0);
        w.ue(0);
        w.flag(false);
        w.flag(false);
        w.u(3, 0);
        w.flag(false);
        w.flag(false);
        w.ue(0);
        w.ue(0);
        w.se(self.pic_init_qp as i32 - 26);
        w.flag(false);
        w.flag(t.transform_skip);
        w.flag(self.cu_qp_delta);
        if self.cu_qp_delta {
            w.ue(self.cu_qp_delta_depth);
        }
        w.se(0);
        w.se(0);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(true);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.ue(0);
        w.flag(false);
        w.flag(false);
        w.trailing_bits();
        let pps = nal_unit(&[0x44, 0x01], w.as_bytes());
        self.headers = [vps, sps, pps].concat();

        let s = &mut self.sequence;
        *s = unsafe { std::mem::zeroed() };
        s.general_profile_idc = Self::profile_idc(n) as u8;
        s.general_level_idc = self.level_idc as u8;
        s.general_tier_flag = self.tier as u8;
        s.intra_period = 1 << (4 + LOG2_MAX_POC_LSB_MINUS4);
        s.intra_idr_period = 1 << (4 + LOG2_MAX_POC_LSB_MINUS4);
        s.ip_period = 1;
        s.bits_per_second = n.bits_per_second;
        s.pic_width_in_luma_samples = surface_width as u16;
        s.pic_height_in_luma_samples = surface_height as u16;
        unsafe {
            let f = &mut s.seq_fields.bits;
            f.set_chroma_format_idc(if n.fullcolor { 3 } else { 1 });
            f.set_bit_depth_luma_minus8(n.bit_depth - 8);
            f.set_bit_depth_chroma_minus8(n.bit_depth - 8);
            f.set_amp_enabled_flag(t.amp as u32);
            f.set_sample_adaptive_offset_enabled_flag(t.sao as u32);
            f.set_pcm_enabled_flag(t.pcm as u32);
            f.set_sps_temporal_mvp_enabled_flag(t.temporal_mvp as u32);
        }
        s.log2_min_luma_coding_block_size_minus3 = (log2_min_cb - 3) as u8;
        s.log2_diff_max_min_luma_coding_block_size = log2_diff_cb as u8;
        s.log2_min_transform_block_size_minus2 = t.log2_min_tb_minus2 as u8;
        s.log2_diff_max_min_transform_block_size = t.log2_diff_max_min_tb as u8;
        s.max_transform_hierarchy_depth_inter = t.max_th_depth_inter as u8;
        s.max_transform_hierarchy_depth_intra = t.max_th_depth_intra as u8;
        s.vui_parameters_present_flag = 0;
    }

    pub(super) fn sequence(&mut self, n: &Negotiated, out: &mut Buffers) {
        out.push(VAEncSequenceParameterBufferType, &self.sequence);
        if n.packed & VA_ENC_PACKED_HEADER_SEQUENCE != 0 {
            let bits = 8 * self.headers.len() as u32;
            out.push_packed(VAEncPackedHeaderSequence, &self.headers, bits);
        }
    }

    /// The picture: its order count since the key frame, the frames the client still has,
    /// newest first, as the driver's reference list and the slice header's reference picture
    /// set, and every slice.
    pub(super) fn picture(
        &mut self,
        n: &Negotiated,
        frame: &Frame,
        out: &mut Buffers,
    ) -> Result<(), String> {
        let t = self.tools;
        let poc_of = |pts: u64| (pts - frame.key_pts) as i32;
        let poc = poc_of(frame.pts);
        let reference_pts = frame.reference.map(|(pts, _)| pts);
        if !frame.key && reference_pts.is_none() {
            return Err("a predicted HEVC picture without a reference".into());
        }
        let mut kept: Vec<(u64, VASurfaceID)> = frame
            .held
            .iter()
            .filter(|h| !frame.key && !h.2 && h.0 >= frame.key_pts)
            .map(|h| (h.0, h.1))
            .collect();
        kept.sort_by_key(|k| std::cmp::Reverse(k.0));
        kept.truncate(15);
        let invalid = VAPictureHEVC {
            picture_id: VA_INVALID_ID,
            pic_order_cnt: 0,
            flags: VA_PICTURE_HEVC_INVALID,
            va_reserved: [0; 4],
        };
        let picture = |pts: u64, surface: VASurfaceID| VAPictureHEVC {
            picture_id: surface,
            pic_order_cnt: poc_of(pts),
            flags: if Some(pts) == reference_pts {
                VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE
            } else {
                0
            },
            va_reserved: [0; 4],
        };

        let mut pic: VAEncPictureParameterBufferHEVC = unsafe { std::mem::zeroed() };
        pic.decoded_curr_pic = VAPictureHEVC {
            picture_id: frame.recon,
            pic_order_cnt: poc,
            flags: 0,
            va_reserved: [0; 4],
        };
        pic.reference_frames = [invalid; 15];
        for (i, &(pts, surface)) in kept.iter().enumerate() {
            pic.reference_frames[i] = picture(pts, surface);
        }
        pic.coded_buf = frame.coded;
        // A key frame names no co-located picture: iHD reads the reference the index names
        // whatever the frame's type, and reference 0 of a key frame is invalid.
        pic.collocated_ref_pic_index = if t.temporal_mvp && !frame.key {
            0
        } else {
            0xff
        };
        pic.pic_init_qp = self.pic_init_qp as u8;
        pic.diff_cu_qp_delta_depth = if self.cu_qp_delta {
            self.cu_qp_delta_depth as u8
        } else {
            0
        };
        pic.nal_unit_type = if frame.key { 19 } else { 1 };
        unsafe {
            let f = &mut pic.pic_fields.bits;
            f.set_idr_pic_flag(frame.key as u32);
            f.set_coding_type(if frame.key { 1 } else { 2 });
            f.set_reference_pic_flag(1);
            f.set_transform_skip_enabled_flag(t.transform_skip as u32);
            f.set_cu_qp_delta_enabled_flag(self.cu_qp_delta as u32);
            f.set_pps_loop_filter_across_slices_enabled_flag(1);
        }
        out.push(VAEncPictureParameterBufferType, &pic);

        let slice_qp = if n.rc_mode == VA_RC_CQP {
            frame.qp.clamp(n.qp_floor.max(1), 51)
        } else {
            self.pic_init_qp
        };
        let slice_qp_delta = slice_qp as i32 - self.pic_init_qp as i32;
        let ctbs = self.ctb_width * self.ctb_height;
        let address_bits = 32 - (ctbs - 1).leading_zeros();
        let (slice_count, slice_rows) = self.slices;
        for i in 0..slice_count {
            let first_row = i * slice_rows;
            let rows = if i + 1 == slice_count {
                self.ctb_height - first_row
            } else {
                slice_rows
            };
            let address = first_row * self.ctb_width;
            let count = rows * self.ctb_width;
            let last = i + 1 == slice_count;
            let slice_type: u32 = match (frame.key, self.low_delay_b) {
                (true, _) => 2,
                (false, true) => 0,
                (false, false) => 1,
            };

            if n.packed & VA_ENC_PACKED_HEADER_SLICE != 0 {
                let mut w = BitWriter::new();
                w.flag(i == 0);
                if frame.key {
                    w.flag(false);
                }
                w.ue(0);
                if i != 0 {
                    w.u(address_bits, address as u64);
                }
                w.ue(slice_type);
                if !frame.key {
                    w.u(
                        4 + LOG2_MAX_POC_LSB_MINUS4,
                        (poc & ((1 << (4 + LOG2_MAX_POC_LSB_MINUS4)) - 1)) as u64,
                    );
                    w.flag(false);
                    w.ue(kept.len() as u32);
                    w.ue(0);
                    let mut previous = poc;
                    for &(pts, _) in &kept {
                        let p = poc_of(pts);
                        w.ue((previous - p - 1) as u32);
                        w.flag(Some(pts) == reference_pts);
                        previous = p;
                    }
                    if t.temporal_mvp {
                        w.flag(true);
                    }
                }
                if t.sao {
                    w.flag(true);
                    w.flag(true);
                }
                if !frame.key {
                    w.flag(false);
                    if self.low_delay_b {
                        w.flag(false);
                        if t.temporal_mvp {
                            w.flag(true);
                        }
                    }
                    w.ue(0);
                }
                w.se(slice_qp_delta);
                w.flag(false);
                w.trailing_bits();
                let header = nal_unit(&[(pic.nal_unit_type << 1), 0x01], w.as_bytes());
                let bits = 8 * header.len() as u32;
                out.push_packed(VAEncPackedHeaderSlice, &header, bits);
            }

            let mut slice: VAEncSliceParameterBufferHEVC = unsafe { std::mem::zeroed() };
            slice.slice_segment_address = address;
            slice.num_ctu_in_slice = count;
            slice.slice_type = slice_type as u8;
            slice.slice_pic_parameter_set_id = 0;
            slice.ref_pic_list0 = [invalid; 15];
            slice.ref_pic_list1 = [invalid; 15];
            if let Some((pts, surface)) = frame.reference {
                slice.ref_pic_list0[0] = picture(pts, surface);
                if self.low_delay_b {
                    slice.ref_pic_list1[0] = picture(pts, surface);
                }
            }
            slice.max_num_merge_cand = 5;
            slice.slice_qp_delta = slice_qp_delta as i8;
            unsafe {
                let f = &mut slice.slice_fields.bits;
                f.set_last_slice_of_pic_flag(last as u32);
                f.set_slice_temporal_mvp_enabled_flag((t.temporal_mvp && !frame.key) as u32);
                f.set_slice_sao_luma_flag(t.sao as u32);
                f.set_slice_sao_chroma_flag(t.sao as u32);
                f.set_collocated_from_l0_flag(1);
            }
            out.push(VAEncSliceParameterBufferType, &slice);
        }
        Ok(())
    }
}
