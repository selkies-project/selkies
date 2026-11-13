/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A throwaway Xvfb of a test's own, on a display nothing else holds.
//!
//! The number is asked for rather than assumed: candidates start well above those desktop
//! sessions and container entrypoints take, a number another server takes first is simply the
//! next candidate, and a display that answers is taken only when its lock names this server, since
//! two suites starting together try the same number. GLX is off because it faults on some GPU
//! hosts and no suite that wants a private server needs it. The server is asked to quit rather
//! than killed, because a killed X server leaves its lock and socket behind and the number stays
//! taken for as long as the machine is up.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::server::tie_to_this_thread;

/// A running private X server. Dropping it stops the server and frees its display number.
pub struct Xvfb {
    child: Child,
    display: String,
}

/// The lock file and the socket of display number `number`.
fn traces(number: u32) -> [PathBuf; 2] {
    [
        PathBuf::from(format!("/tmp/.X{number}-lock")),
        PathBuf::from(format!("/tmp/.X11-unix/X{number}")),
    ]
}

/// The process id the lock of display number `number` names.
fn lock_holder(number: u32) -> Option<u32> {
    std::fs::read_to_string(&traces(number)[0])
        .ok()?
        .trim()
        .parse()
        .ok()
}

impl Xvfb {
    /// Start an Xvfb with a `width`x`height` screen and `extra` arguments appended. Xvfb fixes its
    /// largest screen at this size.
    ///
    /// # Panics
    ///
    /// When Xvfb is missing or none of eight candidate displays came up.
    pub fn start(width: u32, height: u32, extra: &[&str]) -> Xvfb {
        let free = (64..256).filter(|number| traces(*number).iter().all(|trace| !trace.exists()));
        for number in free.take(8) {
            let display = format!(":{number}");
            let mut command = Command::new("Xvfb");
            command
                .arg(&display)
                .args(["-screen", "0", &format!("{width}x{height}x24")])
                .args(["-extension", "GLX", "-nolisten", "tcp", "-ac", "-noreset"])
                .args(extra)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            tie_to_this_thread(&mut command);
            let mut child = command
                .spawn()
                .unwrap_or_else(|e| panic!("Xvfb did not start: {e}; a private X server needs it"));
            let deadline = Instant::now() + Duration::from_secs(15);
            while Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
                if crate::x11::root_size(&display).is_some() {
                    if lock_holder(number) == Some(child.id()) {
                        return Xvfb { child, display };
                    }
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        panic!("no display number yielded a working X server");
    }

    /// The display's name, `:N`.
    pub fn display(&self) -> &str {
        &self.display
    }
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        if let Ok(pid) = libc::pid_t::try_from(self.child.id()) {
            // SAFETY: a signal to a process this test started; no memory is involved.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && matches!(self.child.try_wait(), Ok(None)) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Only what the server just gone left behind: a number already reissued is another's.
        if let Ok(number) = self.display.trim_start_matches(':').parse::<u32>()
            && lock_holder(number) == Some(self.child.id())
        {
            for trace in traces(number) {
                let _ = std::fs::remove_file(trace);
            }
        }
    }
}
