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
RESYNC_S. An anchor the encoder flags goes to a peer without room and ends its run,
and a frame is held once transport-cc reported each of its packets, or a retransmission
of one, and the frame it predicts from is held. Driven with stand-ins; no peer.
"""
import asyncio
import os
import sys
from collections import deque
from types import SimpleNamespace

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies.webrtc import rtcrtpsender as sender_mod  # noqa: E402
from selkies.webrtc.pacer import CLASS_VIDEO, RtpPacer  # noqa: E402
from selkies.webrtc.rtcrtpsender import (FRAME_NUM_WRAP, HELD_S, RESYNC_FAR_FRAMES,  # noqa: E402
                                         RESYNC_LAG_S, RESYNC_LOSS_S, RESYNC_REACH_FRAMES,
                                         RESYNC_ROOM_FRAMES, RESYNC_S, RTCRtpSender)
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

    acked: set = set()
    s = SimpleNamespace(
        _RTCRtpSender__in_flight=deque(maxlen=64), _RTCRtpSender__frame_numbers={},
        _RTCRtpSender__unheld=deque(maxlen=256), _RTCRtpSender__held={},
        _RTCRtpSender__rtx_twcc={}, on_frame_held=None, on_frame_out=None, acked=acked, _anchored=False,
        _stall_since=None, _stall_last=0.0, _key_sent_at=None, _unheld_gap=False,
        _RTCRtpSender__frame_number=0, _RTCRtpSender__rtp_history=history,
        _RTCRtpSender__rtp_header_extensions_map=SimpleNamespace(has_dependency_descriptor=lambda: dd),
        _RTCRtpSender__kind="video", _resync_since=None, _resync_first=None, _resync_run=0,
        _resync_told=False, _since_key=0, _resyncs=0, _lost_at=None, _frame_left=None,
        _RTCRtpSender__send_codec=SimpleNamespace(mimeType=mime),
        on_frame_sent=None, on_resync=on_resync, taken=taken,
        selective=(lambda: selective),
        transport=SimpleNamespace(video_backlog=lambda: tuple(backlog), twcc_arrived=acked.__contains__),
        emit=lambda name, *args: events.append((name,) + args),
        _emit_pli_event=lambda: events.append("pli"))
    for name in ("_describe", "_pacer_resync", "_undecodable", "_frame_on_wire", "_forward",
                 "_open_run", "_close_run", "_repair", "_video_backlog", "_numbered", "_far", "_confirm",
                 "_resync_held", "_anchor_room", "_numbers_frames"):
        setattr(s, name, getattr(RTCRtpSender, name).__get__(s))
    return s, events, history, backlog


def send(s, frame_id: int, reference, tag: int, key: bool = False, anchor: bool = False):
    """What the RTP loop does with a frame: leave it out, describe it and hand it over."""
    if not s._forward(frame_id, reference, key, anchor):
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
res.check("and is reported on the next held frame once taken",
          events == [("resync", 2, False)], events)

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

# Anchors: a frame the encoder flags as predicting from one every peer holds.
s, events, _, backlog = sender_with(selective=True)
send(s, 1, None, 10, key=True)
send(s, 2, 1, 20)
backlog[:2] = [RESYNC_ROOM_FRAMES, 0.0]
res.check("an anchor goes to a peer without room", send(s, 3, 2, 30, anchor=True) is not None
          and s._resync_since is None, s._resync_since)
out = [send(s, n, n - 1, 10 * n) for n in range(4, 4 + RESYNC_REACH_FRAMES - 1)]
res.check("a peer without room is left the frames after it",
          out == [None] * len(out) and s._resync_first == 4 and events == [], (s._resync_first, events))
res.check("the next anchor ends the run", send(s, 20, 3, 200, anchor=True) is not None
          and s._resync_since is None, s._resync_since)
send(s, 21, 20, 210)
backlog[:2] = [RESYNC_FAR_FRAMES, 0.0]
res.check("a peer too far behind is not sent one, and its run is told from it",
          send(s, 30, 3, 300, anchor=True) is None and s._resync_first == 30, s._resync_first)
backlog[:2] = [0, 0.0]
send(s, 31, 30, 310)
res.check("once it has room", events == [("resync", 30, True)], events)
out = []
s._frame_on_wire(0, False, None, 0, 0)
s.on_frame_out = lambda frame_id, key: out.append((frame_id, key))
s._frame_on_wire(0, False, None, 0, 0, 20)
res.check("a frame whose last packet leaves is told as sent", out == [(20, False)], out)
s._RTCRtpSender__frame_numbers.clear()
for n in range(100, 100 + 70):
    s._describe(n, None if n == 100 else n - 1, n == 100)
res.check("a frame is numbered for longer than the 64 recent ones, as a pinned anchor is",
          s._describe(200, 100, False) is not None)

# The frame after an H.264 key frame is left out as any other, and an anchored run waits
# RESYNC_ANCHORED_S rather than RESYNC_S for one.
s, events, _, backlog = sender_with(selective=True)
send(s, 1, None, 10, key=True)
backlog[:2] = [RESYNC_FAR_FRAMES, 0.0]
res.check("the frame after a key frame is left out where the peer is far behind",
          send(s, 2, 1, 20) is None and s._resync_since is not None, s._resync_since)
s, events, _, backlog = sender_with(selective=True)
send(s, 1, None, 10, key=True)
send(s, 2, 1, 20)
backlog[:2] = [RESYNC_ROOM_FRAMES, 0.0]
send(s, 3, 2, 30, anchor=True)
send(s, 4, 3, 40)
backlog[:2] = [RESYNC_FAR_FRAMES, 0.0]
send(s, 5, 4, 50)
clock.t += RESYNC_S + 0.5
send(s, 6, 5, 60)
res.check("an anchored run is not answered with a key frame after RESYNC_S", "pli" not in events, events)
clock.t += sender_mod.RESYNC_ANCHORED_S
send(s, 7, 6, 70)
res.check("but is past RESYNC_ANCHORED_S", "pli" in events, events)

# A peer that lost frames past repair is resynced from the newest frame it holds, once per
# RESYNC_ANCHORED_S, where anchors run and it does not own its display.
s, events, _, backlog = sender_with(selective=True)
for n in range(1, 6):
    send(s, n, None if n == 1 else n - 1, 10 * n, key=n == 1, anchor=n == 3)
res.check("a peer holding no frame, with no key frame on its way, is left one", s._resync_held() is False)
s._stall_since = None
s._RTCRtpSender__held.update(dict.fromkeys([1, 2, 3]))
res.check("one holding frame 3 is resynced from it", s._resync_held() is True
          and events == [("resync", 4, True)] and 4 not in s._RTCRtpSender__frame_numbers, events)
res.check("the frames after it are held back", send(s, 6, 5, 60) is None)
res.check("a request while that run is open is answered by it", s._resync_held() is True
          and events == [("resync", 4, True)], events)
clock.t += sender_mod.RESYNC_ANCHORED_S / 2
s._resync_held()
clock.t += sender_mod.RESYNC_ANCHORED_S / 2 + 0.1
res.check("a stall of requests past RESYNC_ANCHORED_S is left a key frame", s._resync_held() is False)
o, _, _, _ = sender_with()
send(o, 1, None, 10, key=True)
send(o, 2, 1, 20, anchor=True)
o._RTCRtpSender__held.update(dict.fromkeys([1]))
res.check("and the display's owner always is", o._resync_held() is False)

# Held: transport-cc reported every packet of the frame, and its reference is held.
s, events, _, _ = sender_with()
held = []
s.on_frame_held = lambda frame_id, key: held.append((frame_id, key))
unheld = s._RTCRtpSender__unheld
unheld.append((1, None, True, clock.t, [(100, 1), (101, 2)]))
unheld.append((2, 1, False, clock.t, [(102, 3)]))
unheld.append((3, 2, False, clock.t, [(103, 4), (104, 5)]))
s.acked.update({100, 101, 103, 104})
s._confirm()
res.check("a frame whose packets were all reported received is held", held == [(1, True)], held)
res.check("one predicting from a frame not yet held waits for it", [e[0] for e in unheld] == [2, 3],
          list(unheld))
s._RTCRtpSender__rtx_twcc[3] = 110
s.acked.add(110)
s._confirm()
res.check("a retransmission received holds the packet it repairs, and the frame after it",
          held == [(1, True), (2, False), (3, False)] and not unheld, held)
unheld.append((4, 3, False, clock.t, [(120, 6)]))
clock.t += HELD_S + 0.01
s.acked.add(120)
s._confirm()
res.check(f"a frame not held within {HELD_S:.0f} s never is", held[-1] == (3, False) and not unheld, held)

# A peer without the dependency descriptor (Firefox answers so) finds an H.264 frame's
# reference by sequence number: its frames are numbered and left out alike; VP8's and VP9's
# name pictures, and are not.
res.check("an H.264 peer without the descriptor has its frames numbered and left out",
          sender_with(dd=False)[0]._numbers_frames() and sender_with(mime="video/VP8")[0]._numbers_frames()
          and not sender_with(dd=False, mime="video/VP8")[0]._numbers_frames())
s, events, _, backlog = sender_with(dd=False, selective=True)
send(s, 1, None, 10, key=True)
send(s, 2, 1, 20)
backlog[:2] = [RESYNC_ROOM_FRAMES, 0.0]
out = [send(s, n, n - 1, 10 * n) for n in range(3, 3 + RESYNC_REACH_FRAMES)]
res.check("it is left the frames it has no room for, and the encoder told at the reach",
          out == [None] * RESYNC_REACH_FRAMES and events == [("resync", 3, True)], (out, events))
res.check("the frame predicting past the run goes out", send(s, 30, 2, 300) == (2, 1))
s._anchored = True
s._RTCRtpSender__held.update(dict.fromkeys([1, 2]))
res.check("a loss past repair is left a key frame: its receiver decodes nothing past the hole",
          s._resync_held() is False)
s, events, _, _ = sender_with(dd=False)
held = []
s.on_frame_held = lambda frame_id, key: held.append(frame_id)
unheld = s._RTCRtpSender__unheld
unheld.append((1, None, True, clock.t, [(100, 1)]))
unheld.append((2, 1, False, clock.t, [(101, 2)]))
unheld.append((3, 1, False, clock.t, [(102, 3)]))
s.acked.update({100, 102})
s._confirm()
res.check("it holds a frame only once it holds every frame sent before it", held == [1], held)
unheld.append((4, 1, False, clock.t + HELD_S, [(103, 4)]))
clock.t += HELD_S + 0.01
s.acked.add(103)
s._confirm()
res.check("and none past one it never held, until a key frame",
          held == [1] and [e[0] for e in unheld] == [4] and s._unheld_gap, (held, list(unheld)))

# A frame predicting from one held long ago, a pinned anchor or the key frame, is held too.
s, _, _, _ = sender_with()
held = []
s.on_frame_held = lambda frame_id, key: held.append(frame_id)
unheld = s._RTCRtpSender__unheld
unheld.extend([(1, None, True, clock.t, [(1000, 1)])]
              + [(n, 1, False, clock.t, [(1000 + n, n)]) for n in range(2, 200)])
s.acked.update(range(1000, 1200))
s._confirm()
res.check("frames predicting from the key frame are held however many were held since",
          held == list(range(1, 200)), held[-3:])

sys.exit(0 if res.summary() else 1)
