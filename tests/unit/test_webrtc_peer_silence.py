#!/usr/bin/env python3
"""A WebRTC sender stops sending to a peer that has gone silent, and resumes with a key frame.

A browser sends feedback for what it receives and checks consent on its path every
few seconds, so a peer heard from by nothing at all for PEER_SILENCE_S is gone
(asleep, off the network), while ICE consent would keep the stream going to it for
half a minute.
Driven with a stand-in DTLS transport whose peer the test silences and brings back.
"""
import asyncio
import os
import sys
import time
from fractions import Fraction

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402

from selkies.webrtc import rtcrtpsender  # noqa: E402
from selkies.webrtc.codecs.base import EncodedPacket  # noqa: E402
from selkies.webrtc.mediastreams import MediaStreamTrack  # noqa: E402
from selkies.webrtc.rtcrtpparameters import RTCRtcpParameters, RTCRtpCodecParameters, RTCRtpSendParameters  # noqa: E402
from selkies.webrtc.rtcrtpsender import RTCRtpSender  # noqa: E402

res = H.Results("webrtc-peer-silence")


class Transport:
    """The DTLS transport surface the sender drives; `heard` is when the peer last spoke."""

    state = "connected"
    _stats_id = "transport"

    def __init__(self) -> None:
        self.rtp = []
        self.seq = 0
        self.heard = time.monotonic()

    def _register_rtp_sender(self, sender, parameters) -> None:
        pass

    def _unregister_rtp_sender(self, sender) -> None:
        pass

    def _peer_heard_at(self) -> float:
        return self.heard

    def _twcc_next(self, size: int) -> int:
        self.seq = (self.seq + 1) & 0xFFFF
        return self.seq

    def note_video_keyframe(self, size: int, natural: bool = True) -> None:
        pass

    def frame_end(self, twcc_seq: int, sink, *args) -> None:
        sink(*args)

    async def _send_rtp(self, data: bytes, rtc_class=None, twcc_seq=None) -> bool:
        if not (len(data) > 1 and 192 <= data[1] <= 223):
            self.rtp.append(time.monotonic())
        return True


class Track(MediaStreamTrack):
    kind = "video"

    def __init__(self) -> None:
        super().__init__()
        self.queue: asyncio.Queue = asyncio.Queue()

    async def recv(self):
        return await self.queue.get()


async def run() -> dict:
    rtcrtpsender.PEER_SILENCE_S = 0.5
    track, transport = Track(), Transport()
    sender = RTCRtpSender(track, transport)
    plis = []
    sender.on("pli", lambda: plis.append(time.monotonic()))
    codec = RTCRtpCodecParameters(mimeType="video/H264", clockRate=90000, payloadType=102,
                                  parameters={"packetization-mode": "1", "profile-level-id": "42e01f"})
    await sender.send(RTCRtpSendParameters(codecs=[codec], muxId="0", rtcp=RTCRtcpParameters(cname="silence")))
    phases = {}

    async def frames(seconds: float, talking: bool) -> int:
        before = len(transport.rtp)
        end = time.monotonic() + seconds
        i = 0
        while time.monotonic() < end:
            if talking:
                transport.heard = time.monotonic()
            now = time.monotonic_ns()
            track.queue.put_nowait(EncodedPacket(b"\x00\x00\x00\x01\x65" + b"\x88" * 40, i * 3000,
                                                 Fraction(1, 90000), i == 0, (now, now, now), None))
            i += 1
            await asyncio.sleep(1 / 30)
        return len(transport.rtp) - before

    phases["talking"] = await frames(1.0, True)
    phases["silent_first"] = await frames(0.3, False)
    await frames(0.3, False)
    phases["silent_after"] = await frames(1.0, False)
    phases["plis_while_silent"] = len(plis)
    phases["back"] = await frames(0.5, True)
    phases["plis"] = len(plis)
    await sender.stop()
    rtcrtpsender.PEER_SILENCE_S = 10.0
    return phases


p = asyncio.run(run())
res.check("a peer heard from is sent every frame", p["talking"] >= 25, p)
res.check("a peer silent for less than the bound is still sent frames", p["silent_first"] >= 7, p)
res.check("a peer silent past the bound is sent nothing", p["silent_after"] == 0, p)
res.check("and no key frame is asked for while it stays silent", p["plis_while_silent"] == 0, p)
res.check("a peer heard again is sent frames again", p["back"] >= 10, p)
res.check("starting from the key frame asked for once it spoke", p["plis"] == 1, p)

sys.exit(0 if res.summary() else 1)
