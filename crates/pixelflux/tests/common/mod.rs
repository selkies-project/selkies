/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! What the capture tests share: a private runtime directory, a throwaway X server, and the
//! checks' outcomes. Each test binary that starts the in-process compositor is one of its own,
//! the compositor being one per process.

#![allow(dead_code)]

use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// A directory removed when the test ends.
pub struct Scratch(pub PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A private XDG_RUNTIME_DIR for the compositor's socket, named for the test.
pub fn runtime_dir(name: &str) -> Scratch {
    let dir =
        Scratch(std::env::temp_dir().join(format!("pixelflux-{name}-{}", std::process::id())));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir.0)
        .expect("a private runtime directory");
    dir
}

/// A throwaway X server, stopped when the test ends.
pub struct Xvfb(Child);

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `Xvfb` on PATH, on a free display number, at `width`x`height`, and its display name.
pub fn xvfb(width: i32, height: i32) -> (Xvfb, String) {
    let number = (140..200)
        .find(|n| {
            !PathBuf::from(format!("/tmp/.X11-unix/X{n}")).exists()
                && !PathBuf::from(format!("/tmp/.X{n}-lock")).exists()
        })
        .expect("a free display number");
    let display = format!(":{number}");
    let child = Command::new("Xvfb")
        .args([
            display.as_str(),
            "-screen",
            "0",
            &format!("{width}x{height}x24"),
            "-nolisten",
            "tcp",
        ])
        .args([
            "+extension",
            "DAMAGE",
            "+extension",
            "XFIXES",
            "+extension",
            "MIT-SHM",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("Xvfb on PATH");
    let server = Xvfb(child);
    let socket = PathBuf::from(format!("/tmp/.X11-unix/X{number}"));
    let deadline = Instant::now() + Duration::from_secs(15);
    while !socket.exists() {
        assert!(Instant::now() < deadline, "Xvfb {display} never listened");
        thread::sleep(Duration::from_millis(100));
    }
    (server, display)
}

/// Points the process at `runtime` and, where given, at `display`, with no other session's
/// variables steering the capture.
///
/// # Safety
/// Called before the test starts a thread, a capture, or the compositor.
pub unsafe fn isolate(runtime: &Scratch, display: Option<&str>) {
    // SAFETY: the caller holds that nothing else reads the environment yet.
    unsafe {
        std::env::set_var("XDG_RUNTIME_DIR", &runtime.0);
        match display {
            Some(display) => std::env::set_var("DISPLAY", display),
            None => std::env::remove_var("DISPLAY"),
        }
        for name in ["WAYLAND_DISPLAY", "PIXELFLUX_CU", "PIXELFLUX_RECORD"] {
            std::env::remove_var(name);
        }
    }
}

/// Every check's outcome, printed as it is made, so one failure does not hide the rest.
#[derive(Default)]
pub struct Results(pub Vec<String>);

impl Results {
    pub fn check(&mut self, label: &str, ok: bool, detail: impl std::fmt::Debug) {
        println!("{}  {label}  {detail:?}", if ok { "PASS" } else { "FAIL" });
        if !ok {
            self.0.push(label.to_string());
        }
    }

    /// Fails the test with every failed check's label.
    pub fn finish(self) {
        assert!(self.0.is_empty(), "failed: {:?}", self.0);
    }
}

/// Waits up to `timeout` for `ready`, polling.
pub fn wait_until(timeout: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while !ready() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
    true
}

/// A nested labwc on the compositor's socket, stopped when the test ends.
pub struct Labwc(Child);

impl Drop for Labwc {
    fn drop(&mut self) {
        // SAFETY: a signal to a child this test spawned and has not reaped.
        unsafe { libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if matches!(self.0.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `labwc` on PATH, nested on `socket` under `runtime` with wlroots' Wayland backend and the
/// pixman renderer, `env` added, its output in `runtime`/labwc.log.
pub fn labwc(runtime: &Scratch, socket: &str, env: &[(&str, &str)], args: &[&str]) -> Labwc {
    let log = std::fs::File::create(runtime.0.join("labwc.log")).expect("labwc's log");
    let mut command = Command::new("labwc");
    command
        .args(args)
        .env("WAYLAND_DISPLAY", socket)
        .env("WLR_BACKENDS", "wayland")
        .env("XDG_RUNTIME_DIR", &runtime.0)
        .env("WLR_RENDERER", "pixman")
        .env("LIBGL_ALWAYS_SOFTWARE", "1")
        .env_remove("DISPLAY")
        .stdout(log.try_clone().expect("labwc's log"))
        .stderr(log);
    for (name, value) in env {
        command.env(name, value);
    }
    Labwc(command.spawn().expect("labwc on PATH"))
}

/// The nested compositor's own socket under `runtime`: the one that is not `capture`.
pub fn inner_socket(runtime: &Scratch, capture: &str) -> Option<String> {
    let mut names: Vec<String> = std::fs::read_dir(&runtime.0)
        .ok()?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.starts_with("wayland-") && !name.ends_with(".lock") && name != capture)
        .collect();
    names.sort();
    names.into_iter().next()
}

/// The last lines of `runtime`/labwc.log, for a failure's detail.
pub fn labwc_log(runtime: &Scratch) -> String {
    let text = std::fs::read_to_string(runtime.0.join("labwc.log")).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(15)..].join("\n")
}

/// A Wayland client of a compositor, fullscreen on one of its outputs, recording the pointer
/// events it is sent: what a desktop's application sees of the pointer. Its surface takes the
/// configured size (1280x2160 where the compositor leaves the size to the client), so the
/// surface-local coordinates it records are the compositor's logical ones.
pub mod observer {
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use wayland_client::globals::{GlobalListContents, registry_queue_init};
    use wayland_client::protocol::{
        wl_buffer, wl_compositor, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool,
        wl_surface,
    };
    use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
    use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

    /// A pointer event the client was sent.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub enum Event {
        Motion(f64, f64),
        /// The button and its state, 1 pressed and 0 released.
        Button(u32, u32),
    }

    /// The mapped surface: its size, and whether the compositor configured it.
    pub type Mapped = (i32, i32, bool);

    #[derive(Default)]
    struct Shared {
        mapped: Option<Mapped>,
        events: Vec<Event>,
    }

    struct State {
        shared: Arc<Mutex<Shared>>,
        configured: bool,
        asked: (i32, i32),
        pointer: Option<wl_pointer::WlPointer>,
    }

    pub struct Observer {
        shared: Arc<Mutex<Shared>>,
        stop: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl Observer {
        /// Connects to the compositor at `socket`, fullscreen on its `output`th output in
        /// announcement order.
        pub fn start(socket: PathBuf, output: usize) -> Self {
            let shared = Arc::new(Mutex::new(Shared::default()));
            let stop = Arc::new(AtomicBool::new(false));
            let thread = {
                let (shared, stop) = (shared.clone(), stop.clone());
                thread::spawn(move || {
                    if let Err(e) = run(&socket, output, shared, &stop) {
                        eprintln!("observer: {e}");
                    }
                })
            };
            Self {
                shared,
                stop,
                thread: Some(thread),
            }
        }

        /// The mapped surface once it is, waiting up to `timeout`.
        pub fn ready(&self, timeout: Duration) -> Option<Mapped> {
            let deadline = Instant::now() + timeout;
            loop {
                if let Some(mapped) = self.shared.lock().expect("the observer").mapped {
                    return Some(mapped);
                }
                if Instant::now() >= deadline {
                    return None;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }

        pub fn clear(&self) {
            self.shared.lock().expect("the observer").events.clear();
        }

        pub fn events(&self) -> Vec<Event> {
            self.shared.lock().expect("the observer").events.clone()
        }

        /// Waits up to `timeout` for a button event in `state`.
        pub fn wait_button(&self, state: u32, timeout: Duration) -> bool {
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                if self
                    .events()
                    .iter()
                    .any(|e| matches!(e, Event::Button(_, s) if *s == state))
                {
                    return true;
                }
                thread::sleep(Duration::from_millis(100));
            }
            false
        }
    }

    impl Drop for Observer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn run(
        socket: &PathBuf,
        output: usize,
        shared: Arc<Mutex<Shared>>,
        stop: &AtomicBool,
    ) -> Result<(), String> {
        let stream =
            UnixStream::connect(socket).map_err(|e| format!("{}: {e}", socket.display()))?;
        let conn = Connection::from_socket(stream).map_err(|e| e.to_string())?;
        let (globals, mut queue) =
            registry_queue_init::<State>(&conn).map_err(|e| e.to_string())?;
        let qh = queue.handle();
        let compositor: wl_compositor::WlCompositor = globals
            .bind(&qh, 1..=4, ())
            .map_err(|e| format!("wl_compositor: {e}"))?;
        let shm: wl_shm::WlShm = globals
            .bind(&qh, 1..=1, ())
            .map_err(|e| format!("wl_shm: {e}"))?;
        let wm: xdg_wm_base::XdgWmBase = globals
            .bind(&qh, 1..=2, ())
            .map_err(|e| format!("xdg_wm_base: {e}"))?;
        let _seat: wl_seat::WlSeat = globals
            .bind(&qh, 1..=7, ())
            .map_err(|e| format!("wl_seat: {e}"))?;
        let outputs: Vec<_> = globals
            .contents()
            .clone_list()
            .into_iter()
            .filter(|g| g.interface == "wl_output")
            .collect();
        let target = outputs
            .get(output)
            .ok_or_else(|| format!("no output {output}"))?;
        let output: wl_output::WlOutput =
            globals
                .registry()
                .bind(target.name, target.version.min(4), &qh, ());
        let surface = compositor.create_surface(&qh, ());
        let xdg = wm.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg.get_toplevel(&qh, ());
        toplevel.set_title("pixelflux-test-observer".into());
        toplevel.set_fullscreen(Some(&output));
        surface.commit();
        let mut state = State {
            shared: shared.clone(),
            configured: false,
            asked: (0, 0),
            pointer: None,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !state.configured && Instant::now() < deadline {
            queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
            thread::sleep(Duration::from_millis(50));
        }
        let configured = state.asked.0 > 0 && state.asked.1 > 0;
        let (width, height) = if configured {
            state.asked
        } else {
            (1280, 2160)
        };
        let stride = width * 4;
        let size = usize::try_from(stride * height).map_err(|e| e.to_string())?;
        let memfd = memfd(size)?;
        let pool = shm.create_pool(memfd.as_fd(), stride * height, &qh, ());
        let buffer =
            pool.create_buffer(0, width, height, stride, wl_shm::Format::Argb8888, &qh, ());
        pool.destroy();
        surface.attach(Some(&buffer), 0, 0);
        surface.commit();
        queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
        shared.lock().expect("the observer").mapped = Some((width, height, configured));
        let fd = conn
            .backend()
            .poll_fd()
            .try_clone_to_owned()
            .map_err(|e| e.to_string())?;
        let mut fds = [libc::pollfd {
            fd: std::os::fd::AsRawFd::as_raw_fd(&fd),
            events: libc::POLLIN,
            revents: 0,
        }];
        while !stop.load(Ordering::Relaxed) {
            conn.flush().map_err(|e| e.to_string())?;
            if let Some(guard) = queue.prepare_read() {
                // SAFETY: one pollfd that outlives the call.
                if unsafe { libc::poll(fds.as_mut_ptr(), 1, 100) } > 0 {
                    guard.read().map_err(|e| e.to_string())?;
                }
            }
            queue
                .dispatch_pending(&mut state)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// A zeroed, transparent buffer's memory.
    fn memfd(size: usize) -> Result<std::os::fd::OwnedFd, String> {
        // SAFETY: a NUL-terminated name; the descriptor is owned from here on.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_memfd_create,
                c"observer".as_ptr(),
                libc::MFD_CLOEXEC,
            )
        } as i32;
        if fd < 0 {
            return Err("memfd_create failed".into());
        }
        // SAFETY: a descriptor this function just created.
        let owned = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
        let file = std::fs::File::from(owned);
        file.set_len(size as u64).map_err(|e| e.to_string())?;
        Ok(file.into())
    }

    impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
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

    impl Dispatch<wl_seat::WlSeat, ()> for State {
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
                && caps.contains(wl_seat::Capability::Pointer)
                && state.pointer.is_none()
            {
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
        }
    }

    impl Dispatch<wl_pointer::WlPointer, ()> for State {
        fn event(
            state: &mut Self,
            _: &wl_pointer::WlPointer,
            event: wl_pointer::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            let recorded = match event {
                wl_pointer::Event::Motion {
                    surface_x,
                    surface_y,
                    ..
                } => Some(Event::Motion(surface_x, surface_y)),
                wl_pointer::Event::Button {
                    button,
                    state: WEnum::Value(pressed),
                    ..
                } => Some(Event::Button(
                    button,
                    u32::from(pressed == wl_pointer::ButtonState::Pressed),
                )),
                _ => None,
            };
            if let Some(event) = recorded {
                state
                    .shared
                    .lock()
                    .expect("the observer")
                    .events
                    .push(event);
            }
        }
    }

    impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
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

    impl Dispatch<xdg_surface::XdgSurface, ()> for State {
        fn event(
            state: &mut Self,
            xdg: &xdg_surface::XdgSurface,
            event: xdg_surface::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            if let xdg_surface::Event::Configure { serial } = event {
                xdg.ack_configure(serial);
                state.configured = true;
            }
        }
    }

    impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
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

    macro_rules! ignore_events {
        ($($ty:ty),*) => {$(
            impl Dispatch<$ty, ()> for State {
                fn event(_: &mut Self, _: &$ty, _: <$ty as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
            }
        )*};
    }
    ignore_events!(
        wl_compositor::WlCompositor,
        wl_shm::WlShm,
        wl_shm_pool::WlShmPool,
        wl_buffer::WlBuffer,
        wl_surface::WlSurface,
        wl_output::WlOutput
    );
}
