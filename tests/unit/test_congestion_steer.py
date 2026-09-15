#!/usr/bin/env python3
"""The WebRTC congestion loop's bitrate steer: two lossy ticks in a row back the
CBR target off, a single one does not; the backoff holds the target for a while
before a clean tick raises it again; and a raise follows the step or the
measured goodput, never past the ceiling or under the floor.
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
got = steer.target(5600, CEILING, FLOOR, 5_000_000.0, 0.0, 3.5)
check("a clean tick past the hold raises by the step", round(got) == 6440, got)

steer = CongestionSteer()
steer.target(8000, CEILING, FLOOR, GOODPUT, 0.2, 0.0)
steer.target(8000, CEILING, FLOOR, GOODPUT, 0.0, 1.0)
got = steer.target(8000, CEILING, FLOOR, GOODPUT, 0.2, 2.0)
check("a clean tick between two lossy ones clears the strike", got == 8000, got)

steer = CongestionSteer()
got = steer.target(4000, CEILING, FLOOR, 10_000_000.0, 0.0, 0.0)
check("measured goodput lifts the target above the step", got == 8000, got)
got = steer.target(1000, CEILING, FLOOR, 500_000.0, 0.0, 0.0)
check("low goodput never drags the target below the step", got == 1150, got)
got = steer.target(7500, CEILING, FLOOR, GOODPUT, 0.0, 0.0)
check("a raise stops at the ceiling", got == 8000, got)
steer.target(120, CEILING, FLOOR, GOODPUT, 0.2, 0.0)
got = steer.target(120, CEILING, FLOOR, GOODPUT, 0.2, 1.0)
check("a backoff stops at the floor", got == 100, got)

print(f"[congestion-steer] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
