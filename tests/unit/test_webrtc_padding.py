#!/usr/bin/env python3
"""Padding a WebRTC stream for the still-screen path probe (`PathProbe`).

The pacer admits a padding packet only with nothing queued, video flowing and
credit covering it, so a frame never waits behind padding there and none is ever
dropped for it; what it admits leaves on the video's credit. The transport counts
padding apart in its transport-cc window (`padding_bps`), so the congestion loop
weighs what the stream sent itself. The sender pads by resending its last large
packets on its RTX stream, each numbered for transport-cc only once admitted, so a
refused one is never read as lost; it keeps those packets past the retransmission
horizon, and pads nothing without RTX negotiated or a packet to resend.
"""
import asyncio
import os
import sys
import time
from types import SimpleNamespace
from unittest.mock import AsyncMock

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies.webrtc import rtcdtlstransport as dtls  # noqa: E402
from selkies.webrtc.pacer import CLASS_VIDEO, RtpPacer  # noqa: E402
from selkies.webrtc.rtcrtpsender import PADDING_SOURCES, RTCRtpSender  # noqa: E402
from selkies.webrtc.rtp import RtpPacket, pack_twcc_fci  # noqa: E402

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402


async def pacer_checks(res: H.Results) -> None:
    sent: list = []

    async def send(data: bytes) -> None:
        sent.append(len(data))

    pacer = RtpPacer(8_000_000, send)
    try:
        pacer.credit, pacer._last = pacer._debt_cap, time.monotonic()
        res.check("an idle pacer with credit admits padding", pacer.padding_room(1200))
        await pacer.send_padding(b"\x80" * 1200)
        res.check("which leaves on the video's credit, counted apart",
                  sent[-1] == 1200 and pacer.stats["padding_bytes"] == 1200, (sent, pacer.stats["padding_bytes"]))
        pacer.credit, pacer._last = 0.0, time.monotonic()
        res.check("without credit it admits none", not pacer.padding_room(1200))
        await pacer.send(b"\x80" * 1200, CLASS_VIDEO)
        pacer.credit, pacer._last = pacer._debt_cap, time.monotonic()
        res.check("nor with a frame queued, whatever its credit", pacer._bytes_queued and not pacer.padding_room(100))
        start = time.monotonic()
        while pacer._bytes_queued and time.monotonic() - start < 2.0:
            await asyncio.sleep(0.001)
        pacer.credit, pacer._last = pacer._debt_cap, time.monotonic()
        pacer._gop_dead = True
        res.check("nor while its GOP is abandoned", not pacer.padding_room(100))
        pacer._gop_dead = False
        res.check("and admits it again once video flows", pacer.padding_room(100))
    finally:
        await pacer.close()


def window_checks(res: H.Results) -> None:
    tr = dtls.RTCDtlsTransport(SimpleNamespace(), [SimpleNamespace()])
    first = tr._twcc_next(1000)
    tr._twcc_next(1000, padding=True)
    tr._twcc_next(1000, padding=True)
    tr._twcc_window["opened"] = time.monotonic() - 1.0
    tr._twcc_process_feedback(pack_twcc_fci(first, [1000, 1001, 1002], 0))
    value = tr.take_twcc_window()
    res.check("the window counts the padding it sent apart from all it sent",
              value["padding_bps"] > 0 and abs(value["padding_bps"] * 3 - value["sent_bps"] * 2) <= 3,
              (value["padding_bps"], value["sent_bps"]))
    room = []
    tr._pacer = SimpleNamespace(padding_room=lambda n: room.append(n) or True)
    res.check("a transport asks its pacer with SRTP's tag counted", tr.padding_room(1200)
              and room == [1200 + dtls.SRTP_PADDING_OVERHEAD], room)
    tr._pacer = None
    res.check("and admits none without a pacer", not tr.padding_room(1200))


async def sender_checks(res: H.Results) -> None:
    allow = {"n": 3}
    numbered, wire = [], []

    def padding_room(size: int) -> bool:
        if allow["n"] <= 0:
            return False
        allow["n"] -= 1
        return True

    def twcc_next(size: int, padding: bool = False) -> int:
        numbered.append((size, padding))
        return 500 + len(numbered)

    async def send_padding_rtp(data: bytes) -> None:
        wire.append(data)

    transport = SimpleNamespace(state="connected", padding_room=padding_room, _twcc_next=twcc_next,
                                _send_padding_rtp=send_padding_rtp, _send_rtp=AsyncMock(return_value=True))
    sender = RTCRtpSender("video", transport)
    res.check("nothing pads before a packet was sent", await sender.send_padding(100_000) == 0)
    source = sender._RTCRtpSender__padding_source
    for seq in range(PADDING_SOURCES + 2):
        source.append(RtpPacket(payload_type=96, sequence_number=1000 + seq, ssrc=sender._ssrc,
                                payload=bytes([seq]) * 1100))
    res.check("it keeps only the newest packets to pad with",
              len(source) == PADDING_SOURCES and source[0].sequence_number == 1002, len(source))
    res.check("nothing pads without RTX negotiated", await sender.send_padding(100_000) == 0 and not numbered)
    sender._RTCRtpSender__rtx_payload_type = 97
    got = await sender.send_padding(100_000)
    packets = [RtpPacket.parse(data) for data in wire]
    res.check("with RTX it resends its packets on the RTX stream while the pacer admits them",
              len(packets) == 3 and all(p.payload_type == 97 and p.ssrc == sender._rtx_ssrc for p in packets)
              and got == sum(len(d) for d in wire), (len(packets), got))
    res.check("each carrying its original sequence number first",
              [int.from_bytes(p.payload[:2], "big") for p in packets] == [1002, 1003, 1004],
              [int.from_bytes(p.payload[:2], "big") for p in packets])
    res.check("numbered only once admitted, each as padding",
              len(numbered) == 3 and all(padding for _, padding in numbered), numbered)
    allow["n"] = 100
    got = await sender.send_padding(2500)
    res.check("and stops at its budget", 2500 <= got < 2500 + 1300, got)


async def main_async(res: H.Results) -> None:
    await pacer_checks(res)
    window_checks(res)
    await sender_checks(res)


def main() -> int:
    res = H.Results("webrtc-padding")
    asyncio.run(main_async(res))
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
