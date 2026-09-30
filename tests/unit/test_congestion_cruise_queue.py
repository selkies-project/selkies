#!/usr/bin/env python3
"""Pending cruise must respect an observed standing queue and bitrate limits."""
import asyncio
import os
import sys
import unittest
from collections import deque
from types import SimpleNamespace
from typing import Optional
from unittest.mock import AsyncMock, Mock, patch

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies import webrtc_mode, websockets_mode  # noqa: E402
from selkies.stream_server import CongestionSteer  # noqa: E402


class CruiseResumeTests(unittest.TestCase):
    """A pending cruise respects current queue evidence and selected limits."""
    def queued(self) -> CongestionSteer:
        steer = CongestionSteer()
        value = steer.target(6000, 6000, 1000, 2121025, 351 / 596, 31.0,
                             queue_s=0.18666666666666742)
        self.assertEqual(round(value), 1407)
        return steer

    def tick(self, steer: CongestionSteer, now: float = 32.0,
             queue: Optional[float] = 0.18841666666666512, current: float = 1407,
             goodput: float = 1797770, floor: float = 1000, ceiling: float = 6000,
             loss: float = 138 / 339) -> int:
        return round(steer.target(current, ceiling, floor, goodput, loss,
                                  now, queue_s=queue))

    def test_saved_raise_during_hold(self) -> None:
        steer = self.queued()
        self.assertEqual(self.tick(steer), 1407)
        self.assertEqual(self.tick(steer, now=32.01), 1407)

    def test_drain_deadline_with_queue(self) -> None:
        steer = self.queued()
        deadline = steer._drain_until
        self.assertEqual(self.tick(steer, now=deadline - 0.001), 1407)
        self.assertEqual(self.tick(steer, now=deadline), 1407)

    def test_clear_queue_resumes_once(self) -> None:
        steer = self.queued()
        current = self.tick(steer)
        resumed = self.tick(steer, now=32.01, queue=0.0, current=current, loss=0.0)
        self.assertEqual(resumed, 1803)
        self.assertIsNone(steer._cruise_kbps)
        self.assertEqual(self.tick(steer, now=32.02, queue=0.0, current=resumed, loss=0.0), 1803)

    def test_hold_expiry_rechecks_queue(self) -> None:
        steer = self.queued()
        now = steer.hold_until
        self.assertEqual(self.tick(steer, now=now), 1189)
        self.assertGreater(steer._drain_until, now)
        self.assertGreater(steer.hold_until, steer._drain_until)

    def test_hold_expiry_without_goodput_cuts_current(self) -> None:
        steer = self.queued()
        self.assertEqual(self.tick(steer, now=steer.hold_until, goodput=0), 1000)
        self.assertIsNone(steer._cruise_kbps)

    def test_unknown_queue_keeps_compatibility(self) -> None:
        self.assertEqual(self.tick(self.queued(), queue=None, loss=0.0), 1803)

    def test_changed_floor_is_respected(self) -> None:
        self.assertEqual(self.tick(self.queued(), floor=1600), 1600)

    def test_changed_ceiling_is_respected(self) -> None:
        self.assertEqual(self.tick(self.queued(), ceiling=1200), 1200)


class TransportCallerTests(unittest.IsolatedAsyncioTestCase):
    """Both production callers preserve the backed-off rate during the settle."""

    def queued(self) -> CongestionSteer:
        return CruiseResumeTests().queued()

    async def test_webrtc_tick_keeps_queued_target(self) -> None:
        pipeline = SimpleNamespace(rc_mode=webrtc_mode.RateControlMode.CBR,
                                   video_bitrate=1407, set_video_bitrate=AsyncMock())
        service = webrtc_mode.WebRTCService.__new__(webrtc_mode.WebRTCService)
        service.metrics = None
        service.args = SimpleNamespace(congestion_control=True)
        service._congestion_steer = {"primary": self.queued()}
        service._rate_holds = {}
        service._display_setting = lambda did, key: 6000
        service.display_pipelines = {"primary": pipeline}
        service._ensure_pacer = Mock()
        window = dict(goodput_bps=1797770, sent_bps=1797770, loss_fraction=138 / 339,
                      queue_ms=188.41666666666512, queue_rising_ms=None,
                      queue_depth_ms=188.41666666666512)
        transport = SimpleNamespace(take_twcc_window=lambda: dict(window))
        service.rtc_app = SimpleNamespace(peer_connections={"peer": dict(
            peer_conn=SimpleNamespace(sctp=SimpleNamespace(transport=transport)),
            display_id="primary", video_sender=None)}, send_cc_rate=lambda did, kbps: None)
        sleep = AsyncMock(side_effect=[None, asyncio.CancelledError()])
        with patch.object(webrtc_mode, "asyncio", SimpleNamespace(sleep=sleep)), \
                patch.object(webrtc_mode, "time", SimpleNamespace(monotonic=lambda: 32.0)), \
                patch.object(webrtc_mode, "settings", SimpleNamespace(
                    video_bitrate=(1000, 12000), webrtc_pacer=(True,))):
            with self.assertRaises(asyncio.CancelledError):
                await service._congestion_control_loop()
        pipeline.set_video_bitrate.assert_not_awaited()
        service._ensure_pacer.assert_called_once()

    async def test_websocket_tick_keeps_queued_target(self) -> None:
        module = SimpleNamespace(update_video_bitrate=Mock())
        server = websockets_mode.DataStreamingServer.__new__(websockets_mode.DataStreamingServer)
        server.capture_instances = {"primary": {"module": module}}
        server._initial_video_bitrate = 6000
        acks = deque((31.5 + i * .05, 208.41666666666512, i * .05 * 1797770 / 8)
                     for i in range(1, 11))
        state = dict(video_bitrate=6000, link_kbps=1407, link_tick_at=31.5,
                     sent_bytes=int(.5 * 1797770 / 8), link_tick_sent=0,
                     backpressure_enabled=True, rtt_floor_ms=20, link_acks=acks,
                     link_jitter_ms=0, link_steer=self.queued())
        with patch.object(websockets_mode, "app_settings", SimpleNamespace(video_bitrate=(1000, 12000))):
            server._steer_bitrate_to_link("primary", state, 32.0)
        self.assertEqual(state["link_kbps"], 1407)
        module.update_video_bitrate.assert_not_called()


if __name__ == "__main__":
    unittest.main()
