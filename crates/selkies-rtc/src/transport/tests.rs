//! Two transports on loopback: ICE, DTLS, and SRTP end to end over real sockets, through the
//! same handle the media and data layers use.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::*;
use crate::dtls::Certificate;
use crate::ice::config::{IceConfig, IceTopology, open_muxes};
use crate::ice::{CandidateType, Protocol};

const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const WAIT: Duration = Duration::from_secs(15);

fn config(ice: IceConfig) -> TransportConfig {
    TransportConfig {
        ice: IceConfig {
            use_ipv6: false,
            addresses: Some(vec![LOOPBACK]),
            ..ice
        },
        certificate: Arc::new(Certificate::generate().unwrap()),
        lite_timings: LiteTimings::default(),
    }
}

/// One side: the handle, its channels, and the events seen so far.
struct Side {
    transport: Transport,
    io: TransportIo,
    events: Vec<TransportEvent>,
}

impl Side {
    async fn new(config: TransportConfig) -> Side {
        let (transport, io) = Transport::new(config).await.unwrap();
        Side {
            transport,
            io,
            events: Vec::new(),
        }
    }

    /// Read events until `done` holds.
    async fn until(&mut self, done: impl Fn(&Transport) -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + WAIT;
        while !done(&self.transport) {
            tokio::select! {
                event = self.io.events.recv() => match event {
                    Some(event) => self.events.push(event),
                    None => return done(&self.transport),
                },
                _ = tokio::time::sleep_until(deadline) => return false,
            }
        }
        true
    }

    async fn next_incoming(&mut self) -> Incoming {
        tokio::time::timeout(WAIT, self.io.incoming.recv())
            .await
            .expect("something arrives")
            .expect("the transport is up")
    }
}

/// Hand each side the other's parameters and candidates, then start both.
async fn signal(a: &Side, b: &Side, roles: (DtlsRole, DtlsRole)) {
    for c in b.transport.local_candidates() {
        a.transport.add_remote_candidate(Some(c)).await.unwrap();
    }
    a.transport.add_remote_candidate(None).await.unwrap();
    for c in a.transport.local_candidates() {
        b.transport.add_remote_candidate(Some(c)).await.unwrap();
    }
    b.transport.add_remote_candidate(None).await.unwrap();
    a.transport
        .start(
            b.transport.ice_parameters(),
            b.transport.dtls_parameters(),
            roles.0,
        )
        .await
        .unwrap();
    b.transport
        .start(
            a.transport.ice_parameters(),
            a.transport.dtls_parameters(),
            roles.1,
        )
        .await
        .unwrap();
}

async fn connected(a: &mut Side, b: &mut Side) {
    let up = |t: &Transport| t.dtls_state() == DtlsState::Connected;
    let (x, y) = tokio::join!(a.until(up), b.until(up));
    assert!(
        x,
        "{:?} ice {:?} dtls {:?}",
        a.events,
        a.transport.ice_state(),
        a.transport.dtls_state()
    );
    assert!(
        y,
        "{:?} ice {:?} dtls {:?}",
        b.events,
        b.transport.ice_state(),
        b.transport.dtls_state()
    );
}

fn rtp(seq: u16, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![0x80, 96, 0, 0, 0, 0, 0, 1, 0x12, 0x34, 0x56, 0x78];
    p[2..4].copy_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(payload);
    p
}

/// Media and data both ways, each kind arriving as what was sent.
async fn exchange(a: &mut Side, b: &mut Side) {
    let packet = rtp(1, b"one-srtp-packet");
    let before = Instant::now();
    let sent = a.transport.send_rtp(&packet).unwrap();
    assert!(sent >= before, "the send time is the write's");
    match b.next_incoming().await {
        Incoming::Rtp { data, arrived } => {
            assert_eq!(&data[..], &packet[..], "the packet is unprotected whole");
            assert!(arrived >= before, "the arrival time is the read's");
        }
        other => panic!("expected RTP, got {other:?}"),
    }
    let back = rtp(1, b"and-back");
    b.transport.send_rtp(&back).unwrap();
    match a.next_incoming().await {
        Incoming::Rtp { data, .. } => assert_eq!(&data[..], &back[..]),
        other => panic!("expected RTP, got {other:?}"),
    }

    let report = vec![0x80, 201, 0, 1, 0x12, 0x34, 0x56, 0x78];
    a.transport.send_rtcp(&report).unwrap();
    match b.next_incoming().await {
        Incoming::Rtcp { data, .. } => assert_eq!(&data[..], &report[..]),
        other => panic!("expected RTCP, got {other:?}"),
    }

    a.transport.send_data(b"an-sctp-packet").unwrap();
    match b.next_incoming().await {
        Incoming::Data(data) => assert_eq!(&data[..], b"an-sctp-packet"),
        other => panic!("expected data, got {other:?}"),
    }
    b.transport.send_data(b"an-sctp-answer").unwrap();
    match a.next_incoming().await {
        Incoming::Data(data) => assert_eq!(&data[..], b"an-sctp-answer"),
        other => panic!("expected data, got {other:?}"),
    }
    let stats = a.transport.stats();
    assert!(
        stats.packets_sent >= 2 && stats.packets_received >= 2 && stats.bytes_sent > 0,
        "{stats:?}"
    );
}

/// The server's production shape: ICE-lite on a shared UDP port, a full client nominating.
#[tokio::test]
async fn lite_server_on_a_mux_against_a_full_client() {
    let topology = IceTopology {
        udp_mux_port: {
            let probe = std::net::UdpSocket::bind((LOOPBACK, 0)).unwrap();
            probe.local_addr().unwrap().port()
        },
        lite: true,
        ..IceTopology::default()
    };
    let mux = crate::ice::UdpMux::new(topology.udp_mux_port);
    mux.open(&[LOOPBACK]).await.unwrap();
    let muxes = crate::ice::IceMuxes {
        udp: Some(Arc::new(mux)),
        tcp: None,
    };
    let mut server = Side::new(config(topology.peer_config(&muxes))).await;
    let mut client = Side::new(config(IceConfig::default())).await;
    assert!(server.transport.is_lite());
    assert_eq!(server.transport.ice_role(), "controlled");
    assert_eq!(client.transport.ice_role(), "controlling");
    assert_eq!(
        server.transport.ice_parameters().username_fragment.len(),
        crate::ice::LOCAL_UFRAG_LENGTH
    );
    let candidate = &server.transport.local_candidates()[0];
    assert_eq!(
        (candidate.port, candidate.kind),
        (topology.udp_mux_port, CandidateType::Host)
    );
    signal(&server, &client, (DtlsRole::Auto, DtlsRole::Auto)).await;
    connected(&mut server, &mut client).await;
    let (local, remote) = server
        .transport
        .selected_pair()
        .expect("the pair is reported");
    assert_eq!(
        (local.port, local.transport),
        (topology.udp_mux_port, Protocol::Udp)
    );
    assert_eq!(remote.port, client.transport.local_candidates()[0].port);
    assert!(
        server
            .events
            .iter()
            .any(|e| matches!(e, TransportEvent::SelectedPair { .. }))
            && server
                .events
                .iter()
                .any(|e| matches!(e, TransportEvent::IceState(IceState::Completed)))
            && server
                .events
                .iter()
                .any(|e| matches!(e, TransportEvent::DtlsState(DtlsState::Connected))),
        "{:?}",
        server.events
    );
    exchange(&mut server, &mut client).await;
    assert!(server.transport.peer_heard_at().elapsed() < WAIT);

    // A close_notify ends the peer's side as well.
    client.transport.close().await;
    assert!(
        server
            .until(|t| t.dtls_state() == DtlsState::Closed && t.ice_state() == IceState::Closed)
            .await,
        "{:?}",
        server.events
    );
    assert!(matches!(
        server.transport.send_rtp(&rtp(2, b"late")),
        Err(TransportError::NotConnected(_))
    ));
    assert_eq!(
        muxes.udp.as_ref().unwrap().attached(None),
        0,
        "the session left the shared socket"
    );
    muxes.udp.as_ref().unwrap().close().await;
}

/// Two full agents on sockets of their own, the controlling side the DTLS server.
#[tokio::test]
async fn two_full_agents_take_their_dtls_roles_from_ice() {
    let mut a = Side::new(config(IceConfig {
        controlling: true,
        ..IceConfig::default()
    }))
    .await;
    let mut b = Side::new(config(IceConfig {
        controlling: false,
        ..IceConfig::default()
    }))
    .await;
    assert!(matches!(
        a.transport.send_rtp(&rtp(1, b"early")),
        Err(TransportError::NotConnected("RTP"))
    ));
    signal(&a, &b, (DtlsRole::Auto, DtlsRole::Auto)).await;
    connected(&mut a, &mut b).await;
    exchange(&mut a, &mut b).await;
    a.transport.close().await;
    b.transport.close().await;
}

/// A peer presenting a certificate other than the one its SDP fingerprints never connects.
#[tokio::test]
async fn a_fingerprint_mismatch_fails_dtls_and_ends_the_transport() {
    let mut server = Side::new(config(IceConfig {
        lite: true,
        ..IceConfig::default()
    }))
    .await;
    let client = Side::new(config(IceConfig::default())).await;
    for c in client.transport.local_candidates() {
        server
            .transport
            .add_remote_candidate(Some(c))
            .await
            .unwrap();
    }
    for c in server.transport.local_candidates() {
        client
            .transport
            .add_remote_candidate(Some(c))
            .await
            .unwrap();
    }
    let impostor = Certificate::generate().unwrap();
    server
        .transport
        .start(
            client.transport.ice_parameters(),
            DtlsParameters {
                fingerprints: impostor.fingerprints(),
                role: DtlsRole::Auto,
            },
            DtlsRole::Auto,
        )
        .await
        .unwrap();
    client
        .transport
        .start(
            server.transport.ice_parameters(),
            server.transport.dtls_parameters(),
            DtlsRole::Auto,
        )
        .await
        .unwrap();
    assert!(
        server.until(|t| t.dtls_state() == DtlsState::Failed).await,
        "{:?} dtls {:?}",
        server.events,
        server.transport.dtls_state()
    );
    assert!(matches!(
        server.transport.send_rtp(&rtp(1, b"x")),
        Err(TransportError::NotConnected(_))
    ));
    let _ = client.transport.close().await;
}

/// `open_muxes` is what the server calls: nothing here reaches past the topology.
#[tokio::test]
async fn a_peer_config_without_muxes_binds_sockets_of_its_own() {
    let muxes = open_muxes(&IceTopology::default()).await.unwrap();
    let side = Side::new(config(IceTopology::default().peer_config(&muxes))).await;
    assert!(
        side.transport
            .local_candidates()
            .iter()
            .all(|c| c.port != 0)
    );
    side.transport.close().await;
}
