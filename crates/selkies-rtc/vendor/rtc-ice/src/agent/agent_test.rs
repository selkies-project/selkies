use std::str::FromStr;
use std::time::Instant;
use stun::message::*;
use stun::textattrs::Username;

use super::*;
use crate::attributes::{
    control::AttrControlling, priority::PriorityAttr, use_candidate::UseCandidateAttr,
};
use crate::candidate::candidate_host::*;
use crate::candidate::candidate_peer_reflexive::*;
use crate::candidate::candidate_relay::CandidateRelayConfig;
use crate::candidate::candidate_server_reflexive::*;
use crate::candidate::*;

/// Explicit provider for tests. The default-resolving STUN constructors were removed before 1.0,
/// so every `MessageIntegrity` now names its provider.
fn test_crypto_provider() -> std::sync::Arc<dyn crypto::RTCCryptoProvider> {
    crypto::default_provider().expect("a built-in crypto provider must be enabled for tests")
}

#[test]
fn test_pair_search() -> Result<()> {
    let config = Arc::new(AgentConfig::default());
    let mut a = Agent::new(Instant::now(), config, test_crypto_provider())?;

    assert!(
        a.candidate_pairs.is_empty(),
        "TestPairSearch is only a valid test if a.validPairs is empty on construction"
    );

    let cp = a.get_best_available_pair();
    assert!(cp.is_none(), "No Candidate pairs should exist");

    a.close()?;

    Ok(())
}

#[test]
fn test_pair_priority() -> Result<()> {
    let mut a = Agent::new(
        Instant::now(),
        Arc::new(AgentConfig::default()),
        test_crypto_provider(),
    )?;

    let host_config = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "192.168.1.1".to_owned(),
            port: 19216,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let host_local = host_config.new_candidate_host()?;
    a.local_candidates.push(host_local);

    let relay_config = CandidateRelayConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "1.2.3.4".to_owned(),
            port: 12340,
            component: 1,
            ..Default::default()
        },
        rel_addr: "4.3.2.1".to_owned(),
        rel_port: 43210,
        ..Default::default()
    };

    let relay_remote = relay_config.new_candidate_relay()?;

    let srflx_config = CandidateServerReflexiveConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "10.10.10.2".to_owned(),
            port: 19218,
            component: 1,
            ..Default::default()
        },
        rel_addr: "4.3.2.1".to_owned(),
        rel_port: 43212,
        ..Default::default()
    };

    let srflx_remote = srflx_config.new_candidate_server_reflexive()?;

    let prflx_config = CandidatePeerReflexiveConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "10.10.10.2".to_owned(),
            port: 19217,
            component: 1,
            ..Default::default()
        },
        rel_addr: "4.3.2.1".to_owned(),
        rel_port: 43211,
        ..Default::default()
    };

    let prflx_remote = prflx_config.new_candidate_peer_reflexive()?;

    let host_config = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "1.2.3.5".to_owned(),
            port: 12350,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let host_remote = host_config.new_candidate_host()?;

    let remotes: Vec<Candidate> = vec![relay_remote, srflx_remote, prflx_remote, host_remote];
    for remote in remotes {
        a.remote_candidates.push(remote);
    }

    {
        let local = 0;
        for remote in 0..a.remote_candidates.len() {
            if a.find_pair(local, remote).is_none() {
                a.add_pair(local, remote);
            }

            if let Some(p) = a.find_pair(local, remote) {
                a.candidate_pairs[p].state = CandidatePairState::Succeeded;
            }

            if let Some(best_pair) = a.get_best_available_pair() {
                assert_eq!(
                    a.candidate_pairs[best_pair].to_string(),
                    CandidatePair::new(
                        local,
                        remote,
                        a.local_candidates[local].priority(),
                        a.remote_candidates[remote].priority(),
                        a.is_controlling,
                    )
                    .to_string(),
                    "Unexpected bestPair {best_pair} (expected remote: {remote})",
                );
            } else {
                panic!("expected Some, but got None");
            }
        }
    }

    a.close()?;
    Ok(())
}

fn pipe(
    default_config0: Option<AgentConfig>,
    default_config1: Option<AgentConfig>,
) -> Result<(Agent, Agent)> {
    let mut cfg0 = if let Some(cfg) = default_config0 {
        cfg
    } else {
        AgentConfig::default()
    };
    cfg0.urls = vec![];

    let a_agent = Agent::new(Instant::now(), Arc::new(cfg0), test_crypto_provider())?;

    let mut cfg1 = if let Some(cfg) = default_config1 {
        cfg
    } else {
        AgentConfig::default()
    };
    cfg1.urls = vec![];

    let b_agent = Agent::new(Instant::now(), Arc::new(cfg1), test_crypto_provider())?;

    Ok((a_agent, b_agent))
}

#[test]
fn test_on_selected_candidate_pair_change() -> Result<()> {
    let mut a = Agent::new(
        Instant::now(),
        Arc::new(AgentConfig::default()),
        test_crypto_provider(),
    )?;

    let host_config = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "192.168.1.1".to_owned(),
            port: 19216,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let host_local = host_config.new_candidate_host()?;
    a.add_local_candidate(host_local)?;

    let relay_config = CandidateRelayConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "1.2.3.4".to_owned(),
            port: 12340,
            component: 1,
            ..Default::default()
        },
        rel_addr: "4.3.2.1".to_owned(),
        rel_port: 43210,
        ..Default::default()
    };
    let relay_remote = relay_config.new_candidate_relay()?;
    a.add_remote_candidate(relay_remote)?;

    // select the pair
    let (local, remote) = (0, 0);
    a.add_pair(local, remote);
    a.set_selected_pair(Some(Instant::now()), Some(0));

    // ensure that the callback fired on setting the pair
    let mut is_selected_candidate_pair_change_event_fired = false;
    while let Some(event) = a.poll_event() {
        if let Event::SelectedCandidatePairChange(_, _) = event.event {
            is_selected_candidate_pair_change_event_fired = true;
        }
    }

    assert!(is_selected_candidate_pair_change_event_fired);

    a.close()?;
    Ok(())
}

#[test]
fn test_handle_peer_reflexive_udp_pflx_candidate() -> Result<()> {
    let mut a = Agent::new(
        Instant::now(),
        Arc::new(AgentConfig::default()),
        test_crypto_provider(),
    )?;

    let host_config = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "192.168.0.2".to_owned(),
            port: 777,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    };

    let local_candidate = host_config.new_candidate_host()?;
    let local = 0;
    let local_priority = local_candidate.priority();
    let local_network_type = local_candidate.network_type();
    a.add_local_candidate(local_candidate)?;

    let remote_addr = SocketAddr::from_str("172.17.0.3:999")?;

    let (username, local_pwd, tie_breaker) = {
        a.ufrag_pwd.remote_credentials = Some(Credentials {
            ufrag: "".to_string(),
            pwd: "".to_string(),
        });
        (
            a.ufrag_pwd.local_credentials.ufrag.to_owned() + ":",
            a.ufrag_pwd.local_credentials.pwd.clone(),
            a.tie_breaker,
        )
    };

    let mut msg = Message::new();
    msg.build(&[
        Box::new(BINDING_REQUEST),
        Box::new(TransactionId::new()),
        Box::new(Username::new(ATTR_USERNAME, username)),
        Box::new(UseCandidateAttr::new()),
        Box::new(AttrControlling(tie_breaker)),
        Box::new(PriorityAttr(local_priority)),
        Box::new(MessageIntegrity::new_short_term_integrity_with_provider(
            local_pwd,
            test_crypto_provider().crypto(),
        )),
        Box::new(FINGERPRINT),
    ])?;

    {
        a.handle_inbound(Instant::now(), &mut msg, local, remote_addr)?;

        // length of remote candidate list must be one now
        assert_eq!(
            a.remote_candidates.len(),
            1,
            "failed to add a network type to the remote candidate list"
        );

        // length of remote candidate list for a network type must be 1
        if let Some(remote_index) = a.find_remote_candidate(remote_addr) {
            let c = &a.remote_candidates[remote_index];

            assert_eq!(
                c.candidate_type(),
                CandidateType::PeerReflexive,
                "candidate type must be prflx"
            );

            assert_eq!(c.address(), "172.17.0.3", "IP address mismatch");

            assert_eq!(c.port(), 999, "Port number mismatch");
        } else {
            assert!(
                false,
                "expected non-empty remote candidate for network type {}",
                local_network_type,
            );
        }
    }

    a.close()?;
    Ok(())
}

#[test]
fn test_handle_peer_reflexive_unknown_remote() -> Result<()> {
    let mut a = Agent::new(
        Instant::now(),
        Arc::new(AgentConfig::default()),
        test_crypto_provider(),
    )?;

    let mut tid = TransactionId::default();
    tid.0[..3].copy_from_slice("ABC".as_bytes());

    let remote_pwd = {
        a.pending_binding_requests = vec![BindingRequest {
            timestamp: Instant::now(),
            transaction_id: tid,
            destination: SocketAddr::from_str("0.0.0.0:0")?,
            is_use_candidate: false,
        }];
        a.ufrag_pwd.remote_credentials = Some(Credentials {
            ufrag: "".to_string(),
            pwd: "".to_string(),
        });
        "".to_string()
    };

    let host_config = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "192.168.0.2".to_owned(),
            port: 777,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    };

    let local = host_config.new_candidate_host()?;
    let local_index = 0;
    a.add_local_candidate(local)?;
    let remote_addr = SocketAddr::from_str("172.17.0.3:999")?;

    let mut msg = Message::new();
    msg.build(&[
        Box::new(BINDING_SUCCESS),
        Box::new(tid),
        Box::new(MessageIntegrity::new_short_term_integrity_with_provider(
            remote_pwd,
            test_crypto_provider().crypto(),
        )),
        Box::new(FINGERPRINT),
    ])?;

    let result = a.handle_inbound(Instant::now(), &mut msg, local_index, remote_addr);
    assert!(result.is_err());

    assert_eq!(
        a.remote_candidates.len(),
        0,
        "unknown remote was able to create a candidate"
    );

    a.close()?;
    Ok(())
}

/* TODO:
fn gather_and_exchange_candidates(a_agent: &mut Agent, b_agent: &mut Agent) -> Result<()> {
    let wg = WaitGroup::new();

    let w1 = Arc::new(Mutex::new(Some(wg.worker())));
    a_agent.on_candidate(Box::new(
        move |candidate: Option<Arc<dyn Candidate + Send + Sync>>| {
            let w3 = Arc::clone(&w1);
            Box::pin(async move {
                if candidate.is_none() {
                    let mut w = w3.lock().await;
                    w.take();
                }
            })
        },
    ));
    a_agent.gather_candidates()?;

    let w2 = Arc::new(Mutex::new(Some(wg.worker())));
    b_agent.on_candidate(Box::new(
        move |candidate: Option<Arc<dyn Candidate + Send + Sync>>| {
            let w3 = Arc::clone(&w2);
            Box::pin(async move {
                if candidate.is_none() {
                    let mut w = w3.lock().await;
                    w.take();
                }
            })
        },
    ));
    b_agent.gather_candidates()?;

    wg.wait().await;

    let candidates = a_agent.get_local_candidates().await?;
    for c in candidates {
        let c2: Arc<dyn Candidate + Send + Sync> =
            Arc::new(unmarshal_candidate(c.marshal().as_str())?);
        b_agent.add_remote_candidate(&c2)?;
    }

    let candidates = b_agent.get_local_candidates().await?;
    for c in candidates {
        let c2: Arc<dyn Candidate + Send + Sync> =
            Arc::new(unmarshal_candidate(c.marshal().as_str())?);
        a_agent.add_remote_candidate(&c2)?;
    }

    Ok(())
}

// Assert that Agent on startup sends message, and doesn't wait for connectivityTicker to fire
#[test]
fn test_connectivity_on_startup() -> Result<()> {
    let keepalive_interval = Some(Duration::from_secs(3600)); //time.Hour
    let check_interval = Duration::from_secs(3600); //time.Hour
    let cfg0 = AgentConfig {
        keepalive_interval,
        check_interval,
        ..Default::default()
    };

    let mut a_agent = Agent::new(Instant::now(), cfg0, test_crypto_provider())?;

    let cfg1 = AgentConfig {
        keepalive_interval,
        check_interval,
        ..Default::default()
    };

    let mut b_agent = Agent::new(Instant::now(), cfg1, test_crypto_provider())?;

    // Manual signaling
    let (a_ufrag, a_pwd) = a_agent.get_local_user_credentials();
    let (b_ufrag, b_pwd) = b_agent.get_local_user_credentials();

    gather_and_exchange_candidates(&mut a_agent, &mut b_agent)?;

    a_agent.on_connection_state_change(Box::new(move |s: ConnectionState| {
        let accepted_tx_clone = Arc::clone(&accepting_tx);
        Box::pin(async move {
            if s == ConnectionState::Checking {
                let mut tx = accepted_tx_clone.lock();
                tx.take();
            }
        })
    }));

    tokio::spawn(async move {
        let result = a_agent.accept(a_cancel_rx, b_ufrag, b_pwd);
        assert!(result.is_ok(), "agent accept expected OK");
        drop(accepted_tx);
    });

    let _ = accepting_rx.recv();

    let _ = b_agent.dial(b_cancel_rx, a_ufrag, a_pwd)?;

    // Ensure accepted
    let _ = accepted_rx.recv();

    Ok(())
}


#[test]
fn test_connectivity_lite() -> Result<()> {
    let stun_server_url = Url {
        scheme: SchemeType::Stun,
        host: "1.2.3.4".to_owned(),
        port: 3478,
        proto: ProtoType::Udp,
        ..Default::default()
    };

    let nat_type = nat::NatType {
        mapping_behavior: nat::EndpointDependencyType::EndpointIndependent,
        filtering_behavior: nat::EndpointDependencyType::EndpointIndependent,
        ..Default::default()
    };

    let v = build_vnet(nat_type, nat_type)?;

    let (a_notifier, mut a_connected) = on_connected();
    let (b_notifier, mut b_connected) = on_connected();

    let cfg0 = AgentConfig {
        urls: vec![stun_server_url],
        network_types: supported_network_types(),
        net: Some(Arc::clone(&v.net0)),
        ..Default::default()
    };

    let a_agent = Arc::new(Agent::new(Instant::now(), cfg0, test_crypto_provider())?);
    a_agent.on_connection_state_change(a_notifier);

    let cfg1 = AgentConfig {
        urls: vec![],
        lite: true,
        candidate_types: vec![CandidateType::Host],
        network_types: supported_network_types(),
        net: Some(Arc::clone(&v.net1)),
        ..Default::default()
    };

    let b_agent = Arc::new(Agent::new(Instant::now(), cfg1, test_crypto_provider())?);
    b_agent.on_connection_state_change(b_notifier);

    let _ = connect_with_vnet(&a_agent, &b_agent)?;

    // Ensure pair selected
    // Note: this assumes ConnectionStateConnected is thrown after selecting the final pair
    let _ = a_connected.recv();
    let _ = b_connected.recv();

    v.close()?;

    Ok(())
}

struct MockPacketConn;

#[async_trait]
impl Conn for MockPacketConn {
    fn connect(&self, _addr: SocketAddr) -> std::result::Result<(), util::Error> {
        Ok(())
    }

    fn recv(&self, _buf: &mut [u8]) -> std::result::Result<usize, util::Error> {
        Ok(0)
    }

    fn recv_from(&self, _buf: &mut [u8]) -> std::result::Result<(usize, SocketAddr), util::Error> {
        Ok((0, SocketAddr::new(Ipv4Addr::new(0, 0, 0, 0).into(), 0)))
    }

    fn send(&self, _buf: &[u8]) -> std::result::Result<usize, util::Error> {
        Ok(0)
    }

    fn send_to(&self, _buf: &[u8], _target: SocketAddr) -> std::result::Result<usize, util::Error> {
        Ok(0)
    }

    fn local_addr(&self) -> std::result::Result<SocketAddr, util::Error> {
        Ok(SocketAddr::new(Ipv4Addr::new(0, 0, 0, 0).into(), 0))
    }

    fn remote_addr(&self) -> Option<SocketAddr> {
        None
    }

    fn close(&self) -> std::result::Result<(), util::Error> {
        Ok(())
    }
}

fn build_msg(c: MessageClass, username: String, key: String) -> Result<Message> {
    let mut msg = Message::new();
    msg.build(&[
        Box::new(MessageType::new(METHOD_BINDING, c)),
        Box::new(TransactionId::new()),
        Box::new(Username::new(ATTR_USERNAME, username)),
        Box::new(MessageIntegrity::new_short_term_integrity_with_provider(key, test_crypto_provider().crypto())),
        Box::new(FINGERPRINT),
    ])?;
    Ok(msg)
}

#[test]
fn test_inbound_validity() -> Result<()> {
    /*env_logger::Builder::new()
    .format(|buf, record| {
        writeln!(
            buf,
            "{}:{} [{}] {} - {}",
            record.file().unwrap_or("unknown"),
            record.line().unwrap_or(0),
            record.level(),
            chrono::Local::now().format("%H:%M:%S.%6f"),
            record.args()
        )
    })
    .filter(None, LevelFilter::Trace)
    .init();*/

    let remote = SocketAddr::from_str("172.17.0.3:999")?;
    let local: Arc<dyn Candidate + Send + Sync> = Arc::new(
        CandidateHostConfig {
            base_config: CandidateConfig {
                network: "udp".to_owned(),
                address: "192.168.0.2".to_owned(),
                port: 777,
                component: 1,
                conn: Some(Arc::new(MockPacketConn {})),
                ..Default::default()
            },
            ..Default::default()
        }
        .new_candidate_host()?,
    );

    //"Invalid Binding requests should be discarded"
    {
        let a = Agent::new(Instant::now(), AgentConfig::default())?;

        {
            let local_pwd = {
                let ufrag_pwd = a.internal.ufrag_pwd.lock();
                ufrag_pwd.local_pwd.clone()
            };
            a.internal.handle_inbound(
                &mut build_msg(CLASS_REQUEST, "invalid".to_owned(), local_pwd)?,
                &local,
                remote,
            );
            {
                let remote_candidates = a.internal.remote_candidates.lock();
                assert_ne!(
                    remote_candidates.len(),
                    1,
                    "Binding with invalid Username was able to create prflx candidate"
                );
            }

            let username = {
                let ufrag_pwd = a.internal.ufrag_pwd.lock();
                format!("{}:{}", ufrag_pwd.local_ufrag, ufrag_pwd.remote_ufrag)
            };
            a.internal.handle_inbound(
                &mut build_msg(CLASS_REQUEST, username, "Invalid".to_owned())?,
                &local,
                remote,
            );
            {
                let remote_candidates = a.internal.remote_candidates.lock();
                assert_ne!(
                    remote_candidates.len(),
                    1,
                    "Binding with invalid MessageIntegrity was able to create prflx candidate"
                );
            }
        }

        a.close()?;
    }

    //"Invalid Binding success responses should be discarded"
    {
        let a = Agent::new(Instant::now(), AgentConfig::default())?;

        {
            let username = {
                let ufrag_pwd = a.internal.ufrag_pwd.lock();
                format!("{}:{}", ufrag_pwd.local_ufrag, ufrag_pwd.remote_ufrag)
            };
            a.internal.handle_inbound(
                &mut build_msg(CLASS_SUCCESS_RESPONSE, username, "Invalid".to_owned())?,
                &local,
                remote,
            );
            {
                let remote_candidates = a.internal.remote_candidates.lock();
                assert_ne!(
                    remote_candidates.len(),
                    1,
                    "Binding with invalid Username was able to create prflx candidate"
                );
            }
        }

        a.close()?;
    }

    //"Discard non-binding messages"
    {
        let a = Agent::new(Instant::now(), AgentConfig::default())?;

        {
            let username = {
                let ufrag_pwd = a.internal.ufrag_pwd.lock();
                format!("{}:{}", ufrag_pwd.local_ufrag, ufrag_pwd.remote_ufrag)
            };
            a.internal.handle_inbound(
                &mut build_msg(CLASS_ERROR_RESPONSE, username, "Invalid".to_owned())?,
                &local,
                remote,
            );
            let remote_candidates = a.internal.remote_candidates.lock();
            assert_ne!(
                remote_candidates.len(),
                1,
                "non-binding message was able to create prflxRemote"
            );
        }

        a.close()?;
    }

    //"Valid bind request"
    {
        let a = Agent::new(Instant::now(), AgentConfig::default())?;

        {
            let (username, local_pwd) = {
                let ufrag_pwd = a.internal.ufrag_pwd.lock();
                (
                    format!("{}:{}", ufrag_pwd.local_ufrag, ufrag_pwd.remote_ufrag),
                    ufrag_pwd.local_pwd.clone(),
                )
            };
            a.internal.handle_inbound(
                &mut build_msg(CLASS_REQUEST, username, local_pwd)?,
                &local,
                remote,
            );
            let remote_candidates = a.internal.remote_candidates.lock();
            assert_eq!(
                remote_candidates.len(),
                1,
                "Binding with valid values was unable to create prflx candidate"
            );
        }

        a.close()?;
    }

    //"Valid bind without fingerprint"
    {
        let a = Agent::new(Instant::now(), AgentConfig::default())?;

        {
            let (username, local_pwd) = {
                let ufrag_pwd = a.internal.ufrag_pwd.lock();
                (
                    format!("{}:{}", ufrag_pwd.local_ufrag, ufrag_pwd.remote_ufrag),
                    ufrag_pwd.local_pwd.clone(),
                )
            };

            let mut msg = Message::new();
            msg.build(&[
                Box::new(BINDING_REQUEST),
                Box::new(TransactionId::new()),
                Box::new(Username::new(ATTR_USERNAME, username)),
                Box::new(MessageIntegrity::new_short_term_integrity_with_provider(local_pwd, test_crypto_provider())),
            ])?;

            a.internal.handle_inbound(&mut msg, &local, remote);
            let remote_candidates = a.internal.remote_candidates.lock();
            assert_eq!(
                remote_candidates.len(),
                1,
                "Binding with valid values (but no fingerprint) was unable to create prflx candidate"
            );
        }

        a.close()?;
    }

    //"Success with invalid TransactionID"
    {
        let a = Agent::new(Instant::now(), AgentConfig::default())?;

        {
            let remote = SocketAddr::from_str("172.17.0.3:999")?;

            let mut t_id = TransactionId::default();
            t_id.0[..3].copy_from_slice(b"ABC");

            let remote_pwd = {
                let ufrag_pwd = a.internal.ufrag_pwd.lock();
                ufrag_pwd.remote_pwd.clone()
            };

            let mut msg = Message::new();
            msg.build(&[
                Box::new(BINDING_SUCCESS),
                Box::new(t_id),
                Box::new(MessageIntegrity::new_short_term_integrity_with_provider(remote_pwd, test_crypto_provider())),
                Box::new(FINGERPRINT),
            ])?;

            a.internal.handle_inbound(&mut msg, &local, remote);

            {
                let remote_candidates = a.internal.remote_candidates.lock();
                assert_eq!(
                    remote_candidates.len(),
                    0,
                    "unknown remote was able to create a candidate"
                );
            }
        }

        a.close()?;
    }

    Ok(())
}

#[test]
fn test_invalid_agent_starts() -> Result<()> {
    let a = Agent::new(Instant::now(), AgentConfig::default())?;

    let (_cancel_tx1, cancel_rx1) = mpsc::channel(1);
    let result = a.dial(cancel_rx1, "".to_owned(), "bar".to_owned());
    assert!(result.is_err());
    if let Err(err) = result {
        assert_eq!(Error::ErrRemoteUfragEmpty, err);
    }

    let (_cancel_tx2, cancel_rx2) = mpsc::channel(1);
    let result = a.dial(cancel_rx2, "foo".to_owned(), "".to_owned());
    assert!(result.is_err());
    if let Err(err) = result {
        assert_eq!(Error::ErrRemotePwdEmpty, err);
    }

    let (cancel_tx3, cancel_rx3) = mpsc::channel(1);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100));
        drop(cancel_tx3);
    });

    let result = a.dial(cancel_rx3, "foo".to_owned(), "bar".to_owned());
    assert!(result.is_err());
    if let Err(err) = result {
        assert_eq!(Error::ErrCanceledByCaller, err);
    }

    let (_cancel_tx4, cancel_rx4) = mpsc::channel(1);
    let result = a.dial(cancel_rx4, "foo".to_owned(), "bar".to_owned());
    assert!(result.is_err());
    if let Err(err) = result {
        assert_eq!(Error::ErrMultipleStart, err);
    }

    a.close()?;

    Ok(())
}

//use std::io::Write;

// Assert that Agent emits Connecting/Connected/Disconnected/Failed/Closed messages
#[test]
fn test_connection_state_callback() -> Result<()> {
    /*env_logger::Builder::new()
    .format(|buf, record| {
        writeln!(
            buf,
            "{}:{} [{}] {} - {}",
            record.file().unwrap_or("unknown"),
            record.line().unwrap_or(0),
            record.level(),
            chrono::Local::now().format("%H:%M:%S.%6f"),
            record.args()
        )
    })
    .filter(None, LevelFilter::Trace)
    .init();*/

    let disconnected_duration = Duration::from_secs(1);
    let failed_duration = Duration::from_secs(1);
    let keepalive_interval = Duration::from_secs(0);

    let cfg0 = AgentConfig {
        urls: vec![],
        network_types: supported_network_types(),
        disconnected_timeout: Some(disconnected_duration),
        failed_timeout: Some(failed_duration),
        keepalive_interval: Some(keepalive_interval),
        ..Default::default()
    };

    let cfg1 = AgentConfig {
        urls: vec![],
        network_types: supported_network_types(),
        disconnected_timeout: Some(disconnected_duration),
        failed_timeout: Some(failed_duration),
        keepalive_interval: Some(keepalive_interval),
        ..Default::default()
    };

    let a_agent = Arc::new(Agent::new(Instant::now(), cfg0, test_crypto_provider())?);
    let b_agent = Arc::new(Agent::new(Instant::now(), cfg1, test_crypto_provider())?);

    let (is_checking_tx, mut is_checking_rx) = mpsc::channel::<()>(1);
    let (is_connected_tx, mut is_connected_rx) = mpsc::channel::<()>(1);
    let (is_disconnected_tx, mut is_disconnected_rx) = mpsc::channel::<()>(1);
    let (is_failed_tx, mut is_failed_rx) = mpsc::channel::<()>(1);
    let (is_closed_tx, mut is_closed_rx) = mpsc::channel::<()>(1);

    let is_checking_tx = Arc::new(Mutex::new(Some(is_checking_tx)));
    let is_connected_tx = Arc::new(Mutex::new(Some(is_connected_tx)));
    let is_disconnected_tx = Arc::new(Mutex::new(Some(is_disconnected_tx)));
    let is_failed_tx = Arc::new(Mutex::new(Some(is_failed_tx)));
    let is_closed_tx = Arc::new(Mutex::new(Some(is_closed_tx)));

    a_agent.on_connection_state_change(Box::new(move |c: ConnectionState| {
        let is_checking_tx_clone = Arc::clone(&is_checking_tx);
        let is_connected_tx_clone = Arc::clone(&is_connected_tx);
        let is_disconnected_tx_clone = Arc::clone(&is_disconnected_tx);
        let is_failed_tx_clone = Arc::clone(&is_failed_tx);
        let is_closed_tx_clone = Arc::clone(&is_closed_tx);
        Box::pin(async move {
            match c {
                ConnectionState::Checking => {
                    debug!("drop is_checking_tx");
                    let mut tx = is_checking_tx_clone.lock();
                    tx.take();
                }
                ConnectionState::Connected => {
                    debug!("drop is_connected_tx");
                    let mut tx = is_connected_tx_clone.lock();
                    tx.take();
                }
                ConnectionState::Disconnected => {
                    debug!("drop is_disconnected_tx");
                    let mut tx = is_disconnected_tx_clone.lock();
                    tx.take();
                }
                ConnectionState::Failed => {
                    debug!("drop is_failed_tx");
                    let mut tx = is_failed_tx_clone.lock();
                    tx.take();
                }
                ConnectionState::Closed => {
                    debug!("drop is_closed_tx");
                    let mut tx = is_closed_tx_clone.lock();
                    tx.take();
                }
                _ => {}
            };
        })
    }));

    connect_with_vnet(&a_agent, &b_agent)?;

    debug!("wait is_checking_tx");
    let _ = is_checking_rx.recv();
    debug!("wait is_connected_rx");
    let _ = is_connected_rx.recv();
    debug!("wait is_disconnected_rx");
    let _ = is_disconnected_rx.recv();
    debug!("wait is_failed_rx");
    let _ = is_failed_rx.recv();

    a_agent.close()?;
    b_agent.close()?;

    debug!("wait is_closed_rx");
    let _ = is_closed_rx.recv();

    Ok(())
}

#[test]
fn test_invalid_gather() -> Result<()> {
    //"Gather with no OnCandidate should error"
    let a = Agent::new(Instant::now(), AgentConfig::default())?;

    if let Err(err) = a.gather_candidates() {
        assert_eq!(
            Error::ErrNoOnCandidateHandler,
            err,
            "trickle GatherCandidates succeeded without OnCandidate"
        );
    }

    a.close()?;

    Ok(())
}

#[test]
fn test_candidate_pair_stats() -> Result<()> {
    let a = Agent::new(Instant::now(), AgentConfig::default())?;

    let host_local: Arc<dyn Candidate + Send + Sync> = Arc::new(
        CandidateHostConfig {
            base_config: CandidateConfig {
                network: "udp".to_owned(),
                address: "192.168.1.1".to_owned(),
                port: 19216,
                component: 1,
                ..Default::default()
            },
            ..Default::default()
        }
        .new_candidate_host()?,
    );

    let relay_remote: Arc<dyn Candidate + Send + Sync> = Arc::new(
        CandidateRelayConfig {
            base_config: CandidateConfig {
                network: "udp".to_owned(),
                address: "1.2.3.4".to_owned(),
                port: 2340,
                component: 1,
                ..Default::default()
            },
            rel_addr: "4.3.2.1".to_owned(),
            rel_port: 43210,
            ..Default::default()
        }
        .new_candidate_relay()?,
    );

    let srflx_remote: Arc<dyn Candidate + Send + Sync> = Arc::new(
        CandidateServerReflexiveConfig {
            base_config: CandidateConfig {
                network: "udp".to_owned(),
                address: "10.10.10.2".to_owned(),
                port: 19218,
                component: 1,
                ..Default::default()
            },
            rel_addr: "4.3.2.1".to_owned(),
            rel_port: 43212,
        }
        .new_candidate_server_reflexive()?,
    );

    let prflx_remote: Arc<dyn Candidate + Send + Sync> = Arc::new(
        CandidatePeerReflexiveConfig {
            base_config: CandidateConfig {
                network: "udp".to_owned(),
                address: "10.10.10.2".to_owned(),
                port: 19217,
                component: 1,
                ..Default::default()
            },
            rel_addr: "4.3.2.1".to_owned(),
            rel_port: 43211,
        }
        .new_candidate_peer_reflexive()?,
    );

    let host_remote: Arc<dyn Candidate + Send + Sync> = Arc::new(
        CandidateHostConfig {
            base_config: CandidateConfig {
                network: "udp".to_owned(),
                address: "1.2.3.5".to_owned(),
                port: 12350,
                component: 1,
                ..Default::default()
            },
            ..Default::default()
        }
        .new_candidate_host()?,
    );

    for remote in &[
        Arc::clone(&relay_remote),
        Arc::clone(&srflx_remote),
        Arc::clone(&prflx_remote),
        Arc::clone(&host_remote),
    ] {
        let p = a.internal.find_pair(&host_local, remote);

        if p.is_none() {
            a.internal
                .add_pair(Arc::clone(&host_local), Arc::clone(remote));
        }
    }

    {
        if let Some(p) = a.internal.find_pair(&host_local, &prflx_remote) {
            p.state
                .store(CandidatePairState::Failed as u8, Ordering::SeqCst);
        }
    }

    let stats = a.get_candidate_pairs_stats();
    assert_eq!(stats.len(), 4, "expected 4 candidate pairs stats");

    let (mut relay_pair_stat, mut srflx_pair_stat, mut prflx_pair_stat, mut host_pair_stat) = (
        CandidatePairStats::default(),
        CandidatePairStats::default(),
        CandidatePairStats::default(),
        CandidatePairStats::default(),
    );

    for cps in stats {
        assert_eq!(
            cps.local_candidate_id,
            host_local.id(),
            "invalid local candidate id"
        );

        if cps.remote_candidate_id == relay_remote.id() {
            relay_pair_stat = cps;
        } else if cps.remote_candidate_id == srflx_remote.id() {
            srflx_pair_stat = cps;
        } else if cps.remote_candidate_id == prflx_remote.id() {
            prflx_pair_stat = cps;
        } else if cps.remote_candidate_id == host_remote.id() {
            host_pair_stat = cps;
        } else {
            panic!("invalid remote candidate ID");
        }
    }

    assert_eq!(
        relay_pair_stat.remote_candidate_id,
        relay_remote.id(),
        "missing host-relay pair stat"
    );
    assert_eq!(
        srflx_pair_stat.remote_candidate_id,
        srflx_remote.id(),
        "missing host-srflx pair stat"
    );
    assert_eq!(
        prflx_pair_stat.remote_candidate_id,
        prflx_remote.id(),
        "missing host-prflx pair stat"
    );
    assert_eq!(
        host_pair_stat.remote_candidate_id,
        host_remote.id(),
        "missing host-host pair stat"
    );
    assert_eq!(
        prflx_pair_stat.state,
        CandidatePairState::Failed,
        "expected host-prfflx pair to have state failed, it has state {} instead",
        prflx_pair_stat.state
    );

    a.close()?;

    Ok(())
}

#[test]
fn test_local_candidate_stats() -> Result<()> {
    let a = Agent::new(Instant::now(), AgentConfig::default())?;

    let host_local: Arc<dyn Candidate + Send + Sync> = Arc::new(
        CandidateHostConfig {
            base_config: CandidateConfig {
                network: "udp".to_owned(),
                address: "192.168.1.1".to_owned(),
                port: 19216,
                component: 1,
                ..Default::default()
            },
            ..Default::default()
        }
        .new_candidate_host()?,
    );

    let srflx_local: Arc<dyn Candidate + Send + Sync> = Arc::new(
        CandidateServerReflexiveConfig {
            base_config: CandidateConfig {
                network: "udp".to_owned(),
                address: "192.168.1.1".to_owned(),
                port: 19217,
                component: 1,
                ..Default::default()
            },
            rel_addr: "4.3.2.1".to_owned(),
            rel_port: 43212,
        }
        .new_candidate_server_reflexive()?,
    );

    {
        let mut local_candidates = a.internal.local_candidates.lock();
        local_candidates.insert(
            NetworkType::Udp4,
            vec![Arc::clone(&host_local), Arc::clone(&srflx_local)],
        );
    }

    let local_stats = a.get_local_candidates_stats();
    assert_eq!(
        local_stats.len(),
        2,
        "expected 2 local candidates stats, got {} instead",
        local_stats.len()
    );

    let (mut host_local_stat, mut srflx_local_stat) =
        (CandidateStats::default(), CandidateStats::default());
    for stats in local_stats {
        let candidate = if stats.id == host_local.id() {
            host_local_stat = stats.clone();
            Arc::clone(&host_local)
        } else if stats.id == srflx_local.id() {
            srflx_local_stat = stats.clone();
            Arc::clone(&srflx_local)
        } else {
            panic!("invalid local candidate ID");
        };

        assert_eq!(
            stats.candidate_type,
            candidate.candidate_type(),
            "invalid stats CandidateType"
        );
        assert_eq!(
            stats.priority,
            candidate.priority(),
            "invalid stats CandidateType"
        );
        assert_eq!(stats.ip, candidate.address(), "invalid stats IP");
    }

    assert_eq!(
        host_local_stat.id,
        host_local.id(),
        "missing host local stat"
    );
    assert_eq!(
        srflx_local_stat.id,
        srflx_local.id(),
        "missing srflx local stat"
    );

    a.close()?;

    Ok(())
}

#[test]
fn test_remote_candidate_stats() -> Result<()> {
    let a = Agent::new(Instant::now(), AgentConfig::default())?;

    let relay_remote: Arc<dyn Candidate + Send + Sync> = Arc::new(
        CandidateRelayConfig {
            base_config: CandidateConfig {
                network: "udp".to_owned(),
                address: "1.2.3.4".to_owned(),
                port: 12340,
                component: 1,
                ..Default::default()
            },
            rel_addr: "4.3.2.1".to_owned(),
            rel_port: 43210,
            ..Default::default()
        }
        .new_candidate_relay()?,
    );

    let srflx_remote: Arc<dyn Candidate + Send + Sync> = Arc::new(
        CandidateServerReflexiveConfig {
            base_config: CandidateConfig {
                network: "udp".to_owned(),
                address: "10.10.10.2".to_owned(),
                port: 19218,
                component: 1,
                ..Default::default()
            },
            rel_addr: "4.3.2.1".to_owned(),
            rel_port: 43212,
        }
        .new_candidate_server_reflexive()?,
    );

    let prflx_remote: Arc<dyn Candidate + Send + Sync> = Arc::new(
        CandidatePeerReflexiveConfig {
            base_config: CandidateConfig {
                network: "udp".to_owned(),
                address: "10.10.10.2".to_owned(),
                port: 19217,
                component: 1,
                ..Default::default()
            },
            rel_addr: "4.3.2.1".to_owned(),
            rel_port: 43211,
        }
        .new_candidate_peer_reflexive()?,
    );

    let host_remote: Arc<dyn Candidate + Send + Sync> = Arc::new(
        CandidateHostConfig {
            base_config: CandidateConfig {
                network: "udp".to_owned(),
                address: "1.2.3.5".to_owned(),
                port: 12350,
                component: 1,
                ..Default::default()
            },
            ..Default::default()
        }
        .new_candidate_host()?,
    );

    {
        let mut remote_candidates = a.internal.remote_candidates.lock();
        remote_candidates.insert(
            NetworkType::Udp4,
            vec![
                Arc::clone(&relay_remote),
                Arc::clone(&srflx_remote),
                Arc::clone(&prflx_remote),
                Arc::clone(&host_remote),
            ],
        );
    }

    let remote_stats = a.get_remote_candidates_stats();
    assert_eq!(
        remote_stats.len(),
        4,
        "expected 4 remote candidates stats, got {} instead",
        remote_stats.len()
    );

    let (mut relay_remote_stat, mut srflx_remote_stat, mut prflx_remote_stat, mut host_remote_stat) = (
        CandidateStats::default(),
        CandidateStats::default(),
        CandidateStats::default(),
        CandidateStats::default(),
    );
    for stats in remote_stats {
        let candidate = if stats.id == relay_remote.id() {
            relay_remote_stat = stats.clone();
            Arc::clone(&relay_remote)
        } else if stats.id == srflx_remote.id() {
            srflx_remote_stat = stats.clone();
            Arc::clone(&srflx_remote)
        } else if stats.id == prflx_remote.id() {
            prflx_remote_stat = stats.clone();
            Arc::clone(&prflx_remote)
        } else if stats.id == host_remote.id() {
            host_remote_stat = stats.clone();
            Arc::clone(&host_remote)
        } else {
            panic!("invalid remote candidate ID");
        };

        assert_eq!(
            stats.candidate_type,
            candidate.candidate_type(),
            "invalid stats CandidateType"
        );
        assert_eq!(
            stats.priority,
            candidate.priority(),
            "invalid stats CandidateType"
        );
        assert_eq!(stats.ip, candidate.address(), "invalid stats IP");
    }

    assert_eq!(
        relay_remote_stat.id,
        relay_remote.id(),
        "missing relay remote stat"
    );
    assert_eq!(
        srflx_remote_stat.id,
        srflx_remote.id(),
        "missing srflx remote stat"
    );
    assert_eq!(
        prflx_remote_stat.id,
        prflx_remote.id(),
        "missing prflx remote stat"
    );
    assert_eq!(
        host_remote_stat.id,
        host_remote.id(),
        "missing host remote stat"
    );

    a.close()?;

    Ok(())
}

#[test]
fn test_binding_request_timeout() -> Result<()> {
    const EXPECTED_REMOVAL_COUNT: usize = 2;

    let a = Agent::new(Instant::now(), AgentConfig::default())?;

    let now = Instant::now();
    {
        {
            let mut pending_binding_requests = a.internal.pending_binding_requests.lock();
            pending_binding_requests.push(BindingRequest {
                timestamp: now, // valid
                ..Default::default()
            });
            pending_binding_requests.push(BindingRequest {
                timestamp: now.sub(Duration::from_millis(3900)), // valid
                ..Default::default()
            });
            pending_binding_requests.push(BindingRequest {
                timestamp: now.sub(Duration::from_millis(4100)), // invalid
                ..Default::default()
            });
            pending_binding_requests.push(BindingRequest {
                timestamp: now.sub(Duration::from_secs(75)), // invalid
                ..Default::default()
            });
        }

        a.internal.invalidate_pending_binding_requests(now);
        {
            let pending_binding_requests = a.internal.pending_binding_requests.lock();
            assert_eq!(pending_binding_requests.len(), EXPECTED_REMOVAL_COUNT, "Binding invalidation due to timeout did not remove the correct number of binding requests")
        }
    }

    a.close()?;

    Ok(())
}

// test_agent_credentials checks if local username fragments and passwords (if set) meet RFC standard
// and ensure it's backwards compatible with previous versions of the rtc/ice
#[test]
fn test_agent_credentials() -> Result<()> {
    // Agent should not require any of the usernames and password to be set
    // If set, they should follow the default 16/128 bits random number generator strategy

    let a = Agent::new(Instant::now(), AgentConfig::default())?;
    {
        let ufrag_pwd = a.internal.ufrag_pwd.lock();
        assert!(ufrag_pwd.local_ufrag.as_bytes().len() * 8 >= 24);
        assert!(ufrag_pwd.local_pwd.as_bytes().len() * 8 >= 128);
    }
    a.close()?;

    // Should honor RFC standards
    // Local values MUST be unguessable, with at least 128 bits of
    // random number generator output used to generate the password, and
    // at least 24 bits of output to generate the username fragment.

    if let Err(err) = Agent::new(Instant::now(), AgentConfig {
        local_ufrag: "xx".to_owned(),
        ..Default::default()
    }) {
        assert_eq!(Error::ErrLocalUfragInsufficientBits, err);
    } else {
        panic!("expected error, but got ok");
    }

    if let Err(err) = Agent::new(Instant::now(), AgentConfig {
        local_pwd: "xxxxxx".to_owned(),
        ..Default::default()
    }) {
        assert_eq!(Error::ErrLocalPwdInsufficientBits, err);
    } else {
        panic!("expected error, but got ok");
    }

    Ok(())
}

// Assert that Agent on Failure deletes all existing candidates
// User can then do an ICE Restart to bring agent back
#[test]
fn test_connection_state_failed_delete_all_candidates() -> Result<()> {
    let one_second = Duration::from_secs(1);
    let keepalive_interval = Duration::from_secs(0);

    let cfg0 = AgentConfig {
        network_types: supported_network_types(),
        disconnected_timeout: Some(one_second),
        failed_timeout: Some(one_second),
        keepalive_interval: Some(keepalive_interval),
        ..Default::default()
    };
    let cfg1 = AgentConfig {
        network_types: supported_network_types(),
        disconnected_timeout: Some(one_second),
        failed_timeout: Some(one_second),
        keepalive_interval: Some(keepalive_interval),
        ..Default::default()
    };

    let a_agent = Arc::new(Agent::new(Instant::now(), cfg0, test_crypto_provider())?);
    let b_agent = Arc::new(Agent::new(Instant::now(), cfg1, test_crypto_provider())?);

    let (is_failed_tx, mut is_failed_rx) = mpsc::channel::<()>(1);
    let is_failed_tx = Arc::new(Mutex::new(Some(is_failed_tx)));
    a_agent.on_connection_state_change(Box::new(move |c: ConnectionState| {
        let is_failed_tx_clone = Arc::clone(&is_failed_tx);
        Box::pin(async move {
            if c == ConnectionState::Failed {
                let mut tx = is_failed_tx_clone.lock();
                tx.take();
            }
        })
    }));

    connect_with_vnet(&a_agent, &b_agent)?;
    let _ = is_failed_rx.recv();

    {
        {
            let remote_candidates = a_agent.internal.remote_candidates.lock();
            assert_eq!(remote_candidates.len(), 0);
        }
        {
            let local_candidates = a_agent.internal.local_candidates.lock();
            assert_eq!(local_candidates.len(), 0);
        }
    }

    a_agent.close()?;
    b_agent.close()?;

    Ok(())
}

// Assert that the ICE Agent can go directly from Connecting -> Failed on both sides
#[test]
fn test_connection_state_connecting_to_failed() -> Result<()> {
    let one_second = Duration::from_secs(1);
    let keepalive_interval = Duration::from_secs(0);

    let cfg0 = AgentConfig {
        disconnected_timeout: Some(one_second),
        failed_timeout: Some(one_second),
        keepalive_interval: Some(keepalive_interval),
        ..Default::default()
    };
    let cfg1 = AgentConfig {
        disconnected_timeout: Some(one_second),
        failed_timeout: Some(one_second),
        keepalive_interval: Some(keepalive_interval),
        ..Default::default()
    };

    let a_agent = Arc::new(Agent::new(Instant::now(), cfg0, test_crypto_provider())?);
    let b_agent = Arc::new(Agent::new(Instant::now(), cfg1, test_crypto_provider())?);

    let is_failed = WaitGroup::new();
    let is_checking = WaitGroup::new();

    let connection_state_check = move |wf: Worker, wc: Worker| {
        let wf = Arc::new(Mutex::new(Some(wf)));
        let wc = Arc::new(Mutex::new(Some(wc)));
        let hdlr_fn: OnConnectionStateChangeHdlrFn = Box::new(move |c: ConnectionState| {
            let wf_clone = Arc::clone(&wf);
            let wc_clone = Arc::clone(&wc);
            Box::pin(async move {
                if c == ConnectionState::Failed {
                    let mut f = wf_clone.lock();
                    f.take();
                } else if c == ConnectionState::Checking {
                    let mut c = wc_clone.lock();
                    c.take();
                } else if c == ConnectionState::Connected || c == ConnectionState::Completed {
                    panic!("Unexpected ConnectionState: {c}");
                }
            })
        });
        hdlr_fn
    };

    let (wf1, wc1) = (is_failed.worker(), is_checking.worker());
    a_agent.on_connection_state_change(connection_state_check(wf1, wc1));

    let (wf2, wc2) = (is_failed.worker(), is_checking.worker());
    b_agent.on_connection_state_change(connection_state_check(wf2, wc2));

    let agent_a = Arc::clone(&a_agent);
    tokio::spawn(async move {
        let (_cancel_tx, cancel_rx) = mpsc::channel(1);
        let result = agent_a.accept(cancel_rx, "InvalidFrag".to_owned(), "InvalidPwd".to_owned());
        assert!(result.is_err());
    });

    let agent_b = Arc::clone(&b_agent);
    tokio::spawn(async move {
        let (_cancel_tx, cancel_rx) = mpsc::channel(1);
        let result = agent_b.dial(cancel_rx, "InvalidFrag".to_owned(), "InvalidPwd".to_owned());
        assert!(result.is_err());
    });

    is_checking.wait();
    is_failed.wait();

    a_agent.close()?;
    b_agent.close()?;

    Ok(())
}

#[test]
fn test_agent_restart_during_gather() -> Result<()> {
    //"Restart During Gather"

    let agent = Agent::new(Instant::now(), AgentConfig::default())?;

    agent
        .gathering_state
        .store(GatheringState::Gathering as u8, Ordering::SeqCst);

    if let Err(err) = agent.restart(Instant::now(), "".to_owned(), "".to_owned()) {
        assert_eq!(Error::ErrRestartWhenGathering, err);
    } else {
        panic!("expected error, but got ok");
    }

    agent.close()?;

    Ok(())
}

#[test]
fn test_agent_restart_when_closed() -> Result<()> {
    //"Restart When Closed"

    let agent = Agent::new(Instant::now(), AgentConfig::default())?;
    agent.close()?;

    if let Err(err) = agent.restart(Instant::now(), "".to_owned(), "".to_owned()) {
        assert_eq!(Error::ErrClosed, err);
    } else {
        panic!("expected error, but got ok");
    }

    Ok(())
}

#[test]
fn test_agent_restart_one_side() -> Result<()> {
    let one_second = Duration::from_secs(1);

    //"Restart One Side"
    let (_, _, agent_a, agent_b) = pipe(
        Some(AgentConfig {
            disconnected_timeout: Some(one_second),
            failed_timeout: Some(one_second),
            ..Default::default()
        }),
        Some(AgentConfig {
            disconnected_timeout: Some(one_second),
            failed_timeout: Some(one_second),
            ..Default::default()
        }),
    )?;

    let (cancel_tx, mut cancel_rx) = mpsc::channel::<()>(1);
    let cancel_tx = Arc::new(Mutex::new(Some(cancel_tx)));
    agent_b.on_connection_state_change(Box::new(move |c: ConnectionState| {
        let cancel_tx_clone = Arc::clone(&cancel_tx);
        Box::pin(async move {
            if c == ConnectionState::Failed || c == ConnectionState::Disconnected {
                let mut tx = cancel_tx_clone.lock();
                tx.take();
            }
        })
    }));

    agent_a.restart(Instant::now(), "".to_owned(), "".to_owned())?;

    let _ = cancel_rx.recv();

    agent_a.close()?;
    agent_b.close()?;

    Ok(())
}

#[test]
fn test_agent_restart_both_side() -> Result<()> {
    let one_second = Duration::from_secs(1);
    //"Restart Both Sides"

    // Get all addresses of candidates concatenated
    let generate_candidate_address_strings =
        |res: Result<Vec<Arc<dyn Candidate + Send + Sync>>>| -> String {
            assert!(res.is_ok());

            let mut out = String::new();
            if let Ok(candidates) = res {
                for c in candidates {
                    out += c.address().as_str();
                    out += ":";
                    out += c.port().to_string().as_str();
                }
            }
            out
        };

    // Store the original candidates, confirm that after we reconnect we have new pairs
    let (_, _, agent_a, agent_b) = pipe(
        Some(AgentConfig {
            disconnected_timeout: Some(one_second),
            failed_timeout: Some(one_second),
            ..Default::default()
        }),
        Some(AgentConfig {
            disconnected_timeout: Some(one_second),
            failed_timeout: Some(one_second),
            ..Default::default()
        }),
    )?;

    let conn_afirst_candidates = generate_candidate_address_strings(agent_a.get_local_candidates());
    let conn_bfirst_candidates = generate_candidate_address_strings(agent_b.get_local_candidates());

    let (a_notifier, mut a_connected) = on_connected();
    agent_a.on_connection_state_change(a_notifier);

    let (b_notifier, mut b_connected) = on_connected();
    agent_b.on_connection_state_change(b_notifier);

    // Restart and Re-Signal
    agent_a.restart(Instant::now(), "".to_owned(), "".to_owned())?;
    agent_b.restart(Instant::now(), "".to_owned(), "".to_owned())?;

    // Exchange Candidates and Credentials
    let (ufrag, pwd) = agent_b.get_local_user_credentials();
    agent_a.set_remote_credentials(ufrag, pwd)?;

    let (ufrag, pwd) = agent_a.get_local_user_credentials();
    agent_b.set_remote_credentials(ufrag, pwd)?;

    gather_and_exchange_candidates(&agent_a, &agent_b)?;

    // Wait until both have gone back to connected
    let _ = a_connected.recv();
    let _ = b_connected.recv();

    // Assert that we have new candiates each time
    assert_ne!(
        conn_afirst_candidates,
        generate_candidate_address_strings(agent_a.get_local_candidates())
    );
    assert_ne!(
        conn_bfirst_candidates,
        generate_candidate_address_strings(agent_b.get_local_candidates())
    );

    agent_a.close()?;
    agent_b.close()?;

    Ok(())
}

#[test]
fn test_get_remote_credentials() -> Result<()> {
    let a = Agent::new(Instant::now(), AgentConfig::default())?;

    let (remote_ufrag, remote_pwd) = {
        let mut ufrag_pwd = a.internal.ufrag_pwd.lock();
        ufrag_pwd.remote_ufrag = "remoteUfrag".to_owned();
        ufrag_pwd.remote_pwd = "remotePwd".to_owned();
        (
            ufrag_pwd.remote_ufrag.to_owned(),
            ufrag_pwd.remote_pwd.to_owned(),
        )
    };

    let (actual_ufrag, actual_pwd) = a.get_remote_user_credentials();

    assert_eq!(actual_ufrag, remote_ufrag);
    assert_eq!(actual_pwd, remote_pwd);

    a.close()?;

    Ok(())
}

#[test]
fn test_close_in_connection_state_callback() -> Result<()> {
    let disconnected_duration = Duration::from_secs(1);
    let failed_duration = Duration::from_secs(1);
    let keepalive_interval = Duration::from_secs(0);

    let cfg0 = AgentConfig {
        urls: vec![],
        network_types: supported_network_types(),
        disconnected_timeout: Some(disconnected_duration),
        failed_timeout: Some(failed_duration),
        keepalive_interval: Some(keepalive_interval),
        check_interval: Duration::from_millis(500),
        ..Default::default()
    };

    let cfg1 = AgentConfig {
        urls: vec![],
        network_types: supported_network_types(),
        disconnected_timeout: Some(disconnected_duration),
        failed_timeout: Some(failed_duration),
        keepalive_interval: Some(keepalive_interval),
        check_interval: Duration::from_millis(500),
        ..Default::default()
    };

    let a_agent = Arc::new(Agent::new(Instant::now(), cfg0, test_crypto_provider())?);
    let b_agent = Arc::new(Agent::new(Instant::now(), cfg1, test_crypto_provider())?);

    let (is_closed_tx, mut is_closed_rx) = mpsc::channel::<()>(1);
    let (is_connected_tx, mut is_connected_rx) = mpsc::channel::<()>(1);
    let is_closed_tx = Arc::new(Mutex::new(Some(is_closed_tx)));
    let is_connected_tx = Arc::new(Mutex::new(Some(is_connected_tx)));
    a_agent.on_connection_state_change(Box::new(move |c: ConnectionState| {
        let is_closed_tx_clone = Arc::clone(&is_closed_tx);
        let is_connected_tx_clone = Arc::clone(&is_connected_tx);
        Box::pin(async move {
            if c == ConnectionState::Connected {
                let mut tx = is_connected_tx_clone.lock();
                tx.take();
            } else if c == ConnectionState::Closed {
                let mut tx = is_closed_tx_clone.lock();
                tx.take();
            }
        })
    }));

    connect_with_vnet(&a_agent, &b_agent)?;

    let _ = is_connected_rx.recv();
    a_agent.close()?;

    let _ = is_closed_rx.recv();
    b_agent.close()?;

    Ok(())
}

#[test]
fn test_run_task_in_connection_state_callback() -> Result<()> {
    let one_second = Duration::from_secs(1);
    let keepalive_interval = Duration::from_secs(0);

    let cfg0 = AgentConfig {
        urls: vec![],
        network_types: supported_network_types(),
        disconnected_timeout: Some(one_second),
        failed_timeout: Some(one_second),
        keepalive_interval: Some(keepalive_interval),
        check_interval: Duration::from_millis(50),
        ..Default::default()
    };

    let cfg1 = AgentConfig {
        urls: vec![],
        network_types: supported_network_types(),
        disconnected_timeout: Some(one_second),
        failed_timeout: Some(one_second),
        keepalive_interval: Some(keepalive_interval),
        check_interval: Duration::from_millis(50),
        ..Default::default()
    };

    let a_agent = Arc::new(Agent::new(Instant::now(), cfg0, test_crypto_provider())?);
    let b_agent = Arc::new(Agent::new(Instant::now(), cfg1, test_crypto_provider())?);

    let (is_complete_tx, mut is_complete_rx) = mpsc::channel::<()>(1);
    let is_complete_tx = Arc::new(Mutex::new(Some(is_complete_tx)));
    a_agent.on_connection_state_change(Box::new(move |c: ConnectionState| {
        let is_complete_tx_clone = Arc::clone(&is_complete_tx);
        Box::pin(async move {
            if c == ConnectionState::Connected {
                let mut tx = is_complete_tx_clone.lock();
                tx.take();
            }
        })
    }));

    connect_with_vnet(&a_agent, &b_agent)?;

    let _ = is_complete_rx.recv();
    let _ = a_agent.get_local_user_credentials();
    a_agent.restart(Instant::now(), "".to_owned(), "".to_owned())?;

    a_agent.close()?;
    b_agent.close()?;

    Ok(())
}

#[test]
fn test_run_task_in_selected_candidate_pair_change_callback() -> Result<()> {
    let one_second = Duration::from_secs(1);
    let keepalive_interval = Duration::from_secs(0);

    let cfg0 = AgentConfig {
        urls: vec![],
        network_types: supported_network_types(),
        disconnected_timeout: Some(one_second),
        failed_timeout: Some(one_second),
        keepalive_interval: Some(keepalive_interval),
        check_interval: Duration::from_millis(50),
        ..Default::default()
    };

    let cfg1 = AgentConfig {
        urls: vec![],
        network_types: supported_network_types(),
        disconnected_timeout: Some(one_second),
        failed_timeout: Some(one_second),
        keepalive_interval: Some(keepalive_interval),
        check_interval: Duration::from_millis(50),
        ..Default::default()
    };

    let a_agent = Arc::new(Agent::new(Instant::now(), cfg0, test_crypto_provider())?);
    let b_agent = Arc::new(Agent::new(Instant::now(), cfg1, test_crypto_provider())?);

    let (is_tested_tx, mut is_tested_rx) = mpsc::channel::<()>(1);
    let is_tested_tx = Arc::new(Mutex::new(Some(is_tested_tx)));
    a_agent.on_selected_candidate_pair_change(Box::new(
        move |_: &Arc<dyn Candidate + Send + Sync>, _: &Arc<dyn Candidate + Send + Sync>| {
            let is_tested_tx_clone = Arc::clone(&is_tested_tx);
            Box::pin(async move {
                let mut tx = is_tested_tx_clone.lock();
                tx.take();
            })
        },
    ));

    let (is_complete_tx, mut is_complete_rx) = mpsc::channel::<()>(1);
    let is_complete_tx = Arc::new(Mutex::new(Some(is_complete_tx)));
    a_agent.on_connection_state_change(Box::new(move |c: ConnectionState| {
        let is_complete_tx_clone = Arc::clone(&is_complete_tx);
        Box::pin(async move {
            if c == ConnectionState::Connected {
                let mut tx = is_complete_tx_clone.lock();
                tx.take();
            }
        })
    }));

    connect_with_vnet(&a_agent, &b_agent)?;

    let _ = is_complete_rx.recv();
    let _ = is_tested_rx.recv();

    let _ = a_agent.get_local_user_credentials();

    a_agent.close()?;
    b_agent.close()?;

    Ok(())
}

// Assert that a Lite agent goes to disconnected and failed
#[test]
fn test_lite_lifecycle() -> Result<()> {
    let (a_notifier, mut a_connected_rx) = on_connected();

    let a_agent = Arc::new(Agent::new(Instant::now(), AgentConfig {
        network_types: supported_network_types(),
        ..Default::default()
    })?);

    a_agent.on_connection_state_change(a_notifier);

    let disconnected_duration = Duration::from_secs(1);
    let failed_duration = Duration::from_secs(1);
    let keepalive_interval = Duration::from_secs(0);

    let b_agent = Arc::new(Agent::new(Instant::now(), AgentConfig {
        lite: true,
        candidate_types: vec![CandidateType::Host],
        network_types: supported_network_types(),
        disconnected_timeout: Some(disconnected_duration),
        failed_timeout: Some(failed_duration),
        keepalive_interval: Some(keepalive_interval),
        check_interval: Duration::from_millis(500),
        ..Default::default()
    })?);

    let (b_connected_tx, mut b_connected_rx) = mpsc::channel::<()>(1);
    let (b_disconnected_tx, mut b_disconnected_rx) = mpsc::channel::<()>(1);
    let (b_failed_tx, mut b_failed_rx) = mpsc::channel::<()>(1);
    let b_connected_tx = Arc::new(Mutex::new(Some(b_connected_tx)));
    let b_disconnected_tx = Arc::new(Mutex::new(Some(b_disconnected_tx)));
    let b_failed_tx = Arc::new(Mutex::new(Some(b_failed_tx)));

    b_agent.on_connection_state_change(Box::new(move |c: ConnectionState| {
        let b_connected_tx_clone = Arc::clone(&b_connected_tx);
        let b_disconnected_tx_clone = Arc::clone(&b_disconnected_tx);
        let b_failed_tx_clone = Arc::clone(&b_failed_tx);

        Box::pin(async move {
            if c == ConnectionState::Connected {
                let mut tx = b_connected_tx_clone.lock();
                tx.take();
            } else if c == ConnectionState::Disconnected {
                let mut tx = b_disconnected_tx_clone.lock();
                tx.take();
            } else if c == ConnectionState::Failed {
                let mut tx = b_failed_tx_clone.lock();
                tx.take();
            }
        })
    }));

    connect_with_vnet(&b_agent, &a_agent)?;

    let _ = a_connected_rx.recv();
    let _ = b_connected_rx.recv();
    a_agent.close()?;

    let _ = b_disconnected_rx.recv();
    let _ = b_failed_rx.recv();

    b_agent.close()?;

    Ok(())
}
*/

#[test]
fn test_zero_failed_timeout_keeps_checking_agent_recoverable() -> Result<()> {
    let base = Instant::now();
    let mut agent = Agent::new(
        base,
        Arc::new(AgentConfig {
            disconnected_timeout: Some(Duration::from_secs(5)),
            failed_timeout: Some(Duration::ZERO),
            ..Default::default()
        }),
        test_crypto_provider(),
    )?;

    agent.connection_state = ConnectionState::Checking;
    agent.last_connection_state = ConnectionState::Checking;
    agent.checking_duration = base;
    agent.contact(base + Duration::from_secs(3600));

    assert_eq!(
        agent.connection_state,
        ConnectionState::Checking,
        "a zero failed timeout is documented to disable the terminal transition"
    );
    Ok(())
}

/// Test role conflict when both agents are controlling
/// RFC 8445 Section 7.3.1.1 - Agent with smaller tiebreaker should switch to controlled
#[test]
fn test_role_conflict_both_controlling_smaller_tiebreaker_switches() -> Result<()> {
    // Create agent with controlling role
    let mut config = AgentConfig::default();
    config.is_controlling = true;
    let mut agent = Agent::new(Instant::now(), Arc::new(config), test_crypto_provider())?;

    // Set a specific tiebreaker value
    agent.tie_breaker = 100;

    // Add local candidate
    let host_config = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "192.168.0.2".to_owned(),
            port: 7777,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let local_candidate = host_config.new_candidate_host()?;
    let local_index = 0;
    agent.add_local_candidate(local_candidate)?;

    // Add remote candidate
    let remote_addr = SocketAddr::from_str("172.17.0.3:9999")?;
    let remote_host_config = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "172.17.0.3".to_owned(),
            port: 9999,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let remote_candidate = remote_host_config.new_candidate_host()?;
    agent.add_remote_candidate(remote_candidate)?;

    // Set remote credentials
    agent.ufrag_pwd.remote_credentials = Some(Credentials {
        ufrag: "remote_ufrag".to_string(),
        pwd: "remote_pwd".to_string(),
    });

    let username = agent.ufrag_pwd.local_credentials.ufrag.clone() + ":remote_ufrag";
    let local_pwd = agent.ufrag_pwd.local_credentials.pwd.clone();

    // Create STUN binding request from remote agent (also controlling, with larger tiebreaker)
    let remote_tiebreaker = 200; // Larger than ours (100)
    let mut msg = Message::new();
    msg.build(&[
        Box::new(BINDING_REQUEST),
        Box::new(TransactionId::new()),
        Box::new(Username::new(ATTR_USERNAME, username)),
        Box::new(AttrControlling(remote_tiebreaker)), // Remote is also controlling
        Box::new(PriorityAttr(1000)),
        Box::new(MessageIntegrity::new_short_term_integrity_with_provider(
            local_pwd,
            test_crypto_provider().crypto(),
        )),
        Box::new(FINGERPRINT),
    ])?;

    // Agent should be controlling before handling the message
    assert!(
        agent.is_controlling,
        "Agent should be controlling before role conflict"
    );

    // Store initial pair count and priorities
    let initial_pair_count = agent.candidate_pairs.len();
    let initial_priorities: Vec<u64> = agent.candidate_pairs.iter().map(|p| p.priority()).collect();

    // Handle the inbound message - should detect role conflict
    let result = agent.handle_inbound(Instant::now(), &mut msg, local_index, remote_addr);

    // The message should be processed (role conflict handling doesn't stop processing)
    // It may return an error for other reasons, but not for role conflict itself
    let _ = result; // We don't care about the exact error, just that role switching happened

    // Agent should have switched to controlled (because our tiebreaker 100 < 200)
    assert!(
        !agent.is_controlling,
        "Agent should have switched to controlled role (smaller tiebreaker)"
    );

    // Verify that candidate pair priorities were recomputed
    if !agent.candidate_pairs.is_empty() {
        // Check that ice_role_controlling was updated in pairs
        for pair in &agent.candidate_pairs {
            assert!(
                !pair.ice_role_controlling,
                "Candidate pair should have controlling role set to false"
            );
        }

        // Priorities should change when role changes (unless there were no pairs initially)
        if initial_pair_count > 0 && initial_priorities.len() > 0 {
            // At least verify the role was updated in pairs
            assert_eq!(
                agent.candidate_pairs.len(),
                initial_pair_count,
                "Number of pairs should remain the same"
            );
        }
    }

    // Verify nominated pair was cleared
    assert!(
        agent.nominated_pair.is_none(),
        "Nominated pair should be cleared after role switch"
    );

    agent.close()?;
    Ok(())
}

/// Test role conflict when both agents are controlling
/// RFC 8445 Section 7.3.1.1 - Agent with larger tiebreaker should keep controlling role
#[test]
fn test_role_conflict_both_controlling_larger_tiebreaker_stays() -> Result<()> {
    // Create agent with controlling role
    let mut config = AgentConfig::default();
    config.is_controlling = true;
    let mut agent = Agent::new(Instant::now(), Arc::new(config), test_crypto_provider())?;

    // Set a larger tiebreaker value
    agent.tie_breaker = 500;

    // Add local candidate
    let host_config = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "192.168.0.2".to_owned(),
            port: 7777,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let local_candidate = host_config.new_candidate_host()?;
    let local_index = 0;
    agent.add_local_candidate(local_candidate)?;

    // Add remote candidate
    let remote_addr = SocketAddr::from_str("172.17.0.3:9999")?;
    let remote_host_config = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "172.17.0.3".to_owned(),
            port: 9999,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let remote_candidate = remote_host_config.new_candidate_host()?;
    agent.add_remote_candidate(remote_candidate)?;

    // Set remote credentials
    agent.ufrag_pwd.remote_credentials = Some(Credentials {
        ufrag: "remote_ufrag".to_string(),
        pwd: "remote_pwd".to_string(),
    });

    let username = agent.ufrag_pwd.local_credentials.ufrag.clone() + ":remote_ufrag";
    let local_pwd = agent.ufrag_pwd.local_credentials.pwd.clone();

    // Create STUN binding request from remote agent (also controlling, with smaller tiebreaker)
    let remote_tiebreaker = 100; // Smaller than ours (500)
    let mut msg = Message::new();
    msg.build(&[
        Box::new(BINDING_REQUEST),
        Box::new(TransactionId::new()),
        Box::new(Username::new(ATTR_USERNAME, username)),
        Box::new(AttrControlling(remote_tiebreaker)), // Remote is also controlling
        Box::new(PriorityAttr(1000)),
        Box::new(MessageIntegrity::new_short_term_integrity_with_provider(
            local_pwd,
            test_crypto_provider().crypto(),
        )),
        Box::new(FINGERPRINT),
    ])?;

    // Agent should be controlling before handling the message
    assert!(
        agent.is_controlling,
        "Agent should be controlling before role conflict"
    );

    // Handle the inbound message - should detect role conflict but NOT switch
    let _ = agent.handle_inbound(Instant::now(), &mut msg, local_index, remote_addr);

    // Agent should remain controlling (because our tiebreaker 500 > 100)
    assert!(
        agent.is_controlling,
        "Agent should remain controlling (larger tiebreaker)"
    );

    // Verify candidate pairs still have controlling role
    for pair in &agent.candidate_pairs {
        assert!(
            pair.ice_role_controlling,
            "Candidate pair should still have controlling role set to true"
        );
    }

    agent.close()?;
    Ok(())
}

/// Test role conflict when both agents are controlled
/// RFC 8445 Section 7.3.1.1 - Agent with larger tiebreaker should switch to controlling
#[test]
fn test_role_conflict_both_controlled_larger_tiebreaker_switches() -> Result<()> {
    use crate::attributes::control::AttrControlled;

    // Create agent with controlled role
    let mut config = AgentConfig::default();
    config.is_controlling = false; // Controlled
    let mut agent = Agent::new(Instant::now(), Arc::new(config), test_crypto_provider())?;

    // Set a larger tiebreaker value
    agent.tie_breaker = 500;

    // Add local candidate
    let host_config = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "192.168.0.2".to_owned(),
            port: 7777,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let local_candidate = host_config.new_candidate_host()?;
    let local_index = 0;
    agent.add_local_candidate(local_candidate)?;

    // Add remote candidate
    let remote_addr = SocketAddr::from_str("172.17.0.3:9999")?;
    let remote_host_config = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "172.17.0.3".to_owned(),
            port: 9999,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let remote_candidate = remote_host_config.new_candidate_host()?;
    agent.add_remote_candidate(remote_candidate)?;

    // Set remote credentials
    agent.ufrag_pwd.remote_credentials = Some(Credentials {
        ufrag: "remote_ufrag".to_string(),
        pwd: "remote_pwd".to_string(),
    });

    let username = agent.ufrag_pwd.local_credentials.ufrag.clone() + ":remote_ufrag";
    let local_pwd = agent.ufrag_pwd.local_credentials.pwd.clone();

    // Create STUN binding request from remote agent (also controlled, with smaller tiebreaker)
    let remote_tiebreaker = 100; // Smaller than ours (500)
    let mut msg = Message::new();
    msg.build(&[
        Box::new(BINDING_REQUEST),
        Box::new(TransactionId::new()),
        Box::new(Username::new(ATTR_USERNAME, username)),
        Box::new(AttrControlled(remote_tiebreaker)), // Remote is also controlled
        Box::new(PriorityAttr(1000)),
        Box::new(MessageIntegrity::new_short_term_integrity_with_provider(
            local_pwd,
            test_crypto_provider().crypto(),
        )),
        Box::new(FINGERPRINT),
    ])?;

    // Agent should be controlled before handling the message
    assert!(
        !agent.is_controlling,
        "Agent should be controlled before role conflict"
    );

    // Handle the inbound message - should detect role conflict
    let _ = agent.handle_inbound(Instant::now(), &mut msg, local_index, remote_addr);

    // Agent should have switched to controlling (because our tiebreaker 500 > 100)
    assert!(
        agent.is_controlling,
        "Agent should have switched to controlling role (larger tiebreaker)"
    );

    // Verify that candidate pair priorities were recomputed
    for pair in &agent.candidate_pairs {
        assert!(
            pair.ice_role_controlling,
            "Candidate pair should have controlling role set to true"
        );
    }

    // Verify nominated pair was cleared
    assert!(
        agent.nominated_pair.is_none(),
        "Nominated pair should be cleared after role switch"
    );

    agent.close()?;
    Ok(())
}

/// Test role conflict when both agents are controlled
/// RFC 8445 Section 7.3.1.1 - Agent with smaller tiebreaker should stay controlled
#[test]
fn test_role_conflict_both_controlled_smaller_tiebreaker_stays() -> Result<()> {
    use crate::attributes::control::AttrControlled;

    // Create agent with controlled role
    let mut config = AgentConfig::default();
    config.is_controlling = false; // Controlled
    let mut agent = Agent::new(Instant::now(), Arc::new(config), test_crypto_provider())?;

    // Set a smaller tiebreaker value
    agent.tie_breaker = 100;

    // Add local candidate
    let host_config = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "192.168.0.2".to_owned(),
            port: 7777,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let local_candidate = host_config.new_candidate_host()?;
    let local_index = 0;
    agent.add_local_candidate(local_candidate)?;

    // Add remote candidate
    let remote_addr = SocketAddr::from_str("172.17.0.3:9999")?;
    let remote_host_config = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "172.17.0.3".to_owned(),
            port: 9999,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let remote_candidate = remote_host_config.new_candidate_host()?;
    agent.add_remote_candidate(remote_candidate)?;

    // Set remote credentials
    agent.ufrag_pwd.remote_credentials = Some(Credentials {
        ufrag: "remote_ufrag".to_string(),
        pwd: "remote_pwd".to_string(),
    });

    let username = agent.ufrag_pwd.local_credentials.ufrag.clone() + ":remote_ufrag";
    let local_pwd = agent.ufrag_pwd.local_credentials.pwd.clone();

    // Create STUN binding request from remote agent (also controlled, with larger tiebreaker)
    let remote_tiebreaker = 500; // Larger than ours (100)
    let mut msg = Message::new();
    msg.build(&[
        Box::new(BINDING_REQUEST),
        Box::new(TransactionId::new()),
        Box::new(Username::new(ATTR_USERNAME, username)),
        Box::new(AttrControlled(remote_tiebreaker)), // Remote is also controlled
        Box::new(PriorityAttr(1000)),
        Box::new(MessageIntegrity::new_short_term_integrity_with_provider(
            local_pwd,
            test_crypto_provider().crypto(),
        )),
        Box::new(FINGERPRINT),
    ])?;

    // Agent should be controlled before handling the message
    assert!(
        !agent.is_controlling,
        "Agent should be controlled before role conflict"
    );

    // Handle the inbound message - should detect role conflict but NOT switch
    let _ = agent.handle_inbound(Instant::now(), &mut msg, local_index, remote_addr);

    // Agent should remain controlled (because our tiebreaker 100 < 500)
    assert!(
        !agent.is_controlling,
        "Agent should remain controlled (smaller tiebreaker)"
    );

    // Verify candidate pairs still have controlled role
    for pair in &agent.candidate_pairs {
        assert!(
            !pair.ice_role_controlling,
            "Candidate pair should still have controlling role set to false"
        );
    }

    agent.close()?;
    Ok(())
}

#[test]
fn test_candidate_type_filtering() -> Result<()> {
    // Create an agent with only Relay candidate type allowed (simulates RTCIceTransportPolicy::Relay)
    let config = Arc::new(AgentConfig {
        candidate_types: vec![CandidateType::Relay],
        ..Default::default()
    });
    let mut agent = Agent::new(Instant::now(), config, test_crypto_provider())?;

    // Host local candidate should be rejected
    let host_local = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "192.168.1.1".to_owned(),
            port: 5000,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_host()?;
    assert!(
        !agent.add_local_candidate(host_local)?,
        "Host local candidate should be filtered out"
    );

    // Remote candidates of any type should still be accepted (relay policy only filters local)
    let host_remote = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "192.168.1.3".to_owned(),
            port: 5003,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_host()?;
    assert!(
        agent.add_remote_candidate(host_remote)?,
        "Remote candidates should be accepted regardless of candidate_types"
    );

    // Relay local candidate should be accepted
    let relay_local = CandidateRelayConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "1.2.3.4".to_owned(),
            port: 5004,
            component: 1,
            ..Default::default()
        },
        rel_addr: "4.3.2.1".to_owned(),
        rel_port: 5005,
        ..Default::default()
    }
    .new_candidate_relay()?;
    assert!(
        agent.add_local_candidate(relay_local)?,
        "Relay local candidate should be accepted"
    );

    assert_eq!(
        agent.local_candidates.len(),
        1,
        "Only the relay candidate should be stored"
    );
    assert_eq!(
        agent.remote_candidates.len(),
        1,
        "Remote host candidate should be stored"
    );

    agent.close()?;
    Ok(())
}

// Verify that check_keepalive sends a STUN ping even when media traffic has
// recently updated the candidate timestamps (RFC 7675).
#[test]
fn test_keepalive_sent_during_media_flow() -> Result<()> {
    let mut a = Agent::new(
        Instant::now(),
        Arc::new(AgentConfig::default()),
        test_crypto_provider(),
    )?;

    // Set up a selected pair
    let host_local = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "192.168.1.1".to_owned(),
            port: 19216,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_host()?;
    a.add_local_candidate(host_local)?;

    let relay_remote = CandidateRelayConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "1.2.3.4".to_owned(),
            port: 12340,
            component: 1,
            ..Default::default()
        },
        rel_addr: "4.3.2.1".to_owned(),
        rel_port: 43210,
        ..Default::default()
    }
    .new_candidate_relay()?;
    a.add_remote_candidate(relay_remote)?;

    a.ufrag_pwd.remote_credentials = Some(Credentials {
        ufrag: "remoteufrag".to_string(),
        pwd: "remotepwd".to_string(),
    });
    a.is_controlling = true;

    a.add_pair(0, 0);
    a.set_selected_pair(Some(Instant::now()), Some(0));

    // Simulate recent media activity on both candidates
    a.local_candidates[0].seen(Instant::now(), true);
    a.remote_candidates[0].seen(Instant::now(), false);

    // Virtual time: the agent is told the instant, so nothing here sleeps or samples a clock.
    let base = Instant::now();
    let keepalive_interval = a.keepalive_interval;

    // Pretend the last consent ping was long ago such that the interval has elapsed
    a.last_consent_sent = base - Duration::from_secs(10);

    // Drain any events/writes from setup
    a.write_outs.clear();

    // First call should send a consent ping immediately
    a.check_keepalive(base);
    assert!(
        !a.write_outs.is_empty(),
        "check_keepalive must send a STUN ping even when media timestamps are fresh"
    );

    // Drain and call again at the *same* instant; the interval has not elapsed, so no second ping.
    // Before clock injection this relied on the wall clock not advancing between two calls.
    a.write_outs.clear();
    a.check_keepalive(base);
    assert!(
        a.write_outs.is_empty(),
        "check_keepalive must not send again before keepalive_interval elapses"
    );

    // Advance virtual time past the interval: the next ping goes out, instantly and reproducibly.
    a.check_keepalive(base + keepalive_interval + Duration::from_millis(1));
    assert!(
        !a.write_outs.is_empty(),
        "check_keepalive must send again once keepalive_interval has elapsed"
    );

    a.close()?;
    Ok(())
}

#[test]
fn test_pair_network_type_mismatch() -> Result<()> {
    let mut a = Agent::new(
        Instant::now(),
        Arc::new(AgentConfig::default()),
        test_crypto_provider(),
    )?;

    // UDP: IPv4 local should not pair with IPv6 remote.
    let local_v4 = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "192.168.1.1".to_owned(),
            port: 7777,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_host()?;
    a.add_local_candidate(local_v4)?;

    let remote_v6 = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "2001:db8::1".to_owned(),
            port: 8888,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_host()?;
    a.add_remote_candidate(remote_v6)?;

    assert!(
        a.candidate_pairs.is_empty(),
        "IPv4 local and IPv6 remote candidates should not be paired"
    );

    // UDP: matching IPv4 should pair.
    let remote_v4 = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "192.168.1.2".to_owned(),
            port: 8888,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_host()?;
    a.add_remote_candidate(remote_v4)?;

    assert_eq!(
        a.candidate_pairs.len(),
        1,
        "IPv4 local and IPv4 remote candidates should form exactly one pair"
    );

    // TCP: IPv4 active local should not pair with IPv6 passive remote.
    let local_tcp4_active = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "tcp".to_owned(),
            address: "192.168.1.3".to_owned(),
            port: 7778,
            component: 1,
            ..Default::default()
        },
        tcp_type: TcpType::Active,
    }
    .new_candidate_host()?;
    a.add_local_candidate(local_tcp4_active)?;

    let remote_tcp6_passive = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "tcp".to_owned(),
            address: "2001:db8::2".to_owned(),
            port: 8889,
            component: 1,
            ..Default::default()
        },
        tcp_type: TcpType::Passive,
    }
    .new_candidate_host()?;
    a.add_remote_candidate(remote_tcp6_passive)?;

    assert_eq!(
        a.candidate_pairs.len(),
        1,
        "TCP4 active local and TCP6 passive remote should not create an additional pair"
    );

    // TCP: matching IPv4 active/passive should pair.
    let remote_tcp4_passive = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "tcp".to_owned(),
            address: "192.168.1.4".to_owned(),
            port: 8890,
            component: 1,
            ..Default::default()
        },
        tcp_type: TcpType::Passive,
    }
    .new_candidate_host()?;
    a.add_remote_candidate(remote_tcp4_passive)?;

    assert_eq!(
        a.candidate_pairs.len(),
        2,
        "TCP4 active local and TCP4 passive remote should form exactly one pair"
    );

    a.close()?;
    Ok(())
}

// Regression test for https://github.com/webrtc-rs/rtc/issues/88 (Issue 1).
//
// When the agent transitions to `Failed` it calls `delete_all_candidates`, which
// empties `local_candidates`/`remote_candidates`. Previously the candidate pairs -
// and the `selected_pair`/`nominated_pair` indices into them - were left behind, so
// they referenced candidates that no longer existed. Any later access through a pair,
// e.g. `get_best_available_candidate_pair`, then indexed out of bounds and panicked:
//
//     thread '...' panicked at 'index out of bounds: the len is 0 but the index is 0'
#[test]
fn test_transition_to_failed_clears_stale_candidate_pairs() -> Result<()> {
    let mut a = Agent::new(
        Instant::now(),
        Arc::new(AgentConfig::default()),
        test_crypto_provider(),
    )?;

    let local = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "192.168.0.2".to_owned(),
            port: 777,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_host()?;
    a.add_local_candidate(local)?;

    let remote = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "172.17.0.3".to_owned(),
            port: 999,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_host()?;
    a.add_remote_candidate(remote)?;

    // A single local/remote pair now exists; mark it selected and nominated so that
    // both index-into-pairs fields are populated.
    assert_eq!(a.candidate_pairs.len(), 1);
    a.set_selected_pair(Some(Instant::now()), Some(0));
    a.nominated_pair = Some(0);

    // Transition to Failed: this deletes all candidates.
    a.update_connection_state(Some(Instant::now()), ConnectionState::Failed);

    // The pairs and the indices into them must be gone, otherwise they dangle.
    assert!(
        a.candidate_pairs.is_empty(),
        "candidate pairs must be cleared when candidates are deleted"
    );
    assert!(a.selected_pair.is_none(), "selected_pair must be reset");
    assert!(a.nominated_pair.is_none(), "nominated_pair must be reset");
    assert!(a.local_candidates.is_empty());
    assert!(a.remote_candidates.is_empty());

    // Before the fix these accessors dereferenced stale pair indices into the emptied
    // candidate vectors and panicked; now they must simply return `None`.
    assert!(a.get_selected_candidate_pair().is_none());
    assert!(a.get_best_available_candidate_pair().is_none());

    a.close()?;
    Ok(())
}

// Regression test for https://github.com/webrtc-rs/rtc/issues/88 (Issue 2).
//
// Handling an inbound binding request from an unknown address adds a peer-reflexive
// remote candidate. Previously `add_remote_candidate` ran the connectivity check
// inline, which could advance the state to `Failed` and clear `remote_candidates`
// mid-call. The caller then computed `remote_candidates.len() - 1` on an empty vector
// and panicked ('attempt to subtract with overflow' in debug builds). Deferring the
// check to `handle_timeout` keeps candidate mutation free of surprising re-entrant
// state changes.
#[test]
fn test_handle_inbound_request_defers_failing_connectivity_check() -> Result<()> {
    use sansio::Protocol as _;

    let cfg = AgentConfig {
        disconnected_timeout: Some(Duration::from_secs(1)),
        failed_timeout: Some(Duration::from_secs(1)),
        ..Default::default()
    };
    let mut a = Agent::new(Instant::now(), Arc::new(cfg), test_crypto_provider())?;

    let local_candidate = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "192.168.0.2".to_owned(),
            port: 777,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_host()?;
    let local_index = 0;
    let local_priority = local_candidate.priority();
    a.add_local_candidate(local_candidate)?;

    a.ufrag_pwd.remote_credentials = Some(Credentials {
        ufrag: "".to_string(),
        pwd: "".to_string(),
    });
    let username = a.ufrag_pwd.local_credentials.ufrag.clone() + ":";
    let local_pwd = a.ufrag_pwd.local_credentials.pwd.clone();
    let tie_breaker = a.tie_breaker;

    // Put the agent in a state where running a connectivity check now would time out
    // and transition to `Failed` (which deletes all candidates).
    a.connection_state = ConnectionState::Checking;
    a.last_connection_state = ConnectionState::Checking;
    a.checking_duration = Instant::now()
        .checked_sub(Duration::from_secs(3600))
        .unwrap_or(a.start_time);

    let remote_addr = SocketAddr::from_str("172.17.0.3:999")?;
    let mut msg = Message::new();
    msg.build(&[
        Box::new(BINDING_REQUEST),
        Box::new(TransactionId::new()),
        Box::new(Username::new(ATTR_USERNAME, username)),
        Box::new(UseCandidateAttr::new()),
        Box::new(AttrControlling(tie_breaker)),
        Box::new(PriorityAttr(local_priority)),
        Box::new(MessageIntegrity::new_short_term_integrity_with_provider(
            local_pwd,
            test_crypto_provider().crypto(),
        )),
        Box::new(FINGERPRINT),
    ])?;

    // Before the fix this call panicked while adding the peer-reflexive candidate.
    a.handle_inbound(Instant::now(), &mut msg, local_index, remote_addr)?;

    // The candidate was added and no re-entrant state change happened during the call.
    assert_eq!(
        a.remote_candidates.len(),
        1,
        "peer-reflexive remote candidate should have been added"
    );
    assert_eq!(
        a.connection_state,
        ConnectionState::Checking,
        "connection state must not change while handling the inbound request"
    );
    assert!(
        a.force_candidate_contact,
        "a connectivity check should have been deferred to the timeout handler"
    );

    // The deferred check now runs via the timeout handler, fails, and cleans up fully.
    a.handle_timeout(Instant::now())?;
    assert_eq!(a.connection_state, ConnectionState::Failed);
    assert!(a.candidate_pairs.is_empty());
    assert!(a.local_candidates.is_empty());
    assert!(a.remote_candidates.is_empty());
    assert!(a.selected_pair.is_none());
    assert!(a.nominated_pair.is_none());

    a.close()?;
    Ok(())
}

// Safari (and Chrome) expose ICE host candidates behind an mDNS "<uuid>.local"
// hostname by default. A QueryOnly agent (the default) must RESOLVE such a
// candidate -- i.e. issue an mDNS query, which surfaces as an outbound packet to
// MDNS_PORT -- whereas a Disabled agent silently drops it. This guards the
// behavioral consequence of defaulting the SettingEngine mDNS mode to QueryOnly.
//
// `add_remote_candidate` takes no instant, so it only *schedules* the query; the
// packet is stamped and emitted by the next `handle_timeout`, which does have one.
// `poll_timeout` reports an already-due deadline so the driver comes back at once.
#[test]
fn test_query_only_agent_queries_mdns_remote_candidate() -> Result<()> {
    // A .local host candidate in Safari's exact format.
    let cand_line =
        "1114572465 1 udp 2113939711 61b445d2-6503-41ac-96ce-ee3edac00e9f.local 61163 typ host";

    let base = Instant::now();

    // QueryOnly: adding the candidate issues an mDNS query (not a drop).
    let mut agent = Agent::new(
        base,
        Arc::new(AgentConfig {
            multicast_dns_mode: crate::mdns::MulticastDnsMode::QueryOnly,
            ..Default::default()
        }),
        test_crypto_provider(),
    )?;
    let added = agent.add_remote_candidate(unmarshal_candidate(cand_line)?)?;
    assert!(
        !added,
        "an mDNS candidate is not immediately usable; resolution is async"
    );
    let deadline = agent
        .poll_timeout()
        .expect("a scheduled mDNS query must ask the driver for a timeout");
    assert!(
        deadline <= base,
        "the scheduled query is already due, so the driver runs it immediately"
    );
    agent.handle_timeout(deadline)?;
    let pkt = agent
        .poll_write()
        .expect("QueryOnly must emit an mDNS query for a .local remote candidate");
    assert_eq!(
        pkt.transport.peer_addr.port(),
        mdns::MDNS_PORT,
        "the emitted packet must be an mDNS query"
    );
    assert_eq!(
        pkt.now, deadline,
        "the query is stamped with the instant the driver supplied, not an ambient one"
    );

    // Disabled: the same candidate is silently dropped -- no mDNS query.
    let mut agent = Agent::new(
        base,
        Arc::new(AgentConfig {
            multicast_dns_mode: crate::mdns::MulticastDnsMode::Disabled,
            ..Default::default()
        }),
        test_crypto_provider(),
    )?;
    agent.add_remote_candidate(unmarshal_candidate(cand_line)?)?;
    assert!(
        agent.poll_write().is_none(),
        "Disabled must not emit an mDNS query for a .local remote candidate"
    );

    Ok(())
}

/// A terminal ICE agent must not keep advertising its last connectivity-check deadline.
///
/// `contact` intentionally returns without updating `last_checking_time` after the agent fails.
/// If `poll_timeout` continues to derive a deadline from that value, every call after failure
/// returns the same instant in the past and a Sans-I/O driver can spin indefinitely.
#[test]
fn test_failed_agent_stops_and_restart_resumes_connectivity_check_timer() -> Result<()> {
    let base = Instant::now();
    let mut agent = Agent::new(
        base,
        Arc::new(AgentConfig {
            multicast_dns_mode: crate::mdns::MulticastDnsMode::Disabled,
            ..Default::default()
        }),
        test_crypto_provider(),
    )?;

    agent.start_connectivity_checks(
        base,
        true,
        "remote-ufrag".to_owned(),
        "remote-password".to_owned(),
    )?;
    agent.update_connection_state(
        Some(base + Duration::from_secs(190)),
        ConnectionState::Failed,
    );

    assert_eq!(
        agent.poll_timeout(),
        None,
        "a failed agent has no more connectivity checks to schedule"
    );
    agent.handle_timeout(base + Duration::from_secs(191))?;
    assert_eq!(
        agent.poll_timeout(),
        None,
        "handling time after failure must not recreate the stale deadline"
    );

    let restart_time = base + Duration::from_secs(200);
    agent.apply_restart(restart_time, true)?;
    agent.start_connectivity_checks(
        restart_time,
        true,
        "new-remote-ufrag".to_owned(),
        "new-remote-password".to_owned(),
    )?;

    assert!(
        agent.poll_timeout().is_some(),
        "an ICE restart must resume connectivity-check scheduling"
    );

    agent.close()?;
    assert_eq!(
        agent.poll_timeout(),
        None,
        "a closed agent has no more connectivity checks to schedule"
    );

    Ok(())
}

/// Regression test: connectivity checks for a server-reflexive local candidate
/// must be tagged with the candidate's base (the bound host socket), not the
/// NAT-mapped srflx address (RFC 8445 §6.1.2). Drivers that route outbound
/// transmits by `transport.local_addr` otherwise have no socket to send from
/// and drop the packet.
#[test]
fn test_send_stun_from_srflx_uses_base_addr() -> Result<()> {
    let mut a = Agent::new(
        Instant::now(),
        Arc::new(AgentConfig::default()),
        test_crypto_provider(),
    )?;

    let srflx_local = CandidateServerReflexiveConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "10.79.12.1".to_owned(), // NAT mapping; no local socket here
            port: 60823,
            component: 1,
            ..Default::default()
        },
        rel_addr: "192.168.0.2".to_owned(), // base: bound host socket
        rel_port: 5000,
        ..Default::default()
    }
    .new_candidate_server_reflexive()?;
    a.add_local_candidate(srflx_local)?;

    let host_remote = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "10.79.11.1".to_owned(),
            port: 37983,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_host()?;
    a.add_remote_candidate(host_remote)?;

    a.write_outs.clear();

    let msg = Message::new();
    a.send_stun(Instant::now(), &msg, 0, 0);

    let transmit = a.write_outs.pop_front().expect("send_stun must emit");
    assert_eq!(
        transmit.transport.local_addr,
        "192.168.0.2:5000".parse().unwrap(),
        "srflx checks must be sent from the candidate's base, not the mapped address"
    );
    assert_eq!(
        transmit.transport.peer_addr,
        "10.79.11.1:37983".parse().unwrap()
    );

    a.close()?;
    Ok(())
}

/// Staging an ICE restart must not disturb the live session.
///
/// JSEP requires `createOffer` to be free of side effects, so the credentials an ICE-restart offer
/// advertises are generated but *not* installed until the local description is applied. If they
/// were installed at offer time, inbound STUN would immediately start failing its USERNAME and
/// MESSAGE-INTEGRITY checks against the new pair — and permanently so if the offer were discarded,
/// which JSEP explicitly permits.
#[test]
fn test_staged_ice_restart_keeps_live_session_authenticating() -> Result<()> {
    let mut a = Agent::new(
        Instant::now(),
        Arc::new(AgentConfig::default()),
        test_crypto_provider(),
    )?;

    let local_candidate = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "192.168.0.2".to_owned(),
            port: 777,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_host()?;
    let local_priority = local_candidate.priority();
    a.add_local_candidate(local_candidate)?;

    a.ufrag_pwd.remote_credentials = Some(Credentials {
        ufrag: String::new(),
        pwd: String::new(),
    });

    let original = a.ufrag_pwd.local_credentials.clone();
    let remote_addr = SocketAddr::from_str("172.17.0.3:999")?;

    // Builds a binding request authenticated with `pwd`, as the remote peer would send it.
    let inbound = |ufrag: &str, pwd: String, tie_breaker: u64| -> Result<Message> {
        let mut msg = Message::new();
        msg.build(&[
            Box::new(BINDING_REQUEST),
            Box::new(TransactionId::new()),
            Box::new(Username::new(ATTR_USERNAME, format!("{ufrag}:"))),
            Box::new(AttrControlling(tie_breaker)),
            Box::new(PriorityAttr(local_priority)),
            Box::new(MessageIntegrity::new_short_term_integrity_with_provider(
                pwd,
                test_crypto_provider().crypto(),
            )),
            Box::new(FINGERPRINT),
        ])?;
        Ok(msg)
    };

    // Baseline: the peer's checks are accepted.
    let mut msg = inbound(&original.ufrag, original.pwd.clone(), a.tie_breaker)?;
    a.handle_inbound(Instant::now(), &mut msg, 0, remote_addr)?;

    // What `create_offer` does for an ICE restart: generate credentials for the offer to carry.
    a.generate_restart_credentials(String::new(), String::new())?;

    // The offer advertises the new pair...
    let advertised = a.get_local_credentials().clone();
    assert_ne!(
        advertised.ufrag, original.ufrag,
        "staging a restart must generate a fresh ufrag for the offer"
    );
    assert_ne!(advertised.pwd, original.pwd);

    // ...while the live session still authenticates with the old one.
    // (`Credentials` deliberately implements neither `Debug` nor `PartialEq`: `pwd` is the
    // MESSAGE-INTEGRITY key and must not reach a log line, so compare the fields directly.)
    assert_eq!(
        a.ufrag_pwd.local_credentials.ufrag, original.ufrag,
        "staging must not install the new ufrag into the live session"
    );
    assert_eq!(
        a.ufrag_pwd.local_credentials.pwd, original.pwd,
        "staging must not install the new pwd into the live session"
    );

    // The guarantee: the peer is still sending checks keyed on the original credentials, and they
    // must keep being accepted while the offer is in flight.
    let mut msg = inbound(&original.ufrag, original.pwd.clone(), a.tie_breaker)?;
    a.handle_inbound(Instant::now(), &mut msg, 0, remote_addr)
        .expect("inbound STUN keyed on the pre-restart credentials must still validate");

    // The application discards the offer and never sets a local description. The connection must
    // be exactly as it was.
    assert!(a.has_pending_restart());
    let mut msg = inbound(&original.ufrag, original.pwd.clone(), a.tie_breaker)?;
    a.handle_inbound(Instant::now(), &mut msg, 0, remote_addr)
        .expect("a discarded ICE-restart offer must leave the session working");

    // Applying it — what `set_local_description` does — installs the staged pair.
    a.apply_restart(Instant::now(), true)?;
    assert!(!a.has_pending_restart());
    assert_eq!(
        a.ufrag_pwd.local_credentials.ufrag, advertised.ufrag,
        "apply_restart must install exactly the ufrag the offer advertised"
    );
    assert_eq!(
        a.ufrag_pwd.local_credentials.pwd, advertised.pwd,
        "apply_restart must install exactly the pwd the offer advertised"
    );

    a.close()?;
    Ok(())
}

/// Agent events carry the instant their condition was observed at, so a consumer draining
/// `poll_event` is *told* the time rather than having to retain one and guess. `close` has no
/// caller-supplied instant, so it stamps with the newest one the agent was given.
#[test]
fn test_agent_events_carry_the_instant_they_were_observed_at() -> Result<()> {
    let base = Instant::now();
    let t = |secs| base + Duration::from_secs(secs);

    let mut a = Agent::new(
        t(0),
        Arc::new(AgentConfig::default()),
        test_crypto_provider(),
    )?;

    let host_local = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "192.168.0.2".to_owned(),
            port: 1000,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_host()?;
    a.add_local_candidate(host_local)?;

    let relay_remote = CandidateRelayConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "1.2.3.4".to_owned(),
            port: 12340,
            component: 1,
            ..Default::default()
        },
        rel_addr: "4.3.2.1".to_owned(),
        rel_port: 43210,
        ..Default::default()
    }
    .new_candidate_relay()?;
    a.add_remote_candidate(relay_remote)?;

    a.add_pair(0, 0);
    a.set_selected_pair(Some(t(5)), Some(0));

    // Selecting a pair emits Connected *and* SelectedCandidatePairChange; both are stamped
    // with the instant that caused them, not with a reading taken at drain time.
    let stamped: Vec<_> = std::iter::from_fn(|| a.poll_event()).collect();
    assert!(!stamped.is_empty(), "selecting a pair emits events");
    for e in &stamped {
        assert_eq!(
            e.now,
            t(5),
            "event {:?} must carry the instant its condition was observed at",
            std::mem::discriminant(&e.event)
        );
    }

    a.handle_timeout(t(30))?;
    while a.poll_event().is_some() {}

    // `close` is a drain with no caller-supplied instant, and the agent retains none, so the
    // Closed transition is applied to `connection_state` but emits no event — there is no
    // honest instant to stamp one with. A consumer that needs to observe the close should read
    // the state rather than wait for an event.
    a.close()?;
    assert_eq!(a.connection_state, ConnectionState::Closed);
    assert!(
        a.poll_event().is_none(),
        "close emits no event, having no instant to stamp one with"
    );

    Ok(())
}

/// A `getStats` snapshot reports the instant the caller asked at, throughout — including the
/// fields that default rather than being populated from live state. Mixing virtual protocol time
/// with wall-clock timestamps in one report is worse than no report: the reader cannot tell which
/// numbers are comparable.
#[test]
fn test_stats_snapshot_reports_the_callers_instant_throughout() -> Result<()> {
    let base = Instant::now();
    let t = |secs| base + Duration::from_secs(secs);

    let mut a = Agent::new(
        t(0),
        Arc::new(AgentConfig::default()),
        test_crypto_provider(),
    )?;

    let host_local = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "192.168.0.2".to_owned(),
            port: 1000,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_host()?;
    a.add_local_candidate(host_local)?;

    let host_remote = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "192.168.0.3".to_owned(),
            port: 1001,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_host()?;
    a.add_remote_candidate(host_remote)?;

    // Take the snapshot at an instant far from "now", so a stray wall-clock read is obvious.
    let pairs = a.get_candidate_pairs_stats(t(600));
    assert!(!pairs.is_empty(), "adding a remote candidate forms a pair");
    for p in &pairs {
        for (field, stamp) in [
            ("timestamp", p.timestamp),
            ("last_packet_sent_timestamp", p.last_packet_sent_timestamp),
            (
                "last_packet_received_timestamp",
                p.last_packet_received_timestamp,
            ),
            ("first_request_timestamp", p.first_request_timestamp),
            ("last_request_timestamp", p.last_request_timestamp),
            ("last_response_timestamp", p.last_response_timestamp),
            ("consent_expired_timestamp", p.consent_expired_timestamp),
        ] {
            assert_eq!(stamp, t(600), "{field} must report the caller's instant");
        }
    }

    let locals = a.get_local_candidates_stats(t(600));
    assert_eq!(locals.len(), 1);
    assert_eq!(locals[0].timestamp, t(600));

    let remotes = a.get_remote_candidates_stats(t(600));
    assert_eq!(remotes.len(), 1);
    assert_eq!(remotes[0].timestamp, t(600));

    // A second snapshot at a different instant reports that one, with no wall-clock time having
    // passed between the two calls.
    assert_eq!(a.get_candidate_pairs_stats(t(900))[0].timestamp, t(900));

    Ok(())
}
