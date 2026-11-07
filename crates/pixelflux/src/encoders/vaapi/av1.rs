/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The AV1 arm: the sequence and picture parameters, the packed sequence header and frame
//! header OBUs, and the tile group behind them. Main profile, one tile group of as many tiles
//! as the picture width needs, and the eight buffer slots addressed by timestamp: every frame
//! refreshes its own slot and names the slot of the newest frame the client still has as all
//! seven of its references, which is also where its probability contexts load from. The
//! frame header is written with the bit positions of the quantizer, loop filter, and CDEF
//! fields, which a constant-rate driver rewrites in place and a constant-quantizer session
//! fills itself, and with OBU sizes of the fixed width it asks for.

use va_sys::*;

use super::super::bits::BitWriter;
use super::super::codec::av1_level;
use super::super::reference::REFERENCE_FRAMES;
use super::{Buffers, Frame, Negotiated};

const OBU_SEQUENCE_HEADER: u64 = 1;
const OBU_FRAME_HEADER: u64 = 3;
const ORDER_HINT_BITS: u32 = 8;
const MAX_TILE_WIDTH: u32 = 4096;
const MAX_TILE_AREA: u32 = 4096 * 2304;
const MAX_TILE_COLS: u32 = 64;
const MAX_TILE_ROWS: u32 = 64;
/// The quantizer of a constant-rate session's headers, which the driver rewrites.
const RATE_CONTROLLED_QINDEX: u32 = 128;

/// The tile layout of a picture: uniform tiles of `cols` x `rows`, in superblocks.
#[derive(Clone, Copy, Default)]
struct Tiles {
    sb_cols: u32,
    sb_rows: u32,
    cols: u32,
    rows: u32,
    cols_log2: u32,
    rows_log2: u32,
    min_log2_cols: u32,
    max_log2_cols: u32,
    min_log2_rows: u32,
    max_log2_rows: u32,
}

pub(super) struct Arm {
    ext2: VAConfigAttribValEncAV1Ext2,
    tx_mode_select: bool,
    obu_size_bytes: u32,
    level_idx: u32,
    tiles: Tiles,
    sequence: VAEncSequenceParameterBufferAV1,
    sequence_header: Vec<u8>,
    /// The bit `base_q_idx` stands at in the last frame header OBU written.
    qindex_offset: u32,
}

/// The in-loop filters of a frame: the deblocking level of every plane and edge, and the one
/// CDEF strength (primary in the high four bits, secondary in the low two) with its damping.
/// A constant-rate driver chooses its own and rewrites the header; a constant-quantizer
/// session has to name them, and all zero leaves the picture unfiltered.
#[derive(Clone, Copy, Default)]
struct Filters {
    level: u8,
    cdef_strength: u8,
    cdef_damping_minus_3: u8,
}

impl Filters {
    /// The filters of a constant-quantizer frame at `qindex`, measured on Intel Arc on a
    /// text desktop and a gradient wallpaper: deblocking rises with the quantizer, CDEF holds
    /// primary 8 and secondary 2, and its damping widens as the steps coarsen.
    fn at(qindex: u32) -> Self {
        Self {
            level: (qindex / 8).min(63) as u8,
            cdef_strength: (8 << 2) | 2,
            cdef_damping_minus_3: (qindex >> 6).min(3) as u8,
        }
    }
}

/// How many bits `value` needs, at least one.
fn bit_width(value: u32) -> u32 {
    (32 - value.leading_zeros()).max(1)
}

/// The smallest k with `block << k >= target`.
fn tile_log2(block: u32, target: u32) -> u32 {
    let mut k = 0;
    while (block << k) < target {
        k += 1;
    }
    k
}

/// An OBU: the header byte, the size in `size_bytes` of LEB128, the payload, and its
/// trailing bits.
fn obu(kind: u64, payload: &BitWriter, size_bytes: u32) -> Vec<u8> {
    let mut body = payload.as_bytes().to_vec();
    if payload.aligned() {
        body.push(0x80);
    } else {
        let last = body.len() - 1;
        body[last] |= 0x80 >> (payload.len() % 8);
    }
    let mut w = BitWriter::new();
    w.u(1, 0);
    w.u(4, kind);
    w.u(1, 0);
    w.u(1, 1);
    w.u(1, 0);
    let mut size = body.len() as u64;
    for i in 0..size_bytes {
        let byte = size & 0x7f;
        size >>= 7;
        w.u(8, byte | if i + 1 < size_bytes { 0x80 } else { 0 });
    }
    w.bytes(&body);
    w.into_bytes()
}

impl Arm {
    pub(super) fn new(_attr: Option<u32>, _ext1: Option<u32>, ext2: u32) -> Self {
        let ext2 = VAConfigAttribValEncAV1Ext2 { value: ext2 };
        let tx_mode_support = unsafe { ext2.bits.tx_mode_support() };
        Self {
            ext2,
            tx_mode_select: tx_mode_support & 0x04 != 0,
            obu_size_bytes: unsafe { ext2.bits.obu_size_bytes_minus1() } + 1,
            level_idx: 0,
            tiles: Tiles::default(),
            sequence: unsafe { std::mem::zeroed() },
            sequence_header: Vec::new(),
            qindex_offset: 0,
        }
    }

    /// Surfaces align to the 64x64 superblock.
    pub(super) fn alignment(&self) -> (u32, u32) {
        (64, 64)
    }

    /// The uniform tiling of a `width` x `height` picture: as few tile columns as the
    /// codec's widest tile allows and one tile row, or more rows where the area limit asks.
    fn tiles(width: u32, height: u32) -> Result<Tiles, String> {
        let mi_cols = 2 * ((width + 7) >> 3);
        let mi_rows = 2 * ((height + 7) >> 3);
        let sb_cols = (mi_cols + 15) >> 4;
        let sb_rows = (mi_rows + 15) >> 4;
        let sb_size = 6;
        let max_tile_width_sb = MAX_TILE_WIDTH >> sb_size;
        let max_tile_area_sb = MAX_TILE_AREA >> (2 * sb_size);
        let min_log2_cols = tile_log2(max_tile_width_sb, sb_cols);
        let max_log2_cols = tile_log2(1, sb_cols.min(MAX_TILE_COLS));
        let max_log2_rows = tile_log2(1, sb_rows.min(MAX_TILE_ROWS));
        let min_log2_tiles = min_log2_cols.max(tile_log2(max_tile_area_sb, sb_rows * sb_cols));
        let cols = sb_cols.div_ceil(max_tile_width_sb).max(1);
        let cols_log2 = tile_log2(1, cols);
        let tile_width_sb = (sb_cols + (1 << cols_log2) - 1) >> cols_log2;
        for rows in 1..=sb_rows.min(MAX_TILE_ROWS) {
            let rows_log2 = tile_log2(1, rows);
            let tile_height_sb = (sb_rows + (1 << rows_log2) - 1) >> rows_log2;
            if sb_cols.div_ceil(tile_width_sb) == cols
                && sb_rows.div_ceil(tile_height_sb) == rows
                && tile_height_sb <= max_tile_area_sb / tile_width_sb
            {
                return Ok(Tiles {
                    sb_cols,
                    sb_rows,
                    cols,
                    rows,
                    cols_log2,
                    rows_log2,
                    min_log2_cols,
                    max_log2_cols,
                    min_log2_rows: min_log2_tiles.saturating_sub(cols_log2),
                    max_log2_rows,
                });
            }
        }
        Err(format!("no uniform AV1 tiling of {width}x{height}"))
    }

    /// The stream's sequence: its level from the ladder, its tiling, and the sequence header
    /// OBU the key frames carry.
    pub(super) fn configure(
        &mut self,
        n: &Negotiated,
        surface_width: u32,
        surface_height: u32,
    ) -> Result<(), String> {
        self.tiles = Self::tiles(n.width, n.height)?;
        let tiles = self.tiles.cols * self.tiles.rows;
        let max_tiles = unsafe { self.ext2.bits.max_tile_num_minus1() } + 1;
        if max_tiles > 1 && tiles > max_tiles {
            return Err(format!(
                "this VA-API driver encodes at most {max_tiles} AV1 tiles, and {}x{} needs {tiles}",
                n.width, n.height
            ));
        }
        let _ = (surface_width, surface_height);
        self.level_idx = av1_level(n.width, n.height, n.fps.ceil(), n.bits_per_second as u64);
        let cbr = n.rc_mode != VA_RC_CQP;

        let mut w = BitWriter::new();
        w.u(3, 0);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.u(5, 0);
        w.u(12, 0);
        w.u(5, self.level_idx as u64);
        if self.level_idx > 7 {
            w.flag(false);
        }
        let (wbits, hbits) = (bit_width(n.width - 1), bit_width(n.height - 1));
        w.u(4, (wbits - 1) as u64);
        w.u(4, (hbits - 1) as u64);
        w.u(wbits, (n.width - 1) as u64);
        w.u(hbits, (n.height - 1) as u64);
        w.flag(false);
        w.flag(false);
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
        w.u(1, 0);
        w.u(3, (ORDER_HINT_BITS - 1) as u64);
        w.flag(false);
        w.flag(true);
        w.flag(false);
        w.flag(n.bit_depth == 10);
        w.flag(false);
        w.flag(true);
        w.u(8, 1);
        w.u(8, 1);
        w.u(8, 1);
        w.flag(false);
        w.u(2, 0);
        w.flag(false);
        w.flag(false);
        self.sequence_header = obu(OBU_SEQUENCE_HEADER, &w, self.obu_size_bytes);

        let s = &mut self.sequence;
        *s = unsafe { std::mem::zeroed() };
        s.seq_profile = 0;
        s.seq_level_idx = self.level_idx as u8;
        s.seq_tier = 0;
        s.intra_period = i32::MAX as u32;
        s.ip_period = 1;
        s.order_hint_bits_minus_1 = (ORDER_HINT_BITS - 1) as u8;
        unsafe {
            let f = &mut s.seq_fields.bits;
            f.set_enable_order_hint(1);
            f.set_bit_depth_minus8(n.bit_depth - 8);
            f.set_enable_cdef(1);
        }
        if cbr {
            s.bits_per_second = n.bits_per_second;
        }
        Ok(())
    }

    pub(super) fn sequence(&mut self, n: &Negotiated, out: &mut Buffers) {
        out.push(VAEncSequenceParameterBufferType, &self.sequence);
        if n.packed & VA_ENC_PACKED_HEADER_SEQUENCE != 0 {
            let bits = 8 * self.sequence_header.len() as u32;
            out.push_packed(VAEncPackedHeaderSequence, &self.sequence_header, bits);
        }
    }

    /// The `base_q_idx` the driver's rate control wrote into the frame header of the coded
    /// temporal unit `tu`, at the offset the session named for it; None where the unit carries
    /// no frame header laid out as the session wrote it.
    pub(super) fn coded_qindex(&self, tu: &[u8]) -> Option<u32> {
        let mut pos = 0usize;
        while pos < tu.len() {
            let header = tu[pos];
            if header & 0x06 != 0x02 {
                return None;
            }
            let (mut size, mut shift, mut i) = (0usize, 0u32, pos + 1);
            loop {
                let byte = *tu.get(i)?;
                i += 1;
                size |= ((byte & 0x7f) as usize) << shift;
                shift += 7;
                if byte & 0x80 == 0 {
                    break;
                }
                if shift > 28 {
                    return None;
                }
            }
            if matches!((header >> 3) & 0x0f, 3 | 6) {
                if self.qindex_offset == 0 || i - pos != 1 + self.obu_size_bytes as usize {
                    return None;
                }
                let bit = pos * 8 + self.qindex_offset as usize;
                let (byte, lead) = (bit / 8, bit % 8);
                let pair = ((*tu.get(byte)? as u32) << 8) | *tu.get(byte + 1)? as u32;
                return Some((pair >> (8 - lead)) & 0xff);
            }
            pos = i + size;
        }
        None
    }

    /// The picture: its slot, references, order hint, the frame header OBU with the
    /// offsets a driver patches, and the one tile group.
    pub(super) fn picture(
        &mut self,
        n: &Negotiated,
        frame: &Frame,
        out: &mut Buffers,
    ) -> Result<(), String> {
        let slot_of = |pts: u64| (pts % REFERENCE_FRAMES as u64) as u8;
        let cbr = n.rc_mode != VA_RC_CQP;
        let qindex = if cbr {
            RATE_CONTROLLED_QINDEX
        } else {
            frame.qp.clamp(1, 255)
        };
        let order_hint = ((frame.pts - frame.key_pts) & ((1 << ORDER_HINT_BITS) - 1)) as u32;
        let slot = slot_of(frame.pts);
        let reference_slot = match (frame.key, frame.reference) {
            (true, _) => None,
            (false, Some((pts, _))) => Some(slot_of(pts)),
            (false, None) => return Err("a predicted AV1 picture without a reference".into()),
        };
        let t = self.tiles;

        let render_size = |w: &mut BitWriter| {
            w.flag(true);
            w.u(16, (n.width - 1) as u64);
            w.u(16, (n.height - 1) as u64);
        };
        let mut w = BitWriter::new();
        w.flag(false);
        w.u(2, if frame.key { 0 } else { 1 });
        w.flag(true);
        if !frame.key {
            w.flag(false);
        }
        w.flag(false);
        w.flag(false);
        w.u(ORDER_HINT_BITS, order_hint as u64);
        if !frame.key {
            w.u(3, 0);
            w.u(8, 1 << slot);
            w.flag(false);
            for _ in 0..7 {
                w.u(3, reference_slot.unwrap() as u64);
            }
            render_size(&mut w);
            w.flag(false);
            w.flag(false);
            w.u(2, 0);
            w.flag(false);
        } else {
            render_size(&mut w);
        }
        w.flag(false);
        w.flag(true);
        for log2 in t.min_log2_cols..t.max_log2_cols {
            let more = log2 < t.cols_log2;
            w.flag(more);
            if !more {
                break;
            }
        }
        for log2 in t.min_log2_rows..t.max_log2_rows {
            let more = log2 < t.rows_log2;
            w.flag(more);
            if !more {
                break;
            }
        }
        if t.cols_log2 > 0 || t.rows_log2 > 0 {
            w.u(t.cols_log2 + t.rows_log2, 0);
            w.u(2, unsafe { self.ext2.bits.tile_size_bytes_minus1() } as u64);
        }
        let header_prefix_bits = 8 * (1 + self.obu_size_bytes);
        let qindex_offset = header_prefix_bits + w.len() as u32;
        w.u(8, qindex as u64);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        w.flag(false);
        let filters = if cbr {
            Filters::default()
        } else {
            Filters::at(qindex)
        };
        let loopfilter_offset = header_prefix_bits + w.len() as u32;
        w.u(6, filters.level as u64);
        w.u(6, filters.level as u64);
        if filters.level > 0 {
            w.u(6, filters.level as u64);
            w.u(6, filters.level as u64);
        }
        w.u(3, 0);
        w.flag(false);
        let cdef_offset = header_prefix_bits + w.len() as u32;
        w.u(2, filters.cdef_damping_minus_3 as u64);
        w.u(2, 0);
        w.u(6, filters.cdef_strength as u64);
        w.u(6, filters.cdef_strength as u64);
        let cdef_size = header_prefix_bits + w.len() as u32 - cdef_offset;
        w.flag(self.tx_mode_select);
        if !frame.key {
            w.flag(false);
            for _ in 0..7 {
                w.flag(false);
            }
        }
        w.flag(false);
        let frame_header = obu(OBU_FRAME_HEADER, &w, self.obu_size_bytes);
        let frame_header_bits = 8 * frame_header.len() as u32;

        let mut pic: VAEncPictureParameterBufferAV1 = unsafe { std::mem::zeroed() };
        pic.frame_width_minus_1 = (n.width - 1) as u16;
        pic.frame_height_minus_1 = (n.height - 1) as u16;
        pic.reconstructed_frame = frame.recon;
        pic.coded_buf = frame.coded;
        pic.reference_frames = [VA_INVALID_SURFACE; 8];
        for &(pts, surface, _) in frame.held {
            if !frame.key && pts >= frame.key_pts {
                pic.reference_frames[slot_of(pts) as usize] = surface;
            }
        }
        pic.ref_frame_idx = [reference_slot.unwrap_or(0); 7];
        pic.primary_ref_frame = if frame.key { 7 } else { 0 };
        pic.order_hint = order_hint as u8;
        pic.refresh_frame_flags = if frame.key { 0xff } else { 1 << slot };
        unsafe {
            pic.ref_frame_ctrl_l0
                .fields
                .set_search_idx0(if frame.key { 0 } else { 1 });
            let f = &mut pic.picture_flags.bits;
            f.set_frame_type(if frame.key { 0 } else { 1 });
            f.set_error_resilient_mode(frame.key as u32);
            f.set_enable_frame_obu(0);
            let m = &mut pic.mode_control_flags.bits;
            m.set_reference_mode(0);
            m.set_tx_mode(if self.tx_mode_select { 2 } else { 1 });
            pic.tile_group_obu_hdr_info.bits.set_obu_has_size_field(1);
        }
        pic.base_qindex = qindex as u8;
        pic.filter_level = [filters.level; 2];
        pic.filter_level_u = filters.level;
        pic.filter_level_v = filters.level;
        pic.cdef_damping_minus_3 = filters.cdef_damping_minus_3;
        pic.cdef_y_strengths[0] = filters.cdef_strength;
        pic.cdef_uv_strengths[0] = filters.cdef_strength;
        pic.tile_cols = t.cols as u8;
        pic.tile_rows = t.rows as u8;
        let tile_width_sb = (t.sb_cols + (1 << t.cols_log2) - 1) >> t.cols_log2;
        let tile_height_sb = (t.sb_rows + (1 << t.rows_log2) - 1) >> t.rows_log2;
        for i in 0..t.cols as usize {
            pic.width_in_sbs_minus_1[i] = (if i + 1 == t.cols as usize {
                t.sb_cols - (t.cols - 1) * tile_width_sb
            } else {
                tile_width_sb
            } - 1) as u16;
        }
        for i in 0..t.rows as usize {
            pic.height_in_sbs_minus_1[i] = (if i + 1 == t.rows as usize {
                t.sb_rows - (t.rows - 1) * tile_height_sb
            } else {
                tile_height_sb
            } - 1) as u16;
        }
        pic.num_tile_groups_minus1 = 0;
        if cbr {
            pic.min_base_qindex = n.min_qp.clamp(1, 255) as u8;
            pic.max_base_qindex = if n.max_qp > 0 {
                n.max_qp.clamp(1, 255)
            } else {
                255
            } as u8;
            pic.bit_offset_qindex = qindex_offset;
            self.qindex_offset = qindex_offset;
            pic.bit_offset_loopfilter_params = loopfilter_offset;
            pic.bit_offset_cdef_params = cdef_offset;
            pic.size_in_bits_cdef_params = cdef_size;
            pic.size_in_bits_frame_hdr_obu = frame_header_bits;
            pic.byte_offset_frame_hdr_obu_size = if frame.key {
                self.sequence_header.len() as u32
            } else {
                0
            } + 1;
        }
        out.push(VAEncPictureParameterBufferType, &pic);
        if n.packed & VA_ENC_PACKED_HEADER_PICTURE != 0 {
            out.push_packed(VAEncPackedHeaderPicture, &frame_header, frame_header_bits);
        }
        let mut group: VAEncTileGroupBufferAV1 = unsafe { std::mem::zeroed() };
        group.tg_start = 0;
        group.tg_end = (t.cols * t.rows - 1) as u8;
        out.push(VAEncSliceParameterBufferType, &group);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A picture up to the widest tile is one tile; a wider one takes the fewest columns the
    /// codec allows, and every layout the checks compute is uniform.
    #[test]
    fn tiling_is_uniform_and_minimal() {
        let hd = Arm::tiles(1920, 1080).unwrap();
        assert_eq!((hd.cols, hd.rows, hd.cols_log2, hd.rows_log2), (1, 1, 0, 0));
        assert_eq!((hd.sb_cols, hd.sb_rows), (30, 17));
        let uhd = Arm::tiles(3840, 2160).unwrap();
        assert_eq!((uhd.cols, uhd.rows), (1, 1));
        let wide = Arm::tiles(5120, 1440).unwrap();
        assert_eq!((wide.cols, wide.cols_log2, wide.min_log2_cols), (2, 1, 1));
        assert_eq!(
            Arm::tiles(64, 64).unwrap().max_log2_cols,
            0,
            "a single superblock offers no tile columns to choose"
        );
    }

    /// An OBU's size field takes the width the driver asked for, and the payload ends in
    /// its trailing bits whether or not it stood on a byte boundary.
    #[test]
    fn obus_carry_fixed_width_sizes_and_trailing_bits() {
        let mut w = BitWriter::new();
        w.u(3, 5);
        let one = obu(OBU_SEQUENCE_HEADER, &w, 1);
        assert_eq!(one, [0x0a, 0x01, 0b1011_0000]);
        let four = obu(OBU_SEQUENCE_HEADER, &w, 4);
        assert_eq!(four, [0x0a, 0x81, 0x80, 0x80, 0x00, 0b1011_0000]);
        let mut w = BitWriter::new();
        w.u(8, 0xff);
        assert_eq!(
            obu(OBU_FRAME_HEADER, &w, 1),
            [0x1a, 0x02, 0xff, 0x80],
            "an aligned payload takes a trailing byte"
        );
    }
}
