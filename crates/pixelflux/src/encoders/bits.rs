/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A most-significant-bit-first writer for the headers a VA-API session packs itself: the
//! fixed-width and Exp-Golomb fields of H.264, HEVC, and AV1 syntax, the RBSP trailing bits,
//! and the emulation prevention an H.26x NAL unit needs on its way out; and the split of an
//! Annex-B stream into its NAL units on the way in. Nothing here reaches outside the module,
//! so a fuzz build (`fuzz/`) compiles it on its own.

/// Bits appended most significant first.
#[derive(Default)]
pub struct BitWriter {
    bytes: Vec<u8>,
    bits: usize,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of bits written so far.
    pub fn len(&self) -> usize {
        self.bits
    }

    pub fn is_empty(&self) -> bool {
        self.bits == 0
    }

    /// Whether the writer stands on a byte boundary.
    pub fn aligned(&self) -> bool {
        self.bits.is_multiple_of(8)
    }

    /// The low `count` bits of `value`, most significant first.
    pub fn u(&mut self, count: u32, value: u64) {
        for shift in (0..count).rev() {
            if self.bits.is_multiple_of(8) {
                self.bytes.push(0);
            }
            if (value >> shift) & 1 != 0 {
                let index = self.bits >> 3;
                self.bytes[index] |= 1 << (7 - (self.bits & 7));
            }
            self.bits += 1;
        }
    }

    /// One flag bit.
    pub fn flag(&mut self, value: bool) {
        self.u(1, value as u64);
    }

    /// An unsigned Exp-Golomb code.
    pub fn ue(&mut self, value: u32) {
        let v = value as u64 + 1;
        let zeros = 63 - v.leading_zeros();
        self.u(zeros, 0);
        self.u(zeros + 1, v);
    }

    /// A signed Exp-Golomb code.
    pub fn se(&mut self, value: i32) {
        let mapped = if value > 0 {
            2 * value as u32 - 1
        } else {
            (-2 * value) as u32
        };
        self.ue(mapped);
    }

    /// The RBSP stop bit and the zero bits that align what follows.
    pub fn trailing_bits(&mut self) {
        self.flag(true);
        self.align();
    }

    /// Zero bits up to the next byte boundary.
    pub fn align(&mut self) {
        while !self.bits.is_multiple_of(8) {
            self.flag(false);
        }
    }

    /// Whole bytes appended at a byte boundary.
    pub fn bytes(&mut self, bytes: &[u8]) {
        debug_assert!(self.aligned());
        self.bytes.extend_from_slice(bytes);
        self.bits += 8 * bytes.len();
    }

    /// The bytes written; the last one is padded with zero bits.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// An RBSP as a NAL unit payload: an emulation prevention byte ahead of every byte of three
/// or less that follows two zeros.
pub fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + rbsp.len() / 64 + 4);
    let mut zeros = 0;
    for &byte in rbsp {
        if zeros >= 2 && byte <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// One Annex-B NAL unit: a four-byte start code, the header bytes, and the escaped payload.
pub fn nal_unit(header: &[u8], rbsp: &[u8]) -> Vec<u8> {
    let mut out = vec![0, 0, 0, 1];
    out.extend_from_slice(header);
    out.extend(escape(rbsp));
    out
}

/// The NAL units of an Annex-B stream, each without its start code and never empty.
pub(crate) fn annexb_nals(au: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0usize;
    while i + 2 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::with_capacity(starts.len());
    for (n, &start) in starts.iter().enumerate() {
        let mut end = starts.get(n + 1).map_or(au.len(), |&next| next - 3);
        while end > start && au[end - 1] == 0 {
            end -= 1;
        }
        if end > start {
            nals.push(&au[start..end]);
        }
    }
    nals.into_iter()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb_and_fixed_fields_pack_as_the_standards_read_them() {
        let mut w = BitWriter::new();
        w.ue(0);
        w.ue(1);
        w.ue(2);
        w.ue(3);
        w.se(1);
        w.se(-1);
        w.u(3, 0b101);
        w.trailing_bits();
        assert_eq!(w.as_bytes(), &[0b1010_0110, 0b0100_0100, 0b1110_1100]);
        assert_eq!(w.len(), 24);
    }

    #[test]
    fn emulation_prevention_guards_every_start_code() {
        assert_eq!(
            escape(&[0, 0, 1, 0, 0, 0, 5, 0, 0, 3]),
            vec![0, 0, 3, 1, 0, 0, 3, 0, 5, 0, 0, 3, 3]
        );
        assert_eq!(
            nal_unit(&[0x67], &[0, 0, 2]),
            vec![0, 0, 0, 1, 0x67, 0, 0, 3, 2]
        );
    }
}
