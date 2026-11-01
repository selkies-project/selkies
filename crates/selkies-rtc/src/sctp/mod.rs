/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! SCTP over DTLS and the data channels on it.
//!
//! The association is `rtc_sctp`'s, driven sans-IO by `Sctp` (`association`): the DTLS
//! transport hands it the SCTP packets it decrypted and takes the ones it produces through
//! `DtlsDatagrams`; `SctpTransport::spawn` (`driver`) runs that on a tokio task with the
//! association's timers. DCEP (`dcep`) and the channel bookkeeping are this crate's: a
//! client opens odd stream ids and a server even ones, a channel opened before the
//! association is established sends its `DATA_CHANNEL_OPEN` once it is, a message larger
//! than the peer's `a=max-message-size` is refused rather than sent, and a bulk transfer
//! paces itself on `buffered_amount` against `DATA_CHANNEL_BULK_HIGH_WATER`.
//!
//! Six behaviors of the Python association hold here through the vendored `rtc-sctp`
//! (`crates/selkies-rtc/vendor/rtc-sctp`, its diff against the published crate in
//! `rtc-sctp-selkies.patch`), since none has a knob in the published crate:
//!
//! - RTO.Min is 0.4 s with RTO.Initial 1 s, and the last DATA chunk of a burst carries the
//!   RFC 7053 I-bit, so a lone lost message (a cursor, a clipboard, a control reply) is
//!   repaired after 0.4 s rather than 1 s, and cwnd reopens without waiting out a delayed
//!   SACK (`timer.rs`, `pop_pending_data_chunks_to_send`).
//! - A malformed or premature chunk is dropped and the association kept; only an ABORT, or
//!   an ERROR during the handshake, ends it (`handle_inbound`, `handle_chunk`). A DATA chunk
//!   more than 65535 TSNs ahead, which no gap ack block could ever name, is dropped
//!   unbuffered (`handle_data`).
//! - A peer's label, protocol, or string message that is not UTF-8 is taken with
//!   replacement (`dcep`, `Sctp::on_message`).
//! - A SACK's gap blocks are merged into ranges and each chunk in flight looked up in them,
//!   never each TSN they span: a packet of blocks names millions
//!   (`process_selective_ack`).
//! - Reassembly is bounded at `MAX_REASSEMBLY_BYTES` across every stream, aborting past it,
//!   and DATA on a stream the peer's INIT never opened is acknowledged and discarded
//!   (`handle_data`, `TransportConfig::with_max_reassembly_bytes`).
//! - TSNs exactly 2^31 apart are neither ahead nor behind in every comparison, so a FORWARD
//!   TSN that far away is ignored, a held TSN that far away is a duplicate, gap blocks stay
//!   ascending and merged across the wrap, and a FORWARD TSN jumps the cumulative TSN rather
//!   than walking to it (`sna32gt`, `PayloadQueue`, `handle_forward_tsn`).
//!
//! The vendored crate also takes what the hostile cases found beyond those six: its chunk
//! parsers panicked on an error cause or a FORWARD TSN shorter than it claims, its arithmetic
//! overflowed at the TSN wrap, a SACK past anything sent left the ack point behind its queue
//! for good, a retransmitted INIT started a new association rather than being answered, and
//! duplicates were never reported. A lone pending SACK flushes after 20 ms rather than 200,
//! and DATA is retried until ICE or DTLS gives up on the peer rather than five times. A T3 expiry
//! doubles the RTO to 1 s at most, or the RTO the path last measured where that is longer, rather
//! than to the 60 s RFC 9260 allows, so input repairs at about a second after an outage rather than a
//! minute: this association carries input, and its congestion window is what protects the path.

pub mod association;
pub mod channel;
pub mod dcep;
pub mod driver;

pub use association::{
    ADVERTISED_RWND, DEFAULT_REMOTE_MAX_MESSAGE_SIZE, LOCAL_MAX_MESSAGE_SIZE, MAX_REASSEMBLY_BYTES,
    OpenError, SCTP_PORT, Sctp, SctpCapabilities, SctpEvent, SendError, TransportState,
};
pub use channel::{ChannelState, DataChannelParameters, Message};
pub use driver::{
    DATA_CHANNEL_BULK_CHUNK_SIZE, DATA_CHANNEL_BULK_HIGH_WATER, DataChannel, DtlsDatagrams,
    SctpTransport, adjusted_chunk_size,
};
pub use rtc_sctp::Side;

#[cfg(test)]
mod tests;
