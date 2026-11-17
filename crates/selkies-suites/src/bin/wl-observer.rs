/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A Wayland client that reports what the seat tells it.
//!
//! `wl-observer SOCKET` maps an xdg_toplevel, fullscreen on one of the compositor's outputs, and
//! prints a JSON line per seat event it receives: `ptr_enter`/`ptr_motion` with the
//! surface-local position, `ptr_leave`, `ptr_button`, `ptr_axis`, `ptr_axis_source`,
//! `ptr_axis_stop`, `kbd_enter`, `kbd_leave`, `kbd_key`, `kbd_mods`, and `keymap` with the path
//! of a copy of the keymap (removed when it ends). `mapped` (its size, and whether the
//! compositor chose it) is printed once the surface shows, `done` when it ends.
//!
//! - `WLOBS_DURATION` seconds it runs (25), ending early on SIGTERM.
//! - `WLOBS_OUTPUT` the output, in announcement order, it goes fullscreen on (0).
//! - `WLOBS_MAXIMIZE=1` asks for a maximized window instead.
//! - `WLOBS_FILL` an ARGB8888 color (hex) the surface is painted; unset leaves it transparent.
//! - `WLOBS_BLOCK` a rectangle painted over the fill, `x,y,w,h,argb`, so the picture carries a
//!   second known color.
//! - `WLOBS_FILL2` and `WLOBS_BLINK_MS` a second fill the surface alternates with on that period,
//!   so a damage-driven capture keeps receiving frames from an otherwise still screen.
//! - `WLOBS_CURSOR` a cursor it sets on pointer enter, `SIZE` or `SIZExSCALE`: a black SIZE-pixel
//!   square drawn from a buffer SCALE times as large, so the compositor shows this client's cursor
//!   rather than its own (Mutter shows none over a surface that sets none).

use std::io::Read;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use serde_json::Value;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm,
    wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

use selkies_suites::wayland::{self, emit};

#[derive(Default)]
struct Observer {
    configured: bool,
    asked: (i32, i32),
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    keymaps: Vec<std::path::PathBuf>,
    /// The cursor surface set on every pointer enter, with its buffer kept alive beside it.
    cursor: Option<(wl_surface::WlSurface, wl_buffer::WlBuffer)>,
}

fn raw<T: Into<u32>>(value: WEnum<T>) -> u32 {
    match value {
        WEnum::Value(v) => v.into(),
        WEnum::Unknown(u) => u,
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Observer {
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

impl Dispatch<wl_seat::WlSeat, ()> for Observer {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        // Devices are taken only for the capabilities the seat announces, and when it announces
        // one later: a seat without a keyboard refuses get_keyboard with a protocol error, and
        // KWin's seat gains its pointer only once an emulated device joins.
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if state.keyboard.is_none() && caps.contains(wl_seat::Capability::Keyboard) {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
            if state.pointer.is_none() && caps.contains(wl_seat::Capability::Pointer) {
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for Observer {
    fn event(
        state: &mut Self,
        pointer: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface_x,
                surface_y,
                ..
            } => {
                if let Some((cursor, _)) = state.cursor.as_ref() {
                    pointer.set_cursor(serial, Some(cursor), 0, 0);
                }
                emit(
                    "ptr_enter",
                    &[("x", Value::from(surface_x)), ("y", Value::from(surface_y))],
                );
            }
            wl_pointer::Event::Leave { .. } => emit("ptr_leave", &[]),
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => emit(
                "ptr_motion",
                &[("x", Value::from(surface_x)), ("y", Value::from(surface_y))],
            ),
            wl_pointer::Event::Button { button, state, .. } => emit(
                "ptr_button",
                &[
                    ("button", Value::from(button)),
                    ("state", Value::from(raw(state))),
                ],
            ),
            wl_pointer::Event::Axis { axis, value, .. } => emit(
                "ptr_axis",
                &[
                    ("axis", Value::from(raw(axis))),
                    ("value", Value::from(value)),
                ],
            ),
            wl_pointer::Event::AxisSource { axis_source } => {
                emit(
                    "ptr_axis_source",
                    &[("source", Value::from(raw(axis_source)))],
                );
            }
            wl_pointer::Event::AxisStop { axis, .. } => {
                emit("ptr_axis_stop", &[("axis", Value::from(raw(axis)))]);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for Observer {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // The keymap the seat hands out is saved and its path reported: resolving a delivered
            // keycode the way an application does needs that text with the key and modifier
            // events that follow it.
            wl_keyboard::Event::Keymap { format, fd, size } => {
                let mut text = Vec::new();
                let _ = std::fs::File::from(fd)
                    .take(u64::from(size))
                    .read_to_end(&mut text);
                while text.last() == Some(&0) {
                    text.pop();
                }
                if size > 0 {
                    let dir = std::env::temp_dir();
                    let path = dir.join(format!(
                        "wlobs-keymap-{}-{}.xkb",
                        std::process::id(),
                        state.keymaps.len()
                    ));
                    if std::fs::write(&path, &text).is_ok() {
                        emit(
                            "keymap",
                            &[
                                ("format", Value::from(raw(format))),
                                ("path", Value::from(path.to_string_lossy().into_owned())),
                                ("size", Value::from(text.len())),
                            ],
                        );
                        state.keymaps.push(path);
                    }
                }
            }
            wl_keyboard::Event::Enter { .. } => emit("kbd_enter", &[]),
            wl_keyboard::Event::Leave { .. } => emit("kbd_leave", &[]),
            wl_keyboard::Event::Key {
                key, state: down, ..
            } => emit(
                "kbd_key",
                &[("key", Value::from(key)), ("state", Value::from(raw(down)))],
            ),
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => emit(
                "kbd_mods",
                &[
                    ("dep", Value::from(mods_depressed)),
                    ("lat", Value::from(mods_latched)),
                    ("lock", Value::from(mods_locked)),
                    ("group", Value::from(group)),
                ],
            ),
            _ => {}
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Observer {
    fn event(
        _: &mut Self,
        wm: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Mutter takes an unanswered ping as a hung client and stops sending it pointer input.
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for Observer {
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

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for Observer {
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

delegate_noop!(Observer: ignore wl_compositor::WlCompositor);
delegate_noop!(Observer: ignore wl_surface::WlSurface);
delegate_noop!(Observer: ignore wl_shm::WlShm);
delegate_noop!(Observer: ignore wl_shm_pool::WlShmPool);
delegate_noop!(Observer: ignore wl_buffer::WlBuffer);
delegate_noop!(Observer: ignore wl_output::WlOutput);

fn env_number(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn run(socket: &str) -> Result<(), String> {
    let duration = env_number("WLOBS_DURATION", 25.0);
    let output_index = std::env::var("WLOBS_OUTPUT")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let fill = std::env::var("WLOBS_FILL")
        .ok()
        .and_then(|v| u32::from_str_radix(v.trim(), 16).ok())
        .unwrap_or(0);
    let conn = wayland::connect(socket)?;
    let (globals, mut queue) = registry_queue_init::<Observer>(&conn).map_err(|e| e.to_string())?;
    let qh = queue.handle();
    let mut state = Observer::default();
    let Ok(_seat) = globals.bind::<wl_seat::WlSeat, _, _>(&qh, 1..=7, ()) else {
        emit("fatal", &[("error", Value::from("no wl_seat"))]);
        return Err("no wl_seat".into());
    };
    let compositor: wl_compositor::WlCompositor = globals
        .bind(&qh, 1..=4, ())
        .map_err(|e| format!("wl_compositor: {e}"))?;
    let shm: wl_shm::WlShm = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("wl_shm: {e}"))?;
    let wm: xdg_wm_base::XdgWmBase = globals
        .bind(&qh, 1..=6, ())
        .map_err(|e| format!("xdg_wm_base: {e}"))?;
    let output = globals.contents().with_list(|list| {
        list.iter()
            .filter(|g| g.interface == "wl_output")
            .nth(output_index)
            .map(|g| (g.name, g.version))
    });
    let output = output.map(|(name, version)| {
        globals
            .registry()
            .bind::<wl_output::WlOutput, _, _>(name, version.min(4), &qh, ())
    });
    queue.roundtrip(&mut state).map_err(|e| e.to_string())?;

    if let Ok(spec) = std::env::var("WLOBS_CURSOR")
        && !spec.trim().is_empty()
    {
        let (edge, scale) = spec.trim().split_once('x').unwrap_or((spec.trim(), "1"));
        let scale: i32 = scale.parse().unwrap_or(1).max(1);
        let px = edge.parse::<i32>().unwrap_or(24) * scale;
        let buffer = wayland::solid_buffer(&shm, &qh, px, px, 0xff00_0000)?;
        let cursor = compositor.create_surface(&qh, ());
        cursor.set_buffer_scale(scale);
        cursor.attach(Some(&buffer), 0, 0);
        cursor.commit();
        state.cursor = Some((cursor, buffer));
    }

    let surface = compositor.create_surface(&qh, ());
    let xdg = wm.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_title("selkies-wl-observer".into());
    // Fullscreen so pointer motion lands on it wherever the compositor would place it.
    if std::env::var("WLOBS_MAXIMIZE").as_deref() == Ok("1") {
        toplevel.set_maximized();
    } else {
        toplevel.set_fullscreen(output.as_ref());
    }
    surface.commit();
    for _ in 0..100 {
        queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
        if state.configured {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // The buffer takes the configured size: a mismatched buffer is scaled or clipped by the
    // compositor, which skews every surface-local coordinate reported. The fallback covers a
    // compositor that leaves the size to the client.
    let (width, height) = if state.asked.0 > 0 {
        state.asked
    } else {
        (1280, 2160)
    };
    // As the Python observer did, the block is painted over a fill only, never over a
    // transparent surface.
    let block = std::env::var("WLOBS_BLOCK")
        .ok()
        .and_then(|spec| wayland::Block::parse(&spec));
    let painted = |color: u32| {
        wayland::painted_buffer(
            &shm,
            &qh,
            width,
            height,
            color,
            if color == 0 { None } else { block },
        )
    };
    let blink = Duration::from_millis(
        std::env::var("WLOBS_BLINK_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0),
    );
    let fill2 = std::env::var("WLOBS_FILL2")
        .ok()
        .and_then(|v| u32::from_str_radix(v.trim(), 16).ok())
        .unwrap_or(0);
    let mut buffers = vec![painted(fill)?];
    if !blink.is_zero() {
        buffers.push(painted(fill2)?);
    }
    let buffer = &buffers[0];
    surface.attach(Some(buffer), 0, 0);
    surface.commit();
    queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
    emit(
        "mapped",
        &[
            ("w", Value::from(width)),
            ("h", Value::from(height)),
            ("configured", Value::from(state.asked.0 > 0)),
        ],
    );
    let deadline = Instant::now() + Duration::from_secs_f64(duration.max(0.0));
    let ended = if blink.is_zero() {
        wayland::dispatch_until(&mut queue, &mut state, deadline)
    } else {
        // The two buffers alternate on the blink period, the seat's events dispatched between.
        let mut shown = 0;
        let mut next = Instant::now() + blink;
        loop {
            if let Err(e) = wayland::dispatch_until(&mut queue, &mut state, next.min(deadline)) {
                break Err(e);
            }
            if Instant::now() >= deadline || wayland::terminated() {
                break Ok(());
            }
            shown ^= 1;
            surface.attach(Some(&buffers[shown]), 0, 0);
            surface.damage_buffer(0, 0, width, height);
            surface.commit();
            next += blink;
        }
    };
    emit("done", &[]);
    for path in &state.keymaps {
        let _ = std::fs::remove_file(path);
    }
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
            eprintln!("wl-observer: {e}");
            ExitCode::FAILURE
        }
    }
}
