#!/usr/bin/env python3
"""A display's capture rate under a budget its content cannot fit.

A target cut by a queue the path showed (the steer holds the capacity it
measured) whose bridge drops more than FPS_SHED_SHARE of a tick's frames lowers
the capture's rate to the share let through, to FPS_FLOOR at least. A target cut
by loss alone, on a path that never queued, sheds nothing, nor does a tick at the
ceiling or of fewer than FPS_MIN_FRAMES frames. A shed rate climbs FPS_RISE a
tick that dropped FPS_KEEP_SHARE or less, holds otherwise, and returns to the
configured rate past it or once the target is back at its ceiling. A tick whose
bridge dropped most frames, whatever cut the target, keeps the path probe from
padding the display as a still screen. Driven with stand-ins.
"""
import asyncio
import os
import sys
from types import SimpleNamespace
from unittest.mock import AsyncMock, Mock, patch

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402

from selkies import webrtc_mode  # noqa: E402
from selkies.webrtc_mode import (  # noqa: E402
    FPS_FLOOR, FPS_KEEP_SHARE, FPS_MIN_FRAMES, FPS_RISE, FPS_SHED_SHARE, WebRTCService)

res = H.Results("capture-shed")
CEILING = 8000.0


def display(configured: float = 60.0):
    """A congestion loop stand-in with the real `_shed_frames`, a bridge whose counts `tick` advances, and a
    pipeline recording the rates it is shed to."""
    counts = [0, 0]
    rates = []
    pipeline = SimpleNamespace(framerate=configured, shed_framerate=rates.append)
    loop = SimpleNamespace(rtc_app=SimpleNamespace(video_budget_counts=lambda did: tuple(counts)),
                           _budget_counts={}, _shed_fps={})

    def tick(over: int, passed: int, target: float = 1000.0, queued: bool = True):
        counts[0] += over
        counts[1] += passed
        WebRTCService._shed_frames(loop, "primary", pipeline, target, CEILING, queued)
        return loop._shed_fps.get("primary")

    tick(0, 0)
    return tick, rates


tick, rates = display()
res.check("a target cut by loss alone sheds nothing, whatever the bridge drops",
          tick(56, 4, queued=False) is None and tick(56, 4, queued=False) is None and not rates, rates)
shed = tick(50, 10)
res.check("one cut by a queue the path showed sheds the capture to the share let through",
          shed == 60 * 10 / 60 and rates == [shed], rates)
res.check("again at the shed rate", tick(6, 4) == shed * 4 / 10, rates)
res.check("to FPS_FLOOR at least", tick(9, 1) == FPS_FLOOR, rates)
res.check("a tick that dropped nothing climbs FPS_RISE", tick(0, 2) == FPS_FLOOR * FPS_RISE, rates)
before = tick(0, 0)
res.check("so does a still tick", before == FPS_FLOOR * FPS_RISE ** 2, rates)
res.check("and one that dropped FPS_KEEP_SHARE or less", tick(1, 10) == before * FPS_RISE, rates)
held = len(rates)
tick(3, 7)
res.check("one that dropped more, short of FPS_SHED_SHARE, holds", len(rates) == held, rates)
tick(8, 2, queued=False)
res.check("a shed rate climbs back once the cut is loss alone, and sheds no further", len(rates) == held, rates)
while tick(0, 10) is not None:
    pass
res.check("climbing past the configured rate restores it", rates[-1] is None, rates)

tick, rates = display()
tick(50, 10)
res.check("(shed again)", rates == [10.0], rates)
tick(10, 0, target=CEILING)
res.check("a target back at its ceiling restores the configured rate whatever the bridge dropped",
          rates[-1] is None, rates)
tick(50, 10, target=CEILING)
res.check("and a tick at the ceiling never sheds", rates == [10.0, None], rates)
small = FPS_MIN_FRAMES - 1
tick(small, 0)
res.check("nor one of fewer than FPS_MIN_FRAMES frames", rates == [10.0, None], rates)
res.check("(the shares order as described)", FPS_KEEP_SHARE < FPS_SHED_SHARE and FPS_RISE > 1)



def crowded_tick(over: int, passed: int, queued: bool = False) -> bool:
    loop = SimpleNamespace(rtc_app=SimpleNamespace(video_budget_counts=lambda did: (over, passed)),
                           _budget_counts={"primary": (0, 0)}, _shed_fps={})
    pipeline = SimpleNamespace(framerate=60.0, shed_framerate=lambda fps: None)
    return WebRTCService._shed_frames(loop, "primary", pipeline, 1000.0, CEILING, queued)


res.check("a tick whose bridge dropped most frames reads as crowded, whatever cut the target",
          crowded_tick(50, 10) and crowded_tick(50, 10, queued=True))
res.check("one that dropped half or fewer, or of too few frames, does not",
          not crowded_tick(5, 5) and not crowded_tick(FPS_MIN_FRAMES - 1, 0))


async def probed(over: int, passed: int) -> list:
    """One congestion loop tick of a display whose bridge dropped `over` and passed `passed` frames since the
    last: the content rate the path probe was given."""
    given = []
    probe = SimpleNamespace(padding=False, rate_bps=lambda target, ceiling, content, *a: given.append(content) or 0.0)
    steer = SimpleNamespace(hold_until=0.0, restored=False, capacity_kbps=None, target=lambda *a, **k: 800.0)
    pipeline = SimpleNamespace(rc_mode=webrtc_mode.RateControlMode.CBR, video_bitrate=1000,
                               set_video_bitrate=AsyncMock())
    service = WebRTCService.__new__(WebRTCService)
    service.metrics, service.args = None, SimpleNamespace(congestion_control=True)
    service._congestion_steer = {"primary": steer}
    service._path_probes, service._padding, service._rate_holds, service._link_allowances = \
        {"primary": probe}, {}, {}, {}
    service._audio_sent, service._shed_fps, service._budget_counts = {}, {}, {"primary": (0, 0)}
    service._display_setting = lambda did, key: 6000
    service.display_pipelines = {"primary": pipeline}
    service._ensure_pacer = Mock()
    service._pad_display = lambda *a: None
    service._display_owner_tabs, service._peer_tabs = {}, {}
    window = dict(goodput_bps=500_000, sent_bps=200_000, loss_fraction=0.0, queue_ms=0.0,
                  queue_rising_ms=None, queue_depth_ms=0.0)
    transport = SimpleNamespace(take_twcc_window=lambda: dict(window))
    service.rtc_app = SimpleNamespace(peer_connections={"peer": dict(
        peer_conn=SimpleNamespace(sctp=SimpleNamespace(transport=transport)), display_id="primary",
        video_sender=None)}, send_cc_rate=lambda did, kbps: None, set_video_budget=lambda did, bps: None,
        video_budget_counts=lambda did: (over, passed))
    sleep = AsyncMock(side_effect=[None, asyncio.CancelledError()])
    with patch.object(webrtc_mode, "asyncio", SimpleNamespace(sleep=sleep)), \
            patch.object(webrtc_mode, "time", SimpleNamespace(monotonic=lambda: 32.0)), \
            patch.object(webrtc_mode, "settings", SimpleNamespace(video_bitrate=(100, 12000), webrtc_pacer=(True,))):
        try:
            await service._congestion_control_loop()
        except asyncio.CancelledError:
            pass
    return given


given = asyncio.run(probed(50, 10))
res.check("the path probe is given no content rate for a crowded display, so it pads nothing", given == [None], given)
given = asyncio.run(probed(0, 10))
res.check("and what the stream sent for one that is not", given == [200_000.0], given)

sys.exit(0 if res.summary() else 1)
