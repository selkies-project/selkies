#!/usr/bin/env python3
"""A lost frame is left out of every later prediction instead of costing a keyframe.

The RTP sender writes the dependency descriptor the AV1 RTP specification
defines on every packet of a stream whose encoder names what each frame
predicts from: the frame's number, its edges and how far back it predicts,
with the dependency structure on a key frame's first packet. The bytes are
read back with a reader built to libwebrtc's. A second NACK for one packet
says the retransmission was lost too, and the sender then names the frame
lost, and the engine routes that to the encoder of the peer's own display. The
websockets relay is
left as it was: it drops seconds of backlog at a time, which no reference
window reaches back over, so it still skips ahead to a keyframe. Driven with
stand-ins; no peer.
"""
import asyncio
import os
import sys
from types import SimpleNamespace

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

from selkies.webrtc.codecs import HEADER_EXTENSIONS
from selkies.webrtc.rtcrtpparameters import RTCRtpHeaderExtensionParameters, RTCRtpParameters
from selkies.webrtc.rtcrtpsender import RTCRtpSender
from selkies.webrtc.rtp import (
    DEPENDENCY_DESCRIPTOR_URI, RTCP_RTPFB_NACK, HeaderExtensionsMap, RtcpRtpfbPacket, RtpHistory,
    RtpPacket, dependency_descriptor,
)
from selkies.webrtc_engine import RTCApp
from selkies.websockets_mode import _VideoRelay

res = H.Results("reference-invalidation")


class Bits:
    def __init__(self, data: bytes) -> None:
        self.data, self.pos = data, 0

    def read(self, n: int) -> int:
        value = 0
        for _ in range(n):
            value = (value << 1) | ((self.data[self.pos >> 3] >> (7 - (self.pos & 7))) & 1)
            self.pos += 1
        return value

    def ns(self, n: int) -> int:
        if n == 1:
            return 0
        width = n.bit_length()
        low = (1 << width) - n
        value = self.read(width - 1)
        return value if value < low else (value << 1) + self.read(1) - low


def read_descriptor(raw: bytes, structure=None) -> dict:
    """The descriptor as libwebrtc's reader takes it: the mandatory fields, the
    extended ones when the value runs past three bytes, and the frame's
    dependencies from its template with any custom fields over them."""
    bits = Bits(raw)
    first, last, template_id, number = bits.read(1), bits.read(1), bits.read(6), bits.read(16)
    active = custom_dtis = custom_fdiffs = custom_chains = 0
    attached = None
    if len(raw) > 3:
        present, active, custom_dtis, custom_fdiffs, custom_chains = (bits.read(1) for _ in range(5))
        if present:
            attached = {"id": bits.read(6), "decode_targets": bits.read(5) + 1, "templates": []}
            spatial = temporal = 0
            while True:
                attached["templates"].append({"spatial": spatial, "temporal": temporal})
                idc = bits.read(2)
                if idc == 1:
                    temporal += 1
                elif idc == 2:
                    spatial, temporal = spatial + 1, 0
                elif idc == 3:
                    break
            for template in attached["templates"]:
                template["dtis"] = [bits.read(2) for _ in range(attached["decode_targets"])]
            for template in attached["templates"]:
                template["fdiffs"] = []
                while bits.read(1):
                    template["fdiffs"].append(bits.read(4) + 1)
            attached["chains"] = bits.ns(attached["decode_targets"] + 1)
            if attached["chains"]:
                attached["protected_by"] = [bits.ns(attached["chains"]) for _ in range(attached["decode_targets"])]
                for template in attached["templates"]:
                    template["chain_diffs"] = [bits.read(4) for _ in range(attached["chains"])]
            attached["resolutions"] = [(bits.read(16) + 1, bits.read(16) + 1)
                                       for _ in range(attached["templates"][-1]["spatial"] + 1)] if bits.read(1) else []
    structure = attached or structure
    if structure is None:
        raise ValueError("a descriptor before any structure")
    if active:
        bits.read(structure["decode_targets"])
    frame = dict(structure["templates"][(template_id + 64 - structure["id"]) % 64])
    if custom_dtis:
        frame["dtis"] = [bits.read(2) for _ in range(structure["decode_targets"])]
    if custom_fdiffs:
        frame["fdiffs"] = []
        while True:
            size = bits.read(2)
            if not size:
                break
            frame["fdiffs"].append(bits.read(4 * size) + 1)
    if custom_chains:
        frame["chain_diffs"] = [bits.read(8) for _ in range(structure["chains"])]
    return {"first": first, "last": last, "number": number, "structure": attached, "frame": frame}


# --- the descriptor bytes --------------------------------------------------
key_first = dependency_descriptor(True, True, 5, None, True)
opened = read_descriptor(key_first)
structure = opened["structure"]
res.check("a key frame's first packet carries the structure in eight bytes",
          len(key_first) == 8 and structure is not None, key_first.hex())
res.check("the structure has one decode target and two templates on one layer",
          structure and structure["decode_targets"] == 1 and len(structure["templates"]) == 2
          and all(t["spatial"] == 0 and t["temporal"] == 0 for t in structure["templates"]), structure)
res.check("the key template predicts from nothing and the other from the frame before",
          structure and [t["fdiffs"] for t in structure["templates"]] == [[], [1]], structure)
res.check("both templates are switch points of the one target, with no chains or resolutions",
          structure and all(t["dtis"] == [2] for t in structure["templates"])
          and structure["chains"] == 0 and structure["resolutions"] == [], structure)
res.check("the key frame is frame 5, opening and closing its frame, predicting from nothing",
          (opened["first"], opened["last"], opened["number"], opened["frame"]["fdiffs"]) == (1, 1, 5, []), opened)

def frame_of(raw: bytes) -> tuple:
    d = read_descriptor(raw, structure)
    return d["first"], d["last"], d["number"], d["frame"]["fdiffs"]

res.check("a key frame's later packets carry the three mandatory bytes alone",
          len(dependency_descriptor(False, True, 5, None, True)) == 3
          and frame_of(dependency_descriptor(False, True, 5, None, True)) == (0, 1, 5, []))
res.check("a frame predicting from the one before rides its template in three bytes",
          len(dependency_descriptor(True, False, 6, 1, False)) == 3
          and frame_of(dependency_descriptor(True, False, 6, 1, False)) == (1, 0, 6, [1]))
for fdiff, size in ((2, 5), (16, 5), (17, 6), (256, 6), (257, 6), (4096, 6)):
    raw = dependency_descriptor(False, False, 7, fdiff, False)
    res.check(f"a frame predicting {fdiff} back writes its own diff in {size} bytes",
              len(raw) == size and frame_of(raw) == (0, 0, 7, [fdiff]), (len(raw), frame_of(raw)))
res.check("the frame number wraps at sixteen bits",
          frame_of(dependency_descriptor(True, True, 65536 + 3, 1, False))[2] == 3)

# --- negotiation and the packet -----------------------------------------------
offered = [e for e in HEADER_EXTENSIONS["video"] if e.uri == DEPENDENCY_DESCRIPTOR_URI]
res.check("the descriptor is offered for video under an id of its own",
          len(offered) == 1 and len({e.id for e in HEADER_EXTENSIONS["video"]}) == len(HEADER_EXTENSIONS["video"]),
          [(e.id, e.uri) for e in HEADER_EXTENSIONS["video"]])
negotiated = HeaderExtensionsMap()
negotiated.configure(RTCRtpParameters(headerExtensions=[
    RTCRtpHeaderExtensionParameters(id=1, uri="urn:ietf:params:rtp-hdrext:sdes:mid"),
    RTCRtpHeaderExtensionParameters(id=13, uri=DEPENDENCY_DESCRIPTOR_URI)]))
without = HeaderExtensionsMap()
without.configure(RTCRtpParameters(headerExtensions=[
    RTCRtpHeaderExtensionParameters(id=1, uri="urn:ietf:params:rtp-hdrext:sdes:mid")]))
res.check("the map knows whether the peer took the descriptor",
          negotiated.has_dependency_descriptor() and not without.has_dependency_descriptor())
packet = RtpPacket(payload_type=96, sequence_number=1, timestamp=90000, ssrc=7, payload=b"\x65\x88")
packet.extensions.mid = "0"
packet.extensions.dependency_descriptor = key_first
wire = packet.serialize(negotiated)
back = RtpPacket.parse(wire, negotiated)
res.check("the descriptor rides a one-byte extension header and reads back whole",
          wire[12:14] == b"\xbe\xde" and back.extensions.dependency_descriptor == key_first, wire.hex())
res.check("a peer that took no descriptor gets none on the wire",
          RtpPacket.parse(packet.serialize(without), without).extensions.dependency_descriptor is None)

# --- the history counts NACKs ---------------------------------------------------
history = RtpHistory()
for seq, frame in ((1, 10), (2, 10), (3, 11)):
    history.add(RtpPacket(payload_type=96, sequence_number=seq, payload=b"x"), 0.0, frame)
res.check("a NACK names the packet's frame and is counted",
          history.nacked(2)[1:] == (10, 1) and history.nacked(2)[1:] == (10, 2) and history.nacked(3)[1:] == (11, 1))
res.check("a packet let go reads as nothing", history.nacked(99) == (None, None, 0))
res.check("a packet added without a frame is retransmitted but names none",
          (lambda h: (h.add(RtpPacket(sequence_number=4, payload=b"x"), 0.0), h.nacked(4)[1:])[1])(history) == (None, 1))

# --- the sender's numbering ------------------------------------------------------
sender = SimpleNamespace(_RTCRtpSender__frame_number=65534, _RTCRtpSender__frame_numbers={})
describe = lambda fid, ref, key: RTCRtpSender._describe(sender, fid, ref, key)
res.check("a key frame takes the next number and predicts from nothing", describe(100, None, True) == (65534, None))
res.check("a frame predicting from the one before is one back", describe(101, 100, False) == (65535, 1))
res.check("the number wraps and a frame predicting two back is two back", describe(102, 100, False) == (0, 2))
res.check("a frame predicting from one this sender never sent is left out", describe(103, 77, False) is None)
res.check("a key frame forgets the frames before it",
          describe(104, None, True) == (1, None) and describe(105, 102, False) is None)
for fid in range(200, 270):
    describe(fid, fid - 1 if fid > 200 else None, fid == 200)
res.check("the numbering keeps the last sixty-four frames",
          len(sender._RTCRtpSender__frame_numbers) == 64 and describe(270, 205, False) is None
          and describe(271, 269, False) == (72, 1), len(sender._RTCRtpSender__frame_numbers))

# --- the sender's answer to a NACK ----------------------------------------------
async def nacks() -> None:
    history = RtpHistory()
    for seq, frame in ((1, 10), (2, 10), (3, 11)):
        history.add(RtpPacket(payload_type=96, sequence_number=seq, payload=b"x"), 0.0, frame)
    sent, events = [], []

    async def retransmit(packet):
        sent.append(packet.sequence_number)
        return True

    stand_in = SimpleNamespace(_RTCRtpSender__rtp_history=history, _retransmit=retransmit,
                               _RTCRtpSender__abandoned=None,
                               _emit_pli_event=lambda: events.append("pli"),
                               emit=lambda name, *args: events.append((name,) + args))
    nack = lambda *lost: RtcpRtpfbPacket(fmt=RTCP_RTPFB_NACK, ssrc=1, media_ssrc=2, lost=list(lost))
    await RTCRtpSender._handle_rtcp_packet(stand_in, nack(2, 3))
    res.check("a first NACK is answered with the packets alone", sent == [2, 3] and not events, (sent, events))
    await RTCRtpSender._handle_rtcp_packet(stand_in, nack(2))
    res.check("a second NACK for a packet names its frame lost, and retransmits again",
              sent == [2, 3, 2] and events == [("lost_frame", 10)], (sent, events))
    await RTCRtpSender._handle_rtcp_packet(stand_in, nack(1, 3))
    res.check("one NACK names each lost frame once, and only frames NACKed twice",
              events == [("lost_frame", 10), ("lost_frame", 11)] and sent == [2, 3, 2, 1, 3], events)
    await RTCRtpSender._handle_rtcp_packet(stand_in, nack(99, 3))
    res.check("a NACK past the history asks for a keyframe and still repairs the rest",
              events[-1] == "pli" and sent == [2, 3, 2, 1, 3, 3], (events, sent))

asyncio.run(nacks())

# --- the engine routes a lost frame to the peer's own display ---------------------
routed = []
app = SimpleNamespace(peer_connections={"peer-2": {"display_id": "display2"}},
                      invalidate_reference=lambda display, frame: routed.append((display, frame)))
RTCApp.on_lost_frame(app, "peer-2", 77)
RTCApp.on_lost_frame(app, "unknown-peer", 78)
res.check("a peer's lost frame reaches its own display's encoder, an unknown peer's the primary",
          routed == [("display2", 77), ("primary", 78)], routed)

# --- the websockets relay is unchanged by any of this ----------------------------
def chunk(frame_id: int, key: bool = False, size: int = 100) -> dict:
    head = bytes([0x04, 0x11 if key else 0x10]) + frame_id.to_bytes(2, "big") + bytes(2) \
        + (1280).to_bytes(2, "big") + (720).to_bytes(2, "big") + frame_id.to_bytes(2, "big")
    return {"data": memoryview(head + bytes(size - 12)), "owner": None, "frame_id": frame_id}


async def relay_skips_ahead() -> None:
    """A relay holds an asyncio.Event, which needs a running loop to build."""
    relay = _VideoRelay(SimpleNamespace(), "primary", SimpleNamespace(), budget=250)
    res.check("a fresh relay waits for a keyframe", relay.offer(chunk(0)) and not relay.backlog)
    res.check("the keyframe and the frames behind it queue",
              not relay.offer(chunk(1, key=True)) and not relay.offer(chunk(2))
              and len(relay.backlog) == 2)
    relay.offer(chunk(3))
    res.check("a client past its backlog budget skips ahead to the next keyframe",
              not relay.backlog and not relay.live_rows)
    res.check("and its frames wait for that keyframe",
              not relay.offer(chunk(4)) and not relay.backlog)


asyncio.run(relay_skips_ahead())

sys.exit(0 if res.summary() else 1)
