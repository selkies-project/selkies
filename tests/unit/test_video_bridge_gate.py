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
the encoder's next frame with no keyframe, unless the encoder never hears of the
drop: a second held frame it coded after the word went out asks for a keyframe
until one arrives (without encode instants, a run of held frames does). The audio
bridge is a deeper FIFO with no request path and keeps dropping oldest. Driven
with stand-in frames and a manual clock; no encoder, no peer.
"""
import asyncio
import os
import sys
from types import SimpleNamespace

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

from selkies.webrtc_engine import GATE_TIMEOUT_S, IDR_REQUEST_FLOOR_S, LOST_CHAIN_FRAMES, PipelineBridge


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


async def unheard(res: H.Results) -> None:
    """The capture's first two frames arrive together, the keyframe still queued, so the
    second is let go; the encoder never hears of it (a word sent while the capture is
    still starting is lost) and keeps predicting from each frame before."""
    clock = Clock()
    requests, forgotten = [], []
    bridge = PipelineBridge(request_keyframe=lambda: requests.append(clock.now), clock=clock,
                            invalidate_reference=forgotten.append)
    # Encode instants are CLOCK_MONOTONIC nanoseconds, the clock the bridge runs on.
    frame = lambda fid, ref, at=None: SimpleNamespace(
        name=f"F{fid}", dependency=(fid, ref), timing=None if at is None else (0, int(at * 1e9), 0))
    names = lambda items: [f.name for f in items]

    bridge.set_data(frame(0, None, clock.now), keyframe=True)
    bridge.set_data(frame(1, 0, clock.now), keyframe=False)
    told = clock.now
    res.check("a frame behind the queued first keyframe is let go and named",
              names(await drain(bridge)) == ["F0"] and forgotten == [1], forgotten)
    fid = 2
    for _ in range(LOST_CHAIN_FRAMES + 3):
        clock.now += 0.016
        bridge.set_data(frame(fid, fid - 1, told - 0.001), keyframe=False)
        fid += 1
    res.check("frames the encoder coded before it was told are held back without a keyframe, "
              "however late the loop takes them", bridge.empty() and not requests, requests)
    bridge.set_data(frame(fid, fid - 1, told + 0.004), keyframe=False)
    fid += 1
    res.check("the first it coded after the word proves nothing: it may have read its words just before",
              bridge.empty() and not requests, requests)
    clock.now += 0.016
    bridge.set_data(frame(fid, fid - 1, told + 0.02), keyframe=False)
    fid += 1
    res.check("a second one asks for a keyframe", bridge.empty() and len(requests) == 1, requests)
    for _ in range(3):
        clock.now += 0.016
        bridge.set_data(frame(fid, fid - 1, clock.now), keyframe=False)
        fid += 1
    res.check("and not again inside the floor", len(requests) == 1, requests)
    clock.now += IDR_REQUEST_FLOOR_S
    bridge.set_data(frame(fid, fid - 1, clock.now), keyframe=False)
    fid += 1
    res.check("but again past it while the encoder still predicts from what was let go",
              len(requests) == 2, requests)
    bridge.set_data(frame(fid, None, clock.now), keyframe=True)
    got = await drain(bridge)
    bridge.set_data(frame(fid + 1, fid, clock.now), keyframe=False)
    res.check("the keyframe ends it and what predicts from it flows",
              names(got + await drain(bridge)) == [f"F{fid}", f"F{fid + 1}"] and forgotten == [1],
              (forgotten, requests))
    fid += 1
    for _ in range(LOST_CHAIN_FRAMES + 2):
        clock.now += IDR_REQUEST_FLOOR_S
        bridge.set_data(frame(fid + 1, fid, clock.now), keyframe=False)
        await drain(bridge)
        fid += 1
    res.check("frames that flow ask for nothing", len(requests) == 2, requests)

    # Without encode instants the run is counted instead.
    bridge = PipelineBridge(request_keyframe=lambda: requests.append(clock.now), clock=clock,
                            invalidate_reference=forgotten.append)
    requests.clear()
    bridge.set_data(frame(0, None), keyframe=True)
    bridge.set_data(frame(1, 0), keyframe=False)
    await drain(bridge)
    for fid in range(2, 2 + LOST_CHAIN_FRAMES):
        bridge.set_data(frame(fid, fid - 1), keyframe=False)
    res.check("frames without an encode instant are held up to the run", not requests, requests)
    bridge.set_data(frame(2 + LOST_CHAIN_FRAMES, 1 + LOST_CHAIN_FRAMES), keyframe=False)
    res.check("and one more asks for a keyframe", len(requests) == 1, requests)


def main() -> int:
    res = H.Results("video-bridge-gate")
    asyncio.run(scenario(res))
    asyncio.run(references(res))
    asyncio.run(unheard(res))
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
