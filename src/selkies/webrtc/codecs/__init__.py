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

from typing import Optional, Union

from ..rtcrtpparameters import (
    ParametersDict,
    RTCRtcpFeedback,
    RTCRtpCapabilities,
    RTCRtpCodecCapability,
    RTCRtpCodecParameters,
    RTCRtpHeaderExtensionCapability,
    RTCRtpHeaderExtensionParameters,
)
from .base import Decoder, Encoder
from .g711 import PcmaDecoder, PcmaEncoder, PcmuDecoder, PcmuEncoder
from .g722 import G722Decoder, G722Encoder
from .h264 import H264Decoder, H264Encoder, h264_depayload
from .opus import OpusDecoder, OpusEncoder
from .red import RedOpusEncoder, red_block_payload_type
from .vpx import Vp8Decoder, Vp8Encoder, vp8_depayload

# The clockrate for G.722 is 8kHz even though the sampling rate is 16kHz.
# See https://datatracker.ietf.org/doc/html/rfc3551
G722_CODEC = RTCRtpCodecParameters(
    mimeType="audio/G722", clockRate=8000, channels=1, payloadType=9
)
PCMU_CODEC = RTCRtpCodecParameters(
    mimeType="audio/PCMU", clockRate=8000, channels=1, payloadType=0
)
PCMA_CODEC = RTCRtpCodecParameters(
    mimeType="audio/PCMA", clockRate=8000, channels=1, payloadType=8
)
# Opus wrapped in RFC 2198 RED; offered first so peers negotiate redundancy,
# with plain opus (PT96) as the fallback for peers that decline red.
RED_CODEC = RTCRtpCodecParameters(
    mimeType="audio/red",
    clockRate=48000,
    channels=2,
    payloadType=63,
    parameters={"96/96": None},
)

CODECS: dict[str, list[RTCRtpCodecParameters]] = {
    "audio": [
        RED_CODEC,
        RTCRtpCodecParameters(
            mimeType="audio/opus",
            clockRate=48000,
            channels=2,
            payloadType=96,
            rtcpFeedback=[RTCRtcpFeedback(type="transport-cc")],
        ),
        G722_CODEC,
        PCMU_CODEC,
        PCMA_CODEC,
    ],
    "video": [],
}

# Chromium's multistream-Opus surround layouts (matching libwebrtc).
MULTIOPUS_LAYOUTS: dict[int, dict[str, str]] = {
    6: {"channel_mapping": "0,4,1,2,3,5", "num_streams": "4", "coupled_streams": "2"},
    8: {
        "channel_mapping": "0,6,1,2,3,4,5,7",
        "num_streams": "5",
        "coupled_streams": "3",
    },
}


def configure_multiopus(channels: int) -> None:
    """Offer surround audio as Chromium's non-standard `multiopus` codec instead of
    stereo opus/RED (browsers do not RED multiopus). No-op for unknown layouts."""
    layout = MULTIOPUS_LAYOUTS.get(channels)
    if layout is None:
        return
    CODECS["audio"] = [
        RTCRtpCodecParameters(
            mimeType="audio/multiopus",
            clockRate=48000,
            channels=channels,
            payloadType=96,
            rtcpFeedback=[RTCRtcpFeedback(type="transport-cc")],
            parameters=dict(layout),
        ),
        G722_CODEC,
        PCMU_CODEC,
        PCMA_CODEC,
    ]
# Note, the id space for these extensions is shared across media types when BUNDLE
# is negotiated. If you add a audio- or video-specific extension, make sure it has
# a unique id.
HEADER_EXTENSIONS: dict[str, list[RTCRtpHeaderExtensionParameters]] = {
    "audio": [
        RTCRtpHeaderExtensionParameters(
            id=1, uri="urn:ietf:params:rtp-hdrext:sdes:mid"
        ),
        RTCRtpHeaderExtensionParameters(
            id=2, uri="urn:ietf:params:rtp-hdrext:ssrc-audio-level"
        ),
        RTCRtpHeaderExtensionParameters(
            id=5,
            uri="http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01",
        ),
    ],
    "video": [
        RTCRtpHeaderExtensionParameters(
            id=1, uri="urn:ietf:params:rtp-hdrext:sdes:mid"
        ),
        RTCRtpHeaderExtensionParameters(
            id=3, uri="http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time"
        ),
        RTCRtpHeaderExtensionParameters(
            id=4, uri="http://www.webrtc.org/experiments/rtp-hdrext/playout-delay"
        ),
        RTCRtpHeaderExtensionParameters(
            id=5,
            uri="http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01",
        ),
        RTCRtpHeaderExtensionParameters(
            id=6, uri="http://www.webrtc.org/experiments/rtp-hdrext/video-timing"
        ),
    ],
}


def init_codecs() -> None:
    dynamic_pt = 97

    def add_video_codec(
        mimeType: str, parameters: Optional[ParametersDict] = None
    ) -> None:
        nonlocal dynamic_pt

        clockRate = 90000
        CODECS["video"] += [
            RTCRtpCodecParameters(
                mimeType=mimeType,
                clockRate=clockRate,
                payloadType=dynamic_pt,
                rtcpFeedback=[
                    RTCRtcpFeedback(type="nack"),
                    RTCRtcpFeedback(type="nack", parameter="pli"),
                    RTCRtcpFeedback(type="ccm", parameter="fir"),
                    RTCRtcpFeedback(type="goog-remb"),
                    RTCRtcpFeedback(type="transport-cc"),
                ],
                parameters=parameters or {},
            ),
            RTCRtpCodecParameters(
                mimeType="video/rtx",
                clockRate=clockRate,
                payloadType=dynamic_pt + 1,
                parameters={"apt": dynamic_pt},
            ),
        ]
        dynamic_pt += 2

    # VP8 is intentionally NOT offered: pixelflux only produces H.264, so
    # negotiating VP8 would leave the server unable to send the codec the client
    # picked. The VPX infrastructure is kept dormant for a future re-wire —
    # webrtc/codecs/vpx.py (Vp8Encoder/Vp8Decoder/vp8_depayload) plus the
    # get_encoder/get_decoder and depayload() dispatch branches below stay in
    # place. To re-enable once pixelflux can emit VPX, re-add the offer here
    # (and add 'vp8enc' to the encoder_rtc allowed list).
    for profile_level_id in ("42001f", "42e01f"):
        add_video_codec(
            "video/H264",
            {
                "level-asymmetry-allowed": "1",
                "packetization-mode": "1",
                "profile-level-id": profile_level_id,
            },
        )

    # FlexFEC (draft-03): one m-line-level repair stream (its SSRC is announced via
    # the FEC-FR ssrc-group); Chrome negotiates it by default, other browsers drop it.
    CODECS["video"].append(
        RTCRtpCodecParameters(
            mimeType="video/flexfec-03",
            clockRate=90000,
            payloadType=dynamic_pt,
            parameters={"repair-window": "10000000"},
        )
    )
    dynamic_pt += 1


def depayload(codec: RTCRtpCodecParameters, payload: bytes) -> bytes:
    if codec.name == "VP8":
        return vp8_depayload(payload)
    elif codec.name == "H264":
        return h264_depayload(payload)
    else:
        return payload


def get_capabilities(kind: str) -> RTCRtpCapabilities:
    if kind not in CODECS:
        raise ValueError(f"cannot get capabilities for unknown media {kind}")

    codecs = []
    rtx_added = False
    for params in CODECS[kind]:
        if not is_rtx(params):
            codecs.append(
                RTCRtpCodecCapability(
                    mimeType=params.mimeType,
                    clockRate=params.clockRate,
                    channels=params.channels,
                    parameters=params.parameters,
                )
            )
        elif not rtx_added:
            # There will only be a single entry in codecs[] for retransmission
            # via RTX, with sdpFmtpLine not present.
            codecs.append(
                RTCRtpCodecCapability(
                    mimeType=params.mimeType, clockRate=params.clockRate
                )
            )
            rtx_added = True

    headerExtensions = []
    for extension in HEADER_EXTENSIONS[kind]:
        headerExtensions.append(RTCRtpHeaderExtensionCapability(uri=extension.uri))
    return RTCRtpCapabilities(codecs=codecs, headerExtensions=headerExtensions)


def get_decoder(codec: RTCRtpCodecParameters) -> Decoder:
    mimeType = codec.mimeType.lower()

    if mimeType == "audio/g722":
        return G722Decoder()
    elif mimeType in ("audio/opus", "audio/multiopus"):
        return OpusDecoder()
    elif mimeType == "audio/pcma":
        return PcmaDecoder()
    elif mimeType == "audio/pcmu":
        return PcmuDecoder()
    elif mimeType == "video/h264":
        return H264Decoder()
    elif mimeType == "video/vp8":
        return Vp8Decoder()
    else:
        raise ValueError(f"No decoder found for MIME type `{mimeType}`")


def get_encoder(codec: RTCRtpCodecParameters) -> Encoder:
    mimeType = codec.mimeType.lower()

    if mimeType == "audio/g722":
        return G722Encoder()
    elif mimeType in ("audio/opus", "audio/multiopus"):
        # Multiopus reaches this encoder only as pre-encoded packets (pack()), which
        # is channel-count-agnostic; the PCM encode() path stays stereo-only.
        return OpusEncoder()
    elif mimeType == "audio/red":
        return RedOpusEncoder(block_pt=red_block_payload_type(codec.parameters))
    elif mimeType == "audio/pcma":
        return PcmaEncoder()
    elif mimeType == "audio/pcmu":
        return PcmuEncoder()
    elif mimeType == "video/h264":
        return H264Encoder()
    elif mimeType == "video/vp8":
        return Vp8Encoder()
    else:
        raise ValueError(f"No encoder found for MIME type `{mimeType}`")


def is_rtx(codec: Union[RTCRtpCodecCapability, RTCRtpCodecParameters]) -> bool:
    return codec.name.lower() == "rtx"


def is_red(codec: Union[RTCRtpCodecCapability, RTCRtpCodecParameters]) -> bool:
    return codec.name.lower() == "red"


init_codecs()
