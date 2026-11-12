/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The one rasterizer of the Selkies desktop: the window manager's frames and menus and the
//! selkies-desktop shell draw through it, so a title bar and the panel beside it come out of the
//! same text shaper, the same icon lookup, and the same premultiplied RGBA pixel format.
//!
//! [`render`] is the canvas over a tiny-skia pixmap with cosmic-text for text, [`icons`] the XDG
//! icon theme lookup with [`xpm`] for the one legacy format themes still carry, [`geom`] the
//! integer rectangle every drawing call takes, [`dirs`] the XDG base directories the lookups
//! search, and [`apps`] the desktop entries both the shell's start menu and the window manager's
//! applications menu list, and [`desktop_icons`] where the shell's desktop icons stand, which
//! the window manager reads to leave a click on one to the shell. Nothing here knows a display
//! server: a caller presents the pixmaps it gets back. [`appearance`] carries the desktop's theme
//! past its own surfaces, to the toolkits and the Settings portal, from whichever of the two keeps
//! the session's appearance.

pub mod appearance;
pub mod apps;
pub mod desktop_icons;
pub mod dirs;
pub mod geom;
pub mod icons;
pub mod render;
pub mod style;
pub mod xpm;

pub use tiny_skia;
