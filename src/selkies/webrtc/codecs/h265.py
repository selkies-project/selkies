#
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.
#
"""RFC 7798 packetization of H.265 access units, the H.264 packer's shape
with the two-byte NAL header: one NAL per packet where it fits, an aggregation
packet (AP, type 48) for a run of small ones, fragmentation units (FU, type 49)
for a NAL larger than a packet. Every packet's payload header is a NAL header
whose type names the packet kind and whose layer id and TID come from the NALs
it carries."""

import math
from collections.abc import Iterable, Iterator
from struct import pack, unpack_from
from typing import Optional, Union

from ..mediastreams import VIDEO_TIME_BASE, convert_timebase
from .base import Decoder, Encoder, EncodedPacket
from .h264 import PACKET_MAX, H264Encoder

NAL_HEADER_SIZE = 2
NAL_TYPE_AP = 48
NAL_TYPE_FU = 49
FU_HEADER_SIZE = NAL_HEADER_SIZE + 1
LENGTH_FIELD_SIZE = 2
AP_HEADER_SIZE = NAL_HEADER_SIZE + LENGTH_FIELD_SIZE
# IRAP pictures (BLA, IDR, CRA and the reserved IRAP types) open a key frame.
IRAP_NAL_TYPES = range(16, 24)
START_CODE = bytes([0, 0, 0, 1])

Buffer = Union[bytes, memoryview]


def nal_unit_type(nal: Buffer) -> int:
    return (nal[0] >> 1) & 0x3F


def h265_is_key(nals: Iterable[Buffer]) -> bool:
    return any(nal_unit_type(nal) in IRAP_NAL_TYPES for nal in nals)


class H265Decoder(Decoder):
    pass


class H265Encoder(Encoder):
    @staticmethod
    def _packetize_fu(data: Buffer) -> list[bytes]:
        available_size = PACKET_MAX - FU_HEADER_SIZE
        payload_size = len(data) - NAL_HEADER_SIZE
        num_packets = math.ceil(payload_size / available_size)
        num_larger_packets = payload_size % num_packets
        package_size = payload_size // num_packets

        nal_type = nal_unit_type(data)
        # The payload header keeps F, the layer id and the TID; its type is FU.
        payload_header = bytes([(data[0] & 0x81) | (NAL_TYPE_FU << 1), data[1]])
        fu_header_start = payload_header + bytes([0x80 | nal_type])
        fu_header_middle = payload_header + bytes([nal_type])
        fu_header_end = payload_header + bytes([0x40 | nal_type])
        fu_header = fu_header_start

        packages = []
        offset = NAL_HEADER_SIZE
        while offset < len(data):
            if num_larger_packets > 0:
                num_larger_packets -= 1
                payload = data[offset : offset + package_size + 1]
                offset += package_size + 1
            else:
                payload = data[offset : offset + package_size]
                offset += package_size
            if offset == len(data):
                fu_header = fu_header_end
            packages.append(b"".join((fu_header, payload)))
            fu_header = fu_header_middle
        assert offset == len(data), "incorrect fragment data"
        return packages

    @staticmethod
    def _packetize_ap(
        data: Buffer, packages_iterator: Iterator[Buffer]
    ) -> tuple[bytes, Optional[Buffer]]:
        counter = 0
        available_size = PACKET_MAX - AP_HEADER_SIZE
        forbidden = 0
        layer_id = 0x3F
        tid = 0x07

        payload = bytearray()
        try:
            nalu = data
            while len(nalu) <= available_size and counter < 9:
                forbidden |= nalu[0] & 0x80
                layer_id = min(layer_id, ((nalu[0] & 0x01) << 5) | (nalu[1] >> 3))
                tid = min(tid, nalu[1] & 0x07)
                available_size -= LENGTH_FIELD_SIZE + len(nalu)
                counter += 1
                payload += pack("!H", len(nalu))
                payload += nalu
                nalu = next(packages_iterator)
            if counter == 0:
                nalu = next(packages_iterator)
        except StopIteration:
            nalu = None

        if counter <= 1:
            return (data if isinstance(data, bytes) else bytes(data)), nalu
        header = bytes([
            forbidden | (NAL_TYPE_AP << 1) | (layer_id >> 5),
            ((layer_id & 0x1F) << 3) | tid,
        ])
        return b"".join((header, payload)), nalu

    @classmethod
    def _packetize(cls, packages: Iterable[Buffer]) -> list[bytes]:
        packetized_packages = []
        packages_iterator = iter(packages)
        package = next(packages_iterator, None)
        while package is not None:
            if len(package) > PACKET_MAX:
                packetized_packages.extend(cls._packetize_fu(package))
                package = next(packages_iterator, None)
            else:
                packetized, package = cls._packetize_ap(package, packages_iterator)
                packetized_packages.append(packetized)
        return packetized_packages

    def pack(self, packet: EncodedPacket) -> tuple[list[bytes], int, bool]:
        nals = list(H264Encoder._split_bitstream(memoryview(packet.data)))
        timestamp = convert_timebase(packet.pts, packet.time_base, VIDEO_TIME_BASE)
        return self._packetize(nals), timestamp, h265_is_key(nals)


def h265_depayload(payload: bytes) -> bytes:
    """The Annex-B bytes one packet contributes to its access unit: whole NALs
    with start codes, a fragment's data with the NAL header rebuilt on the
    first fragment only."""
    if len(payload) < NAL_HEADER_SIZE + 1:
        raise ValueError("NAL unit is too short")
    nal_type = nal_unit_type(payload)
    if nal_type < NAL_TYPE_AP:
        return START_CODE + payload
    if nal_type == NAL_TYPE_AP:
        output = bytearray()
        pos = NAL_HEADER_SIZE
        while pos < len(payload):
            if len(payload) < pos + LENGTH_FIELD_SIZE:
                raise ValueError("AP length field is truncated")
            size = unpack_from("!H", payload, pos)[0]
            pos += LENGTH_FIELD_SIZE
            if len(payload) < pos + size:
                raise ValueError("AP data is truncated")
            output += START_CODE
            output += payload[pos : pos + size]
            pos += size
        return bytes(output)
    if nal_type == NAL_TYPE_FU:
        fu_header = payload[NAL_HEADER_SIZE]
        data = payload[FU_HEADER_SIZE:]
        if fu_header & 0x80:
            nal_header = bytes([(payload[0] & 0x81) | ((fu_header & 0x3F) << 1), payload[1]])
            return START_CODE + nal_header + data
        return data
    raise ValueError(f"NAL unit type {nal_type} is not supported")
