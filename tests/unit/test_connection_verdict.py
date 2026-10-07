#!/usr/bin/env python3
"""A page is told its connection is poor while too many of its display's frames
do not reach it, and told again once they do.

The verdict (`ConnectionVerdict`) judges windows of a changing screen with
Moonlight's hysteresis: a window missing 30% of its frames, or 15% after one
that missed as many, makes it poor, and one missing 5% or less makes it good
again. A still screen or a caret is not judged, nor is a page's first window.
Over WebSockets a client's relay counts what it drops and what the gate holds
back for the link, never what it holds back from a client that stopped
answering; over WebRTC a peer counts the frames the pacer never sent, the ones
lost past repair, and the ones its display's bridge held back for the link's
rate. Either way the page is told only on a change, and only that page.
"""
import asyncio
import os
import sys
import time
from types import SimpleNamespace

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

from selkies import websockets_mode as w
from selkies.stream_server import ConnectionVerdict
from selkies.webrtc_engine import RTCApp

res = H.Results("connection-verdict")


def verdicts(shares, frames=60):
    """Feed one window per share, `frames` frames each, the missed ones spread
    through it; the verdict each window's close reported, by window."""
    verdict, now, told = ConnectionVerdict(), 0.0, {}
    step = ConnectionVerdict.WINDOW_S / frames
    for window, share in enumerate(shares):
        for i in range(frames):
            missed = round((i + 1) * share) - round(i * share)
            change = verdict.note(1, missed, now)
            if change is not None:
                told[window - 1] = change
            now += step
    # The last window closes on a frame after it.
    change = verdict.note(1, 0, now + ConnectionVerdict.WINDOW_S)
    if change is not None:
        told[len(shares) - 1] = change
    return told


res.check("a page's first window is not judged, however much it missed", verdicts([1.0]) == {},
          verdicts([1.0]))
res.check("a window missing a third of its frames makes the connection poor", verdicts([0, 0.35]) == {1: True},
          verdicts([0, 0.35]))
res.check("one missing a fifth does not", verdicts([0, 0.2, 0]) == {}, verdicts([0, 0.2, 0]))
res.check("two in a row do", verdicts([0, 0.2, 0.2]) == {2: True}, verdicts([0, 0.2, 0.2]))
told = verdicts([0, 0.35, 0.1, 0.1, 0.0])
res.check("the connection stays poor over windows missing a tenth, and turns good on one missing nothing",
          told == {1: True, 4: False}, told)
told = verdicts([0, 1.0, 1.0], frames=ConnectionVerdict.MIN_FRAMES - 1)
res.check("a still screen or a caret, too few frames to judge, is not judged", told == {}, told)


def chunk(frame_id: int, key: bool = False, size: int = 100) -> dict:
    head = bytes([0x04, 0x11 if key else 0x10]) + frame_id.to_bytes(2, "big") + bytes(2) \
        + (1280).to_bytes(2, "big") + (720).to_bytes(2, "big") + frame_id.to_bytes(2, "big")
    return {"data": memoryview(head + bytes(size - 12)), "owner": None, "frame_id": frame_id}


class Socket:
    """A client's socket, as far as the relay's verdict reaches it."""


async def websockets_relay() -> dict:
    """Windows of 50 ms over a relay whose budget holds two chunks, so every
    third chunk drops its backlog and the deltas after it wait for a key frame."""
    told = []

    async def broadcast(clients, message, watched=False, only=None):
        told.append((message, only))
        return set()

    real = w._broadcast_to_clients
    w._broadcast_to_clients = broadcast
    ws, other = Socket(), Socket()
    server = SimpleNamespace(clients={ws, other}, common_frames={})
    server.common_frames_for = lambda did: server.common_frames.setdefault(did, w.CommonFrames(lambda fid: None))
    relay = w._VideoRelay(server, "primary", ws, budget=250)
    out = {}
    try:
        fid = 0

        async def window(frames: int, key_every: int) -> None:
            nonlocal fid
            start = time.monotonic()
            for i in range(frames):
                fid += 1
                relay.offer(chunk(fid, key=i % key_every == 0))
                relay.backlog.clear()
                relay.backlog_bytes = 0
            await asyncio.sleep(max(0.0, ConnectionVerdict.WINDOW_S - (time.monotonic() - start)) + 0.005)

        await window(30, 1)
        await window(30, 1)
        await window(30, 1)
        out["clean"] = list(told)
        for _ in range(2):
            relay.flush_for_gate(False)
            await window(30, 1)
        out["stalled"] = list(told)
        for _ in range(30):
            relay.flush_for_gate()
        await asyncio.sleep(ConnectionVerdict.WINDOW_S + 0.005)
        relay.offer(chunk(fid + 1, key=True))
        await asyncio.sleep(0)
        out["gated"] = list(told)
        await window(30, 1)
        await window(30, 1)
        await asyncio.sleep(0)
        out["recovered"] = list(told)
        out["only"] = id(ws)
    finally:
        w._broadcast_to_clients = real
    return out


ConnectionVerdict.WINDOW_S = 0.05
r = asyncio.run(websockets_relay())
res.check("a websockets client sent every chunk is not told anything", r["clean"] == [], r["clean"])
res.check("nor is one held back as stalled, a page that stopped answering", r["stalled"] == [], r["stalled"])
res.check("one whose chunks the gate holds back for its link is told its connection is poor",
          r["gated"] == [("CONNECTION poor", r["only"])], r["gated"])
res.check("and told it is good again once its chunks arrive",
          r["recovered"] == [("CONNECTION poor", r["only"]), ("CONNECTION ok", r["only"])], r["recovered"])


def webrtc_peer() -> dict:
    """A peer whose frames the pacer sends, then drops, then sends past a bridge
    holding frames back for the link's rate, over windows of 50 ms."""
    told = []
    bridge = SimpleNamespace(over_budget=0)
    channel = SimpleNamespace()
    app = SimpleNamespace(
        peer_connections={"peer": {"display_id": "primary", "data_channel": channel}},
        displays={"primary": {"video_bridge": bridge}},
        send_message_to_channel=lambda ch, kind, data: told.append((ch is channel, kind, data)))
    out = {}

    def window(frames: int, sent: bool, lost: int = 0, held: int = 0) -> None:
        start = time.monotonic()
        for i in range(frames):
            RTCApp._note_connection(app, "peer", 1, 1)
            if sent:
                RTCApp._note_connection(app, "peer", 0, -1)
            if i < lost:
                RTCApp._note_connection(app, "peer", 0, 1)
        bridge.over_budget += held
        time.sleep(max(0.0, ConnectionVerdict.WINDOW_S - (time.monotonic() - start)) + 0.005)

    for _ in range(3):
        window(30, True)
    out["clean"] = list(told)
    window(30, False)
    window(30, True)
    out["dropped"] = list(told)
    window(30, True)
    window(30, True)
    out["recovered"] = list(told)
    window(30, True, lost=10)
    window(30, True)
    out["lost"] = list(told)
    for _ in range(2):
        window(30, True)
    window(20, True, held=20)
    window(30, True)
    out["held"] = list(told)
    RTCApp._note_connection(app, "gone", 1, 1)
    return out


r = webrtc_peer()
poor, good = (True, "connection", {"poor": True}), (True, "connection", {"poor": False})
res.check("a webrtc peer sent every frame is not told anything", r["clean"] == [], r["clean"])
res.check("one whose frames the pacer never sent is told on its own channel its connection is poor",
          r["dropped"] == [poor], r["dropped"])
res.check("and told it is good again once they are sent", r["recovered"] == [poor, good], r["recovered"])
res.check("frames lost past repair count against it", r["lost"][2:3] == [poor], r["lost"])
res.check("and so do frames its display's bridge held back for the link's rate",
          r["held"][4:5] == [poor], r["held"])

sys.exit(0 if res.summary() else 1)
