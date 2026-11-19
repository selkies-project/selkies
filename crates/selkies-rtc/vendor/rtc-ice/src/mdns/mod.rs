//#[cfg(test)]
//mod mdns_test;

use mdns::Mdns;
use mdns::MdnsConfig;
use std::net::{IpAddr, Ipv4Addr};
use std::time::{Duration, Instant};
use uuid::Uuid;

use shared::error::Result;

/// Represents the different Multicast modes that ICE can run.
#[derive(Default, PartialEq, Eq, Debug, Copy, Clone)]
#[non_exhaustive]
pub enum MulticastDnsMode {
    /// Means remote mDNS candidates will be discarded, and local host candidates will use IPs.
    Disabled,

    /// Means remote mDNS candidates will be accepted, and local host candidates will use IPs.
    #[default]
    QueryOnly,

    /// Means remote mDNS candidates will be accepted, and local host candidates will use mDNS.
    QueryAndGather,
}

pub(crate) fn generate_multicast_dns_name() -> String {
    // https://tools.ietf.org/id/draft-ietf-rtcweb-mdns-ice-candidates-02.html#gathering
    // The unique name MUST consist of a version 4 UUID as defined in [RFC4122], followed by “.local”.
    // This is a short-lived privacy alias, not a credential. `Uuid::new_v4` obtains randomness
    // from the UUID crate's secure random source without forcing provider ownership into mDNS.
    let u = Uuid::new_v4();
    format!("{u}.local")
}

pub(crate) fn create_multicast_dns(
    now: Instant,
    mdns_mode: MulticastDnsMode,
    mdns_local_name: &str,
    mdns_local_ip: &Option<IpAddr>,
    mdns_query_timeout: &Option<Duration>,
) -> Result<Option<Mdns>> {
    if mdns_mode == MulticastDnsMode::Disabled {
        return Ok(None);
    }

    let mut config = if mdns_mode == MulticastDnsMode::QueryAndGather {
        let local_ip = if let Some(local_ip) = mdns_local_ip {
            *local_ip
        } else {
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))
        };
        log::info!("mDNS is using {local_ip} as local ip");

        MdnsConfig::new()
            .with_local_names(vec![mdns_local_name.to_owned()])
            .with_local_ip(local_ip)
    } else {
        MdnsConfig::new()
    };

    if let Some(query_timeout) = mdns_query_timeout {
        config = config.with_query_timeout(*query_timeout);
    }

    let mdns_server = Mdns::new(now, config);

    Ok(Some(mdns_server))
}
