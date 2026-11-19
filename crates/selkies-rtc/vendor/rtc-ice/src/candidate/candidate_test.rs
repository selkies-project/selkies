use super::*;
use crate::candidate::candidate_host::CandidateHostConfig;
use crate::candidate::candidate_pair::CandidatePairState;
use crate::candidate::candidate_peer_reflexive::CandidatePeerReflexiveConfig;
use crate::candidate::candidate_relay::CandidateRelayConfig;
use crate::candidate::candidate_server_reflexive::CandidateServerReflexiveConfig;
use crate::candidate::{Candidate, unmarshal_candidate};
use std::time::Instant;

#[test]
fn test_candidate_priority() -> Result<()> {
    let tests = vec![
        (
            Candidate {
                candidate_type: CandidateType::Host,
                component: COMPONENT_RTP,
                ..Default::default()
            },
            2130706431,
        ),
        (
            Candidate {
                candidate_type: CandidateType::Host,
                component: COMPONENT_RTP,
                network_type: NetworkType::Tcp4,
                tcp_type: TcpType::Active,
                ..Default::default()
            },
            2128609279,
        ),
        (
            Candidate {
                candidate_type: CandidateType::Host,
                component: COMPONENT_RTP,
                network_type: NetworkType::Tcp4,
                tcp_type: TcpType::Passive,
                ..Default::default()
            },
            2124414975,
        ),
        (
            Candidate {
                candidate_type: CandidateType::Host,
                component: COMPONENT_RTP,
                network_type: NetworkType::Tcp4,
                tcp_type: TcpType::SimultaneousOpen,
                ..Default::default()
            },
            2120220671,
        ),
        (
            Candidate {
                candidate_type: CandidateType::PeerReflexive,
                component: COMPONENT_RTP,
                ..Default::default()
            },
            1862270975,
        ),
        (
            Candidate {
                candidate_type: CandidateType::PeerReflexive,
                component: COMPONENT_RTP,
                network_type: NetworkType::Tcp6,
                tcp_type: TcpType::SimultaneousOpen,
                ..Default::default()
            },
            1860173823,
        ),
        (
            Candidate {
                candidate_type: CandidateType::PeerReflexive,
                component: COMPONENT_RTP,
                network_type: NetworkType::Tcp6,
                tcp_type: TcpType::Active,
                ..Default::default()
            },
            1855979519,
        ),
        (
            Candidate {
                candidate_type: CandidateType::PeerReflexive,
                component: COMPONENT_RTP,
                network_type: NetworkType::Tcp6,
                tcp_type: TcpType::Passive,
                ..Default::default()
            },
            1851785215,
        ),
        (
            Candidate {
                candidate_type: CandidateType::ServerReflexive,
                component: COMPONENT_RTP,
                ..Default::default()
            },
            1694498815,
        ),
        (
            Candidate {
                candidate_type: CandidateType::Relay,
                component: COMPONENT_RTP,
                ..Default::default()
            },
            16777215,
        ),
    ];

    for (candidate, want) in tests {
        let got = candidate.priority();
        assert_eq!(
            got, want,
            "Candidate({candidate}).Priority() = {got}, want {want}"
        );
    }

    Ok(())
}

#[test]
fn test_candidate_last_sent() -> Result<()> {
    let mut candidate = Candidate::default();
    assert_eq!(
        candidate.last_sent(),
        None,
        "a fresh candidate has sent nothing"
    );

    let now = Instant::now();
    candidate.set_last_sent(now);
    assert_eq!(candidate.last_sent(), Some(now));

    Ok(())
}

#[test]
fn test_candidate_last_received() -> Result<()> {
    let mut candidate = Candidate::default();
    assert_eq!(
        candidate.last_received(),
        None,
        "a fresh candidate has received nothing"
    );

    let now = Instant::now();
    candidate.set_last_received(now);
    assert_eq!(candidate.last_received(), Some(now));

    Ok(())
}

#[test]
fn test_candidate_foundation() -> Result<()> {
    // All fields are the same
    assert_eq!(
        (Candidate {
            candidate_type: CandidateType::Host,
            network_type: NetworkType::Udp4,
            address: "A".to_owned(),
            ..Default::default()
        })
        .foundation(),
        (Candidate {
            candidate_type: CandidateType::Host,
            network_type: NetworkType::Udp4,
            address: "A".to_owned(),
            ..Default::default()
        })
        .foundation()
    );

    // Different Address
    assert_ne!(
        (Candidate {
            candidate_type: CandidateType::Host,
            network_type: NetworkType::Udp4,
            address: "A".to_owned(),
            ..Default::default()
        })
        .foundation(),
        (Candidate {
            candidate_type: CandidateType::Host,
            network_type: NetworkType::Udp4,
            address: "B".to_owned(),
            ..Default::default()
        })
        .foundation(),
    );

    // Different networkType
    assert_ne!(
        (Candidate {
            candidate_type: CandidateType::Host,
            network_type: NetworkType::Udp4,
            address: "A".to_owned(),
            ..Default::default()
        })
        .foundation(),
        (Candidate {
            candidate_type: CandidateType::Host,
            network_type: NetworkType::Udp6,
            address: "A".to_owned(),
            ..Default::default()
        })
        .foundation(),
    );

    // Different candidateType
    assert_ne!(
        (Candidate {
            candidate_type: CandidateType::Host,
            network_type: NetworkType::Udp4,
            address: "A".to_owned(),
            ..Default::default()
        })
        .foundation(),
        (Candidate {
            candidate_type: CandidateType::PeerReflexive,
            network_type: NetworkType::Udp4,
            address: "A".to_owned(),
            ..Default::default()
        })
        .foundation(),
    );

    // Port has no effect
    assert_eq!(
        (Candidate {
            candidate_type: CandidateType::Host,
            network_type: NetworkType::Udp4,
            address: "A".to_owned(),
            port: 8080,
            ..Default::default()
        })
        .foundation(),
        (Candidate {
            candidate_type: CandidateType::Host,
            network_type: NetworkType::Udp4,
            address: "A".to_owned(),
            port: 80,
            ..Default::default()
        })
        .foundation()
    );

    Ok(())
}

#[test]
fn test_candidate_pair_state_serialization() {
    let tests = vec![
        (CandidatePairState::Unspecified, "\"unspecified\""),
        (CandidatePairState::Waiting, "\"waiting\""),
        (CandidatePairState::InProgress, "\"in-progress\""),
        (CandidatePairState::Failed, "\"failed\""),
        (CandidatePairState::Succeeded, "\"succeeded\""),
    ];

    for (candidate_pair_state, expected_string) in tests {
        assert_eq!(
            expected_string.to_string(),
            serde_json::to_string(&candidate_pair_state).unwrap()
        );
    }
}

#[test]
fn test_candidate_pair_state_to_string() {
    let tests = vec![
        (CandidatePairState::Unspecified, "unspecified"),
        (CandidatePairState::Waiting, "waiting"),
        (CandidatePairState::InProgress, "in-progress"),
        (CandidatePairState::Failed, "failed"),
        (CandidatePairState::Succeeded, "succeeded"),
    ];

    for (candidate_pair_state, expected_string) in tests {
        assert_eq!(candidate_pair_state.to_string(), expected_string);
    }
}

#[test]
fn test_candidate_type_serialization() {
    let tests = vec![
        (CandidateType::Unspecified, "\"unspecified\""),
        (CandidateType::Host, "\"host\""),
        (CandidateType::ServerReflexive, "\"srflx\""),
        (CandidateType::PeerReflexive, "\"prflx\""),
        (CandidateType::Relay, "\"relay\""),
    ];

    for (candidate_type, expected_string) in tests {
        assert_eq!(
            serde_json::to_string(&candidate_type).unwrap(),
            expected_string.to_string()
        );
    }
}

#[test]
fn test_candidate_type_to_string() {
    let tests = vec![
        (CandidateType::Unspecified, "Unknown candidate type"),
        (CandidateType::Host, "host"),
        (CandidateType::ServerReflexive, "srflx"),
        (CandidateType::PeerReflexive, "prflx"),
        (CandidateType::Relay, "relay"),
    ];

    for (candidate_type, expected_string) in tests {
        assert_eq!(candidate_type.to_string(), expected_string);
    }
}

#[test]
fn test_candidate_marshal() -> Result<()> {
    let tests = vec![
        (
            Some(Candidate {
                network_type: NetworkType::Udp6,
                candidate_type: CandidateType::Host,
                address: "fcd9:e3b8:12ce:9fc5:74a5:c6bb:d8b:e08a".to_owned(),
                port: 53987,
                priority_override: 500,
                foundation_override: "750".to_owned(),
                ..Default::default()
            }),
            "750 1 udp 500 fcd9:e3b8:12ce:9fc5:74a5:c6bb:d8b:e08a 53987 typ host",
        ),
        (
            Some(Candidate {
                network_type: NetworkType::Udp4,
                candidate_type: CandidateType::Host,
                address: "10.0.75.1".to_owned(),
                port: 53634,
                ..Default::default()
            }),
            "4273957277 1 udp 2130706431 10.0.75.1 53634 typ host",
        ),
        (
            Some(Candidate {
                network_type: NetworkType::Udp4,
                candidate_type: CandidateType::ServerReflexive,
                address: "191.228.238.68".to_owned(),
                port: 53991,
                related_address: Some(CandidateRelatedAddress {
                    address: "192.168.0.274".to_owned(),
                    port: 53991,
                }),
                ..Default::default()
            }),
            "647372371 1 udp 1694498815 191.228.238.68 53991 typ srflx raddr 192.168.0.274 rport 53991",
        ),
        (
            Some(Candidate {
                network_type: NetworkType::Udp4,
                candidate_type: CandidateType::Relay,
                address: "50.0.0.1".to_owned(),
                port: 5000,
                related_address: Some(CandidateRelatedAddress {
                    address: "192.168.0.1".to_owned(),
                    port: 5001,
                }),
                ..Default::default()
            }),
            "848194626 1 udp 16777215 50.0.0.1 5000 typ relay raddr 192.168.0.1 rport 5001",
        ),
        (
            Some(Candidate {
                network_type: NetworkType::Tcp4,
                candidate_type: CandidateType::Host,
                address: "192.168.0.196".to_owned(),
                port: 0,
                tcp_type: TcpType::Active,
                ..Default::default()
            }),
            "1052353102 1 tcp 2128609279 192.168.0.196 0 typ host tcptype active",
        ),
        /*TODO: mDNS (
            Some(Candidate{
                    network_type:   NetworkType::Udp4,
                    candidate_type: CandidateType::Host,
                    address:       "e2494022-4d9a-4c1e-a750-cc48d4f8d6ee.local".to_owned(),
                    port:          60542,
                ..Default::default()
            }),
            "1380287402 1 udp 2130706431 e2494022-4d9a-4c1e-a750-cc48d4f8d6ee.local 60542 typ host",
        ),*/
        // Invalid candidates
        (None, ""),
        (None, "1938809241"),
        (
            None,
            "1986380506 99999999 udp 2122063615 10.0.75.1 53634 typ host generation 0 network-id 2",
        ),
        (
            None,
            "1986380506 1 udp 99999999999 10.0.75.1 53634 typ host",
        ),
        (
            None,
            "4207374051 1 udp 1685790463 191.228.238.68 99999999 typ srflx raddr 192.168.0.278 rport 53991 generation 0 network-id 3",
        ),
        (
            None,
            "4207374051 1 udp 1685790463 191.228.238.68 53991 typ srflx raddr",
        ),
        (
            None,
            "4207374051 1 udp 1685790463 191.228.238.68 53991 typ srflx raddr 192.168.0.278 rport 99999999 generation 0 network-id 3",
        ),
        (
            None,
            "4207374051 INVALID udp 2130706431 10.0.75.1 53634 typ host",
        ),
        (None, "4207374051 1 udp INVALID 10.0.75.1 53634 typ host"),
        (
            None,
            "4207374051 INVALID udp 2130706431 10.0.75.1 INVALID typ host",
        ),
        (
            None,
            "4207374051 1 udp 2130706431 10.0.75.1 53634 typ INVALID",
        ),
    ];

    for (candidate, marshaled) in tests {
        let actual_candidate = unmarshal_candidate(marshaled);
        if let Some(candidate) = candidate {
            if let Ok(actual_candidate) = actual_candidate {
                assert!(
                    candidate.equal(&actual_candidate),
                    "{} vs {}",
                    candidate.marshal(),
                    marshaled
                );
                assert_eq!(marshaled, actual_candidate.marshal());
            } else {
                panic!("expected ok");
            }
        } else {
            assert!(actual_candidate.is_err(), "expected error");
        }
    }

    Ok(())
}

/// Regression test: a candidate's base address must be the local (bound)
/// transport address. For server/peer-reflexive candidates that is the
/// related address, not the NAT-mapped candidate address (RFC 8445 §5.1.1).
#[test]
fn test_candidate_base_addr() -> Result<()> {
    let host = CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "192.168.0.2".to_owned(),
            port: 5000,
            component: 1,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_host()?;
    assert_eq!(host.base_addr(), host.addr(), "host base is itself");

    let srflx = CandidateServerReflexiveConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "10.79.12.1".to_owned(),
            port: 60823,
            component: 1,
            ..Default::default()
        },
        rel_addr: "192.168.0.2".to_owned(),
        rel_port: 5000,
        ..Default::default()
    }
    .new_candidate_server_reflexive()?;
    assert_eq!(
        srflx.base_addr(),
        "192.168.0.2:5000".parse().unwrap(),
        "srflx base must be the related (host) address"
    );
    assert_ne!(srflx.base_addr(), srflx.addr());

    let prflx = CandidatePeerReflexiveConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "10.79.12.1".to_owned(),
            port: 60824,
            component: 1,
            ..Default::default()
        },
        rel_addr: "192.168.0.2".to_owned(),
        rel_port: 5000,
        ..Default::default()
    }
    .new_candidate_peer_reflexive()?;
    assert_eq!(
        prflx.base_addr(),
        "192.168.0.2:5000".parse().unwrap(),
        "prflx base must be the related (host) address"
    );

    // Missing/unparseable related address falls back to the candidate address.
    let prflx_no_rel = CandidatePeerReflexiveConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "10.79.12.1".to_owned(),
            port: 60825,
            component: 1,
            ..Default::default()
        },
        rel_addr: "".to_owned(),
        rel_port: 0,
        ..Default::default()
    }
    .new_candidate_peer_reflexive()?;
    assert_eq!(prflx_no_rel.base_addr(), prflx_no_rel.addr());

    let relay = CandidateRelayConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "50.0.0.1".to_owned(),
            port: 5000,
            component: 1,
            ..Default::default()
        },
        rel_addr: "192.168.0.2".to_owned(),
        rel_port: 5001,
        ..Default::default()
    }
    .new_candidate_relay()?;
    assert_eq!(
        relay.base_addr(),
        relay.addr(),
        "relay base is the relayed address itself"
    );

    Ok(())
}
