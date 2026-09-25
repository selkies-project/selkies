#!/usr/bin/env python3
"""Congestion control over WebSockets: a CBR display whose frame round trip
stands a queue above its floor for two seconds in a row is backed off by the
transports' shared steer, held there, and stepped back up once the round trip
returns to the floor; a round trip at its floor never moves the rate, and
whatever else applies a bitrate applies the steered one.
"""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies.websockets_mode import DataStreamingServer  # noqa: E402

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [ws-link-steer] {label}  {detail}", flush=True)


class Module:
    def __init__(self):
        self.rates = []

    def update_video_bitrate(self, kbps):
        self.rates.append(kbps)


server = DataStreamingServer.__new__(DataStreamingServer)
module = Module()
server.capture_instances = {"display2": {"module": module}}
server._initial_video_bitrate = 4000
state = {"video_bitrate": 4000, "backpressure_enabled": True}


def run(rtt_ms, start, seconds):
    state["smoothed_rtt"] = rtt_ms
    t = start
    while t < start + seconds:
        server._steer_bitrate_to_link("display2", state, t)
        t += 0.5
    return t


t = run(20.0, 0.0, 10)
check("a round trip at its floor leaves the target alone", module.rates == [] and
      server._video_bitrate_kbps(state) == 4000, module.rates)
t = run(120.0, t, 1.5)
check("two queued seconds in a row back the target off", module.rates == [2800], module.rates)
check("and every other path applies the backed-off rate", server._video_bitrate_kbps(state) == 2800)
t = run(20.0, t, 1.0)
check("a clean second inside the hold keeps it", module.rates == [2800], module.rates)
run(20.0, t, 6)
check("past the hold the rate steps back up to the target, never over it",
      module.rates[1:] and all(b > a for a, b in zip(module.rates, module.rates[1:]))
      and module.rates[-1] == 4000, module.rates)

state = {"video_bitrate": 4000, "backpressure_enabled": True}
module.rates.clear()
t = run(20.0, 0.0, 5)
state["backpressure_enabled"] = False
run(20.0, t, 2.5)
check("a gated display counts as queued even at its floor", module.rates == [2800], module.rates)

print(f"\n{passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
