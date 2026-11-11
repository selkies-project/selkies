//! A pointer the end to end checks hold open while they look: `wlrctl` creates
//! and destroys a virtual pointer per command, and the compositor sends the
//! desktop a leave when the device goes, which clears every hover state before
//! a screenshot can show it.  This tool keeps one virtual pointer alive and
//! reads commands from standard input until it closes.
//!
//! Commands, one per line: `move X Y` (absolute, logical pixels), `click
//! [left|right|middle]`, `press`, `release`, `scroll DY`, `sleep MS`, `shot
//! FILE` (runs `grim`, of the output named by `DRIVER_OUTPUT` when set),
//! `exit`.

use std::io::BufRead;
use std::time::{Duration, Instant};

use wayland_client::protocol::{wl_output, wl_pointer, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1, zwlr_virtual_pointer_v1,
};

struct State {
    manager: Option<zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    width: u32,
    height: u32,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
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
            match interface.as_str() {
                "zwlr_virtual_pointer_manager_v1" => {
                    state.manager = Some(registry.bind(name, version.min(2), qh, ()));
                }
                "wl_seat" => state.seat = Some(registry.bind(name, 1, qh, ())),
                "wl_output" => {
                    registry.bind::<wl_output::WlOutput, _, _>(name, version.min(2), qh, ());
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Mode { width, height, .. } = event {
            state.width = width as u32;
            state.height = height as u32;
        }
    }
}

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1);
delegate_noop!(State: ignore zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1);

fn main() {
    let conn = Connection::connect_to_env().expect("wayland connection");
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut state = State {
        manager: None,
        seat: None,
        width: 0,
        height: 0,
    };
    queue.roundtrip(&mut state).unwrap();
    queue.roundtrip(&mut state).unwrap();
    let manager = state
        .manager
        .clone()
        .expect("compositor has no virtual pointer protocol");
    let pointer = manager.create_virtual_pointer(state.seat.as_ref(), &qh, ());
    let started = Instant::now();
    let now = |started: Instant| started.elapsed().as_millis() as u32;
    let (mut extent_w, mut extent_h) = (state.width.max(1), state.height.max(1));
    if let Ok(size) = std::env::var("DRIVER_EXTENT")
        && let Some((w, h)) = size.split_once('x')
    {
        extent_w = w.parse().unwrap_or(extent_w);
        extent_h = h.parse().unwrap_or(extent_h);
    }
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line.unwrap_or_default();
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            ["move", x, y] => {
                let (x, y): (u32, u32) = (x.parse().unwrap_or(0), y.parse().unwrap_or(0));
                pointer.motion_absolute(now(started), x, y, extent_w, extent_h);
                pointer.frame();
            }
            ["click"] | ["click", _] => {
                let button = match words.get(1).copied().unwrap_or("left") {
                    "right" => 0x111,
                    "middle" => 0x112,
                    _ => 0x110,
                };
                pointer.button(now(started), button, wl_pointer::ButtonState::Pressed);
                pointer.frame();
                conn.flush().unwrap();
                std::thread::sleep(Duration::from_millis(40));
                pointer.button(now(started), button, wl_pointer::ButtonState::Released);
                pointer.frame();
            }
            ["press"] => {
                pointer.button(now(started), 0x110, wl_pointer::ButtonState::Pressed);
                pointer.frame();
            }
            ["release"] => {
                pointer.button(now(started), 0x110, wl_pointer::ButtonState::Released);
                pointer.frame();
            }
            ["scroll", dy] => {
                let dy: f64 = dy.parse().unwrap_or(0.0);
                pointer.axis_source(wl_pointer::AxisSource::Wheel);
                pointer.axis(now(started), wl_pointer::Axis::VerticalScroll, dy);
                pointer.axis_discrete(
                    now(started),
                    wl_pointer::Axis::VerticalScroll,
                    dy,
                    if dy < 0.0 { -1 } else { 1 },
                );
                pointer.frame();
            }
            ["sleep", ms] => {
                conn.flush().unwrap();
                std::thread::sleep(Duration::from_millis(ms.parse().unwrap_or(0)));
            }
            ["shot", path] => {
                conn.flush().unwrap();
                let mut grim = std::process::Command::new("grim");
                if let Ok(output) = std::env::var("DRIVER_OUTPUT") {
                    grim.args(["-o", &output]);
                }
                let _ = grim.arg(path).status();
            }
            ["exit"] => break,
            [] => {}
            other => eprintln!("driver: unknown command {other:?}"),
        }
        conn.flush().unwrap();
        let _ = queue.dispatch_pending(&mut state);
    }
    pointer.destroy();
    conn.flush().unwrap();
}
