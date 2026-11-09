//! The X11 backend: a dock window for the bar and a desktop window for the
//! wallpaper, shaped so only the drawn parts take input, ordinary windows for
//! the file manager, and the window list read from the window manager's EWMH
//! properties.  A file drag rides the pointer grab of the press that started
//! it, and its target is found from the stacking order the window manager
//! publishes.

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};

use x11rb::connection::{Connection as _, RequestConnection as _};
use x11rb::protocol::Event as XEvent;
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use super::Backend;
use crate::desktop::Frame;
use crate::event::{Button, Event, Modifiers, Request, SurfaceId, ToplevelState, WindowAction};
use crate::geom::Rect;
use crate::ui;
use crate::xkb::Keymap;

/// Largest output scale honored.
const MAX_SCALE: i32 = 4;
/// Window class of the file manager windows.
const FILES_APP_ID: &str = super::wayland::FILES_APP_ID;
/// Longest `_NET_WM_ICON` property read, in 32-bit units.
const MAX_ICON_LENGTH: u32 = 1 << 20;
/// Pixels one wheel click scrolls.
const WHEEL_STEP: f64 = 15.0;
/// The `_NET_WM_DESKTOP` value of a window on every desktop.
const ALL_DESKTOPS: u32 = 0xFFFF_FFFF;

x11rb::atom_manager! {
    pub Atoms: AtomsCookie {
        UTF8_STRING,
        WM_CHANGE_STATE,
        WM_PROTOCOLS,
        WM_DELETE_WINDOW,
        _NET_CLIENT_LIST,
        _NET_CLIENT_LIST_STACKING,
        _NET_ACTIVE_WINDOW,
        _NET_CLOSE_WINDOW,
        _NET_WM_NAME,
        _NET_WM_PID,
        _NET_WM_ICON,
        _NET_WM_DESKTOP,
        _NET_NUMBER_OF_DESKTOPS,
        _NET_CURRENT_DESKTOP,
        _NET_WM_STATE,
        _NET_WM_STATE_HIDDEN,
        _NET_WM_STATE_STICKY,
        _NET_WM_STATE_SKIP_TASKBAR,
        _NET_WM_STATE_SKIP_PAGER,
        _NET_WM_STATE_MAXIMIZED_VERT,
        _NET_WM_STATE_MAXIMIZED_HORZ,
        _NET_WM_STATE_FULLSCREEN,
        _NET_WM_WINDOW_TYPE,
        _NET_WM_WINDOW_TYPE_DOCK,
        _NET_WM_WINDOW_TYPE_DESKTOP,
        _NET_WM_WINDOW_TYPE_NORMAL,
        _NET_WM_STRUT,
        _NET_WM_STRUT_PARTIAL,
    }
}

/// A shared memory segment the server reads pixels from.
struct ShmSegment {
    seg: shm::Seg,
    map: memmap2::MmapMut,
    _fd: OwnedFd,
}

/// One of the desktop's windows with the pixels it shows.
struct X11Surface {
    window: xproto::Window,
    gc: xproto::Gcontext,
    width: i32,
    height: i32,
    shm: Option<ShmSegment>,
    pixels: Vec<u8>,
    visible: Vec<Rect>,
}

/// A file manager window.
struct X11Window {
    id: u32,
    surface: X11Surface,
}

/// A file drag in progress.
struct X11Drag {
    origin: xproto::Window,
    target: Option<SurfaceId>,
}

/// The X11 backend.
pub struct X11Backend {
    conn: RustConnection,
    screen: usize,
    root: xproto::Window,
    depth: u8,
    atoms: Atoms,
    panel: X11Surface,
    desktop: X11Surface,
    screen_w: i32,
    screen_h: i32,
    /// The monitors in physical root coordinates, the primary first.
    monitors: Vec<Rect>,
    scale: i32,
    shm_works: bool,
    toplevels: HashMap<xproto::Window, u64>,
    next_id: u64,
    /// The virtual desktops the window manager publishes on the root, and
    /// the current one.
    desktops: u32,
    current_desktop: u32,
    last_time: xproto::Timestamp,
    grabbed: bool,
    popup_open: bool,
    keymap: Keymap,
    cursor_hand: Option<xproto::Cursor>,
    cursor_arrow: Option<xproto::Cursor>,
    cursor_drag: Option<xproto::Cursor>,
    pointer_shape: bool,
    panel_top: bool,
    panel_height: i32,
    /// Space between the bar and the screen edges, which floats it and
    /// rounds every corner of it.
    panel_margin: i32,
    panel_autohide: bool,
    events: Vec<Event>,
    windows: Vec<X11Window>,
    drag: Option<X11Drag>,
}

impl X11Backend {
    /// Connects to the X server and sets up the dock and desktop windows along
    /// with the window list tracking on the root window.
    ///
    /// # Errors
    /// Why the display cannot be used.
    pub fn connect() -> Result<X11Backend, String> {
        let (conn, screen) = RustConnection::connect(None).map_err(|e| format!("{e}"))?;
        let setup = conn.setup().clone();
        let root_screen = &setup.roots[screen];
        let root = root_screen.root;
        let depth = root_screen.root_depth;
        if conn
            .extension_information(shape::X11_EXTENSION_NAME)
            .map_err(|e| format!("{e}"))?
            .is_none()
        {
            return Err("server lacks the shape extension".to_string());
        }
        let atoms = Atoms::new(&conn)
            .map_err(|e| format!("{e}"))?
            .reply()
            .map_err(|e| format!("{e}"))?;
        let shm_works = conn
            .extension_information(shm::X11_EXTENSION_NAME)
            .ok()
            .flatten()
            .is_some()
            && conn
                .shm_query_version()
                .ok()
                .and_then(|c| c.reply().ok())
                .is_some();

        let screen_w = root_screen.width_in_pixels as i32;
        let screen_h = root_screen.height_in_pixels as i32;

        conn.change_window_attributes(
            root,
            &xproto::ChangeWindowAttributesAux::new()
                .event_mask(xproto::EventMask::PROPERTY_CHANGE | xproto::EventMask::STRUCTURE_NOTIFY),
        )
        .map_err(|e| format!("{e}"))?;

        let desktop = create_window(
            &conn,
            &atoms,
            root_screen,
            "selkies-desktop wallpaper",
            atoms._NET_WM_WINDOW_TYPE_DESKTOP,
            xproto::EventMask::EXPOSURE
                | xproto::EventMask::BUTTON_PRESS
                | xproto::EventMask::BUTTON_RELEASE
                | xproto::EventMask::POINTER_MOTION
                | xproto::EventMask::LEAVE_WINDOW
                | xproto::EventMask::STRUCTURE_NOTIFY,
        )?;
        let panel = create_window(
            &conn,
            &atoms,
            root_screen,
            "selkies-desktop panel",
            atoms._NET_WM_WINDOW_TYPE_DOCK,
            xproto::EventMask::EXPOSURE
                | xproto::EventMask::BUTTON_PRESS
                | xproto::EventMask::BUTTON_RELEASE
                | xproto::EventMask::POINTER_MOTION
                | xproto::EventMask::LEAVE_WINDOW
                | xproto::EventMask::KEY_PRESS
                | xproto::EventMask::STRUCTURE_NOTIFY,
        )?;

        let (cursor_arrow, cursor_hand, cursor_drag) = load_cursors(&conn, screen);

        let mut backend = X11Backend {
            conn,
            screen,
            root,
            depth,
            atoms,
            panel,
            desktop,
            screen_w,
            screen_h,
            monitors: Vec::new(),
            scale: 1,
            shm_works,
            toplevels: HashMap::new(),
            next_id: 1,
            desktops: 1,
            current_desktop: 0,
            last_time: x11rb::CURRENT_TIME,
            grabbed: false,
            popup_open: false,
            keymap: Keymap::default(),
            cursor_hand,
            cursor_arrow,
            cursor_drag,
            pointer_shape: false,
            panel_top: false,
            panel_height: ui::PANEL_HEIGHT,
            panel_margin: 0,
            panel_autohide: false,
            events: Vec::new(),
            windows: Vec::new(),
            drag: None,
        };
        let _ = backend.conn.randr_select_input(
            root,
            randr::NotifyMask::SCREEN_CHANGE
                | randr::NotifyMask::CRTC_CHANGE
                | randr::NotifyMask::OUTPUT_CHANGE,
        );
        backend.scale = backend.read_scale();
        backend.monitors = backend.read_monitors();
        backend.load_keymap();
        backend.apply_geometry();
        backend
            .conn
            .map_window(backend.desktop.window)
            .map_err(|e| format!("{e}"))?;
        backend
            .conn
            .map_window(backend.panel.window)
            .map_err(|e| format!("{e}"))?;
        backend.sync_desktops();
        backend.sync_clients();
        backend.conn.flush().map_err(|e| format!("{e}"))?;
        Ok(backend)
    }

    /// Reads a cardinal property's first value.
    fn read_cardinal(&self, window: xproto::Window, property: xproto::Atom) -> Option<u32> {
        self.property(window, property, xproto::AtomEnum::CARDINAL.into(), 1)
            .and_then(|r| r.value32().and_then(|mut v| v.next()))
    }

    /// Reads the desktop count and the current desktop from the root and
    /// reports them when they changed; a changed current desktop also
    /// refreshes every window's hidden flag, which is relative to it.
    fn sync_desktops(&mut self) {
        let count = self
            .read_cardinal(self.root, self.atoms._NET_NUMBER_OF_DESKTOPS)
            .unwrap_or(1)
            .max(1);
        let current = self
            .read_cardinal(self.root, self.atoms._NET_CURRENT_DESKTOP)
            .unwrap_or(0)
            .min(count - 1);
        if (count, current) == (self.desktops, self.current_desktop) {
            return;
        }
        let moved = current != self.current_desktop;
        self.desktops = count;
        self.current_desktop = current;
        self.events.push(Event::Desktops { count, current });
        if moved {
            self.sync_active();
        }
    }

    /// Derives the integer UI scale from the `Xft.dpi` resource, the value
    /// toolkits follow for HiDPI on X11 where outputs carry no scale of their own.
    fn read_scale(&self) -> i32 {
        let Ok(db) = x11rb::resource_manager::new_from_default(&self.conn) else {
            return 1;
        };
        let dpi: Option<f64> = db.get_value("Xft.dpi", "Xft.Dpi").ok().flatten();
        dpi.map(|d| ((d + 48.0) / 96.0) as i32)
            .unwrap_or(1)
            .clamp(1, MAX_SCALE)
    }

    /// Reads the keyboard mapping for the menu's search box.
    fn load_keymap(&mut self) {
        let setup = self.conn.setup();
        let first = setup.min_keycode;
        let count = setup.max_keycode.saturating_sub(first).saturating_add(1);
        if let Ok(cookie) = self.conn.get_keyboard_mapping(first, count)
            && let Ok(reply) = cookie.reply()
        {
            self.keymap = Keymap::from_x11(first as u32, reply.keysyms_per_keycode as usize, &reply.keysyms);
        }
    }

    /// Reads the RandR monitors, the primary first, falling back to the whole
    /// screen when the server reports none.
    fn read_monitors(&self) -> Vec<Rect> {
        let mut monitors: Vec<(bool, Rect)> = self
            .conn
            .randr_get_monitors(self.root, true)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|reply| {
                reply
                    .monitors
                    .iter()
                    .filter(|m| m.width > 0 && m.height > 0)
                    .map(|m| {
                        (
                            m.primary,
                            Rect::new(m.x as i32, m.y as i32, m.width as i32, m.height as i32),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        monitors.sort_by_key(|(primary, r)| (!primary, r.x, r.y));
        let mut rects: Vec<Rect> = monitors.into_iter().map(|(_, r)| r).collect();
        if rects.is_empty() {
            rects.push(Rect::new(0, 0, self.screen_w, self.screen_h));
        }
        rects
    }

    /// Reads the monitors again after a RandR change and lays the windows out
    /// when they moved.
    fn sync_monitors(&mut self) {
        let monitors = self.read_monitors();
        if monitors != self.monitors {
            self.monitors = monitors;
            self.apply_geometry();
        }
    }

    /// Lays both windows out for the current screen size and scale and tells
    /// the desktop their sizes.  The panel sits along the bottom of the
    /// primary monitor and spans the bar plus the popup area rather than the
    /// full screen, which keeps its pixel buffer small; the desktop window
    /// covers every monitor.  Runs at startup and whenever the screen or its
    /// monitors change.
    fn apply_geometry(&mut self) {
        let s = self.scale;
        let primary = self
            .monitors
            .first()
            .copied()
            .unwrap_or(Rect::new(0, 0, self.screen_w, self.screen_h));
        let logical_w = (self.screen_w + s - 1) / s;
        let logical_h = (self.screen_h + s - 1) / s;
        let primary_h = (primary.h + s - 1) / s;
        let panel_h = (self.panel_height + ui::POPUP_AREA_HEIGHT).min(primary_h);

        let panel_y = if self.panel_top {
            primary.y
        } else {
            primary.bottom() - panel_h * s
        };
        set_fixed_geometry(
            &self.conn,
            self.panel.window,
            primary.x,
            panel_y,
            primary.w,
            panel_h * s,
        );
        set_fixed_geometry(
            &self.conn,
            self.desktop.window,
            0,
            0,
            self.screen_w,
            self.screen_h,
        );
        self.update_struts(primary);

        self.panel.width = primary.w;
        self.panel.height = panel_h * s;
        self.desktop.width = self.screen_w;
        self.desktop.height = self.screen_h;
        for surface in [&mut self.panel, &mut self.desktop] {
            surface.shm = None;
            surface.pixels.clear();
            surface.visible.clear();
        }

        self.events.push(Event::Configured {
            surface: SurfaceId::Background(0),
            width: logical_w,
            height: logical_h,
            scale: s,
        });
        self.events.push(Event::BackgroundLayout {
            surface: 0,
            monitors: self.monitors.iter().map(|m| scale_down(*m, s)).collect(),
            primary: true,
        });
        self.events.push(Event::Configured {
            surface: SurfaceId::Panel,
            width: (primary.w + s - 1) / s,
            height: panel_h,
            scale: s,
        });
    }

    /// Reserves the bar's strip at the bottom of the primary monitor so the
    /// window manager keeps maximized windows off it.
    ///
    /// # Arguments
    /// * `primary` - The primary monitor in physical root coordinates.
    fn update_struts(&self, primary: Rect) {
        let bar = if self.panel_autohide {
            0
        } else {
            self.panel_height * self.scale
        };
        let (top, bottom) = if self.panel_top {
            ((primary.y + bar).max(0) as u32, 0)
        } else {
            (0, (self.screen_h - primary.bottom() + bar).max(0) as u32)
        };
        let strut = [0u32, 0, top, bottom];
        let (start, end) = (primary.x.max(0) as u32, (primary.right() - 1).max(0) as u32);
        let partial = [0u32, 0, top, bottom, 0, 0, 0, 0, start, end, start, end];
        let _ = self.conn.change_property32(
            xproto::PropMode::REPLACE,
            self.panel.window,
            self.atoms._NET_WM_STRUT,
            xproto::AtomEnum::CARDINAL,
            &strut,
        );
        let _ = self.conn.change_property32(
            xproto::PropMode::REPLACE,
            self.panel.window,
            self.atoms._NET_WM_STRUT_PARTIAL,
            xproto::AtomEnum::CARDINAL,
            &partial,
        );
    }

    /// Fetches a property of a window.
    fn property(
        &self,
        window: xproto::Window,
        property: xproto::Atom,
        kind: xproto::Atom,
        max_len: u32,
    ) -> Option<xproto::GetPropertyReply> {
        let reply = self
            .conn
            .get_property(false, window, property, kind, 0, max_len)
            .ok()?
            .reply()
            .ok()?;
        if reply.value_len == 0 { None } else { Some(reply) }
    }

    /// Whether an atom list property on a window contains a value.
    fn property_has_atom(&self, window: xproto::Window, property: xproto::Atom, value: xproto::Atom) -> bool {
        self.property(window, property, xproto::AtomEnum::ATOM.into(), 1024)
            .and_then(|r| r.value32().map(|mut atoms| atoms.any(|a| a == value)))
            .unwrap_or(false)
    }

    /// Reads a window's title, preferring the UTF-8 EWMH name over the legacy one.
    fn read_title(&self, window: xproto::Window) -> String {
        let utf8 = self.property(window, self.atoms._NET_WM_NAME, self.atoms.UTF8_STRING, 1024);
        let reply = utf8.or_else(|| {
            self.property(
                window,
                xproto::AtomEnum::WM_NAME.into(),
                xproto::AtomEnum::ANY.into(),
                1024,
            )
        });
        reply
            .map(|r| String::from_utf8_lossy(&r.value).to_string())
            .unwrap_or_default()
    }

    /// Reads a window's class, falling back to its instance name.
    fn read_class(&self, window: xproto::Window) -> String {
        let Some(reply) = self.property(
            window,
            xproto::AtomEnum::WM_CLASS.into(),
            xproto::AtomEnum::STRING.into(),
            1024,
        ) else {
            return String::new();
        };
        let mut parts = reply.value.split(|b| *b == 0).filter(|p| !p.is_empty());
        let instance = parts.next().map(|p| String::from_utf8_lossy(p).to_string());
        let class = parts.next().map(|p| String::from_utf8_lossy(p).to_string());
        class.or(instance).unwrap_or_default()
    }

    /// Builds the icon a window publishes through `_NET_WM_ICON`, picking the
    /// smallest image that still covers twice the bar icon size or the largest
    /// available.
    fn read_icon(&self, window: xproto::Window) -> Option<tiny_skia::Pixmap> {
        let reply = self.property(
            window,
            self.atoms._NET_WM_ICON,
            xproto::AtomEnum::CARDINAL.into(),
            MAX_ICON_LENGTH,
        )?;
        let data: Vec<u32> = reply.value32()?.collect();
        let target = (ui::ICON_SIZE * 2) as usize;
        let mut best: Option<(usize, usize, usize)> = None;
        let mut pos = 0usize;
        while pos + 2 <= data.len() {
            let w = data[pos] as usize;
            let h = data[pos + 1] as usize;
            if w == 0 || h == 0 || w > 1024 || h > 1024 || pos + 2 + w * h > data.len() {
                break;
            }
            let better = match best {
                None => true,
                Some((bw, _, _)) if bw >= target => w >= target && w < bw,
                Some((bw, _, _)) => w > bw,
            };
            if better {
                best = Some((w, h, pos + 2));
            }
            pos += 2 + w * h;
        }
        let (w, h, start) = best?;
        let mut pix = tiny_skia::Pixmap::new(w as u32, h as u32)?;
        let pixels = pix.data_mut();
        for (i, argb) in data[start..start + w * h].iter().enumerate() {
            let a = argb >> 24;
            let r = ((argb >> 16) & 0xff) * a / 255;
            let g = ((argb >> 8) & 0xff) * a / 255;
            let b = (argb & 0xff) * a / 255;
            pixels[i * 4] = r as u8;
            pixels[i * 4 + 1] = g as u8;
            pixels[i * 4 + 2] = b as u8;
            pixels[i * 4 + 3] = a as u8;
        }
        Some(pix)
    }

    /// Reads a window's state flags.  A window is hidden when its
    /// `_NET_WM_DESKTOP` names another desktop than the current one; a
    /// window on all desktops (`0xFFFFFFFF`) or without the property never is.
    fn read_state(&self, window: xproto::Window) -> ToplevelState {
        let atoms: Vec<xproto::Atom> = self
            .property(
                window,
                self.atoms._NET_WM_STATE,
                xproto::AtomEnum::ATOM.into(),
                1024,
            )
            .and_then(|r| r.value32().map(|v| v.collect()))
            .unwrap_or_default();
        let active = self
            .property(
                self.root,
                self.atoms._NET_ACTIVE_WINDOW,
                xproto::AtomEnum::WINDOW.into(),
                1,
            )
            .and_then(|r| r.value32().and_then(|mut v| v.next()))
            .unwrap_or(0);
        let hidden = self
            .read_cardinal(window, self.atoms._NET_WM_DESKTOP)
            .is_some_and(|desktop| desktop != ALL_DESKTOPS && desktop != self.current_desktop);
        ToplevelState {
            minimized: atoms.contains(&self.atoms._NET_WM_STATE_HIDDEN),
            active: active == window,
            maximized: atoms.contains(&self.atoms._NET_WM_STATE_MAXIMIZED_VERT)
                || atoms.contains(&self.atoms._NET_WM_STATE_MAXIMIZED_HORZ),
            fullscreen: atoms.contains(&self.atoms._NET_WM_STATE_FULLSCREEN),
            hidden,
        }
    }

    /// Decides whether a managed window belongs on the taskbar: docks,
    /// desktops and windows that opt out through their EWMH state do not.
    fn wants_button(&self, window: xproto::Window) -> bool {
        if self.property_has_atom(
            window,
            self.atoms._NET_WM_WINDOW_TYPE,
            self.atoms._NET_WM_WINDOW_TYPE_DOCK,
        ) || self.property_has_atom(
            window,
            self.atoms._NET_WM_WINDOW_TYPE,
            self.atoms._NET_WM_WINDOW_TYPE_DESKTOP,
        ) {
            return false;
        }
        !self.property_has_atom(
            window,
            self.atoms._NET_WM_STATE,
            self.atoms._NET_WM_STATE_SKIP_TASKBAR,
        )
    }

    /// Reconciles the taskbar with the window manager's client list and
    /// subscribes to property changes on every client.
    fn sync_clients(&mut self) {
        let list: Vec<xproto::Window> = self
            .property(
                self.root,
                self.atoms._NET_CLIENT_LIST,
                xproto::AtomEnum::WINDOW.into(),
                4096,
            )
            .and_then(|r| r.value32().map(|v| v.collect()))
            .unwrap_or_default();
        let mut eligible = Vec::new();
        for window in &list {
            if *window == self.panel.window || *window == self.desktop.window {
                continue;
            }
            if self.windows.iter().any(|w| w.surface.window == *window) {
                eligible.push(*window);
                continue;
            }
            let _ = self.conn.change_window_attributes(
                *window,
                &xproto::ChangeWindowAttributesAux::new().event_mask(xproto::EventMask::PROPERTY_CHANGE),
            );
            if self.wants_button(*window) {
                eligible.push(*window);
            }
        }
        let gone: Vec<xproto::Window> = self
            .toplevels
            .keys()
            .filter(|w| !eligible.contains(w))
            .copied()
            .collect();
        for window in gone {
            if let Some(id) = self.toplevels.remove(&window) {
                self.events.push(Event::ToplevelClosed { id });
            }
        }
        for window in eligible {
            if self.toplevels.contains_key(&window) {
                continue;
            }
            let id = self.next_id;
            self.next_id += 1;
            self.toplevels.insert(window, id);
            self.events.push(Event::ToplevelAdded { id });
            self.events.push(Event::ToplevelTitle {
                id,
                title: self.read_title(window),
            });
            self.events.push(Event::ToplevelAppId {
                id,
                app_id: self.read_class(window),
            });
            if let Some(icon) = self.read_icon(window) {
                self.events.push(Event::ToplevelIcon {
                    id,
                    icon: Box::new(icon),
                });
            }
            self.events.push(Event::ToplevelState {
                id,
                state: self.read_state(window),
            });
        }
        self.sync_active();
    }

    /// Refreshes the active flag of every window.
    fn sync_active(&mut self) {
        let windows: Vec<(xproto::Window, u64)> = self.toplevels.iter().map(|(w, id)| (*w, *id)).collect();
        for (window, id) in windows {
            self.events.push(Event::ToplevelState {
                id,
                state: self.read_state(window),
            });
        }
    }

    /// Sends an EWMH client message about a window to the window manager.
    fn send_wm_message(&self, window: xproto::Window, kind: xproto::Atom, data: [u32; 5]) {
        let event = xproto::ClientMessageEvent::new(32, window, kind, data);
        let _ = self.conn.send_event(
            false,
            self.root,
            xproto::EventMask::SUBSTRUCTURE_REDIRECT | xproto::EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        );
    }

    /// Holds or releases the pointer and keyboard while a popup is open, so a
    /// click anywhere reaches the panel and can dismiss it and typed keys
    /// reach the search box.
    fn sync_grabs(&mut self) {
        if self.popup_open && !self.grabbed {
            let pointer = self
                .conn
                .grab_pointer(
                    false,
                    self.panel.window,
                    xproto::EventMask::BUTTON_PRESS
                        | xproto::EventMask::BUTTON_RELEASE
                        | xproto::EventMask::POINTER_MOTION,
                    xproto::GrabMode::ASYNC,
                    xproto::GrabMode::ASYNC,
                    x11rb::NONE,
                    x11rb::NONE,
                    x11rb::CURRENT_TIME,
                )
                .ok()
                .and_then(|c| c.reply().ok())
                .is_some_and(|r| r.status == xproto::GrabStatus::SUCCESS);
            let _ = self.conn.grab_keyboard(
                false,
                self.panel.window,
                x11rb::CURRENT_TIME,
                xproto::GrabMode::ASYNC,
                xproto::GrabMode::ASYNC,
            );
            self.grabbed = pointer;
        } else if !self.popup_open && self.grabbed {
            let _ = self.conn.ungrab_pointer(x11rb::CURRENT_TIME);
            let _ = self.conn.ungrab_keyboard(x11rb::CURRENT_TIME);
            self.grabbed = false;
        }
    }

    /// Sets the shape of a window to its visible rectangles, the popups with
    /// rounded corners and a bar narrower than the window with its two
    /// corners away from the screen edge rounded, and its input shape to the
    /// input rectangles.
    fn apply_shape(&self, surface: &X11Surface, input: &[Rect], scale: i32, round_popups: bool) {
        let mut bounding: Vec<xproto::Rectangle> = Vec::new();
        for (i, rect) in surface.visible.iter().enumerate() {
            if i == 0 && round_popups && self.panel_margin > 0 {
                add_rounded_shape(&mut bounding, *rect, (ui::panel_radius() as i32) * scale);
            } else if i == 0 && round_popups && rect.w < surface.width {
                let overflow = (ui::panel_radius() as i32) * scale;
                let extended = if self.panel_top {
                    Rect::new(rect.x, rect.y - overflow, rect.w, rect.h + overflow)
                } else {
                    Rect::new(rect.x, rect.y, rect.w, rect.h + overflow)
                };
                add_rounded_shape(&mut bounding, extended, overflow);
            } else if i == 0 || !round_popups {
                push_rect(&mut bounding, *rect);
            } else {
                add_rounded_shape(&mut bounding, *rect, (ui::popup_radius() as i32) * scale);
            }
        }
        let _ = self.conn.shape_rectangles(
            shape::SO::SET,
            shape::SK::BOUNDING,
            xproto::ClipOrdering::UNSORTED,
            surface.window,
            0,
            0,
            &bounding,
        );
        let mut input_rects: Vec<xproto::Rectangle> = Vec::new();
        for rect in input {
            push_rect(&mut input_rects, rect.scaled(scale));
        }
        let _ = self.conn.shape_rectangles(
            shape::SO::SET,
            shape::SK::INPUT,
            xproto::ClipOrdering::UNSORTED,
            surface.window,
            0,
            0,
            &input_rects,
        );
    }

    /// Copies the visible rectangles of a surface's pixels to its window.
    fn blit(&self, surface: &X11Surface, rects: &[Rect]) {
        if surface.pixels.is_empty() && surface.shm.is_none() {
            return;
        }
        let (w, h) = (surface.width, surface.height);
        for rect in rects {
            let r = rect.intersect(&Rect::new(0, 0, w, h));
            if r.is_empty() {
                continue;
            }
            if let Some(seg) = &surface.shm {
                let _ = self.conn.shm_put_image(
                    surface.window,
                    surface.gc,
                    w as u16,
                    h as u16,
                    r.x as u16,
                    r.y as u16,
                    r.w as u16,
                    r.h as u16,
                    r.x as i16,
                    r.y as i16,
                    self.depth,
                    xproto::ImageFormat::Z_PIXMAP.into(),
                    false,
                    seg.seg,
                    0,
                );
            } else {
                let max_bytes = self.conn.maximum_request_bytes().saturating_sub(64);
                let rows_per_request = (max_bytes / (r.w as usize * 4)).max(1) as i32;
                let stride = w as usize * 4;
                let mut y = r.y;
                while y < r.bottom() {
                    let rows = rows_per_request.min(r.bottom() - y);
                    let mut data = Vec::with_capacity(rows as usize * r.w as usize * 4);
                    for row in y..y + rows {
                        let start = row as usize * stride + r.x as usize * 4;
                        data.extend_from_slice(&surface.pixels[start..start + r.w as usize * 4]);
                    }
                    let _ = self.conn.put_image(
                        xproto::ImageFormat::Z_PIXMAP,
                        surface.window,
                        surface.gc,
                        r.w as u16,
                        rows as u16,
                        r.x as i16,
                        y as i16,
                        0,
                        self.depth,
                        &data,
                    );
                    y += rows;
                }
            }
        }
    }

    /// Sets every window's cursor to the arrow or the hand, as last requested.
    fn apply_cursor(&self) {
        let cursor = if self.pointer_shape {
            self.cursor_hand
        } else {
            self.cursor_arrow
        };
        let cursor = cursor.unwrap_or(x11rb::NONE);
        let windows = [self.panel.window, self.desktop.window]
            .into_iter()
            .chain(self.windows.iter().map(|w| w.surface.window));
        for window in windows {
            let _ = self
                .conn
                .change_window_attributes(window, &xproto::ChangeWindowAttributesAux::new().cursor(cursor));
        }
    }

    /// Mutable access to a surface by identifier.
    fn surface_mut(&mut self, id: SurfaceId) -> Option<&mut X11Surface> {
        match id {
            SurfaceId::Background(_) => Some(&mut self.desktop),
            SurfaceId::Panel => Some(&mut self.panel),
            SurfaceId::Window(w) => self
                .windows
                .iter_mut()
                .find(|x| x.id == w)
                .map(|x| &mut x.surface),
        }
    }

    /// Shared access to a surface by identifier.
    fn surface_ref(&self, id: SurfaceId) -> Option<&X11Surface> {
        match id {
            SurfaceId::Background(_) => Some(&self.desktop),
            SurfaceId::Panel => Some(&self.panel),
            SurfaceId::Window(w) => self.windows.iter().find(|x| x.id == w).map(|x| &x.surface),
        }
    }

    /// Creates and maps a file manager window of a logical size, with an icon
    /// for the window manager's title bar.
    fn create_toplevel(
        &mut self,
        id: u32,
        title: &str,
        width: i32,
        height: i32,
        icon: Option<&tiny_skia::Pixmap>,
    ) {
        if self.windows.iter().any(|w| w.id == id) {
            return;
        }
        let setup = self.conn.setup().clone();
        let screen = &setup.roots[self.screen];
        let Ok(mut surface) = create_window(
            &self.conn,
            &self.atoms,
            screen,
            title,
            self.atoms._NET_WM_WINDOW_TYPE_NORMAL,
            xproto::EventMask::EXPOSURE
                | xproto::EventMask::BUTTON_PRESS
                | xproto::EventMask::BUTTON_RELEASE
                | xproto::EventMask::POINTER_MOTION
                | xproto::EventMask::LEAVE_WINDOW
                | xproto::EventMask::KEY_PRESS
                | xproto::EventMask::STRUCTURE_NOTIFY,
        ) else {
            return;
        };
        let window = surface.window;
        let _ = self.conn.delete_property(window, self.atoms._NET_WM_STATE);
        let _ = self.conn.delete_property(window, self.atoms._NET_WM_DESKTOP);
        let _ = self.conn.change_property8(
            xproto::PropMode::REPLACE,
            window,
            xproto::AtomEnum::WM_CLASS,
            xproto::AtomEnum::STRING,
            format!("{FILES_APP_ID}\0{FILES_APP_ID}\0").as_bytes(),
        );
        let hints = x11rb::properties::WmHints {
            input: Some(true),
            ..x11rb::properties::WmHints::new()
        };
        let _ = hints.set(&self.conn, window);
        let _ = self.conn.change_property32(
            xproto::PropMode::REPLACE,
            window,
            self.atoms.WM_PROTOCOLS,
            xproto::AtomEnum::ATOM,
            &[self.atoms.WM_DELETE_WINDOW],
        );
        let _ = self.conn.change_property32(
            xproto::PropMode::REPLACE,
            window,
            self.atoms._NET_WM_PID,
            xproto::AtomEnum::CARDINAL,
            &[std::process::id()],
        );
        if let Some(icon) = icon {
            let mut data: Vec<u32> = vec![icon.width(), icon.height()];
            for px in icon.data().as_chunks::<4>().0 {
                let a = px[3] as u32;
                let unmul = |c: u8| (c as u32 * 255).checked_div(a).unwrap_or(0).min(255);
                data.push((a << 24) | (unmul(px[0]) << 16) | (unmul(px[1]) << 8) | unmul(px[2]));
            }
            let _ = self.conn.change_property32(
                xproto::PropMode::REPLACE,
                window,
                self.atoms._NET_WM_ICON,
                xproto::AtomEnum::CARDINAL,
                &data,
            );
        }
        let s = self.scale;
        let (w, h) = (width * s, height * s);
        let mut size_hints = x11rb::properties::WmSizeHints::new();
        size_hints.size = Some((
            x11rb::properties::WmSizeHintsSpecification::ProgramSpecified,
            w,
            h,
        ));
        size_hints.min_size = Some((ui::WINDOW_MIN_WIDTH * s, ui::WINDOW_MIN_HEIGHT * s));
        let _ = size_hints.set_normal_hints(&self.conn, window);
        let _ = self.conn.configure_window(
            window,
            &xproto::ConfigureWindowAux::new()
                .width(w.max(1) as u32)
                .height(h.max(1) as u32),
        );
        surface.width = w;
        surface.height = h;
        let _ = self.conn.map_window(window);
        self.windows.push(X11Window { id, surface });
        self.events.push(Event::Configured {
            surface: SurfaceId::Window(id),
            width,
            height,
            scale: s,
        });
    }

    /// Keeps every file manager window its logical size across a scale change: the window is
    /// asked for the pixel size that is at the new scale, its hints follow, and the desktop is
    /// told to draw it again at that scale, since a window whose pixel size does not change (a
    /// maximized one) would otherwise never be told.
    fn rescale_windows(&mut self, was: i32) {
        let s = self.scale;
        let mut configured = Vec::new();
        for window in &mut self.windows {
            let width = (window.surface.width + was - 1) / was.max(1);
            let height = (window.surface.height + was - 1) / was.max(1);
            let (w, h) = (width * s, height * s);
            let mut size_hints = x11rb::properties::WmSizeHints::new();
            size_hints.min_size = Some((ui::WINDOW_MIN_WIDTH * s, ui::WINDOW_MIN_HEIGHT * s));
            let _ = size_hints.set_normal_hints(&self.conn, window.surface.window);
            let _ = self.conn.configure_window(
                window.surface.window,
                &xproto::ConfigureWindowAux::new()
                    .width(w.max(1) as u32)
                    .height(h.max(1) as u32),
            );
            window.surface.width = w;
            window.surface.height = h;
            window.surface.shm = None;
            window.surface.pixels.clear();
            window.surface.visible.clear();
            configured.push((window.id, width, height));
        }
        for (id, width, height) in configured {
            self.events.push(Event::Configured {
                surface: SurfaceId::Window(id),
                width,
                height,
                scale: s,
            });
        }
    }

    /// Destroys a file manager window.
    fn destroy_window(&mut self, id: u32) {
        if let Some(index) = self.windows.iter().position(|w| w.id == id) {
            let window = self.windows.remove(index);
            let _ = self.conn.free_gc(window.surface.gc);
            let _ = self.conn.destroy_window(window.surface.window);
        }
    }

    /// Retitles a file manager window.
    fn set_title(&self, id: u32, title: &str) {
        let Some(window) = self.windows.iter().find(|w| w.id == id).map(|w| w.surface.window) else {
            return;
        };
        let _ = self.conn.change_property8(
            xproto::PropMode::REPLACE,
            window,
            xproto::AtomEnum::WM_NAME,
            xproto::AtomEnum::STRING,
            title.as_bytes(),
        );
        let _ = self.conn.change_property8(
            xproto::PropMode::REPLACE,
            window,
            self.atoms._NET_WM_NAME,
            self.atoms.UTF8_STRING,
            title.as_bytes(),
        );
    }

    /// The surface under a root position during a drag, with the position in
    /// its logical coordinates: the topmost client containing the point when
    /// it is one of the desktop's windows, the wallpaper when no client does.
    fn drag_target(&self, root_x: i32, root_y: i32) -> Option<(SurfaceId, f64, f64)> {
        let stacking: Vec<xproto::Window> = self
            .property(
                self.root,
                self.atoms._NET_CLIENT_LIST_STACKING,
                xproto::AtomEnum::WINDOW.into(),
                4096,
            )
            .and_then(|r| r.value32().map(|v| v.collect()))
            .unwrap_or_default();
        let s = self.scale as f64;
        for window in stacking.iter().rev() {
            if *window == self.desktop.window {
                continue;
            }
            let viewable = self
                .conn
                .get_window_attributes(*window)
                .ok()
                .and_then(|c| c.reply().ok())
                .is_some_and(|a| a.map_state == xproto::MapState::VIEWABLE);
            if !viewable {
                continue;
            }
            let Some(geometry) = self.conn.get_geometry(*window).ok().and_then(|c| c.reply().ok()) else {
                continue;
            };
            let Some(origin) = self
                .conn
                .translate_coordinates(*window, self.root, 0, 0)
                .ok()
                .and_then(|c| c.reply().ok())
            else {
                continue;
            };
            let rect = Rect::new(
                origin.dst_x as i32,
                origin.dst_y as i32,
                geometry.width as i32,
                geometry.height as i32,
            );
            if !rect.contains(root_x as f64, root_y as f64) {
                continue;
            }
            if *window == self.panel.window {
                let (px, py) = ((root_x - rect.x) as f64, (root_y - rect.y) as f64);
                if self.panel.visible.iter().any(|r| r.contains(px, py)) {
                    return None;
                }
                continue;
            }
            if let Some(ours) = self.windows.iter().find(|w| w.surface.window == *window) {
                return Some((
                    SurfaceId::Window(ours.id),
                    (root_x - rect.x) as f64 / s,
                    (root_y - rect.y) as f64 / s,
                ));
            }
            return None;
        }
        Some((SurfaceId::Background(0), root_x as f64 / s, root_y as f64 / s))
    }

    /// Follows the pointer during a drag, reporting the surface it is over.
    fn drag_motion(&mut self, root_x: i32, root_y: i32) {
        let Some(previous) = self.drag.as_ref().map(|d| d.target) else {
            return;
        };
        let target = self.drag_target(root_x, root_y);
        let current = target.map(|(surface, _, _)| surface);
        if previous != current
            && let Some(surface) = previous
        {
            self.events.push(Event::DragLeave { surface });
        }
        if let Some((surface, x, y)) = target {
            if previous == current {
                self.events.push(Event::DragMotion { surface, x, y });
            } else {
                self.events.push(Event::DragEnter { surface, x, y });
            }
        }
        if let Some(drag) = self.drag.as_mut() {
            drag.target = current;
        }
    }

    /// Ends a drag at a root position, dropping when a surface is under it.
    fn drag_release(&mut self, root_x: i32, root_y: i32, copy: bool) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        let _ = self.conn.ungrab_pointer(x11rb::CURRENT_TIME);
        let _ = drag.origin;
        if let Some((surface, x, y)) = self.drag_target(root_x, root_y) {
            self.events.push(Event::DragDrop { surface, x, y, copy });
        }
        self.events.push(Event::DragEnd);
        self.apply_cursor();
    }

    /// Starts a drag from a file manager window: takes the pointer with the
    /// drag cursor until the button is released.
    fn start_drag(&mut self, window: u32) {
        let Some(origin) = self
            .windows
            .iter()
            .find(|w| w.id == window)
            .map(|w| w.surface.window)
        else {
            self.events.push(Event::DragEnd);
            return;
        };
        let cursor = self.cursor_drag.unwrap_or(x11rb::NONE);
        let reply = self
            .conn
            .grab_pointer(
                false,
                origin,
                xproto::EventMask::BUTTON_RELEASE | xproto::EventMask::POINTER_MOTION,
                xproto::GrabMode::ASYNC,
                xproto::GrabMode::ASYNC,
                x11rb::NONE,
                cursor,
                self.last_time,
            )
            .map_err(|e| format!("{e}"))
            .and_then(|c| c.reply().map_err(|e| format!("{e}")));
        let grabbed = match &reply {
            Ok(r) => r.status == xproto::GrabStatus::SUCCESS,
            Err(_) => false,
        };
        if !grabbed {
            if std::env::var_os("SELKIES_DESKTOP_DEBUG").is_some() {
                eprintln!("selkies-desktop: drag grab failed: {reply:?}");
            }
            self.events.push(Event::DragEnd);
            return;
        }
        self.drag = Some(X11Drag { origin, target: None });
    }

    /// Handles a property change on the root window or a client.
    fn handle_property(&mut self, event: xproto::PropertyNotifyEvent) {
        if event.window == self.root {
            if event.atom == self.atoms._NET_CLIENT_LIST {
                self.sync_clients();
            } else if event.atom == self.atoms._NET_ACTIVE_WINDOW {
                self.sync_active();
            } else if event.atom == self.atoms._NET_NUMBER_OF_DESKTOPS
                || event.atom == self.atoms._NET_CURRENT_DESKTOP
            {
                self.sync_desktops();
            } else if event.atom == u32::from(xproto::AtomEnum::RESOURCE_MANAGER) {
                let (arrow, hand, drag) = load_cursors(&self.conn, self.screen);
                self.cursor_arrow = arrow;
                self.cursor_hand = hand;
                self.cursor_drag = drag;
                self.apply_cursor();
                let scale = self.read_scale();
                if scale != self.scale {
                    let was = self.scale;
                    self.scale = scale;
                    self.apply_geometry();
                    self.rescale_windows(was);
                }
            }
            return;
        }
        let Some(id) = self.toplevels.get(&event.window).copied() else {
            if event.atom == self.atoms._NET_WM_STATE || event.atom == self.atoms._NET_WM_WINDOW_TYPE {
                self.sync_clients();
            }
            return;
        };
        if event.atom == self.atoms._NET_WM_STATE || event.atom == self.atoms._NET_WM_WINDOW_TYPE {
            if !self.wants_button(event.window) {
                self.sync_clients();
            } else {
                self.events.push(Event::ToplevelState {
                    id,
                    state: self.read_state(event.window),
                });
            }
        } else if event.atom == self.atoms._NET_WM_DESKTOP {
            self.events.push(Event::ToplevelState {
                id,
                state: self.read_state(event.window),
            });
        } else if event.atom == self.atoms._NET_WM_NAME || event.atom == u32::from(xproto::AtomEnum::WM_NAME)
        {
            self.events.push(Event::ToplevelTitle {
                id,
                title: self.read_title(event.window),
            });
        } else if event.atom == u32::from(xproto::AtomEnum::WM_CLASS) {
            self.events.push(Event::ToplevelAppId {
                id,
                app_id: self.read_class(event.window),
            });
        } else if event.atom == self.atoms._NET_WM_ICON
            && let Some(icon) = self.read_icon(event.window)
        {
            self.events.push(Event::ToplevelIcon {
                id,
                icon: Box::new(icon),
            });
        }
    }

    /// The surface a window stands for.
    fn surface_of(&self, window: xproto::Window) -> SurfaceId {
        if window == self.desktop.window {
            SurfaceId::Background(0)
        } else if let Some(ours) = self.windows.iter().find(|w| w.surface.window == window) {
            SurfaceId::Window(ours.id)
        } else {
            SurfaceId::Panel
        }
    }

    /// Dispatches one X event.  Shapes are applied again whenever the window
    /// manager reparents one of the desktop's windows, because openbox only
    /// learns about a client's input shape from the change notification and
    /// never queries it when it starts managing a window.  An expose repaints
    /// the rectangle the server names rather than the surface's visible
    /// rectangles: the desktop is presented as the wallpaper and then the
    /// launcher icons, so its visible list is the icon cells, and a window
    /// closing over it would leave the server's background between them.
    fn handle_event(&mut self, event: XEvent) {
        match event {
            XEvent::Expose(e) => {
                let id = self.surface_of(e.window);
                if let Some(surface) = self.surface_ref(id) {
                    let exposed = Rect::new(e.x as i32, e.y as i32, e.width as i32, e.height as i32);
                    self.blit(surface, &[exposed]);
                }
            }
            XEvent::ButtonPress(e) | XEvent::ButtonRelease(e) => {
                self.last_time = e.time;
                let pressed = e.response_type & 0x7f == xproto::BUTTON_PRESS_EVENT;
                if self.drag.is_some() && !pressed && e.detail == 1 {
                    let copy = e.state.contains(xproto::KeyButMask::CONTROL);
                    self.drag_release(e.root_x as i32, e.root_y as i32, copy);
                    return;
                }
                let surface = self.surface_of(e.event);
                let (x, y) = (
                    e.event_x as f64 / self.scale as f64,
                    e.event_y as f64 / self.scale as f64,
                );
                match e.detail {
                    4 if pressed => self.events.push(Event::PointerAxis {
                        surface,
                        x,
                        y,
                        delta: -WHEEL_STEP,
                    }),
                    5 if pressed => self.events.push(Event::PointerAxis {
                        surface,
                        x,
                        y,
                        delta: WHEEL_STEP,
                    }),
                    1..=3 => {
                        let button = match e.detail {
                            1 => Button::Left,
                            2 => Button::Middle,
                            _ => Button::Right,
                        };
                        self.events.push(Event::PointerButton {
                            surface,
                            x,
                            y,
                            button,
                            pressed,
                            time: e.time,
                            modifiers: modifiers_of(e.state),
                        });
                    }
                    _ => {}
                }
            }
            XEvent::MotionNotify(e) => {
                self.last_time = e.time;
                if self.drag.is_some() {
                    self.drag_motion(e.root_x as i32, e.root_y as i32);
                    return;
                }
                let surface = self.surface_of(e.event);
                let (x, y) = (
                    e.event_x as f64 / self.scale as f64,
                    e.event_y as f64 / self.scale as f64,
                );
                self.events.push(Event::PointerMotion { surface, x, y });
            }
            XEvent::LeaveNotify(e) => {
                if e.mode == xproto::NotifyMode::NORMAL {
                    self.events.push(Event::PointerLeave {
                        surface: self.surface_of(e.event),
                    });
                }
            }
            XEvent::KeyPress(e) => {
                self.last_time = e.time;
                let modifiers = modifiers_of(e.state);
                let caps = e.state.contains(xproto::KeyButMask::LOCK);
                if let Some(key) = self.keymap.translate(e.detail as u32, modifiers.shift, caps) {
                    self.events.push(Event::Key {
                        surface: self.surface_of(e.event),
                        key,
                        modifiers,
                    });
                }
            }
            XEvent::ClientMessage(e) => {
                if e.type_ == self.atoms.WM_PROTOCOLS
                    && e.data.as_data32()[0] == self.atoms.WM_DELETE_WINDOW
                    && let SurfaceId::Window(window) = self.surface_of(e.window)
                {
                    self.events.push(Event::WindowCloseRequested { window });
                }
            }
            XEvent::MappingNotify(_) => self.load_keymap(),
            XEvent::ReparentNotify(_) => {
                let panel_input = self
                    .panel
                    .visible
                    .iter()
                    .map(|r| scale_down(*r, self.scale))
                    .collect::<Vec<_>>();
                let input = if self.popup_open {
                    vec![Rect::new(
                        0,
                        0,
                        self.panel.width / self.scale,
                        self.panel.height / self.scale,
                    )]
                } else {
                    panel_input
                };
                self.apply_shape(&self.panel, &input, self.scale, true);
                let cells: Vec<Rect> = self
                    .desktop
                    .visible
                    .iter()
                    .skip(1)
                    .map(|r| scale_down(*r, self.scale))
                    .collect();
                self.apply_shape(&self.desktop, &cells, self.scale, false);
            }
            XEvent::ConfigureNotify(e) => {
                if e.window == self.root
                    && (e.width as i32 != self.screen_w || e.height as i32 != self.screen_h)
                {
                    self.screen_w = e.width as i32;
                    self.screen_h = e.height as i32;
                    self.monitors = self.read_monitors();
                    self.apply_geometry();
                } else if let SurfaceId::Window(id) = self.surface_of(e.window)
                    && let Some(window) = self.windows.iter_mut().find(|w| w.id == id)
                    && (window.surface.width != e.width as i32 || window.surface.height != e.height as i32)
                {
                    window.surface.width = e.width as i32;
                    window.surface.height = e.height as i32;
                    window.surface.shm = None;
                    window.surface.pixels.clear();
                    let s = self.scale;
                    self.events.push(Event::Configured {
                        surface: SurfaceId::Window(id),
                        width: (e.width as i32 + s - 1) / s,
                        height: (e.height as i32 + s - 1) / s,
                        scale: s,
                    });
                }
            }
            XEvent::RandrScreenChangeNotify(_) | XEvent::RandrNotify(_) => self.sync_monitors(),
            XEvent::PropertyNotify(e) => self.handle_property(e),
            XEvent::Error(_) => {}
            _ => {}
        }
    }
}

/// The modifier keys held in an event's state mask.
fn modifiers_of(state: xproto::KeyButMask) -> Modifiers {
    Modifiers {
        shift: state.contains(xproto::KeyButMask::SHIFT),
        ctrl: state.contains(xproto::KeyButMask::CONTROL),
        alt: state.contains(xproto::KeyButMask::MOD1),
    }
}

/// Builds a rectangle for the shape extension.
fn push_rect(out: &mut Vec<xproto::Rectangle>, rect: Rect) {
    if rect.is_empty() {
        return;
    }
    out.push(xproto::Rectangle {
        x: rect.x.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        y: rect.y.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        width: rect.w.min(u16::MAX as i32) as u16,
        height: rect.h.min(u16::MAX as i32) as u16,
    });
}

/// Divides a physical rectangle back into logical pixels.
fn scale_down(rect: Rect, scale: i32) -> Rect {
    Rect::new(rect.x / scale, rect.y / scale, rect.w / scale, rect.h / scale)
}

/// Appends the rectangles approximating a rounded rectangle to a shape list.
/// Each corner row is inset to follow the circle the UI draws, since X11
/// shapes are one bit and cannot carry the antialiased edge.
fn add_rounded_shape(out: &mut Vec<xproto::Rectangle>, r: Rect, radius: i32) {
    if r.is_empty() {
        return;
    }
    let radius = radius.min(r.h / 2).min(r.w / 2).max(0);
    for i in 0..radius {
        let dy = radius as f64 - i as f64 - 0.5;
        let inset = (radius as f64 - ((radius * radius) as f64 - dy * dy).sqrt()).ceil() as i32;
        push_rect(out, Rect::new(r.x + inset, r.y + i, r.w - 2 * inset, 1));
        push_rect(
            out,
            Rect::new(r.x + inset, r.bottom() - 1 - i, r.w - 2 * inset, 1),
        );
    }
    push_rect(out, Rect::new(r.x, r.y + radius, r.w, r.h - 2 * radius));
}

/// Creates one of the desktop's windows and tags it with the EWMH type and
/// states that make the window manager treat it as part of the desktop shell.
/// The window never takes keyboard focus, so clicking the taskbar does not
/// change which window is active before the click is handled, and it has
/// no background, so the server paints nothing when it is resized and the
/// pixels the desktop blits are the only ones ever shown.
fn create_window(
    conn: &RustConnection,
    atoms: &Atoms,
    screen: &xproto::Screen,
    title: &str,
    kind: xproto::Atom,
    events: xproto::EventMask,
) -> Result<X11Surface, String> {
    let window = conn.generate_id().map_err(|e| format!("{e}"))?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        window,
        screen.root,
        0,
        0,
        1,
        1,
        0,
        xproto::WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &xproto::CreateWindowAux::new()
            .background_pixmap(x11rb::NONE)
            .event_mask(events),
    )
    .map_err(|e| format!("{e}"))?;
    let gc = conn.generate_id().map_err(|e| format!("{e}"))?;
    conn.create_gc(gc, window, &xproto::CreateGCAux::new().graphics_exposures(0))
        .map_err(|e| format!("{e}"))?;

    let _ = conn.change_property8(
        xproto::PropMode::REPLACE,
        window,
        xproto::AtomEnum::WM_NAME,
        xproto::AtomEnum::STRING,
        title.as_bytes(),
    );
    let _ = conn.change_property8(
        xproto::PropMode::REPLACE,
        window,
        atoms._NET_WM_NAME,
        atoms.UTF8_STRING,
        title.as_bytes(),
    );
    let _ = conn.change_property8(
        xproto::PropMode::REPLACE,
        window,
        xproto::AtomEnum::WM_CLASS,
        xproto::AtomEnum::STRING,
        b"selkies-desktop\0Selkies-desktop\0",
    );
    let hints = x11rb::properties::WmHints {
        input: Some(false),
        ..x11rb::properties::WmHints::new()
    };
    let _ = hints.set(conn, window);
    let _ = conn.change_property32(
        xproto::PropMode::REPLACE,
        window,
        atoms._NET_WM_WINDOW_TYPE,
        xproto::AtomEnum::ATOM,
        &[kind],
    );
    let _ = conn.change_property32(
        xproto::PropMode::REPLACE,
        window,
        atoms._NET_WM_STATE,
        xproto::AtomEnum::ATOM,
        &[
            atoms._NET_WM_STATE_STICKY,
            atoms._NET_WM_STATE_SKIP_TASKBAR,
            atoms._NET_WM_STATE_SKIP_PAGER,
        ],
    );
    let _ = conn.change_property32(
        xproto::PropMode::REPLACE,
        window,
        atoms._NET_WM_DESKTOP,
        xproto::AtomEnum::CARDINAL,
        &[0xFFFF_FFFF],
    );
    Ok(X11Surface {
        window,
        gc,
        width: 0,
        height: 0,
        shm: None,
        pixels: Vec::new(),
        visible: Vec::new(),
    })
}

/// Pins a window's position and size through its normal hints so the window
/// manager places it exactly where requested.
fn set_fixed_geometry(conn: &RustConnection, window: xproto::Window, x: i32, y: i32, w: i32, h: i32) {
    let mut hints = x11rb::properties::WmSizeHints::new();
    hints.position = Some((x11rb::properties::WmSizeHintsSpecification::UserSpecified, x, y));
    hints.size = Some((x11rb::properties::WmSizeHintsSpecification::UserSpecified, w, h));
    hints.min_size = Some((w, h));
    hints.max_size = Some((w, h));
    let _ = hints.set_normal_hints(conn, window);
    let _ = conn.configure_window(
        window,
        &xproto::ConfigureWindowAux::new()
            .x(x)
            .y(y)
            .width(w.max(1) as u32)
            .height(h.max(1) as u32),
    );
}

/// Loads the hand and drag cursors from the cursor theme at the size the
/// session's resources name.  The arrow is never loaded: the windows inherit
/// the root window's cursor for it, so it is always the one the window manager
/// shows.
fn load_cursors(
    conn: &RustConnection,
    screen: usize,
) -> (
    Option<xproto::Cursor>,
    Option<xproto::Cursor>,
    Option<xproto::Cursor>,
) {
    let Ok(db) = x11rb::resource_manager::new_from_default(conn) else {
        return (None, None, None);
    };
    let Some(handle) = x11rb::cursor::Handle::new(conn, screen, &db)
        .ok()
        .and_then(|c| c.reply().ok())
    else {
        return (None, None, None);
    };
    let hand = ["pointer", "hand2", "hand1"]
        .iter()
        .find_map(|n| handle.load_cursor(conn, n).ok().filter(|c| *c != x11rb::NONE));
    let drag = ["dnd-move", "grabbing", "fleur", "move"]
        .iter()
        .find_map(|n| handle.load_cursor(conn, n).ok().filter(|c| *c != x11rb::NONE));
    (None, hand, drag)
}

/// Allocates an anonymous shared memory file of a size.
fn create_memfd(size: usize) -> Option<OwnedFd> {
    let name = c"selkies-desktop-x11";
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if unsafe { libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) } != 0 {
        return None;
    }
    Some(fd)
}

impl Backend for X11Backend {
    fn name(&self) -> &'static str {
        "x11"
    }

    fn fd(&self) -> RawFd {
        self.conn.stream().as_fd().as_raw_fd()
    }

    fn prepare(&mut self) -> Result<(), String> {
        while let Some(event) = self.conn.poll_for_event().map_err(|e| format!("{e}"))? {
            self.handle_event(event);
        }
        self.conn.flush().map_err(|e| format!("{e}"))
    }

    fn pending(&self) -> bool {
        !self.events.is_empty()
    }

    fn panel_origin(&self) -> (i32, i32, i32) {
        let primary = self
            .monitors
            .first()
            .copied()
            .unwrap_or(Rect::new(0, 0, self.screen_w, self.screen_h));
        let y = if self.panel_top {
            primary.y
        } else {
            primary.bottom() - self.panel.height
        };
        (primary.x, y, self.scale)
    }

    fn dispatch(&mut self, _readable: bool) -> Result<Vec<Event>, String> {
        while let Some(event) = self.conn.poll_for_event().map_err(|e| format!("{e}"))? {
            self.handle_event(event);
        }
        Ok(std::mem::take(&mut self.events))
    }

    fn present(&mut self, id: SurfaceId, frame: &Frame<'_>) {
        let scale = frame.scale;
        let shm_works = self.shm_works;
        let is_window = matches!(id, SurfaceId::Window(_));
        let Some((w, h)) = self.surface_ref(id).map(|s| (s.width, s.height)) else {
            return;
        };
        if frame.pixmap.width() as i32 != w || frame.pixmap.height() as i32 != h || w <= 0 || h <= 0 {
            return;
        }
        let size = w as usize * h as usize * 4;
        let mut new_segment = None;
        if shm_works && self.surface_ref(id).is_some_and(|s| s.shm.is_none()) {
            if let Some(fd) = create_memfd(size)
                && let Ok(map) = unsafe { memmap2::MmapMut::map_mut(&fd) }
                && let Ok(seg) = self.conn.generate_id()
                && let Ok(server_fd) = fd.try_clone()
                && self.conn.shm_attach_fd(seg, server_fd, false).is_ok()
            {
                new_segment = Some(ShmSegment { seg, map, _fd: fd });
            } else {
                self.shm_works = false;
            }
        }
        let stride = w as usize * 4;
        let visible: Vec<Rect> = frame.visible.iter().map(|r| r.scaled(scale)).collect();
        let damage: Vec<Rect> = frame.damage.iter().map(|r| r.scaled(scale)).collect();
        {
            let Some(surface) = self.surface_mut(id) else {
                return;
            };
            if let Some(segment) = new_segment {
                surface.shm = Some(segment);
            }
            let src = frame.pixmap.data();
            match &mut surface.shm {
                Some(segment) => {
                    for r in &damage {
                        let r = r.intersect(&Rect::new(0, 0, w, h));
                        super::copy_bgra(
                            src,
                            &mut segment.map,
                            stride,
                            r.x as usize,
                            r.y as usize,
                            r.w as usize,
                            r.h as usize,
                        );
                    }
                }
                None => {
                    if surface.pixels.len() != size {
                        surface.pixels = vec![0; size];
                    }
                    for r in &damage {
                        let r = r.intersect(&Rect::new(0, 0, w, h));
                        super::copy_bgra(
                            src,
                            &mut surface.pixels,
                            stride,
                            r.x as usize,
                            r.y as usize,
                            r.w as usize,
                            r.h as usize,
                        );
                    }
                }
            }
            surface.visible = visible;
        }
        let Some(surface) = self.surface_ref(id) else {
            return;
        };
        if !is_window {
            let input: Vec<Rect> = if id == SurfaceId::Panel && frame.grab {
                vec![Rect::new(0, 0, w / scale, h / scale)]
            } else {
                frame.input.clone()
            };
            self.apply_shape(surface, &input, scale, id == SurfaceId::Panel);
        }
        if id == SurfaceId::Panel {
            self.popup_open = frame.grab;
            self.sync_grabs();
        }
        let Some(surface) = self.surface_ref(id) else {
            return;
        };
        let mut to_blit = damage.clone();
        to_blit.extend(surface.visible.iter().copied());
        self.blit(surface, &to_blit);
        let _ = self.conn.flush();
    }

    fn perform(&mut self, request: &Request) {
        match request {
            Request::Window { id, action } => {
                let Some(window) = self.toplevels.iter().find(|(_, v)| **v == *id).map(|(w, _)| *w) else {
                    return;
                };
                match action {
                    WindowAction::Activate => {
                        self.send_wm_message(
                            window,
                            self.atoms._NET_ACTIVE_WINDOW,
                            [2, self.last_time, 0, 0, 0],
                        );
                    }
                    WindowAction::Minimize => {
                        self.send_wm_message(window, self.atoms.WM_CHANGE_STATE, [3, 0, 0, 0, 0]);
                    }
                    WindowAction::Unminimize => {
                        let _ = self.conn.map_window(window);
                        self.send_wm_message(
                            window,
                            self.atoms._NET_ACTIVE_WINDOW,
                            [2, self.last_time, 0, 0, 0],
                        );
                    }
                    WindowAction::Maximize | WindowAction::Unmaximize => {
                        let add = if *action == WindowAction::Maximize { 1 } else { 0 };
                        self.send_wm_message(
                            window,
                            self.atoms._NET_WM_STATE,
                            [
                                add,
                                self.atoms._NET_WM_STATE_MAXIMIZED_VERT,
                                self.atoms._NET_WM_STATE_MAXIMIZED_HORZ,
                                2,
                                0,
                            ],
                        );
                    }
                    WindowAction::Close => {
                        self.send_wm_message(
                            window,
                            self.atoms._NET_CLOSE_WINDOW,
                            [self.last_time, 2, 0, 0, 0],
                        );
                    }
                }
                let _ = self.conn.flush();
            }
            Request::MenuOpen(open) => {
                self.popup_open = *open;
                self.sync_grabs();
                let _ = self.conn.flush();
            }
            Request::Cursor { pointer } => {
                self.pointer_shape = *pointer;
                self.apply_cursor();
                let _ = self.conn.flush();
            }
            Request::Panel {
                top,
                height,
                autohide,
                margin,
            } => {
                self.panel_margin = *margin;
                if self.panel_top != *top || self.panel_height != *height || self.panel_autohide != *autohide
                {
                    self.panel_top = *top;
                    self.panel_height = *height;
                    self.panel_autohide = *autohide;
                    self.apply_geometry();
                    let _ = self.conn.flush();
                }
            }
            Request::OpenWindow {
                id,
                title,
                width,
                height,
                icon,
            } => {
                self.create_toplevel(*id, title, *width, *height, icon.as_deref());
                let _ = self.conn.flush();
            }
            Request::CloseWindow { id } => {
                self.destroy_window(*id);
                let _ = self.conn.flush();
            }
            Request::SetTitle { id, title } => {
                self.set_title(*id, title);
                let _ = self.conn.flush();
            }
            Request::StartDrag { window, .. } => {
                self.start_drag(*window);
                let _ = self.conn.flush();
            }
            Request::SwitchDesktop(index) => {
                self.send_wm_message(self.root, self.atoms._NET_CURRENT_DESKTOP, [*index, 0, 0, 0, 0]);
                let _ = self.conn.flush();
            }
            Request::SetVolume(_) | Request::ToggleMute => {}
        }
    }
}

impl Drop for X11Backend {
    fn drop(&mut self) {
        if self.grabbed {
            let _ = self.conn.ungrab_pointer(x11rb::CURRENT_TIME);
            let _ = self.conn.ungrab_keyboard(x11rb::CURRENT_TIME);
        }
        let _ = self.screen;
        let _ = self.conn.flush();
    }
}
