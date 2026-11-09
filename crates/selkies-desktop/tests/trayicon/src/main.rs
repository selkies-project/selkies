//! An XEmbed tray icon for the end to end checks: a square that asks the
//! tray selection owner to dock it, paints itself in one color and switches
//! to another on every click, printing each click to standard output.  A
//! `redock` line on standard input destroys the window and docks a new one
//! at once, the way Steam does.  It runs until standard input closes.

use std::io::BufRead;
use std::sync::mpsc;

use x11rb::connection::Connection as _;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

/// Dock request opcode.
const SYSTEM_TRAY_REQUEST_DOCK: u32 = 0;

fn main() {
    let (conn, screen_num) = RustConnection::connect(None).expect("X display");
    let screen = conn.setup().roots[screen_num].clone();
    let selection = conn
        .intern_atom(false, format!("_NET_SYSTEM_TRAY_S{screen_num}").as_bytes())
        .unwrap()
        .reply()
        .unwrap()
        .atom;
    let opcode = conn
        .intern_atom(false, b"_NET_SYSTEM_TRAY_OPCODE")
        .unwrap()
        .reply()
        .unwrap()
        .atom;
    let owner = conn
        .get_selection_owner(selection)
        .unwrap()
        .reply()
        .unwrap()
        .owner;
    if owner == x11rb::NONE {
        eprintln!("trayicon: no tray owns {selection}");
        std::process::exit(1);
    }
    let colors = [0xff3b82f6u32, 0xfff59e0b, 0xff10b981];
    let mut current = 0usize;
    let dock = |conn: &RustConnection, color: u32| -> xproto::Window {
        let window = conn.generate_id().unwrap();
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            screen.root,
            0,
            0,
            22,
            22,
            0,
            xproto::WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &xproto::CreateWindowAux::new().background_pixel(color).event_mask(
                xproto::EventMask::BUTTON_PRESS
                    | xproto::EventMask::STRUCTURE_NOTIFY
                    | xproto::EventMask::EXPOSURE,
            ),
        )
        .unwrap();
        let request = xproto::ClientMessageEvent::new(
            32,
            owner,
            opcode,
            [x11rb::CURRENT_TIME, SYSTEM_TRAY_REQUEST_DOCK, window, 0, 0],
        );
        conn.send_event(false, owner, xproto::EventMask::NO_EVENT, request)
            .unwrap();
        conn.flush().unwrap();
        println!("docked {window}");
        window
    };
    let mut window = dock(&conn, colors[current]);

    let (lines_tx, lines_rx) = mpsc::channel::<Option<String>>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let _ = lines_tx.send(Some(line));
        }
        let _ = lines_tx.send(None);
    });
    loop {
        match lines_rx.try_recv() {
            Ok(None) | Err(mpsc::TryRecvError::Disconnected) => break,
            Ok(Some(line)) if line.trim() == "redock" => {
                let _ = conn.destroy_window(window);
                window = dock(&conn, colors[current]);
            }
            _ => {}
        }
        match conn.poll_for_event() {
            Ok(Some(Event::ButtonPress(e))) => {
                current = (current + 1) % colors.len();
                println!("click {} at {},{}", e.detail, e.event_x, e.event_y);
                let _ = conn.change_window_attributes(
                    window,
                    &xproto::ChangeWindowAttributesAux::new().background_pixel(colors[current]),
                );
                let _ = conn.clear_area(false, window, 0, 0, 0, 0);
                let _ = conn.flush();
            }
            Ok(Some(Event::DestroyNotify(_))) => {}
            Ok(Some(Event::ReparentNotify(e))) => println!("reparented into {}", e.parent),
            Ok(Some(_)) => {}
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
            Err(_) => break,
        }
    }
}
