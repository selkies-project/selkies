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
until one arrives (without encode instants, a run of held frames does). A bridge
held to its display's steered rate drops, with the word, the delta frames an
encoder overshoots the rate by, lets a stream within it through whole, and
passes a keyframe uncharged, and never drops the frames where an H.264
frame_num can wrap, whose drop the encoder answers with a keyframe. The audio bridge is a deeper FIFO with no
request path and keeps dropping oldest. Driven with stand-in frames and a manual
clock; no encoder, no peer.
"""
import asyncio
import os
import sys
from types import SimpleNamespace

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

from selkies.webrtc_engine import (BUDGET_WINDOW_S, FRAME_NUM_WRAP, GATE_TIMEOUT_S, IDR_REQUEST_FLOOR_S,
                                   LOST_CHAIN_FRAMES, PipelineBridge)


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


async def wrap(res: H.Results) -> None:
    """A drop the encoder predicts past is forgotten once a frame it coded after the word
    goes out, so the frame id recurring 65536 frames later, with no keyframe between
    (an infinite GOP), is not taken for what was let go."""
    clock = Clock()
    requests, forgotten = [], []
    bridge = PipelineBridge(request_keyframe=lambda: requests.append(clock.now), clock=clock,
                            invalidate_reference=forgotten.append)
    frame = lambda fid, ref, at: SimpleNamespace(
        name=f"F{fid}", dependency=(fid & 0xFFFF, ref if ref is None else ref & 0xFFFF),
        timing=(0, int(at * 1e9), 0))
    names = lambda items: [f.name for f in items]

    bridge.set_data(frame(0, None, clock.now), keyframe=True)
    await drain(bridge)
    for fid in range(1, 5):
        clock.now += 0.016
        bridge.set_data(frame(fid, fid - 1, clock.now), keyframe=False)
        await drain(bridge)
    # The sender stalls with frame 5 queued: 6 evicts it, and 6 and 7, coded before the
    # word, predict from it.
    clock.now += 0.016
    bridge.set_data(frame(5, 4, clock.now), keyframe=False)
    clock.now += 0.016
    bridge.set_data(frame(6, 5, clock.now - 0.001), keyframe=False)
    told = clock.now
    bridge.set_data(frame(7, 6, told - 0.0005), keyframe=False)
    res.check("the drop is named and what the encoder coded before the word is held",
              bridge.empty() and forgotten == [5] and not requests, (forgotten, requests))
    clock.now += 0.016
    bridge.set_data(frame(8, 4, clock.now), keyframe=False)
    res.check("the frame the encoder coded after the word predicts past the drop and flows",
              names(await drain(bridge)) == ["F8"] and not requests, requests)
    end = 0x10000 + 8
    fid = 9
    while fid < end:
        clock.now += 0.016
        bridge.set_data(frame(fid, fid - 1, clock.now), keyframe=False)
        if bridge.empty():
            break
        await drain(bridge)
        fid += 1
    res.check("65536 frames later the recurring ids of the frames let go flow like any other",
              fid == end and bridge.dropped == 3 and forgotten == [5] and not requests,
              (fid, bridge.dropped, forgotten, requests))


async def budget(res: H.Results) -> None:
    """The frames let through follow the steered rate: a stream within it passes whole,
    one three times over it is held to it by delta frames dropped with the word, a
    keyframe passes uncharged, so the delta frames behind it pass too, and frames that
    name nothing, which only a keyframe could repair, are never dropped for it."""
    clock = Clock()
    requests, forgotten = [], []
    rate = 80_000
    window = rate / 8 * BUDGET_WINDOW_S

    async def stream(bridge, first, size, seconds, fps=60, ref=None):
        """Frames of `size` bytes for `seconds`, each predicting from the newest let
        through; the bytes and ids let through."""
        passed, sent, last, fid = [], 0, ref, first
        for _ in range(int(seconds * fps)):
            clock.now += 1 / fps
            item = SimpleNamespace(name=f"F{fid}", dependency=(fid, last),
                                   timing=(0, int(clock.now * 1e9), 0), data=b"x" * size)
            bridge.set_data(item, keyframe=False)
            if not bridge.empty():
                await bridge.get_data()
                passed.append(fid)
                sent += size
                last = fid
            fid += 1
        return sent, passed, fid, last

    bridge = PipelineBridge(request_keyframe=lambda: requests.append(clock.now), clock=clock,
                            invalidate_reference=forgotten.append)
    bridge.set_budget(rate)
    bridge.set_data(SimpleNamespace(name="K0", dependency=(0, None), data=b"x" * 100), keyframe=True)
    await drain(bridge)
    sent, passed, fid, last = await stream(bridge, 1, 150, 2.0, ref=0)
    res.check("a stream within the rate passes whole", len(passed) == 120 and not forgotten,
              (len(passed), forgotten[:3]))
    sent, passed, fid, last = await stream(bridge, fid, 500, 10.0, ref=last)
    want = rate / 8 * 10.0
    res.check("a stream three times over the rate is held to it",
              want <= sent <= want + window + 500 and not requests, (sent, want))
    res.check("each frame it drops is named to the encoder",
              bridge.over_budget == 600 - len(passed) == len(forgotten) > 0,
              (bridge.over_budget, len(passed), len(forgotten)))
    wraps = [f for f in range(121, 721) if (f % FRAME_NUM_WRAP) == 0]
    res.check("a frame where frame_num can wrap is never dropped for it",
              not set(wraps) & set(forgotten) and set(wraps) <= set(passed),
              sorted(set(wraps) & set(forgotten))[:4])

    clock.now += 5.0
    bridge.set_data(SimpleNamespace(name="K1", dependency=(fid, None), data=b"x" * 8000), keyframe=True)
    res.check("a keyframe past the window passes", [f.name for f in await drain(bridge)] == ["K1"])
    key = fid
    dropped = len(forgotten)
    behind = []
    for n in (1, 2):
        clock.now += 1 / 60
        bridge.set_data(SimpleNamespace(name=f"D{n}", dependency=(key + n, key + n - 1), data=b"x" * 100),
                        keyframe=False)
        behind += [f.name for f in await drain(bridge)]
    res.check("it is not charged, so the delta frames behind it pass",
              behind == ["D1", "D2"] and len(forgotten) == dropped, (behind, forgotten[dropped:]))

    bridge.set_budget(None)
    sent, passed, fid, last = await stream(bridge, key + 3, 500, 2.0, ref=key + 2)
    res.check("a lifted budget lets every frame through", len(passed) == 120, len(passed))

    plain = PipelineBridge(request_keyframe=lambda: requests.append(clock.now), clock=clock)
    plain.set_budget(rate)
    plain.set_data(SimpleNamespace(name="K", data=b"x" * 100), keyframe=True)
    await drain(plain)
    delivered = 0
    for n in range(120):
        clock.now += 1 / 60
        plain.set_data(SimpleNamespace(name=f"P{n}", data=b"x" * 500), keyframe=False)
        delivered += len(await drain(plain))
    res.check("frames that name nothing are never dropped for the rate",
              delivered == 120 and plain.over_budget == 0, (delivered, plain.over_budget))


def main() -> int:
    res = H.Results("video-bridge-gate")
    asyncio.run(scenario(res))
    asyncio.run(references(res))
    asyncio.run(unheard(res))
    asyncio.run(wrap(res))
    asyncio.run(budget(res))
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
