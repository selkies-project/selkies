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
//! sockets, the session bus (started here where the environment names none), the session
//! type, and the device interposers the server keeps out of its own process
//! ([`application_preload`]), so what the menus and the shell launch finds the same desktop and
//! the session's gamepads and camera. The bus is started in that environment too, so an
//! application it activates gets them as well. The backend's
//! `autostart` file is the session's application: it runs once the manager is up, with that
//! environment, and again when everything it started exited where `--app-restart` asks
//! ([`selkies_wm::session::start_autostart`]). The handles live for the process, since the
//! manager does: a session's window management is never handed back.
//!
//! The session is the desktop or the application alone, `--desktop` deciding which a session
//! starts in. Unless `--session-switch` is off or the desktop setting is locked, the window
//! manager's root menu and window menu switch between the two at runtime: the shell is started
//! or stopped, the application keeps running, and the choice is kept for the next session
//! ([`selkies_wm::session::SessionControl`]).

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use log::{error, info, warn};
use selkies_wm::config::{Backend, Policy};
use selkies_wm::frame::Buttons;
use selkies_wm::session::{
    SessionBus, SessionControl, SessionEnv, SessionPolicy, ShellLauncher, ShellSupervisor,
    start_autostart,
};

use crate::commands::session::application_preload;
use crate::settings::settings;

/// The shell binary the server starts under `--desktop`.
const SHELL: &str = "selkies-desktop";

/// How long the shell waits for Xwayland's display before it starts without one.
const XWAYLAND_WAIT: Duration = Duration::from_secs(15);

/// What the mode keeps alive: the session bus, and on X11 the manager's thread.
struct Session {
    _bus: Option<SessionBus>,
    _x11: Option<selkies_wm::x11::ManagerHandle>,
    /// What reaches the X11 manager's thread.
    x11_remote: Option<selkies_wm::x11::Remote>,
    /// The environment the session's programs are started in.
    env: Arc<Mutex<SessionEnv>>,
    /// Whether the capture compositor runs the policy and its Xwayland.
    wayland: bool,
}

/// The desktop as the window manager holds it: every window's place, state, and stacking, in
/// logical layout coordinates with each output's scale. `None` where no window manager runs
/// (`--wm` off, or one that failed or panicked), which is how a consumer learns that only the
/// picture describes the session. Blocks for up to two seconds on the thread that owns the
/// window manager, so an async caller runs it on a blocking thread.
pub fn desktop_view() -> Option<pixelflux::wm::DesktopView> {
    let (wayland, x11_remote) = backend()?;
    if wayland {
        pixelflux::desktop_view()
    } else {
        x11_remote?.view()
    }
}

/// Ask the window manager to act on a window (activate, close, minimize, and the like), as a
/// taskbar asks. False where no window manager runs or it does not know the window. Blocks as
/// [`desktop_view`] does.
pub fn window_request(id: pixelflux::wm::WindowId, request: pixelflux::wm::Request) -> bool {
    match backend() {
        Some((true, _)) => pixelflux::window_request(id, request),
        Some((false, Some(remote))) => remote.request(id, request),
        _ => false,
    }
}

/// The address of the session bus the session's programs are started on, which is the one the
/// window manager started where the server's own environment named none; `None` where no
/// window manager runs.
pub fn session_bus_address() -> Option<String> {
    let env = SESSION.get()?.lock().ok()?.as_ref()?.env.clone();
    let env = env.lock().ok()?;
    env.get("DBUS_SESSION_BUS_ADDRESS").map(str::to_string)
}

/// The environment the session's programs are started in, as it stands now; `None` where no
/// window manager runs.
pub fn session_env() -> Option<SessionEnv> {
    let env = SESSION.get()?.lock().ok()?.as_ref()?.env.clone();
    let env = env.lock().ok()?;
    Some(env.clone())
}

/// Which backend the window manager runs on (true for the capture compositor) and, on X11,
/// what reaches its thread; `None` where the session started none.
fn backend() -> Option<(bool, Option<selkies_wm::x11::Remote>)> {
    let slot = SESSION.get()?.lock().ok()?;
    let session = slot.as_ref()?;
    Some((session.wayland, session.x11_remote.clone()))
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
    let (wm, desktop, policy) = {
        let s = settings();
        let desktop = s.desktop();
        let policy = SessionPolicy {
            desktop: desktop.on,
            locked: desktop.locked,
            switch: s.session_switch().on,
            maximize: s.wm_maximize().on,
            keep_maximized: s.wm_maximize().locked,
            restart_app: s.app_restart().on,
            shell: SHELL.to_string(),
            wm: Policy {
                decorations: s.wm_decorations().on,
                buttons: Buttons::parse(s.wm_buttons()),
                menus: s.wm_menus().on,
                keybinds: s.wm_keybinds().on,
            },
        };
        (s.wm().on, desktop.on, policy)
    };
    if !wm && !desktop {
        return;
    }
    let control = SessionControl::new(policy);
    let wayland_display = is_wayland
        .then(|| std::env::var("WAYLAND_DISPLAY").ok())
        .flatten();
    let x11_display = (!is_wayland)
        .then(|| std::env::var("DISPLAY").ok())
        .flatten();
    let mut env = SessionEnv::new(wayland_display.as_deref(), x11_display.as_deref());
    let preload = application_preload(|var| env.get(var).map(str::to_string));
    if !preload.is_empty() {
        env.set("LD_PRELOAD", &preload);
    }
    let bus = SessionBus::ensure(&mut env);
    if settings().pelorus().on {
        enable_accessibility(env.get("DBUS_SESSION_BUS_ADDRESS"));
        env.set(selkies_draw::apps::DESCRIBED_ENV, "1");
        selkies_draw::apps::describe_applications();
    }
    let env = Arc::new(Mutex::new(env));
    let session_env = env.clone();
    let x11 = if is_wayland {
        if !start_wayland(env.clone(), control) {
            return;
        }
        None
    } else {
        let Some(display) = x11_display else {
            error!(target: "wm", "window manager not started: no DISPLAY");
            return;
        };
        match selkies_wm::x11::spawn(&display, env.clone(), control.clone()) {
            Ok(handle) => {
                info!(target: "wm", "window manager on {display}{}", mode_words(&control));
                start_autostart(env.clone(), Backend::X11, control);
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
        x11_remote: x11.as_ref().map(selkies_wm::x11::ManagerHandle::remote),
        env: session_env,
        _x11: x11,
        wayland: is_wayland,
    });
}

/// Turn accessibility on for the session before any of its applications start, which is when
/// Chromium-based ones decide whether to describe themselves at all (`pelorus::atspi`). The
/// session's environment then carries `selkies_draw::apps::DESCRIBED_ENV`, under which the
/// window manager's menus and the shell start those applications with the switch they fill
/// their trees under; one started any other way (a terminal, the `autostart` file) has to be
/// given the switch by what starts it.
fn enable_accessibility(session_bus: Option<&str>) {
    match pelorus::atspi::enable(session_bus) {
        Ok(_) => log::debug!(target: "pelorus", "accessibility is on for the session"),
        Err(e) => warn!(
            target: "pelorus",
            "applications will not describe their windows ({e}); Pelorus has screenshots alone"
        ),
    }
}

/// How the startup line names the session's mode.
fn mode_words(control: &SessionControl) -> &'static str {
    match (control.desktop(), control.mode().switchable) {
        (true, true) => " with the desktop, switchable to the application alone",
        (true, false) => " with the desktop",
        (false, true) => " with the application alone, switchable to the desktop",
        (false, false) => " with the application alone",
    }
}

/// Install the engine in the compositor, start Xwayland, and run the shell under the session's
/// mode once Xwayland's display is known, on a thread of its own, after the `autostart` file.
/// False where the policy could not be installed.
fn start_wayland(env: Arc<Mutex<SessionEnv>>, control: Arc<SessionControl>) -> bool {
    let launcher = ShellLauncher::new(env.clone(), control.clone());
    let engine = selkies_wm::Engine::new(Backend::Wayland, Box::new(launcher));
    if let Err(e) = pixelflux::install_window_policy(Box::new(engine)) {
        error!(target: "wm", "window manager not installed: {e}");
        return false;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    if let Err(e) = pixelflux::start_xwayland(Some(tx)) {
        warn!(target: "wm", "Xwayland not started: {e}; X11 clients have no server");
    }
    info!(target: "wm", "window manager on the capture compositor{}", mode_words(&control));
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
            start_autostart(env.clone(), Backend::Wayland, control.clone());
            let mut supervisor = ShellSupervisor::new(env, control);
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
