/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The published keyboard and pointer where neither the kernel nor the interposer serves a
//! device: the capability sets, the specs, and the publish gate stay what they are, and a
//! device `open` answers that it never came up -- the answer a Linux host without
//! `/dev/uinput` and without the interposer pool gives -- so the caller's own line reports
//! the refusal ("Published device ... could not be opened") and no event is ever emitted.
//! The Windows input-device milestone lands in the place this file holds.

use std::path::Path;
use std::sync::Arc;

use crate::gamepad::abi::{
    BTN_EXTRA, BTN_LEFT, BTN_MIDDLE, BTN_MISC, BTN_RIGHT, BTN_SIDE, EV_KEY, EV_REL, REL_HWHEEL,
    REL_WHEEL, REL_X, REL_Y,
};

/// The evdev capability sets a device advertises.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub evbits: Vec<u16>,
    pub keybits: Vec<u16>,
    pub relbits: Vec<u16>,
    pub absbits: Vec<u16>,
}

/// A device the session publishes: its key in the table, name, product id, and capability
/// sets.
pub struct DeviceSpec {
    pub key: &'static str,
    pub name: &'static str,
    pub product: u16,
    pub caps: Capabilities,
}

/// The keyboard and pointer a session publishes: every keyboard key, and the five pointer
/// buttons with both wheels.
pub fn published_device_specs() -> Vec<DeviceSpec> {
    vec![
        DeviceSpec {
            key: "keyboard",
            name: "Selkies Virtual Keyboard",
            product: 0x0001,
            caps: Capabilities {
                evbits: vec![EV_KEY],
                keybits: (1..BTN_MISC).collect(),
                ..Capabilities::default()
            },
        },
        DeviceSpec {
            key: "pointer",
            name: "Selkies Virtual Pointer",
            product: 0x0002,
            caps: Capabilities {
                evbits: vec![EV_KEY, EV_REL],
                keybits: vec![BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA],
                relbits: vec![REL_X, REL_Y, REL_WHEEL, REL_HWHEEL],
                absbits: vec![],
            },
        },
    ]
}

/// Whether the session publishes its keyboard and pointer: only when asked
/// (`publish_input_devices`), and never under host capture (`wayland_host_display` set),
/// where the session belongs to a compositor reading the kernel's own devices.
pub fn publish_wanted(enabled: bool, wayland_host_display: &str) -> bool {
    enabled && wayland_host_display.trim().is_empty()
}

/// One keyboard or pointer the session would publish, where no backend serves a device: no
/// kernel node and no interposer socket ever comes up, so nothing is emitted onto it.
pub struct VirtualInputDevice;

impl VirtualInputDevice {
    /// A published device from its spec; no backend is opened.
    pub fn from_spec(_spec: &DeviceSpec, _sock_dir: &Path) -> Arc<VirtualInputDevice> {
        Arc::new(VirtualInputDevice)
    }

    /// Bring the device up; reports that no backend could, the answer a Linux host whose
    /// kernel refuses uinput and whose interposer pool is absent gives.
    pub async fn open(self: &Arc<Self>, _kernel: bool) -> bool {
        false
    }

    /// No device is up, so no event reaches one.
    pub fn emit(&self, _ev_type: u16, _code: u16, _value: i32) {}
}
