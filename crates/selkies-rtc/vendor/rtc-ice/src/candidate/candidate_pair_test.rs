use super::*;
use crate::candidate::candidate_host::CandidateHostConfig;
use crate::candidate::candidate_pair::CandidatePair;
use crate::candidate::candidate_peer_reflexive::CandidatePeerReflexiveConfig;
use crate::candidate::candidate_relay::CandidateRelayConfig;
use crate::candidate::candidate_server_reflexive::CandidateServerReflexiveConfig;
use crate::candidate::{Candidate, CandidateConfig};

pub(crate) fn host_candidate() -> Result<Candidate> {
    CandidateHostConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "0.0.0.0".to_owned(),
            component: COMPONENT_RTP,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_host()
}

pub(crate) fn prflx_candidate() -> Result<Candidate> {
    CandidatePeerReflexiveConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "0.0.0.0".to_owned(),
            component: COMPONENT_RTP,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_peer_reflexive()
}

pub(crate) fn srflx_candidate() -> Result<Candidate> {
    CandidateServerReflexiveConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "0.0.0.0".to_owned(),
            component: COMPONENT_RTP,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_server_reflexive()
}

pub(crate) fn relay_candidate() -> Result<Candidate> {
    CandidateRelayConfig {
        base_config: CandidateConfig {
            network: "udp".to_owned(),
            address: "0.0.0.0".to_owned(),
            component: COMPONENT_RTP,
            ..Default::default()
        },
        ..Default::default()
    }
    .new_candidate_relay()
}

#[test]
fn test_candidate_pair_priority() -> Result<()> {
    const HOST_INDEX: usize = 0;
    const PRFLX_INDEX: usize = 1;
    const SRFLX_INDEX: usize = 2;
    const RELAY_INDEX: usize = 3;

    let candidates = vec![
        host_candidate()?,
        prflx_candidate()?,
        srflx_candidate()?,
        relay_candidate()?,
    ];

    let tests = vec![
        (
            CandidatePair::new(
                HOST_INDEX,
                HOST_INDEX,
                candidates[HOST_INDEX].priority(),
                candidates[HOST_INDEX].priority(),
                false,
            ),
            9151314440652587007,
        ),
        (
            CandidatePair::new(
                HOST_INDEX,
                HOST_INDEX,
                candidates[HOST_INDEX].priority(),
                candidates[HOST_INDEX].priority(),
                true,
            ),
            9151314440652587007,
        ),
        (
            CandidatePair::new(
                HOST_INDEX,
                PRFLX_INDEX,
                candidates[HOST_INDEX].priority(),
                candidates[PRFLX_INDEX].priority(),
                true,
            ),
            7998392936314175488,
        ),
        (
            CandidatePair::new(
                HOST_INDEX,
                PRFLX_INDEX,
                candidates[HOST_INDEX].priority(),
                candidates[PRFLX_INDEX].priority(),
                false,
            ),
            7998392936314175487,
        ),
        (
            CandidatePair::new(
                HOST_INDEX,
                SRFLX_INDEX,
                candidates[HOST_INDEX].priority(),
                candidates[SRFLX_INDEX].priority(),
                true,
            ),
            7277816996102668288,
        ),
        (
            CandidatePair::new(
                HOST_INDEX,
                SRFLX_INDEX,
                candidates[HOST_INDEX].priority(),
                candidates[SRFLX_INDEX].priority(),
                false,
            ),
            7277816996102668287,
        ),
        (
            CandidatePair::new(
                HOST_INDEX,
                RELAY_INDEX,
                candidates[HOST_INDEX].priority(),
                candidates[RELAY_INDEX].priority(),
                true,
            ),
            72057593987596288,
        ),
        (
            CandidatePair::new(
                HOST_INDEX,
                RELAY_INDEX,
                candidates[HOST_INDEX].priority(),
                candidates[RELAY_INDEX].priority(),
                false,
            ),
            72057593987596287,
        ),
    ];

    for (pair, want) in tests {
        let got = pair.priority();
        assert_eq!(
            got, want,
            "CandidatePair({pair}).Priority() = {got}, want {want}"
        );
    }

    Ok(())
}

#[test]
fn test_candidate_pair_equality() -> Result<()> {
    const HOST_INDEX: usize = 0;
    const PRFLX_INDEX: usize = 1;
    const SRFLX_INDEX: usize = 2;
    const RELAY_INDEX: usize = 3;

    let candidates = vec![
        host_candidate()?,
        prflx_candidate()?,
        srflx_candidate()?,
        relay_candidate()?,
    ];

    let pair_a = CandidatePair::new(
        HOST_INDEX,
        SRFLX_INDEX,
        candidates[HOST_INDEX].priority(),
        candidates[SRFLX_INDEX].priority(),
        true,
    );
    let pair_b = CandidatePair::new(
        HOST_INDEX,
        SRFLX_INDEX,
        candidates[HOST_INDEX].priority(),
        candidates[SRFLX_INDEX].priority(),
        false,
    );

    assert_eq!(pair_a, pair_b, "Expected {pair_a} to equal {pair_b}");

    Ok(())
}
