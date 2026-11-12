/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Turns the patched codec APIs codec-sys found in the headers of the linked libraries
//! (`patches/`) into cfgs; a library a distribution packages declares none of them, and the
//! encoders then take the path an unpatched library supports.

const PATCHED: &[&str] = &[
    "x264_in_place",
    "x264_qp_offsets",
    "x265_input_layout",
    "x265_constant_blocks",
    "x265_qp_offsets",
    "svtav1_constant_blocks",
    "svtav1_constant_blocks_skip",
    "svtav1_sequence_qp",
];

fn main() {
    for cfg in PATCHED {
        println!("cargo::rustc-check-cfg=cfg({cfg})");
        let found = format!("DEP_PIXELFLUX_CODEC_{}", cfg.to_uppercase());
        println!("cargo::rerun-if-env-changed={found}");
        if std::env::var_os(&found).is_some() {
            println!("cargo::rustc-cfg={cfg}");
        }
    }
}
