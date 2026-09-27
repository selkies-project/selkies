#!/usr/bin/env python3
"""The moving figures cost nothing while nobody looks at them.

A page asks for `stream_stats` with the `_stats` verb while its stats are on
screen, and the host's sampler, which may spawn a vendor tool per GPU query,
samples only while some page has asked: unwatched it keeps ticking, for what
else rides its cadence, and holds no reading rather than a stale one. Whether a
session asked for a hardware encoder at all is what lets a page tell a software
session somebody chose, or one on a host with no GPU, from one that fell back.
A page that opens its stats has its first figures soon, and differenced over
the rush's window rather than whatever was left of a period: a rate taken over
a few milliseconds is one frame more or less, read as a hundred fps.
"""
import asyncio
import os
import sys
import time

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, TESTS)
sys.path.insert(0, os.path.join(os.path.dirname(TESTS), "src"))
# The settings parser reads argv when the package is imported.
sys.argv = ["selkies"]

import helpers as H  # noqa: E402
from selkies import resource_stats as RS  # noqa: E402
from selkies import stream_stats as SS  # noqa: E402

res = H.Results("stream-stats")

res.check("the verb subscribes", SS.stats_request("_stats,1") is True)
res.check("and unsubscribes", SS.stats_request("_stats,0") is False)
res.check("anything but 1 is off", SS.stats_request("_stats,yes") is False and SS.stats_request("_stats") is False)
res.check("another message is not the verb",
          SS.stats_request("_stats_video,{}") is None and SS.stats_request("kd,65") is None)

res.check("a full-frame encoder on a host with a GPU should encode on it", SS.hardware_expected("h264enc", False, True))
res.check("unless software encoding is on", not SS.hardware_expected("h264enc", True, True))
res.check("the striped encoder never does", not SS.hardware_expected("h264enc-striped", False, True))
res.check("nor does JPEG", not SS.hardware_expected("jpeg", False, True))
res.check("and a host with no GPU expects none", not SS.hardware_expected("h264enc", False, False))
probed = {"h264": {"hardware": "nvenc"}, "av1": {"hardware": None}}
res.check("nor a codec the GPU has no engine for, as the startup probe found",
          not SS.hardware_expected("av1enc", False, True, probed) and SS.hardware_expected("h264enc", False, True, probed))
res.check("an unknown probe leaves the engine assumed", SS.hardware_expected("av1enc", False, True, None))
res.check("GPU presence is what the device nodes say",
          SS.gpu_present() == (any(n.startswith("renderD") for n in (os.listdir("/dev/dri") if os.path.isdir("/dev/dri") else []))
                               or os.path.exists("/dev/nvidiactl")))

res.check("no sample, no host figures", SS.host_stats(RS.ResourceMonitor()) == {})


async def sampled(watched):
    """What the monitor holds after two periods, and how often it ticked."""
    monitor = RS.ResourceMonitor()
    monitor.watched = lambda: watched
    ticks = []

    async def on_tick(now):
        ticks.append((now, monitor.system))

    monitor.on_tick = on_tick
    monitor.start()
    await asyncio.sleep(1.5)
    await monitor.stop()
    return monitor, ticks


monitor, ticks = asyncio.run(sampled(False))
res.check("unwatched, the monitor still ticks", len(ticks) >= 2, len(ticks))
res.check("but samples nothing", all(system is None for _, system in ticks) and monitor.system is None)

monitor, ticks = asyncio.run(sampled(True))
host = SS.host_stats(monitor)
res.check("watched, it samples the host", monitor.system is not None
          and 0 <= host["cpu_percent"] <= 100 and 0 < host["mem_used"] <= host["mem_total"], host)


async def rushed_before_the_period_ends():
    """How long after a rush's CPU baseline the next sample comes, for a page that
    opens its stats just before a period would end."""
    monitor = RS.ResourceMonitor()
    monitor.watched = lambda: True
    stamps, ticks = [], []
    usage = monitor._usage.sample

    def stamped():
        stamps.append(time.monotonic())
        return usage()

    async def on_tick(now):
        ticks.append(time.monotonic())

    monitor._usage.sample = stamped
    monitor.on_tick = on_tick
    monitor.start()
    while not ticks:
        await asyncio.sleep(0.01)
    await asyncio.sleep(max(0.0, ticks[0] + monitor.period - 0.05 - time.monotonic()))
    await monitor.rush()
    baseline = stamps[-1]
    while not any(t > baseline for t in ticks):
        await asyncio.sleep(0.01)
    await monitor.stop()
    return min(s for s in stamps if s > baseline) - baseline


window = asyncio.run(rushed_before_the_period_ends())
res.check("a page that opens its stats as a period ends still has its first CPU figure differenced "
          "over the rush's window", window >= RS.RUSH_S / 2, f"{window:.3f} s")


class Counting:
    """A capture whose encode counters the test advances by hand."""

    def __init__(self):
        self.frames = 0

    def stream_stats(self):
        return {"frames": self.frames, "encode_ns": self.frames * 2_000_000, "pipeline_ns": self.frames * 3_000_000}


watch = SS.StreamWatch("primary", None)
watch._module = capture = Counting()
res.check("the first call only takes the baseline", watch.rates() == {})
capture.frames += 1
res.check("a window too short to hold a rate reports none rather than one frame over a few microseconds",
          watch.rates() == {})
time.sleep(SS.RATE_WINDOW_MIN_S)
capture.frames += 29
rates = watch.rates()
res.check("and keeps growing, so the next call covers both", 0 < rates.get("encoded_fps", 0) <= 30 / SS.RATE_WINDOW_MIN_S
          and rates.get("encode_ms") == 2.0, rates)

sys.exit(0 if res.summary() else 1)
