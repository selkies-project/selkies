#!/usr/bin/env python3
"""The RTP colour-space header extension the video sender declares for VP8 and VP9.

The wire form is libwebrtc's four-byte one: primaries, transfer and matrix as their ITU-T
H.273 codes, then the range in the high nibble of the last byte with the chroma siting left
unspecified. It is offered for video, it survives a pack and parse of an RTP packet, and a
packet whose map lacks the extension carries nothing.
"""
import os
import sys

sys.path.insert(0, os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))

from selkies.rtc import VPX_COLOR_SPACE  # noqa: E402
from selkies.webrtc.codecs import HEADER_EXTENSIONS  # noqa: E402
from selkies.webrtc.rtcrtpparameters import RTCRtpHeaderExtensionParameters, RTCRtpParameters  # noqa: E402
from selkies.webrtc.rtp import HeaderExtensionsMap, RtpPacket  # noqa: E402

URI = "http://www.webrtc.org/experiments/rtp-hdrext/color-space"


def configured(with_extension: bool) -> HeaderExtensionsMap:
    extensions = [RTCRtpHeaderExtensionParameters(id=7, uri=URI)] if with_extension else []
    ext_map = HeaderExtensionsMap()
    ext_map.configure(RTCRtpParameters(headerExtensions=extensions))
    return ext_map


def test_offered_for_video() -> None:
    assert any(e.uri == URI for e in HEADER_EXTENSIONS["video"])
    assert not any(e.uri == URI for e in HEADER_EXTENSIONS["audio"])


def test_wire_form_and_round_trip() -> None:
    ext_map = configured(True)
    packet = RtpPacket(payload_type=96, sequence_number=1, timestamp=90000)
    packet.payload = b"\x00"
    packet.extensions.color_space = VPX_COLOR_SPACE
    data = packet.serialize(ext_map)
    assert b"\x01\x01\x06\x10" in data
    parsed = RtpPacket.parse(data, ext_map)
    assert parsed.extensions.color_space == (1, 1, 6, 1)
    packet.extensions.color_space = (1, 1, 1, 2)
    assert RtpPacket.parse(packet.serialize(ext_map), ext_map).extensions.color_space == (1, 1, 1, 2)


def test_absent_without_negotiation() -> None:
    packet = RtpPacket(payload_type=96, sequence_number=1, timestamp=90000)
    packet.payload = b"\x00"
    packet.extensions.color_space = VPX_COLOR_SPACE
    data = packet.serialize(configured(False))
    assert b"\x01\x01\x06\x10" not in data
    assert RtpPacket.parse(data, configured(True)).extensions.color_space is None


if __name__ == "__main__":
    test_offered_for_video()
    test_wire_form_and_round_trip()
    test_absent_without_negotiation()
    print("ok")
