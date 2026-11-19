/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! libei (EIS) input injection for the portal host-capture rung.
//!
//! GNOME and KDE hand a RemoteDesktop session an EIS socket through
//! `ConnectToEIS`; this drives keyboard, pointer, and touch over that socket
//! instead of the portal's `Notify*` D-Bus methods. It is the lower-latency
//! channel — one socket write per event, no bus round trip — and the one the
//! backends develop, so it is preferred wherever the portal offers it. The
//! frontend makes the choice permanent: once `ConnectToEIS` succeeds the
//! `Notify*` methods are refused for the life of the session, so the injector
//! is taken only after its handshake binds a usable device, and a portal that
//! never answers `ConnectToEIS` keeps the `Notify*` path (`portal.rs`).
//!
//! All socket I/O runs on one thread: it completes the handshake, binds the
//! seat's capabilities, tracks the devices the server creates, and emits queued
//! events, waking from a pipe the moment an event is queued so injection does
//! not wait on a poll tick. Keyboard keys arrive as xkb keycodes under selkies'
//! managed keymap; the compositor's own keymap (delivered over EIS) is
//! read-only here, so a key's base keysym is resolved against it and the
//! modifiers its level needs are synthesized, with a raw-keycode fallback for a
//! keysym that keymap does not carry.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use reis::PendingRequestResult;
use reis::ei;
use reis::event::{DeviceCapability, EiEvent, EiEventConverter};
use reis::handshake::ei_handshake_blocking;

use crate::wayland::keymap::{KeymapPolicy, compile_keymap};
use crate::wayland::wlclient::wake_pipe;

/// evdev keycodes carry an 8-count offset from xkb keycodes.
const EVDEV_OFFSET: u32 = 8;

/// The worker's one-shot readiness report to `spawn`: ready once a device bound, else the reason
/// the session yielded no injector.
type Ready = Arc<(Mutex<Option<Result<(), String>>>, std::sync::Condvar)>;

/// One injection request, produced on selkies' input threads and drained on the EIS thread.
enum Cmd {
    /// A point in the compositor's logical space, and where the host knows it, the point as a
    /// fraction of the one output under it (`HostSession::pointer_motion_abs`).
    MotionAbsolute {
        x: f32,
        y: f32,
        output_fraction: Option<(f64, f64)>,
    },
    MotionRelative {
        dx: f32,
        dy: f32,
    },
    Button {
        button: u32,
        pressed: bool,
    },
    Scroll {
        dx: f32,
        dy: f32,
        finish: bool,
    },
    ScrollDiscrete {
        x: i32,
        y: i32,
    },
    /// A key by xkb keycode, with the base keysym selkies resolved for it (used to place the
    /// key in the compositor's own keymap), or `None` for a key without one.
    Key {
        xkb_keycode: u32,
        keysym: Option<u32>,
        pressed: bool,
    },
}

/// A handle to a live EIS session: input methods push commands and wake the worker.
pub(crate) struct EiInjector {
    queue: Arc<Mutex<Vec<Cmd>>>,
    wake: OwnedFd,
    alive: Arc<AtomicBool>,
}

impl EiInjector {
    /// Bring up an injector over an EIS socket (from the portal's `ConnectToEIS`), returning it
    /// only once the handshake has bound a keyboard or a pointer. The worker thread owns the
    /// socket for the session's life.
    pub(crate) fn spawn(fd: OwnedFd) -> Result<Self, String> {
        let stream = std::os::unix::net::UnixStream::from(fd);
        let context = ei::Context::new(stream).map_err(|e| format!("EIS context: {e}"))?;
        let (wake_rd, wake_wr) = wake_pipe()?;
        let queue = Arc::new(Mutex::new(Vec::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let ready = Arc::new((
            Mutex::new(None::<Result<(), String>>),
            std::sync::Condvar::new(),
        ));

        let (q, a, r) = (queue.clone(), alive.clone(), ready.clone());
        std::thread::Builder::new()
            .name("pf-host-eis".into())
            .spawn(move || Worker::run(context, wake_rd, q, a, r))
            .map_err(|e| format!("spawn EIS thread: {e}"))?;

        let (lock, cvar) = &*ready;
        let mut slot = lock.lock().unwrap();
        while slot.is_none() {
            slot = cvar.wait(slot).unwrap();
        }
        slot.take().unwrap()?;
        Ok(Self {
            queue,
            wake: wake_wr,
            alive,
        })
    }

    pub(crate) fn alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    fn push(&self, cmd: Cmd) {
        if !self.alive() {
            return;
        }
        self.queue.lock().unwrap().push(cmd);
        let _ = nix_write(self.wake.as_raw_fd());
    }

    /// Queue absolute motion to the logical point (`x`, `y`). `output_fraction`, the point as a
    /// fraction of the output under it, is placed in the region of the device the point goes to
    /// where that device has exactly one: the device's region follows the output's mode, where
    /// the logical point is built from the stream geometry the portal announced at Start.
    pub(crate) fn pointer_motion_abs(&self, x: f64, y: f64, output_fraction: Option<(f64, f64)>) {
        self.push(Cmd::MotionAbsolute {
            x: x as f32,
            y: y as f32,
            output_fraction,
        });
    }

    pub(crate) fn pointer_motion(&self, dx: f64, dy: f64) {
        self.push(Cmd::MotionRelative {
            dx: dx as f32,
            dy: dy as f32,
        });
    }

    pub(crate) fn pointer_button(&self, button: i32, pressed: bool) {
        self.push(Cmd::Button {
            button: button as u32,
            pressed,
        });
    }

    pub(crate) fn pointer_axis(&self, dx: f64, dy: f64, finish: bool) {
        self.push(Cmd::Scroll {
            dx: dx as f32,
            dy: dy as f32,
            finish,
        });
    }

    pub(crate) fn pointer_axis_discrete(&self, axis: u32, steps: i32) {
        // EIS scroll_discrete counts in 120ths of a wheel notch on each axis, like wl_pointer.
        let v = steps * 120;
        let (x, y) = if axis == 0 { (0, v) } else { (v, 0) };
        self.push(Cmd::ScrollDiscrete { x, y });
    }

    pub(crate) fn key(&self, xkb_keycode: u32, keysym: Option<u32>, pressed: bool) {
        self.push(Cmd::Key {
            xkb_keycode,
            keysym,
            pressed,
        });
    }
}

/// A wake byte on the pipe; a full pipe already carries the pending signal.
fn nix_write(fd: i32) -> std::io::Result<()> {
    let one = [1u8];
    let n = unsafe { libc::write(fd, one.as_ptr() as *const libc::c_void, 1) };
    if n < 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() != ErrorKind::WouldBlock {
            return Err(e);
        }
    }
    Ok(())
}

/// One device the server created: its proto object and the emulation state we keep for it.
struct Device {
    device: ei::Device,
    resumed: bool,
    emulating: bool,
    sequence: u32,
    pointer: Option<ei::Pointer>,
    pointer_abs: Option<ei::PointerAbsolute>,
    button: Option<ei::Button>,
    scroll: Option<ei::Scroll>,
    keyboard: Option<ei::Keyboard>,
    keymap: Option<KeymapPolicy>,
    /// The logical rectangles this device's absolute motion may land in; none means anywhere.
    regions: Vec<Rect>,
}

/// A logical rectangle (x, y, width, height) of the compositor's space, where EIS regions lie.
type Rect = (u32, u32, u32, u32);

/// Whether `r` holds the logical point (`x`, `y`), its far edges excluded. The edges are summed
/// in `f64`, which holds any `u32` sum exactly.
fn holds(r: &Rect, x: f32, y: f32) -> bool {
    let (x, y) = (f64::from(x), f64::from(y));
    let (left, top) = (f64::from(r.0), f64::from(r.1));
    x >= left && y >= top && x < left + f64::from(r.2) && y < top + f64::from(r.3)
}

/// The point at fraction (`fx`, `fy`) of region `r`, clamped inside it, or `None` for an empty
/// region.
fn place_in_region(r: &Rect, (fx, fy): (f64, f64)) -> Option<(f32, f32)> {
    let axis = |fraction: f64, offset: u32, length: u32| {
        let size = f64::from(length);
        (f64::from(offset) + (fraction * size).clamp(0.0, size - 1.0)) as f32
    };
    (r.2 > 0 && r.3 > 0).then(|| (axis(fx, r.0, r.2), axis(fy, r.1, r.3)))
}

/// The absolute device for a point, as an index into `devices` (whether each can take motion
/// now, and its regions): the first ready one whose region holds the point, else the first
/// ready one without regions, which takes a point anywhere, else the first ready one. Mutter
/// gives each screencast stream its own absolute device and drops motion outside a device's
/// regions, so a second monitor needs its own device picked.
fn abs_target<'a>(
    devices: impl IntoIterator<Item = (bool, &'a [Rect])>,
    x: f32,
    y: f32,
) -> Option<usize> {
    devices
        .into_iter()
        .enumerate()
        .filter(|(_, (ready, _))| *ready)
        .min_by_key(|(_, (_, regions))| {
            if regions.iter().any(|r| holds(r, x, y)) {
                0
            } else if regions.is_empty() {
                1
            } else {
                2
            }
        })
        .map(|(i, _)| i)
}

struct Worker {
    context: ei::Context,
    converter: EiEventConverter,
    devices: Vec<Device>,
    last_serial: u32,
    /// Held modifier evdev keycodes and their press count, so a key that synthesized a modifier
    /// releases exactly what it added.
    mod_refs: HashMap<u32, u32>,
    /// Per xkb keycode, the evdev key and synthesized modifiers its press emitted, replayed on
    /// release so a mid-stream keymap read cannot desync it.
    pressed: HashMap<u32, (u32, Vec<u32>)>,
}

impl Worker {
    fn run(
        context: ei::Context,
        wake_rd: OwnedFd,
        queue: Arc<Mutex<Vec<Cmd>>>,
        alive: Arc<AtomicBool>,
        ready: Ready,
    ) {
        let resp = match ei_handshake_blocking(
            &context,
            "pixelflux",
            ei::handshake::ContextType::Sender,
        ) {
            Ok(resp) => resp,
            Err(e) => {
                Self::signal(&ready, Err(format!("EIS handshake: {e}")));
                alive.store(false, Ordering::Relaxed);
                return;
            }
        };
        let mut worker = Worker {
            last_serial: resp.serial,
            converter: EiEventConverter::new(&context, resp),
            context,
            devices: Vec::new(),
            mod_refs: HashMap::new(),
            pressed: HashMap::new(),
        };
        let _ = worker.context.flush();

        // Give the server its round to advertise the seat and create devices before reporting
        // what bound, so the caller's rung decision sees the real capability set.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
        let mut reported = false;
        loop {
            if !reported && (worker.has(|d| d.resumed) || std::time::Instant::now() >= deadline) {
                let bound = worker.devices.iter().any(|d| {
                    d.keyboard.is_some() || d.pointer.is_some() || d.pointer_abs.is_some()
                });
                if !bound {
                    Self::signal(
                        &ready,
                        Err("EIS session bound no keyboard or pointer".into()),
                    );
                    alive.store(false, Ordering::Relaxed);
                    return;
                }
                Self::signal(&ready, Ok(()));
                reported = true;
            }
            let timeout = if reported { -1 } else { 100 };
            if !worker.poll(wake_rd.as_raw_fd(), timeout) {
                break;
            }
            if worker.pump_socket().is_err() {
                break;
            }
            if reported {
                worker.drain(&queue);
            }
        }
        alive.store(false, Ordering::Relaxed);
        if !reported {
            Self::signal(&ready, Err("EIS session closed before it was ready".into()));
        }
    }

    fn signal(ready: &Ready, result: Result<(), String>) {
        let (lock, cvar) = &**ready;
        *lock.lock().unwrap() = Some(result);
        cvar.notify_all();
    }

    fn has(&self, f: impl Fn(&Device) -> bool) -> bool {
        self.devices.iter().any(f)
    }

    /// Block until the socket or the wake pipe is readable (or `timeout` ms elapses, `-1` for no
    /// limit); `false` on a poll error that should end the session.
    fn poll(&self, wake_fd: i32, timeout: i32) -> bool {
        let mut fds = [
            libc::pollfd {
                fd: self.context.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) };
        if n < 0 {
            return std::io::Error::last_os_error().kind() == ErrorKind::Interrupted;
        }
        let broken = libc::POLLHUP | libc::POLLERR | libc::POLLNVAL;
        // The wake pipe's write end lives in the injector handle: its hang-up is the session
        // being torn down, and a broken socket ends the session too.
        if (fds[0].revents | fds[1].revents) & broken != 0 {
            return false;
        }
        if fds[1].revents & libc::POLLIN != 0 {
            let mut buf = [0u8; 64];
            while unsafe { libc::read(wake_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) }
                == buf.len() as isize
            {}
        }
        true
    }

    /// Read and dispatch whatever the server sent; `Err` on a closed or broken socket.
    fn pump_socket(&mut self) -> Result<(), ()> {
        match self.context.read() {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(()),
            Err(_) => return Err(()),
        }
        while let Some(result) = self.context.pending_event() {
            match result {
                PendingRequestResult::Request(event) => {
                    if self.converter.handle_event(event).is_err() {
                        return Err(());
                    }
                }
                PendingRequestResult::ParseError(_) => return Err(()),
                PendingRequestResult::InvalidObject(_) => {}
            }
        }
        while let Some(event) = self.converter.next_event() {
            self.handle(event);
        }
        Ok(())
    }

    fn handle(&mut self, event: EiEvent) {
        match event {
            EiEvent::SeatAdded(evt) => {
                evt.seat.bind_capabilities(
                    DeviceCapability::Pointer
                        | DeviceCapability::PointerAbsolute
                        | DeviceCapability::Keyboard
                        | DeviceCapability::Button
                        | DeviceCapability::Scroll
                        | DeviceCapability::Touch,
                );
                let _ = self.context.flush();
            }
            EiEvent::DeviceAdded(evt) => {
                let device = evt.device;
                let keymap = device
                    .keymap()
                    .and_then(keymap_policy)
                    .filter(|_| device.interface::<ei::Keyboard>().is_some());
                self.devices.push(Device {
                    pointer: device.interface(),
                    pointer_abs: device.interface(),
                    button: device.interface(),
                    scroll: device.interface(),
                    keyboard: device.interface(),
                    keymap,
                    regions: device
                        .regions()
                        .iter()
                        .map(|r| (r.x, r.y, r.width, r.height))
                        .collect(),
                    device: device.device().clone(),
                    resumed: false,
                    emulating: false,
                    sequence: 0,
                });
            }
            EiEvent::DeviceRemoved(evt) => {
                self.devices.retain(|d| d.device != *evt.device.device());
            }
            EiEvent::DevicePaused(evt) => {
                if let Some(d) = self.find_mut(&evt.device) {
                    d.resumed = false;
                    d.emulating = false;
                }
            }
            EiEvent::DeviceResumed(evt) => {
                self.last_serial = evt.serial;
                if let Some(d) = self.find_mut(&evt.device) {
                    d.resumed = true;
                    d.emulating = false;
                }
            }
            EiEvent::KeyboardModifiers(evt) => {
                self.last_serial = evt.serial;
            }
            EiEvent::Disconnected(_) => {
                self.devices.clear();
            }
            _ => {}
        }
    }

    fn find_mut(&mut self, device: &reis::event::Device) -> Option<&mut Device> {
        self.devices
            .iter_mut()
            .find(|d| d.device == *device.device())
    }

    fn drain(&mut self, queue: &Arc<Mutex<Vec<Cmd>>>) {
        let cmds = std::mem::take(&mut *queue.lock().unwrap());
        for cmd in cmds {
            self.emit(cmd);
        }
        let _ = self.context.flush();
    }

    fn emit(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::MotionAbsolute {
                x,
                y,
                output_fraction,
            } => {
                let devices = self
                    .devices
                    .iter()
                    .map(|d| (d.resumed && d.pointer_abs.is_some(), d.regions.as_slice()));
                if let Some(i) = abs_target(devices, x, y) {
                    let (x, y) = match (output_fraction, self.devices[i].regions.as_slice()) {
                        (Some(fraction), [region]) => {
                            place_in_region(region, fraction).unwrap_or((x, y))
                        }
                        _ => (x, y),
                    };
                    self.start(i);
                    self.devices[i]
                        .pointer_abs
                        .as_ref()
                        .unwrap()
                        .motion_absolute(x, y);
                    self.frame(i);
                }
            }
            Cmd::MotionRelative { dx, dy } => {
                if let Some(i) = self.ready_index(|d| d.pointer.is_some()) {
                    self.start(i);
                    self.devices[i]
                        .pointer
                        .as_ref()
                        .unwrap()
                        .motion_relative(dx, dy);
                    self.frame(i);
                }
            }
            Cmd::Button { button, pressed } => {
                if let Some(i) = self.ready_index(|d| d.button.is_some()) {
                    self.start(i);
                    self.devices[i]
                        .button
                        .as_ref()
                        .unwrap()
                        .button(button, button_state(pressed));
                    self.frame(i);
                }
            }
            Cmd::Scroll { dx, dy, finish } => {
                if let Some(i) = self.ready_index(|d| d.scroll.is_some()) {
                    self.start(i);
                    let scroll = self.devices[i].scroll.as_ref().unwrap();
                    if dx != 0.0 || dy != 0.0 {
                        scroll.scroll(dx, dy);
                    }
                    if finish {
                        scroll.scroll_stop(1, 1, 0);
                    }
                    self.frame(i);
                }
            }
            Cmd::ScrollDiscrete { x, y } => {
                if let Some(i) = self.ready_index(|d| d.scroll.is_some()) {
                    self.start(i);
                    self.devices[i]
                        .scroll
                        .as_ref()
                        .unwrap()
                        .scroll_discrete(x, y);
                    self.frame(i);
                }
            }
            Cmd::Key {
                xkb_keycode,
                keysym,
                pressed,
            } => self.emit_key(xkb_keycode, keysym, pressed),
        }
    }

    fn emit_key(&mut self, xkb_keycode: u32, keysym: Option<u32>, pressed: bool) {
        let Some(i) = self.ready_index(|d| d.keyboard.is_some()) else {
            return;
        };
        self.start(i);
        if pressed {
            let (evdev, mods) = self.resolve_key(i, xkb_keycode, keysym);
            let kbd = self.devices[i].keyboard.as_ref().unwrap();
            for &m in &mods {
                if *self.mod_refs.entry(m).or_insert(0) == 0 {
                    kbd.key(m, key_state(true));
                }
                *self.mod_refs.get_mut(&m).unwrap() += 1;
            }
            kbd.key(evdev, key_state(true));
            self.pressed.insert(xkb_keycode, (evdev, mods));
        } else if let Some((evdev, mods)) = self.pressed.remove(&xkb_keycode) {
            let kbd = self.devices[i].keyboard.as_ref().unwrap();
            kbd.key(evdev, key_state(false));
            for m in mods.into_iter().rev() {
                let count = self
                    .mod_refs
                    .get(&m)
                    .copied()
                    .unwrap_or(0)
                    .saturating_sub(1);
                if count == 0 {
                    self.mod_refs.remove(&m);
                    kbd.key(m, key_state(false));
                } else {
                    self.mod_refs.insert(m, count);
                }
            }
        }
        self.frame(i);
    }

    /// The evdev keycode and modifier evdev keycodes to press this key with: its base keysym
    /// placed in the compositor's own keymap where that keymap carries it, else the raw keycode.
    fn resolve_key(&self, index: usize, xkb_keycode: u32, keysym: Option<u32>) -> (u32, Vec<u32>) {
        let raw = xkb_keycode.saturating_sub(EVDEV_OFFSET);
        let (Some(sym), Some(policy)) = (keysym, self.devices[index].keymap.as_ref()) else {
            return (raw, Vec::new());
        };
        match policy.resolve(sym) {
            Some((kc, level)) => {
                let mut mods = Vec::new();
                if level & 1 != 0 {
                    mods.push(policy_shift(policy));
                }
                if level & 2 != 0 {
                    mods.push(policy_altgr(policy));
                }
                (
                    kc.saturating_sub(EVDEV_OFFSET),
                    mods.into_iter().flatten().collect(),
                )
            }
            None => (raw, Vec::new()),
        }
    }

    /// The lowest-indexed resumed device matching `want`.
    fn ready_index(&self, want: impl Fn(&Device) -> bool) -> Option<usize> {
        self.devices.iter().position(|d| d.resumed && want(d))
    }

    fn start(&mut self, index: usize) {
        let d = &mut self.devices[index];
        if !d.emulating {
            d.device.start_emulating(self.last_serial, d.sequence);
            d.sequence = d.sequence.wrapping_add(1);
            d.emulating = true;
        }
    }

    fn frame(&mut self, index: usize) {
        self.devices[index]
            .device
            .frame(self.last_serial, now_micros());
    }
}

/// The Shift keycode of a keymap, as an evdev code, if it carries one.
fn policy_shift(policy: &KeymapPolicy) -> Option<u32> {
    policy
        .resolve(0xffe1)
        .map(|(kc, _)| kc.saturating_sub(EVDEV_OFFSET))
}

/// The ISO_Level3_Shift (AltGr) keycode of a keymap, as an evdev code, if it carries one.
fn policy_altgr(policy: &KeymapPolicy) -> Option<u32> {
    policy
        .resolve(0xfe03)
        .map(|(kc, _)| kc.saturating_sub(EVDEV_OFFSET))
}

/// Compile the compositor's keymap into a resolver over its base layout. The mmap is read once;
/// a keymap that does not compile leaves keys to the raw-keycode path.
fn keymap_policy(keymap: &reis::event::Keymap) -> Option<KeymapPolicy> {
    let map = unsafe {
        memmap2::MmapOptions::new()
            .len(keymap.size as usize)
            .map(keymap.fd.as_raw_fd())
            .ok()?
    };
    let text = std::ffi::CStr::from_bytes_until_nul(&map)
        .ok()?
        .to_str()
        .ok()?;
    policy_from_text(text)
}

/// A base-layout resolver over keymap text, or `None` when it does not compile.
fn policy_from_text(text: &str) -> Option<KeymapPolicy> {
    compile_keymap(text)?;
    let mut policy = KeymapPolicy::empty();
    policy.rebuild_base(text.to_string()).then_some(policy)
}

fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

fn button_state(pressed: bool) -> ei::button::ButtonState {
    if pressed {
        ei::button::ButtonState::Press
    } else {
        ei::button::ButtonState::Released
    }
}

fn key_state(pressed: bool) -> ei::keyboard::KeyState {
    if pressed {
        ei::keyboard::KeyState::Press
    } else {
        ei::keyboard::KeyState::Released
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wayland::keymap::compile_rmlvo;
    use reis::eis;
    use reis::handshake::EisHandshaker;
    use reis::request::{EisRequest, EisRequestConverter};
    use std::os::unix::net::UnixStream;
    use std::thread::JoinHandle;
    use std::time::Duration;

    /// Mutter gives each screencast stream its own absolute device, and drops motion
    /// outside a device's regions: a point goes to the device whose region holds it.
    #[test]
    fn absolute_motion_goes_to_the_device_whose_region_holds_it() {
        let top: &[Rect] = &[(0, 0, 2544, 1318)];
        let bottom: &[Rect] = &[(0, 1318, 2556, 1302)];
        let devices = [(true, top), (true, bottom), (false, bottom)];
        assert_eq!(abs_target(devices, 100.0, 1400.0), Some(1));
        assert_eq!(abs_target(devices, 100.0, 100.0), Some(0));
        assert_eq!(abs_target(devices, 9000.0, 9000.0), Some(0));
        assert_eq!(abs_target([(true, &[][..])], 5.0, 5.0), Some(0));
        assert_eq!(abs_target([(false, &[][..])], 5.0, 5.0), None);
    }

    /// A device without regions takes a point anywhere, so a point no region holds goes to it
    /// rather than to a device that would drop it; a device whose region holds the point still
    /// comes first.
    #[test]
    fn a_device_without_regions_takes_a_point_no_region_holds() {
        let left: &[Rect] = &[(0, 0, 100, 100)];
        let anywhere: &[Rect] = &[];
        assert_eq!(
            abs_target([(true, left), (true, anywhere)], 500.0, 500.0),
            Some(1)
        );
        assert_eq!(
            abs_target([(true, anywhere), (true, left)], 50.0, 50.0),
            Some(1)
        );
    }

    /// A region reaching past `u32::MAX` holds its points instead of overflowing its edge.
    #[test]
    fn a_region_at_the_end_of_the_space_holds_its_points() {
        let far = (u32::MAX - 10, u32::MAX - 10, 100, 100);
        assert!(holds(&far, u32::MAX as f32, u32::MAX as f32));
        assert!(!holds(&far, 0.0, 0.0));
    }

    /// A base keysym present in the compositor keymap resolves to that keymap's keycode with the
    /// modifiers its level needs; a Shift-level glyph carries the keymap's own Shift keycode, and
    /// a keysym the keymap lacks leaves the raw keycode untouched.
    #[test]
    fn keyboard_resolves_against_the_host_keymap() {
        let text = compile_rmlvo("", "", "us", "", "").expect("us keymap");
        let policy = policy_from_text(&text).expect("policy");
        // 'a' sits at level 0 on AD01's neighbor; take it from the keymap itself.
        let (a_kc, a_level) = policy.resolve(0x61).expect("a resolves");
        assert_eq!(a_level, 0, "lowercase is unshifted");
        // 'A' is the same key one level up, so it needs Shift.
        let (upper_kc, upper_level) = policy.resolve(0x41).expect("A resolves");
        assert_eq!(upper_kc, a_kc, "same physical key");
        assert_eq!(upper_level & 1, 1, "uppercase needs Shift");
        // The keymap carries a Shift keycode, returned as an evdev code (xkb - 8).
        let shift = policy_shift(&policy).expect("shift keycode");
        assert!(shift >= 1, "a real evdev keycode");
        // A keysym US cannot type has no base hit.
        assert_eq!(
            policy.resolve(0x0439),
            None,
            "Cyrillic short-i absent from us"
        );
    }

    /// A minimal in-process EIS server on `sock`: it completes the handshake, offers one seat,
    /// and on the client's bind adds one resumed device per entry of `regions`, with those
    /// regions, in one flush. Every request it then receives is recorded in `seen`, motion
    /// naming the device (by its entry) it arrived on.
    fn eis_server(
        sock: UnixStream,
        regions: Vec<Vec<Rect>>,
        seen: Arc<Mutex<Vec<String>>>,
    ) -> JoinHandle<()> {
        std::thread::spawn(move || {
            let ctx = eis::Context::new(sock).unwrap();
            let mut handshaker = EisHandshaker::new(&ctx, 1);
            let resp = 'hs: loop {
                if ctx.read().is_err() {
                    return;
                }
                while let Some(pr) = ctx.pending_request() {
                    if let PendingRequestResult::Request(req) = pr
                        && let Ok(Some(resp)) = handshaker.handle_request(req)
                    {
                        break 'hs resp;
                    }
                }
            };
            let mut converter = EisRequestConverter::new(&ctx, resp, 1);
            let handle = converter.handle().clone();
            let seat = handle.add_seat(
                Some("test"),
                DeviceCapability::Pointer
                    | DeviceCapability::PointerAbsolute
                    | DeviceCapability::Button
                    | DeviceCapability::Scroll
                    | DeviceCapability::Keyboard,
            );
            let _ = handle.flush();
            let mut devices = Vec::new();
            loop {
                if ctx.read().is_err() {
                    return;
                }
                while let Some(pr) = ctx.pending_request() {
                    if let PendingRequestResult::Request(req) = pr {
                        let _ = converter.handle_request(req);
                    }
                }
                while let Some(req) = converter.next_request() {
                    let mut log = seen.lock().unwrap();
                    match req {
                        EisRequest::Bind(b) if devices.is_empty() => {
                            for rects in &regions {
                                let dev = seat.add_device(
                                    Some("virt"),
                                    eis::device::DeviceType::Virtual,
                                    b.capabilities,
                                    |device| {
                                        for r in rects {
                                            device.device().region(r.0, r.1, r.2, r.3, 1.0);
                                        }
                                    },
                                );
                                dev.resumed();
                                devices.push(dev);
                            }
                            let _ = handle.flush();
                        }
                        EisRequest::DeviceStartEmulating(_) => log.push("start".into()),
                        EisRequest::PointerMotionAbsolute(m) => {
                            let on = devices.iter().position(|d| *d == m.device);
                            log.push(format!(
                                "abs {} {} {}",
                                on.map_or(-1, |i| i as i32),
                                m.dx_absolute as i32,
                                m.dy_absolute as i32
                            ));
                        }
                        EisRequest::Button(b) => log.push(format!(
                            "button {} {}",
                            b.button,
                            matches!(b.state, eis::button::ButtonState::Press)
                        )),
                        EisRequest::KeyboardKey(k) => log.push(format!(
                            "key {} {}",
                            k.key,
                            matches!(k.state, eis::keyboard::KeyState::Press)
                        )),
                        EisRequest::ScrollDiscrete(s) => {
                            log.push(format!("scroll {} {}", s.discrete_dx, s.discrete_dy));
                        }
                        _ => {}
                    }
                }
            }
        })
    }

    /// The injector, run against a minimal in-process EIS server, emits the protocol a
    /// compositor expects: it binds the seat, starts emulating before the first event, and
    /// sends absolute motion, buttons, keys, and discrete scroll with the values handed to it,
    /// each committed by a frame. A move carrying its fraction of the output is placed in the
    /// device's region, whose size and origin differ from the logical point's.
    #[test]
    fn injector_emits_the_expected_eis_protocol() {
        let (server_sock, client_sock) = UnixStream::pair().unwrap();
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let server = eis_server(server_sock, vec![vec![(12, 24, 2176, 2160)]], seen.clone());

        let injector =
            EiInjector::spawn(OwnedFd::from(client_sock)).expect("injector binds a device");
        injector.pointer_motion_abs(400.0, 300.0, None);
        injector.pointer_motion_abs(1440.0, 672.0, Some((0.75, 0.75)));
        injector.pointer_button(0x110, true);
        injector.pointer_button(0x110, false);
        injector.key(38, None, true);
        injector.key(38, None, false);
        injector.pointer_axis_discrete(0, 1);
        std::thread::sleep(Duration::from_millis(400));
        drop(injector);
        server.join().unwrap();

        let got = seen.lock().unwrap().clone();
        assert!(
            got.contains(&"start".to_string()),
            "start_emulating: {got:?}"
        );
        assert!(
            got.contains(&"abs 0 400 300".to_string()),
            "absolute motion: {got:?}"
        );
        assert!(
            got.contains(&"abs 0 1644 1644".to_string()),
            "absolute motion placed in the device region: {got:?}"
        );
        assert!(
            got.contains(&"button 272 true".to_string()),
            "left press: {got:?}"
        );
        assert!(
            got.contains(&"button 272 false".to_string()),
            "left release: {got:?}"
        );
        assert!(
            got.contains(&"key 30 true".to_string()),
            "key press: {got:?}"
        );
        assert!(
            got.contains(&"key 30 false".to_string()),
            "key release: {got:?}"
        );
        assert!(
            got.contains(&"scroll 0 120".to_string()),
            "discrete scroll: {got:?}"
        );
    }

    /// With one absolute device per monitor, as Mutter gives each screencast stream, the
    /// injector sends each point on the device whose region holds it, so the second monitor's
    /// motion is not dropped on the first one's device, and a fraction is placed in the region
    /// of the device the point went to.
    #[test]
    fn injector_sends_each_point_on_the_device_of_its_monitor() {
        let (server_sock, client_sock) = UnixStream::pair().unwrap();
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let regions = vec![vec![(0, 0, 2544, 1318)], vec![(0, 1318, 2556, 1302)]];
        let server = eis_server(server_sock, regions, seen.clone());

        let injector =
            EiInjector::spawn(OwnedFd::from(client_sock)).expect("injector binds a device");
        injector.pointer_motion_abs(100.0, 1400.0, None);
        injector.pointer_motion_abs(100.0, 100.0, None);
        injector.pointer_motion_abs(100.0, 1400.0, Some((0.5, 0.5)));
        std::thread::sleep(Duration::from_millis(400));
        drop(injector);
        server.join().unwrap();

        let got = seen.lock().unwrap().clone();
        assert!(
            got.contains(&"abs 1 100 1400".to_string()),
            "bottom monitor's device: {got:?}"
        );
        assert!(
            got.contains(&"abs 0 100 100".to_string()),
            "top monitor's device: {got:?}"
        );
        assert!(
            got.contains(&"abs 1 1278 1969".to_string()),
            "fraction placed in the bottom monitor's region: {got:?}"
        );
    }
}
