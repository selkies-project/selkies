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

import fractions
import uuid
from abc import ABCMeta, abstractmethod
from typing import Any

from pyee.asyncio import AsyncIOEventEmitter

VIDEO_CLOCK_RATE = 90000
VIDEO_TIME_BASE = fractions.Fraction(1, VIDEO_CLOCK_RATE)


def convert_timebase(
    pts: int, from_base: fractions.Fraction, to_base: fractions.Fraction
) -> int:
    if from_base != to_base:
        pts = int(pts * from_base / to_base)
    return pts


class MediaStreamError(Exception):
    pass


class MediaStreamTrack(AsyncIOEventEmitter, metaclass=ABCMeta):
    """
    A single media track within a stream.
    """

    kind = "unknown"

    def __init__(self) -> None:
        super().__init__()
        self.__ended = False
        self._id = str(uuid.uuid4())

    @property
    def id(self) -> str:
        """
        An automatically generated globally unique ID.
        """
        return self._id

    @property
    def readyState(self) -> str:
        return "ended" if self.__ended else "live"

    @abstractmethod
    async def recv(self) -> Any:
        """Receive the next media sample. Selkies' tracks serve a pre-encoded
        `EncodedPacket` from pixelflux/pcmflux."""

    def stop(self) -> None:
        if not self.__ended:
            self.__ended = True
            self.emit("ended")

            # no more events will be emitted, so remove all event listeners
            # to facilitate garbage collection.
            self.remove_all_listeners()


class AudioStreamTrack(MediaStreamTrack):
    """Base audio track. Selkies' `AudioMedia` overrides `recv` to serve
    pre-encoded packets from pcmflux; the base itself produces nothing."""

    kind = "audio"

    async def recv(self) -> Any:
        raise NotImplementedError("recv() must be implemented by a subclass")


class VideoStreamTrack(MediaStreamTrack):
    """Base video track. Selkies' `VideoMedia` overrides `recv` to serve
    pre-encoded packets from pixelflux; the base itself produces nothing."""

    kind = "video"

    async def recv(self) -> Any:
        raise NotImplementedError("recv() must be implemented by a subclass")
