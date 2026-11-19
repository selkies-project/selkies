//! Candidate transports: UDP or TCP, over IPv4 or IPv6.
//!
//! A [`NetworkType`](crate::network_type::NetworkType) pairs the transport protocol with the address family, because ICE treats
//! them together: a candidate's type constrains which remote candidates it can be paired with,
//! and pairs across families are never formed.
#[cfg(test)]
mod network_type_test;

use std::fmt;
use std::net::IpAddr;

use serde::{Deserialize, Serialize};

use shared::TransportProtocol;
use shared::error::*;

pub(crate) const UDP: &str = "udp";
pub(crate) const TCP: &str = "tcp";

#[must_use]
/// Every network type this crate can gather candidates for.
pub fn supported_network_types() -> Vec<NetworkType> {
    vec![
        NetworkType::Udp4,
        NetworkType::Udp6,
        NetworkType::Tcp4,
        NetworkType::Tcp6,
    ]
}

/// Represents the type of network.
#[derive(Default, PartialEq, Debug, Copy, Clone, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum NetworkType {
    #[serde(rename = "unspecified")]
    #[default]
    /// No network type was set.
    Unspecified,

    /// Indicates UDP over IPv4.
    #[serde(rename = "udp4")]
    Udp4,

    /// Indicates UDP over IPv6.
    #[serde(rename = "udp6")]
    Udp6,

    /// Indicates TCP over IPv4.
    #[serde(rename = "tcp4")]
    Tcp4,

    /// Indicates TCP over IPv6.
    #[serde(rename = "tcp6")]
    Tcp6,
}

impl From<u8> for NetworkType {
    fn from(v: u8) -> Self {
        match v {
            1 => Self::Udp4,
            2 => Self::Udp6,
            3 => Self::Tcp4,
            4 => Self::Tcp6,
            _ => Self::Unspecified,
        }
    }
}

impl fmt::Display for NetworkType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match *self {
            Self::Udp4 => "udp4",
            Self::Udp6 => "udp6",
            Self::Tcp4 => "tcp4",
            Self::Tcp6 => "tcp6",
            Self::Unspecified => "unspecified",
        };
        write!(f, "{s}")
    }
}

impl NetworkType {
    /// Returns true when network is UDP4 or UDP6.
    #[must_use]
    pub fn is_udp(self) -> bool {
        self == Self::Udp4 || self == Self::Udp6
    }

    /// Returns true when network is TCP4 or TCP6.
    #[must_use]
    pub fn is_tcp(self) -> bool {
        self == Self::Tcp4 || self == Self::Tcp6
    }

    #[must_use]
    /// The transport protocol, discarding the address family.
    pub fn to_protocol(self) -> TransportProtocol {
        if self.is_tcp() {
            TransportProtocol::TCP
        } else {
            TransportProtocol::UDP
        }
    }

    /// Returns the short network description.
    #[must_use]
    pub fn network_short(self) -> String {
        match self {
            Self::Udp4 | Self::Udp6 => UDP.to_owned(),
            Self::Tcp4 | Self::Tcp6 => TCP.to_owned(),
            Self::Unspecified => "Unspecified".to_owned(),
        }
    }

    /// Returns true if the network is reliable.
    #[must_use]
    pub const fn is_reliable(self) -> bool {
        match self {
            Self::Tcp4 | Self::Tcp6 => true,
            Self::Udp4 | Self::Udp6 | Self::Unspecified => false,
        }
    }

    /// Returns whether the network type is IPv4 or not.
    #[must_use]
    pub const fn is_ipv4(self) -> bool {
        match self {
            Self::Udp4 | Self::Tcp4 => true,
            Self::Udp6 | Self::Tcp6 | Self::Unspecified => false,
        }
    }

    /// Returns whether the network type is IPv6 or not.
    #[must_use]
    pub const fn is_ipv6(self) -> bool {
        match self {
            Self::Udp6 | Self::Tcp6 => true,
            Self::Udp4 | Self::Tcp4 | Self::Unspecified => false,
        }
    }
}

/// Determines the type of network based on the short network string and an IP address.
pub(crate) fn determine_network_type(network: &str, ip: &IpAddr) -> Result<NetworkType> {
    let ipv4 = ip.is_ipv4();
    let net = network.to_lowercase();
    if net.starts_with(UDP) {
        if ipv4 {
            Ok(NetworkType::Udp4)
        } else {
            Ok(NetworkType::Udp6)
        }
    } else if net.starts_with(TCP) {
        if ipv4 {
            Ok(NetworkType::Tcp4)
        } else {
            Ok(NetworkType::Tcp6)
        }
    } else {
        Err(Error::ErrDetermineNetworkType)
    }
}
