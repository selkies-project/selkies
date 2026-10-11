#!/usr/bin/env python3
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
"""Fuzzes RTP and RTCP parsing, which the DTLS transport runs on every
decrypted media packet a peer sends."""
import struct
import sys

import atheris

with atheris.instrument_imports():
    from selkies.webrtc.rtp import HeaderExtensionsMap, RtcpPacket, RtpPacket
    from selkies.webrtc.rtcrtpparameters import RTCRtpHeaderExtensionParameters, RTCRtpParameters

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


def TestOneInput(data):
    try:
        if data[:1] == b"\x01":
            RtcpPacket.parse(data[1:])
        else:
            RtpPacket.parse(data[1:], EXTENSIONS)
    except (ValueError, struct.error):
        pass


def main():
    atheris.Setup(sys.argv, TestOneInput)
    atheris.Fuzz()


if __name__ == "__main__":
    main()
