/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Server-side keyboard, mouse, touch, and IME injection with the input authority both
//! transports enforce, and the fallback ladders for X11 (XTEST) and Wayland (the compositor's
//! seat, the virtual keyboard, a clipboard paste) (`input_handler.py`, `server_keysym_map.py`,
//! `x11_xkb.py`).
//!
//! Being ported; the Python module named is the specification.
