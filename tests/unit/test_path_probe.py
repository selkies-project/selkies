#!/usr/bin/env python3
"""The still-screen path probe (`PathProbe`): a target congestion control cut under
the configured rate is padded up to, while the stream itself fills under half of it
and the steer's hold is over, so the path is tested at the rate the steer asks of it;
nothing pads at the ceiling, during the hold, on a tick that measured no delay, or
for a stream that fills half its target itself. A queue on a tick that followed
padding ends the probe and quiets it, longer each time a probe ends that way, and a
target that reached the ceiling starts the quiet over.
"""
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies.stream_server import CongestionSteer, PathProbe  # noqa: E402

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [path-probe] {label}  {detail}", flush=True)


CEILING = 8000.0
probe = PathProbe()
got = probe.rate_bps(8000, CEILING, 100_000.0, 0.0, False, 0.0)
check("a target at the ceiling pads nothing", got == 0.0, got)
got = probe.rate_bps(2000, CEILING, 100_000.0, 0.0, True, 1.0)
check("nor one the steer still holds after its cut", got == 0.0, got)
got = probe.rate_bps(2000, CEILING, 100_000.0, None, False, 2.0)
check("nor a tick that measured no delay", got == 0.0, got)
got = probe.rate_bps(2000, CEILING, None, 0.0, False, 2.5)
check("nor one whose offer is unknown", got == 0.0, got)
got = probe.rate_bps(2000, CEILING, 1_200_000.0, 0.0, False, 3.0)
check("nor a stream filling half its target or more itself", got == 0.0, got)
got = probe.rate_bps(2000, CEILING, 100_000.0, 0.0, False, 3.5)
check("a still screen under a cut target pads up to the target", got == 1_900_000.0, got)

steer = CongestionSteer()
cur = steer.target(2000, CEILING, 100.0, 1_900_000.0, 0.0, 10.0, queue_s=0.0, offered_bps=100_000.0)
check("unpadded, the steer climbs no further on that trickle", cur == 2000, cur)
cur = steer.target(cur, CEILING, 100.0, 2_000_000.0, 0.0, 11.0, queue_s=0.0, offered_bps=2_000_000.0)
check("padded to the target, it steps up", cur > 2000, cur)

got = probe.rate_bps(2300, CEILING, 100_000.0, 0.3, False, 4.0)
check("a queue after padding ends the probe", got == 0.0, got)
got = probe.rate_bps(2000, CEILING, 100_000.0, 0.0, False, 10.0)
check("and quiets it for QUIET_S", got == 0.0 and probe.quiet_until == 4.0 + PathProbe.QUIET_S, probe.quiet_until)
got = probe.rate_bps(2000, CEILING, 100_000.0, 0.0, False, 4.0 + PathProbe.QUIET_S)
check("after which it pads again", got > 0, got)
probe.rate_bps(2300, CEILING, 100_000.0, 0.3, False, 20.0)
check("a second probe ending in a queue quiets it twice as long",
      probe.quiet_until == 20.0 + 2 * PathProbe.QUIET_S, probe.quiet_until)
for i in range(8):
    probe.padding = True
    probe.rate_bps(2300, CEILING, 100_000.0, 0.3, False, 100.0 + i)
check("never past QUIET_MAX_S", probe.quiet_s == PathProbe.QUIET_MAX_S, probe.quiet_s)
probe.rate_bps(8000, CEILING, 100_000.0, 0.0, False, 400.0)
check("a target that reached the ceiling starts the quiet over", probe.quiet_s == PathProbe.QUIET_S, probe.quiet_s)
probe = PathProbe()
got = probe.rate_bps(2000, CEILING, 100_000.0, 0.4, False, 0.0)
check("a queue on a tick that followed no padding quiets nothing",
      got == 0.0 and probe.quiet_until == 0.0, probe.quiet_until)

print(f"[path-probe] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
