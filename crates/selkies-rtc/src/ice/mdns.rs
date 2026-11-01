/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Resolving a remote candidate's mDNS name.
//!
//! A browser hides its host candidates behind a `<uuid>.local` name, so a peer's direct address
//! reaches a full agent either through this lookup or as a peer-reflexive candidate from its
//! checks. A lookup holds a lookup's worth of time, and the signaling that hands candidates over
//! is shared, so it runs on a task of its own (`IceConnection::add_remote_candidate`), never on a
//! caller's await. Each lookup binds the mDNS port with address and port reuse, joins the
//! multicast group, and drives the sans-IO `rtc_mdns` client until an answer or the timeout. Only
//! resolving is carried: this agent publishes no name, its host candidates being reachable as
//! advertised.

use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use rtc_mdns::{Mdns, MdnsConfig, MdnsEvent, MulticastSocket};
use rtc_shared::{TaggedBytesMut, TransportContext, TransportProtocol};
use sansio::Protocol as _;
use tokio::net::UdpSocket;

/// How long a name is given to answer.
pub const LOOKUP_TIMEOUT: Duration = Duration::from_secs(1);
/// How often an unanswered query is sent again within the timeout.
const QUERY_INTERVAL: Duration = Duration::from_millis(300);

/// What resolves a `.local` name, off the caller's task.
pub type MdnsLookup =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = Option<IpAddr>> + Send>> + Send + Sync>;

/// The lookup a connection uses unless it is given another: `resolve` over the network.
pub fn default_lookup() -> MdnsLookup {
    Arc::new(|name| Box::pin(async move { resolve(&name, LOOKUP_TIMEOUT).await }))
}

/// Whether `name` has the `<label>.local` form of an mDNS candidate.
pub fn is_mdns_hostname(name: &str) -> bool {
    name.strip_suffix(".local").is_some_and(|label| {
        (1..=63).contains(&label.len())
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

/// Resolve `name` over mDNS, or `None` when nothing answers within `timeout` or the mDNS port
/// cannot be joined.
pub async fn resolve(name: &str, timeout: Duration) -> Option<IpAddr> {
    let socket = MulticastSocket::new().into_std().ok()?;
    let socket = UdpSocket::from_std(socket).ok()?;
    let local = socket.local_addr().ok()?;
    let now = Instant::now();
    let mut mdns = Mdns::new(
        now,
        MdnsConfig::default()
            .with_query_interval(QUERY_INTERVAL)
            .with_query_timeout(timeout),
    );
    mdns.query_now(now, name);
    let deadline = tokio::time::Instant::now() + timeout;
    let mut buf = BytesMut::with_capacity(9000);
    loop {
        while let Some(message) = mdns.poll_write() {
            let _ = socket
                .send_to(&message.message, message.transport.peer_addr)
                .await;
        }
        while let Some(event) = mdns.poll_event() {
            match event {
                MdnsEvent::QueryAnswered(_, address) => return Some(address),
                MdnsEvent::QueryTimeout(_) => return None,
                _ => {}
            }
        }
        let wake = mdns
            .poll_timeout()
            .map(tokio::time::Instant::from_std)
            .unwrap_or(deadline)
            .min(deadline);
        buf.clear();
        tokio::select! {
            received = socket.recv_buf_from(&mut buf) => {
                let (_, peer) = received.ok()?;
                let _ = mdns.handle_read(TaggedBytesMut {
                    now: Instant::now(),
                    transport: TransportContext {
                        local_addr: local,
                        peer_addr: peer,
                        transport_protocol: TransportProtocol::UDP,
                        ecn: None,
                    },
                    message: buf.split(),
                });
            }
            _ = tokio::time::sleep_until(wake) => {
                if tokio::time::Instant::now() >= deadline {
                    return None;
                }
                let _ = mdns.handle_timeout(Instant::now());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_label_dot_local_form_is_an_mdns_name() {
        assert!(is_mdns_hostname(
            "3f1c9f0e-77aa-4d1b-9a39-2a2d2a2d2a2d.local"
        ));
        assert!(!is_mdns_hostname(".local"));
        assert!(!is_mdns_hostname("a.b.local"));
        assert!(!is_mdns_hostname("127.0.0.1"));
        assert!(!is_mdns_hostname(&format!("{}.local", "a".repeat(64))));
    }

    /// A responder on the network answers: the real path, multicast through the sandbox's
    /// interface.
    #[tokio::test]
    async fn a_name_published_on_the_network_resolves_and_a_silent_one_does_not() {
        let name = format!(
            "selkies-{}.local",
            crate::ice::stun::random_string(10).to_lowercase()
        );
        let responder_ip: IpAddr = "192.0.2.77".parse().unwrap();
        let socket = UdpSocket::from_std(MulticastSocket::new().into_std().unwrap()).unwrap();
        let local = socket.local_addr().unwrap();
        let mut server = Mdns::new(
            Instant::now(),
            MdnsConfig::default()
                .with_local_names(vec![name.clone()])
                .with_local_ip(responder_ip),
        );
        let responder = tokio::spawn(async move {
            let mut buf = BytesMut::with_capacity(9000);
            loop {
                buf.clear();
                let Ok((_, peer)) = socket.recv_buf_from(&mut buf).await else {
                    return;
                };
                let _ = server.handle_read(TaggedBytesMut {
                    now: Instant::now(),
                    transport: TransportContext {
                        local_addr: local,
                        peer_addr: peer,
                        transport_protocol: TransportProtocol::UDP,
                        ecn: None,
                    },
                    message: buf.split(),
                });
                while let Some(message) = server.poll_write() {
                    let _ = socket
                        .send_to(&message.message, message.transport.peer_addr)
                        .await;
                }
            }
        });
        assert_eq!(
            resolve(&name, Duration::from_secs(3)).await,
            Some(responder_ip)
        );
        let started = Instant::now();
        assert_eq!(
            resolve("selkies-nobody.local", Duration::from_millis(400)).await,
            None
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the lookup is bounded"
        );
        responder.abort();
    }
}
