/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The window policy interface: what a window manager decides for the compositor.
//!
//! The capture compositor (`wayland::frontend`) owns the protocol state of every window and
//! renders what is on each output; it decides nothing about where a window goes, which one has
//! focus, or what a key chord means. Those decisions belong to a [`WindowPolicy`] installed
//! through [`crate::Compositor::install_window_policy`]: the compositor hands it an [`Event`] at
//! each decision point and applies the [`Command`]s it answers with. Without a policy the
//! compositor keeps its own behavior, every window fullscreen on its output and no frame drawn,
//! so a session that nests another compositor or runs one application is exactly what it was.
//!
//! The interface is deliberately free of Smithay types: a window is a [`WindowId`], geometry
//! an integer [`Rect`] in the compositor's logical layout coordinates, and a frame or a menu a
//! raster [`Image`] the policy drew and the compositor presents, so the same policy drives an
//! X11 server through another backend with the same model. Images are premultiplied RGBA in
//! physical pixels at the output's scale, the format both tiny-skia produces and the renderers
//! import without conversion.
//!
//! The policy runs on the compositor thread, inside the handlers that call it, so it must never
//! block; a long task (a pipe menu's command) is run on a thread of its own and its result
//! applied on a later event. A panic in the policy is caught at the boundary
//! (`frontend::AppState::policy`): the compositor drops the policy, logs the panic, and carries
//! on with its own behavior, so a window-manager bug degrades the desktop rather than ending
//! the stream.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The compositor's stable numeric id of a window, the one its window list reports.
pub type WindowId = u32;

/// The display id of an output, the key the consumer addresses displays by.
pub type OutputId = u32;

/// An integer rectangle in logical layout coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    /// A rectangle from its corner and size; a negative size is clamped to zero.
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect {
            x,
            y,
            w: if w < 0 { 0 } else { w },
            h: if h < 0 { 0 } else { h },
        }
    }

    /// The right edge, exclusive.
    pub const fn right(&self) -> i32 {
        self.x + self.w
    }

    /// The bottom edge, exclusive.
    pub const fn bottom(&self) -> i32 {
        self.y + self.h
    }

    /// Whether the point lies inside.
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.right() && y < self.bottom()
    }

    /// Whether the point, in continuous coordinates, lies inside.
    pub fn contains_f64(&self, x: f64, y: f64) -> bool {
        x >= self.x as f64
            && y >= self.y as f64
            && x < self.right() as f64
            && y < self.bottom() as f64
    }

    /// The overlap with another rectangle, empty when they are apart.
    pub fn intersection(&self, other: &Rect) -> Rect {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        if right <= x || bottom <= y {
            return Rect::default();
        }
        Rect::new(x, y, right - x, bottom - y)
    }

    /// Whether the rectangle holds no pixel.
    pub const fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    /// The pixels inside, zero when empty.
    pub const fn area(&self) -> i64 {
        if self.is_empty() {
            0
        } else {
            self.w as i64 * self.h as i64
        }
    }

    /// The same size at another corner.
    pub const fn at(&self, x: i32, y: i32) -> Rect {
        Rect::new(x, y, self.w, self.h)
    }

    /// The rectangle grown by the extents on each side.
    pub fn grown(&self, extents: Extents) -> Rect {
        Rect::new(
            self.x - extents.left,
            self.y - extents.top,
            self.w + extents.left + extents.right,
            self.h + extents.top + extents.bottom,
        )
    }
}

/// A size in logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Size {
    pub w: i32,
    pub h: i32,
}

/// The thickness of a frame around a client area, in logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Extents {
    pub top: i32,
    pub left: i32,
    pub right: i32,
    pub bottom: i32,
}

/// The protocol a window arrived through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowKind {
    /// An xdg-shell toplevel.
    Xdg,
    /// An X11 window, through XWayland or on an X server: `override_redirect` is one the
    /// server told the manager to leave alone (a menu, a tooltip, a drag icon).
    X11 { override_redirect: bool },
}

/// What a window is for, from `_NET_WM_WINDOW_TYPE` or the xdg role.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WindowType {
    #[default]
    Normal,
    Dialog,
    Utility,
    Toolbar,
    Menu,
    Splash,
    Dock,
    Desktop,
    Notification,
    DropdownMenu,
    PopupMenu,
    Tooltip,
    Combo,
    Dnd,
}

/// What a client said about who decorates it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Decoration {
    /// The client said nothing: it bound no decoration protocol and set no hints, so the
    /// configuration's default decides.
    #[default]
    Unspecified,
    /// The client asked for server-side decorations.
    Server,
    /// The client draws its own frame.
    Client,
    /// Motif hints asked for no frame at all.
    None,
}

/// Size constraints a client published (`WM_NORMAL_HINTS`, xdg min and max size).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SizeHints {
    pub min: Option<Size>,
    pub max: Option<Size>,
    /// The step a resize moves in, for terminals.
    pub increment: Option<Size>,
    /// The size the increments count from.
    pub base: Option<Size>,
}

/// Everything the policy knows about a window, sent when it appears and again when any of it
/// changes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WindowInfo {
    pub id: WindowId,
    pub kind: Option<WindowKind>,
    pub window_type: WindowType,
    pub title: String,
    /// The xdg app id, or the X11 class.
    pub app_id: String,
    /// The X11 instance name; empty for a Wayland client.
    pub instance: String,
    /// The window this one is transient for.
    pub parent: Option<WindowId>,
    pub modal: bool,
    pub hints: SizeHints,
    /// The geometry an X11 client asked for, in layout coordinates, where it named one.
    pub requested: Option<Rect>,
    /// Whether the client asked for `requested`'s position (X11 `USPosition` or `PPosition`
    /// hints with a position other than the origin, which toolkits set without meaning it, or a
    /// type that places itself), so the policy keeps it instead of choosing a place.
    pub positioned: bool,
    pub decoration: Decoration,
    /// The output the window was opened on: the one under the pointer, or the one a request
    /// named.
    pub output: OutputId,
    pub pid: Option<u32>,
    /// States the client asked for before it mapped.
    pub wants_fullscreen: bool,
    pub wants_maximized: bool,
    pub wants_minimized: bool,
    pub skip_taskbar: bool,
    /// Whether the client accepts keyboard focus (X11 `WM_HINTS` input, `WM_TAKE_FOCUS`).
    pub accepts_focus: bool,
}

/// One output of the layout.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct OutputInfo {
    pub id: OutputId,
    /// The output's rectangle in logical layout coordinates.
    pub rect: Rect,
    /// The part of it not reserved by panels (layer-shell exclusive zones, X11 struts).
    pub workarea: Rect,
    pub scale: f64,
    pub primary: bool,
}

/// The modifiers held with a key or a button.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub logo: bool,
    pub caps_lock: bool,
    pub num_lock: bool,
}

/// One edge or corner a resize pulls, in xdg-shell's numbering.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ResizeEdge {
    #[default]
    None,
    Top,
    Bottom,
    Left,
    TopLeft,
    BottomLeft,
    Right,
    TopRight,
    BottomRight,
}

impl ResizeEdge {
    /// The edge xdg-shell and `_NET_WM_MOVERESIZE` number `raw`, `None` for the rest.
    pub fn from_xdg(raw: u32) -> ResizeEdge {
        match raw {
            1 => ResizeEdge::Top,
            2 => ResizeEdge::Bottom,
            4 => ResizeEdge::Left,
            5 => ResizeEdge::TopLeft,
            6 => ResizeEdge::BottomLeft,
            8 => ResizeEdge::Right,
            9 => ResizeEdge::TopRight,
            10 => ResizeEdge::BottomRight,
            _ => ResizeEdge::None,
        }
    }

    /// Whether the edge moves the top side.
    pub fn top(&self) -> bool {
        matches!(
            self,
            ResizeEdge::Top | ResizeEdge::TopLeft | ResizeEdge::TopRight
        )
    }

    /// Whether the edge moves the bottom side.
    pub fn bottom(&self) -> bool {
        matches!(
            self,
            ResizeEdge::Bottom | ResizeEdge::BottomLeft | ResizeEdge::BottomRight
        )
    }

    /// Whether the edge moves the left side.
    pub fn left(&self) -> bool {
        matches!(
            self,
            ResizeEdge::Left | ResizeEdge::TopLeft | ResizeEdge::BottomLeft
        )
    }

    /// Whether the edge moves the right side.
    pub fn right(&self) -> bool {
        matches!(
            self,
            ResizeEdge::Right | ResizeEdge::TopRight | ResizeEdge::BottomRight
        )
    }

    /// The CSS cursor name for pulling this edge.
    pub fn cursor(&self) -> &'static str {
        match self {
            ResizeEdge::None => "default",
            ResizeEdge::Top => "n-resize",
            ResizeEdge::Bottom => "s-resize",
            ResizeEdge::Left => "w-resize",
            ResizeEdge::Right => "e-resize",
            ResizeEdge::TopLeft => "nw-resize",
            ResizeEdge::TopRight => "ne-resize",
            ResizeEdge::BottomLeft => "sw-resize",
            ResizeEdge::BottomRight => "se-resize",
        }
    }
}

/// A request a client or a shell made of its window through a protocol.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Request {
    Maximize,
    Unmaximize,
    /// Fullscreen, on the output named where one was.
    Fullscreen(Option<OutputId>),
    Unfullscreen,
    Minimize,
    Unminimize,
    /// An interactive move the client started from a button press it holds.
    Move,
    /// An interactive resize the client started from a button press it holds.
    Resize(ResizeEdge),
    /// Focus and raise: xdg-activation, `_NET_ACTIVE_WINDOW`, or a taskbar.
    Activate,
    /// A taskbar or a shell asks the window to close.
    Close,
    /// The client asks for its window menu at a point in its own coordinates.
    ShowMenu {
        x: i32,
        y: i32,
    },
    /// An X11 client asks to be placed or sized (a `ConfigureRequest`).
    Geometry(Rect),
    /// An X11 client asks to be raised.
    Raise,
    /// An X11 client asks to be lowered.
    Lower,
    /// The window wants attention (`_NET_WM_STATE_DEMANDS_ATTENTION`).
    Attention(bool),
    /// Always on top, or no longer.
    Above(bool),
    /// Always below, or no longer.
    Below(bool),
    /// Shaded to its title bar, or unshaded.
    Shade(bool),
}

/// One pointer event in logical layout coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointerEvent {
    pub x: f64,
    pub y: f64,
    pub kind: PointerKind,
    pub modifiers: Modifiers,
    /// Milliseconds, the input time stamp.
    pub time: u32,
}

/// What the pointer did.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PointerKind {
    Motion,
    /// A button, in the Linux event codes (`0x110` is the left button).
    Button {
        button: u32,
        pressed: bool,
    },
    Axis {
        horizontal: f64,
        vertical: f64,
    },
}

/// What the compositor found under the pointer, for the policy's own hit test to combine
/// with: a layer surface on the top or overlay layers takes the pointer ahead of every window
/// and its frame, a window's client area stands below the policy's frames and menus.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Under {
    /// The window whose client area, or whose popup, lies under the point.
    pub window: Option<WindowId>,
    /// Whether a panel or an overlay surface lies under the point.
    pub top_layer: bool,
    /// Whether a desktop's wallpaper surface lies under the point: a background or bottom
    /// layer surface, the Wayland shape of an X11 desktop window, so a click there is the
    /// desktop's and never the bare root's.
    pub desktop: bool,
}

/// One key event, already resolved through the seat's keymap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    /// The X keysym the keycode produced under the held modifiers.
    pub keysym: u32,
    /// The keysym the keycode produces with no modifier held, what a bind names.
    pub base_keysym: u32,
    /// The evdev keycode, offset by eight as xkb numbers it.
    pub keycode: u32,
    pub pressed: bool,
    pub modifiers: Modifiers,
}

/// What a policy decides about an input event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Verdict {
    /// The compositor delivers the event to the client under the pointer or in focus.
    #[default]
    Pass,
    /// The policy took the event (a frame click, a grab in progress, a bind); the client sees
    /// nothing of it.
    Consumed,
}

/// A decision point the compositor hands the policy.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The layout changed: every output, in display id order.
    Outputs(Vec<OutputInfo>),
    /// A window appeared and waits for its first configure. The policy answers with
    /// [`Command::Configure`] at least; the compositor maps the window once the client has drawn.
    Added(WindowInfo),
    /// What the policy knew about a window changed (its title, parent, hints, or type).
    Changed(WindowInfo),
    /// The client drew its first buffer: the window is on screen at this size.
    Mapped { id: WindowId, size: Size },
    /// A mapped window committed a new size.
    Resized { id: WindowId, size: Size },
    /// The client withdrew its window (a null buffer, an unmap); it may map again.
    Unmapped { id: WindowId },
    /// The window is gone.
    Removed { id: WindowId },
    /// A protocol request about a window.
    Request { id: WindowId, request: Request },
    /// A taskbar told where it shows the window, in layout coordinates, or that it no longer
    /// does; where a minimize animation heads.
    TaskbarRect { id: WindowId, rect: Option<Rect> },
    /// A shell asked to show or to restore the desktop.
    ShowDesktop(bool),
    /// The pointer moved, pressed, or scrolled; answered with a [`Verdict`].
    Pointer(PointerEvent, Under),
    /// A key went down or up; answered with a [`Verdict`].
    Key(KeyEvent),
    /// The time the policy asked for ([`Command::WakeAt`]) came, or a frame is about to render
    /// while an effect runs.
    Tick(Instant),
    /// An X server the compositor runs for its X11 clients is up on this display number, so
    /// the policy can hand `DISPLAY` to what it starts.
    X11Display(u32),
    /// A client (a pager) asked, through the workspace protocol, to switch to this virtual
    /// desktop, counted from zero.
    ActivateDesktop(u32),
}

/// The xdg states a configure carries, which the compositor also publishes to taskbars.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct States {
    pub activated: bool,
    pub maximized: bool,
    pub fullscreen: bool,
    pub resizing: bool,
    /// Hidden by a minimize: the client keeps its surface and nothing of it is composited.
    pub minimized: bool,
    /// On another virtual desktop: kept and composited like a minimized window, but told to
    /// taskbars as a window on no output rather than a minimized one.
    pub hidden: bool,
    /// The window's right edge stays where the rectangle puts it whatever width the client
    /// commits, as a resize from the left edge holds it.
    pub holds_right: bool,
    /// The window's bottom edge stays where the rectangle puts it whatever height the client
    /// commits, as a resize from the top edge holds it.
    pub holds_bottom: bool,
}

/// A raster the policy drew: premultiplied RGBA, `width * 4` bytes a row, in physical pixels
/// at `scale` logical pixels to one.
#[derive(Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub scale: i32,
    pub rgba: Arc<[u8]>,
}

impl fmt::Debug for Image {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Image({}x{} @{})", self.width, self.height, self.scale)
    }
}

/// One drawn part of a window's frame, placed relative to the client area's top-left corner.
#[derive(Clone, Debug, PartialEq)]
pub struct FramePart {
    /// Where the part sits, relative to the client area, in logical pixels.
    pub rect: Rect,
    pub image: Image,
}

/// A window's server-side frame: the border it adds around the client area and the parts
/// that paint it. The compositor hit-tests nothing here; the policy keeps its own map of what
/// a point on the frame means.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Frame {
    pub extents: Extents,
    pub parts: Vec<FramePart>,
}

/// What an effect is for, so a backend that cannot scale a live window (an X server) still
/// knows what to show: a minimize or a restore moves the empty frame, a map, a resize, or a
/// close shows the window as it stands, a shade hides the client under its title bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectKind {
    Minimize,
    Restore,
    Map,
    Shade,
    /// A maximize, a restore from it, or a quick tile: the window glides from the geometry it
    /// had to the one it was configured to.
    Resize,
    /// The window is gone: its last picture, kept by the compositor as a ghost, shrinks and
    /// fades where it stood.
    Close,
}

/// A per-frame transform an effect applies to a window as it is presented: the client area is
/// drawn at `rect` instead of its configured geometry, which scales it, blended at `alpha`, and
/// its frame parts follow it at the same scale. A `Close` transform names a window the
/// compositor no longer has: it applies to the ghost the compositor kept of it, and the ghost
/// is dropped when the transform is cleared.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transform {
    pub kind: EffectKind,
    /// Where the window's client area (frame excluded) is drawn, in layout coordinates; its
    /// size relative to the configured one is the scale.
    pub rect: Rect,
    pub alpha: f32,
}

/// What the policy tells the compositor to do.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// Place and size the client area and set its states. Sent for a window the client has
    /// not mapped yet, it is the initial configure.
    Configure {
        id: WindowId,
        rect: Rect,
        states: States,
    },
    /// Where every window stands, bottom first. A mapped window left out keeps its place
    /// below the listed ones.
    Stack(Vec<WindowId>),
    /// Give the keyboard to this window, or to none.
    Focus(Option<WindowId>),
    /// Ask the client to close the window.
    Close(WindowId),
    /// The frame around a window, or none: the compositor presents the parts with the window
    /// and keeps its layout (`Rect::grown` by the extents) for the window list's geometry.
    Frame { id: WindowId, frame: Option<Frame> },
    /// A raster over everything on the layout (a menu, an on-screen display), replacing the
    /// one of the same key.
    Overlay { key: u32, rect: Rect, image: Image },
    /// Take an overlay down.
    ClearOverlay(u32),
    /// The pointer image while the policy holds the pointer, by CSS cursor name; `default`
    /// hands the choice back to the client under the pointer.
    Cursor(&'static str),
    /// Present a window through a transform (an effect frame), or plainly again.
    Effect {
        id: WindowId,
        transform: Option<Transform>,
    },
    /// Call the policy back with [`Event::Tick`] no later than this.
    WakeAt(Instant),
    /// Everything on the layout changed from the policy's side (a theme), so every frame is
    /// to be presented again.
    Redraw,
    /// The virtual desktops: how many there are and which is current, counted from zero, for
    /// the workspace protocol the pagers read.
    Desktops { count: u32, current: u32 },
}

/// A window manager's decisions for the compositor; see the module documentation.
pub trait WindowPolicy: Send {
    /// Handle one event, pushing the commands it leads to onto `out`, and say whether an input
    /// event was taken. The verdict is read for [`Event::Pointer`] and [`Event::Key`] alone.
    fn handle(&mut self, event: Event, out: &mut Vec<Command>) -> Verdict;
}

/// The interval between effect frames when no capture paces them: a frame at 60 Hz.
pub const EFFECT_FRAME: Duration = Duration::from_micros(16_667);

#[cfg(test)]
mod tests {
    use super::*;

    /// A rectangle grows by its extents on every side and reports its corners exclusively.
    #[test]
    fn a_rectangle_grows_by_its_extents() {
        let client = Rect::new(100, 50, 640, 480);
        let frame = client.grown(Extents {
            top: 30,
            left: 2,
            right: 2,
            bottom: 2,
        });
        assert_eq!(frame, Rect::new(98, 20, 644, 512));
        assert!(frame.contains(98, 20));
        assert!(!frame.contains(742, 532));
        assert_eq!(
            frame.intersection(&Rect::new(700, 500, 100, 100)),
            Rect::new(700, 500, 42, 32)
        );
        assert!(
            Rect::new(0, 0, 10, 10)
                .intersection(&Rect::new(20, 20, 5, 5))
                .is_empty()
        );
    }

    /// The xdg edge numbers map onto the edges they name and the sides they move.
    #[test]
    fn resize_edges_follow_the_xdg_numbering() {
        assert_eq!(ResizeEdge::from_xdg(10), ResizeEdge::BottomRight);
        assert!(ResizeEdge::from_xdg(10).bottom() && ResizeEdge::from_xdg(10).right());
        assert!(ResizeEdge::from_xdg(5).top() && ResizeEdge::from_xdg(5).left());
        assert_eq!(ResizeEdge::from_xdg(3), ResizeEdge::None);
        assert_eq!(ResizeEdge::Left.cursor(), "w-resize");
    }
}
