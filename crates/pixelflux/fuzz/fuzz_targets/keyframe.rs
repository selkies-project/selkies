/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! An encoded frame as a browser's webcam uplink hands one over, through the reads that label
//! it a key frame or not before a decoder sees it (`sniff_keyframe` in `src/webcam/decode.rs`,
//! the per-codec reads in `src/encoders/codec.rs`): the H.264 and H.265 NAL and slice headers,
//! the VP8 frame tag, the VP9 uncompressed header, and the AV1 OBU walk with its leb128 sizes.
//! Nothing panics or reads past the input. The input's first byte picks the codec, as the wire
//! type byte does (`parse_video_type`); the rest is the frame.
#![no_main]

use libfuzzer_sys::fuzz_target;

#[allow(dead_code)]
#[path = "../../src/encoders"]
mod encoders {
    pub mod bits;
    pub mod reference {
        pub const REFERENCE_FRAMES: u32 = 8;
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        pub enum Reference {
            #[default]
            Untracked,
            None,
            Frame(u16),
        }
    }
    pub mod codec;
}

#[allow(dead_code)]
mod ledger {
    pub const REFRESH_FINER: u32 = 3;
}

use encoders::codec::{
    av1_is_key, h264_frame_type, h265_frame_type, parse_video_type, vp8_is_key, vp9_is_key,
};

fuzz_target!(|data: &[u8]| {
    let Some((&pick, frame)) = data.split_first() else {
        return;
    };
    let _ = parse_video_type(pick);
    match pick % 5 {
        0 => {
            let _ = h264_frame_type(frame);
        }
        1 => {
            let _ = h265_frame_type(frame);
        }
        2 => {
            let _ = vp8_is_key(frame);
        }
        3 => {
            let _ = vp9_is_key(frame);
        }
        _ => {
            let _ = av1_is_key(frame);
        }
    }
});
