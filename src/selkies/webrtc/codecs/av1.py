#
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
"""AV1 RTP payload format (AOM, v1.0) packetization of temporal units, and the
reassembly of a received frame's packets back into one.

Packing: the temporal delimiter, tile lists and padding go; every other OBU
becomes an element without its size field. Elements fill packets behind a
one-byte aggregation header (Z: the first element continues an OBU from the
previous packet, Y: the last continues in the next, W: the element count when
three or fewer, N: the first packet of a coded video sequence), a LEB128 length
ahead of every element but a counted packet's last, and an OBU larger than the
room a packet has left is split across as many packets as it needs.

Assembly runs over a frame's packets in sequence order once the jitter buffer
has them all: fragments are joined across packets by the Y and Z flags, each
OBU gets the size field the elements travel without, and a temporal delimiter
opens the unit, which is then the low-overhead bitstream the virtual camera's
decoder reads. A fragment whose start was lost with a packet is dropped, as
are the rest of that OBU's fragments."""

from typing import Union

from ..mediastreams import VIDEO_TIME_BASE, convert_timebase
from .base import Decoder, Encoder, EncodedPacket
from .h264 import PACKET_MAX

Buffer = Union[bytes, memoryview]

OBU_SEQUENCE_HEADER = 1
OBU_TEMPORAL_DELIMITER = 2
OBU_FRAME_HEADER = 3
OBU_FRAME = 6
OBU_TILE_LIST = 8
OBU_PADDING = 15
DROPPED_OBUS = (OBU_TEMPORAL_DELIMITER, OBU_TILE_LIST, OBU_PADDING)

AGGREGATION_HEADER_SIZE = 1
# The smallest fragment worth starting in a packet's remaining room.
MIN_FRAGMENT = 16


def leb128(value: int) -> bytes:
    out = bytearray()
    while True:
        byte = value & 0x7F
        value >>= 7
        if value:
            out.append(byte | 0x80)
        else:
            out.append(byte)
            return bytes(out)


def read_leb128(data: Buffer, pos: int) -> tuple[int, int]:
    value = 0
    shift = 0
    while True:
        if pos >= len(data) or shift > 56:
            raise ValueError("LEB128 is truncated")
        byte = data[pos]
        pos += 1
        value |= (byte & 0x7F) << shift
        if not byte & 0x80:
            return value, pos
        shift += 7


def av1_obus(tu: Buffer) -> list[tuple[int, bytes, Buffer]]:
    """The OBUs of a temporal unit as `(type, header, payload)`, the header
    without its size flag so the element carries no size field."""
    obus = []
    pos = 0
    while pos < len(tu):
        header = tu[pos]
        obu_type = (header >> 3) & 0x0F
        has_extension = bool(header & 0x04)
        has_size = bool(header & 0x02)
        start = pos + 1 + has_extension
        if has_size:
            size, start = read_leb128(tu, start)
        else:
            size = len(tu) - start
        end = start + size
        if end > len(tu):
            raise ValueError("OBU is truncated")
        header_bytes = bytes([header & ~0x02]) + (bytes(tu[pos + 1 : pos + 2]) if has_extension else b"")
        obus.append((obu_type, header_bytes, tu[start:end]))
        pos = end
    return obus


def av1_is_key(obus: list[tuple[int, bytes, Buffer]]) -> bool:
    """Whether the unit opens with a key frame: its first frame header or frame
    OBU's `frame_type`, read past the temporal delimiter and sequence header.
    A shown existing frame, or a unit without a frame, is a delta."""
    for obu_type, _, payload in obus:
        if obu_type in (OBU_FRAME_HEADER, OBU_FRAME):
            if not payload:
                return False
            first = payload[0]
            return not first & 0x80 and (first >> 5) & 0x03 == 0
    return False


class Av1Decoder(Decoder):
    pass


def _obu_elements(payload: bytes) -> tuple[bool, bool, list[bytes]]:
    """A packet's Z and Y flags and its OBU elements, sizes read off."""
    header = payload[0]
    w = (header >> 4) & 3
    pos = 1
    elements: list[bytes] = []
    while pos < len(payload):
        if w == 0 or len(elements) < w - 1:
            size, pos = read_leb128(payload, pos)
        else:
            size = len(payload) - pos
        if pos + size > len(payload):
            raise ValueError("OBU element is truncated")
        elements.append(payload[pos : pos + size])
        pos += size
    return bool(header & 0x80), bool(header & 0x40), elements


def _sized_obu(obu: bytes) -> bytes:
    """An OBU element with the size field the bitstream format wants."""
    header = obu[0]
    header_size = 2 if header & 0x04 else 1
    if header & 0x02 or len(obu) < header_size:
        return obu
    return bytes([header | 0x02]) + obu[1:header_size] + leb128(len(obu) - header_size) + obu[header_size:]


def av1_assemble(payloads: list[bytes]) -> bytes:
    """The temporal unit a frame's packets carry, in the low-overhead bitstream
    format: a temporal delimiter, then every OBU element made whole across
    packets and given its size field."""
    out = bytearray(b"\x12\x00")
    pending = bytearray()
    for payload in payloads:
        if not payload:
            continue
        try:
            continues, continued, elements = _obu_elements(payload)
        except ValueError:
            pending = bytearray()
            continue
        for index, element in enumerate(elements):
            first = index == 0
            last = index == len(elements) - 1
            if first and continues:
                if not pending:
                    # The OBU's start went with a lost packet; so does its rest.
                    continue
                pending += element
            else:
                pending = bytearray(element)
            if last and continued:
                continue
            if pending:
                out += _sized_obu(bytes(pending))
            pending = bytearray()
    return bytes(out)


class Av1Encoder(Encoder):
    def pack(self, packet: EncodedPacket) -> tuple[list[bytes], int, bool]:
        obus = av1_obus(memoryview(packet.data))
        keyframe = av1_is_key(obus)
        elements = [b"".join((header, payload)) for obu_type, header, payload in obus
                    if obu_type not in DROPPED_OBUS]
        timestamp = convert_timebase(packet.pts, packet.time_base, VIDEO_TIME_BASE)
        return self._packetize(elements, keyframe), timestamp, keyframe

    @staticmethod
    def _packetize(elements: list[bytes], keyframe: bool) -> list[bytes]:
        packets: list[bytes] = []
        current: list[bytes] = []
        used = AGGREGATION_HEADER_SIZE
        continues_previous = False

        def flush(continues_next: bool) -> None:
            nonlocal current, used, continues_previous
            count = len(current)
            w = count if count <= 3 else 0
            header = (
                (0x80 if continues_previous else 0)
                | (0x40 if continues_next else 0)
                | (w << 4)
                | (0x08 if keyframe and not packets else 0)
            )
            body = [bytes([header])]
            for index, element in enumerate(current):
                if w == 0 or index < count - 1:
                    body.append(leb128(len(element)))
                body.append(element)
            packets.append(b"".join(body))
            current = []
            used = AGGREGATION_HEADER_SIZE
            continues_previous = continues_next

        for element in elements:
            # Every element is costed with its length prefix; a counted packet's
            # last element drops it, which only leaves room to spare.
            need = len(leb128(len(element))) + len(element)
            if used + need <= PACKET_MAX:
                current.append(element)
                used += need
                continue
            room = PACKET_MAX - used - len(leb128(PACKET_MAX))
            if room < MIN_FRAGMENT:
                flush(False)
                room = PACKET_MAX - used - len(leb128(PACKET_MAX))
            pos = 0
            while pos < len(element):
                take = min(len(element) - pos, room)
                current.append(element[pos : pos + take])
                used += len(leb128(take)) + take
                pos += take
                if pos < len(element):
                    flush(True)
                    room = PACKET_MAX - used - len(leb128(PACKET_MAX))
        if current or not packets:
            flush(False)
        return packets
