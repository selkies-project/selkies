#
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
"""RFC 9628 packetization of VP9 frames in the non-flexible mode a single-layer
stream takes: a 15-bit picture id on every packet, the inter-picture flag off
on key frames, which also open with the scalability structure (one spatial
layer at the frame's size, one temporal layer, each picture predicted from
the one before), and the frame's first and last packets marked."""

import random
from struct import pack
from typing import Optional, Union

from ..mediastreams import VIDEO_TIME_BASE, convert_timebase
from .base import Decoder, Encoder, EncodedPacket
from .h264 import PACKET_MAX

Buffer = Union[bytes, memoryview]

FLAG_I = 0x80  # picture id present
FLAG_P = 0x40  # inter-picture predicted
FLAG_L = 0x20  # layer indices present
FLAG_F = 0x10  # flexible mode
FLAG_B = 0x08  # first packet of a frame
FLAG_E = 0x04  # last packet of a frame
FLAG_V = 0x02  # scalability structure present
FLAG_Z = 0x01  # not a reference for upper spatial layers

VP9_SYNC_CODE = (0x49, 0x83, 0x42)


class _BitReader:
    def __init__(self, data: Buffer) -> None:
        self.data = data
        self.pos = 0

    def u(self, bits: int) -> int:
        value = 0
        for _ in range(bits):
            byte = self.data[self.pos >> 3]
            value = (value << 1) | ((byte >> (7 - (self.pos & 7))) & 1)
            self.pos += 1
        return value


def vp9_profile(frame: Buffer) -> int:
    b = frame[0]
    return ((b >> 5) & 1) | (((b >> 4) & 1) << 1)


def vp9_is_key(frame: Buffer) -> bool:
    """Read from the uncompressed header: not a shown existing frame, and
    `frame_type` KEY_FRAME. Profile 3 carries a reserved bit ahead of both."""
    if not frame or frame[0] >> 6 != 0b10:
        return False
    b = frame[0]
    if vp9_profile(frame) == 3:
        return b & 0x04 == 0 and b & 0x02 == 0
    return b & 0x08 == 0 and b & 0x04 == 0


def vp9_key_frame_size(frame: Buffer) -> Optional[tuple[int, int]]:
    """The frame size a key frame's uncompressed header states, or None for
    anything that is not a parseable key frame."""
    if len(frame) < 10 or not vp9_is_key(frame):
        return None
    try:
        r = _BitReader(frame)
        r.u(2)
        profile = r.u(1) | (r.u(1) << 1)
        if profile == 3:
            r.u(1)
        r.u(1)  # show_existing_frame
        r.u(1)  # frame_type
        r.u(1)  # show_frame
        r.u(1)  # error_resilient_mode
        if (r.u(8), r.u(8), r.u(8)) != VP9_SYNC_CODE:
            return None
        if profile >= 2:
            r.u(1)  # ten_or_twelve_bit
        color_space = r.u(3)
        if color_space != 7:
            r.u(1)  # color_range
            if profile in (1, 3):
                r.u(3)  # subsampling_x, subsampling_y, reserved
        elif profile in (1, 3):
            r.u(1)
        width = r.u(16) + 1
        height = r.u(16) + 1
        return width, height
    except IndexError:
        return None


class Vp9Decoder(Decoder):
    pass


class Vp9Encoder(Encoder):
    def __init__(self) -> None:
        self.picture_id = random.randint(0, (1 << 15) - 1)

    def pack(self, packet: EncodedPacket) -> tuple[list[bytes], int, bool]:
        frame = memoryview(packet.data)
        keyframe = vp9_is_key(frame)
        payloads = self._packetize(frame, self.picture_id, keyframe)
        timestamp = convert_timebase(packet.pts, packet.time_base, VIDEO_TIME_BASE)
        self.picture_id = (self.picture_id + 1) % (1 << 15)
        return payloads, timestamp, keyframe

    @staticmethod
    def _scalability_structure(frame: Buffer) -> bytes:
        size = vp9_key_frame_size(frame)
        # One spatial layer, its resolution when known, one picture group whose
        # single picture (temporal layer 0, an up-switch point) references the
        # picture before it.
        ss = bytearray([0x08 | (0x10 if size else 0)])
        if size:
            ss += pack("!HH", size[0], size[1])
        ss += bytes([1, 0x14, 1])
        return bytes(ss)

    @classmethod
    def _packetize(cls, frame: Buffer, picture_id: int, keyframe: bool) -> list[bytes]:
        pid = pack("!H", 0x8000 | picture_id)
        ss = cls._scalability_structure(frame) if keyframe else b""
        payloads = []
        length = len(frame)
        pos = 0
        first = True
        while pos < length or first:
            flags = FLAG_I | (0 if keyframe else FLAG_P)
            if first:
                flags |= FLAG_B | (FLAG_V if keyframe else 0)
            descriptor = bytes([flags]) + pid + (ss if first else b"")
            size = min(length - pos, PACKET_MAX - len(descriptor))
            end = pos + size
            if end >= length:
                descriptor = bytes([flags | FLAG_E]) + descriptor[1:]
            payloads.append(b"".join((descriptor, frame[pos:end])))
            pos = end
            first = False
        return payloads


def vp9_depayload(payload: bytes) -> bytes:
    if len(payload) < 1:
        raise ValueError("VP9 descriptor is too short")
    flags = payload[0]
    pos = 1
    try:
        if flags & FLAG_I:
            pos += 2 if payload[pos] & 0x80 else 1
        if flags & FLAG_L:
            pos += 1
            if not flags & FLAG_F:
                pos += 1  # TL0PICIDX
        if flags & FLAG_F and flags & FLAG_P:
            while True:
                more = payload[pos] & 1
                pos += 1
                if not more:
                    break
        if flags & FLAG_V:
            b = payload[pos]
            pos += 1
            layers = (b >> 5) + 1
            if b & 0x10:
                pos += 4 * layers
            if b & 0x08:
                groups = payload[pos]
                pos += 1
                for _ in range(groups):
                    pos += 1 + ((payload[pos] >> 2) & 3)
    except IndexError:
        raise ValueError("VP9 descriptor is truncated")
    if pos > len(payload):
        raise ValueError("VP9 descriptor is truncated")
    return payload[pos:]
