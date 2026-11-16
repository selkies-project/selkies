/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Wayland clients of a suite's own against the server's compositor: what the bins
//! (`wl-observer`, `wl-lock-probe`) share, and the handle a test drives one of them through.
//!
//! Each bin prints one JSON object per line, `{"kind": …, …}`, for what it was told; the
//! [`WlObserver`] keeps them for the test to wait on and read.

use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

/// The compositor socket `socket` names: a path, or a name under `XDG_RUNTIME_DIR`.
///
/// # Errors
///
/// When a bare name is given and `XDG_RUNTIME_DIR` is unset.
pub fn socket_path(socket: &str) -> Result<PathBuf, String> {
    if socket.contains('/') {
        return Ok(PathBuf::from(socket));
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is unset")?;
    Ok(PathBuf::from(runtime).join(socket))
}

/// A connection to the compositor at `socket`.
///
/// # Errors
///
/// When the socket does not accept the connection.
pub fn connect(socket: &str) -> Result<wayland_client::Connection, String> {
    let path = socket_path(socket)?;
    let stream = UnixStream::connect(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    wayland_client::Connection::from_socket(stream).map_err(|e| e.to_string())
}

/// Print one event line: `kind` and `fields`, flushed at once.
pub fn emit(kind: &str, fields: &[(&str, Value)]) {
    let mut line = Map::new();
    line.insert("kind".into(), Value::from(kind));
    for (key, value) in fields {
        line.insert((*key).into(), value.clone());
    }
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}", Value::Object(line));
    let _ = out.flush();
}

/// An anonymous file of `width` × `height` ARGB8888 pixels, each `fill` (zero leaves it
/// transparent), for a `wl_shm` pool.
///
/// # Errors
///
/// When the file cannot be made or written.
pub fn pixels(width: i32, height: i32, fill: u32) -> Result<OwnedFd, String> {
    // SAFETY: memfd_create with a NUL-terminated name and no flags returns a new descriptor or -1.
    let raw = unsafe { libc::memfd_create(c"selkies-wl-buffer".as_ptr(), 0) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    // SAFETY: `raw` is a descriptor this call just opened and nothing else owns.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let count =
        usize::try_from(width.max(0)).unwrap_or(0) * usize::try_from(height.max(0)).unwrap_or(0);
    let mut file = std::fs::File::from(fd);
    let row = fill.to_le_bytes().repeat(count.min(1 << 20));
    let mut left = count;
    while left > 0 {
        let take = left.min(1 << 20);
        file.write_all(&row[..take * 4])
            .map_err(|e| e.to_string())?;
        left -= take;
    }
    Ok(OwnedFd::from(file))
}

/// Whether SIGTERM has asked this process to end; [`watch_for_termination`] installs the handler.
pub fn terminated() -> bool {
    TERMINATED.load(std::sync::atomic::Ordering::Relaxed)
}

static TERMINATED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_term(_: libc::c_int) {
    TERMINATED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Turn SIGTERM into [`terminated`], so a bin ends its loop and cleans up as the driver stops it.
pub fn watch_for_termination() {
    // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
    unsafe {
        libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t);
    }
}

/// Dispatch `queue` until `deadline` or SIGTERM, waking at least every 100 ms.
///
/// # Errors
///
/// When the connection fails.
pub fn dispatch_until<S>(
    queue: &mut wayland_client::EventQueue<S>,
    state: &mut S,
    deadline: Instant,
) -> Result<(), String> {
    while Instant::now() < deadline && !terminated() {
        queue.flush().map_err(|e| e.to_string())?;
        queue.dispatch_pending(state).map_err(|e| e.to_string())?;
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let mut fds = [libc::pollfd {
            fd: std::os::fd::AsRawFd::as_raw_fd(&guard.connection_fd()),
            events: libc::POLLIN,
            revents: 0,
        }];
        // Never past the deadline, so a caller dispatching up to its next paint paints on time.
        let wait = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(100));
        let wait = libc::c_int::try_from(wait.as_millis()).unwrap_or(100);
        // SAFETY: one valid pollfd, for the length given.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 1, wait) };
        if ready > 0 {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e.to_string()),
            }
        } else {
            drop(guard);
        }
        queue.dispatch_pending(state).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// A Wayland client bin run against the compositor of a server, its event lines kept as they
/// arrive and the rest of what it prints (an error) in [`WlObserver::output`].
pub struct WlObserver {
    child: Child,
    lines: Arc<Mutex<Vec<Value>>>,
    output: Arc<Mutex<Vec<String>>>,
}

impl WlObserver {
    /// Run `bin` on `socket` under the runtime directory `runtime`, with `env` added.
    ///
    /// # Panics
    ///
    /// When the bin does not start.
    pub fn start(bin: &Path, socket: &str, runtime: &Path, env: &[(&str, &str)]) -> WlObserver {
        let mut command = Command::new(bin);
        command
            .arg(socket)
            .env("XDG_RUNTIME_DIR", runtime)
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        crate::server::tie_to_this_thread(&mut command);
        let mut child = command
            .spawn()
            .unwrap_or_else(|e| panic!("{} did not start: {e}", bin.display()));
        let lines = Arc::new(Mutex::new(Vec::new()));
        let output = Arc::new(Mutex::new(Vec::new()));
        if let Some(stdout) = child.stdout.take() {
            let (lines, output) = (Arc::clone(&lines), Arc::clone(&output));
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    let line = line.trim().to_owned();
                    match serde_json::from_str::<Value>(&line) {
                        Ok(value @ Value::Object(_)) => {
                            lines
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .push(value);
                        }
                        _ if !line.is_empty() => {
                            output
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .push(line);
                        }
                        _ => {}
                    }
                }
            });
        }
        if let Some(stderr) = child.stderr.take() {
            let output = Arc::clone(&output);
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if !line.trim().is_empty() {
                        output
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(line);
                    }
                }
            });
        }
        WlObserver {
            child,
            lines,
            output,
        }
    }

    /// The event lines so far.
    pub fn lines(&self) -> Vec<Value> {
        self.lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// How many event lines have arrived: a mark for [`WlObserver::lines_from`].
    pub fn count(&self) -> usize {
        self.lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// The event lines from index `mark` on.
    pub fn lines_from(&self, mark: usize) -> Vec<Value> {
        self.lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .skip(mark)
            .cloned()
            .collect()
    }

    /// Forget the event lines so far.
    pub fn clear(&self) {
        self.lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    /// What the bin printed that was not an event line.
    pub fn output(&self) -> Vec<String> {
        self.output
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The first event of `kind` within `timeout`.
    pub async fn wait_for(&self, kind: &str, timeout: Duration) -> Option<Value> {
        self.wait_for_match(kind, &Map::new(), timeout).await
    }

    /// The first event of `kind` whose fields named in `want` hold those values, within
    /// `timeout`.
    pub async fn wait_for_match(
        &self,
        kind: &str,
        want: &Map<String, Value>,
        timeout: Duration,
    ) -> Option<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            let found = self
                .lines
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .find(|l| {
                    l.get("kind").and_then(Value::as_str) == Some(kind)
                        && want
                            .iter()
                            .all(|(field, value)| l.get(field) == Some(value))
                })
                .cloned();
            if found.is_some() || Instant::now() >= deadline {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Whether the client's surface mapped within `timeout`.
    pub async fn ready(&self, timeout: Duration) -> bool {
        self.wait_for("mapped", timeout).await.is_some()
    }
}

impl Drop for WlObserver {
    fn drop(&mut self) {
        if let Ok(pid) = libc::pid_t::try_from(self.child.id()) {
            // SAFETY: a signal to the child this handle started and has not reaped.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A `wl_shm` buffer of `width` × `height` filled with `fill`, made through `shm`.
///
/// # Errors
///
/// When the pixels cannot be made.
pub fn solid_buffer<S>(
    shm: &wayland_client::protocol::wl_shm::WlShm,
    qh: &wayland_client::QueueHandle<S>,
    width: i32,
    height: i32,
    fill: u32,
) -> Result<wayland_client::protocol::wl_buffer::WlBuffer, String>
where
    S: wayland_client::Dispatch<wayland_client::protocol::wl_shm_pool::WlShmPool, ()>
        + wayland_client::Dispatch<wayland_client::protocol::wl_buffer::WlBuffer, ()>
        + 'static,
{
    painted_buffer(shm, qh, width, height, fill, None)
}

/// A rectangle painted over a buffer's fill: x, y, width, height and its ARGB8888 color.
#[derive(Clone, Copy, Debug)]
pub struct Block {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub color: u32,
}

impl Block {
    /// A block from `x,y,w,h,argb` (`500,100,300,200,ffff0000`).
    pub fn parse(spec: &str) -> Option<Block> {
        let parts: Vec<&str> = spec.trim().split(',').collect();
        let [x, y, width, height, color] = parts.as_slice() else {
            return None;
        };
        Some(Block {
            x: x.trim().parse().ok()?,
            y: y.trim().parse().ok()?,
            width: width.trim().parse().ok()?,
            height: height.trim().parse().ok()?,
            color: u32::from_str_radix(color.trim(), 16).ok()?,
        })
    }
}

/// [`solid_buffer`] with `block` painted over the fill.
///
/// # Errors
///
/// When the pixels cannot be written.
pub fn painted_buffer<S>(
    shm: &wayland_client::protocol::wl_shm::WlShm,
    qh: &wayland_client::QueueHandle<S>,
    width: i32,
    height: i32,
    fill: u32,
    block: Option<Block>,
) -> Result<wayland_client::protocol::wl_buffer::WlBuffer, String>
where
    S: wayland_client::Dispatch<wayland_client::protocol::wl_shm_pool::WlShmPool, ()>
        + wayland_client::Dispatch<wayland_client::protocol::wl_buffer::WlBuffer, ()>
        + 'static,
{
    let fd = pixels(width, height, fill)?;
    if let Some(block) = block {
        use std::os::unix::fs::FileExt;
        let file = std::fs::File::from(fd.try_clone().map_err(|e| e.to_string())?);
        let x = block.x.clamp(0, width);
        let span = usize::try_from(block.width.min(width - x).max(0)).unwrap_or(0);
        let row = block.color.to_le_bytes().repeat(span);
        for y in block.y.max(0)..block.y.saturating_add(block.height).min(height) {
            let at = (u64::try_from(y).unwrap_or(0) * u64::try_from(width).unwrap_or(0)
                + u64::try_from(x).unwrap_or(0))
                * 4;
            file.write_all_at(&row, at).map_err(|e| e.to_string())?;
        }
    }
    let size = width.saturating_mul(height).saturating_mul(4);
    let pool = shm.create_pool(fd.as_fd(), size, qh, ());
    let buffer = pool.create_buffer(
        0,
        width,
        height,
        width * 4,
        wayland_client::protocol::wl_shm::Format::Argb8888,
        qh,
        (),
    );
    pool.destroy();
    Ok(buffer)
}
