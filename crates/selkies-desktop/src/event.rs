//! The events a display backend feeds the desktop, and the requests the
//! desktop hands back to the backend.

use tiny_skia::Pixmap;

use crate::notify::CloseReason;
use crate::xkb::Key;

/// Identifies which desktop surface an event refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SurfaceId {
    /// The bar along the bottom of the primary display and the popups that open above it.
    Panel,
    /// The bar of another display and the popups that open above it, by the backend's
    /// number for the display. The desktop handles a bar's events as the panel's while the
    /// bar stands in for the panel, so it meets this only at the backends.
    Bar(u32),
    /// A wallpaper layer, one per output, by the backend's output number.
    Background(u32),
    /// A window of the shell's own, a file manager window or the settings
    /// window, by the desktop's window number.
    Window(u32),
    /// The stack of notification popups in the top right corner.
    Notifications,
}

/// The modifier keys held during an input event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    /// A shift key.
    pub shift: bool,
    /// A control key.
    pub ctrl: bool,
    /// An alt key.
    pub alt: bool,
    /// A Super key.
    pub logo: bool,
}

/// A pointer button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    /// The primary button.
    Left,
    /// The wheel button.
    Middle,
    /// The secondary button.
    Right,
}

/// Window state flags reported by the window manager.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ToplevelState {
    /// The window is iconified.
    pub minimized: bool,
    /// The window has keyboard focus.
    pub active: bool,
    /// The window is maximized.
    pub maximized: bool,
    /// The window is fullscreen.
    pub fullscreen: bool,
    /// The window is on another virtual desktop.
    pub hidden: bool,
}

/// An input or window event from the display server, in logical coordinates.
#[derive(Debug)]
pub enum Event {
    /// A surface was given a size, at a scale.
    Configured {
        surface: SurfaceId,
        width: i32,
        height: i32,
        scale: i32,
    },
    /// The monitors a background surface spans, in its logical coordinates,
    /// and whether it is the primary one that carries the desktop icons.
    BackgroundLayout {
        surface: u32,
        monitors: Vec<crate::geom::Rect>,
        primary: bool,
    },
    /// A background surface's output went away.
    BackgroundRemoved { surface: u32 },
    /// A display's bar went away, with its display or because bars on displays other than
    /// the primary were turned off.
    BarRemoved { bar: u32 },
    /// Where a display's top-left corner sits in the window manager's layout, by the
    /// backend's number for the display, and whether it is the primary one.
    DisplayOrigin {
        display: u32,
        x: i32,
        y: i32,
        primary: bool,
    },
    /// The pointer moved over a surface.
    PointerMotion { surface: SurfaceId, x: f64, y: f64 },
    /// A pointer button went down or up.
    PointerButton {
        surface: SurfaceId,
        x: f64,
        y: f64,
        button: Button,
        pressed: bool,
        time: u32,
        modifiers: Modifiers,
    },
    /// The wheel scrolled; positive deltas scroll down.
    PointerAxis {
        surface: SurfaceId,
        x: f64,
        y: f64,
        delta: f64,
    },
    /// The pointer left a surface.
    PointerLeave { surface: SurfaceId },
    /// A key was pressed while a surface had keyboard focus.
    Key {
        surface: SurfaceId,
        key: Key,
        modifiers: Modifiers,
    },
    /// A key went down or up while one of the shell's windows had keyboard
    /// focus, as the unshifted keysym of its key, for capturing a shortcut:
    /// a bare modifier counts as a shortcut only on its release.
    KeySym {
        surface: SurfaceId,
        keysym: u32,
        modifiers: Modifiers,
        pressed: bool,
    },
    /// A surface lost keyboard focus.
    KeyboardLeave { surface: SurfaceId },
    /// The window manager asked one of the shell's windows to close.
    WindowCloseRequested { window: u32 },
    /// A drag the desktop started entered one of its surfaces.
    DragEnter { surface: SurfaceId, x: f64, y: f64 },
    /// The drag moved over the surface it is in.
    DragMotion { surface: SurfaceId, x: f64, y: f64 },
    /// The drag left the surface it was in.
    DragLeave { surface: SurfaceId },
    /// The drag was dropped on one of the desktop's surfaces; `copy` when the
    /// user asked for a copy rather than a move.
    DragDrop {
        surface: SurfaceId,
        x: f64,
        y: f64,
        copy: bool,
    },
    /// The drag ended, dropped elsewhere or cancelled.
    DragEnd,
    /// A new toplevel window appeared.
    ToplevelAdded { id: u64 },
    /// A toplevel's title changed.
    ToplevelTitle { id: u64, title: String },
    /// A toplevel's application ID or class changed.
    ToplevelAppId { id: u64, app_id: String },
    /// A toplevel published its own icon.
    ToplevelIcon { id: u64, icon: Box<Pixmap> },
    /// A toplevel's state flags changed.
    ToplevelState { id: u64, state: ToplevelState },
    /// A toplevel closed.
    ToplevelClosed { id: u64 },
    /// The display a toplevel is on, by the backend's number for the display; `None` while
    /// it is on none.
    ToplevelDisplay { id: u64, display: Option<u32> },
    /// The window manager's virtual desktops: how many, and the index of
    /// the current one.
    Desktops { count: u32, current: u32 },
    /// The display server went away.
    Quit,
}

impl Event {
    /// The surface an input or configure event is for, where it names one.
    fn surface_mut(&mut self) -> Option<&mut SurfaceId> {
        match self {
            Event::Configured { surface, .. }
            | Event::PointerMotion { surface, .. }
            | Event::PointerButton { surface, .. }
            | Event::PointerAxis { surface, .. }
            | Event::PointerLeave { surface }
            | Event::Key { surface, .. }
            | Event::KeySym { surface, .. }
            | Event::KeyboardLeave { surface }
            | Event::DragEnter { surface, .. }
            | Event::DragMotion { surface, .. }
            | Event::DragLeave { surface }
            | Event::DragDrop { surface, .. } => Some(surface),
            _ => None,
        }
    }

    /// The display bar an event is for, if it is for one, and the same event aimed at the
    /// panel, which the desktop handles while that bar stands in for the panel.
    pub fn for_bar(mut self) -> Result<(u32, Event), Event> {
        match self.surface_mut() {
            Some(surface) => match *surface {
                SurfaceId::Bar(bar) => {
                    *surface = SurfaceId::Panel;
                    Ok((bar, self))
                }
                _ => Err(self),
            },
            None => Err(self),
        }
    }
}

/// What the desktop asks of a window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowAction {
    /// Focus and raise.
    Activate,
    /// Iconify.
    Minimize,
    /// Restore from iconified.
    Unminimize,
    /// Maximize.
    Maximize,
    /// Restore from maximized.
    Unmaximize,
    /// Ask the window to close.
    Close,
}

/// A request the desktop hands to the backend after handling events.
#[derive(Debug)]
pub enum Request {
    /// Perform an action on a toplevel.
    Window { id: u64, action: WindowAction },
    /// A popup opened or closed on a bar: adjust layer, grabs and keyboard focus there.
    MenuOpen { open: bool, surface: SurfaceId },
    /// Give every display but the primary one a bar of its own, or take them away.
    Bars(bool),
    /// Use the pointer shape for a clickable or a plain area.
    Cursor { pointer: bool },
    /// Place the bar: along the top edge or the bottom, reserving a height
    /// that includes the margin that floats it off the edge, and whether it
    /// hides, in which case it reserves no space. The bar lies above windows either way, a
    /// fullscreen window alone covering it.
    Panel { top: bool, height: i32, autohide: bool },
    /// Set the default sink's volume, in percent.
    SetVolume(u8),
    /// Toggle the default sink's mute.
    ToggleMute,
    /// Switch to the virtual desktop at an index.
    SwitchDesktop(u32),
    /// Open one of the shell's windows of a size, titled, with the app id a
    /// window manager and a taskbar know it by, the icon name a compositor
    /// resolves in its theme, and our own rendering of that icon as pixels.
    OpenWindow {
        id: u32,
        title: String,
        width: i32,
        height: i32,
        app_id: &'static str,
        icon_name: &'static str,
        icon: Option<Box<Pixmap>>,
    },
    /// Destroy one of the shell's windows.
    CloseWindow { id: u32 },
    /// Where a bar shows each of its windows, in the bar surface's logical
    /// coordinates, so the window manager's minimize heads for the button.
    TaskbarRects {
        surface: SurfaceId,
        rects: Vec<(u64, crate::geom::Rect)>,
    },
    /// Hold the keyboard for one of the shell's windows, or let it go, so a
    /// shortcut being captured reaches the window rather than the window
    /// manager's grabs.
    GrabKeyboard { id: u32, grab: bool },
    /// Retitle one of the shell's windows.
    SetTitle { id: u32, title: String },
    /// Start dragging files from a window, with a ghost image of the selection
    /// at the output scale the window renders at.
    StartDrag {
        window: u32,
        uris: Vec<String>,
        icon: Option<Box<Pixmap>>,
        scale: i32,
    },
    /// Place the notification stack's surface at a logical size in the top
    /// right corner of the primary output, `top` pixels below its top edge;
    /// a zero size removes it.
    Notifications { width: i32, height: i32, top: i32 },
    /// Signal that a notification closed, for the notification server.
    NotificationClosed { id: u32, reason: CloseReason },
    /// Signal that the user invoked a notification's action, for the
    /// notification server.
    ActionInvoked { id: u32, key: String },
}
