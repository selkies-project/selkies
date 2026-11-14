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

/// A nested labwc on the compositor's socket, stopped when the test ends.
pub struct Labwc(Child);

impl Drop for Labwc {
    fn drop(&mut self) {
        // SAFETY: a signal to a child this test spawned and has not reaped.
        unsafe { libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if matches!(self.0.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `labwc` on PATH, nested on `socket` under `runtime` with wlroots' Wayland backend and the
/// pixman renderer, `env` added, its output in `runtime`/labwc.log.
pub fn labwc(runtime: &Scratch, socket: &str, env: &[(&str, &str)], args: &[&str]) -> Labwc {
    let log = std::fs::File::create(runtime.0.join("labwc.log")).expect("labwc's log");
    let mut command = Command::new("labwc");
    command
        .args(args)
        .env("WAYLAND_DISPLAY", socket)
        .env("WLR_BACKENDS", "wayland")
        .env("XDG_RUNTIME_DIR", &runtime.0)
        .env("WLR_RENDERER", "pixman")
        .env("LIBGL_ALWAYS_SOFTWARE", "1")
        .env_remove("DISPLAY")
        .stdout(log.try_clone().expect("labwc's log"))
        .stderr(log);
    for (name, value) in env {
        command.env(name, value);
    }
    Labwc(command.spawn().expect("labwc on PATH"))
}

/// The nested compositor's own socket under `runtime`: the one that is not `capture`.
pub fn inner_socket(runtime: &Scratch, capture: &str) -> Option<String> {
    let mut names: Vec<String> = std::fs::read_dir(&runtime.0)
        .ok()?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.starts_with("wayland-") && !name.ends_with(".lock") && name != capture)
        .collect();
    names.sort();
    names.into_iter().next()
}

/// The last lines of `runtime`/labwc.log, for a failure's detail.
pub fn labwc_log(runtime: &Scratch) -> String {
    let text = std::fs::read_to_string(runtime.0.join("labwc.log")).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(15)..].join("\n")
}
