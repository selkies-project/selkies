//! The server picks the SRTP protection profile by its own order among those both sides
//! offer, so an AEAD profile it lists first wins over a client's SHA1_80-first offer
//! (RFC 8827 section 6.5: SHOULD favor AEAD).

use crypto::default_provider;
use rtc_dtls::{
    config::ConfigBuilder,
    crypto::Certificate,
    endpoint::{Endpoint, EndpointEvent},
    extension::extension_use_srtp::SrtpProtectionProfile::{
        self, Srtp_Aead_Aes_128_Gcm, Srtp_Aead_Aes_256_Gcm, Srtp_Aes128_Cm_Hmac_Sha1_80,
    },
};
use shared::TransportProtocol;
use std::{net::SocketAddr, sync::Arc, time::Instant};

type TestResult<T> = Result<T, Box<dyn std::error::Error>>;

/// Delivers everything `source` has queued; true when that completed the receiver's handshake.
fn transfer(
    source: &mut Endpoint,
    destination: &mut Endpoint,
    source_addr: SocketAddr,
    now: Instant,
) -> TestResult<bool> {
    let mut completed = false;
    while let Some(packet) = source.poll_transmit() {
        completed |= destination
            .read(now, source_addr, packet.transport.ecn, packet.message)?
            .iter()
            .any(|event| matches!(event, EndpointEvent::HandshakeComplete));
    }
    Ok(completed)
}

/// The profile the server settles on for a client offering `offered`.
fn negotiate(
    offered: Vec<SrtpProtectionProfile>,
    supported: Vec<SrtpProtectionProfile>,
) -> TestResult<SrtpProtectionProfile> {
    let provider = default_provider()?;
    let certificate =
        Certificate::generate_self_signed(vec!["localhost".into()], provider.crypto())?;
    let configuration = ConfigBuilder::default()
        .with_crypto_provider(provider)
        .with_certificates(vec![certificate])
        .with_insecure_skip_verify(true);
    let client_addr: SocketAddr = "127.0.0.1:44011".parse()?;
    let server_addr: SocketAddr = "127.0.0.1:44012".parse()?;
    let mut client = Endpoint::new(client_addr, TransportProtocol::UDP, None);
    let mut server = Endpoint::new(
        server_addr,
        TransportProtocol::UDP,
        Some(Arc::new(
            configuration
                .clone()
                .with_srtp_protection_profiles(supported)
                .build(false, None)?,
        )),
    );
    let now = Instant::now();
    client.connect(
        now,
        server_addr,
        Arc::new(
            configuration
                .with_srtp_protection_profiles(offered)
                .build(true, None)?,
        ),
        None,
    )?;
    for _ in 0..8 {
        let server_done = transfer(&mut client, &mut server, client_addr, now)?;
        transfer(&mut server, &mut client, server_addr, now)?;
        if server_done {
            break;
        }
    }
    Ok(server
        .get_connection_state(client_addr)
        .expect("the server holds the association")
        .srtp_protection_profile())
}

#[test]
fn a_sha1_first_offer_takes_the_servers_first_aead_profile() -> TestResult<()> {
    let chosen = negotiate(
        vec![
            Srtp_Aes128_Cm_Hmac_Sha1_80,
            Srtp_Aead_Aes_256_Gcm,
            Srtp_Aead_Aes_128_Gcm,
        ],
        vec![
            Srtp_Aead_Aes_256_Gcm,
            Srtp_Aead_Aes_128_Gcm,
            Srtp_Aes128_Cm_Hmac_Sha1_80,
        ],
    )?;
    assert_eq!(chosen, Srtp_Aead_Aes_256_Gcm);
    Ok(())
}

#[test]
fn an_offer_without_aead_takes_sha1_80() -> TestResult<()> {
    let chosen = negotiate(
        vec![Srtp_Aes128_Cm_Hmac_Sha1_80],
        vec![
            Srtp_Aead_Aes_256_Gcm,
            Srtp_Aead_Aes_128_Gcm,
            Srtp_Aes128_Cm_Hmac_Sha1_80,
        ],
    )?;
    assert_eq!(chosen, Srtp_Aes128_Cm_Hmac_Sha1_80);
    Ok(())
}

#[test]
fn the_servers_order_decides_between_two_aead_profiles() -> TestResult<()> {
    let chosen = negotiate(
        vec![Srtp_Aead_Aes_128_Gcm, Srtp_Aead_Aes_256_Gcm],
        vec![Srtp_Aead_Aes_256_Gcm, Srtp_Aead_Aes_128_Gcm],
    )?;
    assert_eq!(chosen, Srtp_Aead_Aes_256_Gcm);
    Ok(())
}
