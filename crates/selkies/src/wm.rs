/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The Selkies window manager and desktop in the server: what `--wm` and `--desktop` start.
//!
//! Under `--wm` the server owns the session's window management on the backend it runs on. On
//! Wayland the window manager's engine is installed in the capture compositor as its window
//! policy and Xwayland is started for the X11 clients, so nothing nests inside the compositor;
//! on X11 the engine runs on a thread of its own with its own connection to the Xvfb and claims
//! the screen's manager selection. Under `--desktop` the `selkies-desktop` shell is started on top
//! and kept running, as its own process, once the display it needs is up: the compositor's
//! socket and Xwayland's display on Wayland, the X display on X11.
//!
//! Every program the manager starts, the shell included, shares one environment: the display
//! sockets, the session bus (started here where the environment names none), and the session
//! type, so what the menus and the shell launch finds the same desktop. The backend's
//! `autostart` file runs once the manager is up, with that environment. The handles live for
//! the process, since the manager does: a session's window management is never handed back.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use log::{error, info, warn};
use selkies_wm::config::Backend;
use selkies_wm::session::{SessionBus, SessionEnv, ShellLauncher, ShellSupervisor, run_autostart};

use crate::settings::settings;

/// The shell binary the server starts under `--desktop`.
const SHELL: &str = "selkies-desktop";

/// How long the shell waits for Xwayland's display before it starts without one.
const XWAYLAND_WAIT: Duration = Duration::from_secs(15);

/// What the mode keeps alive: the session bus, and on X11 the manager's thread.
struct Session {
    _bus: Option<SessionBus>,
    _x11: Option<selkies_wm::x11::ManagerHandle>,
    /// Whether the capture compositor runs the policy and its Xwayland.
    wayland: bool,
}

/// Whether the capture compositor manages the session's X11 clients itself: its Xwayland is
/// rootless, bridged, and no screen of its own, so a second display is a second output and the
/// selection watchers have nothing to add.
pub fn owns_x11_session() -> bool {
    SESSION
        .get()
        .and_then(|slot| {
            slot.lock()
                .ok()
                .map(|s| s.as_ref().is_some_and(|s| s.wayland))
        })
        .unwrap_or(false)
}

static SESSION: OnceLock<Mutex<Option<Session>>> = OnceLock::new();

/// Start the window manager and, under `--desktop`, the shell, for the backend in use. Runs on
/// a blocking thread at startup, after the display is up and before the transports bind.
pub fn start(is_wayland: bool) {
    let (wm, desktop) = {
        let s = settings();
        (s.wm().on, s.desktop().on)
    };
    if !wm && !desktop {
        return;
    }
    let wayland_display = is_wayland
        .then(|| std::env::var("WAYLAND_DISPLAY").ok())
        .flatten();
    let x11_display = (!is_wayland)
        .then(|| std::env::var("DISPLAY").ok())
        .flatten();
    let mut env = SessionEnv::new(wayland_display.as_deref(), x11_display.as_deref());
    let bus = SessionBus::ensure(&mut env);
    let env = Arc::new(Mutex::new(env));
    let shell = desktop.then(|| SHELL.to_string());
    let x11 = if is_wayland {
        if !start_wayland(env.clone(), shell) {
            return;
        }
        None
    } else {
        let Some(display) = x11_display else {
            error!(target: "wm", "window manager not started: no DISPLAY");
            return;
        };
        match selkies_wm::x11::spawn(&display, env.clone(), shell) {
            Ok(handle) => {
                info!(target: "wm", "window manager on {display}{}", if desktop { " with the desktop" } else { "" });
                autostart(&env);
                Some(handle)
            }
            Err(e) => {
                error!(target: "wm", "window manager not started: {e}");
                None
            }
        }
    };
    let slot = SESSION.get_or_init(|| Mutex::new(None));
    *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(Session {
        _bus: bus,
        _x11: x11,
        wayland: is_wayland,
    });
}

/// Run the `autostart` file where one exists.
fn autostart(env: &Arc<Mutex<SessionEnv>>) {
    let env = env.lock().unwrap_or_else(|p| p.into_inner()).clone();
    if let Some(file) = run_autostart(&env) {
        info!(target: "wm", "autostart: {}", file.display());
    }
}

/// Install the engine in the compositor, start Xwayland, and run the shell once Xwayland's
/// display is known, on a thread of its own, after the `autostart` file. False where the
/// policy could not be installed.
fn start_wayland(env: Arc<Mutex<SessionEnv>>, shell: Option<String>) -> bool {
    let launcher = ShellLauncher::new(env.clone());
    let engine = selkies_wm::Engine::new(Backend::Wayland, Box::new(launcher));
    if let Err(e) = pixelflux::install_window_policy(Box::new(engine)) {
        error!(target: "wm", "window manager not installed: {e}");
        return false;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    if let Err(e) = pixelflux::start_xwayland(Some(tx)) {
        warn!(target: "wm", "Xwayland not started: {e}; X11 clients have no server");
    }
    info!(target: "wm", "window manager on the capture compositor{}", if shell.is_some() { " with the desktop" } else { "" });
    let result = std::thread::Builder::new()
        .name("selkies-wm-session".to_string())
        .spawn(move || {
            match rx.recv_timeout(XWAYLAND_WAIT) {
                Ok(display) => {
                    env.lock().unwrap_or_else(|p| p.into_inner()).set("DISPLAY", &format!(":{display}"));
                    info!(target: "wm", "Xwayland on display :{display}");
                }
                Err(_) => warn!(target: "wm", "Xwayland gave no display within {XWAYLAND_WAIT:?}; the desktop starts without one"),
            }
            autostart(&env);
            let Some(program) = shell else {
                return;
            };
            let mut supervisor = ShellSupervisor::new(env, &program);
            loop {
                let next = supervisor.poll(Instant::now());
                std::thread::sleep(next.saturating_duration_since(Instant::now()).max(Duration::from_millis(200)));
            }
        });
    if let Err(e) = result {
        error!(target: "wm", "session thread not started: {e}");
    }
    true
}
