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
from typing import Optional

from .codecs import get_capabilities
from .rtcrtpparameters import (
    RTCRtpCodecCapability,
    RTCRtpCodecParameters,
    RTCRtpHeaderExtensionParameters,
)
from .rtcrtpreceiver import RTCRtpReceiver
from .rtcrtpsender import RTCRtpSender
from .sdp import DIRECTIONS

logger = logging.getLogger(__name__)


class RTCRtpTransceiver:
    """
    The RTCRtpTransceiver interface describes a permanent pairing of an
    :class:`RTCRtpSender` and an :class:`RTCRtpReceiver`, along with some
    shared state.
    """

    def __init__(
        self,
        kind: str,
        receiver: RTCRtpReceiver,
        sender: RTCRtpSender,
        direction: str = "sendrecv",
    ):
        self.__currentDirection: Optional[str] = None
        self.__direction = direction
        self.__kind = kind
        self.__mid: Optional[str] = None
        self.__mline_index: Optional[int] = None
        self.__receiver = receiver
        self.__sender = sender
        self.__stopped = False

        self._offerDirection: Optional[str] = None
        self._preferred_codecs: list[RTCRtpCodecCapability] = []

        # Owned and driven by RTCPeerConnection's BUNDLE bookkeeping.
        self._bundled = False
        self._codecs: list[RTCRtpCodecParameters] = []
        self._headerExtensions: list[RTCRtpHeaderExtensionParameters] = []

    @property
    def currentDirection(self) -> Optional[str]:
        """
        The currently negotiated direction of the transceiver.

        One of `'sendrecv'`, `'sendonly'`, `'recvonly'`, `'inactive'` or `None`.
        """
        return self.__currentDirection

    @property
    def direction(self) -> str:
        """
        The preferred direction of the transceiver, which will be used in
        :meth:`RTCPeerConnection.createOffer` and
        :meth:`RTCPeerConnection.createAnswer`.

        One of `'sendrecv'`, `'sendonly'`, `'recvonly'` or `'inactive'`.
        """
        return self.__direction

    @direction.setter
    def direction(self, direction: str) -> None:
        assert direction in DIRECTIONS
        self.__direction = direction

    @property
    def kind(self) -> str:
        return self.__kind

    @property
    def mid(self) -> Optional[str]:
        return self.__mid

    @property
    def receiver(self) -> RTCRtpReceiver:
        """
        The :class:`RTCRtpReceiver` that handles receiving and decoding
        incoming media.
        """
        return self.__receiver

    @property
    def sender(self) -> RTCRtpSender:
        """
        The :class:`RTCRtpSender` responsible for encoding and sending
        data to the remote peer.
        """
        return self.__sender

    @property
    def stopped(self) -> bool:
        return self.__stopped

    def setCodecPreferences(self, codecs: list[RTCRtpCodecCapability]) -> None:
        """
        Override the default codec preferences.

        See :meth:`RTCRtpSender.getCapabilities` and
        :meth:`RTCRtpReceiver.getCapabilities` for the supported codecs.

        :param codecs: A list of :class:`RTCRtpCodecCapability`, in decreasing order
                        of preference. If empty, restores the default preferences.
        """
        if not codecs:
            self._preferred_codecs = []

        capabilities = get_capabilities(self.kind).codecs
        unique: list[RTCRtpCodecCapability] = []
        for codec in reversed(codecs):
            if codec not in capabilities:
                raise ValueError("Codec is not in capabilities")
            if codec not in unique:
                unique.insert(0, codec)
        self._preferred_codecs = unique

    async def stop(self) -> None:
        """
        Permanently stops the :class:`RTCRtpTransceiver`.
        """
        await self.__receiver.stop()
        await self.__sender.stop()
        self.__stopped = True

    def _setCurrentDirection(self, direction: str) -> None:
        self.__currentDirection = direction

        if direction == "sendrecv":
            self.__sender._enabled = True
            self.__receiver._enabled = True
        elif direction == "sendonly":
            self.__sender._enabled = True
            self.__receiver._enabled = False
        elif direction == "recvonly":
            self.__sender._enabled = False
            self.__receiver._enabled = True
        elif direction == "inactive":
            self.__sender._enabled = False
            self.__receiver._enabled = False

    def _set_mid(self, mid: str) -> None:
        self.__mid = mid

    def _get_mline_index(self) -> Optional[int]:
        return self.__mline_index

    def _set_mline_index(self, idx: int) -> None:
        self.__mline_index = idx
