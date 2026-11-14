/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What the capture tests share: a private runtime directory, a throwaway X server, and the
//! checks' outcomes. Each test binary that starts the in-process compositor is one of its own,
//! the compositor being one per process.

#![allow(dead_code)]

use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// A directory removed when the test ends.
pub struct Scratch(pub PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A private XDG_RUNTIME_DIR for the compositor's socket, named for the test.
pub fn runtime_dir(name: &str) -> Scratch {
    let dir =
        Scratch(std::env::temp_dir().join(format!("pixelflux-{name}-{}", std::process::id())));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir.0)
        .expect("a private runtime directory");
    dir
}

/// A throwaway X server, stopped when the test ends.
pub struct Xvfb(Child);

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `Xvfb` on PATH, on a free display number, at `width`x`height`, and its display name.
pub fn xvfb(width: i32, height: i32) -> (Xvfb, String) {
    let number = (140..200)
        .find(|n| {
            !PathBuf::from(format!("/tmp/.X11-unix/X{n}")).exists()
                && !PathBuf::from(format!("/tmp/.X{n}-lock")).exists()
        })
        .expect("a free display number");
    let display = format!(":{number}");
    let child = Command::new("Xvfb")
        .args([
            display.as_str(),
            "-screen",
            "0",
            &format!("{width}x{height}x24"),
            "-nolisten",
            "tcp",
        ])
        .args([
            "+extension",
            "DAMAGE",
            "+extension",
            "XFIXES",
            "+extension",
            "MIT-SHM",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("Xvfb on PATH");
    let server = Xvfb(child);
    let socket = PathBuf::from(format!("/tmp/.X11-unix/X{number}"));
    let deadline = Instant::now() + Duration::from_secs(15);
    while !socket.exists() {
        assert!(Instant::now() < deadline, "Xvfb {display} never listened");
        thread::sleep(Duration::from_millis(100));
    }
    (server, display)
}

/// Points the process at `runtime` and, where given, at `display`, with no other session's
/// variables steering the capture.
///
/// # Safety
/// Called before the test starts a thread, a capture, or the compositor.
pub unsafe fn isolate(runtime: &Scratch, display: Option<&str>) {
    // SAFETY: the caller holds that nothing else reads the environment yet.
    unsafe {
        std::env::set_var("XDG_RUNTIME_DIR", &runtime.0);
        match display {
            Some(display) => std::env::set_var("DISPLAY", display),
            None => std::env::remove_var("DISPLAY"),
        }
        for name in ["WAYLAND_DISPLAY", "PIXELFLUX_CU", "PIXELFLUX_RECORD"] {
            std::env::remove_var(name);
        }
    }
}

/// Every check's outcome, printed as it is made, so one failure does not hide the rest.
#[derive(Default)]
pub struct Results(pub Vec<String>);

impl Results {
    pub fn check(&mut self, label: &str, ok: bool, detail: impl std::fmt::Debug) {
        println!("{}  {label}  {detail:?}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            self.0.push(label.to_string());
        }
    }

    /// Fails the test with every failed check's label.
    pub fn finish(self) {
        assert!(self.0.is_empty(), "failed: {:?}", self.0);
    }
}

/// Waits up to `timeout` for `ready`, polling.
pub fn wait_until(timeout: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while !ready() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
    true
}
