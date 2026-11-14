/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A Wayland app owning the clipboard, for the suites: it takes the selection of the compositor
//! on the socket it is given, offering each `--offer MIME VALUE` pair, serves every paste until
//! another client copies or its standard input closes, and then lets the selection go, as a
//! password manager clearing its copy does. It is a clipboard manager to the compositor
//! (`ext_data_control_manager_v1`, which Selkies' compositor, KWin and wlroots advertise), so it
//! needs no surface and no focus. It says `owned` on its standard output once the selection is
//! its own.
//!
//! Usage: wl-clipboard-owner SOCKET [--offer MIME VALUE]...
//! (SOCKET is a name under XDG_RUNTIME_DIR, or a path)

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1 as device, ext_data_control_manager_v1 as manager,
    ext_data_control_offer_v1 as offer, ext_data_control_source_v1 as source,
};

/// What the selection serves, and whether another client took it.
struct Owner {
    entries: Vec<(String, Vec<u8>)>,
    cancelled: bool,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Owner {
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

impl Dispatch<wl_seat::WlSeat, ()> for Owner {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<manager::ExtDataControlManagerV1, ()> for Owner {
    fn event(
        _: &mut Self,
        _: &manager::ExtDataControlManagerV1,
        _: manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<device::ExtDataControlDeviceV1, ()> for Owner {
    fn event(
        _: &mut Self,
        _: &device::ExtDataControlDeviceV1,
        _: device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }

    wayland_client::event_created_child!(Owner, device::ExtDataControlDeviceV1, [
        device::EVT_DATA_OFFER_OPCODE => (offer::ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<offer::ExtDataControlOfferV1, ()> for Owner {
    fn event(
        _: &mut Self,
        _: &offer::ExtDataControlOfferV1,
        _: offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<source::ExtDataControlSourceV1, ()> for Owner {
    fn event(
        state: &mut Self,
        _: &source::ExtDataControlSourceV1,
        event: source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            source::Event::Send { mime_type, fd } => {
                if let Some((_, data)) = state.entries.iter().find(|(mime, _)| *mime == mime_type) {
                    let _ = std::fs::File::from(fd).write_all(data);
                }
            }
            source::Event::Cancelled => state.cancelled = true,
            _ => {}
        }
    }
}

/// The socket the name or path given stands for.
fn socket_path(socket: &str) -> Result<PathBuf, String> {
    if socket.contains('/') {
        return Ok(PathBuf::from(socket));
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is unset")?;
    Ok(PathBuf::from(runtime).join(socket))
}

fn run(args: &[String]) -> Result<(), String> {
    let usage = "usage: wl-clipboard-owner SOCKET [--offer MIME VALUE]...";
    let (socket, rest) = args.split_first().ok_or(usage)?;
    let mut entries = Vec::new();
    for pair in rest.chunks(3) {
        match pair {
            [flag, mime, value] if flag == "--offer" => {
                entries.push((mime.clone(), value.clone().into_bytes()))
            }
            _ => return Err(usage.into()),
        }
    }
    let path = socket_path(socket)?;
    let stream = UnixStream::connect(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let conn = Connection::from_socket(stream).map_err(|e| e.to_string())?;
    let (globals, mut queue) = registry_queue_init::<Owner>(&conn).map_err(|e| e.to_string())?;
    let qh = queue.handle();
    let seat: wl_seat::WlSeat = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("wl_seat: {e}"))?;
    let manager: manager::ExtDataControlManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("ext_data_control_manager_v1: {e}"))?;
    let device = manager.get_data_device(&seat, &qh, ());
    let source = manager.create_data_source(&qh, ());
    for (mime, _) in &entries {
        source.offer(mime.clone());
    }
    device.set_selection(Some(&source));
    let mut owner = Owner {
        entries,
        cancelled: false,
    };
    queue.roundtrip(&mut owner).map_err(|e| e.to_string())?;
    println!("owned");
    let _ = std::io::stdout().flush();

    // Standard input closing lets the selection go; the compositor's events are served meanwhile.
    let (closed_tx, closed_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut sink = [0u8; 256];
        while matches!(std::io::stdin().read(&mut sink), Ok(n) if n > 0) {}
        let _ = closed_tx.send(());
    });
    let fd = conn
        .backend()
        .poll_fd()
        .try_clone_to_owned()
        .map_err(|e| e.to_string())?;
    let mut fds = [libc::pollfd {
        fd: std::os::fd::AsRawFd::as_raw_fd(&fd),
        events: libc::POLLIN,
        revents: 0,
    }];
    while !owner.cancelled && closed_rx.try_recv().is_err() {
        conn.flush().map_err(|e| e.to_string())?;
        if let Some(guard) = queue.prepare_read() {
            // SAFETY: one pollfd that outlives the call.
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), 1, 100) };
            if ready > 0 {
                guard.read().map_err(|e| e.to_string())?;
            }
        }
        queue
            .dispatch_pending(&mut owner)
            .map_err(|e| e.to_string())?;
    }
    if !owner.cancelled {
        device.set_selection(None);
        queue.roundtrip(&mut owner).map_err(|e| e.to_string())?;
    }
    source.destroy();
    device.destroy();
    conn.flush().map_err(|e| e.to_string())?;
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("wl-clipboard-owner: {message}");
            ExitCode::from(2)
        }
    }
}
