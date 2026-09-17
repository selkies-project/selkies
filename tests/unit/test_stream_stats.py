#!/usr/bin/env python3
"""The moving figures cost nothing while nobody looks at them.

A page asks for `stream_stats` with the `_stats` verb while its stats are on
screen, and the host's sampler, which may spawn a vendor tool per GPU query,
samples only while some page has asked: unwatched it keeps ticking, for what
else rides its cadence, and holds no reading rather than a stale one. Whether a
session asked for a hardware encoder at all is what lets a page tell a software
session somebody chose, or one on a host with no GPU, from one that fell back.
"""
import asyncio
import os
import sys

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

sys.exit(0 if res.summary() else 1)
