//! The ICE suites on this layer: ICE-lite, the UDP and TCP muxes, the port window, the
//! remote candidate bounds, consent freshness, and the topology, all on loopback with real
//! sockets and a hand-rolled peer where the browser's side has to be scripted.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{mpsc, oneshot};

use super::candidate::{Candidate, CandidateType, TcpType, candidate_priority};
use super::config::{IceConfig, IceMuxes, IceServer, IceTopology, open_muxes};
use super::connection::{ConnectionEvent, IceConnection, IceParameters};
use super::gather::{bind_udp, host_addresses};
use super::lite::LiteTimings;
use super::mux::{TcpMux, UdpMux, inbound_channel};
use super::stun::{self, Class, Role};
use super::{IceState, Inbound, Protocol};

const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// Held around spawning coturn, and shared by the tests that rebind a port they just closed:
/// a forked child holds a copy of every socket until it execs, keeping a closed port bound.
static FORK: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind((LOOPBACK, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn free_tcp_port() -> u16 {
    std::net::TcpListener::bind((LOOPBACK, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn loopback_config(controlling: bool, lite: bool) -> IceConfig {
    IceConfig {
        controlling,
        lite,
        use_ipv6: false,
        addresses: Some(vec![LOOPBACK]),
        ..IceConfig::default()
    }
}

/// One connection driven to completion in a test: its inbound channel, the data it received,
/// and the STUN requests the peer sent it.
struct Harness {
    conn: IceConnection,
    rx: mpsc::Receiver<Inbound>,
    data: Vec<BytesMut>,
    requests_seen: usize,
    events: Vec<ConnectionEvent>,
}

impl Harness {
    async fn new(config: IceConfig) -> Harness {
        Self::with_timings(config, LiteTimings::default()).await
    }

    async fn with_timings(config: IceConfig, timings: LiteTimings) -> Harness {
        let (tx, mut rx) = inbound_channel();
        let mut conn = IceConnection::new(config, tx, timings).unwrap();
        conn.gather(&mut rx).await.unwrap();
        Harness {
            conn,
            rx,
            data: Vec::new(),
            requests_seen: 0,
            events: Vec::new(),
        }
    }

    fn completed(&self) -> bool {
        matches!(self.conn.state(), IceState::Connected | IceState::Completed)
            && self.conn.selected_pair().is_some()
    }

    fn host_candidates(&self) -> Vec<Candidate> {
        self.conn
            .local_candidates()
            .iter()
            .filter(|c| c.kind == CandidateType::Host)
            .cloned()
            .collect()
    }

    /// One step: the next inbound message or the agent's next deadline, whichever comes first,
    /// and never later than `until`.
    async fn step(&mut self, until: tokio::time::Instant) {
        let deadline = self
            .conn
            .poll_timeout()
            .map(tokio::time::Instant::from_std)
            .unwrap_or(until)
            .min(until);
        tokio::select! {
            message = self.rx.recv() => {
                if let Some(message) = message {
                    if let Inbound::Packet(p) = &message
                        && stun::is_stun(&p.data)
                        && stun::parse(&p.data).map(|m| stun::class(&m) == Class::Request).unwrap_or(false)
                    {
                        self.requests_seen += 1;
                    }
                    if let Some(packet) = self.conn.handle_inbound(message) {
                        self.data.push(packet.data);
                    }
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                self.conn.handle_timeout(Instant::now());
            }
        }
        while let Some(event) = self.conn.poll_event() {
            self.events.push(event);
        }
    }

    async fn run_for(&mut self, duration: Duration) {
        let until = tokio::time::Instant::now() + duration;
        while tokio::time::Instant::now() < until {
            self.step(until).await;
        }
    }

    async fn run_until(&mut self, done: impl Fn(&Harness) -> bool, timeout: Duration) -> bool {
        let until = tokio::time::Instant::now() + timeout;
        while !done(self) && tokio::time::Instant::now() < until {
            self.step(until).await;
        }
        done(self)
    }

    async fn recv(&mut self, timeout: Duration) -> Option<Vec<u8>> {
        if self.run_until(|h| !h.data.is_empty(), timeout).await {
            Some(self.data.remove(0).to_vec())
        } else {
            None
        }
    }

    fn start(&mut self) {
        self.conn.start(Instant::now()).unwrap();
        while let Some(event) = self.conn.poll_event() {
            self.events.push(event);
        }
    }
}

fn pair_up(a: &mut Harness, b: &mut Harness) {
    let now = Instant::now();
    for c in b.conn.local_candidates().to_vec() {
        a.conn.add_remote_candidate(now, Some(c));
    }
    a.conn.add_remote_candidate(now, None);
    for c in a.conn.local_candidates().to_vec() {
        b.conn.add_remote_candidate(now, Some(c));
    }
    b.conn.add_remote_candidate(now, None);
    a.conn.set_remote_parameters(b.conn.local_parameters());
    b.conn.set_remote_parameters(a.conn.local_parameters());
}

async fn both_complete(a: &mut Harness, b: &mut Harness, timeout: Duration) -> bool {
    let (x, y) = tokio::join!(
        a.run_until(Harness::completed, timeout),
        b.run_until(Harness::completed, timeout)
    );
    x && y
}

type Waiters = Arc<Mutex<HashMap<[u8; 12], oneshot::Sender<rtc_stun::message::Message>>>>;

/// The browser's side of a check over UDP, scripted: it checks the server, answers the
/// server's requests after a delay or never, and keeps what else arrives.
struct FakePeer {
    socket: Arc<UdpSocket>,
    ufrag: String,
    pwd: String,
    server_ufrag: String,
    server_pwd: String,
    requests_seen: Arc<AtomicUsize>,
    answer_delay: Arc<Mutex<Option<Duration>>>,
    waiters: Waiters,
}

impl FakePeer {
    async fn new(server: &IceConnection) -> FakePeer {
        let socket = Arc::new(UdpSocket::bind((LOOPBACK, 0)).await.unwrap());
        let pwd = "clientpasswordclientpassword".to_string();
        let requests_seen = Arc::new(AtomicUsize::new(0));
        let answer_delay = Arc::new(Mutex::new(Some(Duration::ZERO)));
        let waiters: Waiters = Arc::new(Mutex::new(HashMap::new()));
        let params = server.local_parameters();
        let reader = socket.clone();
        let (seen, delay, w, p) = (
            requests_seen.clone(),
            answer_delay.clone(),
            waiters.clone(),
            pwd.clone(),
        );
        tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                let Ok((n, from)) = reader.recv_from(&mut buf).await else {
                    return;
                };
                let datagram = buf[..n].to_vec();
                if stun::is_stun(&datagram) {
                    let Ok(message) = stun::parse(&datagram) else {
                        continue;
                    };
                    match stun::class(&message) {
                        Class::Request => {
                            seen.fetch_add(1, Ordering::SeqCst);
                            let delay = *delay.lock().unwrap();
                            if let Some(delay) = delay {
                                let answer = stun::binding_success(&message, from, &p).unwrap();
                                let s = reader.clone();
                                tokio::spawn(async move {
                                    tokio::time::sleep(delay).await;
                                    let _ = s.send_to(&answer, from).await;
                                });
                            }
                        }
                        _ => {
                            if let Some(waiter) =
                                w.lock().unwrap().remove(&message.transaction_id.0)
                            {
                                let _ = waiter.send(message);
                            }
                        }
                    }
                }
            }
        });
        FakePeer {
            socket,
            ufrag: "clientfrag".into(),
            pwd,
            server_ufrag: params.username_fragment,
            server_pwd: params.password,
            requests_seen,
            answer_delay,
            waiters,
        }
    }

    fn parameters(&self) -> IceParameters {
        IceParameters {
            username_fragment: self.ufrag.clone(),
            password: self.pwd.clone(),
            ice_lite: false,
        }
    }

    fn candidate(&self) -> Candidate {
        let addr = self.socket.local_addr().unwrap();
        Candidate::from_sdp(&format!(
            "1 1 udp 2113937151 {} {} typ host",
            addr.ip(),
            addr.port()
        ))
        .unwrap()
    }

    async fn check(
        &self,
        server: SocketAddr,
        nominate: bool,
        controlling: bool,
    ) -> Option<rtc_stun::message::Message> {
        let role = if controlling {
            Role::Controlling(12345)
        } else {
            Role::Controlled(12345)
        };
        let request = stun::binding_request(
            &self.server_ufrag,
            &self.ufrag,
            candidate_priority(1, CandidateType::Prflx, 65535),
            role,
            nominate,
            &self.server_pwd,
        )
        .unwrap();
        let (tx, rx) = oneshot::channel();
        self.waiters
            .lock()
            .unwrap()
            .insert(request.transaction_id.0, tx);
        self.socket.send_to(&request.raw, server).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .ok()
            .and_then(|r| r.ok())
    }
}

fn server_addr(h: &Harness) -> SocketAddr {
    h.host_candidates()
        .iter()
        .find(|c| c.transport == Protocol::Udp)
        .unwrap()
        .addr()
        .unwrap()
}

// --- ICE-lite ----------------------------------------------------------------------------

#[test]
fn lite_construction() {
    let (tx, _rx) = inbound_channel();
    let conn = IceConnection::new(loopback_config(true, true), tx, LiteTimings::default()).unwrap();
    assert!(
        !conn.is_controlling(),
        "an ICE-lite agent is controlled however it was asked"
    );
    assert!(conn.local_parameters().ice_lite);
    for url in ["stun:127.0.0.1:3478", "turn:127.0.0.1:3478"] {
        let config = IceConfig {
            servers: vec![IceServer::new(url)],
            ..loopback_config(false, true)
        };
        let (tx, _rx) = inbound_channel();
        assert!(
            IceConnection::new(config, tx, LiteTimings::default()).is_err(),
            "ICE-lite refuses {url}"
        );
    }
}

async fn lite_against_full(peer_first: bool) {
    let mut server = Harness::new(loopback_config(true, true)).await;
    let mut client = Harness::new(loopback_config(true, false)).await;
    pair_up(&mut server, &mut client);
    let mut remote = server.conn.local_parameters();
    remote.ice_lite = true;
    client.conn.set_remote_parameters(remote);
    if peer_first {
        client.start();
        client.run_for(Duration::from_millis(300)).await;
        server.start();
    } else {
        server.start();
        server.run_for(Duration::from_millis(300)).await;
        client.start();
    }
    assert!(
        both_complete(&mut server, &mut client, Duration::from_secs(10)).await,
        "both complete"
    );
    assert_eq!(client.requests_seen, 0, "the ICE-lite agent sent no check");
    assert!(
        !server.conn.is_controlling() && client.conn.is_controlling(),
        "roles are controlled and controlling"
    );
    server.conn.send(b"lite-to-full").unwrap();
    client.conn.send(b"full-to-lite").unwrap();
    assert_eq!(
        client.recv(Duration::from_secs(3)).await.as_deref(),
        Some(&b"lite-to-full"[..])
    );
    assert_eq!(
        server.recv(Duration::from_secs(3)).await.as_deref(),
        Some(&b"full-to-lite"[..])
    );
    assert!(
        server
            .events
            .iter()
            .any(|e| matches!(e, ConnectionEvent::SelectedPair { .. }))
    );
}

#[tokio::test]
async fn lite_against_full_server_waits_first() {
    lite_against_full(false).await;
}

#[tokio::test]
async fn lite_against_full_peer_checks_first() {
    lite_against_full(true).await;
}

#[tokio::test]
async fn lite_role_conflict_and_inbound_timeout() {
    let timings = LiteTimings {
        inbound_check_timeout: Duration::from_secs(1),
        ..LiteTimings::default()
    };
    let mut server = Harness::with_timings(loopback_config(false, true), timings).await;
    let peer = FakePeer::new(&server.conn).await;
    server.conn.set_remote_parameters(peer.parameters());
    let addr = server_addr(&server);
    let (answer, _) = tokio::join!(
        peer.check(addr, false, false),
        server.run_for(Duration::from_millis(500))
    );
    let answer = answer.expect("answered");
    assert_eq!(stun::class(&answer), Class::Error);
    assert_eq!(
        stun::error_code(&answer),
        Some(487),
        "a peer claiming the controlled role is answered 487"
    );

    let other =
        Candidate::from_sdp(&format!("1 1 udp 1 127.0.0.1 {} typ host", addr.port() + 1)).unwrap();
    server
        .conn
        .add_remote_candidate(Instant::now(), Some(other));
    server.conn.add_remote_candidate(Instant::now(), None);
    let started = Instant::now();
    server.start();
    assert!(
        server
            .run_until(
                |h| h.conn.state() == IceState::Failed,
                Duration::from_secs(5)
            )
            .await,
        "ends in failure"
    );
    let elapsed = started.elapsed().as_secs_f64();
    assert!(
        (0.8..=3.0).contains(&elapsed),
        "within the bound: {elapsed:.1}s"
    );
}

// --- consent freshness -------------------------------------------------------------------

async fn lite_with_fake_peer(timings: LiteTimings) -> (Harness, FakePeer) {
    let mut server = Harness::with_timings(loopback_config(false, true), timings).await;
    let peer = FakePeer::new(&server.conn).await;
    server.conn.set_remote_parameters(peer.parameters());
    server
        .conn
        .add_remote_candidate(Instant::now(), Some(peer.candidate()));
    server.conn.add_remote_candidate(Instant::now(), None);
    server.start();
    let addr = server_addr(&server);
    let (answer, _) = tokio::join!(
        peer.check(addr, true, true),
        server.run_for(Duration::from_millis(300))
    );
    assert_eq!(stun::class(&answer.expect("answered")), Class::Success);
    assert!(server.completed());
    (server, peer)
}

const CONSENT_INTERVAL: Duration = Duration::from_millis(200);
const CONSENT_TIMEOUT: Duration = Duration::from_secs(2);

fn consent_timings() -> LiteTimings {
    LiteTimings {
        consent_interval: CONSENT_INTERVAL,
        consent_timeout: CONSENT_TIMEOUT,
        ..LiteTimings::default()
    }
}

#[tokio::test]
async fn consent_survives_answers_past_the_first_rto() {
    let (mut server, peer) = lite_with_fake_peer(consent_timings()).await;
    *peer.answer_delay.lock().unwrap() = Some(Duration::from_millis(800));
    server.run_for(CONSENT_TIMEOUT.mul_f64(2.5)).await;
    assert!(
        server.completed(),
        "answers past the first RTO keep consent"
    );
    assert!(
        peer.requests_seen.load(Ordering::SeqCst) >= 2,
        "consent checks were sent"
    );
}

#[tokio::test]
async fn consent_lapses_without_answers() {
    let (mut server, peer) = lite_with_fake_peer(consent_timings()).await;
    *peer.answer_delay.lock().unwrap() = None;
    let started = Instant::now();
    // One unanswered check: the RTO doubling from 0.5 s over its retransmissions.
    let check_s = 0.5 + 1.0 + 2.0;
    let bound =
        CONSENT_TIMEOUT.as_secs_f64() + check_s + 1.2 * CONSENT_INTERVAL.as_secs_f64() + 1.0;
    let lost = server
        .run_until(
            |h| h.conn.state() == IceState::Failed,
            Duration::from_secs_f64(bound),
        )
        .await;
    let elapsed = started.elapsed().as_secs_f64();
    assert!(
        lost && elapsed >= CONSENT_TIMEOUT.as_secs_f64() && elapsed <= bound,
        "lost within the timeout and one check: {elapsed:.1}s"
    );
}

// --- remote candidates -------------------------------------------------------------------

#[tokio::test]
async fn remote_candidates_are_deduplicated_and_bounded() {
    let mut conn = Harness::new(loopback_config(true, false)).await;
    let candidate = |host: &str, port: u16| {
        Candidate::from_sdp(&format!("1 1 udp 2113664127 {host} {port} typ host")).unwrap()
    };
    let start = Instant::now();
    let now = Instant::now();
    for _ in 0..2000 {
        conn.conn
            .add_remote_candidate(now, Some(candidate("127.0.0.1", 30000)));
    }
    assert_eq!(
        conn.conn.remote_candidates().len(),
        1,
        "a candidate repeated 2000 times is taken once"
    );
    for port in 31000..31500 {
        conn.conn
            .add_remote_candidate(now, Some(candidate("127.0.0.1", port)));
    }
    assert_eq!(
        conn.conn.remote_candidates().len(),
        super::MAX_REMOTE_CANDIDATES,
        "64 distinct ones in all"
    );
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "2500 offers cost well under a second: {:?}",
        start.elapsed()
    );
}

/// An mDNS resolver that answers after a while, and only for `known`, noting what it was asked.
fn slow_resolver(known: &[(&str, IpAddr)]) -> (super::mdns::MdnsLookup, Arc<Mutex<Vec<String>>>) {
    let known: HashMap<String, IpAddr> = known.iter().map(|(n, a)| (n.to_string(), *a)).collect();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let noted = asked.clone();
    let lookup: super::mdns::MdnsLookup = Arc::new(move |name: String| {
        noted.lock().unwrap().push(name.clone());
        let answer = known.get(&name).copied();
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            answer
        })
    });
    (lookup, asked)
}

#[tokio::test]
async fn mdns_candidates_resolve_off_the_callers_task() {
    let candidate = |host: &str, port: u16| {
        Candidate::from_sdp(&format!("1 1 udp 2113664127 {host} {port} typ host")).unwrap()
    };
    let (lookup, asked) = slow_resolver(&[("b.local", LOOPBACK)]);
    let config = IceConfig {
        mdns_lookup: lookup,
        ..loopback_config(true, false)
    };
    let mut conn = Harness::new(config).await;
    let start = Instant::now();
    let now = Instant::now();
    conn.conn
        .add_remote_candidate(now, Some(candidate("a.local", 40000)));
    conn.conn
        .add_remote_candidate(now, Some(candidate("b.local", 40001)));
    conn.conn.add_remote_candidate(now, None);
    assert!(
        start.elapsed() < Duration::from_millis(100),
        "mDNS candidates and end-of-candidates return without the lookups"
    );
    assert!(
        !conn.conn.remote_candidates_ended(),
        "and end-of-candidates waits for them"
    );
    conn.run_for(Duration::from_millis(500)).await;
    let hosts: Vec<&str> = conn
        .conn
        .remote_candidates()
        .iter()
        .map(|c| c.host.as_str())
        .collect();
    assert_eq!(hosts, vec!["127.0.0.1"], "the name that resolves is taken");
    assert!(
        conn.conn.remote_candidates_ended(),
        "end-of-candidates lands once the lookups settle"
    );
    conn.conn.close();

    let (lookup, asked_again) = slow_resolver(&[]);
    let config = IceConfig {
        mdns_lookup: lookup,
        ..loopback_config(true, false)
    };
    let mut conn = Harness::new(config).await;
    conn.conn
        .add_remote_candidate(Instant::now(), Some(candidate("a.local", 40000)));
    conn.run_for(Duration::from_millis(500)).await;
    asked_again.lock().unwrap().clear();
    conn.conn
        .add_remote_candidate(Instant::now(), Some(candidate("a.local", 40002)));
    conn.run_for(Duration::from_millis(500)).await;
    assert!(
        asked_again.lock().unwrap().is_empty(),
        "a name that did not resolve is not looked up again"
    );
    assert_eq!(asked.lock().unwrap().len(), 2);
}

// --- the UDP mux -------------------------------------------------------------------------

fn mux_config(mux: &Arc<UdpMux>, controlling: bool) -> IceConfig {
    IceConfig {
        udp_mux: Some(mux.clone()),
        ..loopback_config(controlling, false)
    }
}

#[tokio::test]
async fn two_sessions_one_udp_port() {
    let _no_fork = FORK.read().await;
    let port = free_udp_port();
    let mux = Arc::new(UdpMux::new(port));
    mux.open(&[LOOPBACK]).await.unwrap();
    assert_eq!(
        mux.listen_addresses(),
        vec![SocketAddr::new(LOOPBACK, port)],
        "open binds the port on the address"
    );
    let mut servers = [
        Harness::new(mux_config(&mux, false)).await,
        Harness::new(mux_config(&mux, false)).await,
    ];
    let mut clients = [
        Harness::new(loopback_config(true, false)).await,
        Harness::new(loopback_config(true, false)).await,
    ];
    for s in &servers {
        let hosts = s.host_candidates();
        assert!(
            hosts.len() == 1 && hosts[0].port == port && hosts[0].transport == Protocol::Udp,
            "one host candidate on the mux port"
        );
        assert!(
            s.conn.local_parameters().username_fragment.len() >= 8,
            "fragments long enough to key a shared socket"
        );
    }
    assert_eq!(
        mux.attached(Some(LOOPBACK)),
        2,
        "both sessions are attached"
    );
    for (s, c) in servers.iter_mut().zip(clients.iter_mut()) {
        pair_up(s, c);
        s.start();
        c.start();
    }
    let (a, b) = {
        let ([s0, s1], [c0, c1]) = (&mut servers[..], &mut clients[..]) else {
            unreachable!("two of each");
        };
        tokio::join!(
            both_complete(s0, c0, Duration::from_secs(15)),
            both_complete(s1, c1, Duration::from_secs(15))
        )
    };
    assert!(
        a && b,
        "both sessions connect through the one socket at once"
    );
    for (i, (s, c)) in servers.iter_mut().zip(clients.iter_mut()).enumerate() {
        s.conn.send(format!("server-{i}").as_bytes()).unwrap();
        c.conn.send(format!("client-{i}").as_bytes()).unwrap();
    }
    for (i, (s, c)) in servers.iter_mut().zip(clients.iter_mut()).enumerate() {
        assert_eq!(
            c.recv(Duration::from_secs(3)).await,
            Some(format!("server-{i}").into_bytes()),
            "each client receives its own server's datagram"
        );
        assert_eq!(
            s.recv(Duration::from_secs(3)).await,
            Some(format!("client-{i}").into_bytes()),
            "each server receives its own client's datagram"
        );
    }
    assert!(
        servers[0].recv(Duration::from_millis(400)).await.is_none()
            && clients[1].recv(Duration::from_millis(400)).await.is_none(),
        "nothing crosses"
    );

    let stranger = UdpSocket::bind((LOOPBACK, 0)).await.unwrap();
    stranger
        .send_to(b"\x16\x00garbage", (LOOPBACK, port))
        .await
        .unwrap();
    let request = stun::binding_request(
        "nobodyhere",
        &clients[0].conn.local_parameters().username_fragment,
        1,
        Role::Controlling(1),
        false,
        "wrongpassword",
    )
    .unwrap();
    stranger
        .send_to(&request.raw, (LOOPBACK, port))
        .await
        .unwrap();
    let mut buf = [0u8; 1500];
    let answered = tokio::time::timeout(Duration::from_millis(500), stranger.recv_from(&mut buf))
        .await
        .is_ok();
    assert!(!answered, "a stranger's datagrams are dropped unanswered");
    servers[0].conn.send(b"still-there").unwrap();
    assert_eq!(
        clients[0].recv(Duration::from_secs(3)).await.as_deref(),
        Some(&b"still-there"[..]),
        "the sessions are undisturbed"
    );

    servers[0].conn.close();
    clients[0].conn.close();
    assert_eq!(
        mux.attached(Some(LOOPBACK)),
        1,
        "closing one session detaches it alone"
    );
    servers[1].conn.send(b"after-close").unwrap();
    clients[1].conn.send(b"after-close-back").unwrap();
    assert_eq!(
        clients[1].recv(Duration::from_secs(3)).await.as_deref(),
        Some(&b"after-close"[..])
    );
    assert_eq!(
        servers[1].recv(Duration::from_secs(3)).await.as_deref(),
        Some(&b"after-close-back"[..]),
        "the remaining session keeps its socket"
    );
    mux.close().await;
    assert!(
        mux.attached(None) == 0 && mux.listen_addresses().is_empty(),
        "closing the mux detaches everything"
    );
    let rebound = UdpMux::new(port);
    rebound
        .open(&[LOOPBACK])
        .await
        .expect("the port is free on return, whoever still holds the point");
    rebound.close().await;
    assert!(
        servers[1]
            .run_until(
                |h| h.conn.state() == IceState::Failed,
                Duration::from_secs(3)
            )
            .await,
        "closing the mux ends the session that rode it"
    );
}

#[tokio::test]
async fn checks_need_credentials() {
    let port = free_udp_port();
    let mux = Arc::new(UdpMux::new(port));
    mux.open(&[LOOPBACK]).await.unwrap();
    let mut server = Harness::new(mux_config(&mux, false)).await;
    let params = server.conn.local_parameters();
    let stranger = UdpSocket::bind((LOOPBACK, 0)).await.unwrap();
    let stranger_addr = stranger.local_addr().unwrap();
    let check_with = |key: Option<&str>| {
        let request = match key {
            Some(key) => stun::binding_request(
                &params.username_fragment,
                "peerfragment",
                1,
                Role::Controlling(1),
                false,
                key,
            )
            .unwrap(),
            None => {
                let mut m = rtc_stun::message::Message::new();
                m.build(&[
                    Box::new(rtc_stun::message::BINDING_REQUEST),
                    Box::new(rtc_stun::message::TransactionId::new()),
                    Box::new(rtc_stun::textattrs::TextAttribute::new(
                        rtc_stun::attributes::ATTR_USERNAME,
                        format!("{}:peerfragment", params.username_fragment),
                    )),
                    Box::new(rtc_ice::attributes::priority::PriorityAttr(1)),
                ])
                .unwrap();
                m
            }
        };
        request.raw
    };
    for (label, key) in [
        ("without MESSAGE-INTEGRITY", None),
        ("with the wrong key", Some("not-the-password")),
    ] {
        let raw = check_with(key);
        stranger.send_to(&raw, (LOOPBACK, port)).await.unwrap();
        let mut buf = [0u8; 1500];
        let (answer, _) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(2), stranger.recv_from(&mut buf)),
            server.run_for(Duration::from_millis(300))
        );
        let (n, _) = answer.expect("answered").unwrap();
        let message = stun::parse(&buf[..n]).unwrap();
        assert!(
            stun::class(&message) == Class::Error && stun::error_code(&message) == Some(400),
            "a check {label} is refused with 400"
        );
        assert!(
            !mux.learned_peers(LOOPBACK).contains(&stranger_addr),
            "and not learned"
        );
    }
    let raw = check_with(Some(&params.password));
    stranger.send_to(&raw, (LOOPBACK, port)).await.unwrap();
    let mut buf = [0u8; 1500];
    let (answer, _) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(2), stranger.recv_from(&mut buf)),
        server.run_for(Duration::from_millis(300))
    );
    let (n, _) = answer.expect("answered").unwrap();
    let message = stun::parse(&buf[..n]).unwrap();
    assert_eq!(
        stun::class(&message),
        Class::Success,
        "a check with the session's credentials is answered"
    );
    assert!(
        mux.learned_peers(LOOPBACK).contains(&stranger_addr),
        "and learned"
    );
    server.conn.close();
    mux.close().await;
}

#[tokio::test]
async fn srflx_through_the_mux() {
    let responder = Arc::new(UdpSocket::bind((LOOPBACK, 0)).await.unwrap());
    let stun_port = responder.local_addr().unwrap().port();
    let requests = Arc::new(AtomicUsize::new(0));
    {
        let (responder, requests) = (responder.clone(), requests.clone());
        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            while let Ok((n, from)) = responder.recv_from(&mut buf).await {
                let Ok(message) = stun::parse(&buf[..n]) else {
                    continue;
                };
                if stun::class(&message) != Class::Request {
                    continue;
                }
                requests.fetch_add(1, Ordering::SeqCst);
                let mut answer = rtc_stun::message::Message::new();
                answer
                    .build(&[
                        Box::new(message.transaction_id),
                        Box::new(rtc_stun::message::BINDING_SUCCESS),
                        Box::new(rtc_stun::xoraddr::XorMappedAddress {
                            ip: from.ip(),
                            port: from.port(),
                        }),
                    ])
                    .unwrap();
                let _ = responder.send_to(&answer.raw, from).await;
            }
        });
    }
    let port = free_udp_port();
    let mux = Arc::new(UdpMux::new(port));
    mux.open(&[LOOPBACK]).await.unwrap();
    let config = IceConfig {
        servers: vec![IceServer::new(&format!("stun:127.0.0.1:{stun_port}"))],
        ..mux_config(&mux, false)
    };
    let server = Harness::new(config).await;
    let srflx: Vec<_> = server
        .conn
        .local_candidates()
        .iter()
        .filter(|c| c.kind == CandidateType::Srflx)
        .collect();
    assert!(
        requests.load(Ordering::SeqCst) == 1
            && srflx.len() == 1
            && srflx[0].port == port
            && srflx[0].related_port == Some(port),
        "the server-reflexive query rides the mux socket: {:?}",
        server.conn.local_candidates()
    );
    assert!(
        !mux.learned_peers(LOOPBACK)
            .contains(&SocketAddr::new(LOOPBACK, stun_port)),
        "the STUN server is not learned as a peer"
    );
    drop(server);
    mux.close().await;
}

#[tokio::test]
async fn a_truncated_attribute_is_dropped_by_the_mux() {
    let port = free_udp_port();
    let mux = Arc::new(UdpMux::new(port));
    mux.open(&[LOOPBACK]).await.unwrap();
    let mut datagram = vec![0x00, 0x01, 0x00, 0x08];
    datagram.extend_from_slice(&0x2112A442u32.to_be_bytes());
    datagram.extend_from_slice(&[9u8; 12]);
    datagram.extend_from_slice(&[0x00, 0x24, 0x00, 0x02, 0x00, 0x01, 0x00, 0x00]);
    let sender = UdpSocket::bind((LOOPBACK, 0)).await.unwrap();
    sender.send_to(&datagram, (LOOPBACK, port)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        mux.listen_addresses().len(),
        1,
        "the socket is still being read"
    );
    mux.close().await;
}

#[tokio::test]
async fn a_udp_port_in_use_fails_open() {
    let blocker = std::net::UdpSocket::bind((LOOPBACK, 0)).unwrap();
    let mux = UdpMux::new(blocker.local_addr().unwrap().port());
    let err = mux
        .open(&[LOOPBACK])
        .await
        .expect_err("a port in use fails open");
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::AddrInUse,
        "with the kernel's reason"
    );
    assert!(
        mux.listen_addresses().is_empty(),
        "and leaves nothing bound"
    );
}

#[tokio::test]
async fn late_address_nat1to1_and_port_range() {
    let port = free_udp_port();
    let mux = Arc::new(UdpMux::new(port));
    mux.open(&[LOOPBACK]).await.unwrap();
    let second: IpAddr = "127.0.0.2".parse().unwrap();
    let config = IceConfig {
        public_ips: vec!["203.0.113.5".parse().unwrap()],
        port_range: Some((port + 1, port + 1)),
        addresses: Some(vec![LOOPBACK, second]),
        ..mux_config(&mux, false)
    };
    let server = Harness::new(config).await;
    let mut listening = mux.listen_addresses();
    listening.sort();
    assert_eq!(
        listening,
        vec![
            SocketAddr::new(LOOPBACK, port),
            SocketAddr::new(second, port)
        ],
        "an address the mux was not opened on is bound on first use"
    );
    let candidates = server.host_candidates();
    assert_eq!(
        candidates.iter().map(|c| c.port).collect::<Vec<_>>(),
        vec![port, port],
        "every host candidate sits on the mux port"
    );
    assert!(
        candidates.iter().all(|c| c.host == "203.0.113.5"),
        "NAT1TO1 rewrites the muxed candidates' address"
    );
    let priorities: std::collections::HashSet<u32> =
        candidates.iter().map(|c| c.priority).collect();
    assert_eq!(
        priorities.len(),
        2,
        "local preferences are unique per address"
    );
    drop(server);
    mux.close().await;
}

// --- the TCP mux -------------------------------------------------------------------------

/// The browser's side of ICE-TCP: one stream to the passive candidate, RFC 4571 frames.
struct ActivePeer {
    writer: Arc<tokio::sync::Mutex<tokio::net::tcp::OwnedWriteHalf>>,
    reader: tokio::task::JoinHandle<()>,
    local_addr: SocketAddr,
    ufrag: String,
    pwd: String,
    server_ufrag: String,
    server_pwd: String,
    requests_seen: Arc<AtomicUsize>,
    waiters: Waiters,
    data: mpsc::Receiver<Vec<u8>>,
}

impl Drop for ActivePeer {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

async fn write_frame(writer: &tokio::sync::Mutex<tokio::net::tcp::OwnedWriteHalf>, data: &[u8]) {
    let mut frame = Vec::with_capacity(2 + data.len());
    frame.extend_from_slice(&(data.len() as u16).to_be_bytes());
    frame.extend_from_slice(data);
    let _ = writer.lock().await.write_all(&frame).await;
}

impl ActivePeer {
    async fn open(server: &IceConnection, port: u16) -> ActivePeer {
        let stream = TcpStream::connect((LOOPBACK, port)).await.unwrap();
        let local_addr = stream.local_addr().unwrap();
        let (mut reader, writer) = stream.into_split();
        let writer = Arc::new(tokio::sync::Mutex::new(writer));
        let params = server.local_parameters();
        let pwd = "clientpasswordclientpassword".to_string();
        let requests_seen = Arc::new(AtomicUsize::new(0));
        let waiters: Waiters = Arc::new(Mutex::new(HashMap::new()));
        let (data_tx, data) = mpsc::channel(64);
        let (seen, w, p) = (requests_seen.clone(), waiters.clone(), pwd.clone());
        let answers = writer.clone();
        let reader = tokio::spawn(async move {
            loop {
                let mut header = [0u8; 2];
                if reader.read_exact(&mut header).await.is_err() {
                    break;
                }
                let len = u16::from_be_bytes(header) as usize;
                let mut frame = vec![0u8; len];
                if reader.read_exact(&mut frame).await.is_err() {
                    break;
                }
                if stun::is_stun(&frame) {
                    let Ok(message) = stun::parse(&frame) else {
                        continue;
                    };
                    if stun::class(&message) == Class::Request {
                        seen.fetch_add(1, Ordering::SeqCst);
                        let answer = stun::binding_success(&message, local_addr, &p).unwrap();
                        write_frame(&answers, &answer).await;
                    } else if let Some(waiter) = w.lock().unwrap().remove(&message.transaction_id.0)
                    {
                        let _ = waiter.send(message);
                    }
                } else {
                    let _ = data_tx.send(frame).await;
                }
            }
        });
        ActivePeer {
            writer,
            reader,
            local_addr,
            ufrag: "clientfrag".into(),
            pwd,
            server_ufrag: params.username_fragment,
            server_pwd: params.password,
            requests_seen,
            waiters,
            data,
        }
    }

    async fn send_frame(&mut self, data: &[u8]) {
        write_frame(&self.writer, data).await;
    }

    async fn check(
        &mut self,
        nominate: bool,
        controlling: bool,
    ) -> Option<rtc_stun::message::Message> {
        let role = if controlling {
            Role::Controlling(12345)
        } else {
            Role::Controlled(12345)
        };
        let request = stun::binding_request(
            &self.server_ufrag,
            &self.ufrag,
            candidate_priority(1, CandidateType::Prflx, 65535),
            role,
            nominate,
            &self.server_pwd,
        )
        .unwrap();
        let (tx, rx) = oneshot::channel();
        self.waiters
            .lock()
            .unwrap()
            .insert(request.transaction_id.0, tx);
        self.send_frame(&request.raw).await;
        tokio::time::timeout(Duration::from_secs(5), rx)
            .await
            .ok()
            .and_then(|r| r.ok())
    }

    fn parameters(&self) -> IceParameters {
        IceParameters {
            username_fragment: self.ufrag.clone(),
            password: self.pwd.clone(),
            ice_lite: false,
        }
    }
}

/// The active TCP candidate a browser offers: port 9, never connected to.
fn active_candidate() -> Candidate {
    Candidate::from_sdp(&format!(
        "1 1 tcp {} 127.0.0.1 9 typ host tcptype active",
        candidate_priority(1, CandidateType::Host, 24575)
    ))
    .unwrap()
}

fn tcp_config(mux: &Arc<TcpMux>, controlling: bool, lite: bool) -> IceConfig {
    IceConfig {
        tcp_mux: Some(mux.clone()),
        ..loopback_config(controlling, lite)
    }
}

/// Bring `server` and a fresh active peer to a nominated pair over the mux port.
async fn connected_over_tcp(server: &mut Harness, port: u16, label: &str) -> ActivePeer {
    server
        .conn
        .add_remote_candidate(Instant::now(), Some(active_candidate()));
    server.conn.add_remote_candidate(Instant::now(), None);
    let mut peer = ActivePeer::open(&server.conn, port).await;
    server.conn.set_remote_parameters(peer.parameters());
    server.start();
    server.run_for(Duration::from_millis(200)).await;
    assert!(
        !server.completed(),
        "{label}: the passive side waits, it never dials the active candidate"
    );
    let (answer, _) = tokio::join!(
        peer.check(true, true),
        server.run_until(Harness::completed, Duration::from_secs(5))
    );
    let answer = answer.expect("answered");
    assert!(
        stun::class(&answer) == Class::Success
            && stun::mapped_address(&answer) == Some(peer.local_addr),
        "{label}: the check over the stream is answered with the peer's address"
    );
    assert!(
        server
            .run_until(Harness::completed, Duration::from_secs(10))
            .await,
        "{label}: ICE completes on the stream"
    );
    peer
}

async fn peer_data(
    peer: &mut ActivePeer,
    server: &mut Harness,
    timeout: Duration,
) -> Option<Vec<u8>> {
    let until = tokio::time::Instant::now() + timeout;
    loop {
        tokio::select! {
            d = peer.data.recv() => return d,
            _ = server.step(tokio::time::Instant::now() + Duration::from_millis(20)) => {}
            _ = tokio::time::sleep_until(until) => return None,
        }
    }
}

#[tokio::test]
async fn tcp_mux_full_lite_strangers_and_port_in_use() {
    let port = free_tcp_port();
    let mux = Arc::new(TcpMux::new(port).with_first_frame_timeout(Duration::from_millis(500)));
    mux.open(&[LOOPBACK]).await.unwrap();
    assert_eq!(
        mux.listen_addresses(),
        vec![SocketAddr::new(LOOPBACK, port)],
        "open listens on the address"
    );

    // A full agent over TCP.
    {
        let mut server = Harness::new(tcp_config(&mux, false, false)).await;
        let candidates = server.host_candidates();
        let tcp: Vec<_> = candidates
            .iter()
            .filter(|c| c.transport == Protocol::Tcp)
            .collect();
        let udp: Vec<_> = candidates
            .iter()
            .filter(|c| c.transport == Protocol::Udp)
            .collect();
        assert!(
            tcp.len() == 1 && tcp[0].port == port && tcp[0].tcptype == Some(TcpType::Passive),
            "a passive TCP host candidate is offered on the mux port"
        );
        assert!(
            !udp.is_empty() && tcp[0].priority < udp[0].priority,
            "it ranks below the UDP host candidate"
        );
        assert!(
            tcp[0].to_sdp().contains("tcptype passive"),
            "the SDP form names the direction"
        );
        let mut peer = connected_over_tcp(&mut server, port, "full agent").await;
        server.run_for(Duration::from_millis(300)).await;
        assert!(
            peer.requests_seen.load(Ordering::SeqCst) >= 1,
            "the full agent checked back over the stream"
        );
        server.conn.send(b"from-server").unwrap();
        assert_eq!(
            peer_data(&mut peer, &mut server, Duration::from_secs(3))
                .await
                .as_deref(),
            Some(&b"from-server"[..]),
            "data reaches the peer as one frame"
        );
        peer.send_frame(b"from-peer").await;
        assert_eq!(
            server.recv(Duration::from_secs(3)).await.as_deref(),
            Some(&b"from-peer"[..]),
            "a frame from the peer is received"
        );
        let big: Vec<u8> = (0..=255u8).cycle().take(1280).collect();
        server.conn.send(&big).unwrap();
        assert_eq!(
            peer_data(&mut peer, &mut server, Duration::from_secs(3)).await,
            Some(big),
            "a frame larger than one TCP segment arrives whole"
        );
        drop(peer);
        assert!(
            server
                .run_until(
                    |h| h.conn.state() == IceState::Failed,
                    Duration::from_secs(3)
                )
                .await,
            "losing the nominated stream ends the connection at once"
        );
        server.conn.close();
    }
    assert_eq!(mux.attached(None), 0, "closing the session detaches it");

    // An ICE-lite agent over TCP.
    {
        let mut server = Harness::new(tcp_config(&mux, false, true)).await;
        let mut peer = connected_over_tcp(&mut server, port, "ICE-lite").await;
        server.run_for(Duration::from_millis(500)).await;
        assert_eq!(
            peer.requests_seen.load(Ordering::SeqCst),
            0,
            "the ICE-lite agent sent no check of its own"
        );
        server.conn.send(b"lite-data").unwrap();
        assert_eq!(
            peer_data(&mut peer, &mut server, Duration::from_secs(3))
                .await
                .as_deref(),
            Some(&b"lite-data"[..]),
            "data flows over the peer-nominated stream"
        );
        server.conn.close();
    }

    // UDP keeps the session when the same peer's TCP stream succeeds later.
    {
        let mut server = Harness::new(tcp_config(&mux, true, false)).await;
        let mut client = Harness::new(loopback_config(false, false)).await;
        let now = Instant::now();
        for c in client.conn.local_candidates().to_vec() {
            server.conn.add_remote_candidate(now, Some(c));
        }
        server
            .conn
            .add_remote_candidate(now, Some(active_candidate()));
        server.conn.add_remote_candidate(now, None);
        for c in server.conn.local_candidates().to_vec() {
            if c.transport == Protocol::Udp {
                client.conn.add_remote_candidate(now, Some(c));
            }
        }
        client.conn.add_remote_candidate(now, None);
        server
            .conn
            .set_remote_parameters(client.conn.local_parameters());
        client
            .conn
            .set_remote_parameters(server.conn.local_parameters());
        server.start();
        client.start();
        assert!(both_complete(&mut server, &mut client, Duration::from_secs(10)).await);
        assert_eq!(
            server.conn.selected_pair().unwrap().0.transport,
            Protocol::Udp,
            "the UDP pair is selected first"
        );
        let mut peer = ActivePeer::open(&server.conn, port).await;
        let params = client.conn.local_parameters();
        peer.ufrag = params.username_fragment;
        peer.pwd = params.password;
        let (answer, _) = tokio::join!(
            peer.check(false, false),
            server.run_for(Duration::from_millis(300))
        );
        assert_eq!(stun::class(&answer.expect("answered")), Class::Success);
        server.run_for(Duration::from_secs(1)).await;
        assert_eq!(
            server.conn.selected_pair().unwrap().0.transport,
            Protocol::Udp,
            "the session stays on UDP"
        );
        server.conn.send(b"still-udp").unwrap();
        assert_eq!(
            client.recv(Duration::from_secs(3)).await.as_deref(),
            Some(&b"still-udp"[..]),
            "data still takes the UDP pair"
        );
        server.conn.close();
        client.conn.close();
    }

    // Strangers are closed.
    {
        let mut server = Harness::new(tcp_config(&mux, false, false)).await;
        async fn first_frame_closes(port: u16, payload: &[u8]) -> bool {
            let mut stream = TcpStream::connect((LOOPBACK, port)).await.unwrap();
            let mut frame = (payload.len() as u16).to_be_bytes().to_vec();
            frame.extend_from_slice(payload);
            stream.write_all(&frame).await.unwrap();
            let mut byte = [0u8; 1];
            matches!(
                tokio::time::timeout(Duration::from_secs(3), stream.read(&mut byte)).await,
                Ok(Ok(0))
            )
        }
        let (closed, _) = tokio::join!(
            first_frame_closes(port, b"\x16\x03\x01hello"),
            server.run_for(Duration::from_millis(200))
        );
        assert!(
            closed,
            "a first frame that is not STUN gets the stream closed"
        );
        let request = stun::binding_request(
            "nobody",
            "clientfrag",
            1,
            Role::Controlling(1),
            false,
            "whatever",
        )
        .unwrap();
        let (closed, _) = tokio::join!(
            first_frame_closes(port, &request.raw),
            server.run_for(Duration::from_millis(200))
        );
        assert!(closed, "a check naming no session gets the stream closed");
        let mut silent = TcpStream::connect((LOOPBACK, port)).await.unwrap();
        let mut byte = [0u8; 1];
        let (read, _) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(3), silent.read(&mut byte)),
            server.run_for(Duration::from_millis(700))
        );
        assert!(
            matches!(read, Ok(Ok(0))),
            "a silent stream is closed at the first-frame deadline"
        );
        server.conn.close();
    }
    mux.close().await;

    let blocker = std::net::TcpListener::bind((LOOPBACK, 0)).unwrap();
    let taken = TcpMux::new(blocker.local_addr().unwrap().port());
    assert!(
        taken.open(&[LOOPBACK]).await.is_err(),
        "a port in use fails open"
    );
}

// --- the port window ---------------------------------------------------------------------

#[test]
fn port_range_bounds_are_rejected_rather_than_clamped() {
    for bad in [(1023, 2000), (2000, 1000), (5000, 70000), (0, 0)] {
        assert!(
            super::config::validate_port_range(bad.0, bad.1).is_err(),
            "rejects {bad:?}"
        );
        let config = IceConfig {
            port_range: Some((bad.0.min(65535) as u16, bad.1.min(65535) as u16)),
            ..loopback_config(true, false)
        };
        if bad != (5000, 70000) {
            let (tx, _rx) = inbound_channel();
            assert!(
                IceConnection::new(config, tx, LiteTimings::default()).is_err(),
                "a connection rejects {bad:?}"
            );
        }
    }
    assert!(
        IceConfig::default().port_range.is_none(),
        "default stays ephemeral"
    );
}

/// A window whose `taken` lowest ports are held, with its highest free.
fn occupied_window(taken: u16) -> ((u16, u16), Vec<std::net::UdpSocket>, u16) {
    for _ in 0..20 {
        let base = free_udp_port();
        if base.checked_add(taken).is_none() {
            continue;
        }
        let mut held = Vec::new();
        let mut ok = true;
        for offset in 0..=taken {
            match std::net::UdpSocket::bind((LOOPBACK, base + offset)) {
                Ok(s) => held.push(s),
                Err(_) => {
                    ok = false;
                    break;
                }
            }
        }
        if !ok {
            continue;
        }
        held.pop();
        return ((base, base + taken), held, base + taken);
    }
    panic!("no window of consecutive free UDP ports on 127.0.0.1");
}

#[tokio::test]
async fn host_candidates_bind_inside_the_window() {
    let base = free_udp_port().min(65535 - 24);
    let window = (base, base + 24);
    let config = IceConfig {
        port_range: Some(window),
        ..loopback_config(true, false)
    };
    let server = Harness::new(config).await;
    let ports: Vec<u16> = server.host_candidates().iter().map(|c| c.port).collect();
    assert!(
        ports.len() == 1 && (window.0..=window.1).contains(&ports[0]),
        "binds inside the window {window:?}: {ports:?}"
    );
}

#[tokio::test]
async fn held_ports_are_retried_past_in_every_order() {
    let _no_fork = FORK.read().await;
    let (window, held, free) = occupied_window(3);
    let ordered: Vec<u16> = (window.0..=window.1).collect();
    let socket = bind_udp(LOOPBACK, &ordered).unwrap();
    assert_eq!(
        socket.local_addr().unwrap().port(),
        free,
        "the held ports of a window are retried past, in order"
    );
    drop(socket);
    let mut landed = Vec::new();
    for _ in 0..6 {
        let config = IceConfig {
            port_range: Some(window),
            ..loopback_config(true, false)
        };
        let server = Harness::new(config).await;
        landed.extend(server.host_candidates().iter().map(|c| c.port));
        drop(server);
        // The socket closes as its reader task ends, which takes a turn of the runtime.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        landed,
        vec![free; 6],
        "every draw of the window's order finds the free port"
    );
    drop(held);
}

#[tokio::test]
async fn an_exhausted_window_skips_the_address() {
    let blocker = std::net::UdpSocket::bind((LOOPBACK, 0)).unwrap();
    let port = blocker.local_addr().unwrap().port();
    let config = IceConfig {
        port_range: Some((port, port)),
        ..loopback_config(true, false)
    };
    let server = Harness::new(config).await;
    assert!(
        server.host_candidates().is_empty(),
        "exhausted window skips the address without raising"
    );
    let unreachable: IpAddr = "203.0.113.1".parse().unwrap();
    let free = free_udp_port();
    assert_eq!(
        bind_udp(unreachable, &[free]).unwrap_err().kind(),
        std::io::ErrorKind::AddrNotAvailable,
        "an address that is gone is the kernel's reason, not a port to move past"
    );
}

// --- the topology ------------------------------------------------------------------------

#[tokio::test]
async fn the_topology_binds_every_host_address_once_and_hands_it_to_every_peer() {
    let _no_fork = FORK.read().await;
    let addresses = host_addresses(true, true);
    assert!(
        !addresses.is_empty(),
        "the sandbox has a non-loopback interface"
    );
    let defaults = open_muxes(&IceTopology::default()).await.unwrap();
    assert!(
        defaults.udp.is_none() && defaults.tcp.is_none(),
        "defaults: no mux"
    );
    let topology = IceTopology {
        lite: true,
        servers: vec![IceServer::new("stun:stun.example.test:3478")],
        ..IceTopology::default()
    };
    let config = topology.peer_config(&IceMuxes::default());
    assert!(
        config.lite && config.servers.is_empty(),
        "ice lite leaves STUN and TURN out of the server's configuration"
    );

    let udp = free_udp_port();
    let tcp = free_tcp_port();
    let topology = IceTopology {
        udp_mux_port: udp,
        tcp_mux_port: tcp,
        lite: true,
        port_range: Some((50000, 50100)),
        ..IceTopology::default()
    };
    let mut muxes = open_muxes(&topology).await.unwrap();
    let mut udp_listening = muxes.udp.as_ref().unwrap().listen_addresses();
    let mut tcp_listening = muxes.tcp.as_ref().unwrap().listen_addresses();
    udp_listening.sort();
    tcp_listening.sort();
    let mut expected_udp: Vec<SocketAddr> =
        addresses.iter().map(|a| SocketAddr::new(*a, udp)).collect();
    let mut expected_tcp: Vec<SocketAddr> =
        addresses.iter().map(|a| SocketAddr::new(*a, tcp)).collect();
    expected_udp.sort();
    expected_tcp.sort();
    assert_eq!(
        udp_listening, expected_udp,
        "the UDP mux binds every host address on its port"
    );
    assert_eq!(
        tcp_listening, expected_tcp,
        "the TCP mux binds every host address on its port"
    );
    let config = topology.peer_config(&muxes);
    assert!(
        config.udp_mux.is_some() && config.tcp_mux.is_some() && config.lite,
        "every new peer's configuration carries the muxes"
    );

    let (tx, mut rx) = inbound_channel();
    let mut peer = IceConnection::new(config, tx, LiteTimings::default()).unwrap();
    let candidates = peer.gather(&mut rx).await.unwrap();
    let udp_hosts: Vec<_> = candidates
        .iter()
        .filter(|c| c.transport == Protocol::Udp && c.kind == CandidateType::Host)
        .collect();
    let tcp_hosts: Vec<_> = candidates
        .iter()
        .filter(|c| c.transport == Protocol::Tcp)
        .collect();
    assert!(
        !udp_hosts.is_empty() && udp_hosts.iter().all(|c| c.port == udp),
        "the UDP host candidates all sit on the UDP mux port"
    );
    assert!(
        tcp_hosts.len() == addresses.len()
            && tcp_hosts
                .iter()
                .all(|c| c.port == tcp && c.tcptype == Some(TcpType::Passive)),
        "one passive TCP candidate per address on the TCP mux port"
    );
    assert!(
        peer.local_parameters().ice_lite,
        "the lite peer says ice-lite"
    );
    assert!(
        peer.local_parameters().username_fragment.len() >= 8,
        "the username fragment is long enough to key the shared sockets"
    );
    peer.close();
    assert!(
        muxes.udp.as_ref().unwrap().attached(None) == 0
            && muxes.tcp.as_ref().unwrap().attached(None) == 0,
        "closing the peer detached it from the muxes"
    );

    muxes.close().await;
    let again = UdpMux::new(udp);
    again
        .open(&addresses)
        .await
        .expect("a later bind of the UDP port succeeds");
    again.close().await;

    let blocker = std::net::UdpSocket::bind((addresses[0], 0)).unwrap();
    let taken = IceTopology {
        udp_mux_port: blocker.local_addr().unwrap().port(),
        ..IceTopology::default()
    };
    assert!(
        open_muxes(&taken).await.is_err(),
        "a UDP mux port in use fails startup"
    );
    drop(blocker);
    let blocker = std::net::TcpListener::bind((addresses[0], 0)).unwrap();
    let taken = IceTopology {
        udp_mux_port: udp,
        tcp_mux_port: blocker.local_addr().unwrap().port(),
        ..IceTopology::default()
    };
    assert!(
        open_muxes(&taken).await.is_err(),
        "a TCP mux port in use fails startup"
    );
    let released = UdpMux::new(udp);
    released
        .open(&addresses)
        .await
        .expect("and releases the UDP mux");
    released.close().await;
}

// --- a real TURN server ---------------------------------------------------------------------

/// A coturn of this test's own on loopback, ended with the test.
struct Coturn {
    child: tokio::process::Child,
    port: u16,
    tls_port: u16,
    certificate: rcgen::CertifiedKey<rcgen::KeyPair>,
    dir: std::path::PathBuf,
}

impl Coturn {
    async fn start() -> Coturn {
        let dir = std::env::temp_dir().join(format!("selkies-coturn-{}", stun::random_string(8)));
        std::fs::create_dir_all(&dir).unwrap();
        let certificate =
            rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        std::fs::write(dir.join("cert.pem"), certificate.cert.pem()).unwrap();
        std::fs::write(dir.join("key.pem"), certificate.signing_key.serialize_pem()).unwrap();
        let (port, tls_port) = (free_tcp_port(), free_tcp_port());
        let fork = FORK.write().await;
        let child = tokio::process::Command::new("turnserver")
            .args([
                "-n",
                "--no-cli",
                "--no-dtls",
                "--simple-log",
                "--log-file=stdout",
            ])
            .args([
                "--listening-ip=127.0.0.1",
                "--relay-ip=127.0.0.1",
                "--allow-loopback-peers",
            ])
            .arg(format!("--listening-port={port}"))
            .arg(format!("--tls-listening-port={tls_port}"))
            .args(["--min-port=49160", "--max-port=49300"])
            .args([
                "--lt-cred-mech",
                "--user=selkies:turnpass",
                "--realm=selkies.test",
            ])
            .arg(format!("--cert={}", dir.join("cert.pem").display()))
            .arg(format!("--pkey={}", dir.join("key.pem").display()))
            .arg(format!("--pidfile={}", dir.join("turn.pid").display()))
            .arg(format!("--userdb={}", dir.join("turndb").display()))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("turnserver on PATH");
        drop(fork);
        for _ in 0..100 {
            if TcpStream::connect((LOOPBACK, port)).await.is_ok()
                && TcpStream::connect((LOOPBACK, tls_port)).await.is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Coturn {
            child,
            port,
            tls_port,
            certificate,
            dir,
        }
    }

    /// End the server and reap it, which `kill_on_drop` alone leaves to a runtime that is gone.
    async fn stop(mut self) {
        let _ = self.child.kill().await;
    }

    fn server(&self, transport: Protocol, tls: bool) -> super::config::TurnServer {
        super::config::TurnServer {
            host: "localhost".into(),
            port: if tls { self.tls_port } else { self.port },
            tls,
            transport,
            username: Some("selkies".into()),
            password: Some("turnpass".into()),
        }
    }
}

impl Drop for Coturn {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A datagram leaves through the allocation to a plain peer, whose answer comes back through it.
async fn relay_round_trip(
    server: super::config::TurnServer,
    roots: Option<rustls::RootCertStore>,
    label: &str,
) {
    let (tx, mut rx) = inbound_channel();
    let relay = super::turn::allocate_trusting(server, tx, roots)
        .await
        .unwrap_or_else(|e| panic!("{label}: {e}"));
    let peer = UdpSocket::bind((LOOPBACK, 0)).await.unwrap();
    let peer_addr = peer.local_addr().unwrap();
    relay.send(peer_addr, b"out-through-the-relay").unwrap();
    let mut buf = [0u8; 1500];
    let (n, from) = tokio::time::timeout(Duration::from_secs(5), peer.recv_from(&mut buf))
        .await
        .expect(label)
        .unwrap();
    assert_eq!(&buf[..n], b"out-through-the-relay", "{label}");
    assert_eq!(
        from, relay.relayed_addr,
        "{label}: the peer sees the relayed address"
    );
    peer.send_to(b"back-through-the-relay", from).await.unwrap();
    let inbound = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect(label);
    let Some(Inbound::Packet(packet)) = inbound else {
        panic!("{label}: no packet")
    };
    assert_eq!(&packet.data[..], b"back-through-the-relay", "{label}");
    assert_eq!(
        (packet.local, packet.peer),
        (relay.relayed_addr, peer_addr),
        "{label}"
    );
    relay.close();
}

#[tokio::test]
#[ignore = "needs coturn's turnserver on PATH"]
async fn turn_relays_over_udp_tcp_and_tls() {
    let coturn = Coturn::start().await;
    relay_round_trip(coturn.server(Protocol::Udp, false), None, "udp").await;
    relay_round_trip(coturn.server(Protocol::Tcp, false), None, "tcp").await;
    let mut roots = rustls::RootCertStore::empty();
    roots.add(coturn.certificate.cert.der().clone()).unwrap();
    relay_round_trip(coturn.server(Protocol::Tcp, true), Some(roots), "tls").await;

    let mut wrong = rustls::RootCertStore::empty();
    let other = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    wrong.add(other.cert.der().clone()).unwrap();
    let (tx, _rx) = inbound_channel();
    assert!(
        super::turn::allocate_trusting(coturn.server(Protocol::Tcp, true), tx, Some(wrong))
            .await
            .is_err(),
        "a server certificate outside the roots is refused"
    );
    let mut denied = coturn.server(Protocol::Udp, false);
    denied.password = Some("wrong".into());
    let (tx, _rx) = inbound_channel();
    assert!(
        super::turn::allocate(denied, tx).await.is_err(),
        "wrong credentials are refused"
    );
    coturn.stop().await;
}

/// ICE over nothing but the relay: the server's one candidate is `relay`, a full client reaches
/// it, and data flows on the pair it nominates.
#[tokio::test]
#[ignore = "needs coturn's turnserver on PATH"]
async fn ice_completes_over_a_relay_candidate() {
    let coturn = Coturn::start().await;
    for (url, label) in [
        (
            format!("turn:127.0.0.1:{}?transport=udp", coturn.port),
            "udp",
        ),
        (
            format!("turn:127.0.0.1:{}?transport=tcp", coturn.port),
            "tcp",
        ),
    ] {
        let config = IceConfig {
            servers: vec![IceServer {
                urls: vec![url],
                username: Some("selkies".into()),
                credential: Some("turnpass".into()),
            }],
            addresses: Some(Vec::new()),
            ..loopback_config(true, false)
        };
        let mut server = Harness::new(config).await;
        let relay = server.conn.local_candidates().to_vec();
        assert!(
            relay.len() == 1 && relay[0].kind == CandidateType::Relay,
            "{label}: {relay:?}"
        );
        assert_eq!(relay[0].transport, Protocol::Udp, "{label}");
        let mut client = Harness::new(loopback_config(false, false)).await;
        pair_up(&mut server, &mut client);
        server.start();
        client.start();
        assert!(
            both_complete(&mut server, &mut client, Duration::from_secs(15)).await,
            "{label}: both complete"
        );
        assert_eq!(
            server.conn.selected_pair().unwrap().0.kind,
            CandidateType::Relay,
            "{label}"
        );
        server.conn.send(b"via-relay").unwrap();
        client.conn.send(b"via-relay-back").unwrap();
        assert_eq!(
            client.recv(Duration::from_secs(5)).await.as_deref(),
            Some(&b"via-relay"[..]),
            "{label}"
        );
        assert_eq!(
            server.recv(Duration::from_secs(5)).await.as_deref(),
            Some(&b"via-relay-back"[..]),
            "{label}"
        );
        server.conn.close();
        client.conn.close();
    }
    coturn.stop().await;
}
