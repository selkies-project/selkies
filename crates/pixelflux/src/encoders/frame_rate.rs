/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A frame rate as the fraction of whole numbers that encoder parameters and bitstream timing
//! take, read from the `f64` a capture carries.
//!
//! A caller names a rate by its fraction, carried as the `f64` of that fraction: the NTSC rates
//! are `N * 1000 / 1001` (59.94 is 60000/1001), which no whole number or decimal names. The
//! fraction nearest the `f64` with a denominator up to `MAX_DENOMINATOR` is that fraction
//! exactly, since any other in reach lies at least a millionth of a frame per second away, far
//! past an `f64`'s error. A rate written with up to three decimals reads as its own decimal
//! fraction, and any other rate as the nearest fraction in reach, within a thousandth of a frame
//! per second.

/// The largest denominator a rate is read with: 1001 holds every NTSC rate and every rate
/// written with up to three decimals.
const MAX_DENOMINATOR: u128 = 1001;

/// The span of rates a capture runs at, in frames per second.
const MIN_FPS: f64 = 1.0;
const MAX_FPS: f64 = 1000.0;

/// A frame rate of `num / den` frames per second, in lowest terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameRate {
    pub num: u32,
    pub den: u32,
}

impl FrameRate {
    /// The fraction `fps` names (see the module docs), within one to 1000 frames per second; a
    /// rate that is not a number reads as one.
    pub fn of(fps: f64) -> Self {
        let fps = if fps.is_nan() {
            MIN_FPS
        } else {
            fps.clamp(MIN_FPS, MAX_FPS)
        };
        let bits = fps.to_bits();
        let mantissa = ((bits & ((1 << 52) - 1)) | (1 << 52)) as u128;
        let shift = 1075 - ((bits >> 52) & 0x7ff) as i32;
        let (num, den) = limit_denominator(mantissa, 1 << shift, MAX_DENOMINATOR);
        Self {
            num: num as u32,
            den: den as u32,
        }
    }

    /// The rate in frames per second.
    pub fn fps(self) -> f64 {
        self.num as f64 / self.den as f64
    }

    /// The whole frames per second a level has to admit: the rate, rounded up.
    pub fn ceil(self) -> u32 {
        self.num.div_ceil(self.den)
    }

    /// The fraction nearest the rate whose terms both fit `max`, for a field narrower than 32
    /// bits (VA-API packs each into 16).
    pub fn within(self, max: u32) -> Self {
        let mut bound = (max as u64 * self.den as u64 / self.num as u64).max(1) as u128;
        loop {
            let (num, den) = limit_denominator(self.num as u128, self.den as u128, bound);
            if num <= max as u128 || bound == 1 {
                return Self {
                    num: num.min(max as u128) as u32,
                    den: den as u32,
                };
            }
            bound -= 1;
        }
    }
}

/// The fraction nearest `num / den` whose denominator is at most `max_den`, in lowest terms:
/// the last continued-fraction convergent within the bound, or the semiconvergent past it where
/// that lies nearer.
fn limit_denominator(num: u128, den: u128, max_den: u128) -> (u128, u128) {
    let g = gcd(num, den);
    let (num, den) = (num / g, den / g);
    if den <= max_den {
        return (num, den);
    }
    let (mut p0, mut q0, mut p1, mut q1) = (0u128, 1u128, 1u128, 0u128);
    let (mut n, mut d) = (num, den);
    loop {
        let a = n / d;
        let q2 = q0 + a * q1;
        if q2 > max_den {
            break;
        }
        (p0, q0, p1, q1) = (p1, q1, p0 + a * p1, q2);
        (n, d) = (d, n - a * d);
    }
    let k = (max_den - q0) / q1;
    if 2 * d * (q0 + k * q1) <= den {
        (p1, q1)
    } else {
        (p0 + k * p1, q0 + k * q1)
    }
}

fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate(num: u32, den: u32) -> FrameRate {
        FrameRate { num, den }
    }

    /// Every rate a client's display runs at comes back as the fraction it was sent as, whether
    /// the page computed it as `N * 1000 / 1001` or snapped it to a whole number.
    #[test]
    fn a_rate_carried_as_its_fraction_reads_back_exactly() {
        for n in [24, 25, 30, 48, 50, 60, 72, 75, 90, 100, 120, 144, 165, 240] {
            assert_eq!(FrameRate::of(n as f64), rate(n, 1));
            let g = gcd(n as u128 * 1000, 1001) as u32;
            assert_eq!(
                FrameRate::of(n as f64 * 1000.0 / 1001.0),
                rate(n * 1000 / g, 1001 / g),
                "{n}000/1001"
            );
        }
        assert_eq!(FrameRate::of(60000.0 / 1001.0).fps(), 60000.0 / 1001.0);
    }

    /// A rate written as a decimal names its own fraction; the NTSC rates spelled that way lie
    /// within a millionth of the NTSC fraction.
    #[test]
    fn a_decimal_rate_reads_as_its_decimal_fraction() {
        assert_eq!(FrameRate::of(59.94), rate(2997, 50));
        assert_eq!(FrameRate::of(119.88), rate(2997, 25));
        assert_eq!(FrameRate::of(143.856), rate(17982, 125));
        assert_eq!(FrameRate::of(23.976), rate(2997, 125));
        assert_eq!(FrameRate::of(29.97), rate(2997, 100));
        assert_eq!(FrameRate::of(12.5), rate(25, 2));
        for (decimal, n) in [(59.94, 60), (119.88, 120), (143.856, 144)] {
            let ntsc = n as f64 * 1000.0 / 1001.0;
            assert!((FrameRate::of(decimal).fps() - ntsc).abs() / ntsc < 1.1e-6);
        }
    }

    /// A rate no small fraction names -- a measured display, a rate scaled by a factor -- reads
    /// as the nearest fraction in reach, within a thousandth of a frame per second, and one just
    /// off an NTSC fraction lands on it.
    #[test]
    fn any_other_rate_reads_within_a_thousandth() {
        for fps in [
            53.28900001,
            59.9512345,
            143.98123,
            7.3,
            999.9999,
            1.0000001,
            164.987654321,
            60.0005,
            1.0004999,
        ] {
            let r = FrameRate::of(fps);
            assert!(r.den <= 1001, "{fps}: {r:?}");
            assert!((r.fps() - fps).abs() < 1e-3, "{fps}: {r:?} = {}", r.fps());
        }
        assert_eq!(FrameRate::of(59.9400599), rate(60000, 1001));
        assert_eq!(FrameRate::of(119.880120), rate(120000, 1001));
    }

    #[test]
    fn a_rate_outside_the_span_reads_at_its_edge() {
        assert_eq!(FrameRate::of(0.0), rate(1, 1));
        assert_eq!(FrameRate::of(-5.0), rate(1, 1));
        assert_eq!(FrameRate::of(f64::NAN), rate(1, 1));
        assert_eq!(FrameRate::of(f64::INFINITY), rate(1000, 1));
        assert_eq!(FrameRate::of(5000.0), rate(1000, 1));
    }

    /// A level admits the whole frames per second at or above the rate.
    #[test]
    fn a_level_is_sized_for_the_rate_rounded_up() {
        assert_eq!(rate(60000, 1001).ceil(), 60);
        assert_eq!(rate(144000, 1001).ceil(), 144);
        assert_eq!(rate(60, 1).ceil(), 60);
        assert_eq!(rate(2997, 50).ceil(), 60);
    }

    /// A field of 16 bits takes the NTSC rates whose terms fit it as they are, and the others
    /// as the nearest fraction that fits, within a tenth of a millionth.
    #[test]
    fn a_narrow_field_takes_the_nearest_fraction_that_fits() {
        assert_eq!(rate(60000, 1001).within(0xffff), rate(60000, 1001));
        assert_eq!(rate(60, 1).within(0xffff), rate(60, 1));
        for n in [120, 144, 240] {
            let exact = rate(n * 1000, 1001);
            let fit = exact.within(0xffff);
            assert!(fit.num <= 0xffff && fit.den <= 0xffff, "{fit:?}");
            assert!(
                (fit.fps() - exact.fps()).abs() / exact.fps() < 1e-7,
                "{n}: {fit:?}"
            );
        }
        assert_eq!(rate(1000, 1).within(0xffff), rate(1000, 1));
    }
}
