use super::*;
use crate::network_type::determine_network_type;
use crate::rand::generate_cand_id;
use shared::error::*;

/// The config required to create a new `CandidateRelay`.
#[derive(Default)]
pub struct CandidateRelayConfig {
    /// The fields shared by every candidate type.
    pub base_config: CandidateConfig,

    /// The base address this candidate was derived from, reported as `raddr`.
    pub rel_addr: String,
    /// The base port, reported as `rport`.
    pub rel_port: u16,

    /// The server this candidate was gathered from.
    pub url: Option<String>,
}

impl CandidateRelayConfig {
    /// Creates a new relay candidate.
    pub fn new_candidate_relay(self) -> Result<Candidate> {
        let mut candidate_id = self.base_config.candidate_id;
        if candidate_id.is_empty() {
            candidate_id = generate_cand_id();
        }

        let ip: IpAddr = match self.base_config.address.parse() {
            Ok(ip) => ip,
            Err(_) => return Err(Error::ErrAddressParseFailed),
        };
        let network_type = determine_network_type(&self.base_config.network, &ip)?;

        Ok(Candidate {
            id: candidate_id,
            network_type,
            candidate_type: CandidateType::Relay,
            address: self.base_config.address,
            port: self.base_config.port,
            resolved_addr: SocketAddr::new(ip, self.base_config.port),
            component: self.base_config.component,
            foundation_override: self.base_config.foundation,
            priority_override: self.base_config.priority,
            related_address: Some(CandidateRelatedAddress {
                address: self.rel_addr,
                port: self.rel_port,
            }),
            url: self.url,
            ..Candidate::default()
        })
    }
}
