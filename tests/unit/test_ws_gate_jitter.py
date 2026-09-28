#!/usr/bin/env python3
"""The websockets backpressure gate of a display congestion control steers shuts
on a queue that stands, not on a moment.

Each check runs the real backpressure loop, one check per step of a script of
how much stream stands in flight past the path's floor: a moment's hold (a
lost segment's retransmission, a Wi-Fi hop's jitter) that one check sees and
the next does not leaves the gate open, however often it recurs; a queue that
stands through two checks in a row shuts it, which stays shut while the queue
stands and lifts once it drains. Without the steer, which bounds a queue on the
path, the gate is that bound and the first check over the allowance shuts it.
"""
import asyncio
import os
import sys
import time
from collections import OrderedDict, deque
from types import SimpleNamespace

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(
    os.path.dirname(os.path.abspath(__file__)))), "src"))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402

from selkies import websockets_mode as w  # noqa: E402
from selkies.websockets_mode import DataStreamingServer  # noqa: E402

res = H.Results("ws-gate-jitter")
FPS = 60
FLOOR_MS = 30.0
INTERVAL = w.BACKPRESSURE_CHECK_INTERVAL_S


class Module:
    """The capture's encoder, as far as the gate reaches it."""

    def request_idr_frame(self) -> None:
        pass


def gate_states(script: list, steered: bool = True) -> list:
    """Whether the gate is open after each check, the n-th check seeing
    `script[n]` ms of stream standing in flight past the floor, with congestion
    control steering the display or not."""
    server = DataStreamingServer.__new__(DataStreamingServer)
    server.client_settings_received = None
    server.backpressure_check_interval_s = INTERVAL
    server.allowed_desync_ms = w.BACKPRESSURE_ALLOWED_DESYNC_MS
    server.capture_instances = {"primary": {"module": Module()}}
    server.cli_args = SimpleNamespace(congestion_control=(steered,))
    server.rc_mode = SimpleNamespace(value=w.RateControlMode.CBR.value)
    server.metrics = None
    state = {
        "ws": object(), "framerate": FPS, "acknowledged_frame_id": 100, "acked_sent_at": None,
        "last_sent_frame_id": 100, "has_sent_any_frame": True, "sent_timestamps": OrderedDict(),
        "rtt_samples": deque(maxlen=20), "smoothed_rtt": 0.0, "backpressure_enabled": True,
        "unacked_since": None, "stall_gated_at": None, "rtt_floor_ms": FLOOR_MS,
    }
    server.display_clients = {"primary": state}
    steps = list(script)
    seen = []
    real_sleep = asyncio.sleep

    def stand(ms: float) -> None:
        now = time.monotonic()
        in_flight = round((FLOOR_MS + ms) / 1000.0 * FPS)
        state["acked_sent_at"] = now - 1.0
        state["sent_timestamps"] = OrderedDict(
            (101 + i, (now - 0.9 + i / 1000.0, 20000)) for i in range(in_flight))
        state["last_sent_frame_id"] = 100 + in_flight

    async def one_check_per_sleep(delay, *args, **kwargs):
        if delay == INTERVAL:
            if len(steps) < len(script):
                seen.append(state["backpressure_enabled"])
            if not steps:
                raise asyncio.CancelledError
            stand(steps.pop(0))
        return await real_sleep(0)

    async def run() -> None:
        asyncio.sleep = one_check_per_sleep
        try:
            await server._run_frame_backpressure_logic("primary")
        except asyncio.CancelledError:
            pass
        finally:
            asyncio.sleep = real_sleep

    asyncio.run(run())
    return seen


moment = gate_states([0, 300, 0, 0])
res.check("a moment's 300 ms hold past the floor, gone at the next check, leaves the gate open",
          all(moment), moment)
recurring = gate_states([300, 0, 300, 0, 300, 0])
res.check("moments a check apart, each released by the next check, never shut it", all(recurring), recurring)
standing = gate_states([0, 300, 300, 300, 0, 0])
res.check("a queue standing through two checks shuts it at the second", standing[:3] == [True, True, False], standing)
res.check("it stays shut while the queue stands and lifts once it drains",
          standing[3:] == [False, True, True], standing)
res.check("a stream in flight within the allowance never shuts it", all(gate_states([100, 150, 200, 150])))
unsteered = gate_states([0, 300, 0], steered=False)
res.check("without congestion control a moment past the allowance shuts it at once", unsteered == [True, False, True],
          unsteered)

sys.exit(0 if res.summary() else 1)
