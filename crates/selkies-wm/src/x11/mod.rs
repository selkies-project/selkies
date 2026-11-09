/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The X11 backend: the engine's decisions carried out on an X server, the Xvfb a session runs
//! on, from a thread of its own with its own connection.
//!
//! [`Manager`] claims `WM_S0`, redirects the root's substructure, and manages every client the
//! ICCCM way: a client is reparented into a frame window the engine's frame image is painted
//! into, placed and sized as the engine configures it, told its state through `WM_STATE` and
//! `_NET_WM_STATE`, and focused through `set_input_focus` and `WM_TAKE_FOCUS`. EWMH is served to
//! the level a desktop and the applications read: the supporting check window, the client lists,
//! the active window, the workarea from the docks' struts, the frame extents, the allowed
//! actions, `_NET_WM_MOVERESIZE`, `_NET_MOVERESIZE_WINDOW`, `_NET_CLOSE_WINDOW`,
//! `_NET_ACTIVE_WINDOW`, `_NET_SHOWING_DESKTOP`, and the window types and states. RandR's
//! monitors are the engine's outputs, followed live through its notifications, so no restart
//! re-reads a layout.
//!
//! Input reaches the engine as the X server reports it: the frames' own button and motion
//! events, the root's buttons where no client lies, the key chords grabbed on the root for the
//! bindings, and the buttons grabbed on each client for the bindings that act inside a client
//! (a click to focus, Alt and a drag to move), replayed to the client when the engine passes
//! them. While a menu or the Alt-Tab list is open the pointer and the keyboard are grabbed, so
//! the engine sees every event until it closes them.
//!
//! The server composites nothing, so an effect that scales a live window on Wayland is a frame
//! moving without its client here: a minimize shrinks the empty frame toward the taskbar and a
//! restore grows it back before the client is shown again; a map fades nothing.
//!
//! The engine works in logical coordinates on every backend, so the manager converts at the
//! server's edge with the session's integer scale: the one `Xft.dpi` in the root's resource
//! database names, the resource the server writes for a display's density and the toolkits
//! size themselves by. Geometry read from the server (monitors, struts, a client's requested
//! rectangle, its size hints, the pointer) is divided by it, geometry sent (frames, clients,
//! overlays, the EWMH extents and workarea) multiplied, and the engine draws its frames and
//! menus at that scale, so the decorations keep pace with the applications where openbox
//! leaves them a quarter their size. A scale change keeps every client's pixel size and
//! re-dresses it, since only the applications that read the resource rescale themselves.

pub mod frame;
pub mod props;

use std::collections::{BTreeMap, HashMap};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pixelflux::wm::{
    Command, Event, Extents, Frame, KeyEvent, Modifiers, OutputInfo, PointerEvent, PointerKind, Rect,
    Request, ResizeEdge, Size, SizeHints, States, Transform, Under, Verdict, WindowId, WindowPolicy,
};
use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    self, AtomEnum, ChangeWindowAttributesAux, ConfigureWindowAux, ConnectionExt, CreateWindowAux, EventMask,
    GrabMode, InputFocus, ModMask, SetMode, StackMode, Window, WindowClass,
};
use x11rb::protocol::{Event as XEvent, shape, shape::ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

use self::frame::{FrameWindow, OverlayWindow, Visual};
use self::props::{Atoms, Props, Strut};
use crate::config::Backend;
use crate::core::Engine;
use crate::session::{SessionEnv, ShellSupervisor};

/// Largest integer scale honored from `Xft.dpi`, the same cap the desktop shell applies.
const MAX_SCALE: i32 = 4;

/// The manager's name on the supporting check window, which clients read to decide how to
/// behave: Chromium and Electron draw their own frame only under a window manager they know by
/// this name and ask for a server frame under any other, so the manager answers to the name of
/// the one whose configuration and behavior it carries rather than an unknown one.
const NET_WM_NAME: &str = "Openbox";
/// The `_NET_WM_DESKTOP` value of a window on every desktop.
const ALL_DESKTOPS: u32 = 0xffff_ffff;

/// The session's integer scale from the `Xft.dpi` resource in the root's database, 1 without
/// one; 96 dots an inch is scale 1, with the rounding the shell uses so the two agree.
fn read_scale(conn: &RustConnection) -> i32 {
    let Ok(db) = x11rb::resource_manager::new_from_default(conn) else {
        return 1;
    };
    let dpi: Option<f64> = db.get_value("Xft.dpi", "Xft.Dpi").ok().flatten();
    dpi.map(|d| ((d + 48.0) / 96.0) as i32)
        .unwrap_or(1)
        .clamp(1, MAX_SCALE)
}

/// Linux event codes of the pointer buttons the engine reads.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

/// Keysyms of the modifier keys the modifier map is read by.
const XK_ALT_L: u32 = 0xffe9;
const XK_META_L: u32 = 0xffe7;
const XK_SUPER_L: u32 = 0xffeb;
const XK_HYPER_L: u32 = 0xffed;
const XK_NUM_LOCK: u32 = 0xff7f;
const XK_SCROLL_LOCK: u32 = 0xff14;

/// The keyboard as the core protocol describes it: the keysyms of each keycode and which
/// modifier bits the Alt, Super, and lock keys set.
struct Keymap {
    min_keycode: u8,
    per_keycode: usize,
    keysyms: Vec<u32>,
    alt: u16,
    logo: u16,
    num_lock: u16,
    scroll_lock: u16,
}

impl Keymap {
    fn read(conn: &RustConnection) -> Result<Keymap, String> {
        let setup = conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let mapping = conn
            .get_keyboard_mapping(min, max - min + 1)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        let modifiers = conn
            .get_modifier_mapping()
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        let mut keymap = Keymap {
            min_keycode: min,
            per_keycode: mapping.keysyms_per_keycode as usize,
            keysyms: mapping.keysyms,
            alt: 0,
            logo: 0,
            num_lock: 0,
            scroll_lock: 0,
        };
        let per = modifiers.keycodes_per_modifier() as usize;
        for (index, chunk) in modifiers.keycodes.chunks(per.max(1)).enumerate().take(8) {
            let mask = 1u16 << index;
            for &keycode in chunk {
                if keycode == 0 {
                    continue;
                }
                for sym in keymap.syms_of(keycode) {
                    match sym {
                        XK_ALT_L | XK_META_L => keymap.alt |= mask,
                        XK_SUPER_L | XK_HYPER_L => keymap.logo |= mask,
                        XK_NUM_LOCK => keymap.num_lock |= mask,
                        XK_SCROLL_LOCK => keymap.scroll_lock |= mask,
                        _ => {}
                    }
                }
            }
        }
        if keymap.alt == 0 {
            keymap.alt = ModMask::M1.into();
        }
        if keymap.logo == 0 {
            keymap.logo = ModMask::M4.into();
        }
        Ok(keymap)
    }

    /// The keysyms of a keycode, level 0 first.
    fn syms_of(&self, keycode: u8) -> Vec<u32> {
        if keycode < self.min_keycode || self.per_keycode == 0 {
            return Vec::new();
        }
        let start = (keycode - self.min_keycode) as usize * self.per_keycode;
        self.keysyms
            .get(start..start + self.per_keycode)
            .map(|s| s.iter().copied().filter(|k| *k != 0).collect())
            .unwrap_or_default()
    }

    /// The keysym a key produces: the shifted one with Shift held where the keycode has one.
    fn keysym(&self, keycode: u8, shift: bool) -> (u32, u32) {
        let syms = self.syms_of(keycode);
        let base = syms.first().copied().unwrap_or(0);
        let shifted = if shift {
            syms.get(1).copied().unwrap_or(base)
        } else {
            base
        };
        (shifted, base)
    }

    /// The keycodes producing a keysym at level 0 or 1, letters case aside.
    fn keycodes_for(&self, keysym: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let count = self.keysyms.len() / self.per_keycode.max(1);
        for i in 0..count {
            let keycode = self.min_keycode as usize + i;
            if keycode > 255 {
                break;
            }
            let syms = self.syms_of(keycode as u8);
            if syms
                .iter()
                .take(2)
                .any(|s| crate::config::lower_keysym(*s) == keysym)
            {
                out.push(keycode as u8);
            }
        }
        out
    }

    /// The engine's modifiers for an X state mask.
    fn modifiers(&self, state: u16) -> Modifiers {
        Modifiers {
            shift: state & u16::from(ModMask::SHIFT) != 0,
            ctrl: state & u16::from(ModMask::CONTROL) != 0,
            alt: state & self.alt != 0,
            logo: state & self.logo != 0,
            caps_lock: state & u16::from(ModMask::LOCK) != 0,
            num_lock: state & self.num_lock != 0,
        }
    }

    /// The X modifier mask of a bind's modifiers.
    fn mask(&self, modifiers: Modifiers) -> u16 {
        let mut mask = 0u16;
        if modifiers.shift {
            mask |= u16::from(ModMask::SHIFT);
        }
        if modifiers.ctrl {
            mask |= u16::from(ModMask::CONTROL);
        }
        if modifiers.alt {
            mask |= self.alt;
        }
        if modifiers.logo {
            mask |= self.logo;
        }
        mask
    }

    /// The lock masks a grab is repeated with, so a bind works with Num Lock or Caps Lock on.
    fn lock_variants(&self) -> Vec<u16> {
        let caps = u16::from(ModMask::LOCK);
        let mut variants = vec![0, caps];
        if self.num_lock != 0 {
            variants.push(self.num_lock);
            variants.push(self.num_lock | caps);
        }
        variants
    }
}

/// One managed client.
struct Client {
    id: WindowId,
    window: Window,
    frame: FrameWindow,
    /// The frame image last drawn, painted again after an effect.
    last_frame: Option<Frame>,
    extents: Extents,
    rect: Rect,
    states: States,
    mapped: bool,
    /// Unmaps of the client the manager caused, which are not withdrawals.
    ignore_unmaps: u32,
    protocols: Vec<xproto::Atom>,
    strut: Option<props::Reservation>,
    /// Whether the client is unmapped for an effect or a shade, its frame shown alone.
    client_hidden: bool,
    shaded: bool,
    net_states: Vec<xproto::Atom>,
}

/// The handle the server keeps: stopping it ends the manager's thread.
pub struct ManagerHandle {
    running: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ManagerHandle {
    /// Ask the manager to stop and wait for its thread.
    pub fn stop(mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ManagerHandle {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

/// The window manager on an X server.
pub struct Manager {
    conn: RustConnection,
    root: Window,
    screen: usize,
    atoms: Atoms,
    visual: Visual,
    engine: Engine,
    check_window: Window,
    clients: HashMap<Window, Client>,
    by_id: HashMap<WindowId, Window>,
    next_id: WindowId,
    overlays: BTreeMap<u32, OverlayWindow>,
    cursors: Option<x11rb::cursor::Handle>,
    cursor_cache: HashMap<&'static str, xproto::Cursor>,
    keymap: Keymap,
    grabbed_keys: Vec<(u8, u16)>,
    grabbed: bool,
    outputs: Vec<OutputInfo>,
    wake_at: Option<Instant>,
    last_time: u32,
    focus: Option<WindowId>,
    shell: Option<ShellSupervisor>,
    running: Arc<AtomicBool>,
    /// Frames shown plainly for an effect, with the client rectangle they return to.
    effects: HashMap<WindowId, Transform>,
    /// Button grabs on clients, so a client is not grabbed twice.
    client_grabs: Vec<(u8, u16)>,
    /// The session's integer scale: logical coordinates times this are the server's pixels.
    scale: i32,
    /// Whether the pointer is grabbed for a move or resize a client asked for through
    /// `_NET_WM_MOVERESIZE`, where no frame press holds it; released with the engine's grab.
    moveresize_grab: bool,
}

/// Start the manager on `display`, with the session environment programs are started in and
/// the shell binary to supervise where `SELKIES_DESKTOP` asks for one. Fails where the display
/// cannot be reached or another window manager runs there.
pub fn spawn(
    display: &str,
    env: Arc<Mutex<SessionEnv>>,
    shell: Option<String>,
) -> Result<ManagerHandle, String> {
    let running = Arc::new(AtomicBool::new(true));
    let mut manager = Manager::connect(display, env.clone(), running.clone())?;
    if let Some(program) = shell {
        manager.shell = Some(ShellSupervisor::new(env, &program));
    }
    let flag = running.clone();
    let thread = std::thread::Builder::new()
        .name("selkies-wm-x11".to_string())
        .spawn(move || {
            manager.run();
            flag.store(false, Ordering::Relaxed);
        })
        .map_err(|e| e.to_string())?;
    Ok(ManagerHandle {
        running,
        thread: Some(thread),
    })
}

/// Everything the manager publishes in `_NET_SUPPORTED`.
fn supported(a: &Atoms) -> Vec<xproto::Atom> {
    vec![
        a._NET_SUPPORTING_WM_CHECK,
        a._NET_WM_NAME,
        a._NET_CLIENT_LIST,
        a._NET_CLIENT_LIST_STACKING,
        a._NET_ACTIVE_WINDOW,
        a._NET_CLOSE_WINDOW,
        a._NET_WORKAREA,
        a._NET_DESKTOP_GEOMETRY,
        a._NET_DESKTOP_VIEWPORT,
        a._NET_NUMBER_OF_DESKTOPS,
        a._NET_CURRENT_DESKTOP,
        a._NET_DESKTOP_NAMES,
        a._NET_SHOWING_DESKTOP,
        a._NET_WM_DESKTOP,
        a._NET_WM_STATE,
        a._NET_WM_STATE_MODAL,
        a._NET_WM_STATE_MAXIMIZED_VERT,
        a._NET_WM_STATE_MAXIMIZED_HORZ,
        a._NET_WM_STATE_SHADED,
        a._NET_WM_STATE_SKIP_TASKBAR,
        a._NET_WM_STATE_SKIP_PAGER,
        a._NET_WM_STATE_HIDDEN,
        a._NET_WM_STATE_FULLSCREEN,
        a._NET_WM_STATE_ABOVE,
        a._NET_WM_STATE_BELOW,
        a._NET_WM_STATE_DEMANDS_ATTENTION,
        a._NET_WM_STATE_FOCUSED,
        a._NET_WM_WINDOW_TYPE,
        a._NET_WM_WINDOW_TYPE_DESKTOP,
        a._NET_WM_WINDOW_TYPE_DOCK,
        a._NET_WM_WINDOW_TYPE_TOOLBAR,
        a._NET_WM_WINDOW_TYPE_MENU,
        a._NET_WM_WINDOW_TYPE_UTILITY,
        a._NET_WM_WINDOW_TYPE_SPLASH,
        a._NET_WM_WINDOW_TYPE_DIALOG,
        a._NET_WM_WINDOW_TYPE_DROPDOWN_MENU,
        a._NET_WM_WINDOW_TYPE_POPUP_MENU,
        a._NET_WM_WINDOW_TYPE_TOOLTIP,
        a._NET_WM_WINDOW_TYPE_NOTIFICATION,
        a._NET_WM_WINDOW_TYPE_COMBO,
        a._NET_WM_WINDOW_TYPE_DND,
        a._NET_WM_WINDOW_TYPE_NORMAL,
        a._NET_WM_ALLOWED_ACTIONS,
        a._NET_WM_ACTION_MOVE,
        a._NET_WM_ACTION_RESIZE,
        a._NET_WM_ACTION_MINIMIZE,
        a._NET_WM_ACTION_SHADE,
        a._NET_WM_ACTION_MAXIMIZE_HORZ,
        a._NET_WM_ACTION_MAXIMIZE_VERT,
        a._NET_WM_ACTION_FULLSCREEN,
        a._NET_WM_ACTION_CLOSE,
        a._NET_WM_ACTION_ABOVE,
        a._NET_WM_ACTION_BELOW,
        a._NET_WM_STRUT,
        a._NET_WM_STRUT_PARTIAL,
        a._NET_WM_MOVERESIZE,
        a._NET_MOVERESIZE_WINDOW,
        a._NET_REQUEST_FRAME_EXTENTS,
        a._NET_FRAME_EXTENTS,
        a._NET_WM_PING,
        a._NET_WM_PID,
        a._NET_WM_ICON,
    ]
}

impl Manager {
    fn connect(
        display: &str,
        env: Arc<Mutex<SessionEnv>>,
        running: Arc<AtomicBool>,
    ) -> Result<Manager, String> {
        let (conn, screen) = RustConnection::connect(Some(display)).map_err(|e| format!("{display}: {e}"))?;
        let screen_info = conn.setup().roots.get(screen).ok_or("no screen")?.clone();
        let root = screen_info.root;
        let atoms = Atoms::new(&conn)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        if let Some(owner) = conn
            .get_selection_owner(atoms.WM_S0)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.owner)
            .filter(|o| *o != 0)
        {
            log::warn!(target: "wm", "{display}: another window manager owns WM_S0 (window {owner:#x}); taking the screen over");
        }
        let root_mask = EventMask::SUBSTRUCTURE_REDIRECT
            | EventMask::SUBSTRUCTURE_NOTIFY
            | EventMask::STRUCTURE_NOTIFY
            | EventMask::PROPERTY_CHANGE
            | EventMask::BUTTON_PRESS
            | EventMask::BUTTON_RELEASE
            | EventMask::POINTER_MOTION
            | EventMask::FOCUS_CHANGE;
        conn.change_window_attributes(root, &ChangeWindowAttributesAux::new().event_mask(root_mask))
            .map_err(|e| e.to_string())?
            .check()
            .map_err(|e| format!("{display}: the root's substructure is redirected already ({e}); another window manager runs"))?;
        let check_window = conn.generate_id().map_err(|e| e.to_string())?;
        conn.create_window(
            0,
            check_window,
            root,
            -1,
            -1,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().override_redirect(1),
        )
        .map_err(|e| e.to_string())?;
        conn.set_selection_owner(check_window, atoms.WM_S0, x11rb::CURRENT_TIME)
            .map_err(|e| e.to_string())?;
        conn.change_window_attributes(
            check_window,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .map_err(|e| e.to_string())?;
        let props = Props {
            conn: &conn,
            atoms: &atoms,
        };
        props.set32(
            root,
            atoms._NET_SUPPORTING_WM_CHECK,
            AtomEnum::WINDOW,
            &[check_window],
        );
        props.set32(
            check_window,
            atoms._NET_SUPPORTING_WM_CHECK,
            AtomEnum::WINDOW,
            &[check_window],
        );
        props.set_text(check_window, atoms._NET_WM_NAME, NET_WM_NAME);
        props.set32(root, atoms._NET_SUPPORTED, AtomEnum::ATOM, &supported(&atoms));
        props.set32(root, atoms._NET_NUMBER_OF_DESKTOPS, AtomEnum::CARDINAL, &[1]);
        props.set32(root, atoms._NET_CURRENT_DESKTOP, AtomEnum::CARDINAL, &[0]);
        props.set32(root, atoms._NET_DESKTOP_VIEWPORT, AtomEnum::CARDINAL, &[0, 0]);
        props.set32(root, atoms._NET_SHOWING_DESKTOP, AtomEnum::CARDINAL, &[0]);
        props.set32(root, atoms._NET_ACTIVE_WINDOW, AtomEnum::WINDOW, &[0]);
        props.set32(root, atoms._NET_CLIENT_LIST, AtomEnum::WINDOW, &[]);
        props.set32(root, atoms._NET_CLIENT_LIST_STACKING, AtomEnum::WINDOW, &[]);
        props.set_text(root, atoms._NET_DESKTOP_NAMES, "Desktop");
        // Raw key presses tell the manager of every key the session presses, which its grabs
        // alone do not: a bind on a bare modifier runs on the modifier's release only when no
        // other key was pressed meanwhile, and that key goes to the client, never to a grab.
        if conn
            .extension_information(xinput::X11_EXTENSION_NAME)
            .ok()
            .flatten()
            .is_some()
            && conn
                .xinput_xi_query_version(2, 0)
                .ok()
                .and_then(|c| c.reply().ok())
                .is_some()
        {
            let _ = conn.xinput_xi_select_events(
                root,
                &[xinput::EventMask {
                    deviceid: xinput::Device::ALL_MASTER.into(),
                    mask: vec![xinput::XIEventMask::RAW_KEY_PRESS],
                }],
            );
        }
        let _ = conn.randr_select_input(
            root,
            randr::NotifyMask::SCREEN_CHANGE
                | randr::NotifyMask::CRTC_CHANGE
                | randr::NotifyMask::OUTPUT_CHANGE,
        );
        let _ = conn.shape_query_version();
        let keymap = Keymap::read(&conn)?;
        let cursors = x11rb::resource_manager::new_from_default(&conn)
            .ok()
            .and_then(|db| {
                x11rb::cursor::Handle::new(&conn, screen, &db)
                    .ok()
                    .and_then(|c| c.reply().ok())
            });
        conn.flush().map_err(|e| e.to_string())?;
        let launcher = Box::new(crate::session::ShellLauncher::new(env));
        let engine = Engine::new(Backend::X11, launcher);
        let visual = Visual {
            depth: screen_info.root_depth,
            visual: screen_info.root_visual,
            root,
        };
        let scale = read_scale(&conn);
        log::info!(target: "wm", "managing the windows on {display}: {}x{} at scale {scale}", screen_info.width_in_pixels, screen_info.height_in_pixels);
        Ok(Manager {
            conn,
            root,
            screen,
            atoms,
            visual,
            engine,
            check_window,
            clients: HashMap::new(),
            by_id: HashMap::new(),
            next_id: 1,
            overlays: BTreeMap::new(),
            cursors,
            cursor_cache: HashMap::new(),
            keymap,
            grabbed_keys: Vec::new(),
            grabbed: false,
            outputs: Vec::new(),
            wake_at: None,
            last_time: x11rb::CURRENT_TIME,
            focus: None,
            shell: None,
            running,
            effects: HashMap::new(),
            client_grabs: Vec::new(),
            scale,
            moveresize_grab: false,
        })
    }

    fn props(&self) -> Props<'_> {
        Props {
            conn: &self.conn,
            atoms: &self.atoms,
        }
    }

    // ---- scale ---------------------------------------------------------------------------------

    /// A pixel position in logical coordinates, rounded toward the origin.
    fn logical_pos(&self, v: i32) -> i32 {
        v.div_euclid(self.scale)
    }

    /// A pixel length in logical units, rounded up so a client is never given less than it
    /// asked for.
    fn logical_len(&self, v: i32) -> i32 {
        (v + self.scale - 1).div_euclid(self.scale)
    }

    /// A pixel rectangle in logical coordinates.
    fn to_logical(&self, r: Rect) -> Rect {
        Rect::new(
            self.logical_pos(r.x),
            self.logical_pos(r.y),
            self.logical_len(r.w),
            self.logical_len(r.h),
        )
    }

    /// A logical rectangle in pixels.
    fn to_pixels(&self, r: Rect) -> Rect {
        Rect::new(
            r.x * self.scale,
            r.y * self.scale,
            r.w * self.scale,
            r.h * self.scale,
        )
    }

    /// Logical extents in pixels.
    fn px_extents(&self, e: Extents) -> Extents {
        Extents {
            top: e.top * self.scale,
            left: e.left * self.scale,
            right: e.right * self.scale,
            bottom: e.bottom * self.scale,
        }
    }

    /// A client's size hints, read in pixels, in logical units; an increment rounds to the
    /// nearest step and never below one.
    fn logical_hints(&self, hints: SizeHints) -> SizeHints {
        let scale = self.scale;
        let len = |s: Size| Size {
            w: self.logical_len(s.w),
            h: self.logical_len(s.h),
        };
        let step = |s: Size| Size {
            w: ((s.w + scale / 2) / scale).max(1),
            h: ((s.h + scale / 2) / scale).max(1),
        };
        SizeHints {
            min: hints.min.map(len),
            max: hints.max.map(len),
            increment: hints.increment.map(step),
            base: hints.base.map(len),
        }
    }

    /// Everything the engine is told about a client, its hints in logical units.
    fn client_info(&self, window: Window, id: WindowId, rect: Rect) -> pixelflux::wm::WindowInfo {
        let output = self.output_at(rect.x, rect.y);
        let props = self.props();
        let parent = props.transient_for(window).and_then(|p| self.id_of(p));
        let mut info = props.window_info(window, id, rect, parent, output);
        info.hints = self.logical_hints(info.hints);
        info
    }

    /// Read the scale again after the resource database changed; when it moved, every client
    /// keeps its pixel rectangle under a frame drawn at the new scale, the cursors are loaded
    /// at the new size, and the outputs are published again in the new units.
    fn update_scale(&mut self) {
        let scale = read_scale(&self.conn);
        if scale == self.scale {
            return;
        }
        let pixel_rects: Vec<(Window, Rect)> = self
            .clients
            .iter()
            .map(|(w, c)| (*w, self.to_pixels(c.rect)))
            .collect();
        log::info!(target: "wm", "scale {scale} from Xft.dpi; the frames follow");
        self.scale = scale;
        self.cursor_cache.clear();
        self.cursors = x11rb::resource_manager::new_from_default(&self.conn)
            .ok()
            .and_then(|db| {
                x11rb::cursor::Handle::new(&self.conn, self.screen, &db)
                    .ok()
                    .and_then(|c| c.reply().ok())
            });
        self.outputs.clear();
        self.publish_outputs();
        for (window, pixels) in pixel_rects {
            let Some(id) = self.id_of(window) else {
                continue;
            };
            let rect = self.to_logical(pixels);
            let info = self.client_info(window, id, rect);
            self.dispatch(Event::Changed(info));
            self.dispatch(Event::Request {
                id,
                request: Request::Geometry(rect),
            });
        }
    }

    /// The thread body: adopt what is shown, then events, ticks, and the shell until stopped.
    fn run(&mut self) {
        // The root has no cursor of its own, so without this a session lands on the X cursor.
        self.apply(Command::Cursor("default"));
        self.publish_outputs();
        self.grab_keys();
        self.adopt_existing();
        let fd = self.conn.stream().as_raw_fd();
        let mut next_shell = Instant::now();
        while self.running.load(Ordering::Relaxed) {
            let now = Instant::now();
            if let Some(shell) = self.shell.as_mut()
                && now >= next_shell
            {
                next_shell = shell.poll(now);
            }
            if self.wake_at.is_some_and(|at| at <= now) {
                self.wake_at = None;
                self.dispatch(Event::Tick(now));
            }
            let _ = self.conn.flush();
            let mut handled = false;
            loop {
                match self.conn.poll_for_event() {
                    Ok(Some(event)) => {
                        handled = true;
                        self.handle(event);
                    }
                    Ok(None) => break,
                    Err(e) => {
                        log::error!(target: "wm", "the X connection ended: {e}");
                        self.running.store(false, Ordering::Relaxed);
                        return;
                    }
                }
            }
            if handled {
                continue;
            }
            let _ = self.conn.flush();
            let mut deadline = next_shell.min(now + Duration::from_secs(1));
            if let Some(at) = self.wake_at {
                deadline = deadline.min(at);
            }
            let timeout = deadline.saturating_duration_since(Instant::now());
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let ms = timeout.as_millis().clamp(1, 1000) as i32;
            // SAFETY: pollfd is a valid, initialized array of one entry for the call's duration.
            unsafe {
                libc::poll(&mut pfd, 1, ms);
            }
        }
        self.shutdown();
    }

    /// Hand the clients back to the root as they stand, so a manager that follows adopts them.
    fn shutdown(&mut self) {
        let windows: Vec<Window> = self.clients.keys().copied().collect();
        for window in windows {
            self.unmanage(window, true);
        }
        let _ = self.conn.destroy_window(self.check_window);
        let _ = self.conn.flush();
    }

    /// Run one engine event and carry its commands out.
    fn dispatch(&mut self, event: Event) -> Verdict {
        let mut out = Vec::new();
        let verdict = self.engine.handle(event, &mut out);
        for command in out {
            self.apply(command);
        }
        verdict
    }

    // ---- outputs -------------------------------------------------------------------------------

    /// The outputs from RandR's monitors in logical units, the workarea each shrunk by the
    /// docks' struts.
    fn read_outputs(&self) -> Vec<OutputInfo> {
        let mut rects: Vec<(Rect, bool)> = self
            .conn
            .randr_get_monitors(self.root, true)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| {
                r.monitors
                    .iter()
                    .map(|m| {
                        (
                            self.to_logical(Rect::new(
                                m.x as i32,
                                m.y as i32,
                                m.width as i32,
                                m.height as i32,
                            )),
                            m.primary,
                        )
                    })
                    .filter(|(r, _)| !r.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        if rects.is_empty() {
            let screen = &self.conn.setup().roots[self.screen];
            rects.push((
                self.to_logical(Rect::new(
                    0,
                    0,
                    screen.width_in_pixels as i32,
                    screen.height_in_pixels as i32,
                )),
                true,
            ));
        }
        if !rects.iter().any(|(_, p)| *p) {
            rects[0].1 = true;
        }
        let screen_w = rects.iter().map(|(r, _)| r.right()).max().unwrap_or(0);
        let screen_h = rects.iter().map(|(r, _)| r.bottom()).max().unwrap_or(0);
        let struts: Vec<props::Reservation> = self
            .clients
            .values()
            .filter_map(|c| c.strut)
            .map(|r| props::Reservation {
                strut: Strut {
                    left: self.logical_len(r.strut.left as i32) as u32,
                    right: self.logical_len(r.strut.right as i32) as u32,
                    top: self.logical_len(r.strut.top as i32) as u32,
                    bottom: self.logical_len(r.strut.bottom as i32) as u32,
                },
                spans: r.spans.map(|spans| {
                    spans.map(|(start, end)| (self.logical_pos(start), self.logical_pos(end + 1) - 1))
                }),
            })
            .collect();
        rects
            .into_iter()
            .enumerate()
            .map(|(i, (rect, primary))| {
                let mut area = rect;
                for reservation in &struts {
                    let strut = reservation.strut;
                    let covers = |edge: usize, from: i32, to: i32| {
                        reservation
                            .spans
                            .is_none_or(|spans| spans[edge].0 < to && spans[edge].1 >= from)
                    };
                    if strut.top > 0 && rect.y < strut.top as i32 && covers(2, rect.x, rect.right()) {
                        let cut = strut.top as i32 - area.y;
                        if cut > 0 && cut < area.h {
                            area.y += cut;
                            area.h -= cut;
                        }
                    }
                    if strut.bottom > 0
                        && rect.bottom() > screen_h - strut.bottom as i32
                        && covers(3, rect.x, rect.right())
                    {
                        let cut = area.bottom() - (screen_h - strut.bottom as i32);
                        if cut > 0 && cut < area.h {
                            area.h -= cut;
                        }
                    }
                    if strut.left > 0 && rect.x < strut.left as i32 && covers(0, rect.y, rect.bottom()) {
                        let cut = strut.left as i32 - area.x;
                        if cut > 0 && cut < area.w {
                            area.x += cut;
                            area.w -= cut;
                        }
                    }
                    if strut.right > 0
                        && rect.right() > screen_w - strut.right as i32
                        && covers(1, rect.y, rect.bottom())
                    {
                        let cut = area.right() - (screen_w - strut.right as i32);
                        if cut > 0 && cut < area.w {
                            area.w -= cut;
                        }
                    }
                }
                OutputInfo {
                    id: i as u32,
                    rect,
                    workarea: area,
                    scale: self.scale as f64,
                    primary,
                }
            })
            .collect()
    }

    /// Read the outputs and tell the engine and the root when they changed.
    fn publish_outputs(&mut self) {
        let outputs = self.read_outputs();
        if outputs == self.outputs {
            return;
        }
        self.outputs = outputs.clone();
        let primary = outputs.iter().find(|o| o.primary).or(outputs.first()).copied();
        let props = self.props();
        if let Some(p) = primary {
            let w = self.to_pixels(p.workarea);
            props.set32(
                self.root,
                self.atoms._NET_WORKAREA,
                AtomEnum::CARDINAL,
                &[
                    w.x.max(0) as u32,
                    w.y.max(0) as u32,
                    w.w.max(0) as u32,
                    w.h.max(0) as u32,
                ],
            );
        }
        let screen_w = outputs.iter().map(|o| o.rect.right()).max().unwrap_or(0) * self.scale;
        let screen_h = outputs.iter().map(|o| o.rect.bottom()).max().unwrap_or(0) * self.scale;
        props.set32(
            self.root,
            self.atoms._NET_DESKTOP_GEOMETRY,
            AtomEnum::CARDINAL,
            &[screen_w as u32, screen_h as u32],
        );
        self.dispatch(Event::Outputs(outputs));
    }

    // ---- keys and grabs -------------------------------------------------------------------------

    fn grab_keys(&mut self) {
        for (keycode, mask) in self.grabbed_keys.drain(..) {
            let _ = self.conn.ungrab_key(keycode, self.root, ModMask::from(mask));
        }
        let binds: Vec<(u32, Modifiers)> = self
            .engine
            .keybinds()
            .iter()
            .map(|b| (b.keysym, b.modifiers))
            .collect();
        for (keysym, modifiers) in binds {
            let base = self.keymap.mask(modifiers);
            for keycode in self.keymap.keycodes_for(keysym) {
                for lock in self.keymap.lock_variants() {
                    let mask = base | lock;
                    if self.grabbed_keys.contains(&(keycode, mask)) {
                        continue;
                    }
                    let _ = self.conn.grab_key(
                        false,
                        self.root,
                        ModMask::from(mask),
                        keycode,
                        GrabMode::ASYNC,
                        GrabMode::ASYNC,
                    );
                    self.grabbed_keys.push((keycode, mask));
                }
            }
        }
        self.client_grabs = {
            let mut grabs = vec![(1u8, 0u16), (2, 0), (3, 0)];
            for bind in self.engine.mousebinds() {
                if bind.context == crate::config::Context::Client
                    && bind.modifiers != Modifiers::default()
                    && bind.button <= 5
                {
                    let mask = self.keymap.mask(bind.modifiers);
                    if !grabs.contains(&(bind.button as u8, mask)) {
                        grabs.push((bind.button as u8, mask));
                    }
                }
            }
            grabs
        };
        let windows: Vec<Window> = self.clients.keys().copied().collect();
        for window in windows {
            self.grab_client_buttons(window);
        }
    }

    /// Grab on a client the buttons the engine wants to see first: every plain button, replayed
    /// where the engine passes it, and the chords of the bindings that act inside a client.
    fn grab_client_buttons(&self, window: Window) {
        let _ = self
            .conn
            .ungrab_button(xproto::ButtonIndex::ANY, window, ModMask::ANY);
        for (button, mask) in &self.client_grabs {
            for lock in if *mask == 0 {
                vec![0]
            } else {
                self.keymap.lock_variants()
            } {
                let index = match button {
                    1 => xproto::ButtonIndex::M1,
                    2 => xproto::ButtonIndex::M2,
                    3 => xproto::ButtonIndex::M3,
                    4 => xproto::ButtonIndex::M4,
                    _ => xproto::ButtonIndex::M5,
                };
                let _ = self.conn.grab_button(
                    false,
                    window,
                    EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
                    GrabMode::SYNC,
                    GrabMode::ASYNC,
                    0u32,
                    0u32,
                    index,
                    ModMask::from(mask | lock),
                );
            }
        }
    }

    /// Take the pointer and the keyboard while an overlay is up, so the engine sees every event.
    fn set_grabbed(&mut self, on: bool) {
        if on == self.grabbed {
            return;
        }
        self.grabbed = on;
        if on {
            let _ = self.conn.grab_pointer(
                true,
                self.root,
                EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                0u32,
                0u32,
                self.last_time,
            );
            let _ =
                self.conn
                    .grab_keyboard(true, self.root, self.last_time, GrabMode::ASYNC, GrabMode::ASYNC);
        } else {
            let _ = self.conn.ungrab_pointer(self.last_time);
            let _ = self.conn.ungrab_keyboard(self.last_time);
        }
    }

    // ---- clients -------------------------------------------------------------------------------

    /// Manage what is already on the screen: every viewable top-level window, and every
    /// unmapped one a previous manager left in `WM_STATE` normal or iconic, which is a client it
    /// had minimized or hidden for an effect and that no one else will show again.
    fn adopt_existing(&mut self) {
        let Ok(tree) = self.conn.query_tree(self.root).map(|c| c.reply()) else {
            return;
        };
        let Ok(tree) = tree else {
            return;
        };
        for window in tree.children {
            let Ok(attributes) = self.conn.get_window_attributes(window).map(|c| c.reply()) else {
                continue;
            };
            let Ok(attributes) = attributes else {
                continue;
            };
            if attributes.override_redirect || attributes.class == WindowClass::INPUT_ONLY {
                continue;
            }
            if attributes.map_state != xproto::MapState::VIEWABLE {
                let state = self
                    .props()
                    .cardinals(window, self.atoms.WM_STATE, self.atoms.WM_STATE)
                    .first()
                    .copied();
                if !matches!(state, Some(props::WM_STATE_NORMAL) | Some(props::WM_STATE_ICONIC)) {
                    continue;
                }
                log::info!(target: "wm", "window {window:#x} left behind by a previous manager; adopted");
            }
            self.manage(window);
        }
        let _ = self.conn.flush();
    }

    fn id_of(&self, window: Window) -> Option<WindowId> {
        self.clients.get(&window).map(|c| c.id)
    }

    /// The client whose frame or client window is `window`.
    fn client_of_window(&self, window: Window) -> Option<Window> {
        if self.clients.contains_key(&window) {
            return Some(window);
        }
        self.clients
            .iter()
            .find(|(_, c)| c.frame.window == window)
            .map(|(w, _)| *w)
    }

    /// The output a point lies on.
    fn output_at(&self, x: i32, y: i32) -> u32 {
        self.outputs
            .iter()
            .find(|o| o.rect.contains(x, y))
            .or_else(|| self.outputs.iter().find(|o| o.primary))
            .map(|o| o.id)
            .unwrap_or(0)
    }

    /// Bring a client under management: a frame around it, the engine told, the client mapped.
    fn manage(&mut self, window: Window) {
        if self.clients.contains_key(&window) {
            return;
        }
        let Ok(geometry) = self.conn.get_geometry(window).map(|c| c.reply()) else {
            return;
        };
        let Ok(geometry) = geometry else {
            return;
        };
        let Ok(attributes) = self.conn.get_window_attributes(window).map(|c| c.reply()) else {
            return;
        };
        let Ok(attributes) = attributes else {
            return;
        };
        if attributes.override_redirect || attributes.class == WindowClass::INPUT_ONLY {
            return;
        }
        // Reparenting a viewable window (an adopted one) unmaps and maps it again, an unmap
        // that is not a withdrawal.
        let viewable = attributes.map_state == xproto::MapState::VIEWABLE;
        let id = self.next_id;
        self.next_id += 1;
        let rect = self.to_logical(Rect::new(
            geometry.x as i32,
            geometry.y as i32,
            geometry.width as i32,
            geometry.height as i32,
        ));
        let props = self.props();
        let parent = props.transient_for(window).and_then(|p| self.id_of(p));
        let pointer = self
            .conn
            .query_pointer(self.root)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| {
                (
                    self.logical_pos(r.root_x as i32),
                    self.logical_pos(r.root_y as i32),
                )
            })
            .unwrap_or((0, 0));
        let output = self.output_at(pointer.0, pointer.1);
        let mut info = props.window_info(window, id, rect, parent, output);
        info.hints = self.logical_hints(info.hints);
        let protocols = props.protocols(window);
        let strut = props.strut(window);
        let background = self.conn.setup().roots[self.screen].black_pixel;
        let Ok(frame) = FrameWindow::create(
            &self.conn,
            &self.visual,
            self.to_pixels(rect),
            Extents::default(),
            background,
        ) else {
            return;
        };
        let _ = self.conn.change_save_set(SetMode::INSERT, window);
        let _ = self.conn.change_window_attributes(
            window,
            &ChangeWindowAttributesAux::new().event_mask(
                EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY | EventMask::FOCUS_CHANGE,
            ),
        );
        let _ = self.conn.shape_select_input(window, true);
        let _ = self
            .conn
            .configure_window(window, &ConfigureWindowAux::new().border_width(0));
        let _ = self.conn.reparent_window(window, frame.window, 0, 0);
        let client = Client {
            id,
            window,
            frame,
            last_frame: None,
            extents: Extents::default(),
            rect,
            states: States::default(),
            mapped: false,
            ignore_unmaps: u32::from(viewable),
            protocols,
            strut,
            client_hidden: false,
            shaded: false,
            net_states: Vec::new(),
        };
        self.clients.insert(window, client);
        self.by_id.insert(id, window);
        self.sync_client_shape(window);
        self.grab_client_buttons(window);
        if strut.is_some() {
            self.publish_outputs();
        }
        log::debug!(target: "wm", "window {id} ({}) managed: {}x{} at {},{}", info.app_id, rect.w, rect.h, rect.x, rect.y);
        self.dispatch(Event::Added(info));
        let rect = self.clients.get(&window).map(|c| c.rect).unwrap_or(rect);
        let minimized = self.clients.get(&window).is_some_and(|c| c.states.minimized);
        if minimized {
            self.set_wm_state(window, props::WM_STATE_ICONIC);
        } else {
            self.set_wm_state(window, props::WM_STATE_NORMAL);
            let _ = self.conn.map_window(window);
            if let Some(client) = self.clients.get(&window) {
                let _ = self.conn.map_window(client.frame.window);
            }
        }
        if let Some(client) = self.clients.get_mut(&window) {
            client.mapped = true;
        }
        self.dispatch(Event::Mapped {
            id,
            size: Size { w: rect.w, h: rect.h },
        });
        self.publish_client_list();
        let _ = self.conn.flush();
    }

    /// Read a client's bounding and input shapes and put them on its frame; a client shaped
    /// neither way gets the plain frame back.
    fn sync_client_shape(&mut self, window: Window) {
        if !self.clients.contains_key(&window) {
            return;
        }
        let Some(extents) = self
            .conn
            .shape_query_extents(window)
            .ok()
            .and_then(|c| c.reply().ok())
        else {
            return;
        };
        let rectangles = |kind: shape::SK| {
            self.conn
                .shape_get_rectangles(window, kind)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|r| r.rectangles)
        };
        let bounding = if extents.bounding_shaped {
            rectangles(shape::SK::BOUNDING)
        } else {
            None
        };
        // The server reports no flag for an input shape, so one is told from the rectangles:
        // an unshaped window's input is one rectangle covering it.
        let input = rectangles(shape::SK::INPUT).filter(|rects| {
            let client = self.clients[&window].frame.client;
            !(rects.len() == 1
                && rects[0].x == 0
                && rects[0].y == 0
                && rects[0].width as i32 >= client.w
                && rects[0].height as i32 >= client.h)
        });
        if let Some(client) = self.clients.get_mut(&window) {
            client.frame.set_client_shape(&self.conn, bounding, input);
        }
    }

    /// Stop managing a client: the frame destroyed, the client back on the root where its frame
    /// was, mapped where it is still shown, so a manager that follows adopts it.
    fn unmanage(&mut self, window: Window, keep_mapped: bool) {
        let Some(mut client) = self.clients.remove(&window) else {
            return;
        };
        let _ = self.conn.shape_select_input(window, false);
        self.by_id.remove(&client.id);
        self.effects.remove(&client.id);
        let at = client.frame.client;
        let _ = self
            .conn
            .ungrab_button(xproto::ButtonIndex::ANY, window, ModMask::ANY);
        let _ = self.conn.change_window_attributes(
            window,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT),
        );
        let _ = self
            .conn
            .reparent_window(window, self.root, at.x as i16, at.y as i16);
        let _ = self.conn.change_save_set(SetMode::DELETE, window);
        if keep_mapped && client.mapped && !client.states.minimized {
            let _ = self.conn.map_window(window);
        } else {
            let _ = self.conn.delete_property(window, self.atoms.WM_STATE);
        }
        client.frame.destroy(&self.conn);
        if client.strut.is_some() {
            self.publish_outputs();
        }
        self.publish_client_list();
    }

    fn set_wm_state(&self, window: Window, state: u32) {
        self.props()
            .set32(window, self.atoms.WM_STATE, self.atoms.WM_STATE, &[state, 0]);
    }

    /// Tell a client where it is on the root, as the ICCCM asks of a reparenting manager.
    fn send_configure_notify(&self, client: &Client) {
        let event = xproto::ConfigureNotifyEvent {
            response_type: xproto::CONFIGURE_NOTIFY_EVENT,
            sequence: 0,
            event: client.window,
            window: client.window,
            above_sibling: 0,
            x: client.frame.client.x as i16,
            y: client.frame.client.y as i16,
            width: client.frame.client.w.max(1) as u16,
            height: client.frame.client.h.max(1) as u16,
            border_width: 0,
            override_redirect: false,
        };
        let _ = self
            .conn
            .send_event(false, client.window, EventMask::STRUCTURE_NOTIFY, event);
    }

    fn publish_client_list(&self) {
        let mut list: Vec<Window> = self
            .clients
            .values()
            .filter(|c| c.mapped)
            .map(|c| c.window)
            .collect();
        list.sort_by_key(|w| self.clients[w].id);
        self.props()
            .set32(self.root, self.atoms._NET_CLIENT_LIST, AtomEnum::WINDOW, &list);
    }

    fn publish_states(&mut self, window: Window) {
        let a = self.atoms;
        let Some(client) = self.clients.get(&window) else {
            return;
        };
        let client_id = client.id;
        let mut states = Vec::new();
        if client.states.maximized {
            states.push(a._NET_WM_STATE_MAXIMIZED_HORZ);
            states.push(a._NET_WM_STATE_MAXIMIZED_VERT);
        }
        if client.states.fullscreen {
            states.push(a._NET_WM_STATE_FULLSCREEN);
        }
        if client.states.minimized {
            states.push(a._NET_WM_STATE_HIDDEN);
        }
        if client.states.activated {
            states.push(a._NET_WM_STATE_FOCUSED);
        }
        if client.shaded {
            states.push(a._NET_WM_STATE_SHADED);
        }
        for keep in [
            a._NET_WM_STATE_ABOVE,
            a._NET_WM_STATE_BELOW,
            a._NET_WM_STATE_SKIP_TASKBAR,
            a._NET_WM_STATE_SKIP_PAGER,
            a._NET_WM_STATE_MODAL,
            a._NET_WM_STATE_DEMANDS_ATTENTION,
        ] {
            if client.net_states.contains(&keep) {
                states.push(keep);
            }
        }
        let props = self.props();
        props.set32(window, a._NET_WM_STATE, AtomEnum::ATOM, &states);
        props.set32(
            window,
            a._NET_WM_ALLOWED_ACTIONS,
            AtomEnum::ATOM,
            &[
                a._NET_WM_ACTION_MOVE,
                a._NET_WM_ACTION_RESIZE,
                a._NET_WM_ACTION_MINIMIZE,
                a._NET_WM_ACTION_SHADE,
                a._NET_WM_ACTION_MAXIMIZE_HORZ,
                a._NET_WM_ACTION_MAXIMIZE_VERT,
                a._NET_WM_ACTION_FULLSCREEN,
                a._NET_WM_ACTION_CLOSE,
                a._NET_WM_ACTION_ABOVE,
                a._NET_WM_ACTION_BELOW,
            ],
        );
        let e = self.px_extents(client.extents);
        props.set32(
            window,
            a._NET_FRAME_EXTENTS,
            AtomEnum::CARDINAL,
            &[e.left as u32, e.right as u32, e.top as u32, e.bottom as u32],
        );
        let desktop = self.engine.desktop_of(client_id).unwrap_or(ALL_DESKTOPS);
        props.set32(window, a._NET_WM_DESKTOP, AtomEnum::CARDINAL, &[desktop]);
    }

    // ---- commands -------------------------------------------------------------------------------

    fn apply(&mut self, command: Command) {
        match command {
            Command::Configure { id, rect, states } => self.configure(id, rect, states),
            Command::Stack(order) => self.restack(&order),
            Command::Focus(id) => self.set_focus(id),
            Command::Close(id) => {
                let Some(window) = self.by_id.get(&id).copied() else {
                    return;
                };
                let delete = self
                    .clients
                    .get(&window)
                    .is_some_and(|c| c.protocols.contains(&self.atoms.WM_DELETE_WINDOW));
                if delete {
                    self.props()
                        .send_protocol(window, self.atoms.WM_DELETE_WINDOW, self.last_time);
                } else {
                    let _ = self.conn.kill_client(window);
                }
            }
            Command::Frame { id, frame } => {
                let Some(window) = self.by_id.get(&id).copied() else {
                    return;
                };
                let Some(client) = self.clients.get_mut(&window) else {
                    return;
                };
                match frame {
                    Some(frame) => {
                        let extents_changed = client.extents != frame.extents;
                        client.extents = frame.extents;
                        if extents_changed {
                            let rect = client.rect;
                            self.place(window, rect);
                        }
                        let client = self.clients.get_mut(&window).unwrap();
                        if !self.effects.contains_key(&id) {
                            client.frame.draw(&self.conn, &self.visual, &frame, self.scale);
                        }
                        client.last_frame = Some(frame);
                    }
                    None => {
                        client.last_frame = None;
                        if client.extents != Extents::default() {
                            client.extents = Extents::default();
                            let rect = client.rect;
                            self.place(window, rect);
                        }
                        let client = self.clients.get_mut(&window).unwrap();
                        client
                            .frame
                            .draw(&self.conn, &self.visual, &Frame::default(), self.scale);
                    }
                }
                self.publish_states(window);
            }
            Command::Overlay { key, rect, image } => {
                let rect = self.to_pixels(rect);
                match self.overlays.get_mut(&key) {
                    Some(overlay) => overlay.update(&self.conn, &self.visual, rect, &image),
                    None => {
                        if let Ok(overlay) = OverlayWindow::create(&self.conn, &self.visual, rect, &image) {
                            self.overlays.insert(key, overlay);
                        }
                    }
                }
                self.set_grabbed(true);
            }
            Command::ClearOverlay(key) => {
                if let Some(mut overlay) = self.overlays.remove(&key) {
                    overlay.destroy(&self.conn);
                }
                if self.overlays.is_empty() {
                    self.set_grabbed(false);
                }
            }
            Command::Cursor(name) => {
                let cursor = self.cursor(name);
                let _ = self
                    .conn
                    .change_window_attributes(self.root, &ChangeWindowAttributesAux::new().cursor(cursor));
            }
            Command::Effect { id, transform } => self.effect(id, transform),
            Command::WakeAt(at) => {
                self.wake_at = Some(self.wake_at.map_or(at, |w| w.min(at)));
            }
            Command::Desktops { count, current } => {
                let a = self.atoms;
                let props = self.props();
                props.set32(self.root, a._NET_NUMBER_OF_DESKTOPS, AtomEnum::CARDINAL, &[count]);
                props.set32(self.root, a._NET_CURRENT_DESKTOP, AtomEnum::CARDINAL, &[current]);
                props.set32(
                    self.root,
                    a._NET_DESKTOP_VIEWPORT,
                    AtomEnum::CARDINAL,
                    &vec![0; 2 * count as usize],
                );
            }
            Command::Redraw => {
                let windows: Vec<Window> = self.clients.keys().copied().collect();
                for window in windows {
                    if let Some(client) = self.clients.get_mut(&window)
                        && let Some(frame) = client.last_frame.clone()
                        && !self.effects.contains_key(&client.id)
                    {
                        client.frame.draw(&self.conn, &self.visual, &frame, self.scale);
                    }
                }
            }
        }
    }

    fn cursor(&mut self, name: &'static str) -> xproto::Cursor {
        if let Some(cursor) = self.cursor_cache.get(name) {
            return *cursor;
        }
        let x_name = match name {
            "default" => "left_ptr",
            "grabbing" => "fleur",
            "n-resize" => "top_side",
            "s-resize" => "bottom_side",
            "w-resize" => "left_side",
            "e-resize" => "right_side",
            "nw-resize" => "top_left_corner",
            "ne-resize" => "top_right_corner",
            "sw-resize" => "bottom_left_corner",
            "se-resize" => "bottom_right_corner",
            other => other,
        };
        let cursor = self
            .cursors
            .as_ref()
            .and_then(|h| h.load_cursor(&self.conn, x_name).ok())
            .or_else(|| {
                self.cursors
                    .as_ref()
                    .and_then(|h| h.load_cursor(&self.conn, name).ok())
            })
            .unwrap_or(0);
        self.cursor_cache.insert(name, cursor);
        cursor
    }

    /// Place a client's frame and the client in it for a client rectangle.
    fn place(&mut self, window: Window, rect: Rect) {
        let Some(client) = self.clients.get_mut(&window) else {
            return;
        };
        client.rect = rect;
        let scale = self.scale;
        let pixels = Rect::new(rect.x * scale, rect.y * scale, rect.w * scale, rect.h * scale);
        let extents = Extents {
            top: client.extents.top * scale,
            left: client.extents.left * scale,
            right: client.extents.right * scale,
            bottom: client.extents.bottom * scale,
        };
        let rect = pixels;
        let mut frame_extents = extents;
        if client.shaded {
            frame_extents.bottom = 0;
        }
        if client.shaded || client.client_hidden {
            let frame_rect = Rect::new(
                rect.x - extents.left,
                rect.y - extents.top,
                rect.w + extents.left + extents.right,
                extents.top + frame_extents.bottom,
            );
            client.frame.rect = frame_rect;
            client.frame.client = rect;
            client.frame.extents = extents;
            let _ = self.conn.configure_window(
                client.frame.window,
                &ConfigureWindowAux::new()
                    .x(frame_rect.x)
                    .y(frame_rect.y)
                    .width(frame_rect.w.max(1) as u32)
                    .height(frame_rect.h.max(1) as u32),
            );
            let _ = self.conn.configure_window(
                window,
                &ConfigureWindowAux::new()
                    .x(extents.left)
                    .y(extents.top)
                    .width(rect.w.max(1) as u32)
                    .height(rect.h.max(1) as u32),
            );
        } else {
            client.frame.place(&self.conn, window, rect, extents);
        }
        let client = &self.clients[&window];
        self.send_configure_notify(client);
    }

    fn configure(&mut self, id: WindowId, rect: Rect, states: States) {
        let Some(window) = self.by_id.get(&id).copied() else {
            return;
        };
        let Some(client) = self.clients.get_mut(&window) else {
            return;
        };
        let was_minimized = client.states.minimized || client.states.hidden;
        client.states = states;
        if rect.w > 0 && rect.h > 0 {
            self.place(window, rect);
        }
        let client = self.clients.get_mut(&window).unwrap();
        let minimized = states.minimized || states.hidden;
        if minimized && !was_minimized {
            if client.mapped && !client.client_hidden {
                client.ignore_unmaps += 1;
                let _ = self.conn.unmap_window(window);
            }
            let _ = self.conn.unmap_window(client.frame.window);
            self.set_wm_state(window, props::WM_STATE_ICONIC);
        } else if !minimized && was_minimized {
            if !client.client_hidden {
                let _ = self.conn.map_window(window);
            }
            let _ = self.conn.map_window(client.frame.window);
            self.set_wm_state(window, props::WM_STATE_NORMAL);
        }
        self.publish_states(window);
    }

    fn restack(&mut self, order: &[WindowId]) {
        let mut previous: Option<Window> = None;
        let mut stacking = Vec::new();
        for id in order.iter().rev() {
            let Some(window) = self.by_id.get(id).copied() else {
                continue;
            };
            let Some(client) = self.clients.get(&window) else {
                continue;
            };
            let frame = client.frame.window;
            let aux = match previous {
                None => ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
                Some(above) => ConfigureWindowAux::new()
                    .sibling(above)
                    .stack_mode(StackMode::BELOW),
            };
            let _ = self.conn.configure_window(frame, &aux);
            previous = Some(frame);
            stacking.push(window);
        }
        stacking.reverse();
        for overlay in self.overlays.values() {
            let _ = self.conn.configure_window(
                overlay.window,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            );
        }
        self.props().set32(
            self.root,
            self.atoms._NET_CLIENT_LIST_STACKING,
            AtomEnum::WINDOW,
            &stacking,
        );
    }

    fn set_focus(&mut self, id: Option<WindowId>) {
        let previous = self.focus;
        self.focus = id;
        match id.and_then(|id| self.by_id.get(&id).copied()) {
            Some(window) => {
                let take_focus = self
                    .clients
                    .get(&window)
                    .is_some_and(|c| c.protocols.contains(&self.atoms.WM_TAKE_FOCUS));
                let _ = self
                    .conn
                    .set_input_focus(InputFocus::PARENT, window, self.last_time);
                if take_focus {
                    self.props()
                        .send_protocol(window, self.atoms.WM_TAKE_FOCUS, self.last_time);
                }
                self.props().set32(
                    self.root,
                    self.atoms._NET_ACTIVE_WINDOW,
                    AtomEnum::WINDOW,
                    &[window],
                );
            }
            None => {
                let _ = self
                    .conn
                    .set_input_focus(InputFocus::POINTER_ROOT, self.root, self.last_time);
                self.props()
                    .set32(self.root, self.atoms._NET_ACTIVE_WINDOW, AtomEnum::WINDOW, &[0]);
            }
        }
        for changed in [previous, id].into_iter().flatten() {
            if let Some(window) = self.by_id.get(&changed).copied() {
                self.publish_states(window);
            }
        }
    }

    /// An effect on X11: the frame alone moves, the client hidden while it runs; a shade keeps
    /// the client hidden under its title bar.
    fn effect(&mut self, id: WindowId, transform: Option<Transform>) {
        let Some(window) = self.by_id.get(&id).copied() else {
            return;
        };
        let Some((shaded, hidden, activated, extents, rect, minimized)) =
            self.clients.get(&window).map(|c| {
                (
                    c.shaded,
                    c.client_hidden,
                    c.states.activated,
                    c.extents,
                    c.rect,
                    c.states.minimized,
                )
            })
        else {
            return;
        };
        match transform {
            Some(t) if t.kind == pixelflux::wm::EffectKind::Map => {}
            Some(t) if t.kind == pixelflux::wm::EffectKind::Shade => {
                if shaded {
                    return;
                }
                if let Some(client) = self.clients.get_mut(&window) {
                    client.shaded = true;
                    if !client.client_hidden {
                        client.client_hidden = true;
                        client.ignore_unmaps += 1;
                        let _ = self.conn.unmap_window(window);
                    }
                }
                self.place(window, rect);
                self.publish_states(window);
            }
            Some(t) => {
                let color = self.title_color(activated);
                let plain = self.to_pixels(t.rect.grown(extents));
                if let Some(client) = self.clients.get_mut(&window) {
                    if !hidden {
                        client.client_hidden = true;
                        client.ignore_unmaps += 1;
                        let _ = self.conn.unmap_window(window);
                    }
                    client.frame.show_plain(&self.conn, plain, color);
                }
                self.effects.insert(id, t);
            }
            None => {
                let was_effect = self.effects.remove(&id).is_some();
                if let Some(client) = self.clients.get_mut(&window) {
                    client.shaded = false;
                    if client.client_hidden && (was_effect || shaded) {
                        client.client_hidden = false;
                        if !minimized {
                            let _ = self.conn.map_window(window);
                        }
                    }
                }
                self.place(window, rect);
                if let Some(client) = self.clients.get_mut(&window)
                    && let Some(frame) = client.last_frame.clone()
                {
                    client.frame.draw(&self.conn, &self.visual, &frame, self.scale);
                }
                self.publish_states(window);
            }
        }
    }

    /// The title bar's color as an X pixel, for a frame shown plainly.
    fn title_color(&self, active: bool) -> u32 {
        let look = self.engine.look();
        let color = if active {
            look.palette.title_active
        } else {
            look.palette.title_inactive
        }
        .to_color_u8();
        ((color.red() as u32) << 16) | ((color.green() as u32) << 8) | color.blue() as u32
    }

    // ---- events ----------------------------------------------------------------------------------

    fn handle(&mut self, event: XEvent) {
        match event {
            XEvent::MapRequest(e) => {
                if self.clients.contains_key(&e.window) {
                    let id = self.clients[&e.window].id;
                    self.dispatch(Event::Request {
                        id,
                        request: Request::Unminimize,
                    });
                } else {
                    self.manage(e.window);
                }
            }
            XEvent::UnmapNotify(e) => {
                // One unmap reaches the manager twice, from the client's own StructureNotify and
                // from its parent's SubstructureNotify; only the client's copy, or the synthetic
                // one the ICCCM has a withdrawing client send to the root, is counted.
                let synthetic = e.response_type & 0x80 != 0;
                if e.event != e.window && !(synthetic && e.event == self.root) {
                    return;
                }
                let Some(client) = self.clients.get_mut(&e.window) else {
                    return;
                };
                if client.ignore_unmaps > 0 {
                    client.ignore_unmaps -= 1;
                    return;
                }
                let id = client.id;
                client.mapped = false;
                self.dispatch(Event::Unmapped { id });
                self.unmanage(e.window, false);
                self.dispatch(Event::Removed { id });
            }
            XEvent::DestroyNotify(e) => {
                if let Some(id) = self.id_of(e.window) {
                    let mapped = self.clients[&e.window].mapped;
                    if mapped {
                        self.dispatch(Event::Unmapped { id });
                    }
                    if let Some(mut client) = self.clients.remove(&e.window) {
                        self.by_id.remove(&id);
                        self.effects.remove(&id);
                        client.frame.destroy(&self.conn);
                        if client.strut.is_some() {
                            self.publish_outputs();
                        }
                    }
                    self.dispatch(Event::Removed { id });
                    self.publish_client_list();
                }
            }
            XEvent::ConfigureRequest(e) => self.configure_request(e),
            XEvent::PropertyNotify(e) => self.property_notify(e),
            XEvent::ClientMessage(e) => self.client_message(e),
            XEvent::ButtonPress(e) => {
                self.last_time = e.time;
                self.button(
                    e.event,
                    e.child,
                    e.root_x as f64 / self.scale as f64,
                    e.root_y as f64 / self.scale as f64,
                    e.detail,
                    e.state.into(),
                    true,
                    e.time,
                );
            }
            XEvent::ButtonRelease(e) => {
                self.last_time = e.time;
                self.button(
                    e.event,
                    e.child,
                    e.root_x as f64 / self.scale as f64,
                    e.root_y as f64 / self.scale as f64,
                    e.detail,
                    e.state.into(),
                    false,
                    e.time,
                );
            }
            XEvent::MotionNotify(e) => {
                self.last_time = e.time;
                let under = self.under_at(e.event, e.child);
                self.dispatch(Event::Pointer(
                    PointerEvent {
                        x: e.root_x as f64 / self.scale as f64,
                        y: e.root_y as f64 / self.scale as f64,
                        kind: PointerKind::Motion,
                        modifiers: self.keymap.modifiers(e.state.into()),
                        time: e.time,
                    },
                    under,
                ));
            }
            XEvent::EnterNotify(e) | XEvent::LeaveNotify(e) => {
                self.last_time = e.time;
                let under = self.under_at(e.event, e.child);
                self.dispatch(Event::Pointer(
                    PointerEvent {
                        x: e.root_x as f64 / self.scale as f64,
                        y: e.root_y as f64 / self.scale as f64,
                        kind: PointerKind::Motion,
                        modifiers: self.keymap.modifiers(e.state.into()),
                        time: e.time,
                    },
                    under,
                ));
            }
            XEvent::KeyPress(e) => {
                self.last_time = e.time;
                self.key(e.detail, e.state.into(), true);
            }
            XEvent::KeyRelease(e) => {
                self.last_time = e.time;
                self.key(e.detail, e.state.into(), false);
            }
            XEvent::XinputRawKeyPress(e) => {
                if self.engine.release_bind_armed() {
                    let (keysym, _) = self.keymap.keysym(e.detail.min(255) as u8, false);
                    self.engine.note_key_pressed(keysym);
                }
            }
            XEvent::MappingNotify(_) => {
                if let Ok(keymap) = Keymap::read(&self.conn) {
                    self.keymap = keymap;
                    self.grab_keys();
                }
            }
            XEvent::RandrScreenChangeNotify(_) | XEvent::RandrNotify(_) => self.publish_outputs(),
            XEvent::ShapeNotify(e) => self.sync_client_shape(e.affected_window),
            XEvent::SelectionClear(e) => {
                if e.selection == self.atoms.WM_S0 {
                    log::warn!(target: "wm", "another window manager took WM_S0; leaving the screen to it");
                    self.running.store(false, Ordering::Relaxed);
                }
            }
            XEvent::Error(e) => {
                log::debug!(target: "wm", "X error: {e:?}");
            }
            _ => {}
        }
    }

    /// What lies under an event: the managed window the event or its child belongs to.
    fn under_at(&self, event: Window, child: Window) -> Under {
        let window = self
            .client_of_window(event)
            .or_else(|| self.client_of_window(child))
            .and_then(|w| self.id_of(w));
        Under {
            window,
            top_layer: false,
            desktop: false,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn button(
        &mut self,
        event: Window,
        child: Window,
        x: f64,
        y: f64,
        detail: u8,
        state: u16,
        pressed: bool,
        time: u32,
    ) {
        let under = self.under_at(event, child);
        let on_client = self.clients.contains_key(&event);
        let kind = match detail {
            1 => PointerKind::Button {
                button: BTN_LEFT,
                pressed,
            },
            2 => PointerKind::Button {
                button: BTN_MIDDLE,
                pressed,
            },
            3 => PointerKind::Button {
                button: BTN_RIGHT,
                pressed,
            },
            4 | 5 => {
                if !pressed {
                    if on_client {
                        let _ = self.conn.allow_events(xproto::Allow::REPLAY_POINTER, time);
                    }
                    return;
                }
                PointerKind::Axis {
                    horizontal: 0.0,
                    vertical: if detail == 4 { -1.0 } else { 1.0 },
                }
            }
            other => PointerKind::Button {
                button: 0x110 + other as u32 - 1,
                pressed,
            },
        };
        let verdict = self.dispatch(Event::Pointer(
            PointerEvent {
                x,
                y,
                kind,
                modifiers: self.keymap.modifiers(state),
                time,
            },
            under,
        ));
        if self.moveresize_grab && !self.engine.grabbing() {
            self.moveresize_grab = false;
            let _ = self.conn.ungrab_pointer(time);
        }
        if on_client && pressed {
            let mode = match verdict {
                Verdict::Pass => xproto::Allow::REPLAY_POINTER,
                Verdict::Consumed => xproto::Allow::ASYNC_POINTER,
            };
            let _ = self.conn.allow_events(mode, time);
            if verdict == Verdict::Pass
                && let PointerKind::Button { button, .. } = kind
            {
                // The replayed press ends the grab, so its release goes to the client alone:
                // the engine's press ends here, else a later release would read as its click.
                self.dispatch(Event::Pointer(
                    PointerEvent {
                        x,
                        y,
                        kind: PointerKind::Button {
                            button,
                            pressed: false,
                        },
                        modifiers: self.keymap.modifiers(state),
                        time,
                    },
                    under,
                ));
            }
        }
    }

    fn key(&mut self, keycode: u8, state: u16, pressed: bool) {
        let modifiers = self.keymap.modifiers(state);
        let (keysym, base) = self.keymap.keysym(keycode, modifiers.shift);
        self.dispatch(Event::Key(KeyEvent {
            keysym,
            base_keysym: base,
            keycode: keycode as u32,
            pressed,
            modifiers,
        }));
    }

    fn configure_request(&mut self, e: xproto::ConfigureRequestEvent) {
        let mask = e.value_mask;
        let Some(client) = self.clients.get(&e.window) else {
            let mut aux = ConfigureWindowAux::new();
            if mask.contains(xproto::ConfigWindow::X) {
                aux = aux.x(e.x as i32);
            }
            if mask.contains(xproto::ConfigWindow::Y) {
                aux = aux.y(e.y as i32);
            }
            if mask.contains(xproto::ConfigWindow::WIDTH) {
                aux = aux.width(e.width as u32);
            }
            if mask.contains(xproto::ConfigWindow::HEIGHT) {
                aux = aux.height(e.height as u32);
            }
            if mask.contains(xproto::ConfigWindow::BORDER_WIDTH) {
                aux = aux.border_width(e.border_width as u32);
            }
            if mask.contains(xproto::ConfigWindow::SIBLING) {
                aux = aux.sibling(e.sibling);
            }
            if mask.contains(xproto::ConfigWindow::STACK_MODE) {
                aux = aux.stack_mode(e.stack_mode);
            }
            let _ = self.conn.configure_window(e.window, &aux);
            return;
        };
        let id = client.id;
        let mut rect = client.rect;
        if mask.contains(xproto::ConfigWindow::X) {
            rect.x = self.logical_pos(e.x as i32);
        }
        if mask.contains(xproto::ConfigWindow::Y) {
            rect.y = self.logical_pos(e.y as i32);
        }
        if mask.contains(xproto::ConfigWindow::WIDTH) {
            rect.w = self.logical_len(e.width as i32);
        }
        if mask.contains(xproto::ConfigWindow::HEIGHT) {
            rect.h = self.logical_len(e.height as i32);
        }
        self.dispatch(Event::Request {
            id,
            request: Request::Geometry(rect),
        });
        if mask.contains(xproto::ConfigWindow::STACK_MODE) {
            let request = match e.stack_mode {
                StackMode::ABOVE | StackMode::TOP_IF => Some(Request::Raise),
                StackMode::BELOW | StackMode::BOTTOM_IF => Some(Request::Lower),
                _ => None,
            };
            if let Some(request) = request {
                self.dispatch(Event::Request { id, request });
            }
        }
        if let Some(client) = self.clients.get(&e.window) {
            self.send_configure_notify(client);
        }
    }

    fn property_notify(&mut self, e: xproto::PropertyNotifyEvent) {
        if e.window == self.root {
            if e.atom == u32::from(AtomEnum::RESOURCE_MANAGER) {
                self.update_scale();
            }
            return;
        }
        let Some(client) = self.clients.get(&e.window) else {
            return;
        };
        let a = self.atoms;
        let id = client.id;
        if e.atom == a._NET_WM_STRUT || e.atom == a._NET_WM_STRUT_PARTIAL {
            let strut = self.props().strut(e.window);
            if let Some(client) = self.clients.get_mut(&e.window) {
                client.strut = strut;
            }
            self.publish_outputs();
            return;
        }
        if e.atom == a.WM_PROTOCOLS {
            let protocols = self.props().protocols(e.window);
            if let Some(client) = self.clients.get_mut(&e.window) {
                client.protocols = protocols;
            }
            return;
        }
        if [
            a.WM_NAME,
            a._NET_WM_NAME,
            a.WM_CLASS,
            a.WM_HINTS,
            a.WM_NORMAL_HINTS,
            a.WM_TRANSIENT_FOR,
            a._NET_WM_WINDOW_TYPE,
            a._MOTIF_WM_HINTS,
        ]
        .contains(&e.atom)
        {
            let info = self.client_info(e.window, id, client.rect);
            self.dispatch(Event::Changed(info));
        }
    }

    fn client_message(&mut self, e: xproto::ClientMessageEvent) {
        let a = self.atoms;
        let data = e.data.as_data32();
        if e.type_ == a._NET_CURRENT_DESKTOP {
            self.dispatch(Event::ActivateDesktop(data[0]));
            return;
        }
        if e.type_ == a._NET_SHOWING_DESKTOP {
            let on = data[0] != 0;
            self.props().set32(
                self.root,
                a._NET_SHOWING_DESKTOP,
                AtomEnum::CARDINAL,
                &[on as u32],
            );
            self.dispatch(Event::ShowDesktop(on));
            return;
        }
        if e.type_ == a._NET_REQUEST_FRAME_EXTENTS && !self.clients.contains_key(&e.window) {
            let extents = self.px_extents(crate::frame::extents(&self.engine.look().style, false, false));
            self.props().set32(
                e.window,
                a._NET_FRAME_EXTENTS,
                AtomEnum::CARDINAL,
                &[
                    extents.left as u32,
                    extents.right as u32,
                    extents.top as u32,
                    extents.bottom as u32,
                ],
            );
            return;
        }
        let Some((id, states, net_states, shaded)) = self
            .clients
            .get(&e.window)
            .map(|c| (c.id, c.states, c.net_states.clone(), c.shaded))
        else {
            return;
        };
        if e.type_ == a._NET_WM_STATE {
            let action = data[0];
            for atom in [data[1], data[2]] {
                if atom == 0 {
                    continue;
                }
                let want = |current: bool| match action {
                    props::NET_WM_STATE_ADD => true,
                    props::NET_WM_STATE_REMOVE => false,
                    _ => !current,
                };
                let request =
                    if atom == a._NET_WM_STATE_MAXIMIZED_HORZ || atom == a._NET_WM_STATE_MAXIMIZED_VERT {
                        if want(states.maximized) {
                            Request::Maximize
                        } else {
                            Request::Unmaximize
                        }
                    } else if atom == a._NET_WM_STATE_FULLSCREEN {
                        if want(states.fullscreen) {
                            Request::Fullscreen(None)
                        } else {
                            Request::Unfullscreen
                        }
                    } else if atom == a._NET_WM_STATE_HIDDEN {
                        if want(states.minimized) {
                            Request::Minimize
                        } else {
                            Request::Unminimize
                        }
                    } else if atom == a._NET_WM_STATE_ABOVE {
                        let on = want(net_states.contains(&atom));
                        self.toggle_net_state(e.window, atom, on);
                        Request::Above(on)
                    } else if atom == a._NET_WM_STATE_BELOW {
                        let on = want(net_states.contains(&atom));
                        self.toggle_net_state(e.window, atom, on);
                        Request::Below(on)
                    } else if atom == a._NET_WM_STATE_SHADED {
                        Request::Shade(want(shaded))
                    } else if atom == a._NET_WM_STATE_DEMANDS_ATTENTION {
                        let on = want(net_states.contains(&atom));
                        self.toggle_net_state(e.window, atom, on);
                        Request::Attention(on)
                    } else if atom == a._NET_WM_STATE_SKIP_TASKBAR
                        || atom == a._NET_WM_STATE_SKIP_PAGER
                        || atom == a._NET_WM_STATE_MODAL
                    {
                        let on = want(net_states.contains(&atom));
                        self.toggle_net_state(e.window, atom, on);
                        self.publish_states(e.window);
                        continue;
                    } else {
                        continue;
                    };
                self.dispatch(Event::Request { id, request });
            }
        } else if e.type_ == a._NET_WM_DESKTOP {
            let desktop = (data[0] != ALL_DESKTOPS).then_some(data[0]);
            let mut out = Vec::new();
            self.engine.send_window_to_desktop(id, desktop, &mut out);
            for command in out {
                self.apply(command);
            }
        } else if e.type_ == a._NET_ACTIVE_WINDOW {
            self.dispatch(Event::Request {
                id,
                request: Request::Activate,
            });
        } else if e.type_ == a._NET_CLOSE_WINDOW {
            self.dispatch(Event::Request {
                id,
                request: Request::Close,
            });
        } else if e.type_ == a.WM_CHANGE_STATE {
            if data[0] == props::WM_STATE_ICONIC {
                self.dispatch(Event::Request {
                    id,
                    request: Request::Minimize,
                });
            }
        } else if e.type_ == a._NET_WM_MOVERESIZE {
            let request = match data[2] {
                0 => Request::Resize(ResizeEdge::TopLeft),
                1 => Request::Resize(ResizeEdge::Top),
                2 => Request::Resize(ResizeEdge::TopRight),
                3 => Request::Resize(ResizeEdge::Right),
                4 => Request::Resize(ResizeEdge::BottomRight),
                5 => Request::Resize(ResizeEdge::Bottom),
                6 => Request::Resize(ResizeEdge::BottomLeft),
                7 => Request::Resize(ResizeEdge::Left),
                8 | 10 => Request::Move,
                9 => Request::Resize(ResizeEdge::None),
                _ => return,
            };
            let _ = self.conn.grab_pointer(
                true,
                self.root,
                EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                0u32,
                0u32,
                self.last_time,
            );
            self.moveresize_grab = true;
            self.dispatch(Event::Request { id, request });
            if !self.engine.grabbing() {
                self.moveresize_grab = false;
                let _ = self.conn.ungrab_pointer(self.last_time);
            }
        } else if e.type_ == a._NET_MOVERESIZE_WINDOW {
            let flags = data[0];
            let mut rect = self.clients.get(&e.window).map(|c| c.rect).unwrap_or_default();
            if flags & (1 << 8) != 0 {
                rect.x = self.logical_pos(data[1] as i32);
            }
            if flags & (1 << 9) != 0 {
                rect.y = self.logical_pos(data[2] as i32);
            }
            if flags & (1 << 10) != 0 {
                rect.w = self.logical_len(data[3] as i32);
            }
            if flags & (1 << 11) != 0 {
                rect.h = self.logical_len(data[4] as i32);
            }
            self.dispatch(Event::Request {
                id,
                request: Request::Geometry(rect),
            });
        }
    }

    fn toggle_net_state(&mut self, window: Window, atom: xproto::Atom, on: bool) {
        if let Some(client) = self.clients.get_mut(&window) {
            client.net_states.retain(|s| *s != atom);
            if on {
                client.net_states.push(atom);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The keymap resolves modifier masks from the modifier map and matches binds' keysyms
    /// case aside.
    #[test]
    fn the_keymap_reads_modifiers_and_keycodes() {
        let keymap = Keymap {
            min_keycode: 8,
            per_keycode: 2,
            keysyms: vec![0x61, 0x41, XK_ALT_L, 0, XK_SUPER_L, 0, 0xff09, 0xfe20],
            alt: u16::from(ModMask::M1),
            logo: u16::from(ModMask::M4),
            num_lock: u16::from(ModMask::M2),
            scroll_lock: 0,
        };
        assert_eq!(keymap.keycodes_for(0x61), vec![8]);
        assert_eq!(keymap.keysym(8, true), (0x41, 0x61));
        assert_eq!(keymap.keycodes_for(0xff09), vec![11]);
        let mods = keymap.modifiers(u16::from(ModMask::M1) | u16::from(ModMask::SHIFT));
        assert!(mods.alt && mods.shift && !mods.logo);
        assert_eq!(
            keymap.mask(mods),
            u16::from(ModMask::M1) | u16::from(ModMask::SHIFT)
        );
        assert_eq!(keymap.lock_variants().len(), 4);
    }
}
