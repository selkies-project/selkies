#!/usr/bin/env python3
"""The media relay reads its source only for consumers that wait for a frame.

A consumer slower than its source (a sender whose frames take longer to put on
the wire than the capture takes to produce them) gets the newest frame the
source holds each time it asks, the source's own policy dropping the rest, and
never a backlog whose age grows for as long as it lags; every consumer of the
source sees the same frames, and one that stops holds no one else back.
"""
import asyncio
import os
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies.webrtc.contrib.relay import MediaRelay  # noqa: E402
from selkies.webrtc.mediastreams import MediaStreamTrack  # noqa: E402

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402


class LatestSource(MediaStreamTrack):
    """A source keeping only its newest frame, as the pipeline bridges keep video."""

    kind = "video"

    def __init__(self) -> None:
        super().__init__()
        self.queue: asyncio.Queue = asyncio.Queue(maxsize=1)
        self.reads = 0

    def put(self, frame) -> None:
        if self.queue.full():
            self.queue.get_nowait()
        self.queue.put_nowait(frame)

    async def recv(self):
        self.reads += 1
        return await self.queue.get()


async def produce(source: LatestSource, count: int, interval: float) -> None:
    for i in range(count):
        source.put((i, time.monotonic()))
        await asyncio.sleep(interval)


async def consume(track, got: list, per_frame: float, limit: int) -> None:
    while len(got) < limit:
        frame = await track.recv()
        got.append((frame[0], time.monotonic() - frame[1]))
        await asyncio.sleep(per_frame)


async def main_async(res: H.Results) -> None:
    source = LatestSource()
    relay = MediaRelay()
    slow = relay.subscribe(source)
    got: list = []
    consumer = asyncio.ensure_future(consume(slow, got, 0.012, 30))
    await produce(source, 120, 0.004)
    await asyncio.wait_for(consumer, 5)
    ages = [age for _, age in got[5:]]
    res.check("a consumer slower than its source is handed recent frames, not a backlog",
              max(ages) < 0.030, f"oldest {max(ages) * 1000:.1f} ms over {len(ages)} frames")
    res.check("the frames it could not keep up with stay in the source",
              got[-1][0] - got[0][0] >= 2 * len(got) and len({i for i, _ in got}) == len(got),
              [i for i, _ in got[-5:]])
    res.check("the source is read once per frame handed on", source.reads <= len(got) + 1,
              (source.reads, len(got)))
    await relay.stop()

    source = LatestSource()
    relay = MediaRelay()
    fast, slower = relay.subscribe(source), relay.subscribe(source)
    got_fast: list = []
    got_slow: list = []
    tasks = [asyncio.ensure_future(consume(fast, got_fast, 0.0, 20)),
             asyncio.ensure_future(consume(slower, got_slow, 0.010, 20))]
    await produce(source, 80, 0.004)
    await asyncio.wait_for(asyncio.gather(*tasks), 5)
    res.check("every consumer is handed the same frames",
              [i for i, _ in got_fast] == [i for i, _ in got_slow], ([i for i, _ in got_fast][:6],
                                                                     [i for i, _ in got_slow][:6]))
    await relay.stop()

    source = LatestSource()
    relay = MediaRelay()
    leaving, staying = relay.subscribe(source), relay.subscribe(source)
    got_leaving: list = []
    got_staying: list = []

    async def leave() -> None:
        await consume(leaving, got_leaving, 0.0, 3)
        leaving.stop()

    tasks = [asyncio.ensure_future(leave()), asyncio.ensure_future(consume(staying, got_staying, 0.0, 15))]
    await produce(source, 40, 0.004)
    await asyncio.wait_for(asyncio.gather(*tasks), 5)
    res.check("a consumer that stops holds the others back from nothing",
              len(got_leaving) == 3 and len(got_staying) == 15, (len(got_leaving), len(got_staying)))
    await relay.stop()


def main() -> int:
    res = H.Results("media-relay-pull")
    asyncio.run(main_async(res))
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
