/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The moving scene the suites decode and check (`src/bin/motion-scene.rs` on X11,
//! `src/bin/wl-motion-scene.rs` on Wayland): where each part sits, and the noise both draw, from
//! the random stream of CPython's `random.Random(1)`, so the frames are the ones the suites'
//! checkers rebuild.

pub const CODE_BITS: u32 = 16;
pub const CODE_SIZE: i16 = 48;
pub const CODE_GAP: i16 = 8;
pub const CODE_X: i16 = 16;
pub const CODE_Y: i16 = 16;
pub const BAND_Y: i16 = 96;
pub const BAND_H: u16 = 64;
pub const CHECK: u32 = 16;
pub const BAND_STEP: u32 = 4;
pub const BAR_W: u32 = 160;
pub const BAR_STEP: u32 = 12;
pub const NOISE_H: u16 = 240;
pub const NOISE_IMAGES: usize = 4;
pub const BACKGROUND: u32 = 0x001e_2878;
pub const BAR: u32 = 0x00dc_3c28;
pub const WHITE: u32 = 0x00ff_ffff;
pub const BLACK: u32 = 0;

/// MT19937, seeded as CPython seeds `random.Random(n)` for a small `n`: `init_by_array([n])`.
pub struct Twister {
    state: [u32; 624],
    index: usize,
}

impl Twister {
    /// The generator `random.Random(seed)` is, for a seed below 2^32.
    pub fn python(seed: u32) -> Twister {
        let mut state = [0u32; 624];
        state[0] = 19_650_218;
        for i in 1..624 {
            let prev = state[i - 1];
            state[i] = 1_812_433_253u32
                .wrapping_mul(prev ^ (prev >> 30))
                .wrapping_add(u32::try_from(i).unwrap_or(0));
        }
        let key = [seed];
        let (mut i, mut j) = (1usize, 0usize);
        for _ in 0..624.max(key.len()) {
            let prev = state[i - 1];
            state[i] = (state[i] ^ (prev ^ (prev >> 30)).wrapping_mul(1_664_525))
                .wrapping_add(key[j])
                .wrapping_add(u32::try_from(j).unwrap_or(0));
            i += 1;
            j += 1;
            if i >= 624 {
                state[0] = state[623];
                i = 1;
            }
            if j >= key.len() {
                j = 0;
            }
        }
        for _ in 0..623 {
            let prev = state[i - 1];
            state[i] = (state[i] ^ (prev ^ (prev >> 30)).wrapping_mul(1_566_083_941))
                .wrapping_sub(u32::try_from(i).unwrap_or(0));
            i += 1;
            if i >= 624 {
                state[0] = state[623];
                i = 1;
            }
        }
        state[0] = 0x8000_0000;
        Twister { state, index: 624 }
    }

    /// The next 32-bit output, as `genrand_uint32`.
    pub fn next_u32(&mut self) -> u32 {
        if self.index >= 624 {
            for k in 0..624 {
                let y = (self.state[k] & 0x8000_0000) | (self.state[(k + 1) % 624] & 0x7fff_ffff);
                let mut next = self.state[(k + 397) % 624] ^ (y >> 1);
                if y & 1 != 0 {
                    next ^= 0x9908_b0df;
                }
                self.state[k] = next;
            }
            self.index = 0;
        }
        let mut y = self.state[self.index];
        self.index += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c_5680;
        y ^= (y << 15) & 0xefc6_0000;
        y ^ (y >> 18)
    }

    /// `getrandbits(24)`: the top 24 bits of the next word.
    pub fn bits24(&mut self) -> u32 {
        self.next_u32() >> 8
    }
}
