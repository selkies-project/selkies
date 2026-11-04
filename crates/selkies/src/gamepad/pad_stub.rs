/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! One virtual gamepad slot where neither the interposer nor the kernel serves one: the
//! slot table keeps its client associations, its slot gate, and its heartbeat sweep, and the
//! pad the table drives never comes up -- a `start` that reports it did not, a control
//! change that reaches no device, nothing held to neutralize -- so a `js,` message is
//! received, parsed, and answered, and nothing happens on the desktop. The start line the
//! slot table already logs tells the story ("0 virtual gamepads ready"). The Windows
//! gamepad milestone lands in the place this file holds.

use std::path::Path;
use std::sync::Arc;

/// What a pad tells the client to play: each motor's level, 0 to 1, and how long in ms.
pub type RumbleSink = Arc<dyn Fn(f64, f64, u32) + Send + Sync>;

/// One virtual gamepad slot where no backend runs.
pub struct Pad {
    index: usize,
    uinput_enabled: bool,
}

impl Pad {
    /// A pad for slot `index`; no sockets are bound and no kernel device is created.
    pub fn new(index: usize, _socket_dir: &Path, uinput_enabled: bool) -> Arc<Pad> {
        Arc::new(Pad {
            index,
            uinput_enabled,
        })
    }

    /// This slot's index.
    pub fn index(&self) -> usize {
        self.index
    }

    /// No servers run.
    pub fn running(&self) -> bool {
        false
    }

    /// The kernel mirroring the settings asked for; it never comes up off Linux.
    pub fn uinput_enabled(&self) -> bool {
        self.uinput_enabled
    }

    /// No kernel device is up.
    pub fn has_uinput(&self) -> bool {
        false
    }

    /// Nothing is held, since no control change reached a device.
    pub fn held_count(&self) -> usize {
        0
    }

    /// Where the pad's rumble would go; nothing plays one to send.
    pub fn set_on_rumble(&self, _sink: Option<RumbleSink>) {}

    /// Start; reports that no socket could be bound, the answer a Linux pad gives a socket
    /// directory it cannot listen in.
    pub async fn start(&self) -> bool {
        false
    }

    /// A control change from the client; no device is driven by it.
    pub fn send_event(&self, _client_index: u32, _client_value: f64, _is_button: bool) {}

    /// Nothing is held, so there is nothing to neutralize.
    pub fn reset_state(&self) {}

    /// No kernel device is up to bring up.
    pub fn ensure_uinput(&self) {}

    /// No effect plays, so there is no rumble to hand the sink again.
    pub fn ff_replay(&self) {}

    /// Stop; nothing runs, so there is nothing to stop.
    pub async fn close(&self) {}
}
