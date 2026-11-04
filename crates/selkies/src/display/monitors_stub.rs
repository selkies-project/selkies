/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The logical-monitor names where no X server runs: every surface the transports and the
//! session wiring reach, answering that no monitor set is laid out or announced on this
//! OS the way a Linux build answers a server that refuses -- a layout the caller falls
//! back from, a window-manager decision that decides nothing -- so the transports keep
//! their shape. The X11 window bookkeeping a layout would move (a client-list snapshot and
//! the moves that follow it) has no counterpart here and stays under Linux with the
//! server it reads. The Windows display milestone lands in the place this file holds.

use super::Layouts;

/// The once-per-session window-manager decision, holding nothing off Linux: the desktop's
/// own manager takes every window, so there is nothing to restart for a second display.
#[derive(Default)]
pub struct MultiMonitorWindowManager;

impl MultiMonitorWindowManager {
    /// A manager that has decided nothing yet.
    pub fn new() -> Self {
        Self
    }

    /// Restart the running window manager if it is one that needs it; off Linux no manager
    /// runs that a second display would change, so the decision decides nothing.
    pub async fn ensure_for(&self, _display_count: usize, _is_wayland: bool) {}
}

/// Tell the toolkits already running that the monitor set changed; no monitor set is
/// served off Linux to announce.
pub async fn announce_monitor_change() {}

/// Swap the `selkies-*` logical monitor set to exactly `layouts`; no monitor set is served
/// off Linux, so the swap reports that nothing was laid out.
pub async fn replace_selkies_monitors(_layouts: &Layouts, _screen_name: Option<&str>) -> bool {
    false
}

/// Delete every logical monitor this software created; none is created off Linux.
pub async fn clear_selkies_monitors() {}

/// Drive the server into a logical-monitor layout covering `layouts`; no monitor set is
/// served off Linux, so the layout reports that nothing was laid out and the caller tears
/// it down.
pub async fn apply_monitor_layout(
    _layouts: &mut Layouts,
    _total_w: i32,
    _total_h: i32,
    _refresh: Option<f64>,
) -> bool {
    false
}
