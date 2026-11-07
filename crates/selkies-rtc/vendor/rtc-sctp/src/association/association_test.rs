use super::stream::ReliabilityType;
use super::*;

const ACCEPT_CH_SIZE: usize = 16;

fn create_association(config: TransportConfig) -> Association {
    Association::new(
        None,
        Arc::new(config),
        1400,
        0,
        SocketAddr::from_str("0.0.0.0:0").unwrap(),
        SocketAddr::from_str("0.0.0.0:0").unwrap(),
        TransportProtocol::UDP,
        Instant::now(),
    )
}

// `create_forward_tsn` no longer rescans the in-flight window; it emits the
// `fwd_tsn_stream_map` that the RFC 3758 C2 loops fill via
// `note_abandoned_for_forward_tsn` as each chunk is abandoned. These unit tests
// drive that same entry point directly (ascending TSN, as C2 would); the full
// window/SACK-driven path is exercised end-to-end by the `endpoint_test`
// `test_assoc_unreliable_rexmit_*` suite.
#[test]
fn test_create_forward_tsn_forward_one_abandoned() -> Result<()> {
    let mut a = Association::default();

    a.cumulative_tsn_ack_point = 9;
    a.advanced_peer_tsn_ack_point = 10;
    // tsn=10, ordered, si=1, ssn=2
    a.note_abandoned_for_forward_tsn(false, 1, 2);

    let fwdtsn = a.create_forward_tsn();

    assert_eq!(10, fwdtsn.new_cumulative_tsn, "should be able to serialize");
    assert_eq!(1, fwdtsn.streams.len(), "there should be one stream");
    assert_eq!(1, fwdtsn.streams[0].identifier, "si should be 1");
    assert_eq!(2, fwdtsn.streams[0].sequence, "ssn should be 2");

    Ok(())
}

#[test]
fn test_create_forward_tsn_forward_two_abandoned_with_the_same_si() -> Result<()> {
    let mut a = Association::default();

    a.cumulative_tsn_ack_point = 9;
    a.advanced_peer_tsn_ack_point = 12;
    a.note_abandoned_for_forward_tsn(false, 1, 2); // tsn=10
    a.note_abandoned_for_forward_tsn(false, 1, 3); // tsn=11 -> greatest SSN for si=1
    a.note_abandoned_for_forward_tsn(false, 2, 1); // tsn=12

    let fwdtsn = a.create_forward_tsn();

    assert_eq!(12, fwdtsn.new_cumulative_tsn, "should be able to serialize");
    assert_eq!(2, fwdtsn.streams.len(), "there should be two stream");

    let mut si1ok = false;
    let mut si2ok = false;
    for s in &fwdtsn.streams {
        match s.identifier {
            1 => {
                assert_eq!(3, s.sequence, "ssn should be 3");
                si1ok = true;
            }
            2 => {
                assert_eq!(1, s.sequence, "ssn should be 1");
                si2ok = true;
            }
            _ => assert!(false, "unexpected stream indentifier"),
        }
    }
    assert!(si1ok, "si=1 should be present");
    assert!(si2ok, "si=2 should be present");

    Ok(())
}

#[test]
fn test_create_forward_tsn_omits_unordered_streams() -> Result<()> {
    // Unordered chunks carry no meaningful stream-sequence-number: the receiver
    // advances unordered streams purely by `new_cumulative_tsn` and ignores the
    // per-stream list (see handle_forward_tsn), so create_forward_tsn must not
    // report them — only ordered streams contribute.
    let mut a = Association::default();

    a.cumulative_tsn_ack_point = 9;
    a.advanced_peer_tsn_ack_point = 11;
    a.note_abandoned_for_forward_tsn(true, 1, 5); // unordered -> omitted
    a.note_abandoned_for_forward_tsn(false, 2, 7); // ordered   -> reported

    let fwdtsn = a.create_forward_tsn();

    assert_eq!(11, fwdtsn.new_cumulative_tsn);
    assert_eq!(
        1,
        fwdtsn.streams.len(),
        "only the ordered stream is reported"
    );
    assert_eq!(2, fwdtsn.streams[0].identifier, "si should be 2");
    assert_eq!(7, fwdtsn.streams[0].sequence, "ssn should be 7");

    Ok(())
}

// The allocation-avoiding marshal_control_chunk() must produce byte-identical wire
// output to create_packet(vec![Box::new(chunk)]).marshal(). A 2-stream FORWARD-TSN
// also exercises ChunkForwardTsn::marshal_to's per-stream marshal_to loop, and a
// SACK covers the other call site.
#[test]
fn test_marshal_control_chunk_byte_identical_to_create_packet() -> Result<()> {
    let mut a = Association::default();
    a.peer_verification_tag = 0x1234_5678;
    a.source_port = 5000;
    a.destination_port = 5001;

    let fwd_tsn = ChunkForwardTsn {
        new_cumulative_tsn: 42,
        streams: vec![
            ChunkForwardTsnStream {
                identifier: 1,
                sequence: 7,
            },
            ChunkForwardTsnStream {
                identifier: 3,
                sequence: 9,
            },
        ],
    };
    // Borrow for the helper, then move into create_packet (chunks are not Clone).
    let via_helper = a.marshal_control_chunk(&fwd_tsn)?;
    let via_packet = a.create_packet(vec![Box::new(fwd_tsn)]).marshal()?;
    assert_eq!(
        via_helper, via_packet,
        "FORWARD-TSN: marshal_control_chunk must match create_packet(..).marshal()"
    );

    let sack = a.create_selective_ack_chunk();
    let sack_via_helper = a.marshal_control_chunk(&sack)?;
    let sack_via_packet = a.create_packet(vec![Box::new(sack)]).marshal()?;
    assert_eq!(
        sack_via_helper, sack_via_packet,
        "SACK: marshal_control_chunk must match create_packet(..).marshal()"
    );

    Ok(())
}

#[test]
fn test_handle_forward_tsn_forward_3unreceived_chunks() -> Result<()> {
    let mut a = Association::default();

    a.use_forward_tsn = true;
    let prev_tsn = a.peer_last_tsn;

    let fwdtsn = ChunkForwardTsn {
        new_cumulative_tsn: a.peer_last_tsn + 3,
        streams: vec![ChunkForwardTsnStream {
            identifier: 0,
            sequence: 0,
        }],
    };

    let p = a.handle_forward_tsn(&fwdtsn)?;

    let delayed_ack_triggered = a.delayed_ack_triggered;
    let immediate_ack_triggered = a.immediate_ack_triggered;
    assert_eq!(
        a.peer_last_tsn,
        prev_tsn + 3,
        "peerLastTSN should advance by 3 "
    );
    assert!(delayed_ack_triggered, "delayed sack should be triggered");
    assert!(
        !immediate_ack_triggered,
        "immediate sack should NOT be triggered"
    );
    assert!(p.is_empty(), "should return empty");

    Ok(())
}

#[test]
fn test_handle_forward_tsn_forward_1for1_missing() -> Result<()> {
    let mut a = Association::default();

    a.use_forward_tsn = true;
    let prev_tsn = a.peer_last_tsn;

    // this chunk is blocked by the missing chunk at tsn=1
    a.payload_queue.push(
        ChunkPayloadData {
            beginning_fragment: true,
            ending_fragment: true,
            tsn: a.peer_last_tsn + 2,
            stream_identifier: 0,
            stream_sequence_number: 1,
            user_data: Bytes::from_static(b"ABC"),
            ..Default::default()
        },
        a.peer_last_tsn,
    );

    let fwdtsn = ChunkForwardTsn {
        new_cumulative_tsn: a.peer_last_tsn + 1,
        streams: vec![ChunkForwardTsnStream {
            identifier: 0,
            sequence: 1,
        }],
    };

    let p = a.handle_forward_tsn(&fwdtsn)?;

    let delayed_ack_triggered = a.delayed_ack_triggered;
    let immediate_ack_triggered = a.immediate_ack_triggered;
    assert_eq!(
        a.peer_last_tsn,
        prev_tsn + 2,
        "peerLastTSN should advance by 2"
    );
    assert!(delayed_ack_triggered, "delayed sack should be triggered");
    assert!(
        !immediate_ack_triggered,
        "immediate sack should NOT be triggered"
    );
    assert!(p.is_empty(), "should return empty");

    Ok(())
}

#[test]
fn test_handle_forward_tsn_forward_1for2_missing() -> Result<()> {
    let mut a = Association::default();

    a.use_forward_tsn = true;
    let prev_tsn = a.peer_last_tsn;

    // this chunk is blocked by the missing chunk at tsn=1
    a.payload_queue.push(
        ChunkPayloadData {
            beginning_fragment: true,
            ending_fragment: true,
            tsn: a.peer_last_tsn + 3,
            stream_identifier: 0,
            stream_sequence_number: 1,
            user_data: Bytes::from_static(b"ABC"),
            ..Default::default()
        },
        a.peer_last_tsn,
    );

    let fwdtsn = ChunkForwardTsn {
        new_cumulative_tsn: a.peer_last_tsn + 1,
        streams: vec![ChunkForwardTsnStream {
            identifier: 0,
            sequence: 1,
        }],
    };

    let p = a.handle_forward_tsn(&fwdtsn)?;

    let immediate_ack_triggered = a.immediate_ack_triggered;
    assert_eq!(
        a.peer_last_tsn,
        prev_tsn + 1,
        "peerLastTSN should advance by 1"
    );
    assert!(
        immediate_ack_triggered,
        "immediate sack should be triggered"
    );
    assert!(p.is_empty(), "should return empty");

    Ok(())
}

#[test]
fn test_handle_forward_tsn_dup_forward_tsn_chunk_should_generate_sack() -> Result<()> {
    let mut a = Association::default();

    a.use_forward_tsn = true;
    let prev_tsn = a.peer_last_tsn;

    let fwdtsn = ChunkForwardTsn {
        new_cumulative_tsn: a.peer_last_tsn,
        streams: vec![ChunkForwardTsnStream {
            identifier: 0,
            sequence: 1,
        }],
    };

    let p = a.handle_forward_tsn(&fwdtsn)?;

    let ack_state = a.ack_state;
    assert_eq!(a.peer_last_tsn, prev_tsn, "peerLastTSN should not advance");
    assert_eq!(AckState::Immediate, ack_state, "sack should be requested");
    assert!(p.is_empty(), "should return empty");

    Ok(())
}

#[test]
fn test_assoc_create_new_stream() -> Result<()> {
    let mut a = Association::default();

    for i in 0..ACCEPT_CH_SIZE {
        let stream_identifier =
            if let Some(s) = a.create_stream(i as u16, true, PayloadProtocolIdentifier::Unknown) {
                s.stream_identifier
            } else {
                assert!(false, "{} should success", i);
                0
            };
        let result = a.streams.get(&stream_identifier);
        assert!(result.is_some(), "should be in a.streams map");
    }

    let new_si = ACCEPT_CH_SIZE as u16;
    let result = a.streams.get(&new_si);
    assert!(result.is_none(), "should NOT be in a.streams map");

    let to_be_ignored = ChunkPayloadData {
        beginning_fragment: true,
        ending_fragment: true,
        tsn: a.peer_last_tsn + 1,
        stream_identifier: new_si,
        user_data: Bytes::from_static(b"ABC"),
        ..Default::default()
    };

    let p = a.handle_data(&to_be_ignored)?;
    assert!(p.is_empty(), "should return empty");

    Ok(())
}

fn handle_init_test(name: &str, initial_state: AssociationState, expect_err: bool) {
    let mut a = create_association(TransportConfig::default());
    a.set_state(initial_state);
    let pkt = Packet {
        common_header: CommonHeader {
            source_port: 5001,
            destination_port: 5002,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut init = ChunkInit {
        initial_tsn: 1234,
        num_outbound_streams: 1001,
        num_inbound_streams: 1002,
        initiate_tag: 5678,
        advertised_receiver_window_credit: 512 * 1024,
        ..Default::default()
    };
    init.set_supported_extensions();

    let result = a.handle_init(&pkt, &init);
    if expect_err {
        assert!(result.is_err(), "{} should fail", name);
        return;
    } else {
        assert!(result.is_ok(), "{} should be ok", name);
    }
    assert_eq!(
        if init.initial_tsn == 0 {
            u32::MAX
        } else {
            init.initial_tsn - 1
        },
        a.peer_last_tsn,
        "{} should match",
        name
    );
    assert_eq!(1001, a.my_max_num_outbound_streams, "{} should match", name);
    assert_eq!(1002, a.my_max_num_inbound_streams, "{} should match", name);
    assert_eq!(5678, a.peer_verification_tag, "{} should match", name);
    assert_eq!(
        pkt.common_header.source_port, a.destination_port,
        "{} should match",
        name
    );
    assert_eq!(
        pkt.common_header.destination_port, a.source_port,
        "{} should match",
        name
    );
    assert!(a.use_forward_tsn, "{} should be set to true", name);
}

// W3C `RTCSctpTransport.maxChannels` is the minimum of the negotiated inbound and outbound
// stream counts, and is null until the association is established. `handle_init` is where the
// negotiation happens: the peer's advertised counts narrow this endpoint's configured ones.
#[test]
fn test_assoc_negotiated_max_streams() -> Result<()> {
    let mut a = create_association(TransportConfig::default());

    // Before the handshake completes the two counts still hold the *configured* limits, which
    // were never agreed with anyone. Reporting them would overstate the association.
    assert!(a.is_handshaking());
    assert_eq!(
        None,
        a.negotiated_max_streams(),
        "a handshaking association has negotiated nothing yet"
    );

    a.set_state(AssociationState::Closed);
    let pkt = Packet {
        common_header: CommonHeader {
            source_port: 5001,
            destination_port: 5002,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut init = ChunkInit {
        initial_tsn: 1234,
        num_outbound_streams: 1001,
        num_inbound_streams: 1002,
        initiate_tag: 5678,
        advertised_receiver_window_credit: 512 * 1024,
        ..Default::default()
    };
    init.set_supported_extensions();
    a.handle_init(&pkt, &init)?;

    // `handle_init` narrowed the counts to 1001 outbound / 1002 inbound (see
    // `handle_init_test`), but the handshake is still in flight.
    assert_eq!(
        None,
        a.negotiated_max_streams(),
        "still handshaking after INIT, so still nothing to report"
    );

    a.handshake_completed = true;
    assert_eq!(
        Some(1001),
        a.negotiated_max_streams(),
        "the smaller of 1001 outbound and 1002 inbound"
    );

    Ok(())
}

#[test]
fn test_assoc_handle_init() -> Result<()> {
    handle_init_test("normal", AssociationState::Closed, false);

    handle_init_test(
        "unexpected state established",
        AssociationState::Established,
        true,
    );

    handle_init_test(
        "unexpected state shutdownAckSent",
        AssociationState::ShutdownAckSent,
        true,
    );

    handle_init_test(
        "unexpected state shutdownPending",
        AssociationState::ShutdownPending,
        true,
    );

    handle_init_test(
        "unexpected state shutdownReceived",
        AssociationState::ShutdownReceived,
        true,
    );

    handle_init_test(
        "unexpected state shutdownSent",
        AssociationState::ShutdownSent,
        true,
    );

    Ok(())
}

#[test]
fn test_assoc_max_message_size_default() -> Result<()> {
    let mut a = create_association(TransportConfig::default().with_max_message_size(65536));
    assert_eq!(65536, a.max_message_size, "should match");

    let ppi = PayloadProtocolIdentifier::Unknown;
    let stream = a.create_stream(1, false, ppi);
    assert!(stream.is_some(), "should succeed");

    if let Some(mut s) = stream {
        let p = Bytes::from(vec![0u8; 65537]);

        if let Err(err) = s.write_sctp(Instant::now(), &p.slice(..65536), ppi) {
            assert_ne!(
                Error::ErrOutboundPacketTooLarge,
                err,
                "should be not Error::ErrOutboundPacketTooLarge"
            );
        } else {
            assert!(false, "should be error");
        }

        if let Err(err) = s.write_sctp(Instant::now(), &p.slice(..65537), ppi) {
            assert_eq!(
                Error::ErrOutboundPacketTooLarge,
                err,
                "should be Error::ErrOutboundPacketTooLarge"
            );
        } else {
            assert!(false, "should be error");
        }
    }

    Ok(())
}

#[test]
fn test_assoc_max_message_size_explicit() -> Result<()> {
    let mut a = create_association(TransportConfig::default().with_max_message_size(30000));

    assert_eq!(30000, a.max_message_size, "should match");

    let ppi = PayloadProtocolIdentifier::Unknown;
    let stream = a.create_stream(1, false, ppi);
    assert!(stream.is_some(), "should succeed");

    if let Some(mut s) = stream {
        let p = Bytes::from(vec![0u8; 30001]);

        if let Err(err) = s.write_sctp(Instant::now(), &p.slice(..30000), ppi) {
            assert_ne!(
                Error::ErrOutboundPacketTooLarge,
                err,
                "should be not Error::ErrOutboundPacketTooLarge"
            );
        } else {
            assert!(false, "should be error");
        }

        if let Err(err) = s.write_sctp(Instant::now(), &p.slice(..30001), ppi) {
            assert_eq!(
                Error::ErrOutboundPacketTooLarge,
                err,
                "should be Error::ErrOutboundPacketTooLarge"
            );
        } else {
            assert!(false, "should be error");
        }
    }

    Ok(())
}

// The MTU-split rule in `bundle_data_chunks_into_packets` must bound every
// marshalled datagram: bundle decisions are made on padded wire sizes (the
// chunk header + payload, rounded up to the SCTP 4-byte boundary), never on
// raw payload sizes. These tests marshal real packets and measure the emitted
// bytes, so any accounting drift fails here rather than as silent on-path
// datagram loss.

use crate::EndpointConfig;
use crate::config::{INITIAL_MTU, max_payload_size_for_mtu};

fn create_association_with_mtu(mtu: u32) -> Association {
    Association::new(
        None,
        Arc::new(TransportConfig::default()),
        mtu - (COMMON_HEADER_SIZE + DATA_CHUNK_HEADER_SIZE),
        0,
        SocketAddr::from_str("0.0.0.0:0").unwrap(),
        SocketAddr::from_str("0.0.0.0:0").unwrap(),
        TransportProtocol::UDP,
        Instant::now(),
    )
}

fn payload_chunk(tsn: u32, len: usize) -> ChunkPayloadData {
    ChunkPayloadData {
        beginning_fragment: true,
        ending_fragment: true,
        tsn,
        stream_identifier: 0,
        stream_sequence_number: 0,
        user_data: Bytes::from(vec![0u8; len]),
        ..Default::default()
    }
}

#[test]
fn bundle_split_decides_on_padded_wire_sizes() {
    // Payloads of 1147 + 16 bytes pass a payload-only boundary check at
    // exactly an MTU of 1191, but the two chunks marshal to
    // 12 + 1164 + 32 = 1208 bytes — past the MTU the association promised.
    let a = create_association_with_mtu(1191);
    let chunks = vec![payload_chunk(1, 1147), payload_chunk(2, 16)];
    let mut raw_packets = vec![];
    a.bundle_data_chunks_into_packets(chunks, &mut raw_packets);
    assert_eq!(
        raw_packets.len(),
        2,
        "a bundle that only fits unpadded must split"
    );
    for p in &raw_packets {
        assert!(
            p.len() as u32 <= a.mtu,
            "emitted packet is {} bytes, mtu is {}",
            p.len(),
            a.mtu
        );
    }
}

#[test]
fn single_maximum_size_chunk_marshals_within_initial_mtu() {
    let max_payload = EndpointConfig::default().get_max_payload_size();
    let a = create_association_with_mtu(max_payload + COMMON_HEADER_SIZE + DATA_CHUNK_HEADER_SIZE);
    let chunks = vec![payload_chunk(1, max_payload as usize)];
    let mut raw_packets = vec![];
    a.bundle_data_chunks_into_packets(chunks, &mut raw_packets);
    assert_eq!(raw_packets.len(), 1);
    assert!(
        raw_packets[0].len() as u32 <= INITIAL_MTU,
        "a single maximum-size chunk is {} bytes, INITIAL_MTU is {}",
        raw_packets[0].len(),
        INITIAL_MTU
    );
}

#[test]
fn single_maximum_size_chunk_marshals_within_a_custom_mtu() {
    // `max_payload_size_for_mtu(N)` derives a payload budget for which a
    // single maximum-size DATA chunk marshals to at most N bytes, mirroring
    // the default derivation's guarantee for INITIAL_MTU. Prove it on
    // emitted bytes at the 32-byte floor (exactly the smallest
    // representable padded DATA packet), across
    // a 4-byte padding boundary (1201 emits 1200), and at a typical larger
    // path MTU (1500, fully used).
    for (mtu, expected) in [(32u32, 32usize), (1201, 1200), (1500, 1500)] {
        let max_payload = max_payload_size_for_mtu(mtu);
        let a =
            create_association_with_mtu(max_payload + COMMON_HEADER_SIZE + DATA_CHUNK_HEADER_SIZE);
        let chunks = vec![payload_chunk(1, max_payload as usize)];
        let mut raw_packets = vec![];
        a.bundle_data_chunks_into_packets(chunks, &mut raw_packets);
        assert_eq!(raw_packets.len(), 1);
        assert_eq!(
            raw_packets[0].len(),
            expected,
            "a single maximum-size chunk under mtu({mtu})"
        );
    }
}

#[test]
fn initial_cwnd_is_computed_safely_for_non_default_mtus() {
    // Regression: the initial congestion window was computed as
    // `(2 * mtu).clamp(4380, 4 * mtu)`, which panicked for effective MTUs
    // below ~1095 (`Ord::clamp` with min > max) and overflowed near
    // `u32::MAX` — reachable through `EndpointConfig::max_payload_size`
    // and MTU-derived payload budgets. RFC 4960 §7.2.1:
    // cwnd = min(4*MTU, max(2*MTU, 4380)); for a small MTU that is 4*MTU.
    let a = create_association_with_mtu(100);
    assert_eq!(a.cwnd, 4 * a.mtu);

    // Directly via the low-level `max_payload_size` setter path: the
    // effective-MTU reconstruction (`max_payload_size + 28`) must saturate
    // rather than overflow when the payload budget itself is near
    // `u32::MAX` (the helper above subtracts the headers first, so it
    // never exercises this addition's edge).
    let a = Association::new(
        None,
        Arc::new(TransportConfig::default()),
        u32::MAX,
        0,
        SocketAddr::from_str("0.0.0.0:0").unwrap(),
        SocketAddr::from_str("0.0.0.0:0").unwrap(),
        TransportProtocol::UDP,
        Instant::now(),
    );
    assert_eq!(a.mtu, u32::MAX);
    assert_eq!(a.cwnd, u32::MAX);
}

#[test]
fn many_small_chunks_bundle_within_mtu() {
    // Per-chunk padding (17-byte payloads: 33 raw, 36 padded) compounds; a
    // payload-only accounting packed bundles that marshalled well past the
    // MTU once dozens of small chunks rode one flight.
    let a = create_association_with_mtu(1191);
    let chunks: Vec<ChunkPayloadData> = (0..60).map(|i| payload_chunk(i, 17)).collect();
    let mut raw_packets = vec![];
    a.bundle_data_chunks_into_packets(chunks, &mut raw_packets);
    assert!(raw_packets.len() >= 2);
    let total: usize = raw_packets.iter().map(|p| p.len()).sum();
    for p in &raw_packets {
        assert!(
            p.len() as u32 <= a.mtu,
            "emitted packet is {} bytes, mtu is {}",
            p.len(),
            a.mtu
        );
    }
    // Nothing was dropped: at least every chunk's unpadded wire size plus one
    // common header per emitted packet.
    let min_expected = 60 * (DATA_CHUNK_HEADER_SIZE as usize + 17)
        + raw_packets.len() * COMMON_HEADER_SIZE as usize;
    assert!(total >= min_expected);
}

fn timed_test_association() -> Association {
    let mut a = create_association(TransportConfig::default());
    a.control_queue.clear();
    a.timers.stop(Timer::T1Init);
    a.set_state(AssociationState::Established);
    a.use_forward_tsn = true;
    a.rwnd = 65536;
    a.rto_mgr.set_rto(1000, true);
    a
}

fn data_chunks(packets: &[Bytes]) -> usize {
    packets
        .iter()
        .map(|raw| {
            Packet::unmarshal(raw)
                .unwrap()
                .chunks
                .iter()
                .filter(|c| c.as_any().is::<ChunkPayloadData>())
                .count()
        })
        .sum()
}

fn transmitted_data(packets: &[Bytes]) -> Vec<ChunkPayloadData> {
    packets
        .iter()
        .flat_map(|raw| Packet::unmarshal(raw).unwrap().chunks)
        .filter_map(|c| c.as_any().downcast_ref::<ChunkPayloadData>().cloned())
        .collect()
}

#[test]
fn test_queued_message_keeps_its_original_reliability_policy() -> Result<()> {
    use ReliabilityType::{Reliable, Rexmit, Timed};
    for (original, value, replacement, ppi, forward_tsn, retransmit) in [
        (
            Reliable,
            0,
            Rexmit,
            PayloadProtocolIdentifier::Binary,
            true,
            true,
        ),
        (
            Rexmit,
            0,
            Reliable,
            PayloadProtocolIdentifier::Binary,
            true,
            false,
        ),
        (
            Rexmit,
            2,
            Rexmit,
            PayloadProtocolIdentifier::Binary,
            true,
            true,
        ),
        (
            Timed,
            100,
            Reliable,
            PayloadProtocolIdentifier::Binary,
            true,
            false,
        ),
        (
            Rexmit,
            0,
            Reliable,
            PayloadProtocolIdentifier::Dcep,
            true,
            true,
        ),
        (
            Rexmit,
            0,
            Reliable,
            PayloadProtocolIdentifier::Binary,
            false,
            true,
        ),
    ] {
        let mut a = timed_test_association();
        a.use_forward_tsn = forward_tsn;
        let now = Instant::now();
        let mut stream = a.open_stream(1, ppi)?;
        stream.set_reliability_params(false, original, value)?;
        stream.write_sctp(now, &Bytes::from_static(b"queued"), ppi)?;
        // Policy is captured at enqueue, before even the first transmission.
        stream.set_reliability_params(false, replacement, 0)?;
        assert_eq!(1, data_chunks(&a.gather_outbound(now).0));
        let at = a.timers.get(Timer::T3RTX).unwrap();
        a.handle_timeout(at);
        assert_eq!(
            usize::from(retransmit),
            data_chunks(&a.gather_outbound(at).0),
            "original={original:?} value={value} replacement={replacement:?} ppi={ppi:?} forward_tsn={forward_tsn}"
        );
        assert_eq!(
            if retransmit { 6 } else { 0 },
            a.stream(1)?.buffered_amount()?
        );
    }
    Ok(())
}

#[test]
fn test_rexmit_one_waits_for_ack_of_first_retry() -> Result<()> {
    let mut a = timed_test_association();
    let now = Instant::now();
    let ppi = PayloadProtocolIdentifier::Binary;
    let mut s = a.open_stream(1, ppi)?;
    s.set_reliability_params(false, ReliabilityType::Rexmit, 1)?;
    s.write_sctp(now, &Bytes::from(vec![0x55; 2000]), ppi)?;
    let initial = transmitted_data(&a.gather_outbound(now).0);
    assert_eq!(2, initial.len());
    let at = a.poll_timeout().unwrap();
    a.handle_timeout(at);
    let first_retry = transmitted_data(&a.gather_outbound(at).0);
    assert_eq!(1, first_retry.len());
    assert_eq!(initial[0].tsn, first_retry[0].tsn);
    // The normal driver drains poll_transmit until None at the same instant.
    let more = a.gather_outbound(at).0;
    assert!(
        a.inflight_queue
            .get(initial[0].tsn)
            .unwrap()
            .is_outstanding(),
        "a just-retried fragment must wait for ACK or a new loss indication; second drain emitted {more:?}"
    );
    a.handle_sack(
        &ChunkSelectiveAck {
            cumulative_tsn_ack: initial[0].tsn,
            advertised_receiver_window_credit: 65536,
            ..Default::default()
        },
        at + Duration::from_millis(10),
    )?;
    // The tail may be emitted by either drain. In both cases each fragment
    // gets its permitted retry and remains outstanding until the peer ACKs it.
    let mut tail_retry = transmitted_data(&more);
    tail_retry.extend(transmitted_data(
        &a.gather_outbound(at + Duration::from_millis(10)).0,
    ));
    assert_eq!(1, tail_retry.len());
    assert_eq!(initial[1].tsn, tail_retry[0].tsn);
    assert!(
        a.inflight_queue
            .get(initial[1].tsn)
            .unwrap()
            .is_outstanding()
    );
    let mut receiver = timed_test_association();
    receiver.peer_last_tsn = initial[0].tsn - 1;
    for chunk in first_retry.iter().chain(tail_retry.iter()) {
        receiver.handle_data(chunk)?;
    }
    assert_eq!(2000, receiver.stream(1)?.read_sctp()?.unwrap().len());
    a.handle_sack(
        &receiver.create_selective_ack_chunk(),
        at + Duration::from_millis(20),
    )?;
    assert!(a.inflight_queue.is_empty());
    Ok(())
}

#[test]
fn test_fast_retry_does_not_abandon_unsent_fragment_tail_of_new_rexmit_zero() -> Result<()> {
    for policy in [ReliabilityType::Rexmit, ReliabilityType::Timed] {
        let mut a = timed_test_association();
        let now = Instant::now();
        let ppi = PayloadProtocolIdentifier::Binary;
        a.open_stream(1, ppi)?
            .write_sctp(now, &Bytes::from_static(b"lost reliable"), ppi)?;
        let old = transmitted_data(&a.gather_outbound(now).0).remove(0);
        // Three real gap ACK reports request a fast retransmission for stream 1.
        for _ in 0..3 {
            a.stream(1)?
                .write_sctp(now, &Bytes::from_static(b"received"), ppi)?;
        }
        a.gather_outbound(now);
        for end in 2..=4 {
            a.handle_sack(
                &ChunkSelectiveAck {
                    cumulative_tsn_ack: old.tsn - 1,
                    advertised_receiver_window_credit: 65536,
                    gap_ack_blocks: vec![crate::chunk::chunk_selective_ack::GapAckBlock {
                        start: 2,
                        end,
                    }],
                    ..Default::default()
                },
                now,
            )?;
        }
        assert!(a.will_retransmit_fast);
        let mut fresh = a.open_stream(2, ppi)?;
        fresh.set_reliability_params(false, policy, 0)?;
        fresh.write_sctp(now, &Bytes::from(vec![0x33; 16000]), ppi)?;
        let sent = a.gather_outbound(now).0;
        assert!(
            transmitted_data(&sent)
                .iter()
                .any(|c| c.stream_identifier == 1)
        );
        assert!(
            transmitted_data(&sent)
                .iter()
                .any(|c| c.stream_identifier == 2)
        );
        assert!(
            !a.pending_queue.is_empty(),
            "an unrelated fast retry discarded never-sent fragments of fresh DATA"
        );
        assert_eq!(16000, a.stream(2)?.buffered_amount()?);
    }
    Ok(())
}

#[test]
fn test_timed_abandonment_preserves_t3_restart_for_outstanding_data() -> Result<()> {
    for gap_ack in [false, true] {
        let mut a = timed_test_association();
        a.rto_mgr.set_rto(1000, false);
        let now = Instant::now();
        let first_tsn = a.my_next_tsn;
        let ppi = PayloadProtocolIdentifier::Binary;
        let mut stream = a.open_stream(1, ppi)?;
        stream.set_reliability_params(false, ReliabilityType::Timed, 100)?;
        stream.write_sctp(now, &Bytes::from_static(b"lost"), ppi)?;
        let mut stream = a.open_stream(2, ppi)?;
        for _ in 0..2 {
            stream.write_sctp(now, &Bytes::from(vec![0; 1000]), ppi)?;
        }
        a.gather_outbound(now);
        let timeout = a.poll_timeout().unwrap();
        a.handle_timeout(timeout);
        assert_eq!(1, data_chunks(&a.gather_outbound(timeout).0));
        let at = timeout + Duration::from_millis(100);
        let sack = ChunkSelectiveAck {
            cumulative_tsn_ack: if gap_ack {
                first_tsn - 1
            } else {
                first_tsn + 1
            },
            advertised_receiver_window_credit: 65536,
            gap_ack_blocks: if gap_ack {
                vec![crate::chunk::chunk_selective_ack::GapAckBlock { start: 2, end: 2 }]
            } else {
                vec![]
            },
            ..Default::default()
        };
        a.handle_sack(&sack, at)?;
        assert_eq!(Some(at + Duration::from_secs(2)), a.poll_timeout());
        a.handle_sack(&sack, at + Duration::from_millis(100))?;
        assert_eq!(
            Some(at + Duration::from_secs(2)),
            a.poll_timeout(),
            "duplicate SACK must not restart T3"
        );
    }
    Ok(())
}

#[test]
fn test_timed_retransmit_deadline_and_reliable_exemptions() -> Result<()> {
    let lifetime = Duration::from_millis(100);
    for (policy, ppi, forward_tsn, elapsed, retransmit) in [
        (
            ReliabilityType::Timed,
            PayloadProtocolIdentifier::Binary,
            true,
            lifetime - Duration::from_nanos(1),
            true,
        ),
        (
            ReliabilityType::Timed,
            PayloadProtocolIdentifier::Binary,
            true,
            lifetime,
            false,
        ),
        (
            ReliabilityType::Timed,
            PayloadProtocolIdentifier::Binary,
            true,
            Duration::from_millis(u32::MAX as u64 + 1),
            false,
        ),
        (
            ReliabilityType::Timed,
            PayloadProtocolIdentifier::Dcep,
            true,
            lifetime,
            true,
        ),
        (
            ReliabilityType::Timed,
            PayloadProtocolIdentifier::Binary,
            false,
            lifetime,
            true,
        ),
        (
            ReliabilityType::Reliable,
            PayloadProtocolIdentifier::Binary,
            true,
            lifetime,
            true,
        ),
    ] {
        let mut a = timed_test_association();
        a.use_forward_tsn = forward_tsn;
        let now = Instant::now();
        let mut stream = a.open_stream(1, ppi)?;
        stream.set_reliability_params(true, policy, 100)?;
        stream.write_sctp(now, &Bytes::from_static(b"test"), ppi)?;
        assert_eq!(1, data_chunks(&a.gather_outbound(now).0));
        a.inflight_queue.mark_all_to_retrasmit();
        a.t3_retransmit_pending = true;
        assert_eq!(
            usize::from(retransmit),
            data_chunks(&a.gather_outbound(now + elapsed).0)
        );
        assert_eq!(if retransmit { 4 } else { 0 }, a.buffered_amount());
    }
    Ok(())
}

#[test]
fn test_timed_abandonment_covers_pending_tail_and_retries_forward_tsn() -> Result<()> {
    for unordered in [false, true] {
        for ack_prefix in [false, true] {
            let mut a = timed_test_association();
            a.cwnd = a.max_payload_size;
            let now = Instant::now();
            let first_tsn = a.my_next_tsn;
            let ppi = PayloadProtocolIdentifier::Binary;
            let mut stream = a.open_stream(1, ppi)?;
            stream.set_reliability_params(unordered, ReliabilityType::Timed, 100)?;
            stream.write_sctp(now, &Bytes::from(vec![0; 4000]), ppi)?;
            assert_eq!(1, data_chunks(&a.gather_outbound(now).0));
            assert!(!a.pending_queue.is_empty());
            if ack_prefix {
                a.handle_sack(
                    &ChunkSelectiveAck {
                        cumulative_tsn_ack: first_tsn,
                        advertised_receiver_window_credit: 65536,
                        ..Default::default()
                    },
                    now + Duration::from_millis(50),
                )?;
            }
            let packets = a.gather_outbound(now + Duration::from_millis(100)).0;
            assert_eq!(0, data_chunks(&packets));
            assert!(!packets.is_empty());
            assert!(a.pending_queue.is_empty());
            assert_eq!(0, a.stream(1)?.buffered_amount()?);
            assert_eq!(a.my_next_tsn - 1, a.advanced_peer_tsn_ack_point);
            let fwd = a.create_forward_tsn();
            assert_eq!(usize::from(!unordered), fwd.streams.len());
            let released: usize = std::iter::from_fn(|| a.poll())
                .filter_map(|event| {
                    if let Event::Stream(StreamEvent::BufferedAmountReleased { n_bytes, .. }) =
                        event
                    {
                        Some(n_bytes)
                    } else {
                        None
                    }
                })
                .sum();
            assert_eq!(4000, released);

            // Lose FORWARD TSN, including while waiting to shut down.
            a.set_state(AssociationState::ShutdownPending);
            let retry = a.poll_timeout().unwrap();
            a.handle_timeout(retry);
            assert_eq!(packets, a.gather_outbound(retry).0);
            assert!(
                a.poll().is_none(),
                "abandoned bytes must not be released twice"
            );
            let cwnd = a.cwnd;
            a.handle_sack(
                &ChunkSelectiveAck {
                    cumulative_tsn_ack: a.advanced_peer_tsn_ack_point,
                    advertised_receiver_window_credit: 65536,
                    ..Default::default()
                },
                retry,
            )?;
            assert!(a.inflight_queue.is_empty());
            assert_eq!(cwnd, a.cwnd, "abandoned bytes must not grow cwnd");
            assert!(a.poll().is_none());
        }
    }
    Ok(())
}

#[test]
fn test_timed_expiry_does_not_abandon_gap_acked_messages() -> Result<()> {
    let mut a = timed_test_association();
    let now = Instant::now();
    let first_tsn = a.my_next_tsn;
    let ppi = PayloadProtocolIdentifier::Binary;
    let mut stream = a.open_stream(1, ppi)?;
    stream.set_reliability_params(false, ReliabilityType::Timed, 100)?;
    for _ in 0..2 {
        stream.write_sctp(now, &Bytes::from_static(b"test"), ppi)?;
    }
    a.gather_outbound(now);
    a.handle_sack(
        &ChunkSelectiveAck {
            cumulative_tsn_ack: first_tsn - 1,
            advertised_receiver_window_credit: 65536,
            gap_ack_blocks: vec![crate::chunk::chunk_selective_ack::GapAckBlock {
                start: 2,
                end: 2,
            }],
            ..Default::default()
        },
        now + Duration::from_millis(50),
    )?;
    let timeout = a.poll_timeout().unwrap();
    a.handle_timeout(timeout);
    a.gather_outbound(timeout);
    assert!(!a.inflight_queue.get(first_tsn + 1).unwrap().abandoned());
    assert!(sna32lt(a.advanced_peer_tsn_ack_point, first_tsn + 1));
    let streams = a.create_forward_tsn().streams;
    assert!(streams.iter().all(|s| s.sequence != 1));
    Ok(())
}

#[test]
fn test_timed_abandonment_discards_gap_acked_fragments() -> Result<()> {
    for unordered in [false, true] {
        for (size, lost_fragment) in [(2000, 0), (2000, 1), (4000, 0), (4000, 1), (4000, 2)] {
            let mut sender = timed_test_association();
            let mut receiver = timed_test_association();
            sender.cwnd = 65536;
            receiver.ack_mode = AckMode::NoDelay;
            receiver.peer_last_tsn = sender.my_next_tsn.wrapping_sub(1);
            let receive_window = receiver.get_my_receiver_window_credit();
            let now = Instant::now();
            let ppi = PayloadProtocolIdentifier::Binary;
            let mut stream = sender.open_stream(1, ppi)?;
            stream.set_reliability_params(unordered, ReliabilityType::Timed, 100)?;
            stream.write_sctp(now, &Bytes::from(vec![0x55; size]), ppi)?;
            // A fully received message must survive even with the same SID,
            // deadline and (for unordered DATA) SSN as the abandoned message.
            let delivered = Bytes::from(vec![0x66; size]);
            stream.write_sctp(now, &delivered, ppi)?;
            receiver.open_stream(1, ppi)?.set_reliability_params(
                unordered,
                ReliabilityType::Timed,
                100,
            )?;
            let data: Vec<ChunkPayloadData> = sender
                .gather_outbound(now)
                .0
                .iter()
                .flat_map(|raw| Packet::unmarshal(raw).unwrap().chunks)
                .filter_map(|c| c.as_any().downcast_ref::<ChunkPayloadData>().cloned())
                .collect();
            let fragments = size.div_ceil(sender.max_payload_size as usize);
            assert_eq!(2 * fragments, data.len());
            for (index, chunk) in data.iter().enumerate() {
                if index != lost_fragment {
                    receiver.handle_data(chunk)?;
                }
            }
            sender.handle_sack(
                &receiver.create_selective_ack_chunk(),
                now + Duration::from_millis(10),
            )?;
            let at = sender.poll_timeout().unwrap();
            sender.handle_timeout(at);
            let packets = sender.gather_outbound(at).0;
            assert_eq!(
                0,
                data_chunks(&packets),
                "expired DATA must not be retransmitted"
            );
            for chunk in &data[fragments..] {
                assert!(
                    !sender.inflight_queue.get(chunk.tsn).unwrap().abandoned(),
                    "a fully Gap-ACKed message must not be abandoned"
                );
            }
            let forwards: Vec<ChunkForwardTsn> = packets
                .iter()
                .flat_map(|raw| Packet::unmarshal(raw).unwrap().chunks)
                .filter_map(|c| c.as_any().downcast_ref::<ChunkForwardTsn>().cloned())
                .collect();
            assert_eq!(1, forwards.len());
            assert_eq!(data[fragments - 1].tsn, forwards[0].new_cumulative_tsn);
            receiver.handle_forward_tsn(&forwards[0])?;
            sender.handle_sack(&receiver.create_selective_ack_chunk(), at)?;
            assert!(sender.inflight_queue.is_empty());
            assert_eq!(0, sender.stream(1)?.buffered_amount()?);
            let released: usize = std::iter::from_fn(|| sender.poll())
                .filter_map(|event| {
                    if let Event::Stream(StreamEvent::BufferedAmountReleased { n_bytes, .. }) =
                        event
                    {
                        Some(n_bytes)
                    } else {
                        None
                    }
                })
                .sum();
            assert_eq!(
                2 * size,
                released,
                "Gap-ACKed bytes must not be released twice"
            );
            let queue = &receiver.streams.get(&1).unwrap().reassembly_queue;
            assert_eq!(
                size,
                queue.get_num_bytes(),
                "abandoned fragments still occupy rwnd: unordered={unordered}, size={size}, lost_fragment={lost_fragment}"
            );
            let chunks = receiver
                .stream(1)?
                .read_sctp()?
                .expect("the complete message must remain readable");
            let mut received = vec![0; size];
            assert_eq!(size, chunks.read(&mut received)?);
            assert_eq!(delivered.as_ref(), received.as_slice());
            assert!(receiver.stream(1)?.read_sctp()?.is_none());
            assert_eq!(receive_window, receiver.get_my_receiver_window_credit());
        }
    }
    Ok(())
}

#[test]
fn test_late_real_data_acks_keep_t3_alive() -> Result<()> {
    let mut sender = timed_test_association();
    let mut receiver = timed_test_association();
    sender.rto_mgr.set_rto(1000, false);
    receiver.peer_last_tsn = sender.my_next_tsn - 1;
    let ppi = PayloadProtocolIdentifier::Binary;
    sender
        .open_stream(1, ppi)?
        .set_reliability_params(true, ReliabilityType::Timed, 100)?;
    receiver.open_stream(1, ppi)?;
    let deliver_new_data =
        |sender: &mut Association, receiver: &mut Association, at: Instant| -> Result<()> {
            sender
                .stream(1)?
                .write_sctp(at, &Bytes::from_static(b"actually received"), ppi)?;
            let mut delivered = 0;
            for raw in sender.gather_outbound(at).0 {
                for c in Packet::unmarshal(&raw)?.chunks {
                    if let Some(c) = c.as_any().downcast_ref::<ChunkPayloadData>() {
                        receiver.handle_data(c)?;
                        delivered += 1;
                    }
                }
            }
            assert_eq!(1, delivered);
            assert!(receiver.stream(1)?.read_sctp()?.is_some());
            Ok(())
        };
    deliver_new_data(&mut sender, &mut receiver, Instant::now())?;
    for round in 1..=5 {
        let at = sender.poll_timeout().unwrap();
        let previous_ack = receiver.create_selective_ack_chunk();
        sender.handle_timeout(at);
        // Ignore the retransmission/FORWARD TSN: the peer already received this DATA.
        sender.gather_outbound(at);
        // A busy sender has the next message in flight when the old SACK arrives.
        if round < 5 {
            deliver_new_data(&mut sender, &mut receiver, at)?;
        } else {
            sender.open_stream(2, ppi)?.write_sctp(
                at,
                &Bytes::from_static(b"reliable, first send lost"),
                ppi,
            )?;
            assert!(!sender.gather_outbound(at).0.is_empty());
        }
        sender.handle_sack(&previous_ack, at + Duration::from_millis(10))?;
    }
    let at = sender.poll_timeout().unwrap();
    sender.handle_timeout(at);
    let retried_reliable = sender
        .gather_outbound(at)
        .0
        .iter()
        .flat_map(|raw| Packet::unmarshal(raw).unwrap().chunks)
        .filter(|c| {
            c.as_any()
                .downcast_ref::<ChunkPayloadData>()
                .is_some_and(|c| c.stream_identifier == 2)
        })
        .count();
    assert_eq!(
        1,
        retried_reliable,
        "T3 must retry the lost reliable DATA after five successful but late timed DATA ACKs; timer={:?}",
        sender.poll_timeout()
    );
    Ok(())
}

#[test]
fn test_message_abandonment_is_idempotent_after_partial_and_late_acks() -> Result<()> {
    for unordered in [false, true] {
        let mut a = timed_test_association();
        let now = Instant::now();
        let ppi = PayloadProtocolIdentifier::Binary;
        a.cwnd = 2 * a.max_payload_size + 1;
        let cumulative_ack = a.my_next_tsn.wrapping_sub(1);
        a.open_stream(2, ppi)?
            .write_sctp(now, &Bytes::from_static(b"x"), ppi)?;
        a.gather_outbound(now); // Leave a gap before the fragmented message.
        let first = a.my_next_tsn;
        let mut stream = a.open_stream(1, ppi)?;
        stream.set_reliability_params(unordered, ReliabilityType::Timed, 100)?;
        stream.write_sctp(now, &Bytes::from(vec![0; 4000]), ppi)?;
        stream.write_sctp(now, &Bytes::from_static(b"next"), ppi)?;
        assert_eq!(2, data_chunks(&a.gather_outbound(now).0));
        let mut sack = ChunkSelectiveAck {
            cumulative_tsn_ack: cumulative_ack,
            advertised_receiver_window_credit: 65536,
            gap_ack_blocks: vec![crate::chunk::chunk_selective_ack::GapAckBlock {
                start: 2,
                end: 2,
            }],
            ..Default::default()
        };
        a.handle_sack(&sack, now + Duration::from_millis(50))?;
        while a.poll().is_some() {}
        let at = now + Duration::from_millis(100);
        let messages = a.unretransmittable_messages(at, ChunkPayloadData::is_outstanding)?;
        assert_eq!(1, messages.len());
        let message = messages[0];
        assert!(a.abandon_message(message)?);
        let released: usize = std::iter::from_fn(|| a.poll())
            .filter_map(|event| {
                if let Event::Stream(StreamEvent::BufferedAmountReleased { n_bytes, .. }) = event {
                    Some(n_bytes)
                } else {
                    None
                }
            })
            .sum();
        assert_eq!(4000 - a.max_payload_size as usize, released);
        assert!(!a.abandon_message(message)?);
        assert!(
            a.poll().is_none(),
            "repeated abandonment must not release bytes twice"
        );
        assert_eq!(4, a.pending_queue.get_num_bytes());
        assert_eq!(4, a.stream(1)?.buffered_amount()?);
        assert_eq!(
            Bytes::from_static(b"next"),
            a.pending_queue.peek().unwrap().user_data
        );
        for offset in 0..3 {
            let c = a.inflight_queue.get(first.wrapping_add(offset)).unwrap();
            assert!(c.abandoned());
            assert!(!c.is_outstanding());
            assert!(c.user_data.is_empty());
        }
        assert!(!a.inflight_queue.get(first + 1).unwrap().acknowledged);
        // A late real ACK changes receipt state, without reclaiming payload again.
        sack.gap_ack_blocks[0].end = 3;
        a.handle_sack(&sack, at)?;
        a.handle_sack(&sack, at + Duration::from_millis(1))?;
        assert!(a.inflight_queue.get(first + 1).unwrap().acknowledged);
        assert!(!a.inflight_queue.get(first + 2).unwrap().acknowledged);
        assert!(a.poll().is_none());
        assert!(!a.abandon_message(message)?);
        assert_eq!(4, a.stream(1)?.buffered_amount()?);
    }
    Ok(())
}

#[test]
fn test_repeated_pending_abandonment_preserves_the_next_message() -> Result<()> {
    let mut a = timed_test_association();
    let now = Instant::now();
    let first_tsn = a.my_next_tsn;
    let ppi = PayloadProtocolIdentifier::Binary;
    let mut stream = a.open_stream(1, ppi)?;
    stream.set_reliability_params(true, ReliabilityType::Timed, 100)?;
    stream.write_sctp(now, &Bytes::from(vec![0; 4000]), ppi)?;
    stream.write_sctp(now, &Bytes::from_static(b"next"), ppi)?;
    let message = a.pending_message_to_abandon()?;
    assert!(a.abandon_message(message)?);
    assert!(!a.abandon_message(message)?);
    a.on_messages_abandoned(now + Duration::from_millis(100));
    assert_eq!(first_tsn, a.my_next_tsn);
    assert!(a.inflight_queue.is_empty());
    assert!(a.poll_timeout().is_none());
    assert_eq!(4, a.stream(1)?.buffered_amount()?);
    assert_eq!(
        Bytes::from_static(b"next"),
        a.pending_queue.peek().unwrap().user_data
    );
    Ok(())
}

#[test]
fn test_rexmit_budget_counts_fast_and_timer_retransmissions() -> Result<()> {
    for max_retransmits in [0, 1, 2] {
        for fast_first in [false, true] {
            let mut a = timed_test_association();
            let now = Instant::now();
            let ppi = PayloadProtocolIdentifier::Binary;
            let first_tsn = a.my_next_tsn;
            let mut stream = a.open_stream(1, ppi)?;
            stream.set_reliability_params(true, ReliabilityType::Rexmit, max_retransmits)?;
            stream.write_sctp(now, &Bytes::from_static(b"lost"), ppi)?;
            assert_eq!(1, data_chunks(&a.gather_outbound(now).0));
            for attempt in 0..=max_retransmits {
                let at = if fast_first && attempt == 0 {
                    a.inflight_queue.get_mut(first_tsn).unwrap().miss_indicator = 3;
                    a.will_retransmit_fast = true;
                    now + Duration::from_millis(1)
                } else {
                    let at = a.timers.get(Timer::T3RTX).unwrap();
                    a.handle_timeout(at);
                    at
                };
                let packets = a.gather_outbound(at).0;
                let retransmit = attempt < max_retransmits;
                assert_eq!(
                    usize::from(retransmit),
                    data_chunks(&packets),
                    "max_retransmits={max_retransmits} attempt={attempt} fast_first={fast_first}"
                );
                let chunk = a.inflight_queue.get(first_tsn).unwrap();
                assert_eq!(!retransmit, chunk.abandoned());
                assert_eq!(
                    if retransmit { 4 } else { 0 },
                    a.inflight_queue.get_num_bytes()
                );
                if !retransmit {
                    assert!(packets.iter().any(|raw| {
                        Packet::unmarshal(raw)
                            .unwrap()
                            .chunks
                            .iter()
                            .any(|c| c.as_any().is::<ChunkForwardTsn>())
                    }));
                }
            }
        }
    }
    Ok(())
}

#[test]
fn test_t3_recovers_with_acked_timed_zero_data() -> Result<()> {
    for gap_ack in [false, true] {
        let mut a = timed_test_association();
        let mut receiver = timed_test_association();
        receiver.peer_last_tsn = a.my_next_tsn.wrapping_sub(1);
        receiver.ack_mode = AckMode::NoDelay;
        a.rto_mgr.set_rto(1000, false);
        let ppi = PayloadProtocolIdentifier::Binary;
        a.open_stream(1, ppi)?
            .set_reliability_params(true, ReliabilityType::Timed, 100)?;
        a.open_stream(2, ppi)?
            .set_reliability_params(true, ReliabilityType::Timed, 0)?;
        receiver
            .open_stream(2, ppi)?
            .set_reliability_params(true, ReliabilityType::Timed, 0)?;
        let mut now = Instant::now();
        a.stream(1)?
            .write_sctp(now, &Bytes::from_static(b"lost timed message"), ppi)?;
        a.gather_outbound(now);
        for round in 1..=8 {
            now = a
                .poll_timeout()
                .expect("unacknowledged DATA needs a T3 timer");
            a.handle_timeout(now);
            let packets = a.gather_outbound(now).0;
            assert_eq!(
                0,
                data_chunks(&packets),
                "expired DATA must not be retransmitted"
            );
            let forwards: Vec<ChunkForwardTsn> = packets
                .iter()
                .flat_map(|raw| Packet::unmarshal(raw).unwrap().chunks)
                .filter_map(|c| c.as_any().downcast_ref::<ChunkForwardTsn>().cloned())
                .collect();
            assert!(
                !forwards.is_empty(),
                "round {round}, gap_ack={gap_ack}: T3 stopped producing FORWARD TSN despite peer progress"
            );
            if !gap_ack {
                for forward in &forwards {
                    receiver.handle_forward_tsn(forward)?;
                }
            }
            // Deliver fresh Timed(0) DATA. Drop FORWARD TSN in the gap-ACK case
            // so the actual DATA receipt must clear T3's error counter there too.
            a.stream(2)?.write_sctp(
                now,
                &Bytes::from_static(b"delivered timed zero message"),
                ppi,
            )?;
            for raw in a.gather_outbound(now).0 {
                for c in Packet::unmarshal(&raw)?.chunks {
                    if let Some(data) = c.as_any().downcast_ref::<ChunkPayloadData>() {
                        receiver.handle_data(data)?;
                    }
                }
            }
            assert!(
                receiver.stream(2)?.read_sctp()?.is_some(),
                "fresh Timed(0) DATA reached the peer"
            );
            let sack = receiver.create_selective_ack_chunk();
            if gap_ack {
                assert!(!sack.gap_ack_blocks.is_empty());
            } else {
                assert_eq!(a.my_next_tsn.wrapping_sub(1), sack.cumulative_tsn_ack);
            }
            // Keep new traffic in flight when the preceding DATA is ACKed.
            a.stream(1)?
                .write_sctp(now, &Bytes::from_static(b"lost timed message"), ppi)?;
            a.gather_outbound(now);
            a.handle_sack(&sack, now + Duration::from_millis(10))?;
        }
    }
    Ok(())
}

#[test]
fn test_timed_zero_fragment_loss_restores_receive_window() -> Result<()> {
    let mut sender = timed_test_association();
    let mut receiver = timed_test_association();
    sender.cwnd = 1400;
    receiver.peer_last_tsn = sender.my_next_tsn - 1;
    let now = Instant::now();
    let ppi = PayloadProtocolIdentifier::Binary;
    sender
        .open_stream(1, ppi)?
        .set_reliability_params(true, ReliabilityType::Timed, 0)?;
    receiver.open_stream(1, ppi)?;
    sender
        .stream(1)?
        .write_sctp(now, &Bytes::from(vec![0x55; 4000]), ppi)?;
    let lost = sender.gather_outbound(now).0;
    assert_eq!(1, lost.len());
    assert!(!sender.pending_queue.is_empty());
    let at = sender.poll_timeout().unwrap();
    sender.handle_timeout(at);
    for _ in 0..10 {
        let packets = sender.gather_outbound(at).0;
        for raw in packets {
            for c in Packet::unmarshal(&raw)?.chunks {
                if let Some(c) = c.as_any().downcast_ref::<ChunkPayloadData>() {
                    receiver.handle_data(c)?;
                } else if let Some(c) = c.as_any().downcast_ref::<ChunkForwardTsn>() {
                    receiver.handle_forward_tsn(c)?;
                }
            }
        }
        sender.handle_sack(&receiver.create_selective_ack_chunk(), at)?;
        while receiver.stream(1)?.read_sctp()?.is_some() {}
        if sender.pending_queue.is_empty() && sender.inflight_queue.is_empty() {
            break;
        }
    }
    assert!(sender.pending_queue.is_empty());
    assert!(sender.inflight_queue.is_empty());
    assert!(sender.poll_timeout().is_none());
    assert_eq!(
        0,
        receiver
            .streams
            .get(&1)
            .unwrap()
            .reassembly_queue
            .get_num_bytes(),
        "abandoned Timed(0) message must not strand ACKed tail fragments in rwnd"
    );
    Ok(())
}

#[path = "timer_deadline_test.rs"]
mod timer_deadline_test;

#[path = "retransmission_review_test.rs"]
mod retransmission_review_test;

#[test]
fn test_reconfig_backoff_must_double_once() -> Result<()> {
    let mut a = timed_test_association();
    a.rto_mgr.set_rto(1000, false);
    let now = Instant::now();
    let ppi = PayloadProtocolIdentifier::Binary;
    let mut stream = a.open_stream(1, ppi)?;
    stream.write_sctp(now, &Bytes::from_static(b"reliable data"), ppi)?;
    stream.close(now)?;
    a.gather_outbound(now);
    let timeout = a.timers.get(Timer::Reconfig).unwrap();
    assert_eq!(Some(timeout), a.timers.get(Timer::T3RTX));
    a.handle_timeout(timeout);
    a.gather_outbound(timeout);
    assert_eq!(
        Some(timeout + Duration::from_secs(2)),
        a.timers.get(Timer::Reconfig),
        "Reconfiguration timer must double its previous 1s interval, not back off twice"
    );
    Ok(())
}

#[path = "message_selection_test.rs"]
mod message_selection_test;
