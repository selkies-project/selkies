//! The panel, start menu, taskbar, and desktop layer of Selkies containers, over the Selkies
//! window manager, the labwc Wayland compositor, or the openbox X11 window manager.

mod audio;
mod backend;
mod clock;
mod config;
mod desktop;
mod entries;
mod event;
mod files;
mod i18n;
mod ipc;
mod portal;
mod toolkits;
mod tray;
mod ui;
mod watch;
mod wmtheme;
mod xkb;

use selkies_draw::{geom, icons, render};

use std::time::Instant;

use event::{Event, Request, SurfaceId};

/// The message shown for `--help`.
const USAGE: &str = "\
Usage: selkies-desktop [COMMAND]

Runs the Selkies desktop panel on the Wayland compositor or X display of the
session.  With a command, talks to the running panel instead:

  --toggle-menu    open or close the start menu
  --launcher       open or close the quick launcher
  --files [DIR]    open the file manager, on a folder when given
  --settings       open the settings window
  --show-desktop   minimize every window, or restore them
  --reload         read the applications and settings again
  --quit           exit the running panel
  --lookup-icon N  print the file an icon name resolves to and exit
  --version        print the version
  --help           print this message

Settings are read from ~/.config/selkies-desktop/config and from
SELKIES_DESKTOP_<KEY> environment variables; see the README.
";

/// Program entry point.  Without arguments runs the desktop; with a command
/// word sends it to the running desktop over the control socket.
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(arg) = args.first() {
        match arg.as_str() {
            "--help" | "-h" | "help" => {
                print!("{USAGE}");
                return;
            }
            "--version" | "-V" | "version" => {
                println!("selkies-desktop {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "--lookup-icon" => {
                let name = args.get(1).cloned().unwrap_or_default();
                let config = config::Config::load();
                let mut icons = icons::Icons::new(config.icon_theme.as_deref());
                match icons.locate(&name, 48) {
                    Some(path) => println!("{}", path.display()),
                    None => {
                        eprintln!("selkies-desktop: no icon named {name}");
                        std::process::exit(1);
                    }
                }
                return;
            }
            other => match ipc::Command::parse(&args.join(" ")) {
                Some(ipc::Command::Files(dir)) => {
                    let dir = dir.map(|d| std::fs::canonicalize(&d).unwrap_or(d));
                    if let Err(error) = ipc::send(ipc::Command::Files(dir)) {
                        eprintln!("selkies-desktop: {error}");
                        std::process::exit(1);
                    }
                    return;
                }
                Some(command) => {
                    if let Err(error) = ipc::send(command) {
                        eprintln!("selkies-desktop: {error}");
                        std::process::exit(1);
                    }
                    return;
                }
                None => {
                    eprintln!("selkies-desktop: unknown argument {other}\n{USAGE}");
                    std::process::exit(2);
                }
            },
        }
    }
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    std::process::exit(run());
}

/// Runs the desktop until the display goes away or a quit command arrives.
///
/// # Returns
/// The process exit code.
fn run() -> i32 {
    let mut desktop = desktop::Desktop::new();
    let Some(mut backend) = backend::connect() else {
        eprintln!("selkies-desktop: no Wayland layer shell compositor or X11 display available");
        return 1;
    };
    let debug = std::env::var_os("SELKIES_DESKTOP_DEBUG").is_some();
    desktop.opaque_output = backend.name() == "x11";
    if debug {
        eprintln!(
            "selkies-desktop: using the {} backend, language {}",
            backend.name(),
            i18n::language()
        );
    }
    let mut watcher = watch::Watcher::new();
    let server = ipc::Server::bind();
    let mut audio = if desktop.config.show_volume {
        Some(audio::Audio::new())
    } else {
        None
    };
    let mut tray = if desktop.config.show_tray {
        tray::Tray::new()
    } else {
        None
    };
    if debug && tray.is_none() {
        eprintln!("selkies-desktop: no system tray host");
    }

    loop {
        let now = Instant::now();
        desktop.tick(now);
        if watcher.take_due(now) {
            desktop.reload();
        }
        if let Some(audio) = audio.as_mut()
            && let Some(volume) = audio.poll(now)
        {
            desktop.set_volume(volume);
        }

        for request in desktop.take_requests() {
            match request {
                Request::SetVolume(percent) => {
                    if let Some(audio) = audio.as_mut() {
                        audio.set_volume(percent);
                    }
                }
                Request::ToggleMute => {
                    if let Some(audio) = audio.as_mut() {
                        audio.toggle_mute();
                    }
                }
                other => backend.perform(&other),
            }
        }
        let layered = backend.layered_desktop();
        for id in desktop.background_ids() {
            if layered {
                if let Some((wallpaper, launchers)) = desktop.render_background_layers(id) {
                    backend.present(SurfaceId::Background(id), &wallpaper);
                    backend.present_launchers(id, &launchers);
                }
            } else if let Some(frame) = desktop.render_background(id) {
                backend.present(SurfaceId::Background(id), &frame);
            }
        }
        {
            let (ox, oy, scale) = backend.panel_origin();
            desktop.panel_offset = (ox / scale.max(1), oy / scale.max(1));
        }
        if let Some(frame) = desktop.render_panel() {
            backend.present(SurfaceId::Panel, &frame);
        }
        if desktop.tray_moved
            && let Some(tray) = tray.as_mut()
        {
            desktop.tray_moved = false;
            let (ox, oy, scale) = backend.panel_origin();
            let slot = desktop.tray_slot.map(|r| {
                let r = r.scaled(scale);
                geom::Rect::new(r.x + ox, r.y + oy, r.w, r.h)
            });
            tray.place(slot, (desktop.panel_icon_size() + 4) * scale);
        }
        for id in desktop.window_ids() {
            if let Some(frame) = desktop.render_window(id) {
                backend.present(SurfaceId::Window(id), &frame);
            }
        }
        for request in desktop.take_requests() {
            backend.perform(&request);
        }

        if let Err(error) = backend.prepare() {
            eprintln!("selkies-desktop: {} connection lost: {error}", backend.name());
            return 1;
        }

        let mut deadline = desktop.next_deadline(now);
        for candidate in [watcher.deadline(), audio.as_ref().map(|a| a.deadline(now))]
            .into_iter()
            .flatten()
        {
            deadline = Some(deadline.map_or(candidate, |d| d.min(candidate)));
        }
        let timeout = if backend.pending() {
            0
        } else {
            deadline
                .map(|d| {
                    d.saturating_duration_since(Instant::now())
                        .as_millis()
                        .min(i32::MAX as u128) as i32
                })
                .unwrap_or(-1)
        };

        let mut fds = vec![libc::pollfd {
            fd: backend.fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        fds.push(libc::pollfd {
            fd: watcher.fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        let ipc_index = server.as_ref().map(|s| {
            fds.push(libc::pollfd {
                fd: s.fd(),
                events: libc::POLLIN,
                revents: 0,
            });
            fds.len() - 1
        });
        let audio_index = audio.as_ref().and_then(|a| a.fd()).map(|fd| {
            fds.push(libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            });
            fds.len() - 1
        });
        let tray_index = tray.as_ref().map(|t| {
            fds.push(libc::pollfd {
                fd: t.fd(),
                events: libc::POLLIN,
                revents: 0,
            });
            fds.len() - 1
        });
        let files_start = fds.len();
        for fd in desktop.files.fds() {
            fds.push(libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            });
        }

        let ret = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if ret < 0 {
            let interrupted = std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted;
            if let Err(error) = backend.dispatch(false) {
                eprintln!("selkies-desktop: {error}");
                return 1;
            }
            if interrupted {
                continue;
            }
            return 1;
        }

        let woke = Instant::now();
        let readable = fds[0].revents & libc::POLLIN != 0;
        let hangup = fds[0].revents & (libc::POLLERR | libc::POLLHUP) != 0;
        let events = match backend.dispatch(readable) {
            Ok(events) => events,
            Err(error) => {
                eprintln!("selkies-desktop: {} connection lost: {error}", backend.name());
                return 1;
            }
        };
        for event in events {
            if debug {
                eprintln!("selkies-desktop: event {event:?}");
            }
            if matches!(event, Event::Quit) {
                return 0;
            }
            desktop.handle(event);
        }
        if hangup {
            eprintln!("selkies-desktop: {} connection closed", backend.name());
            return 1;
        }

        if fds[1].revents & libc::POLLIN != 0 {
            watcher.drain();
        }
        if let Some(index) = ipc_index
            && fds[index].revents & libc::POLLIN != 0
            && let Some(server) = server.as_ref()
        {
            for command in server.drain() {
                match command {
                    ipc::Command::ToggleMenu => desktop.toggle_menu(),
                    ipc::Command::Launcher => desktop.toggle_launcher(),
                    ipc::Command::Reload => desktop.reload(),
                    ipc::Command::ShowDesktop => desktop.show_desktop(),
                    ipc::Command::Files(dir) => {
                        desktop.open_files(dir, None);
                    }
                    ipc::Command::Settings => desktop.open_popup(desktop::Popup::Settings),
                    ipc::Command::Quit => return 0,
                }
            }
        }
        if let Some(index) = audio_index
            && fds[index].revents & (libc::POLLIN | libc::POLLHUP) != 0
            && let Some(audio) = audio.as_mut()
        {
            audio.drain();
        }
        if let Some(index) = tray_index
            && fds[index].revents & (libc::POLLIN | libc::POLLHUP) != 0
            && let Some(t) = tray.as_mut()
            && t.dispatch()
        {
            desktop.set_tray_icons(t.count());
        }
        if fds[files_start..].iter().any(|fd| fd.revents & libc::POLLIN != 0) {
            desktop.files.drain_worker();
            desktop.files.drain_watch();
        }
        if debug {
            let took = woke.elapsed();
            if took > std::time::Duration::from_millis(50) {
                eprintln!("selkies-desktop: handling took {took:?}");
            }
        }
    }
}
