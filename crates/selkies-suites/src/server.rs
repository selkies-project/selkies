/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The server under test, as a process the test owns.
//!
//! [`launch`] describes a server and [`Launch::start`] runs it: the environment is built from
//! nothing, so no variable of the shell running the suites reconfigures it; the port is one the
//! kernel just handed out, the home and the runtime directory are the run's own under the work
//! directory, and the X display is `E2E_DISPLAY`, never `DISPLAY`, because the suites resize the
//! root and inject input. The server is a session leader that the kernel kills with the thread
//! that started it, and [`Server`] ends its process group when dropped, so neither a panic nor a
//! killed run leaves one behind.
//!
//! Servers of one test binary take turns ([`SESSION`]): libtest runs tests on parallel threads,
//! and two servers on one display would resize it and scale it under each other.

use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, MutexGuard};

use crate::{http, on_path, private_dir, repo, server_binary, suite, tail, workdir};

/// Held by the one server of this test binary that is running.
static SESSION: Mutex<()> = Mutex::const_new(());

/// How long a server is given to answer `/api/status` after it starts.
const STARTUP: Duration = Duration::from_secs(75);
/// How long a server is given to exit on `SIGTERM` before its group is killed.
const SHUTDOWN: Duration = Duration::from_secs(10);
/// How long `pactl` is given to answer: a sound server that accepts a connection and then stops
/// answering would otherwise hold the test until the run's own timeout.
const PACTL: Duration = Duration::from_secs(10);
/// How far apart the pages of one address must connect: the server refuses a connection that
/// follows another from its address within half a second as a reconnect storm, and every page of
/// a suite comes from the loopback address.
const BETWEEN_PAGES: Duration = Duration::from_millis(600);
/// How often a condition on the log or on a process is looked at again.
const POLL: Duration = Duration::from_millis(50);

/// A server to start: its transport, its capture backend, and what its environment adds.
pub struct Launch {
    label: String,
    mode: String,
    wayland: bool,
    display: Option<String>,
    env: Vec<(String, String)>,
}

/// A WebSockets server on the X test display, whose log is named after `label`.
pub fn launch(label: &str) -> Launch {
    Launch {
        label: label.to_owned(),
        mode: "websockets".to_owned(),
        wayland: false,
        display: None,
        env: Vec::new(),
    }
}

impl Launch {
    /// Capture from the server's own Wayland compositor in place of the X test display.
    pub fn wayland(mut self, wayland: bool) -> Launch {
        self.wayland = wayland;
        self
    }

    /// Serve over `mode` (`websockets` or `webrtc`).
    pub fn mode(mut self, mode: &str) -> Launch {
        mode.clone_into(&mut self.mode);
        self
    }

    /// Capture the X display `display` (a private server's) in place of the test display.
    pub fn display(mut self, display: &str) -> Launch {
        self.display = Some(display.to_owned());
        self
    }

    /// Set `name` in the server's environment, over what the harness sets.
    pub fn env(mut self, name: &str, value: &str) -> Launch {
        self.env.push((name.to_owned(), value.to_owned()));
        self
    }

    /// Start the server and wait until `/api/status` answers.
    ///
    /// # Panics
    ///
    /// As [`Launch::spawn`] and [`Server::ready`] do.
    pub async fn start(self) -> Server {
        let server = self.spawn().await;
        server.ready().await;
        server
    }

    /// Start the server and return at once, for a test that meets it while it comes up;
    /// [`Server::ready`] is the wait [`Launch::start`] adds.
    ///
    /// Called from the test's own thread (a `#[tokio::test]` of the default flavor): the kernel
    /// ties the server's life to the thread that started it.
    ///
    /// # Panics
    ///
    /// When there is no server to run or no `E2E_DISPLAY` for an X11 server.
    pub async fn spawn(self) -> Server {
        let turn = SESSION.lock().await;
        watch_the_deadline();
        pulse_setup();
        let binary = server_binary();
        let binary = if binary.components().count() == 1 {
            on_path(&binary.to_string_lossy()).unwrap_or(binary)
        } else {
            binary
        };
        assert!(
            binary.is_file(),
            "no selkies server to run at {}: set SELKIES_TEST_SERVER or build target/release/selkies",
            binary.display()
        );
        let port = free_port();
        let log = log_path(&self.label);
        let sink = File::create(&log).expect("the server log");
        let mut command = Command::new(&binary);
        command
            // Any argument runs the server alone; a bare selkies is the standalone desktop
            .arg(format!("--mode={}", self.mode))
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", private_dir("home"))
            .env("XDG_RUNTIME_DIR", private_dir("run"))
            .env("SELKIES_PORT", port.to_string())
            .env("SELKIES_ENABLE_BASIC_AUTH", "false")
            .env("SELKIES_MODE", &self.mode)
            .env("SELKIES_WEB_ROOT", repo().join("web/selkies-web-core/dist"))
            .env(
                "SELKIES_WAYLAND",
                if self.wayland { "true" } else { "false" },
            )
            .env("SELKIES_ENABLE_HTTPS", "false");
        // GPU visibility passes through, so `CUDA_VISIBLE_DEVICES=` keeps the encoders off a
        // host's GPUs.
        for name in ["CUDA_VISIBLE_DEVICES", "CUDA_DEVICE_ORDER"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        if !self.wayland {
            command.env("DISPLAY", self.display.clone().unwrap_or_else(test_display));
        }
        if let Some(pulse) = pulse_server() {
            command.env("PULSE_SERVER", pulse);
        }
        command
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .current_dir(workdir())
            .stdin(Stdio::null())
            .stdout(sink.try_clone().expect("the server log, for stdout"))
            .stderr(sink);
        tie_to_this_thread(&mut command);
        let child = command
            .spawn()
            .unwrap_or_else(|e| panic!("{} did not start: {e}", binary.display()));
        running_logs(|logs| logs.push(log.clone()));
        Server {
            pid: child.id(),
            child: std::sync::Mutex::new(child),
            port,
            log,
            last_page: std::sync::Mutex::new(None),
            pages: std::sync::Mutex::new(Vec::new()),
            _turn: turn,
        }
    }
}

/// Make `command`'s process a session leader the kernel kills with the thread that starts it, so
/// an externally killed run (a timeout skips every destructor) leaves nothing on the display.
pub(crate) fn tie_to_this_thread(command: &mut Command) {
    let parent = libc::pid_t::try_from(std::process::id()).expect("a process id");
    // SAFETY: the calls are async-signal-safe and touch nothing the parent shares; the closure
    // allocates nothing.
    unsafe {
        command.pre_exec(move || {
            libc::setsid();
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // A parent that died between the fork and the prctl left no signal behind.
            if libc::getppid() != parent {
                return Err(std::io::Error::other("the test process is gone"));
            }
            Ok(())
        });
    }
}

/// What a page's socket task sends next; `None` closes the socket.
pub(crate) type PageSender = tokio::sync::mpsc::UnboundedSender<Option<tokio_websockets::Message>>;

/// A running server. Dropping it ends the server's process group; [`Server::stop`] is the orderly
/// end, and the drop what a panic leaves.
pub struct Server {
    child: std::sync::Mutex<Child>,
    pid: u32,
    port: u16,
    log: PathBuf,
    /// When the last page set out to connect.
    last_page: std::sync::Mutex<Option<Instant>>,
    /// The pages that connected, for [`Server::stop`] to close.
    pages: std::sync::Mutex<Vec<PageSender>>,
    _turn: MutexGuard<'static, ()>,
}

impl Server {
    /// The port every route of the server is on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The data WebSocket's address, with `query` (`?role=viewer`, or nothing) appended.
    pub fn websocket(&self, query: &str) -> String {
        format!("ws://127.0.0.1:{}/api/websockets{query}", self.port)
    }

    /// Wait until a page may connect without being taken for a reconnect storm
    /// ([`BETWEEN_PAGES`]), and count the page that asked as the latest.
    pub(crate) async fn page_turn(&self) {
        let ready = {
            let mut last = self
                .last_page
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let ready =
                last.map_or_else(Instant::now, |at| (at + BETWEEN_PAGES).max(Instant::now()));
            *last = Some(ready);
            ready
        };
        tokio::time::sleep_until(ready.into()).await;
    }

    /// The server's exit status once it has exited.
    pub fn exited(&self) -> Option<std::process::ExitStatus> {
        self.child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .try_wait()
            .ok()
            .flatten()
    }

    /// Wait for `/api/status`.
    ///
    /// # Panics
    ///
    /// When the server exits or stays silent through its startup; the message carries its log.
    pub async fn ready(&self) {
        let deadline = Instant::now() + STARTUP;
        loop {
            let answer = http::request(
                self.port,
                "GET",
                "/api/status",
                &[],
                &[],
                Duration::from_secs(2),
            )
            .await;
            if matches!(answer, Ok(ref response) if response.status == 200) {
                return;
            }
            if let Some(status) = self.exited() {
                panic!(
                    "selkies exited during startup ({status}):\n{}",
                    self.log_tail()
                );
            }
            assert!(
                Instant::now() < deadline,
                "selkies did not answer /api/status within {STARTUP:?}:\n{}",
                self.log_tail()
            );
            tokio::time::sleep(POLL).await;
        }
    }

    /// The length of the log now, in bytes: a mark for [`Server::saw`] and [`Server::log_from`].
    pub fn mark(&self) -> usize {
        std::fs::metadata(&self.log).map_or(0, |m| usize::try_from(m.len()).unwrap_or(usize::MAX))
    }

    /// The log from byte `mark` on.
    pub fn log_from(&self, mark: usize) -> String {
        let bytes = std::fs::read(&self.log).unwrap_or_default();
        String::from_utf8_lossy(bytes.get(mark..).unwrap_or_default()).into_owned()
    }

    /// The whole log.
    pub fn log(&self) -> String {
        self.log_from(0)
    }

    /// The end of the log, for a failure message.
    pub fn log_tail(&self) -> String {
        tail(&self.log(), 60)
    }

    /// When `needle` reached the log at or after `mark`, waiting up to `timeout` for it; `None`
    /// when it did not.
    pub async fn saw_at(&self, mark: usize, needle: &str, timeout: Duration) -> Option<Instant> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.log_from(mark).contains(needle) {
                return Some(Instant::now());
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Whether `needle` reaches the log at or after `mark` within `timeout`.
    pub async fn saw(&self, mark: usize, needle: &str, timeout: Duration) -> bool {
        self.saw_at(mark, needle, timeout).await.is_some()
    }

    /// Whether `needle` stays out of the log from `mark` on for the whole of `window`.
    pub async fn stays_without(&self, mark: usize, needle: &str, window: Duration) -> bool {
        !self.saw(mark, needle, window).await
    }

    /// Give the server until `needle` reaches the log at or after `mark`, and `at_most` when it
    /// never does: the pause a step needs before the next one, which no check hangs on.
    pub async fn settle(&self, mark: usize, needle: &str, at_most: Duration) {
        self.saw(mark, needle, at_most).await;
    }

    /// Close the pages still connected, stop the server, and give the turn to the next one; the
    /// end of its log, for the failure message of the checks made on it.
    ///
    /// The server is asked to exit, as an operator stops it, and killed only when it does not
    /// within [`SHUTDOWN`].
    pub async fn stop(self) -> String {
        for page in self
            .pages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
        {
            let _ = page.send(None);
        }
        self.signal(libc::SIGTERM);
        let deadline = Instant::now() + SHUTDOWN;
        while Instant::now() < deadline && self.exited().is_none() {
            tokio::time::sleep(POLL).await;
        }
        self.log_tail()
    }

    /// Count `page` among the pages [`Server::stop`] closes.
    pub(crate) fn attach(&self, page: PageSender) {
        self.pages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(page);
    }

    /// Send `signal` to the server's process group, which it leads.
    fn signal(&self, signal: libc::c_int) {
        if let Ok(pid) = libc::pid_t::try_from(self.pid) {
            // SAFETY: a signal to a process group this test started; no memory is involved.
            unsafe {
                libc::kill(-pid, signal);
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.exited().is_none() {
            // A test that panicked still holds its pages' sockets on this thread, and a server
            // waits for those: it is given a moment, not the whole of SHUTDOWN.
            self.signal(libc::SIGTERM);
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline && self.exited().is_none() {
                std::thread::sleep(POLL);
            }
        }
        // Whatever the server started and left in its group goes with it.
        self.signal(libc::SIGKILL);
        let _ = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .wait();
        running_logs(|logs| logs.retain(|log| *log != self.log));
    }
}

/// Run `with` on the logs of the servers of this process that are running.
fn running_logs(with: impl FnOnce(&mut Vec<PathBuf>)) {
    static RUNNING: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());
    with(
        &mut RUNNING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
}

/// Hold the suite to `SELKIES_SUITE_DEADLINE`, the budget in seconds its runner gives it: at the
/// deadline the end of every running server's log is printed and the process exits, taking its
/// servers with it. The runner's own kill, a minute later, would leave no such record; libtest
/// names the tests still running as they pass a minute, and every wait of the harness is bounded,
/// so what is left to say is what the servers were doing.
fn watch_the_deadline() {
    static WATCHING: std::sync::Once = std::sync::Once::new();
    WATCHING.call_once(|| {
        let Some(budget) = std::env::var("SELKIES_SUITE_DEADLINE")
            .ok()
            .and_then(|seconds| seconds.parse::<f64>().ok())
            .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
        else {
            return;
        };
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs_f64(budget));
            let mut said = format!("FAIL  [{}] the suite ran past {budget:.0}s\n", suite());
            running_logs(|logs| {
                for log in logs.iter() {
                    let text = String::from_utf8_lossy(&std::fs::read(log).unwrap_or_default())
                        .into_owned();
                    said.push_str(&format!("--- {} ---\n{}\n", log.display(), tail(&text, 40)));
                }
            });
            // Straight to the descriptor: libtest holds what a running test prints.
            let _ = std::io::Write::write_all(&mut std::io::stderr(), said.as_bytes());
            std::process::exit(124);
        });
    });
}

/// The X display the suites drive.
///
/// # Panics
///
/// When `E2E_DISPLAY` is unset.
pub fn test_display() -> String {
    std::env::var("E2E_DISPLAY")
        .ok()
        .filter(|d| !d.is_empty())
        .expect(
            "E2E_DISPLAY is not set: point it at a throwaway X server for the suites to drive \
             (see tests/README.md). DISPLAY is not inherited, because these suites resize the \
             root and inject input.",
        )
}

/// A loopback port nothing is listening on, from the kernel's own pool.
fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|probe| probe.local_addr())
        .expect("a free loopback port")
        .port()
}

/// The log of the next server named `label`: `<suite>-<label>.log` in the work directory, and
/// `<suite>-<label>.<n>.log` for each further server of that name in this process.
fn log_path(label: &str) -> PathBuf {
    static HANDED_OUT: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let stem = format!("{}-{label}", suite());
    let mut handed_out = HANDED_OUT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let earlier = handed_out.iter().filter(|s| **s == stem).count();
    handed_out.push(stem.clone());
    workdir().join(if earlier == 0 {
        format!("{stem}.log")
    } else {
        format!("{stem}.{earlier}.log")
    })
}

/// Run `command` to its end within `limit`; its standard output, or `None` when it could not
/// run, failed, or was still running at the limit (it is killed then).
pub fn output_within(command: &mut Command, limit: Duration) -> Option<String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let output = child.wait_with_output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `pactl` with `args`, bounded.
fn pactl(pactl: &Path, args: &[&str]) -> Option<String> {
    output_within(Command::new(pactl).args(args), PACTL)
}

/// Make sure the `output` null sink the audio capture reads through its monitor exists and is the
/// default, as the deployed layout has it. Without `pactl` the sink has to exist already; only
/// the audio checks depend on it.
fn pulse_setup() {
    let Some(tool) = on_path("pactl") else {
        return;
    };
    let Some(sources) = pactl(&tool, &["list", "short", "sources"]) else {
        return;
    };
    if !sources.contains("output.monitor") {
        pactl(
            &tool,
            &[
                "load-module",
                "module-null-sink",
                "sink_name=output",
                "rate=48000",
                "channels=2",
            ],
        );
    }
    pactl(&tool, &["set-default-sink", "output"]);
}

/// The sound server's address as libpulse takes it in `PULSE_SERVER`: the one the environment
/// names, else the one `pactl info` reports. The server runs under a runtime directory of the
/// run's own, where libpulse would look for a sound server that is not there.
fn pulse_server() -> Option<String> {
    if let Ok(named) = std::env::var("PULSE_SERVER")
        && !named.is_empty()
    {
        return Some(named);
    }
    let info = pactl(&on_path("pactl")?, &["info"])?;
    info.lines()
        .find_map(|line| line.strip_prefix("Server String:"))
        .map(|address| address.trim().to_owned())
}
