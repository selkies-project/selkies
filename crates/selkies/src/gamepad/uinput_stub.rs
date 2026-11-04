/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The kernel backend's names where no kernel serves one: the `uinput_gamepad` setting
//! resolves to no kernel device whatever it asks, the answer a Linux host without
//! `/dev/uinput` gives, so the slot table's start line already says ("kernel uinput off").
//! The Windows gamepad milestone lands in the place this file holds.

/// Resolve the `uinput_gamepad` setting against the host: no kernel input devices exist off
/// Linux, so every mode stays off.
pub fn uinput_gamepads_enabled(_mode: &str) -> bool {
    false
}
