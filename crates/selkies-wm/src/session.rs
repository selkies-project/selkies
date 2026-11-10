/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What the window manager starts and keeps running: the programs its bindings and menus name,
//! the session bus they share, the session's application, and the desktop shell while the
//! session is the desktop.
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
//! The session is the desktop or the application alone ([`SessionControl`]): the shell runs only
//! in the first, and the user switches between the two from the menus where the image allows,
//! the application running on through either. The application is the `autostart` file, run
//! once the manager is up as openbox runs its own, the first found along the backend's
//! configuration directories ([`crate::config::search_dirs`], the user's over the system's),
//! with `sh` in the session environment, and followed until everything it started exited, when
//! it may run again ([`start_autostart`]).

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::{self, Backend};
use crate::core::{Launcher, SessionMode};

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

/// What the image decides of the session, from the server's settings.
#[derive(Clone, Debug)]
pub struct SessionPolicy {
    /// The mode a session starts in where the user kept no choice: the desktop, or the
    /// application alone.
    pub desktop: bool,
    /// The image's mode is the only one: no choice is read or offered.
    pub locked: bool,
    /// The menus offer the switch between the modes.
    pub switch: bool,
    /// The application alone has its windows maximized.
    pub maximize: bool,
    /// The autostart runs again when everything it started has exited, while the session is
    /// the application alone.
    pub restart_app: bool,
    /// The desktop shell's program, a name on the `PATH` or a path.
    pub shell: String,
    /// What the image takes away from the window manager over its files.
    pub wm: crate::config::Policy,
}

/// The session's mode as the server and its window manager share it: whether the desktop shell
/// runs, which the user switches from the window manager's menus, and what the image allows.
///
/// A user's choice is kept in `selkies-wm/session` under the configuration home, the one word
/// `desktop` or `application`, and is the mode the next session starts in; the image's mode is
/// the start where no choice was kept, and the only mode where the image locks it. The switch is
/// offered only where the image allows it, the mode is not locked, and the shell's program is
/// installed, so a session without the shell never offers a desktop it cannot start.
#[derive(Debug)]
pub struct SessionControl {
    desktop: AtomicBool,
    /// The autostart's state: [`NO_AUTOSTART`], [`AUTOSTART_RUNNING`], or [`AUTOSTART_ENDED`].
    application: AtomicU8,
    /// The user asked for the autostart to run again.
    relaunch: AtomicBool,
    switchable: bool,
    maximize: bool,
    restart_app: bool,
    shell: String,
    wm: crate::config::Policy,
    state_file: PathBuf,
}

/// No autostart file was found.
const NO_AUTOSTART: u8 = 0;
/// What the autostart started runs.
const AUTOSTART_RUNNING: u8 = 1;
/// Everything the autostart started exited.
const AUTOSTART_ENDED: u8 = 2;

/// The file a user's choice of mode is kept in.
fn session_state_path() -> PathBuf {
    selkies_draw::dirs::config_home().join("selkies-wm/session")
}

impl SessionControl {
    /// The session as the policy and the user's kept choice decide it.
    pub fn new(policy: SessionPolicy) -> Arc<SessionControl> {
        Arc::new(SessionControl::with_state_file(policy, session_state_path()))
    }

    fn with_state_file(policy: SessionPolicy, state_file: PathBuf) -> SessionControl {
        let installed = selkies_draw::apps::resolve_program(&policy.shell).is_some();
        let kept = if policy.locked {
            None
        } else {
            std::fs::read_to_string(&state_file)
                .ok()
                .and_then(|text| match text.trim() {
                    "desktop" => Some(true),
                    "application" => Some(false),
                    _ => None,
                })
        };
        SessionControl {
            desktop: AtomicBool::new(kept.unwrap_or(policy.desktop)),
            application: AtomicU8::new(NO_AUTOSTART),
            relaunch: AtomicBool::new(false),
            switchable: policy.switch && !policy.locked && installed,
            maximize: policy.maximize,
            restart_app: policy.restart_app,
            shell: policy.shell,
            wm: policy.wm,
            state_file,
        }
    }

    /// Whether the desktop shell runs now.
    pub fn desktop(&self) -> bool {
        self.desktop.load(Ordering::Relaxed)
    }

    /// The mode as the window manager meets it.
    pub fn mode(&self) -> SessionMode {
        SessionMode {
            desktop: self.desktop(),
            switchable: self.switchable,
            maximize: self.maximize,
            application: match self.application.load(Ordering::Relaxed) {
                AUTOSTART_RUNNING => Some(true),
                AUTOSTART_ENDED => Some(false),
                _ => None,
            },
        }
    }

    /// Note whether what the autostart started runs.
    fn set_application(&self, running: bool) {
        let state = if running {
            AUTOSTART_RUNNING
        } else {
            AUTOSTART_ENDED
        };
        self.application.store(state, Ordering::Relaxed);
    }

    /// Ask for the autostart to run again, once what it started has ended.
    pub fn relaunch(&self) {
        if self.application.load(Ordering::Relaxed) == AUTOSTART_ENDED {
            self.relaunch.store(true, Ordering::Relaxed);
        }
    }

    /// Whether the autostart runs again after everything it started exited.
    pub fn restart_app(&self) -> bool {
        self.restart_app
    }

    /// The desktop shell's program.
    pub fn shell(&self) -> &str {
        &self.shell
    }

    /// Switch to the desktop or to the application alone and keep the choice for the next
    /// session; refused where the session is not switchable.
    pub fn set_desktop(&self, on: bool) {
        if !self.switchable || self.desktop.swap(on, Ordering::Relaxed) == on {
            return;
        }
        log::info!(target: "wm", "session switched to {}", if on { "the desktop" } else { "the application alone" });
        let word = if on { "desktop\n" } else { "application\n" };
        let written = self
            .state_file
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&self.state_file, word));
        if let Err(e) = written {
            log::warn!(target: "wm", "{}: the mode is not kept: {e}", self.state_file.display());
        }
    }
}

/// The lock that is held while anything the autostart started runs: in the session's runtime
/// directory, else one per user under the temporary directory.
fn autostart_lock_path(env: &SessionEnv) -> PathBuf {
    match env.get("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir).join("selkies-autostart.lock"),
        // SAFETY: getuid has no preconditions and cannot fail.
        None => std::env::temp_dir().join(format!("selkies-autostart-{}.lock", unsafe { libc::getuid() })),
    }
}

/// Open the autostart lock, creating it, for one holder.
fn open_lock(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
}

/// Take the lock on an open file, waiting for it or not; false where another holds it.
fn take_lock(file: &std::fs::File, wait: bool) -> bool {
    let operation = if wait {
        libc::LOCK_EX
    } else {
        libc::LOCK_EX | libc::LOCK_NB
    };
    loop {
        // SAFETY: the descriptor is the open file's own for the length of the call.
        if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
            return true;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return false;
        }
    }
}

/// Whether something an earlier autostart started still holds the lock.
fn autostart_running(path: &Path) -> bool {
    open_lock(path).is_ok_and(|file| !take_lock(&file, false))
}

/// Wait until nothing holds the lock.
fn wait_for_autostart(path: &Path) {
    if let Ok(file) = open_lock(path) {
        take_lock(&file, true);
    }
}

/// Run the autostart file and wait until it and everything it started exited, by the lock its
/// processes inherit.
fn run_autostart_once(env: &SessionEnv, file: &Path, lock_path: &Path) -> std::io::Result<()> {
    let lock = open_lock(lock_path)?;
    if !take_lock(&lock, false) {
        drop(lock);
        wait_for_autostart(lock_path);
        return Ok(());
    }
    let fd = lock.as_raw_fd();
    let mut command = env.command("sh");
    command
        .arg(file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    // SAFETY: the hook runs in the forked child before exec and only calls fcntl on a
    // descriptor the child's table holds, which is async-signal-safe.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    drop(lock);
    let _ = child.wait();
    wait_for_autostart(lock_path);
    Ok(())
}

/// The first `autostart` file along the backend's configuration directories.
fn find_autostart(backend: Backend) -> Option<PathBuf> {
    let dirs = config::search_dirs(backend);
    config::find_file(&dirs, "autostart").or_else(|| config::find_file(&dirs, "autostart.sh"))
}

/// Run the session's application, the `autostart` file found along the backend's
/// configuration directories, with `sh` in the session environment, and run it again when the
/// policy asks, on a thread of its own.
///
/// What the autostart started is followed by a lock rather than by process ids: the autostart
/// inherits a descriptor holding an exclusive `flock` on the session's lock file, every process
/// it starts inherits it in turn, and the lock is free once the last of them exited. That
/// follows an application that forks and leaves, a browser's wrapper or a single-instance
/// launcher, without the server adopting or reaping anyone's processes, and it outlives the
/// server: one started again while the application an earlier server started still runs (an
/// X11 session, whose display is not the server's) finds the lock held and leaves that
/// application be rather than starting a second. A process that closes every descriptor it
/// inherited drops out of the count, as a daemon does that leaves its session.
///
/// The autostart runs again when the user asks ([`SessionControl::relaunch`]), and by itself
/// only while the session is the application alone and the policy restarts it, with the shell
/// supervisor's backoff: a run that ends within [`STABLE`] counts toward [`GIVE_UP`], so an
/// autostart that starts nothing does not loop.
pub fn start_autostart(env: Arc<Mutex<SessionEnv>>, backend: Backend, control: Arc<SessionControl>) {
    let result = std::thread::Builder::new()
        .name("selkies-wm-autostart".to_string())
        .spawn(move || {
            let lock_path = autostart_lock_path(&env.lock().unwrap_or_else(|p| p.into_inner()));
            let mut first = true;
            let mut failures = 0;
            loop {
                let Some(file) = find_autostart(backend) else {
                    if first {
                        log::info!(target: "wm", "no autostart file");
                    }
                    return;
                };
                control.set_application(true);
                let started = Instant::now();
                let earlier = first && autostart_running(&lock_path);
                if earlier {
                    log::info!(target: "wm", "autostart {}: still running from an earlier start, not run again", file.display());
                    wait_for_autostart(&lock_path);
                } else {
                    log::info!(target: "wm", "autostart {}", file.display());
                    let session = env.lock().unwrap_or_else(|p| p.into_inner()).clone();
                    if let Err(e) = run_autostart_once(&session, &file, &lock_path) {
                        log::warn!(target: "wm", "autostart {}: not run: {e}", file.display());
                    }
                }
                control.set_application(false);
                let ran = started.elapsed();
                if earlier || ran >= STABLE {
                    failures = 0;
                } else {
                    failures += 1;
                }
                first = false;
                let restarts = control.restart_app() && failures < GIVE_UP;
                if control.restart_app() && failures >= GIVE_UP {
                    log::error!(target: "wm", "autostart {}: exited {GIVE_UP} times right after starting; run again only when asked", file.display());
                } else if restarts {
                    log::info!(target: "wm", "autostart {}: everything it started exited after {:.0?}; running it again", file.display(), ran);
                } else {
                    log::info!(target: "wm", "autostart {}: everything it started exited after {:.0?}", file.display(), ran);
                }
                let backoff = Instant::now() + Duration::from_secs(1 << failures.min(4));
                loop {
                    if control.relaunch.swap(false, Ordering::Relaxed) {
                        failures = 0;
                        break;
                    }
                    if restarts && !control.desktop() && Instant::now() >= backoff {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
        });
    if let Err(e) = result {
        log::error!(target: "wm", "autostart thread not started: {e}");
    }
}

/// The launcher the engine runs `Execute` actions and pipe menus through: `sh -c` in the
/// session environment, detached, output to the log's stream.
pub struct ShellLauncher {
    env: Arc<Mutex<SessionEnv>>,
    control: Arc<SessionControl>,
}

impl ShellLauncher {
    /// A launcher over a shared environment, so the X display the compositor brings up later
    /// reaches programs started after it.
    pub fn new(env: Arc<Mutex<SessionEnv>>, control: Arc<SessionControl>) -> ShellLauncher {
        ShellLauncher { env, control }
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

    fn session(&self) -> SessionMode {
        self.control.mode()
    }

    fn set_desktop(&mut self, on: bool) {
        self.control.set_desktop(on);
    }

    fn relaunch(&mut self) {
        self.control.relaunch();
    }

    fn policy(&self) -> crate::config::Policy {
        self.control.wm
    }
}

/// How long a shell has to stay up before an exit counts as a crash to back off from rather
/// than a failure to start.
const STABLE: Duration = Duration::from_secs(10);
/// Immediate exits in a row after which the shell is given up on.
const GIVE_UP: u32 = 5;

/// How long a shell asked to stop for the application alone has before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(3);

/// The desktop shell as a supervised child: started while the session is the desktop,
/// restarted when it exits, stopped when the session switches to the application alone and with
/// the manager.
pub struct ShellSupervisor {
    env: Arc<Mutex<SessionEnv>>,
    control: Arc<SessionControl>,
    program: String,
    child: Option<Child>,
    started: Option<Instant>,
    /// When the running shell was asked to stop.
    stopping: Option<Instant>,
    failures: u32,
    next_start: Instant,
    given_up: bool,
}

impl ShellSupervisor {
    /// A supervisor for the session's shell, not started yet.
    pub fn new(env: Arc<Mutex<SessionEnv>>, control: Arc<SessionControl>) -> ShellSupervisor {
        let program = control.shell().to_string();
        ShellSupervisor {
            env,
            control,
            program,
            child: None,
            started: None,
            stopping: None,
            failures: 0,
            next_start: Instant::now(),
            given_up: false,
        }
    }

    /// Stop the shell for the application alone: asked with SIGTERM, killed after
    /// [`STOP_GRACE`], and a shell given up on is tried afresh once the desktop is back.
    fn stop(&mut self, now: Instant) -> Instant {
        self.given_up = false;
        self.failures = 0;
        self.next_start = now;
        let Some(child) = self.child.as_mut() else {
            return now + Duration::from_millis(250);
        };
        if matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
            log::info!(target: "wm", "{} stopped for the application alone", self.program);
            self.child = None;
            self.stopping = None;
            return now + Duration::from_millis(250);
        }
        match self.stopping {
            None => {
                if let Ok(pid) = i32::try_from(child.id()) {
                    // SAFETY: kill has no memory preconditions; the pid is our own unreaped child.
                    unsafe {
                        libc::kill(pid, libc::SIGTERM);
                    }
                }
                self.stopping = Some(now);
            }
            Some(asked) if now.saturating_duration_since(asked) >= STOP_GRACE => {
                let _ = child.kill();
            }
            Some(_) => {}
        }
        now + Duration::from_millis(100)
    }

    /// Start the shell if the session is the desktop, it is not running, and its backoff
    /// passed; reap it when it exited; stop it while the session is the application alone.
    /// Called on the manager's ticks; returns when it next wants to be called.
    pub fn poll(&mut self, now: Instant) -> Instant {
        if !self.control.desktop() {
            return self.stop(now);
        }
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
                    if self.stopping.take().is_some() {
                        self.next_start = now;
                        return now;
                    }
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

    /// A fresh directory of the test's own under the temporary directory.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("selkies-wm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A switchable policy for a shell program, starting in the desktop or not.
    fn policy(shell: &str, desktop: bool) -> SessionPolicy {
        SessionPolicy {
            desktop,
            locked: false,
            switch: true,
            maximize: true,
            restart_app: false,
            shell: shell.to_string(),
            wm: crate::config::Policy::default(),
        }
    }

    /// A user's switch is kept for the next session, which starts in it, unless the image
    /// locks its own mode.
    #[test]
    fn a_kept_choice_starts_the_next_session_unless_the_mode_is_locked() {
        let dir = scratch("kept-choice");
        let state = dir.join("selkies-wm/session");
        let first = SessionControl::with_state_file(policy("sh", false), state.clone());
        assert!(first.mode().switchable);
        first.set_desktop(true);
        assert_eq!(std::fs::read_to_string(&state).unwrap(), "desktop\n");
        let next = SessionControl::with_state_file(policy("sh", false), state.clone());
        assert!(next.desktop());
        let locked = SessionControl::with_state_file(
            SessionPolicy {
                locked: true,
                ..policy("sh", false)
            },
            state,
        );
        assert!(!locked.desktop());
        assert!(!locked.mode().switchable);
        locked.set_desktop(true);
        assert!(!locked.desktop());
    }

    /// A session whose shell is not installed never offers a desktop it cannot start.
    #[test]
    fn a_missing_shell_is_never_offered() {
        let dir = scratch("missing-shell");
        let control =
            SessionControl::with_state_file(policy("selkies-no-such-shell", false), dir.join("session"));
        assert!(!control.mode().switchable);
    }

    /// The autostart counts as running until the last process it started exited, a
    /// background one it left behind included, and a second start meanwhile finds it running.
    #[test]
    fn the_autostart_runs_until_what_it_left_behind_exits() {
        let dir = scratch("autostart-lock");
        let lock = dir.join("lock");
        let file = dir.join("autostart");
        std::fs::write(&file, "sleep 1 &\n").unwrap();
        let env = SessionEnv::new(None, None);
        let (lock_for_run, file_for_run) = (lock.clone(), file.clone());
        let run = std::thread::spawn(move || {
            let started = Instant::now();
            run_autostart_once(&env, &file_for_run, &lock_for_run).unwrap();
            started.elapsed()
        });
        std::thread::sleep(Duration::from_millis(300));
        assert!(autostart_running(&lock));
        let ran = run.join().unwrap();
        assert!(ran >= Duration::from_millis(900), "returned after {ran:?}");
        assert!(!autostart_running(&lock));
    }

    /// The shell runs only while the session is the desktop: a switch to the application
    /// alone stops it, and one back starts it again.
    #[test]
    fn the_shell_follows_the_mode() {
        let dir = scratch("shell-follows");
        let shell = dir.join("shell");
        std::fs::write(&shell, "#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&shell, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let control = Arc::new(SessionControl::with_state_file(
            policy(&shell.to_string_lossy(), true),
            dir.join("session"),
        ));
        let env = Arc::new(Mutex::new(SessionEnv::new(None, None)));
        let mut supervisor = ShellSupervisor::new(env, control.clone());
        let mut now = Instant::now();
        supervisor.poll(now);
        assert!(supervisor.running());
        control.set_desktop(false);
        for _ in 0..50 {
            now += Duration::from_millis(100);
            supervisor.poll(now);
            if !supervisor.running() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!supervisor.running());
        control.set_desktop(true);
        supervisor.poll(now);
        assert!(supervisor.running());
    }

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
        let dir = scratch("shell-gives-up");
        let control = Arc::new(SessionControl::with_state_file(
            policy("false", true),
            dir.join("session"),
        ));
        let mut supervisor = ShellSupervisor::new(env, control);
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
