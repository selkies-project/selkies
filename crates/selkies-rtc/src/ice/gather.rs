/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The host addresses a gatherer offers and the sockets it binds for them.
//!
//! `host_addresses` filters the interfaces: loopback is left out, and an IPv6 address carries a
//! zone exactly when its scope id is non-zero, so the scope is what marks the link-local addresses
//! to leave out. So is every address of an interface the integrator names, by name or by a prefix
//! of names: a container or VM bridge (`docker0`) is reached from the host's other networks only
//! through the NAT its runtime installs, which rewrites the source address and port of a check
//! sent from the bridge address anywhere but onto the bridge, so a peer paired with that address
//! meets the server at an address and port it was never offered. A name is what marks such a
//! bridge: its address is private like any LAN's, and the NAT rule lives in the kernel's tables,
//! out of an unprivileged server's sight. `bind_udp` is the port-window rule: with a configured
//! range the bind is the reservation, the ports are tried in random order and the kernel rejects
//! occupied ones, so concurrent sessions sharing one range spread without coordination; only an
//! occupied port is cured by trying another, since anything else (address gone, permissions)
//! would fail across the whole window identically. Every ICE socket gets the receive and send
//! buffers libwebrtc gives its own (`UDP_SOCKET_BUFFER_SIZE`), clamped by the kernel where its
//! per-socket ceiling is lower.

use std::io;
use std::net::{IpAddr, SocketAddr};

use rand::seq::SliceRandom;
use socket2::{Domain, Socket, Type};

/// Asked for both directions, the size libwebrtc gives its own UDP sockets.
pub const UDP_SOCKET_BUFFER_SIZE: usize = 262_144;

/// The local addresses host candidates are gathered on, but those of the interfaces `ignored`
/// names: each entry an interface name, or a prefix of names ending in `*`.
pub fn host_addresses(use_ipv4: bool, use_ipv6: bool, ignored: &[String]) -> Vec<IpAddr> {
    let Ok(interfaces) = rtc_shared::ifaces::ifaces() else {
        return Vec::new();
    };
    gatherable(
        interfaces
            .iter()
            .filter_map(|interface| Some((interface.name.as_str(), interface.addr?))),
        use_ipv4,
        use_ipv6,
        ignored,
    )
}

/// The addresses among `interfaces`, each an interface name and one of its addresses, that
/// [`host_addresses`] gathers on.
fn gatherable<'a>(
    interfaces: impl IntoIterator<Item = (&'a str, SocketAddr)>,
    use_ipv4: bool,
    use_ipv6: bool,
    ignored: &[String],
) -> Vec<IpAddr> {
    let mut addresses = Vec::new();
    for (name, addr) in interfaces {
        if ignored.iter().any(|pattern| names_interface(pattern, name)) {
            continue;
        }
        match addr {
            SocketAddr::V4(v4) if use_ipv4 => {
                if !v4.ip().is_loopback() {
                    addresses.push(IpAddr::V4(*v4.ip()));
                }
            }
            SocketAddr::V6(v6) if use_ipv6 && !v6.ip().is_loopback() && v6.scope_id() == 0 => {
                addresses.push(IpAddr::V6(*v6.ip()));
            }
            _ => {}
        }
    }
    addresses.dedup();
    addresses
}

/// Whether `pattern` names the interface `name`: the name itself, or, ending in `*`, every name
/// that begins with what precedes it.
fn names_interface(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == pattern,
    }
}

/// Give an ICE or relay socket its receive and send buffers.
pub fn size_udp_socket(socket: &Socket) {
    let _ = socket.set_recv_buffer_size(UDP_SOCKET_BUFFER_SIZE);
    let _ = socket.set_send_buffer_size(UDP_SOCKET_BUFFER_SIZE);
}

/// The ports a host candidate socket tries, in the order it tries them: the window shuffled,
/// or the ephemeral port alone.
pub fn bind_ports(port_range: Option<(u16, u16)>) -> Vec<u16> {
    match port_range {
        Some((lo, hi)) => {
            let mut ports: Vec<u16> = (lo..=hi).collect();
            ports.shuffle(&mut rand::rng());
            ports
        }
        None => vec![0],
    }
}

/// Bind a non-blocking UDP socket on `address` at the first of `ports` the kernel grants.
///
/// An occupied port (`EADDRINUSE`) costs one try; any other failure ends the attempt, since
/// no other port could cure it.
pub fn bind_udp(address: IpAddr, ports: &[u16]) -> io::Result<tokio::net::UdpSocket> {
    first_bound(ports, |port| bind_udp_once(address, port))
}

/// The first of `ports` that `bind` grants, moving past an occupied port and nothing else.
fn first_bound<T>(ports: &[u16], mut bind: impl FnMut(u16) -> io::Result<T>) -> io::Result<T> {
    let mut last = io::Error::new(io::ErrorKind::AddrNotAvailable, "no port to bind");
    for &port in ports {
        match bind(port) {
            Ok(bound) => return Ok(bound),
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

fn bind_udp_once(address: IpAddr, port: u16) -> io::Result<tokio::net::UdpSocket> {
    let domain = if address.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = Socket::new(domain, Type::DGRAM, None)?;
    if address.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    socket.set_nonblocking(true)?;
    size_udp_socket(&socket);
    socket.bind(&SocketAddr::new(address, port).into())?;
    tokio::net::UdpSocket::from_std(socket.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_is_left_out_and_zones_mark_link_local() {
        let all = host_addresses(true, true, &[]);
        assert!(all.iter().all(|a| !a.is_loopback()));
        assert!(all.iter().all(|a| match a {
            IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) != 0xfe80,
            IpAddr::V4(_) => true,
        }));
        assert!(host_addresses(false, false, &[]).is_empty());
    }

    #[test]
    fn an_ignored_interface_offers_none_of_its_addresses() {
        let interfaces: Vec<(&str, SocketAddr)> = [
            ("lo", "127.0.0.1:0"),
            ("eth0", "10.1.0.81:0"),
            ("eth0", "[2001:db8::81]:0"),
            ("docker0", "172.17.0.1:0"),
            ("docker0", "[fd00:dead:beef::1]:0"),
            ("br-0123456789ab", "172.18.0.1:0"),
            ("br0", "192.168.1.2:0"),
        ]
        .map(|(name, addr)| (name, addr.parse().unwrap()))
        .into();
        let ignored = ["docker0".to_string(), "br-*".to_string()];
        let expect: Vec<IpAddr> = ["10.1.0.81", "2001:db8::81", "192.168.1.2"]
            .map(|ip| ip.parse().unwrap())
            .into();
        assert_eq!(
            gatherable(interfaces.iter().copied(), true, true, &ignored),
            expect
        );
        assert_eq!(
            gatherable(interfaces.iter().copied(), true, true, &[]).len(),
            6,
            "with nothing ignored, every address but loopback's is gathered"
        );
    }

    #[test]
    fn a_pattern_names_an_interface_whole_or_by_a_trailing_asterisk() {
        assert!(names_interface("docker0", "docker0"));
        assert!(!names_interface("docker0", "docker01"));
        assert!(names_interface("br-*", "br-0123456789ab"));
        assert!(!names_interface("br-*", "br0"));
        assert!(names_interface("*", "eth0"));
        assert!(!names_interface("", "eth0"));
    }

    /// Which bind failures are worth another port, by the ports they cost: only an occupied
    /// port can be cured by trying another.
    #[test]
    fn what_each_refusal_costs() {
        let window = [50000, 50001, 50002, 50003];
        for (kind, expect) in [
            (io::ErrorKind::AddrInUse, 4),
            (io::ErrorKind::PermissionDenied, 1),
            (io::ErrorKind::AddrNotAvailable, 1),
        ] {
            let mut tried = 0;
            let result: io::Result<()> = first_bound(&window, |_| {
                tried += 1;
                Err(io::Error::from(kind))
            });
            assert_eq!(result.unwrap_err().kind(), kind);
            assert_eq!(
                tried, expect,
                "{kind:?} costs {expect} of the window's ports"
            );
        }
    }

    #[test]
    fn a_window_is_shuffled_and_the_default_is_ephemeral() {
        let ports = bind_ports(Some((50000, 50003)));
        let mut sorted = ports;
        sorted.sort_unstable();
        assert_eq!(sorted, vec![50000, 50001, 50002, 50003]);
        assert_eq!(bind_ports(None), vec![0]);
    }
}
