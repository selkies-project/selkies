use crypto::default_provider;
use rtc_dtls::{
    config::{ClientAuthType, ConfigBuilder, VerifyPeerCertificateFn},
    crypto::Certificate,
    endpoint::{Endpoint, EndpointEvent},
};
use shared::{TransportProtocol, error::Error};
use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Association {
    client: Endpoint,
    server: Endpoint,
    client_addr: SocketAddr,
    server_addr: SocketAddr,
    start: Instant,
}

impl Association {
    /// Starts a mutually authenticated handshake; the ClientHello is queued.
    fn connect() -> Result<Self, Box<dyn std::error::Error>> {
        Self::connect_with_server_verifier(None)
    }

    fn connect_with_server_verifier(
        verifier: Option<VerifyPeerCertificateFn>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let provider = default_provider()?;
        let certificate =
            Certificate::generate_self_signed(vec!["localhost".into()], provider.crypto())?;
        let configuration = ConfigBuilder::default()
            .with_crypto_provider(provider)
            .with_certificates(vec![certificate])
            // Self-signed peers keep this packet-loss regression independent of a CA.
            .with_insecure_skip_verify(true)
            .with_client_auth(ClientAuthType::RequireAnyClientCert);
        let client_addr: SocketAddr = "127.0.0.1:44001".parse()?;
        let server_addr: SocketAddr = "127.0.0.1:44002".parse()?;
        let mut client = Endpoint::new(client_addr, TransportProtocol::UDP, None);
        let server = Endpoint::new(
            server_addr,
            TransportProtocol::UDP,
            Some(Arc::new(
                configuration
                    .clone()
                    .with_verify_peer_certificate(verifier)
                    .build(false, None)?,
            )),
        );
        let start = Instant::now();

        client.connect(
            start,
            server_addr,
            Arc::new(configuration.build(true, None)?),
            None,
        )?;

        Ok(Self {
            client,
            server,
            client_addr,
            server_addr,
            start,
        })
    }
}

fn transfer(
    source: &mut Endpoint,
    destination: &mut Endpoint,
    source_addr: SocketAddr,
    now: Instant,
) -> Result<bool, Box<dyn std::error::Error>> {
    let mut completed = false;

    while let Some(packet) = source.poll_transmit() {
        completed |= destination
            .read(now, source_addr, packet.transport.ecn, packet.message)?
            .iter()
            .any(|event| matches!(event, EndpointEvent::HandshakeComplete));
    }

    Ok(completed)
}

#[test]
fn lost_server_flight_recovers_after_repeated_client_hello() -> TestResult {
    recover_lost_flight(LostFlight::Server)
}

#[test]
fn lost_client_certificate_flight_recovers_after_repeated_server_flight() -> TestResult {
    recover_lost_flight(LostFlight::ClientCertificate)
}

#[test]
fn rejected_client_certificate_cancels_retransmission() -> TestResult {
    let Association {
        mut client,
        mut server,
        client_addr,
        server_addr,
        start,
    } = Association::connect_with_server_verifier(Some(Arc::new(|_, _| {
        Err(Error::ErrInvalidCertificate)
    })))?;

    // ClientHello -> HelloVerifyRequest -> ClientHello -> server flight.
    transfer(&mut client, &mut server, client_addr, start)?;
    transfer(&mut server, &mut client, server_addr, start)?;
    transfer(&mut client, &mut server, client_addr, start)?;
    transfer(&mut server, &mut client, server_addr, start)?;

    let deadline = server
        .poll_timeout(&client_addr)
        .expect("the server is waiting for the client certificate flight");
    let error = transfer(&mut client, &mut server, client_addr, start)
        .expect_err("the server verifier must reject the client certificate");
    assert!(matches!(
        error.downcast_ref::<Error>(),
        Some(Error::ErrInvalidCertificate)
    ));

    // Drain the fatal alert before checking for any later handshake output.
    assert!(server.poll_transmit().is_some(), "a fatal alert is queued");
    while server.poll_transmit().is_some() {}

    assert_eq!(
        server.poll_timeout(&client_addr),
        None,
        "a fatal handshake error must cancel the retransmission deadline"
    );
    server.handle_timeout(client_addr, deadline + Duration::from_secs(1))?;
    assert!(
        server.poll_transmit().is_none(),
        "the rejected handshake must not retransmit its previous flight"
    );
    assert_eq!(server.poll_timeout(&client_addr), None);
    Ok(())
}

#[derive(Clone, Copy)]
enum LostFlight {
    Server,
    ClientCertificate,
}

fn recover_lost_flight(lost_flight: LostFlight) -> TestResult {
    let drop_client_flight = matches!(lost_flight, LostFlight::ClientCertificate);
    let Association {
        mut client,
        mut server,
        client_addr,
        server_addr,
        start,
    } = Association::connect()?;

    // ClientHello -> HelloVerifyRequest -> ClientHello with the cookie.
    transfer(&mut client, &mut server, client_addr, start)?;
    transfer(&mut server, &mut client, server_addr, start)?;
    transfer(&mut client, &mut server, client_addr, start)?;

    let mut client_completed = false;
    let mut server_completed = false;

    if drop_client_flight {
        client_completed |= transfer(&mut server, &mut client, server_addr, start)?;
    }

    // Lose one complete flight. Receiving the peer's repeated previous flight
    // must not disable our retransmission while waiting for its next flight.
    let lost_sender = if drop_client_flight {
        &mut client
    } else {
        &mut server
    };
    let mut dropped = 0;

    while lost_sender.poll_transmit().is_some() {
        dropped += 1;
    }

    assert!(dropped > 0, "a handshake flight must actually be lost");

    for tick in 1..=50 {
        let now = start + Duration::from_millis(tick * 100);

        // Deliver the peer's repeated previous flight before our own timer:
        // the precise race that cancels retransmission on the affected side.
        if drop_client_flight {
            if server
                .poll_timeout(&client_addr)
                .is_some_and(|deadline| deadline <= now)
            {
                server.handle_timeout(client_addr, now)?;
            }
            client_completed |= transfer(&mut server, &mut client, server_addr, now)?;
        }
        if client
            .poll_timeout(&server_addr)
            .is_some_and(|deadline| deadline <= now)
        {
            client.handle_timeout(server_addr, now)?;
        }
        server_completed |= transfer(&mut client, &mut server, client_addr, now)?;
        if server
            .poll_timeout(&client_addr)
            .is_some_and(|deadline| deadline <= now)
        {
            server.handle_timeout(client_addr, now)?;
        }
        client_completed |= transfer(&mut server, &mut client, server_addr, now)?;

        if client_completed && server_completed {
            break;
        }
    }
    assert!(
        client_completed && server_completed,
        "DTLS must recover a lost flight after the peer repeats its previous flight (lost client flight: {drop_client_flight})"
    );

    client.write(
        start + Duration::from_secs(5),
        server_addr,
        b"direct after packet loss",
    )?;
    let response = client
        .poll_transmit()
        .expect("an established association sends application data");
    let replay = response.message.clone();
    let events = server.read(
        start + Duration::from_secs(5),
        client_addr,
        response.transport.ecn,
        response.message,
    )?;
    let application_delivered = events.iter().any(|event| {
        matches!(event, EndpointEvent::ApplicationData(data) if data.as_ref() == b"direct after packet loss")
    });
    assert!(
        application_delivered,
        "the recovered DTLS association delivers application data"
    );
    assert!(
        server
            .read(start + Duration::from_secs(5), client_addr, None, replay)?
            .is_empty(),
        "retransmission recovery must preserve application replay protection"
    );
    Ok(())
}
