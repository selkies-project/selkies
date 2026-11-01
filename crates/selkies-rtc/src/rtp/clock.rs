/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 * This file incorporates work covered by the following copyright and
 * permission notice:
 *
 *   Copyright (c) Jeremy Lainé.
 *   All rights reserved.
 *
 *   Redistribution and use in source and binary forms, with or without
 *   modification, are permitted provided that the following conditions are met:
 *
 *       * Redistributions of source code must retain the above copyright notice,
 *       this list of conditions and the following disclaimer.
 *       * Redistributions in binary form must reproduce the above copyright notice,
 *       this list of conditions and the following disclaimer in the documentation
 *       and/or other materials provided with the distribution.
 *       * Neither the name of aiortc nor the names of its contributors may
 *       be used to endorse or promote products derived from this software without
 *       specific prior written permission.
 *
 *   THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
 *   ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
 *   WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
 *   DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
 *   FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
 *   DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
 *   SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
 *   CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
 *   OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
 *   OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
 */

//! The clocks the RTP layer stamps packets with.
//!
//! Every instant inside the layer is `CLOCK_MONOTONIC`, the clock pixelflux stamps a
//! frame's capture and encode with, read as nanoseconds (`monotonic_ns`) or seconds
//! (`monotonic`). NTP time, which sender reports and the abs-capture-time extension
//! carry, is derived from it through one offset taken at first use, so the NTP clock runs
//! with the monotonic one and never jumps with the wall clock: a receiver mapping a
//! frame's RTP timestamp through two sender reports reads its capture instant to well
//! under a millisecond.

use std::sync::OnceLock;

/// Seconds between the NTP epoch (1900) and the Unix epoch (1970).
pub const NTP_UNIX_OFFSET: u64 = 2_208_988_800;

/// Nanoseconds of `CLOCK_MONOTONIC` now.
pub fn monotonic_ns() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes the timespec it is handed and nothing else.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
}

/// Seconds of `CLOCK_MONOTONIC` now.
pub fn monotonic() -> f64 {
    monotonic_ns() as f64 / 1e9
}

fn ntp_offset_ns() -> i64 {
    static OFFSET: OnceLock<i64> = OnceLock::new();
    *OFFSET.get_or_init(|| {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: as above, for the wall clock.
        unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
        let wall_ns = ts.tv_sec * 1_000_000_000 + ts.tv_nsec;
        wall_ns + NTP_UNIX_OFFSET as i64 * 1_000_000_000 - monotonic_ns()
    })
}

/// The 64-bit NTP timestamp (UQ32.32) of a `CLOCK_MONOTONIC` instant in nanoseconds.
pub fn ntp_from_monotonic_ns(instant_ns: i64) -> u64 {
    let total = (instant_ns + ntp_offset_ns()) as u64;
    let seconds = total / 1_000_000_000;
    let rest = total % 1_000_000_000;
    ((seconds & 0xFFFF_FFFF) << 32) | ((rest << 32) / 1_000_000_000)
}

/// The NTP timestamp now.
pub fn current_ntp_time() -> u64 {
    ntp_from_monotonic_ns(monotonic_ns())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ntp_runs_with_the_monotonic_clock() {
        let a = ntp_from_monotonic_ns(10_i64.pow(15));
        let b = ntp_from_monotonic_ns(10_i64.pow(15) + 1_500_000_000);
        let elapsed = (b - a) as f64 / (1u64 << 32) as f64;
        assert!((elapsed - 1.5).abs() < 1e-8, "{elapsed}");
    }

    #[test]
    fn ntp_seconds_are_past_the_unix_epoch_offset() {
        let now = current_ntp_time();
        assert!((now >> 32) > NTP_UNIX_OFFSET);
        let wall = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
            + NTP_UNIX_OFFSET as f64;
        let ntp = (now >> 32) as f64 + (now & 0xFFFF_FFFF) as f64 / (1u64 << 32) as f64;
        assert!((ntp - wall).abs() < 0.1, "{ntp} vs {wall}");
    }
}
