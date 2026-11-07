/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Checks of what a process reads once and for good (its environment, the installed settings, the
//! one logger), or that act on every child of the process, run through a process of their own: the
//! test binary re-executes one test, which stays ignored in an ordinary run and does nothing unless
//! it was started this way.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const ISOLATED: &str = "SELKIES_TEST_ISOLATED";

/// Whether this is the process `run_isolated` started.
pub(crate) fn isolated() -> bool {
    std::env::var_os(ISOLATED).is_some()
}

/// Run one ignored test of this binary in a process of its own, over `env` on this one's, and
/// fail with its output when it fails.
pub(crate) fn run_isolated(test: &str, env: &[(&str, &str)]) {
    let output = Command::new(std::env::current_exe().expect("the test binary"))
        .args([
            "--exact",
            test,
            "--include-ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ISOLATED, "1")
        .envs(env.iter().copied())
        .output()
        .expect("the test binary runs");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && text.contains("1 passed"),
        "{test} ended {:?}:\n{text}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A private directory, removed with this value.
pub(crate) struct Scratch(pub PathBuf);

impl Scratch {
    pub(crate) fn new(tag: &str) -> Scratch {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after the epoch")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("selkies-{tag}-{}-{unique}", std::process::id()));
        std::fs::create_dir(&dir).expect("scratch directory");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("private");
        Scratch(dir)
    }

    pub(crate) fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    pub(crate) fn text(&self) -> String {
        self.0.display().to_string()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Write an executable shell script.
pub(crate) fn script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).expect("script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("executable");
}

/// A child that ends with this value, by the process group it leads.
pub(crate) struct Owned(pub Child);

impl Owned {
    /// `command` in a process group of its own.
    pub(crate) fn spawn(command: &mut Command) -> Owned {
        use std::os::unix::process::CommandExt;
        Owned(
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
                .expect("the process starts"),
        )
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        let group = i32::try_from(self.0.id()).expect("a pid fits");
        // SAFETY: kill has no memory preconditions, and the group is the one this value started.
        unsafe { libc::kill(-group, libc::SIGKILL) };
        let _ = self.0.wait();
    }
}

/// What `named_process` runs: sleeps, and ends on SIGHUP as the daemons it stands in for reload on
/// one.
#[test]
#[ignore = "a stand-in process named_process starts"]
fn stand_in() {
    if isolated() {
        std::thread::sleep(Duration::from_secs(120));
    }
}

/// A sleeping child the kernel names `name`, as `pgrep -x` then finds that daemon or session: this
/// test binary through a link of that name, running `stand_in`. Only `env` reaches it, beside what
/// the binary needs to load and run that test.
pub(crate) fn named_process(dir: &Path, name: &str, env: &[(&str, &str)]) -> Child {
    let link = dir.join(name);
    if !link.exists() {
        std::os::unix::fs::symlink(std::env::current_exe().expect("the test binary"), &link)
            .expect("a link");
    }
    let child = Command::new(&link)
        .args([
            "--exact",
            "session_screens::testing::stand_in",
            "--ignored",
            "--test-threads=1",
        ])
        .env_clear()
        .env(ISOLATED, "1")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env(
            "LD_LIBRARY_PATH",
            std::env::var_os("LD_LIBRARY_PATH").unwrap_or_default(),
        )
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the stand-in starts");
    let deadline = Instant::now() + Duration::from_secs(5);
    let comm: String = name.chars().take(15).collect();
    while std::fs::read_to_string(format!("/proc/{}/comm", child.id()))
        .map_or(true, |c| c.trim_end() != comm)
    {
        assert!(Instant::now() < deadline, "{name} never ran");
        std::thread::sleep(Duration::from_millis(10));
    }
    child
}

/// A throwaway Xvfb on a display nothing else holds, and the display's name.
pub(crate) fn private_x_server(width: i32, height: i32) -> (Owned, String) {
    use std::io::BufRead;
    use std::os::unix::process::CommandExt;
    let mut child = Command::new("Xvfb")
        .args([
            "-displayfd",
            "1",
            "-screen",
            "0",
            &format!("{width}x{height}x24"),
            "-nolisten",
            "tcp",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("Xvfb starts");
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take().expect("piped"))
        .read_line(&mut line)
        .expect("Xvfb names its display");
    let display = format!(":{}", line.trim());
    (Owned(child), display)
}

/// A headless labwc on a private runtime directory, with its Wayland socket.
pub(crate) struct PrivateLabwc {
    _process: Owned,
    pub runtime: Scratch,
}

impl PrivateLabwc {
    /// A labwc opening `outputs` headless screens.
    pub(crate) fn start(outputs: u32) -> PrivateLabwc {
        let runtime = Scratch::new("labwc");
        let process = Owned::spawn(
            Command::new("labwc")
                .env("XDG_RUNTIME_DIR", &runtime.0)
                .env("WLR_BACKENDS", "headless")
                .env("WLR_RENDERER", "pixman")
                .env("WLR_HEADLESS_OUTPUTS", outputs.to_string())
                .env("LIBSEAT_BACKEND", "noop")
                .env_remove("WAYLAND_DISPLAY")
                .env_remove("DISPLAY"),
        );
        let labwc = PrivateLabwc {
            _process: process,
            runtime,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !crate::clipboard::x11::socket_live(&labwc.runtime.path("wayland-0")) {
            assert!(Instant::now() < deadline, "labwc never listened");
            std::thread::sleep(Duration::from_millis(20));
        }
        labwc
    }

    /// The socket as pixelflux takes it, whatever this process's own runtime directory is.
    pub(crate) fn socket(&self) -> String {
        self.runtime.path("wayland-0").display().to_string()
    }
}
