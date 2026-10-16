#!/usr/bin/env python3
"""A still screen's cleanup runs at what the path carries, over WebRTC as over WebSockets.

`LinkAllowance` keeps 0.9 of the least a path showed over its last twenty readings, the
ceiling for a path too fast to tell and 0 before a reading, and says so only when it moves.
Each WebRTC peer's pacer reads its path from transport-cc (what frames' leading bursts
arrived at, typically) within 0.8 of its pace; the congestion tick tells the encoder the
lowest of a display's peers, lets the display's video budget run to it, and has every
peer's pacer take a cleanup frame at it into its video queue whole.
"""
import asyncio
import os
import sys
import unittest
from types import SimpleNamespace
from unittest.mock import AsyncMock, Mock, patch

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies import webrtc_mode  # noqa: E402
from selkies.stream_server import LinkAllowance  # noqa: E402
from selkies.webrtc.pacer import CLEANUP_PACE_SHARE, LINK_MAX_AGE_S, LINK_SAMPLES, RtpPacer  # noqa: E402


class LinkAllowanceTests(unittest.TestCase):
    def test_the_least_reading_of_the_window_sets_it(self) -> None:
        allowance = LinkAllowance()
        self.assertEqual(allowance.note(None, 12000), 0, "no reading yet")
        self.assertIsNone(allowance.note(None, 12000), "said once")
        self.assertEqual(allowance.note(2_000_000, 12000), 1800)
        self.assertEqual(allowance.note(1_000_000, 12000), 900)
        self.assertIsNone(allowance.note(3_000_000, 12000), "the least stands")
        for _ in range(LinkAllowance.TICKS - 1):
            allowance.note(3_000_000, 12000)
        self.assertEqual(allowance.kbps, 2700, "a reading out of the window no longer counts")

    def test_a_path_too_fast_to_tell_takes_the_ceiling(self) -> None:
        self.assertEqual(LinkAllowance().note(float("inf"), 12000), 12000)


class PacerRoomTests(unittest.IsolatedAsyncioTestCase):
    async def test_the_room_is_the_typical_link_within_the_pace(self) -> None:
        async def send(data: bytes) -> None:
            pass

        pacer = RtpPacer(1_000_000, send)
        try:
            self.assertIsNone(pacer.room_bps(), "nothing measured")
            for _ in range(LINK_SAMPLES):
                pacer.set_link_bps(1_500_000)
            self.assertEqual(pacer.room_bps(), 1_500_000)
            for _ in range(LINK_SAMPLES):
                pacer.set_link_bps(10_000_000)
            self.assertEqual(pacer.room_bps(), CLEANUP_PACE_SHARE * pacer._pace_bps, "within the pace")
            pacer._link_at -= LINK_MAX_AGE_S + 1
            self.assertIsNone(pacer.room_bps(), "a stale reading")
            pacer.set_link_bps(0.0)
            self.assertEqual(pacer.room_bps(), 0.0, "the wire lost packets")
        finally:
            await pacer.close()

    async def test_the_video_queue_takes_a_cleanup_frame_whole(self) -> None:
        async def send(data: bytes) -> None:
            pass

        pacer = RtpPacer(1_000_000, send)
        try:
            cap = pacer.video_room()
            pacer.set_frame_floor(3 * cap)
            self.assertEqual(pacer.video_room(), 3 * cap)
            pacer.set_frame_floor(0)
            self.assertEqual(pacer.video_room(), cap)
        finally:
            await pacer.close()


class CongestionTickTests(unittest.IsolatedAsyncioTestCase):
    async def tick(self, rc_mode, rooms, steer=None, before=None):
        links, budgets, floors = [], [], []
        pipeline = SimpleNamespace(rc_mode=rc_mode, video_bitrate=500, set_video_bitrate=AsyncMock(),
                                   set_link_rate=links.append)
        service = webrtc_mode.WebRTCService.__new__(webrtc_mode.WebRTCService)
        service.metrics = None
        service.args = SimpleNamespace(congestion_control=True)
        service._congestion_steer, service._rate_holds, service._link_allowances = {}, {}, {}
        if steer is not None:
            service._congestion_steer["primary"] = steer
        if before is not None:
            service._link_allowances["primary"] = before
        service._display_setting = lambda did, key: 500
        service.display_pipelines = {"primary": pipeline}
        service._ensure_pacer = Mock()
        service._display_owner_tabs, service._peer_tabs = {}, {}
        window = dict(goodput_bps=400_000, sent_bps=400_000, loss_fraction=0.0, queue_ms=None,
                      queue_rising_ms=None, queue_depth_ms=0.0)

        def transport(room):
            return SimpleNamespace(take_twcc_window=lambda: dict(window), pacer_room_bps=lambda: room,
                                   set_pacer_frame_floor=floors.append)

        service.rtc_app = SimpleNamespace(
            peer_connections={f"peer{i}": dict(peer_conn=SimpleNamespace(sctp=SimpleNamespace(transport=transport(r))),
                                               display_id="primary", video_sender=None)
                              for i, r in enumerate(rooms)},
            send_cc_rate=lambda did, kbps: None,
            set_video_budget=lambda did, bps: budgets.append(bps))
        sleep = AsyncMock(side_effect=[None, asyncio.CancelledError()])
        with patch.object(webrtc_mode, "asyncio", SimpleNamespace(sleep=sleep)), \
                patch.object(webrtc_mode, "time", SimpleNamespace(monotonic=lambda: 10.0)), \
                patch.object(webrtc_mode, "settings", SimpleNamespace(
                    video_bitrate=(100, 12000), webrtc_pacer=(True,))):
            with self.assertRaises(asyncio.CancelledError):
                await service._congestion_control_loop()
        return links, budgets, floors

    async def test_the_tick_tells_the_encoder_and_makes_room(self) -> None:
        links, budgets, floors = await self.tick(webrtc_mode.RateControlMode.CBR, (2_000_000, 1_200_000))
        self.assertEqual(links, [1080], "0.9 of the slower peer's path")
        self.assertEqual(budgets, [1080 * 1000], "the video budget runs to it")
        frame = int(1080 * 1000 / 8 * webrtc_mode.CLEANUP_FRAME_S * webrtc_mode.CLEANUP_FRAME_OVERHEAD)
        self.assertEqual(floors, [frame, frame], "every peer's pacer takes a cleanup frame whole")

    async def test_an_unmeasured_path_leaves_the_session_rate(self) -> None:
        links, budgets, floors = await self.tick(webrtc_mode.RateControlMode.CBR, (None,))
        self.assertEqual(links, [0])
        self.assertEqual(budgets, [500 * 1000])
        self.assertEqual(floors, [0])

    async def test_a_target_backed_off_reads_as_no_room(self) -> None:
        steer = SimpleNamespace(target=lambda *a, **k: 300.0)
        links, budgets, floors = await self.tick(webrtc_mode.RateControlMode.CBR, (2_000_000,), steer)
        self.assertEqual((links, budgets, floors), ([0], [300 * 1000], [0]))

    async def test_a_target_backed_off_ends_an_allowance_with_no_reading(self) -> None:
        before = LinkAllowance()
        before.note(2_000_000, 12000)
        steer = SimpleNamespace(target=lambda *a, **k: 300.0)
        links, budgets, floors = await self.tick(webrtc_mode.RateControlMode.CBR, (None,), steer, before)
        self.assertEqual((links, budgets, floors), ([0], [300 * 1000], [0]))

    async def test_a_constant_quality_leaves_it_alone(self) -> None:
        links, budgets, floors = await self.tick(webrtc_mode.RateControlMode.CRF, (2_000_000,))
        self.assertEqual((links, budgets, floors), ([], [None], []))


if __name__ == "__main__":
    unittest.main()
