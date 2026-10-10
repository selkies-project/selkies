# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""Exercise scene-bound transport methods without native capture dependencies.

AST extraction compiles the production methods unchanged; only socket writes,
network pacing, and native capture are replaced. This tests protocol flow and
admission, not browser presentation, real congestion, or a compositor's pixels.
"""

import ast
import asyncio
import json
import os
import struct
import sys
import time
import unittest
from collections import deque
from dataclasses import replace
from pathlib import Path
from types import SimpleNamespace
from typing import Any, Optional, Set, Tuple
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, os.fspath(ROOT / 'src'))
from selkies.static_refinement import (  # noqa: E402
    PROTOCOL_VERSION, RefinedImage, RefinementUnavailable, SceneSample, StaticRefinement,
)


STAMP = SceneSample(11, 12, 13, 14, 256, 128)
PNG = b'\x89PNG\r\n\x1a\n' + bytes(range(256)) * 130


def methods(filename, class_name, names, namespace):
    """Compile the named real methods, retaining their annotations and bodies."""
    path = ROOT / 'src' / 'selkies' / filename
    tree = ast.parse(path.read_text(encoding='utf-8'), filename=os.fspath(path))
    source = next(node for node in tree.body
                  if isinstance(node, ast.ClassDef) and node.name == class_name)
    selected = [node for node in source.body
                if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and node.name in names]
    if {node.name for node in selected} != set(names):
        raise RuntimeError('Transport method selection is incomplete')
    for node in selected:
        exec(compile(ast.Module(body=[node], type_ignores=[]), os.fspath(path), 'exec'), namespace)
    return type(class_name, (), {name: namespace[name] for name in names})


class Socket:
    """A bounded fixture socket with a hook at every completed write."""

    def __init__(self):
        self.sent = []
        self.on_send = None


class Native:
    """Return one scene-bound native result while counting all admitted work."""

    def __init__(self):
        self.scene_tracking_supported = True
        self.scene_tracking_enabled = False
        self.transitions = []
        self.calls = 0
        self.stamp = STAMP

    def set_scene_tracking(self, enabled):
        self.scene_tracking_enabled = enabled
        self.transitions.append(enabled)

    def snapshot_png(self, run, timeout):
        self.calls += 1
        if run != self.stamp.run or timeout <= 0:
            raise RuntimeError('Unexpected native admission')
        return dict(run_id=self.stamp.run, source_id=self.stamp.source,
                    scene_id=self.stamp.scene, sample_seq=self.stamp.sample,
                    width=self.stamp.width, height=self.stamp.height,
                    preserved_rgb_bits=8, png=PNG)


async def send_live(socket, data, what):
    socket.sent.append(data)
    if socket.on_send is not None:
        await socket.on_send(data)


async def no_wait(*args):
    return None


NAMESPACE = dict(Any=Any, Optional=Optional, Set=Set, Tuple=Tuple, asyncio=asyncio, json=json,
                 time=time, struct=struct, StaticRefinement=StaticRefinement,
                 SceneSample=SceneSample, RefinementUnavailable=RefinementUnavailable,
                 PROTOCOL_VERSION=PROTOCOL_VERSION, _send_live=send_live,
                 socket_gauge=lambda socket: socket,
                 TransferPacer=lambda **kwargs: object(), _bulk_pace=no_wait,
                 _await_bulk_window=no_wait, BULK_DRAIN_TIMEOUT_S=1)
Server = methods('websockets_mode.py', 'DataStreamingServer', (
    '_refinement_display', '_refinement_consumers', '_refinement_status',
    '_publish_refinement_status', '_refresh_refinement', '_handle_refinement',
    '_send_refinement'), NAMESPACE)
RTC = methods('webrtc_mode.py', 'WebRTCService', ('_answer_refinement_capability',),
              dict(Any=Any, Optional=Optional, json=json))
Relay = methods('websockets_mode.py', '_VideoRelay', ('_send_picture', '_run'), NAMESPACE)


class TransportTests(unittest.IsolatedAsyncioTestCase):
    """Drive authenticated display binding, cancellation, and wire framing."""

    async def asyncSetUp(self):
        self.server = Server()
        self.socket = Socket()
        self.native = Native()
        self.server.clients = {self.socket}
        self.server.video_paused_clients = set()
        self.server.display_clients = {'primary': dict(ws=self.socket, encoder='x264enc',
                                                      use_paint_over_quality=True,
                                                      lossless_static_refinement=True)}
        self.server.co_controllers = {}
        self.server._refinements = {}
        self.server._refinement_clients = {}
        self.server._refinement_tokens = {}
        self.server._refinement_pending = {}
        self.server._refinement_transfers = {}
        self.server._refinement_transfer_id = 0
        self.server._initial_lossless_static_refinement = False
        self.server._initial_use_paint_over_quality = False
        self.server.capture_instances = {'primary': {'module': self.native}}
        self.server._stream_watches = {'primary': SimpleNamespace(info={'striped': False})}
        self.server.app = SimpleNamespace(encoder='x264enc')
        self.idr = []
        self.server._schedule_idr_for_display = self.idr.append
        self.quiet = patch('selkies.static_refinement.QUIET_SECONDS', 0)
        self.quiet.start()

    async def asyncTearDown(self):
        await self.drain()
        self.quiet.stop()
        NAMESPACE['_bulk_pace'] = no_wait
        NAMESPACE['_await_bulk_window'] = no_wait

    async def drain(self):
        async def finish():
            while self.server._refinement_transfers:
                await asyncio.gather(*tuple(self.server._refinement_transfers.values()))
        await asyncio.wait_for(finish(), timeout=2)

    async def capability(self, socket=None, supported=True, sink='worker-canvas'):
        socket = socket or self.socket
        await self.server._handle_refinement(socket, json.dumps(
            dict(version=1, op='capability', supported=supported, sink=sink)))

    async def activate(self):
        await self.capability()
        controller = self.server._refinements['primary']
        controller.observe(STAMP)
        self.socket.sent.clear()
        return controller

    async def request(self, controller, socket=None, stamp=STAMP, **extras):
        payload = dict(stamp.wire(), version=1, op='request', epoch=controller.epoch)
        payload.update(extras)
        await self.server._handle_refinement(socket or self.socket, json.dumps(payload))

    def messages(self, kind):
        return [value for raw in self.socket.sent if isinstance(raw, str)
                for value in (json.loads(raw),) if value['type'] == kind]

    async def test_absent_preference_and_disabled_parent_admit_no_capture(self):
        self.server.display_clients['primary']['lossless_static_refinement'] = False
        await self.capability()
        controller = self.server._refinements['primary']
        controller.observe(STAMP)
        await self.request(controller)
        self.assertEqual(self.native.calls, 0)
        self.assertEqual(self.native.transitions, [])
        self.assertEqual(self.messages('lossless_status')[-1]['reason'], 'disabled')
        self.server.display_clients['primary'].update(
            lossless_static_refinement=True, use_paint_over_quality=False)
        await self.server._refresh_refinement('primary')
        await self.request(controller)
        self.assertFalse(controller.effective)
        self.assertEqual(self.native.transitions, [])
        self.assertEqual(self.native.calls, 0)

    async def test_unauthenticated_or_malformed_negotiation_does_not_allocate(self):
        stranger = Socket()
        await self.capability(stranger)
        for payload in ('{}', '[]', 'invalid', 'x' * 2049,
                        '{"version":2,"op":"capability","supported":true}',
                        '{"version":true,"op":"capability","supported":true}',
                        '{"version":1.0,"op":"capability","supported":true}'):
            await self.server._handle_refinement(self.socket, payload)
        self.assertFalse(self.server._refinement_clients)
        self.assertFalse(self.server._refinements)
        self.assertEqual(self.native.transitions, [])

    async def test_incompatible_sink_and_striped_stream_never_enable_native(self):
        await self.capability(sink='webrtc-video')
        self.assertEqual(self.messages('lossless_status')[-1]['reason'], 'canvas-required')
        self.server._stream_watches['primary'].info = {'striped': True}
        await self.capability()
        status = self.messages('lossless_status')[-1]
        self.assertFalse(status['supported'])
        self.assertFalse(status['effective'])
        self.assertEqual(status['reason'], 'full-frame-required')
        self.assertEqual(self.native.transitions, [])

    async def test_page_track_requires_explicit_capability_and_preserves_parent_gate(self):
        await self.capability(supported=False, sink='track-generator')
        self.assertEqual(self.native.transitions, [])
        await self.capability(sink='track-generator')
        controller = self.server._refinements['primary']
        controller.observe(STAMP)
        await self.request(controller)
        await self.drain()
        self.assertEqual(self.native.calls, 1)
        self.assertEqual(len(self.messages('lossless_end')), 1)
        self.server.display_clients['primary']['use_paint_over_quality'] = False
        await self.server._refresh_refinement('primary')
        await self.request(controller)
        await self.drain()
        self.assertEqual(self.native.calls, 1)
        self.assertFalse(controller.effective)

    async def test_settings_and_capability_cannot_reenable_a_stopping_capture(self):
        controller = await self.activate()
        old_epoch = controller.epoch
        self.server.capture_instances['primary']['refinement_stopping'] = True
        await self.server._refresh_refinement('primary')
        await self.capability()
        self.server.display_clients['primary']['lossless_static_refinement'] = True
        await self.server._refresh_refinement('primary')
        await self.request(controller, epoch=old_epoch)
        await self.drain()
        self.assertFalse(controller.effective)
        self.assertFalse(controller.supported)
        self.assertEqual(controller.reason, 'capture-unavailable')
        self.assertGreater(controller.epoch, old_epoch)
        self.assertEqual(self.native.transitions, [True, False])
        self.assertEqual(self.native.calls, 0)

    async def test_status_publication_survives_retiring_display_before_send_runs(self):
        controller = await self.activate()
        published_epoch = controller.epoch
        task = asyncio.create_task(self.server._publish_refinement_status('primary'))
        await asyncio.sleep(0)
        controller.close()
        self.server._refinements.pop('primary')
        await asyncio.wait_for(task, 1)
        status = self.messages('lossless_status')[-1]
        self.assertEqual(status['epoch'], published_epoch)
        self.assertTrue(status['effective'])
        self.assertGreater(controller.epoch, published_epoch)

    async def test_request_is_bound_to_receiving_display_not_payload_display(self):
        primary = await self.activate()
        secondary = Socket()
        other = Native()
        other.stamp = replace(STAMP, run=30, source=31, scene=32)
        self.server.clients.add(secondary)
        self.server.display_clients['secondary'] = dict(
            ws=secondary, encoder='x264enc', use_paint_over_quality=True,
            lossless_static_refinement=True)
        self.server.capture_instances['secondary'] = {'module': other}
        self.server._stream_watches['secondary'] = SimpleNamespace(info={'striped': False})
        await self.capability(secondary)
        controller = self.server._refinements['secondary']
        controller.observe(other.stamp)
        await self.request(primary, secondary, displayId='primary')
        await self.drain()
        self.assertEqual((self.native.calls, other.calls), (0, 0))
        await self.request(controller, secondary, other.stamp, displayId='primary')
        await self.drain()
        self.assertEqual((self.native.calls, other.calls), (0, 1))

    async def test_png_framing_roundtrips_exact_payload_and_u64_identity(self):
        controller = await self.activate()
        exact = replace(STAMP, run=(1 << 64) - 1, sample=(1 << 64) - 2)
        self.native.stamp = exact
        controller.observe(exact)
        await self.request(controller, stamp=exact)
        await self.drain()
        begin = self.messages('lossless_begin')[0]
        end = self.messages('lossless_end')[0]
        binary = [raw for raw in self.socket.sent if isinstance(raw, bytes)]
        recovered = bytearray()
        for raw in binary:
            opcode, transfer, offset = struct.unpack('!BII', raw[:9])
            self.assertEqual((opcode, transfer, offset), (10, begin['transferId'], len(recovered)))
            self.assertLessEqual(len(raw), 16 * 1024 + 9)
            recovered.extend(raw[9:])
        self.assertEqual(bytes(recovered), PNG)
        self.assertEqual(begin['bytes'], len(PNG))
        self.assertEqual(begin['transferId'], end['transferId'])
        self.assertEqual(begin['run'], str(exact.run))
        self.assertEqual(begin['sample'], str(exact.sample))
        self.assertEqual(self.native.calls, 1)
        self.assertFalse(self.server._refinement_tokens)
        self.assertFalse(self.server._refinement_pending)

    async def test_cancel_during_pacing_prevents_first_binary_write(self):
        controller = await self.activate()

        async def cancel(*args):
            await self.server._handle_refinement(self.socket, json.dumps(
                dict(STAMP.wire(), version=1, op='cancel', epoch=controller.epoch)))

        NAMESPACE['_bulk_pace'] = cancel
        await self.request(controller)
        await self.drain()
        self.assertEqual(len(self.messages('lossless_begin')), 1)
        self.assertFalse(any(isinstance(raw, bytes) for raw in self.socket.sent))
        self.assertFalse(self.messages('lossless_end'))
        self.assertFalse(self.server._refinement_tokens)

    async def test_scene_change_after_first_chunk_stops_old_transfer(self):
        controller = await self.activate()

        async def change(raw):
            if isinstance(raw, bytes):
                controller.observe(replace(STAMP, scene=20, sample=21))

        self.socket.on_send = change
        await self.request(controller)
        await self.drain()
        self.assertEqual(sum(isinstance(raw, bytes) for raw in self.socket.sent), 1)
        self.assertFalse(self.messages('lossless_end'))

    async def test_coalesced_latest_request_reuses_one_native_capture(self):
        controller = await self.activate()
        blocked, release = asyncio.Event(), asyncio.Event()

        async def block_once(*args):
            if not blocked.is_set():
                blocked.set()
                await release.wait()

        NAMESPACE['_bulk_pace'] = block_once
        await self.request(controller)
        await asyncio.wait_for(blocked.wait(), 1)
        old = self.server._refinement_transfers[self.socket]
        for _ in range(5):
            await self.request(controller)
            self.assertIs(self.server._refinement_transfers[self.socket], old)
        self.assertEqual(len(self.server._refinement_pending), 1)
        release.set()
        await self.drain()
        self.assertEqual(self.native.calls, 1)
        self.assertEqual(len(self.messages('lossless_begin')), 2)
        self.assertEqual(len(self.messages('lossless_end')), 1)
        self.assertFalse(self.server._refinement_pending)

    async def test_disconnected_or_paused_consumer_cannot_capture(self):
        controller = await self.activate()
        self.server.video_paused_clients.add(self.socket)
        await self.request(controller)
        self.assertFalse(self.server._refinement_status('primary', self.socket)['effective'])
        self.server.clients.remove(self.socket)
        self.server.video_paused_clients.clear()
        await self.request(controller)
        await self.drain()
        self.assertEqual(self.native.calls, 0)

    async def test_one_client_cannot_disable_another_negotiated_consumer(self):
        controller = await self.activate()
        viewer = Socket()
        self.server.clients.add(viewer)
        await self.capability(viewer, sink='track-generator')
        await self.capability(supported=False)
        self.assertTrue(controller.effective)
        self.assertEqual(self.server._refinement_consumers('primary'), {viewer})
        await self.request(controller, viewer)
        await self.drain()
        self.assertEqual(self.native.calls, 1)

    async def test_cached_png_still_obeys_current_membership_before_send(self):
        controller = await self.activate()
        controller.image = RefinedImage(controller.epoch, STAMP, PNG)
        self.server._refinement_tokens[self.socket] = (controller.epoch, STAMP)
        token = self.server._refinement_tokens[self.socket]
        self.server.clients.remove(self.socket)
        await self.server._send_refinement(self.socket, 'primary', controller.epoch, STAMP, token)
        self.assertFalse(self.socket.sent)
        self.assertEqual(self.native.calls, 0)

    async def test_relay_sends_identity_with_its_item_but_not_a_retired_epoch(self):
        controller = await self.activate()
        data = b'\x04\x01\x00\x05\x00\x00\x01\x00\x00\x80\x00\x05payload'
        metadata = dict(STAMP.wire(), type='lossless_sample', version=1,
                        epoch=controller.epoch, frame_id=5, y=0, payload_bytes=len(data))
        for stale in (False, True):
            self.socket.sent.clear()
            relay = Relay()
            relay.server = self.server
            relay.display_id = 'primary'
            relay.ws = self.socket
            relay.stopped = False
            relay.refinement_epoch = None
            relay.gauged = False
            relay.backlog = deque([dict(data=data, frame_id=5, lossless=metadata)])
            relay.backlog_bytes = len(data)
            relay.written = 0
            relay.marks = deque()
            relay._hold = lambda *args: None
            self.server.video_relay_groups = {'primary': {self.socket: relay}}
            self.server.display_clients['primary']['ws'] = None
            if stale:
                controller.invalidate()

            async def finish(raw, active_relay=relay):
                if isinstance(raw, bytes):
                    active_relay.stopped = True

            self.socket.on_send = finish
            await asyncio.wait_for(relay._run(), 1)
            self.assertEqual(self.socket.sent[-1], data)
            self.assertEqual(self.messages('lossless_sample'), [] if stale else [metadata])
            self.assertEqual(len(self.messages('lossless_status')), 0 if stale else 1)
            self.assertFalse(self.server.video_relay_groups['primary'])

    async def relay_replacement(self, stop_waiter):
        """Replace a relay while a sample announcement is still draining."""
        controller = await self.activate()
        header = b'\x04\x01\x00\x05\x00\x00\x01\x00\x00\x80\x00\x05'
        payloads = (header + b'old', header + b'new')
        samples = (STAMP, replace(STAMP, source=20, scene=21, sample=22))
        relays = []
        for data, stamp in zip(payloads, samples):
            relay = Relay()
            relay.server, relay.ws, relay.display_id = self.server, self.socket, 'primary'
            relay.stopped, relay.gauged, relay.refinement_epoch = False, False, None
            relay.backlog = deque([dict(data=data, frame_id=5, lossless=dict(
                stamp.wire(), type='lossless_sample', version=1,
                epoch=controller.epoch + len(relays), frame_id=5, y=0, payload_bytes=len(data)))])
            relay.backlog_bytes, relay.written = len(data), 0
            relay.marks = deque()
            relay._hold = lambda *args: None
            relays.append(relay)
        self.server.display_clients['primary']['ws'] = None
        self.server.video_relay_groups = {'primary': {self.socket: relays[0]}}
        draining, release = asyncio.Event(), asyncio.Event()

        async def gate(raw):
            if isinstance(raw, str):
                message = json.loads(raw)
                if message['type'] == 'lossless_sample' and message['scene'] == str(STAMP.scene):
                    draining.set()
                    await release.wait()
            elif isinstance(raw, bytes):
                relays[payloads.index(raw)].stopped = True

        self.socket.on_send = gate
        first = asyncio.create_task(relays[0]._run())
        second = None
        try:
            await asyncio.wait_for(draining.wait(), 1)
            relays[0].stopped = True
            controller.observe(samples[1])
            self.server.video_relay_groups['primary'][self.socket] = relays[1]
            second = asyncio.create_task(relays[1]._run())
            await asyncio.sleep(0)
            self.assertEqual(len(self.messages('lossless_sample')), 1)
            self.assertFalse(any(isinstance(raw, bytes) for raw in self.socket.sent))
            if stop_waiter:
                relays[1].stopped = True
            release.set()
            await asyncio.wait_for(asyncio.gather(first, second), 1)
        finally:
            release.set()
            for relay in relays:
                relay.stopped = True
            await asyncio.wait_for(asyncio.gather(*(task for task in (first, second)
                                                  if task is not None)), 1)
        relevant = [json.loads(raw) if isinstance(raw, str) else raw
                    for raw in self.socket.sent
                    if isinstance(raw, bytes) or json.loads(raw)['type'] == 'lossless_sample']
        self.assertEqual(len(relevant), 2 if stop_waiter else 4)
        self.assertEqual(relevant[0]['scene'], str(STAMP.scene))
        self.assertEqual(relevant[1], payloads[0])
        if not stop_waiter:
            self.assertEqual(relevant[2]['scene'], str(samples[1].scene))
            self.assertEqual(relevant[3], payloads[1])
        self.assertFalse(self.server.video_relay_groups['primary'])

    async def test_relay_restart_pairs_identical_headers_across_metadata_drain(self):
        await self.relay_replacement(stop_waiter=False)

    async def test_relay_stopped_while_waiting_for_pair_lock_writes_nothing(self):
        await self.relay_replacement(stop_waiter=True)

    async def test_old_epoch_cancel_cannot_withdraw_current_request(self):
        controller = await self.activate()
        token = (controller.epoch, STAMP)
        pending = ('primary', controller.epoch, STAMP, token)
        self.server._refinement_tokens[self.socket] = token
        self.server._refinement_pending[self.socket] = pending
        for epoch in (controller.epoch - 1, None, True, 1.0):
            await self.server._handle_refinement(self.socket, json.dumps(
                dict(STAMP.wire(), version=1, op='cancel', epoch=epoch)))
            self.assertIs(self.server._refinement_tokens.get(self.socket), token)
            self.assertIs(self.server._refinement_pending.get(self.socket), pending)
        for stamp in (replace(STAMP, scene=10), replace(STAMP, source=10),
                      replace(STAMP, run=10), replace(STAMP, sample=10),
                      replace(STAMP, width=128)):
            await self.server._handle_refinement(self.socket, json.dumps(
                dict(stamp.wire(), version=1, op='cancel', epoch=controller.epoch)))
            self.assertIs(self.server._refinement_tokens.get(self.socket), token)
            self.assertIs(self.server._refinement_pending.get(self.socket), pending)
        await self.server._handle_refinement(self.socket, json.dumps(
            dict(STAMP.wire(), version=1, op='cancel', epoch=controller.epoch)))
        self.assertNotIn(self.socket, self.server._refinement_tokens)
        self.assertNotIn(self.socket, self.server._refinement_pending)

    async def test_parent_disabled_during_pacing_withdraws_transfer_and_tracking(self):
        controller = await self.activate()

        async def disable(*args):
            self.server.display_clients['primary']['use_paint_over_quality'] = False
            await self.server._refresh_refinement('primary')

        NAMESPACE['_await_bulk_window'] = disable
        await self.request(controller)
        await self.drain()
        self.assertFalse(any(isinstance(raw, bytes) for raw in self.socket.sent))
        self.assertFalse(self.messages('lossless_end'))
        self.assertEqual(self.native.transitions, [True, False])
        self.assertIsNone(controller.image)


class UnsupportedRTCTests(unittest.TestCase):
    """The real WebRTC responder negotiates unavailability without capture work."""

    def test_only_known_peer_capability_gets_explicit_unsupported_response(self):
        rtc = RTC()
        sent = []
        channel = object()
        rtc.rtc_app = SimpleNamespace(peer_connections={'peer': {'data_channel': channel}},
                                     send_message_to_channel=lambda *args: sent.append(args))
        rtc._display_setting = lambda display, key: True
        payload = json.dumps(dict(version=1, op='capability', supported=True, sink='worker-canvas'))
        rtc._answer_refinement_capability(payload, 'primary', 'peer')
        self.assertEqual(len(sent), 1)
        target, kind, status = sent[0]
        self.assertIs(target, channel)
        self.assertEqual(kind, 'lossless_status')
        self.assertTrue(status['requested'])
        self.assertFalse(status['supported'])
        self.assertFalse(status['effective'])
        self.assertEqual(status['reason'], 'webrtc-unavailable')
        for value, peer in ((payload, 'unknown'), ('[]', 'peer'), ('bad', 'peer'),
                            ('x' * 2049, 'peer'), ('{"version":1,"op":"request"}', 'peer'),
                            ('{"version":true,"op":"capability"}', 'peer'),
                            ('{"version":1.0,"op":"capability"}', 'peer')):
            rtc._answer_refinement_capability(value, 'primary', peer)
        self.assertEqual(len(sent), 1)


if __name__ == '__main__':
    unittest.main()
