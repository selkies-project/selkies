#!/usr/bin/env python3
"""The WebRTC video bridge keeps the wire decodable across its own drops.

A frame the bridge drops for a lagging sender is never packetized, so the
receiver sees no gap and asks for nothing, while every delta frame behind it
references a picture that was never sent. The bridge closes a gate on the
first drop and holds delta frames until a keyframe arrives, asks for that
keyframe while the gate is closed, never lets a delta frame evict a queued
keyframe, and reopens on the keyframe alone. A frame that names what it
predicts from is dropped with a word to the encoder instead, and only the
frames predicting from a dropped one are held back, so the stream resumes on
the encoder's next frame with no keyframe. The audio bridge is a deeper FIFO
with no request path and keeps dropping oldest. Driven with stand-in frames
and a manual clock; no encoder, no peer.
"""
import asyncio
import os
import sys
from types import SimpleNamespace

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

from selkies.webrtc_engine import GATE_TIMEOUT_S, IDR_REQUEST_FLOOR_S, PipelineBridge


class Clock:
    def __init__(self) -> None:
        self.now = 1000.0

    def __call__(self) -> float:
        return self.now


async def drain(bridge: PipelineBridge) -> list:
    """Everything queued right now, in order."""
    out = []
    while not bridge.empty():
        out.append(await bridge.get_data())
    return out


async def scenario(res: H.Results) -> None:
    clock = Clock()
    requests = []
    bridge = PipelineBridge(request_keyframe=lambda: requests.append(clock.now), clock=clock)

    bridge.set_data("P0", keyframe=False)
    res.check("a delta frame queues while the sender keeps up",
              await drain(bridge) == ["P0"] and bridge.dropped == 0 and not requests)

    # The sender stalls: the queued delta frame and the one behind it are both
    # useless once one of them is missing from the wire.
    bridge.set_data("P1", keyframe=False)
    bridge.set_data("P2", keyframe=False)
    res.check("a drop discards both delta frames and asks for a keyframe",
              bridge.empty() and bridge.dropped == 2 and requests == [clock.now],
              (bridge.dropped, requests))
    clock.now += 0.05
    bridge.set_data("P3", keyframe=False)
    res.check("delta frames behind the gap are held back",
              bridge.empty() and bridge.dropped == 3, bridge.dropped)
    res.check("a request inside the floor is not repeated", len(requests) == 1, requests)

    clock.now += 0.05
    bridge.set_data("IDR1", keyframe=True)
    clock.now += 0.02
    bridge.set_data("P4", keyframe=False)
    res.check("a delta frame arriving behind a queued keyframe never evicts it",
              await drain(bridge) == ["IDR1"] and bridge.dropped == 4, bridge.dropped)
    clock.now += 0.02
    bridge.set_data("P5", keyframe=False)
    res.check("the chain behind that keyframe is broken again, so delta frames wait",
              bridge.empty() and bridge.dropped == 5, bridge.dropped)

    clock.now += IDR_REQUEST_FLOOR_S
    bridge.set_data("P6", keyframe=False)
    res.check("a held delta frame past the floor asks again",
              len(requests) == 2 and requests[-1] == clock.now, requests)

    bridge.set_data("IDR2", keyframe=True)
    got = await drain(bridge)
    bridge.set_data("P7", keyframe=False)
    got += await drain(bridge)
    bridge.set_data("P8", keyframe=False)
    got += await drain(bridge)
    res.check("delta frames flow again behind a delivered keyframe",
              got == ["IDR2", "P7", "P8"], got)

    before = len(requests)
    bridge.set_data("IDR4", keyframe=True)
    bridge.set_data("IDR5", keyframe=True)
    res.check("a newer keyframe replaces a queued one without closing the gate",
              await drain(bridge) == ["IDR5"] and len(requests) == before, requests)
    bridge.set_data("P10", keyframe=False)
    res.check("and the delta frame behind the delivered keyframe still flows",
              await drain(bridge) == ["P10"])

    # A keyframe that never comes must not starve the stream forever.
    bridge.set_data("P11", keyframe=False)
    bridge.set_data("P12", keyframe=False)
    clock.now += GATE_TIMEOUT_S + 0.01
    bridge.set_data("P13", keyframe=False)
    res.check("a gate no keyframe answers within the timeout lets delta frames through",
              await drain(bridge) == ["P13"], bridge.dropped)

    audio = PipelineBridge(maxsize=3)
    for i in range(5):
        audio.set_data(f"A{i}")
    res.check("a deeper bridge without a request path drops oldest and keeps order",
              await drain(audio) == ["A2", "A3", "A4"] and audio.dropped == 2, audio.dropped)


async def references(res: H.Results) -> None:
    clock = Clock()
    requests, forgotten = [], []
    bridge = PipelineBridge(request_keyframe=lambda: requests.append(clock.now), clock=clock,
                            invalidate_reference=forgotten.append)
    frame = lambda fid, ref: SimpleNamespace(name=f"F{fid}", dependency=(fid, ref))
    names = lambda items: [f.name for f in items]

    bridge.set_data(frame(0, None), keyframe=True)
    got = await drain(bridge)
    bridge.set_data(frame(1, 0), keyframe=False)
    res.check("a frame predicting from the delivered keyframe flows",
              names(got + await drain(bridge)) == ["F0", "F1"] and not forgotten)

    # The sender stalls with frame 2 queued: 3 evicts it, and 3 itself predicts
    # from the evicted frame.
    bridge.set_data(frame(2, 1), keyframe=False)
    bridge.set_data(frame(3, 2), keyframe=False)
    res.check("a drop tells the encoder which frame went and holds what predicts from it",
              bridge.empty() and forgotten == [2] and bridge.dropped == 2 and not requests,
              (forgotten, bridge.dropped, requests))
    bridge.set_data(frame(4, 3), keyframe=False)
    res.check("a frame predicting from a held frame is held too",
              bridge.empty() and bridge.dropped == 3 and forgotten == [2])
    bridge.set_data(frame(5, 1), keyframe=False)
    res.check("the encoder's frame predicting past the drop flows, with no keyframe asked",
              names(await drain(bridge)) == ["F5"] and not requests, requests)

    bridge.set_data(frame(6, None), keyframe=True)
    bridge.set_data(frame(7, 6), keyframe=False)
    res.check("a frame behind a queued keyframe is dropped and named, the keyframe kept",
              names(await drain(bridge)) == ["F6"] and forgotten == [2, 7], forgotten)
    bridge.set_data(frame(8, 6), keyframe=False)
    res.check("a frame predicting from the delivered keyframe flows",
              names(await drain(bridge)) == ["F8"])
    bridge.set_data(frame(9, 8), keyframe=False)
    bridge.set_data(frame(10, 8), keyframe=False)
    res.check("a frame evicted by one predicting from an earlier frame does not hold it back",
              names(await drain(bridge)) == ["F10"] and forgotten == [2, 7, 9], forgotten)


def main() -> int:
    res = H.Results("video-bridge-gate")
    asyncio.run(scenario(res))
    asyncio.run(references(res))
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
