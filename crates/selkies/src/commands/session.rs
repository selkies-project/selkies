/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A desktop session and Selkies streaming it, from one command.
//!
//! For platforms that start one process behind a port or a Unix socket: Jupyter's server
//! proxy, an Open OnDemand job, a Coder workspace. It starts what the host lacks, a sound
//! server unless one answers and an Xvfb on the X11 backend, its screen started at the images'
//! size inside a framebuffer resizes can grow it to, then the desktop and Selkies with every
//! argument it was given. The backend is Selkies' own `SELKIES_WAYLAND` or `--wayland`, read
//! by Selkies' parser ([`selkies_settings`]). The desktop is found as a display manager finds
//! one ([`super::sessions`]); nothing is assumed of the host's Xvfb or compositor beyond what
//! every release of them offers.
//!
//! The children share a process group the launcher is not in, so stopping ends every process
//! they started while a signal meant for the launcher's caller, Ctrl-C in a terminal or a
//! scheduler's for the job, still reaches the launcher and stops them in order. What they
//! orphan, a desktop's agents and every application a menu double-forks, the launcher adopts
//! ([`adopt_orphans`]), collects once it exits, and ends with the session.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, BufRead, BufReader, Read};
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, kill, killpg, sigaction};
use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid, waitpid};
use nix::unistd::{Pid, getpgid, setpgid};
use rand::Rng;
use regex::Regex;
use serde_json::{Value as Json, json};

use super::sessions::{Desktops, Entry, which};
use crate::settings::Settings;

/// Xvfb's arguments: the framebuffer bounds the sizes Selkies can resize the screen to, and
/// `-s 0 -dpms` keeps the server from blanking what is streamed.
pub const XVFB_ARGS: &[&str] = &[
    "-screen",
    "0",
    "8192x4096x24",
    "-nolisten",
    "tcp",
    "-noreset",
    "-s",
    "0",
    "-dpms",
];
/// The screen inside the framebuffer until a client sizes it, as the images start theirs: a
/// desktop laid out or a capture started first would take all of the framebuffer.
pub const START_SIZE: (u32, u32) = (1920, 1080);
/// Where every X server puts its socket; a host's boot creates it.
pub const X11_SOCKET_DIR: &str = "/tmp/.X11-unix";
/// What a host session leaves in the environment that must not reach this one.
pub const HOST_SESSION_VARS: &[&str] = &[
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "DBUS_SESSION_BUS_ADDRESS",
    "PIPEWIRE_RUNTIME_DIR",
    "PULSE_RUNTIME_PATH",
];

/// A process environment.
pub type Env = BTreeMap<String, String>;

/// One line of the launcher's own log, on stderr beside its children's.
pub fn log(message: &str) {
    eprintln!("[selkies-session] {message}");
}

/// Whether the Unix socket at `path` accepts a connection.
pub fn answers(path: &Path) -> bool {
    UnixStream::connect(path).is_ok()
}

/// Whether `child` has exited, and with what status (`None` while it runs).
fn exited(child: &mut Child) -> Option<i32> {
    child
        .try_wait()
        .ok()
        .flatten()
        .map(|status| status.code().unwrap_or(1))
}

/// Poll `predicate` until it holds; fail once `child` exits or `seconds` pass.
fn wait_for(
    mut predicate: impl FnMut() -> bool,
    seconds: u64,
    what: &str,
    mut child: Option<&mut Child>,
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !predicate() {
        if let Some(status) = child.as_deref_mut().and_then(exited) {
            return Err(format!("{what} exited with status {status}"));
        }
        if Instant::now() > deadline {
            return Err(format!("{what} did not come up within {seconds}s"));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

/// The directory X servers put their sockets in, world-writable and sticky as a host's boot
/// makes it. An Xvfb creates it itself; the XWayland that kwin_wayland starts does not, and
/// without it Plasma's Wayland session never starts its shell.
pub fn x11_socket_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    match fs::create_dir(path) {
        Ok(()) => {
            if let Err(err) = fs::set_permissions(path, fs::Permissions::from_mode(0o1777)) {
                log(&format!(
                    "cannot make {} world-writable ({err})",
                    path.display()
                ));
            }
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(err) => log(&format!(
            "cannot create {} ({err}); the session's X11 applications will not start",
            path.display()
        )),
    }
}

/// Selkies' environment: the session's, with the libraries `preload` (`SELKIES_PRELOAD`)
/// names preloaded into Selkies alone, as a packaging that carries its own copies needs for
/// one the platform's drivers link against.
pub fn for_selkies(env: &Env, preload: &str) -> Env {
    if preload.is_empty() {
        return env.clone();
    }
    let mut out = env.clone();
    let joined: Vec<&str> = [
        preload,
        env.get("LD_PRELOAD").map(String::as_str).unwrap_or(""),
    ]
    .into_iter()
    .filter(|p| !p.is_empty())
    .collect();
    out.insert("LD_PRELOAD".to_string(), joined.join(":"));
    out
}

/// Selkies' settings for `args` and the environment, as its own parser reads them.
pub fn selkies_settings(args: &[String]) -> Settings {
    Settings::parse(args, &|name| std::env::var(name).ok())
}

/// One `FamilyWild` entry of an X authority file: the display number is the server's to pick,
/// and a wildcard cookie matches it on any display as libXau does.
pub fn xauth_wildcard_entry(cookie: &[u8; 16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(44);
    out.extend_from_slice(&0xFFFFu16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&18u16.to_be_bytes());
    out.extend_from_slice(b"MIT-MAGIC-COOKIE-1");
    out.extend_from_slice(&16u16.to_be_bytes());
    out.extend_from_slice(cookie);
    out
}

/// The command `selkies resize WxH` as the launcher runs it on `env`, its stdout, or `None`
/// when it did not answer in time.
pub fn run_resize(argv: &[String], env: &Env) -> Option<String> {
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while exited(&mut child).is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    Some(out)
}

/// The processes of one session, in a runtime directory of its own.
pub struct Session {
    pub runtime_dir: PathBuf,
    pub env: Env,
    children: Vec<Child>,
    groups: BTreeSet<i32>,
    /// Index into `children` of the Selkies server, once started.
    selkies: Option<usize>,
}

impl Session {
    /// A session with a fresh runtime directory under the environment's, where that exists
    /// and is writable, else the system's temporary directory.
    pub fn new() -> io::Result<Session> {
        let base = std::env::var("XDG_RUNTIME_DIR").ok().map(PathBuf::from);
        Self::new_in(base.as_deref(), std::env::vars().collect())
    }

    /// A session whose runtime directory is made under `base`, with `env` as the launcher's
    /// environment.
    pub fn new_in(base: Option<&Path>, mut env: Env) -> io::Result<Session> {
        let usable = base.filter(|b| {
            b.is_dir()
                && tempfile::Builder::new()
                    .prefix(".selkies-probe-")
                    .tempfile_in(b)
                    .is_ok()
        });
        let runtime_dir = match usable {
            Some(base) => tempfile::Builder::new()
                .prefix("selkies-session-")
                .tempdir_in(base),
            None => tempfile::Builder::new()
                .prefix("selkies-session-")
                .tempdir(),
        }?
        .keep();
        for var in HOST_SESSION_VARS {
            env.remove(*var);
        }
        env.insert(
            "XDG_RUNTIME_DIR".to_string(),
            runtime_dir.to_string_lossy().into_owned(),
        );
        Ok(Session {
            runtime_dir,
            env,
            children: Vec::new(),
            groups: BTreeSet::new(),
            selkies: None,
        })
    }

    /// Start `argv` in the session's process group, founding it with the first child; the
    /// result indexes `children`. `inherit` is a descriptor the child keeps open past exec.
    fn spawn(
        &mut self,
        argv: &[String],
        env: Option<&Env>,
        inherit: Option<&OwnedFd>,
    ) -> io::Result<usize> {
        let group = self.groups.iter().next().copied().unwrap_or(0);
        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .env_clear()
            .envs(env.unwrap_or(&self.env))
            .stdin(Stdio::null());
        let keep = inherit.map(std::os::fd::AsRawFd::as_raw_fd);
        unsafe {
            command.pre_exec(move || {
                if setpgid(Pid::from_raw(0), Pid::from_raw(group)).is_err() {
                    setpgid(Pid::from_raw(0), Pid::from_raw(0))?;
                }
                if let Some(fd) = keep {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags >= 0 {
                        libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
                    }
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        if let Ok(pgid) = getpgid(Some(Pid::from_raw(child.id() as i32))) {
            self.groups.insert(pgid.as_raw());
        }
        self.children.push(child);
        Ok(self.children.len() - 1)
    }

    /// A sound server Selkies can capture: the one the environment names or runs, else the
    /// first of PipeWire and PulseAudio that comes up here. A named socket that nothing
    /// serves, as an image's own init would, is passed over.
    pub fn sound(&mut self) -> bool {
        let named = self.env.get("PULSE_SERVER").cloned().unwrap_or_default();
        let socket_path = named
            .strip_prefix("unix:")
            .map(str::to_string)
            .or_else(|| named.starts_with('/').then(|| named.clone()))
            .unwrap_or_default();
        if !named.is_empty() && (socket_path.is_empty() || answers(Path::new(&socket_path))) {
            return true;
        }
        let running = std::env::var("PULSE_RUNTIME_PATH")
            .ok()
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(
                    &std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/nonexistent".into()),
                )
                .join("pulse")
            });
        let native = running.join("native");
        if answers(&native) {
            self.env.insert(
                "PULSE_SERVER".to_string(),
                format!("unix:{}", native.display()),
            );
            return true;
        }
        let own = self.runtime_dir.join("pulse/native");
        let manager = ["wireplumber", "pipewire-media-session"]
            .into_iter()
            .find(|n| which(n).is_some());
        let mut candidates: Vec<Vec<Vec<String>>> = Vec::new();
        if let Some(manager) = manager {
            candidates.push(vec![
                vec!["pipewire".into()],
                vec![manager.into()],
                vec!["pipewire-pulse".into()],
            ]);
        }
        candidates.push(vec![vec![
            "pulseaudio".into(),
            "--daemonize=no".into(),
            "--exit-idle-time=-1".into(),
        ]]);
        for servers in candidates {
            if !servers.iter().all(|argv| which(&argv[0]).is_some()) {
                continue;
            }
            let mut started = Vec::new();
            for argv in &servers {
                match self.spawn(argv, None, None) {
                    Ok(index) => started.push(index),
                    Err(err) => log(&format!("{} did not start: {err}", argv[0])),
                }
            }
            let Some(&last) = started.last() else {
                continue;
            };
            let waited = {
                let own = own.clone();
                let what = servers[servers.len() - 1][0].clone();
                let child = &mut self.children[last];
                wait_for(|| answers(&own), 20, &what, Some(child))
            };
            if let Err(err) = waited {
                log(&err);
                for index in started {
                    let _ = self.children[index].kill();
                }
                continue;
            }
            self.env.insert(
                "PULSE_SERVER".to_string(),
                format!("unix:{}", own.display()),
            );
            log(&format!("{} serves the session's audio", servers[0][0]));
            return true;
        }
        log("no sound server came up; the session has no audio");
        false
    }

    /// An Xvfb on the first free display that admits this session's cookie alone, rendering
    /// on the GPU through glamor where the server offers it.
    ///
    /// A server whose GLX cannot load (a vendor's GL stack beside a stock Xvfb) is started
    /// again without it, so the desktop comes up and only GL applications on X11 go without.
    pub fn x11(&mut self, render_dri: &str) -> Result<(), String> {
        if which("Xvfb").is_none() {
            return Err(
                "the X11 backend needs Xvfb; install it, or set SELKIES_WAYLAND=true".into(),
            );
        }
        let auth = self.runtime_dir.join("Xauthority");
        let mut cookie = [0u8; 16];
        rand::rng().fill_bytes(&mut cookie);
        fs::write(&auth, xauth_wildcard_entry(&cookie))
            .map_err(|e| format!("cannot write {}: {e}", auth.display()))?;
        let node = if render_dri.is_empty() {
            first_render_node()
        } else {
            render_dri.to_string()
        };
        let usage = Command::new("Xvfb")
            .arg("-help")
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout).into_owned()
                    + &String::from_utf8_lossy(&o.stderr)
            })
            .unwrap_or_default();
        let glx = vec!["+extension".to_string(), "GLX".to_string()];
        let mut attempts: Vec<Vec<String>> = Vec::new();
        if !node.is_empty() && usage.contains("-glamor") {
            let mut gpu = glx.clone();
            gpu.extend(["-glamor".to_string(), "-dri".to_string(), node.clone()]);
            attempts.push(gpu);
        }
        attempts.push(glx);
        attempts.push(vec!["-extension".to_string(), "GLX".to_string()]);
        let auth_text = auth.to_string_lossy().into_owned();
        for extra in attempts {
            let (read, write) = pipe()?;
            let mut argv = vec![
                "Xvfb".to_string(),
                "-displayfd".to_string(),
                std::os::fd::AsRawFd::as_raw_fd(&write).to_string(),
                "-auth".to_string(),
                auth_text.clone(),
            ];
            argv.extend(XVFB_ARGS.iter().map(|a| a.to_string()));
            argv.extend(extra.iter().cloned());
            let index = self
                .spawn(&argv, None, Some(&write))
                .map_err(|e| format!("Xvfb did not start: {e}"))?;
            drop(write);
            let number = read_line_within(read, Duration::from_secs(30));
            if let Some(number) = number.filter(|n| !n.is_empty()) {
                self.env.insert("DISPLAY".to_string(), format!(":{number}"));
                self.env.insert("XAUTHORITY".to_string(), auth_text.clone());
                let mut line = format!("Xvfb serves display :{number}");
                if extra.iter().any(|a| a == "-glamor") {
                    line.push_str(&format!(", rendering on {node}"));
                }
                if extra.iter().any(|a| a == "-extension") {
                    line.push_str(", without GLX, which it could not load");
                }
                log(&line);
                return Ok(());
            }
            let _ = self.children[index].kill();
        }
        Err("Xvfb did not come up".into())
    }

    /// Start the screen at the manual resolution where one is set, else at [`START_SIZE`],
    /// through `selkies resize` run by `resize`; a server that cannot be resized keeps its
    /// whole framebuffer, as before. Returns the line logged.
    pub fn size_screen(
        &self,
        settings: &Settings,
        resize: impl FnOnce(&[String], &Env) -> Option<String>,
    ) -> String {
        let mut size = START_SIZE;
        let manual = settings.manual_resolution().on;
        if manual && settings.manual_width() > 0 && settings.manual_height() > 0 {
            size = (
                settings.manual_width() as u32,
                settings.manual_height() as u32,
            );
        }
        let exe = std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "selkies".to_string());
        let argv = vec![exe, "resize".to_string(), format!("{}x{}", size.0, size.1)];
        let realized = resize(&argv, &self.env).and_then(|out| {
            let pattern = Regex::new(r"^\((\d+), (\d+)\)$").ok()?;
            let caps = pattern.captures(out.trim())?;
            Some((caps[1].to_string(), caps[2].to_string()))
        });
        let line = match realized {
            Some((w, h)) => format!("the screen starts at {w}x{h}"),
            None => "the screen could not be sized; it starts at the whole framebuffer".to_string(),
        };
        log(&line);
        line
    }

    /// The socket of Selkies' compositor, the first in the runtime directory.
    fn capture_socket(&mut self) -> Result<String, String> {
        let dir = self.runtime_dir.clone();
        let found = move || -> String {
            let Ok(entries) = fs::read_dir(&dir) else {
                return String::new();
            };
            let mut names: Vec<String> = entries
                .flatten()
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .collect();
            names.sort();
            names
                .into_iter()
                .find(|n| {
                    n.starts_with("wayland-") && !n.ends_with(".lock") && answers(&dir.join(n))
                })
                .unwrap_or_default()
        };
        let selkies = self.selkies.map(|i| &mut self.children[i]);
        wait_for(|| !found().is_empty(), 60, "Selkies' compositor", selkies)?;
        Ok(found())
    }

    /// The session as a display manager starts one, on a session bus of its own.
    pub fn desktop(&mut self, sid: &str, entry: &Entry, wayland: bool) -> Result<(), String> {
        let mut env = self.env.clone();
        env.insert(
            "XDG_SESSION_TYPE".to_string(),
            if wayland { "wayland" } else { "x11" }.to_string(),
        );
        if wayland {
            let socket = self.capture_socket()?;
            env.insert("WAYLAND_DISPLAY".to_string(), socket);
            x11_socket_dir(Path::new(X11_SOCKET_DIR));
        }
        let names = entry
            .get("DesktopNames")
            .map(|n| n.trim_matches(';').replace(';', ":"))
            .unwrap_or_default();
        if names.is_empty() {
            env.remove("XDG_CURRENT_DESKTOP");
        } else {
            env.insert("XDG_CURRENT_DESKTOP".to_string(), names);
        }
        if !sid.is_empty() {
            env.insert("XDG_SESSION_DESKTOP".to_string(), sid.to_string());
            env.insert("DESKTOP_SESSION".to_string(), sid.to_string());
        }
        let mut preload: Vec<String> = ["SELKIES_INTERPOSER", "SELKIES_WEBCAM_INTERPOSER"]
            .iter()
            .filter_map(|v| env.get(*v).cloned())
            .filter(|p| Path::new(p).is_file())
            .collect();
        preload.extend(
            env.get("LD_PRELOAD")
                .map(|p| {
                    p.split(':')
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        );
        if !preload.is_empty() {
            env.insert("LD_PRELOAD".to_string(), preload.join(":"));
        }
        let exec = entry.get("Exec").cloned().unwrap_or_default();
        let mut argv: Vec<String> = if which("dbus-run-session").is_some() {
            vec!["dbus-run-session".to_string(), "--".to_string()]
        } else {
            Vec::new()
        };
        argv.extend(
            shlex::split(&exec)
                .ok_or_else(|| format!("the session command cannot be parsed: {exec}"))?,
        );
        if argv.is_empty() {
            return Err("the session has no command".into());
        }
        let shown = env
            .get("WAYLAND_DISPLAY")
            .filter(|d| !d.is_empty())
            .or_else(|| env.get("DISPLAY"))
            .cloned()
            .unwrap_or_default();
        self.spawn(&argv, Some(&env), None)
            .map_err(|e| format!("the desktop did not start: {e}"))?;
        log(&format!(
            "desktop {} on {shown}",
            if sid.is_empty() { exec.as_str() } else { sid }
        ));
        Ok(())
    }

    /// Start Selkies itself with `command`; the result is its exit status once it ends.
    fn start_selkies(&mut self, command: &[String]) -> Result<(), String> {
        let preload = std::env::var("SELKIES_PRELOAD").unwrap_or_default();
        let env = for_selkies(&self.env, &preload);
        let index = self
            .spawn(command, Some(&env), None)
            .map_err(|e| format!("selkies did not start: {e}"))?;
        self.selkies = Some(index);
        Ok(())
    }

    /// Whether Selkies has exited, and with what status.
    fn selkies_exited(&mut self) -> Option<i32> {
        match self.selkies {
            Some(index) => exited(&mut self.children[index]),
            None => Some(1),
        }
    }

    /// The processes outside the session's process groups: those carrying its runtime
    /// directory, and the orphans the launcher adopted, since an agent that makes itself
    /// undumpable (ssh-agent) hides its environment.
    pub fn stragglers(&self) -> Vec<i32> {
        let marker = format!("XDG_RUNTIME_DIR={}", self.runtime_dir.display()).into_bytes();
        let launcher = std::process::id().to_string();
        let ours: BTreeSet<u32> = self.children.iter().map(Child::id).collect();
        let mut found = Vec::new();
        let Ok(entries) = fs::read_dir("/proc") else {
            return found;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(pid) = name
                .to_str()
                .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
            else {
                continue;
            };
            let Ok(pid_num) = pid.parse::<i32>() else {
                continue;
            };
            let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            let adopted = stat
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().nth(1))
                .is_some_and(|ppid| ppid == launcher);
            if adopted && !ours.contains(&(pid_num as u32)) {
                found.push(pid_num);
                continue;
            }
            let Ok(environ) = fs::read(entry.path().join("environ")) else {
                continue;
            };
            if environ
                .split(|b| *b == 0)
                .any(|var| var == marker.as_slice())
            {
                found.push(pid_num);
            }
        }
        found
    }

    /// Collect the children that exited: the session's own through their handles, which
    /// keep their status, and the orphans it adopted.
    pub fn reap(&mut self) {
        for child in &mut self.children {
            let _ = child.try_wait();
        }
        let ours: BTreeSet<i32> = self.children.iter().map(|c| c.id() as i32).collect();
        loop {
            let status = waitid(
                Id::All,
                WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT,
            );
            let pid = match status {
                Ok(WaitStatus::StillAlive) | Err(_) => return,
                Ok(status) => status.pid(),
            };
            let Some(pid) = pid else { return };
            if ours.contains(&pid.as_raw()) {
                return;
            }
            let _ = waitpid(pid, None);
        }
    }

    /// The session's process groups, then whatever left them.
    pub fn signal_session(&self, signal: Signal) {
        for pgid in &self.groups {
            let _ = killpg(Pid::from_raw(*pgid), signal);
        }
        for pid in self.stragglers() {
            let _ = kill(Pid::from_raw(pid), signal);
        }
    }

    /// Selkies first, so its capture closes before its display, then the session's process
    /// groups and whatever left them, and the runtime directory.
    pub fn stop(&mut self) {
        if let Some(index) = self.selkies {
            let child = &mut self.children[index];
            if exited(child).is_none() {
                let _ = kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM);
                let deadline = Instant::now() + Duration::from_secs(10);
                while exited(child).is_none() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(100));
                }
                if exited(child).is_none() {
                    let _ = child.kill();
                }
            }
        }
        self.signal_session(Signal::SIGTERM);
        let deadline = Instant::now() + Duration::from_secs(5);
        for child in &mut self.children {
            while exited(child).is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        self.signal_session(Signal::SIGKILL);
        for child in &mut self.children {
            let _ = child.wait();
        }
        self.reap();
        let _ = fs::remove_dir_all(&self.runtime_dir);
    }
}

/// The first render node, for an Xvfb that draws on the GPU.
fn first_render_node() -> String {
    let Ok(entries) = fs::read_dir("/dev/dri") else {
        return String::new();
    };
    let mut nodes: Vec<String> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| n.starts_with("renderD"))
        .collect();
    nodes.sort();
    nodes
        .first()
        .map(|n| format!("/dev/dri/{n}"))
        .unwrap_or_default()
}

/// A pipe whose write end a child may inherit (no close-on-exec).
fn pipe() -> Result<(OwnedFd, OwnedFd), String> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(format!("pipe: {}", io::Error::last_os_error()));
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// The first line written to `fd` within `timeout`, trimmed, or `None`.
fn read_line_within(fd: OwnedFd, timeout: Duration) -> Option<String> {
    let mut fds = [PollFd::new(fd.as_fd(), PollFlags::POLLIN)];
    let millis = u16::try_from(timeout.as_millis()).unwrap_or(u16::MAX);
    let ready = poll(&mut fds, PollTimeout::from(millis)).ok()?;
    if ready == 0 {
        return None;
    }
    let mut line = String::new();
    BufReader::new(fs::File::from(fd))
        .read_line(&mut line)
        .ok()?;
    Some(line.trim().to_string())
}

/// Have the kernel make the launcher, not the host's init, the parent of every process the
/// session orphans.
pub fn adopt_orphans() {
    unsafe {
        libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1);
    }
}

/// Have the kernel send SIGTERM when the parent exits.
pub fn follow_parent() {
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
    }
}

/// The signal that asked the launcher to stop, or 0.
static STOPPING: AtomicI32 = AtomicI32::new(0);

extern "C" fn note_stop(signal: libc::c_int) {
    STOPPING.store(signal, Ordering::SeqCst);
}

/// Stop in order on SIGTERM, SIGINT, and SIGHUP, except one the caller ignores (nohup's, and
/// a background job's SIGINT), which stays ignored.
fn install_stop_handlers() {
    let action = SigAction::new(
        SigHandler::Handler(note_stop),
        SaFlags::empty(),
        SigSet::empty(),
    );
    for signal in [Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP] {
        let previous = unsafe { sigaction(signal, &action) };
        if let Ok(previous) = previous
            && matches!(previous.handler(), SigHandler::SigIgn)
        {
            let _ = unsafe { sigaction(signal, &previous) };
        }
    }
}

/// The launcher's own options.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// A session by file, desktop, or program name, else a command to run.
    pub session: Option<String>,
    /// End the session when the parent exits, for a parent that owns it and may die without
    /// stopping it: the Jupyter server proxy's entry.
    pub follow: bool,
}

/// What `selkies session`'s arguments ask for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Parsed {
    /// The launcher's options and the arguments that go to Selkies.
    Run(Options, Vec<String>),
    /// `--help`: the usage text to print.
    Help,
    /// `--jupyter-config`: the Jupyter server configuration that offers this desktop
    /// (`jupyter_config`) to print.
    JupyterConfig,
    /// A usage error, as argparse reports one.
    Error(String),
}

/// The usage text, as `--help` prints it.
pub fn usage(prog: &str) -> String {
    format!(
        "usage: {prog} [-h] [--session SESSION] [--follow] ...\n\n\
         Start a desktop session and Selkies streaming it; every other argument goes to selkies.\n\n\
         options:\n\
         \x20 -h, --help         show this help message and exit\n\
         \x20 --session SESSION  a session by file, desktop, or program name (plasma, KDE, xfce, startlxqt), else a command\n\
         \x20                    to run; unset, the desktop XDG_CURRENT_DESKTOP names, else the host's default desktop\n\
         \x20 --follow           end the session when the parent exits, as the Jupyter server proxy's entry asks\n\
         \x20 --jupyter-config   print the Jupyter server configuration that offers this desktop and exit\n"
    )
}

/// Split the launcher's options from Selkies'. Only the exact spellings are the launcher's
/// (no abbreviation takes a Selkies flag); a leading `--` is dropped and a later one kept.
pub fn parse(argv: &[String]) -> Parsed {
    let mut options = Options::default();
    let mut rest: Vec<String> = Vec::new();
    let mut only_rest = false;
    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        i += 1;
        if only_rest {
            rest.push(arg.clone());
        } else if arg == "--" {
            rest.push(arg.clone());
            only_rest = true;
        } else if arg == "-h" || arg == "--help" {
            return Parsed::Help;
        } else if arg == "--follow" {
            options.follow = true;
        } else if arg == "--jupyter-config" {
            return Parsed::JupyterConfig;
        } else if arg == "--session" {
            match argv.get(i).filter(|v| !v.starts_with('-')) {
                Some(value) => {
                    options.session = Some(value.clone());
                    i += 1;
                }
                None => return Parsed::Error("argument --session: expected one argument".into()),
            }
        } else if let Some(value) = arg.strip_prefix("--session=") {
            options.session = Some(value.to_string());
        } else {
            rest.push(arg.clone());
        }
    }
    if rest.first().is_some_and(|a| a == "--") {
        rest.remove(0);
    }
    Parsed::Run(options, rest)
}

/// Run a session until Selkies ends or a signal arrives.
///
/// Args:
///     argv: The launcher's arguments, after the command's name.
///     prog: The name it was called by, for its usage text.
///
/// Returns:
///     Selkies' exit status, or 0 after a signal, 2 for a usage error.
pub fn main(argv: &[String], prog: &str) -> i32 {
    let (options, selkies_args) = match parse(argv) {
        Parsed::Help => {
            print!("{}", usage(prog));
            return 0;
        }
        Parsed::JupyterConfig => {
            let data_home = super::sessions::data_dirs().into_iter().next();
            println!("{:#}", jupyter_config(data_home.as_deref()));
            return 0;
        }
        Parsed::Error(message) => {
            eprint!("{}", usage(prog));
            eprintln!("{prog}: error: {message}");
            return 2;
        }
        Parsed::Run(options, rest) => (options, rest),
    };
    let settings = selkies_settings(&selkies_args);
    let wayland = settings.wayland().on;
    if options.follow {
        follow_parent();
    }
    adopt_orphans();
    install_stop_handlers();
    let mut session = match Session::new() {
        Ok(session) => session,
        Err(err) => {
            log(&format!("cannot make a runtime directory: {err}"));
            return 1;
        }
    };
    let mut status = 1;
    let run = |session: &mut Session, status: &mut i32| -> Result<(), String> {
        let exe = std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .map_err(|e| format!("cannot find the selkies executable: {e}"))?;
        let mut command = vec![exe];
        if !session.sound() {
            command.push("--audio-enabled=false".to_string());
        }
        command.extend(selkies_args.iter().cloned());
        if !wayland {
            session.x11(settings.render_dri())?;
            session.size_screen(&settings, run_resize);
        }
        let (sid, entry) = Desktops::from_env().pick(options.session.as_deref(), wayland)?;
        if wayland {
            session.start_selkies(&command)?;
        }
        if entry.is_empty() {
            log(&format!(
                "no {} session is installed; the display stays empty",
                if wayland { "Wayland" } else { "X11" }
            ));
        } else {
            session.desktop(&sid, &entry, wayland)?;
        }
        if !wayland {
            session.start_selkies(&command)?;
        }
        loop {
            if STOPPING.load(Ordering::SeqCst) != 0 {
                *status = 0;
                return Ok(());
            }
            if let Some(code) = session.selkies_exited() {
                *status = code;
                return Ok(());
            }
            session.reap();
            std::thread::sleep(Duration::from_millis(500));
        }
    };
    if let Err(err) = run(&mut session, &mut status) {
        log(&err);
    }
    session.stop();
    status
}

/// The Jupyter server configuration that offers this desktop: jupyter-server-proxy reads its
/// servers from `ServerProxy.servers` in any file of a Jupyter configuration directory's
/// `jupyter_server_config.d`, so `selkies session --jupyter-config` written there gives every
/// Jupyter server of that environment a Selkies launcher item. The entry names this executable
/// and the web client's `selkies.svg` as a file ([`jupyter_icon`]), `data_home` being the
/// user's data directory.
pub fn jupyter_config(data_home: Option<&Path>) -> Json {
    let selkies = std::env::current_exe().unwrap_or_else(|_| "selkies".into());
    let icon = jupyter_icon(crate::http::static_files::WebClient::bundled(), data_home);
    json!({"ServerProxy": {"servers": {"selkies": jupyter(&selkies, &icon)}}})
}

/// The launcher's icon, the web client's `selkies.svg`, as the file jupyter-server-proxy reads
/// on every request for it: a directory serving the client holds one, and an embedded client's
/// is written to `selkies/selkies.svg` under `data_home`, where it outlives the command that
/// printed its path, whenever what is there differs. A bare `selkies.svg` where neither is to
/// be had, which the launcher draws as a missing icon.
pub fn jupyter_icon(
    client: Option<crate::http::static_files::WebClient>,
    data_home: Option<&Path>,
) -> PathBuf {
    use crate::http::static_files::WebClient;
    let fallback = PathBuf::from("selkies.svg");
    match client {
        Some(WebClient::Directory(dir)) => dir.join("selkies.svg"),
        Some(WebClient::Embedded(table)) => {
            let (Some(home), Some(svg)) = (data_home, table.read("selkies.svg")) else {
                return fallback;
            };
            let path = home.join("selkies/selkies.svg");
            if fs::read(&path).is_ok_and(|held| held == svg) {
                return path;
            }
            // Two Jupyter servers may start at once: each renames a whole file into place.
            let staged = path.with_extension(format!("svg.{}", std::process::id()));
            let written = fs::create_dir_all(home.join("selkies"))
                .and_then(|()| fs::write(&staged, &svg))
                .and_then(|()| fs::rename(&staged, &path));
            if written.is_err() {
                let _ = fs::remove_file(&staged);
                return fallback;
            }
            path
        }
        None => fallback,
    }
}

/// The server process jupyter-server-proxy starts a desktop with.
///
/// Jupyter authenticates the route and the proxy's Unix socket sits in a directory only this
/// user reaches, so Selkies runs without its own login or TLS. The session follows the server
/// down (`--follow`), which on a kill stops nothing itself. A notebook panel cannot hold the
/// pointer or the keyboard, hence a tab. `icon` is the dashboard's `selkies.svg` as a file
/// ([`jupyter_icon`]).
pub fn jupyter(selkies: &Path, icon: &Path) -> Json {
    json!({
        "command": [
            selkies.to_string_lossy(), "session", "--follow",
            "--unix-socket", "{unix_socket}", "--enable-basic-auth=false", "--enable-https=false",
        ],
        "unix_socket": true,
        "timeout": 120,
        "new_browser_tab": true,
        "launcher_entry": {"title": "Selkies", "icon_path": icon.to_string_lossy()},
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_screens::testing::{isolated, run_isolated};

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn the_launchers_options_are_split_from_selkies() {
        let parsed = parse(&argv(&[
            "--session",
            "xfce",
            "--port=1",
            "--",
            "--subfolder=/x",
        ]));
        let options = Options {
            session: Some("xfce".into()),
            follow: false,
        };
        assert_eq!(
            parsed,
            Parsed::Run(options, argv(&["--port=1", "--", "--subfolder=/x"])),
            "--session is the launcher's and the rest goes to selkies"
        );
        assert_eq!(
            parse(&argv(&["--", "--wayland"])),
            Parsed::Run(Options::default(), argv(&["--wayland"])),
            "a leading -- is dropped"
        );
        assert_eq!(
            parse(&argv(&["--sess"])),
            Parsed::Run(Options::default(), argv(&["--sess"])),
            "no abbreviation takes a selkies flag"
        );
        assert_eq!(
            parse(&argv(&["--session=KDE", "--follow"])),
            Parsed::Run(
                Options {
                    session: Some("KDE".into()),
                    follow: true
                },
                Vec::new()
            )
        );
        assert_eq!(parse(&argv(&["--port=1", "--help"])), Parsed::Help);
        assert_eq!(parse(&argv(&["--jupyter-config"])), Parsed::JupyterConfig);
        assert!(matches!(parse(&argv(&["--session"])), Parsed::Error(_)));
        assert!(usage("selkies session").starts_with("usage: selkies session "));
    }

    #[test]
    fn the_backend_is_read_as_selkies_reads_it() {
        let parse = |args: &[&str], env: &[(&str, &str)]| {
            let args = argv(args);
            Settings::parse(&args, &|k| {
                env.iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            })
        };
        assert!(!parse(&[], &[]).wayland().on, "the backend defaults to X11");
        assert!(
            parse(&[], &[("SELKIES_WAYLAND", "true")]).wayland().on,
            "SELKIES_WAYLAND picks Wayland"
        );
        assert!(
            !parse(&["--wayland=false"], &[("SELKIES_WAYLAND", "true")])
                .wayland()
                .on,
            "the command line outranks it"
        );
        assert!(
            parse(&[], &[("PIXELFLUX_WAYLAND", "1")]).wayland().on,
            "the legacy PIXELFLUX_WAYLAND is read as Selkies reads it"
        );
        assert!(
            parse(&["--wayland"], &[]).wayland().on,
            "a bare --wayland picks Wayland"
        );
    }

    #[test]
    fn the_authority_file_holds_one_wildcard_cookie() {
        let cookie = [7u8; 16];
        let entry = xauth_wildcard_entry(&cookie);
        assert_eq!(entry.len(), 44);
        assert_eq!(
            &entry[..2],
            &[0xff, 0xff],
            "the cookie entry is the one the launcher writes"
        );
        assert_eq!(&entry[8..26], b"MIT-MAGIC-COOKIE-1");
        assert_eq!(&entry[28..], &cookie);
        let flags: Vec<&&str> = XVFB_ARGS
            .iter()
            .filter(|a| a.starts_with(['-', '+']))
            .collect();
        assert!(
            flags.iter().all(|a| [
                "-screen",
                "-nolisten",
                "-noreset",
                "-s",
                "-dpms",
                "+extension"
            ]
            .contains(a)),
            "the launcher's Xvfb flags stay within what every Xvfb takes"
        );
    }

    #[test]
    fn the_screen_starts_at_the_images_size() {
        let env = Env::from([("DISPLAY".to_string(), ":7".to_string())]);
        let session = Session {
            runtime_dir: PathBuf::new(),
            env,
            children: Vec::new(),
            groups: BTreeSet::new(),
            selkies: None,
        };
        let calls = std::cell::RefCell::new(Vec::new());
        let answer = |out: &'static str| {
            |argv: &[String], env: &Env| {
                calls.borrow_mut().push((argv.to_vec(), env.clone()));
                Some(out.to_string())
            }
        };
        let line = session.size_screen(&selkies_settings(&[]), answer("(1920, 1080)\n"));
        let (argv, env) = calls.borrow()[0].clone();
        assert_eq!(&argv[1..], ["resize", "1920x1080"]);
        assert_eq!(env, session.env);
        assert_eq!(
            XVFB_ARGS[2], "8192x4096x24",
            "inside a framebuffer that leaves room to grow"
        );
        assert!(line.ends_with("starts at 1920x1080"));
        let manual = Settings::parse(
            &argv_of(&[
                "--manual_resolution=true",
                "--manual_width=1362",
                "--manual_height=1036",
            ]),
            &|_| None,
        );
        let line = session.size_screen(&manual, answer("(1368, 1036)\n"));
        assert_eq!(
            calls.borrow()[1].0[2],
            "1362x1036",
            "a manual resolution is the size it starts at"
        );
        assert!(line.ends_with("starts at 1368x1036"), "as realized");
        let line = session.size_screen(&selkies_settings(&[]), answer("None\n"));
        assert!(
            line.contains("whole framebuffer"),
            "a server that cannot be resized is reported and keeps going"
        );
        let line = session.size_screen(&selkies_settings(&[]), |_, _| None);
        assert!(line.contains("whole framebuffer"));
    }

    fn argv_of(args: &[&str]) -> Vec<String> {
        argv(args)
    }

    #[test]
    fn preload_reaches_selkies_alone() {
        let env = Env::from([
            ("LD_PRELOAD".to_string(), "/lib/other.so".to_string()),
            ("A".to_string(), "1".to_string()),
        ]);
        let preloaded = for_selkies(&env, "/usr/lib/libxcb.so.1");
        assert_eq!(
            preloaded["LD_PRELOAD"], "/usr/lib/libxcb.so.1:/lib/other.so",
            "ahead of what it already preloads"
        );
        let plain = Env::from([("A".to_string(), "1".to_string())]);
        assert_eq!(for_selkies(&plain, ""), plain);
        assert_eq!(for_selkies(&plain, "/x.so")["LD_PRELOAD"], "/x.so");
    }

    #[test]
    fn a_missing_runtime_directory_falls_back_to_the_temp_directory() {
        let env = Env::from([
            ("DISPLAY".to_string(), ":1".to_string()),
            ("KEEP".to_string(), "1".to_string()),
        ]);
        let session = Session::new_in(Some(Path::new("/nonexistent/runtime")), env).unwrap();
        assert_eq!(
            session.runtime_dir.parent(),
            Some(std::env::temp_dir().as_path())
        );
        assert!(session.runtime_dir.is_dir());
        assert_eq!(
            session.env["XDG_RUNTIME_DIR"],
            session.runtime_dir.to_string_lossy()
        );
        assert!(
            !session.env.contains_key("DISPLAY"),
            "a host session's variables do not reach this one"
        );
        assert_eq!(session.env["KEEP"], "1");
        let base = std::env::temp_dir().join(format!("selkies-runtime-{}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        let under = Session::new_in(Some(&base), Env::new()).unwrap();
        assert_eq!(
            under.runtime_dir.parent(),
            Some(base.as_path()),
            "a usable runtime directory holds the session's"
        );
        fs::remove_dir_all(&session.runtime_dir).unwrap();
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn the_x11_socket_directory_is_world_writable_and_sticky() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("selkies-x11-unix-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let sockets = dir.join(".X11-unix");
        fs::create_dir_all(&dir).unwrap();
        x11_socket_dir(&sockets);
        assert_eq!(
            fs::metadata(&sockets).unwrap().permissions().mode() & 0o7777,
            0o1777
        );
        x11_socket_dir(&sockets);
        fs::remove_dir_all(&dir).unwrap();
        assert!(!answers(&sockets.join("X0")));
    }

    #[test]
    fn the_jupyter_config_offers_this_executable_as_a_proxy_server() {
        let data_home = crate::http::testing::tempfile_dir("selkies-jupyter-data-");
        let config = jupyter_config(Some(&data_home));
        let entry = &config["ServerProxy"]["servers"]["selkies"];
        let exe = std::env::current_exe().unwrap();
        assert_eq!(
            entry["command"][0].as_str(),
            Some(exe.to_string_lossy().as_ref()),
            "the executable that printed it"
        );
        assert_eq!(entry["command"][1], "session");
        assert!(
            entry["launcher_entry"]["icon_path"]
                .as_str()
                .is_some_and(|p| p.ends_with("selkies.svg")),
            "the web client's icon"
        );
        let _ = fs::remove_dir_all(&data_home);
    }

    #[test]
    fn the_embedded_icon_is_written_where_jupyter_reads_it_and_left_while_current() {
        use crate::http::static_files::WebClient;
        use std::os::unix::fs::MetadataExt;
        let home = crate::http::testing::tempfile_dir("selkies-jupyter-icon-");
        let svg = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\">{}</svg>",
            "<path d=\"M0 0h10v10z\"/>".repeat(20)
        );
        let table = crate::http::embedded::leak(&[("selkies.svg", svg.as_bytes())], 0);
        assert!(
            table.files[0].gzip_etag.is_some(),
            "stored compressed, so it is inflated"
        );
        let embedded = || Some(WebClient::Embedded(table));
        let path = jupyter_icon(embedded(), Some(&home));
        assert_eq!(path, home.join("selkies/selkies.svg"));
        assert_eq!(fs::read_to_string(&path).unwrap(), svg);
        let inode = fs::metadata(&path).unwrap().ino();
        jupyter_icon(embedded(), Some(&home));
        assert_eq!(
            fs::metadata(&path).unwrap().ino(),
            inode,
            "a current icon is left"
        );
        fs::write(&path, "stale").unwrap();
        jupyter_icon(embedded(), Some(&home));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            svg,
            "a stale one is replaced"
        );
        assert_eq!(
            fs::read_dir(home.join("selkies")).unwrap().count(),
            1,
            "nothing staged is left beside it"
        );
        assert_eq!(
            jupyter_icon(Some(WebClient::Directory(home.join("web"))), Some(&home)),
            home.join("web/selkies.svg"),
            "a directory's own icon"
        );
        assert_eq!(jupyter_icon(embedded(), None), Path::new("selkies.svg"));
        assert_eq!(jupyter_icon(None, Some(&home)), Path::new("selkies.svg"));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn the_jupyter_entry_runs_the_launcher_on_the_proxys_socket() {
        let entry = jupyter(
            Path::new("/opt/selkies/bin/selkies"),
            Path::new("/opt/selkies/web/selkies.svg"),
        );
        let command: Vec<&str> = entry["command"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            command[1..3],
            ["session", "--follow"],
            "following the server"
        );
        assert_eq!(
            command[3..],
            [
                "--unix-socket",
                "{unix_socket}",
                "--enable-basic-auth=false",
                "--enable-https=false"
            ],
            "without a login"
        );
        assert_eq!(entry["unix_socket"], true);
        assert_eq!(
            entry["new_browser_tab"], true,
            "the desktop opens in a tab of its own"
        );
        assert_eq!(
            entry["launcher_entry"]["icon_path"],
            "/opt/selkies/web/selkies.svg"
        );
    }

    /// Children share one process group the launcher is not in, and a stopped session leaves
    /// none of them and no runtime directory.
    #[test]
    fn a_session_groups_and_stops_its_children() {
        // A session reaps and signals every child of its process, so it needs one of its own.
        if !isolated() {
            run_isolated(
                "commands::session::tests::a_session_groups_and_stops_its_children",
                &[],
            );
            return;
        }
        let mut session = Session::new_in(None, Env::new()).unwrap();
        let first = session.spawn(&argv(&["sleep", "30"]), None, None).unwrap();
        let second = session.spawn(&argv(&["sleep", "30"]), None, None).unwrap();
        let pids: Vec<i32> = [first, second]
            .iter()
            .map(|i| session.children[*i].id() as i32)
            .collect();
        let groups: BTreeSet<i32> = pids
            .iter()
            .map(|p| getpgid(Some(Pid::from_raw(*p))).unwrap().as_raw())
            .collect();
        assert_eq!(groups.len(), 1, "the children share one process group");
        assert_eq!(session.groups, groups);
        assert_ne!(
            groups.iter().next(),
            Some(&getpgid(None).unwrap().as_raw()),
            "which the launcher is not in"
        );
        let runtime_dir = session.runtime_dir.clone();
        assert!(
            session.selkies_exited() == Some(1),
            "without Selkies there is nothing to wait for"
        );
        session.stop();
        assert!(session.children.iter_mut().all(|c| exited(c).is_some()));
        assert!(
            !runtime_dir.exists(),
            "the runtime directory goes with the session"
        );
    }

    /// A real Xvfb on a free display, admitting the session's cookie alone, and gone with the
    /// session.
    #[test]
    #[ignore = "starts an Xvfb; run where one is installed"]
    fn an_xvfb_comes_up_on_a_free_display() {
        // A session reaps and signals every child of its process, so it needs one of its own.
        if !isolated() {
            run_isolated(
                "commands::session::tests::an_xvfb_comes_up_on_a_free_display",
                &[],
            );
            return;
        }
        let mut session = Session::new_in(None, Env::new()).unwrap();
        session.x11("").expect("Xvfb comes up");
        let display = session.env["DISPLAY"].clone();
        assert!(display.starts_with(':'), "{display}");
        let auth = PathBuf::from(&session.env["XAUTHORITY"]);
        assert_eq!(
            fs::read(&auth).unwrap()[..2],
            [0xff, 0xff],
            "the authority file holds the wildcard cookie"
        );
        let socket = Path::new(X11_SOCKET_DIR).join(format!("X{}", &display[1..]));
        assert!(
            answers(&socket),
            "the server listens on {}",
            socket.display()
        );
        session.stop();
        assert!(!answers(&socket), "the server goes with the session");
        assert!(!auth.exists());
    }

    #[test]
    fn a_display_number_is_read_from_the_display_pipe() {
        let (read, write) = pipe().unwrap();
        std::io::Write::write_all(&mut fs::File::from(write), b"7\n").unwrap();
        assert_eq!(
            read_line_within(read, Duration::from_secs(1)).as_deref(),
            Some("7")
        );
        let (read, _write) = pipe().unwrap();
        assert_eq!(
            read_line_within(read, Duration::from_millis(50)),
            None,
            "a server that never names its display is given up on"
        );
    }
}
