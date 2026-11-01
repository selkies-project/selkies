/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Display plumbing shared by the WebSocket and WebRTC transports.
//!
//! Covers X11 RandR display management (resolution modes, extended-desktop outputs and
//! logical monitors, framebuffer sizing), per-desktop-environment DPI application, Wayland
//! output-id mapping and output management through `pixelflux::Compositor`, the
//! `pixelflux::CaptureSettings` population both transports share, and the cursor payload
//! helpers.
//!
//! Every RandR operation runs natively on a retained `x11rb` connection (`x11`); the
//! xrandr/cvt/gtf subprocess each one degrades to when the native call fails lives in
//! `xrandr`. Blocking X work runs on `tokio::task::spawn_blocking` under one lock so the
//! runtime's workers never wait on the X server. A helper drops the cached connection on
//! any failure other than an X protocol error: a protocol error leaves the connection
//! healthy, while a broken connection also frees this session's RandR modes. Every
//! mutation reads its result back before claiming success.
//!
//! An extended desktop is laid out one of two ways, decided by what the server offers
//! (`apply_extended_layout`). Where it has pluggable outputs -- spare RandR outputs
//! carrying a `Connected` property, each with a CRTC of its own, which the Xvfb the
//! images build provides -- every display is a real output: a secondary is plugged in,
//! given its exact mode and a position, and unplugged when its client leaves, so window
//! managers and toolkits meet what hardware would show them and need no knowledge of the
//! framebuffer (`x11::apply_output_layout`). A layout that both adds a display and moves
//! the primary publishes the move first (`geometry::output_layout_stage`). Anywhere else
//! the server has one CRTC covering the framebuffer and each display is a RandR 1.5
//! logical monitor over it, which consumers that build their screens from CRTCs do not
//! follow; that layout, the window-manager restart it needs, and the subprocess fallbacks
//! are `monitors` and `xrandr`, reached only at the point the output layout falls back.
//!
//! Either way RandR moves a screen and leaves every window at its root coordinates: the
//! primary moves back to the origin when the display on its left leaves, or aside when one
//! is added there, and takes none of its windows along, so they end up beyond the screen
//! or on the other display. Openbox and xfwm4 carry an unmaximized window along with its
//! monitor by themselves; Marco, Metacity, and KWin leave it where it was, and xfwm4
//! maximizes a window again on the monitor its restored geometry falls on. Every layout
//! change therefore reads the manager's client list before it moves anything and asks the
//! manager afterwards, through `_NET_MOVERESIZE_WINDOW`, to move each window by its own
//! display's move and each window of a display that is gone onto the primary
//! (`geometry::window_moves`, `x11::follow_display_moves`), once the framebuffer holds the
//! layout, since a manager constrains a move against the screen it has; the desktop
//! window and panels place themselves and are left alone, except that a desktop window
//! its manager left reaching past the screen is asked back to the origin
//! (`x11::seat_desktop_windows`).
//!
//! DPI handling here is X11-only by design: on the Wayland backend a DPI is an output
//! scale on the session compositor (applied in-process through wlr-output-management),
//! never Xft resources -- XWayland runs in the compositor's logical space and is scaled
//! with it, so Xft resources merged there would scale applications twice. On X11 a density
//! or cursor size reaches only the session on this display: the daemons told to reload and
//! the settings stores written through a session bus are found by the DISPLAY their
//! processes run with (`dpi::pids_on_display`, `dpi::session_env`), never through whatever
//! bus Selkies inherited, since another display's session may share the home and would
//! apply the change live.

pub mod capture;
pub mod dpi;
pub mod geometry;
pub mod modes;
pub mod monitors;
pub mod resize;
pub mod wayland;
pub mod x11;
pub mod xrandr;

use std::collections::BTreeMap;
use std::fmt;

pub use capture::{
    CommonFrames, CursorPayload, DisplayCapture, FIRST_FRAME_WAIT_S, FRAME_ANCHOR, FRAME_NUM_WRAP,
    LOST_FRAME_MEMORY, cursor_content_handle, format_cursor, no_first_frame,
    populate_capture_settings, structurally_same,
};
pub use dpi::{applied_dpi, desktop_dpi, restore_dpi, set_cursor_size, set_dpi};
pub use geometry::{
    DPI_FOLLOWS_PRIMARY, RealizedFit, accepts_dpi_sync, align_dims_16, clamp_primary_feedback,
    compute_dual_layout, cursor_size_for_dpi, fit_res, layout_extent, output_layout_stage,
    parse_resize_dims, reconcile_realized_layout, window_moves,
};
pub use monitors::{
    MultiMonitorWindowManager, WindowSnapshot, announce_monitor_change, apply_monitor_layout,
    clear_selkies_monitors, follow_display_moves, replace_selkies_monitors, window_snapshot,
};
pub use resize::{
    ResolutionInfo, apply_extended_layout, apply_output_layout, ensure_mode, get_new_res,
    grow_framebuffer, has_pluggable_outputs, read_realized_root, refresh_output_mode,
    resize_display, retire_displays,
};
pub use wayland::{WAYLAND_SCREEN_OUTPUT_ID, WaylandOutputs, wayland_output_id};
pub use xrandr::generate_xrandr_gtf_modeline;

/// A display's rectangle, `(x, y, w, h)` in root coordinates.
pub type Rect = (i32, i32, i32, i32);

/// One display's rectangle in an extended layout, as the transports keep it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Layout {
    /// A layout at `(x, y)` of `w` by `h`.
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Layout {
        Layout { x, y, w, h }
    }

    /// The layout as a rectangle tuple.
    pub const fn rect(&self) -> Rect {
        (self.x, self.y, self.w, self.h)
    }

    /// Whether `(px, py)` lies inside the rectangle.
    pub const fn contains(&self, px: i32, py: i32) -> bool {
        self.x <= px && px < self.x + self.w && self.y <= py && py < self.y + self.h
    }
}

impl From<Rect> for Layout {
    fn from((x, y, w, h): Rect) -> Layout {
        Layout { x, y, w, h }
    }
}

/// Display id to rectangle, the table every layout change is expressed in.
pub type Layouts = BTreeMap<String, Layout>;

/// Why a display operation failed, kept apart so the connection discipline can tell a
/// protocol error (the connection stays healthy) from a broken connection (dropped and
/// reopened on the next call).
#[derive(Debug)]
pub enum DisplayError {
    /// The X server refused a request; the connection is still good.
    Protocol(String),
    /// The connection to the X server failed or timed out.
    Connection(String),
    /// The server has no connected RandR output, so there is no mode to set.
    NoOutput,
    /// A request was carried out but the server did not realize it.
    Refused(String),
    /// An argument that cannot be acted on.
    Invalid(String),
}

impl DisplayError {
    /// Whether the error leaves the connection usable.
    pub fn is_protocol(&self) -> bool {
        !matches!(self, DisplayError::Connection(_))
    }

    /// Whether the server reported no connected RandR output.
    pub fn is_no_output(&self) -> bool {
        matches!(self, DisplayError::NoOutput)
    }
}

impl fmt::Display for DisplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DisplayError::Protocol(m) => write!(f, "X error {m}"),
            DisplayError::Connection(m) => write!(f, "X connection failed: {m}"),
            DisplayError::NoOutput => f.write_str("no connected RandR output"),
            DisplayError::Refused(m) | DisplayError::Invalid(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for DisplayError {}

impl From<x11rb::errors::ReplyError> for DisplayError {
    fn from(e: x11rb::errors::ReplyError) -> DisplayError {
        match e {
            x11rb::errors::ReplyError::X11Error(x) => {
                DisplayError::Protocol(format!("{:?} (bad value {})", x.error_kind, x.bad_value))
            }
            x11rb::errors::ReplyError::ConnectionError(c) => {
                DisplayError::Connection(c.to_string())
            }
        }
    }
}

impl From<x11rb::errors::ConnectionError> for DisplayError {
    fn from(e: x11rb::errors::ConnectionError) -> DisplayError {
        DisplayError::Connection(e.to_string())
    }
}

impl From<x11rb::errors::ConnectError> for DisplayError {
    fn from(e: x11rb::errors::ConnectError) -> DisplayError {
        DisplayError::Connection(e.to_string())
    }
}

impl From<tokio::task::JoinError> for DisplayError {
    fn from(e: tokio::task::JoinError) -> DisplayError {
        DisplayError::Connection(format!("X worker failed: {e}"))
    }
}
