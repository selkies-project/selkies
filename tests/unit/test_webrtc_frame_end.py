#!/usr/bin/env python3
"""A frame is on the wire once its last packet leaves the pacer.

The RTP sender tags a frame's last packet with what to call once it is out
(`RtpPacer.frame_end`), which is where a stream's stats read the frame as sent:
the time it queued behind the pace is part of its way to the client. A packet
that goes straight out calls at once, a queued one when the drain sends it, and
one the pacer drops or purges with its GOP never calls, since that frame never
reached the wire. The waiting table stays bounded however many frames are lost.
A transport that paces nothing calls once the packet is written to the socket.
"""
import asyncio
import os
import sys
import time
from types import SimpleNamespace

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies.webrtc.pacer import CLASS_VIDEO, FRAME_ENDS_MAX, RtpPacer  # noqa: E402
from selkies.webrtc.rtcdtlstransport import RTCDtlsTransport, State  # noqa: E402

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402

PACKET = b"\x80" * 1200


async def main(res: H.Results) -> None:
    sent = []

    async def send(data: bytes) -> None:
        sent.append(time.monotonic())

    pacer = RtpPacer(8_000_000, send)
    ended = []
    try:
        pacer.credit = pacer._debt_cap
        pacer.frame_end(7, lambda *args: ended.append((args, len(sent))), 123, 4800)
        for tag in range(4, 8):
            await pacer.send(PACKET, CLASS_VIDEO, tag)
        res.check("a frame sent straight out is reported once its last packet is",
                  ended == [((123, 4800), 4)], ended)

        pacer.credit = 0.0
        pacer._last = time.monotonic()
        pacer.frame_end(11, lambda *args: ended.append((args, len(sent))), 456, 2400)
        for tag in (10, 11):
            await pacer.send(PACKET, CLASS_VIDEO, tag)
        queued = len(sent)
        start = time.monotonic()
        while len(ended) < 2 and time.monotonic() - start < 2.0:
            await asyncio.sleep(0.001)
        res.check("a queued one when the drain sends that packet, after every packet before it",
                  len(ended) == 2 and ended[1][0] == (456, 2400) and ended[1][1] == queued + 2,
                  (ended, queued))

        pacer.frame_end(21, lambda *args: ended.append(args), 789, 1200)
        pacer._drop(21)
        pacer.frame_end(31, lambda *args: ended.append(args), 790, 1200)
        pacer._video_tags.append(31)
        pacer._purge_video()
        res.check("a frame whose last packet is dropped, or purged with its GOP, is never reported",
                  len(ended) == 2 and not pacer.frame_ends, (ended, pacer.frame_ends))

        for tag in range(1000, 1000 + 3 * FRAME_ENDS_MAX):
            pacer.frame_end(tag, ended.append, tag)
        res.check("and the frames still waiting stay bounded, the oldest forgotten first",
                  len(pacer.frame_ends) == FRAME_ENDS_MAX
                  and min(pacer.frame_ends) == 1000 + 2 * FRAME_ENDS_MAX, len(pacer.frame_ends))
    finally:
        await pacer.close()

    written = []

    async def write(data: bytes) -> None:
        written.append(data)

    unpaced = RTCDtlsTransport(SimpleNamespace(), [SimpleNamespace()])
    unpaced._state = State.CONNECTED
    unpaced._tx_srtp = SimpleNamespace(protect=lambda data: data, protect_rtcp=lambda data: data)
    unpaced._transport = SimpleNamespace(_send=write)
    calls = []
    unpaced.frame_end(42, lambda *args: calls.append((args, len(written))), 1234, 2400)
    for seq in (41, 42, 43):
        await unpaced._send_rtp(b"\x80\x60" + bytes(10), CLASS_VIDEO, seq)
    res.check("a transport that paces nothing reports the frame once its last packet is written to the socket",
              calls == [((1234, 2400), 2)] and not unpaced._frame_ends, (calls, unpaced._frame_ends))


res = H.Results("webrtc-frame-end")
asyncio.run(main(res))
sys.exit(0 if res.summary() else 1)
