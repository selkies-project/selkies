/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Pipe menus: a menu whose entries are what a command prints. The command runs on a thread of
//! its own, since the engine runs inside the compositor's handlers and may not wait on a
//! process, and its output is picked up on a later tick; while it runs the menu shows one
//! loading entry. The output is bounded in size and in time, since the command is the configuration's to name and a
//! runaway one must not grow the compositor's memory or leave a thread behind each time its menu
//! opens.

use std::io::{ErrorKind, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdout};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};

/// The most a pipe menu's output may be.
const MAX_OUTPUT: usize = 4 << 20;
/// How long a pipe menu's command may run before its output is given up on.
const TIMEOUT: Duration = Duration::from_secs(10);

/// One command running for a menu.
pub struct PipeJob {
    /// The menu the output fills.
    pub menu_id: String,
    receiver: Receiver<Result<String, String>>,
}

impl PipeJob {
    /// Start the command through `sh -c` with the launcher's environment.
    pub fn spawn(menu_id: String, command: String, env: Vec<(String, String)>) -> PipeJob {
        let (sender, receiver) = channel();
        std::thread::Builder::new()
            .name("wm-pipe-menu".to_string())
            .spawn(move || {
                let _ = sender.send(run(&command, &env, TIMEOUT));
            })
            .ok();
        PipeJob { menu_id, receiver }
    }

    /// The command's output once it is in, `None` while it still runs.
    pub fn poll(&self) -> Option<Result<String, String>> {
        match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err("the pipe menu's thread ended".to_string())),
        }
    }
}

/// Run `command` and return what it printed, within `timeout`.
///
/// The command leads a process group of its own and its output is read through `poll` against
/// the deadline, so neither a command that keeps its output open without writing nor a child it
/// left holding that output can hold the thread past it: at the deadline, or past the output
/// bound, the whole group is killed and the command reaped. A command that printed its menu and
/// closed its output but has not exited by the deadline is killed the same way, its menu kept.
fn run(command: &str, env: &[(String, String)], timeout: Duration) -> Result<String, String> {
    let mut child = std::process::Command::new("sh");
    child
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .process_group(0);
    for (key, value) in env {
        child.env(key, value);
    }
    let mut child = child.spawn().map_err(|e| format!("{command}: {e}"))?;
    let deadline = Instant::now() + timeout;
    let output = match child.stdout.take() {
        Some(stdout) => read_until(stdout, deadline, timeout),
        None => Err("no stdout".to_string()),
    };
    if output.is_err() || !exits_by(&mut child, deadline) {
        kill_group(&child);
        let _ = child.wait();
    }
    let output = output.map_err(|e| format!("{command}: {e}"))?;
    String::from_utf8(output).map_err(|e| format!("{command}: {e}"))
}

/// Read `stdout` to its end, failing at `deadline` or past [`MAX_OUTPUT`].
fn read_until(mut stdout: ChildStdout, deadline: Instant, timeout: Duration) -> Result<Vec<u8>, String> {
    let mut pfd = libc::pollfd {
        fd: stdout.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let mut output = Vec::new();
    let mut buf = [0u8; 65536];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(format!("not done within {timeout:?}"));
        }
        let ms = left.as_millis().clamp(1, i32::MAX as u128) as i32;
        // SAFETY: pfd is one initialized pollfd that outlives the call.
        let ready = unsafe { libc::poll(&mut pfd, 1, ms) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(error.to_string());
        }
        if ready == 0 {
            continue;
        }
        match stdout.read(&mut buf) {
            Ok(0) => return Ok(output),
            Ok(n) => {
                if output.len() + n > MAX_OUTPUT {
                    return Err(format!("output over {MAX_OUTPUT} bytes"));
                }
                output.extend_from_slice(&buf[..n]);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// Whether `child` exits by `deadline`.
fn exits_by(child: &mut Child, deadline: Instant) -> bool {
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => return false,
        }
    }
}

/// Kill the process group `child` leads, the command and everything it started.
fn kill_group(child: &Child) {
    let Ok(group) = i32::try_from(child.id()) else {
        return;
    };
    // SAFETY: kill takes no pointers; the group is named by the child's pid, which is not reused
    // before the child is reaped.
    unsafe {
        libc::kill(-group, libc::SIGKILL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pipe menu's command runs off the calling thread and its output arrives on a poll.
    #[test]
    fn a_pipe_menu_runs_off_thread() {
        let job = PipeJob::spawn(
            "pipe".to_string(),
            "printf '<openbox_pipe_menu><item label=\"x\"/></openbox_pipe_menu>'".to_string(),
            Vec::new(),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = job.poll() {
                assert!(result.unwrap().contains("openbox_pipe_menu"));
                break;
            }
            assert!(std::time::Instant::now() < deadline, "the command did not answer");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A command that keeps its output open without writing is given up on at the deadline,
    /// and what it started in the background is killed with it.
    #[test]
    fn a_pipe_menu_that_never_finishes_is_killed_at_its_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let command = format!("sleep 30 & echo $! > '{}'; wait", pid_file.display());
        let started = Instant::now();
        let result = run(&command, &[], Duration::from_millis(300));
        assert!(result.is_err(), "the command was not given up on: {result:?}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the deadline did not hold"
        );
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let pid: u32 = pid.trim().parse().unwrap();
        let stat = format!("/proc/{pid}/stat");
        let deadline = Instant::now() + Duration::from_secs(2);
        while std::fs::read_to_string(&stat).is_ok_and(|s| !s.contains(") Z ")) {
            assert!(
                Instant::now() < deadline,
                "the background child outlived its command"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
