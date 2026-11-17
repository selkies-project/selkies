/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A desktop that reads the CRTC when told the screen changed finds the final mode.
//!
//! MATE's panel lays itself out from the CRTCs of the connected outputs, re-read in an idle
//! callback after GDK reports the screen or its monitors changed (mate-panel's
//! panel-multimonitor.c). Where the server has no spare outputs, a lone display is a logical
//! monitor over the one output, whose CRTC shows exactly that rectangle, and the WebSockets
//! transport both swaps that monitor and sets the mode. GDK takes the screen's size from its
//! monitors, so a swap ahead of the mode set reports the new size while the CRTC still has the old
//! mode, and the mode set that follows changes no monitor, so nothing is reported again: the panel
//! stayed laid out for the root the session started on, its bottom half off screen
//! (jupyter-server-proxy#620).
//!
//! Driven against a private X server started larger than the page, as a session is, with no
//! spare outputs, as a stock Xvfb has (the images' Xvfb offers them, unplugged here), by a GDK
//! client that re-reads the CRTCs as the panel does (tests/tools/crtc_probe.c), through a page's
//! first size, a later resize, and one to a width off the 8-pixel CVT cell, which the framebuffer
//! rounds up and the CRTC does not. Every read must find the CRTC where GDK's monitor is: a read
//! that finds them apart is the panel laid out for a mode the screen no longer has, and a Qt
//! desktop, which announces a monitor's new geometry only where it is exactly its CRTC, kept at
//! the old one.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use selkies_suites::server::run_within;
use selkies_suites::xvfb::Xvfb;
use selkies_suites::{Checks, Client, on_path, repo, server, workdir, x11};

const ROOT: (u32, u32) = (4096, 2160);
const FIRST: (i64, i64) = (1280, 720);
const SECOND: (i64, i64) = (1024, 768);
/// A width off the 8-pixel CVT cell: the root takes it rounded up, the CRTC and the monitor the
/// width itself.
const ODD: (i64, i64) = (1366, 768);

/// One re-read of the probe's: the connected outputs' CRTCs and GDK's monitors, each sorted.
#[derive(Debug, PartialEq)]
struct Read {
    crtcs: Vec<Vec<i64>>,
    monitors: Vec<Vec<i64>>,
}

/// The GDK probe, built into the work directory against the GTK3 and Xrandr `pkg-config` finds.
fn build_probe() -> Result<PathBuf, String> {
    let pkg_config = on_path("pkg-config").ok_or("no pkg-config")?;
    let ask = |args: &[&str]| -> Result<String, String> {
        let out = Command::new(&pkg_config)
            .args(args)
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
        }
    };
    let flags = ask(&["--cflags", "--libs", "gtk+-3.0", "xrandr", "x11"])?;
    let libdir = ask(&["--variable=libdir", "gtk+-3.0"])?;
    let probe = workdir().join(format!("crtc_probe-{}", std::process::id()));
    let mut cc = Command::new("cc");
    cc.args(["-O2", "-o"])
        .arg(&probe)
        .arg(repo().join("tests/tools/crtc_probe.c"))
        .args(flags.split_whitespace())
        .arg(format!("-Wl,-rpath,{libdir}"));
    let built = run_within(&mut cc, Duration::from_secs(180))?;
    if built.status.success() {
        Ok(probe)
    } else {
        Err(built.stderr.chars().take(300).collect())
    }
}

/// Leave the server as a stock Xvfb is: the images' Xvfb offers spare outputs through a
/// `Connected` property, which the server takes as the capability to lay displays out as outputs
/// of their own.
///
/// # Panics
///
/// When the server cannot be asked or does not drop a property: with an output left pluggable
/// the session lays the display out as an output, and the logical monitor is never exercised.
fn unplug_spare_outputs(display: &str) {
    const ASKED: &str = "the private X server answers";
    let (conn, screen) = RustConnection::connect(Some(display))
        .unwrap_or_else(|e| panic!("no connection to {display}: {e}"));
    let root = conn.setup().roots[screen].root;
    let connected = conn
        .intern_atom(false, b"Connected")
        .expect(ASKED)
        .reply()
        .expect(ASKED)
        .atom;
    let outputs = conn
        .randr_get_screen_resources(root)
        .expect(ASKED)
        .reply()
        .expect(ASKED)
        .outputs;
    for output in outputs {
        let listed = conn
            .randr_list_output_properties(output)
            .expect(ASKED)
            .reply()
            .expect(ASKED)
            .atoms
            .contains(&connected);
        if listed {
            conn.randr_delete_output_property(output, connected)
                .expect(ASKED)
                .check()
                .expect("the Connected property is deleted");
        }
    }
    conn.sync().expect(ASKED);
}

/// The probe's re-reads, oldest first.
fn reads(log: &Path) -> Vec<Read> {
    let rects = |value: &serde_json::Value| -> Vec<Vec<i64>> {
        value
            .as_array()
            .map(|all| {
                all.iter()
                    .map(|r| {
                        r.as_array()
                            .map(|v| v.iter().filter_map(serde_json::Value::as_i64).collect())
                            .unwrap_or_default()
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .map(|v| Read {
            crtcs: rects(&v["crtcs"]),
            monitors: rects(&v["monitors"]),
        })
        .collect()
}

/// Waits for the root to reach `size` and the probe's reads to stop; the root size reached and
/// the reads.
async fn settle(display: &str, log: &Path, size: (u16, u16)) -> (Option<(u16, u16)>, Vec<Read>) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && x11::root_size(display) != Some(size) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let mut seen = reads(log).len();
    let mut quiet = Instant::now();
    while quiet.elapsed() < Duration::from_secs(2)
        && Instant::now() < deadline + Duration::from_secs(10)
    {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let now = reads(log).len();
        if now != seen {
            seen = now;
            quiet = Instant::now();
        }
    }
    (x11::root_size(display), reads(log))
}

/// Kills and reaps the probe when it goes out of scope.
struct Probe(Child);

impl Drop for Probe {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
#[ignore = "needs a built selkies server, Xvfb, and GTK3's development files"]
async fn a_desktop_reading_the_crtc_finds_the_final_mode() {
    let mut checks = Checks::new("lone-display-crtc-follows");
    let probe = build_probe().unwrap_or_else(|e| panic!("the GDK probe did not build: {e}"));
    let xvfb = Xvfb::start(ROOT.0, ROOT.1, &[]);
    let display = xvfb.display().to_owned();
    let ready = workdir().join("crtc-probe-ready");
    let log = workdir().join("crtc-probe.jsonl");
    let errors = workdir().join("crtc-probe.stderr");
    for path in [&ready, &log] {
        let _ = std::fs::remove_file(path);
    }
    unplug_spare_outputs(&display);
    let running = Command::new(&probe)
        .arg(&ready)
        .arg(&log)
        .env("DISPLAY", &display)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&errors).expect("the probe's stderr"))
        .spawn()
        .map(Probe)
        .expect("the GDK probe");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready.exists() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !ready.exists() {
        let said = std::fs::read_to_string(&errors).unwrap_or_default();
        let tail: String = said
            .chars()
            .rev()
            .take(160)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        checks.check("the GDK probe started", false, tail);
        checks.finish("");
        return;
    }

    let server = server::launch("lone-display-crtc-follows")
        .display(&display)
        .start()
        .await;
    let page = Client::connect(&server, "").await;
    let settings = json!({
        "displayId": "primary", "initialClientWidth": FIRST.0, "initialClientHeight": FIRST.1,
        "manual_resolution": false, "framerate": 60, "encoder": "h264enc", "scaling_dpi": 96,
    });
    page.send(&format!("SETTINGS,{settings}"));
    let mut done = 0;
    for (label, size) in [
        ("a page's first size", FIRST),
        ("a later resize", SECOND),
        ("a width off the CVT cell", ODD),
    ] {
        if size != FIRST {
            page.send(&format!("r,{}x{},primary", size.0, size.1));
        }
        let root = (
            u16::try_from((size.0 + 7) / 8 * 8).unwrap_or(0),
            u16::try_from(size.1).unwrap_or(0),
        );
        let (got, seen) = settle(&display, &log, root).await;
        let want = vec![vec![0, 0, size.0, size.1]];
        checks.check(
            &format!("{label} reaches the root"),
            got == Some(root),
            format!("{got:?}"),
        );
        let apart: Vec<&Read> = seen
            .iter()
            .skip(done)
            .filter(|r| r.crtcs != r.monitors)
            .collect();
        checks.check(
            &format!("{label}: every re-read found the CRTC where GDK's monitor is"),
            apart.is_empty(),
            format!("{:?}", &apart[..apart.len().min(2)]),
        );
        checks.check(
            &format!("{label}: and the last at the new mode"),
            seen.len() > done && seen.last().is_some_and(|r| r.crtcs == want),
            format!("{:?}", seen.last()),
        );
        done = seen.len();
    }
    let log_text = server.stop().await;
    drop(running);
    drop(xvfb);
    checks.finish(&log_text);
}
