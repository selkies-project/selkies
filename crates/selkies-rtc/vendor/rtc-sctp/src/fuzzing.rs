//! Entry points for fuzz targets and benchmarks.
//!
//! Each function drives one encode or decode step over a raw byte slice, so a fuzzer or a
//! benchmark can reach the packet codec without establishing an association first. Gated behind
//! `cfg(fuzzing)` or the `bench` feature; not part of the supported API and not subject to the
//! crate's stability guarantees.
use bytes::Bytes;
use shared::error::Result;

use crate::chunk::Chunk;
use crate::chunk::ErrorCause;
use crate::chunk::chunk_abort::ChunkAbort;
use crate::chunk::chunk_cookie_ack::ChunkCookieAck;
use crate::chunk::chunk_cookie_echo::ChunkCookieEcho;
use crate::chunk::chunk_error::ChunkError;
use crate::chunk::chunk_forward_tsn::{ChunkForwardTsn, ChunkForwardTsnStream};
use crate::chunk::chunk_header::ChunkHeader;
use crate::chunk::chunk_heartbeat::ChunkHeartbeat;
use crate::chunk::chunk_heartbeat_ack::ChunkHeartbeatAck;
use crate::chunk::chunk_init::ChunkInit;
use crate::chunk::chunk_payload_data::ChunkPayloadData;
use crate::chunk::chunk_reconfig::ChunkReconfig;
use crate::chunk::chunk_selective_ack::ChunkSelectiveAck;
use crate::chunk::chunk_shutdown::ChunkShutdown;
use crate::chunk::chunk_shutdown_ack::ChunkShutdownAck;
use crate::chunk::chunk_shutdown_complete::ChunkShutdownComplete;
use crate::packet::{Packet, PartialDecode};
use crate::param::Param;
use crate::param::param_chunk_list::ParamChunkList;
use crate::param::param_forward_tsn_supported::ParamForwardTsnSupported;
use crate::param::param_header::ParamHeader;
use crate::param::param_heartbeat_info::ParamHeartbeatInfo;
use crate::param::param_outgoing_reset_request::ParamOutgoingResetRequest;
use crate::param::param_random::ParamRandom;
use crate::param::param_reconfig_response::ParamReconfigResponse;
use crate::param::param_requested_hmac_algorithm::ParamRequestedHmacAlgorithm;
use crate::param::param_state_cookie::ParamStateCookie;
use crate::param::param_supported_extensions::ParamSupportedExtensions;
use crate::param::param_uknown::ParamUnknown;

fn to_bytes(data: &[u8]) -> Bytes {
    Bytes::copy_from_slice(data)
}

/// Unmarshals a raw SCTP packet, discarding the result.
pub fn packet_unmarshal(data: &[u8]) -> Result<()> {
    let raw = to_bytes(data);
    let _ = Packet::unmarshal(&raw)?;
    Ok(())
}

/// Differential/roundtrip helper: unmarshal a raw SCTP packet and marshal it
/// back to the wire. Exposed so fuzzers and benchmarks can exercise the
/// marshalling path (which is otherwise `pub(crate)`).
pub fn packet_marshal(data: &[u8]) -> Result<Vec<u8>> {
    let pkt = Packet::unmarshal(&to_bytes(data))?;
    Ok(pkt.marshal()?.to_vec())
}

/// Build the wire bytes of a representative DATA packet carrying `num_chunks`
/// payload-data chunks of `payload_len` bytes each. Used to seed marshalling
/// micro-benchmarks with a valid (correctly check-summed) packet.
#[doc(hidden)]
pub fn sample_data_packet(payload_len: usize, num_chunks: usize) -> Vec<u8> {
    use crate::chunk::chunk_payload_data::PayloadProtocolIdentifier;
    use crate::packet::CommonHeader;

    let chunks: Vec<Box<dyn Chunk>> = (0..num_chunks)
        .map(|i| {
            Box::new(ChunkPayloadData {
                beginning_fragment: true,
                ending_fragment: true,
                tsn: i as u32 + 1,
                stream_identifier: 0,
                stream_sequence_number: i as u16,
                payload_type: PayloadProtocolIdentifier::Binary,
                user_data: Bytes::from(vec![0x42u8; payload_len]),
                ..Default::default()
            }) as Box<dyn Chunk>
        })
        .collect();

    let pkt = Packet {
        common_header: CommonHeader {
            source_port: 5000,
            destination_port: 5000,
            verification_tag: 0x1234_5678,
        },
        chunks,
    };
    pkt.marshal().unwrap().to_vec()
}

/// Marshal a fixed, pre-parsed packet `iterations` times into a fresh buffer.
/// Isolated marshalling micro-benchmark surface for `perf`/`poop` (parsing is
/// done once, outside the timed loop).
#[doc(hidden)]
pub fn bench_packet_marshal(data: &[u8], iterations: u64) -> Result<usize> {
    use crate::packet::PACKET_HEADER_SIZE;
    use bytes::BytesMut;

    let pkt = Packet::unmarshal(&to_bytes(data))?;
    let mut last = 0;
    for _ in 0..iterations {
        let mut buf = BytesMut::with_capacity(data.len() + PACKET_HEADER_SIZE);
        last = pkt.marshal_to(&mut buf)?;
        std::hint::black_box(&buf);
    }
    Ok(last)
}

/// Unmarshals the common header, checksum, and first chunk header of a raw SCTP packet, as
/// an endpoint reads them before it decodes the rest, discarding the result.
pub fn packet_partial_decode_unmarshal(data: &[u8]) -> Result<()> {
    let raw = to_bytes(data);
    let _ = PartialDecode::unmarshal(&raw)?;
    Ok(())
}

/// Defines one entry point per chunk or parameter type, each unmarshaling a raw slice as that
/// type alone, without the packet around it.
macro_rules! unmarshal_entries {
    ($($name:ident => $ty:ty),+ $(,)?) => {
        $(
            #[doc = concat!("Unmarshals a raw slice as a `", stringify!($ty), "`, discarding the result.")]
            pub fn $name(data: &[u8]) -> Result<()> {
                let raw = to_bytes(data);
                let _ = <$ty>::unmarshal(&raw)?;
                Ok(())
            }
        )+
    };
}

unmarshal_entries! {
    chunk_chunk_header_unmarshal => ChunkHeader,
    chunk_chunk_abort_unmarshal => ChunkAbort,
    chunk_chunk_cookie_ack_unmarshal => ChunkCookieAck,
    chunk_chunk_cookie_echo_unmarshal => ChunkCookieEcho,
    chunk_chunk_error_unmarshal => ChunkError,
    chunk_chunk_forward_tsn_unmarshal => ChunkForwardTsn,
    chunk_chunk_forward_tsn_stream_unmarshal => ChunkForwardTsnStream,
    chunk_chunk_heartbeat_unmarshal => ChunkHeartbeat,
    chunk_chunk_heartbeat_ack_unmarshal => ChunkHeartbeatAck,
    chunk_chunk_init_unmarshal => ChunkInit,
    chunk_chunk_payload_data_unmarshal => ChunkPayloadData,
    chunk_chunk_reconfig_unmarshal => ChunkReconfig,
    chunk_chunk_selective_ack_unmarshal => ChunkSelectiveAck,
    chunk_chunk_shutdown_unmarshal => ChunkShutdown,
    chunk_chunk_shutdown_ack_unmarshal => ChunkShutdownAck,
    chunk_chunk_shutdown_complete_unmarshal => ChunkShutdownComplete,
    chunk_error_cause_unmarshal => ErrorCause,
    param_param_header_unmarshal => ParamHeader,
    param_param_forward_tsn_supported_unmarshal => ParamForwardTsnSupported,
    param_param_supported_extensions_unmarshal => ParamSupportedExtensions,
    param_param_random_unmarshal => ParamRandom,
    param_param_requested_hmac_algorithm_unmarshal => ParamRequestedHmacAlgorithm,
    param_param_chunk_list_unmarshal => ParamChunkList,
    param_param_state_cookie_unmarshal => ParamStateCookie,
    param_param_heartbeat_info_unmarshal => ParamHeartbeatInfo,
    param_param_outgoing_reset_request_unmarshal => ParamOutgoingResetRequest,
    param_param_reconfig_response_unmarshal => ParamReconfigResponse,
    param_param_unknown_unmarshal => ParamUnknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sample_data_packet_roundtrips() {
        // sample_data_packet builds a valid, check-summed packet; packet_marshal
        // unmarshals then re-marshals it, so the bytes must round-trip exactly.
        let pkt = sample_data_packet(1200, 2);
        assert_eq!(packet_marshal(&pkt).unwrap(), pkt);
        // exercise the perf-harness entry point as well.
        assert!(bench_packet_marshal(&pkt, 3).is_ok());
    }
}
