/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What the window manager starts and keeps running: the programs its bindings and menus name,
//! the session bus they share, and the desktop shell under `SELKIES_DESKTOP`.
//!
//! Every child gets one environment, [`SessionEnv`]: the display sockets the backend knows
//! (`WAYLAND_DISPLAY`, `DISPLAY`), the session bus, and the session type, so a program started
//! from a menu finds the same desktop the shell draws. The bus is `dbus-daemon --session` started
//! here when the environment names none, since the images' services start the shell under
//! `dbus-launch` and the applications it launches inherit that bus; a bus already in the
//! environment is kept.
//!
//! The shell is a child process on purpose: its file manager decodes images from the user's home
//! and scans directories, untrusted-file parsing that does not belong beside the stream, and a
//! shell crash is a restart rather than a lost stream. [`ShellSupervisor`] restarts it with a
//! backoff and stops after repeated immediate exits, so a shell that cannot start does not loop.
//! The shell is also tied to its supervisor with the parent-death signal, so a server that is
//! killed takes its shell down instead of leaving one drawing beside the shell the next server
//! starts; the programs the user launches are left running, as they are under any manager.
//!
//! An `autostart` file is run once the manager is up, as openbox runs its own: the first
//! one found along the configuration directories ([`crate::config::search_dirs`],
//! the user's over the system's), executed with `sh` in the session environment.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config;
use crate::core::Launcher;

/// The environment the session's children run in.
#[derive(Clone, Debug, Default)]
pub struct SessionEnv {
    vars: HashMap<String, String>,
}

impl SessionEnv {
    /// The environment for a backend: the process's own with the display variables set as given.
    pub fn new(wayland_display: Option<&str>, x11_display: Option<&str>) -> SessionEnv {
        let mut vars: HashMap<String, String> = std::env::vars().collect();
        match wayland_display {
            Some(name) => {
                vars.insert("WAYLAND_DISPLAY".to_string(), name.to_string());
                vars.insert("XDG_SESSION_TYPE".to_string(), "wayland".to_string());
            }
            None => {
                vars.remove("WAYLAND_DISPLAY");
                vars.insert("XDG_SESSION_TYPE".to_string(), "x11".to_string());
            }
        }
        match x11_display {
            Some(name) => {
                vars.insert("DISPLAY".to_string(), name.to_string());
            }
            None => {
                vars.remove("DISPLAY");
            }
        }
        vars.insert("XDG_CURRENT_DESKTOP".to_string(), "selkies".to_string());
        if let Some(home) = vars.get("HOME").cloned() {
            let local = format!("{home}/.local/bin");
            let path = vars.get("PATH").cloned().unwrap_or_default();
            if !path.split(':').any(|dir| dir == local) {
                vars.insert(
                    "PATH".to_string(),
                    if path.is_empty() {
                        local
                    } else {
                        format!("{local}:{path}")
                    },
                );
            }
        }
        SessionEnv { vars }
    }

    /// Set a variable.
    pub fn set(&mut self, key: &str, value: &str) {
        self.vars.insert(key.to_string(), value.to_string());
    }

    /// A variable's value.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str)
    }

    /// Every variable, for a child's environment.
    pub fn vars(&self) -> Vec<(String, String)> {
        self.vars.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    /// A command with this environment, in the user's home.
    pub fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command.env_clear();
        command.envs(self.vars.iter());
        if let Some(home) = self.vars.get("HOME") {
            command.current_dir(home);
        }
        command
    }
}

/// Make sure the environment names a session bus, starting one when it does not; the daemon
/// is kept as long as the handle lives.
pub struct SessionBus {
    child: Option<Child>,
    pub address: String,
}

impl SessionBus {
    /// The bus in `env`, or a new `dbus-daemon --session` whose address is put there.
    pub fn ensure(env: &mut SessionEnv) -> Option<SessionBus> {
        if let Some(address) = env.get("DBUS_SESSION_BUS_ADDRESS").filter(|a| !a.is_empty()) {
            return Some(SessionBus {
                child: None,
                address: address.to_string(),
            });
        }
        let mut command = env.command("dbus-daemon");
        command
            .args(["--session", "--nofork", "--nopidfile", "--print-address"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                log::warn!(target: "wm", "no session bus: dbus-daemon did not start: {e}");
                return None;
            }
        };
        let stdout = child.stdout.take()?;
        let mut line = String::new();
        if BufReader::new(stdout).read_line(&mut line).is_err() || line.trim().is_empty() {
            log::warn!(target: "wm", "no session bus: dbus-daemon printed no address");
            let _ = child.kill();
            return None;
        }
        let address = line.trim().to_string();
        env.set("DBUS_SESSION_BUS_ADDRESS", &address);
        log::info!(target: "wm", "session bus at {address}");
        Some(SessionBus {
            child: Some(child),
            address,
        })
    }
}

impl Drop for SessionBus {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Start `program` with `args` in `env`, detached from the caller: its exit is reaped on a
/// thread of its own and its output goes to the log's stream.
fn spawn_detached(env: &SessionEnv, program: &str, args: &[&str], what: &str) {
    let mut child = env.command(program);
    child
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    match child.spawn() {
        Ok(mut child) => {
            log::info!(target: "wm", "started: {what}");
            std::thread::Builder::new()
                .name("wm-reap".to_string())
                .spawn(move || {
                    let _ = child.wait();
                })
                .ok();
        }
        Err(e) => log::warn!(target: "wm", "{what}: not started: {e}"),
    }
}

/// Run the `autostart` file where one exists: the first along the configuration directories,
/// with `sh`. Returns the file run.
pub fn run_autostart(env: &SessionEnv) -> Option<PathBuf> {
    let dirs = config::search_dirs();
    let file = config::find_file(&dirs, "autostart").or_else(|| config::find_file(&dirs, "autostart.sh"))?;
    let path = file.to_string_lossy().into_owned();
    spawn_detached(env, "sh", &[&path], &format!("autostart {path}"));
    Some(file)
}

/// The launcher the engine runs `Execute` actions and pipe menus through: `sh -c` in the
/// session environment, detached, output to the log's stream.
pub struct ShellLauncher {
    env: Arc<Mutex<SessionEnv>>,
}

impl ShellLauncher {
    /// A launcher over a shared environment, so the X display the compositor brings up later
    /// reaches programs started after it.
    pub fn new(env: Arc<Mutex<SessionEnv>>) -> ShellLauncher {
        ShellLauncher { env }
    }
}

impl Launcher for ShellLauncher {
    fn spawn(&mut self, command: &str) {
        let env = self.env.lock().unwrap_or_else(|p| p.into_inner()).clone();
        spawn_detached(&env, "sh", &["-c", command], command);
    }

    fn env(&self) -> Vec<(String, String)> {
        self.env.lock().unwrap_or_else(|p| p.into_inner()).vars()
    }

    fn set_x11_display(&mut self, display: u32) {
        self.env
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .set("DISPLAY", &format!(":{display}"));
    }
}

/// How long a shell has to stay up before an exit counts as a crash to back off from rather
/// than a failure to start.
const STABLE: Duration = Duration::from_secs(10);
/// Immediate exits in a row after which the shell is given up on.
const GIVE_UP: u32 = 5;

/// The desktop shell as a supervised child: started, restarted when it exits, stopped with the
/// manager.
pub struct ShellSupervisor {
    env: Arc<Mutex<SessionEnv>>,
    program: String,
    child: Option<Child>,
    started: Option<Instant>,
    failures: u32,
    next_start: Instant,
    given_up: bool,
}

impl ShellSupervisor {
    /// A supervisor for `program` (the shell's binary name or path), not started yet.
    pub fn new(env: Arc<Mutex<SessionEnv>>, program: &str) -> ShellSupervisor {
        ShellSupervisor {
            env,
            program: program.to_string(),
            child: None,
            started: None,
            failures: 0,
            next_start: Instant::now(),
            given_up: false,
        }
    }

    /// Start the shell if it is not running and its backoff passed, and reap it when it exited.
    /// Called on the manager's ticks; returns when it next wants to be called.
    pub fn poll(&mut self, now: Instant) -> Instant {
        if self.given_up {
            return now + Duration::from_secs(3600);
        }
        if let Some(child) = self.child.as_mut() {
            match child.try_wait() {
                Ok(None) => return now + Duration::from_secs(1),
                Ok(Some(status)) => {
                    let ran = self
                        .started
                        .map(|s| now.saturating_duration_since(s))
                        .unwrap_or_default();
                    self.child = None;
                    if ran < STABLE {
                        self.failures += 1;
                    } else {
                        self.failures = 0;
                    }
                    let delay = Duration::from_secs(1 << self.failures.min(4));
                    log::warn!(target: "wm", "{} exited ({status}) after {ran:?}; starting again in {delay:?}", self.program);
                    if self.failures >= GIVE_UP {
                        log::error!(target: "wm", "{} exited {GIVE_UP} times right after starting; not started again", self.program);
                        self.given_up = true;
                        return now + Duration::from_secs(3600);
                    }
                    self.next_start = now + delay;
                    return self.next_start;
                }
                Err(e) => {
                    log::warn!(target: "wm", "{}: {e}", self.program);
                    self.child = None;
                }
            }
        }
        if now < self.next_start {
            return self.next_start;
        }
        let env = self.env.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let mut command = env.command(&self.program);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        // SAFETY: the hook runs in the forked child before exec and only calls prctl, which is
        // async-signal-safe and touches no memory shared with the parent.
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        match command.spawn() {
            Ok(child) => {
                log::info!(target: "wm", "{} started on {}", self.program, env.get("WAYLAND_DISPLAY").or(env.get("DISPLAY")).unwrap_or("no display"));
                self.child = Some(child);
                self.started = Some(now);
            }
            Err(e) => {
                self.failures += 1;
                log::warn!(target: "wm", "{}: not started: {e}", self.program);
                if self.failures >= GIVE_UP {
                    self.given_up = true;
                }
                self.next_start = now + Duration::from_secs(1 << self.failures.min(4));
                return self.next_start;
            }
        }
        now + Duration::from_secs(1)
    }

    /// Whether the shell runs now.
    pub fn running(&self) -> bool {
        self.child.is_some()
    }
}

impl Drop for ShellSupervisor {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The environment names the displays it is given and drops the ones it is not.
    #[test]
    fn the_environment_names_the_displays() {
        let env = SessionEnv::new(Some("wayland-1"), Some(":2"));
        assert_eq!(env.get("WAYLAND_DISPLAY"), Some("wayland-1"));
        assert_eq!(env.get("DISPLAY"), Some(":2"));
        assert_eq!(env.get("XDG_SESSION_TYPE"), Some("wayland"));
        let x11 = SessionEnv::new(None, Some(":20"));
        assert_eq!(x11.get("WAYLAND_DISPLAY"), None);
        assert_eq!(x11.get("XDG_SESSION_TYPE"), Some("x11"));
    }

    /// A shell that exits at once is restarted with a growing delay and given up on after a
    /// few tries.
    #[test]
    fn a_shell_that_exits_at_once_is_given_up_on() {
        let env = Arc::new(Mutex::new(SessionEnv::new(None, None)));
        let mut supervisor = ShellSupervisor::new(env, "false");
        let mut now = Instant::now();
        let mut starts = 0;
        for _ in 0..100 {
            let next = supervisor.poll(now);
            if supervisor.running() {
                starts += 1;
                let _ = supervisor.child.as_mut().unwrap().wait();
            }
            if supervisor.given_up {
                break;
            }
            now = next.max(now + Duration::from_millis(1));
        }
        assert!(supervisor.given_up);
        assert_eq!(starts, GIVE_UP);
    }
}
