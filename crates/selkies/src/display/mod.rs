/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Display plumbing shared by both transports: X11 RandR display management (resolution
//! modes, extended-desktop outputs and logical monitors, framebuffer sizing), per-desktop DPI
//! application, Wayland output mapping through the compositor, and cursor sizing
//! (`display_utils.py`, `display_utils_xrandr.py`).
//!
//! Being ported; the Python module named is the specification.
