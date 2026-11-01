/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Virtual gamepads: kernel uinput devices where `/dev/uinput` is writable, else the
//! interposer sockets the fake-udev and input interposer libraries read, with rumble back to
//! the client (`input_handler.py`'s `SelkiesGamepad`, `UInputGamepad`, `GamepadMapper`).
//!
//! Being ported; the Python module named is the specification.
