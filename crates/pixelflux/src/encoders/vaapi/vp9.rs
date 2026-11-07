/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The VP9 arm: the sequence and picture parameters; the driver writes the uncompressed
//! header. The eight buffer slots hold the last eight frames by timestamp, a frame refreshes
//! its own and predicts from the slot of the newest frame the client still has, and every
//! frame is coded in the error-resilient mode, since a decoder otherwise takes the previous
//! frame's motion vectors and probability contexts whatever the reference says.

use va_sys::*;

use super::super::reference::REFERENCE_FRAMES;
use super::{Buffers, Frame, Negotiated};

const LOOP_FILTER_LEVEL: u8 = 16;
const SHARPNESS: u8 = 4;
/// The widest tile the codec allows, which sets the fewest tile columns a picture needs.
const MAX_TILE_WIDTH: u32 = 4096;

pub(super) struct Arm {
    sequence: VAEncSequenceParameterBufferVP9,
    log2_tile_columns: u8,
}

impl Arm {
    pub(super) fn new() -> Self {
        Self {
            sequence: unsafe { std::mem::zeroed() },
            log2_tile_columns: 0,
        }
    }

    pub(super) fn configure(&mut self, n: &Negotiated) {
        let s = &mut self.sequence;
        *s = unsafe { std::mem::zeroed() };
        s.max_frame_width = n.width;
        s.max_frame_height = n.height;
        s.kf_auto = 0;
        if n.rc_mode != VA_RC_CQP {
            s.bits_per_second = n.bits_per_second;
            s.intra_period = i32::MAX as u32;
        }
        let columns = n.width.div_ceil(MAX_TILE_WIDTH);
        self.log2_tile_columns = if columns <= 1 {
            0
        } else {
            (32 - (columns - 1).leading_zeros()) as u8
        };
    }

    pub(super) fn sequence(&mut self, _n: &Negotiated, out: &mut Buffers) {
        out.push(VAEncSequenceParameterBufferType, &self.sequence);
    }

    pub(super) fn picture(
        &mut self,
        n: &Negotiated,
        frame: &Frame,
        out: &mut Buffers,
    ) -> Result<(), String> {
        let slot_of = |pts: u64| (pts % REFERENCE_FRAMES as u64) as u8;
        let mut pic: VAEncPictureParameterBufferVP9 = unsafe { std::mem::zeroed() };
        pic.frame_width_src = n.width;
        pic.frame_height_src = n.height;
        pic.frame_width_dst = n.width;
        pic.frame_height_dst = n.height;
        pic.reconstructed_frame = frame.recon;
        pic.coded_buf = frame.coded;
        pic.reference_frames = [VA_INVALID_SURFACE; 8];
        for &(pts, surface, _) in frame.held {
            if !frame.key && pts >= frame.key_pts {
                pic.reference_frames[slot_of(pts) as usize] = surface;
            }
        }
        pic.log2_tile_columns = self.log2_tile_columns;
        unsafe {
            let r = &mut pic.ref_flags.bits;
            if frame.key {
                r.set_force_kf(1);
                pic.refresh_frame_flags = 0xff;
            } else {
                let (ref_pts, _) = frame
                    .reference
                    .ok_or("a predicted VP9 picture without a reference")?;
                let slot = slot_of(ref_pts) as u32;
                r.set_ref_frame_ctrl_l0(1);
                r.set_ref_last_idx(slot);
                r.set_ref_last_sign_bias(1);
                r.set_ref_gf_idx(slot);
                r.set_ref_arf_idx(slot);
                pic.refresh_frame_flags = 1 << slot_of(frame.pts);
            }
            let p = &mut pic.pic_flags.bits;
            p.set_frame_type((!frame.key) as u32);
            p.set_show_frame(1);
            p.set_error_resilient_mode(1);
        }
        pic.luma_ac_qindex = if n.rc_mode == VA_RC_CQP {
            frame.qp.min(255) as u8
        } else {
            100
        };
        pic.filter_level = LOOP_FILTER_LEVEL;
        pic.sharpness_level = SHARPNESS;
        out.push(VAEncPictureParameterBufferType, &pic);
        Ok(())
    }
}
