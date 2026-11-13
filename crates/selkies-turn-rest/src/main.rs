/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The TURN REST service's executable: the listeners, and the process around them.
//!
//! It listens on `TURN_REST_ADDR`, a host name or address or a comma-separated list of them
//! (the loopback addresses by default), at `TURN_REST_PORT` (8008). Every address a name
//! resolves to gets a socket of its own, an IPv6 one for IPv6 alone, so `0.0.0.0,::` (what the
//! image sets) serves both families without the two wildcards colliding, and a host without
//! one of the families serves the other. A `TURN_REST_PORT` that is not a port number is 8008
//! too: Kubernetes sets the variable to a URL in every pod beside a Service named `turn-rest`.
//! `WEB_CONCURRENCY` is the number of worker threads.
//! The process ends on `SIGTERM` or `SIGINT`, which it has to ask for to receive as a
//! container's first process.

use std::io;
use std::net::{SocketAddr, TcpListener, ToSocketAddrs};
use std::process::ExitCode;

use selkies_turn_rest::{Config, parse_port, router, unix_now};
use socket2::{Domain, Socket, Type};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;

fn main() -> ExitCode {
    let var = |name: &str| std::env::var_os(name).map(|v| v.to_string_lossy().into_owned());
    let config = Config::from_env(var);
    if let Some(warning) = config.open_warning() {
        eprintln!("{warning}");
    }
    let addr = var("TURN_REST_ADDR")
        .filter(|addr| !addr.is_empty())
        .unwrap_or_else(|| "localhost".into());
    let port = var("TURN_REST_PORT").map_or(8008, |port| parse_port(&port, 8008));
    let listeners = match listen(&addr, port) {
        Ok(listeners) => listeners,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };

    let mut runtime = tokio::runtime::Builder::new_multi_thread();
    runtime.enable_all();
    if let Some(threads) = var("WEB_CONCURRENCY").and_then(|n| n.trim().parse().ok())
        && threads > 0
    {
        runtime.worker_threads(threads);
    }
    let served = runtime
        .build()
        .and_then(|runtime| runtime.block_on(serve(config, listeners)));
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

/// A listening socket on every address `addr` names at `port`.
///
/// An address the host lacks is skipped with a line saying so while another binds; any other
/// failure, a name that does not resolve, or no address binding at all is the error.
fn listen(addr: &str, port: u16) -> io::Result<Vec<TcpListener>> {
    let mut candidates: Vec<SocketAddr> = Vec::new();
    for host in addr
        .split(',')
        .map(str::trim)
        .filter(|host| !host.is_empty())
    {
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let resolved = (host, port)
            .to_socket_addrs()
            .map_err(|error| io::Error::new(error.kind(), format!("{host}: {error}")))?;
        for candidate in resolved {
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
    }
    let mut listeners = Vec::new();
    let mut skipped = Vec::new();
    for candidate in candidates {
        match bind(candidate) {
            Ok(listener) => listeners.push(listener),
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(libc::EADDRNOTAVAIL | libc::EAFNOSUPPORT)
                ) =>
            {
                skipped.push(format!("{} ({error})", candidate.ip()));
            }
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("{candidate}: {error}"),
                ));
            }
        }
    }
    if listeners.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            format!(
                "no address of '{addr}' is available to listen on: {}",
                skipped.join(", ")
            ),
        ));
    }
    for note in skipped {
        println!("Not listening on {note}");
    }
    Ok(listeners)
}

/// A nonblocking listener on `addr`, an IPv6 one taking no IPv4 connections.
fn bind(addr: SocketAddr) -> io::Result<TcpListener> {
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, None)?;
    socket.set_reuse_address(true)?;
    if addr.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    socket.bind(&addr.into())?;
    socket.listen(128)?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

/// Serve every listener until the process is asked to end, then let open requests finish. A
/// listener that stops on its own ends the process with it, so a container is restarted rather
/// than left running deaf.
async fn serve(config: Config, listeners: Vec<TcpListener>) -> io::Result<()> {
    let app = router(config, unix_now);
    let (stop, stopped) = watch::channel(());
    let mut servers = tokio::task::JoinSet::new();
    for listener in listeners {
        let listener = tokio::net::TcpListener::from_std(listener)?;
        println!(
            "TURN REST server listening on http://{}",
            listener.local_addr()?
        );
        let mut stopped = stopped.clone();
        servers.spawn(
            axum::serve(listener, app.clone())
                .with_graceful_shutdown(async move {
                    let _ = stopped.changed().await;
                })
                .into_future(),
        );
    }
    let (mut term, mut int) = (
        signal(SignalKind::terminate())?,
        signal(SignalKind::interrupt())?,
    );
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
        Some(ended) = servers.join_next() => {
            ended??;
            return Err(io::Error::other("a listener stopped serving"));
        }
    }
    let _ = stop.send(());
    while let Some(ended) = servers.join_next().await {
        ended??;
    }
    Ok(())
}
