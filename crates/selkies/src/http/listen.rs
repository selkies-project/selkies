/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Listening: the addresses the server binds, the Unix socket it may serve instead, the
//! connection loop that drives hyper over each, and the connection's place in a request.
//!
//! `addr` is a host name or IP literal, or a comma-separated list of them, each bound on
//! every address it resolves to (`localhost` covers both loopback families); an address the
//! host does not carry (the IPv6 loopback with IPv6 disabled) is skipped with a warning while
//! another binds, and any other failure, or nothing binding at all, fails the start. IPv6
//! sockets are IPv6-only so `::` and `0.0.0.0` can share the port. A `unix_socket` replaces
//! the TCP listeners: a stale socket file nothing accepts on is removed, a live one or a
//! non-socket at the path refuses the start, and the file is unlinked on stop unless another
//! instance took it over.
//!
//! Off Linux the same rules ride the standard library's own listener, put in non-blocking
//! mode for the runtime, and the Unix socket is a Linux feature: a configured `unix_socket`
//! refuses the start rather than being silently dropped, so the operator learns the setting
//! took nothing, and [`ConnInfo`] carries no descriptor for the liveness watch -- the kernel
//! view of a connection is read through interfaces the platform does not have, so a socket
//! is judged by what stands in this process's own queue and by its pings.
//!
//! Every connection runs HTTP/1.1 with hyper's header-read timer set to the idle timeout,
//! which hyper re-arms while it waits for each request's head: a peer that connects and sends
//! nothing, stops inside its headers, or leaves a kept-alive connection idle after a response
//! is closed after [`super::IDLE_CONNECTION_TIMEOUT`], while a request in progress is never
//! idle. Each request carries a [`ConnInfo`] extension with the connection's addresses and
//! whether it is TLS, which the entry page reads for [`ipv6_loopback_redirect`]. The contract
//! line `Selkies server running on <endpoint>` is logged once the listeners are up; the
//! suites wait on it.
//!
//! The access log, off by default, writes the request line and the Referer without their
//! queries: a secure-mode session token rides `?token=` on the data WebSocket URL, the
//! file-manager listing, and the Referer of every request that page makes.

use std::net::{SocketAddr, ToSocketAddrs};
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, header};
use axum::middleware::Next;
use axum::response::Response;
use hyper_util::rt::{TokioIo, TokioTimer};
#[cfg(target_os = "linux")]
use nix::errno::Errno;
#[cfg(target_os = "linux")]
use nix::sys::socket::{self, AddressFamily, SockFlag, SockType, SockaddrIn, SockaddrIn6, sockopt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

/// A bind failure's errno, as the platform reports one: nix's on Linux, the standard
/// library's I/O error elsewhere.
#[cfg(not(target_os = "linux"))]
type Errno = std::io::Error;

/// What the listener binds and how long an idle connection lives.
#[derive(Clone, Debug)]
pub struct ListenConfig {
    pub addr: String,
    pub port: u16,
    /// A Unix socket path to serve on instead of TCP, or empty.
    pub unix_socket: String,
    pub idle_timeout: Duration,
    /// Whether every request leaves an access line at INFO under the `access` target.
    pub access_log: bool,
}

impl Default for ListenConfig {
    fn default() -> ListenConfig {
        ListenConfig {
            addr: "localhost".into(),
            port: 8080,
            unix_socket: String::new(),
            idle_timeout: super::IDLE_CONNECTION_TIMEOUT,
            access_log: false,
        }
    }
}

/// The connection a request arrived on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnInfo {
    /// The accepting socket's own address; none on a Unix socket.
    pub local: Option<SocketAddr>,
    /// The peer's address; none on a Unix socket.
    pub remote: Option<SocketAddr>,
    /// Whether the connection is TLS.
    pub secure: bool,
    /// The accepted socket, valid while this connection's handlers run, for what reads its
    /// kernel state (`TCP_INFO`, the unacknowledged bytes); none where nothing accepted one.
    /// Linux alone, with the kernel view that read is.
    #[cfg(target_os = "linux")]
    pub fd: Option<std::os::fd::RawFd>,
}

/// Why the listener could not come up.
#[derive(Debug, thiserror::Error)]
pub enum ListenError {
    #[error("cannot resolve listen address '{host}': {source}")]
    Resolve {
        host: String,
        source: std::io::Error,
    },
    #[error("cannot bind {place}: {errno}")]
    Bind { place: String, errno: Errno },
    #[error("no address of '{addr}' is available to listen on: {skipped}")]
    Unavailable { addr: String, skipped: String },
    #[error("{0}")]
    UnixSocket(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl ListenError {
    /// The errno of a failed bind, for a caller telling a taken port from the rest.
    #[cfg(target_os = "linux")]
    pub fn errno(&self) -> Option<Errno> {
        match self {
            ListenError::Bind { errno, .. } => Some(*errno),
            _ => None,
        }
    }

    /// The errno of a failed bind, for a caller telling a taken port from the rest.
    /// Off Linux the error is the standard library's, which has no copy: this hands the
    /// bind's own error over.
    #[cfg(not(target_os = "linux"))]
    pub fn errno(self) -> Option<Errno> {
        match self {
            ListenError::Bind { errno, .. } => Some(errno),
            _ => None,
        }
    }
}

/// `host:port` for a socket address, the host bracketed for IPv6.
pub fn format_sockaddr(addr: &SocketAddr) -> String {
    match addr {
        SocketAddr::V4(v4) => format!("{}:{}", v4.ip(), v4.port()),
        SocketAddr::V6(v6) => format!("[{}]:{}", v6.ip(), v6.port()),
    }
}

/// Every address the comma-separated `addr` resolves to on `port`, in resolution order,
/// each once.
pub fn resolve_listen_addresses(addr: &str, port: u16) -> Result<Vec<SocketAddr>, ListenError> {
    let mut candidates: Vec<SocketAddr> = Vec::new();
    for host in addr.split(',').map(str::trim).filter(|h| !h.is_empty()) {
        let resolved = (host, port)
            .to_socket_addrs()
            .map_err(|source| ListenError::Resolve {
                host: host.to_string(),
                source,
            })?;
        for candidate in resolved {
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
    }
    Ok(candidates)
}

/// One listening socket on `addr`: reusable, IPv6-only where IPv6, non-blocking.
#[cfg(target_os = "linux")]
fn bind_one(addr: &SocketAddr) -> Result<OwnedFd, Errno> {
    let family = if addr.is_ipv4() {
        AddressFamily::Inet
    } else {
        AddressFamily::Inet6
    };
    let fd = socket::socket(
        family,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
        None,
    )?;
    socket::setsockopt(&fd, sockopt::ReuseAddr, &true)?;
    match addr {
        SocketAddr::V4(v4) => socket::bind(fd.as_raw_fd(), &SockaddrIn::from(*v4))?,
        SocketAddr::V6(v6) => {
            socket::setsockopt(&fd, sockopt::Ipv6V6Only, &true)?;
            socket::bind(fd.as_raw_fd(), &SockaddrIn6::from(*v6))?;
        }
    }
    socket::listen(
        &fd,
        socket::Backlog::new(128).unwrap_or(socket::Backlog::MAXCONN),
    )?;
    Ok(fd)
}

/// One listening socket on `addr`: reusable, IPv6-only where IPv6, non-blocking. Off Linux
/// the standard library's own bind serves the platform's rules, put in non-blocking mode for
/// the runtime the listener hands it to.
#[cfg(not(target_os = "linux"))]
fn bind_one(addr: &SocketAddr) -> Result<std::net::TcpListener, Errno> {
    let listener = std::net::TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// Bind a listening socket on every address of `candidates` (what `addr` resolved to).
///
/// `EADDRNOTAVAIL` and `EAFNOSUPPORT` are skipped with a warning while another binds; any
/// other failure closes what was bound and fails, as does nothing binding at all.
pub fn bind_listen_sockets(
    addr: &str,
    candidates: &[SocketAddr],
) -> Result<Vec<std::net::TcpListener>, ListenError> {
    let mut bound = Vec::new();
    let mut skipped = Vec::new();
    for candidate in candidates {
        let place = format_sockaddr(candidate);
        #[cfg(target_os = "linux")]
        let attempt = bind_one(candidate).map(std::net::TcpListener::from);
        #[cfg(not(target_os = "linux"))]
        let attempt = bind_one(candidate);
        match attempt {
            Ok(listener) => bound.push(listener),
            #[cfg(target_os = "linux")]
            Err(errno @ (Errno::EADDRNOTAVAIL | Errno::EAFNOSUPPORT)) => {
                skipped.push(format!("{place} ({})", errno.desc()));
            }
            // An address the host does not carry, in the standard library's spelling.
            #[cfg(not(target_os = "linux"))]
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::AddrNotAvailable | std::io::ErrorKind::Unsupported
                ) =>
            {
                skipped.push(format!("{place} ({e})"));
            }
            Err(errno) => return Err(ListenError::Bind { place, errno }),
        }
    }
    if bound.is_empty() {
        return Err(ListenError::Unavailable {
            addr: addr.to_string(),
            skipped: skipped.join(", "),
        });
    }
    for note in skipped {
        log::warn!(target: "server", "Not listening on {note}");
    }
    Ok(bound)
}

/// Whether something accepts a connection on `path`: the socket file belongs to a running
/// listener rather than a dead one's leftover. Only a refusal or a missing file proves the
/// path dead, since an error that does not prove it must not license removing it. The probe
/// is non-blocking, so a listener with a full backlog reads as live rather than holding the
/// caller.
#[cfg(target_os = "linux")]
pub fn unix_socket_is_live(path: &Path) -> bool {
    let probe = socket::socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
        None,
    );
    let (Ok(probe), Ok(addr)) = (probe, socket::UnixAddr::new(path)) else {
        return true;
    };
    !matches!(
        socket::connect(probe.as_raw_fd(), &addr),
        Err(Errno::ECONNREFUSED | Errno::ENOENT)
    )
}

/// Remove a leftover socket file so the bind cannot fail with `EADDRINUSE`; a path still in
/// use, or occupied by anything that is not a socket, aborts the start instead.
#[cfg(target_os = "linux")]
pub fn clear_stale_unix_socket(path: &Path) -> Result<(), ListenError> {
    use std::os::unix::fs::FileTypeExt;
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(ListenError::UnixSocket(format!(
                "Cannot inspect unix socket path '{}': {e}",
                path.display()
            )));
        }
    };
    if !meta.file_type().is_socket() {
        return Err(ListenError::UnixSocket(format!(
            "Unix socket path '{}' exists and is not a socket; refusing to remove it.",
            path.display()
        )));
    }
    if unix_socket_is_live(path) {
        return Err(ListenError::UnixSocket(format!(
            "Another server is already listening on '{}'.",
            path.display()
        )));
    }
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(ListenError::UnixSocket(format!(
            "Cannot remove stale unix socket '{}': {e}",
            path.display()
        ))),
    }
}

/// Drop this listener's socket file on shutdown; a path something is accepting on again
/// belongs to another instance and is left alone.
#[cfg(target_os = "linux")]
pub fn remove_own_unix_socket(path: &Path) {
    use std::os::unix::fs::FileTypeExt;
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if !meta.file_type().is_socket() || unix_socket_is_live(path) {
        return;
    }
    let _ = std::fs::remove_file(path);
}

/// Drop this listener's socket file on shutdown; a path something is accepting on again
/// belongs to another instance and is left alone. No Unix socket is served off Linux, so
/// there is no file to drop.
#[cfg(not(target_os = "linux"))]
pub fn remove_own_unix_socket(_path: &Path) {}

/// The host of a `Host` header, lowercased, without port or brackets.
fn host_name(headers: &HeaderMap) -> Option<String> {
    let host = headers.get(header::HOST)?.to_str().ok()?.trim();
    if let Some(rest) = host.strip_prefix('[') {
        return Some(rest.split(']').next()?.to_ascii_lowercase());
    }
    Some(
        host.rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(host)
            .to_ascii_lowercase(),
    )
}

/// Where to serve a page that arrived on the IPv6 loopback from, or none.
///
/// A browser gathers no ICE host candidates for a page whose origin is `::1`, so a WebRTC
/// session opened from one never nominates a pair. The IPv4 loopback reaches this same
/// listener, so the page is served from there instead; which family `localhost` resolves to
/// is the browser's choice, and the page cannot tell from script which one it got. Only a
/// browser that asked this host for itself is moved: the request has to have arrived on
/// `::1` and name a loopback in `Host`, with no proxy header in front of it, since a reverse
/// proxy that forwards to `[::1]` carries the site's own name and redirecting it would send
/// the browser to its own machine. Where the browser goes is this process's to say: the
/// literal host, the port this socket listens on, and the caller's own `path`; only the query
/// rides along, which a page opened with `?token=` needs and which cannot name another origin.
pub fn ipv6_loopback_redirect(
    conn: &ConnInfo,
    headers: &HeaderMap,
    path: &str,
    query: Option<&str>,
) -> Option<String> {
    let SocketAddr::V6(local) = conn.local? else {
        return None;
    };
    if *local.ip() != std::net::Ipv6Addr::LOCALHOST {
        return None;
    }
    let host = host_name(headers)?;
    if host != "localhost" && host != "::1" {
        return None;
    }
    if ["x-forwarded-for", "x-forwarded-host", "forwarded"]
        .iter()
        .any(|h| headers.contains_key(*h))
    {
        return None;
    }
    let scheme = if conn.secure { "https" } else { "http" };
    let query = query
        .filter(|q| !q.is_empty())
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    Some(format!(
        "{scheme}://127.0.0.1:{}{path}{query}",
        local.port()
    ))
}

/// One answered request, as the access log formats it: remote or `-`, the start time, the
/// request line without its query, the status, the body length or `-`, the Referer stripped
/// at `?` and `#` (or `-`), and the agent.
pub struct AccessRecord<'a> {
    pub remote: Option<SocketAddr>,
    pub started: chrono::DateTime<chrono::Local>,
    pub method: &'a str,
    /// The request path, never its query.
    pub path: &'a str,
    pub version: &'a str,
    pub status: u16,
    pub body_length: Option<u64>,
    pub headers: &'a HeaderMap,
}

impl std::fmt::Display for AccessRecord<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let referer = self
            .headers
            .get(header::REFERER)
            .and_then(|v| v.to_str().ok())
            .map(|r| r.split(['?', '#']).next().unwrap_or("").to_string())
            .unwrap_or_else(|| "-".to_string());
        let agent = self
            .headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("-");
        write!(
            f,
            "{} [{}] \"{} {} {}\" {} {} \"{referer}\" \"{agent}\"",
            self.remote
                .map(|a| a.ip().to_string())
                .unwrap_or_else(|| "-".to_string()),
            self.started.format("%d/%b/%Y:%H:%M:%S %z"),
            self.method,
            self.path,
            self.version,
            self.status,
            self.body_length
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".to_string()),
        )
    }
}

/// The access-log middleware: one INFO line per response under the `access` target.
pub async fn access_log(request: Request, next: Next) -> Response {
    let started = chrono::Local::now();
    let remote = request
        .extensions()
        .get::<ConnInfo>()
        .and_then(|c| c.remote);
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let version = format!("{:?}", request.version());
    let headers = request.headers().clone();
    let response = next.run(request).await;
    let length = response
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    let record = AccessRecord {
        remote,
        started,
        method: &method,
        path: &path,
        version: &version,
        status: response.status().as_u16(),
        body_length: length,
        headers: &headers,
    };
    log::info!(target: "access", "{record}");
    response
}

/// A running listener: where it is reached and how to stop it.
pub struct Listener {
    endpoint: String,
    kind: &'static str,
    cancel: CancellationToken,
    accepts: Vec<JoinHandle<()>>,
    unix_socket: Option<PathBuf>,
    local_addrs: Vec<SocketAddr>,
}

impl Listener {
    /// Where the server is reached: every bound address as `http(s)://host:port`, joined by
    /// `, `, or `unix://<path>`.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// `TCP listener` or `Unix socket listener`.
    pub fn kind(&self) -> &'static str {
        self.kind
    }

    /// The addresses bound, in bind order; empty on a Unix socket.
    pub fn local_addrs(&self) -> &[SocketAddr] {
        &self.local_addrs
    }

    /// Stop accepting, end the open connections, and drop the Unix socket file.
    pub async fn stop(self) {
        self.cancel.cancel();
        for accept in self.accepts {
            let _ = accept.await;
        }
        if let Some(path) = &self.unix_socket {
            remove_own_unix_socket(path);
        }
    }
}

/// The URL a bound address is reached by, as the contract line names it.
fn site_name(addr: &SocketAddr, secure: bool) -> String {
    format!(
        "{}://{}",
        if secure { "https" } else { "http" },
        format_sockaddr(addr)
    )
}

/// Bind the sockets `cfg` names on a blocking thread: name resolution and the Unix socket's
/// checks can each take a while.
fn prepare(cfg: &ListenConfig) -> Result<Prepared, ListenError> {
    if !cfg.unix_socket.is_empty() {
        #[cfg(target_os = "linux")]
        {
            let path = PathBuf::from(&cfg.unix_socket);
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)?;
            }
            clear_stale_unix_socket(&path)?;
            return Ok(Prepared::Unix(path));
        }
        // The Unix socket is a Linux feature; refusing a configured one tells the operator
        // the setting took nothing rather than serving TCP beside their back.
        #[cfg(not(target_os = "linux"))]
        return Err(ListenError::UnixSocket(format!(
            "a Unix socket is not served on this OS: '{}'",
            cfg.unix_socket
        )));
    }
    let candidates = resolve_listen_addresses(&cfg.addr, cfg.port)?;
    Ok(Prepared::Tcp(bind_listen_sockets(&cfg.addr, &candidates)?))
}

/// What [`prepare`] made ready for the runtime to take over.
enum Prepared {
    #[cfg(target_os = "linux")]
    Unix(PathBuf),
    Tcp(Vec<std::net::TcpListener>),
}

/// Bind the configured listener and serve `router` on it until the result is stopped.
///
/// A start that fails part-way leaves nothing bound and logs why. Logs the contract line once
/// up.
pub async fn start(
    router: Router,
    cfg: &ListenConfig,
    tls: Option<Arc<rustls::ServerConfig>>,
) -> Result<Listener, ListenError> {
    let started = serve_prepared(router, cfg, tls).await;
    match &started {
        Ok(listener) => {
            log::info!(target: "server", "Selkies server running on {}", listener.endpoint)
        }
        Err(exc) => {
            let place = if cfg.unix_socket.is_empty() {
                format!("{} port {}", cfg.addr, cfg.port)
            } else {
                format!("unix://{}", cfg.unix_socket)
            };
            log::error!(target: "server", "Cannot bind {place}: {exc}");
        }
    }
    started
}

async fn serve_prepared(
    router: Router,
    cfg: &ListenConfig,
    tls: Option<Arc<rustls::ServerConfig>>,
) -> Result<Listener, ListenError> {
    let prepared = {
        let cfg = cfg.clone();
        tokio::task::spawn_blocking(move || prepare(&cfg))
            .await
            .map_err(std::io::Error::other)??
    };
    let cancel = CancellationToken::new();
    let secure = tls.is_some();
    let mut accepts = Vec::new();
    let mut local_addrs = Vec::new();
    let (endpoint, kind, unix_socket) = match prepared {
        #[cfg(target_os = "linux")]
        Prepared::Unix(path) => {
            let listener = tokio::net::UnixListener::bind(&path)?;
            accepts.push(tokio::spawn(accept_unix(
                listener,
                router,
                cfg.idle_timeout,
                tls,
                cancel.clone(),
            )));
            (
                format!("unix://{}", path.display()),
                "Unix socket listener",
                Some(path),
            )
        }
        Prepared::Tcp(sockets) => {
            let mut listeners = Vec::with_capacity(sockets.len());
            for socket in sockets {
                listeners.push(tokio::net::TcpListener::from_std(socket)?);
            }
            let mut names = Vec::new();
            for listener in listeners {
                let local = listener.local_addr()?;
                names.push(site_name(&local, secure));
                local_addrs.push(local);
                accepts.push(tokio::spawn(accept_tcp(
                    listener,
                    router.clone(),
                    cfg.idle_timeout,
                    tls.clone(),
                    cancel.clone(),
                )));
            }
            (names.join(", "), "TCP listener", None)
        }
    };
    Ok(Listener {
        endpoint,
        kind,
        cancel,
        accepts,
        unix_socket,
        local_addrs,
    })
}

async fn accept_tcp(
    listener: tokio::net::TcpListener,
    router: Router,
    idle: Duration,
    tls: Option<Arc<rustls::ServerConfig>>,
    cancel: CancellationToken,
) {
    let local = listener.local_addr().ok();
    loop {
        let accepted = tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => accepted,
        };
        let Ok((stream, remote)) = accepted else {
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        };
        let _ = stream.set_nodelay(true);
        let info = ConnInfo {
            local,
            remote: Some(remote),
            secure: tls.is_some(),
            #[cfg(target_os = "linux")]
            fd: Some(std::os::fd::AsRawFd::as_raw_fd(&stream)),
        };
        tokio::spawn(serve_connection(
            stream,
            info,
            router.clone(),
            idle,
            tls.clone(),
            cancel.clone(),
        ));
    }
}

#[cfg(target_os = "linux")]
async fn accept_unix(
    listener: tokio::net::UnixListener,
    router: Router,
    idle: Duration,
    tls: Option<Arc<rustls::ServerConfig>>,
    cancel: CancellationToken,
) {
    loop {
        let accepted = tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => accepted,
        };
        let Ok((stream, _)) = accepted else {
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        };
        let info = ConnInfo {
            local: None,
            remote: None,
            secure: tls.is_some(),
            fd: None,
        };
        tokio::spawn(serve_connection(
            stream,
            info,
            router.clone(),
            idle,
            tls.clone(),
            cancel.clone(),
        ));
    }
}

/// Drive one connection, TLS first where configured, then HTTP/1.1 with upgrades.
async fn serve_connection<I>(
    stream: I,
    info: ConnInfo,
    router: Router,
    idle: Duration,
    tls: Option<Arc<rustls::ServerConfig>>,
    cancel: CancellationToken,
) where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    match tls {
        Some(config) => {
            let acceptor = tokio_rustls::TlsAcceptor::from(config);
            let handshake = tokio::time::timeout(idle, acceptor.accept(stream));
            match handshake.await {
                Ok(Ok(tls_stream)) => serve_http(tls_stream, info, router, idle, cancel).await,
                Ok(Err(e)) => log::debug!(target: "server", "TLS handshake failed: {e}"),
                Err(_) => log::debug!(target: "server", "TLS handshake timed out"),
            }
        }
        None => serve_http(stream, info, router, idle, cancel).await,
    }
}

async fn serve_http<I>(
    stream: I,
    info: ConnInfo,
    router: Router,
    idle: Duration,
    cancel: CancellationToken,
) where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service =
        hyper::service::service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
            let router = router.clone();
            async move {
                let mut request = request.map(Body::new);
                request.extensions_mut().insert(info);
                router.oneshot(request).await
            }
        });
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(idle)
        .keep_alive(true);
    let connection = builder
        .serve_connection(TokioIo::new(stream), service)
        .with_upgrades();
    let mut connection = std::pin::pin!(connection);
    tokio::select! {
        outcome = connection.as_mut() => {
            if let Err(e) = outcome {
                log::debug!(target: "server", "connection ended: {e}");
            }
        }
        _ = cancel.cancelled() => {
            connection.as_mut().graceful_shutdown();
            let _ = tokio::time::timeout(Duration::from_secs(5), connection).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderName, HeaderValue};
    use std::net::TcpListener as StdListener;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        map
    }

    fn v6(ip: &str, port: u16) -> ConnInfo {
        ConnInfo {
            local: Some(SocketAddr::new(ip.parse().unwrap(), port)),
            remote: None,
            secure: false,
            fd: None,
        }
    }

    /// One redirect case: label, connection, headers, route path, query, and the expected target.
    type Case<'a> = (
        &'a str,
        ConnInfo,
        &'a [(&'a str, &'a str)],
        &'a str,
        Option<&'a str>,
        Option<&'a str>,
    );

    #[test]
    fn ipv6_loopback_cases() {
        let local = v6("::1", 8080);
        let cases: &[Case] = &[
            (
                "a browser on ::1 asking for localhost moves",
                local,
                &[("host", "localhost:8080")],
                "/",
                None,
                Some("http://127.0.0.1:8080/"),
            ),
            (
                "the IPv6 literal moves too",
                local,
                &[("host", "[::1]:8080")],
                "/",
                None,
                Some("http://127.0.0.1:8080/"),
            ),
            (
                "the route's own path is used, and the query rides along",
                local,
                &[("host", "localhost:8080")],
                "/",
                Some("a=1"),
                Some("http://127.0.0.1:8080/?a=1"),
            ),
            (
                "a proxy forwarding its own name stays",
                local,
                &[("host", "example.com")],
                "/",
                None,
                None,
            ),
            (
                "a proxy that says so stays",
                local,
                &[
                    ("host", "localhost:8080"),
                    ("x-forwarded-for", "203.0.113.9"),
                ],
                "/",
                None,
                None,
            ),
            (
                "a forwarded header of any spelling stays",
                local,
                &[("host", "localhost:8080"), ("forwarded", "for=203.0.113.9")],
                "/",
                None,
                None,
            ),
            (
                "a global IPv6 address stays",
                v6("2001:db8::1", 8080),
                &[("host", "localhost:8080")],
                "/",
                None,
                None,
            ),
            (
                "an IPv4 connection is already where it should be",
                ConnInfo {
                    local: Some("127.0.0.1:8080".parse().unwrap()),
                    remote: None,
                    secure: false,
                    fd: None,
                },
                &[("host", "localhost:8080")],
                "/",
                None,
                None,
            ),
            (
                "a v4-mapped address is an IPv4 connection",
                v6("::ffff:127.0.0.1", 8080),
                &[("host", "localhost:8080")],
                "/",
                None,
                None,
            ),
            (
                "a unix socket carries no address family to compare",
                ConnInfo {
                    local: None,
                    remote: None,
                    secure: false,
                    fd: None,
                },
                &[("host", "localhost:8080")],
                "/",
                None,
                None,
            ),
        ];
        for (label, conn, hdrs, path, query, want) in cases {
            let got = ipv6_loopback_redirect(conn, &headers(hdrs), path, *query);
            assert_eq!(got.as_deref(), *want, "{label}");
        }
        let secure = ConnInfo {
            secure: true,
            ..local
        };
        assert_eq!(
            ipv6_loopback_redirect(&secure, &headers(&[("host", "localhost")]), "/desk/", None)
                .as_deref(),
            Some("https://127.0.0.1:8080/desk/"),
            "the scheme follows the connection and the prefix the route"
        );
    }

    #[test]
    fn access_line_drops_queries() {
        let hdrs = headers(&[
            ("user-agent", "probe/1"),
            ("referer", "http://example.test/?token=ref-tok"),
        ]);
        let line = AccessRecord {
            remote: Some("127.0.0.1:5".parse().unwrap()),
            started: chrono::Local::now(),
            method: "GET",
            path: "/api/websockets",
            version: "HTTP/1.1",
            status: 200,
            body_length: Some(2),
            headers: &hdrs,
        }
        .to_string();
        assert!(line.contains("\"GET /api/websockets HTTP/1.1\""), "{line}");
        assert!(
            !line.contains("ref-tok") && line.contains("\"http://example.test/\""),
            "{line}"
        );
        assert!(
            line.contains(" 200 2 ") && line.contains("probe/1"),
            "{line}"
        );
    }

    fn host_has_v6() -> bool {
        StdListener::bind("[::1]:0").is_ok()
    }

    fn names(socks: &[std::net::TcpListener]) -> Vec<String> {
        let mut got: Vec<String> = socks
            .iter()
            .map(|s| {
                let a = s.local_addr().unwrap();
                format!(
                    "{} {}",
                    if a.is_ipv4() { "AF_INET" } else { "AF_INET6" },
                    a.ip()
                )
            })
            .collect();
        got.sort();
        got
    }

    fn bind(addr: &str, port: u16) -> Result<Vec<std::net::TcpListener>, ListenError> {
        bind_listen_sockets(addr, &resolve_listen_addresses(addr, port)?)
    }

    #[test]
    fn binder_cases() {
        if crate::http::testing::rerun_alone("http::listen::tests::binder_cases") {
            return;
        }
        let port = crate::http::testing::free_port();
        let has_v6 = host_has_v6();
        let got = names(&bind("localhost", port).unwrap());
        assert!(
            got.contains(&"AF_INET 127.0.0.1".to_string()),
            "localhost binds the IPv4 loopback: {got:?}"
        );
        assert_eq!(
            got.contains(&"AF_INET6 ::1".to_string()),
            has_v6,
            "localhost binds the IPv6 loopback where the host has one"
        );
        assert!(
            got.iter()
                .all(|n| !n.ends_with(" 0.0.0.0") && !n.ends_with(" ::")),
            "localhost binds no wildcard address"
        );
        assert_eq!(
            names(&bind("0.0.0.0", port).unwrap()),
            ["AF_INET 0.0.0.0"],
            "0.0.0.0 binds the IPv4 wildcard alone"
        );
        if has_v6 {
            let socks = bind("::", port).unwrap();
            assert_eq!(
                names(&socks),
                ["AF_INET6 ::"],
                ":: binds the IPv6 wildcard alone"
            );
            let v6only = socket::getsockopt(&socks[0], sockopt::Ipv6V6Only).unwrap();
            assert!(
                v6only,
                "an IPv6 socket is IPv6-only, so :: and 0.0.0.0 can share a port"
            );
            drop(socks);
            assert_eq!(
                names(&bind("127.0.0.1, ::1", port).unwrap()),
                ["AF_INET 127.0.0.1", "AF_INET6 ::1"],
                "a comma-separated list binds every entry"
            );
            assert_eq!(
                names(&bind("0.0.0.0,::", port).unwrap()),
                ["AF_INET 0.0.0.0", "AF_INET6 ::"],
                "both wildcards bind together"
            );
        }
        assert_eq!(
            names(&bind("127.0.0.1,127.0.0.1", port).unwrap()),
            ["AF_INET 127.0.0.1"],
            "a repeated address binds once"
        );

        // A TEST-NET address no interface carries: bind fails with EADDRNOTAVAIL and is skipped.
        let unassignable: SocketAddr = format!("192.0.2.1:{port}").parse().unwrap();
        let mut candidates = vec![unassignable];
        candidates.extend(resolve_listen_addresses("127.0.0.1", port).unwrap());
        let socks = bind_listen_sockets("127.0.0.1", &candidates).unwrap();
        assert_eq!(
            names(&socks),
            ["AF_INET 127.0.0.1"],
            "an address the host lacks is skipped while another binds"
        );
        drop(socks);
        let err = bind_listen_sockets("192.0.2.1", &[unassignable])
            .expect_err("nothing bindable is an error");
        assert!(err.to_string().contains("192.0.2.1"), "{err}");

        let taken = StdListener::bind("127.0.0.1:0").unwrap();
        let err = bind("localhost", taken.local_addr().unwrap().port())
            .expect_err("a port already taken is an error");
        assert_eq!(err.errno(), Some(Errno::EADDRINUSE), "{err}");
        if has_v6 {
            assert!(
                StdListener::bind("[::1]:0").is_ok(),
                "a failed bind leaves nothing else bound"
            );
        }
        let err = bind("selkies-listen-address.invalid", port)
            .expect_err("a name that does not resolve is an error");
        assert!(
            err.to_string().contains("selkies-listen-address.invalid"),
            "{err}"
        );
    }

    #[test]
    fn unix_socket_rules() {
        if crate::http::testing::rerun_alone("http::listen::tests::unix_socket_rules") {
            return;
        }
        let dir = crate::http::testing::tempfile_dir("selkies-unix-");
        let path = dir.join("s.sock");
        assert!(
            clear_stale_unix_socket(&path).is_ok(),
            "a missing path is fine"
        );
        std::fs::write(&path, b"x").unwrap();
        let err = clear_stale_unix_socket(&path).unwrap_err().to_string();
        assert!(err.contains("exists and is not a socket"), "{err}");
        std::fs::remove_file(&path).unwrap();
        let live = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(unix_socket_is_live(&path));
        let err = clear_stale_unix_socket(&path).unwrap_err().to_string();
        assert!(err.contains("Another server is already listening"), "{err}");
        remove_own_unix_socket(&path);
        assert!(
            path.exists(),
            "a socket another instance accepts on is left alone"
        );
        drop(live);
        assert!(
            !unix_socket_is_live(&path),
            "a dead listener's socket refuses"
        );
        assert!(
            clear_stale_unix_socket(&path).is_ok() && !path.exists(),
            "a dead one is unlinked"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
