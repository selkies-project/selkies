#!/usr/bin/env python3
"""A receiver's wait on a still screen costs no key frame.
A libwebrtc receiver asks for a key frame whenever no frame decodes within 375 ms
(three times the offer's rtx-time) of a packet in the last five seconds, and
repeats that every 375 ms; a still screen sends nothing, so every still was
answered with a key frame and the cleanup after it, which ended in another
still. A request is that wait when the newest frame is on the wire, the peer
acknowledged its last packet, it left STILL_REQUEST_S past the round trip
before, the peer lost nothing since past repair, and the request before came
no sooner than that: one that comes sooner is a failed decode, which asks at
the rtx-time's own pace. Every other request is answered with a key frame: a
frame still in the pacer, a last packet the peer never acknowledged (lost with
nothing after it to show the gap), a frame lost past repair until a frame
coded after the loss is on the wire, a frame this peer was not sent (a
paused tab, a peer gone quiet), or a request close behind the frame. A FIR
asks what a PLI does. Driven with stand-ins; no peer.
"""
import asyncio
import os
import sys
import time
from collections import deque
from types import SimpleNamespace
from unittest.mock import AsyncMock

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies.webrtc.mediastreams import MediaStreamError  # noqa: E402
from selkies.webrtc.rtcdtlstransport import RTCDtlsTransport  # noqa: E402
from selkies.webrtc.rtcrtpsender import STILL_REQUEST_S, RTCRtpSender  # noqa: E402
from selkies.webrtc.rtp import RTCP_PSFB_FIR, RTCP_PSFB_PLI, RtcpPsfbPacket  # noqa: E402

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402


def sender(asks: list, sent: list) -> SimpleNamespace:
    """A sender whose newest frame, transport-wide number 40, left 0.4 s ago and was
    acknowledged, with no request before."""
    s = SimpleNamespace(_RTCRtpSender__rtt=0.02, _frame_handed=40, _RTCRtpSender__in_flight=deque(),
                        _frame_left=(40, time.monotonic() - 0.4), _key_asked_at=0.0, _lost_at=None,
                        transport=SimpleNamespace(_twcc_acked=lambda seq: seq != 41),
                        _send_keyframe=lambda: None, _emit_pli_event=lambda: asks.append("key"),
                        on_frame_sent=lambda *args: sent.append(args))
    s._still_request = lambda: RTCRtpSender._still_request(s)
    return s


def left(s: SimpleNamespace, seq: int, keyframe: bool = False, encoded: float = 0.0) -> None:
    """Frame `seq`, its encode begun at `encoded` on the monotonic clock, leaves now."""
    s._frame_handed = seq
    RTCRtpSender._frame_on_wire(s, seq, keyframe, (1, int(encoded * 1e9)), 5, 100)


async def main(res: H.Results) -> None:
    pli = RtcpPsfbPacket(fmt=RTCP_PSFB_PLI, ssrc=1, media_ssrc=2)
    fir = RtcpPsfbPacket(fmt=RTCP_PSFB_FIR, ssrc=1, media_ssrc=2)
    asks, sent = [], []
    s = sender(asks, sent)

    async def ask(packet=pli, after: float = 0.4) -> bool:
        """Whether a request `after` s past the last request is answered with a key frame."""
        s._key_asked_at = time.monotonic() - after
        n = len(asks)
        await RTCRtpSender._handle_rtcp_packet(s, packet)
        return len(asks) > n

    res.check("a request 400 ms after an acknowledged frame on a still screen asks for nothing",
              not await ask(), asks)
    res.check("nor its repeats 375 ms apart", not await ask(after=0.375) and not await ask(after=0.375), asks)
    res.check("a FIR is the same wait", not await ask(fir), asks)
    res.check("a request sooner than STILL_REQUEST_S after the last is a failed decode, and answered",
              await ask(after=STILL_REQUEST_S - 0.13), asks)

    s._frame_left = (40, time.monotonic() - STILL_REQUEST_S + 0.05)
    res.check("a request close behind the frame is answered: that frame's decode failed", await ask())
    s._RTCRtpSender__rtt = 0.3
    s._frame_left = (40, time.monotonic() - 0.4)
    res.check("the round trip is part of the wait", await ask())
    s._frame_left = (40, time.monotonic() - 0.6)
    res.check("and past it the request is the wait again", not await ask())
    s._RTCRtpSender__rtt = None
    res.check("an unmeasured round trip counts as none", not await ask())

    s._frame_handed = 42
    res.check("a request while the newest frame is still in the pacer is answered", await ask())
    s._frame_handed, s._frame_left = 41, (41, time.monotonic() - 0.4)
    res.check("as is one whose last packet the peer never acknowledged", await ask())
    s._frame_handed, s._frame_left = 40, (40, time.monotonic() - 0.4)
    s._lost_at = time.monotonic() - 1.0
    res.check("and one after the peer lost a frame past repair", await ask())

    left(s, 43, encoded=s._lost_at - 0.1)
    s._frame_left = (43, time.monotonic() - 0.4)
    res.check("a frame coded before the loss leaves the peer short of it",
              s._lost_at is not None and await ask(), s._lost_at)
    left(s, 44, encoded=s._lost_at + 0.01)
    s._frame_left = (44, time.monotonic() - 0.4)
    res.check("one coded after it predicts past the loss, and the wait asks for nothing again",
              s._lost_at is None and not await ask(), s._lost_at)
    s._lost_at = time.monotonic()
    left(s, 45, keyframe=True)
    res.check("a key frame on the wire leaves nothing lost either", s._lost_at is None)
    s._lost_at = time.monotonic()
    left(s, 47)
    res.check("a frame with no encode instant never clears a loss", s._lost_at is not None)
    res.check("each frame on the wire reaches the owner's sink with its capture instant and size",
              sent and all(args == (5, 100) for args in sent), sent)
    res.check("a frame on the wire is the newest that left",
              s._frame_left[0] == 47 and time.monotonic() - s._frame_left[1] < 1.0, s._frame_left)

    real = RTCRtpSender("video", SimpleNamespace(state="connected"))
    real.replaceTrack(SimpleNamespace(id="test", stop=lambda: None))
    real._frame_left = (40, time.monotonic() - 0.4)
    real._next_encoded_frame = AsyncMock(side_effect=[None, MediaStreamError()])
    await asyncio.wait_for(real._run_rtp(), 2)
    res.check("a frame this peer is not sent (paused, gone) leaves no frame on the wire to wait on",
              real._frame_left is None, real._frame_left)

    history = SimpleNamespace(_twcc_history={7: (100, 0.0)})
    res.check("a packet awaiting feedback is unacknowledged, one the feedback matched acknowledged",
              not RTCDtlsTransport._twcc_acked(history, 7) and RTCDtlsTransport._twcc_acked(history, 8))


res = H.Results("still-request")
asyncio.run(main(res))
sys.exit(0 if res.summary() else 1)
