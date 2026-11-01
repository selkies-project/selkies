/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The WebSockets data plane: per-client connection handling (roles, input dispatch,
//! settings), the per-display video relays with their bounded backpressure and decode gate, the
//! shared audio fan-out, microphone forwarding, clipboard and cursor delivery, stats, and the
//! display owner model (`websockets_mode.py`).
//!
//! Being ported; the Python module named is the specification.
