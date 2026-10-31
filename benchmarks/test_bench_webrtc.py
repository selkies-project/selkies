# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
"""Benchmarks of the WebRTC stack's per-packet and per-frame work: the
packetizers every encoded frame passes through, RTP and RTCP framing, SCTP
data-channel framing, the receive-side bandwidth estimator and jitter buffer,
and the session description a peer sends over signaling."""
import os
import random
from fractions import Fraction

import pytest

from selkies.webrtc import sdp
from selkies.webrtc.codecs.base import EncodedPacket
from selkies.webrtc.codecs.h264 import H264Encoder, h264_depayload
from selkies.webrtc.codecs.vpx import Vp8Encoder
from selkies.webrtc.jitterbuffer import JitterBuffer
from selkies.webrtc.rate import RemoteBitrateEstimator
from selkies.webrtc.rtcrtpparameters import RTCRtpHeaderExtensionParameters, RTCRtpParameters
from selkies.webrtc.rtcsctptransport import DataChunk, parse_packet, serialize_packet
from selkies.webrtc.rtp import (
    HeaderExtensionsMap,
    RtcpPacket,
    RtcpReceiverInfo,
    RtcpRrPacket,
    RtcpSdesPacket,
    RtcpSourceInfo,
    RtcpSrPacket,
    RtcpSenderInfo,
    RtpPacket,
)

SDP_ANSWER = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                          "tests", "fuzz", "corpus", "sdp", "answer.sdp")
VIDEO_TIME_BASE = Fraction(1, 90000)


def _payload(size: int, seed: int) -> bytes:
    """Return `size` pseudo-random bytes free of Annex-B start codes."""
    rng = random.Random(seed)
    return bytes(rng.randrange(1, 256) for _ in range(size))


def _h264_frame(slice_sizes: list, keyframe: bool) -> bytes:
    """Build an Annex-B access unit: SPS and PPS ahead of an IDR, then slices."""
    nals = []
    if keyframe:
        nals.append(b"\x00\x00\x00\x01\x67" + _payload(24, 1))
        nals.append(b"\x00\x00\x00\x01\x68" + _payload(6, 2))
    nal_type = b"\x65" if keyframe else b"\x41"
    for index, size in enumerate(slice_sizes):
        nals.append(b"\x00\x00\x00\x01" + nal_type + _payload(size, 10 + index))
    return b"".join(nals)


H264_KEYFRAME = _h264_frame([60000, 60000], keyframe=True)
H264_DELTA = _h264_frame([4000], keyframe=False)
H264_SMALL_DELTAS = _h264_frame([300] * 8, keyframe=False)
VP8_FRAME = _payload(50000, 3)

EXTENSIONS = HeaderExtensionsMap()
EXTENSIONS.configure(
    RTCRtpParameters(
        headerExtensions=[
            RTCRtpHeaderExtensionParameters(id=1, uri="urn:ietf:params:rtp-hdrext:sdes:mid"),
            RTCRtpHeaderExtensionParameters(id=2, uri="http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time"),
            RTCRtpHeaderExtensionParameters(id=3, uri="urn:ietf:params:rtp-hdrext:toffset"),
            RTCRtpHeaderExtensionParameters(
                id=4, uri="http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01"
            ),
        ]
    )
)


def _rtp_packet(sequence_number: int, payload: bytes) -> RtpPacket:
    packet = RtpPacket(payload_type=96, marker=1, sequence_number=sequence_number,
                       timestamp=3000 * sequence_number, ssrc=1001, payload=payload)
    packet.extensions.mid = "0"
    packet.extensions.abs_send_time = 0x123456
    packet.extensions.transmission_offset = 12
    packet.extensions.transport_sequence_number = sequence_number
    return packet


RTP_PACKET = _rtp_packet(1234, _payload(1200, 4))
RTP_BYTES = RTP_PACKET.serialize(EXTENSIONS)


def _rtcp_compound() -> bytes:
    report = RtcpReceiverInfo(ssrc=1001, fraction_lost=3, packets_lost=17, highest_sequence=40000,
                              jitter=120, lsr=0x12345678, dlsr=500)
    sender = RtcpSenderInfo(ntp_timestamp=0x1122334455667788, rtp_timestamp=90000,
                            packet_count=4000, octet_count=4800000)
    sr = RtcpSrPacket(ssrc=2002, sender_info=sender, reports=[report])
    rr = RtcpRrPacket(ssrc=2002, reports=[report, report])
    sdes = RtcpSdesPacket(chunks=[RtcpSourceInfo(ssrc=2002, items=[(1, b"selkies")])])
    return bytes(sr) + bytes(rr) + bytes(sdes)


RTCP_COMPOUND = _rtcp_compound()


def _sctp_packet() -> bytes:
    chunk = DataChunk(flags=3)
    chunk.tsn = 1000
    chunk.stream_id = 1
    chunk.stream_seq = 7
    chunk.protocol = 51
    chunk.user_data = _payload(1000, 5)
    return serialize_packet(5000, 5000, 0xDEADBEEF, chunk)


SCTP_PACKET = _sctp_packet()


@pytest.mark.parametrize("frame", [H264_KEYFRAME, H264_DELTA, H264_SMALL_DELTAS],
                         ids=["keyframe-120k", "delta-4k", "stap-a-8x300"])
def test_h264_pack(benchmark, frame):
    encoder = H264Encoder()
    packet = EncodedPacket(frame, pts=3000, time_base=VIDEO_TIME_BASE)
    payloads, _, _ = benchmark(encoder.pack, packet)
    assert payloads


def test_h264_depayload(benchmark):
    payloads, _, _ = H264Encoder().pack(EncodedPacket(H264_KEYFRAME, pts=0, time_base=VIDEO_TIME_BASE))

    def depayload():
        return [h264_depayload(payload) for payload in payloads]

    assert benchmark(depayload)


def test_vp8_pack(benchmark):
    encoder = Vp8Encoder()
    packet = EncodedPacket(VP8_FRAME, pts=3000, time_base=VIDEO_TIME_BASE)
    payloads, _, _ = benchmark(encoder.pack, packet)
    assert payloads


def test_rtp_serialize(benchmark):
    assert benchmark(RTP_PACKET.serialize, EXTENSIONS) == RTP_BYTES


def test_rtp_parse(benchmark):
    packet = benchmark(RtpPacket.parse, RTP_BYTES, EXTENSIONS)
    assert packet.sequence_number == 1234


def test_rtcp_parse_compound(benchmark):
    assert len(benchmark(RtcpPacket.parse, RTCP_COMPOUND)) == 3


def test_sctp_serialize_data(benchmark):
    chunk = parse_packet(SCTP_PACKET)[3][0]
    assert benchmark(serialize_packet, 5000, 5000, 0xDEADBEEF, chunk) == SCTP_PACKET


def test_sctp_parse_data(benchmark):
    assert benchmark(parse_packet, SCTP_PACKET)[2] == 0xDEADBEEF


def test_remote_bitrate_estimator(benchmark):
    """Feed one second of a 60 fps stream, ten packets per frame, with jitter."""
    rng = random.Random(6)
    arrivals = []
    for frame in range(60):
        send_ms = frame * 1000 // 60
        for _ in range(10):
            arrival_ms = 1000 + send_ms + rng.randrange(0, 6)
            abs_send_time = ((send_ms << 18) // 1000) & 0xFFFFFF
            arrivals.append((arrival_ms, abs_send_time))

    def estimate():
        estimator = RemoteBitrateEstimator()
        for arrival_ms, abs_send_time in arrivals:
            estimator.add(arrival_ms, abs_send_time, 1200, 1001)
        return estimator

    benchmark(estimate)


def test_jitter_buffer_video(benchmark):
    """Reassemble 32 frames of four packets each, two of them swapped in flight."""
    packets = []
    for frame in range(32):
        for index in range(4):
            sequence_number = frame * 4 + index
            packet = RtpPacket(payload_type=96, marker=int(index == 3), sequence_number=sequence_number,
                               timestamp=3000 * frame, ssrc=1001, payload=b"\x00" * 1000)
            # The receiver depayloads each packet before it reaches the buffer.
            packet._data = packet.payload
            packets.append(packet)
    for index in range(1, len(packets) - 1, 8):
        packets[index], packets[index + 1] = packets[index + 1], packets[index]

    def reassemble():
        buffer = JitterBuffer(capacity=128, is_video=True)
        frames = 0
        for packet in packets:
            _, frame = buffer.add(packet)
            if frame is not None:
                frames += 1
        return frames

    assert benchmark(reassemble) > 0


def test_sdp_parse(benchmark):
    with open(SDP_ANSWER, encoding="utf-8") as f:
        text = f.read()
    description = benchmark(sdp.SessionDescription.parse, text)
    assert description.media


def test_sdp_serialize(benchmark):
    with open(SDP_ANSWER, encoding="utf-8") as f:
        description = sdp.SessionDescription.parse(f.read())
    assert benchmark(str, description).startswith("v=0")
