#!/usr/bin/env python3
"""Burst contract of the WebRTC packet pacer.

While the link's room is unknown, short, or losing packets, or the pace is
braked, the burst budget is 5 ms of the pace. A link that transport-cc feedback
shows delivering a frame's leading burst at twice the encoder's rate or faster takes
what it delivers in 10 ms, at most libwebrtc's 40 ms of the pace under 63 KB,
so a frame leaves within the sender's own sends rather than trickling out at
the pace behind its first packets; what a frame has beyond the budget queues
and drains at the pace. The rate comes from the arrivals of each burst's
unpaced leading bytes (`burst_delivery_bps`).
"""
import asyncio
import os
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies.webrtc.pacer import (  # noqa: E402
    CLASS_VIDEO, LINK_MAX_AGE_S, MAX_BURST_BYTES, RtpPacer,
)
from selkies.webrtc.rtcdtlstransport import RTCDtlsTransport, burst_delivery_bps  # noqa: E402
from selkies.webrtc.rtp import pack_twcc_fci  # noqa: E402

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402

PACKET = b"\x80" * 1200
NARROW_8M = 2.5 * 8_000_000 / 8 * 0.005


async def frame(pacer: RtpPacer, sent: list, size: int) -> int:
    """Hand `pacer` a frame of `size` bytes on a full bucket; the packets sent at once."""
    pacer.credit = pacer._debt_cap
    pacer._last = time.monotonic()
    before = len(sent)
    for _ in range(size // len(PACKET)):
        await pacer.send(PACKET, CLASS_VIDEO)
    return len(sent) - before


async def drain(pacer: RtpPacer) -> float:
    """Wait for the queue to empty; the milliseconds it took."""
    start = time.monotonic()
    while pacer._bytes_queued and time.monotonic() - start < 2.0:
        await asyncio.sleep(0.001)
    return round((time.monotonic() - start) * 1000, 1)


def burst(start_s: float, n: int, rate_bps: float, size: int = 1200, paced_bps: float = 0.0,
          paced_from: int = 0) -> list:
    """A burst of `n` packets sent at `start_s`, arriving `rate_bps` apart, the
    ones from `paced_from` on sent and arriving at `paced_bps`."""
    out, at = [], 1e6
    for i in range(n):
        if paced_bps and i >= paced_from:
            gap = size * 8e6 / paced_bps
            out.append((start_s + (i - paced_from + 1) * gap / 1e6, at + gap, size))
            at += gap
        else:
            out.append((start_s + i * 50e-6, at, size))
            at += size * 8e6 / rate_bps
    return out


async def main_async(res: H.Results) -> None:
    sent: list = []

    async def send(data: bytes) -> None:
        sent.append(time.monotonic())

    pacer = RtpPacer(8_000_000, send)
    try:
        res.check("an unknown link gets 5 ms of the pace", pacer._debt_cap == NARROW_8M, pacer._debt_cap)
        now = await frame(pacer, sent, 20_400)
        res.check("a 20 KB frame on an unknown link sends what 5 ms covers and queues the rest",
                  now == 10 and pacer._bytes_queued == 7 * 1200, (now, pacer._bytes_queued))
        await drain(pacer)

        pacer.set_link_bps(5_500_000)
        res.check("a link slower than the pace keeps 5 ms of the pace", pacer._debt_cap == NARROW_8M,
                  pacer._debt_cap)
        pacer.set_link_bps(15_000_000)
        res.check("a link under twice the encoder's rate keeps 5 ms of the pace", pacer._debt_cap == NARROW_8M,
                  pacer._debt_cap)
        pacer.set_link_bps(16_000_000)
        res.check("a link at twice the encoder's rate takes what it delivers in 10 ms",
                  pacer._debt_cap == 20_000, pacer._debt_cap)
        pacer.set_link_bps(40_000_000)
        res.check("a link with room takes what it delivers in 10 ms", pacer._debt_cap == 50_000, pacer._debt_cap)
        pacer.set_link_bps(1e9)
        res.check("a fast link takes libwebrtc's capped burst", pacer._debt_cap == MAX_BURST_BYTES,
                  pacer._debt_cap)
        now = await frame(pacer, sent, 20_400)
        res.check("a 20 KB frame on a fast link leaves within its own sends", now == 17 and not pacer._bytes_queued,
                  (now, pacer._bytes_queued))
        now = await frame(pacer, sent, 100_800)
        res.check("a 100 KB frame sends the capped burst at once and queues the rest",
                  52 <= now <= 56 and pacer._bytes_queued > 0, (now, pacer._bytes_queued))
        ms = await drain(pacer)
        res.check("the rest drains at the pace", not pacer._bytes_queued and 8 <= ms <= 60, ms)

        pacer.set_link_bps(0.0)
        res.check("wire loss returns the budget to 5 ms of the pace", pacer._debt_cap == NARROW_8M,
                  pacer._debt_cap)
        pacer.set_link_bps(None)
        res.check("a feedback without a burst leaves the budget", pacer._debt_cap == NARROW_8M, pacer._debt_cap)

        pacer.set_link_bps(1e9)
        pacer._link_at -= LINK_MAX_AGE_S + 0.1
        pacer._last_pace_update_at -= 1.0
        pacer._maybe_recover_pace()
        res.check("an estimate older than its age returns the budget to 5 ms of the pace",
                  pacer._debt_cap == NARROW_8M, pacer._debt_cap)

        pacer.set_link_bps(1e9)
        pacer._ever_overflowed = True
        pacer._pace_bps = 10_000_000
        pacer._apply_pace()
        res.check("a braked pace keeps 5 ms of itself on a fast link", pacer._debt_cap == 10_000_000 / 8 * 0.005,
                  pacer._debt_cap)
    finally:
        await pacer.close()

    slow = RtpPacer(1_000_000, send)
    try:
        slow.set_link_bps(1e9)
        res.check("a slow pace on a fast link is bounded by 40 ms of it, not the cap",
                  slow._debt_cap == 2.5 * 1_000_000 / 8 * 0.040, slow._debt_cap)
    finally:
        await slow.close()

    fed = RtpPacer(8_000_000, send)
    try:
        tr = object.__new__(RTCDtlsTransport)
        tr._twcc_reference, tr.twcc_estimate, tr._pacer = None, None, fed
        tr._twcc_window = RTCDtlsTransport._twcc_window_zero()
        for gap_ms, lost, cap, what in ((0.0, False, MAX_BURST_BYTES, "a burst arriving at once widens the budget"),
                                        (1.75, False, NARROW_8M, "a burst queued at a 5.5 Mbps hop narrows it"),
                                        (0.0, False, MAX_BURST_BYTES, "and one arriving at once widens it again"),
                                        (0.0, True, NARROW_8M, "a feedback with wire loss narrows it")):
            tr._twcc_history = {i: (1200, time.monotonic() + i * 50e-6) for i in range(10)}
            times = [100.0 + i * gap_ms for i in range(10)]
            if lost:
                times[5] = None
            tr._twcc_process_feedback(pack_twcc_fci(0, times, 0))
            res.check(f"transport-cc feedback: {what}", fed._debt_cap == cap, fed._debt_cap)
    finally:
        await fed.close()

    rate = burst_delivery_bps(burst(0.0, 10, 5_500_000), 12_500)
    res.check("a burst queued at a 5.5 Mbps hop measures 5.5 Mbps", rate and abs(rate - 5.5e6) < 1e3, rate)
    rate = burst_delivery_bps(burst(0.0, 17, 5e9, paced_bps=20e6, paced_from=10), 12_500)
    res.check("a burst's paced tail is left out of its rate", rate and rate > 300e6, rate)
    rate = burst_delivery_bps(burst(0.0, 17, 5e9, paced_bps=20e6, paced_from=10), 30_000)
    res.check("counting the paced tail would read the pace", rate and rate < 60e6, rate)
    rate = burst_delivery_bps(burst(0.0, 10, 100e6) + burst(0.0167, 10, 5_500_000)
                              + burst(0.0334, 10, 5_500_000), 12_500)
    res.check("bursts a frame apart are measured apart, and their median taken",
              rate and abs(rate - 5.5e6) < 1e3, rate)
    res.check("fewer than four packets measure nothing",
              burst_delivery_bps(burst(0.0, 3, 5_500_000), 12_500) is None, None)


def main() -> int:
    res = H.Results("webrtc-pacer-burst")
    asyncio.run(main_async(res))
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
