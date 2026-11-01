/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The Data Channel Establishment Protocol (RFC 8832) messages.
//!
//! `DATA_CHANNEL_OPEN` names a channel's label, protocol, order, and reliability on the
//! stream it opens, and `DATA_CHANNEL_ACK` answers it. A peer's label or protocol need not
//! be UTF-8 (libwebrtc takes the bytes as they come), so both are decoded with replacement
//! rather than refused, and a message too short for the fields it declares is a parse
//! error the association drops.

use bytes::{BufMut, Bytes, BytesMut};

use super::channel::DataChannelParameters;

/// Message type of `DATA_CHANNEL_ACK`.
pub const DATA_CHANNEL_ACK: u8 = 2;
/// Message type of `DATA_CHANNEL_OPEN`.
pub const DATA_CHANNEL_OPEN: u8 = 3;

const CHANNEL_TYPE_RELIABLE: u8 = 0x00;
const CHANNEL_TYPE_PARTIAL_RELIABLE_REXMIT: u8 = 0x01;
const CHANNEL_TYPE_PARTIAL_RELIABLE_TIMED: u8 = 0x02;
const CHANNEL_TYPE_UNORDERED: u8 = 0x80;

/// A parsed DCEP message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DcepMessage {
    /// A peer opens a channel on the stream the message arrived on.
    Open(DataChannelParameters),
    /// A peer acknowledges the channel this side opened on the stream.
    Ack,
}

/// Why a DCEP message was not taken.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DcepError {
    /// The message is empty.
    #[error("empty DCEP message")]
    Empty,
    /// An OPEN shorter than its fixed header or the strings it declares.
    #[error("DCEP OPEN of {0} bytes is too short")]
    Short(usize),
    /// A message type neither OPEN nor ACK.
    #[error("unknown DCEP message type {0}")]
    UnknownType(u8),
}

/// Encode a `DATA_CHANNEL_OPEN` for `params`.
pub fn encode_open(params: &DataChannelParameters) -> Bytes {
    let mut channel_type = CHANNEL_TYPE_RELIABLE;
    let mut reliability: u32 = 0;
    if !params.ordered {
        channel_type |= CHANNEL_TYPE_UNORDERED;
    }
    if let Some(max_retransmits) = params.max_retransmits {
        channel_type |= CHANNEL_TYPE_PARTIAL_RELIABLE_REXMIT;
        reliability = max_retransmits;
    } else if let Some(max_packet_life_time) = params.max_packet_life_time {
        channel_type |= CHANNEL_TYPE_PARTIAL_RELIABLE_TIMED;
        reliability = max_packet_life_time;
    }
    let label = params.label.as_bytes();
    let protocol = params.protocol.as_bytes();
    let mut out = BytesMut::with_capacity(12 + label.len() + protocol.len());
    out.put_u8(DATA_CHANNEL_OPEN);
    out.put_u8(channel_type);
    out.put_u16(0);
    out.put_u32(reliability);
    out.put_u16(label.len().min(usize::from(u16::MAX)) as u16);
    out.put_u16(protocol.len().min(usize::from(u16::MAX)) as u16);
    out.put_slice(&label[..label.len().min(usize::from(u16::MAX))]);
    out.put_slice(&protocol[..protocol.len().min(usize::from(u16::MAX))]);
    out.freeze()
}

/// Encode a `DATA_CHANNEL_ACK`.
pub fn encode_ack() -> Bytes {
    Bytes::from_static(&[DATA_CHANNEL_ACK])
}

/// Parse a DCEP message as it arrived on stream `stream_id`.
///
/// An OPEN whose declared label or protocol runs past the message takes what is there,
/// as libwebrtc does, rather than refusing the channel.
pub fn decode(stream_id: u16, data: &[u8]) -> Result<DcepMessage, DcepError> {
    let Some(&message_type) = data.first() else {
        return Err(DcepError::Empty);
    };
    match message_type {
        DATA_CHANNEL_ACK => Ok(DcepMessage::Ack),
        DATA_CHANNEL_OPEN => {
            if data.len() < 12 {
                return Err(DcepError::Short(data.len()));
            }
            let channel_type = data[1];
            let reliability = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
            let label_length = usize::from(u16::from_be_bytes([data[8], data[9]]));
            let protocol_length = usize::from(u16::from_be_bytes([data[10], data[11]]));
            let rest = &data[12..];
            let label_end = label_length.min(rest.len());
            let label = String::from_utf8_lossy(&rest[..label_end]).into_owned();
            let protocol_start = label_end;
            let protocol_end = (protocol_start + protocol_length).min(rest.len());
            let protocol =
                String::from_utf8_lossy(&rest[protocol_start..protocol_end]).into_owned();
            let (max_retransmits, max_packet_life_time) = match channel_type & 0x03 {
                CHANNEL_TYPE_PARTIAL_RELIABLE_REXMIT => (Some(reliability), None),
                CHANNEL_TYPE_PARTIAL_RELIABLE_TIMED => (None, Some(reliability)),
                _ => (None, None),
            };
            Ok(DcepMessage::Open(DataChannelParameters {
                label,
                protocol,
                ordered: channel_type & CHANNEL_TYPE_UNORDERED == 0,
                max_retransmits,
                max_packet_life_time,
                negotiated: false,
                id: Some(stream_id),
            }))
        }
        other => Err(DcepError::UnknownType(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_round_trips() {
        let params = DataChannelParameters {
            label: "pointer".into(),
            protocol: "".into(),
            ordered: false,
            max_retransmits: Some(3),
            max_packet_life_time: None,
            negotiated: false,
            id: None,
        };
        let encoded = encode_open(&params);
        assert_eq!(encoded[1], 0x81);
        let decoded = decode(7, &encoded).unwrap();
        let DcepMessage::Open(got) = decoded else {
            panic!("not an open");
        };
        assert_eq!(got.label, "pointer");
        assert!(!got.ordered);
        assert_eq!(got.max_retransmits, Some(3));
        assert_eq!(got.id, Some(7));
    }

    #[test]
    fn a_label_that_is_not_utf8_is_taken_with_replacement() {
        let mut raw = vec![DATA_CHANNEL_OPEN, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0];
        raw.extend_from_slice(&[0xff, 0xcf]);
        let DcepMessage::Open(params) = decode(1, &raw).unwrap() else {
            panic!("not an open");
        };
        assert_eq!(params.label, "\u{fffd}\u{fffd}");
    }

    #[test]
    fn short_and_unknown_messages_are_errors() {
        assert_eq!(decode(0, &[]), Err(DcepError::Empty));
        assert_eq!(
            decode(0, &[DATA_CHANNEL_OPEN, 0, 0]),
            Err(DcepError::Short(3))
        );
        assert_eq!(decode(0, &[9]), Err(DcepError::UnknownType(9)));
        assert_eq!(decode(0, &encode_ack()), Ok(DcepMessage::Ack));
    }
}
