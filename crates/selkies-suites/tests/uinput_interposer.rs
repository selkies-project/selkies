/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! An application under the input interposer with no kernel /dev/uinput still creates virtual
//! input devices: its open("/dev/uinput"), the UI_* setup ioctls and the event writes are served
//! in userspace, the created device appears as a /dev/input/eventN backed by a socket, and a
//! sibling process reads it as an ordinary evdev device. On a host with a writable /dev/uinput
//! the path is not taken: the kernel node is opened for real.
//!
//! - create: one process builds a device, another opens the node and checks its EVIOCG*
//!   identity and that the event stream arrives byte for byte.
//! - udev: a gamepad-capable device is discovered through fake-udev with its ID_INPUT_JOYSTICK
//!   class, and vanishes once its creator exits.
//!
//! These need only a C compiler, make and libudev's headers, so they run with `cargo test
//! --workspace`.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use selkies_suites::server::run_within;
use selkies_suites::{Checks, repo, workdir};

fn scratch(name: &str) -> PathBuf {
    let dir = workdir().join(format!("uinput-interposer-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the scratch directory");
    dir
}

/// Compiles with the C compiler, failing the test with the compiler's errors.
fn cc(args: &[&str]) {
    let ran = run_within(Command::new("cc").args(args), Duration::from_secs(120))
        .unwrap_or_else(|e| panic!("cc {args:?}: {e}"));
    assert!(ran.status.success(), "cc {args:?} failed: {}", ran.stderr);
}

fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The interposer and the device tool (tests/tools/uinput_create_consume.c), built into `dir`.
fn build(dir: &Path) -> (PathBuf, PathBuf) {
    let lib = dir.join("selkies_input_interposer.so");
    cc(&[
        "-O2",
        "-shared",
        "-fPIC",
        "-o",
        &text(&lib),
        &text(&repo().join("addons/input-interposer/input_interposer.c")),
        "-ldl",
        "-pthread",
    ]);
    let tool = dir.join("uinput_create_consume");
    cc(&[
        "-O2",
        "-o",
        &text(&tool),
        &text(&repo().join("tests/tools/uinput_create_consume.c")),
    ]);
    (lib, tool)
}

/// fake-udev and the scanner that lists what it enumerates, built into `dir`.
fn build_udev(dir: &Path) -> (PathBuf, PathBuf) {
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
    assert!(
        make.status.success(),
        "fake-udev did not build: {}",
        make.stderr
    );
    let scan = dir.join("udevscan");
    cc(&[
        "-O2",
        "-o",
        &text(&scan),
        &text(&repo().join("tests/tools/gamepad/udevscan.c")),
        "-ludev",
    ]);
    (src.join("libudev.so.1"), scan)
}

#[test]
fn a_device_created_in_one_process_is_read_in_another() {
    let mut checks = Checks::new("uinput-interposer");
    let work = scratch("create");
    let (lib, tool) = build(&work);
    let sockets = work.join("create");
    std::fs::create_dir_all(&sockets).expect("the socket directory");
    let ran = run_within(
        Command::new(&tool)
            .env_remove("SDL_JOYSTICK_DEVICE")
            .env("LD_PRELOAD", &lib)
            .env("SELKIES_JS_SOCKET_PATH", &sockets),
        Duration::from_secs(60),
    );
    let (line, err) = match ran {
        Ok(ran) => (
            ran.stdout.trim().to_owned(),
            ran.stderr.trim().replace('\n', " ; "),
        ),
        Err(e) => (String::new(), e),
    };
    checks.check(
        "create-consume-round-trip",
        line.starts_with("PASS"),
        format!("{line} | {err}"),
    );
    let _ = std::fs::remove_dir_all(&work);
    checks.finish("");
}

/// What the scanner lists through fake-udev for the devices served from `sockets`.
fn scan(scanner: &Path, fake: &Path, sockets: &Path) -> String {
    run_within(
        Command::new(scanner)
            .arg("input")
            .env_remove("SDL_JOYSTICK_DEVICE")
            .env("LD_PRELOAD", fake)
            .env("SELKIES_JS_SOCKET_PATH", sockets)
            .env("SELKIES_REAL_LIBUDEV", "none"),
        Duration::from_secs(30),
    )
    .map(|ran| ran.stdout)
    .unwrap_or_default()
}

#[test]
fn a_created_gamepad_is_enumerated_as_a_joystick_until_its_creator_exits() {
    let mut checks = Checks::new("uinput-interposer");
    let work = scratch("udev");
    let (lib, tool) = build(&work);
    let (fake, scanner) = build_udev(&work);
    let sockets = work.join("udev");
    std::fs::create_dir_all(&sockets).expect("the socket directory");
    let mut hold = Command::new(&tool)
        .arg("hold")
        .env_remove("SDL_JOYSTICK_DEVICE")
        .env("LD_PRELOAD", &lib)
        .env("SELKIES_JS_SOCKET_PATH", &sockets)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("the device's creator");
    // The creator prints its node once the device exists; the rest of its output is drained so
    // it never writes into a closed pipe.
    let (said, heard) = std::sync::mpsc::channel();
    let stdout = hold.stdout.take().expect("its stdout");
    std::thread::spawn(move || {
        let mut sent = false;
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if !sent && !line.trim().is_empty() {
                let _ = said.send(line.trim().to_owned());
                sent = true;
            }
        }
    });
    let node = heard
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_default();

    let listed = scan(&scanner, &fake, &sockets);
    let seen = !node.is_empty() && listed.lines().any(|line| line.contains(&node));
    let joystick = !node.is_empty()
        && listed
            .split_once("joystick:")
            .and_then(|(_, rest)| rest.lines().next())
            .is_some_and(|line| line.contains(&format!("/{node}")));
    checks.check(
        "udev-dynamic-enumeration",
        seen && joystick,
        format!("node={node} listed={seen} joystick={joystick}"),
    );

    // SIGTERM, as an application ends, then a kill if it has not gone within five seconds.
    if let Ok(pid) = libc::pid_t::try_from(hold.id()) {
        // SAFETY: a signal to the child this test started and has not yet reaped.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while hold.try_wait().ok().flatten().is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = hold.kill();
    let _ = hold.wait();
    std::thread::sleep(Duration::from_millis(200));
    let gone = scan(&scanner, &fake, &sockets);
    let still = gone.lines().any(|line| line.contains(&node));
    checks.check(
        "udev-node-vanishes-with-creator",
        !node.is_empty() && !still,
        format!("node={node} still_listed={still}"),
    );
    let _ = std::fs::remove_dir_all(&work);
    checks.finish("");
}
