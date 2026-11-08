/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! An ICE candidate, its SDP form, and the priority rules of RFC 8445 and RFC 6544.
//!
//! The candidate the SDP layer signals and the agents pair is this type; the `rtc_ice`
//! candidate the full agent keeps is derived from its SDP form when the agent takes it, so one
//! representation crosses every boundary. Priorities follow RFC 8445 5.1.2.1 (`priority`): type
//! preference 126 for host, 110 for peer-reflexive, 100 for server-reflexive, 0 for relay, and a
//! local preference unique per address (`host_local_preference`), counting down from the
//! single-address value so a host on several addresses still ranks them; a TCP candidate takes
//! the RFC 6544 4.2 form, its direction preference (passive 4, active 6) below every UDP
//! candidate, so a session never prefers TCP where UDP works. A foundation groups candidates of
//! one type, transport, and base address; it is a hash of those (never the bound address
//! verbatim), as a peer behind 1:1 NAT must not learn the private one from it.

use std::fmt;
use std::net::{IpAddr, SocketAddr};

use aws_lc_rs::digest::{SHA256, digest};

use super::Protocol;

/// The type of an ICE candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CandidateType {
    Host,
    Srflx,
    Prflx,
    Relay,
}

impl CandidateType {
    /// The type as SDP spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            CandidateType::Host => "host",
            CandidateType::Srflx => "srflx",
            CandidateType::Prflx => "prflx",
            CandidateType::Relay => "relay",
        }
    }

    /// The type SDP spells as `text`.
    pub fn parse(text: &str) -> Option<CandidateType> {
        match text {
            "host" => Some(CandidateType::Host),
            "srflx" => Some(CandidateType::Srflx),
            "prflx" => Some(CandidateType::Prflx),
            "relay" => Some(CandidateType::Relay),
            _ => None,
        }
    }

    /// The RFC 8445 5.1.2.2 type preference.
    pub fn preference(self) -> u32 {
        match self {
            CandidateType::Host => 126,
            CandidateType::Prflx => 110,
            CandidateType::Srflx => 100,
            CandidateType::Relay => 0,
        }
    }
}

/// The direction of an ICE-TCP candidate (RFC 6544 4.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TcpType {
    Active,
    Passive,
    So,
}

impl TcpType {
    /// The direction as SDP spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            TcpType::Active => "active",
            TcpType::Passive => "passive",
            TcpType::So => "so",
        }
    }

    /// The direction SDP spells as `text`.
    pub fn parse(text: &str) -> Option<TcpType> {
        match text {
            "active" => Some(TcpType::Active),
            "passive" => Some(TcpType::Passive),
            "so" => Some(TcpType::So),
            _ => None,
        }
    }
}

/// An ICE candidate as SDP carries it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub foundation: String,
    pub component: u16,
    pub transport: Protocol,
    pub priority: u32,
    /// The address, an IP literal or an mDNS name.
    pub host: String,
    pub port: u16,
    pub kind: CandidateType,
    pub related_address: Option<String>,
    pub related_port: Option<u16>,
    pub tcptype: Option<TcpType>,
    pub generation: Option<u32>,
}

impl Candidate {
    /// Parse a candidate from its SDP attribute value (after `candidate:`).
    ///
    /// `6815297761 1 udp 659136 1.2.3.4 31102 typ host generation 0` is the form; unknown
    /// extension pairs are skipped.
    pub fn from_sdp(sdp: &str) -> Result<Candidate, String> {
        let bits: Vec<&str> = sdp.split_whitespace().collect();
        if bits.len() < 8 {
            return Err("SDP does not have enough properties".into());
        }
        let transport = match bits[2].to_ascii_lowercase().as_str() {
            "udp" => Protocol::Udp,
            "tcp" => Protocol::Tcp,
            other => return Err(format!("unknown transport {other:?}")),
        };
        let kind = CandidateType::parse(bits[7])
            .ok_or_else(|| format!("unknown candidate type {:?}", bits[7]))?;
        let mut candidate = Candidate {
            foundation: bits[0].to_string(),
            component: bits[1].parse().map_err(|_| "bad component")?,
            transport,
            priority: bits[3].parse().map_err(|_| "bad priority")?,
            host: bits[4].to_string(),
            port: bits[5].parse().map_err(|_| "bad port")?,
            kind,
            related_address: None,
            related_port: None,
            tcptype: None,
            generation: None,
        };
        let mut i = 8;
        while i + 1 < bits.len() {
            match bits[i] {
                "raddr" => candidate.related_address = Some(bits[i + 1].to_string()),
                "rport" => candidate.related_port = bits[i + 1].parse().ok(),
                "tcptype" => candidate.tcptype = TcpType::parse(bits[i + 1]),
                "generation" => candidate.generation = bits[i + 1].parse().ok(),
                _ => {}
            }
            i += 2;
        }
        Ok(candidate)
    }

    /// The candidate as an SDP attribute value.
    pub fn to_sdp(&self) -> String {
        let mut sdp = format!(
            "{} {} {} {} {} {} typ {}",
            self.foundation,
            self.component,
            self.transport.as_str(),
            self.priority,
            self.host,
            self.port,
            self.kind.as_str()
        );
        if let Some(addr) = &self.related_address {
            sdp.push_str(&format!(" raddr {addr}"));
        }
        if let Some(port) = self.related_port {
            sdp.push_str(&format!(" rport {port}"));
        }
        if let Some(tcptype) = self.tcptype {
            sdp.push_str(&format!(" tcptype {}", tcptype.as_str()));
        }
        if let Some(generation) = self.generation {
            sdp.push_str(&format!(" generation {generation}"));
        }
        sdp
    }

    /// The candidate's socket address, when its host is an IP literal.
    pub fn addr(&self) -> Option<SocketAddr> {
        self.host
            .parse::<IpAddr>()
            .ok()
            .map(|ip| SocketAddr::new(ip, self.port))
    }

    /// Whether the host is an mDNS name rather than an address.
    pub fn is_mdns(&self) -> bool {
        super::mdns::is_mdns_hostname(&self.host)
    }

    /// Whether a local candidate pairs with `other`: same component, same transport, same IP
    /// version (RFC 8445 6.1.2.2).
    pub fn can_pair_with(&self, other: &Candidate) -> bool {
        match (self.addr(), other.addr()) {
            (Some(a), Some(b)) => {
                self.component == other.component
                    && self.transport == other.transport
                    && a.is_ipv4() == b.is_ipv4()
            }
            _ => false,
        }
    }

    /// The key a connection dedupes remote candidates by: the peer repeating one costs nothing.
    pub fn dedupe_key(&self) -> (u16, Protocol, String, u16, CandidateType, Option<TcpType>) {
        (
            self.component,
            self.transport,
            self.host.clone(),
            self.port,
            self.kind,
            self.tcptype,
        )
    }
}

impl fmt::Display for Candidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Candidate({})", self.to_sdp())
    }
}

/// The foundation of a candidate (RFC 8445 5.1.1.3): one value per type, transport, and base
/// address, 32 hex digits of a hash, so the bound address is never signaled verbatim.
pub fn candidate_foundation(
    kind: CandidateType,
    transport: Protocol,
    base_address: &str,
) -> String {
    let key = format!("{}|{}|{}", kind.as_str(), transport.as_str(), base_address);
    digest(&SHA256, key.as_bytes()).as_ref()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The RFC 8445 5.1.2.1 priority of a candidate.
pub fn candidate_priority(component: u16, kind: CandidateType, local_pref: u32) -> u32 {
    (1 << 24) * kind.preference() + (1 << 8) * local_pref + (256 - u32::from(component))
}

/// Local preference of the host candidate on the `index`-th address.
///
/// Unique per address as RFC 8445 5.1.2.1 requires, counting down from the single-address
/// value; a TCP candidate takes the RFC 6544 4.2 form, its direction preference (passive 4,
/// active 6) below every UDP candidate.
pub fn host_local_preference(index: u32, tcptype: Option<TcpType>) -> u32 {
    match tcptype {
        None => 65535 - index,
        Some(tcptype) => {
            let direction = match tcptype {
                TcpType::Active => 6,
                TcpType::Passive => 4,
                TcpType::So => 2,
            };
            (1 << 13) * direction + (8191 - index)
        }
    }
}

/// RFC 8445 6.1.2.3 pair priority, which orders a check list.
pub fn candidate_pair_priority(local: &Candidate, remote: &Candidate, controlling: bool) -> u64 {
    let (g, d) = if controlling {
        (u64::from(local.priority), u64::from(remote.priority))
    } else {
        (u64::from(remote.priority), u64::from(local.priority))
    };
    (1u64 << 32) * g.min(d) + 2 * g.max(d) + u64::from(g > d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdp_round_trip_keeps_every_field() {
        let sdp = "6815297761 1 udp 659136 1.2.3.4 31102 typ srflx raddr 10.0.0.1 rport 5000 generation 0";
        let c = Candidate::from_sdp(sdp).unwrap();
        assert_eq!(c.kind, CandidateType::Srflx);
        assert_eq!(c.related_address.as_deref(), Some("10.0.0.1"));
        assert_eq!(c.related_port, Some(5000));
        assert_eq!(c.generation, Some(0));
        assert_eq!(c.to_sdp(), sdp);
        let tcp = Candidate::from_sdp("1 1 tcp 1 127.0.0.1 9 typ host tcptype active").unwrap();
        assert_eq!(tcp.tcptype, Some(TcpType::Active));
        assert!(tcp.to_sdp().contains("tcptype active"));
        assert!(Candidate::from_sdp("1 1 udp").is_err());
    }

    #[test]
    fn tcp_ranks_below_udp_and_addresses_are_unique() {
        let udp = candidate_priority(1, CandidateType::Host, host_local_preference(0, None));
        let tcp = candidate_priority(
            1,
            CandidateType::Host,
            host_local_preference(0, Some(TcpType::Passive)),
        );
        assert!(tcp < udp);
        assert_ne!(
            host_local_preference(0, None),
            host_local_preference(1, None)
        );
        assert_eq!(
            candidate_priority(1, CandidateType::Host, 65535),
            2130706431
        );
    }

    #[test]
    fn foundation_hides_the_base_address() {
        let f = candidate_foundation(CandidateType::Host, Protocol::Udp, "192.168.1.10");
        assert_eq!(f.len(), 32);
        assert!(!f.contains("192"));
        assert_eq!(
            f,
            candidate_foundation(CandidateType::Host, Protocol::Udp, "192.168.1.10")
        );
        assert_ne!(
            f,
            candidate_foundation(CandidateType::Host, Protocol::Tcp, "192.168.1.10")
        );
    }
}
