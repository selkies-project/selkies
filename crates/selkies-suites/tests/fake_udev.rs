/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! fake-udev (addons/fake-udev) adds the pads the interposer is serving to what the real libudev
//! reports.
//!
//! A udev consumer that opens a node the server is not serving pays the interposer's full connect
//! timeout before the open fails, once per unbound slot, so fake-udev lists a js or event node only
//! while its socket is bound, the way the interposer's own directory listing does. Everything
//! outside the pads passes through to the system library, so a consumer asking for another
//! subsystem (KWin for its render nodes) sees the same devices as without the preload, and the
//! host's own input devices stay listed beside the pads.
//!
//! Builds fake-udev and the `udevscan` tool into a scratch directory, then scans under
//! `LD_PRELOAD` with sockets present for none, one and all four slots, scans a subsystem the pads
//! do not live in, and scans with passthrough disabled. It needs a C compiler, make and libudev's
//! headers, which building the server needs too, so it runs with `cargo test --workspace`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use selkies_suites::server::run_within;
use selkies_suites::{Checks, repo, workdir};

/// What one scan reported: its counts, and the device nodes it listed.
#[derive(Debug, Default)]
struct Scan {
    counts: BTreeMap<String, i64>,
    nodes: Vec<String>,
}

impl Scan {
    fn count(&self, key: &str) -> Option<i64> {
        self.counts.get(key).copied()
    }
}

/// Builds fake-udev from a copy of its source and the scan tool, both into `dir`.
fn build(dir: &Path) -> (PathBuf, PathBuf) {
    let src = dir.join("fake-udev");
    std::fs::create_dir_all(&src).expect("the build directory");
    for entry in std::fs::read_dir(repo().join("addons/fake-udev")).expect("addons/fake-udev") {
        let path = entry.expect("an entry").path();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if path.is_file() && !name.ends_with(".o") && !name.contains(".so") {
            std::fs::copy(&path, src.join(&name)).expect("a source file");
        }
    }
    let make = run_within(
        Command::new("make")
            .args(["-s", "-C"])
            .arg(&src)
            .arg("libudev.so.1"),
        Duration::from_secs(180),
    )
    .expect("make ran");
    assert!(make.status.success(), "make failed: {}", make.stderr);
    let tool = dir.join("udevscan");
    let cc = run_within(
        Command::new("cc")
            .arg("-O2")
            .arg("-o")
            .arg(&tool)
            .arg(repo().join("tests/tools/gamepad/udevscan.c"))
            .arg("-ludev"),
        Duration::from_secs(120),
    )
    .expect("cc ran");
    assert!(cc.status.success(), "udevscan did not build: {}", cc.stderr);
    (src.join("libudev.so.1"), tool)
}

/// Counts `udevscan` reports under fake-udev (`lib`, or none) with the sockets in `sockets`.
fn scan(
    lib: Option<&Path>,
    tool: &Path,
    sockets: &Path,
    subsystem: &str,
    passthrough: bool,
) -> Scan {
    let mut command = Command::new(tool);
    command
        .arg(subsystem)
        .env("SELKIES_JS_SOCKET_PATH", sockets)
        .env_remove("LD_PRELOAD");
    if let Some(lib) = lib {
        command.env("LD_PRELOAD", lib);
    }
    if !passthrough {
        command.env("SELKIES_REAL_LIBUDEV", "none");
    }
    let out = run_within(&mut command, Duration::from_secs(30))
        .map(|r| r.stdout)
        .unwrap_or_default();
    let mut result = Scan::default();
    if let Some(line) = out.lines().find(|l| l.starts_with("RESULT")) {
        for pair in line.split_whitespace().skip(1) {
            if let Some((k, v)) = pair.split_once('=') {
                result.counts.insert(k.to_owned(), v.parse().unwrap_or(-1));
            }
        }
    }
    result.nodes = out
        .lines()
        .filter(|l| l.starts_with("DEV "))
        .filter_map(|l| l.split_whitespace().nth(2))
        .map(str::to_owned)
        .collect();
    result
}

/// Stand-ins for the two sockets the interposer binds for `slot`.
fn bind(sockets: &Path, slot: usize) {
    for name in [format!("js{slot}"), format!("event{}", 1000 + slot)] {
        std::fs::File::create(sockets.join(format!("selkies_{name}.sock")))
            .expect("a socket stand-in");
    }
}

#[test]
fn fake_udev_enumeration_follows_the_bound_sockets() {
    let dir = workdir().join(format!("fake-udev-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the scratch directory");
    let (lib, tool) = build(&dir);
    let sockets = dir.join("sockets");
    std::fs::create_dir_all(&sockets).expect("the socket directory");
    let mut checks = Checks::new("fake-udev-enumeration");

    // The host's own input devices, which stay visible beside the pads; a real node that shares a
    // pad's name is hidden, so it is left out of the baseline.
    let baseline = scan(None, &tool, &sockets, "input", true);
    let hidden: Vec<String> = (0..4)
        .map(|i| format!("/dev/input/js{i}"))
        .chain((0..4).map(|i| format!("/dev/input/event{}", 1000 + i)))
        .collect();
    let host = i64::try_from(
        baseline
            .nodes
            .iter()
            .filter(|n| !hidden.contains(n))
            .count(),
    )
    .expect("a count");

    let empty = scan(Some(&lib), &tool, &sockets, "input", true);
    checks.check(
        "no bound socket, no pad enumerated",
        empty.count("virtual") == Some(0) && empty.count("input_devs") == Some(host),
        format!("{empty:?}"),
    );
    bind(&sockets, 0);
    let one = scan(Some(&lib), &tool, &sockets, "input", true);
    checks.check(
        "one bound slot enumerates its js and event nodes only",
        one.count("virtual") == Some(2) && one.count("input_devs") == Some(host + 2),
        format!("{one:?}"),
    );
    for slot in 1..4 {
        bind(&sockets, slot);
    }
    let four = scan(Some(&lib), &tool, &sockets, "input", true);
    checks.check(
        "all four bound slots enumerate eight nodes",
        four.count("virtual") == Some(8) && four.count("input_devs") == Some(host + 8),
        format!("{four:?}"),
    );
    checks.check(
        "every pad node reports ID_INPUT_JOYSTICK",
        four.count("joydevs").unwrap_or(0) >= 8,
        format!("{four:?}"),
    );
    std::fs::remove_file(sockets.join("selkies_js2.sock")).expect("a socket stand-in");
    let partial = scan(Some(&lib), &tool, &sockets, "input", true);
    checks.check(
        "a slot serving only its event socket lists just that node",
        partial.count("virtual") == Some(7) && partial.count("input_devs") == Some(host + 7),
        format!("{partial:?}"),
    );
    let real_mem = scan(None, &tool, &sockets, "mem", true);
    let faked_mem = scan(Some(&lib), &tool, &sockets, "mem", true);
    checks.check(
        "another subsystem passes through to the real libudev",
        faked_mem.nodes == real_mem.nodes && faked_mem.nodes.iter().any(|n| n == "/dev/null"),
        format!("{faked_mem:?}"),
    );
    let isolated = scan(Some(&lib), &tool, &sockets, "input", false);
    checks.check(
        "with passthrough disabled only the pads remain",
        isolated.count("virtual") == Some(7) && isolated.count("input_devs") == Some(7),
        format!("{isolated:?}"),
    );
    checks.check(
        "with passthrough disabled another subsystem is empty",
        scan(Some(&lib), &tool, &sockets, "mem", false).count("input_devs") == Some(0),
        "",
    );
    checks.finish("");
}
