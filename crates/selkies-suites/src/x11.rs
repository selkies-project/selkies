/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What a suite reads back from an X display and puts on it.
//!
//! Every function names its display: nothing here reads `DISPLAY`, which on a developer's host
//! is a desktop in use. A read opens a connection of its own and closes it, since a size rides
//! the connection handshake and an X server allows a bounded number of clients.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::xf86vidmode::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, PropMode, Rectangle,
    SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// A connection to `display` and its root window.
fn open(display: &str) -> Option<(RustConnection, Window)> {
    let (connection, screen) = RustConnection::connect(Some(display)).ok()?;
    let root = connection.setup().roots.get(screen)?.root;
    Some((connection, root))
}

/// The root's size on `display`, `None` when the display does not answer.
pub fn root_size(display: &str) -> Option<(u16, u16)> {
    let (connection, screen) = RustConnection::connect(Some(display)).ok()?;
    let screen = connection.setup().roots.get(screen)?;
    Some((screen.width_in_pixels, screen.height_in_pixels))
}

/// Whether `display` offers outputs a display can be plugged in as: an output beside the first
/// connected one that carries the `Connected` property the images' Xvfb gives its spare outputs.
/// False on any failure, as the server reads it.
pub fn has_pluggable_outputs(display: &str) -> bool {
    let probe = || -> Option<bool> {
        let (connection, root) = open(display)?;
        let resources = connection
            .randr_get_screen_resources(root)
            .ok()?
            .reply()
            .ok()?;
        let primary = resources.outputs.iter().copied().find(|output| {
            connection
                .randr_get_output_info(*output, resources.config_timestamp)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .is_some_and(|info| info.connection == randr::Connection::CONNECTED)
        })?;
        let connected = connection
            .intern_atom(false, b"Connected")
            .ok()?
            .reply()
            .ok()?
            .atom;
        Some(resources.outputs.iter().any(|output| {
            *output != primary
                && connection
                    .randr_list_output_properties(*output)
                    .ok()
                    .and_then(|cookie| cookie.reply().ok())
                    .is_some_and(|properties| properties.atoms.contains(&connected))
        }))
    };
    probe().unwrap_or(false)
}

/// A thread working on an X display until its owner is dropped.
struct Worker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    /// Run `work` on a thread of its own, handing it the flag that tells it to end.
    fn start(work: impl FnOnce(&AtomicBool) + Send + 'static) -> Worker {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        Worker {
            stop,
            thread: Some(std::thread::spawn(move || work(&flag))),
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// An owner of CLIPBOARD on a display. Dropping it releases the selection: a connection left
/// open would keep a window the owner with nobody answering, and wedge the next server's read.
pub struct SelectionOwner {
    _worker: Worker,
}

/// Own CLIPBOARD on `display` with a window of `connection`'s.
fn own_clipboard(connection: &RustConnection, root: Window) -> Option<()> {
    let window = connection.generate_id().ok()?;
    connection
        .create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new(),
        )
        .ok()?;
    let clipboard = connection
        .intern_atom(false, b"CLIPBOARD")
        .ok()?
        .reply()
        .ok()?
        .atom;
    connection
        .set_selection_owner(window, clipboard, x11rb::CURRENT_TIME)
        .ok()?;
    connection.flush().ok()
}

impl SelectionOwner {
    /// Own CLIPBOARD on `display` and answer no conversion request, as a browser being torn down
    /// does.
    ///
    /// # Panics
    ///
    /// When the display does not answer.
    pub fn silent(display: &str) -> SelectionOwner {
        let (connection, root) = open(display).expect("the X display, for a selection owner");
        own_clipboard(&connection, root).expect("CLIPBOARD, for a silent owner");
        // The connection is the ownership: it lives on a thread that only waits to be ended.
        SelectionOwner {
            _worker: Worker::start(move |stop| {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(20));
                }
                drop(connection);
            }),
        }
    }

    /// Own CLIPBOARD on `display` and hand `payload` to any requestor, offering `UTF8_STRING`.
    ///
    /// # Panics
    ///
    /// When the display does not answer.
    pub fn serving(display: &str, payload: &[u8]) -> SelectionOwner {
        let (connection, root) = open(display).expect("the X display, for a selection owner");
        own_clipboard(&connection, root).expect("CLIPBOARD, for an owner that answers");
        let atom = |name: &[u8]| {
            connection
                .intern_atom(false, name)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .map_or(0, |reply| reply.atom)
        };
        let (targets, utf8) = (atom(b"TARGETS"), atom(b"UTF8_STRING"));
        let payload = payload.to_vec();
        let worker = Worker::start(move |stop| {
            while !stop.load(Ordering::Relaxed) {
                let Ok(Some(event)) = connection.poll_for_event() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                let Event::SelectionRequest(request) = event else {
                    continue;
                };
                let _ = if request.target == targets {
                    connection.change_property32(
                        PropMode::REPLACE,
                        request.requestor,
                        request.property,
                        AtomEnum::ATOM,
                        &[utf8],
                    )
                } else {
                    connection.change_property8(
                        PropMode::REPLACE,
                        request.requestor,
                        request.property,
                        request.target,
                        &payload,
                    )
                };
                let _ = connection.send_event(
                    false,
                    request.requestor,
                    EventMask::NO_EVENT,
                    SelectionNotifyEvent {
                        response_type: SELECTION_NOTIFY_EVENT,
                        sequence: 0,
                        time: request.time,
                        requestor: request.requestor,
                        selection: request.selection,
                        target: request.target,
                        property: request.property,
                    },
                );
                let _ = connection.flush();
            }
        });
        SelectionOwner { _worker: worker }
    }
}

/// Continuous damage on a display: a window drawing a moving block thirty times a second, for a
/// suite that needs the capture to keep sending. Dropping it destroys the window, which repaints
/// the root once and then leaves the screen still.
pub struct Damage {
    _worker: Worker,
}

impl Damage {
    /// Map the window on `display` and start drawing.
    ///
    /// # Panics
    ///
    /// When the display does not answer.
    pub fn start(display: &str) -> Damage {
        const SIZE: (u16, u16) = (900, 700);
        const BLOCK: (u16, u16) = (300, 200);
        let (connection, screen) =
            RustConnection::connect(Some(display)).expect("the X display, for a damage source");
        let screen = &connection.setup().roots[screen];
        let (root, black) = (screen.root, screen.black_pixel);
        let window = connection.generate_id().expect("a window id");
        let drawn = || -> Result<Vec<u32>, Box<dyn std::error::Error>> {
            connection.create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                window,
                root,
                0,
                0,
                SIZE.0,
                SIZE.1,
                0,
                WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new()
                    .background_pixel(black)
                    .override_redirect(1),
            )?;
            connection.map_window(window)?;
            let mut colors = Vec::new();
            for color in [0x00E0_5070, 0x0040_D080, 0x0040_80E0] {
                let gc = connection.generate_id()?;
                connection.create_gc(gc, window, &CreateGCAux::new().foreground(color))?;
                colors.push(gc);
            }
            connection.flush()?;
            Ok(colors)
        };
        let colors = drawn().expect("the damage window");
        let worker = Worker::start(move |stop| {
            let mut step: u16 = 0;
            while !stop.load(Ordering::Relaxed) {
                let x = (u32::from(step) * 23) % u32::from(SIZE.0 - BLOCK.0);
                let y = (u32::from(step) * 11) % u32::from(SIZE.1 - BLOCK.1);
                let _ = connection.clear_area(false, window, 0, 0, SIZE.0, SIZE.1);
                let _ = connection.poly_fill_rectangle(
                    window,
                    colors[usize::from(step % 3)],
                    &[Rectangle {
                        x: i16::try_from(x).unwrap_or(0),
                        y: i16::try_from(y).unwrap_or(0),
                        width: BLOCK.0,
                        height: BLOCK.1,
                    }],
                );
                let _ = connection.flush();
                step = step.wrapping_add(1);
                std::thread::sleep(Duration::from_millis(33));
            }
            let _ = connection.destroy_window(window);
            let _ = connection.flush();
        });
        Damage { _worker: worker }
    }
}

/// The mode line XFree86-VidModeExtension reports for a display's first screen.
pub struct ModeLine {
    /// The dot clock, in kHz.
    pub dot_clock: u32,
    pub hdisplay: u16,
    pub hsyncstart: u16,
    pub hsyncend: u16,
    pub htotal: u16,
    pub vdisplay: u16,
    pub vsyncstart: u16,
    pub vsyncend: u16,
    pub vtotal: u16,
}

/// The mode line of `display`, read on a fresh connection: `None` where the display does not
/// answer or lacks the extension.
pub fn mode_line(display: &str) -> Option<ModeLine> {
    let (connection, _) = open(display)?;
    connection
        .extension_information(xf86vidmode::X11_EXTENSION_NAME)
        .ok()??;
    let line = connection.xf86vidmode_get_mode_line(0).ok()?.reply().ok()?;
    Some(ModeLine {
        dot_clock: line.dotclock,
        hdisplay: line.hdisplay,
        hsyncstart: line.hsyncstart,
        hsyncend: line.hsyncend,
        htotal: line.htotal,
        vdisplay: line.vdisplay,
        vsyncstart: line.vsyncstart,
        vsyncend: line.vsyncend,
        vtotal: line.vtotal,
    })
}

/// Set the root window's CARDINAL property `name` on `display` to `value`, or delete it for
/// `None`; whether the server took it.
pub fn set_root_cardinal(display: &str, name: &str, value: Option<u32>) -> bool {
    let set = || -> Option<()> {
        let (connection, root) = open(display)?;
        let atom = connection
            .intern_atom(false, name.as_bytes())
            .ok()?
            .reply()
            .ok()?
            .atom;
        match value {
            Some(value) => connection
                .change_property32(PropMode::REPLACE, root, atom, AtomEnum::CARDINAL, &[value])
                .ok()?
                .check()
                .ok()?,
            None => connection.delete_property(root, atom).ok()?.check().ok()?,
        }
        Some(())
    };
    set().is_some()
}
