/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Client bindings for the KDE output protocols, generated from the XMLs
//! vendored under `protocols/`.
//!
//! Vendored rather than taken from `wayland-protocols-plasma` because that
//! crate's bundled copies lag KDE, and the lag is fatal: KWin sends the mode
//! `flags` event to every client whatever version it bound — its guard
//! compares the bound version against the flag enum's `custom` value instead
//! of the event's since-version — and a wayland-rs client treats an event its
//! spec does not know as a malformed message, killing the connection mid-read.
//! The vendored XMLs know every event current KWin can leak; events KDE adds
//! later stay unsent behind the low versions `kdeclient` binds, unless they
//! too leak unguarded, which re-vendoring the XMLs cures.

/// `kde-output-device-v2.xml`: the per-output `kde_output_device_v2` globals
/// and their `kde_output_device_mode_v2` children.
#[allow(missing_docs, non_upper_case_globals, unused_imports, clippy::all)]
pub mod output_device {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("./protocols/kde-output-device-v2.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("./protocols/kde-output-device-v2.xml");
}

/// `kde-output-management-v2.xml`: `kde_output_management_v2` and the
/// configuration objects it creates against the output devices.
#[allow(missing_docs, non_upper_case_globals, unused_imports, clippy::all)]
pub mod output_management {
    use wayland_client;
    use wayland_client::protocol::*;

    use super::output_device::*;

    pub mod __interfaces {
        use super::super::output_device::__interfaces::*;
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("./protocols/kde-output-management-v2.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("./protocols/kde-output-management-v2.xml");
}
