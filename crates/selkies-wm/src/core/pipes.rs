/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Pipe menus: a menu whose entries are what a command prints. The command runs on a thread of
//! its own, since the engine runs inside the compositor's handlers and may not wait on a
//! process, and its output is picked up on a later tick; while it runs the menu shows one
//! loading entry. The output is bounded, since the command is the configuration's to name and a
//! runaway one must not grow the compositor's memory.

use std::io::Read;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::Duration;

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
                let _ = sender.send(run(&command, &env));
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

fn run(command: &str, env: &[(String, String)]) -> Result<String, String> {
    let mut child = std::process::Command::new("sh");
    child
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    for (key, value) in env {
        child.env(key, value);
    }
    let mut child = child.spawn().map_err(|e| format!("{command}: {e}"))?;
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let started = std::time::Instant::now();
    let mut output = Vec::new();
    let mut buf = [0u8; 65536];
    loop {
        match stdout.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if output.len() + n > MAX_OUTPUT {
                    let _ = child.kill();
                    return Err(format!("{command}: output over {MAX_OUTPUT} bytes"));
                }
                output.extend_from_slice(&buf[..n]);
            }
            Err(e) => {
                let _ = child.kill();
                return Err(format!("{command}: {e}"));
            }
        }
        if started.elapsed() > TIMEOUT {
            let _ = child.kill();
            return Err(format!("{command}: no output within {TIMEOUT:?}"));
        }
    }
    let _ = child.wait();
    String::from_utf8(output).map_err(|e| format!("{command}: {e}"))
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
}
