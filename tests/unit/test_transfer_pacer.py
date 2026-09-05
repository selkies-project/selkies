#!/usr/bin/env python3
"""Transfer-pacing contracts. The token bucket must deliver the configured
rate (not a multiple of it), the congestion step must arm its recovery
ceiling once per epoch and probe multiplicatively only before the first
congestion, a pacer with neither a cap nor a gauge must not throttle, and
the file_transfer_limit_mbps setting must neutralize unusable values.

Uploads and downloads alike are gauged end to end over the client's session
socket (`UplinkGauge`): the gauge times its own pings against their pongs,
and a round trip inflating past a per-session floor is the congestion
verdict the direction's pacer backs off on, so a transfer takes what the
link has spare and yields the moment the session's own delay rises. A
gauged transfer's chunks are sized to its rate so its own bursts stay under
the gauge's threshold. The floor is a minute-bucketed minimum so a route
change re-baselines instead of reading as permanent congestion, and a
session socket that dies mid-transfer takes the gauge with it — nothing is
left to protect.
"""
import asyncio
import os
import subprocess
import sys
import time

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))

from collections import deque  # noqa: E402

from selkies import stream_server  # noqa: E402
from selkies.stream_server import (  # noqa: E402
    TRANSFER_CHUNK_MAX_BYTES, TRANSFER_CHUNK_MIN_BYTES, TransferPacer,
    UplinkGauge, _gauged_chunk_size, _observe_rtt_floor,
)

passed = failed = 0


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [pacer] {label}  {detail}", flush=True)


async def timed_pace(pacer: TransferPacer, total_bytes: int,
                     chunk: int = TransferPacer._CHUNK) -> float:
    start = time.monotonic()
    sent = 0
    while sent < total_bytes:
        n = min(chunk, total_bytes - sent)
        await pacer.pace(n)
        sent += n
    return time.monotonic() - start


# A static cap must deliver the configured rate: the initial burst is half a
# second of budget, and everything past it drains at rate_bps. A bucket that
# forgives a slept-off deficit instead delivers twice the cap.
rate = 2 * 1024 * 1024
pacer = TransferPacer(static_bps=rate, adaptive=False)
sent = 3 * 1024 * 1024
expected = (sent - rate * 0.5) / rate
elapsed = asyncio.run(timed_pace(pacer, sent))
check("static cap delivers the configured rate",
      expected * 0.85 <= elapsed <= expected * 1.4,
      f"elapsed={elapsed:.2f}s expected~{expected:.2f}s")

# Clear samples before any congestion ramp multiplicatively (an unknown link
# rate is discovered in chunks, not minutes).
pacer = TransferPacer(adaptive=True)
r0 = pacer.rate_bps
for _ in range(6):
    pacer._gauge_backoff(congested=False, clear=True, cut=0.6)
check("pre-congestion ramp is multiplicative", pacer.rate_bps >= r0 * 8,
      f"{r0} -> {pacer.rate_bps}")

# The first congested sample of an epoch arms the ceiling at the pre-cut rate
# and cuts. A congested sample inside the drain window is the queue that cut
# is already draining: it neither cuts again nor re-arms. Past the window a
# link still congested is cut again, in the same epoch.
pacer.rate_bps = r_at_cong = 1024 * 1024
pacer._gauge_backoff(congested=True, clear=False, cut=0.6)
armed = pacer._probe_ceiling
cut_rate = pacer.rate_bps
pacer._gauge_backoff(congested=True, clear=False, cut=0.6)
check("ceiling arms once per epoch at the pre-cut rate",
      armed == max(r_at_cong, 2 * TransferPacer._RATE_FLOOR)
      and pacer._probe_ceiling == armed,
      f"armed={armed} rate_at_cong={r_at_cong}")
check("a congested sample inside the drain window does not cut again",
      pacer.rate_bps == cut_rate, f"{cut_rate} -> {pacer.rate_bps}")
pacer._hold_until = 0.0
pacer._gauge_backoff(congested=True, clear=False, cut=0.6)
check("a congested sample past the window cuts again without re-arming",
      pacer.rate_bps < cut_rate and pacer._probe_ceiling == armed,
      f"{cut_rate} -> {pacer.rate_bps}, ceiling={pacer._probe_ceiling}")

# A clear sample inside the post-cut drain window must not grow the rate:
# the queue behind the cut needs time to drain before probing resumes.
held_rate = pacer.rate_bps
pacer._gauge_backoff(congested=False, clear=True, cut=0.6)
check("post-cut drain window holds the rate", pacer.rate_bps == held_rate,
      f"{held_rate} -> {pacer.rate_bps}")

# A congested sample after a clear that fell inside the drain window is the
# same epoch: the ceiling must keep its armed value, not re-arm from the
# freshly cut rate (an oscillating gauge would otherwise ratchet it down).
pacer._gauge_backoff(congested=True, clear=False, cut=0.6)
check("oscillation inside the drain window does not re-arm the ceiling",
      pacer._probe_ceiling == armed, f"{pacer._probe_ceiling} vs armed={armed}")
pacer._hold_until = 0.0

# After congestion (and the drain window), growth is additive; reaching the
# ceiling releases it so probing continues past the last congested rate.
grew = []
for _ in range(200):
    before = pacer.rate_bps
    pacer._gauge_backoff(congested=False, clear=True, cut=0.6)
    grew.append(pacer.rate_bps - before)
    if pacer._probe_ceiling is None:
        break
check("post-congestion recovery is additive",
      grew and all(0 < g <= 8 * 1024 for g in grew), f"steps={grew[:3]}...")
check("reaching the ceiling releases it", pacer._probe_ceiling is None,
      f"ceiling={pacer._probe_ceiling}")
after_release = pacer.rate_bps
pacer._gauge_backoff(congested=False, clear=True, cut=0.6)
check("probing continues past the released ceiling",
      pacer.rate_bps > after_release and pacer.rate_bps > armed,
      f"{after_release} -> {pacer.rate_bps}")

# Past the window, the gauge's inflation tells a standing queue from one
# already draining: the first is cut for in full, the second by a quarter.
pacer = TransferPacer(adaptive=True)
pacer.rate_bps = 1024 * 1024
pacer._gauge_backoff(congested=True, clear=False, cut=0.6, inflation_us=200_000)
pacer._hold_until = 0.0
r_standing = pacer.rate_bps
pacer._gauge_backoff(congested=True, clear=False, cut=0.6, inflation_us=200_000)
standing_cut = pacer.rate_bps / r_standing
pacer._hold_until = 0.0
r_draining = pacer.rate_bps
pacer._gauge_backoff(congested=True, clear=False, cut=0.6, inflation_us=120_000)
draining_cut = pacer.rate_bps / r_draining
check("a standing queue past the window takes the full cut",
      abs(standing_cut - 0.6) < 1e-6, f"cut={standing_cut:.3f}")
check("a draining queue past the window takes a quarter of the cut",
      abs(draining_cut - 0.9) < 1e-6, f"cut={draining_cut:.3f}")

# The shared cap pacer with no cap set is inactive: a transfer with no
# session socket to gauge rides it alone, and must pass unthrottled rather
# than being pinned at some initial allowance.
pacer = TransferPacer()
elapsed = asyncio.run(timed_pace(pacer, 64 * 1024 * 1024))
check("an unset cap does not throttle", not pacer.active and elapsed < 0.5,
      f"elapsed={elapsed:.2f}s active={pacer.active}")

# A gauged transfer's chunk follows its pacer's rate, clamped: a fixed chunk
# would burst past the gauge's delay budget on a slow link, and an unbounded
# one would pin a fast link's thread hops to a single huge read.
pacer = TransferPacer(adaptive=True)
pacer.rate_bps = 8 * 1024 * 1024
mid = _gauged_chunk_size(pacer)
pacer.rate_bps = 1024
small = _gauged_chunk_size(pacer)
pacer.rate_bps = 1 << 30
large = _gauged_chunk_size(pacer)
check("chunk size follows the rate within its clamps",
      small == TRANSFER_CHUNK_MIN_BYTES and large == TRANSFER_CHUNK_MAX_BYTES
      and TRANSFER_CHUNK_MIN_BYTES < mid < TRANSFER_CHUNK_MAX_BYTES
      and mid == int(8 * 1024 * 1024 * stream_server.TRANSFER_CHUNK_DELAY_BUDGET),
      f"small={small} mid={mid} large={large}")


# file_transfer_limit_mbps: negative and non-finite values must resolve to 0
# (pacing off) rather than throttling to garbage or crashing at boot.
BASE_ENV = {k: v for k, v in os.environ.items() if not k.startswith("SELKIES_")}


def probe_limit(value: str) -> str:
    out = subprocess.run(
        [sys.executable, "-c",
         "import selkies.settings as s; print(s.settings.file_transfer_limit_mbps)"],
        capture_output=True, text=True, timeout=120,
        env=dict(BASE_ENV, PYTHONPATH=os.path.join(REPO, "src"),
                 SELKIES_FILE_TRANSFER_LIMIT_MBPS=value))
    return out.stdout.strip()


check("negative limit clamps to 0 (pacing off)", probe_limit("-5") == "0.0",
      probe_limit("-5"))
check("nan limit clamps to 0 (pacing off)", probe_limit("nan") == "0.0",
      probe_limit("nan"))

# The gauged leg both directions use: an externally supplied verdict drives
# the AIMD, and a chunk with no fresh sample must hold the rate rather than
# growing or cutting on stale information.
pacer = TransferPacer(adaptive=True)
r0 = pacer.rate_bps
asyncio.run(pacer.pace_verdict(1024, None))
check("a sampleless verdict holds the rate", pacer.rate_bps == r0,
      f"{r0} -> {pacer.rate_bps}")
asyncio.run(pacer.pace_verdict(1024, False))
check("a clear verdict grows the rate", pacer.rate_bps > r0,
      f"{r0} -> {pacer.rate_bps}")
before = pacer.rate_bps
asyncio.run(pacer.pace_verdict(1024, True))
check("a congested verdict cuts the rate", pacer.rate_bps < before,
      f"{before} -> {pacer.rate_bps}")

# The verdict leg still drains the token bucket: with the rate pinned (no
# fresh samples), delivery must match it, not bypass it.
rate = 2 * 1024 * 1024
pacer = TransferPacer(static_bps=rate, adaptive=False)
sent = 3 * 1024 * 1024
expected = (sent - rate * 0.5) / rate


async def timed_verdict(pacer: TransferPacer, total_bytes: int) -> float:
    start = time.monotonic()
    done = 0
    while done < total_bytes:
        n = min(TransferPacer._CHUNK, total_bytes - done)
        await pacer.pace_verdict(n, None)
        done += n
    return time.monotonic() - start


elapsed = asyncio.run(timed_verdict(pacer, sent))
check("the verdict leg delivers the bucket's rate",
      expected * 0.85 <= elapsed <= expected * 1.4,
      f"elapsed={elapsed:.2f}s expected~{expected:.2f}s")

# UplinkGauge: verdicts come from timing the gauge's own pings against their
# pongs. No pong yet is no verdict, a prompt pong reads clear, a round trip
# inflated past the floor reads congested, a pong the clock never sent is
# ignored, and a session socket whose ping fails takes the gauge with it.
class FakeSessionWS:
    def __init__(self):
        self.pings = []
        self.fail = False

    async def ping(self, payload=b""):
        if self.fail:
            raise ConnectionResetError()
        self.pings.append(payload)


ws = FakeSessionWS()
state = stream_server._uplink_session_state(ws)
gauge = UplinkGauge([[ws, state, state["seq"]]])
check("no pong yet gives no verdict", asyncio.run(gauge.sample()) is None, "")
stream_server.note_pong(ws, ws.pings[-1])
check("a prompt pong reads clear", asyncio.run(gauge.sample()) is False, "")
gauge._last_ping = 0.0
asyncio.run(gauge.sample())
late = ws.pings[-1]
state["pending"][late] = time.monotonic() - (
    UplinkGauge.INFLATION_US + 10_000) / 1e6
stream_server.note_pong(ws, late)
check("an inflated round trip reads congested",
      asyncio.run(gauge.sample()) is True, "")
seq_before = state["seq"]
stream_server.note_pong(ws, b"never-sent")
check("a pong the clock never sent is ignored",
      state["seq"] == seq_before and asyncio.run(gauge.sample()) is None, "")
ws.fail = True
gauge._last_ping = 0.0
asyncio.run(gauge.sample())
check("a dead session socket takes the gauge with it", not gauge.alive, "")

# The RTT floor is the minimum over a bounded history of minute buckets: it
# must hold through jitter, and a route change onto a longer path must age
# out of it rather than reading as permanent congestion.
floor_state = {"buckets": deque(maxlen=UplinkGauge.FLOOR_BUCKETS)}
_observe_rtt_floor(floor_state, 5_000, now=0.0)
floor = _observe_rtt_floor(floor_state, 9_000, now=1.0)
check("the floor holds through higher samples", floor == 5_000, f"{floor}")
for i in range(1, UplinkGauge.FLOOR_BUCKETS + 1):
    floor = _observe_rtt_floor(
        floor_state, 20_000, now=i * UplinkGauge.FLOOR_BUCKET_SECONDS)
check("a route change re-baselines the floor", floor == 20_000, f"{floor}")

print(f"[pacer] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
