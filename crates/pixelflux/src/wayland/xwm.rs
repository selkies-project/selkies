/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! XWayland for the compositor's own desktop: the `Xwayland` server spawned and kept running,
//! and Smithay's X11 window manager driven as the window policy decides.
//!
//! Started only under a window policy (`Compositor::start_xwayland`, which `SELKIES_WM` on the
//! Wayland backend asks for): the X11 clients become windows of the same `Space` as the Wayland
//! ones (`Window::new_x11_window`), told to the policy through the same events with their
//! ICCCM and EWMH details read into a `WindowInfo` (class and instance, transient parent, window
//! type, size hints, Motif decorations, the states they asked for), and placed, framed, and
//! stacked by the same commands. Override-redirect windows (menus, tooltips, drag icons) are
//! placed where they ask, above everything, and never told to the policy, as X11 requires.
//!
//! Smithay's manager handles the selections (clipboard and primary between X11 and Wayland
//! clients), XDND, `_NET_CLIENT_LIST`, `_NET_WM_STATE`, and `_NET_WM_MOVERESIZE`; what it does
//! not publish and a desktop reads (`_NET_WORKAREA`, `_NET_ACTIVE_WINDOW`,
//! `_NET_CLIENT_LIST_STACKING`, `_NET_FRAME_EXTENTS`) is written here over a second connection to
//! the same server, since the manager's own is private to it, and so is the name it answers to
//! on its check window (`publish_name`). A crashed Xwayland is started
//! again after a short wait, so an X11 client started later still finds a server.
//!
//! The second connection also watches each override-redirect window's focus changes, since
//! Smithay's manager tells the compositor nothing of a keyboard grab: an X11 client that grabs
//! the keyboard for a popup (a menu opened from a tray icon on the panel, which holds the
//! keyboard) is given it on the grab's `NotifyGrab` focus-in, and the window the policy focused
//! takes it back when the popup unmaps holding it, as wlroots' `grab_focus` does
//! (`grabbed_keyboard`); Xwayland hears keys only through a surface of its own. A window is
//! watched from its creation, as wlroots selects it, and again at its map for one that turned
//! override-redirect in between.
//!
//! Where the host has no DRM render node, the EGL vendor library a GPU container runtime injects
//! (NVIDIA's) segfaults inside Xwayland's EGL initialization, and glamor would have nothing to
//! accelerate there anyway, so Xwayland starts pinned to Mesa's EGL vendor, loading no external
//! EGL platform, with its buffers in shared memory (`-shm`, `no_render_node_launch`). An
//! Xwayland a caller ships beside its own executables is named by `PIXELFLUX_XWAYLAND`
//! ([`XWAYLAND_VAR`]) and found ahead of the host's for that child alone.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use smithay::desktop::Window;
use smithay::input::Seat;
use smithay::reexports::calloop::LoopHandle;
use smithay::utils::{Logical, Point, Rectangle};
use smithay::wayland::compositor::CompositorHandler;
use smithay::wayland::selection::SelectionTarget;
use smithay::wayland::selection::data_device::{
    clear_data_device_selection, current_data_device_selection_userdata,
    request_data_device_client_selection, set_data_device_selection,
};
use smithay::wayland::selection::primary_selection::{
    clear_primary_selection, current_primary_selection_userdata, request_primary_client_selection,
    set_primary_selection,
};
use smithay::wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState};
use smithay::xwayland::xwm::{
    Reorder, ResizeEdge as X11ResizeEdge, WmWindowProperty, WmWindowType, XwmId,
};
use smithay::xwayland::{X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, AtomEnum, ConnectionExt, PropMode};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::wayland::frontend::{AppState, FocusTarget, NEXT_WINDOW_ID, WindowMeta};
use crate::wayland::wm;
use crate::wm::{
    Decoration, Rect, Request, ResizeEdge, Size, SizeHints, States, WindowId, WindowInfo,
    WindowKind, WindowType,
};

/// How long a crashed Xwayland waits before it is started again.
const RESTART_DELAY: Duration = Duration::from_secs(2);
/// The glvnd vendor file of Mesa's EGL, the vendor Xwayland is pinned to without a render node.
const MESA_EGL_VENDOR: &str = "/usr/share/glvnd/egl_vendor.d/50_mesa.json";

/// The X11 side of the compositor.
pub struct XwmState {
    pub wm: Option<X11Wm>,
    pub display: Option<u32>,
    /// Who is told the display number when the server is up.
    pub notify: Vec<std::sync::mpsc::Sender<u32>>,
    /// Windows mapped through this manager, by the policy's id.
    pub windows: HashMap<WindowId, Window>,
    /// When a crashed server is started again.
    pub restart_at: Option<Instant>,
    /// The second connection the EWMH properties the manager does not write go over.
    extras: Option<Extras>,
    pub shell_state: XWaylandShellState,
}

impl XwmState {
    pub fn new(shell_state: XWaylandShellState) -> XwmState {
        XwmState {
            wm: None,
            display: None,
            notify: Vec::new(),
            windows: HashMap::new(),
            restart_at: None,
            extras: None,
            shell_state,
        }
    }
}

x11rb::atom_manager! {
    pub Atoms: AtomsCookie {
        _NET_WORKAREA,
        _NET_ACTIVE_WINDOW,
        _NET_CLIENT_LIST_STACKING,
        _NET_FRAME_EXTENTS,
        _NET_SUPPORTED,
        _NET_SUPPORTING_WM_CHECK,
        _NET_WM_NAME,
        UTF8_STRING,
        _NET_WM_STATE_HIDDEN,
        _NET_WM_ACTION_MOVE,
        _NET_WM_ACTION_RESIZE,
        _NET_WM_ACTION_MINIMIZE,
        _NET_WM_ACTION_MAXIMIZE_HORZ,
        _NET_WM_ACTION_MAXIMIZE_VERT,
        _NET_WM_ACTION_FULLSCREEN,
        _NET_WM_ACTION_CLOSE,
        _NET_WM_ALLOWED_ACTIONS,
    }
}

/// The second X connection and the atoms it writes with.
struct Extras {
    conn: RustConnection,
    root: xproto::Window,
    atoms: Atoms,
    /// Which connection this is, so the event source of one an Xwayland restart replaced
    /// removes itself rather than reading the next.
    generation: u64,
}

/// The generation the next second connection takes.
static NEXT_EXTRAS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl Extras {
    fn connect(display: u32) -> Option<Extras> {
        let (conn, screen) = RustConnection::connect(Some(&format!(":{display}"))).ok()?;
        let root = conn.setup().roots.get(screen)?.root;
        let atoms = Atoms::new(&conn).ok()?.reply().ok()?;
        let generation = NEXT_EXTRAS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(Extras {
            conn,
            root,
            atoms,
            generation,
        })
    }

    /// Report `window`'s focus changes over this connection.
    fn watch_focus(&self, window: xproto::Window) {
        let _ = self.conn.change_window_attributes(
            window,
            &xproto::ChangeWindowAttributesAux::new().event_mask(xproto::EventMask::FOCUS_CHANGE),
        );
        self.flush();
    }

    fn set_cardinals(&self, window: xproto::Window, property: xproto::Atom, values: &[u32]) {
        let _ = self.conn.change_property32(
            PropMode::REPLACE,
            window,
            property,
            AtomEnum::CARDINAL,
            values,
        );
    }

    fn set_windows(&self, window: xproto::Window, property: xproto::Atom, values: &[u32]) {
        let _ = self.conn.change_property32(
            PropMode::REPLACE,
            window,
            property,
            AtomEnum::WINDOW,
            values,
        );
    }

    fn set_atoms(&self, window: xproto::Window, property: xproto::Atom, values: &[u32]) {
        let _ = self.conn.change_property32(
            PropMode::REPLACE,
            window,
            property,
            AtomEnum::ATOM,
            values,
        );
    }

    fn flush(&self) {
        let _ = self.conn.flush();
    }
}

/// Where the Xwayland a caller ships is named, an executable called `Xwayland`; unset, the
/// first on the `PATH`.
pub const XWAYLAND_VAR: &str = "PIXELFLUX_XWAYLAND";

/// The `PATH` Xwayland is looked up on: the directory of the one [`XWAYLAND_VAR`] names ahead of
/// the process's own, so a bundle's Xwayland is found first for this child alone and the
/// session's applications keep the host's programs; `None` where none is named or it is not
/// there.
fn xwayland_path(
    named: Option<std::ffi::OsString>,
    path: Option<std::ffi::OsString>,
) -> Option<String> {
    let named = std::path::PathBuf::from(named?);
    let dir = named.parent()?.to_string_lossy().into_owned();
    if !named.is_file() || dir.is_empty() {
        return None;
    }
    let rest = path
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    Some(if rest.is_empty() {
        dir
    } else {
        format!("{dir}:{rest}")
    })
}

/// The environment and arguments Xwayland starts with: the `PATH` that finds a named Xwayland
/// ([`xwayland_path`]); nothing more where the host has a DRM render node, else Mesa's EGL
/// vendor where it is installed, no external EGL platform, and `-shm`.
fn no_render_node_launch() -> (Vec<(&'static str, String)>, Vec<&'static str>) {
    let mut env = Vec::new();
    if let Some(path) = xwayland_path(std::env::var_os(XWAYLAND_VAR), std::env::var_os("PATH")) {
        env.push(("PATH", path));
    }
    let has_render_node = std::fs::read_dir("/dev/dri").is_ok_and(|dir| {
        dir.flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with("renderD"))
    });
    if has_render_node {
        return (env, Vec::new());
    }
    println!(
        "[Wayland] No DRM render node: Xwayland starts on Mesa's EGL with shared-memory buffers."
    );
    env.push(("__EGL_EXTERNAL_PLATFORM_CONFIG_FILENAMES", String::new()));
    if std::path::Path::new(MESA_EGL_VENDOR).exists() {
        env.push((
            "__EGL_VENDOR_LIBRARY_FILENAMES",
            MESA_EGL_VENDOR.to_string(),
        ));
    }
    (env, vec!["-shm"])
}

/// Spawn Xwayland and arm the start of its window manager once it is ready. `notify` is
/// told the display number when it is.
pub fn start(
    state: &mut AppState,
    handle: LoopHandle<'static, AppState>,
    notify: Option<std::sync::mpsc::Sender<u32>>,
) {
    if let Some(notify) = notify {
        state.xwm.notify.push(notify);
    }
    if state.xwm.wm.is_some() || state.xwm.restart_at.is_some() {
        if let Some(display) = state.xwm.display {
            for n in &state.xwm.notify {
                let _ = n.send(display);
            }
            state.xwm.notify.clear();
        }
        return;
    }
    ensure_x11_socket_dir();
    let (env, args) = no_render_node_launch();
    let (xwayland, client) = match XWayland::spawn(
        &state.dh,
        None,
        env,
        args,
        true,
        std::process::Stdio::null(),
        std::process::Stdio::null(),
        |_| (),
    ) {
        Ok(spawned) => spawned,
        Err(e) => {
            eprintln!("[Wayland] Xwayland did not start: {e}; X11 clients have no server.");
            state.xwm.restart_at = Some(Instant::now() + RESTART_DELAY);
            return;
        }
    };
    let dh = state.dh.clone();
    let loop_handle = handle.clone();
    let inserted = handle.insert_source(xwayland, move |event, _, state| match event {
        XWaylandEvent::Ready {
            x11_socket,
            display_number,
        } => {
            state.client_compositor_state(&client).set_client_scale(1.0);
            match X11Wm::start_wm(loop_handle.clone(), &dh, x11_socket, client.clone()) {
                Ok(mut wm) => {
                    let image = state.cursor_helper.get_image(1, Duration::ZERO);
                    let _ = wm.set_cursor(
                        &image.pixels_rgba,
                        (image.width as u16, image.height as u16).into(),
                        (image.xhot as u16, image.yhot as u16).into(),
                    );
                    state.xwm.wm = Some(wm);
                    state.xwm.display = Some(display_number);
                    state.xwm.extras = Extras::connect(display_number);
                    if state.xwm.extras.is_none() {
                        eprintln!("[Wayland] Xwayland :{display_number}: no second connection; _NET_WORKAREA and _NET_ACTIVE_WINDOW stay unset.");
                    }
                    listen_extras(state, &loop_handle);
                    publish_supported(state);
                    publish_name(state);
                    publish_workarea(state);
                    println!("[Wayland] Xwayland on display :{display_number}; X11 clients are managed with the Wayland ones.");
                    for n in state.xwm.notify.drain(..) {
                        let _ = n.send(display_number);
                    }
                    wm::x11_display(state, display_number);
                }
                Err(e) => {
                    eprintln!("[Wayland] Xwayland window manager did not start: {e}");
                    state.xwm.restart_at = Some(Instant::now() + RESTART_DELAY);
                }
            }
        }
        XWaylandEvent::Error => {
            eprintln!("[Wayland] Xwayland exited during startup; starting it again in {RESTART_DELAY:?}.");
            state.xwm.wm = None;
            state.xwm.display = None;
            state.xwm.extras = None;
            state.xwm.restart_at = Some(Instant::now() + RESTART_DELAY);
        }
    });
    if let Err(e) = inserted {
        eprintln!("[Wayland] Xwayland source not registered: {e}");
    }
}

/// The directory X servers put their sockets in, which only an X server or a display manager
/// creates: on a backend that runs no Xvfb nothing has, and Xwayland's sockets cannot be made
/// without it.
fn ensure_x11_socket_dir() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::path::Path::new("/tmp/.X11-unix");
    if dir.is_dir() {
        return;
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("[Wayland] {}: not created: {e}", dir.display());
        return;
    }
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o1777));
}

/// Start the server again where it exited and its wait passed; called on the frame timer.
pub fn maintain(state: &mut AppState, handle: &LoopHandle<'static, AppState>, now: Instant) {
    if state.xwm.wm.is_none() && state.xwm.restart_at.is_some_and(|at| at <= now) {
        state.xwm.restart_at = None;
        start(state, handle.clone(), None);
    }
}

/// The policy's window for an X11 surface.
fn window_of(state: &AppState, surface: &X11Surface) -> Option<(WindowId, Window)> {
    state
        .xwm
        .windows
        .iter()
        .find(|(_, w)| w.x11_surface() == Some(surface))
        .map(|(id, w)| (*id, w.clone()))
}

fn window_type_of(surface: &X11Surface) -> WindowType {
    match surface.window_type() {
        Some(WmWindowType::Dialog) => WindowType::Dialog,
        Some(WmWindowType::Utility) => WindowType::Utility,
        Some(WmWindowType::Toolbar) => WindowType::Toolbar,
        Some(WmWindowType::Menu) => WindowType::Menu,
        Some(WmWindowType::Splash) => WindowType::Splash,
        Some(WmWindowType::Dock) => WindowType::Dock,
        Some(WmWindowType::Desktop) => WindowType::Desktop,
        Some(WmWindowType::Notification) => WindowType::Notification,
        Some(WmWindowType::DropdownMenu) => WindowType::DropdownMenu,
        Some(WmWindowType::PopupMenu) => WindowType::PopupMenu,
        Some(WmWindowType::Tooltip) => WindowType::Tooltip,
        Some(WmWindowType::Combo) => WindowType::Combo,
        Some(WmWindowType::Dnd) => WindowType::Dnd,
        Some(WmWindowType::Normal) | None => {
            if surface.is_transient_for().is_some() {
                WindowType::Dialog
            } else {
                WindowType::Normal
            }
        }
    }
}

/// What the policy is told about an X11 window.
pub fn window_info(
    state: &AppState,
    id: WindowId,
    surface: &X11Surface,
    output: u32,
    _decoration: Decoration,
) -> WindowInfo {
    let parent = surface.is_transient_for().and_then(|parent| {
        state
            .xwm
            .windows
            .iter()
            .find(|(_, w)| w.x11_surface().is_some_and(|s| s.window_id() == parent))
            .map(|(id, _)| *id)
    });
    let hints = surface.size_hints();
    let geometry = surface.geometry();
    let origin = surface.last_configure().loc;
    let decoration = if surface.is_decorated() {
        Decoration::None
    } else {
        Decoration::Server
    };
    let accepts_focus = !matches!(
        surface.input_model(),
        smithay::xwayland::xwm::WmInputModel::None
    );
    WindowInfo {
        id,
        kind: Some(WindowKind::X11 {
            override_redirect: surface.is_override_redirect(),
        }),
        window_type: window_type_of(surface),
        title: surface.title(),
        app_id: surface.class(),
        instance: surface.instance(),
        parent,
        modal: surface.is_modal(),
        hints: SizeHints {
            min: surface.min_size().map(|s| Size { w: s.w, h: s.h }),
            max: surface.max_size().map(|s| Size { w: s.w, h: s.h }),
            increment: hints
                .as_ref()
                .and_then(|h| h.size_increment)
                .map(|(w, h)| Size { w, h }),
            base: surface.base_size().map(|s| Size { w: s.w, h: s.h }),
        },
        requested: Some(Rect::new(
            origin.x,
            origin.y,
            geometry.size.w,
            geometry.size.h,
        )),
        positioned: hints
            .as_ref()
            .is_some_and(|h| h.position.is_some() && (origin.x != 0 || origin.y != 0))
            || !matches!(
                window_type_of(surface),
                WindowType::Normal | WindowType::Dialog | WindowType::Utility | WindowType::Toolbar
            ),
        decoration,
        output,
        pid: surface.pid(),
        wants_fullscreen: surface.is_fullscreen(),
        wants_maximized: surface.is_maximized(),
        wants_minimized: false,
        skip_taskbar: surface.is_skip_taskbar(),
        accepts_focus,
    }
}

/// Read the second connection's events as they arrive, for as long as it is the current one.
fn listen_extras(state: &AppState, handle: &LoopHandle<'static, AppState>) {
    use smithay::reexports::calloop::generic::Generic;
    use smithay::reexports::calloop::{Interest, Mode, PostAction};
    use std::os::fd::AsFd;
    let Some(extras) = state.xwm.extras.as_ref() else {
        return;
    };
    let Ok(fd) = extras.conn.stream().as_fd().try_clone_to_owned() else {
        return;
    };
    let generation = extras.generation;
    let source = Generic::new(fd, Interest::READ, Mode::Level);
    let inserted = handle.insert_source(source, move |_, _, state| {
        loop {
            let event = match state.xwm.extras.as_ref() {
                Some(extras) if extras.generation == generation => extras.conn.poll_for_event(),
                _ => return Ok(PostAction::Remove),
            };
            match event {
                Ok(Some(x11rb::protocol::Event::FocusIn(focus)))
                    if focus.mode == xproto::NotifyMode::GRAB =>
                {
                    grabbed_keyboard(state, focus.event);
                }
                Ok(Some(_)) => {}
                Ok(None) => return Ok(PostAction::Continue),
                Err(_) => return Ok(PostAction::Remove),
            }
        }
    });
    if let Err(e) = inserted {
        eprintln!(
            "[Wayland] Xwayland second connection not watched: {e}; X11 popups that grab the keyboard get it only through the window that has it."
        );
    }
}

/// Give the keyboard to the override-redirect window `window` whose client grabbed it.
fn grabbed_keyboard(state: &mut AppState, window: xproto::Window) {
    let popup = state
        .space
        .elements()
        .find(|w| {
            w.x11_surface().is_some_and(|s| {
                s.is_override_redirect() && s.window_id() == window && s.wl_surface().is_some()
            })
        })
        .cloned();
    if let Some(popup) = popup {
        crate::log::debug!(
            "[Wayland] X11 popup {window:#x} grabbed the keyboard; it takes the seat's"
        );
        wm::hand_keyboard(state, Some(popup));
    }
}

/// Where an X11 window's visible area sits on the root: the position the X server holds for
/// it, inside the shadow its toolkit draws (`_GTK_FRAME_EXTENTS`). [`X11Surface::geometry`]
/// is relative to the window's own origin, so it gives that inset and never a position.
fn root_location(surface: &X11Surface) -> Point<i32, Logical> {
    surface.last_configure().loc + surface.geometry().loc
}

/// Carry a configure out on an X11 window: its geometry and its states.
pub fn configure(surface: &X11Surface, rect: Rect, states: States) {
    if rect.w > 0 && rect.h > 0 {
        let _ = surface.configure(Some(Rectangle::<i32, Logical>::new(
            (rect.x, rect.y).into(),
            (rect.w, rect.h).into(),
        )));
    }
    let _ = surface.set_maximized(states.maximized);
    let _ = surface.set_fullscreen(states.fullscreen);
    let _ = surface.set_activated(states.activated);
    let _ = surface.set_hidden(states.minimized || states.hidden);
}

/// Publish what the manager supports beyond Smithay's own list, so clients ask for it.
fn publish_supported(state: &mut AppState) {
    let Some(extras) = state.xwm.extras.as_ref() else {
        return;
    };
    let a = &extras.atoms;
    let supported = [
        a._NET_WORKAREA,
        a._NET_ACTIVE_WINDOW,
        a._NET_CLIENT_LIST_STACKING,
        a._NET_FRAME_EXTENTS,
        a._NET_WM_STATE_HIDDEN,
        a._NET_WM_ALLOWED_ACTIONS,
    ];
    let existing: Vec<u32> = extras
        .conn
        .get_property(
            false,
            extras.root,
            a._NET_SUPPORTED,
            AtomEnum::ATOM,
            0,
            1024,
        )
        .ok()
        .and_then(|c| c.reply().ok())
        .and_then(|r| r.value32().map(|v| v.collect()))
        .unwrap_or_default();
    let mut all = existing;
    for atom in supported {
        if !all.contains(&atom) {
            all.push(atom);
        }
    }
    extras.set_atoms(extras.root, a._NET_SUPPORTED, &all);
    extras.flush();
}

/// The manager's name on the supporting check window, which clients read to decide how to
/// behave: Chromium and Electron draw their own frame only under a window manager they know by
/// this name and ask for a server frame under any other, Smithay's own among them, so the
/// manager answers to the name the X11 backend's manager gives, the one whose behavior the
/// policy carries.
const NET_WM_NAME: &str = "Openbox";

/// Rename Smithay's supporting check window to [`NET_WM_NAME`].
fn publish_name(state: &mut AppState) {
    let Some(extras) = state.xwm.extras.as_ref() else {
        return;
    };
    let a = &extras.atoms;
    let check = extras
        .conn
        .get_property(
            false,
            extras.root,
            a._NET_SUPPORTING_WM_CHECK,
            AtomEnum::WINDOW,
            0,
            1,
        )
        .ok()
        .and_then(|c| c.reply().ok())
        .and_then(|r| r.value32().and_then(|mut v| v.next()));
    let Some(check) = check else {
        return;
    };
    let _ = extras.conn.change_property8(
        PropMode::REPLACE,
        check,
        a._NET_WM_NAME,
        a.UTF8_STRING,
        NET_WM_NAME.as_bytes(),
    );
    extras.flush();
}

/// `_NET_WORKAREA` from the policy's outputs: the primary's workarea, as one desktop.
pub fn publish_workarea(state: &mut AppState) {
    let Some(extras) = state.xwm.extras.as_ref() else {
        return;
    };
    let workarea = state
        .wm
        .as_ref()
        .and_then(|wm| {
            wm.outputs
                .iter()
                .find(|o| o.primary)
                .or(wm.outputs.first())
                .map(|o| o.workarea)
        })
        .or_else(|| {
            state
                .output_nodes
                .first()
                .and_then(|n| n.logical_geometry())
                .map(|g| Rect::new(g.loc.x, g.loc.y, g.size.w, g.size.h))
        });
    if let Some(w) = workarea {
        extras.set_cardinals(
            extras.root,
            extras.atoms._NET_WORKAREA,
            &[
                w.x.max(0) as u32,
                w.y.max(0) as u32,
                w.w.max(0) as u32,
                w.h.max(0) as u32,
            ],
        );
        extras.flush();
    }
}

/// `_NET_ACTIVE_WINDOW` and `_NET_CLIENT_LIST_STACKING`, after a focus or stacking change.
pub fn publish_stacking(state: &mut AppState) {
    let Some(extras) = state.xwm.extras.as_ref() else {
        return;
    };
    let active = state
        .keyboard_focus
        .and_then(|id| state.xwm.windows.get(&id))
        .and_then(|w| w.x11_surface().map(|s| s.window_id()))
        .unwrap_or(0);
    extras.set_windows(extras.root, extras.atoms._NET_ACTIVE_WINDOW, &[active]);
    let stacking: Vec<u32> = state
        .space
        .elements()
        .filter_map(|w| w.x11_surface().map(|s| s.window_id()))
        .collect();
    extras.set_windows(
        extras.root,
        extras.atoms._NET_CLIENT_LIST_STACKING,
        &stacking,
    );
    extras.flush();
}

/// `_NET_FRAME_EXTENTS` and `_NET_WM_ALLOWED_ACTIONS` on a window the policy framed.
pub fn publish_frame(state: &mut AppState, id: WindowId) {
    let Some(extras) = state.xwm.extras.as_ref() else {
        return;
    };
    let Some(window) = state.xwm.windows.get(&id) else {
        return;
    };
    let Some(surface) = window.x11_surface() else {
        return;
    };
    let e = wm::frame_extents(state, id);
    extras.set_cardinals(
        surface.window_id(),
        extras.atoms._NET_FRAME_EXTENTS,
        &[e.left as u32, e.right as u32, e.top as u32, e.bottom as u32],
    );
    let a = &extras.atoms;
    extras.set_atoms(
        surface.window_id(),
        a._NET_WM_ALLOWED_ACTIONS,
        &[
            a._NET_WM_ACTION_MOVE,
            a._NET_WM_ACTION_RESIZE,
            a._NET_WM_ACTION_MINIMIZE,
            a._NET_WM_ACTION_MAXIMIZE_HORZ,
            a._NET_WM_ACTION_MAXIMIZE_VERT,
            a._NET_WM_ACTION_FULLSCREEN,
            a._NET_WM_ACTION_CLOSE,
        ],
    );
    extras.flush();
}

/// The end of a drag-and-drop grab, an X11 client's or a Wayland client's: the drag's icon is
/// no longer drawn.
impl smithay::input::dnd::DndGrabHandler for AppState {
    fn dropped(
        &mut self,
        _target: Option<smithay::input::dnd::DndTarget<'_, Self>>,
        _validated: bool,
        _seat: Seat<Self>,
        _location: Point<f64, Logical>,
    ) {
        self.dnd_icon = None;
    }

    fn cancelled(&mut self, _seat: Seat<Self>, _location: Point<f64, Logical>) {
        self.dnd_icon = None;
    }
}

impl XWaylandShellHandler for AppState {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwm.shell_state
    }
}

/// Hand a selection a Wayland client or the consumer took to Xwayland, so X11 clients paste
/// it; `None` drops the X11 ownership this side holds. A selection an X11 client took is
/// never announced back, since only the compositor's own setters carry it and they reach no
/// [`SelectionHandler`](smithay::wayland::selection::SelectionHandler).
pub(crate) fn announce_selection(
    state: &mut AppState,
    selection: SelectionTarget,
    mime_types: Option<Vec<String>>,
) {
    if let Some(xwm) = state.xwm.wm.as_mut()
        && let Err(e) = xwm.new_selection(selection, mime_types)
    {
        eprintln!("[Wayland] Selection not offered to X11 clients: {e:?}");
    }
}

impl XwmHandler for AppState {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm
            .wm
            .as_mut()
            .expect("the X11 window manager that called")
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn new_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(extras) = self.xwm.extras.as_ref() {
            extras.watch_focus(window.window_id());
        }
    }

    /// A client maps a top-level window: manage it and place it where the policy says.
    ///
    /// A window withdrawn and mapped again, as an application hiding to its tray icon does,
    /// keeps the entry and id it had, as a Wayland toplevel keeps its own across a null-buffer
    /// unmap: a second entry for the same surface would leave [`window_of`] answering with
    /// the unmapped one, whose moves, maximizes, and fullscreen requests the policy ignores.
    fn map_window_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        let (id, window) = window_of(self, &surface).unwrap_or_else(|| {
            let window = Window::new_x11_window(surface.clone());
            let id = NEXT_WINDOW_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let output = self.pointer_display();
            window
                .user_data()
                .insert_if_missing_threadsafe(|| WindowMeta {
                    id,
                    output: std::sync::atomic::AtomicU32::new(output),
                    placed: std::sync::atomic::AtomicBool::new(true),
                    parked: std::sync::atomic::AtomicBool::new(false),
                });
            self.xwm.windows.insert(id, window.clone());
            (id, window)
        });
        let _ = surface.set_mapped(true);
        if wm::active(self) && !wm::knows(self, id) {
            wm::window_added(self, &window);
        }
        let rect = wm::rect_of(self, id)
            .filter(|r| r.w > 0 && r.h > 0)
            .unwrap_or_else(|| {
                let at = surface.last_configure().loc;
                let size = surface.geometry().size;
                Rect::new(at.x, at.y, size.w.max(1), size.h.max(1))
            });
        let _ = surface.configure(Some(Rectangle::<i32, Logical>::new(
            (rect.x, rect.y).into(),
            (rect.w, rect.h).into(),
        )));
        self.space
            .map_element(window.clone(), (rect.x, rect.y), true);
        wm::window_mapped(
            self,
            &window,
            Size {
                w: rect.w,
                h: rect.h,
            },
        );
        crate::wayland::foreign_toplevel::add(self, &window);
        publish_frame(self, id);
        publish_stacking(self);
        if !wm::active(self)
            && let Some(keyboard) = self.seat.get_keyboard()
        {
            keyboard.set_focus(
                self,
                Some(FocusTarget::Window(window)),
                crate::wayland::frontend::next_serial(),
            );
        }
        self.refocus_pointer();
    }

    fn map_window_notify(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(extras) = self.xwm.extras.as_ref() {
            extras.watch_focus(surface.window_id());
        }
        let location = root_location(&surface);
        let window = Window::new_x11_window(surface);
        window
            .user_data()
            .insert_if_missing_threadsafe(|| WindowMeta {
                id: NEXT_WINDOW_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                output: std::sync::atomic::AtomicU32::new(self.pointer_display()),
                placed: std::sync::atomic::AtomicBool::new(true),
                parked: std::sync::atomic::AtomicBool::new(false),
            });
        self.space.map_element(window, location, true);
        self.refocus_pointer();
        for node in &mut self.output_nodes {
            if let Some(cap) = node.capture.as_mut() {
                cap.change_pending = true;
            }
        }
    }

    fn unmapped_window(&mut self, _xwm: XwmId, surface: X11Surface) {
        let found = self
            .space
            .elements()
            .find(|w| w.x11_surface() == Some(&surface))
            .cloned();
        if let Some(window) = found {
            wm::snapshot(self, &window);
            let held_keyboard = surface.is_override_redirect()
                && self.seat.get_keyboard().is_some_and(|k| {
                    k.current_focus() == Some(FocusTarget::Window(window.clone()))
                });
            self.space.unmap_elem(&window);
            if held_keyboard {
                wm::hand_keyboard(self, None);
            }
        }
        if let Some((id, window)) = window_of(self, &surface) {
            wm::window_unmapped(self, &window);
            crate::wayland::foreign_toplevel::remove(self, id);
            if let Some(wm) = self.wm.as_mut() {
                wm.hidden.remove(&id);
            }
        }
        if !surface.is_override_redirect() {
            let _ = surface.set_mapped(false);
        }
        self.refocus_pointer();
        publish_stacking(self);
    }

    fn destroyed_window(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some((id, _)) = window_of(self, &surface) {
            self.xwm.windows.remove(&id);
            wm::window_removed(self, id);
            crate::wayland::foreign_toplevel::remove(self, id);
        }
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        surface: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        reorder: Option<Reorder>,
    ) {
        let mut geo = surface.last_configure();
        if let Some(x) = x {
            geo.loc.x = x;
        }
        if let Some(y) = y {
            geo.loc.y = y;
        }
        if let Some(w) = w {
            geo.size.w = w as i32;
        }
        if let Some(h) = h {
            geo.size.h = h as i32;
        }
        match window_of(self, &surface) {
            Some((id, _)) if wm::active(self) => {
                wm::request(
                    self,
                    id,
                    Request::Geometry(Rect::new(geo.loc.x, geo.loc.y, geo.size.w, geo.size.h)),
                );
                match reorder {
                    Some(Reorder::Top) => {
                        wm::request(self, id, Request::Raise);
                    }
                    Some(Reorder::Bottom) => {
                        wm::request(self, id, Request::Lower);
                    }
                    _ => {}
                }
            }
            _ => {
                let _ = surface.configure(Some(geo));
            }
        }
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        surface: X11Surface,
        _geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        if !surface.is_override_redirect() {
            return;
        }
        let found = self
            .space
            .elements()
            .find(|w| w.x11_surface() == Some(&surface))
            .cloned();
        if let Some(window) = found {
            self.space
                .map_element(window, root_location(&surface), false);
            for node in &mut self.output_nodes {
                if let Some(cap) = node.capture.as_mut() {
                    cap.change_pending = true;
                }
            }
        }
    }

    fn property_notify(&mut self, _xwm: XwmId, surface: X11Surface, property: WmWindowProperty) {
        if matches!(
            property,
            WmWindowProperty::Title
                | WmWindowProperty::Class
                | WmWindowProperty::TransientFor
                | WmWindowProperty::WindowType
                | WmWindowProperty::MotifHints
                | WmWindowProperty::NormalHints
                | WmWindowProperty::Hints
        ) && let Some((id, window)) = window_of(self, &surface)
        {
            wm::window_changed(self, &window);
            crate::wayland::foreign_toplevel::update(self, id);
        }
    }

    fn maximize_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some((id, _)) = window_of(self, &surface) {
            wm::request(self, id, Request::Maximize);
        }
    }

    fn unmaximize_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some((id, _)) = window_of(self, &surface) {
            wm::request(self, id, Request::Unmaximize);
        }
    }

    fn fullscreen_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some((id, _)) = window_of(self, &surface) {
            wm::request(self, id, Request::Fullscreen(None));
        }
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some((id, _)) = window_of(self, &surface) {
            wm::request(self, id, Request::Unfullscreen);
        }
    }

    fn minimize_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some((id, _)) = window_of(self, &surface) {
            wm::request(self, id, Request::Minimize);
        }
    }

    fn unminimize_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some((id, _)) = window_of(self, &surface) {
            wm::request(self, id, Request::Unminimize);
        }
    }

    fn above_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some((id, _)) = window_of(self, &surface) {
            wm::request(self, id, Request::Above(true));
        }
    }

    fn unabove_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some((id, _)) = window_of(self, &surface) {
            wm::request(self, id, Request::Above(false));
        }
    }

    fn below_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some((id, _)) = window_of(self, &surface) {
            wm::request(self, id, Request::Below(true));
        }
    }

    fn unbelow_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some((id, _)) = window_of(self, &surface) {
            wm::request(self, id, Request::Below(false));
        }
    }

    fn shade_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some((id, _)) = window_of(self, &surface) {
            wm::request(self, id, Request::Shade(true));
        }
    }

    fn unshade_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some((id, _)) = window_of(self, &surface) {
            wm::request(self, id, Request::Shade(false));
        }
    }

    fn demands_attention_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some((id, _)) = window_of(self, &surface) {
            wm::request(self, id, Request::Attention(true));
        }
    }

    fn undemands_attention_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some((id, _)) = window_of(self, &surface) {
            wm::request(self, id, Request::Attention(false));
        }
    }

    fn resize_request(
        &mut self,
        _xwm: XwmId,
        surface: X11Surface,
        _button: u32,
        edges: X11ResizeEdge,
    ) {
        let Some((id, _)) = window_of(self, &surface) else {
            return;
        };
        let edge = match edges {
            X11ResizeEdge::Top => ResizeEdge::Top,
            X11ResizeEdge::Bottom => ResizeEdge::Bottom,
            X11ResizeEdge::Left => ResizeEdge::Left,
            X11ResizeEdge::Right => ResizeEdge::Right,
            X11ResizeEdge::TopLeft => ResizeEdge::TopLeft,
            X11ResizeEdge::TopRight => ResizeEdge::TopRight,
            X11ResizeEdge::BottomLeft => ResizeEdge::BottomLeft,
            X11ResizeEdge::BottomRight => ResizeEdge::BottomRight,
        };
        wm::request(self, id, Request::Resize(edge));
    }

    fn move_request(&mut self, _xwm: XwmId, surface: X11Surface, _button: u32) {
        if let Some((id, _)) = window_of(self, &surface) {
            wm::request(self, id, Request::Move);
        }
    }

    fn active_window_request(
        &mut self,
        _xwm: XwmId,
        surface: X11Surface,
        _timestamp: u32,
        _current: Option<X11Surface>,
    ) {
        if let Some((id, _)) = window_of(self, &surface) {
            wm::request(self, id, Request::Activate);
        }
    }

    fn allow_selection_access(&mut self, xwm: XwmId, _selection: SelectionTarget) -> bool {
        self.seat
            .get_keyboard()
            .and_then(|k| k.current_focus())
            .is_some_and(|f| matches!(f, FocusTarget::Window(w) if w.x11_surface().and_then(|s| s.xwm_id()) == Some(xwm)))
    }

    fn send_selection(
        &mut self,
        _xwm: XwmId,
        selection: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
    ) {
        let owned = match selection {
            SelectionTarget::Clipboard => {
                current_data_device_selection_userdata(&self.seat).map(|e| e.clone())
            }
            SelectionTarget::Primary => {
                current_primary_selection_userdata(&self.seat).map(|e| e.clone())
            }
        };
        if let Some(entries) = owned.filter(|e| !e.is_empty()) {
            crate::wayland::frontend::serve_selection(entries, &mime_type, fd);
            return;
        }
        match selection {
            SelectionTarget::Clipboard => {
                let _ = request_data_device_client_selection(&self.seat, mime_type, fd);
            }
            SelectionTarget::Primary => {
                let _ = request_primary_client_selection(&self.seat, mime_type, fd);
            }
        }
    }

    fn new_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        let dh = self.dh.clone();
        match selection {
            SelectionTarget::Clipboard => {
                let flavors = crate::wayland::frontend::clipboard_flavors(&mime_types);
                set_data_device_selection(&dh, &self.seat, mime_types, Arc::new(Vec::new()));
                self.current_selection_mimes.clone_from(&flavors);
                if self.clipboard_callback.is_some() {
                    self.pending_clipboard_read = Some(flavors);
                }
            }
            SelectionTarget::Primary => {
                set_primary_selection(&dh, &self.seat, mime_types, Arc::new(Vec::new()));
            }
        }
    }

    fn cleared_selection(&mut self, _xwm: XwmId, selection: SelectionTarget) {
        let dh = self.dh.clone();
        match selection {
            SelectionTarget::Clipboard => {
                if current_data_device_selection_userdata(&self.seat).is_some() {
                    clear_data_device_selection(&dh, &self.seat);
                }
            }
            SelectionTarget::Primary => {
                if current_primary_selection_userdata(&self.seat).is_some() {
                    clear_primary_selection(&dh, &self.seat);
                }
            }
        }
    }

    fn show_desktop_request(&mut self, _xwm: XwmId) {
        wm::show_desktop(self, true);
    }

    fn unshow_desktop_request(&mut self, _xwm: XwmId) {
        wm::show_desktop(self, false);
    }

    fn disconnected(&mut self, _xwm: XwmId) {
        eprintln!("[Wayland] Xwayland disconnected; starting it again in {RESTART_DELAY:?}.");
        let ids: Vec<WindowId> = self.xwm.windows.keys().copied().collect();
        for id in ids {
            if let Some(window) = self.xwm.windows.remove(&id) {
                self.space.unmap_elem(&window);
            }
            wm::window_removed(self, id);
            crate::wayland::foreign_toplevel::remove(self, id);
        }
        self.xwm.wm = None;
        self.xwm.display = None;
        self.xwm.extras = None;
        self.xwm.restart_at = Some(Instant::now() + RESTART_DELAY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_named_xwayland_is_found_first_and_a_missing_one_is_not_named() {
        let dir = std::env::temp_dir().join(format!("pixelflux-xwayland-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("Xwayland");
        std::fs::write(&bin, "").unwrap();
        assert_eq!(
            xwayland_path(Some(bin.clone().into()), Some("/usr/bin:/bin".into())),
            Some(format!("{}:/usr/bin:/bin", dir.display()))
        );
        assert_eq!(
            xwayland_path(Some(bin.into()), None),
            Some(dir.display().to_string())
        );
        assert_eq!(
            xwayland_path(Some(dir.join("missing").into()), Some("/usr/bin".into())),
            None
        );
        assert_eq!(xwayland_path(None, Some("/usr/bin".into())), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
