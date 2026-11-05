/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A microphone payload as a client may send it, through the RFC 2198 parse that runs before
//! anything is decoded (`src/red.rs`): every block it finds lies inside the payload, in order and
//! apart, the primary last and running to the payload's end, at most `RED_MAX_DISTANCE`
//! redundant blocks before it, none of them empty, and nothing panics. The input's first eight
//! bytes are the primary timestamp; the rest is the payload.
#![no_main]

use libfuzzer_sys::fuzz_target;

#[path = "../../src/red.rs"]
mod red;

fuzz_target!(|data: &[u8]| {
    if data.len() < 8 {
        return;
    }
    let (ts, payload) = data.split_at(8);
    let primary_ts = i64::from_le_bytes(ts.try_into().unwrap()) >> 1;
    let Some((blocks, n)) = red::red_blocks(payload, primary_ts) else {
        return;
    };
    assert!(
        (1..=red::RED_MAX_DISTANCE as usize + 1).contains(&n),
        "{n} blocks"
    );
    let mut end = 0;
    for (k, &(ts, start, len)) in blocks[..n].iter().enumerate() {
        assert!(
            start >= end && start + len <= payload.len(),
            "block {k} at {start}+{len}"
        );
        if k + 1 < n {
            assert!(len > 0, "an empty redundant block kept");
            assert!(
                ts <= primary_ts && primary_ts - ts <= 0x3FFF,
                "redundant ts {ts}"
            );
        } else {
            assert_eq!(
                (ts, start + len),
                (primary_ts, payload.len()),
                "the primary"
            );
        }
        end = start + len;
    }
});
