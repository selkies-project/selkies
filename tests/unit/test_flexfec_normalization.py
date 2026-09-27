#!/usr/bin/env python3
"""FlexFEC protects the same mutable-extension values libwebrtc recovers.

The receiver zeroes mutable extension bytes before XOR recovery. Unequal RTP
header lengths can place a known packet's video-timing bytes over a missing
packet's media payload, so protecting nonzero values can corrupt that payload.
An independent oracle normalizes structured fields before serialization; the
sender checks exercise its actual packetization, history, and repair path.
"""
import asyncio
import copy
import os
import sys
from dataclasses import replace
from struct import pack
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402

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


def zeroed(data: bytes, *spans: tuple) -> bytes:
    """`data` with each `(start, stop)` span zeroed."""
    out = bytearray(data)
    for start, stop in spans:
        out[start:stop] = bytes(stop - start)
    return bytes(out)


def recovery(res: H.Results) -> None:
    """Every single loss in groups of 2, 3, and 10, in both extension forms."""
    differ, unrecovered, timings = [], [], []
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
            if protected != expected or packets[-1] == protected[-1]:
                differ.append((extended, size))
            fec = build_flexfec_03(protected, 100, 0x1234, 118, 900, 123456, 8)
            for missing in range(size):
                known = {100 + i: data for i, data in enumerate(expected) if i != missing}
                if recover([fec], known) != {100 + missing: expected[missing]}:
                    unrecovered.append((extended, size, missing))
            timings.append(RtpPacket.parse(packets[-1], mapping).extensions.video_timing)
    res.check("the copy is the receiver's, in the one- and the two-byte form, with CSRCs",
              not differ, differ)
    res.check("each of the 30 single losses comes back as the receiver's copy",
              not unrecovered, unrecovered)
    res.check("the packet sent keeps its video timing whole",
              timings == [(1, 3, 5, 7, 2, 11, 13)] * 6, timings)


def layouts(res: H.Results) -> None:
    """Padding, CSRCs, unknown IDs, and RTP padding stay; a truncated packet or
    extension raises."""
    one = raw_packet(0xBEDE, b"\x00\x22\x31\x41\x59\x71\x12\x34\x90\xab\x00\x00")
    res.check("one-byte form: absolute send time and transmission offset zeroed, the rest kept",
              configured().for_fec(one) == zeroed(one, (26, 29), (30, 32))
              and one[-9:] == b"media\x00\x00\x00\x04")
    two = raw_packet(0x100F, b"\x00\x16\x03\x31\x41\x59\x1b\x03\x12\x34\x56\x09\x00\x00\x00\x00")
    res.check("two-byte form with app bits and an empty element: the same",
              configured(True).for_fec(two) == zeroed(two, (27, 30), (32, 35)))
    mapping = configured()
    unchanged = [RtpPacket(payload=b"media").serialize(),
                 raw_packet(0xBEDE, b"\x90\xab\x00\x00"),
                 raw_packet(0x4321, b"\x22\x31\x41\x59"),
                 raw_packet(0xBEDE, b"\xf0\x22\x31\x41"),
                 raw_packet(0xBEDE, b"")]
    kept = [mapping.for_fec(data) == data for data in unchanged]
    res.check("no extension, no mutable ID, another profile, a one-byte ID of 15 first, "
              "or an empty block: left as it is", all(kept), kept)
    raised = []
    for data in (b"\x80", b"\x90" + bytes(11),
                 raw_packet(0xBEDE, b"\x22\x31\x41\x59")[:27],
                 raw_packet(0xBEDE, b"\x2f\x00\x00\x00"),
                 raw_packet(0x1000, b"\x00\x00\x00\x16"),
                 raw_packet(0x1000, b"\x16\x03\x31\x41")):
        try:
            configured().for_fec(data)
            raised.append(False)
        except ValueError:
            raised.append(True)
    res.check("a truncated packet, header, or extension raises", all(raised), raised)


async def run_sender(fec_enabled: bool) -> tuple:
    """One frame of two packets through the real sender."""
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
    repairs = [data for data in wire if data[1] & 0x7F == 118]
    stored = [sender._RTCRtpSender__rtp_history.get(p.sequence_number) for p in media]
    return mapping, frame, media, repairs, stored


def sender(res: H.Results) -> None:
    """Lose one real sender packet after normalizing the receiver's copy."""
    for fec_enabled in (True, False):
        tag = "with FlexFEC" if fec_enabled else "without FlexFEC"
        mapping, frame, media, repairs, stored = asyncio.run(run_sender(fec_enabled))
        last = media[-1].extensions if media else None
        res.check(f"{tag}: two packets go out, the last with its video timing and sequence",
                  len(media) == 2 and last.video_timing == (1, 3, 5, 7, 2, 0, 0)
                  and last.transport_sequence_number == 321,
                  (len(media), last and last.video_timing))
        res.check(f"{tag}: the retransmission history holds the packets as sent",
                  all(s is not None and s.serialize(mapping) == p.serialize(mapping)
                      for s, p in zip(stored, media)))
        res.check(f"{tag}: {'one repair packet' if fec_enabled else 'no repair packet'}",
                  len(repairs) == int(fec_enabled), len(repairs))
        if fec_enabled and len(media) == 2:
            known = {media[1].sequence_number: receiver_packet(media[1], mapping)}
            recovered = recover(repairs, known).get(media[0].sequence_number)
            res.check(f"{tag}: the first packet, lost, comes back as the receiver's copy with its payload",
                      recovered == receiver_packet(media[0], mapping)
                      and RtpPacket.parse(recovered, mapping).payload == frame.payloads[0],
                      recovered and RtpPacket.parse(recovered, mapping).payload[:12])


def main() -> int:
    res = H.Results("flexfec-normalization")
    sender(res)
    recovery(res)
    layouts(res)
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
