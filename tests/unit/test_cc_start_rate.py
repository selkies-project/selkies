#!/usr/bin/env python3
"""A steered rate that held is told to the page once, and a page's remembered
rate starts the next stream of a restarted server.

`RateHold` returns a rate once it has stayed within 5 % of where it settled
for 10 s, and starts over when it moves out of that band; `start_kbps` takes
the page's number into the configured range and refuses anything else. Over
WebRTC the seed applies to a display's only controller, never under a steer
another page's stream already drives, and never above the display's ceiling.
"""
import asyncio
import math
import os
import sys
import tempfile
from types import SimpleNamespace

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(TESTS), "src"))
sys.path.insert(0, TESTS)

for _key in [k for k in os.environ if k.startswith("SELKIES_")]:
    del os.environ[_key]
os.environ["SELKIES_FILE_MANAGER_PATH"] = tempfile.mkdtemp(prefix="selkies-ccstart-")

import helpers as H  # noqa: E402

from selkies.settings import RateControlMode  # noqa: E402
from selkies.stream_server import RateHold, start_kbps  # noqa: E402
from selkies.webrtc_engine import ClientType  # noqa: E402
import selkies.webrtc_mode as W  # noqa: E402


class FakePipeline:
    """A CBR pipeline stand-in recording the rates it is set to."""

    def __init__(self, kbps: float) -> None:
        self.rc_mode = RateControlMode.CBR
        self.video_bitrate = kbps
        self.set_to = []

    async def set_video_bitrate(self, kbps: int) -> None:
        self.set_to.append(kbps)
        self.video_bitrate = kbps


def seed(remembered, others: int = 0, ceiling: float = 8000.0, cc: bool = True) -> list:
    """What `_seed_start_rate` sets a primary display's pipeline to."""
    service = W.WebRTCService.__new__(W.WebRTCService)
    service.args = SimpleNamespace(congestion_control=cc)
    pipeline = FakePipeline(8000.0)
    service.display_pipelines = {"primary": pipeline}
    peers = {"me": {"client_type": ClientType.CONTROLLER, "display_id": "primary"}}
    for i in range(others):
        peers[f"other{i}"] = {"client_type": ClientType.CONTROLLER, "display_id": "primary"}
    service.rtc_app = SimpleNamespace(peer_connections=peers)
    service._display_setting = lambda did, key: ceiling
    asyncio.run(service._seed_start_rate("me", "primary", remembered))
    return pipeline.set_to


def main() -> int:
    res = H.Results("cc-start-rate")

    hold = RateHold()
    told = [hold.note(kbps, t) for t, kbps in [(0, 2000), (4, 2050), (9.9, 1960), (10, 2040), (15, 2000)]]
    res.check("a rate held within 5 % for 10 s is told once", told == [None, None, None, 2000, None], told)
    hold = RateHold()
    told = [hold.note(kbps, t) for t, kbps in [(0, 2000), (6, 2300), (12, 2300), (16, 2310)]]
    res.check("a move out of the band starts the 10 s over", told == [None, None, None, 2300], told)
    hold = RateHold()
    told = [hold.note(kbps, t) for t, kbps in [(0, 800), (11, 800), (12, 3000), (23, 3000)]]
    res.check("and a new level that holds is told in turn", told == [None, 800, None, 3000], told)

    lo, hi = 100.0, 100000.0
    res.check("a remembered rate starts within the configured range",
              start_kbps(2500, lo, hi) == 2500 and start_kbps(40, lo, hi) == lo and start_kbps(5e6, lo, hi) == hi)
    res.check("anything but a positive number starts nothing",
              all(start_kbps(v, lo, hi) is None for v in (None, "fast", -3, 0, math.nan, math.inf, [2000])))

    res.check("webrtc: the display's only controller starts at its remembered rate", seed(2000) == [2000], seed(2000))
    res.check("but not beside another controller, whose stream the steer already drives", seed(2000, others=1) == [])
    res.check("nor above the display's ceiling, nor without congestion control",
              seed(12000) == [] and seed(2000, ceiling=1500) == [1500] and seed(2000, cc=False) == [])
    res.check("nor from nothing remembered", seed(None) == [])
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
