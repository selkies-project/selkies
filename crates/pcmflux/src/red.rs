/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! RFC 2198 redundant audio as a client's microphone sends it: where each block of a payload
//! sits, before anything is decoded. Nothing here reaches outside the module, so a fuzz build
//! (`fuzz/`) compiles it on its own.

/// Max redundant copies per frame and RED history depth.
pub(crate) const RED_MAX_DISTANCE: i32 = 4;

/// The blocks of a payload, oldest first, as `(ts, start, len)`; `red_blocks` says how many.
pub(crate) type Blocks = [(i64, usize, usize); RED_MAX_DISTANCE as usize + 1];

/// The blocks of an RFC 2198 payload, oldest first, as `(ts, start, len)` within it: each
/// redundant block at `primary_ts` less its 14-bit offset (an empty one left out), then the
/// primary, which is whatever bytes remain. `None` for a payload whose headers or blocks run
/// past it, or that carries more than `RED_MAX_DISTANCE` redundant blocks.
pub(crate) fn red_blocks(payload: &[u8], primary_ts: i64) -> Option<(Blocks, usize)> {
    let n = payload.len();
    let mut i = 0usize;
    let mut offs = [0i64; RED_MAX_DISTANCE as usize];
    let mut lens = [0usize; RED_MAX_DISTANCE as usize];
    let mut nh = 0usize;
    while i < n && (payload[i] & 0x80) != 0 {
        if i + 4 > n || nh >= RED_MAX_DISTANCE as usize {
            return None;
        }
        let field = ((payload[i + 1] as u32) << 16)
            | ((payload[i + 2] as u32) << 8)
            | (payload[i + 3] as u32);
        offs[nh] = ((field >> 10) & 0x3FFF) as i64;
        lens[nh] = (field & 0x3FF) as usize;
        nh += 1;
        i += 4;
    }
    if i >= n {
        return None;
    }
    i += 1;

    let mut frames: Blocks = [(0, 0, 0); RED_MAX_DISTANCE as usize + 1];
    let mut nf = 0usize;
    for k in 0..nh {
        if i + lens[k] > n {
            return None;
        }
        if lens[k] > 0 {
            frames[nf] = (primary_ts - offs[k], i, lens[k]);
            nf += 1;
        }
        i += lens[k];
    }
    frames[nf] = (primary_ts, i, n - i);
    nf += 1;
    Some((frames, nf))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One redundant block header: the follow bit, payload type 111, a 14-bit offset and a
    /// 10-bit length.
    fn header(offset: u32, len: u32) -> [u8; 4] {
        let field = (offset << 10) | len;
        [
            0x80 | 111,
            (field >> 16) as u8,
            (field >> 8) as u8,
            field as u8,
        ]
    }

    #[test]
    fn blocks_come_oldest_first_and_the_primary_takes_the_rest() {
        let mut p = header(960, 3).to_vec();
        p.push(111);
        p.extend_from_slice(&[1, 2, 3, 9, 9]);
        let (blocks, n) = red_blocks(&p, 10_000).expect("well formed");
        assert_eq!(n, 2);
        assert_eq!(blocks[0], (10_000 - 960, 5, 3));
        assert_eq!(blocks[1], (10_000, 8, 2));
    }

    #[test]
    fn an_empty_redundant_block_is_left_out() {
        let mut p = header(960, 0).to_vec();
        p.push(111);
        p.extend_from_slice(&[7]);
        let (blocks, n) = red_blocks(&p, 5_000).expect("well formed");
        assert_eq!(n, 1);
        assert_eq!(blocks[0], (5_000, 5, 1));
    }

    #[test]
    fn a_payload_that_runs_out_is_refused() {
        assert_eq!(red_blocks(&[], 0), None, "empty");
        assert_eq!(
            red_blocks(&header(960, 3)[..3], 0),
            None,
            "a header cut short"
        );
        assert_eq!(red_blocks(&header(960, 3), 0), None, "no primary header");
        let mut p = header(960, 8).to_vec();
        p.push(111);
        p.extend_from_slice(&[1, 2]);
        assert_eq!(red_blocks(&p, 0), None, "a block past the end");
    }

    #[test]
    fn more_redundant_blocks_than_kept_are_refused() {
        let mut p = Vec::new();
        for _ in 0..=RED_MAX_DISTANCE {
            p.extend_from_slice(&header(960, 0));
        }
        p.push(111);
        p.push(1);
        assert_eq!(red_blocks(&p, 0), None);
        let mut ok = Vec::new();
        for _ in 0..RED_MAX_DISTANCE {
            ok.extend_from_slice(&header(960, 0));
        }
        ok.push(111);
        ok.push(1);
        assert!(red_blocks(&ok, 0).is_some(), "as many as kept are taken");
    }
}
