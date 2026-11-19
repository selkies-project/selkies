//! Candidate pairs and their check state.
//!
//! ICE forms a pair from each compatible local/remote candidate combination and works through
//! them in priority order. A pair's combined priority is computed from both sides' priorities with
//! the controlling agent's dominating, so both agents derive the same ordering.
//!
//! [`CandidatePairState`](crate::candidate::candidate_pair::CandidatePairState) tracks how far a pair has got: waiting, in progress, succeeded or
//! failed. The controlling agent nominates one of the succeeded pairs, and that pair carries the
//! media.
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;

/// Represent the ICE candidate pair state.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum CandidatePairState {
    #[default]
    #[serde(rename = "unspecified")]
    /// No state was set.
    Unspecified = 0,

    /// Means a check has not been performed for this pair.
    #[serde(rename = "waiting")]
    Waiting = 1,

    /// Means a check has been sent for this pair, but the transaction is in progress.
    #[serde(rename = "in-progress")]
    InProgress = 2,

    /// Means a check for this pair was already done and failed, either never producing any response
    /// or producing an unrecoverable failure response.
    #[serde(rename = "failed")]
    Failed = 3,

    /// Means a check for this pair was already done and produced a successful result.
    #[serde(rename = "succeeded")]
    Succeeded = 4,
}

impl From<u8> for CandidatePairState {
    fn from(v: u8) -> Self {
        match v {
            1 => Self::Waiting,
            2 => Self::InProgress,
            3 => Self::Failed,
            4 => Self::Succeeded,
            _ => Self::Unspecified,
        }
    }
}

impl fmt::Display for CandidatePairState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match *self {
            Self::Waiting => "waiting",
            Self::InProgress => "in-progress",
            Self::Failed => "failed",
            Self::Succeeded => "succeeded",
            Self::Unspecified => "unspecified",
        };

        write!(f, "{s}")
    }
}

/// Represents a combination of a local and remote candidate.
#[derive(Clone, Copy)]
pub struct CandidatePair {
    /// Index of the local candidate in the agent's list.
    pub local_index: usize,
    /// Index of the remote candidate in the agent's list.
    pub remote_index: usize,
    /// The local candidate's priority.
    pub local_priority: u32,
    /// The remote candidate's priority.
    pub remote_priority: u32,
    pub(crate) ice_role_controlling: bool,
    pub(crate) binding_request_count: u16,
    pub(crate) state: CandidatePairState,
    pub(crate) nominated: bool,

    // STUN transaction stats
    /// Total number of STUN connectivity check requests sent (not including retransmissions).
    pub(crate) requests_sent: u64,
    /// Total number of STUN connectivity check requests received.
    pub(crate) requests_received: u64,
    /// Total number of STUN connectivity check responses sent.
    pub(crate) responses_sent: u64,
    /// Total number of STUN connectivity check responses received.
    pub(crate) responses_received: u64,
    /// Total number of consent freshness requests sent.
    pub(crate) consent_requests_sent: u64,

    // RTT tracking
    /// Sum of all round trip time measurements.
    pub(crate) total_round_trip_time: Duration,
    /// Latest round trip time measured.
    pub(crate) current_round_trip_time: Duration,
}

impl fmt::Debug for CandidatePair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "prio {} (local, prio {}) {} <-> {} (remote, prio {})",
            self.priority(),
            self.local_priority,
            self.local_index,
            self.remote_index,
            self.remote_priority,
        )
    }
}

impl fmt::Display for CandidatePair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "prio {} (local, prio {}) {} <-> {} (remote, prio {})",
            self.priority(),
            self.local_priority,
            self.local_index,
            self.remote_index,
            self.remote_priority,
        )
    }
}

impl PartialEq for CandidatePair {
    fn eq(&self, other: &Self) -> bool {
        self.local_index == other.local_index && self.remote_index == other.remote_index
    }
}

impl CandidatePair {
    #[must_use]
    /// Forms a pair from a local and a remote candidate.
    ///
    /// `controlling` selects which priority dominates in the pair's combined priority.
    pub fn new(
        local_index: usize,
        remote_index: usize,
        local_priority: u32,
        remote_priority: u32,
        ice_role_controlling: bool,
    ) -> Self {
        Self {
            local_index,
            remote_index,
            local_priority,
            remote_priority,
            ice_role_controlling,
            state: CandidatePairState::Waiting,
            binding_request_count: 0,
            nominated: false,
            // STUN transaction stats
            requests_sent: 0,
            requests_received: 0,
            responses_sent: 0,
            responses_received: 0,
            consent_requests_sent: 0,
            // RTT tracking
            total_round_trip_time: Duration::ZERO,
            current_round_trip_time: Duration::ZERO,
        }
    }

    /// RFC 5245 - 5.7.2.  Computing Pair Priority and Ordering Pairs
    /// Let G be the priority for the candidate provided by the controlling
    /// agent.  Let D be the priority for the candidate provided by the
    /// controlled agent.
    /// pair priority = 2^32*MIN(G,D) + 2*MAX(G,D) + (G>D?1:0)
    pub fn priority(&self) -> u64 {
        let (g, d) = if self.ice_role_controlling {
            (self.local_priority, self.remote_priority)
        } else {
            (self.remote_priority, self.local_priority)
        };

        // 1<<32 overflows uint32; and if both g && d are
        // maxUint32, this result would overflow uint64
        ((1 << 32_u64) - 1) * u64::from(std::cmp::min(g, d))
            + 2 * u64::from(std::cmp::max(g, d))
            + u64::from(g > d)
    }

    /// Called when a STUN binding request is sent.
    pub fn on_request_sent(&mut self) {
        self.requests_sent += 1;
    }

    /// Called when a STUN binding request is received.
    pub fn on_request_received(&mut self) {
        self.requests_received += 1;
    }

    /// Called when a STUN binding success response is sent.
    pub fn on_response_sent(&mut self) {
        self.responses_sent += 1;
    }

    /// Called when a STUN binding success response is received.
    /// Also updates RTT measurements.
    pub fn on_response_received(&mut self, rtt: Duration) {
        self.responses_received += 1;
        self.current_round_trip_time = rtt;
        self.total_round_trip_time += rtt;
    }

    /// Called when a consent freshness request is sent (keepalive).
    pub fn on_consent_request_sent(&mut self) {
        self.consent_requests_sent += 1;
    }
}
