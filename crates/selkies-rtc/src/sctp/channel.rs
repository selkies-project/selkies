/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! The data channel as the engine sees it: its parameters, its messages, and its states.

use bytes::Bytes;

/// The configuration of one data channel, as `RTCDataChannelInit` names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataChannelParameters {
    /// A name describing the channel; labels need not be unique.
    pub label: String,
    /// The subprotocol name, empty for none.
    pub protocol: String,
    /// Whether the channel delivers its messages in order.
    pub ordered: bool,
    /// The most retransmissions attempted for a message; `None` for a reliable channel.
    pub max_retransmits: Option<u32>,
    /// How long transmissions of a message are attempted, in milliseconds; `None` for reliable.
    pub max_packet_life_time: Option<u32>,
    /// Whether the channel was negotiated out of band, both sides agreeing on `id`.
    pub negotiated: bool,
    /// The stream id, 0 to 65534; chosen by the transport when `None`.
    pub id: Option<u16>,
}

impl DataChannelParameters {
    /// A reliable channel with `label`, ordered or not, as the engine opens its channels.
    pub fn new(label: &str, ordered: bool) -> DataChannelParameters {
        DataChannelParameters {
            label: label.to_owned(),
            protocol: String::new(),
            ordered,
            max_retransmits: None,
            max_packet_life_time: None,
            negotiated: false,
            id: None,
        }
    }
}

/// One data channel message, a string or binary as the WebRTC PPIDs tell them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// A string message; one that was not UTF-8 arrives decoded with replacement.
    Text(String),
    /// A binary message.
    Binary(Bytes),
}

impl Message {
    /// The message's length in bytes on the wire.
    pub fn len(&self) -> usize {
        match self {
            Message::Text(text) => text.len(),
            Message::Binary(bytes) => bytes.len(),
        }
    }

    /// Whether the message carries no bytes.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// `RTCDataChannel.readyState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelState {
    /// Opened locally, its OPEN not yet acknowledged, or the association not yet established.
    Connecting,
    /// Messages flow.
    Open,
    /// A stream reset was asked for and not yet answered.
    Closing,
    /// The channel is gone; no event follows.
    Closed,
}

impl ChannelState {
    /// The state as the W3C names it.
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelState::Connecting => "connecting",
            ChannelState::Open => "open",
            ChannelState::Closing => "closing",
            ChannelState::Closed => "closed",
        }
    }
}
