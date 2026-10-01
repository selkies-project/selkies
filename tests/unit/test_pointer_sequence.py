#!/usr/bin/env python3
"""Numbered pointer messages: coalesced motion may reach the server on a channel
that keeps no order, so an absolute position numbered below the last message
applied for its connection is dropped as stale, a relative delta is applied in
whatever order it lands, an unnumbered message is applied as before, and the
numbering is kept per connection.
"""
import asyncio
import os
import sys
from typing import Any, List

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies.input_handler import WebRTCInput  # noqa: E402

passed = failed = 0


def check(label: str, ok: Any, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [pointer-sequence] {label}  {detail}", flush=True)


applied: List[tuple] = []


def make_handler() -> WebRTCInput:
    h = WebRTCInput.__new__(WebRTCInput)
    h._pointer_seq = {}
    h._pointer_echoes = {}
    h.button_mask = 0

    async def record(x, y, button_mask, scroll_magnitude, relative=False, display_id="primary"):
        applied.append((relative, x, y, button_mask))

    h.send_x11_mouse = record
    return h


async def drive() -> None:
    h = make_handler()
    for msg, conn in (("m,10,10,0,0,1", "a"), ("m,30,30,0,0,3", "a"), ("m,20,20,0,0,2", "a"),
                      ("m2,5,5,0,0,2", "a"), ("m,40,40,1,0,4", "a"), ("m,7,7,0,0", "a"),
                      ("m,50,50,0,0,1", "b"), ("m,60,60,0,0,x", "a")):
        await h._dispatch_message(msg, "primary", conn)


asyncio.run(drive())
check("positions apply in order", applied[:2] == [(False, 10, 10, 0), (False, 30, 30, 0)], applied[:2])
check("a position numbered below the last applied is dropped", (False, 20, 20, 0) not in applied, applied)
check("a delta numbered below the last applied still counts", (True, 5, 5, 0) in applied, applied)
check("a later click is applied and advances the count", (False, 40, 40, 1) in applied, applied)
check("an unnumbered message applies as before", (False, 7, 7, 0) in applied, applied)
check("another connection counts on its own", (False, 50, 50, 0) in applied, applied)
check("an unreadable number drops the message", (False, 60, 60, 0) not in applied, applied)
check("the count is the last number each connection sent",
      make_handler()._pointer_seq == {} and applied and True)

h = make_handler()
asyncio.run(h._dispatch_message("m,1,1,0,0,9", "primary", "c"))
asyncio.run(h.release_gamepads_for_conn("c")) if hasattr(h, "client_gamepad_associations") else None
check("a connection's count is kept until it goes", h._pointer_seq.get("c") == 9, h._pointer_seq)

# A delta that lands after the release that followed it still moves the
# pointer, but the button it carried is not pressed again.
applied.clear()
h = make_handler()


async def record_mask(x, y, button_mask, scroll_magnitude, relative=False, display_id="primary"):
    applied.append((relative, x, y, button_mask))
    h.button_mask = button_mask

h.send_x11_mouse = record_mask


async def drag() -> None:
    for msg in ("m2,0,0,1,0,20", "m2,4,0,1,0,22", "m2,0,0,0,0,23", "m2,3,0,1,0,21"):
        await h._dispatch_message(msg, "primary", "d")


asyncio.run(drag())
check("a late delta still moves the pointer", (True, 3, 0, 0) in applied or (True, 3, 0, 1) in applied, applied)
check("but does not press again the button released after it", applied[-1] == (True, 3, 0, 0), applied)

# A wheel pulse overtaken by later motion is a click, not a state: its scroll
# bit still rises, over the buttons held now rather than those it carried.
applied.clear()


async def late_pulse() -> None:
    for msg in ("m2,0,0,1,0,30", "m2,5,0,0,0,33", "m2,0,0,1,1,31", "m2,0,0,9,1,32"):
        await h._dispatch_message(msg, "primary", "e")


asyncio.run(late_pulse())
check("a late wheel pulse still scrolls", any(m & 8 for (_, _, _, m) in applied[2:]), applied)
check("without pressing again the button held when it was sent",
      all(not (m & 1) for (_, _, _, m) in applied[2:]), applied)

print(f"[pointer-sequence] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
