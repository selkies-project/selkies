/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The VP8 arm: the sequence and picture parameters and the quantizer matrix; the driver
//! writes the frame header. The three buffers, LAST, GOLDEN, and ALTREF, follow the session's
//! slot plan, and no frame updates the entropy probabilities, so a frame a client lost leaves
//! nothing behind that a later one depends on.

use va_sys::*;

use super::super::reference::SlotRefresh;
use super::{Buffers, Frame, Negotiated};

const LOOP_FILTER_LEVEL: i8 = 16;
const SHARPNESS: u8 = 4;

pub(super) struct Arm {
    sequence: VAEncSequenceParameterBufferVP8,
}

impl Arm {
    pub(super) fn new() -> Self {
        Self {
            sequence: unsafe { std::mem::zeroed() },
        }
    }

    pub(super) fn configure(&mut self, n: &Negotiated) {
        let s = &mut self.sequence;
        *s = unsafe { std::mem::zeroed() };
        s.frame_width = n.width;
        s.frame_height = n.height;
        s.error_resilient = 0;
        s.kf_auto = 0;
        if n.rc_mode != VA_RC_CQP {
            s.bits_per_second = n.bits_per_second;
            s.intra_period = i32::MAX as u32;
        }
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
        let plan = frame.slots;
        let mut pic: VAEncPictureParameterBufferVP8 = unsafe { std::mem::zeroed() };
        pic.reconstructed_frame = frame.recon;
        pic.coded_buf = frame.coded;
        if frame.key {
            pic.ref_last_frame = VA_INVALID_SURFACE;
            pic.ref_gf_frame = VA_INVALID_SURFACE;
            pic.ref_arf_frame = VA_INVALID_SURFACE;
        } else {
            if plan.predict_from == 0 {
                return Err("a predicted VP8 picture without a reference".into());
            }
            pic.ref_last_frame = frame.slot_surfaces[0];
            pic.ref_gf_frame = frame.slot_surfaces[1];
            pic.ref_arf_frame = frame.slot_surfaces[2];
        }
        unsafe {
            let r = &mut pic.ref_flags.bits;
            r.set_force_kf(frame.key as u32);
            r.set_no_ref_last((plan.predict_from != SlotRefresh::LAST) as u32);
            r.set_no_ref_gf((plan.predict_from != SlotRefresh::GOLDEN) as u32);
            r.set_no_ref_arf((plan.predict_from != SlotRefresh::ALTREF) as u32);
            let p = &mut pic.pic_flags.bits;
            p.set_frame_type((!frame.key) as u32);
            p.set_show_frame(1);
            p.set_refresh_entropy_probs(0);
            p.set_refresh_last(plan.refresh.refreshes(SlotRefresh::LAST) as u32);
            p.set_refresh_golden_frame(plan.refresh.refreshes(SlotRefresh::GOLDEN) as u32);
            p.set_refresh_alternate_frame(plan.refresh.refreshes(SlotRefresh::ALTREF) as u32);
        }
        pic.loop_filter_level = [LOOP_FILTER_LEVEL; 4];
        pic.sharpness_level = SHARPNESS;
        pic.clamp_qindex_low = 0;
        pic.clamp_qindex_high = 127;
        out.push(VAEncPictureParameterBufferType, &pic);

        let q = if n.rc_mode == VA_RC_CQP {
            frame.qp.min(127) as u16
        } else {
            0
        };
        let mut quant: VAQMatrixBufferVP8 = unsafe { std::mem::zeroed() };
        quant.quantization_index = [q; 4];
        out.push(VAQMatrixBufferType, &quant);
        Ok(())
    }
}
