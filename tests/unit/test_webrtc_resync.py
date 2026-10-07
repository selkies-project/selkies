#!/usr/bin/env python3
"""A WebRTC peer is resynced without a key frame on the shared stream.

Where the encoder names each frame's reference and the peer reads the dependency
descriptor, a peer that does not own its display is left the frames its pacer has no
room for, before they are numbered, with the frames predicting from them. The encoder
is told the run's first frame once the peer has room, or sooner where waiting would
cost a key frame (at the reach, and ahead of a frame_num wrap, a frame never left out),
and the peer decodes the first frame predicting past the run. A pacer GOP reset hands
its cut to the sender the same way: the pacer brakes, asks for no key frame, and video
goes on flowing. A sender that cannot (no descriptor, nothing it described in flight)
leaves the pacer its key frame, as does a run the encoder never predicts past within
RESYNC_S. Driven with stand-ins; no peer.
"""
import asyncio
import os
import sys
from collections import deque
from types import SimpleNamespace

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies.webrtc import rtcrtpsender as sender_mod  # noqa: E402
from selkies.webrtc.pacer import CLASS_VIDEO, RtpPacer  # noqa: E402
from selkies.webrtc.rtcrtpsender import (FRAME_NUM_WRAP, RESYNC_FAR_FRAMES, RESYNC_LAG_S,  # noqa: E402
                                         RESYNC_LOSS_S, RESYNC_REACH_FRAMES, RESYNC_ROOM_FRAMES, RESYNC_S,
                                         RTCRtpSender)
from selkies.webrtc.rtp import RtpHistory, RtpPacket  # noqa: E402

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402

res = H.Results("webrtc-resync")


def held_pacer(keyreqs: list, resync) -> RtpPacer:
    """A pacer whose drain never runs, so what it holds is what send() decided."""
    async def record(data: bytes) -> None:
        pass

    pacer = RtpPacer(8_000_000, record, request_keyframe=lambda: keyreqs.append(True), resync=resync)
    pacer.credit = 0
    pacer._kick = lambda: None
    pacer._accrue = lambda: None
    return pacer


async def pacer_hands_the_drop() -> None:
    keyreqs, offered = [], []
    pacer = held_pacer(keyreqs, lambda first: offered.append(first) or True)
    try:
        count = pacer._video_cap_bytes() // 1000 + 1
        for i in range(count):
            await pacer.send(bytes(1000), CLASS_VIDEO, tag=i)
        res.check("an overflow offers the sender the oldest packet it dropped", offered == [0], offered)
        res.check("which takes it: the queue is purged, no key frame asked for, video not abandoned",
                  len(pacer._queues[CLASS_VIDEO]) == 0 and keyreqs == [] and not pacer._gop_dead
                  and pacer.stats["resyncs"] == 1, pacer.snapshot())
        res.check("and video flows on", await pacer.send(bytes(1000), CLASS_VIDEO, tag=count) is True,
                  pacer.snapshot())
    finally:
        await pacer.close()

    keyreqs = []
    pacer = held_pacer(keyreqs, lambda first: False)
    try:
        count = pacer._video_cap_bytes() // 1000 + 1
        for i in range(count):
            await pacer.send(bytes(1000), CLASS_VIDEO, tag=i)
        res.check("a sender that cannot resync leaves the pacer its key frame and the abandoned GOP",
                  keyreqs == [True] and pacer._gop_dead, pacer.snapshot())
    finally:
        await pacer.close()


asyncio.run(pacer_hands_the_drop())


class Clock:
    def __init__(self) -> None:
        self.t = 1000.0

    def monotonic(self) -> float:
        return self.t

    def time(self) -> float:
        return self.t


def sender_with(dd: bool = True, selective: bool = False, taken: bool = True, mime: str = "video/H264"):
    """A stand-in sender; `backlog` is what its pacer holds, as (frames, seconds)."""
    events: list = []
    history = RtpHistory()
    for seq in range(1, 5):
        history.add(RtpPacket(payload_type=96, sequence_number=seq, payload=b"x"), 0.0, seq)
    backlog = [0, 0.0, float("inf")]

    def on_resync(frame_id, reach):
        if not reach and not s.taken:
            return False
        events.append(("resync", frame_id, reach))
        return True

    s = SimpleNamespace(
        _RTCRtpSender__in_flight=deque(maxlen=64), _RTCRtpSender__frame_numbers={},
        _RTCRtpSender__frame_number=0, _RTCRtpSender__rtp_history=history,
        _RTCRtpSender__rtp_header_extensions_map=SimpleNamespace(has_dependency_descriptor=lambda: dd),
        _RTCRtpSender__kind="video", _resync_since=None, _resync_first=None, _resync_run=0,
        _resync_told=False, _since_key=0, _resyncs=0, _lost_at=None, _frame_left=None,
        _RTCRtpSender__send_codec=SimpleNamespace(mimeType=mime),
        on_frame_sent=None, on_resync=on_resync, taken=taken,
        selective=(lambda: selective),
        transport=SimpleNamespace(video_backlog=lambda: tuple(backlog)),
        emit=lambda name, *args: events.append((name,) + args),
        _emit_pli_event=lambda: events.append("pli"))
    for name in ("_describe", "_pacer_resync", "_undecodable", "_frame_on_wire", "_forward",
                 "_open_run", "_close_run", "_repair", "_video_backlog", "_numbered"):
        setattr(s, name, getattr(RTCRtpSender, name).__get__(s))
    return s, events, history, backlog


def send(s, frame_id: int, reference, tag: int, key: bool = False):
    """What the RTP loop does with a frame: leave it out, describe it and hand it over."""
    if not s._forward(frame_id, reference, key):
        s._frame_left = None
        return None
    described = s._describe(frame_id, reference, key)
    if described is None:
        s._frame_left = None
        s._undecodable(frame_id, reference)
        return None
    s._RTCRtpSender__in_flight.append((tag, frame_id))
    return described


clock = Clock()
sender_mod.time = clock

# A pacer cut: the frames whose last packet had not left are lost to the peer.
s, events, history, _ = sender_with(selective=True)
send(s, 1, None, 10, key=True)
send(s, 2, 1, 20)
send(s, 3, 2, 30)
send(s, 4, 3, 40)
s._frame_on_wire(20, False, None, 0, 0)
res.check("frames whose last packet left are no longer in flight",
          list(s._RTCRtpSender__in_flight) == [(30, 3), (40, 4)], list(s._RTCRtpSender__in_flight))
taken = s._pacer_resync(25)
res.check("a reset cutting frames 3 and 4 is taken by the sender", taken is True)
res.check("the encoder is told the first frame cut, within its reach", events == [("resync", 3, True)], events)
res.check("the cut frames leave the frames the peer was sent",
          set(s._RTCRtpSender__frame_numbers) == {1, 2}, s._RTCRtpSender__frame_numbers)
res.check("and the repairs of what was sent before it end", all(history.abandoned(q) for q in range(1, 5)))
held = [send(s, 5, 4, 50), send(s, 6, 5, 60)]
res.check("frames predicting from a cut one are held back, the report not repeated, no key frame",
          held == [None, None] and events == [("resync", 3, True)], events)
resumed = send(s, 7, 2, 70)
res.check("the first frame predicting past the cut goes out, and the run ends",
          resumed is not None and s._resync_since is None, resumed)
res.check("its descriptor points back to frame 2's number past the two cut",
          resumed == (4, 3), resumed)

s, events, _, _ = sender_with(selective=True)
send(s, 1, None, 10, key=True)
send(s, 2, 1, 20)
s._pacer_resync(15)
clock.t += RESYNC_S + 0.01
send(s, 3, 2, 30)
res.check("a cut the encoder never predicts past within RESYNC_S is answered with a key frame",
          "pli" in events and s._resync_since is None, events)

s, events, _, _ = sender_with(dd=False, selective=True)
s._RTCRtpSender__in_flight.append((10, 1))
res.check("without the dependency descriptor the sender leaves the pacer its key frame",
          s._pacer_resync(5) is False and events == [], events)
s, events, _, _ = sender_with(selective=True)
res.check("and with nothing it described in flight", s._pacer_resync(5) is False and events == [], events)
s, events, _, _ = sender_with()
send(s, 1, None, 10, key=True)
send(s, 2, 1, 20)
res.check("and for the display's owner, whose receiver closes the gap on a key frame",
          s._pacer_resync(15) is False and events == [], events)

# A peer beside the owner whose pacer has no room is left frames, and resynced.
s, events, _, backlog = sender_with(selective=True)
send(s, 1, None, 10, key=True)
res.check("a frame goes out while the peer's pacer has room", send(s, 2, 1, 20) is not None)
backlog[:2] = [RESYNC_ROOM_FRAMES, 0.0]
out = [send(s, n, n - 1, 10 * n) for n in range(3, 3 + RESYNC_REACH_FRAMES - 1)]
res.check(f"with {RESYNC_ROOM_FRAMES} of its frames in its pacer the next is left out, and those after it",
          out == [None] * (RESYNC_REACH_FRAMES - 1) and s._resync_first == 3, (out, s._resync_first))
res.check("and the encoder is not told while it has no room", events == [], events)
send(s, 3 + RESYNC_REACH_FRAMES - 1, 3 + RESYNC_REACH_FRAMES - 2, 0)
res.check("at the reach it is told, while it still holds the frame to predict from",
          events == [("resync", 3, True)], events)
branch = send(s, 20, 2, 200)
res.check("the frame predicting past the run goes out though the pacer has no room",
          branch == (2, 1) and s._resync_since is None, branch)
backlog[:2] = [0, RESYNC_LAG_S]
res.check(f"a pacer {RESYNC_LAG_S * 1000:.0f} ms behind has no room either",
          send(s, 21, 20, 210) is None and s._resync_first == 21, s._resync_first)
s, events, _, backlog = sender_with(selective=False)
send(s, 1, None, 10, key=True)
backlog[:2] = [RESYNC_FAR_FRAMES, 5.0]
res.check("the owner is sent every frame however full its pacer",
          all(send(s, n, n - 1, 10 * n) is not None for n in range(2, 12)) and events == [], events)

# A frame_num wrap: never left out, and a run open just ahead of it is reported at once.
s, events, _, backlog = sender_with(selective=True)
send(s, 1, None, 10, key=True)
for n in range(2, FRAME_NUM_WRAP - 1):
    send(s, n, n - 1, 10 * n)
backlog[:2] = [RESYNC_ROOM_FRAMES, 0.0]
n = FRAME_NUM_WRAP - 1
send(s, n, n - 1, 10 * n)
res.check(f"a run opened {FRAME_NUM_WRAP - 2} frames past a key frame is reported at once",
          events == [("resync", n, True)], events)
res.check("the frame answering it, and the one where frame_num may wrap, go out without room",
          send(s, n + 1, n - 1, 0) is not None and send(s, n + 2, n + 1, 0) is not None)
s, events, _, backlog = sender_with(selective=True, mime="video/AV1")
send(s, 1, None, 10, key=True)
for n in range(2, FRAME_NUM_WRAP + 1):
    send(s, n, n - 1, 10 * n)
backlog[:2] = [RESYNC_ROOM_FRAMES, 0.0]
res.check("a codec without frame_num is left that frame like any other, and not reported early",
          send(s, FRAME_NUM_WRAP + 1, FRAME_NUM_WRAP, 0) is None and events == [], events)

# A peer far behind is answered only once it has room, as a key-frame request is.
s, events, _, backlog = sender_with(selective=True, taken=False)
send(s, 1, None, 10, key=True)
backlog[:2] = [RESYNC_FAR_FRAMES, 0.0]
for n in range(2, 4 + RESYNC_REACH_FRAMES):
    send(s, n, n - 1, 10 * n)
res.check("a peer far behind is not answered at the reach", events == [], events)
backlog[:2] = [0, 0.0]
n = 4 + RESYNC_REACH_FRAMES
send(s, n, n - 1, 0)
res.check("once it has room, a run past the reach waits while its key-frame requests are spent",
          events == [], events)
s.taken = True
send(s, n + 1, n, 0)
res.check("and is reported on the next held frame once taken", events == [("resync", 2, False)], events)

# A path dropping what overflows its buffer shows no deeper queue than the buffer holds: loss
# while a queue stands is far behind too, and loss on an empty path is not.
s, events, _, backlog = sender_with(selective=True)
send(s, 1, None, 10, key=True)
backlog[:] = [0, 2 * RESYNC_LAG_S, RESYNC_LOSS_S / 2]
for n in range(2, 3 + RESYNC_REACH_FRAMES):
    send(s, n, n - 1, 10 * n)
res.check("a path losing packets behind a standing queue is not answered at the reach", events == [], events)
s, events, _, backlog = sender_with(selective=True)
send(s, 1, None, 10, key=True)
backlog[:] = [0, 0.0, RESYNC_LOSS_S / 2]
res.check("while loss on a path with no queue standing takes no room away", send(s, 2, 1, 20) is not None)

sys.exit(0 if res.summary() else 1)
