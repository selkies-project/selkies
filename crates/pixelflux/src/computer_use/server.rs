/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The standalone HTTP server of the Python package, built with the `computer-use-server`
//! feature: `POST /computer-use` and the recorder's `/record_start`, `/record_stop`, and
//! `/record_status`, each answered through the entry points of [`super`] that any other
//! transport calls.
//!
//! Enabled by setting the `PIXELFLUX_CU` environment variable to a bind: comma-separated entries,
//! each a port, served on the loopback addresses only, or `host:port`, the address to serve on
//! (`0.0.0.0:5000,[::]:5000` serves every interface).
//!
//! A caller drives the session with its owner's full authority, and a loopback address is open to
//! every account on the host, so the server does not start without a token (`PIXELFLUX_CU_TOKEN`,
//! or the caller's own through [`start_cu_server`]) and every request has to carry it as
//! `Authorization: Bearer <token>`.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::thread;
use std::time::Duration;

use super::{InvalidRequest, action, record_start, record_status, record_stop};

/// The recorder endpoints sharing the CU server: `record_start`, `record_stop`, and
/// `record_status`. Returns `None` for any other URL.
fn handle_record_endpoint(url: &str, body: &str) -> Option<String> {
    Some(match url {
        "/record_start" => record_start(body),
        "/record_stop" => record_stop(),
        "/record_status" => record_status(),
        _ => return None,
    })
}

/// Start the CU server if `PIXELFLUX_CU` names a bind.
pub fn spawn_cu_from_env() {
    if let Ok(bind) = std::env::var("PIXELFLUX_CU")
        && let Err(e) = start_cu_server(&bind, None)
    {
        println!("[ComputerUse] Not started: {e}");
    }
}

/// The token every request has to carry, fixed when the server starts.
static CU_TOKEN: OnceLock<String> = OnceLock::new();

/// Start the CU server on `bind`, comma-separated entries: a bare port listens on
/// the loopback addresses only, `host:port` names the address to listen on
/// (`0.0.0.0:port,[::]:port` accepts every interface), requiring `token`, else
/// `PIXELFLUX_CU_TOKEN`, of every request. Guarded so that exactly one server
/// binds per process no matter how many call sites (module import, Wayland
/// compositor init, a script) race to spawn it; backend selection stays
/// per-request, so a server bound at import serves a compositor that only starts
/// later.
pub fn start_cu_server(bind: &str, token: Option<&str>) -> Result<(), String> {
    static SPAWNED: OnceLock<()> = OnceLock::new();
    if SPAWNED.get().is_some() {
        return Ok(());
    }
    let token = token
        .map(str::to_string)
        .or_else(|| std::env::var("PIXELFLUX_CU_TOKEN").ok())
        .filter(|t| !t.is_empty())
        .ok_or("no token: set PIXELFLUX_CU_TOKEN, the bearer token every request has to carry")?;
    let listeners = cu_listeners(bind)?;
    if SPAWNED.set(()).is_err() {
        return Ok(());
    }
    let _ = CU_TOKEN.set(token);
    for listener in listeners {
        thread::spawn(move || run_cu_server(listener));
    }
    Ok(())
}

/// Whether an `Authorization` value carries the server's token, compared in constant time.
fn authorized(authorization: Option<&str>) -> bool {
    let Some(token) = CU_TOKEN.get() else {
        return false;
    };
    authorization
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|given| {
            given.len() == token.len()
                && given
                    .bytes()
                    .zip(token.bytes())
                    .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                    == 0
        })
}

/// The addresses a bind names, each once: for each comma-separated entry, both
/// loopback addresses for a bare port and every address the host resolves to for `host:port`.
/// An entry that is neither, or a bind naming nothing, is an error. Resolving a host name
/// blocks.
fn bind_addresses(bind: &str) -> Result<Vec<SocketAddr>, String> {
    let mut addrs: Vec<SocketAddr> = Vec::new();
    for entry in bind
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
    {
        let resolved: Vec<SocketAddr> = match entry.parse::<u16>() {
            Ok(port) => vec![
                SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
                SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
            ],
            Err(_) if entry.contains(':') => entry
                .to_socket_addrs()
                .map_err(|e| format!("cannot resolve '{entry}': {e}"))?
                .collect(),
            Err(_) => {
                return Err(format!(
                    "invalid bind '{entry}': expected a port or host:port"
                ));
            }
        };
        for addr in resolved {
            if !addrs.contains(&addr) {
                addrs.push(addr);
            }
        }
    }
    if addrs.is_empty() {
        return Err(format!(
            "invalid bind '{bind}': expected a port or host:port"
        ));
    }
    Ok(addrs)
}

/// Bound listeners for a CU bind ([`bind_addresses`]). An address the host lacks (the IPv6
/// loopback with IPv6 disabled) is skipped while another binds; any other failure, or nothing
/// binding at all, is an error.
fn cu_listeners(bind: &str) -> Result<Vec<TcpListener>, String> {
    let addrs = bind_addresses(bind)?;
    let mut listeners = Vec::new();
    let mut skipped = Vec::new();
    for addr in addrs {
        match bind_listener(addr) {
            Ok(listener) => listeners.push(listener),
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(libc::EADDRNOTAVAIL | libc::EAFNOSUPPORT)
                ) =>
            {
                skipped.push(format!("{addr} ({e})"));
            }
            Err(e) => return Err(format!("cannot bind {addr}: {e}")),
        }
    }
    if listeners.is_empty() {
        return Err(format!(
            "no address of '{bind}' is available: {}",
            skipped.join(", ")
        ));
    }
    for note in skipped {
        println!("[ComputerUse] Not listening on {note}");
    }
    Ok(listeners)
}

/// A listening socket on `addr`, IPv6-only for an IPv6 address so `[::]` and
/// `0.0.0.0` can share a port: the standard library binds `[::]` dual-stack and
/// offers no way to change that before the bind.
fn bind_listener(addr: SocketAddr) -> std::io::Result<TcpListener> {
    use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};

    let family = if addr.is_ipv4() {
        libc::AF_INET
    } else {
        libc::AF_INET6
    };
    let fd = unsafe { libc::socket(family, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    let one: libc::c_int = 1;
    let enable = |level: libc::c_int, name: libc::c_int| -> std::io::Result<()> {
        let rc = unsafe {
            libc::setsockopt(
                fd,
                level,
                name,
                &one as *const libc::c_int as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    };
    enable(libc::SOL_SOCKET, libc::SO_REUSEADDR)?;
    if addr.is_ipv6() {
        enable(libc::IPPROTO_IPV6, libc::IPV6_V6ONLY)?;
    }
    let rc = match addr {
        SocketAddr::V4(v4) => {
            let mut sin: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            sin.sin_family = libc::AF_INET as libc::sa_family_t;
            sin.sin_port = v4.port().to_be();
            sin.sin_addr = libc::in_addr {
                s_addr: u32::from_ne_bytes(v4.ip().octets()),
            };
            unsafe {
                libc::bind(
                    fd,
                    &sin as *const libc::sockaddr_in as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            }
        }
        SocketAddr::V6(v6) => {
            let mut sin6: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
            sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            sin6.sin6_port = v6.port().to_be();
            sin6.sin6_flowinfo = v6.flowinfo();
            sin6.sin6_addr = libc::in6_addr {
                s6_addr: v6.ip().octets(),
            };
            sin6.sin6_scope_id = v6.scope_id();
            unsafe {
                libc::bind(
                    fd,
                    &sin6 as *const libc::sockaddr_in6 as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                )
            }
        }
    };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::listen(fd, 128) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { TcpListener::from_raw_fd(sock.into_raw_fd()) })
}

/// Expose the captured desktop to an AI agent over HTTP, so a Computer Use client can drive
/// the session much as a human viewer would.
///
/// It runs on its own thread serving `listener` POST `/computer-use` JSON actions
/// (screenshot, mouse_move, click, key, scroll, …). The backend and the framebuffer dimensions
/// are re-resolved on every request rather than cached, because the compositor can start, the
/// stream can resize, or the X server can restart underneath the agent — a stale size would
/// misplace every coordinate. On Wayland a screenshot forces a one-frame GPU readback when the
/// pipeline is in zero-copy mode; on X11 it is a one-shot `GetImage` of the root window.
pub fn run_cu_server(listener: TcpListener) {
    let addr = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();
    println!("[ComputerUse] Server listening on {addr}");
    // Requests are read on threads of their own, bounded in count, size, and time before
    // the token is checked, and acted on here one at a time.
    let (tx, rx) = mpsc::sync_channel::<(TcpStream, String, String)>(MAX_CU_PENDING);
    let pending = Arc::new(AtomicUsize::new(0));
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            if pending.fetch_add(1, Ordering::AcqRel) >= MAX_CU_PENDING {
                pending.fetch_sub(1, Ordering::AcqRel);
                continue;
            }
            let (tx, pending) = (tx.clone(), pending.clone());
            thread::spawn(move || {
                if let Some(request) = read_cu_request(stream) {
                    let _ = tx.send(request);
                }
                pending.fetch_sub(1, Ordering::AcqRel);
            });
        }
    });

    for (mut stream, url, body) in rx {
        if let Some(json) = handle_record_endpoint(&url, &body) {
            cu_respond(&mut stream, 200, &json);
            continue;
        }
        match action(&body) {
            Ok(json) => cu_respond(&mut stream, 200, &json),
            Err(InvalidRequest(json)) => cu_respond(&mut stream, 400, &json),
        }
    }
}

/// The most a CU request's line and headers may take; its body is one command.
const MAX_CU_HEAD: usize = 16 * 1024;
const MAX_CU_BODY: usize = 4 * 1024 * 1024;
/// Connections whose requests are being read or wait to be acted on; more are closed.
const MAX_CU_PENDING: usize = 16;
/// How long a connection has to send its request.
const CU_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a refused request's connection is read on after its answer (`cu_refuse`).
const CU_LINGER: Duration = Duration::from_secs(1);

/// Read one HTTP/1.1 request: `(stream, path, body)` for an authorized one; anything
/// else is answered here (401, 4xx) or dropped, before more than its head is read.
fn read_cu_request(mut stream: TcpStream) -> Option<(TcpStream, String, String)> {
    stream.set_read_timeout(Some(CU_READ_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(CU_READ_TIMEOUT)).ok()?;
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_CU_HEAD {
            cu_refuse(&mut stream, 431, "{\"error\":\"request head too large\"}");
            return None;
        }
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&buf[..head_end]).ok()?;
    let mut lines = head.split("\r\n");
    let path = lines.next()?.split(' ').nth(1)?.to_string();
    let (mut authorization, mut length, mut chunked) = (None, Some(0usize), false);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("authorization") {
            authorization = Some(value);
        } else if name.eq_ignore_ascii_case("content-length") {
            length = value.parse().ok();
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = true;
        }
    }
    if !authorized(authorization) {
        cu_refuse(&mut stream, 401, "{\"error\":\"unauthorized\"}");
        return None;
    }
    let Some(length) = length.filter(|_| !chunked) else {
        cu_refuse(
            &mut stream,
            411,
            "{\"error\":\"a Content-Length is required\"}",
        );
        return None;
    };
    if length > MAX_CU_BODY {
        cu_refuse(&mut stream, 413, "{\"error\":\"request body too large\"}");
        return None;
    }
    let mut body = buf.split_off(head_end + 4);
    body.truncate(length);
    if body.len() < length {
        let have = body.len();
        body.resize(length, 0);
        stream.read_exact(&mut body[have..]).ok()?;
    }
    match String::from_utf8(body) {
        Ok(body) => Some((stream, path, body)),
        Err(_) => {
            cu_refuse(&mut stream, 400, "{\"error\":\"body is not UTF-8\"}");
            None
        }
    }
}

/// Answer a request with a JSON body and close the connection.
/// Answer a request refused before all of it was read, then read on and drop what the client
/// still sends, for `CU_LINGER` and `MAX_CU_BODY` at most, before the connection is dropped: a
/// socket closed with unread data resets the connection, which can cut the answer short.
fn cu_refuse(stream: &mut TcpStream, status: u16, json: &str) {
    cu_respond(stream, status, json);
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let _ = stream.set_read_timeout(Some(CU_LINGER));
    let deadline = std::time::Instant::now() + CU_LINGER;
    let (mut sink, mut drained) = ([0u8; 4096], 0usize);
    while drained < MAX_CU_BODY && std::time::Instant::now() < deadline {
        match stream.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(n) => drained += n,
        }
    }
}

fn cu_respond(stream: &mut TcpStream, status: u16, json: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        411 => "Length Required",
        413 => "Payload Too Large",
        _ => "Request Header Fields Too Large",
    };
    let challenge = if status == 401 {
        "WWW-Authenticate: Bearer\r\n"
    } else {
        ""
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{challenge}Connection: close\r\n\r\n{json}",
        json.len()
    );
    let _ = stream.flush();
}

#[cfg(test)]
mod tests {
    use super::{CU_TOKEN, MAX_CU_BODY, bind_addresses, cu_listeners, read_cu_request};
    use std::io::{Read, Write};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};

    /// Send `raw` as a client, read it as the server does: what the reader returned and
    /// what the client was answered.
    fn exchange(raw: Vec<u8>) -> (Option<(String, String)>, String) {
        let token = CU_TOKEN.get_or_init(|| "cu-test-token".into()).clone();
        let raw = String::from_utf8_lossy(&raw)
            .replace("TOKEN", &token)
            .into_bytes();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut c = TcpStream::connect(addr).unwrap();
            let _ = c.write_all(&raw);
            let _ = c.shutdown(std::net::Shutdown::Write);
            let mut answer = String::new();
            let _ = c.read_to_string(&mut answer);
            answer
        });
        let (stream, _) = listener.accept().unwrap();
        let got = read_cu_request(stream).map(|(_, path, body)| (path, body));
        (got, client.join().unwrap())
    }

    /// A request is read only so far as the bounds allow, and only an authorized one past
    /// its head: a huge header line, a missing token, and an oversized body are answered
    /// without the server reading on.
    #[test]
    fn a_request_is_bounded_and_authorized_before_its_body_is_read() {
        let body = r#"{"action":"screenshot"}"#;
        let (got, _) = exchange(
            format!("POST /computer-use HTTP/1.1\r\nAuthorization: Bearer TOKEN\r\nContent-Length: {}\r\n\r\n{body}", body.len())
                .into_bytes(),
        );
        assert_eq!(got, Some(("/computer-use".into(), body.into())));

        let mut huge = b"POST /computer-use HTTP/1.1\r\nX-Pad: ".to_vec();
        huge.resize(huge.len() + (1 << 20), b'a');
        let (got, answer) = exchange(huge);
        assert!(
            got.is_none() && answer.starts_with("HTTP/1.1 431"),
            "{answer}"
        );

        let (got, answer) =
            exchange(b"POST /computer-use HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}".to_vec());
        assert!(
            got.is_none() && answer.starts_with("HTTP/1.1 401"),
            "{answer}"
        );
        assert!(answer.contains("WWW-Authenticate: Bearer"), "{answer}");

        let (got, answer) = exchange(
            format!("POST /computer-use HTTP/1.1\r\nAuthorization: Bearer TOKEN\r\nContent-Length: {}\r\n\r\n", MAX_CU_BODY + 1)
                .into_bytes(),
        );
        assert!(
            got.is_none() && answer.starts_with("HTTP/1.1 413"),
            "{answer}"
        );

        let (got, answer) = exchange(
            b"POST /computer-use HTTP/1.1\r\nAuthorization: Bearer TOKEN\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n".to_vec(),
        );
        assert!(
            got.is_none() && answer.starts_with("HTTP/1.1 411"),
            "{answer}"
        );
    }

    /// A bare port is both loopback addresses, `host:port` the address it names, an entry
    /// named twice one address, and anything else an error.
    #[test]
    fn a_bind_names_its_addresses() {
        let v4 = SocketAddr::from((Ipv4Addr::LOCALHOST, 9500));
        let v6 = SocketAddr::from((Ipv6Addr::LOCALHOST, 9500));
        assert_eq!(bind_addresses("9500").unwrap(), [v4, v6]);
        assert_eq!(
            bind_addresses("9500, 127.0.0.1:9500,9500").unwrap(),
            [v4, v6]
        );
        assert_eq!(
            bind_addresses("0.0.0.0:9500,[::]:9501").unwrap(),
            [
                SocketAddr::from((Ipv4Addr::UNSPECIFIED, 9500)),
                SocketAddr::from((Ipv6Addr::UNSPECIFIED, 9501)),
            ]
        );
        for bind in ["abc", "70000", "", ",", "127.0.0.1:0,abc"] {
            assert!(bind_addresses(bind).is_err(), "{bind:?}");
        }
    }

    #[test]
    fn bare_port_binds_loopback_only() {
        let listeners = cu_listeners("0").expect("loopback binds");
        assert!(!listeners.is_empty());
        for listener in &listeners {
            let addr = listener.local_addr().unwrap();
            assert!(addr.ip().is_loopback(), "{addr}");
            assert_ne!(addr.port(), 0);
        }
        assert!(
            listeners
                .iter()
                .any(|l| l.local_addr().unwrap().ip() == IpAddr::V4(Ipv4Addr::LOCALHOST))
        );
    }

    #[test]
    fn host_port_names_the_address() {
        let scoped = cu_listeners("127.0.0.1:0").unwrap();
        assert_eq!(scoped.len(), 1);
        assert_eq!(
            scoped[0].local_addr().unwrap().ip(),
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
        let open = cu_listeners("0.0.0.0:0").unwrap();
        assert_eq!(open.len(), 1);
        assert!(open[0].local_addr().unwrap().ip().is_unspecified());
    }

    #[test]
    fn a_taken_port_is_an_error() {
        let taken = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        assert!(cu_listeners(&port.to_string()).is_err());
        assert!(cu_listeners(&format!("127.0.0.1:{port}")).is_err());
    }

    #[test]
    fn malformed_binds_are_errors() {
        assert!(cu_listeners("abc").is_err());
        assert!(cu_listeners("70000").is_err());
        assert!(cu_listeners("").is_err());
        assert!(cu_listeners(",").is_err());
        assert!(cu_listeners("127.0.0.1:0,abc").is_err());
    }

    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn v6only(listener: &TcpListener) -> libc::c_int {
        use std::os::fd::AsRawFd;
        let mut value: libc::c_int = -1;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                listener.as_raw_fd(),
                libc::IPPROTO_IPV6,
                libc::IPV6_V6ONLY,
                &mut value as *mut libc::c_int as *mut libc::c_void,
                &mut len,
            )
        };
        assert_eq!(rc, 0);
        value
    }

    #[test]
    fn both_wildcards_share_a_port() {
        let port = free_port();
        let listeners = cu_listeners(&format!("0.0.0.0:{port}, [::]:{port}")).unwrap();
        assert!(
            listeners
                .iter()
                .all(|l| l.local_addr().unwrap().port() == port)
        );
        assert!(
            listeners
                .iter()
                .all(|l| l.local_addr().unwrap().ip().is_unspecified())
        );
        if let Some(v6) = listeners.iter().find(|l| l.local_addr().unwrap().is_ipv6()) {
            assert_eq!(listeners.len(), 2);
            assert_eq!(v6only(v6), 1);
        }
    }

    #[test]
    fn repeated_entries_bind_once() {
        let port = free_port();
        let listeners = cu_listeners(&format!("{port},127.0.0.1:{port},{port}")).unwrap();
        assert!(listeners.len() <= 2);
        assert!(
            listeners
                .iter()
                .all(|l| l.local_addr().unwrap().ip().is_loopback())
        );
    }
}
