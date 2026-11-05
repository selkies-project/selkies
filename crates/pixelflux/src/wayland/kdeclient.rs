/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! KDE session-screen client: grows, removes, and arranges the screens of a
//! nested KWin session, in-process.
//!
//! KWin serves no control socket; its interface for a screen that exists on
//! demand is `zkde_screencast_unstable_v1`. A `stream_virtual_output` request
//! creates a real output carrying the requested name, and that output lives
//! exactly as long as the stream object: KWin removes it when the stream is
//! closed or its client disconnects, and only then. A `failed` event is the
//! PipeWire stream's failure, not the screen's — the output is created before
//! the stream starts — so no PipeWire consumer is required and frames keep
//! arriving through the host window the nested output opens, which also
//! drives the screen's size ([`add_screen`] only seeds it). Each grown screen
//! therefore parks one open connection in a process-wide registry; nothing
//! dispatches it afterwards, because holding the socket open is the entire
//! contract, and [`remove_screen`] closes the stream to give the screen back.
//! A session is offered a second display on this rung when the compositor
//! serves the request ([`screen_control_offered`], a registry read that
//! touches no screen); whether it registers the screen it grows is proven
//! when a display asks for one, since a KWin whose nested backend registers
//! no virtual output serves the request all the same: [`grow`] confirms the
//! screen against the output devices and refuses the display with that
//! reason.
//!
//! Arrangement rides `kde_output_management_v2` over the per-output
//! `kde_output_device_v2` globals. Unlike `zwlr_output_management_v1` there
//! is no state serial and a configuration is a delta — a device left out
//! keeps its state — and modes come from the advertised list only, never
//! custom, so [`set_screen_layout`] positions screens and leaves their size
//! to the capture side; their scale is not set here either, because a nested
//! KWin accepts one and ignores it, following the host window's preferred
//! fractional scale instead. Screens are addressed by position in name order,
//! matching `outclient`. Blocking, off the compositor thread, with every
//! round-trip deadline-bounded.

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use crate::wayland::kdeproto::output_device::{
    kde_output_device_mode_v2::{self, KdeOutputDeviceModeV2},
    kde_output_device_v2::{self, KdeOutputDeviceV2},
};
use crate::wayland::kdeproto::output_management::{
    kde_output_configuration_v2::{self, KdeOutputConfigurationV2},
    kde_output_management_v2::KdeOutputManagementV2,
};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, delegate_noop};
use wayland_protocols_plasma::screencast::v1::client::{
    zkde_screencast_stream_unstable_v1::{self, ZkdeScreencastStreamUnstableV1},
    zkde_screencast_unstable_v1::ZkdeScreencastUnstableV1,
};

use crate::wayland::outclient::trailing_number;
use crate::wayland::wlclient::{IO_TIMEOUT, SyncState, bounded_roundtrip, impl_sync_callback};

/// `zkde_screencast_unstable_v1.pointer` value asking for no cursor in the
/// stream nobody consumes.
const POINTER_HIDDEN: u32 = 1;

/// A grown screen's keep-alive: the connection whose lifetime is the screen's.
struct HeldScreen {
    conn: Connection,
    _queue: EventQueue<CastState>,
    _state: CastState,
    stream: ZkdeScreencastStreamUnstableV1,
}

/// Held screens by the name they were grown under, process-wide: the FFI layer
/// is stateless per call, and a screen must outlive the call that grew it.
fn held_screens() -> &'static Mutex<HashMap<String, HeldScreen>> {
    static HELD: OnceLock<Mutex<HashMap<String, HeldScreen>>> = OnceLock::new();
    HELD.get_or_init(Default::default)
}

#[derive(Default)]
struct CastState {
    manager: Option<ZkdeScreencastUnstableV1>,
    /// The stream's verdict: created, or the error `failed`/`closed` carried.
    outcome: Option<Result<(), String>>,
    sync_done: bool,
}

impl SyncState for CastState {
    fn sync_done_mut(&mut self) -> &mut bool {
        &mut self.sync_done
    }
}
impl_sync_callback!(CastState);

impl Dispatch<wl_registry::WlRegistry, ()> for CastState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            // stream_virtual_output is a version-2 request.
            if interface == "zkde_screencast_unstable_v1" && state.manager.is_none() && version >= 2
            {
                state.manager = Some(registry.bind(name, version.min(2), qh, ()));
            }
        }
    }
}
delegate_noop!(CastState: ZkdeScreencastUnstableV1);

impl Dispatch<ZkdeScreencastStreamUnstableV1, ()> for CastState {
    fn event(
        state: &mut Self,
        _: &ZkdeScreencastStreamUnstableV1,
        event: zkde_screencast_stream_unstable_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zkde_screencast_stream_unstable_v1::Event::Created { .. } => {
                state.outcome = Some(Ok(()))
            }
            zkde_screencast_stream_unstable_v1::Event::Failed { error } => {
                state.outcome = Some(Err(error))
            }
            zkde_screencast_stream_unstable_v1::Event::Closed if state.outcome.is_none() => {
                state.outcome = Some(Err("the compositor closed the stream".to_string()));
            }
            _ => {}
        }
    }
}

/// Why a screen could not be grown: the compositor was unreachable, or it
/// answered and refused. A probe reports the first and answers "no" to the
/// second.
enum GrowError {
    Unreachable(String),
    Refused(String),
}

/// Connect to the compositor at `socket_path` and read its registry, which binds the
/// screencast manager when one is served at the version carrying `stream_virtual_output`.
fn connect(socket_path: &str) -> Result<(Connection, EventQueue<CastState>, CastState), String> {
    let stream =
        UnixStream::connect(socket_path).map_err(|e| format!("connect {socket_path}: {e}"))?;
    let conn = Connection::from_socket(stream).map_err(|e| format!("wayland setup: {e}"))?;
    let mut queue: EventQueue<CastState> = conn.new_event_queue();
    let _registry = conn.display().get_registry(&queue.handle(), ());
    let mut state = CastState::default();
    bounded_roundtrip(&conn, &mut queue, &mut state)?;
    Ok((conn, queue, state))
}

/// Grow a screen named `name` on the compositor at `socket_path` and return
/// its keep-alive; the screen lives until that is dropped or its stream closed.
fn grow(
    socket_path: &str,
    name: &str,
    size: (i32, i32),
    scale: f64,
) -> Result<HeldScreen, GrowError> {
    let (conn, mut queue, mut state) = connect(socket_path).map_err(GrowError::Unreachable)?;
    let qh = queue.handle();
    let Some(manager) = state.manager.clone() else {
        return Err(GrowError::Refused(
            "the compositor offers no zkde_screencast_unstable_v1".to_string(),
        ));
    };
    let stream = manager.stream_virtual_output(
        name.to_string(),
        size.0,
        size.1,
        scale,
        POINTER_HIDDEN,
        &qh,
        (),
    );
    queue
        .flush()
        .map_err(|e| GrowError::Unreachable(format!("flush: {e}")))?;
    let deadline = Instant::now() + IO_TIMEOUT;
    while state.outcome.is_none() && Instant::now() < deadline {
        bounded_roundtrip(&conn, &mut queue, &mut state).map_err(GrowError::Unreachable)?;
    }
    // The stream's verdict is the PipeWire stream's; the screen is confirmed
    // against the output-device globals whatever it said.
    if !screen_present(socket_path, name).map_err(GrowError::Unreachable)? {
        return Err(GrowError::Refused(match state.outcome.take() {
            Some(Err(e)) => format!("virtual screen '{name}' refused: {e}"),
            _ => format!("virtual screen '{name}' produced no output device"),
        }));
    }
    Ok(HeldScreen {
        conn,
        _queue: queue,
        _state: state,
        stream,
    })
}

/// Whether an enabled screen named `name` is among the compositor's output devices.
fn screen_present(socket_path: &str, name: &str) -> Result<bool, String> {
    Ok(list_screens(socket_path)?.iter().any(|(n, ..)| n == name))
}

/// Whether the compositor on `socket_path` serves `zkde_screencast_unstable_v1` at the
/// version carrying `stream_virtual_output`: the rung a session with no control socket is
/// offered a second display on. A registry read alone, so a session is asked on every
/// landing without a screen coming or going on it; whether a grown screen registers is
/// [`grow`]'s to prove. Err only when the compositor is unreachable.
pub fn screen_control_offered(socket_path: &str) -> Result<bool, String> {
    let (_conn, _queue, state) = connect(socket_path)?;
    Ok(state.manager.is_some())
}

/// Grow a screen named `name` on the compositor at `socket_path`, holding it
/// open until [`remove_screen`]. Idempotent for a name already held.
///
/// `size` and `scale` only seed the screen: a nested KWin output follows the
/// host window a capture output adopts. The stream's verdict is not the
/// screen's (see the module docstring), so a refusal is confirmed against the
/// output-device globals before it is reported.
pub fn add_screen(
    socket_path: &str,
    name: &str,
    size: (i32, i32),
    scale: f64,
) -> Result<(), String> {
    if name.is_empty() {
        return Err("a screen needs a name".to_string());
    }
    if size.0 <= 0 || size.1 <= 0 {
        return Err(format!("size {}x{} out of range", size.0, size.1));
    }
    if !(0.1..=16.0).contains(&scale) {
        return Err(format!("scale {scale} out of range"));
    }
    if held_screens().lock().unwrap().contains_key(name) {
        return Ok(());
    }
    let held = grow(socket_path, name, size, scale).map_err(|e| match e {
        GrowError::Unreachable(e) | GrowError::Refused(e) => e,
    })?;
    held_screens()
        .lock()
        .unwrap()
        .insert(name.to_string(), held);
    Ok(())
}

/// Close the held stream behind the screen `name`, removing it — the
/// compositor's own output-destroy path evacuates its windows. False when no
/// such screen is held (one the session booted with, or another process's).
pub fn remove_screen(name: &str) -> bool {
    let Some(held) = held_screens().lock().unwrap().remove(name) else {
        return false;
    };
    held.stream.close();
    let _ = held.conn.flush();
    true
}

/// One `kde_output_device_v2` global with the state its events carried.
struct Device {
    dev: KdeOutputDeviceV2,
    name: Option<String>,
    enabled: bool,
    pos: (i32, i32),
    scale: f64,
    current: Option<KdeOutputDeviceModeV2>,
}

#[derive(Default)]
struct KdeOutState {
    manager: Option<KdeOutputManagementV2>,
    devices: Vec<Device>,
    /// Size per announced mode object; a device carries no size of its own,
    /// so its current mode's is reported.
    modes: Vec<(KdeOutputDeviceModeV2, (i32, i32))>,
    applied: Option<bool>,
    sync_done: bool,
}

impl SyncState for KdeOutState {
    fn sync_done_mut(&mut self) -> &mut bool {
        &mut self.sync_done
    }
}
impl_sync_callback!(KdeOutState);

impl Dispatch<wl_registry::WlRegistry, ()> for KdeOutState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == "kde_output_management_v2" && state.manager.is_none() {
                state.manager = Some(registry.bind(name, version.min(1), qh, ()));
            } else if interface == "kde_output_device_v2" {
                // Version 2 carries the name event screens are addressed by.
                let dev: KdeOutputDeviceV2 = registry.bind(name, version.min(2), qh, ());
                state.devices.push(Device {
                    dev,
                    name: None,
                    enabled: false,
                    pos: (0, 0),
                    scale: 1.0,
                    current: None,
                });
            }
        }
    }
}
delegate_noop!(KdeOutState: KdeOutputManagementV2);

impl Dispatch<KdeOutputDeviceV2, ()> for KdeOutState {
    fn event(
        state: &mut Self,
        dev: &KdeOutputDeviceV2,
        event: kde_output_device_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(entry) = state.devices.iter_mut().find(|d| &d.dev == dev) else {
            return;
        };
        match event {
            kde_output_device_v2::Event::Name { name } => entry.name = Some(name),
            kde_output_device_v2::Event::Enabled { enabled } => entry.enabled = enabled != 0,
            kde_output_device_v2::Event::Geometry { x, y, .. } => entry.pos = (x, y),
            kde_output_device_v2::Event::Scale { factor } => entry.scale = factor,
            kde_output_device_v2::Event::CurrentMode { mode } => entry.current = Some(mode),
            _ => {}
        }
    }

    wayland_client::event_created_child!(KdeOutState, KdeOutputDeviceV2, [
        kde_output_device_v2::EVT_MODE_OPCODE => (KdeOutputDeviceModeV2, ()),
    ]);
}

impl Dispatch<KdeOutputDeviceModeV2, ()> for KdeOutState {
    fn event(
        state: &mut Self,
        mode: &KdeOutputDeviceModeV2,
        event: kde_output_device_mode_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let kde_output_device_mode_v2::Event::Size { width, height } = event {
            let mode = mode.clone();
            state.modes.retain(|(m, _)| *m != mode);
            state.modes.push((mode, (width, height)));
        }
    }
}

impl Dispatch<KdeOutputConfigurationV2, ()> for KdeOutState {
    fn event(
        state: &mut Self,
        _: &KdeOutputConfigurationV2,
        event: kde_output_configuration_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            kde_output_configuration_v2::Event::Applied => state.applied = Some(true),
            kde_output_configuration_v2::Event::Failed => state.applied = Some(false),
            _ => {}
        }
    }
}

/// Connect to `socket_path` and read the output-device globals to a settled
/// state: one round-trip announces them, a second delivers their events.
fn read_devices(
    socket_path: &str,
) -> Result<(Connection, EventQueue<KdeOutState>, KdeOutState), String> {
    let stream =
        UnixStream::connect(socket_path).map_err(|e| format!("connect {socket_path}: {e}"))?;
    let conn = Connection::from_socket(stream).map_err(|e| format!("wayland setup: {e}"))?;
    let mut queue: EventQueue<KdeOutState> = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut state = KdeOutState::default();
    bounded_roundtrip(&conn, &mut queue, &mut state)?;
    bounded_roundtrip(&conn, &mut queue, &mut state)?;
    Ok((conn, queue, state))
}

/// Indices of the enabled devices in name order — the order screens map to
/// displays, matching `outclient`'s addressing.
fn enabled_sorted(state: &KdeOutState) -> Vec<usize> {
    let mut order: Vec<(usize, String)> = state
        .devices
        .iter()
        .enumerate()
        .filter(|(_, d)| d.enabled)
        .map(|(i, d)| (i, d.name.clone().unwrap_or_default()))
        .collect();
    order.sort_by_key(|(_, name)| (trailing_number(name), name.clone()));
    order.into_iter().map(|(i, _)| i).collect()
}

/// The session's enabled screens as `(name, x, y, width, height)` in screen
/// order; the size is the current mode's, `(0, 0)` where none was announced.
/// Empty when the compositor serves no `kde_output_device_v2` globals.
pub fn list_screens(socket_path: &str) -> Result<Vec<super::AppScreen>, String> {
    let (_conn, _queue, state) = read_devices(socket_path)?;
    let screens = enabled_sorted(&state)
        .into_iter()
        .map(|i| {
            let d = &state.devices[i];
            let (w, h) = d
                .current
                .as_ref()
                .and_then(|mode| state.modes.iter().find(|(m, _)| m == mode))
                .map(|(_, size)| *size)
                .unwrap_or((0, 0));
            (d.name.clone().unwrap_or_default(), d.pos.0, d.pos.1, w, h)
        })
        .collect();
    Ok(screens)
}

/// Position the session's screens at `rects`, one `(x, y, width, height)` per
/// screen in screen order; sizes are validated but not applied, because a
/// nested KWin screen's size follows its host window. The rectangles are
/// capture pixels and the positions land in the session's logical space, closed
/// up around each screen's scale (`outclient::close_gaps`). Returns how many
/// were positioned; 0 when the compositor serves no `kde_output_management_v2`.
pub fn set_screen_layout(
    socket_path: &str,
    rects: Vec<(i32, i32, i32, i32)>,
) -> Result<usize, String> {
    if rects.iter().any(|(_, _, w, h)| *w <= 0 || *h <= 0) {
        return Err("a screen rectangle has a non-positive size".to_string());
    }
    let (conn, mut queue, mut state) = read_devices(socket_path)?;
    let Some(manager) = state.manager.clone() else {
        return Ok(0);
    };
    let qh = queue.handle();
    let order = enabled_sorted(&state);
    let n = order.len().min(rects.len());
    let sizes: Vec<(i32, i32)> = order
        .iter()
        .zip(&rects)
        .map(|(&i, r)| {
            let scale = state.devices[i].scale;
            (
                (r.2 as f64 / scale).round() as i32,
                (r.3 as f64 / scale).round() as i32,
            )
        })
        .collect();
    let placed: Vec<(KdeOutputDeviceV2, (i32, i32))> = order
        .iter()
        .zip(crate::wayland::outclient::close_gaps(&rects[..n], &sizes))
        .map(|(&i, p)| (state.devices[i].dev.clone(), (p.0, p.1)))
        .collect();
    if placed.is_empty() {
        return Ok(0);
    }
    let config = manager.create_configuration(&qh, ());
    for (dev, (x, y)) in &placed {
        config.position(dev, *x, *y);
    }
    apply_configuration(&conn, &mut queue, &mut state, config)?;
    Ok(placed.len())
}

/// Apply a configuration and wait, deadline-bounded, for the compositor's
/// verdict on it; the configuration object is released either way.
fn apply_configuration(
    conn: &Connection,
    queue: &mut EventQueue<KdeOutState>,
    state: &mut KdeOutState,
    config: KdeOutputConfigurationV2,
) -> Result<(), String> {
    state.applied = None;
    config.apply();
    queue
        .flush()
        .map_err(|e| format!("flush configuration: {e}"))?;
    let deadline = Instant::now() + IO_TIMEOUT;
    while state.applied.is_none() && Instant::now() < deadline {
        bounded_roundtrip(conn, queue, state)?;
    }
    config.destroy();
    let _ = queue.flush();
    match state.applied {
        Some(true) => Ok(()),
        Some(false) => Err("the compositor refused the configuration".to_string()),
        None => Err("timed out waiting for the configuration".to_string()),
    }
}
