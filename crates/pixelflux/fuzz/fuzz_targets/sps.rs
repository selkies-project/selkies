/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! An access unit as a hardware encoder may emit one, through the reads and rewrites a session
//! makes of its sequence parameter sets (`src/encoders/sps.rs`): a color signal written into a
//! set reads back as written, and nothing panics or reads past the input. The input's first
//! byte picks the signal and the reorder proof; the rest is the access unit.
#![no_main]

use libfuzzer_sys::fuzz_target;

#[allow(dead_code)]
#[path = "../../src/encoders"]
mod encoders {
    pub mod bits;
    pub mod codec {
        pub(crate) use super::bits::annexb_nals;
    }
    pub mod sps;
}

use encoders::sps::{self, ColorSignal};

fuzz_target!(|data: &[u8]| {
    let Some((&pick, unit)) = data.split_first() else {
        return;
    };
    let signal = ColorSignal {
        full_range: pick & 1 != 0,
        primaries: pick >> 1,
        transfer: pick.rotate_left(3),
        matrix: pick.rotate_left(5),
    };
    for (start, end) in sps::sequence_parameter_sets(unit) {
        let nal = &unit[start..end];
        let _ = sps::read_color(nal);
        if let Ok(written) = sps::write_color(nal, signal) {
            assert_eq!(
                sps::read_color(&written),
                Some(signal),
                "a color written reads back"
            );
        }
        let _ = sps::bound_reorder(nal, pick & 2 != 0);
    }
    let _ = sps::h264_frame_num_range(unit);
});
