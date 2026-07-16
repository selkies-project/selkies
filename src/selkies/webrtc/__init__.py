# ruff: noqa: F401

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

import logging

from .exceptions import InvalidAccessError, InvalidStateError
from .mediastreams import (
    AudioStreamTrack,
    MediaStreamError,
    MediaStreamTrack,
    VideoStreamTrack,
)
from .rtcconfiguration import RTCBundlePolicy, RTCConfiguration, RTCIceServer
from .rtcdatachannel import RTCDataChannel, RTCDataChannelParameters
from .rtcdtlstransport import (
    RTCCertificate,
    RTCDtlsFingerprint,
    RTCDtlsParameters,
    RTCDtlsTransport,
)
from .rtcicetransport import (
    RTCIceCandidate,
    RTCIceGatherer,
    RTCIceParameters,
    RTCIceTransport,
)
from .rtcpeerconnection import RTCPeerConnection
from .rtcrtpparameters import (
    RTCRtcpParameters,
    RTCRtpCapabilities,
    RTCRtpCodecCapability,
    RTCRtpCodecParameters,
    RTCRtpHeaderExtensionCapability,
    RTCRtpHeaderExtensionParameters,
    RTCRtpParameters,
)
from .rtcrtpreceiver import (
    RTCRtpContributingSource,
    RTCRtpReceiver,
    RTCRtpSynchronizationSource,
)
from .rtcrtpsender import RTCRtpSender
from .rtcrtptransceiver import RTCRtpTransceiver
from .rtcsctptransport import RTCSctpCapabilities, RTCSctpTransport
from .rtcsessiondescription import RTCSessionDescription
from .stats import (
    RTCInboundRtpStreamStats,
    RTCOutboundRtpStreamStats,
    RTCRemoteInboundRtpStreamStats,
    RTCRemoteOutboundRtpStreamStats,
    RTCStatsReport,
    RTCTransportStats,
)

__version__ = "1.15.0"

# Set default logging handler to avoid "No handler found" warnings.
logging.getLogger(__name__).addHandler(logging.NullHandler())

__all__ = [
    "AudioStreamTrack",
    "InvalidAccessError",
    "InvalidStateError",
    "MediaStreamError",
    "MediaStreamTrack",
    "RTCBundlePolicy",
    "RTCCertificate",
    "RTCConfiguration",
    "RTCDataChannel",
    "RTCDataChannelParameters",
    "RTCDtlsFingerprint",
    "RTCDtlsParameters",
    "RTCDtlsTransport",
    "RTCIceCandidate",
    "RTCIceGatherer",
    "RTCIceParameters",
    "RTCIceServer",
    "RTCIceTransport",
    "RTCInboundRtpStreamStats",
    "RTCOutboundRtpStreamStats",
    "RTCPeerConnection",
    "RTCRemoteInboundRtpStreamStats",
    "RTCRemoteOutboundRtpStreamStats",
    "RTCRtcpParameters",
    "RTCRtpCapabilities",
    "RTCRtpCodecCapability",
    "RTCRtpCodecParameters",
    "RTCRtpContributingSource",
    "RTCRtpHeaderExtensionCapability",
    "RTCRtpHeaderExtensionParameters",
    "RTCRtpParameters",
    "RTCRtpReceiver",
    "RTCRtpSender",
    "RTCRtpSynchronizationSource",
    "RTCRtpTransceiver",
    "RTCSctpCapabilities",
    "RTCSctpTransport",
    "RTCSessionDescription",
    "RTCStatsReport",
    "RTCTransportStats",
    "VideoStreamTrack",
]
