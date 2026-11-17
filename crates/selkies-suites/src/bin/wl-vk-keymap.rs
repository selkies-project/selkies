// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A Wayland client that hands the compositor a virtual-keyboard keymap whose size runs past its
//! file, for the suite that holds the session's compositor to ignoring it.
//!
//! ```text
//! wl-vk-keymap SOCKET   (VK_KEYMAP_SIZE: the size claimed, default 1 GiB)
//! ```
//!
//! The file holds a few bytes of keymap text; the request claims VK_KEYMAP_SIZE. Prints JSON
//! lines: `sent` once the request went out and a roundtrip came back after it, or `error` when the
//! compositor offers no virtual keyboard or the connection broke.

use std::io::{Seek, SeekFrom, Write};
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::process::ExitCode;

use selkies_suites::wayland::{self, emit};
use serde_json::Value;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1, zwp_virtual_keyboard_v1,
};

struct Client;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Client {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(Client: ignore wl_seat::WlSeat);
delegate_noop!(Client: zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1);
delegate_noop!(Client: zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1);

/// An anonymous file holding `text`.
fn keymap_file(text: &[u8]) -> Result<OwnedFd, String> {
    // SAFETY: memfd_create with a NUL-terminated name and no flags returns a new descriptor or -1.
    let raw = unsafe { libc::memfd_create(c"selkies-vk-keymap".as_ptr(), 0) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    // SAFETY: `raw` is a descriptor this call just opened and nothing else owns.
    let mut file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(raw) });
    file.write_all(text).map_err(|e| e.to_string())?;
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    Ok(OwnedFd::from(file))
}

fn run(socket: &str, size: u32) -> Result<(), (u8, String)> {
    let conn = wayland::connect(socket).map_err(|e| (3, e))?;
    let (globals, mut queue) =
        registry_queue_init::<Client>(&conn).map_err(|e| (3, e.to_string()))?;
    let qh = queue.handle();
    let seat = globals.bind::<wl_seat::WlSeat, _, _>(&qh, 1..=1, ());
    let manager = globals
        .bind::<zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1, _, _>(&qh, 1..=1, ());
    let (Ok(seat), Ok(manager)) = (seat, manager) else {
        return Err((2, "no seat or virtual keyboard manager".into()));
    };
    let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());
    let file = keymap_file(b"xkb_keymap {};\n").map_err(|e| (3, e))?;
    // Format 1 is xkb_v1.
    keyboard.keymap(1, file.as_fd(), size);
    let mut state = Client;
    for _ in 0..2 {
        queue
            .roundtrip(&mut state)
            .map_err(|e| (3, e.to_string()))?;
    }
    Ok(())
}

fn main() -> ExitCode {
    let socket = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "wayland-1".into());
    let size = std::env::var("VK_KEYMAP_SIZE")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(1 << 30);
    match run(&socket, size) {
        Ok(()) => {
            emit("sent", &[("size", Value::from(size))]);
            ExitCode::SUCCESS
        }
        Err((code, reason)) => {
            emit("error", &[("reason", Value::from(reason))]);
            ExitCode::from(code)
        }
    }
}
