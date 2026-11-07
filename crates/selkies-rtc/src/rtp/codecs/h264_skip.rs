/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A picture that stands in for an H.264 frame a peer is not sent: every macroblock
//! skipped, so it repeats the picture before it, under the left-out frame's own frame_num,
//! picture order, reference list modification, weights, and reference marking. A decoder
//! that refuses a frame_num gap (OpenH264, which Firefox decodes WebRTC H.264 with where it
//! has no system FFmpeg) then takes the stream whole, its decoded picture buffer the
//! encoder's in every structure but the pixels of the left-out frames, which the encoder
//! predicts past.
//!
//! Only the slice header's entropy context index, quantizer, and deblocking change:
//! `cabac_init_idc` 0 and `SKIP_QP` let one arithmetic-coded run of skips serve every slice
//! of a size, and a picture of skips has no edge to filter. A slice this cannot rewrite (an
//! I slice, slice groups, separate color planes, a redundant picture) gets no stand-in.

use std::collections::HashMap;

/// Table 9-44: rangeTabLPS, by pStateIdx and qCodIRangeIdx.
pub const RANGE_LPS: [[u32; 4]; 64] = [
    [128, 176, 208, 240],
    [128, 167, 197, 227],
    [128, 158, 187, 216],
    [123, 150, 178, 205],
    [116, 142, 169, 195],
    [111, 135, 160, 185],
    [105, 128, 152, 175],
    [100, 122, 144, 166],
    [95, 116, 137, 158],
    [90, 110, 130, 150],
    [85, 104, 123, 142],
    [81, 99, 117, 135],
    [77, 94, 111, 128],
    [73, 89, 105, 122],
    [69, 85, 100, 116],
    [66, 80, 95, 110],
    [62, 76, 90, 104],
    [59, 72, 86, 99],
    [56, 69, 81, 94],
    [53, 65, 77, 89],
    [51, 62, 73, 85],
    [48, 59, 69, 80],
    [46, 56, 66, 76],
    [43, 53, 63, 72],
    [41, 50, 59, 69],
    [39, 48, 56, 65],
    [37, 45, 54, 62],
    [35, 43, 51, 59],
    [33, 41, 48, 56],
    [32, 39, 46, 53],
    [30, 37, 43, 50],
    [29, 35, 41, 48],
    [27, 33, 39, 45],
    [26, 31, 37, 43],
    [24, 30, 35, 41],
    [23, 28, 33, 39],
    [22, 27, 32, 37],
    [21, 26, 30, 35],
    [20, 24, 29, 33],
    [19, 23, 27, 31],
    [18, 22, 26, 30],
    [17, 21, 25, 28],
    [16, 20, 23, 27],
    [15, 19, 22, 25],
    [14, 18, 21, 24],
    [14, 17, 20, 23],
    [13, 16, 19, 22],
    [12, 15, 18, 21],
    [12, 14, 17, 20],
    [11, 14, 16, 19],
    [11, 13, 15, 18],
    [10, 12, 15, 17],
    [10, 12, 14, 16],
    [9, 11, 13, 15],
    [9, 11, 12, 14],
    [8, 10, 12, 14],
    [8, 9, 11, 13],
    [7, 9, 11, 12],
    [7, 9, 10, 12],
    [7, 8, 10, 11],
    [6, 8, 9, 11],
    [6, 7, 9, 10],
    [6, 7, 8, 9],
    [2, 2, 2, 2],
];
/// Table 9-45: transIdxLPS, by pStateIdx.
pub const TRANS_LPS: [usize; 64] = [
    0, 0, 1, 2, 2, 4, 4, 5, 6, 7, 8, 9, 9, 11, 11, 12, 13, 13, 15, 15, 16, 16, 18, 18, 19, 19, 21,
    21, 22, 22, 23, 24, 24, 25, 26, 26, 27, 27, 28, 29, 29, 30, 30, 30, 31, 32, 32, 33, 33, 33, 34,
    34, 35, 35, 35, 36, 36, 36, 37, 37, 37, 38, 38, 63,
];
/// Table 9-13: (m, n) of mb_skip_flag's context in a P slice whose neighbors are skipped or
/// absent (ctxIdx 11), at cabac_init_idc 0.
pub const SKIP_CONTEXT: (i32, i32) = (23, 33);
/// The quantizer a stand-in declares. It codes no residual, so only the arithmetic coder's
/// start reads it.
pub const SKIP_QP: i32 = 26;
const P_SLICE: u32 = 0;
pub const NAL_SLICE: u8 = 1;
pub const NAL_IDR: u8 = 5;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
/// The bytes of a slice a stand-in is read from: its header, which encoders keep to a few
/// dozen bytes. A header running past them gets no stand-in.
pub const HEAD_BYTES: usize = 256;

/// A read past the end of a NAL unit, or a syntax element this does not rewrite.
#[derive(Debug, PartialEq, Eq)]
pub struct Unreadable;

/// Exp-Golomb and fixed-width reads over a NAL unit's payload, emulation prevention removed.
pub struct Reader {
    rbsp: Vec<u8>,
    pub pos: usize,
}

impl Reader {
    pub fn new(nal: &[u8]) -> Self {
        let mut rbsp = Vec::with_capacity(nal.len());
        let mut zeros = 0;
        for &byte in nal.iter().skip(1) {
            if zeros >= 2 && byte == 3 {
                zeros = 0;
                continue;
            }
            zeros = if byte == 0 { zeros + 1 } else { 0 };
            rbsp.push(byte);
        }
        Self { rbsp, pos: 0 }
    }

    pub fn size(&self) -> usize {
        self.rbsp.len() * 8
    }

    pub fn u(&mut self, n: usize) -> Result<u32, Unreadable> {
        if n == 0 {
            return Ok(0);
        }
        if self.pos + n > self.size() {
            return Err(Unreadable);
        }
        let mut value = 0u32;
        for _ in 0..n {
            let bit = (self.rbsp[self.pos >> 3] >> (7 - (self.pos & 7))) & 1;
            value = (value << 1) | bit as u32;
            self.pos += 1;
        }
        Ok(value)
    }

    /// `u` for a field that may run to 64 bits.
    pub fn u64(&mut self, n: usize) -> Result<u64, Unreadable> {
        let mut value = 0u64;
        let mut left = n;
        while left > 0 {
            let take = left.min(32);
            value = (value << take) | self.u(take)? as u64;
            left -= take;
        }
        Ok(value)
    }

    pub fn ue(&mut self) -> Result<u32, Unreadable> {
        let mut zeros = 0;
        while self.u(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err(Unreadable);
            }
        }
        Ok(((1u64 << zeros) - 1 + self.u(zeros)? as u64) as u32)
    }

    pub fn se(&mut self) -> Result<i32, Unreadable> {
        let k = self.ue()? as i64;
        Ok(if k % 2 == 1 {
            ((k + 1) / 2) as i32
        } else {
            (-(k / 2)) as i32
        })
    }
}

/// Bits gathered into an RBSP, written out as a NAL unit with emulation prevention.
#[derive(Default)]
pub struct Writer {
    bits: Vec<u8>,
}

impl Writer {
    pub fn u(&mut self, n: usize, v: u32) {
        for i in (0..n).rev() {
            self.bits.push(((v >> i) & 1) as u8);
        }
    }

    pub fn ue(&mut self, v: u32) {
        let code = v as u64 + 1;
        let width = 64 - code.leading_zeros() as usize;
        for _ in 0..width - 1 {
            self.bits.push(0);
        }
        for i in (0..width).rev() {
            self.bits.push(((code >> i) & 1) as u8);
        }
    }

    pub fn se(&mut self, v: i32) {
        let v = v as i64;
        self.ue(if v > 0 { 2 * v - 1 } else { -2 * v } as u32);
    }

    pub fn raw(&mut self, bits: &[u8]) {
        self.bits.extend_from_slice(bits);
    }

    pub fn length(&self) -> usize {
        self.bits.len()
    }

    pub fn nal(&self, header: u8) -> Vec<u8> {
        let mut bits = self.bits.clone();
        while !bits.len().is_multiple_of(8) {
            bits.push(0);
        }
        let rbsp: Vec<u8> = bits
            .chunks(8)
            .map(|c| c.iter().fold(0u8, |acc, b| (acc << 1) | b))
            .collect();
        let mut out = vec![header];
        let mut zeros = 0;
        for byte in rbsp {
            if zeros >= 2 && byte <= 3 {
                out.push(3);
                zeros = 0;
            }
            out.push(byte);
            zeros = if byte == 0 { zeros + 1 } else { 0 };
        }
        out
    }
}

fn skip_scaling_list(r: &mut Reader, size: usize) -> Result<(), Unreadable> {
    let mut last = 8i32;
    let mut following = 8i32;
    for _ in 0..size {
        if following != 0 {
            following = (last + r.se()? + 256).rem_euclid(256);
        }
        last = if following != 0 { following } else { last };
    }
    Ok(())
}

/// A `log2_max_*_minus4` field as the bit width it names, 16 at most by the specification
/// (7.4.2.1.1); wider is no SPS this reads.
fn log2_field(minus4: u32) -> Result<usize, Unreadable> {
    if minus4 > 12 {
        return Err(Unreadable);
    }
    Ok(minus4 as usize + 4)
}

/// The fields of a sequence parameter set a slice header's syntax depends on.
#[derive(Clone, Debug, Default)]
pub struct Sps {
    pub chroma_format_idc: u32,
    pub separate_colour_plane: u32,
    pub log2_max_frame_num: usize,
    pub poc_type: u32,
    pub log2_max_poc_lsb: usize,
    pub delta_pic_order_always_zero: u32,
    pub frame_mbs_only: u32,
    pub mbs: u32,
}

pub fn parse_sps(nal: &[u8]) -> Result<(u32, Sps), Unreadable> {
    let mut r = Reader::new(nal);
    let profile = r.u(8)?;
    r.u(16)?;
    let sps_id = r.ue()?;
    let mut sps = Sps {
        chroma_format_idc: 1,
        ..Default::default()
    };
    if [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].contains(&profile) {
        sps.chroma_format_idc = r.ue()?;
        if sps.chroma_format_idc == 3 {
            sps.separate_colour_plane = r.u(1)?;
        }
        r.ue()?;
        r.ue()?;
        r.u(1)?;
        if r.u(1)? == 1 {
            let lists = if sps.chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..lists {
                if r.u(1)? == 1 {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    sps.log2_max_frame_num = log2_field(r.ue()?)?;
    sps.poc_type = r.ue()?;
    if sps.poc_type == 0 {
        sps.log2_max_poc_lsb = log2_field(r.ue()?)?;
    } else if sps.poc_type == 1 {
        sps.delta_pic_order_always_zero = r.u(1)?;
        r.se()?;
        r.se()?;
        let cycles = r.ue()?;
        for _ in 0..cycles {
            r.se()?;
        }
    }
    r.ue()?;
    r.u(1)?;
    let width = r.ue()? + 1;
    let height_units = r.ue()? + 1;
    sps.frame_mbs_only = r.u(1)?;
    sps.mbs = width
        .checked_mul(height_units)
        .and_then(|mbs| mbs.checked_mul(2 - sps.frame_mbs_only))
        .ok_or(Unreadable)?;
    Ok((sps_id, sps))
}

/// The fields of a picture parameter set a slice header's syntax depends on.
#[derive(Clone, Debug, Default)]
pub struct Pps {
    pub sps_id: u32,
    pub cabac: u32,
    pub bottom_field_poc: u32,
    pub slice_groups: u32,
    pub num_ref_idx_l0_default: u32,
    pub weighted_pred: u32,
    pub pic_init_qp: i32,
    pub deblocking_control: u32,
    pub redundant_pic_cnt: u32,
}

pub fn parse_pps(nal: &[u8]) -> Result<(u32, Pps), Unreadable> {
    let mut r = Reader::new(nal);
    let pps_id = r.ue()?;
    let mut pps = Pps {
        sps_id: r.ue()?,
        cabac: r.u(1)?,
        bottom_field_poc: r.u(1)?,
        slice_groups: r.ue()? + 1,
        ..Default::default()
    };
    if pps.slice_groups > 1 {
        return Ok((pps_id, pps));
    }
    pps.num_ref_idx_l0_default = r.ue()? + 1;
    r.ue()?;
    pps.weighted_pred = r.u(1)?;
    r.u(2)?;
    pps.pic_init_qp = r.se()?.checked_add(26).ok_or(Unreadable)?;
    r.se()?;
    r.se()?;
    pps.deblocking_control = r.u(1)?;
    r.u(1)?;
    pps.redundant_pic_cnt = r.u(1)?;
    Ok((pps_id, pps))
}

/// The slice data, from its first byte, of `mbs` macroblocks each skipped, the last ending
/// the slice: mb_skip_flag 1 and end_of_slice_flag 0 for each, the arithmetic coder flushed
/// after the last end_of_slice_flag 1 (9.3.4), its stop bit included.
fn cabac_skips(mbs: u32) -> Vec<u8> {
    let (m, n) = SKIP_CONTEXT;
    let pre = (((m * SKIP_QP) >> 4) + n).clamp(1, 126);
    let (mut state, mut mps): (usize, u32) = if pre <= 63 {
        ((63 - pre) as usize, 0)
    } else {
        ((pre - 64) as usize, 1)
    };
    let mut low: u32 = 0;
    let mut rng: u32 = 510;
    let mut outstanding = 0usize;
    let mut first = true;
    let mut bits: Vec<u8> = Vec::with_capacity(mbs as usize / 4 + 16);

    fn put(bits: &mut Vec<u8>, first: &mut bool, outstanding: &mut usize, bit: u8) {
        if *first {
            *first = false;
        } else {
            bits.push(bit);
        }
        if *outstanding > 0 {
            bits.extend(std::iter::repeat_n(1 - bit, *outstanding));
            *outstanding = 0;
        }
    }

    fn renorm(
        bits: &mut Vec<u8>,
        first: &mut bool,
        outstanding: &mut usize,
        low: &mut u32,
        rng: &mut u32,
    ) {
        while *rng < 256 {
            if *low < 256 {
                put(bits, first, outstanding, 0);
            } else if *low >= 512 {
                *low -= 512;
                put(bits, first, outstanding, 1);
            } else {
                *low -= 256;
                *outstanding += 1;
            }
            *rng <<= 1;
            *low <<= 1;
        }
    }

    for i in 0..mbs {
        let lps = RANGE_LPS[state][((rng >> 6) & 3) as usize];
        rng -= lps;
        if mps != 1 {
            low += rng;
            rng = lps;
            if state == 0 {
                mps = 1 - mps;
            }
            state = TRANS_LPS[state];
        } else {
            state = (state + 1).min(62);
        }
        renorm(&mut bits, &mut first, &mut outstanding, &mut low, &mut rng);
        rng -= 2;
        if i < mbs - 1 {
            renorm(&mut bits, &mut first, &mut outstanding, &mut low, &mut rng);
        }
    }
    low += rng;
    rng = 2;
    renorm(&mut bits, &mut first, &mut outstanding, &mut low, &mut rng);
    put(
        &mut bits,
        &mut first,
        &mut outstanding,
        ((low >> 9) & 1) as u8,
    );
    let tail = ((low >> 7) & 3) | 1;
    bits.push(((tail >> 1) & 1) as u8);
    bits.push((tail & 1) as u8);
    bits
}

/// Stand-ins for the frames of one stream a peer is not sent (`stand_in`), read against the
/// parameter sets its key frames carried (`learn`).
#[derive(Default)]
pub struct SkipPictures {
    sps: HashMap<u32, Sps>,
    pps: HashMap<u32, Pps>,
    cabac: HashMap<u32, Vec<u8>>,
}

impl SkipPictures {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take the SPS and PPS among a frame's NAL units.
    pub fn learn<'a>(&mut self, nals: impl IntoIterator<Item = &'a [u8]>) {
        for nal in nals {
            let Some(&first) = nal.first() else {
                continue;
            };
            match first & 0x1F {
                NAL_SPS => {
                    if let Ok((id, sps)) = parse_sps(nal) {
                        self.sps.insert(id, sps);
                    }
                }
                NAL_PPS => {
                    if let Ok((id, pps)) = parse_pps(nal) {
                        self.pps.insert(id, pps);
                    }
                }
                _ => {}
            }
        }
    }

    /// A skipped slice for each slice of the frame `nals`, or `None` where one cannot be
    /// made. Only a slice's first `HEAD_BYTES` are read, so the caller may pass no more.
    pub fn stand_in<'a>(
        &mut self,
        nals: impl IntoIterator<Item = &'a [u8]>,
    ) -> Option<Vec<Vec<u8>>> {
        let nals: Vec<&[u8]> = nals.into_iter().collect();
        if nals
            .iter()
            .any(|n| n.first().is_some_and(|b| b & 0x1F == NAL_IDR))
        {
            return None;
        }
        let slices: Vec<&[u8]> = nals
            .iter()
            .filter(|n| n.first().is_some_and(|b| b & 0x1F == NAL_SLICE))
            .map(|n| &n[..n.len().min(HEAD_BYTES)])
            .collect();
        if slices.is_empty() {
            return None;
        }
        let starts: Vec<u32> = slices
            .iter()
            .map(|nal| Reader::new(nal).ue())
            .collect::<Result<_, _>>()
            .ok()?;
        let mut out = Vec::with_capacity(slices.len());
        for (k, nal) in slices.iter().enumerate() {
            let next = starts.get(k + 1).copied();
            out.push(self.skip_slice(nal, starts[k], next).ok()??);
        }
        Some(out)
    }

    fn skip_slice(
        &mut self,
        nal: &[u8],
        first_mb: u32,
        next_mb: Option<u32>,
    ) -> Result<Option<Vec<u8>>, Unreadable> {
        let mut r = Reader::new(nal);
        let mut w = Writer::default();
        w.ue(r.ue()?);
        let slice_type = r.ue()?;
        if slice_type % 5 != P_SLICE {
            return Ok(None);
        }
        w.ue(slice_type);
        let pps_id = r.ue()?;
        let pps = self.pps.get(&pps_id).ok_or(Unreadable)?.clone();
        if pps.slice_groups > 1 {
            return Ok(None);
        }
        let sps = self.sps.get(&pps.sps_id).ok_or(Unreadable)?.clone();
        if sps.separate_colour_plane != 0 {
            return Ok(None);
        }
        w.ue(pps_id);
        w.u(sps.log2_max_frame_num, r.u(sps.log2_max_frame_num)?);
        let mut field = 0;
        if sps.frame_mbs_only == 0 {
            field = r.u(1)?;
            w.u(1, field);
            if field == 1 {
                w.u(1, r.u(1)?);
            }
        }
        if sps.poc_type == 0 {
            w.u(sps.log2_max_poc_lsb, r.u(sps.log2_max_poc_lsb)?);
            if pps.bottom_field_poc == 1 && field == 0 {
                w.se(r.se()?);
            }
        } else if sps.poc_type == 1 && sps.delta_pic_order_always_zero == 0 {
            w.se(r.se()?);
            if pps.bottom_field_poc == 1 && field == 0 {
                w.se(r.se()?);
            }
        }
        if pps.redundant_pic_cnt == 1 {
            let redundant = r.ue()?;
            if redundant != 0 {
                return Ok(None);
            }
            w.ue(redundant);
        }
        let mut refs = pps.num_ref_idx_l0_default;
        let override_flag = r.u(1)?;
        w.u(1, override_flag);
        if override_flag == 1 {
            refs = r.ue()? + 1;
            w.ue(refs - 1);
        }
        let modified = r.u(1)?;
        w.u(1, modified);
        if modified == 1 {
            loop {
                let idc = r.ue()?;
                w.ue(idc);
                match idc {
                    0..=2 => w.ue(r.ue()?),
                    3 => break,
                    _ => return Ok(None),
                }
            }
        }
        if pps.weighted_pred == 1 {
            let chroma = sps.chroma_format_idc != 0;
            w.ue(r.ue()?);
            if chroma {
                w.ue(r.ue()?);
            }
            for _ in 0..refs {
                let flag = r.u(1)?;
                w.u(1, flag);
                if flag == 1 {
                    w.se(r.se()?);
                    w.se(r.se()?);
                }
                if chroma {
                    let flag = r.u(1)?;
                    w.u(1, flag);
                    if flag == 1 {
                        for _ in 0..4 {
                            w.se(r.se()?);
                        }
                    }
                }
            }
        }
        if nal[0] & 0x60 != 0 {
            let adaptive = r.u(1)?;
            w.u(1, adaptive);
            if adaptive == 1 {
                loop {
                    let op = r.ue()?;
                    w.ue(op);
                    if op == 0 {
                        break;
                    }
                    if op == 1 || op == 3 {
                        w.ue(r.ue()?);
                    }
                    if op == 2 {
                        w.ue(r.ue()?);
                    }
                    if op == 3 || op == 6 {
                        w.ue(r.ue()?);
                    }
                    if op == 4 {
                        w.ue(r.ue()?);
                    }
                }
            }
        }
        if pps.cabac == 1 {
            r.ue()?;
            w.ue(0);
        }
        r.se()?;
        w.se(SKIP_QP.saturating_sub(pps.pic_init_qp));
        if pps.deblocking_control == 1 {
            if r.ue()? != 1 {
                r.se()?;
                r.se()?;
            }
            w.ue(1);
        }
        let end = next_mb.unwrap_or(sps.mbs);
        if end <= first_mb {
            return Ok(None);
        }
        let mbs = end - first_mb;
        if pps.cabac == 1 {
            let align = (8 - w.length() % 8) % 8;
            w.raw(&vec![1u8; align]);
            let data = self.cabac.entry(mbs).or_insert_with(|| cabac_skips(mbs));
            w.raw(data);
        } else {
            w.ue(mbs);
            w.raw(&[1]);
        }
        Ok(Some(w.nal(nal[0])))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const WIDTH_MBS: u32 = 80;
    pub(crate) const HEIGHT_MBS: u32 = 45;

    pub(crate) fn sps(poc_type: u32) -> Vec<u8> {
        let mut w = Writer::default();
        w.u(8, 100);
        w.u(16, 32);
        w.ue(0);
        w.ue(1);
        w.ue(0);
        w.ue(0);
        w.u(1, 0);
        w.u(1, 0);
        w.ue(4);
        w.ue(poc_type);
        if poc_type == 0 {
            w.ue(4);
        }
        w.ue(8);
        w.u(1, 0);
        w.ue(WIDTH_MBS - 1);
        w.ue(HEIGHT_MBS - 1);
        w.u(1, 1);
        w.u(1, 1);
        w.u(1, 0);
        w.u(1, 0);
        w.raw(&[1]);
        w.nal(0x67)
    }

    pub(crate) fn pps(cabac: u32, weighted: u32) -> Vec<u8> {
        let mut w = Writer::default();
        w.ue(0);
        w.ue(0);
        w.u(1, cabac);
        w.u(1, 0);
        w.ue(0);
        w.ue(0);
        w.ue(0);
        w.u(1, weighted);
        w.u(2, 0);
        w.se(-3);
        w.se(0);
        w.se(0);
        w.u(1, 1);
        w.u(1, 0);
        w.u(1, 0);
        w.raw(&[1]);
        w.nal(0x68)
    }

    /// A P slice whose header carries an override of two references, a list modification,
    /// long-term marking, a non-zero cabac_init_idc, quantizer, and deblocking offsets, and
    /// whose data is arbitrary bytes.
    pub(crate) fn p_slice(
        first_mb: u32,
        poc_type: u32,
        cabac: u32,
        weighted: u32,
        slice_type: u32,
    ) -> Vec<u8> {
        let mut w = Writer::default();
        w.ue(first_mb);
        w.ue(slice_type);
        w.ue(0);
        w.u(8, 7);
        if poc_type == 0 {
            w.u(8, 14);
        }
        w.u(1, 1);
        w.ue(1);
        w.u(1, 1);
        w.ue(0);
        w.ue(4);
        w.ue(2);
        w.ue(1);
        w.ue(3);
        if weighted == 1 {
            w.ue(5);
            w.ue(4);
            for (luma, chroma) in [(1, 0), (0, 1)] {
                w.u(1, luma);
                if luma == 1 {
                    w.se(3);
                    w.se(-2);
                }
                w.u(1, chroma);
                if chroma == 1 {
                    for v in [1, -1, 2, -2] {
                        w.se(v);
                    }
                }
            }
        }
        w.u(1, 1);
        w.ue(6);
        w.ue(1);
        w.ue(0);
        if cabac == 1 {
            w.ue(2);
        }
        w.se(5);
        w.ue(0);
        w.se(1);
        w.se(-1);
        if cabac == 1 {
            let align = (8 - w.length() % 8) % 8;
            w.raw(&vec![1u8; align]);
        }
        let pattern: Vec<u8> = "1011001110001111".bytes().map(|b| b - b'0').collect();
        for _ in 0..8 {
            w.raw(&pattern);
        }
        w.nal(0x61)
    }

    #[derive(Debug, Default, PartialEq)]
    struct Header {
        first_mb: u32,
        slice_type: u32,
        pps: u32,
        frame_num: u32,
        poc_lsb: Option<u32>,
        override_flag: u32,
        refs: u32,
        mods: Vec<(u32, u32)>,
        denoms: Option<(u32, u32)>,
        weights: Vec<Vec<i32>>,
        mmco: Vec<(u32, u32)>,
        cabac_init_idc: Option<u32>,
        qp: i32,
        deblocking: u32,
        offsets: Option<(i32, i32)>,
    }

    /// A slice header read back, and the reader left where its data begins.
    fn header(nal: &[u8], poc_type: u32, cabac: u32, weighted: u32) -> (Header, Reader) {
        let mut r = Reader::new(nal);
        let mut f = Header {
            first_mb: r.ue().unwrap(),
            slice_type: r.ue().unwrap(),
            pps: r.ue().unwrap(),
            frame_num: r.u(8).unwrap(),
            ..Default::default()
        };
        if poc_type == 0 {
            f.poc_lsb = Some(r.u(8).unwrap());
        }
        f.override_flag = r.u(1).unwrap();
        f.refs = if f.override_flag == 1 {
            r.ue().unwrap() + 1
        } else {
            1
        };
        if r.u(1).unwrap() == 1 {
            loop {
                let idc = r.ue().unwrap();
                if idc == 3 {
                    break;
                }
                f.mods.push((idc, r.ue().unwrap()));
            }
        }
        if weighted == 1 {
            f.denoms = Some((r.ue().unwrap(), r.ue().unwrap()));
            for _ in 0..f.refs {
                let luma = r.u(1).unwrap();
                f.weights.push(if luma == 1 {
                    vec![r.se().unwrap(), r.se().unwrap()]
                } else {
                    vec![]
                });
                let chroma = r.u(1).unwrap();
                f.weights.push(if chroma == 1 {
                    (0..4).map(|_| r.se().unwrap()).collect()
                } else {
                    vec![]
                });
            }
        }
        if r.u(1).unwrap() == 1 {
            loop {
                let op = r.ue().unwrap();
                if op == 0 {
                    break;
                }
                f.mmco.push((op, r.ue().unwrap()));
            }
        }
        if cabac == 1 {
            f.cabac_init_idc = Some(r.ue().unwrap());
        }
        f.qp = 23 + r.se().unwrap();
        f.deblocking = r.ue().unwrap();
        if f.deblocking != 1 {
            f.offsets = Some((r.se().unwrap(), r.se().unwrap()));
        }
        (f, r)
    }

    /// `(skips, ends)` and `(bits left, their value)`.
    type Decoded = ((u32, u32), (usize, u64));

    /// Decode `mbs` macroblocks of a P slice as CABAC does (cabac_init_idc 0 at SKIP_QP):
    /// each one's mb_skip_flag and end_of_slice_flag, then what follows the terminating bin.
    fn cabac_decode(r: &mut Reader, mbs: u32) -> Result<Decoded, String> {
        while !r.pos.is_multiple_of(8) {
            if r.u(1).unwrap() != 1 {
                return Err("alignment bit not 1".into());
            }
        }
        let (m, n) = SKIP_CONTEXT;
        let pre = (((m * SKIP_QP) >> 4) + n).clamp(1, 126);
        let (mut state, mut mps): (usize, u32) = if pre <= 63 {
            ((63 - pre) as usize, 0)
        } else {
            ((pre - 64) as usize, 1)
        };
        let mut rng: u32 = 510;
        let mut offset = r.u(9).unwrap();
        let (mut skips, mut ends) = (0, 0);
        for i in 0..mbs {
            let lps = RANGE_LPS[state][((rng >> 6) & 3) as usize];
            rng -= lps;
            let binval;
            if offset >= rng {
                binval = 1 - mps;
                offset -= rng;
                rng = lps;
                if state == 0 {
                    mps = 1 - mps;
                }
                state = TRANS_LPS[state];
            } else {
                binval = mps;
                state = (state + 1).min(62);
            }
            while rng < 256 {
                rng <<= 1;
                offset = (offset << 1) | r.u(1).unwrap();
            }
            skips += binval;
            rng -= 2;
            if offset >= rng {
                ends += 1;
                if i != mbs - 1 {
                    return Err(format!("slice ended at macroblock {i}"));
                }
                break;
            }
            while rng < 256 {
                rng <<= 1;
                offset = (offset << 1) | r.u(1).unwrap();
            }
        }
        let rest = r.size() - r.pos;
        let tail = if rest > 0 { r.u64(rest).unwrap() } else { 0 };
        Ok(((skips, ends), (rest, tail)))
    }

    #[test]
    fn stand_ins_keep_the_reference_structure_and_skip_every_macroblock() {
        for (label, poc_type, cabac, weighted) in [
            ("CABAC, picture order type 2", 2, 1, 0),
            ("CABAC, type 0, weighted prediction", 0, 1, 1),
            ("CAVLC, picture order type 2", 2, 0, 0),
        ] {
            let mut skips = SkipPictures::new();
            skips.learn([sps(poc_type).as_slice(), pps(cabac, weighted).as_slice()]);
            let original = p_slice(0, poc_type, cabac, weighted, 5);
            let out = skips
                .stand_in([original.as_slice()])
                .unwrap_or_else(|| panic!("{label}"));
            assert_eq!(out.len(), 1, "{label}");
            let (before, _) = header(&original, poc_type, cabac, weighted);
            let (after, mut r) = header(&out[0], poc_type, cabac, weighted);
            assert_eq!(out[0][0], original[0]);
            assert_eq!(
                (
                    before.first_mb,
                    before.slice_type,
                    before.pps,
                    before.frame_num,
                    before.poc_lsb,
                    before.override_flag,
                    before.refs,
                    &before.mods,
                    &before.weights,
                    &before.mmco
                ),
                (
                    after.first_mb,
                    after.slice_type,
                    after.pps,
                    after.frame_num,
                    after.poc_lsb,
                    after.override_flag,
                    after.refs,
                    &after.mods,
                    &after.weights,
                    &after.mmco
                ),
                "{label}"
            );
            assert_eq!(
                (
                    after.qp,
                    after.deblocking,
                    after.cabac_init_idc.unwrap_or(0)
                ),
                (SKIP_QP, 1, 0),
                "{label}"
            );
            let mbs = WIDTH_MBS * HEIGHT_MBS;
            if cabac == 1 {
                let (counts, tail) =
                    cabac_decode(&mut r, mbs).unwrap_or_else(|e| panic!("{label}: {e}"));
                assert_eq!(counts, (mbs, 1), "{label}");
                assert!(tail.0 < 8 && tail.1 == 0, "{label}: {tail:?}");
            } else {
                let run = r.ue().unwrap();
                let stop = r.u(1).unwrap();
                let rest = r.size() - r.pos;
                assert_eq!((run, stop), (mbs, 1), "{label}");
                assert!(rest < 8 && r.u(rest).unwrap() == 0, "{label}");
            }
        }
    }

    #[test]
    fn two_slices_cover_their_own_macroblocks_and_the_refusals() {
        let mut skips = SkipPictures::new();
        skips.learn([sps(2).as_slice(), pps(1, 0).as_slice()]);
        let half = WIDTH_MBS * HEIGHT_MBS / 2;
        let a = p_slice(0, 2, 1, 0, 5);
        let b = p_slice(half, 2, 1, 0, 5);
        let two = skips.stand_in([a.as_slice(), b.as_slice()]).unwrap();
        let mut covered = Vec::new();
        for nal in &two {
            let (f, mut r) = header(nal, 2, 1, 0);
            let mbs = if f.first_mb > 0 {
                WIDTH_MBS * HEIGHT_MBS - f.first_mb
            } else {
                half
            };
            let (counts, _) = cabac_decode(&mut r, mbs).unwrap();
            covered.push((f.first_mb, counts));
        }
        assert_eq!(covered, vec![(0, (half, 1)), (half, (half, 1))]);
        assert!(
            skips
                .stand_in([p_slice(0, 2, 1, 0, 7).as_slice()])
                .is_none(),
            "an I slice"
        );
        let mut idr = vec![0x65u8];
        idr.extend_from_slice(&p_slice(0, 2, 1, 0, 5)[1..]);
        assert!(
            skips
                .stand_in([sps(2).as_slice(), pps(1, 0).as_slice(), idr.as_slice()])
                .is_none(),
            "an IDR"
        );
        assert!(
            SkipPictures::new()
                .stand_in([p_slice(0, 2, 1, 0, 5).as_slice()])
                .is_none(),
            "unknown parameter sets"
        );
    }
}
