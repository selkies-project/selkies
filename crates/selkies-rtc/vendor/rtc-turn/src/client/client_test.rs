use super::*;
use sansio::Protocol;
use std::collections::HashSet;
use std::net::UdpSocket;

/// Tests may resolve the built-in provider; library code never does.
fn test_crypto_provider() -> std::sync::Arc<dyn crypto::RTCCryptoProvider> {
    crypto::default_provider().expect("a built-in crypto provider must be enabled for tests")
}

fn create_listening_test_client(rto_in_ms: u64) -> Result<(UdpSocket, Client)> {
    let udp_socket = UdpSocket::bind("0.0.0.0:0")?;

    let client = Client::new(
        ClientConfig {
            stun_serv_addr: String::new(),
            turn_serv_addr: String::new(),
            local_addr: udp_socket.local_addr()?,
            transport_protocol: TransportProtocol::UDP,
            username: String::new(),
            password: String::new(),
            realm: String::new(),
            software: "TEST SOFTWARE".to_owned(),
            rto_in_ms,
            allocation_refresh_interval_cap: None,
        },
        test_crypto_provider(),
    )?;

    Ok((udp_socket, client))
}

fn create_listening_test_client_with_stun_serv(
    stun_serv_addr: SocketAddr,
) -> Result<(UdpSocket, Client)> {
    let udp_socket = UdpSocket::bind("0.0.0.0:0")?;

    let client = Client::new(
        ClientConfig {
            stun_serv_addr: stun_serv_addr.to_string(),
            turn_serv_addr: String::new(),
            local_addr: udp_socket.local_addr()?,
            transport_protocol: TransportProtocol::UDP,
            username: String::new(),
            password: String::new(),
            realm: String::new(),
            software: "TEST SOFTWARE".to_owned(),
            rto_in_ms: 0,
            allocation_refresh_interval_cap: None,
        },
        test_crypto_provider(),
    )?;

    Ok((udp_socket, client))
}

/// A STUN server on the loopback interface, so the binding tests reach no server on the Internet.
fn local_stun_server() -> Result<UdpSocket> {
    let server = UdpSocket::bind("127.0.0.1:0")?;
    server.set_read_timeout(Some(Duration::from_secs(5)))?;
    Ok(server)
}

/// Answers `count` Binding requests on `server` as a STUN server does: a success response with the
/// request's transaction id and the sender's address as XOR-MAPPED-ADDRESS.
fn answer_binding_requests(server: &UdpSocket, count: usize) -> Result<()> {
    let mut buffer = vec![0u8; 2048];
    for _ in 0..count {
        let (n, from) = server.recv_from(&mut buffer)?;
        let mut request = Message::new();
        request.unmarshal_binary(&buffer[..n])?;
        let mut response = Message::new();
        response.build(&[
            Box::new(request.transaction_id),
            Box::new(BINDING_SUCCESS),
            Box::new(XorMappedAddress {
                ip: from.ip(),
                port: from.port(),
            }),
        ])?;
        server.send_to(&response.raw, from)?;
    }
    Ok(())
}

/// A host demultiplexing STUN on a socket it shares — a STUN gatherer and this client pointed at
/// one server address — routes a response by transaction id, because nothing else identifies its
/// owner. See webrtc-rs/webrtc#890.
#[test]
fn test_has_transaction_reports_outstanding_requests() -> Result<()> {
    let (_conn, mut client) = create_listening_test_client(0)?;
    let server = "127.0.0.1:3478".parse::<std::net::SocketAddr>().unwrap();

    let unknown = TransactionId::new();
    assert!(
        !client.has_transaction(&unknown),
        "a fresh client is waiting on nothing"
    );

    let tid = client.send_binding_request_to(Instant::now(), server)?;
    assert!(
        client.has_transaction(&tid),
        "the transaction just started is outstanding"
    );
    assert!(
        !client.has_transaction(&unknown),
        "an id this client never sent is not its own"
    );

    Ok(())
}

#[test]
fn test_client_with_stun_send_binding_request() -> Result<()> {
    //env_logger::init();

    let server = local_stun_server()?;
    let (conn, mut client) = create_listening_test_client_with_stun_serv(server.local_addr()?)?;
    conn.set_read_timeout(Some(Duration::from_secs(5)))?;
    let local_addr = conn.local_addr()?;

    let tid = client.send_binding_request(Instant::now())?;

    while let Some(transmit) = client.poll_write() {
        conn.send_to(&transmit.message, transmit.transport.peer_addr)?;
    }
    answer_binding_requests(&server, 1)?;

    let mut buffer = vec![0u8; 2048];
    let (n, peer_addr) = conn.recv_from(&mut buffer)?;
    client.handle_read(TransportMessage {
        now: Instant::now(),
        transport: TransportContext {
            local_addr,
            peer_addr,
            transport_protocol: TransportProtocol::UDP,
            ecn: None,
        },
        message: BytesMut::from(&buffer[..n]),
    })?;

    if let Some(event) = client.poll_event() {
        match event {
            Event::BindingResponse(id, refl_addr) => {
                assert_eq!(tid, id);
                debug!("mapped-addr: {}", refl_addr);
            }
            _ => assert!(false),
        }
    } else {
        assert!(false);
    }

    assert_eq!(0, client.tr_map.size(), "should be no transaction left");

    client.close()
}

#[test]
fn test_client_with_stun_send_binding_request_to_parallel() -> Result<()> {
    //env_logger::init();

    let server = local_stun_server()?;
    let (conn, mut client) = create_listening_test_client(0)?;
    conn.set_read_timeout(Some(Duration::from_secs(5)))?;
    let local_addr = conn.local_addr()?;

    let to = server.local_addr()?;

    let tid1 = client.send_binding_request_to(Instant::now(), to)?;
    let tid2 = client.send_binding_request_to(Instant::now(), to)?;
    while let Some(transmit) = client.poll_write() {
        conn.send_to(&transmit.message, transmit.transport.peer_addr)?;
    }
    answer_binding_requests(&server, 2)?;

    let mut buffer = vec![0u8; 2048];
    for _ in 0..2 {
        let (n, peer_addr) = conn.recv_from(&mut buffer)?;
        client.handle_read(TransportMessage {
            now: Instant::now(),
            transport: TransportContext {
                local_addr,
                peer_addr,
                transport_protocol: TransportProtocol::UDP,
                ecn: None,
            },
            message: BytesMut::from(&buffer[..n]),
        })?;
    }

    let mut tids = HashSet::new();
    while let Some(event) = client.poll_event() {
        match event {
            Event::BindingResponse(tid, refl_addr) => {
                tids.insert(tid);
                debug!("mapped-addr: {}", refl_addr);
            }
            _ => {}
        }
    }

    assert_eq!(2, tids.len());
    assert!(tids.contains(&tid1));
    assert!(tids.contains(&tid2));

    client.close()
}

#[test]
fn test_client_with_stun_send_binding_request_to_timeout() -> Result<()> {
    //env_logger::init();

    let (conn, mut client) = create_listening_test_client(10)?;

    let to = lookup_host(true, "127.0.0.1:9")?;

    let tid = client.send_binding_request_to(Instant::now(), to)?;
    while let Some(transmit) = client.poll_write() {
        conn.send_to(&transmit.message, transmit.transport.peer_addr)?;
    }

    while let Some(to) = client.poll_timeout() {
        client.handle_timeout(to)?;
    }

    if let Some(event) = client.poll_event() {
        match event {
            Event::TransactionTimeout(id) => {
                assert_eq!(tid, id);
            }
            _ => assert!(false),
        }
    } else {
        assert!(false);
    }

    client.close()
}

/// The allocation-refresh timer is seeded from the instant passed to `RelayState::new` and
/// compared against the instant passed to `handle_timeout`, so a refresh can be observed by
/// arithmetic on a base instant with no wall-clock time passing and no sleeping.
#[test]
fn test_relay_refresh_timers_run_on_injected_time() -> Result<()> {
    let base = Instant::now();
    let t = |secs| base + Duration::from_secs(secs);

    let udp_socket = UdpSocket::bind("0.0.0.0:0")?;
    let mut client = Client::new(
        ClientConfig {
            stun_serv_addr: String::new(),
            turn_serv_addr: "127.0.0.1:3478".to_owned(),
            local_addr: udp_socket.local_addr()?,
            transport_protocol: TransportProtocol::UDP,
            username: "user".to_owned(),
            password: "pass".to_owned(),
            realm: "realm".to_owned(),
            software: "TEST SOFTWARE".to_owned(),
            rto_in_ms: 0,
            allocation_refresh_interval_cap: None,
        },
        test_crypto_provider(),
    )?;

    // Stand in for a granted allocation, seeded at t(0) with a 10-minute lifetime.
    let lifetime = Duration::from_secs(600);
    let relayed_addr: RelayedAddr = "127.0.0.1:50000".parse().unwrap();
    client.relays.insert(
        relayed_addr,
        RelayState::new(
            t(0),
            relayed_addr,
            vec![0u8; 16],
            Nonce::new(ATTR_NONCE, "nonce".to_owned()),
            lifetime,
            None,
        ),
    );

    // The allocation is refreshed at half its lifetime; permissions sooner, at 120s.
    assert_eq!(client.relay(relayed_addr)?.poll_timeout(), Some(t(120)));

    // One second before the allocation deadline nothing is sent for it. (The permission
    // refresh at t(120) has already fired by then, so drain whatever it queued first.)
    client.relay(relayed_addr)?.handle_timeout(t(120));
    while client.poll_write().is_some() {}
    client.relay(relayed_addr)?.handle_timeout(t(299));
    assert!(
        client.poll_write().is_none(),
        "no refresh is due one second before the deadline"
    );

    // At the deadline the Refresh request goes out, stamped with the caller's instant.
    client.relay(relayed_addr)?.handle_timeout(t(300));
    let transmit = client
        .poll_write()
        .expect("the allocation must be refreshed at half its lifetime");
    assert_eq!(
        transmit.now,
        t(300),
        "the request carries the instant the caller supplied, not an ambient reading"
    );

    let mut msg = Message::new();
    msg.raw = transmit.message.to_vec();
    msg.decode()?;
    assert_eq!(msg.typ.method, METHOD_REFRESH);

    // The late permission refresh at t(299) is rescheduled from the time it was handled, so its
    // next deadline is t(419). The allocation remains due at t(600).
    assert_eq!(client.relay(relayed_addr)?.poll_timeout(), Some(t(419)));

    client.relay(relayed_addr)?.handle_timeout(t(600));
    let transmit = client
        .poll_write()
        .expect("the allocation must be refreshed again a half-lifetime later");
    assert_eq!(transmit.now, t(600));

    client.close()
}

/// Applications may refresh an otherwise idle allocation more frequently than half its server
/// lifetime so the client-to-server NAT mapping does not sit idle for several minutes.
#[test]
fn test_allocation_refresh_interval_cap_is_applied() -> Result<()> {
    let base = Instant::now();
    let t = |secs| base + Duration::from_secs(secs);

    let udp_socket = UdpSocket::bind("0.0.0.0:0")?;
    let mut client = Client::new(
        ClientConfig {
            stun_serv_addr: String::new(),
            turn_serv_addr: "127.0.0.1:3478".to_owned(),
            local_addr: udp_socket.local_addr()?,
            transport_protocol: TransportProtocol::UDP,
            username: "user".to_owned(),
            password: "pass".to_owned(),
            realm: "realm".to_owned(),
            software: "TEST SOFTWARE".to_owned(),
            rto_in_ms: 0,
            allocation_refresh_interval_cap: Some(Duration::from_secs(30)),
        },
        test_crypto_provider(),
    )?;

    let relayed_addr: RelayedAddr = "127.0.0.1:50000".parse().unwrap();
    let mut response = Message::new();
    response.build(&[
        Box::new(TransactionId::new()),
        Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_SUCCESS_RESPONSE)),
        Box::new(RelayedAddress {
            ip: relayed_addr.ip(),
            port: relayed_addr.port(),
        }),
        Box::new(crate::proto::lifetime::Lifetime(Duration::from_secs(600))),
    ])?;
    client.handle_allocate_response(
        base,
        response,
        TransactionType::AllocateRequest(Nonce::new(ATTR_NONCE, "nonce".to_owned())),
    )?;

    for expected_secs in [30, 60, 90, 120, 150, 180] {
        assert_eq!(
            client.relay(relayed_addr)?.poll_timeout(),
            Some(t(expected_secs))
        );
        client.relay(relayed_addr)?.handle_timeout(t(expected_secs));

        let transmit = client
            .poll_write()
            .expect("the configured cadence must emit an allocation Refresh");
        let mut message = Message::new();
        message.raw = transmit.message.to_vec();
        message.decode()?;
        assert_eq!(message.typ.method, METHOD_REFRESH);
    }

    assert_eq!(client.relay(relayed_addr)?.poll_timeout(), Some(t(210)));

    client.close()
}

/// Handling an overdue relay timer once must put every refreshed deadline back in the future.
/// A runtime may be suspended for longer than several refresh intervals on a mobile platform;
/// replaying each missed interval makes a Sans-I/O driver spin through historical deadlines.
#[test]
fn test_overdue_relay_refreshes_are_rescheduled_from_now() -> Result<()> {
    let base = Instant::now();
    let t = |secs| base + Duration::from_secs(secs);

    let udp_socket = UdpSocket::bind("0.0.0.0:0")?;
    let mut client = Client::new(
        ClientConfig {
            stun_serv_addr: String::new(),
            turn_serv_addr: "127.0.0.1:3478".to_owned(),
            local_addr: udp_socket.local_addr()?,
            transport_protocol: TransportProtocol::UDP,
            username: "user".to_owned(),
            password: "pass".to_owned(),
            realm: "realm".to_owned(),
            software: "TEST SOFTWARE".to_owned(),
            rto_in_ms: 0,
            allocation_refresh_interval_cap: None,
        },
        test_crypto_provider(),
    )?;

    let relayed_addr: RelayedAddr = "127.0.0.1:50000".parse().unwrap();
    client.relays.insert(
        relayed_addr,
        RelayState::new(
            t(0),
            relayed_addr,
            vec![0u8; 16],
            Nonce::new(ATTR_NONCE, "nonce".to_owned()),
            Duration::from_secs(600),
            None,
        ),
    );

    let resumed_at = t(1000);
    client.relay(relayed_addr)?.handle_timeout(resumed_at);

    assert_eq!(
        client.relay(relayed_addr)?.poll_timeout(),
        Some(t(1120)),
        "one timeout pass must move the permission and allocation deadlines past now"
    );

    client.close()
}

/// A relay whose lifetime has gone to zero must not hand the caller a deadline that never
/// advances.
///
/// This is the root cause behind [webrtc#862](https://github.com/webrtc-rs/webrtc/issues/862).
/// `Relay::handle_timeout` advances the allocation timer by adding to the timer's **own**
/// previous value rather than to `now`:
///
/// ```ignore
/// relay.refresh_alloc_timer = relay.refresh_alloc_timer.add(relay.lifetime / 2);
/// ```
///
/// so the step size is `lifetime / 2`. `lifetime` is assigned straight from the server's
/// LIFETIME attribute on every refresh response, with no floor — and `Relay::close` refreshes
/// with `LIFETIME=0`, which is how RFC 5766 deallocates. Once the server echoes that back the
/// step becomes zero, the timer is frozen, and because nothing ever removes the entry from
/// `client.relays` the relay keeps reporting it forever.
///
/// The peer-connection driver treats an expired deadline as "handle it and loop again", so a
/// frozen expired deadline is an unbounded hot loop — starving the very `Close` that triggered
/// it. That matches the report: cleanup never completes and the process will not exit.
#[test]
fn test_zero_lifetime_relay_does_not_freeze_its_deadline() -> Result<()> {
    let base = Instant::now();
    let t = |secs| base + Duration::from_secs(secs);

    let udp_socket = UdpSocket::bind("0.0.0.0:0")?;
    let mut client = Client::new(
        ClientConfig {
            stun_serv_addr: String::new(),
            turn_serv_addr: "127.0.0.1:3478".to_owned(),
            local_addr: udp_socket.local_addr()?,
            transport_protocol: TransportProtocol::UDP,
            username: "user".to_owned(),
            password: "pass".to_owned(),
            realm: "realm".to_owned(),
            software: "TEST SOFTWARE".to_owned(),
            rto_in_ms: 0,
            allocation_refresh_interval_cap: None,
        },
        test_crypto_provider(),
    )?;

    let relayed_addr: RelayedAddr = "127.0.0.1:50000".parse().unwrap();
    client.relays.insert(
        relayed_addr,
        RelayState::new(
            t(0),
            relayed_addr,
            vec![0u8; 16],
            Nonce::new(ATTR_NONCE, "nonce".to_owned()),
            Duration::from_secs(600),
            None,
        ),
    );

    // What the server's response to `close()`'s LIFETIME=0 refresh does.
    client.relays.get_mut(&relayed_addr).unwrap().lifetime = Duration::from_secs(0);

    // Drive past both timers, exactly as the driver does: read the deadline, handle it,
    // read again. The deadline must move, or the caller is told to act on the same instant
    // forever.
    let mut previous = client.relay(relayed_addr)?.poll_timeout();
    for iteration in 0..10 {
        let now = t(1000 + iteration);
        client.relay(relayed_addr)?.handle_timeout(now);
        let next = client.relay(relayed_addr)?.poll_timeout();

        assert!(
            next != previous || next.is_none(),
            "iteration {iteration}: deadline did not advance ({previous:?} -> {next:?}) while \
             `now` is {:?} past it. The driver reads this as a zero delay, calls handle_timeout \
             and loops — forever.",
            now.saturating_duration_since(previous.unwrap_or(now)),
        );
        previous = next;
    }

    Ok(())
}

#[test]
fn test_refresh_response_reschedules_shorter_lifetime_from_response_time() -> Result<()> {
    let base = Instant::now();
    let t = |secs| base + Duration::from_secs(secs);
    let udp_socket = UdpSocket::bind("0.0.0.0:0")?;
    let mut client = Client::new(
        ClientConfig {
            stun_serv_addr: String::new(),
            turn_serv_addr: "127.0.0.1:3478".to_owned(),
            local_addr: udp_socket.local_addr()?,
            transport_protocol: TransportProtocol::UDP,
            username: "user".to_owned(),
            password: "pass".to_owned(),
            realm: "realm".to_owned(),
            software: "TEST SOFTWARE".to_owned(),
            rto_in_ms: 0,
            allocation_refresh_interval_cap: None,
        },
        test_crypto_provider(),
    )?;
    let relayed_addr: RelayedAddr = "127.0.0.1:50000".parse().unwrap();
    client.relays.insert(
        relayed_addr,
        RelayState::new(
            t(0),
            relayed_addr,
            vec![0u8; 16],
            Nonce::new(ATTR_NONCE, "nonce".to_owned()),
            Duration::from_secs(600),
            None,
        ),
    );
    assert_eq!(client.relay(relayed_addr)?.poll_timeout(), Some(t(120)));

    let mut response = Message::new();
    response.build(&[
        Box::new(TransactionId::new()),
        Box::new(MessageType::new(METHOD_REFRESH, CLASS_SUCCESS_RESPONSE)),
        Box::new(crate::proto::lifetime::Lifetime(Duration::from_secs(60))),
    ])?;
    client
        .relay(relayed_addr)?
        .handle_refresh_allocation_response(t(30), response)?;

    assert_eq!(
        client.relay(relayed_addr)?.poll_timeout(),
        Some(t(60)),
        "a shorter server lifetime must move the allocation refresh deadline earlier"
    );
    Ok(())
}

#[test]
fn test_refresh_response_reschedule_applies_interval_cap() -> Result<()> {
    let base = Instant::now();
    let t = |secs| base + Duration::from_secs(secs);
    let udp_socket = UdpSocket::bind("0.0.0.0:0")?;
    let mut client = Client::new(
        ClientConfig {
            stun_serv_addr: String::new(),
            turn_serv_addr: "127.0.0.1:3478".to_owned(),
            local_addr: udp_socket.local_addr()?,
            transport_protocol: TransportProtocol::UDP,
            username: "user".to_owned(),
            password: "pass".to_owned(),
            realm: "realm".to_owned(),
            software: "TEST SOFTWARE".to_owned(),
            rto_in_ms: 0,
            allocation_refresh_interval_cap: Some(Duration::from_secs(20)),
        },
        test_crypto_provider(),
    )?;
    let relayed_addr: RelayedAddr = "127.0.0.1:50000".parse().unwrap();
    client.relays.insert(
        relayed_addr,
        RelayState::new(
            t(0),
            relayed_addr,
            vec![0u8; 16],
            Nonce::new(ATTR_NONCE, "nonce".to_owned()),
            Duration::from_secs(600),
            Some(Duration::from_secs(20)),
        ),
    );

    let mut response = Message::new();
    response.build(&[
        Box::new(TransactionId::new()),
        Box::new(MessageType::new(METHOD_REFRESH, CLASS_SUCCESS_RESPONSE)),
        Box::new(crate::proto::lifetime::Lifetime(Duration::from_secs(60))),
    ])?;
    client
        .relay(relayed_addr)?
        .handle_refresh_allocation_response(t(30), response)?;

    assert_eq!(client.relay(relayed_addr)?.poll_timeout(), Some(t(50)));
    Ok(())
}

/// A server confirming deallocation must leave no relay behind.
///
/// `close()` refreshes the allocation with `LIFETIME=0`; the server echoes that back to confirm
/// the allocation is gone. Before the fix for
/// [webrtc#862](https://github.com/webrtc-rs/webrtc/issues/862) the response handler stored the
/// zero and nothing else — the entry stayed in `client.relays` forever, still reporting refresh
/// deadlines for an allocation that no longer existed, and still trying to refresh it.
///
/// Flooring the refresh step stops the *hot loop*; only removing the relay stops the dead
/// allocation. Both are needed, and this covers the second.
#[test]
fn test_zero_lifetime_response_drops_the_relay() -> Result<()> {
    let base = Instant::now();
    let t = |secs| base + Duration::from_secs(secs);

    let udp_socket = UdpSocket::bind("0.0.0.0:0")?;
    let mut client = Client::new(
        ClientConfig {
            stun_serv_addr: String::new(),
            turn_serv_addr: "127.0.0.1:3478".to_owned(),
            local_addr: udp_socket.local_addr()?,
            transport_protocol: TransportProtocol::UDP,
            username: "user".to_owned(),
            password: "pass".to_owned(),
            realm: "realm".to_owned(),
            software: "TEST SOFTWARE".to_owned(),
            rto_in_ms: 0,
            allocation_refresh_interval_cap: None,
        },
        test_crypto_provider(),
    )?;

    let relayed_addr: RelayedAddr = "127.0.0.1:50000".parse().unwrap();
    client.relays.insert(
        relayed_addr,
        RelayState::new(
            t(0),
            relayed_addr,
            vec![0u8; 16],
            Nonce::new(ATTR_NONCE, "nonce".to_owned()),
            Duration::from_secs(600),
            None,
        ),
    );
    assert!(client.relay(relayed_addr)?.poll_timeout().is_some());

    // What the TURN server sends back when it honours `close()`'s LIFETIME=0 refresh.
    let mut res = Message::new();
    res.build(&[
        Box::new(TransactionId::new()),
        Box::new(MessageType::new(METHOD_REFRESH, CLASS_SUCCESS_RESPONSE)),
        Box::new(crate::proto::lifetime::Lifetime(Duration::from_secs(0))),
    ])?;

    client
        .relay(relayed_addr)?
        .handle_refresh_allocation_response(t(1), res)?;

    assert!(
        !client.relays.contains_key(&relayed_addr),
        "a deallocated allocation must not stay in the relay map — it would keep reporting \
         refresh deadlines and keep trying to refresh something that no longer exists"
    );

    Ok(())
}

/// An Allocate success response carrying `LIFETIME=0` is rejected, not accepted as a
/// degenerate allocation.
///
/// RFC 5766 §6.2 has the server take `min(client proposed, server maximum)` and fall back to
/// the *default* lifetime (600 s) whenever that does not exceed it, so a successful Allocate
/// never returns a lifetime below the default — zero is a protocol violation. Zero is
/// meaningful only on the Refresh path (§7), where it means the allocation was deleted.
///
/// Accepting it would insert a `RelayState` whose `refresh_alloc_timer` is `now.add(0)` —
/// already expired at construction — for an allocation that does not exist. That relay then
/// reports an expired refresh deadline forever, which is the hot loop in
/// [webrtc#862](https://github.com/webrtc-rs/webrtc/issues/862), reached here without any
/// close being involved.
#[test]
fn test_zero_lifetime_allocate_response_is_an_error() -> Result<()> {
    let base = Instant::now();
    let udp_socket = UdpSocket::bind("0.0.0.0:0")?;
    let mut client = Client::new(
        ClientConfig {
            stun_serv_addr: String::new(),
            turn_serv_addr: "127.0.0.1:3478".to_owned(),
            local_addr: udp_socket.local_addr()?,
            transport_protocol: TransportProtocol::UDP,
            username: "user".to_owned(),
            password: "pass".to_owned(),
            realm: "realm".to_owned(),
            software: "TEST SOFTWARE".to_owned(),
            rto_in_ms: 0,
            allocation_refresh_interval_cap: None,
        },
        test_crypto_provider(),
    )?;

    let mut res = Message::new();
    res.build(&[
        Box::new(TransactionId::new()),
        Box::new(MessageType::new(METHOD_ALLOCATE, CLASS_SUCCESS_RESPONSE)),
        Box::new(RelayedAddress {
            ip: "127.0.0.1".parse().unwrap(),
            port: 50000,
        }),
        Box::new(crate::proto::lifetime::Lifetime(Duration::from_secs(0))),
    ])?;

    client.handle_allocate_response(
        base,
        res,
        TransactionType::AllocateRequest(Nonce::new(ATTR_NONCE, "nonce".to_owned())),
    )?;

    assert!(
        client.relays.is_empty(),
        "a zero-lifetime Allocate response must not create a relay — the allocation does not \
         exist, and the relay would report an already-expired refresh deadline forever"
    );

    match client.poll_event() {
        Some(Event::AllocateError(_, _)) => Ok(()),
        other => {
            panic!("expected AllocateError for a zero-lifetime Allocate response, got {other:?}")
        }
    }
}

/// Over a TCP connection to the server the relay is still UDP: `REQUESTED-TRANSPORT` names what
/// the allocation relays, not how the client reaches the server (RFC 8656 §7.1). Deriving it from
/// the control transport asks a TCP client's server for an RFC 6062 TCP allocation instead.
#[test]
fn test_allocate_over_tcp_requests_a_udp_relay() -> Result<()> {
    let mut client = Client::new(
        ClientConfig {
            turn_serv_addr: "127.0.0.1:3478".to_owned(),
            local_addr: "127.0.0.1:50000".parse().unwrap(),
            transport_protocol: TransportProtocol::TCP,
            ..Default::default()
        },
        test_crypto_provider(),
    )?;
    client.allocate(Instant::now())?;

    let transmit = client.poll_write().expect("an Allocate request");
    assert_eq!(
        transmit.transport.transport_protocol,
        TransportProtocol::TCP,
        "the request itself still travels over TCP"
    );
    let mut msg = Message::new();
    msg.raw = transmit.message.to_vec();
    msg.decode()?;
    let mut requested = RequestedTransport::default();
    requested.get_from(&msg)?;
    assert_eq!(requested.protocol, PROTO_UDP, "the relay is UDP");
    Ok(())
}

/// Over a reliable transport a request is sent once and answered or timed out as a whole: TCP
/// already retransmits, and repeating the request on top of it only sends the server duplicates
/// (RFC 8489 §6.2.2). The wait is Ti, 39.5 seconds, not the UDP schedule's ~8.
#[test]
fn test_a_request_over_tcp_is_sent_once_and_times_out_at_ti() -> Result<()> {
    let start = Instant::now();
    let mut client = Client::new(
        ClientConfig {
            turn_serv_addr: "127.0.0.1:3478".to_owned(),
            local_addr: "127.0.0.1:50000".parse().unwrap(),
            transport_protocol: TransportProtocol::TCP,
            ..Default::default()
        },
        test_crypto_provider(),
    )?;
    client.allocate(start)?;
    // allocate() returns the id its authenticated retry will use; the timeout names the
    // request that was actually sent, so take the id from the wire.
    let sent = client.poll_write().expect("the request goes out once");
    let mut request = Message::new();
    request.raw = sent.message.to_vec();
    request.decode()?;
    let tid = request.transaction_id;

    // Walk every deadline the client asks for, up to just short of Ti.
    let ti = Duration::from_millis(39_500);
    let mut sends = 0;
    while let Some(deadline) = client.poll_timeout() {
        if deadline >= start + ti {
            break;
        }
        client.handle_timeout(deadline)?;
        while client.poll_write().is_some() {
            sends += 1;
        }
    }
    assert_eq!(sends, 0, "nothing is retransmitted over TCP");
    assert!(
        client.poll_event().is_none(),
        "no timeout before Ti has passed"
    );

    client.handle_timeout(start + ti)?;
    assert!(client.poll_write().is_none(), "not even at the deadline");
    assert!(
        matches!(client.poll_event(), Some(Event::TransactionTimeout(id)) if id == tid),
        "at Ti the request has timed out"
    );
    Ok(())
}

/// UDP keeps its schedule: the first retransmission is due one RTO after the request.
#[test]
fn test_a_request_over_udp_is_still_retransmitted() -> Result<()> {
    let start = Instant::now();
    let mut client = Client::new(
        ClientConfig {
            turn_serv_addr: "127.0.0.1:3478".to_owned(),
            local_addr: "127.0.0.1:50000".parse().unwrap(),
            transport_protocol: TransportProtocol::UDP,
            ..Default::default()
        },
        test_crypto_provider(),
    )?;
    client.allocate(start)?;
    assert!(client.poll_write().is_some());
    let first = client.poll_timeout().expect("a retransmit deadline");
    assert!(
        first < start + Duration::from_secs(1),
        "{:?}",
        first - start
    );
    client.handle_timeout(first)?;
    assert!(client.poll_write().is_some(), "UDP retransmits");
    Ok(())
}

/// A random source that counts up and records every fill it serves, and fails once told to, over
/// the built-in provider's primitives: which bytes on the wire the client drew from its provider.
#[derive(Default)]
struct RecordingRandom {
    counter: std::sync::Mutex<u8>,
    fills: std::sync::Mutex<Vec<Vec<u8>>>,
    failing: std::sync::atomic::AtomicBool,
}

impl crypto::RTCRandom for RecordingRandom {
    fn fill(&self, output: &mut [u8]) -> std::result::Result<(), crypto::CryptoError> {
        if self.failing.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(crypto::CryptoError::RandomnessFailed);
        }
        let mut counter = self.counter.lock().unwrap();
        for byte in output.iter_mut() {
            *byte = *counter;
            *counter = counter.wrapping_add(1);
        }
        self.fills.lock().unwrap().push(output.to_vec());
        Ok(())
    }
}

struct RecordingProvider {
    inner: Arc<dyn RTCCryptoProvider>,
    random: RecordingRandom,
}

impl RecordingProvider {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: test_crypto_provider(),
            random: RecordingRandom::default(),
        })
    }

    fn fills(&self) -> Vec<Vec<u8>> {
        self.random.fills.lock().unwrap().clone()
    }

    fn fail(&self) {
        self.random
            .failing
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl RTCCryptoProvider for RecordingProvider {
    fn name(&self) -> &'static str {
        "recording"
    }

    fn crypto(&self) -> &dyn crypto::RTCCrypto {
        self.inner.crypto()
    }

    fn random(&self) -> &dyn crypto::RTCRandom {
        &self.random
    }
}

fn recording_client(provider: &Arc<RecordingProvider>, software: &str) -> Result<Client> {
    Client::new(
        ClientConfig {
            stun_serv_addr: String::new(),
            turn_serv_addr: "127.0.0.1:3478".to_owned(),
            local_addr: "127.0.0.1:40000".parse().unwrap(),
            transport_protocol: TransportProtocol::UDP,
            username: "user".to_owned(),
            password: "pass".to_owned(),
            realm: "realm".to_owned(),
            software: software.to_owned(),
            rto_in_ms: 0,
            allocation_refresh_interval_cap: None,
        },
        provider.clone(),
    )
}

/// The next datagram the client sends, decoded.
fn next_sent(client: &mut Client) -> Message {
    let transmit = client.poll_write().expect("the client sends a message");
    let mut msg = Message::new();
    msg.raw = transmit.message.to_vec();
    msg.decode().expect("the client sends a STUN message");
    msg
}

/// Asserts that `msg` is a `method` message whose transaction id is the provider's fill number
/// `draw`, and that the provider has served no fill after it.
fn assert_drawn_from_provider(
    provider: &RecordingProvider,
    draw: usize,
    msg: &Message,
    method: Method,
) {
    let fills = provider.fills();
    assert_eq!(msg.typ.method, method);
    assert!(
        fills.len() > draw,
        "{method} drew its transaction id from the provider"
    );
    assert_eq!(
        fills[draw][..],
        msg.transaction_id.0[..],
        "the {method} transaction id is the provider's draw"
    );
}

fn seed_relay(client: &mut Client, relayed_addr: RelayedAddr) {
    client.relays.insert(
        relayed_addr,
        RelayState::new(
            Instant::now(),
            relayed_addr,
            vec![0u8; 16],
            Nonce::new(ATTR_NONCE, "nonce".to_owned()),
            Duration::from_secs(600),
            None,
        ),
    );
}

/// Every request and indication the client originates takes its transaction id from the crypto
/// provider it was given, never from `rand`: binding requests with and without SOFTWARE, the
/// allocation, and a relay's permission, refresh, channel binding and send indication.
#[test]
fn test_transaction_ids_are_drawn_from_the_provider() -> Result<()> {
    let provider = RecordingProvider::new();
    let server: SocketAddr = "127.0.0.1:3478".parse().unwrap();
    let now = Instant::now();

    for software in ["TEST SOFTWARE", ""] {
        let mut client = recording_client(&provider, software)?;
        let drawn = provider.fills().len();
        let tid = client.send_binding_request_to(now, server)?;
        let msg = next_sent(&mut client);
        assert_drawn_from_provider(&provider, drawn, &msg, METHOD_BINDING);
        assert_eq!(provider.fills().len(), drawn + 1, "one draw");
        assert_eq!(tid, msg.transaction_id);
        client.close()?;
    }

    let mut client = recording_client(&provider, "TEST SOFTWARE")?;
    let drawn = provider.fills().len();
    let tid = client.allocate(now)?;
    let msg = next_sent(&mut client);
    assert_drawn_from_provider(&provider, drawn, &msg, METHOD_ALLOCATE);
    assert_eq!(provider.fills().len(), drawn + 1, "one draw");
    let mut expected = msg.transaction_id;
    expected.0[TRANSACTION_ID_SIZE - 1] = expected.0[TRANSACTION_ID_SIZE - 1].wrapping_add(1);
    assert_eq!(tid, expected, "allocate names its authenticated retry");

    let relayed_addr: RelayedAddr = "127.0.0.1:50000".parse().unwrap();
    let peer: SocketAddr = "127.0.0.1:60000".parse().unwrap();
    seed_relay(&mut client, relayed_addr);

    let drawn = provider.fills().len();
    let tid = client
        .relay(relayed_addr)?
        .create_permission(now, peer)?
        .expect("a new peer needs a permission");
    let msg = next_sent(&mut client);
    assert_drawn_from_provider(&provider, drawn, &msg, METHOD_CREATE_PERMISSION);
    assert_eq!(provider.fills().len(), drawn + 1, "one draw");
    assert_eq!(tid, msg.transaction_id);

    let mut response = Message::new();
    response.build(&[
        Box::new(tid),
        Box::new(MessageType::new(
            METHOD_CREATE_PERMISSION,
            CLASS_SUCCESS_RESPONSE,
        )),
    ])?;
    client.handle_read(TransportMessage {
        now,
        transport: TransportContext {
            local_addr: "127.0.0.1:40000".parse().unwrap(),
            peer_addr: server,
            transport_protocol: TransportProtocol::UDP,
            ecn: None,
        },
        message: BytesMut::from(&response.raw[..]),
    })?;

    let drawn = provider.fills().len();
    client
        .relay(relayed_addr)?
        .refresh_allocation(now, Duration::from_secs(600))?;
    let msg = next_sent(&mut client);
    assert_drawn_from_provider(&provider, drawn, &msg, METHOD_REFRESH);
    assert_eq!(provider.fills().len(), drawn + 1, "one draw");

    // The first datagram to a permitted peer binds a channel and goes out as a send indication.
    let drawn = provider.fills().len();
    client.relay(relayed_addr)?.send_to(now, b"payload", peer)?;
    let channel_bind = next_sent(&mut client);
    assert_drawn_from_provider(&provider, drawn, &channel_bind, METHOD_CHANNEL_BIND);
    let indication = next_sent(&mut client);
    assert_drawn_from_provider(&provider, drawn + 1, &indication, METHOD_SEND);
    assert_eq!(provider.fills().len(), drawn + 2, "one draw for each");

    client.close()
}

/// A provider that cannot produce random bytes fails the request with a crypto error, and
/// nothing goes on the wire.
#[test]
fn test_a_failing_provider_sends_nothing() -> Result<()> {
    let provider = RecordingProvider::new();
    let mut client = recording_client(&provider, "TEST SOFTWARE")?;
    let relayed_addr: RelayedAddr = "127.0.0.1:50000".parse().unwrap();
    seed_relay(&mut client, relayed_addr);
    provider.fail();
    let now = Instant::now();

    assert!(matches!(
        client.send_binding_request_to(now, "127.0.0.1:3478".parse().unwrap()),
        Err(Error::Crypto(_))
    ));
    assert!(matches!(client.allocate(now), Err(Error::Crypto(_))));
    assert!(matches!(
        client
            .relay(relayed_addr)?
            .create_permission(now, "127.0.0.1:60000".parse().unwrap()),
        Err(Error::Crypto(_))
    ));
    assert!(client.poll_write().is_none(), "nothing was sent");

    Ok(())
}
