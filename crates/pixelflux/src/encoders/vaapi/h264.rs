/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The H.264 arm: the sequence, picture, and slice parameters, the packed SPS, PPS, and slice
//! headers, and the reference lists behind them. The stream is High profile with CABAC and the
//! 8x8 transform where the driver serves High, a single reference per slice, and a decoded
//! picture buffer of the frames the level admits: a frame predicts from the newest one the
//! client still has, which the slice header's reference list modification names whenever that
//! is not the previous frame.

use va_sys::*;

use super::super::bits::{BitWriter, nal_unit};
use super::super::codec::h264_level;
use super::{Buffers, Frame, Negotiated};

/// `log2_max_frame_num_minus4`: sixteen bits of `frame_num`, so the wrap that costs a key
/// frame comes every 65536 frames.
const LOG2_MAX_FRAME_NUM_MINUS4: u32 = 12;
/// The quantizer the PPS names; every slice carries its own delta from it.
const PIC_INIT_QP: u32 = 26;
/// The lowest quantizer a picture is coded at: below it radeonsi's VCN 1 codes sharp-edged intra
/// blocks that decode wrong.
pub(super) const MIN_QP: u32 = 7;

pub(super) struct Arm {
    profile: VAProfile,
    level_idc: u32,
    mb_width: u32,
    mb_height: u32,
    /// Slices per picture and macroblock rows per slice.
    slices: (u32, u32),
    cabac: bool,
    transform_8x8: bool,
    dpb: u32,
    sequence: VAEncSequenceParameterBufferH264,
    sps: Vec<u8>,
    pps: Vec<u8>,
    idr_pic_id: u16,
}

impl Arm {
    pub(super) fn new(profile: VAProfile) -> Self {
        Self {
            profile,
            level_idc: 0,
            mb_width: 0,
            mb_height: 0,
            slices: (1, 1),
            cabac: profile != VAProfileH264ConstrainedBaseline,
            transform_8x8: profile == VAProfileH264High,
            dpb: 1,
            sequence: unsafe { std::mem::zeroed() },
            sps: Vec::new(),
            pps: Vec::new(),
            idr_pic_id: 0,
        }
    }

    /// How many values `frame_num` takes before it wraps.
    pub(super) fn frame_num_range(&self) -> u32 {
        1 << (4 + LOG2_MAX_FRAME_NUM_MINUS4)
    }

    pub(super) fn slices(&self) -> (u32, u32) {
        self.slices
    }

    /// The `level_idc` the sequence names.
    pub(super) fn level(&self) -> u32 {
        self.level_idc
    }

    /// The stream's sequence: the level the ladder names at this geometry and rate, never
    /// below the one the decoded picture buffer was sized for, the picture size in macroblocks
    /// with the cropping that trims it, and the SPS and PPS the key frames carry.
    pub(super) fn configure(
        &mut self,
        n: &Negotiated,
        surface_width: u32,
        surface_height: u32,
        slices: (u32, u32),
    ) {
        self.mb_width = surface_width / 16;
        self.mb_height = surface_height / 16;
        self.slices = slices;
        self.dpb = n.dpb;
        self.level_idc = h264_level(
            n.width,
            n.height,
            n.fps.ceil(),
            u64::from(n.bits_per_second),
        )
        .max(n.dpb_level);
        let profile_idc = if self.profile == VAProfileH264High {
            100
        } else if self.profile == VAProfileH264Main {
            77
        } else {
            66
        };
        let crop = (self.mb_width * 16 != n.width) || (self.mb_height * 16 != n.height);

        let mut w = BitWriter::new();
        w.u(8, profile_idc);
        let (set1, set3, set4, set5) = (
            profile_idc != 100,
            false,
            profile_idc != 66,
            profile_idc != 66,
        );
        w.flag(false);
        w.flag(set1);
        w.flag(false);
        w.flag(set3);
        w.flag(set4);
        w.flag(set5);
        w.u(2, 0);
        w.u(8, u64::from(self.level_idc));
        w.ue(0);
        if profile_idc == 100 {
            w.ue(1);
            w.ue(0);
            w.ue(0);
            w.flag(false);
            w.flag(false);
        }
        w.ue(LOG2_MAX_FRAME_NUM_MINUS4);
        w.ue(2);
        w.ue(self.dpb);
        w.flag(false);
        w.ue(self.mb_width - 1);
        w.ue(self.mb_height - 1);
        w.flag(true);
        w.flag(true);
        w.flag(crop);
        if crop {
            w.ue(0);
            w.ue((self.mb_width * 16 - n.width) / 2);
            w.ue(0);
            w.ue((self.mb_height * 16 - n.height) / 2);
        }
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
        w.flag(true);
        w.u(32, u64::from(n.fps.den));
        w.u(32, 2 * u64::from(n.fps.num));
        w.flag(true);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(true);
        w.flag(true);
        w.ue(0);
        w.ue(0);
        w.ue(15);
        w.ue(15);
        w.ue(0);
        w.ue(self.dpb);
        w.trailing_bits();
        self.sps = nal_unit(&[0x67], w.as_bytes());

        let mut w = BitWriter::new();
        w.ue(0);
        w.ue(0);
        w.flag(self.cabac);
        w.flag(false);
        w.ue(0);
        w.ue(0);
        w.ue(0);
        w.flag(false);
        w.u(2, 0);
        w.se(PIC_INIT_QP as i32 - 26);
        w.se(0);
        w.se(0);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        if self.transform_8x8 {
            w.flag(true);
            w.flag(false);
            w.se(0);
        }
        w.trailing_bits();
        self.pps = nal_unit(&[0x68], w.as_bytes());

        let s = &mut self.sequence;
        *s = unsafe { std::mem::zeroed() };
        s.seq_parameter_set_id = 0;
        s.level_idc = self.level_idc.min(n.h264_driver_level) as u8;
        s.intra_period = i32::MAX as u32;
        s.intra_idr_period = i32::MAX as u32;
        s.ip_period = 1;
        s.bits_per_second = n.bits_per_second;
        s.max_num_ref_frames = self.dpb;
        s.picture_width_in_mbs = self.mb_width as u16;
        s.picture_height_in_mbs = self.mb_height as u16;
        unsafe {
            let f = &mut s.seq_fields.bits;
            f.set_chroma_format_idc(1);
            f.set_frame_mbs_only_flag(1);
            f.set_direct_8x8_inference_flag(1);
            f.set_log2_max_frame_num_minus4(LOG2_MAX_FRAME_NUM_MINUS4);
            f.set_pic_order_cnt_type(2);
            let v = &mut s.vui_fields.bits;
            v.set_timing_info_present_flag(1);
            v.set_bitstream_restriction_flag(1);
            v.set_log2_max_mv_length_horizontal(15);
            v.set_log2_max_mv_length_vertical(15);
        }
        s.frame_cropping_flag = u8::from(crop);
        if crop {
            s.frame_crop_right_offset = (self.mb_width * 16 - n.width) / 2;
            s.frame_crop_bottom_offset = (self.mb_height * 16 - n.height) / 2;
        }
        s.vui_parameters_present_flag = 1;
        s.num_units_in_tick = n.fps.den;
        s.time_scale = 2 * n.fps.num;
    }

    pub(super) fn sequence(&mut self, n: &Negotiated, out: &mut Buffers) {
        out.push(VAEncSequenceParameterBufferType, &self.sequence);
        if n.packed & VA_ENC_PACKED_HEADER_SEQUENCE != 0 {
            let mut bytes = self.sps.clone();
            bytes.extend_from_slice(&self.pps);
            let bits = 8 * bytes.len() as u32;
            out.push_packed(VAEncPackedHeaderSequence, &bytes, bits);
        }
    }

    /// The picture: its frame number and order count since the key frame, the decoded picture
    /// buffer as `ReferenceFrames`, and every slice with its packed header, whose reference
    /// list modification moves any reference but the newest short-term frame, which the
    /// default list puts first, to the front: a short-term one by its distance in frame numbers,
    /// an anchor by its long-term index. In a session keeping anchors the key frame is marked
    /// long-term under index 0 and each anchor after it under its index (memory management
    /// operation 6), the one it is marked over leaving the buffer.
    pub(super) fn picture(
        &mut self,
        n: &Negotiated,
        frame: &Frame,
        out: &mut Buffers,
    ) -> Result<(), String> {
        let frame_num = ((frame.pts - frame.key_pts) % u64::from(self.frame_num_range())) as u32;
        let poc = 2 * (frame.pts - frame.key_pts) as i32;
        if frame.key {
            self.idr_pic_id = self
                .idr_pic_id
                .wrapping_add(if frame.pts == 0 { 0 } else { 1 });
        }
        let long_term = |pts: u64| frame.long_term.iter().find(|l| l.0 == pts).map(|l| l.1);
        let picture = |pts: u64, surface: VASurfaceID| VAPictureH264 {
            picture_id: surface,
            frame_idx: long_term(pts).map_or(
                ((pts - frame.key_pts) % u64::from(self.frame_num_range())) as u32,
                u32::from,
            ),
            flags: if long_term(pts).is_some() {
                VA_PICTURE_H264_LONG_TERM_REFERENCE
            } else {
                VA_PICTURE_H264_SHORT_TERM_REFERENCE
            },
            TopFieldOrderCnt: 2 * (pts - frame.key_pts) as i32,
            BottomFieldOrderCnt: 2 * (pts - frame.key_pts) as i32,
            va_reserved: [0; 4],
        };
        let invalid = VAPictureH264 {
            picture_id: VA_INVALID_ID,
            frame_idx: 0,
            flags: VA_PICTURE_H264_INVALID,
            TopFieldOrderCnt: 0,
            BottomFieldOrderCnt: 0,
            va_reserved: [0; 4],
        };

        let marked = if frame.key && frame.anchored {
            Some(0)
        } else {
            frame.anchor
        };
        let mut pic: VAEncPictureParameterBufferH264 = unsafe { std::mem::zeroed() };
        pic.CurrPic = VAPictureH264 {
            picture_id: frame.recon,
            frame_idx: marked.map_or(frame_num, u32::from),
            flags: if marked.is_some() {
                VA_PICTURE_H264_LONG_TERM_REFERENCE
            } else {
                0
            },
            TopFieldOrderCnt: poc,
            BottomFieldOrderCnt: poc,
            va_reserved: [0; 4],
        };
        pic.ReferenceFrames = [invalid; 16];
        let mut held: Vec<(u64, VASurfaceID)> = frame
            .held
            .iter()
            .filter(|h| !frame.key && h.0 >= frame.key_pts)
            .map(|h| (h.0, h.1))
            .collect();
        held.sort_by_key(|h| std::cmp::Reverse(h.0));
        for (i, &(pts, surface)) in held.iter().take(16).enumerate() {
            pic.ReferenceFrames[i] = picture(pts, surface);
        }
        pic.coded_buf = frame.coded;
        pic.frame_num = frame_num as u16;
        pic.pic_init_qp = PIC_INIT_QP as u8;
        unsafe {
            let f = &mut pic.pic_fields.bits;
            f.set_idr_pic_flag(u32::from(frame.key));
            f.set_reference_pic_flag(1);
            f.set_entropy_coding_mode_flag(u32::from(self.cabac));
            f.set_transform_8x8_mode_flag(u32::from(self.transform_8x8));
        }
        out.push(VAEncPictureParameterBufferType, &pic);

        let reference = frame.reference.map(|(pts, surface)| picture(pts, surface));
        if !frame.key && reference.is_none() {
            return Err("a predicted H.264 picture without a reference".into());
        }
        let slice_qp = if n.rc_mode == VA_RC_CQP {
            frame.qp.clamp(MIN_QP, 51)
        } else {
            PIC_INIT_QP
        };
        let slice_qp_delta = slice_qp as i32 - PIC_INIT_QP as i32;
        let (slice_count, slice_rows) = self.slices;
        for i in 0..slice_count {
            let first_row = i * slice_rows;
            let rows = if i + 1 == slice_count {
                self.mb_height - first_row
            } else {
                slice_rows
            };
            let first_mb = first_row * self.mb_width;
            let mbs = rows * self.mb_width;
            let slice_type = if frame.key { 7 } else { 5 };

            if n.packed & VA_ENC_PACKED_HEADER_SLICE != 0 {
                let mut w = BitWriter::new();
                w.ue(first_mb);
                w.ue(slice_type);
                w.ue(0);
                w.u(4 + LOG2_MAX_FRAME_NUM_MINUS4, u64::from(frame_num));
                if frame.key {
                    w.ue(u32::from(self.idr_pic_id));
                }
                if !frame.key {
                    w.flag(false);
                    let (ref_pts, _) = frame.reference.unwrap();
                    if let Some(index) = long_term(ref_pts) {
                        w.flag(true);
                        w.ue(2);
                        w.ue(u32::from(index));
                        w.ue(3);
                    } else {
                        let ref_frame_num =
                            ((ref_pts - frame.key_pts) % u64::from(self.frame_num_range())) as u32;
                        let newest = held.iter().find(|h| long_term(h.0).is_none()).map(|h| h.0)
                            == Some(ref_pts);
                        w.flag(!newest);
                        if !newest {
                            let diff = (frame_num + self.frame_num_range() - ref_frame_num)
                                % self.frame_num_range();
                            w.ue(0);
                            w.ue(diff - 1);
                            w.ue(3);
                        }
                    }
                }
                if frame.key {
                    w.flag(false);
                    w.flag(frame.anchored);
                } else if let Some(index) = frame.anchor {
                    w.flag(true);
                    w.ue(6);
                    w.ue(u32::from(index));
                    w.ue(0);
                } else {
                    w.flag(false);
                }
                if self.cabac && !frame.key {
                    w.ue(0);
                }
                w.se(slice_qp_delta);
                let header = nal_unit(if frame.key { &[0x65] } else { &[0x41] }, w.as_bytes());
                let bits = 8 * (header.len() as u32 - w.as_bytes().len() as u32) + w.len() as u32;
                out.push_packed(VAEncPackedHeaderSlice, &header, bits);
            }

            let mut slice: VAEncSliceParameterBufferH264 = unsafe { std::mem::zeroed() };
            slice.macroblock_address = first_mb;
            slice.num_macroblocks = mbs;
            slice.macroblock_info = VA_INVALID_ID;
            slice.slice_type = (slice_type % 5) as u8;
            slice.pic_parameter_set_id = 0;
            slice.idr_pic_id = self.idr_pic_id;
            slice.pic_order_cnt_lsb = 0;
            slice.direct_spatial_mv_pred_flag = 1;
            slice.RefPicList0 = [invalid; 32];
            slice.RefPicList1 = [invalid; 32];
            if let Some(r) = reference {
                slice.RefPicList0[0] = r;
            }
            slice.slice_qp_delta = slice_qp_delta as i8;
            out.push(VAEncSliceParameterBufferType, &slice);
        }
        Ok(())
    }
}
