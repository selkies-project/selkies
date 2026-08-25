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

from abc import ABCMeta, abstractmethod
from fractions import Fraction
from typing import Any, Optional


class EncodedPacket:
    """A pre-encoded media sample on the WebRTC send path: the encoder's output
    buffer with its presentation timestamp and clock.

    It is what pixelflux/pcmflux hand the packers, replacing `av.Packet` as the
    passthrough container so packing pulls in no libav wrapper and allocates no
    per-frame FFmpeg object. `data` is any buffer-protocol object (the encoder's
    own memory), referenced and never copied; packers walk it through
    `memoryview(packet.data)` and materialize bytes only for the RTP payloads
    they emit. Keeping the whole-frame copy out of the path both cuts latency
    and frees the GIL that a `bytes(frame)` copy would hold for the memcpy.
    """

    __slots__ = ("data", "pts", "dts", "time_base")

    def __init__(self, data: Any, pts: Optional[int] = None,
                 time_base: Optional[Fraction] = None) -> None:
        self.data = data
        self.pts = pts
        self.dts = pts
        self.time_base = time_base

    def __len__(self) -> int:
        return len(self.data)


class Decoder:
    """Receive-codec marker. Decoding of the browser uplink is done by pcmflux
    (audio) and pixelflux (video), which the encoded frames are routed to, so a
    registry decoder carries only the codec's identity, no libav decode."""


class Encoder(metaclass=ABCMeta):
    """Packs frames a pixelflux/pcmflux encoder has already produced into RTP
    payloads. Encoding itself lives in those libraries, not here."""

    @abstractmethod
    def pack(self, packet: EncodedPacket) -> tuple[list[bytes], int]:
        pass  # pragma: no cover
