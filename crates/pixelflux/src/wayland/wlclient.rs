//! Shared plumbing for pixelflux's outbound Wayland CLIENT connections (the
//! virtual-keyboard typer and the data-control clipboard bridge, both talking to
//! a nested app compositor): socket resolution, deadline-bounded round-trips so a
//! wedged compositor turns into an error instead of a hang, and fd read/write
//! helpers for clipboard pipes.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use wayland_client::protocol::wl_callback;
use wayland_client::{Connection, Dispatch, EventQueue};

pub(crate) const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Absolute socket path for a Wayland display name (absolute paths pass through,
/// names join XDG_RUNTIME_DIR).
pub(crate) fn socket_path(name: &str) -> Option<String> {
    if name.starts_with('/') {
        return Some(name.to_string());
    }
    let rt = std::env::var("XDG_RUNTIME_DIR").ok()?;
    Some(format!("{}/{}", rt.trim_end_matches('/'), name))
}

/// Implemented by every client state so [`bounded_roundtrip`] can flag the sync
/// callback's completion; pair with [`impl_sync_callback`].
pub(crate) trait SyncState {
    fn sync_done_mut(&mut self) -> &mut bool;
}

/// `Dispatch<WlCallback>` for a [`SyncState`] type (a blanket impl would violate
/// the orphan rule, so each state stamps its own).
macro_rules! impl_sync_callback {
    ($t:ty) => {
        impl wayland_client::Dispatch<wayland_client::protocol::wl_callback::WlCallback, ()>
            for $t
        {
            fn event(
                state: &mut Self,
                _: &wayland_client::protocol::wl_callback::WlCallback,
                event: wayland_client::protocol::wl_callback::Event,
                _: &(),
                _: &wayland_client::Connection,
                _: &wayland_client::QueueHandle<Self>,
            ) {
                if let wayland_client::protocol::wl_callback::Event::Done { .. } = event {
                    *crate::wayland::wlclient::SyncState::sync_done_mut(state) = true;
                }
            }
        }
    };
}
pub(crate) use impl_sync_callback;

/// Block until `fd` is readable or `timeout` passes (false = timed out).
pub(crate) fn wait_readable(fd: RawFd, timeout: Duration) -> Result<bool, String> {
    loop {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let n = unsafe { libc::poll(&mut pfd, 1, timeout.as_millis().max(1) as libc::c_int) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(format!("poll: {err}"));
        }
        return Ok(n > 0);
    }
}

/// Poll two fds for readability at once (wayland socket + a wake pipe); returns
/// `(a_readable, b_readable)`, both false on timeout. Hangup counts as readable
/// so a closed wake pipe unblocks its waiter.
pub(crate) fn wait_readable2(
    a: RawFd,
    b: RawFd,
    timeout: Option<Duration>,
) -> Result<(bool, bool), String> {
    loop {
        let mut pfds = [
            libc::pollfd {
                fd: a,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: b,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let ms = timeout
            .map(|t| t.as_millis().max(1) as libc::c_int)
            .unwrap_or(-1);
        let n = unsafe { libc::poll(pfds.as_mut_ptr(), 2, ms) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(format!("poll: {err}"));
        }
        let hit = |r: libc::c_short| r & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0;
        return Ok((hit(pfds[0].revents), hit(pfds[1].revents)));
    }
}

/// Drain every pending byte from a wake pipe without blocking.
pub(crate) fn drain_pipe(fd: RawFd) {
    let mut buf = [0u8; 64];
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            return;
        }
    }
}

/// One wake byte, non-blocking; a full pipe already guarantees a pending wake.
pub(crate) fn wake_write(fd: RawFd) {
    let b = [1u8];
    unsafe { libc::write(fd, b.as_ptr() as *const libc::c_void, 1) };
}

/// `EventQueue::roundtrip` with a deadline: a wedged compositor becomes an error
/// instead of hanging the calling thread (and everything queued behind it).
pub(crate) fn bounded_roundtrip<S>(
    conn: &Connection,
    queue: &mut EventQueue<S>,
    state: &mut S,
) -> Result<(), String>
where
    S: SyncState + Dispatch<wl_callback::WlCallback, ()> + 'static,
{
    *state.sync_done_mut() = false;
    let _cb = conn.display().sync(&queue.handle(), ());
    let deadline = Instant::now() + IO_TIMEOUT;
    loop {
        queue
            .dispatch_pending(state)
            .map_err(|e| format!("dispatch: {e}"))?;
        if *state.sync_done_mut() {
            return Ok(());
        }
        queue.flush().map_err(|e| format!("flush: {e}"))?;
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or("compositor round-trip timed out")?;
        let Some(guard) = conn.prepare_read() else {
            continue;
        };
        if !wait_readable(guard.connection_fd().as_raw_fd(), remaining)? {
            return Err("compositor round-trip timed out".into());
        }
        guard.read().map_err(|e| format!("read: {e}"))?;
    }
}

/// Anonymous CLOEXEC memfd holding `data` (keymap uploads, shm-style payloads).
pub(crate) fn memfd_with(data: &[u8]) -> Result<OwnedFd, String> {
    let name = b"pixelflux-wl\0";
    let fd = unsafe { libc::memfd_create(name.as_ptr() as *const libc::c_char, libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(format!("memfd_create: {}", std::io::Error::last_os_error()));
    }
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut written = 0;
    while written < data.len() {
        let n = unsafe {
            libc::write(
                owned.as_raw_fd(),
                data[written..].as_ptr() as *const libc::c_void,
                data.len() - written,
            )
        };
        if n < 0 {
            return Err(format!("write memfd: {}", std::io::Error::last_os_error()));
        }
        written += n as usize;
    }
    Ok(owned)
}

/// CLOEXEC pipe as (read end, write end).
pub(crate) fn pipe_cloexec() -> Result<(OwnedFd, OwnedFd), String> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(format!("pipe2: {}", std::io::Error::last_os_error()));
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Non-blocking CLOEXEC pipe for wake signaling (read end, write end).
pub(crate) fn wake_pipe() -> Result<(OwnedFd, OwnedFd), String> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } < 0 {
        return Err(format!("pipe2: {}", std::io::Error::last_os_error()));
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Read `fd` to EOF. The deadline is per-chunk (idle), so a large transfer that
/// keeps flowing is never cut off while a stalled writer still errors out.
pub(crate) fn read_fd_to_end(fd: &OwnedFd, idle: Duration) -> Result<Vec<u8>, String> {
    read_fd_to_end_capped(fd, idle, READ_CAP_MAX)
}

/// Ceiling on an unbounded clipboard source, matching what the selkies clipboard protocol
/// allows: a hostile or stuck peer could otherwise stream bytes into memory forever.
const READ_CAP_MAX: usize = 64 * 1024 * 1024;

/// Same as `read_fd_to_end` but refuses a source larger than `cap`.
///
/// `cap` is inclusive: a payload of exactly `cap` bytes is accepted. The limit is therefore
/// tested against the size this chunk *would* produce, before appending — a test on the
/// already-accumulated length is reached again before EOF can be observed, so it would reject
/// a payload that exactly fills the cap.
pub(crate) fn read_fd_to_end_capped(
    fd: &OwnedFd,
    idle: Duration,
    cap: usize,
) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut chunk = [0u8; 65536];
    loop {
        if !wait_readable(fd.as_raw_fd(), idle)? {
            return Err("clipboard source stalled".into());
        }
        let n = unsafe {
            libc::read(
                fd.as_raw_fd(),
                chunk.as_mut_ptr() as *mut libc::c_void,
                chunk.len(),
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(format!("read: {err}"));
        }
        if n == 0 {
            return Ok(out);
        }
        if out.len() + n as usize > cap {
            return Err(format!("clipboard source exceeds the {cap} byte cap"));
        }
        out.extend_from_slice(&chunk[..n as usize]);
    }
}

/// Write all of `data` to `fd`, tolerating a slow reader up to `idle` per chunk.
/// EPIPE is success-shaped: the paster stopped reading, which is its right.
///
/// The fd is switched to non-blocking for the transfer (original flags are
/// restored on every exit path): a single blocking write() of a large payload
/// would stay parked in the kernel once the peer stops draining mid-transfer,
/// and the poll deadline could never fire. With O_NONBLOCK, write() returns
/// EAGAIN instead and the idle deadline bounds the whole transfer.
pub(crate) fn write_fd_all(fd: &OwnedFd, data: &[u8], idle: Duration) -> Result<(), String> {
    let raw = fd.as_raw_fd();
    let old_flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if old_flags < 0 {
        return Err(format!(
            "fcntl F_GETFL: {}",
            std::io::Error::last_os_error()
        ));
    }
    if unsafe { libc::fcntl(raw, libc::F_SETFL, old_flags | libc::O_NONBLOCK) } < 0 {
        return Err(format!(
            "fcntl F_SETFL: {}",
            std::io::Error::last_os_error()
        ));
    }
    let result = write_fd_all_nb(raw, data, idle);
    unsafe { libc::fcntl(raw, libc::F_SETFL, old_flags) };
    result
}

fn write_fd_all_nb(raw: i32, data: &[u8], idle: Duration) -> Result<(), String> {
    let mut written = 0;
    while written < data.len() {
        let mut pfd = libc::pollfd {
            fd: raw,
            events: libc::POLLOUT,
            revents: 0,
        };
        let n = unsafe { libc::poll(&mut pfd, 1, idle.as_millis().max(1) as libc::c_int) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(format!("poll: {err}"));
        }
        if n == 0 {
            return Err("clipboard reader stalled".into());
        }
        let w = unsafe {
            libc::write(
                raw,
                data[written..].as_ptr() as *const libc::c_void,
                data.len() - written,
            )
        };
        if w < 0 {
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) | Some(libc::EAGAIN) => continue,
                Some(libc::EPIPE) => return Ok(()),
                _ => return Err(format!("write: {err}")),
            }
        }
        written += w as usize;
    }
    Ok(())
}
