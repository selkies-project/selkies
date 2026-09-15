#!/usr/bin/env python3
"""A card that measures nothing is not published as a reading of zero.

Cards are listed for what they are even where their driver exposes no
unprivileged counters, so the pipeline can match the one it captures on by
vendor or PCI address. Publishing such a card's zeros every tick would leave a
page showing a utilization that can never move and a memory total of nothing,
so a sample with no counters at all is no sample, and the collector stops
probing. A card that really is idle still has a memory total, so it keeps
reporting.

Driven against the collector with stand-in detections; no GPU needed.
"""
import os
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, TESTS)
sys.path.insert(0, os.path.join(os.path.dirname(TESTS), "src"))

import helpers as H  # noqa: E402
from selkies import resource_stats as RS  # noqa: E402

res = H.Results("gpu-stats-gate")


def sample(*gpus):
    """What the collector makes of that detection."""
    original = RS.get_gpus
    RS.get_gpus = lambda dri_node=None: list(gpus)
    try:
        return RS.ResourceMonitor()._gpu_sample()
    finally:
        RS.get_gpus = original


def stat(load, total, used, vendor):
    return RS.GPUStat(0, load, total, used, "0000:01:00.0", vendor)


res.check("a card exposing no counters is not a reading",
          sample(stat(0.0, 0.0, 0.0, "intel")) is None)
res.check("nor is an empty detection", sample() is None)

idle = sample(stat(0.0, 8192.0, 512.0, "amd"))
res.check("an idle card with memory still reports",
          idle is not None and idle["gpu_percent"] == 0.0
          and idle["memory_total"] == 8192 * 1024 * 1024, idle)

busy = sample(stat(0.42, 0.0, 0.0, "apple"))
res.check("so does a card that reports utilization but no memory",
          busy is not None and round(busy["gpu_percent"], 2) == 42.0, busy)

sys.exit(0 if res.summary() else 1)
