/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Wayland backend: a headless Smithay compositor that stands in for a real display server.
//!
//! `frontend` owns the compositor state machine, protocol handlers, and input routing.
//! `cursor` resolves Wayland cursor shapes to PNG images for the Python callback.

/// Wayland cursor shape to PNG resolution and the cursor delivery worker.
pub mod cursor;
/// Data-control clipboard client bridging a nested app compositor's selection.
pub mod dcclient;
/// libei (EIS) input injection over a portal session's `ConnectToEIS` socket.
pub mod eiclient;
/// Fake-input client delivering relative pointer motion into a nested KWin session.
pub mod ficlient;
/// `zwlr_foreign_toplevel_manager_v1`, the window list a taskbar reads and acts on.
pub mod foreign_toplevel;
/// Headless Smithay compositor, protocol handlers, and input routing.
pub mod frontend;
/// Host-capture mode: capture/inject as a client of an external compositor.
pub mod host;
/// KDE client growing, removing, and arranging a nested KWin session's screens.
pub mod kdeclient;
/// Generated bindings for the KDE output protocols vendored under `protocols/`.
pub mod kdeproto;
/// Seat keymap ownership: base layout plus batched overlay keysym binding.
pub mod keymap;
/// Mutter host arranger placing a GNOME host's monitors through DisplayConfig.
pub mod mutterclient;
/// Output-management client scaling a nested app compositor's screens.
pub mod outclient;
/// xdg-desktop-portal RemoteDesktop/ScreenCast session: streams and input for hosts without
/// the privileged Wayland protocols.
pub mod portal;
/// PipeWire consumer turning a portal monitor stream into host frames.
pub mod pwcapture;
/// Virtual-keyboard client for typing into a nested app compositor's socket.
pub mod vkclient;
/// Shared plumbing for outbound Wayland client connections.
pub mod wlclient;
/// The installed window policy's seat in the compositor: events out, commands applied.
pub mod wm;
/// `ext_workspace_manager_v1`, the virtual desktops a pager reads and switches.
pub mod workspace;
/// XWayland and Smithay's X11 window manager, driven by the window policy.
pub mod xwm;

/// A finger's scroll in a browser page's pixels per Wayland axis unit: Chromium and GTK 3 take
/// 10 units as one wheel step, which a page reads as 120 pixels. A touchpad's stroke sent in
/// axis units at this rate scrolls their windows as far as it scrolled the page.
pub const FINGER_PX_PER_AXIS: f64 = 12.0;

/// An enabled screen as clients see it: name, layout position, and the size of
/// the mode it currently holds.
pub type AppScreen = (String, i32, i32, i32, i32);
