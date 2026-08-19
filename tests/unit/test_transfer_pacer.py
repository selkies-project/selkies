#!/usr/bin/env python3
"""Download-pacing contracts. The token bucket must deliver the configured
rate (not a multiple of it), the congestion step must arm its recovery
ceiling once per epoch and probe multiplicatively only before the first
congestion, a connection with no usable gauge must not be throttled blindly,
and the file_transfer_limit_mbps setting must neutralize unusable values.
"""
import asyncio
import os
import subprocess
import sys
import time
from typing import Optional

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))

from selkies import stream_server  # noqa: E402
from selkies.stream_server import TransferPacer  # noqa: E402

passed = failed = 0

# Both gauge probes fail on a plain object, so it stands in for a download
# socket the pacer cannot gauge.
UNGAUGED_SOCK = object()


def check(label: str, ok, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [pacer] {label}  {detail}", flush=True)


async def timed_pace(pacer: TransferPacer, total_bytes: int,
                     chunk: int = TransferPacer._CHUNK, sock=UNGAUGED_SOCK,
                     conn: Optional[dict] = None) -> tuple:
    if conn is None:
        conn = pacer.connection_state()
    start = time.monotonic()
    sent = 0
    while sent < total_bytes:
        n = min(chunk, total_bytes - sent)
        await pacer.pace(sock, n, conn)
        sent += n
    return time.monotonic() - start, conn


# A static cap must deliver the configured rate: the initial burst is half a
# second of budget, and everything past it drains at rate_bps. The pre-fix
# bucket forgave every slept-off deficit and delivered double the cap.
rate = 2 * 1024 * 1024
pacer = TransferPacer(static_bps=rate, adaptive=False)
sent = 3 * 1024 * 1024
expected = (sent - rate * 0.5) / rate
elapsed, _ = asyncio.run(timed_pace(pacer, sent))
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

# The first congested sample of an epoch arms the ceiling at the pre-cut rate;
# further congested samples keep cutting without re-arming.
pacer.rate_bps = r_at_cong = 1024 * 1024
pacer._gauge_backoff(congested=True, clear=False, cut=0.6)
armed = pacer._probe_ceiling
pacer._gauge_backoff(congested=True, clear=False, cut=0.6)
check("ceiling arms once per epoch at the pre-cut rate",
      armed == max(r_at_cong, 2 * TransferPacer._RATE_FLOOR)
      and pacer._probe_ceiling == armed,
      f"armed={armed} rate_at_cong={r_at_cong}")

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

# A connection with neither the queue ioctl nor TCP RTT gives adaptive pacing
# nothing to react to: it must pass unthrottled instead of being pinned at the
# initial allowance. (The static cap path above still paces such connections.)
_saved_outq, _saved_rtt = stream_server._sock_unsent_bytes, stream_server._sock_rtt_us
stream_server._sock_unsent_bytes = lambda sock: None
stream_server._sock_rtt_us = lambda sock: None
try:
    pacer = TransferPacer(adaptive=True)
    elapsed, conn = asyncio.run(timed_pace(pacer, 64 * 1024 * 1024))
    check("gaugeless connection is not throttled blindly",
          elapsed < 0.5 and conn["gauged"] is False,
          f"elapsed={elapsed:.2f}s gauged={conn['gauged']}")
finally:
    stream_server._sock_unsent_bytes, stream_server._sock_rtt_us = _saved_outq, _saved_rtt

# The RTT floor lives per connection: one download's short path must not turn
# another's base RTT into permanent congestion.
pacer = TransferPacer(adaptive=True)
check("rtt floor is per-connection state",
      pacer.connection_state()["rtt_floor_us"] is None
      and "rtt_floor_us" not in vars(pacer), "")

# Upload reads pace through connection_state(gauged=False) with no socket:
# adaptive-only mode must leave them unpaced (the client's uplink queue is
# invisible to the server), while a static cap paces them from the same
# shared bucket as downloads.
pacer = TransferPacer(adaptive=True)
elapsed, _ = asyncio.run(timed_pace(
    pacer, 64 * 1024 * 1024, sock=None,
    conn=pacer.connection_state(gauged=False)))
check("ungauged upload passes adaptive-only mode unpaced", elapsed < 0.5,
      f"elapsed={elapsed:.2f}s")
rate = 4 * 1024 * 1024
pacer = TransferPacer(static_bps=rate, adaptive=True)
sent = 3 * 1024 * 1024
expected = (sent - rate * 0.5) / rate
elapsed, _ = asyncio.run(timed_pace(
    pacer, sent, sock=None, conn=pacer.connection_state(gauged=False)))
check("static cap paces ungauged uploads at the configured rate",
      expected * 0.85 <= elapsed <= expected * 1.4,
      f"elapsed={elapsed:.2f}s expected~{expected:.2f}s")


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

print(f"[pacer] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
