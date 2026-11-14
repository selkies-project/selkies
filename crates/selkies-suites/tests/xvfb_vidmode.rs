/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The images' Xvfb reports its screen's rate through XFree86-VidModeExtension.
//!
//! Mesa answers `glXGetMscRateOML` from the VidMode mode line, as dot clock * 1000 over the
//! totals' product in 32-bit arithmetic, and Chromium's GPU process asks ANGLE's GL backend for
//! that rate twice a second, logging a line from Xlib and one from ANGLE each time it fails. The
//! images' Xvfb reports one mode per screen: the screen's size, at the rate its fake vblank runs
//! at, from `-fakescreenfps` or from what `_FAKE_SCREEN_MILLIHZ` or `_FAKE_SCREEN_FPS` on the
//! root window sets. The mode line is read here over the wire, the reply libXxf86vm hands Mesa,
//! and the rate is checked the way Mesa computes it, exactly: at the server's start rate, at
//! 59.94 Hz, at 144 Hz and back, and on a server started at 30.

use std::process::Command;
use std::time::Duration;

use selkies_suites::Checks;
use selkies_suites::server::run_within;
use selkies_suites::x11::{ModeLine, mode_line, set_root_cardinal};
use selkies_suites::xvfb::Xvfb;

const SIZE: (u16, u16) = (1920, 1080);

/// The rate as Mesa's `glXGetMscRateOML` computes it, as a fraction; 0 over 1 where its 32 bits
/// overflow or the totals are empty.
fn mesa_rate(line: &ModeLine) -> (u64, u64) {
    let numerator = u64::from(line.dot_clock) * 1000;
    let denominator = u64::from(line.htotal) * u64::from(line.vtotal);
    if denominator == 0 || numerator >= 1 << 32 {
        (0, 1)
    } else {
        (numerator, denominator)
    }
}

/// Whether `rate` is exactly `hz_times_1000` over 1000.
fn is_exactly(rate: (u64, u64), millihertz: u64) -> bool {
    rate.0 * 1000 == millihertz * rate.1
}

/// Whether the Xvfb on `PATH` is the patched one the bundle builds and the images ship: its
/// patches add the spare outputs' CRTCs to its usage.
fn images_xvfb() -> bool {
    run_within(Command::new("Xvfb").arg("-help"), Duration::from_secs(10))
        .is_ok_and(|ran| format!("{}{}", ran.stdout, ran.stderr).contains("CRTCs per screen"))
}

#[test]
#[ignore = "needs the bundle's patched Xvfb (the wheel's libexec/selkies/Xvfb) on PATH"]
fn the_images_xvfb_reports_its_fake_vblank_rate_through_vidmode() {
    let mut checks = Checks::new("xvfb-vidmode");
    assert!(
        images_xvfb(),
        "the Xvfb on PATH is not the bundle's patched one: put the wheel's libexec/selkies/Xvfb on PATH"
    );
    {
        let server = Xvfb::start(u32::from(SIZE.0), u32::from(SIZE.1), &[]);
        let line = mode_line(server.display());
        checks.check(
            "the server offers XFree86-VidModeExtension",
            line.is_some(),
            "",
        );
        if let Some(line) = line {
            checks.check(
                "the mode line is the screen's size",
                (line.hdisplay, line.vdisplay) == SIZE,
                format!("{}x{}", line.hdisplay, line.vdisplay),
            );
            checks.check(
                "its sync pulses sit inside its blanking",
                line.hdisplay <= line.hsyncstart
                    && line.hsyncstart <= line.hsyncend
                    && line.hsyncend <= line.htotal
                    && line.vdisplay < line.vsyncstart
                    && line.vsyncstart <= line.vsyncend
                    && line.vsyncend < line.vtotal,
                format!(
                    "{} {} {} / {} {} {}",
                    line.hsyncstart,
                    line.hsyncend,
                    line.htotal,
                    line.vsyncstart,
                    line.vsyncend,
                    line.vtotal
                ),
            );
            let rate = mesa_rate(&line);
            checks.check(
                "it runs at the 60 Hz the server started at",
                is_exactly(rate, 60_000),
                format!("{rate:?}"),
            );
        }
        for (name, value, want) in [
            (
                "_FAKE_SCREEN_MILLIHZ",
                Some(59940),
                Some((59_940, "59.94 Hz, from _FAKE_SCREEN_MILLIHZ")),
            ),
            ("_FAKE_SCREEN_MILLIHZ", None, None),
            (
                "_FAKE_SCREEN_FPS",
                Some(144),
                Some((144_000, "144 Hz, from _FAKE_SCREEN_FPS")),
            ),
            (
                "_FAKE_SCREEN_FPS",
                None,
                Some((60_000, "60 Hz again once the properties are gone")),
            ),
        ] {
            set_root_cardinal(server.display(), name, value);
            let Some((millihertz, label)) = want else {
                continue;
            };
            let line = mode_line(server.display());
            let rate = line.as_ref().map_or((0, 1), mesa_rate);
            checks.check(
                &format!("it reads exactly {label}"),
                is_exactly(rate, millihertz),
                format!("{rate:?}"),
            );
        }
    }
    let server = Xvfb::start(
        u32::from(SIZE.0),
        u32::from(SIZE.1),
        &["-fakescreenfps", "30"],
    );
    let rate = mode_line(server.display())
        .as_ref()
        .map_or((0, 1), mesa_rate);
    checks.check(
        "a server started at -fakescreenfps 30 reads 30 Hz",
        is_exactly(rate, 30_000),
        format!("{rate:?}"),
    );
    checks.finish("");
}
