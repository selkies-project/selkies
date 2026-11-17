/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A Wayland client that opens a popup over a layer surface, as a panel opens a menu, or over a
//! window, for the popup suite of the pixelflux compositor.
//!
//! `wl-popup-client SOCKET [layer|window]` (or `POPUP_MODE`). The parent, 300x40 at the top left
//! (300x200 as a window), is painted `PARENT_FILL` and the popup, 200x120 hanging below it,
//! `POPUP_FILL` (ARGB hex). It prints a JSON line for each surface `mapped`, for
//! `popup_configure` and `popup_done`, and for pointer enter, leave, motion and button events
//! with the surface (`parent` or `popup`) they reached. It runs `POPUP_DURATION` seconds (60),
//! ending early on SIGTERM.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use serde_json::Value;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum, backend::ObjectId, delegate_noop,
};
use wayland_protocols::xdg::shell::client::{
    xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base,
};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use selkies_suites::wayland::{self, emit};

/// Which of the client's two surfaces an `xdg_surface` belongs to.
const PARENT: u8 = 0;
const POPUP: u8 = 1;

#[derive(Default)]
struct Client {
    parent: Option<ObjectId>,
    popup: Option<ObjectId>,
    /// The serial of a configure not yet acknowledged, by surface.
    pending: [Option<u32>; 2],
    pointer: Option<wl_pointer::WlPointer>,
    under: Option<&'static str>,
}

impl Client {
    fn tag(&self, surface: &wl_surface::WlSurface) -> &'static str {
        let id = surface.id();
        if self.popup.as_ref() == Some(&id) {
            "popup"
        } else if self.parent.as_ref() == Some(&id) {
            "parent"
        } else {
            "other"
        }
    }

    fn under(&self) -> Value {
        self.under.map_or(Value::Null, Value::from)
    }
}

fn raw<T: Into<u32>>(value: WEnum<T>) -> u32 {
    match value {
        WEnum::Value(v) => v.into(),
        WEnum::Unknown(u) => u,
    }
}

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

impl Dispatch<wl_seat::WlSeat, ()> for Client {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
            && state.pointer.is_none()
            && caps.contains(wl_seat::Capability::Pointer)
        {
            state.pointer = Some(seat.get_pointer(qh, ()));
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for Client {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface,
                surface_x,
                surface_y,
                ..
            } => {
                state.under = Some(state.tag(&surface));
                emit(
                    "ptr_enter",
                    &[
                        ("surface", state.under()),
                        ("x", Value::from(surface_x)),
                        ("y", Value::from(surface_y)),
                    ],
                );
            }
            wl_pointer::Event::Leave { surface, .. } => {
                emit(
                    "ptr_leave",
                    &[("surface", Value::from(state.tag(&surface)))],
                );
                state.under = None;
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                emit(
                    "ptr_motion",
                    &[
                        ("surface", state.under()),
                        ("x", Value::from(surface_x)),
                        ("y", Value::from(surface_y)),
                    ],
                );
            }
            wl_pointer::Event::Button {
                button,
                state: pressed,
                ..
            } => {
                emit(
                    "ptr_button",
                    &[
                        ("surface", state.under()),
                        ("button", Value::from(button)),
                        ("state", Value::from(raw(pressed))),
                    ],
                );
            }
            _ => {}
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Client {
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

impl Dispatch<xdg_surface::XdgSurface, u8> for Client {
    fn event(
        state: &mut Self,
        _: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        which: &u8,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            state.pending[usize::from(*which)] = Some(serial);
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for Client {
    fn event(
        state: &mut Self,
        _: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure { serial, .. } = event {
            state.pending[usize::from(PARENT)] = Some(serial);
        }
    }
}

impl Dispatch<xdg_popup::XdgPopup, ()> for Client {
    fn event(
        _: &mut Self,
        _: &xdg_popup::XdgPopup,
        event: xdg_popup::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_popup::Event::Configure {
                x,
                y,
                width,
                height,
            } => emit(
                "popup_configure",
                &[
                    ("x", Value::from(x)),
                    ("y", Value::from(y)),
                    ("w", Value::from(width)),
                    ("h", Value::from(height)),
                ],
            ),
            xdg_popup::Event::PopupDone => emit("popup_done", &[]),
            _ => {}
        }
    }
}

delegate_noop!(Client: ignore wl_compositor::WlCompositor);
delegate_noop!(Client: ignore wl_surface::WlSurface);
delegate_noop!(Client: ignore wl_shm::WlShm);
delegate_noop!(Client: ignore wl_shm_pool::WlShmPool);
delegate_noop!(Client: ignore wl_buffer::WlBuffer);
delegate_noop!(Client: ignore xdg_positioner::XdgPositioner);
delegate_noop!(Client: ignore xdg_toplevel::XdgToplevel);
delegate_noop!(Client: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);

fn color(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|v| u32::from_str_radix(v.trim(), 16).ok())
        .unwrap_or(default)
}

/// Dispatch until `which` has a configure to acknowledge; its serial.
fn configured(
    queue: &mut wayland_client::EventQueue<Client>,
    state: &mut Client,
    which: u8,
) -> Result<u32, String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(serial) = state.pending[usize::from(which)].take() {
            return Ok(serial);
        }
        wayland::dispatch_until(queue, state, Instant::now() + Duration::from_millis(100))?;
    }
    Err(format!(
        "no configure for the {}",
        if which == PARENT { "parent" } else { "popup" }
    ))
}

fn run(socket: &str, mode: &str) -> Result<(), String> {
    let duration = std::env::var("POPUP_DURATION")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(60.0);
    let (parent_fill, popup_fill) = (
        color("PARENT_FILL", 0xff20_60c0),
        color("POPUP_FILL", 0xffe0_8020),
    );
    let conn = wayland::connect(socket)?;
    let (globals, mut queue) = registry_queue_init::<Client>(&conn).map_err(|e| e.to_string())?;
    let qh = queue.handle();
    let mut state = Client::default();
    let mut missing = Vec::new();
    let compositor = globals
        .bind::<wl_compositor::WlCompositor, _, _>(&qh, 1..=4, ())
        .map_err(|_| missing.push("wl_compositor"))
        .ok();
    let shm = globals
        .bind::<wl_shm::WlShm, _, _>(&qh, 1..=1, ())
        .map_err(|_| missing.push("wl_shm"))
        .ok();
    let seat = globals
        .bind::<wl_seat::WlSeat, _, _>(&qh, 1..=5, ())
        .map_err(|_| missing.push("wl_seat"))
        .ok();
    let wm = globals
        .bind::<xdg_wm_base::XdgWmBase, _, _>(&qh, 1..=2, ())
        .map_err(|_| missing.push("xdg_wm_base"))
        .ok();
    let layer_shell = globals
        .bind::<zwlr_layer_shell_v1::ZwlrLayerShellV1, _, _>(&qh, 1..=3, ())
        .ok();
    if mode == "layer" && layer_shell.is_none() {
        missing.push("zwlr_layer_shell_v1");
    }
    let (Some(compositor), Some(shm), Some(_seat), Some(wm)) = (compositor, shm, seat, wm) else {
        emit("error", &[("missing", Value::from(missing))]);
        std::process::exit(2);
    };
    if !missing.is_empty() {
        emit("error", &[("missing", Value::from(missing))]);
        std::process::exit(2);
    }
    queue.roundtrip(&mut state).map_err(|e| e.to_string())?;

    let parent = compositor.create_surface(&qh, ());
    let popup_surface = compositor.create_surface(&qh, ());
    state.parent = Some(parent.id());
    state.popup = Some(popup_surface.id());
    let mut kept = Vec::new();
    let show = |surface: &wl_surface::WlSurface,
                width: i32,
                height: i32,
                fill: u32,
                tag: &str,
                kept: &mut Vec<wl_buffer::WlBuffer>|
     -> Result<(), String> {
        let buffer = wayland::solid_buffer(&shm, &qh, width, height, fill)?;
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, width, height);
        surface.commit();
        kept.push(buffer);
        emit("mapped", &[("surface", Value::from(tag))]);
        Ok(())
    };
    // The 200x120 popup below the parent's top 300x40.
    let popup_xdg = wm.get_xdg_surface(&popup_surface, &qh, POPUP);
    let positioner = wm.create_positioner(&qh, ());
    positioner.set_size(200, 120);
    positioner.set_anchor_rect(0, 0, 300, 40);
    positioner.set_anchor(xdg_positioner::Anchor::BottomLeft);
    positioner.set_gravity(xdg_positioner::Gravity::BottomRight);

    let mut held: Vec<Box<dyn std::any::Any>> = Vec::new();
    let _popup = if mode == "layer" {
        let Some(shell) = layer_shell.as_ref() else {
            return Err("no layer shell".into());
        };
        let layer = shell.get_layer_surface(
            &parent,
            None,
            zwlr_layer_shell_v1::Layer::Top,
            "popup-test".into(),
            &qh,
            (),
        );
        layer.set_size(300, 40);
        layer.set_anchor(zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Left);
        parent.commit();
        let serial = configured(&mut queue, &mut state, PARENT)?;
        layer.ack_configure(serial);
        show(&parent, 300, 40, parent_fill, "parent", &mut kept)?;
        // A layer surface adopts a popup made with no parent of its own.
        let popup = popup_xdg.get_popup(None, &positioner, &qh, ());
        layer.get_popup(&popup);
        held.push(Box::new(layer));
        popup
    } else {
        let xdg = wm.get_xdg_surface(&parent, &qh, PARENT);
        let toplevel = xdg.get_toplevel(&qh, ());
        toplevel.set_title("popup-test".into());
        parent.commit();
        let serial = configured(&mut queue, &mut state, PARENT)?;
        xdg.ack_configure(serial);
        show(&parent, 300, 200, parent_fill, "parent", &mut kept)?;
        let popup = popup_xdg.get_popup(Some(&xdg), &positioner, &qh, ());
        held.push(Box::new((xdg, toplevel)));
        popup
    };
    popup_surface.commit();
    let serial = configured(&mut queue, &mut state, POPUP)?;
    popup_xdg.ack_configure(serial);
    show(&popup_surface, 200, 120, popup_fill, "popup", &mut kept)?;

    let deadline = Instant::now() + Duration::from_secs_f64(duration.max(0.0));
    wayland::dispatch_until(&mut queue, &mut state, deadline)?;
    drop(held);
    Ok(())
}

fn main() -> ExitCode {
    wayland::watch_for_termination();
    let mut args = std::env::args().skip(1);
    let socket = args.next().unwrap_or_else(|| "wayland-1".into());
    let mode = args
        .next()
        .or_else(|| std::env::var("POPUP_MODE").ok())
        .unwrap_or_else(|| "layer".into());
    match run(&socket, &mode) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("wl-popup-client: {e}");
            ExitCode::FAILURE
        }
    }
}
