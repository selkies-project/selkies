/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The display-density names where no X server runs: every surface the transports and the
//! session wiring reach, answering that no desktop density is read or written on this OS
//! the way a Linux build answers a desktop that names none -- a density that stays what
//! the settings gave it, a cursor size no channel applies -- so a page's density verb keeps
//! its wire shape and the refusal is the caller's one line. A session's density here is
//! the user's own setting; the Windows display milestone lands in the place this file
//! holds.

/// The density the last `set_dpi` gave the desktop; `None` before the first. No desktop
/// density is applied off Linux.
pub fn applied_dpi() -> Option<i32> {
    None
}

/// The density the desktop has before this process applies one; no X desktop runs off
/// Linux to read one from.
pub async fn desktop_dpi() -> Option<i32> {
    None
}

/// Settle the session's density at startup and return the one in effect: the configured
/// value, which is what a Linux build answers a desktop that names no density, since none
/// is read or written off Linux.
pub async fn restore_dpi(configured: i32, _locked: bool) -> i32 {
    configured
}

/// Set the display DPI; no channel applies one off Linux, so the answer is that none
/// succeeded and the caller keeps reporting the refusal.
pub async fn set_dpi(_dpi: i32) -> bool {
    false
}

/// Set the cursor size through the settings channels; no channel runs off Linux, so the
/// answer is that none succeeded.
pub async fn set_cursor_size(_size: i32) -> bool {
    false
}
