//! Control socket for the Selkies V4L2 interposer.
//!
//! Each application `open()` of the virtual device becomes one client connection. The server sends
//! the ring configuration once, with the ring memfd attached as `SCM_RIGHTS` ancillary data, then
//! only ever writes one-byte doorbells — one per published frame — so the interposer can block in
//! `poll()`/`VIDIOC_DQBUF` on plain socket readability. Frame bytes never cross the socket. The
//! interposer answers the handshake with a single byte; until that arrives a client gets no
//! doorbells, and a closed connection is retired the moment its read end reports hangup.
//!
//! The accept/hangup loop runs on its own thread, woken through an eventfd for shutdown, so a
//! process that opens the device while no frames flow is still served immediately.

use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixListener;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use super::ring::CONFIG_SIZE;

struct Client {
    fd: OwnedFd,
    /// Set once the interposer's handshake byte arrived; only ready clients receive doorbells.
    ready: bool,
}

struct Shared {
    clients: Mutex<Vec<Client>>,
    ready_count: AtomicUsize,
    stop: AtomicBool,
}

pub struct Server {
    path: String,
    shared: Arc<Shared>,
    wake: OwnedFd,
    thread: Option<JoinHandle<()>>,
}

impl Server {
    /// Bind `path` (replacing a stale socket file) and start serving `config` + `ring_fd`.
    ///
    /// A file there that another account owns is refused: in a shared directory it is that
    /// account's listener, and every camera client of this session would reach it.
    pub fn bind(path: &str, config: [u8; CONFIG_SIZE], ring_fd: RawFd) -> io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        if let Some(dir) = std::path::Path::new(path).parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir)?;
        }
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.uid() != unsafe { libc::geteuid() } => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{path} belongs to uid {}, not this session", meta.uid()),
                ));
            }
            Ok(_) => std::fs::remove_file(path)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let listener = UnixListener::bind(path)?;
        restrict_to_owner(path)?;
        listener.set_nonblocking(true)?;
        let wake_raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if wake_raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let wake = unsafe { OwnedFd::from_raw_fd(wake_raw) };
        let shared = Arc::new(Shared {
            clients: Mutex::new(Vec::new()),
            ready_count: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
        });
        let t_shared = shared.clone();
        let t_wake = wake.as_raw_fd();
        let thread = thread::Builder::new()
            .name("pixelflux-webcam-ctl".into())
            .spawn(move || serve(listener, t_wake, config, ring_fd, t_shared))?;
        Ok(Server {
            path: path.to_string(),
            shared,
            wake,
            thread: Some(thread),
        })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// Number of interposer clients that completed the handshake.
    pub fn client_count(&self) -> usize {
        self.shared.ready_count.load(Ordering::Relaxed)
    }

    /// Wake every ready client: one byte per published frame. A full socket buffer means a
    /// wakeup is already pending, so `EAGAIN` is not an error; a dead peer is retired here.
    pub fn ring_doorbell(&self) {
        let mut clients = self
            .shared
            .clients
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut i = 0;
        while i < clients.len() {
            if !clients[i].ready {
                i += 1;
                continue;
            }
            let byte = [1u8];
            let n = unsafe {
                libc::send(
                    clients[i].fd.as_raw_fd(),
                    byte.as_ptr() as *const _,
                    1,
                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                )
            };
            if n < 0 {
                let err = io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(libc::EAGAIN) | Some(libc::EINTR) => {}
                    _ => {
                        clients.remove(i);
                        self.shared.ready_count.fetch_sub(1, Ordering::Relaxed);
                        continue;
                    }
                }
            }
            i += 1;
        }
    }

    fn wake_thread(&self) {
        let one: u64 = 1;
        unsafe { libc::write(self.wake.as_raw_fd(), &one as *const u64 as *const _, 8) };
    }
}

impl Drop for Server {
    /// Stop accepting, close every client (the interposer sees EOF and reports `ENODEV`), and remove
    /// the socket file.
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.wake_thread();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.shared
            .clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.shared.ready_count.store(0, Ordering::Relaxed);
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Send the configuration struct with the ring memfd attached as `SCM_RIGHTS`.
fn send_config_with_fd(sock: RawFd, config: &[u8], fd: RawFd) -> io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: config.as_ptr() as *mut libc::c_void,
        iov_len: config.len(),
    };
    let space = unsafe { libc::CMSG_SPACE(mem::size_of::<RawFd>() as u32) } as usize;
    let mut cbuf = vec![0u64; space.div_ceil(mem::size_of::<u64>())];
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = space as _;
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(mem::size_of::<RawFd>() as u32) as _;
        ptr::write_unaligned(libc::CMSG_DATA(cmsg) as *mut RawFd, fd);
    }
    loop {
        let n = unsafe { libc::sendmsg(sock, &msg, libc::MSG_NOSIGNAL) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err);
        }
        if n as usize != config.len() {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short config send",
            ));
        }
        return Ok(());
    }
}

fn serve(
    listener: UnixListener,
    wake: RawFd,
    config: [u8; CONFIG_SIZE],
    ring_fd: RawFd,
    shared: Arc<Shared>,
) {
    let listen_fd = listener.as_raw_fd();
    let mut pollfds: Vec<libc::pollfd> = Vec::new();
    while !shared.stop.load(Ordering::Acquire) {
        pollfds.clear();
        pollfds.push(libc::pollfd {
            fd: listen_fd,
            events: libc::POLLIN,
            revents: 0,
        });
        pollfds.push(libc::pollfd {
            fd: wake,
            events: libc::POLLIN,
            revents: 0,
        });
        {
            let clients = shared.clients.lock().unwrap_or_else(|e| e.into_inner());
            for c in clients.iter() {
                pollfds.push(libc::pollfd {
                    fd: c.fd.as_raw_fd(),
                    events: libc::POLLIN | libc::POLLRDHUP,
                    revents: 0,
                });
            }
        }
        let n = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as libc::nfds_t, -1) };
        if n < 0 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break;
        }
        if pollfds[1].revents != 0 {
            let mut v: u64 = 0;
            unsafe { libc::read(wake, &mut v as *mut u64 as *mut _, 8) };
            if shared.stop.load(Ordering::Acquire) {
                break;
            }
        }
        if pollfds[0].revents != 0 {
            accept_clients(&listener, &config, ring_fd, &shared);
        }
        let mut clients = shared.clients.lock().unwrap_or_else(|e| e.into_inner());
        let mut idx = 0;
        for pfd in pollfds.iter().skip(2) {
            if idx >= clients.len() {
                break;
            }
            if clients[idx].fd.as_raw_fd() != pfd.fd {
                idx += 1;
                continue;
            }
            if pfd.revents == 0 {
                idx += 1;
                continue;
            }
            let mut buf = [0u8; 64];
            let r = unsafe {
                libc::recv(
                    clients[idx].fd.as_raw_fd(),
                    buf.as_mut_ptr() as *mut _,
                    buf.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            let closed = if r > 0 {
                if !clients[idx].ready {
                    clients[idx].ready = true;
                    shared.ready_count.fetch_add(1, Ordering::Relaxed);
                }
                false
            } else if r == 0 {
                true
            } else {
                !matches!(
                    io::Error::last_os_error().raw_os_error(),
                    Some(libc::EAGAIN) | Some(libc::EINTR)
                )
            };
            if closed
                || pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLRDHUP) != 0 && r <= 0
            {
                if clients[idx].ready {
                    shared.ready_count.fetch_sub(1, Ordering::Relaxed);
                }
                clients.remove(idx);
                continue;
            }
            idx += 1;
        }
    }
}

/// Make a socket this process bound connectable by its user alone, whatever the umask.
pub(crate) fn restrict_to_owner(path: &str) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// Whether a socket's peer runs as this process's user or as root, who passes the
/// socket's mode anyway: one that connected before `restrict_to_owner` is refused here.
pub(crate) fn peer_is_session_user(fd: RawFd) -> bool {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    r == 0 && (cred.uid == unsafe { libc::geteuid() } || cred.uid == 0)
}

fn accept_clients(
    listener: &UnixListener,
    config: &[u8; CONFIG_SIZE],
    ring_fd: RawFd,
    shared: &Shared,
) {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if !peer_is_session_user(stream.as_raw_fd()) {
                    continue;
                }
                let _ = stream.set_nonblocking(true);
                let fd = OwnedFd::from(stream);
                match send_config_with_fd(fd.as_raw_fd(), config, ring_fd) {
                    Ok(()) => {
                        shared
                            .clients
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(Client { fd, ready: false });
                    }
                    Err(e) => eprintln!("[webcam] interposer handshake failed: {}", e),
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("[webcam] accept failed: {}", e);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    /// The socket is its user's alone whatever the umask, and a peer of that user passes.
    #[test]
    fn the_socket_is_connectable_by_its_user_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("pxwc-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("cam.sock").to_string_lossy().into_owned();
        let old = unsafe { libc::umask(0) };
        let listener = UnixListener::bind(&path).unwrap();
        unsafe { libc::umask(old) };
        restrict_to_owner(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _client = UnixStream::connect(&path).unwrap();
        let (peer, _) = listener.accept().unwrap();
        assert!(peer_is_session_user(peer.as_raw_fd()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn recv_config(stream: &UnixStream) -> (Vec<u8>, RawFd) {
        let mut data = vec![0u8; CONFIG_SIZE];
        let mut iov = libc::iovec {
            iov_base: data.as_mut_ptr() as *mut _,
            iov_len: data.len(),
        };
        let space = unsafe { libc::CMSG_SPACE(4) } as usize;
        let mut cbuf = vec![0u64; space.div_ceil(8)];
        let mut msg: libc::msghdr = unsafe { mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr() as *mut _;
        msg.msg_controllen = space as _;
        let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, 0) };
        assert_eq!(n as usize, CONFIG_SIZE);
        let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
        assert!(!cmsg.is_null());
        let fd = unsafe { ptr::read_unaligned(libc::CMSG_DATA(cmsg) as *const RawFd) };
        (data, fd)
    }

    #[test]
    fn handshake_fd_and_doorbells() {
        let dir = std::env::temp_dir().join(format!("pxwc-{}", std::process::id()));
        let path = dir.join("cam.sock");
        let ring = unsafe { libc::memfd_create(c"t".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(ring >= 0);
        unsafe { libc::ftruncate(ring, 8192) };
        let mut config = [0u8; CONFIG_SIZE];
        config[..4].copy_from_slice(&0xDEADBEEFu32.to_le_bytes());
        let server = Server::bind(path.to_str().unwrap(), config, ring).unwrap();

        let mut client = UnixStream::connect(&path).unwrap();
        let (data, fd) = recv_config(&client);
        assert_eq!(&data[..4], &0xDEADBEEFu32.to_le_bytes());
        assert!(fd >= 0);
        let mut st: libc::stat = unsafe { mem::zeroed() };
        assert_eq!(unsafe { libc::fstat(fd, &mut st) }, 0);
        assert_eq!(st.st_size, 8192);
        unsafe { libc::close(fd) };

        server.ring_doorbell();
        assert_eq!(server.client_count(), 0);
        client.write_all(&[8u8]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while server.client_count() != 1 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(server.client_count(), 1);
        server.ring_doorbell();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut b = [0u8; 1];
        client.read_exact(&mut b).unwrap();
        assert_eq!(b[0], 1);

        drop(client);
        let deadline = Instant::now() + Duration::from_secs(3);
        while server.client_count() != 0 && Instant::now() < deadline {
            server.ring_doorbell();
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(server.client_count(), 0);
        drop(server);
        assert!(!path.exists());
        unsafe { libc::close(ring) };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
