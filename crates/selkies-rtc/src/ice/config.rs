/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The ICE topology, decided once at startup, and the per-peer configuration it hands out.
//!
//! `IceTopology` is what the integrator fills from the settings (`webrtc_udp_mux_port`,
//! `webrtc_tcp_mux_port`, `webrtc_ice_lite`, `webrtc_port_range`, `webrtc_public_ip`, and the
//! STUN and TURN servers resolved for the deployment); `open_muxes` binds the shared ports
//! ahead of the first peer, so a port in use fails the service rather than a session; and
//! `IceTopology::peer_config` is the `RTCConfiguration` of a new peer: the muxes, the lite
//! choice, the port range, the public addresses, and the servers. An ICE-lite agent gathers
//! host candidates only, so the servers are left out of its configuration and serve the
//! client side alone. ICE server URLs are parsed as RFC 7064 and RFC 7065 spell them; one STUN
//! server and one TURN server are taken, `turn` over `udp` or `tcp`, `turns` over `tcp`.

use std::net::IpAddr;
use std::sync::Arc;

use log::{error, info, warn};

use super::gather::host_addresses;
use super::mdns::{MdnsLookup, default_lookup};
use super::mux::{TcpMux, UdpMux};
use super::{IceError, Protocol};

/// One STUN or TURN server as a peer connection is configured with.
///
/// Its `Debug` form names the URLs and hides the credentials, which are live.
#[derive(Clone, PartialEq, Eq)]
pub struct IceServer {
    pub urls: Vec<String>,
    pub username: Option<String>,
    pub credential: Option<String>,
}

impl IceServer {
    /// A server with no credentials, as a STUN server is.
    pub fn new(url: &str) -> IceServer {
        IceServer {
            urls: vec![url.to_string()],
            username: None,
            credential: None,
        }
    }
}

impl std::fmt::Debug for IceServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IceServer")
            .field("urls", &self.urls)
            .finish_non_exhaustive()
    }
}

/// A parsed `stun:`/`stuns:`/`turn:`/`turns:` URI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IceUri {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    /// `udp` or `tcp` for TURN; a STUN URI carries none.
    pub transport: Option<String>,
}

/// Parse an ICE server URI (RFC 7064, RFC 7065).
///
/// A missing port is 3478, or 5349 for the TLS schemes; a `turn` URI without a transport is
/// UDP, a `turns` one TCP; a STUN URI must not carry a transport.
pub fn parse_stun_turn_uri(uri: &str) -> Result<IceUri, IceError> {
    let (scheme, rest) = uri
        .split_once(':')
        .ok_or_else(|| IceError::Config("malformed uri: invalid scheme".into()))?;
    if !matches!(scheme, "stun" | "stuns" | "turn" | "turns") {
        return Err(IceError::Config("malformed uri: invalid scheme".into()));
    }
    let (hostport, query) = match rest.split_once('?') {
        Some((h, q)) => (h, Some(q)),
        None => (rest, None),
    };
    let (host, port) = split_host_port(hostport)?;
    if host.is_empty() {
        return Err(IceError::Config("malformed uri".into()));
    }
    let port = match port {
        Some(p) => p,
        None if scheme.ends_with('s') => 5349,
        None => 3478,
    };
    let mut transport = match query {
        Some(q) => match q.strip_prefix("transport=") {
            Some(t) => Some(t.to_string()),
            None => return Err(IceError::Config("malformed uri".into())),
        },
        None => None,
    };
    match scheme {
        "stun" | "stuns" => {
            if transport.is_some() {
                return Err(IceError::Config(format!(
                    "malformed uri: {scheme} must not contain transport"
                )));
            }
        }
        "turn" => transport = transport.or_else(|| Some("udp".into())),
        _ => transport = transport.or_else(|| Some("tcp".into())),
    }
    Ok(IceUri {
        scheme: scheme.to_string(),
        host: host.to_string(),
        port,
        transport,
    })
}

fn split_host_port(hostport: &str) -> Result<(&str, Option<u16>), IceError> {
    if let Some(rest) = hostport.strip_prefix('[') {
        let (host, after) = rest
            .split_once(']')
            .ok_or_else(|| IceError::Config("malformed uri".into()))?;
        let port = match after.strip_prefix(':') {
            Some(p) => Some(
                p.parse()
                    .map_err(|_| IceError::Config("malformed uri".into()))?,
            ),
            None if after.is_empty() => None,
            None => return Err(IceError::Config("malformed uri".into())),
        };
        return Ok((host, port));
    }
    match hostport.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => {
            let port = port
                .parse()
                .map_err(|_| IceError::Config("malformed uri".into()))?;
            Ok((host, Some(port)))
        }
        _ => Ok((hostport, None)),
    }
}

/// The TURN server a connection relays through.
#[derive(Clone, PartialEq, Eq)]
pub struct TurnServer {
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub transport: Protocol,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl std::fmt::Debug for TurnServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnServer")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("tls", &self.tls)
            .field("transport", &self.transport)
            .finish_non_exhaustive()
    }
}

/// The one STUN and one TURN server a connection uses, as `connection_kwargs` picked them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Servers {
    pub stun: Option<(String, u16)>,
    pub turn: Option<TurnServer>,
}

impl Servers {
    /// The first usable STUN and TURN server among `servers`; a URI that does not parse is
    /// skipped, as a browser skips it.
    pub fn from_ice_servers(servers: &[IceServer]) -> Servers {
        let mut out = Servers::default();
        for server in servers {
            for url in &server.urls {
                let Ok(uri) = parse_stun_turn_uri(url) else {
                    continue;
                };
                match uri.scheme.as_str() {
                    "stun" => {
                        if out.stun.is_none() {
                            out.stun = Some((uri.host, uri.port));
                        }
                    }
                    "turn" | "turns" => {
                        if out.turn.is_some() {
                            continue;
                        }
                        let transport = match (uri.scheme.as_str(), uri.transport.as_deref()) {
                            ("turn", Some("udp")) => Protocol::Udp,
                            ("turn", Some("tcp")) | ("turns", Some("tcp")) => Protocol::Tcp,
                            _ => continue,
                        };
                        out.turn = Some(TurnServer {
                            host: uri.host,
                            port: uri.port,
                            tls: uri.scheme == "turns",
                            transport,
                            username: server.username.clone(),
                            password: server.credential.clone(),
                        });
                    }
                    _ => {}
                }
            }
        }
        out
    }
}

/// Parse the `webrtc_port_range` setting (`min-max`) into an inclusive window.
///
/// Empty input keeps ephemeral ports. A malformed or out-of-bounds value is an error so a
/// misconfigured deployment fails loudly instead of binding outside the operator's firewalled
/// window; both bounds must lie within 1024-65535.
pub fn parse_port_range(raw: &str) -> Result<Option<(u16, u16)>, IceError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let shape = || {
        IceError::Config(format!(
            "webrtc_port_range must look like \"50000-50100\", got {raw:?}"
        ))
    };
    let (lo, hi) = raw.split_once('-').ok_or_else(shape)?;
    let lo: u32 = lo.trim().parse().map_err(|_| shape())?;
    let hi: u32 = hi.trim().parse().map_err(|_| shape())?;
    validate_port_range(lo, hi).map(Some)
}

/// Check a port window: 1024 <= min <= max <= 65535.
pub fn validate_port_range(lo: u32, hi: u32) -> Result<(u16, u16), IceError> {
    if !(1024 <= lo && lo <= hi && hi <= 65535) {
        return Err(IceError::Config(format!(
            "webrtc_port_range must satisfy 1024 <= min <= max <= 65535, got ({lo}, {hi})"
        )));
    }
    Ok((lo as u16, hi as u16))
}

/// The ICE topology of the service, decided once from the settings.
#[derive(Clone, Debug, Default)]
pub struct IceTopology {
    /// `webrtc_udp_mux_port`; 0 gives each session sockets of its own.
    pub udp_mux_port: u16,
    /// `webrtc_tcp_mux_port`; 0 offers no TCP candidates.
    pub tcp_mux_port: u16,
    /// `webrtc_ice_lite`.
    pub lite: bool,
    /// `webrtc_port_range`, unused while the UDP mux is set.
    pub port_range: Option<(u16, u16)>,
    /// `webrtc_public_ip`: addresses advertised in host candidates of their family (1:1 NAT).
    pub public_ips: Vec<IpAddr>,
    /// The STUN and TURN servers of the deployment, as the client receives them.
    pub servers: Vec<IceServer>,
}

/// The shared sockets `open_muxes` bound, held by the service for its lifetime.
#[derive(Clone, Default)]
pub struct IceMuxes {
    pub udp: Option<Arc<UdpMux>>,
    pub tcp: Option<Arc<TcpMux>>,
}

impl IceMuxes {
    /// Release the shared ports, ending every peer's ICE that rode them.
    pub async fn close(&mut self) {
        if let Some(udp) = self.udp.take() {
            udp.close().await;
        }
        if let Some(tcp) = self.tcp.take() {
            tcp.close().await;
        }
    }
}

/// Bind the shared ICE ports the topology names, ahead of the first peer.
///
/// `udp_mux_port` binds one UDP socket per host address and `tcp_mux_port` one TCP listener,
/// once for the service; every peer's gatherer then shares them. A port in use fails here, at
/// startup, rather than on a session; nothing stays bound on failure. An address that appears
/// later is bound on first use by a gatherer.
pub async fn open_muxes(topology: &IceTopology) -> Result<IceMuxes, IceError> {
    let mut muxes = IceMuxes::default();
    if topology.udp_mux_port == 0 && topology.tcp_mux_port == 0 {
        return Ok(muxes);
    }
    let addresses = host_addresses(true, true);
    let listed = if addresses.is_empty() {
        "no host address yet".to_string()
    } else {
        addresses
            .iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    if topology.udp_mux_port != 0 {
        let mux = Arc::new(UdpMux::new(topology.udp_mux_port));
        if let Err(e) = mux.open(&addresses).await {
            error!(target: "webrtc", "WebRTC UDP mux: cannot bind port {}: {e}", topology.udp_mux_port);
            return Err(e.into());
        }
        info!(
            target: "webrtc",
            "WebRTC UDP mux: every session's host candidates share UDP port {} on {listed}",
            topology.udp_mux_port
        );
        if topology.port_range.is_some() {
            warn!(target: "webrtc", "webrtc_port_range is unused while webrtc_udp_mux_port is set: the mux port is the only UDP port");
        }
        muxes.udp = Some(mux);
    }
    if topology.tcp_mux_port != 0 {
        let mux = Arc::new(TcpMux::new(topology.tcp_mux_port));
        if let Err(e) = mux.open(&addresses).await {
            error!(target: "webrtc", "WebRTC TCP mux: cannot bind port {}: {e}", topology.tcp_mux_port);
            muxes.close().await;
            return Err(e.into());
        }
        info!(target: "webrtc", "WebRTC TCP mux: ICE-TCP accepted on TCP port {} on {listed}", topology.tcp_mux_port);
        muxes.tcp = Some(mux);
    }
    Ok(muxes)
}

impl IceTopology {
    /// The ICE part of a new peer's `RTCConfiguration`.
    ///
    /// The muxes, when opened, and the ICE-lite choice travel with every peer; an ICE-lite agent
    /// gathers host candidates only, so the servers are left out of its configuration.
    pub fn peer_config(&self, muxes: &IceMuxes) -> IceConfig {
        IceConfig {
            servers: if self.lite {
                Vec::new()
            } else {
                self.servers.clone()
            },
            public_ips: self.public_ips.clone(),
            port_range: self.port_range,
            udp_mux: muxes.udp.clone(),
            tcp_mux: muxes.tcp.clone(),
            lite: self.lite,
            ..IceConfig::default()
        }
    }
}

/// What one peer's ICE agent is built from.
#[derive(Clone)]
pub struct IceConfig {
    /// The STUN and TURN servers; empty for an ICE-lite agent.
    pub servers: Vec<IceServer>,
    /// Addresses advertised in host candidates of their family in place of the bound one.
    pub public_ips: Vec<IpAddr>,
    /// The UDP window the host candidate sockets bind into; unused on a UDP mux.
    pub port_range: Option<(u16, u16)>,
    /// Shared UDP sockets the host candidates ride, or `None` for sockets per peer.
    pub udp_mux: Option<Arc<UdpMux>>,
    /// Shared TCP listeners adding a passive ICE-TCP host candidate per address.
    pub tcp_mux: Option<Arc<TcpMux>>,
    /// Run as an ICE-lite agent: host candidates only, controlled, answering checks.
    pub lite: bool,
    /// Whether this agent takes the controlling role; an ICE-lite agent never does.
    pub controlling: bool,
    pub use_ipv4: bool,
    pub use_ipv6: bool,
    /// The host addresses to gather on; `None` scans the interfaces.
    pub addresses: Option<Vec<IpAddr>>,
    /// Credentials to use in place of random ones.
    pub local_username: Option<String>,
    pub local_password: Option<String>,
    /// What resolves a remote candidate's `.local` name.
    pub mdns_lookup: MdnsLookup,
}

impl Default for IceConfig {
    fn default() -> Self {
        IceConfig {
            servers: Vec::new(),
            public_ips: Vec::new(),
            port_range: None,
            udp_mux: None,
            tcp_mux: None,
            lite: false,
            controlling: true,
            use_ipv4: true,
            use_ipv6: true,
            addresses: None,
            local_username: None,
            local_password: None,
            mdns_lookup: default_lookup(),
        }
    }
}

impl IceConfig {
    /// Check the configuration an agent is built from.
    ///
    /// An ICE-lite agent takes no STUN or TURN server, and a port window must be within
    /// 1024-65535.
    pub fn validate(&self) -> Result<(), IceError> {
        if self.lite && Servers::from_ice_servers(&self.servers) != Servers::default() {
            return Err(IceError::Config(
                "An ICE-lite agent gathers host candidates only; it takes no STUN or TURN server."
                    .into(),
            ));
        }
        if let Some((lo, hi)) = self.port_range {
            validate_port_range(u32::from(lo), u32::from(hi))?;
        }
        if let Some(name) = &self.local_username
            && !super::stun::valid_username(name)
        {
            return Err(IceError::Config(
                "Username must satisfy 4*256ice-char".into(),
            ));
        }
        if let Some(pwd) = &self.local_password
            && !super::stun::valid_password(pwd)
        {
            return Err(IceError::Config(
                "Password must satisfy 22*256ice-char".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_parse_as_the_rfcs_spell_them() {
        let stun = parse_stun_turn_uri("stun:stun.l.google.com:19302").unwrap();
        assert_eq!(
            (
                stun.scheme.as_str(),
                stun.host.as_str(),
                stun.port,
                stun.transport
            ),
            ("stun", "stun.l.google.com", 19302, None)
        );
        let turn = parse_stun_turn_uri("turn:turn.example.test").unwrap();
        assert_eq!((turn.port, turn.transport.as_deref()), (3478, Some("udp")));
        let turns = parse_stun_turn_uri("turns:turn.example.test?transport=tcp").unwrap();
        assert_eq!(
            (turns.port, turns.transport.as_deref()),
            (5349, Some("tcp"))
        );
        let v6 = parse_stun_turn_uri("turn:[2001:db8::5]:3478?transport=tcp").unwrap();
        assert_eq!((v6.host.as_str(), v6.port), ("2001:db8::5", 3478));
        assert!(parse_stun_turn_uri("stun:host?transport=udp").is_err());
        assert!(parse_stun_turn_uri("http://x").is_err());
    }

    #[test]
    fn one_stun_and_one_turn_server_are_taken() {
        let servers = vec![
            IceServer::new("stun:a.test:3478"),
            IceServer::new("stun:b.test:3478"),
            IceServer {
                urls: vec!["turn:relay.test:3478?transport=tcp".into()],
                username: Some("u".into()),
                credential: Some("p".into()),
            },
            IceServer {
                urls: vec!["turn:other.test:3478".into()],
                username: Some("x".into()),
                credential: Some("y".into()),
            },
        ];
        let picked = Servers::from_ice_servers(&servers);
        assert_eq!(picked.stun, Some(("a.test".into(), 3478)));
        let turn = picked.turn.unwrap();
        assert_eq!(
            (turn.host.as_str(), turn.transport, turn.tls),
            ("relay.test", Protocol::Tcp, false)
        );
        assert_eq!(turn.username.as_deref(), Some("u"));
    }

    #[test]
    fn port_ranges_are_rejected_rather_than_clamped() {
        assert_eq!(parse_port_range("").unwrap(), None);
        assert_eq!(
            parse_port_range(" 50000-50100 ").unwrap(),
            Some((50000, 50100))
        );
        for bad in [
            "1023-2000",
            "2000-1000",
            "5000-70000",
            "0-0",
            "50000",
            "a-b",
        ] {
            assert!(parse_port_range(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn lite_refuses_servers_and_leaves_them_to_the_client() {
        let topology = IceTopology {
            lite: true,
            servers: vec![IceServer::new("stun:stun.example.test:3478")],
            ..IceTopology::default()
        };
        let config = topology.peer_config(&IceMuxes::default());
        assert!(config.lite && config.servers.is_empty());
        let full = IceTopology {
            lite: false,
            ..topology.clone()
        }
        .peer_config(&IceMuxes::default());
        assert_eq!(full.servers.len(), 1);
        let bad = IceConfig {
            lite: true,
            servers: vec![IceServer::new("turn:relay.test:3478")],
            ..IceConfig::default()
        };
        assert!(bad.validate().is_err());
    }
}
