#!/usr/bin/env python3
"""Congestion control over WebSockets: a CBR display whose acked frames all stand
a queue above the round-trip floor through one half-second window is backed off
at once by the transports' shared steer, to the headroom of what the path
delivered through that window less the queue's drain; it is held while that
queue drains, then stepped back up while the round trip stays at its floor. A
queue still building is read before it stands a whole window, and a gated
display is measured by the frames still acked from before the gate shut. A
window a stall acked late backs off from what the path delivered just before.
A round trip at its floor never moves the rate, one key frame's burst does not
either, round trips a jittering path spreads or a lossy one holds back never do,
a gate shut on a moment's delay or frames held back a moment and delivered
together do not either, while a queue behind an encoder running over its target
still does; a window with nothing acked moves nothing, a page taking the
display over is measured against its own path, and whatever else applies a
bitrate applies the steered one.
"""
import os
import random
import sys
from collections import deque

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies.websockets_mode import DataStreamingServer, _forget_path, _note_round_trip  # noqa: E402

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


def fresh_state():
    return {"video_bitrate": 4000, "backpressure_enabled": True, "sent_bytes": 0,
            "rtt_samples": deque(maxlen=20)}


state = fresh_state()


def run(rtt_ms, start, seconds, delivered_kbps=None, acks=True, spike_at=None, every=1):
    """Ack every 50 ms (every `every` x 50 ms) at `rtt_ms` with the path
    delivering `delivered_kbps` (by default the rate in force: the stream, not
    the path, is the limit), running the steer every half second as the
    backpressure loop does."""
    t = start
    step = 0.05
    n = 0
    while t < start + seconds - 1e-9:
        t = round(t + step, 3)
        n += 1
        if acks and n % every == 0:
            rate = delivered_kbps if delivered_kbps is not None else server._video_bitrate_kbps(state)
            state["sent_bytes"] += rate * 125 * step
            rtt = rtt_ms(t) if callable(rtt_ms) else rtt_ms
            if spike_at is not None and abs(t - spike_at) < 1e-6:
                rtt += 150.0
            _note_round_trip(state, rtt, state["sent_bytes"], t)
        if abs((t * 2) - round(t * 2)) < 1e-6:
            server._steer_bitrate_to_link("display2", state, t)
    return t


t = run(20.0, 0.0, 10, delivered_kbps=3000.0)
check("a round trip at its floor leaves the target alone", module.rates == [] and
      server._video_bitrate_kbps(state) == 4000, module.rates)
t = run(20.0, t, 2, delivered_kbps=3000.0, spike_at=t + 0.3)
check("one key frame's burst inside a window moves nothing", module.rates == [], module.rates)
t = run(320.0, t, 0.5, delivered_kbps=3000.0)
check("the first queued window backs the target off to the headroom less the 300 ms drain",
      module.rates == [1650], module.rates)
check("and every other path applies the backed-off rate", server._video_bitrate_kbps(state) == 1650)
t = run(250.0, t, 0.5, delivered_kbps=3000.0)
check("a window while that queue drains cuts nothing more", module.rates == [1650], module.rates)
t = run(20.0, t, 0.5, delivered_kbps=3000.0)
check("the drain over, the rate settles at the headroom of the capacity", module.rates == [1650, 2550],
      module.rates)
run(20.0, t, 12)
check("past the hold the rate steps back up to the target, never over it",
      module.rates[2:] and all(b > a for a, b in zip(module.rates[1:], module.rates[2:]))
      and module.rates[-1] == 4000, module.rates)

state = fresh_state()
module.rates.clear()
t = run(20.0, 0.0, 5)
state["backpressure_enabled"] = False
run(20.0, t, 0.5, acks=False)
check("a gated display with nothing acked counts as a queue of unknown depth", module.rates == [2800],
      module.rates)

state = fresh_state()
module.rates.clear()
t = run(20.0, 0.0, 5, delivered_kbps=3000.0)
state["backpressure_enabled"] = False
run(220.0, t, 0.5, delivered_kbps=3000.0)
check("one whose frames from before the gate shut still arrive is measured by them",
      module.rates == [1950], module.rates)

state = fresh_state()
module.rates.clear()
t = run(20.0, 0.0, 5, delivered_kbps=3000.0)
start = t
run(lambda at: 20.0 if at < start + 0.1 else 20.0 + (at - start - 0.1) * 500.0, t, 0.5, delivered_kbps=3000.0)
check("a queue still building reads as one before the whole window stands over the floor",
      module.rates == [1950], module.rates)

state = fresh_state()
module.rates.clear()
rng = random.Random(1)
t = run(lambda at: 20.0 + rng.uniform(0.0, 40.0) + rng.uniform(0.0, 40.0), 0.0, 600)
check("round trips a Wi-Fi hop spreads over 80 ms, both ways, with no queue never read as one",
      module.rates == [], module.rates)
run(lambda at: 20.0 + rng.uniform(0.0, 40.0) + rng.uniform(0.0, 40.0) + max(0.0, at - t - 0.5) * 300.0, t, 2)
check("while a queue building past that jitter still reads as one", module.rates != [], module.rates)

state = fresh_state()
module.rates.clear()
run(lambda at: 20.0 + rng.uniform(0.0, 40.0) + rng.uniform(0.0, 40.0), 0.0, 600, every=5)
check("nor do they with two acks a window, a caret blinking on a still screen", module.rates == [], module.rates)

state = fresh_state()
module.rates.clear()
run(lambda at: 100.0 + rng.uniform(0.0, 2.0) + (125.0 if rng.random() < 0.1 else 0.0), 0.0, 600)
check("nor do the single late round trips a lossy path's retransmissions leave", module.rates == [], module.rates)

state = fresh_state()
module.rates.clear()
t = run(20.0, 0.0, 5)
state["backpressure_enabled"] = False
run(20.0, t, 0.5)
check("a gate shut on a moment's delay that its acked frames do not show backs nothing off",
      module.rates == [], module.rates)

state = fresh_state()
module.rates.clear()
t = run(20.0, 0.0, 5)
start = t
run(lambda at: 150.0 - (at - start) * 60.0, t, 0.5, delivered_kbps=4500.0)
check("nor does a window of frames held back a moment and delivered together, faster than the rate in force",
      module.rates == [], module.rates)

state = fresh_state()
module.rates.clear()
t = run(20.0, 0.0, 5)
run(120.0, t, 0.5, delivered_kbps=4500.0)
check("while a queue behind an encoder running over its target still reads as one",
      module.rates == [3375], module.rates)

state = fresh_state()
module.rates.clear()
t = run(20.0, 0.0, 5)
start = t
run(lambda at: 20.0 if at < start + 0.25 else max(20.0, 190.0 - (at - start - 0.25) * 800.0), t, 0.5)
check("the frames behind a key frame's burst arrive ever sooner, so they never read that way",
      module.rates == [], module.rates)

state = fresh_state()
module.rates.clear()
t = run(20.0, 0.0, 5)
t = run(120.0, t, 0.5, delivered_kbps=3000.0)
n = len(module.rates)
run(20.0, t, 10, acks=False)
check("windows with nothing acked move nothing", len(module.rates) == n, module.rates)

state = fresh_state()
module.rates.clear()
t = run(20.0, 0.0, 5)
run(320.0, t, 0.5, delivered_kbps=1000.0)
check("a window whose frames a stall acked late and together backs off from what the path just "
      "delivered, not from the stall's trickle", module.rates == [2200], module.rates)

state = fresh_state()
module.rates.clear()
t = run(2.0, 0.0, 5)
_forget_path(state)
run(60.0, t, 5)
check("a page taking the display over measures its own path: its longer round trip is not a queue",
      module.rates == [], module.rates)
state = fresh_state()
module.rates.clear()
t = run(2.0, 0.0, 5)
run(60.0, t, 1)
check("where the old path's floor stayed, that round trip read as one", module.rates != [], module.rates)

print(f"\n{passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
