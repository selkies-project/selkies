#!/usr/bin/env python3
"""The transports' shared bitrate steer.

Loss (WebRTC): two lossy ticks in a row back the CBR target off, a single one
does not; the backoff holds the target for a while before a clean tick raises it
again; and a raise follows the step or the measured goodput, never past the
ceiling or under the floor.

A standing queue: the first tick that shows one backs the target off to the
headroom of what the path delivered, lower still for the time the queue takes
to drain; no second cut follows while that drain runs, and the target then
settles at the headroom. A raise is slow while the path delivers near the
capacity it showed and fast far from it, and a path that delivers clear past
it forgets it. A queue under a stream sending a trickle of its target, which
says nothing of the path, cuts nothing.
"""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies.webrtc_mode import CongestionSteer  # noqa: E402

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [congestion-steer] {label}  {detail}", flush=True)


CEILING, FLOOR, GOODPUT = 8000.0, 100.0, 20_000_000.0

steer = CongestionSteer()
check("one lossy tick leaves the target", steer.target(8000, CEILING, FLOOR, GOODPUT, 0.2, 0.0) == 8000)
got = steer.target(8000, CEILING, FLOOR, GOODPUT, 0.2, 1.0)
check("the second lossy tick in a row backs off by 30%", got == 5600, got)
check("a clean tick inside the hold keeps the backed-off target",
      steer.target(5600, CEILING, FLOOR, GOODPUT, 0.0, 2.0) == 5600)
got = steer.target(5600, CEILING, FLOOR, 5_000_000.0, 0.0, 3.0)
check("a clean tick past the hold raises by the step", round(got) == 6440, got)

steer = CongestionSteer()
steer.target(8000, CEILING, FLOOR, GOODPUT, 0.2, 0.0)
steer.target(8000, CEILING, FLOOR, GOODPUT, 0.0, 1.0)
got = steer.target(8000, CEILING, FLOOR, GOODPUT, 0.2, 2.0)
check("a clean tick between two lossy ones clears the strike", got == 8000, got)

steer = CongestionSteer()
got = steer.target(4000, CEILING, FLOOR, 10_000_000.0, 0.0, 0.0)
check("measured goodput lifts the target above the step", got == 8000, got)
got = steer.target(1000, CEILING, FLOOR, 500_000.0, 0.0, 1.0)
check("low goodput never drags the target below the step", got == 1150, got)
got = steer.target(7500, CEILING, FLOOR, GOODPUT, 0.0, 2.0)
check("a raise stops at the ceiling", got == 8000, got)
steer.target(120, CEILING, FLOOR, GOODPUT, 0.2, 3.0)
got = steer.target(120, CEILING, FLOOR, GOODPUT, 0.2, 4.0)
check("a backoff stops at the floor", got == 100, got)

# A standing queue on a path delivering 5000 kbps under an 8000 kbps target.
DELIVERED = 5_000_000.0
steer = CongestionSteer()
got = steer.target(8000, CEILING, FLOOR, DELIVERED, 0.0, 0.0, queue_s=0.0)
check("a clean tick at the ceiling stays there", got == 8000, got)
got = steer.target(8000, CEILING, FLOOR, DELIVERED, 0.0, 0.5, queue_s=0.1)
check("the first queued tick backs off to the headroom less the drain of a 100 ms queue",
      round(got) == 3750, got)
got = steer.target(3750, CEILING, FLOOR, DELIVERED, 0.0, 0.7, queue_s=0.15)
check("a queued tick while that queue drains cuts nothing more", got == 3750, got)
got = steer.target(3750, CEILING, FLOOR, 3_750_000.0, 0.0, 1.0, queue_s=0.0)
check("the drain over, the target settles at the headroom of the capacity", round(got) == 4250, got)
got = steer.target(4250, CEILING, FLOOR, DELIVERED, 0.0, 1.2, queue_s=0.0)
check("a clean tick inside the settle keeps it", round(got) == 4250, got)
got = steer.target(4250, CEILING, FLOOR, 3_600_000.0, 0.0, 2.2, queue_s=0.0)
check("delivering below the near band of the capacity, a clean second raises by the full step",
      round(got) == round(4250 * 1.15), got)
got = steer.target(4900, CEILING, FLOOR, 4_600_000.0, 0.0, 3.2, queue_s=0.0)
check("delivering within it, by the near step", round(got) == round(4900 * 1.05), got)
got = steer.target(6000, CEILING, FLOOR, 4_600_000.0, 0.0, 4.2, queue_s=0.0)
check("the band follows what the path delivers, not the target the encoder runs under",
      round(got) == round(6000 * 1.05), got)

steer = CongestionSteer()
got = steer.target(8000, CEILING, FLOOR, DELIVERED, 0.0, 0.0, queue_s=1.5)
check("a deep queue drains at half the capacity, never lower", round(got) == 2500, got)
got = steer.target(2500, CEILING, FLOOR, DELIVERED, 0.0, 2.9, queue_s=0.9)
check("and holds for the three seconds that takes", got == 2500, got)
got = steer.target(2500, CEILING, FLOOR, DELIVERED, 0.0, 3.0, queue_s=0.0)
check("then settles at the headroom", round(got) == 4250, got)

steer = CongestionSteer()
steer.target(8000, CEILING, FLOOR, DELIVERED, 0.0, 0.0, queue_s=0.1)
steer.target(3750, CEILING, FLOOR, DELIVERED, 0.0, 0.5, queue_s=0.0)
got = steer.target(4250, CEILING, FLOOR, DELIVERED, 0.0, 1.0, queue_s=0.1)
check("a queue standing past the settle backs off again", round(got) == 3750, got)

steer = CongestionSteer()
steer.target(4000, CEILING, FLOOR, 2_000_000.0, 0.0, 0.0, queue_s=0.1)
got = steer.target(1500, CEILING, FLOOR, 0.0, 0.0, 1.0, queue_s=0.0)
check("the tick that ends the drain only settles", round(got) == 1700, got)
got = steer.target(1000, CEILING, FLOOR, 0.0, 0.0, 2.0, queue_s=0.0)
check("far below the capacity a clean second raises by the full step", round(got) == 1150, got)
got = steer.target(1900, CEILING, FLOOR, 1_900_000.0, 0.0, 2.5, queue_s=0.0)
check("half a second near it raises by half a second's near step",
      round(got) == round(1900 * 1.05 ** 0.5), got)
got = steer.target(2300, CEILING, FLOOR, 2_300_000.0, 0.0, 3.5, queue_s=0.0)
check("delivering clear past the capacity with the path clean, the capacity is forgotten for the full step",
      steer.capacity_kbps is None and round(got) == round(2300 * 1.15), (got, steer.capacity_kbps))

steer = CongestionSteer()
got = steer.target(8000, CEILING, FLOOR, 0.0, 0.0, 0.0, queue_s=0.04)
check("a queue with no measured delivery backs off by 30%", got == 5600, got)
check("and holds", steer.target(5600, CEILING, FLOOR, 0.0, 0.0, 1.9, queue_s=0.0) == 5600)
got = steer.target(200, CEILING, FLOOR, 150_000.0, 0.0, 10.0, queue_s=0.5)
check("a queue backoff stops at the floor", got == 100, got)

steer = CongestionSteer()
got = steer.target(8000, CEILING, FLOOR, 24_000.0, 0.0, 0.0, queue_s=0.03, offered_bps=24_000.0)
check("a queue under a stream sending a trickle of its target cuts nothing", got == 8000, got)
got = steer.target(8000, CEILING, FLOOR, DELIVERED, 0.0, 1.0, queue_s=0.1, offered_bps=7_600_000.0)
check("the queue under a stream sending its target backs off as before", round(got) == 3750, got)

print(f"[congestion-steer] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
