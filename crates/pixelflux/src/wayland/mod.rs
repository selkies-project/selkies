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

/// An enabled screen as clients see it: name, layout position, and the size of
/// the mode it currently holds.
pub type AppScreen = (String, i32, i32, i32, i32);
