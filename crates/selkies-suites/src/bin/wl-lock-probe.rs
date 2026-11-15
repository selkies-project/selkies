/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A Wayland client holding a pointer lock, for what a game holding one is told.
//!
//! `wl-lock-probe SOCKET` maps a fullscreen surface, takes a `zwp_locked_pointer_v1` on it once
//! the pointer is inside, and prints a JSON line for the lock's confirmation (`locked`,
//! `unlocked`), every relative-pointer delta (`relative`), and every absolute pointer event
//! (`enter`, `leave`, `motion`, `button`). A compositor that honors the lock sends the deltas
//! alone; one that moves the pointer as well sends absolute motion too, which a toolkit turns into
//! a second delta. `unavailable` names the globals it needs that the compositor lacks; `mapped`
//! is printed once the surface shows, `done` when it ends after `WL_LOCK_DURATION` seconds (40)
//! or SIGTERM.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use serde_json::Value;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool,
    wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::wp::pointer_constraints::zv1::client::{
    zwp_locked_pointer_v1, zwp_pointer_constraints_v1,
};
use wayland_protocols::wp::relative_pointer::zv1::client::{
    zwp_relative_pointer_manager_v1, zwp_relative_pointer_v1,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

use selkies_suites::wayland::{self, emit};

struct Probe {
    configured: bool,
    asked: (i32, i32),
    constraints: zwp_pointer_constraints_v1::ZwpPointerConstraintsV1,
    surface: Option<wl_surface::WlSurface>,
    lock: Option<zwp_locked_pointer_v1::ZwpLockedPointerV1>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Probe {
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

impl Dispatch<wl_pointer::WlPointer, ()> for Probe {
    fn event(
        state: &mut Self,
        pointer: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            } => {
                emit(
                    "enter",
                    &[("x", Value::from(surface_x)), ("y", Value::from(surface_y))],
                );
                // One lock for the life of the probe, held whenever the pointer is inside.
                if state.lock.is_none()
                    && let Some(surface) = &state.surface
                {
                    state.lock = Some(state.constraints.lock_pointer(
                        surface,
                        pointer,
                        None,
                        zwp_pointer_constraints_v1::Lifetime::Persistent,
                        qh,
                        (),
                    ));
                    surface.commit();
                }
            }
            wl_pointer::Event::Leave { .. } => emit("leave", &[]),
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => emit(
                "motion",
                &[("x", Value::from(surface_x)), ("y", Value::from(surface_y))],
            ),
            wl_pointer::Event::Button { button, state, .. } => emit(
                "button",
                &[
                    ("button", Value::from(button)),
                    (
                        "down",
                        Value::from(matches!(
                            state,
                            WEnum::Value(wl_pointer::ButtonState::Pressed)
                        )),
                    ),
                ],
            ),
            _ => {}
        }
    }
}

impl Dispatch<zwp_relative_pointer_v1::ZwpRelativePointerV1, ()> for Probe {
    fn event(
        _: &mut Self,
        _: &zwp_relative_pointer_v1::ZwpRelativePointerV1,
        event: zwp_relative_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_relative_pointer_v1::Event::RelativeMotion { dx, dy, .. } = event {
            emit(
                "relative",
                &[("dx", Value::from(dx)), ("dy", Value::from(dy))],
            );
        }
    }
}

impl Dispatch<zwp_locked_pointer_v1::ZwpLockedPointerV1, ()> for Probe {
    fn event(
        _: &mut Self,
        _: &zwp_locked_pointer_v1::ZwpLockedPointerV1,
        event: zwp_locked_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_locked_pointer_v1::Event::Locked => emit("locked", &[]),
            zwp_locked_pointer_v1::Event::Unlocked => emit("unlocked", &[]),
            _ => {}
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Probe {
    fn event(
        _: &mut Self,
        wm: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for Probe {
    fn event(
        state: &mut Self,
        surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            surface.ack_configure(serial);
            state.configured = true;
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for Probe {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Configure { width, height, .. } = event
            && width > 0
            && height > 0
        {
            state.asked = (width, height);
        }
    }
}

delegate_noop!(Probe: ignore wl_compositor::WlCompositor);
delegate_noop!(Probe: ignore wl_surface::WlSurface);
delegate_noop!(Probe: ignore wl_shm::WlShm);
delegate_noop!(Probe: ignore wl_shm_pool::WlShmPool);
delegate_noop!(Probe: ignore wl_buffer::WlBuffer);
delegate_noop!(Probe: ignore wl_output::WlOutput);
delegate_noop!(Probe: ignore wl_seat::WlSeat);
delegate_noop!(Probe: zwp_pointer_constraints_v1::ZwpPointerConstraintsV1);
delegate_noop!(Probe: zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1);

fn run(socket: &str) -> Result<(), String> {
    let duration = std::env::var("WL_LOCK_DURATION")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(40.0);
    let conn = wayland::connect(socket)?;
    let (globals, mut queue) = registry_queue_init::<Probe>(&conn).map_err(|e| e.to_string())?;
    let qh = queue.handle();
    let compositor = globals.bind::<wl_compositor::WlCompositor, _, _>(&qh, 1..=4, ());
    let shm = globals.bind::<wl_shm::WlShm, _, _>(&qh, 1..=1, ());
    let wm = globals.bind::<xdg_wm_base::XdgWmBase, _, _>(&qh, 1..=1, ());
    let seat = globals.bind::<wl_seat::WlSeat, _, _>(&qh, 1..=5, ());
    let constraints =
        globals.bind::<zwp_pointer_constraints_v1::ZwpPointerConstraintsV1, _, _>(&qh, 1..=1, ());
    let relative = globals
        .bind::<zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1, _, _>(&qh, 1..=1, ());
    let missing: Vec<&str> = [
        ("comp", compositor.is_err()),
        ("shm", shm.is_err()),
        ("xdg", wm.is_err()),
        ("seat", seat.is_err()),
        ("constraints", constraints.is_err()),
        ("relative", relative.is_err()),
    ]
    .into_iter()
    .filter_map(|(name, absent)| absent.then_some(name))
    .collect();
    let (Ok(compositor), Ok(shm), Ok(wm), Ok(seat), Ok(constraints), Ok(relative)) =
        (compositor, shm, wm, seat, constraints, relative)
    else {
        emit("unavailable", &[("missing", Value::from(missing))]);
        return Ok(());
    };
    let output = globals.contents().with_list(|list| {
        list.iter()
            .rfind(|g| g.interface == "wl_output")
            .map(|g| (g.name, g.version))
    });
    let output = output.map(|(name, version)| {
        globals
            .registry()
            .bind::<wl_output::WlOutput, _, _>(name, version.min(1), &qh, ())
    });
    let mut state = Probe {
        configured: false,
        asked: (0, 0),
        constraints,
        surface: None,
        lock: None,
    };
    let surface = compositor.create_surface(&qh, ());
    let xdg = wm.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_title("selkies-wl-lock-probe".into());
    toplevel.set_fullscreen(output.as_ref());
    surface.commit();
    for _ in 0..30 {
        queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
        if state.configured {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let (width, height) = if state.asked.0 > 0 {
        state.asked
    } else {
        (1280, 720)
    };
    let buffer = wayland::solid_buffer(&shm, &qh, width, height, 0xFF10_1010)?;
    surface.attach(Some(&buffer), 0, 0);
    surface.commit();
    queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
    let pointer = seat.get_pointer(&qh, ());
    let _relative = relative.get_relative_pointer(&pointer, &qh, ());
    state.surface = Some(surface);
    queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
    emit(
        "mapped",
        &[("w", Value::from(width)), ("h", Value::from(height))],
    );
    let deadline = Instant::now() + Duration::from_secs_f64(duration.max(0.0));
    let ended = wayland::dispatch_until(&mut queue, &mut state, deadline);
    emit("done", &[]);
    ended
}

fn main() -> ExitCode {
    wayland::watch_for_termination();
    let socket = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "wayland-1".into());
    match run(&socket) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("wl-lock-probe: {e}");
            ExitCode::FAILURE
        }
    }
}
