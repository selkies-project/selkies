#!/usr/bin/env python3
"""The RTP packers of the WebRTC transport, one per video codec.

Every packet stays within the MTU, a frame's key-frame flag is read from its
own bitstream, and the depayloaders rebuild what the packers cut up: H.265
through single NAL units, aggregation packets and fragmentation units, VP9
through its payload descriptor, AV1 through the frame assembler that joins
one OBU's fragments across packets and restores the size fields, and whose
packets are also checked against the payload format itself: the aggregation
header's element count and continuation bits, LEB128 lengths ahead of every
element but a counted packet's last, no temporal delimiter, no size fields.
"""
import os
import sys
from fractions import Fraction

sys.path.insert(0, os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))

from selkies.webrtc.codecs import CODECS, depayload, frame_assembler, get_encoder  # noqa: E402
from selkies.webrtc.codecs.av1 import (  # noqa: E402
    Av1Encoder, av1_assemble, av1_is_key, av1_obus, leb128, read_leb128,
)
from selkies.webrtc.codecs.base import EncodedPacket  # noqa: E402
from selkies.webrtc.codecs.h264 import PACKET_MAX  # noqa: E402
from selkies.webrtc.codecs.h265 import H265Encoder, h265_depayload  # noqa: E402
from selkies.webrtc.codecs.vp9 import Vp9Encoder, vp9_depayload, vp9_is_key, vp9_key_frame_size  # noqa: E402
from selkies.webrtc.codecs.vpx import Vp8Encoder  # noqa: E402
from selkies.webrtc.rtcrtpparameters import RTCRtpCodecParameters  # noqa: E402

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [rtp-packetizers] {label}  {detail}", flush=True)


def packet(data: bytes) -> EncodedPacket:
    return EncodedPacket(data, 9000, Fraction(1, 90000))


def codec(mime: str) -> RTCRtpCodecParameters:
    return next(c for c in CODECS["video"] if c.mimeType.lower() == mime)


# --- registry ---------------------------------------------------------------
offered = [c.mimeType for c in CODECS["video"] if "rtx" not in c.mimeType.lower()]
check("every video codec is offered",
      all(m in offered for m in ("video/H264", "video/VP8", "video/VP9", "video/AV1", "video/H265")),
      offered)
for mime, cls in (("video/vp9", Vp9Encoder), ("video/av1", Av1Encoder), ("video/h265", H265Encoder),
                  ("video/vp8", Vp8Encoder)):
    check(f"{mime} has a packer", isinstance(get_encoder(codec(mime)), cls), mime)


# --- H.265 ------------------------------------------------------------------
def h265_nal(nal_type: int, body: bytes, tid: int = 0) -> bytes:
    return bytes([(nal_type << 1), 1 + tid]) + body


START = b"\x00\x00\x00\x01"
vps, sps, pps = h265_nal(32, b"\x0c\x01\xff\xff"), h265_nal(33, b"\x01\x60"), h265_nal(34, b"\xc1\x72")
idr = h265_nal(19, bytes(range(256)) * 20)
trail = h265_nal(1, bytes(range(256)) * 3)
au_key = START + vps + START + sps + START + pps + START + idr
au_delta = b"\x00\x00\x01" + trail

payloads, ts, key = H265Encoder().pack(packet(au_key))
check("h265: key frame flagged from its IRAP NAL", key is True, key)
check("h265: packets within the MTU", all(len(p) <= PACKET_MAX for p in payloads),
      max(len(p) for p in payloads))
check("h265: parameter sets aggregated ahead of the fragments",
      (payloads[0][0] >> 1) & 0x3F == 48 and (payloads[1][0] >> 1) & 0x3F == 49,
      [(p[0] >> 1) & 0x3F for p in payloads[:3]])
check("h265: the aggregation packet's TID is the lowest of its NALs", payloads[0][1] & 0x07 == 1)
rebuilt = b"".join(h265_depayload(p) for p in payloads)
check("h265: the depayloaded access unit is the one packed",
      rebuilt == START + vps + START + sps + START + pps + START + idr, len(rebuilt))
payloads, ts, key = H265Encoder().pack(packet(au_delta))
check("h265: delta frame not flagged", key is False, key)
check("h265: a small NAL travels as a single NAL unit packet",
      len(payloads) == 1 and payloads[0] == trail, len(payloads))
check("h265: a single NAL unit depayloads with its start code",
      h265_depayload(payloads[0]) == START + trail)
check("h265: RTP timestamp in the 90 kHz clock", ts == 9000, ts)


# --- VP9 --------------------------------------------------------------------
def vp9_key_header(width: int, height: int) -> bytes:
    """A profile-0 key frame's uncompressed header up to the frame size."""
    bits = "10" + "0" + "0" + "0" + "0" + "1" + "0"          # marker, profile 0, not existing, key, shown, not resilient
    bits += format(0x49, "08b") + format(0x83, "08b") + format(0x42, "08b")
    bits += "001" + "0"                                       # color space BT.601, limited range
    bits += format(width - 1, "016b") + format(height - 1, "016b")
    bits += "0" * ((8 - len(bits) % 8) % 8)
    return int(bits, 2).to_bytes(len(bits) // 8, "big")


vp9_key = vp9_key_header(1280, 720) + bytes(range(256)) * 12
vp9_delta = bytes([0x84]) + bytes(range(256)) * 2
check("vp9: key frame read from the uncompressed header", vp9_is_key(vp9_key) and not vp9_is_key(vp9_delta))
check("vp9: frame size read from a key frame", vp9_key_frame_size(vp9_key) == (1280, 720),
      vp9_key_frame_size(vp9_key))
enc = Vp9Encoder()
payloads, ts, key = enc.pack(packet(vp9_key))
check("vp9: key frame flagged", key is True)
check("vp9: packets within the MTU", all(len(p) <= PACKET_MAX for p in payloads))
first, last = payloads[0][0], payloads[-1][0]
check("vp9: first packet marks the frame start and carries the scalability structure",
      first & 0x88 == 0x88 and first & 0x02, hex(first))
check("vp9: key frame packets are not inter-picture predicted", all(not p[0] & 0x40 for p in payloads))
check("vp9: last packet marks the frame end", last & 0x04 and not any(p[0] & 0x04 for p in payloads[:-1]))
check("vp9: every packet carries a 15-bit picture id",
      all(p[0] & 0x80 and p[1] & 0x80 for p in payloads))
pid = ((payloads[0][1] & 0x7F) << 8) | payloads[0][2]
ss = payloads[0][3:3 + 8]
check("vp9: one spatial layer at the frame's size, one picture group",
      ss[0] == 0x18 and int.from_bytes(ss[1:3], "big") == 1280 and int.from_bytes(ss[3:5], "big") == 720
      and ss[5:8] == bytes([1, 0x14, 1]), ss.hex())
check("vp9: the depayloaded frame is the one packed",
      b"".join(vp9_depayload(p) for p in payloads) == vp9_key)
payloads2, _, key2 = enc.pack(packet(vp9_delta))
pid2 = ((payloads2[0][1] & 0x7F) << 8) | payloads2[0][2]
check("vp9: delta frame flagged inter-predicted, picture id advanced",
      key2 is False and all(p[0] & 0x40 for p in payloads2) and pid2 == (pid + 1) % (1 << 15)
      and not payloads2[0][0] & 0x02, (pid, pid2))
check("vp9: the delta frame depayloads whole",
      b"".join(vp9_depayload(p) for p in payloads2) == vp9_delta)


# --- AV1 --------------------------------------------------------------------
def obu(obu_type: int, payload: bytes) -> bytes:
    return bytes([(obu_type << 3) | 0x02]) + leb128(len(payload)) + payload


def element(obu_bytes: bytes) -> bytes:
    """The OBU as an element: its header without the size flag, no size field."""
    size, pos = read_leb128(obu_bytes, 1)
    return bytes([obu_bytes[0] & ~0x02]) + obu_bytes[pos:]


td = obu(2, b"")
seq = obu(1, b"\x00\x00\x00\x04\x3e\x7f\xff\xe8")
key_frame = obu(6, b"\x10" + bytes(range(256)) * 16)   # show_existing 0, frame_type KEY
delta_frame = obu(6, b"\x30" + bytes(range(256)) * 2)  # frame_type INTER
padding = obu(15, b"\x00" * 10)
tu_key = td + seq + key_frame + padding
tu_delta = td + delta_frame

check("av1: LEB128 round trip", all(read_leb128(leb128(n), 0)[0] == n for n in (0, 127, 128, 16383, 16384, 1 << 20)))
obus = av1_obus(tu_key)
check("av1: OBUs walked with their size fields dropped",
      [t for t, _, _ in obus] == [2, 1, 6, 15] and all(not h[0] & 0x02 for _, h, _ in obus))
check("av1: key frame read from the frame OBU", av1_is_key(obus) and not av1_is_key(av1_obus(tu_delta)))


def av1_elements(payloads: list) -> tuple[list, list]:
    """Each packet's (Z, Y, W, N) and the OBUs re-assembled across packets."""
    flags, obus_out, pending = [], [], b""
    for p in payloads:
        z, y, w, n = bool(p[0] & 0x80), bool(p[0] & 0x40), (p[0] >> 4) & 3, bool(p[0] & 0x08)
        flags.append((z, y, w, n))
        pos, elements = 1, []
        while pos < len(p):
            if w == 0 or len(elements) < w - 1:
                size, pos = read_leb128(p, pos)
            else:
                size = len(p) - pos
            elements.append(p[pos:pos + size])
            pos += size
        if w:
            assert len(elements) == w, (len(elements), w)
        for i, el in enumerate(elements):
            if i == 0 and z:
                pending += el
            else:
                if pending:
                    obus_out.append(pending)
                pending = el
            if i == len(elements) - 1 and y:
                continue
            obus_out.append(pending)
            pending = b""
    if pending:
        obus_out.append(pending)
    return flags, obus_out


enc = Av1Encoder()
payloads, ts, key = enc.pack(packet(tu_key))
check("av1: key frame flagged", key is True)
check("av1: packets within the MTU", all(len(p) <= PACKET_MAX for p in payloads), max(len(p) for p in payloads))
flags, rebuilt = av1_elements(payloads)
check("av1: N set on the key frame's first packet only",
      flags[0][3] and not any(f[3] for f in flags[1:]), flags[:2])
check("av1: the first element never continues a previous packet; Y then Z chain the fragments",
      not flags[0][0] and all(flags[i][1] == flags[i + 1][0] for i in range(len(flags) - 1))
      and not flags[-1][1], flags)
check("av1: the temporal delimiter and padding are not sent, the rest arrives whole",
      rebuilt == [element(seq), element(key_frame)],
      [len(o) for o in rebuilt])
payloads, ts, key = enc.pack(packet(tu_delta))
flags, rebuilt = av1_elements(payloads)
check("av1: delta frame not flagged, N clear", key is False and not flags[0][3])
check("av1: the delta frame's OBU arrives whole", rebuilt == [element(delta_frame)])
small = td + obu(5, b"\x01\x02") + obu(6, b"\x30" + b"\x07" * 40)
payloads, _, _ = enc.pack(packet(small))
check("av1: small OBUs share one packet with a counted W and an unprefixed last element",
      len(payloads) == 1 and (payloads[0][0] >> 4) & 3 == 2 and payloads[0][1] == 3
      and payloads[0][-41:] == b"\x30" + b"\x07" * 40, payloads[0][:4].hex())
check("av1: packets depayload as they are, the frame assembler joins them",
      depayload(codec("video/av1"), payloads[0]) == payloads[0]
      and frame_assembler(codec("video/av1")) is av1_assemble
      and frame_assembler(codec("video/vp9")) is None)
payloads, _, _ = enc.pack(packet(tu_key))
check("av1: the assembled key unit is the packed one, delimiter restored, padding gone",
      av1_assemble([depayload(codec("video/av1"), p) for p in payloads]) == td + seq + key_frame,
      len(payloads))
payloads, _, _ = enc.pack(packet(tu_delta))
check("av1: the assembled delta unit is the packed one",
      av1_assemble(payloads) == td + delta_frame)
payloads, _, _ = enc.pack(packet(tu_key))
check("av1: continuations whose start is missing are dropped, the unit stays well formed",
      len(payloads) > 2 and av1_assemble(payloads[1:]) == td, len(payloads))
seq_sized, frame_sized = obu(1, b"\x00\x00\x00\x04\x3e\x7f\xff\xe8"), obu(6, b"\x10" + bytes(48))
sized_payloads = [bytes([0x20]) + leb128(len(seq_sized)) + seq_sized + frame_sized]
check("av1: an element that kept its size field passes through unchanged",
      av1_assemble(sized_payloads) == td + seq_sized + frame_sized)


# --- H.264 / VP8 key-frame flags -------------------------------------------
sps264 = b"\x67\x64\x00\x1f\xac"
idr264 = b"\x65" + bytes(range(256)) * 8
p264 = b"\x41" + bytes(range(256))
_, _, key = get_encoder(codec("video/h264")).pack(packet(START + sps264 + START + idr264))
check("h264: key frame flagged from its SPS/IDR", key is True)
_, _, key = get_encoder(codec("video/h264")).pack(packet(START + p264))
check("h264: delta frame not flagged", key is False)
_, _, key = Vp8Encoder().pack(packet(b"\x10\x02\x00\x9d\x01\x2a" + bytes(64)))
check("vp8: key frame flagged from the frame tag", key is True)
_, _, key = Vp8Encoder().pack(packet(b"\x11\x02\x00" + bytes(64)))
check("vp8: delta frame not flagged", key is False)

print(f"{passed} passed, {failed} failed")
sys.exit(1 if failed else 0)
