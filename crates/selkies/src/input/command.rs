/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Client-requested commands (`cmd,`): the environment they launch in and the watch that
//! reports how they ended.
//!
//! A command runs in the session's environment, so a launched application lands on the display
//! and session bus the desktop uses: the server's own, with the apps repository as the runner
//! reads it, `DISPLAY`, `WAYLAND_DISPLAY`, and `XDG_SESSION_TYPE` set for where the applications
//! run ([`AppSession`]), and the session bus plus desktop identity adopted from that session's
//! processes when the server has none of its own. A launch failure or any nonzero exit, above
//! all 127, the command-not-installed case, is reported with the runtime, the reason its output
//! gives, and the echoed command, because the dashboards' apps UI marks the action done
//! optimistically and needs a counter-signal to roll back. A clean exit settles a pending
//! install; the dashboards decide relevance by age, since a long-lived app quitting nonzero
//! hours later is application lifecycle rather than a launch failure.

use std::collections::{HashMap, HashSet, VecDeque};
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use log::{debug, error, info, warn};
use tokio::io::AsyncReadExt;

/// The last lines of a command's output kept.
const COMMAND_TAIL_LINES: usize = 20;
/// One line's worth of an unterminated write, and the read size.
const COMMAND_LINE_CHARS: usize = 4096;
/// The slice of the chosen line a failure notice carries.
const COMMAND_REASON_CHARS: usize = 200;
/// How long an empty session scan stands, so a command burst on a session with no bus does not
/// rescan `/proc` each time.
const SESSION_ENV_NEGCACHE: Duration = Duration::from_secs(5);
/// What a launch from outside adopts from a session process: the session bus above all
/// (`dbus-launch` and `dbus-run-session` addresses live only there), plus the desktop identity
/// toolkits and xdg-desktop-portal read.
pub const SESSION_ENV_ADOPTED: &[&str] = &[
    "DBUS_SESSION_BUS_ADDRESS",
    "XAUTHORITY",
    "XDG_CURRENT_DESKTOP",
    "DESKTOP_SESSION",
    "XDG_SESSION_DESKTOP",
    "XDG_MENU_PREFIX",
    "XDG_SESSION_CLASS",
    "XDG_SESSION_ID",
];

/// Where the session's applications run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppSession {
    /// The X server they connect to, if any.
    pub x11_display: Option<String>,
    /// The compositor socket, when they are Wayland clients.
    pub wayland_display: Option<String>,
    /// `x11` or `wayland`.
    pub kind: &'static str,
}

/// The pids launched from here, whose environments carry what was adopted for them and are
/// never a source of it.
fn launched_pids() -> &'static Mutex<HashSet<i32>> {
    static PIDS: OnceLock<Mutex<HashSet<i32>>> = OnceLock::new();
    PIDS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Local X display names (`:N`) with a server accepting connections, lowest number first.
pub fn live_x_displays() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/tmp/.X11-unix") else {
        return Vec::new();
    };
    let mut found: Vec<u32> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_prefix('X')?
                .parse::<u32>()
                .ok()
        })
        .filter(|n| crate::clipboard::x11::display_live(&format!(":{n}")))
        .collect();
    found.sort_unstable();
    found.into_iter().map(|n| format!(":{n}")).collect()
}

/// Whether the first transport of a D-Bus address is a Unix socket that accepts a connection,
/// or is not a Unix socket at all (unverifiable, kept).
pub fn dbus_address_live(address: &str) -> bool {
    let first = address.split(';').next().unwrap_or("");
    let Some(params) = first.strip_prefix("unix:") else {
        return true;
    };
    let param = |key: &str| {
        params.split(',').find_map(|kv| {
            kv.split_once('=')
                .filter(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        })
    };
    if let Some(path) = param("path") {
        return UnixStream::connect(path).is_ok();
    }
    if let Some(name) = param("abstract") {
        use std::os::linux::net::SocketAddrExt;
        return std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
            .and_then(|addr| UnixStream::connect_addr(&addr))
            .is_ok();
    }
    true
}

/// The given pids plus every live descendant of theirs, from `/proc/*/stat`.
fn descendants(roots: &HashSet<i32>) -> HashSet<i32> {
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return HashSet::new();
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let ppid = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1)?.parse::<i32>().ok());
        if let Some(ppid) = ppid {
            children.entry(ppid).or_default().push(pid);
        }
    }
    let mut out = roots.clone();
    let mut todo: Vec<i32> = roots.iter().copied().collect();
    while let Some(pid) = todo.pop() {
        for child in children.get(&pid).into_iter().flatten() {
            if out.insert(*child) {
                todo.push(*child);
            }
        }
    }
    out
}

/// A process's environment from `/proc`, or None when unreadable.
fn process_environ(pid: i32) -> Option<HashMap<String, String>> {
    let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    Some(
        raw.split(|b| *b == 0)
            .filter_map(|item| {
                let at = item.iter().position(|b| *b == b'=')?;
                Some((
                    String::from_utf8_lossy(&item[..at]).into_owned(),
                    String::from_utf8_lossy(&item[at + 1..]).into_owned(),
                ))
            })
            .collect(),
    )
}

/// The `SESSION_ENV_ADOPTED` subset of the environment of the desktop session running on the
/// given displays, read from the oldest process of this user that carries a session bus address
/// and exports one of those displays. Empty when no such process exists (the session has not
/// started, or runs no bus).
///
/// A Wayland socket name is relative to `XDG_RUNTIME_DIR`, so it identifies a session only
/// together with it; an X display name is host-wide and outlives the server that used it, so a
/// process of this runtime dir ranks before a leftover of an earlier session. Commands launched
/// from here and their descendants carry whatever was adopted for them and are never a source.
/// Blocking: it walks `/proc`.
pub fn session_environment(
    x11_display: Option<&str>,
    wayland_display: Option<&str>,
) -> HashMap<String, String> {
    if x11_display.is_none() && wayland_display.is_none() {
        return HashMap::new();
    }
    let uid = nix::unistd::getuid().as_raw();
    let me = std::process::id() as i32;
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").ok();
    let launched = launched_pids()
        .lock()
        .map(|p| p.clone())
        .unwrap_or_default();
    let own = if launched.is_empty() {
        HashSet::new()
    } else {
        descendants(&launched)
    };
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return HashMap::new();
    };
    let mut candidates: Vec<(u8, i32, HashMap<String, String>)> = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        if pid == me || own.contains(&pid) {
            continue;
        }
        if entry.metadata().map(|m| m.uid() != uid).unwrap_or(true) {
            continue;
        }
        let Some(env) = process_environ(pid) else {
            continue;
        };
        if env
            .get("DBUS_SESSION_BUS_ADDRESS")
            .is_none_or(|a| a.is_empty())
        {
            continue;
        }
        let same_runtime = runtime_dir
            .as_ref()
            .is_none_or(|r| !r.is_empty() && env.get("XDG_RUNTIME_DIR") == Some(r));
        let same_x = x11_display.is_some() && env.get("DISPLAY").map(String::as_str) == x11_display;
        let same_wl = wayland_display.is_some()
            && env.get("WAYLAND_DISPLAY").map(String::as_str) == wayland_display
            && same_runtime;
        if same_x || same_wl {
            candidates.push((if same_runtime { 0 } else { 1 }, pid, env));
        }
    }
    candidates.sort_by_key(|c| (c.0, c.1));
    for (_, _, env) in candidates {
        if dbus_address_live(&env["DBUS_SESSION_BUS_ADDRESS"]) {
            return SESSION_ENV_ADOPTED
                .iter()
                .filter_map(|k| {
                    env.get(*k)
                        .filter(|v| !v.is_empty())
                        .map(|v| (k.to_string(), v.clone()))
                })
                .collect();
        }
    }
    HashMap::new()
}

/// The adopted session environment per `(x11, wayland)` pair; an empty scan is negative-cached
/// so a command burst does not rescan `/proc` each time.
#[derive(Default)]
pub struct SessionEnvCache {
    adopted: HashMap<(Option<String>, Option<String>), HashMap<String, String>>,
    empty_at: HashMap<(Option<String>, Option<String>), Instant>,
}

impl SessionEnvCache {
    /// The subset for `session`, refreshed when its bus stops answering. Blocking.
    fn adopted(&mut self, session: &AppSession) -> HashMap<String, String> {
        let key = (session.x11_display.clone(), session.wayland_display.clone());
        let cached = self.adopted.get(&key);
        let live = cached.is_some_and(|env| {
            dbus_address_live(
                env.get("DBUS_SESSION_BUS_ADDRESS")
                    .map_or("", String::as_str),
            )
        });
        if live {
            return cached.cloned().unwrap_or_default();
        }
        if cached.is_none()
            && self
                .empty_at
                .get(&key)
                .is_some_and(|at| at.elapsed() < SESSION_ENV_NEGCACHE)
        {
            return HashMap::new();
        }
        let adopted = session_environment(
            session.x11_display.as_deref(),
            session.wayland_display.as_deref(),
        );
        if adopted.is_empty() {
            self.adopted.remove(&key);
            self.empty_at.insert(key, Instant::now());
        } else {
            debug!(target: "input", "Application launches adopt the session on {key:?}: {:?}", adopted.keys().collect::<Vec<_>>());
            self.empty_at.remove(&key);
            self.adopted.insert(key, adopted.clone());
        }
        adopted
    }

    /// The environment for a client-requested command, on the server's own (module docstring).
    /// `apps_repo` is the setting's reading, written where the runner reads it: set, the runner
    /// installs from the folder the panel lists; empty, the variable is withdrawn, so the runner
    /// and the panel never read two repositories. Blocking.
    pub fn launch_env(&mut self, session: &AppSession, apps_repo: &str) -> HashMap<String, String> {
        let mut env: HashMap<String, String> = std::env::vars().collect();
        if apps_repo.is_empty() {
            env.remove("PA_REPO_FOLDER");
        } else {
            env.insert("PA_REPO_FOLDER".into(), apps_repo.to_string());
        }
        env.remove("DISPLAY");
        env.remove("WAYLAND_DISPLAY");
        if let Some(x11) = &session.x11_display {
            env.insert("DISPLAY".into(), x11.clone());
        }
        if let Some(wayland) = &session.wayland_display {
            env.insert("WAYLAND_DISPLAY".into(), wayland.clone());
        }
        env.insert("XDG_SESSION_TYPE".into(), session.kind.to_string());
        for (name, value) in self.adopted(session) {
            env.entry(name).or_insert(value);
        }
        env
    }
}

/// Split output into the tail's lines. Progress meters rewrite their line with a carriage
/// return, so both terminators end one; what is left unterminated returns to `buf`.
fn take_lines(buf: &mut String, tail: &mut VecDeque<String>) {
    let mut parts: Vec<String> = buf.split(['\r', '\n']).map(str::to_string).collect();
    let rest = parts.pop().unwrap_or_default();
    for part in parts {
        let line = part.trim();
        if !line.is_empty() {
            if tail.len() == COMMAND_TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(line.to_string());
        }
    }
    let skip = rest.chars().count().saturating_sub(COMMAND_LINE_CHARS);
    *buf = rest.chars().skip(skip).collect();
}

/// The line a reader would quote from failed output: the first that names a failure, else the
/// last thing written.
pub fn command_failure_reason(tail: &VecDeque<String>) -> String {
    let names_failure = |line: &&String| {
        let lower = line.to_lowercase();
        ["error", "fail", "denied", "not permitted", "no such"]
            .iter()
            .any(|w| lower.contains(w))
    };
    tail.iter()
        .find(names_failure)
        .or(tail.back())
        .map(|line| line.chars().take(COMMAND_REASON_CHARS).collect())
        .unwrap_or_default()
}

/// How a client command ended, as the requester hears it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandOutcome {
    /// `command_done,<cmd>`: exited 0.
    Done,
    /// `command_error,<text>`: failed to launch or exited nonzero.
    Error(String),
}

/// Launch `command` through the shell in `env`, from the home directory, and return once it is
/// running; `on_end` hears how it ended. The output is read rather than discarded so a failure
/// can say why, and read continuously so a chatty application never blocks on a full pipe.
pub async fn run_client_command<F>(command: String, env: HashMap<String, String>, on_end: F)
where
    F: FnOnce(CommandOutcome) + Send + 'static,
{
    let spawned = spawn_shell(&command, &env);
    let (mut child, reader) = match spawned {
        Ok(pair) => pair,
        Err(e) => {
            error!(target: "input", "Failed to launch command '{command}': {e}");
            on_end(CommandOutcome::Error(format!(
                "failed to launch: {command}"
            )));
            return;
        }
    };
    let pid = child.id().map(|p| p as i32).unwrap_or(0);
    info!(target: "input", "Launched command '{command}' with PID {pid}");
    if let Ok(mut pids) = launched_pids().lock() {
        pids.insert(pid);
    }
    let started = Instant::now();
    tokio::spawn(async move {
        let mut tail = VecDeque::with_capacity(COMMAND_TAIL_LINES);
        let mut buf = String::new();
        let mut chunk = vec![0u8; COMMAND_LINE_CHARS];
        let mut reader = reader;
        while let Ok(n) = reader.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            buf.push_str(&String::from_utf8_lossy(&chunk[..n]));
            take_lines(&mut buf, &mut tail);
        }
        if !buf.trim().is_empty() {
            if tail.len() == COMMAND_TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(buf.trim().to_string());
        }
        let status = child.wait().await;
        if let Ok(mut pids) = launched_pids().lock() {
            pids.remove(&pid);
        }
        let Ok(status) = status else { return };
        let runtime = started.elapsed().as_secs_f64();
        use std::os::unix::process::ExitStatusExt;
        // A signal death reads as the negative number a process's owner would see.
        let code = status
            .code()
            .unwrap_or_else(|| -status.signal().unwrap_or(1));
        if code == 0 {
            on_end(CommandOutcome::Done);
            return;
        }
        let hint = if code == 126 || code == 127 {
            " (command not found)"
        } else {
            ""
        };
        let reason = command_failure_reason(&tail);
        let detail = if reason.is_empty() {
            String::new()
        } else {
            format!(" -- {reason}")
        };
        warn!(target: "input", "Command '{command}' exited with status {code}{hint} after {runtime:.1}s{detail}");
        if !tail.is_empty() {
            // The whole captured tail, not just the quoted line: a launch that fails has no
            // window left to read its output in.
            debug!(target: "input", "Output of '{command}':\n{}", Vec::from(tail).join("\n"));
        }
        on_end(CommandOutcome::Error(format!(
            "exited with status {code}{hint} after {runtime:.1}s{detail}: {command}"
        )));
    });
}

/// `sh -c command` with stdout and stderr on one pipe, whose read end comes back.
fn spawn_shell(
    command: &str,
    env: &HashMap<String, String>,
) -> std::io::Result<(tokio::process::Child, tokio::net::unix::pipe::Receiver)> {
    let (read, write): (OwnedFd, OwnedFd) = nix::unistd::pipe().map_err(std::io::Error::from)?;
    let stderr = write.try_clone()?;
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    let child = tokio::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .env_clear()
        .envs(env)
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::from(write))
        .stderr(Stdio::from(stderr))
        .spawn()?;
    Ok((
        child,
        tokio::net::unix::pipe::Receiver::from_owned_fd(read)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn outcome(command: &str) -> CommandOutcome {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let env: HashMap<String, String> = std::env::vars().collect();
        run_client_command(command.to_string(), env, move |o| {
            let _ = tx.send(o);
        })
        .await;
        rx.await.expect("the watch reports")
    }

    #[tokio::test]
    async fn a_clean_exit_is_done_and_a_failure_says_why() {
        assert_eq!(outcome("true").await, CommandOutcome::Done);
        let CommandOutcome::Error(text) =
            outcome("echo hello; echo 'error: nope' >&2; exit 3").await
        else {
            panic!("a nonzero exit is an error")
        };
        assert!(text.starts_with("exited with status 3 after "), "{text}");
        assert!(
            text.contains(" -- error: nope: "),
            "the first line naming a failure is quoted: {text}"
        );
        assert!(text.ends_with("exit 3"), "the command is echoed: {text}");
        let CommandOutcome::Error(text) = outcome("selkies-no-such-program-at-all").await else {
            panic!("127 is an error")
        };
        assert!(text.contains("status 127 (command not found)"), "{text}");
    }

    #[test]
    fn tail_keeps_the_last_lines_and_treats_both_terminators_alike() {
        let mut tail = VecDeque::new();
        let mut buf = "10%\r20%\r\n\nlast".to_string();
        take_lines(&mut buf, &mut tail);
        assert_eq!(Vec::from(tail.clone()), vec!["10%", "20%"]);
        assert_eq!(buf, "last");
        let mut buf: String = (0..40).map(|i| format!("line {i}\n")).collect();
        take_lines(&mut buf, &mut tail);
        assert_eq!(tail.len(), COMMAND_TAIL_LINES);
        assert_eq!(tail.back().map(String::as_str), Some("line 39"));
        assert_eq!(command_failure_reason(&VecDeque::new()), "");
        let quoted: VecDeque<String> = ["ok", "Permission denied", "bye"].map(String::from).into();
        assert_eq!(command_failure_reason(&quoted), "Permission denied");
    }

    #[test]
    fn launch_env_sets_the_session_and_the_repository() {
        let mut cache = SessionEnvCache::default();
        let session = AppSession {
            x11_display: None,
            wayland_display: Some("wayland-7".into()),
            kind: "wayland",
        };
        let env = cache.launch_env(&session, "/repo");
        assert_eq!(
            env.get("WAYLAND_DISPLAY").map(String::as_str),
            Some("wayland-7")
        );
        assert!(!env.contains_key("DISPLAY"));
        assert_eq!(
            env.get("XDG_SESSION_TYPE").map(String::as_str),
            Some("wayland")
        );
        assert_eq!(env.get("PA_REPO_FOLDER").map(String::as_str), Some("/repo"));
        assert!(
            !cache
                .launch_env(&session, "")
                .contains_key("PA_REPO_FOLDER"),
            "an empty setting withdraws the variable"
        );
    }

    /// The session bus is adopted from a real dbus-daemon's session, never from our own launched
    /// children.
    #[test]
    #[ignore = "needs dbus-run-session"]
    fn a_launch_adopts_the_desktops_session_bus() {
        use std::os::unix::process::CommandExt;
        let display = format!(":{}", 60000 + std::process::id() % 5000);
        let mut session = std::process::Command::new("dbus-run-session")
            .args(["--", "sleep", "30"])
            .env("DISPLAY", &display)
            .env("XDG_CURRENT_DESKTOP", "MATE")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .expect("dbus-run-session runs");
        let group = nix::unistd::Pid::from_raw(session.id() as i32);
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let mut adopted = HashMap::new();
        while adopted.is_empty() && Instant::now() < deadline {
            adopted = session_environment(Some(&display), None);
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let bus = adopted
            .get("DBUS_SESSION_BUS_ADDRESS")
            .cloned()
            .unwrap_or_default();
        assert!(
            bus.starts_with("unix:"),
            "session bus found from the desktop's process: {adopted:?}"
        );
        assert_eq!(
            adopted.get("XDG_CURRENT_DESKTOP").map(String::as_str),
            Some("MATE"),
            "desktop identity adopted with it"
        );

        let on_x11 = AppSession {
            x11_display: Some(display.clone()),
            wayland_display: None,
            kind: "x11",
        };
        let mut cache = SessionEnvCache::default();
        let launch = cache.launch_env(&on_x11, "");
        assert_eq!(launch.get("DISPLAY"), Some(&display));
        assert!(!launch.contains_key("WAYLAND_DISPLAY"));
        assert_eq!(
            launch.get("XDG_SESSION_TYPE").map(String::as_str),
            Some("x11")
        );
        assert_eq!(
            launch.get("DBUS_SESSION_BUS_ADDRESS"),
            Some(&bus),
            "the launch carries the adopted bus"
        );
        assert_eq!(
            cache
                .launch_env(&on_x11, "")
                .get("DBUS_SESSION_BUS_ADDRESS"),
            Some(&bus),
            "adoption is cached while the bus answers"
        );
        assert_eq!(cache.adopted.len(), 1);

        nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL).expect("our group");
        let _ = session.wait();
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!dbus_address_live(&bus), "a dead bus is not kept");
        let mut fresh = SessionEnvCache::default();
        let launch = fresh.launch_env(&on_x11, "");
        assert_ne!(
            launch.get("DBUS_SESSION_BUS_ADDRESS"),
            Some(&bus),
            "no session process: nothing adopted, the server's environment stays"
        );
    }

    #[test]
    fn dbus_addresses() {
        assert!(
            dbus_address_live("tcp:host=localhost,port=1"),
            "not a Unix socket: unverifiable, kept"
        );
        assert!(!dbus_address_live("unix:path=/nonexistent/selkies-bus"));
        assert!(dbus_address_live("unix:guid=abc"), "no address to try");
    }
}
