# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
# This file incorporates work covered by the following copyright and
# permission notice:
#
#   Copyright (c) Jeremy Lainé.
#   All rights reserved.
#
#   Redistribution and use in source and binary forms, with or without
#   modification, are permitted provided that the following conditions are met:
#
#       * Redistributions of source code must retain the above copyright notice,
#       this list of conditions and the following disclaimer.
#       * Redistributions in binary form must reproduce the above copyright notice,
#       this list of conditions and the following disclaimer in the documentation
#       and/or other materials provided with the distribution.
#       * Neither the name of aiortc nor the names of its contributors may
#       be used to endorse or promote products derived from this software without
#       specific prior written permission.
#
#   THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
#   ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
#   WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
#   DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
#   FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
#   DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
#   SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
#   CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
#   OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
#   OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

from dataclasses import dataclass, field
from typing import Optional, Union

ParametersDict = dict[str, Union[int, str, None]]


@dataclass
class RTCRtpCodecCapability:
    """
    The :class:`RTCRtpCodecCapability` dictionary provides information on
    codec capabilities.
    """

    mimeType: str
    "The codec MIME media type/subtype, for instance `'audio/PCMU'`."
    clockRate: int
    "The codec clock rate expressed in Hertz."
    channels: Optional[int] = None
    "The number of channels supported (e.g. two for stereo)."
    parameters: ParametersDict = field(default_factory=dict)
    "Codec-specific parameters available for signaling."

    @property
    def name(self) -> str:
        return self.mimeType.split("/")[1]


@dataclass
class RTCRtpCodecParameters:
    """
    The :class:`RTCRtpCodecParameters` dictionary provides information on
    codec settings.
    """

    mimeType: str
    "The codec MIME media type/subtype, for instance `'audio/PCMU'`."
    clockRate: int
    "The codec clock rate expressed in Hertz."
    channels: Optional[int] = None
    "The number of channels supported (e.g. two for stereo)."
    payloadType: Optional[int] = None
    "The value that goes in the RTP Payload Type Field."
    rtcpFeedback: list["RTCRtcpFeedback"] = field(default_factory=list)
    "Transport layer and codec-specific feedback messages for this codec."
    parameters: ParametersDict = field(default_factory=dict)
    "Codec-specific parameters available for signaling."

    @property
    def name(self) -> str:
        return self.mimeType.split("/")[1]

    def __str__(self) -> str:
        s = f"{self.name}/{self.clockRate}"
        # Multi-channel audio (stereo opus, surround multiopus) carries its channel
        # count in the rtpmap encoding parameters.
        if self.channels is not None and self.channels >= 2:
            s += f"/{self.channels}"
        return s


@dataclass
class RTCRtpRtxParameters:
    ssrc: int


@dataclass
class RTCRtpCodingParameters:
    ssrc: int
    payloadType: int
    rtx: Optional[RTCRtpRtxParameters] = None


class RTCRtpDecodingParameters(RTCRtpCodingParameters):
    pass


class RTCRtpEncodingParameters(RTCRtpCodingParameters):
    pass


@dataclass
class RTCRtpHeaderExtensionCapability:
    """
    The :class:`RTCRtpHeaderExtensionCapability` dictionary provides information
    on a supported header extension.
    """

    uri: str
    "The URI of the RTP header extension."


@dataclass
class RTCRtpHeaderExtensionParameters:
    """
    The :class:`RTCRtpHeaderExtensionParameters` dictionary enables a header
    extension to be configured for use within an :class:`RTCRtpSender` or
    :class:`RTCRtpReceiver`.
    """

    id: int
    "The value that goes in the packet."
    uri: str
    "The URI of the RTP header extension."


@dataclass
class RTCRtpCapabilities:
    """
    The :class:`RTCRtpCapabilities` dictionary provides information about
    support codecs and header extensions.
    """

    codecs: list[RTCRtpCodecCapability] = field(default_factory=list)
    "A list of :class:`RTCRtpCodecCapability`."
    headerExtensions: list[RTCRtpHeaderExtensionCapability] = field(
        default_factory=list
    )
    "A list of :class:`RTCRtpHeaderExtensionCapability`."


@dataclass
class RTCRtcpFeedback:
    """
    The :class:`RTCRtcpFeedback` dictionary provides information on RTCP feedback
    messages.
    """

    type: str
    parameter: Optional[str] = None


@dataclass
class RTCRtcpParameters:
    """
    The :class:`RTCRtcpParameters` dictionary provides information on RTCP settings.
    """

    cname: Optional[str] = None
    "The Canonical Name (CNAME) used by RTCP."
    mux: bool = False
    "Whether RTP and RTCP are multiplexed."
    ssrc: Optional[int] = None
    "The Synchronization Source identifier."


@dataclass
class RTCRtpParameters:
    """
    The :class:`RTCRtpParameters` dictionary describes the configuration of
    an :class:`RTCRtpReceiver` or an :class:`RTCRtpSender`.
    """

    codecs: list[RTCRtpCodecParameters] = field(default_factory=list)
    "A list of :class:`RTCRtpCodecParameters` to send or receive."
    headerExtensions: list[RTCRtpHeaderExtensionParameters] = field(
        default_factory=list
    )
    "A list of :class:`RTCRtpHeaderExtensionParameters`."
    muxId: str = ""
    "The muxId assigned to the RTP stream, if any, empty string if unset."
    rtcp: RTCRtcpParameters = field(default_factory=RTCRtcpParameters)
    "Parameters to configure RTCP."


@dataclass
class RTCRtpReceiveParameters(RTCRtpParameters):
    encodings: list[RTCRtpDecodingParameters] = field(default_factory=list)


@dataclass
class RTCRtpSendParameters(RTCRtpParameters):
    encodings: list[RTCRtpEncodingParameters] = field(default_factory=list)
