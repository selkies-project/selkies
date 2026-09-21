#!/usr/bin/env python3
"""A GOP the pacer abandons is gone from the queue, the wire and the repairs.

A video packet the queue budget cannot hold abandons its GOP: every queued
packet of it is purged rather than trimmed to fit, since nothing behind the
keyframe the pacer asks for can use them and they would only delay it, and
the packet itself is refused. The stale deadline and a failed send abandon
the same way. The sender learns of an abandonment from the refusal and stops
repairing what it sent up to then: a NACK batch ends at the reset, and a late
NACK for an abandoned packet is ignored rather than putting it back on the
wire behind the keyframe. Driven with stand-ins; no peer."""
import asyncio
import os
import sys
import time
from types import SimpleNamespace
from typing import Optional

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies.webrtc import pacer as pacer_mod  # noqa: E402
from selkies.webrtc.pacer import CLASS_DC, CLASS_VIDEO, RtpPacer  # noqa: E402
from selkies.webrtc.rtcrtpsender import RTCRtpSender  # noqa: E402
from selkies.webrtc.rtp import RTCP_RTPFB_NACK, HeaderExtensionsMap, RtcpRtpfbPacket, RtpHistory, RtpPacket  # noqa: E402

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402

res = H.Results("webrtc-gop-reset")


def held_pacer(sent: list, keyreqs: list, send_now=None, dropped: Optional[list] = None) -> RtpPacer:
    """A pacer whose drain never runs, so what it holds is what send() decided."""
    async def record(data: bytes) -> None:
        sent.append(data)

    pacer = RtpPacer(8_000_000, send_now or record, request_keyframe=lambda: keyreqs.append(True),
                     on_dropped=dropped.append if dropped is not None else None)
    pacer.credit = 0
    pacer._kick = lambda: None
    pacer._accrue = lambda: None
    return pacer


async def queue_abandonment() -> None:
    sent, keyreqs, dropped = [], [], []
    pacer = held_pacer(sent, keyreqs, dropped=dropped)
    try:
        count = pacer._video_cap_bytes() // 1000 + 1
        accepted = [await pacer.send(bytes(1000), CLASS_VIDEO, tag=i) for i in range(count)]
        res.check("every packet the abandonment dropped is reported by its tag, queued ones and the refused one",
                  sorted(dropped) == list(range(count)), (len(dropped), count))
        res.check("the packets the budget holds queue, the one it cannot hold is refused",
                  accepted[:-1] == [True] * (count - 1) and accepted[-1] is False, accepted.count(True))
        res.check("the overflow abandons the GOP: nothing of it stays queued",
                  len(pacer._queues[CLASS_VIDEO]) == 0 and pacer._video_bytes == 0 and pacer._bytes_queued == 0
                  and pacer._gop_dead and keyreqs == [True] and pacer.stats["video_dropped"] == count,
                  pacer.snapshot())
        res.check("video is refused until a keyframe, data is not",
                  await pacer.send(bytes(100), CLASS_VIDEO) is False
                  and await pacer.send(bytes(100), CLASS_DC) is True
                  and len(pacer._queues[CLASS_DC]) == 1, pacer.snapshot())
        pacer.note_keyframe(20_000, natural=False)
        res.check("the keyframe resurrects video into an empty queue",
                  not pacer._gop_dead and await pacer.send(bytes(1000), CLASS_VIDEO) is True
                  and len(pacer._queues[CLASS_VIDEO]) == 1 and pacer.stats["idr_resurrects"] == 1,
                  pacer.snapshot())
        pacer._reset_gop("again")
        res.check("a reset right after the keyframe landed asks for another, inside the throttle",
                  keyreqs == [True, True], keyreqs)
        pacer._gop_dead = False
        pacer._reset_gop("and again")
        res.check("a reset while that one is still outstanding does not",
                  keyreqs == [True, True], keyreqs)
        pacer.note_keyframe(20_000, natural=False)
        await pacer.send(bytes(1000), CLASS_VIDEO)

        pacer_mod.VIDEO_STALE_S = 0.05
        pacer._video_ts[0] = time.monotonic() - 1.0
        stale = await pacer.send(bytes(1000), CLASS_VIDEO)
        res.check("a stale backlog is abandoned whole, and the packet that found it stale is refused",
                  stale is False and len(pacer._queues[CLASS_VIDEO]) == 0 and pacer._video_bytes == 0
                  and pacer._gop_dead and pacer.stats["stale_resets"] == 1, pacer.snapshot())
    finally:
        pacer_mod.VIDEO_STALE_S = 0.0
        await pacer.close()

    async def failing(data: bytes) -> None:
        raise OSError("wire gone")

    sent, keyreqs = [], []
    pacer = held_pacer(sent, keyreqs, failing)
    try:
        for _ in range(3):
            await pacer.send(bytes(1000), CLASS_VIDEO)
        pacer.credit = 1e9
        await pacer._drain()
        res.check("a send that fails abandons the GOP and empties the queue",
                  pacer._gop_dead and len(pacer._queues[CLASS_VIDEO]) == 0 and pacer._bytes_queued == 0
                  and keyreqs == [True], pacer.snapshot())
    finally:
        await pacer.close()


asyncio.run(queue_abandonment())


async def sender_repairs() -> None:
    history = RtpHistory()
    for seq in range(1, 6):
        history.add(RtpPacket(payload_type=96, sequence_number=seq, payload=b"x"), 0.0, seq)
    refusals: list = []
    wire: list = []

    async def send_rtp(data: bytes, rtc_class=None, twcc_seq=None) -> bool:
        wire.append(twcc_seq)
        return not (refusals and refusals.pop(0))

    events: list = []
    sender = SimpleNamespace(
        _RTCRtpSender__rtp_history=history, _RTCRtpSender__abandoned=None, _RTCRtpSender__last_sequence=5,
        _RTCRtpSender__kind="video", _RTCRtpSender__rtx_payload_type=None,
        _RTCRtpSender__rtp_header_extensions_map=HeaderExtensionsMap(),
        _RTCRtpSender__log_debug=lambda *a: None,
        transport=SimpleNamespace(_send_rtp=send_rtp, _twcc_next=lambda n: 7),
        _emit_pli_event=lambda: events.append("pli"), emit=lambda name, *args: events.append((name,) + args))
    sender._retransmit = lambda packet: RTCRtpSender._retransmit(sender, packet)
    sender._send = lambda data, twcc_seq=None: RTCRtpSender._send(sender, data, twcc_seq)
    nack = lambda *lost: RtcpRtpfbPacket(fmt=RTCP_RTPFB_NACK, ssrc=1, media_ssrc=2, lost=list(lost))

    await RTCRtpSender._handle_rtcp_packet(sender, nack(1, 2))
    res.check("repairs the pacer takes leave nothing abandoned",
              len(wire) == 2 and sender._RTCRtpSender__abandoned is None, (len(wire), sender._RTCRtpSender__abandoned))
    refusals[:] = [False, True, False]
    wire.clear()
    await RTCRtpSender._handle_rtcp_packet(sender, nack(1, 2, 3, 4, 5))
    res.check("a batch stops at the repair the pacer refused, and everything sent so far is abandoned",
              len(wire) == 2 and sender._RTCRtpSender__abandoned == 5 and events == [("lost_frame", 1)],
              (len(wire), events))
    res.check("a repair goes out under its own transport sequence", wire == [7, 7], wire)
    wire.clear()
    events.clear()
    await RTCRtpSender._handle_rtcp_packet(sender, nack(3, 4, 5))
    res.check("a late NACK for an abandoned packet is ignored: no repair, no frame named, no keyframe asked",
              wire == [] and events == [], (wire, events))
    for seq in (6, 7):
        history.add(RtpPacket(payload_type=96, sequence_number=seq, payload=b"x"), 0.0, seq)
    sender._RTCRtpSender__last_sequence = 7
    await RTCRtpSender._handle_rtcp_packet(sender, nack(5, 7))
    res.check("a packet sent after the abandonment is repaired as before",
              len(wire) == 1 and events == [], (len(wire), events))
    sender._RTCRtpSender__abandoned = 65535
    wire.clear()
    await RTCRtpSender._handle_rtcp_packet(sender, nack(7))
    res.check("the boundary compares across the sequence wrap", len(wire) == 1, len(wire))
    events.clear()
    await RTCRtpSender._handle_rtcp_packet(sender, nack(99))
    res.check("a NACK past the history still asks for a keyframe", events == ["pli"], events)


asyncio.run(sender_repairs())

sys.exit(0 if res.summary() else 1)
