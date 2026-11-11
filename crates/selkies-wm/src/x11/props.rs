/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The X properties the manager reads and writes: the atoms, the ICCCM and EWMH reads that fill
//! a window's [`WindowInfo`], and the writes that publish the manager and each window's state.

use pixelflux::wm::{Decoration, Rect, Size, SizeHints, WindowInfo, WindowKind, WindowType};

/// A dock's reservation: the pixels its strut takes at each screen edge and, from
/// `_NET_WM_STRUT_PARTIAL`, the span along each edge it covers (left, right, top, bottom, each
/// as a start and an inclusive end), `None` for a plain `_NET_WM_STRUT`, which spans the whole
/// edge. A strut is measured from the whole screen's edge, so a panel at the bottom of a short
/// output reaches past the bottom of a taller one beside it; the span says the taller output
/// keeps that room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reservation {
    pub strut: Strut,
    pub spans: Option<[(i32, i32); 4]>,
}
use x11rb::protocol::xproto::{self, AtomEnum, ConnectionExt, PropMode, Window};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

x11rb::atom_manager! {
    pub Atoms: AtomsCookie {
        UTF8_STRING,
        WM_S0,
        WM_NAME,
        WM_CLASS,
        WM_STATE,
        WM_CHANGE_STATE,
        WM_PROTOCOLS,
        WM_DELETE_WINDOW,
        WM_TAKE_FOCUS,
        WM_TRANSIENT_FOR,
        WM_NORMAL_HINTS,
        WM_HINTS,
        _MOTIF_WM_HINTS,
        _NET_SUPPORTED,
        _NET_SUPPORTING_WM_CHECK,
        _NET_WM_NAME,
        _NET_WM_PID,
        _NET_WM_ICON_NAME,
        _NET_CLIENT_LIST,
        _NET_CLIENT_LIST_STACKING,
        _NET_ACTIVE_WINDOW,
        _NET_CLOSE_WINDOW,
        _NET_WORKAREA,
        _NET_DESKTOP_GEOMETRY,
        _NET_DESKTOP_VIEWPORT,
        _NET_NUMBER_OF_DESKTOPS,
        _NET_CURRENT_DESKTOP,
        _NET_DESKTOP_NAMES,
        _NET_SHOWING_DESKTOP,
        _NET_WM_DESKTOP,
        _NET_WM_STATE,
        _NET_WM_STATE_MODAL,
        _NET_WM_STATE_STICKY,
        _NET_WM_STATE_MAXIMIZED_VERT,
        _NET_WM_STATE_MAXIMIZED_HORZ,
        _NET_WM_STATE_SHADED,
        _NET_WM_STATE_SKIP_TASKBAR,
        _NET_WM_STATE_SKIP_PAGER,
        _NET_WM_STATE_HIDDEN,
        _NET_WM_STATE_FULLSCREEN,
        _NET_WM_STATE_ABOVE,
        _NET_WM_STATE_BELOW,
        _NET_WM_STATE_DEMANDS_ATTENTION,
        _NET_WM_STATE_FOCUSED,
        _NET_WM_WINDOW_TYPE,
        _NET_WM_WINDOW_TYPE_DESKTOP,
        _NET_WM_WINDOW_TYPE_DOCK,
        _NET_WM_WINDOW_TYPE_TOOLBAR,
        _NET_WM_WINDOW_TYPE_MENU,
        _NET_WM_WINDOW_TYPE_UTILITY,
        _NET_WM_WINDOW_TYPE_SPLASH,
        _NET_WM_WINDOW_TYPE_DIALOG,
        _NET_WM_WINDOW_TYPE_DROPDOWN_MENU,
        _NET_WM_WINDOW_TYPE_POPUP_MENU,
        _NET_WM_WINDOW_TYPE_TOOLTIP,
        _NET_WM_WINDOW_TYPE_NOTIFICATION,
        _NET_WM_WINDOW_TYPE_COMBO,
        _NET_WM_WINDOW_TYPE_DND,
        _NET_WM_WINDOW_TYPE_NORMAL,
        _NET_WM_ALLOWED_ACTIONS,
        _NET_WM_ACTION_MOVE,
        _NET_WM_ACTION_RESIZE,
        _NET_WM_ACTION_MINIMIZE,
        _NET_WM_ACTION_SHADE,
        _NET_WM_ACTION_MAXIMIZE_HORZ,
        _NET_WM_ACTION_MAXIMIZE_VERT,
        _NET_WM_ACTION_FULLSCREEN,
        _NET_WM_ACTION_CLOSE,
        _NET_WM_ACTION_ABOVE,
        _NET_WM_ACTION_BELOW,
        _NET_WM_STRUT,
        _NET_WM_STRUT_PARTIAL,
        _NET_WM_MOVERESIZE,
        _NET_MOVERESIZE_WINDOW,
        _NET_REQUEST_FRAME_EXTENTS,
        _NET_FRAME_EXTENTS,
        _NET_WM_PING,
        _NET_WM_USER_TIME,
        _NET_WM_USER_TIME_WINDOW,
        _NET_WM_ICON,
        _NET_WM_ICON_GEOMETRY,
    }
}

/// `_NET_WM_STATE` values a client may ask for.
pub const NET_WM_STATE_REMOVE: u32 = 0;
pub const NET_WM_STATE_ADD: u32 = 1;
pub const NET_WM_STATE_TOGGLE: u32 = 2;

/// `WM_STATE` values.
pub const WM_STATE_WITHDRAWN: u32 = 0;
pub const WM_STATE_NORMAL: u32 = 1;
pub const WM_STATE_ICONIC: u32 = 3;

/// Motif `_MOTIF_WM_HINTS` flags and the decoration bit.
const MWM_HINTS_DECORATIONS: u32 = 1 << 1;
const MWM_DECOR_ALL: u32 = 1 << 0;

/// `WM_NORMAL_HINTS` flags.
const US_POSITION: u32 = 1 << 0;
const P_POSITION: u32 = 1 << 2;
const P_MIN_SIZE: u32 = 1 << 4;
const P_MAX_SIZE: u32 = 1 << 5;
const P_RESIZE_INC: u32 = 1 << 6;
const P_BASE_SIZE: u32 = 1 << 8;

/// `WM_HINTS` flags.
const INPUT_HINT: u32 = 1 << 0;
const STATE_HINT: u32 = 1 << 1;

/// A struts reservation: pixels taken at each screen edge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Strut {
    pub left: u32,
    pub right: u32,
    pub top: u32,
    pub bottom: u32,
}

/// What `WM_HINTS` and `WM_NORMAL_HINTS` say beyond the size hints.
#[derive(Clone, Copy, Debug, Default)]
pub struct WmHints {
    pub input: bool,
    pub initially_iconic: bool,
    pub position_set: bool,
}

/// The connection and the atoms, for the property reads.
pub struct Props<'a> {
    pub conn: &'a RustConnection,
    pub atoms: &'a Atoms,
}

impl Props<'_> {
    fn get(
        &self,
        window: Window,
        property: xproto::Atom,
        kind: impl Into<xproto::Atom>,
        length: u32,
    ) -> Option<xproto::GetPropertyReply> {
        self.conn
            .get_property(false, window, property, kind, 0, length)
            .ok()?
            .reply()
            .ok()
            .filter(|r| r.value_len > 0)
    }

    /// A 32-bit property's values.
    pub fn cardinals(
        &self,
        window: Window,
        property: xproto::Atom,
        kind: impl Into<xproto::Atom>,
    ) -> Vec<u32> {
        self.get(window, property, kind, 1024)
            .and_then(|r| r.value32().map(|v| v.collect()))
            .unwrap_or_default()
    }

    /// Where a taskbar shows the window, `_NET_WM_ICON_GEOMETRY` in root pixels, or `None`
    /// where no taskbar set it.
    pub fn icon_geometry(&self, window: Window) -> Option<Rect> {
        let values = self.cardinals(window, self.atoms._NET_WM_ICON_GEOMETRY, AtomEnum::CARDINAL);
        if values.len() < 4 || values[2] == 0 || values[3] == 0 {
            return None;
        }
        Some(Rect::new(
            values[0] as i32,
            values[1] as i32,
            values[2] as i32,
            values[3] as i32,
        ))
    }

    /// A text property, UTF-8 or Latin-1.
    pub fn text(&self, window: Window, property: xproto::Atom) -> Option<String> {
        let reply = self.get(window, property, AtomEnum::ANY, 4096)?;
        if reply.format != 8 {
            return None;
        }
        if reply.type_ == self.atoms.UTF8_STRING {
            String::from_utf8(reply.value).ok()
        } else {
            Some(reply.value.iter().map(|b| *b as char).collect())
        }
    }

    /// The window's title: `_NET_WM_NAME`, else `WM_NAME`.
    pub fn title(&self, window: Window) -> String {
        self.text(window, self.atoms._NET_WM_NAME)
            .or_else(|| self.text(window, self.atoms.WM_NAME))
            .unwrap_or_default()
    }

    /// `WM_CLASS` as instance and class.
    pub fn class(&self, window: Window) -> (String, String) {
        let Some(reply) = self.get(window, self.atoms.WM_CLASS, AtomEnum::STRING, 1024) else {
            return (String::new(), String::new());
        };
        let mut parts = reply
            .value
            .split(|b| *b == 0)
            .map(|p| String::from_utf8_lossy(p).to_string());
        let instance = parts.next().unwrap_or_default();
        let class = parts.next().unwrap_or_default();
        (instance, class)
    }

    /// The window this one is transient for.
    pub fn transient_for(&self, window: Window) -> Option<Window> {
        self.cardinals(window, self.atoms.WM_TRANSIENT_FOR, AtomEnum::WINDOW)
            .first()
            .copied()
            .filter(|w| *w != 0)
    }

    /// The first `_NET_WM_WINDOW_TYPE` the manager knows, else normal, or dialog for a transient.
    pub fn window_type(&self, window: Window, transient: bool) -> WindowType {
        let a = self.atoms;
        for atom in self.cardinals(window, a._NET_WM_WINDOW_TYPE, AtomEnum::ATOM) {
            let kind = match atom {
                x if x == a._NET_WM_WINDOW_TYPE_DESKTOP => WindowType::Desktop,
                x if x == a._NET_WM_WINDOW_TYPE_DOCK => WindowType::Dock,
                x if x == a._NET_WM_WINDOW_TYPE_TOOLBAR => WindowType::Toolbar,
                x if x == a._NET_WM_WINDOW_TYPE_MENU => WindowType::Menu,
                x if x == a._NET_WM_WINDOW_TYPE_UTILITY => WindowType::Utility,
                x if x == a._NET_WM_WINDOW_TYPE_SPLASH => WindowType::Splash,
                x if x == a._NET_WM_WINDOW_TYPE_DIALOG => WindowType::Dialog,
                x if x == a._NET_WM_WINDOW_TYPE_DROPDOWN_MENU => WindowType::DropdownMenu,
                x if x == a._NET_WM_WINDOW_TYPE_POPUP_MENU => WindowType::PopupMenu,
                x if x == a._NET_WM_WINDOW_TYPE_TOOLTIP => WindowType::Tooltip,
                x if x == a._NET_WM_WINDOW_TYPE_NOTIFICATION => WindowType::Notification,
                x if x == a._NET_WM_WINDOW_TYPE_COMBO => WindowType::Combo,
                x if x == a._NET_WM_WINDOW_TYPE_DND => WindowType::Dnd,
                x if x == a._NET_WM_WINDOW_TYPE_NORMAL => WindowType::Normal,
                _ => continue,
            };
            return kind;
        }
        if transient {
            WindowType::Dialog
        } else {
            WindowType::Normal
        }
    }

    /// Motif's decoration hint: `None` where it asks for no frame.
    pub fn decoration(&self, window: Window) -> Decoration {
        let hints = self.cardinals(window, self.atoms._MOTIF_WM_HINTS, self.atoms._MOTIF_WM_HINTS);
        if hints.len() >= 3 && hints[0] & MWM_HINTS_DECORATIONS != 0 {
            let decorations = hints[2];
            if decorations == 0 || (decorations & MWM_DECOR_ALL == 0 && decorations & !MWM_DECOR_ALL == 0) {
                return Decoration::None;
            }
        }
        Decoration::Unspecified
    }

    /// `WM_NORMAL_HINTS`: the size hints and whether a position was asked for.
    pub fn normal_hints(&self, window: Window) -> (SizeHints, bool) {
        let values = self.cardinals(window, self.atoms.WM_NORMAL_HINTS, self.atoms.WM_NORMAL_HINTS);
        if values.len() < 15 {
            return (SizeHints::default(), false);
        }
        let flags = values[0];
        let size = |w: u32, h: u32| Size {
            w: w as i32,
            h: h as i32,
        };
        let hints = SizeHints {
            min: (flags & P_MIN_SIZE != 0 && (values[5] > 0 || values[6] > 0))
                .then(|| size(values[5], values[6])),
            max: (flags & P_MAX_SIZE != 0 && values[7] > 0 && values[8] > 0)
                .then(|| size(values[7], values[8])),
            increment: (flags & P_RESIZE_INC != 0 && (values[9] > 0 || values[10] > 0))
                .then(|| size(values[9], values[10])),
            base: (flags & P_BASE_SIZE != 0 && values.len() >= 17).then(|| size(values[15], values[16])),
        };
        (hints, flags & (US_POSITION | P_POSITION) != 0)
    }

    /// `WM_HINTS`: whether the client takes input and starts iconic.
    pub fn wm_hints(&self, window: Window) -> WmHints {
        let values = self.cardinals(window, self.atoms.WM_HINTS, self.atoms.WM_HINTS);
        let mut hints = WmHints {
            input: true,
            initially_iconic: false,
            position_set: false,
        };
        if values.len() >= 2 {
            let flags = values[0];
            if flags & INPUT_HINT != 0 {
                hints.input = values[1] != 0;
            }
            if flags & STATE_HINT != 0 && values.len() >= 3 {
                hints.initially_iconic = values[2] == WM_STATE_ICONIC;
            }
        }
        hints
    }

    /// The `WM_PROTOCOLS` the client speaks.
    pub fn protocols(&self, window: Window) -> Vec<xproto::Atom> {
        self.cardinals(window, self.atoms.WM_PROTOCOLS, AtomEnum::ATOM)
    }

    /// The `_NET_WM_STATE` atoms a client set before it mapped.
    pub fn net_wm_state(&self, window: Window) -> Vec<xproto::Atom> {
        self.cardinals(window, self.atoms._NET_WM_STATE, AtomEnum::ATOM)
    }

    /// The window's reservation, from `_NET_WM_STRUT_PARTIAL` or `_NET_WM_STRUT`.
    pub fn strut(&self, window: Window) -> Option<Reservation> {
        let partial = self.cardinals(window, self.atoms._NET_WM_STRUT_PARTIAL, AtomEnum::CARDINAL);
        let (values, spans) = if partial.len() >= 12 {
            let span = |i: usize| (partial[i] as i32, partial[i + 1] as i32);
            (partial.clone(), Some([span(4), span(6), span(8), span(10)]))
        } else if partial.len() >= 4 {
            (partial, None)
        } else {
            (
                self.cardinals(window, self.atoms._NET_WM_STRUT, AtomEnum::CARDINAL),
                None,
            )
        };
        if values.len() < 4 {
            return None;
        }
        Some(Reservation {
            strut: Strut {
                left: values[0],
                right: values[1],
                top: values[2],
                bottom: values[3],
            },
            spans,
        })
    }

    /// `_NET_WM_PID`.
    pub fn pid(&self, window: Window) -> Option<u32> {
        self.cardinals(window, self.atoms._NET_WM_PID, AtomEnum::CARDINAL)
            .first()
            .copied()
    }

    /// Everything the policy is told about a client window, `geometry` its current root-relative
    /// geometry and `parent` the policy id of the window it is transient for.
    pub fn window_info(
        &self,
        window: Window,
        id: u32,
        geometry: Rect,
        parent: Option<u32>,
        output: u32,
    ) -> WindowInfo {
        let (instance, class) = self.class(window);
        let transient = self.transient_for(window).is_some();
        let (hints, position_set) = self.normal_hints(window);
        let wm_hints = self.wm_hints(window);
        let states = self.net_wm_state(window);
        let a = self.atoms;
        let has = |atom: xproto::Atom| states.contains(&atom);
        let window_type = self.window_type(window, transient);
        let places_itself = !matches!(
            window_type,
            WindowType::Normal | WindowType::Dialog | WindowType::Utility | WindowType::Toolbar
        );
        WindowInfo {
            id,
            kind: Some(WindowKind::X11 {
                override_redirect: false,
            }),
            window_type,
            title: self.title(window),
            app_id: class,
            instance,
            parent,
            modal: has(a._NET_WM_STATE_MODAL),
            hints,
            requested: Some(if position_set || places_itself {
                geometry
            } else {
                Rect::new(0, 0, geometry.w, geometry.h)
            }),
            positioned: (position_set && (geometry.x != 0 || geometry.y != 0)) || places_itself,
            decoration: self.decoration(window),
            output,
            pid: self.pid(window),
            wants_fullscreen: has(a._NET_WM_STATE_FULLSCREEN),
            wants_maximized: has(a._NET_WM_STATE_MAXIMIZED_HORZ) || has(a._NET_WM_STATE_MAXIMIZED_VERT),
            wants_minimized: wm_hints.initially_iconic || has(a._NET_WM_STATE_HIDDEN),
            skip_taskbar: has(a._NET_WM_STATE_SKIP_TASKBAR),
            accepts_focus: wm_hints.input || self.protocols(window).contains(&a.WM_TAKE_FOCUS),
        }
    }

    /// Set a 32-bit property.
    pub fn set32(
        &self,
        window: Window,
        property: xproto::Atom,
        kind: impl Into<xproto::Atom>,
        values: &[u32],
    ) {
        let _ = self
            .conn
            .change_property32(PropMode::REPLACE, window, property, kind, values);
    }

    /// Set a UTF-8 text property.
    pub fn set_text(&self, window: Window, property: xproto::Atom, text: &str) {
        let _ = self.conn.change_property8(
            PropMode::REPLACE,
            window,
            property,
            self.atoms.UTF8_STRING,
            text.as_bytes(),
        );
    }

    /// Send a `WM_PROTOCOLS` client message to a window.
    pub fn send_protocol(&self, window: Window, protocol: xproto::Atom, timestamp: u32) {
        let event = xproto::ClientMessageEvent::new(
            32,
            window,
            self.atoms.WM_PROTOCOLS,
            [protocol, timestamp, 0, 0, 0],
        );
        let _ = self
            .conn
            .send_event(false, window, xproto::EventMask::NO_EVENT, event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A partial strut reads its four edges and the span of each; the state constants are EWMH's.
    #[test]
    fn constants_follow_the_specifications() {
        assert_eq!(NET_WM_STATE_TOGGLE, 2);
        assert_eq!(WM_STATE_ICONIC, 3);
        assert_eq!(
            Strut {
                left: 0,
                right: 0,
                top: 0,
                bottom: 32
            }
            .bottom,
            32
        );
    }
}
