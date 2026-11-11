/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The Selkies window manager: the one window manager of a `selkies` session that owns its
//! desktop, on both backends.
//!
//! Under `SELKIES_WM` the `selkies` process manages the windows of the backend pixelflux runs
//! on: on Wayland [`core::Engine`] is installed in the capture compositor as its window policy
//! (`pixelflux::wm`), deciding placement, focus, stacking, frames, and bindings for the Wayland
//! clients and, through XWayland, the X11 ones; on X11 [`x11::Manager`] runs the same engine on a
//! thread with its own connection to the Xvfb, reparenting clients into frames the engine draws.
//! `SELKIES_DESKTOP` adds the desktop shell on top, supervised by [`session::ShellSupervisor`].
//!
//! The crate is split along what differs: [`core`] decides, [`frame`] and [`menu`] draw through
//! `selkies-draw`, [`config`] reads the openbox-format files the images configure, [`theme`]
//! follows the shell's theme, [`session`] starts programs and the shell, and [`x11`] carries the
//! engine's commands out on an X server, where the compositor does the same on Wayland.

pub mod config;
pub mod core;
pub mod frame;
pub mod i18n;
pub mod menu;
pub mod session;
pub mod theme;
pub mod x11;

pub use core::Engine;
