//! TURN's STUN attributes and ChannelData framing.
//!
//! TURN is defined as a set of STUN methods and attributes, so these build on
//! [`rtc-stun`](https://docs.rs/rtc-stun). The attributes name the relay's parts:
//! [`relayaddr`](crate::proto::relayaddr) the allocated public address, [`peeraddr`](crate::proto::peeraddr) the far end, [`lifetime`](crate::proto::lifetime) the
//! allocation's expiry, [`data`](crate::proto::data) the relayed payload.
//!
//! [`chandata`](crate::proto::chandata) is the exception — a ChannelData message is not STUN at all, but a compact
//! four-byte framing that replaces the 36-byte Send/Data indication header once a channel is
//! bound. Its [`channum`](crate::proto::channum) range is chosen so the two can be told apart on a shared port.
#[cfg(test)]
mod proto_test;

/// Address helpers and the five-tuple that identifies an allocation.
pub mod addr;
/// ChannelData messages — the compact 4-byte framing for relayed data.
pub mod chandata;
/// The `CHANNEL-NUMBER` attribute.
pub mod channum;
/// The `DATA` attribute, which carries relayed payloads in Send/Data indications.
pub mod data;
/// The `DONT-FRAGMENT` attribute, asking the server to set DF on relayed packets.
pub mod dontfrag;
/// The `EVEN-PORT` attribute, requesting an even relayed port (for RTP/RTCP pairs).
pub mod evenport;
/// The `LIFETIME` attribute, which sets and reports allocation expiry.
pub mod lifetime;
/// The `XOR-PEER-ADDRESS` attribute, naming the peer in permission and data messages.
pub mod peeraddr;
/// The `XOR-RELAYED-ADDRESS` attribute, which reports the allocated public address.
pub mod relayaddr;
/// The `REQUESTED-ADDRESS-FAMILY` attribute, for asking for an IPv4 or IPv6 allocation.
pub mod reqfamily;
/// The `REQUESTED-TRANSPORT` attribute, which selects the relay's transport to peers.
pub mod reqtrans;
/// The `RESERVATION-TOKEN` attribute, used to claim a previously reserved port.
pub mod rsrvtoken;

use std::fmt;

use stun::message::*;

// proto implements RFC 5766 Traversal Using Relays around NAT.

/// `Protocol` is IANA assigned protocol number.
#[derive(PartialEq, Eq, Default, Debug, Clone, Copy, Hash)]
pub struct Protocol(pub u8);

/// `PROTO_TCP` is IANA assigned protocol number for TCP.
pub const PROTO_TCP: Protocol = Protocol(6);
/// `PROTO_UDP` is IANA assigned protocol number for UDP.
pub const PROTO_UDP: Protocol = Protocol(17);

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let others = format!("{}", self.0);
        let s = match *self {
            PROTO_UDP => "UDP",
            PROTO_TCP => "TCP",
            _ => others.as_str(),
        };

        write!(f, "{s}")
    }
}

// Default ports for TURN from RFC 5766 Section 4.

/// `DEFAULT_PORT` for TURN is same as STUN.
pub const DEFAULT_PORT: u16 = stun::DEFAULT_PORT;
/// `DEFAULT_TLSPORT` is for TURN over TLS and is same as STUN.
pub const DEFAULT_TLS_PORT: u16 = stun::DEFAULT_TLS_PORT;

/// Shorthand for create permission request type.
pub fn create_permission_request() -> MessageType {
    MessageType::new(METHOD_CREATE_PERMISSION, CLASS_REQUEST)
}

/// Shorthand for allocation request message type.
pub fn allocate_request() -> MessageType {
    MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST)
}

/// Shorthand for send indication message type.
pub fn send_indication() -> MessageType {
    MessageType::new(METHOD_SEND, CLASS_INDICATION)
}

/// Shorthand for refresh request message type.
pub fn refresh_request() -> MessageType {
    MessageType::new(METHOD_REFRESH, CLASS_REQUEST)
}
