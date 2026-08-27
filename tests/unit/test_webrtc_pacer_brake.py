#!/usr/bin/env python3
"""Brake contracts of the WebRTC packet pacer's rate control.

An internal queue overflow only proves injection outran the pace setting, so
it must reset the GOP without moving the pace unless fresh wire evidence (a
recent goodput estimate) sizes a brake — and then at most one brake per hold.
Recovery must keep climbing through reset churn, since only a brake resets
its clock. The failure these pin down: a single cratered feedback window used
to size every later brake while sub-second overflows starved recovery, so the
pace latched at the floor until the page was reloaded.
"""
import asyncio
import os
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies.webrtc.pacer import (  # noqa: E402
    AIMD_FLOOR_FACTOR, BRAKE_HOLD_S, GOODPUT_MAX_AGE_S, GOODPUT_WARMUP_S,
    PACE_FACTOR, RtpPacer,
)

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402

ENCODER_BPS = 8_000_000
FLOOR = int(AIMD_FLOOR_FACTOR * ENCODER_BPS)


async def main_async(res: H.Results) -> None:
    async def send(_data):
        pass

    pacer = RtpPacer(ENCODER_BPS, send)
    try:
        res.check("the pace starts at the configured ceiling",
                  pacer._pace_bps == int(PACE_FACTOR * ENCODER_BPS),
                  pacer._pace_bps)

        pacer._on_overflow()
        res.check("an overflow without an estimate does not move the pace",
                  pacer._pace_bps == int(PACE_FACTOR * ENCODER_BPS),
                  pacer._pace_bps)

        pacer._enabled_at -= GOODPUT_WARMUP_S + 1
        pacer.set_goodput_bps(374_857)
        pacer._on_overflow()
        res.check("a fresh cratered estimate brakes to the depth floor",
                  pacer._pace_bps == FLOOR, pacer._pace_bps)

        pacer._last_pace_update_at -= 1.0
        pacer._maybe_recover_pace()
        grown = pacer._pace_bps
        res.check("recovery climbs a quarter-second after the brake",
                  grown > FLOOR, grown)

        pacer._on_overflow()
        res.check("an overflow inside the hold does not re-brake",
                  pacer._pace_bps == grown, pacer._pace_bps)

        pacer._brake_hold_until = 0.0
        pacer._goodput_at -= GOODPUT_MAX_AGE_S + 1
        pacer._on_overflow()
        res.check("a stale estimate does not size a brake",
                  pacer._pace_bps == grown, pacer._pace_bps)

        for _ in range(10):
            pacer._last_pace_update_at -= 0.3
            pacer._maybe_recover_pace()
            pacer._gop_dead = False
            pacer._reset_gop()
        res.check("the pace outgrows reset churn with no wire evidence",
                  pacer._pace_bps > 2 * FLOOR, pacer._pace_bps)
        res.check("each churn reset still requested a keyframe path",
                  pacer.stats["gop_resets"] >= 10, pacer.stats["gop_resets"])

        pacer.set_goodput_bps(4_000_000)
        pacer._brake_hold_until = 0.0
        before = pacer._pace_bps
        pacer._on_overflow()
        res.check("a fresh estimate past the hold brakes again",
                  pacer._pace_bps == 4_000_000 - 500_000
                  and pacer._pace_bps < before,
                  pacer._pace_bps)
        now = time.monotonic()
        res.check("that brake re-arms the hold for BRAKE_HOLD_S",
                  now < pacer._brake_hold_until <= now + BRAKE_HOLD_S,
                  pacer._brake_hold_until - now)
    finally:
        await pacer.close()


def main() -> int:
    res = H.Results("webrtc-pacer-brake")
    asyncio.run(main_async(res))
    ok = res.summary()
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
