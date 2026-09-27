#!/usr/bin/env python3
"""FlexFEC protects the same mutable-extension values libwebrtc recovers.

The receiver zeroes mutable extension bytes before XOR recovery. Unequal RTP
header lengths can place a known packet's video-timing bytes over a missing
packet's media payload, so protecting nonzero values can corrupt that payload.
An independent oracle normalizes structured fields before serialization; the
sender test exercises its actual packetization, history, and repair path.
"""
import asyncio
import copy
import os
import sys
import unittest
from dataclasses import replace
from struct import pack
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))

from selkies.webrtc.mediastreams import MediaStreamError  # noqa: E402
from selkies.webrtc.rtcrtpparameters import (  # noqa: E402
    RTCRtpCodecParameters, RTCRtpHeaderExtensionParameters, RTCRtpParameters,
)
from selkies.webrtc.rtcrtpsender import RTCEncodedFrame, RTCRtpSender  # noqa: E402
from selkies.webrtc.rtp import (  # noqa: E402
    DEPENDENCY_DESCRIPTOR_URI, HeaderExtensions, HeaderExtensionsMap,
    RtpPacket, build_flexfec_03,
)
from test_flexfec_repair import recover  # noqa: E402

URIS = [
    "urn:ietf:params:rtp-hdrext:sdes:mid",
    "http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time",
    "http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01",
    "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay",
    "http://www.webrtc.org/experiments/rtp-hdrext/video-timing",
    DEPENDENCY_DESCRIPTOR_URI,
    "urn:ietf:params:rtp-hdrext:toffset",
]


def configured(extended: bool = False) -> HeaderExtensionsMap:
    mapping = HeaderExtensionsMap()
    mapping.configure(RTCRtpParameters(headerExtensions=[
        RTCRtpHeaderExtensionParameters(uri=uri, id=i + 1 + (20 if extended else 0))
        for i, uri in enumerate(URIS)
    ]))
    return mapping


def receiver_packet(packet: RtpPacket, mapping: HeaderExtensionsMap) -> bytes:
    """Serialize libwebrtc's recovery input independently of the byte parser."""
    normalized = copy.copy(packet)
    timing = packet.extensions.video_timing
    normalized.extensions = replace(
        packet.extensions,
        abs_send_time=0 if packet.extensions.abs_send_time is not None else None,
        transport_sequence_number=(
            0 if packet.extensions.transport_sequence_number is not None else None),
        video_timing=timing[:4] + (0, 0, 0) if timing is not None else None,
    )
    return normalized.serialize(mapping)


def raw_packet(profile: int, extensions: bytes) -> bytes:
    """Keep extension padding, CSRCs, and RTP padding visible to byte checks."""
    assert len(extensions) % 4 == 0
    return (pack("!BBHII", 0xB2, 96, 100, 123456, 0x1234)
            + pack("!IIHH", 19, 23, profile, len(extensions) // 4)
            + extensions + b"media\x00\x00\x00\x04")


class FlexfecNormalization(unittest.TestCase):
    """Exact packet recovery and layout preservation, including sender wiring."""

    def test_recovery_at_every_loss_position(self) -> None:
        for extended in (False, True):
            mapping = configured(extended)
            for size in (2, 3, 10):
                packets, expected = [], []
                for i in range(size):
                    packet = RtpPacket(payload_type=96, sequence_number=100 + i,
                                       timestamp=123456, ssrc=0x1234,
                                       payload=b"\x7c\x01" + bytes([0x55 + i]) * 128)
                    packet.extensions = HeaderExtensions(
                        mid="0", abs_send_time=0x314159 + i,
                        transport_sequence_number=200 + i, playout_delay=(0, 0),
                        dependency_descriptor=b"\x01\x00\x08",
                        video_timing=(1, 3, 5, 7, 2, 11, 13) if i == size - 1 else None)
                    if extended:
                        packet.csrc = [19, 23]
                    packets.append(packet.serialize(mapping))
                    expected.append(receiver_packet(packet, mapping))
                protected = [mapping.for_fec(data) for data in packets]
                self.assertEqual(protected, expected)
                fec = build_flexfec_03(protected, 100, 0x1234, 118, 900, 123456, 8)
                for missing in range(size):
                    with self.subTest(extended=extended, size=size, missing=missing):
                        known = {100 + i: data for i, data in enumerate(expected) if i != missing}
                        self.assertEqual(recover([fec], known), {100 + missing: expected[missing]})
                self.assertNotEqual(packets[-1], protected[-1])
                self.assertEqual(RtpPacket.parse(packets[-1], mapping).extensions.video_timing,
                                 (1, 3, 5, 7, 2, 11, 13))

    def test_one_byte_layout_and_padding(self) -> None:
        data = raw_packet(0xBEDE, b"\x00\x22\x31\x41\x59\x71\x12\x34\x90\xab\x00\x00")
        expected = bytearray(data)
        expected[26:29] = bytes(3)
        expected[30:32] = bytes(2)
        self.assertEqual(configured().for_fec(data), bytes(expected))
        self.assertEqual(data[-9:], b"media\x00\x00\x00\x04")

    def test_two_byte_layout_and_padding(self) -> None:
        data = raw_packet(0x100F, b"\x00\x16\x03\x31\x41\x59\x1b\x03\x12\x34\x56\x09\x00\x00\x00\x00")
        expected = bytearray(data)
        expected[27:30] = bytes(3)
        expected[32:35] = bytes(3)
        self.assertEqual(configured(True).for_fec(data), bytes(expected))

    def test_unchanged_extensions(self) -> None:
        mapping = configured()
        for data in (RtpPacket(payload=b"media").serialize(),
                     raw_packet(0xBEDE, b"\x90\xab\x00\x00"),
                     raw_packet(0x4321, b"\x22\x31\x41\x59"),
                     raw_packet(0xBEDE, b"\xf0\x22\x31\x41"),
                     raw_packet(0xBEDE, b"")):
            with self.subTest(packet=data):
                self.assertEqual(mapping.for_fec(data), data)

    def test_truncated_extensions(self) -> None:
        for data in (b"\x80", b"\x90" + bytes(11),
                     raw_packet(0xBEDE, b"\x22\x31\x41\x59")[:27],
                     raw_packet(0xBEDE, b"\x2f\x00\x00\x00"),
                     raw_packet(0x1000, b"\x00\x00\x00\x16"),
                     raw_packet(0x1000, b"\x16\x03\x31\x41")):
            with self.subTest(packet=data), self.assertRaises(ValueError):
                configured().for_fec(data)

    def test_sender_recovery_and_wire_history(self) -> None:
        asyncio.run(self.sender_case(True))

    def test_sender_without_fec(self) -> None:
        asyncio.run(self.sender_case(False))

    async def sender_case(self, fec_enabled: bool) -> None:
        """Lose one real sender packet after normalizing the receiver's copy."""
        transport = SimpleNamespace(state="connected", _twcc_next=lambda size: 321,
                                    _send_rtp=AsyncMock(return_value=True))
        sender = RTCRtpSender("video", transport)
        sender._ssrc = 0x1234
        sender.replaceTrack(SimpleNamespace(id="test", stop=lambda: None))
        mapping = configured()
        sender._RTCRtpSender__rtp_header_extensions_map = mapping
        sender._RTCRtpSender__send_codec = RTCRtpCodecParameters(
            mimeType="video/H264", clockRate=90000, payloadType=96)
        sender._RTCRtpSender__fec_payload_type = 118 if fec_enabled else None
        frame = RTCEncodedFrame([b"\x7c\x81" + b"a" * 128, b"\x7c\x41" + b"b" * 128], 0, None)
        sender._next_encoded_frame = AsyncMock(side_effect=[frame, MediaStreamError()])
        with patch("selkies.webrtc.rtcrtpsender.video_timing_legs", return_value=(1, 3, 5, 7, 2, 0, 0)):
            await asyncio.wait_for(sender._run_rtp(), 2)
        wire = [call.args[0] for call in transport._send_rtp.call_args_list]
        media = [RtpPacket.parse(data, mapping) for data in wire if data[1] & 0x7F == 96]
        self.assertEqual(len(media), 2)
        self.assertEqual(media[-1].extensions.video_timing, (1, 3, 5, 7, 2, 0, 0))
        self.assertEqual(media[-1].extensions.transport_sequence_number, 321)
        for packet in media:
            stored = sender._RTCRtpSender__rtp_history.get(packet.sequence_number)
            self.assertEqual(stored.serialize(mapping), packet.serialize(mapping))
        repairs = [data for data in wire if data[1] & 0x7F == 118]
        self.assertEqual(len(repairs), int(fec_enabled))
        if fec_enabled:
            known = {media[1].sequence_number: receiver_packet(media[1], mapping)}
            recovered = recover(repairs, known)[media[0].sequence_number]
            self.assertEqual(RtpPacket.parse(recovered, mapping).payload, frame.payloads[0])
            self.assertEqual(recovered, receiver_packet(media[0], mapping))


if __name__ == "__main__":
    unittest.main()
