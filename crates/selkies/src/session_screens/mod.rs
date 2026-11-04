/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The session compositor's own screens, one implementation both transports share: the DPI ladder,
//! the second-display capability, the screens a display owns, and the apps runner.
//!
//! A nested session runs its applications on a compositor of its own, whose screens are not the
//! capture compositor's outputs, so everything a display's output is told has to reach that
//! screen as well. [`SessionScreens`] owns that half; the capture half is
//! `display::WaylandOutputs`, and [`layout`] runs the passes over the two in the order the
//! compositors require.
//!
//! - A DPI is an output scale on the session compositor, never Xft resources. Applications draw
//!   larger when the compositor they are on scales its own output, so a nested session is scaled
//!   through its output management and the capture keeps 1.0: scaling the capture instead would
//!   halve the logical size the session is handed and upscale the whole desktop. A session that
//!   manages no outputs for clients takes the capture output's scale, which it follows through
//!   the host window's preferred fractional scale (a nested KWin accepts a scale over its own
//!   output management and ignores it), and so does a plain session, where the capture output is
//!   the only screen there is. XWayland applications run in the compositor's logical space and
//!   are scaled with it.
//! - A second display on the nested backend needs a screen. The first rung is the control socket
//!   a labwc serves (`labwc.sock`, the patch the images carry), one newline-terminated command
//!   per connection answered by one JSON line. KWin serves no socket: its screens are
//!   `zkde_screencast_unstable_v1` virtual outputs through pixelflux, a rung offered when the
//!   socket is absent and its registry names the protocol, a read that changes nothing on the
//!   session and is kept per compositor instance (its socket's path, inode, and creation time).
//!   Whether that KWin registers a screen it grows is proven when a display asks for one. With
//!   neither rung, the screens the session opened at startup can be arranged for a display, the
//!   spare-screen hold keeping them out of the desktop's way until then. An X11 desktop in a
//!   rootful Xwayland is one X screen of a fixed size, which no second capture output extends;
//!   Wayland clients directly on the capture compositor need nothing, every capture output being
//!   a monitor of its own there.
//! - Each secondary display owns one screen, grown right before its capture output is created so
//!   that output adopts the screen's host window, one at a time under one lock: the compositor
//!   hands the newest parked host window to the next output that asks, and that pairing is what
//!   keeps every screen showing its own display. A screen is retired by the name the compositor
//!   coined, so an older display's departure never costs a newer one its screen, and the
//!   compositor's output-destroy path returns only that screen's windows to the primary.
//! - A session lays its desktop out once per applied configuration, so a mode and a scale land
//!   together where the size is known, and every screen is positioned in one configuration, since
//!   an arrangement applied a screen at a time passes through a state where two overlap.
//!
//! The optional rungs are taken only once detected, so what runs where can be read from the log.
//! A transport builds one [`SessionScreens`] ahead of its input handler, [`attach`es] the handler
//! once it exists, reaches the ladder through it, and forwards
//! [`input::InputHost::app_compositor_adopted`] to [`SessionScreens::compositor_adopted`] with its
//! [`ScreenTransport`].
//!
//! [`attach`es]: SessionScreens::attach
//! [`input::InputHost::app_compositor_adopted`]: crate::input::InputHost::app_compositor_adopted

pub mod apps;
pub mod layout;
pub mod verdict;

#[cfg(test)]
mod entrypoint_tests;
#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use log::{debug, info, warn};
use parking_lot::{Mutex, RwLock};
use pixelflux::ScreenCapture;
pub use pixelflux::wayland::AppScreen;
use serde_json::Value as Json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::core::PRIMARY_DISPLAY;
use crate::display::{Layouts, wayland_output_id};
use crate::settings::settings;

pub use apps::{APP_RUNNER, AppsRunner};

/// Basename of the session compositor's control socket in `XDG_RUNTIME_DIR`.
pub const SESSION_IPC_SOCKET: &str = "labwc.sock";
/// Size of a held spare session screen: small enough to leave the desktop's center on the screen
/// that is shown, large enough for a compositor to lay out.
pub const SPARE_SCREEN_SIZE: (i32, i32) = (320, 240);

/// How long a probe of the control socket waits on the connect.
const IPC_PROBE_TIMEOUT: Duration = Duration::from_secs(1);
/// How long a command waits on the connect.
const IPC_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a command waits on the compositor's reply.
const IPC_REPLY_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a grown screen's host window gets to park, so the output created next adopts it.
const SCREEN_PARK_WAIT: Duration = Duration::from_secs(5);
/// How often the parked host windows are counted during that wait.
const SCREEN_PARK_POLL: Duration = Duration::from_millis(50);

/// pixelflux's session-screen calls, one to one, each blocking on a short-lived Wayland client or
/// the capture compositor's thread, so [`SessionScreens`] runs them off the runtime. Implemented
/// for [`ScreenCapture`]; errors are the message pixelflux gave.
pub trait AppScreens: Send + Sync + 'static {
    /// Scale the `index`-th screen; false when the compositor manages no outputs for clients.
    fn set_app_output_scale(&self, display: &str, scale: f64, index: usize)
    -> Result<bool, String>;
    /// Give the `index`-th screen a mode and scale in one configuration; false as above.
    fn set_app_screen_geometry(
        &self,
        display: &str,
        width: i32,
        height: i32,
        scale: f64,
        index: usize,
    ) -> Result<bool, String>;
    /// Lay the screens out at `(x, y, width, height)` in screen order; how many were positioned.
    fn set_app_screen_layout(
        &self,
        display: &str,
        rects: Vec<(i32, i32, i32, i32)>,
    ) -> Result<usize, String>;
    /// The enabled screens in screen order; empty when the compositor manages no outputs.
    fn list_app_screens(&self, display: &str) -> Result<Vec<AppScreen>, String>;
    /// Whether the compositor's registry offers KWin's virtual outputs.
    fn app_screen_control_offered(&self, display: &str) -> Result<bool, String>;
    /// Grow a virtual output named `name`, held until removed.
    fn add_app_screen(
        &self,
        display: &str,
        name: &str,
        width: i32,
        height: i32,
        scale: f64,
    ) -> Result<(), String>;
    /// Close a virtual output this process grew; false when none has that name.
    fn remove_app_screen(&self, name: &str) -> Result<bool, String>;
    /// Hold the screens past the first `keep` at a size; how many were resized.
    fn hold_spare_app_screens(
        &self,
        display: &str,
        keep: usize,
        width: i32,
        height: i32,
    ) -> Result<usize, String>;
    /// The session's host windows the capture compositor holds parked, waiting for an output.
    fn parked_windows(&self) -> Result<usize, String>;
    /// The capture compositor's outputs, by id.
    fn capture_output_ids(&self) -> Result<Vec<u32>, String>;
}

fn text<T>(result: Result<T, pixelflux::Error>) -> Result<T, String> {
    result.map_err(|e| e.to_string())
}

impl AppScreens for ScreenCapture {
    fn set_app_output_scale(
        &self,
        display: &str,
        scale: f64,
        index: usize,
    ) -> Result<bool, String> {
        text(ScreenCapture::set_app_output_scale(
            self,
            display.to_string(),
            scale,
            index,
        ))
    }

    fn set_app_screen_geometry(
        &self,
        display: &str,
        width: i32,
        height: i32,
        scale: f64,
        index: usize,
    ) -> Result<bool, String> {
        text(ScreenCapture::set_app_screen_geometry(
            self,
            display.to_string(),
            width,
            height,
            scale,
            index,
        ))
    }

    fn set_app_screen_layout(
        &self,
        display: &str,
        rects: Vec<(i32, i32, i32, i32)>,
    ) -> Result<usize, String> {
        text(ScreenCapture::set_app_screen_layout(
            self,
            display.to_string(),
            rects,
        ))
    }

    fn list_app_screens(&self, display: &str) -> Result<Vec<AppScreen>, String> {
        text(ScreenCapture::list_app_screens(self, display.to_string()))
    }

    fn app_screen_control_offered(&self, display: &str) -> Result<bool, String> {
        text(ScreenCapture::app_screen_control_offered(
            self,
            display.to_string(),
        ))
    }

    fn add_app_screen(
        &self,
        display: &str,
        name: &str,
        width: i32,
        height: i32,
        scale: f64,
    ) -> Result<(), String> {
        text(ScreenCapture::add_app_screen(
            self,
            display.to_string(),
            name.to_string(),
            width,
            height,
            scale,
        ))
    }

    fn remove_app_screen(&self, name: &str) -> Result<bool, String> {
        text(ScreenCapture::remove_app_screen(self, name.to_string()))
    }

    fn hold_spare_app_screens(
        &self,
        display: &str,
        keep: usize,
        width: i32,
        height: i32,
    ) -> Result<usize, String> {
        text(ScreenCapture::hold_spare_app_screens(
            self,
            display.to_string(),
            keep,
            width,
            height,
        ))
    }

    fn parked_windows(&self) -> Result<usize, String> {
        Ok(text(ScreenCapture::list_windows(self))?
            .iter()
            .filter(|window| window.4)
            .count())
    }

    fn capture_output_ids(&self) -> Result<Vec<u32>, String> {
        Ok(text(ScreenCapture::list_outputs(self))?
            .iter()
            .map(|output| output.0)
            .collect())
    }
}

/// What the ladder reads about the session from the input handler, which resolves the compositor
/// the applications run under. Both resolving calls are throttled reads of a cache, rescanning
/// the runtime directory at most every couple of seconds.
pub trait AppCompositor: Send + Sync + 'static {
    /// Socket of the compositor applications run under.
    fn app_wayland_display(&self) -> String;
    /// Whether that compositor is distinct from the capture compositor.
    fn has_separate_app_compositor(&self) -> bool;
    /// The live rootful Xwayland hosting an X11 desktop on the capture compositor, if any.
    fn x11_session_display(&self) -> Option<String>;
    /// The last DPI a client synced, 96 until one has.
    fn system_dpi(&self) -> f64;
}

/// What a transport does when a session compositor is adopted after its displays connected.
#[async_trait]
pub trait ScreenTransport: Send + Sync + 'static {
    /// Run the scale ladder for every display, so the session takes `dpi` as its output scale and
    /// the capture output, which took it while the session was starting, drops back to 1.0.
    /// `dpi` is the primary's; each other display re-applies its own. Serialized against the
    /// transport's layout pass, since the adoption can land during the one that started the
    /// captures this restarts.
    async fn resync_session_scale(&self, dpi: i32);
    /// Re-read what backs a second display and re-announce the server settings, so a page shows
    /// the offer the session allows now rather than the one it allowed when the page connected.
    async fn republish_second_screen(&self);
}

/// The answer a compositor gave on the KWin rung, with the socket identity it was measured on.
struct KdeProbe {
    identity: (PathBuf, u64, i128),
    offered: bool,
}

/// The session compositor's screens as one transport's displays use them; see the module docs.
/// Hold it in the `Arc` the constructors return.
pub struct SessionScreens {
    backend: Option<Arc<dyn AppScreens>>,
    is_wayland: bool,
    runtime_dir: Option<PathBuf>,
    compositor: RwLock<Option<Weak<dyn AppCompositor>>>,
    /// Serializes growing and retiring: each screen is claimed by its own output before the next.
    grow_lock: tokio::sync::Mutex<()>,
    /// The screen each display owns, by the name the compositor coined.
    owned: Mutex<HashMap<String, String>>,
    /// The attached secondaries in layout order, the positions of a pre-provisioned session.
    display_order: Mutex<Vec<String>>,
    ipc_ok: AtomicBool,
    kde_ok: AtomicBool,
    screen_count: AtomicUsize,
    kde_probe: Mutex<Option<KdeProbe>>,
}

impl SessionScreens {
    /// The ladder for this server: pixelflux's session calls on the Wayland backend, nothing to
    /// do on X11. Reads the installed settings.
    pub fn for_server() -> Arc<SessionScreens> {
        let is_wayland = settings().wayland().on;
        let backend: Option<Arc<dyn AppScreens>> = if is_wayland {
            Some(Arc::new(ScreenCapture::new()))
        } else {
            None
        };
        Self::new(backend, is_wayland, None)
    }

    /// A ladder over `backend`, with the runtime directory the control socket lives in given, or
    /// `XDG_RUNTIME_DIR` read per use.
    pub fn new(
        backend: Option<Arc<dyn AppScreens>>,
        is_wayland: bool,
        runtime_dir: Option<PathBuf>,
    ) -> Arc<SessionScreens> {
        Arc::new(SessionScreens {
            backend,
            is_wayland,
            runtime_dir,
            compositor: RwLock::new(None),
            grow_lock: tokio::sync::Mutex::new(()),
            owned: Mutex::new(HashMap::new()),
            display_order: Mutex::new(Vec::new()),
            ipc_ok: AtomicBool::new(false),
            kde_ok: AtomicBool::new(false),
            screen_count: AtomicUsize::new(0),
            kde_probe: Mutex::new(None),
        })
    }

    /// Read the app compositor through `compositor`, the transport's input handler, built over
    /// this ladder's transport. Held weakly: the handler's host holds this ladder.
    pub fn attach<C: AppCompositor>(&self, compositor: &Arc<C>) {
        let shared: Arc<dyn AppCompositor> = compositor.clone();
        *self.compositor.write() = Some(Arc::downgrade(&shared));
    }

    fn app_compositor(&self) -> Option<Arc<dyn AppCompositor>> {
        self.compositor.read().as_ref()?.upgrade()
    }

    /// The runtime directory, for the control socket and a session socket given by name.
    fn runtime_dir(&self) -> Option<PathBuf> {
        self.runtime_dir.clone().or_else(|| {
            std::env::var_os("XDG_RUNTIME_DIR")
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from)
        })
    }

    /// Whether a host compositor is captured instead, which owns its screens outright.
    fn host_capture() -> bool {
        !settings().wayland_host_display().trim().is_empty()
    }

    /// `f` over the backend on a blocking thread; `None` without a backend or when the thread
    /// panicked.
    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&dyn AppScreens) -> Result<T, String> + Send + 'static,
    ) -> Option<Result<T, String>> {
        let backend = self.backend.clone()?;
        tokio::task::spawn_blocking(move || f(&*backend)).await.ok()
    }

    /// The app compositor's socket and whether it is nested, resolved off the runtime: a rescan
    /// lists the runtime directory and connects to each candidate.
    async fn resolve(&self) -> Option<(String, bool)> {
        let compositor = self.app_compositor()?;
        tokio::task::spawn_blocking(move || {
            (
                compositor.app_wayland_display(),
                compositor.has_separate_app_compositor(),
            )
        })
        .await
        .ok()
    }

    /// The control socket's path, off Wayland or without a runtime directory `None`.
    fn ipc_path(&self) -> Option<PathBuf> {
        if !self.is_wayland {
            return None;
        }
        Some(self.runtime_dir()?.join(SESSION_IPC_SOCKET))
    }

    /// The last probed answer to whether the control socket accepts connections.
    pub fn ipc_available(&self) -> bool {
        self.ipc_ok.load(Ordering::Relaxed)
    }

    /// Whether a session-screen control answered the last probe: the control socket, or KWin's
    /// virtual-output protocol.
    pub fn control_available(&self) -> bool {
        self.ipc_available() || self.kde_ok.load(Ordering::Relaxed)
    }

    /// The screens the last probe counted on a session with no control.
    pub fn probed_screen_count(&self) -> usize {
        self.screen_count.load(Ordering::Relaxed)
    }

    /// Probe the control socket and keep the answer. A connect rather than a stat, so a socket
    /// file a dead compositor left behind reads as absent.
    pub async fn probe_ipc(&self) -> bool {
        let mut ok = false;
        if let Some(path) = self.ipc_path() {
            ok = matches!(
                tokio::time::timeout(IPC_PROBE_TIMEOUT, UnixStream::connect(&path)).await,
                Ok(Ok(_))
            );
        }
        self.ipc_ok.store(ok, Ordering::Relaxed);
        ok
    }

    /// A socket as an identity (path, inode, creation time, zeros where none exists), so a
    /// restarted compositor reads as a new one.
    fn socket_identity(&self, display: &str) -> (PathBuf, u64, i128) {
        let path = if Path::new(display).is_absolute() {
            PathBuf::from(display)
        } else {
            self.runtime_dir().unwrap_or_default().join(display)
        };
        match std::fs::metadata(&path) {
            Ok(meta) => {
                let created =
                    i128::from(meta.ctime()) * 1_000_000_000 + i128::from(meta.ctime_nsec());
                (path, meta.ino(), created)
            }
            Err(_) => (path, 0, 0),
        }
    }

    /// Whether this session can show a second display, from the last probes, with the reason
    /// where it cannot (module docs).
    pub fn capability(&self) -> (bool, String) {
        if !self.is_wayland || self.control_available() {
            return (true, String::new());
        }
        let Some(compositor) = self.app_compositor() else {
            return (true, String::new());
        };
        if !compositor.has_separate_app_compositor() {
            if compositor.x11_session_display().is_some() {
                return (
                    false,
                    "An X11 desktop in a rootful Xwayland is one screen of a fixed size: a second display has nowhere to show.".into(),
                );
            }
            return (true, String::new());
        }
        if self.probed_screen_count() >= 2 {
            return (true, String::new());
        }
        (
            false,
            "The session compositor cannot show a second display: it has no screen control and no spare screen.".into(),
        )
    }

    /// Re-probe what backs a second display and return the fresh answer. The control socket
    /// decides when it answers; without one, a nested session is asked whether it serves KWin's
    /// virtual outputs, an answer kept for as long as its socket is the same one, and only an
    /// unreachable compositor is asked again; a session with neither control is asked for its
    /// screen count, so a compositor started with spare screens keeps its second display.
    pub async fn probe_capability(&self) -> (bool, String) {
        self.probe_ipc().await;
        let (mut kde, mut count) = (false, 0);
        if !self.ipc_available()
            && self.backend.is_some()
            && let Some((display, true)) = self.resolve().await
        {
            let identity = self.socket_identity(&display);
            let cached = self
                .kde_probe
                .lock()
                .as_ref()
                .filter(|probe| probe.identity == identity)
                .map(|probe| probe.offered);
            if let Some(offered) = cached {
                kde = offered;
            } else {
                let asked = display.clone();
                match self
                    .blocking(move |b| b.app_screen_control_offered(&asked))
                    .await
                {
                    Some(Ok(offered)) => {
                        kde = offered;
                        *self.kde_probe.lock() = Some(KdeProbe { identity, offered });
                        if offered {
                            debug!(target: "input", "Session compositor offers screens on demand.");
                        } else {
                            debug!(target: "input", "Session compositor serves no virtual-output protocol; a second display needs a spare screen.");
                        }
                    }
                    Some(Err(e)) => {
                        debug!(target: "input", "Session screen control probe failed: {e}");
                    }
                    None => {}
                }
            }
            if !kde {
                match self.blocking(move |b| b.list_app_screens(&display)).await {
                    Some(Ok(screens)) => count = screens.len(),
                    Some(Err(e)) => {
                        debug!(target: "input", "Session screen count probe failed: {e}");
                    }
                    None => {}
                }
            }
        }
        self.kde_ok.store(kde, Ordering::Relaxed);
        self.screen_count.store(count, Ordering::Relaxed);
        self.capability()
    }

    /// One command over the control socket: a fresh connection, a newline-terminated command, a
    /// JSON line back. Bounded; an unreachable or silent compositor yields an empty object.
    async fn ipc_command(&self, command: &str) -> Json {
        let empty = Json::Object(Default::default());
        let Some(path) = self.ipc_path() else {
            return empty;
        };
        let stream =
            match tokio::time::timeout(IPC_CONNECT_TIMEOUT, UnixStream::connect(&path)).await {
                Ok(Ok(stream)) => stream,
                Ok(Err(e)) => {
                    debug!(target: "input", "Session IPC connect failed: {e}");
                    return empty;
                }
                Err(e) => {
                    debug!(target: "input", "Session IPC connect failed: {e}");
                    return empty;
                }
            };
        let (reader, mut writer) = stream.into_split();
        if let Err(e) = writer.write_all(format!("{command}\n").as_bytes()).await {
            debug!(target: "input", "Session IPC '{command}' failed: {e}");
            return empty;
        }
        let mut line = String::new();
        let mut reader = BufReader::new(reader);
        match tokio::time::timeout(IPC_REPLY_TIMEOUT, reader.read_line(&mut line)).await {
            Ok(Ok(_)) => serde_json::from_str(&line).unwrap_or_else(|e| {
                debug!(target: "input", "Session IPC '{command}' failed: {e}");
                empty
            }),
            Ok(Err(e)) => {
                debug!(target: "input", "Session IPC '{command}' failed: {e}");
                empty
            }
            Err(e) => {
                debug!(target: "input", "Session IPC '{command}' failed: {e}");
                empty
            }
        }
    }

    /// Retire the session screens whose displays are gone, by the names the compositor coined. A
    /// screen owned by no display is retired the same way, so a session also started with
    /// pre-provisioned screens converges on one screen per display. Removal follows the rung that
    /// grew: the control socket's `REMOVE_SCREEN`, or closing the held virtual output. A session
    /// with neither control keeps the screens it was started with, which the spare-screen hold
    /// covers.
    pub async fn ensure_screens(&self, secondaries: &[String]) {
        let mut wanted: Vec<String> = secondaries
            .iter()
            .filter(|d| !d.is_empty() && *d != PRIMARY_DISPLAY)
            .cloned()
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        wanted.sort_by_key(|d| (wayland_output_id(Some(d)), d.clone()));
        *self.display_order.lock() = wanted.clone();
        if self.backend.is_none() || !self.control_available() || Self::host_capture() {
            return;
        }
        let _grow = self.grow_lock.lock().await;
        let Some((display, _)) = self.resolve().await else {
            return;
        };
        let names: Vec<String> = match self.blocking(move |b| b.list_app_screens(&display)).await {
            Some(Ok(screens)) => screens.into_iter().map(|screen| screen.0).collect(),
            Some(Err(e)) => {
                debug!(target: "input", "Session screen list failed: {e}");
                return;
            }
            None => return,
        };
        let doomed: Vec<(String, Option<String>)> = {
            let mut owned = self.owned.lock();
            owned.retain(|_, name| names.contains(name));
            let claimed: HashSet<&String> = owned.values().collect();
            owned
                .iter()
                .filter(|(did, _)| !wanted.contains(did))
                .map(|(did, name)| (name.clone(), Some(did.clone())))
                .chain(
                    names
                        .iter()
                        .skip(1)
                        .filter(|name| !claimed.contains(name))
                        .map(|name| (name.clone(), None)),
                )
                .collect()
        };
        for (name, did) in doomed {
            let (removed, error) = if self.ipc_available() {
                let reply = self.ipc_command(&format!("REMOVE_SCREEN {name}")).await;
                (
                    reply.get("ok").and_then(Json::as_bool).unwrap_or(false),
                    reply
                        .get("error")
                        .and_then(Json::as_str)
                        .unwrap_or("no reply")
                        .to_string(),
                )
            } else {
                let closing = name.clone();
                match self.blocking(move |b| b.remove_app_screen(&closing)).await {
                    Some(Ok(removed)) => (removed, "not a screen this session grew".to_string()),
                    Some(Err(e)) => (false, e),
                    None => (false, "no reply".to_string()),
                }
            };
            if !removed {
                warn!(target: "input", "Session compositor could not remove screen '{name}': {error}");
                continue;
            }
            if let Some(did) = did {
                self.owned.lock().remove(&did);
            }
            info!(target: "input", "Session compositor removed screen '{name}'; its windows return to the primary.");
        }
    }

    /// Grow the session screen a display owns, right before its capture output is created (module
    /// docs), and wait, bounded, for its host window to park so that output adopts it. `size` and
    /// `scale` seed a KWin virtual output, whose real size the capture output drives; the labwc
    /// rung sizes screens through output management instead.
    ///
    /// Returns false when the compositor refused the screen, a control socket answering no or a
    /// KWin whose grown output registers as no screen, which leaves the display nowhere to show;
    /// true when it has one, or needs none.
    pub async fn ensure_screen(
        &self,
        display_id: &str,
        size: Option<(i32, i32)>,
        scale: f64,
    ) -> bool {
        if display_id.is_empty()
            || display_id == PRIMARY_DISPLAY
            || self.backend.is_none()
            || !self.control_available()
            || Self::host_capture()
        {
            return true;
        }
        let _grow = self.grow_lock.lock().await;
        if self.owned.lock().contains_key(display_id) {
            return true;
        }
        let before = self.parked().await;
        let name = if self.ipc_available() {
            let reply = self.ipc_command("ADD_SCREEN").await;
            if !reply.get("ok").and_then(Json::as_bool).unwrap_or(false) {
                let why = reply
                    .get("error")
                    .and_then(Json::as_str)
                    .unwrap_or("no reply");
                warn!(target: "input", "Session compositor could not add a screen for '{display_id}': {why}");
                return false;
            }
            reply
                .get("output")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_string()
        } else {
            let Some((display, _)) = self.resolve().await else {
                return false;
            };
            let name = format!("SELKIES-{}", wayland_output_id(Some(display_id)));
            let (width, height) = size.unwrap_or((1920, 1080));
            let scale = if scale > 0.0 { scale } else { 1.0 };
            let growing = name.clone();
            let grown = self
                .blocking(move |b| b.add_app_screen(&display, &growing, width, height, scale))
                .await;
            match grown {
                Some(Ok(())) => name,
                Some(Err(e)) => {
                    warn!(target: "input", "Session compositor could not add a screen for '{display_id}': {e}");
                    return false;
                }
                None => return false,
            }
        };
        info!(target: "input", "Session compositor added screen '{name}' for '{display_id}'.");
        self.owned
            .lock()
            .insert(display_id.to_string(), name.clone());
        let deadline = Instant::now() + SCREEN_PARK_WAIT;
        while self.parked().await <= before {
            if Instant::now() > deadline {
                warn!(target: "input", "Screen '{name}' produced no host window to adopt.");
                break;
            }
            tokio::time::sleep(SCREEN_PARK_POLL).await;
        }
        true
    }

    /// The parked host windows, none when they cannot be read.
    async fn parked(&self) -> usize {
        match self.blocking(|b| b.parked_windows()).await {
            Some(Ok(count)) => count,
            _ => 0,
        }
    }

    /// The screen each display of `display_ids` owns as a position among the session's screens,
    /// resolved in one read. The primary is position 0, the screen the session booted with. A
    /// secondary whose screen was grown here is found by its recorded name; on a session whose
    /// screens were pre-provisioned, the attached secondaries hold them in the order they were
    /// laid out, after the primary, so a display's rank in that order is its position.
    pub async fn positions(&self, display_ids: &[String]) -> HashMap<String, usize> {
        let wanted: Vec<String> = display_ids
            .iter()
            .map(|d| {
                if d.is_empty() {
                    PRIMARY_DISPLAY.to_string()
                } else {
                    d.clone()
                }
            })
            .collect();
        let owned = self.owned.lock().clone();
        let mut names = Vec::new();
        if wanted
            .iter()
            .any(|d| d != PRIMARY_DISPLAY && owned.contains_key(d))
            && let Some((display, _)) = self.resolve().await
        {
            match self.blocking(move |b| b.list_app_screens(&display)).await {
                Some(Ok(screens)) => names = screens.into_iter().map(|screen| screen.0).collect(),
                Some(Err(e)) => debug!(target: "input", "Session screen list failed: {e}"),
                None => {}
            }
        }
        let mut order = self.display_order.lock().clone();
        let mut unplaced: Vec<&String> = wanted
            .iter()
            .filter(|d| *d != PRIMARY_DISPLAY && !order.contains(d))
            .collect();
        unplaced.sort_by_key(|d| (wayland_output_id(Some(d)), (*d).clone()));
        unplaced.dedup();
        order.extend(unplaced.into_iter().cloned());
        wanted
            .into_iter()
            .map(|d| {
                let by_name = owned
                    .get(&d)
                    .and_then(|name| names.iter().position(|n| n == name));
                let position = if d == PRIMARY_DISPLAY {
                    0
                } else if let Some(position) = by_name {
                    position
                } else {
                    order
                        .iter()
                        .position(|o| *o == d)
                        .map_or(0, |rank| rank + 1)
                };
                (d, position)
            })
            .collect()
    }

    /// Which of the session's screens backs `display_id`; 0 is the primary's.
    pub async fn position(&self, display_id: &str) -> usize {
        let did = if display_id.is_empty() {
            PRIMARY_DISPLAY
        } else {
            display_id
        };
        self.positions(&[did.to_string()])
            .await
            .get(did)
            .copied()
            .unwrap_or(0)
    }

    /// Apply a DPI on the Wayland backend and return the capture output scale it leaves behind:
    /// 1.0 once a session absorbed it, the full scale otherwise (module docs). `size` is the size
    /// that screen is about to carry, when known, so the mode and the scale land together.
    pub async fn realize_dpi(&self, dpi: &str, display_id: &str, size: Option<(i32, i32)>) -> f64 {
        let Some(value) = dpi.trim().parse::<f64>().ok().filter(|v| v.is_finite()) else {
            return 1.0;
        };
        let scale = (value / 96.0).max(0.1);
        let Some((display, true)) = self.resolve().await else {
            return scale;
        };
        let index = self.position(display_id).await;
        let applied = self
            .blocking(move |b| match size {
                Some((w, h)) if w > 0 && h > 0 => {
                    b.set_app_screen_geometry(&display, w, h, scale, index)
                }
                _ => b.set_app_output_scale(&display, scale, index),
            })
            .await;
        match applied {
            Some(Ok(true)) => {
                let named = if display_id.is_empty() {
                    PRIMARY_DISPLAY
                } else {
                    display_id
                };
                info!(target: "input", "Session compositor screen {index} ('{named}') scaled to {scale}.");
                1.0
            }
            Some(Ok(false)) | None => scale,
            Some(Err(e)) => {
                debug!(target: "input", "Session output scale failed: {e}");
                scale
            }
        }
    }

    /// Realize the configured DPI at service start, ahead of any client; 96 is unity, where a
    /// compositor starts. Returns the scale left for the capture output.
    pub async fn realize_startup_dpi(&self) -> f64 {
        let dpi = settings().scaling_dpi().to_string();
        if dpi.trim().parse::<f64>().ok() == Some(96.0) {
            return 1.0;
        }
        self.realize_dpi(&dpi, PRIMARY_DISPLAY, None).await
    }

    /// Re-hold the spare screens after a layout pass created or destroyed an output: which of the
    /// session's screens a capture drives has changed.
    pub fn resync(self: &Arc<Self>) {
        if self.backend.is_some() {
            self.schedule_spare_hold();
        }
    }

    /// A nested session opens the screens it was started with, whether or not the capture drives
    /// that many: the extra ones stretch its desktop onto a screen nobody sees, which is where a
    /// client that centers itself then lands. They are held small until a display arrives for
    /// them; pixelflux resizes one to its full size the moment it gets an output, and back when
    /// it loses one. Only for a session whose screens are pre-provisioned: when a control grows
    /// them there is never a spare to hold, and a hold computed before a layout pass created its
    /// capture output would land late and shrink the screen that output just adopted.
    fn schedule_spare_hold(self: &Arc<Self>) {
        if self.control_available() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let me = self.clone();
        runtime.spawn(async move { me.hold_spares().await });
    }

    /// Hold every session screen without a capture output at [`SPARE_SCREEN_SIZE`]. The primary's
    /// screen and every secondary's count; the primary's default view is a capture over screen
    /// 0, not a screen.
    async fn hold_spares(&self) {
        let Some((display, _)) = self.resolve().await else {
            return;
        };
        let held = self
            .blocking(move |b| {
                let view = wayland_output_id(None);
                let keep = b
                    .capture_output_ids()?
                    .iter()
                    .filter(|id| **id != view)
                    .count()
                    .max(1);
                let (width, height) = SPARE_SCREEN_SIZE;
                b.hold_spare_app_screens(&display, keep, width, height)
            })
            .await;
        match held {
            Some(Ok(0)) | None => {}
            Some(Ok(held)) => {
                let (width, height) = SPARE_SCREEN_SIZE;
                info!(target: "input", "Session compositor has {held} screen(s) with no capture output; held at {width}x{height}.");
            }
            Some(Err(e)) => debug!(target: "input", "Holding spare session screens failed: {e}"),
        }
    }

    /// Lay the session's own screens out the way the capture outputs were placed, on a task of
    /// its own. The session arranges the screens it opens by its own rule (wlroots puts them side
    /// by side in opening order), and that layout is what places windows and carries the pointer
    /// between screens, so a display asked for above or left of the primary would otherwise land
    /// to its right.
    pub fn schedule_layout(self: &Arc<Self>, layouts: &Layouts) {
        if self.backend.is_none() || layouts.is_empty() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let me = self.clone();
        let layouts = layouts.clone();
        runtime.spawn(async move { me.apply_layout(&layouts).await });
    }

    /// Hand the arrangement to the session compositor, each rectangle on the screen its display
    /// owns, in one configuration.
    pub async fn apply_layout(&self, layouts: &Layouts) {
        let Some((display, _)) = self.resolve().await else {
            return;
        };
        let ids: Vec<String> = layouts.keys().cloned().collect();
        let positions = self.positions(&ids).await;
        let mut ordered: Vec<(&String, usize)> = layouts
            .keys()
            .map(|d| (d, positions.get(d).copied().unwrap_or(0)))
            .collect();
        ordered.sort_by_key(|(_, position)| *position);
        let rects: Vec<(i32, i32, i32, i32)> = ordered
            .iter()
            .map(|(d, _)| {
                let rect = &layouts[*d];
                (rect.x, rect.y, rect.w, rect.h)
            })
            .collect();
        let sent = rects.clone();
        match self
            .blocking(move |b| b.set_app_screen_layout(&display, sent))
            .await
        {
            Some(Ok(0)) => {
                // A compositor without either output-management protocol arranges them itself.
                info!(target: "input", "Session compositor manages no outputs for clients; its screen arrangement is its own.");
            }
            Some(Ok(placed)) => {
                info!(target: "input", "Session compositor laid {placed} screen(s) out at {rects:?}.");
            }
            Some(Err(e)) => debug!(target: "input", "Session screen layout failed: {e}"),
            None => {}
        }
    }

    /// The DPI a just-adopted session compositor takes: an operator-set DPI governs the desktop,
    /// since client syncs never reach it then, otherwise the last client-synced one. `None` at
    /// 96, which is unity: nothing to apply and nothing the capture could have taken.
    fn adoption_dpi(&self) -> Option<i32> {
        let operator = {
            let s = settings();
            s.was_provided("scaling_dpi")
                .then(|| s.scaling_dpi().trim().parse::<f64>().ok())
        };
        let dpi = match operator {
            Some(dpi) => dpi?,
            None => self.app_compositor()?.system_dpi(),
        };
        Some(dpi as i32).filter(|dpi| *dpi != 96)
    }

    /// A session compositor was just adopted. A scale applied before the session existed landed
    /// on the capture output, and the session does not take it from there: its screen keeps the
    /// capture's pixel mode as its own, so scaling the session on top of a scaled capture draws
    /// the desktop at twice the size and shows a quarter of it, upscaled. So the transport runs
    /// its ladder, its pages are told what a second display can do now, and the spare screens are
    /// held. Each runs on a task of its own.
    pub fn compositor_adopted(self: &Arc<Self>, transport: Arc<dyn ScreenTransport>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if let Some(dpi) = self.adoption_dpi() {
            let scaling = transport.clone();
            runtime.spawn(async move { scaling.resync_session_scale(dpi).await });
        }
        runtime.spawn(async move { transport.republish_second_screen().await });
        self.schedule_spare_hold();
    }
}
