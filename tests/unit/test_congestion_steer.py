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
says nothing of the path, cuts nothing, the trickle measured, where the transport
counts delivery by acked frames, by what the stream sent of those frames rather
than a burst the tick sent after them; and a clean tick of that trickle raises
the target no further than the capacity the path showed, or not at all while
the steer knows none; nor does a tick that measured no offer. A stall (a deep
queue over a trickle of the target, or none delivered) is restored from on the
second tick in a row back at the floor; a path that slowed is not, nor a
restored target that queues again.
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
got = steer.target(5600, CEILING, FLOOR, 5_000_000.0, 0.0, 3.0, offered_bps=5_600_000.0)
check("a clean tick past the hold raises by the step", round(got) == 6440, got)

steer = CongestionSteer()
steer.target(8000, CEILING, FLOOR, GOODPUT, 0.2, 0.0)
steer.target(8000, CEILING, FLOOR, GOODPUT, 0.0, 1.0)
got = steer.target(8000, CEILING, FLOOR, GOODPUT, 0.2, 2.0)
check("a clean tick between two lossy ones clears the strike", got == 8000, got)

steer = CongestionSteer()
got = steer.target(4000, CEILING, FLOOR, 10_000_000.0, 0.0, 0.0)
check("measured goodput lifts the target above the step", got == 8000, got)
got = steer.target(1000, CEILING, FLOOR, 500_000.0, 0.0, 1.0, offered_bps=1_000_000.0)
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
got = steer.target(4900, CEILING, FLOOR, 4_600_000.0, 0.0, 3.2, queue_s=0.0, offered_bps=4_900_000.0)
check("delivering within it, by the near step", round(got) == round(4900 * 1.05), got)
got = steer.target(6000, CEILING, FLOOR, 4_600_000.0, 0.0, 4.2, queue_s=0.0, offered_bps=6_000_000.0)
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
got = steer.target(2300, CEILING, FLOOR, 2_300_000.0, 0.0, 3.5, queue_s=0.0, offered_bps=2_300_000.0)
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
steer = CongestionSteer()
got = steer.target(8000, CEILING, FLOOR, 733_000.0, 0.25, 0.0, queue_s=0.48, offered_bps=1_083_000.0)
check("a queue under a stream offering the path more than it delivers backs off, though the stream offers under half its target",
      got < 733, got)
steer = CongestionSteer()
got = steer.target(8000, CEILING, FLOOR, 217_000.0, 0.0, 0.0, queue_s=0.15, offered_bps=1_558_000.0,
                   acked_offered_bps=217_000.0)
check("a queue under a trickle whose tick also sent a burst, still unacked, cuts nothing: the acked frames delivered "
      "what the stream sent of them", got == 8000, got)
steer = CongestionSteer()
got = steer.target(8000, CEILING, FLOOR, 733_000.0, 0.25, 0.0, queue_s=0.48, offered_bps=1_083_000.0,
                   acked_offered_bps=1_083_000.0)
check("while frames that sent the path more than it delivered still back off", got < 733, got)
steer = CongestionSteer()
got = steer.target(8000, CEILING, FLOOR, 217_000.0, 0.0, 0.0, queue_s=0.15, offered_bps=None,
                   acked_offered_bps=217_000.0)
check("and a tick the gate held, which measured no offer, backs off whatever its acked frames sent", got < 8000, got)

# A still screen's trickle after a backoff on a 5000 kbps path under a 16000 kbps ceiling.
TOP, BOTTOM, TRICKLE = 16000.0, 1000.0, 300_000.0
steer = CongestionSteer()
cur = steer.target(TOP, TOP, BOTTOM, DELIVERED, 0.0, 0.0, queue_s=0.3, offered_bps=9_000_000.0)
for t in range(1, 41):
    cur = steer.target(cur, TOP, BOTTOM, TRICKLE, 0.0, float(t), queue_s=0.0, offered_bps=TRICKLE)
check("a still screen's trickle raises the target no further than the capacity the path showed",
      round(cur) == 5000, cur)
got = steer.target(cur, TOP, BOTTOM, 4_800_000.0, 0.0, 41.0, queue_s=0.0, offered_bps=5_000_000.0)
check("motion that fills the path probes past it by the near step", round(got) == round(5000 * 1.05), got)
got = steer.target(got, TOP, BOTTOM, TRICKLE, 0.0, 42.0, queue_s=0.0, offered_bps=TRICKLE)
check("a trickle keeps a target motion took past the capacity", round(got) == round(5000 * 1.05), got)
got = steer.target(got, TOP, BOTTOM, 6_000_000.0, 0.0, 43.0, queue_s=0.0, offered_bps=6_000_000.0)
check("motion the path delivers clear past it forgets it for the full step",
      steer.capacity_kbps is None and round(got) == round(5000 * 1.05 * 1.15), (got, steer.capacity_kbps))

steer = CongestionSteer()
got = steer.target(5000, TOP, BOTTOM, TRICKLE, 0.0, 0.0, queue_s=0.0, offered_bps=TRICKLE)
check("with no capacity known, a trickle keeps the target where it is", got == 5000, got)
got = steer.target(5000, TOP, BOTTOM, 4_800_000.0, 0.0, 1.0, queue_s=0.0, offered_bps=4_900_000.0)
check("and a stream sending its target raises it by the full step", round(got) == 5750, got)
steer = CongestionSteer()
steer.target(TOP, TOP, BOTTOM, 4_000_000.0, 0.2, 0.0, offered_bps=4_000_000.0)
cur = steer.target(TOP, TOP, BOTTOM, 4_000_000.0, 0.2, 1.0, offered_bps=4_000_000.0)
for t in range(2, 20):
    cur = steer.target(cur, TOP, BOTTOM, TRICKLE, 0.0, float(t), offered_bps=TRICKLE)
check("after a loss backoff a trickle keeps the backed-off target", round(cur) == 11200, cur)

steer = CongestionSteer()
cur = steer.target(8000, TOP, BOTTOM, 3_000_000.0, 0.0, 0.0, queue_s=0.3, offered_bps=6_000_000.0)
for t in range(1, 12):
    cur = steer.target(cur, TOP, BOTTOM, TRICKLE, 0.0, float(t), queue_s=0.0)
check("clean ticks that measured no offer raise the target no further than the capacity", round(cur) == 3000, cur)
steer = CongestionSteer()
got = steer.target(5000, TOP, BOTTOM, TRICKLE, 0.0, 0.0, queue_s=0.0)
check("nor at all with none known", got == 5000, got)

def settle(steer, cur, t, delivered=120_000.0, ceiling=None):
    """Two clean ticks half a second apart from `t` (a restore wants the second); the target after them."""
    cur = steer.target(cur, ceiling or CEILING, 135.0, delivered, 0.0, t, queue_s=0.0, offered_bps=delivered)
    first = steer.restored
    cur = steer.target(cur, ceiling or CEILING, 135.0, delivered, 0.0, t + 0.5, queue_s=0.0, offered_bps=delivered)
    return cur, first


# A stream the path carried (each case opens on clean ticks at its rate through CARRIED_S), then a stall: the desktop's 2026-11-02
# cut, a tick the gate held and a 3.4 s queue over a trickle of the target.
steer = CongestionSteer()
steer.target(5774, CEILING, 135.0, 5_196_600.0, 0.0, -3.0, queue_s=0.0, offered_bps=5_774_000.0)
steer.target(5774, CEILING, 135.0, 5_196_600.0, 0.0, -0.5, queue_s=0.0, offered_bps=5_774_000.0)
cur = steer.target(5774, CEILING, 135.0, 0.0, 0.0, 0.0, queue_s=0.15)
check("a queued tick with no delivery backs off", round(cur) == 4042, cur)
cur = steer.target(cur, CEILING, 135.0, 269_000.0, 0.0, 2.0, queue_s=3.36)
check("a deep queue over a trickle of the target cuts to the floor", round(cur) == 135, cur)
got, first = settle(steer, cur, 3.0)
check("the second tick back at the floor restores the rate the stall began at, the first waiting for the frames "
      "the stall held", not first and round(got) == 5774 and steer.restored and steer.capacity_kbps is None,
      (first, got, steer.capacity_kbps))
got = steer.target(got, CEILING, 135.0, 120_000.0, 0.0, 4.0, queue_s=0.0, offered_bps=120_000.0)
check("and restores once", round(got) == 5774 and not steer.restored, got)
# The rig's hold seen on the quiet node: a clean tick, then the frames the hold kept, acked late and together.
steer = CongestionSteer()
steer.target(8000, CEILING, 135.0, 7_200_000.0, 0.0, -3.0, queue_s=0.0, offered_bps=8_000_000.0)
steer.target(8000, CEILING, 135.0, 7_200_000.0, 0.0, -0.5, queue_s=0.0, offered_bps=8_000_000.0)
cur = steer.target(8000, CEILING, 135.0, 0.0, 0.0, 0.0, queue_s=0.04)
cur = steer.target(cur, CEILING, 135.0, 1_500_000.0, 0.0, 2.0, queue_s=0.0, offered_bps=None)
check("the first clean tick after a hold restores nothing yet", round(cur) == 5600 and not steer.restored, cur)
cur = steer.target(cur, CEILING, 135.0, 2_916_000.0, 0.0, 2.5, queue_s=2.2)
check("the hold's frames arriving behind it are the stall's queue, which keeps the restore",
      steer._restore_kbps == 8000, (cur, steer._restore_kbps))
got = cur
for t in (5.0, 6.0, 7.0, 8.0, 9.0, 10.0):
    got = steer.target(got, CEILING, 135.0, 120_000.0, 0.0, t, queue_s=0.0, offered_bps=120_000.0)
    if steer.restored:
        break
check("restored once they drained", round(got) == 8000 and steer.restored, got)

steer = CongestionSteer()
steer.target(5000, CEILING, FLOOR, 4_500_000.0, 0.0, -3.0, queue_s=0.0, offered_bps=5_000_000.0)
steer.target(5000, CEILING, FLOOR, 4_500_000.0, 0.0, -0.5, queue_s=0.0, offered_bps=5_000_000.0)
cur = steer.target(5000, CEILING, FLOOR, 2_000_000.0, 0.0, 0.0, queue_s=0.3)
got = steer.target(cur, CEILING, FLOOR, 1_700_000.0, 0.0, 1.0, queue_s=0.0, offered_bps=1_700_000.0)
check("a path that slowed (a shallow queue over its real delivery) restores nothing",
      round(got) == 1700 and not steer.restored, got)
steer = CongestionSteer()
cur = steer.target(5000, CEILING, FLOOR, 2_000_000.0, 0.0, 0.0, queue_s=1.5)
got, _ = settle(steer, cur, 6.0, 1_000_000.0)
check("nor does a deep queue over a quarter or more of the target", round(got) < 2500 and not steer.restored, got)

steer = CongestionSteer()
steer.target(5000, CEILING, 135.0, 4_500_000.0, 0.0, -3.0, queue_s=0.0, offered_bps=5_000_000.0)
steer.target(5000, CEILING, 135.0, 4_500_000.0, 0.0, -0.5, queue_s=0.0, offered_bps=5_000_000.0)
cur = steer.target(5000, CEILING, 135.0, 300_000.0, 0.0, 0.0, queue_s=2.0)
cur = steer.target(cur, CEILING, 135.0, 120_000.0, 0.0, 9.0, queue_s=0.4, offered_bps=150_000.0)
got, _ = settle(steer, cur, 20.0)
check("a stall followed by a queue over its real delivery restores nothing", round(got) < 1000, got)
steer = CongestionSteer()
steer.target(5000, CEILING, 135.0, 4_500_000.0, 0.0, -3.0, queue_s=0.0, offered_bps=5_000_000.0)
steer.target(5000, CEILING, 135.0, 4_500_000.0, 0.0, -0.5, queue_s=0.0, offered_bps=5_000_000.0)
cur = steer.target(5000, CEILING, 135.0, 300_000.0, 0.0, 0.0, queue_s=2.0)
got = steer.target(cur, CEILING, 135.0, 120_000.0, 0.0, 1.0, queue_s=None)
check("a tick that measured no delay restores nothing", round(got) == round(cur), got)
got = steer.target(cur, CEILING, 135.0, 120_000.0, 0.0, 1.5, queue_s=0.0)
got = steer.target(cur, CEILING, 135.0, 120_000.0, 0.3, 2.0, queue_s=0.0)
check("nor does a lossy one", round(got) == round(cur), got)
got = steer.target(cur, CEILING, 135.0, 120_000.0, 0.0, 3.0, queue_s=0.0)
check("a clean one after them does", round(got) == 5000 and steer.restored, got)

# The cut rig's stalls (rigs-2026-10-30/cut, 3.5 s holds over a 145 ms path): a deep queue straight after a clean
# tick, however much the window delivered, and a gated tick with nothing delivered.
steer = CongestionSteer()
steer.target(8000, CEILING, 135.0, 6_000_000.0, 0.0, -2.5, queue_s=0.0, offered_bps=6_000_000.0)
steer.target(8000, CEILING, 135.0, 6_000_000.0, 0.0, 0.0, queue_s=0.0, offered_bps=6_000_000.0)
cur = steer.target(8000, CEILING, 135.0, 6_125_000.0, 0.0, 0.5, queue_s=1.2)
got, _ = settle(steer, cur, 3.0, 200_000.0)
check("a queue a second deep straight after a clean tick is a stall, restored when the floor is back",
      round(got) == 8000 and steer.restored, got)
steer = CongestionSteer()
steer.target(8000, CEILING, 135.0, 7_200_000.0, 0.0, -3.0, queue_s=0.0, offered_bps=8_000_000.0)
steer.target(8000, CEILING, 135.0, 7_200_000.0, 0.0, -0.5, queue_s=0.0, offered_bps=8_000_000.0)
cur = steer.target(8000, CEILING, 135.0, 0.0, 0.0, 0.0, queue_s=0.1)
got = steer.target(cur, CEILING, 135.0, 200_000.0, 0.0, 1.0, queue_s=0.0, offered_bps=200_000.0)
check("a backoff with nothing delivered holds", round(got) == 5600, got)
got = steer.target(got, CEILING, 135.0, 200_000.0, 0.0, 2.5, queue_s=0.0, offered_bps=200_000.0)
check("and its target comes back on the clean ticks past the hold", round(got) == 8000 and steer.restored, got)
steer = CongestionSteer()
steer.target(8000, CEILING, 135.0, 7_200_000.0, 0.0, -3.0, queue_s=0.0, offered_bps=8_000_000.0)
steer.target(8000, CEILING, 135.0, 7_200_000.0, 0.0, -0.5, queue_s=0.0, offered_bps=8_000_000.0)
cur = steer.target(8000, CEILING, 135.0, 6_000_000.0, 0.0, 0.0, queue_s=0.05)
for t in (0.5, 1.0, 1.5, 2.0):
    cur = steer.target(cur, CEILING, 135.0, 6_000_000.0, 0.0, t, queue_s=0.3)
cur = steer.target(cur, CEILING, 135.0, 6_000_000.0, 0.0, 2.5, queue_s=1.2)
got, _ = settle(steer, cur, 6.0, 6_000_000.0)
check("a deep queue reached through queued ticks over a real delivery is the path, not restored",
      round(got) < 8000 and not steer.restored, got)
# The hold that the rig's frame round trips smear: the tick before it already read a little of the queue.
steer = CongestionSteer()
steer.target(8000, CEILING, 135.0, 7_200_000.0, 0.0, -3.0, queue_s=0.0, offered_bps=8_000_000.0)
steer.target(8000, CEILING, 135.0, 7_200_000.0, 0.0, -0.5, queue_s=0.0, offered_bps=8_000_000.0)
steer.target(8000, CEILING, 135.0, 6_000_000.0, 0.0, 0.0, queue_s=0.17)
cur = steer.target(8000, CEILING, 135.0, 6_263_000.0, 0.0, 2.5, queue_s=1.22)
got, _ = settle(steer, cur, 4.0, 200_000.0)
check("a queue that grew a second since the tick before is a stall too", round(got) == 8000 and steer.restored, got)
steer = CongestionSteer()
cur = steer.target(8000, CEILING, 135.0, 6_000_000.0, 0.0, 0.0, queue_s=1.2)
got, _ = settle(steer, cur, 4.0, 6_000_000.0)
check("a deep queue on the first tick that reads the delay, over most of the target, is not",
      round(got) < 8000 and not steer.restored, got)

# A path that did slow (the stall's trickle was its real rate): the restored target queues again, and the
# backoff on it keeps nothing, so the queue's drain is not followed by another restore.
steer = CongestionSteer()
steer.target(5000, CEILING, 135.0, 4_500_000.0, 0.0, -3.0, queue_s=0.0, offered_bps=5_000_000.0)
steer.target(5000, CEILING, 135.0, 4_500_000.0, 0.0, -0.5, queue_s=0.0, offered_bps=5_000_000.0)
cur = steer.target(5000, CEILING, 135.0, 300_000.0, 0.0, 0.0, queue_s=2.0)
cur, _ = settle(steer, cur, 1.0)
check("the stall restores", round(cur) == 5000 and steer.restored, cur)
cur = steer.target(cur, CEILING, 135.0, 300_000.0, 0.0, 2.0, queue_s=2.0)
check("the restored target queueing over the slow path backs off", round(cur) < 300, cur)
got = cur
for t in (6.0, 7.0, 8.0, 9.0):
    got = steer.target(got, CEILING, 135.0, 120_000.0, 0.0, t, queue_s=0.0, offered_bps=120_000.0)
    if steer.restored:
        break
check("and its drain restores nothing", round(got) < 1000 and not steer.restored, got)
steer = CongestionSteer()
steer.target(8000, CEILING, 135.0, 7_200_000.0, 0.0, -3.0, queue_s=0.0, offered_bps=8_000_000.0)
steer.target(8000, CEILING, 135.0, 7_200_000.0, 0.0, -0.5, queue_s=0.0, offered_bps=8_000_000.0)
cur = steer.target(8000, CEILING, 135.0, 0.0, 0.0, 0.0, queue_s=0.1)
cur, _ = settle(steer, cur, 2.5, 200_000.0)
cur = steer.target(cur, CEILING, 135.0, 0.0, 0.0, 3.5, queue_s=0.1)
got, _ = settle(steer, cur, 5.5, 200_000.0)
check("nor a gated tick straight after a restore", round(got) == 5600 and not steer.restored, got)

# The cut rig's desk switches its window every 5 s: past the hold, the switch's burst reads a shallow
# queue over a path that delivers it at once.
steer = CongestionSteer()
steer.target(8000, CEILING, 135.0, 7_200_000.0, 0.0, -3.0, queue_s=0.0, offered_bps=8_000_000.0)
steer.target(8000, CEILING, 135.0, 7_200_000.0, 0.0, -0.5, queue_s=0.0, offered_bps=8_000_000.0)
cur = steer.target(8000, CEILING, 135.0, 0.0, 0.0, 0.0, queue_s=0.04)
cur = steer.target(cur, CEILING, 135.0, 12_000_000.0, 0.0, 2.5, queue_s=0.15)
check("a burst's shallow queue the path delivered past the stall's rate keeps the restore",
      steer._restore_kbps == 8000, (cur, steer._restore_kbps))
got, _ = settle(steer, cur, 6.0, 200_000.0)
check("so the target comes back once the round trips are clean", round(got) == 8000, got)
steer = CongestionSteer()
steer.target(8000, CEILING, 135.0, 7_200_000.0, 0.0, -3.0, queue_s=0.0, offered_bps=8_000_000.0)
steer.target(8000, CEILING, 135.0, 7_200_000.0, 0.0, -0.5, queue_s=0.0, offered_bps=8_000_000.0)
cur = steer.target(8000, CEILING, 135.0, 0.0, 0.0, 0.0, queue_s=0.04)
cur = steer.target(cur, CEILING, 135.0, 3_000_000.0, 0.0, 2.5, queue_s=0.15, offered_bps=3_500_000.0)
got, _ = settle(steer, cur, 6.0, 200_000.0)
check("one delivered under it is a path that slowed, and restores nothing", round(got) < 8000 and not steer.restored,
      got)
steer = CongestionSteer()
steer.target(8000, CEILING, 135.0, 7_200_000.0, 0.0, -3.0, queue_s=0.0, offered_bps=8_000_000.0)
steer.target(8000, CEILING, 135.0, 7_200_000.0, 0.0, -0.5, queue_s=0.0, offered_bps=8_000_000.0)
cur = steer.target(8000, CEILING, 135.0, 0.0, 0.0, 0.0, queue_s=0.04)
cur = steer.target(cur, CEILING, 135.0, 3_000_000.0, 0.0, 2.5, queue_s=0.15)
got, _ = settle(steer, cur, 6.0, 200_000.0)
check("nor does the gate's release, a tick whose offer went unmeasured, take it for one", round(got) == 8000
      and steer.restored, got)

# A session started over a path slower than its rate (the noise cell's 12000 kbps into 1.2 Mbit/s): its first
# ticks are queues and gated ticks with nothing delivered, before any clean one, so there is no rate to go back to.
steer = CongestionSteer()
cur = steer.target(12000, CEILING * 2, 100.0, 0.0, 0.0, 0.0, queue_s=0.04)
cur = steer.target(cur, CEILING * 2, 100.0, 1_200_000.0, 0.0, 2.5, queue_s=3.0)
got = cur
for t in (8.0, 8.5, 9.0):
    got = steer.target(got, CEILING * 2, 100.0, 300_000.0, 0.0, t, queue_s=0.0, offered_bps=300_000.0)
check("a session that starts over a path slower than its rate restores nothing once its first queue drains",
      got < 2000 and not steer.restored and steer._restore_kbps is None, (got, steer._restore_kbps))
steer = CongestionSteer()
cur = steer.target(12000, CEILING * 2, 100.0, 900_000.0, 0.0, 0.0, queue_s=0.0, offered_bps=1_000_000.0)
cur = steer.target(cur, CEILING * 2, 100.0, 0.0, 0.0, 0.5, queue_s=0.04)
cur = steer.target(cur, CEILING * 2, 100.0, 1_200_000.0, 0.0, 3.0, queue_s=3.0)
got = cur
for t in (8.0, 8.5, 9.0):
    got = steer.target(got, CEILING * 2, 100.0, 300_000.0, 0.0, t, queue_s=0.0, offered_bps=300_000.0)
check("nor does one whose first tick read no queue before its start-up queue built (one tick carries no rate)",
      got < 2000 and not steer.restored and steer._restore_kbps is None, (got, steer._restore_kbps))

print(f"[congestion-steer] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
