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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::res::ConnectionExt as _;
use x11rb::protocol::xf86vidmode::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, InputFocus, PropMode,
    Rectangle, SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, Window, WindowClass,
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
    window: Window,
    lost: Arc<AtomicBool>,
    _worker: Worker,
}

/// Own CLIPBOARD on `display` with a window of `connection`'s; that window.
fn own_clipboard(connection: &RustConnection, root: Window) -> Option<Window> {
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
    connection.flush().ok()?;
    Some(window)
}

/// How much of a selection's data one `ChangeProperty` carries: the request's length field is
/// 16 bits without BIG-REQUESTS, so a larger payload is appended in pieces.
const PROPERTY_CHUNK: usize = 60_000;

impl SelectionOwner {
    /// Own CLIPBOARD on `display` and answer no conversion request, as a browser being torn down
    /// does.
    ///
    /// # Panics
    ///
    /// When the display does not answer.
    pub fn silent(display: &str) -> SelectionOwner {
        let (connection, root) = open(display).expect("the X display, for a selection owner");
        let window = own_clipboard(&connection, root).expect("CLIPBOARD, for a silent owner");
        // The connection is the ownership: it lives on a thread that only waits to be ended.
        SelectionOwner {
            window,
            lost: Arc::new(AtomicBool::new(false)),
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
        SelectionOwner::offering(display, &[], Some(payload))
            .expect("CLIPBOARD, for an owner that answers")
    }

    /// Own CLIPBOARD on `display` as an application does: `formats` are the targets offered, each
    /// answered with its own bytes, and `fallback`, where given, answers any other target,
    /// `UTF8_STRING` offered for it; without one another target is refused.
    ///
    /// # Errors
    ///
    /// When the display does not answer or the selection cannot be taken.
    pub fn offering(
        display: &str,
        formats: &[(String, Vec<u8>)],
        fallback: Option<&[u8]>,
    ) -> Result<SelectionOwner, String> {
        let (connection, root) =
            open(display).ok_or_else(|| format!("{display} did not answer"))?;
        let window = own_clipboard(&connection, root).ok_or("CLIPBOARD could not be taken")?;
        let atom = |name: &[u8]| {
            connection
                .intern_atom(false, name)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .map_or(0, |reply| reply.atom)
        };
        let (targets, utf8) = (atom(b"TARGETS"), atom(b"UTF8_STRING"));
        let offered: Vec<(u32, Vec<u8>)> = formats
            .iter()
            .map(|(name, data)| (atom(name.as_bytes()), data.clone()))
            .collect();
        let mut listed = vec![targets];
        if fallback.is_some() {
            listed.push(utf8);
        }
        listed.extend(offered.iter().map(|(target, _)| *target));
        let fallback = fallback.map(<[u8]>::to_vec);
        let lost = Arc::new(AtomicBool::new(false));
        let lost_flag = Arc::clone(&lost);
        let worker = Worker::start(move |stop| {
            while !stop.load(Ordering::Relaxed) {
                let Ok(Some(event)) = connection.poll_for_event() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                let request = match event {
                    Event::SelectionClear(_) => {
                        lost_flag.store(true, Ordering::Relaxed);
                        continue;
                    }
                    Event::SelectionRequest(request) => request,
                    _ => continue,
                };
                // An obsolete requestor names no property: the target is the property then.
                let property = if request.property == u32::from(AtomEnum::NONE) {
                    request.target
                } else {
                    request.property
                };
                let data = offered
                    .iter()
                    .find(|(target, _)| *target == request.target)
                    .map(|(_, data)| data)
                    .or(fallback.as_ref());
                let answered = if request.target == targets {
                    connection
                        .change_property32(
                            PropMode::REPLACE,
                            request.requestor,
                            property,
                            AtomEnum::ATOM,
                            &listed,
                        )
                        .is_ok()
                } else if let Some(data) = data {
                    let mut pieces = data.chunks(PROPERTY_CHUNK);
                    let first = pieces.next().unwrap_or(&[]);
                    connection
                        .change_property8(
                            PropMode::REPLACE,
                            request.requestor,
                            property,
                            request.target,
                            first,
                        )
                        .is_ok()
                        && pieces.all(|piece| {
                            connection
                                .change_property8(
                                    PropMode::APPEND,
                                    request.requestor,
                                    property,
                                    request.target,
                                    piece,
                                )
                                .is_ok()
                        })
                } else {
                    false
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
                        property: if answered {
                            property
                        } else {
                            u32::from(AtomEnum::NONE)
                        },
                    },
                );
                let _ = connection.flush();
            }
        });
        Ok(SelectionOwner {
            window,
            lost,
            _worker: worker,
        })
    }

    /// Whether this owner still holds CLIPBOARD on `display`: nothing took it since.
    #[must_use]
    pub fn owns(&self, display: &str) -> bool {
        if self.lost.load(Ordering::Relaxed) {
            return false;
        }
        let asked = || -> Option<bool> {
            let (connection, _) = open(display)?;
            let clipboard = connection
                .intern_atom(false, b"CLIPBOARD")
                .ok()?
                .reply()
                .ok()?
                .atom;
            let owner = connection
                .get_selection_owner(clipboard)
                .ok()?
                .reply()
                .ok()?
                .owner;
            Some(owner == self.window)
        };
        asked().unwrap_or(false)
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
    // The server answers a client that states no version in the extension's first layout.
    connection.xf86vidmode_set_client_version(2, 2).ok()?;
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

/// The root window's CARDINAL property `name` on `display`: its first value, `None` where the
/// property is absent or the display does not answer.
pub fn root_cardinal(display: &str, name: &str) -> Option<u32> {
    let (connection, root) = open(display)?;
    let atom = connection
        .intern_atom(false, name.as_bytes())
        .ok()?
        .reply()
        .ok()?
        .atom;
    let reply = connection
        .get_property(false, root, atom, AtomEnum::CARDINAL, 0, 1)
        .ok()?
        .reply()
        .ok()?;
    reply.value32()?.next()
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

/// The first keycode whose keysyms carry `keysym` on `connection`'s keyboard.
fn keycode_on(connection: &RustConnection, keysym: u32) -> Option<u8> {
    let setup = connection.setup();
    let (first, last) = (setup.min_keycode, setup.max_keycode);
    let mapping = connection
        .get_keyboard_mapping(first, last - first + 1)
        .ok()?
        .reply()
        .ok()?;
    let per = usize::from(mapping.keysyms_per_keycode).max(1);
    let index = mapping
        .keysyms
        .chunks(per)
        .position(|syms| syms.contains(&keysym))?;
    first.checked_add(u8::try_from(index).ok()?)
}

/// The keycode that types `keysym` on `display`: `None` when no keycode carries it or the
/// display does not answer.
pub fn keycode(display: &str, keysym: u32) -> Option<u8> {
    let (connection, _) = open(display)?;
    keycode_on(&connection, keysym)
}

/// Whether the key that types `keysym` is held on `display`, read from the keymap the server
/// reports: `None` when no keycode carries the keysym or the display does not answer.
pub fn key_held(display: &str, keysym: u32) -> Option<bool> {
    let (connection, _) = open(display)?;
    let code = usize::from(keycode_on(&connection, keysym)?);
    let keys = connection.query_keymap().ok()?.reply().ok()?.keys;
    Some(keys.get(code / 8)? & (1 << (code % 8)) != 0)
}

/// The core pointer buttons `display` reports held, lowest first: the pointer's modifier mask
/// carries Button1Mask at bit 8, so a held drag shows without asking the client what it sent.
/// `None` when the display does not answer.
pub fn buttons_held(display: &str) -> Option<Vec<u8>> {
    let (connection, root) = open(display)?;
    let mask = u16::from(connection.query_pointer(root).ok()?.reply().ok()?.mask);
    Some((1..=5u8).filter(|b| mask & (1 << (7 + b)) != 0).collect())
}

/// An event an [`InputWatch`] saw.
#[derive(Clone, Debug)]
pub struct InputEvent {
    /// `KeyPress`, `KeyRelease`, `ButtonPress`, `ButtonRelease` or `MotionNotify`.
    pub kind: &'static str,
    /// The keycode or button; 0 for a motion.
    pub detail: u8,
    /// Where a motion went, in the window; 0 for the rest.
    pub x: i16,
    pub y: i16,
    /// Where the pointer was on the root window as the event happened.
    pub root_x: i16,
    pub root_y: i16,
    /// When the event was read, in seconds on `CLOCK_MONOTONIC`.
    pub at: f64,
}

/// Seconds on `CLOCK_MONOTONIC`, the clock a test process stamps its own steps on.
#[must_use]
pub fn monotonic() -> f64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a valid timespec for the call to fill.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut now) };
    #[expect(
        clippy::cast_precision_loss,
        reason = "seconds since boot fit an f64 to the nanosecond"
    )]
    let seconds = now.tv_sec as f64 + now.tv_nsec as f64 / 1e9;
    seconds
}

/// An override-redirect window that records the keys, buttons and motion it receives, the
/// keyboard focus following the pointer onto it: 200 by 200 in the middle of a display, or
/// covering it, at any size a resize gives it, so the pointer is over it wherever it goes. A
/// plain root event mask is refused: the streaming server holds the root's. Dropping it takes
/// the window down with its connection.
pub struct InputWatch {
    events: Arc<Mutex<Vec<InputEvent>>>,
    _worker: Worker,
}

impl InputWatch {
    /// Map the window on `display`, covering it when `cover`, and record from now on.
    ///
    /// # Errors
    ///
    /// When the display does not answer or refuses the window.
    pub fn start(display: &str, cover: bool) -> Result<InputWatch, String> {
        let (connection, root) =
            open(display).ok_or_else(|| format!("{display} did not answer"))?;
        let (width, height) = {
            let screen = connection
                .setup()
                .roots
                .iter()
                .find(|s| s.root == root)
                .ok_or("no screen")?;
            (screen.width_in_pixels, screen.height_in_pixels)
        };
        let id = connection.generate_id().map_err(|e| e.to_string())?;
        let centre = |side: u16| i16::try_from(side / 2).unwrap_or(0) - 100;
        let (x, y, w, h) = if cover {
            (0, 0, 8192, 4096)
        } else {
            (centre(width), centre(height), 200, 200)
        };
        connection
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                id,
                root,
                x,
                y,
                w,
                h,
                0,
                WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new().override_redirect(1).event_mask(
                    EventMask::KEY_PRESS
                        | EventMask::KEY_RELEASE
                        | EventMask::BUTTON_PRESS
                        | EventMask::BUTTON_RELEASE
                        | EventMask::POINTER_MOTION,
                ),
            )
            .map_err(|e| e.to_string())?;
        connection.map_window(id).map_err(|e| e.to_string())?;
        // PointerRoot: the keys go to the window under the pointer.
        let pointer_root = u32::from(u8::from(InputFocus::POINTER_ROOT));
        connection
            .set_input_focus(InputFocus::NONE, pointer_root, x11rb::CURRENT_TIME)
            .map_err(|e| e.to_string())?;
        connection.sync().map_err(|e| e.to_string())?;
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let worker = Worker::start(move |stop| {
            while !stop.load(Ordering::Relaxed) {
                let Ok(Some(event)) = connection.poll_for_event() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                let seen = match event {
                    Event::KeyPress(e) => InputEvent {
                        kind: "KeyPress",
                        detail: e.detail,
                        x: 0,
                        y: 0,
                        root_x: e.root_x,
                        root_y: e.root_y,
                        at: monotonic(),
                    },
                    Event::KeyRelease(e) => InputEvent {
                        kind: "KeyRelease",
                        detail: e.detail,
                        x: 0,
                        y: 0,
                        root_x: e.root_x,
                        root_y: e.root_y,
                        at: monotonic(),
                    },
                    Event::ButtonPress(e) => InputEvent {
                        kind: "ButtonPress",
                        detail: e.detail,
                        x: 0,
                        y: 0,
                        root_x: e.root_x,
                        root_y: e.root_y,
                        at: monotonic(),
                    },
                    Event::ButtonRelease(e) => InputEvent {
                        kind: "ButtonRelease",
                        detail: e.detail,
                        x: 0,
                        y: 0,
                        root_x: e.root_x,
                        root_y: e.root_y,
                        at: monotonic(),
                    },
                    Event::MotionNotify(e) => InputEvent {
                        kind: "MotionNotify",
                        detail: 0,
                        x: e.event_x,
                        y: e.event_y,
                        root_x: e.root_x,
                        root_y: e.root_y,
                        at: monotonic(),
                    },
                    _ => continue,
                };
                if let Ok(mut list) = sink.lock() {
                    list.push(seen);
                }
            }
            drop(connection);
        });
        Ok(InputWatch {
            events,
            _worker: worker,
        })
    }

    /// What the window has received so far, oldest first.
    pub fn events(&self) -> Vec<InputEvent> {
        self.events
            .lock()
            .map(|list| list.clone())
            .unwrap_or_default()
    }
}

/// Where the pointer is on `display`'s root, `None` when the display does not answer.
pub fn pointer(display: &str) -> Option<(i16, i16)> {
    let (connection, root) = open(display)?;
    let reply = connection.query_pointer(root).ok()?.reply().ok()?;
    Some((reply.root_x, reply.root_y))
}

/// The CLIPBOARD's text on `display` as its owner converts it to `UTF8_STRING`, waiting up to
/// `timeout` for the answer: `None` when there is no owner, it refuses, or it does not answer.
pub fn read_clipboard(display: &str, timeout: Duration) -> Option<String> {
    convert_clipboard(display, "UTF8_STRING", timeout)
        .map(|value| String::from_utf8_lossy(&value).into_owned())
}

/// The CLIPBOARD on `display` as its owner converts it to `target`, in one conversion on a
/// connection of its own as a paste makes, waiting up to `timeout`: `None` when there is no
/// owner, it refuses, or it does not answer.
pub fn convert_clipboard(display: &str, target: &str, timeout: Duration) -> Option<Vec<u8>> {
    let (connection, root) = open(display)?;
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
    let atom = |name: &[u8]| -> Option<u32> {
        Some(connection.intern_atom(false, name).ok()?.reply().ok()?.atom)
    };
    let (clipboard, wanted, probe) = (
        atom(b"CLIPBOARD")?,
        atom(target.as_bytes())?,
        atom(b"SELKIES_PROBE")?,
    );
    connection
        .convert_selection(window, clipboard, wanted, probe, x11rb::CURRENT_TIME)
        .ok()?;
    connection.flush().ok()?;
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        match connection.poll_for_event() {
            Ok(Some(Event::SelectionNotify(notify))) => {
                if notify.property == u32::from(AtomEnum::NONE) {
                    return None;
                }
                let value = connection
                    .get_property(
                        false,
                        window,
                        notify.property,
                        AtomEnum::ANY,
                        0,
                        u32::MAX / 4,
                    )
                    .ok()?
                    .reply()
                    .ok()?
                    .value;
                return Some(value);
            }
            Ok(Some(_)) => {}
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return None,
        }
    }
    None
}

/// The targets CLIPBOARD's owner on `display` offers, by name, as its answer to `TARGETS` lists
/// them: `None` when there is no owner, it refuses, or it does not answer within `timeout`.
pub fn clipboard_targets(display: &str, timeout: Duration) -> Option<Vec<String>> {
    let value = convert_clipboard(display, "TARGETS", timeout)?;
    let (connection, _) = open(display)?;
    Some(
        value
            .as_chunks::<4>()
            .0
            .iter()
            .filter_map(|atom| {
                let atom = u32::from_ne_bytes(*atom);
                let name = connection.get_atom_name(atom).ok()?.reply().ok()?.name;
                Some(String::from_utf8_lossy(&name).into_owned())
            })
            .collect(),
    )
}

/// The window manager managing `display`: its process id, 0 when none is known, and the name it
/// advertises, as its check window (`_NET_SUPPORTING_WM_CHECK`) carries them. A manager that sets
/// no `_NET_WM_PID` (Openbox) is found through what the X-Resource extension knows of the client
/// owning that window. `None` when no manager runs or the display does not answer.
pub fn window_manager(display: &str) -> Option<(u32, String)> {
    let (connection, root) = open(display)?;
    let atom = |name: &[u8]| -> Option<u32> {
        Some(connection.intern_atom(false, name).ok()?.reply().ok()?.atom)
    };
    let property = |window: Window, name: u32, kind: u32| {
        connection
            .get_property(false, window, name, kind, 0, 1024)
            .ok()?
            .reply()
            .ok()
    };
    let check = property(
        root,
        atom(b"_NET_SUPPORTING_WM_CHECK")?,
        AtomEnum::ANY.into(),
    )?
    .value32()?
    .next()?;
    let named = property(check, atom(b"_NET_WM_NAME")?, atom(b"UTF8_STRING")?)
        .map(|reply| String::from_utf8_lossy(&reply.value).into_owned())
        .unwrap_or_default();
    let pid = property(check, atom(b"_NET_WM_PID")?, AtomEnum::ANY.into())
        .and_then(|reply| reply.value32()?.next())
        .filter(|pid| *pid != 0)
        .or_else(|| {
            connection
                .extension_information(x11rb::protocol::res::X11_EXTENSION_NAME)
                .ok()??;
            let mask = x11rb::protocol::res::ClientIdMask::LOCAL_CLIENT_PID;
            let reply = connection
                .res_query_client_ids(&[x11rb::protocol::res::ClientIdSpec {
                    client: check,
                    mask,
                }])
                .ok()?
                .reply()
                .ok()?;
            reply
                .ids
                .iter()
                .find(|id| u32::from(id.spec.mask) & u32::from(mask) != 0)
                .and_then(|id| id.value.first().copied())
        })
        .unwrap_or(0);
    Some((pid, named))
}

/// Solid windows mapped on a display, override-redirect so no manager moves them, and one that
/// flips between two colors on a period, for a capture that has to keep sending; dropping it
/// takes them all down with its connection.
pub struct Painting {
    _worker: Worker,
}

/// A window's rectangle and its color (0x00RRGGBB).
#[derive(Clone, Copy, Debug)]
pub struct Paint {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub color: u32,
}

impl Painting {
    /// Map `paints` on `display`, and when `flicker` names one, a window that flips between its
    /// color and `second` every `period`.
    ///
    /// # Errors
    ///
    /// When the display does not answer or refuses a window.
    pub fn start(
        display: &str,
        paints: &[Paint],
        flicker: Option<(Paint, u32, Duration)>,
    ) -> Result<Painting, String> {
        let (connection, root) =
            open(display).ok_or_else(|| format!("{display} did not answer"))?;
        let window = |paint: &Paint| -> Result<Window, String> {
            let id = connection.generate_id().map_err(|e| e.to_string())?;
            connection
                .create_window(
                    x11rb::COPY_DEPTH_FROM_PARENT,
                    id,
                    root,
                    paint.x,
                    paint.y,
                    paint.width,
                    paint.height,
                    0,
                    WindowClass::INPUT_OUTPUT,
                    x11rb::COPY_FROM_PARENT,
                    &CreateWindowAux::new()
                        .background_pixel(paint.color)
                        .override_redirect(1),
                )
                .map_err(|e| e.to_string())?;
            connection.map_window(id).map_err(|e| e.to_string())?;
            Ok(id)
        };
        for paint in paints {
            window(paint)?;
        }
        let flipping = flicker
            .map(|(paint, second, period)| {
                window(&paint).map(|id| (id, [paint.color, second], period))
            })
            .transpose()?;
        connection.sync().map_err(|e| e.to_string())?;
        Ok(Painting {
            _worker: Worker::start(move |stop| {
                let mut flips = 0usize;
                while !stop.load(Ordering::Relaxed) {
                    let Some((id, colors, period)) = flipping else {
                        std::thread::sleep(Duration::from_millis(20));
                        continue;
                    };
                    std::thread::sleep(period);
                    flips += 1;
                    let _ = connection.change_window_attributes(
                        id,
                        &x11rb::protocol::xproto::ChangeWindowAttributesAux::new()
                            .background_pixel(colors[flips % 2]),
                    );
                    let _ = connection.clear_area(false, id, 0, 0, 0, 0);
                    let _ = connection.sync();
                }
                drop(connection);
            }),
        })
    }
}

/// Continuous damage on a display: a window drawing a moving block thirty times a second, for a
/// suite that needs the capture to keep sending. Dropping it destroys the window, which repaints
/// the root once and then leaves the screen still.
pub struct Churn {
    _worker: Worker,
}

impl Churn {
    /// Map a `width` by `height` window at the origin and start drawing; returns once frames are
    /// flowing.
    ///
    /// # Errors
    ///
    /// When the display does not answer or refuses the window.
    pub fn start(display: &str, width: u16, height: u16) -> Result<Churn, String> {
        let (connection, root) =
            open(display).ok_or_else(|| format!("{display} did not answer"))?;
        let black = connection
            .setup()
            .roots
            .iter()
            .find(|s| s.root == root)
            .map_or(0, |s| s.black_pixel);
        let id = connection.generate_id().map_err(|e| e.to_string())?;
        connection
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                id,
                root,
                0,
                0,
                width,
                height,
                0,
                WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new()
                    .background_pixel(black)
                    .override_redirect(1),
            )
            .map_err(|e| e.to_string())?;
        connection.map_window(id).map_err(|e| e.to_string())?;
        let mut gcs = Vec::new();
        for color in [0x00E0_5070u32, 0x0040_D080, 0x0040_80E0] {
            let gc = connection.generate_id().map_err(|e| e.to_string())?;
            connection
                .create_gc(gc, id, &CreateGCAux::new().foreground(color))
                .map_err(|e| e.to_string())?;
            gcs.push(gc);
        }
        connection.sync().map_err(|e| e.to_string())?;
        let worker = Worker::start(move |stop| {
            let span = |side: u16, block: u16| i32::from(side.saturating_sub(block)).max(1);
            let mut step: i32 = 0;
            while !stop.load(Ordering::Relaxed) {
                let _ = connection.clear_area(false, id, 0, 0, width, height);
                let x = i16::try_from((step * 23) % span(width, 300)).unwrap_or(0);
                let y = i16::try_from((step * 11) % span(height, 200)).unwrap_or(0);
                let gc = gcs[usize::try_from(step).unwrap_or(0) % gcs.len()];
                let _ = connection.poly_fill_rectangle(
                    id,
                    gc,
                    &[Rectangle {
                        x,
                        y,
                        width: 300,
                        height: 200,
                    }],
                );
                let _ = connection.flush();
                step = step.wrapping_add(1);
                std::thread::sleep(Duration::from_millis(33));
            }
            let _ = connection.destroy_window(id);
            let _ = connection.sync();
        });
        std::thread::sleep(Duration::from_millis(500));
        Ok(Churn { _worker: worker })
    }
}

/// A plain titled window for a window manager to take: the manager reparents it into a frame of
/// its own, which is where a drag by the titlebar moves it. Dropping it destroys the window.
pub struct ManagedProbe {
    connection: RustConnection,
    root: Window,
    id: Window,
}

impl ManagedProbe {
    /// Map a white `width` by `height` window named `name` on `display`.
    ///
    /// # Errors
    ///
    /// When the display does not answer or refuses the window.
    pub fn start(
        display: &str,
        name: &str,
        width: u16,
        height: u16,
    ) -> Result<ManagedProbe, String> {
        let (connection, root) =
            open(display).ok_or_else(|| format!("{display} did not answer"))?;
        let white = connection
            .setup()
            .roots
            .iter()
            .find(|s| s.root == root)
            .map_or(0x00FF_FFFF, |s| s.white_pixel);
        let id = connection.generate_id().map_err(|e| e.to_string())?;
        connection
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                id,
                root,
                0,
                0,
                width,
                height,
                0,
                WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &CreateWindowAux::new()
                    .background_pixel(white)
                    .event_mask(EventMask::STRUCTURE_NOTIFY),
            )
            .map_err(|e| e.to_string())?;
        connection
            .change_property8(
                PropMode::REPLACE,
                id,
                AtomEnum::WM_NAME,
                AtomEnum::STRING,
                name.as_bytes(),
            )
            .map_err(|e| e.to_string())?;
        connection.map_window(id).map_err(|e| e.to_string())?;
        connection.sync().map_err(|e| e.to_string())?;
        Ok(ManagedProbe {
            connection,
            root,
            id,
        })
    }

    /// The window's X id.
    #[must_use]
    pub fn id(&self) -> Window {
        self.id
    }

    /// The frame a window manager put the window in, as `[x, y, width, height]` on the root;
    /// `None` while the window is still the root's own child.
    #[must_use]
    pub fn frame(&self) -> Option<[i32; 4]> {
        let parent = self
            .connection
            .query_tree(self.id)
            .ok()?
            .reply()
            .ok()?
            .parent;
        if parent == self.root {
            return None;
        }
        let geometry = self.connection.get_geometry(parent).ok()?.reply().ok()?;
        Some([
            i32::from(geometry.x),
            i32::from(geometry.y),
            i32::from(geometry.width),
            i32::from(geometry.height),
        ])
    }
}

impl Drop for ManagedProbe {
    fn drop(&mut self) {
        let _ = self.connection.destroy_window(self.id);
        let _ = self.connection.sync();
    }
}

/// Put the pointer at (`x`, `y`) on `display`'s root; whether the display answered.
pub fn warp(display: &str, x: i16, y: i16) -> bool {
    let warped = || -> Option<()> {
        let (connection, root) = open(display)?;
        connection
            .warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, x, y)
            .ok()?;
        connection.sync().ok()
    };
    warped().is_some()
}

/// Press or release pointer `button` on `display` through XTEST, as a real mouse would; whether
/// the display answered.
pub fn fake_button(display: &str, button: u8, press: bool) -> bool {
    use x11rb::protocol::xproto::{BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT};
    use x11rb::protocol::xtest::ConnectionExt as _;
    let faked = || -> Option<()> {
        let (connection, _) = open(display)?;
        let kind = if press {
            BUTTON_PRESS_EVENT
        } else {
            BUTTON_RELEASE_EVENT
        };
        connection
            .xtest_fake_input(kind, button, x11rb::CURRENT_TIME, x11rb::NONE, 0, 0, 0)
            .ok()?;
        connection.sync().ok()
    };
    faked().is_some()
}

/// Press or release the key that types `keysym` on `display` through XTEST, as a real keyboard
/// would; whether the display answered and has a keycode for it.
pub fn fake_key(display: &str, keysym: u32, press: bool) -> bool {
    use x11rb::protocol::xproto::{KEY_PRESS_EVENT, KEY_RELEASE_EVENT};
    use x11rb::protocol::xtest::ConnectionExt as _;
    let faked = || -> Option<()> {
        let (connection, _) = open(display)?;
        let code = keycode_on(&connection, keysym)?;
        let kind = if press {
            KEY_PRESS_EVENT
        } else {
            KEY_RELEASE_EVENT
        };
        connection
            .xtest_fake_input(kind, code, x11rb::CURRENT_TIME, x11rb::NONE, 0, 0, 0)
            .ok()?;
        connection.sync().ok()
    };
    faked().is_some()
}

/// Move the pointer on `display` by (`dx`, `dy`) `count` times through XTEST, relative motion as
/// a mouse makes it, `gap` apart; whether the display answered.
pub fn fake_motion(display: &str, dx: i16, dy: i16, count: u32, gap: Duration) -> bool {
    use x11rb::protocol::xproto::MOTION_NOTIFY_EVENT;
    use x11rb::protocol::xtest::ConnectionExt as _;
    let faked = || -> Option<()> {
        let (connection, _) = open(display)?;
        for _ in 0..count {
            // A detail of 1 makes the motion relative.
            connection
                .xtest_fake_input(
                    MOTION_NOTIFY_EVENT,
                    1,
                    x11rb::CURRENT_TIME,
                    x11rb::NONE,
                    dx,
                    dy,
                    0,
                )
                .ok()?;
            connection.flush().ok()?;
            if !gap.is_zero() {
                std::thread::sleep(gap);
            }
        }
        connection.sync().ok()
    };
    faked().is_some()
}
