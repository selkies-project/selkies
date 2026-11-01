/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Clipboard synchronization both ways: the X11 XFixes monitor and xclip fallback, the
//! compositor's selection sinks on Wayland, the flavors of a copy, digests, precedence, secrets,
//! and the streamed transfer of large contents (`input_handler.py`'s clipboard paths).
//!
//! Being ported; the Python module named is the specification.
