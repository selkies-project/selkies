#!/usr/bin/env python3
"""The websockets backpressure gate of a display congestion control steers shuts
on a queue that stands, not on a moment.

Each check runs the real backpressure loop, one check per step of a script of
how much stream stands in flight past the path's floor: a moment's hold (a
lost segment's retransmission, a Wi-Fi hop's jitter) that one check sees and
the next does not leaves the gate open, however often it recurs; a queue that
the client's acks move on behind through two checks in a row shuts it, which
stays shut while the queue stands and lifts once it drains. Without the steer,
which bounds a queue on the path, the gate is that bound: the first check over
the allowance shuts it while the client's acks move on, a queue the path
delivers behind. A path that holds the whole stream, its acks standing still
for a check, is waited for, steered or not, until the queue has stood over the
allowance for PATH_HOLD_MAX_S: what it held arrives at once, where frames the
gate dropped would be predicted past from whatever the encoder still holds.
Time is the checks': each moves the loop's clock on by a check interval.
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
from selkies.stream_server import note_pong  # noqa: E402
from selkies.websockets_mode import DataStreamingServer  # noqa: E402

res = H.Results("ws-gate-jitter")
FPS = 60
FLOOR_MS = 30.0
INTERVAL = w.BACKPRESSURE_CHECK_INTERVAL_S


class Module:
    """The capture's encoder, as far as the gate reaches it."""

    def request_idr_frame(self) -> None:
        pass


class Socket:
    """The client's socket, as far as the gate reaches it: a browser answers a
    ping once everything written before it arrived, here at once, or once a
    path holding the stream (`holding`) lets it through."""

    def __init__(self) -> None:
        self.holding = False
        self.held = []

    async def ping(self, payload: bytes) -> None:
        if self.holding:
            self.held.append(payload)
        else:
            note_pong(self, payload)

    def hold(self, holding: bool) -> None:
        self.holding = holding
        if not holding:
            for payload in self.held:
                note_pong(self, payload)
            self.held.clear()


def gate_states(script: list, steered: bool = True, acks_moving: bool = False, page: str = "path") -> list:
    """Whether the gate is open after each check, the n-th check seeing
    `script[n]` ms of stream standing in flight past the floor, with congestion
    control steering the display or not, and the client's acks moving on or
    standing still since the check before. Acks standing still over the
    allowance are held by `page`: "path" holds the stream, pings and all, and
    the page goes on sending; "silent" is the page's thread stalled, sending
    nothing, its socket delivering; "heartbeat" a client that keeps sending,
    its socket delivering, but acks nothing new."""
    server = DataStreamingServer.__new__(DataStreamingServer)
    server.client_settings_received = None
    server.backpressure_check_interval_s = INTERVAL
    server.allowed_desync_ms = w.BACKPRESSURE_ALLOWED_DESYNC_MS
    server.capture_instances = {"primary": {"module": Module()}}
    server.cli_args = SimpleNamespace(congestion_control=(steered,))
    server.rc_mode = SimpleNamespace(value=w.RateControlMode.CBR.value)
    server.metrics = None
    server.video_relay_groups = {}
    server._heard_at = {}
    socket = Socket()
    state = {
        "ws": socket, "framerate": FPS, "acknowledged_frame_id": 100, "acked_sent_at": None,
        "last_sent_frame_id": 100, "has_sent_any_frame": True, "sent_timestamps": OrderedDict(),
        "rtt_samples": deque(maxlen=20), "smoothed_rtt": 0.0, "backpressure_enabled": True,
        "unacked_since": None, "stall_gated_at": None, "rtt_floor_ms": FLOOR_MS,
    }
    server.display_clients = {"primary": state}
    steps = list(script)
    seen = []
    real_sleep = asyncio.sleep
    clock = [1000.0]

    class Clock:
        """The loop's `time`, its monotonic clock the checks'."""

        def __getattr__(self, name):
            return getattr(time, name)

        def monotonic(self) -> float:
            return clock[0]

    def stand(ms: float) -> None:
        now = clock[0]
        in_flight = round((FLOOR_MS + ms) / 1000.0 * FPS)
        state["acked_sent_at"] = now - 1.0
        state["sent_timestamps"] = OrderedDict(
            (101 + i, (now - 0.9 + i / 1000.0, 20000)) for i in range(in_flight))
        state["last_sent_frame_id"] = 100 + in_flight
        state["acked_at"] = now if acks_moving else now - 1.0
        held = not acks_moving and ms > w.BACKPRESSURE_ALLOWED_DESYNC_MS
        socket.hold(held and page == "path")
        server._heard_at[socket] = now - 1.0 if held and page == "silent" else now

    async def one_check_per_sleep(delay, *args, **kwargs):
        if delay == INTERVAL:
            if len(steps) < len(script):
                seen.append(state["backpressure_enabled"])
            if not steps:
                raise asyncio.CancelledError
            clock[0] += INTERVAL
            stand(steps.pop(0))
        return await real_sleep(0)

    async def run() -> None:
        asyncio.sleep = one_check_per_sleep
        w.time = Clock()
        try:
            await server._run_frame_backpressure_logic("primary")
        except asyncio.CancelledError:
            pass
        finally:
            asyncio.sleep = real_sleep
            w.time = time

    asyncio.run(run())
    return seen


moment = gate_states([0, 300, 0, 0])
res.check("a moment's 300 ms hold past the floor, gone at the next check, leaves the gate open",
          all(moment), moment)
recurring = gate_states([300, 0, 300, 0, 300, 0])
res.check("moments a check apart, each released by the next check, never shut it", all(recurring), recurring)
standing = gate_states([0, 300, 300, 300, 0, 0], acks_moving=True)
res.check("a queue the path delivers behind, standing through two checks, shuts it at the second",
          standing[:3] == [True, True, False], standing)
res.check("it stays shut while the queue stands and lifts once it drains",
          standing[3:] == [False, True, True], standing)
hold = gate_states([0, 300, 300, 300, 300, 300, 0, 0])
over_checks = round(w.PATH_HOLD_MAX_S / INTERVAL)
res.check(f"a path holding the stream, acks standing still, is waited for {w.PATH_HOLD_MAX_S} s before it shuts",
          hold[:over_checks + 1] == [True] * (over_checks + 1) and hold[over_checks + 1] is False, hold)
res.check("and the gate lifts once the hold has delivered", hold[-1] is True, hold)
res.check("a stream in flight within the allowance never shuts it", all(gate_states([100, 150, 200, 150])))
delivered = gate_states([0, 300, 0], steered=False, acks_moving=True)
res.check("without congestion control a queue the path goes on delivering behind shuts it at once",
          delivered == [True, False, True], delivered)
held = gate_states([0, 300, 0, 0], steered=False)
res.check("without congestion control a moment the path holds the stream, acks standing still, leaves it open",
          all(held), held)
silent = gate_states([0, 300, 300, 300, 300, 300, 0, 0], page="silent")
res.check(f"a page that stalls, sending nothing while its socket delivers, is waited for {w.PATH_HOLD_MAX_S} s too",
          silent[:over_checks + 1] == [True] * (over_checks + 1) and silent[over_checks + 1] is False, silent)
beating = gate_states([0, 300, 300, 300, 0, 0], page="heartbeat")
res.check("a client that keeps sending but acks nothing new (a decoder waiting for a key frame) shuts it at the second",
          beating[:3] == [True, True, False], beating)
still_held = gate_states([0, 300, 300, 0], steered=False)
res.check("a hold still there at the next check is waited for", all(still_held), still_held)
stuck = gate_states([0, 300, 300, 300, 300, 300, 0], steered=False)
res.check(f"one that outlasts {w.PATH_HOLD_MAX_S} s shuts it", stuck[:over_checks + 1] == [True] * (over_checks + 1)
          and stuck[over_checks + 1] is False, stuck)

sys.exit(0 if res.summary() else 1)
