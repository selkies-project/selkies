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

import collections
from typing import Optional

from av.frame import Frame
from av.packet import Packet

from ... import audio_config
from ..mediastreams import convert_timebase
from .base import Encoder
from .opus import TIME_BASE, OpusEncoder

# RFC 2198 field limits: 14-bit timestamp offset, 10-bit block length.
MAX_TIMESTAMP_OFFSET = 0x3FFF
MAX_BLOCK_LENGTH = 0x3FF

# Opus payload type nominally encapsulated by RED when the fmtp is missing.
DEFAULT_BLOCK_PT = 96

# Default number of prior frames carried as redundancy (RFC 2198 distance).
DEFAULT_DISTANCE = 2
MAX_DISTANCE = 4


def red_block_payload_type(
    parameters: Optional[dict], default: int = DEFAULT_BLOCK_PT
) -> int:
    """Extract the inner payload type from a red codec fmtp such as ``96/96``."""
    for key in parameters or {}:
        head = str(key).split("/", 1)[0].strip()
        if head.isdigit():
            return int(head)
    return default


def _build_red(
    history: list[tuple[bytes, int]], primary: bytes, primary_ts: int, block_pt: int
) -> bytes:
    """Assemble one RFC 2198 payload: redundant blocks (oldest first) + primary.

    Redundant blocks whose age or size overflow the 14/10-bit header fields are
    skipped; an empty history yields a valid primary-only payload.
    """
    headers = bytearray()
    datas = bytearray()
    for payload, ts in history:
        offset = primary_ts - ts
        if offset < 1 or offset > MAX_TIMESTAMP_OFFSET:
            continue
        length = len(payload)
        if length > MAX_BLOCK_LENGTH:
            continue
        # F bit set marks a non-final (redundant) 4-byte block header.
        headers.append(0x80 | (block_pt & 0x7F))
        combined = (offset << 10) | length
        headers.append((combined >> 16) & 0xFF)
        headers.append((combined >> 8) & 0xFF)
        headers.append(combined & 0xFF)
        datas += payload
    # F bit clear marks the final (primary) 1-byte block header.
    headers.append(block_pt & 0x7F)
    datas += primary
    return bytes(headers + datas)


class RedOpusEncoder(Encoder):
    """Wrap an :class:`OpusEncoder`, framing each payload as RFC 2198 RED.

    The current payload is the primary block; up to ``distance`` recent payloads
    ride along as redundancy so a single lost packet can be recovered by peers.
    """

    def __init__(self, block_pt: int = DEFAULT_BLOCK_PT, distance: Optional[int] = None):
        self.inner = OpusEncoder()
        self.block_pt = block_pt & 0x7F
        if distance is None:
            # The server publishes its resolved redundancy depth to audio_config
            # (encoders are constructed per connection by the codec factory,
            # which passes no distance).
            configured = audio_config.red_distance
            distance = DEFAULT_DISTANCE if configured is None else configured
        self.distance = max(0, min(MAX_DISTANCE, distance))
        self.history: "collections.deque[tuple[bytes, int]]" = collections.deque(
            maxlen=self.distance
        )

    def encode(
        self, frame: Frame, force_keyframe: bool = False
    ) -> tuple[list[bytes], int]:
        payloads, timestamp = self.inner.encode(frame, force_keyframe)
        if not payloads:
            return [], timestamp
        red_payloads = []
        for payload in payloads:
            red_payloads.append(
                _build_red(list(self.history), payload, timestamp, self.block_pt)
            )
            self.history.append((payload, timestamp))
        return red_payloads, timestamp

    def pack(self, packet: Packet) -> tuple[list[bytes], int]:
        timestamp = convert_timebase(packet.pts, packet.time_base, TIME_BASE)
        primary = bytes(packet)
        red = _build_red(list(self.history), primary, timestamp, self.block_pt)
        self.history.append((primary, timestamp))
        return [red], timestamp
