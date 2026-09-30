#!/usr/bin/env python3
"""Pending cruise preserves consecutive-loss handling in both transports."""
import asyncio
import math
import os
import sys
import unittest
from collections import deque
from types import SimpleNamespace
from typing import Optional, Tuple
from unittest.mock import AsyncMock, Mock, call, patch

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies import webrtc_mode, websockets_mode  # noqa: E402
from selkies.stream_server import CongestionSteer  # noqa: E402


def draining() -> Tuple[CongestionSteer, float]:
    """Reach a pending cruise through an ordinary measured-queue tick."""
    steer = CongestionSteer()
    current = steer.target(6000, 6000, 1000, 5_000_000, 0.0, 0.0,
                           queue_s=0.1, offered_bps=6_000_000)
    return steer, current


def tick(steer: CongestionSteer, current: float, loss: float, now: float = 1.0,
         queue: Optional[float] = 0.0, floor: float = 1000,
         ceiling: float = 6000) -> float:
    """Provide the next receiver observation with unchanged capacity and limits."""
    return steer.target(current, ceiling, floor, 5_000_000, loss, now,
                        queue_s=queue, offered_bps=6_000_000)


class CruiseLossTests(unittest.TestCase):
    """Loss holds or backs off; a clean restoration breaks the loss streak."""

    def test_first_loss_does_not_raise(self) -> None:
        steer, current = draining()
        self.assertEqual(current, 3750)
        self.assertEqual(tick(steer, current, 0.2), current)
        self.assertEqual(steer.strikes, 1)

    def test_second_loss_backs_off_drain_target(self) -> None:
        steer, current = draining()
        current = tick(steer, current, 0.2)
        self.assertEqual(tick(steer, current, 0.2, 2.0), 2625)
        self.assertEqual(steer.strikes, 0)
        self.assertIsNone(steer._cruise_kbps)

    def test_clean_restore_separates_losses(self) -> None:
        steer, current = draining()
        current = tick(steer, current, 0.2)
        current = tick(steer, current, 0.0, 2.0)
        self.assertEqual(current, 4250)
        self.assertEqual(steer.strikes, 0)
        self.assertEqual(tick(steer, current, 0.2, 3.0), current)
        self.assertEqual(steer.strikes, 1)

    def test_clean_restore_clears_loss_before_drain_deadline(self) -> None:
        steer, current = draining()
        current = tick(steer, current, 0.2, 0.2)
        self.assertEqual(steer.strikes, 1)
        current = tick(steer, current, 0.0)
        self.assertEqual(current, 4250)
        self.assertEqual(steer.strikes, 0)
        self.assertEqual(tick(steer, current, 0.2, 2.0), current)

    def test_threshold_equality_restores(self) -> None:
        steer, current = draining()
        self.assertEqual(tick(steer, current, 0.1), 4250)
        self.assertIsNone(steer._cruise_kbps)

    def test_next_float_above_threshold_holds(self) -> None:
        steer, current = draining()
        self.assertEqual(tick(steer, current, math.nextafter(0.1, 1.0)), current)

    def test_unknown_queue_preserves_loss_handling(self) -> None:
        steer, current = draining()
        self.assertEqual(tick(steer, current, 0.2, queue=None), current)
        self.assertEqual(steer.strikes, 1)

    def test_changed_bounds_apply_during_loss(self) -> None:
        for floor, ceiling, expected in ((4000, 6000, 4000), (1000, 3000, 3000)):
            with self.subTest(floor=floor, ceiling=ceiling):
                steer, current = draining()
                self.assertEqual(tick(steer, current, 0.2, floor=floor,
                                      ceiling=ceiling), expected)

    def test_clean_restore_and_hold_remain(self) -> None:
        steer, current = draining()
        current = tick(steer, current, 0.0, 0.5)
        self.assertEqual(current, 4250)
        self.assertEqual(tick(steer, current, 0.0, 0.6), current)

    def test_application_limited_queue_does_not_cut(self) -> None:
        steer = CongestionSteer()
        self.assertEqual(steer.target(6000, 6000, 1000, 1_000_000, 0.0, 1.0,
                                      queue_s=0.1, offered_bps=1_000_000), 6000)


class TransportCallerTests(unittest.IsolatedAsyncioTestCase):
    """Exercise actual bitrate setters from the transports' normal feedback paths."""

    async def test_webrtc_feedback_preserves_loss_sequences(self) -> None:
        for losses, expected, strikes in (((0.2, 0.2), [2625], 0),
                                         ((0.2, 0.0, 0.2), [4250], 1)):
            with self.subTest(losses=losses):
                steer, current = draining()
                pipeline = SimpleNamespace(rc_mode=webrtc_mode.RateControlMode.CBR,
                                           video_bitrate=current)

                async def set_bitrate(value: int, target: SimpleNamespace = pipeline) -> None:
                    target.video_bitrate = value

                pipeline.set_video_bitrate = AsyncMock(side_effect=set_bitrate)
                service = webrtc_mode.WebRTCService.__new__(webrtc_mode.WebRTCService)
                service.metrics = None
                service.args = SimpleNamespace(congestion_control=True)
                service._congestion_steer = {"primary": steer}
                service._display_setting = lambda did, key: 6000
                service.display_pipelines = {"primary": pipeline}
                service._ensure_pacer = Mock()
                windows = [dict(goodput_bps=5_000_000, sent_bps=6_000_000,
                                loss_fraction=loss, queue_ms=0.0,
                                queue_rising_ms=0.0, queue_depth_ms=0.0)
                           for loss in losses]
                transport = SimpleNamespace(take_twcc_window=Mock(side_effect=windows))
                service.rtc_app = SimpleNamespace(peer_connections={"peer": dict(
                    peer_conn=SimpleNamespace(sctp=SimpleNamespace(transport=transport)),
                    display_id="primary", video_sender=None)})
                sleep = AsyncMock(side_effect=[None] * len(losses) + [asyncio.CancelledError()])
                clock = Mock(side_effect=range(1, len(losses) + 1))
                with patch.object(webrtc_mode, "asyncio", SimpleNamespace(sleep=sleep)), \
                        patch.object(webrtc_mode, "time", SimpleNamespace(monotonic=clock)), \
                        patch.object(webrtc_mode, "settings", SimpleNamespace(
                            video_bitrate=(1000, 12000), webrtc_pacer=(True,))):
                    with self.assertRaises(asyncio.CancelledError):
                        await service._congestion_control_loop()
                self.assertEqual(pipeline.set_video_bitrate.await_args_list,
                                 [call(value) for value in expected])
                self.assertEqual(steer.strikes, strikes)
                self.assertEqual(service._ensure_pacer.call_count, len(losses))

    async def test_websocket_clean_feedback_restores_cruise(self) -> None:
        steer, current = draining()
        module = SimpleNamespace(update_video_bitrate=Mock())
        server = websockets_mode.DataStreamingServer.__new__(websockets_mode.DataStreamingServer)
        server.capture_instances = {"primary": {"module": module}}
        server._initial_video_bitrate = 6000
        acks = deque((0.5 + i * .05, 20.0, i * .05 * 5_000_000 / 8)
                     for i in range(1, 11))
        state = dict(video_bitrate=6000, link_kbps=current, link_tick_at=0.5,
                     backpressure_enabled=True, rtt_floor_ms=20, link_acks=acks,
                     link_jitter_ms=0, link_steer=steer,
                     sent_bytes=375_000, link_tick_sent=0)
        with patch.object(websockets_mode, "app_settings", SimpleNamespace(video_bitrate=(1000, 12000))):
            server._steer_bitrate_to_link("primary", state, 1.0)
        self.assertEqual(state["link_kbps"], 4250)
        module.update_video_bitrate.assert_called_once_with(4250)


if __name__ == "__main__":
    unittest.main()
