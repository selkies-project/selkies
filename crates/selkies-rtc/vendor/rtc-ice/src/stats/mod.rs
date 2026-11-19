use std::time::Instant;

use crate::candidate::candidate_pair::CandidatePairState;
use crate::candidate::*;

/// CandidatePairStats contains ICE candidate pair statistics.
#[derive(Debug, Clone)]
pub struct CandidatePairStats {
    /// The timestamp associated with this object.
    pub timestamp: Instant,

    /// The id of the local candidate.
    pub local_candidate_id: String,

    /// The id of the remote candidate.
    pub remote_candidate_id: String,

    /// The state of the checklist for the local and remote
    /// candidates in a pair.
    pub state: CandidatePairState,

    /// True when this valid pair that should be used for media
    /// if it is the highest-priority one amongst those whose nominated flag is set.
    pub nominated: bool,

    /// The total number of packets sent on this candidate pair.
    pub packets_sent: u32,

    /// The total number of packets received on this candidate pair.
    pub packets_received: u32,

    /// The total number of payload bytes sent on this candidate pair
    /// not including headers or padding.
    pub bytes_sent: u64,

    /// The total number of payload bytes received on this candidate pair
    /// not including headers or padding.
    pub bytes_received: u64,

    /// The timestamp at which the last packet was
    /// sent on this particular candidate pair, excluding STUN packets.
    pub last_packet_sent_timestamp: Instant,

    /// The timestamp at which the last packet
    /// was received on this particular candidate pair, excluding STUN packets.
    pub last_packet_received_timestamp: Instant,

    /// The timestamp at which the first STUN request
    /// was sent on this particular candidate pair.
    pub first_request_timestamp: Instant,

    /// The timestamp at which the last STUN request
    /// was sent on this particular candidate pair. The average interval between two
    /// consecutive connectivity checks sent can be calculated with
    /// (last_request_timestamp - first_request_timestamp) / requests_sent.
    pub last_request_timestamp: Instant,

    /// The timestamp at which the last STUN response
    /// was received on this particular candidate pair.
    pub last_response_timestamp: Instant,

    /// The sum of all round trip time measurements
    /// in seconds since the beginning of the session, based on STUN connectivity
    /// check responses (responses_received), including those that reply to requests
    /// that are sent in order to verify consent. The average round trip time can
    /// be computed from total_round_trip_time by dividing it by responses_received.
    pub total_round_trip_time: f64,

    /// The latest round trip time measured in seconds,
    /// computed from both STUN connectivity checks, including those that are sent
    /// for consent verification.
    pub current_round_trip_time: f64,

    /// Calculated by the underlying congestion control
    /// by combining the available bitrate for all the outgoing RTP streams using
    /// this candidate pair. The bitrate measurement does not count the size of the
    /// ip or other transport layers like TCP or UDP. It is similar to the TIAS defined
    /// in RFC 3890, i.e., it is measured in bits per second and the bitrate is calculated
    /// over a 1 second window.
    pub available_outgoing_bitrate: f64,

    /// Calculated by the underlying congestion control
    /// by combining the available bitrate for all the incoming RTP streams using
    /// this candidate pair. The bitrate measurement does not count the size of the
    /// ip or other transport layers like TCP or UDP. It is similar to the TIAS defined
    /// in  RFC 3890, i.e., it is measured in bits per second and the bitrate is
    /// calculated over a 1 second window.
    pub available_incoming_bitrate: f64,

    /// The number of times the circuit breaker
    /// is triggered for this particular 5-tuple, ceasing transmission.
    pub circuit_breaker_trigger_count: u32,

    /// The total number of connectivity check requests
    /// received (including retransmissions). It is impossible for the receiver to
    /// tell whether the request was sent in order to check connectivity or check
    /// consent, so all connectivity checks requests are counted here.
    pub requests_received: u64,

    /// The total number of connectivity check requests
    /// sent (not including retransmissions).
    pub requests_sent: u64,

    /// The total number of connectivity check responses received.
    pub responses_received: u64,

    /// Responses_sent epresents the total number of connectivity check responses sent.
    /// Since we cannot distinguish connectivity check requests and consent requests,
    /// all responses are counted.
    pub responses_sent: u64,

    /// The total number of connectivity check
    /// request retransmissions received.
    pub retransmissions_received: u64,

    /// The total number of connectivity check
    /// request retransmissions sent.
    pub retransmissions_sent: u64,

    /// The total number of consent requests sent.
    pub consent_requests_sent: u64,

    /// The timestamp at which the latest valid.
    /// STUN binding response expired.
    pub consent_expired_timestamp: Instant,
}

/// CandidateStats contains ICE candidate statistics related to the ICETransport objects.
#[derive(Debug, Clone)]
pub struct CandidateStats {
    /// The timestamp associated with this object.
    pub timestamp: Instant,

    /// The candidate id.
    pub id: String,

    /// The ip address of the candidate, allowing for IPv4 addresses and.
    /// IPv6 addresses, but fully qualified domain names (FQDNs) are not allowed.
    pub ip: String,

    /// The port number of the candidate.
    pub port: u16,

    /// The "Type" field of the ICECandidate.
    pub candidate_type: CandidateType,

    /// The "priority" field of the ICECandidate.
    pub priority: u32,

    /// The url of the TURN or STUN server indicated in the that translated
    /// this ip address. It is the url address surfaced in an PeerConnectionICEEvent.
    pub url: String,

    /// The protocol used by the endpoint to communicate with the.
    /// TURN server. This is only present for local candidates. Valid values for
    /// the TURN url protocol is one of udp, tcp, or tls.
    pub relay_protocol: String,

    // deleted is true if the candidate has been deleted/freed. For host candidates,
    // this means that any network resources (typically a socket) associated with the
    // candidate have been released. For TURN candidates, this means the TURN allocation
    // is no longer active.
    //
    /// Only defined for local candidates. For remote candidates, this property is not applicable.
    pub deleted: bool,
}
