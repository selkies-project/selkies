# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Scene capture admission, serialization, cancellation, and exact native provenance."""

import asyncio
import os
import struct
import sys
import threading
import unittest
from dataclasses import replace
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0, os.path.join(os.path.dirname(__file__), '..', '..', 'src'))
from selkies.static_refinement import (  # noqa: E402
    RefinementUnavailable, SceneSample, StaticRefinement,
)


STAMP = SceneSample(1, 2, 3, 4, 2, 1)
PNG = b'\x89PNG\r\n\x1a\nfixture'


class NativeCapture:
    """A synchronous capture whose completion can straddle a scene transition."""

    def __init__(self):
        self.scene_tracking_supported = True
        self.scene_tracking_enabled = False
        self.calls = 0
        self.transitions = []
        self.stamp = STAMP
        self.entered = threading.Event()
        self.release = threading.Event()
        self.release.set()
        self.error = None

    def set_scene_tracking(self, enabled):
        self.scene_tracking_enabled = enabled
        self.transitions.append(enabled)

    def snapshot_png(self, run, timeout):
        """Freeze identity before waiting, as the native raw-snapshot queue does."""
        self.calls += 1
        stamp = self.stamp
        self.entered.set()
        if not self.release.wait(2):
            raise RuntimeError('Fixture did not release capture')
        if self.error:
            raise RuntimeError(self.error)
        return dict(run_id=stamp.run, source_id=stamp.source, scene_id=stamp.scene,
                    sample_seq=stamp.sample + 1, width=stamp.width, height=stamp.height,
                    png=PNG, preserved_rgb_bits=8)


class RefinementTests(unittest.IsolatedAsyncioTestCase):
    """Drive races through the asynchronous controller with bounded thread waits."""

    def setUp(self):
        self.native = NativeCapture()
        self.controller = StaticRefinement()
        self.quiet = patch('selkies.static_refinement.QUIET_SECONDS', 0)
        self.quiet.start()

    def tearDown(self):
        self.native.release.set()
        self.quiet.stop()

    def activate(self):
        self.controller.configure(self.native, True, True, True, True)
        self.controller.observe(STAMP)
        return self.controller.epoch

    async def test_live_resize_uses_each_native_payload_geometry(self):
        self.controller.configure(self.native, True, True, True, True)
        native = SimpleNamespace(sample_run_id=1, source_id=2, scene_id=3, sample_seq=4)

        def delivered(width, height):
            payload = memoryview(struct.pack('!BBHHHHH', 4, 0, 7, 0, width, height, 6))
            return SceneSample.from_frame(native, payload)

        first = delivered(1280, 720)
        self.controller.observe(first)
        first_epoch = self.controller.epoch
        self.assertTrue(self.controller.valid(first_epoch, first))
        native.source_id, native.scene_id, native.sample_seq = 3, 4, 5
        resized = delivered(640, 360)
        self.assertEqual((resized.width, resized.height), (640, 360))
        self.assertTrue(self.controller.observe(resized))
        self.assertGreater(self.controller.epoch, first_epoch)
        self.assertFalse(self.controller.valid(first_epoch, first))
        self.assertTrue(self.controller.valid(self.controller.epoch, resized))
        self.native.stamp = resized
        image = await self.controller.capture(self.controller.epoch, resized)
        self.assertEqual(image.stamp.key, resized.key)
        self.assertEqual(self.native.transitions, [True])

    async def test_native_payload_requires_full_frame_header_geometry_and_identity(self):
        native = SimpleNamespace(sample_run_id=1, source_id=2, scene_id=3, sample_seq=4)
        for data_type, row, width, height in ((3, 0, 2, 1), (4, 1, 2, 1),
                                             (4, 0, 0, 1), (4, 0, 2, 0)):
            payload = memoryview(struct.pack('!BBHHHHH', data_type, 0, 7, row, width, height, 6))
            self.assertIsNone(SceneSample.from_frame(native, payload))
        payload = memoryview(struct.pack('!BBHHHHH', 4, 0, 7, 0, 2, 1, 6))
        self.assertIsNone(SceneSample.from_frame(native, payload[:11]))
        for name in ('sample_run_id', 'source_id', 'scene_id', 'sample_seq'):
            original = getattr(native, name)
            setattr(native, name, None)
            self.assertIsNone(SceneSample.from_frame(native, payload))
            setattr(native, name, original)

    async def test_default_off_and_parent_disabled_do_no_work(self):
        for requested, parent, consumer, full in (
                (False, True, True, True), (True, False, True, True),
                (True, True, False, True), (True, True, True, False)):
            self.controller.configure(self.native, requested, parent, consumer, full)
            self.controller.observe(STAMP)
            with self.assertRaises(RefinementUnavailable):
                await self.controller.capture(self.controller.epoch, STAMP)
        self.assertEqual(self.native.calls, 0)
        self.assertEqual(self.native.transitions, [])
        self.assertIsNone(self.controller._capture)

    async def test_consumers_share_one_capture_and_future_sample_cache(self):
        epoch = self.activate()
        images = await asyncio.gather(*(self.controller.capture(epoch, STAMP) for _ in range(20)))
        self.assertEqual(self.native.calls, 1)
        self.assertTrue(all(image is images[0] for image in images))
        self.assertEqual(images[0].stamp.sample, STAMP.sample + 1)
        self.assertIs(await self.controller.capture(epoch, STAMP), images[0])

    async def test_new_scene_waits_past_stale_capture_then_runs_once(self):
        epoch = self.activate()
        self.native.release.clear()
        old = asyncio.create_task(self.controller.capture(epoch, STAMP))
        self.assertTrue(await asyncio.to_thread(self.native.entered.wait, 1))
        new = replace(STAMP, scene=4, sample=5)
        self.native.stamp = new
        self.controller.observe(new)
        newer = [asyncio.create_task(self.controller.capture(epoch, new)) for _ in range(3)]
        await asyncio.sleep(0)
        self.native.release.set()
        outcomes = await asyncio.gather(old, *newer, return_exceptions=True)
        self.assertIsInstance(outcomes[0], RefinementUnavailable)
        results = outcomes[1:]
        self.assertEqual(self.native.calls, 2)
        self.assertEqual(results[0].stamp.scene, new.scene)
        self.assertTrue(all(image is results[0] for image in results))

    async def test_off_during_compression_discards_late_result(self):
        epoch = self.activate()
        self.native.release.clear()
        pending = asyncio.create_task(self.controller.capture(epoch, STAMP))
        self.assertTrue(await asyncio.to_thread(self.native.entered.wait, 1))
        self.controller.configure(self.native, False, True, True, True)
        self.native.release.set()
        with self.assertRaises(RefinementUnavailable):
            await pending
        self.assertIsNone(self.controller.image)
        self.assertEqual(self.native.transitions, [True, False])

    async def test_queued_cancel_prevents_native_work_and_allows_fresh_request(self):
        epoch = self.activate()
        admitted = False
        with self.assertRaises(RefinementUnavailable):
            await self.controller.capture(epoch, STAMP, lambda: admitted)
        self.assertEqual(self.native.calls, 0)
        admitted = True
        await self.controller.capture(epoch, STAMP, lambda: admitted)
        self.assertEqual(self.native.calls, 1)

    async def test_client_cancellation_does_not_cancel_shared_native_thread(self):
        epoch = self.activate()
        self.native.release.clear()
        first = asyncio.create_task(self.controller.capture(epoch, STAMP))
        self.assertTrue(await asyncio.to_thread(self.native.entered.wait, 1))
        other = asyncio.create_task(self.controller.capture(epoch, STAMP))
        await asyncio.sleep(0)
        first.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await first
        self.native.release.set()
        self.assertEqual((await other).png, PNG)
        self.assertEqual(self.native.calls, 1)

    async def test_wrong_scene_and_failed_capture_are_not_retried_in_loop(self):
        epoch = self.activate()
        self.native.stamp = replace(STAMP, source=99)
        for _ in range(3):
            with self.assertRaises(RefinementUnavailable):
                await self.controller.capture(epoch, STAMP)
        self.assertEqual(self.native.calls, 1)
        self.assertIsNone(self.controller.image)

    async def test_quiet_gate_rechecks_scene_before_native_admission(self):
        epoch = self.activate()
        with patch('selkies.static_refinement.QUIET_SECONDS', 0.02):
            pending = asyncio.create_task(self.controller.capture(epoch, STAMP))
            await asyncio.sleep(0)
            self.controller.observe(replace(STAMP, scene=4, sample=5))
            with self.assertRaises(RefinementUnavailable):
                await pending
        self.assertEqual(self.native.calls, 0)

    def test_source_change_advances_epoch_and_old_samples_cannot_restore_it(self):
        epoch = self.activate()
        self.assertTrue(self.controller.observe(replace(STAMP, source=3, scene=4, sample=9)))
        self.assertGreater(self.controller.epoch, epoch)
        self.controller.observe(None)
        self.controller.observe(STAMP)
        self.assertIsNone(self.controller.latest)

    def test_realized_capability_demotion_and_recovery_require_new_epoch(self):
        epoch = self.activate()
        self.native.scene_tracking_enabled = False
        self.assertTrue(self.controller.configure(self.native, True, True, True, True))
        self.assertGreater(self.controller.epoch, epoch)
        self.assertEqual(self.native.transitions, [True, True])

    def test_replacement_display_controller_never_reuses_connection_epoch(self):
        self.activate()
        self.controller.close()
        last = self.controller.epoch
        replacement = StaticRefinement()
        replacement.configure(self.native, True, True, True, True)
        self.assertGreater(replacement.epoch, last)

    def test_wire_ids_are_exact_u64_strings_and_geometry_is_bounded(self):
        wire = replace(STAMP, run=(1 << 64) - 1).wire()
        self.assertEqual(SceneSample.parse(wire).run, (1 << 64) - 1)
        for value in (1, True, '01', '0', '-1', '18446744073709551616', '１'):
            with self.assertRaises(ValueError):
                SceneSample.parse(dict(wire, run=value))
        for value in (True, 0, 65536, 1.0):
            with self.assertRaises(ValueError):
                SceneSample.parse(dict(wire, width=value))


if __name__ == '__main__':
    unittest.main()
