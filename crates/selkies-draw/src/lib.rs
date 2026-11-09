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
//! applications menu list. Nothing here knows a display server: a caller presents the pixmaps it
//! gets back.

pub mod apps;
pub mod dirs;
pub mod geom;
pub mod icons;
pub mod render;
pub mod style;
pub mod xpm;

pub use tiny_skia;
