#!/usr/bin/env python3
"""A touchpad's scroll as a finger's (`sf,<dx>,<dy>`, `sfe`): a Wayland session
whose pixelflux injects one takes the travel and the lift, bounded and finite,
behind any key still waiting in the keyboard worker as a wheel click is; a
session without one, X11 among them, takes neither, and says so in the display
config the page reads (`finger_scroll`). A read-only viewer may not send it, a
collaborator may.
"""
import asyncio
import os
import sys
from typing import Any, List

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies import input_handler  # noqa: E402
from selkies.input_handler import WebRTCInput  # noqa: E402
from selkies.webrtc_mode import WebRTCService  # noqa: E402
from selkies.websockets_mode import DataStreamingServer  # noqa: E402

passed = failed = 0


def check(label: str, ok: Any, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [finger-scroll] {label}  {detail}", flush=True)


class Seat:
    def __init__(self) -> None:
        self.calls: List[tuple] = []

    def inject_finger_scroll(self, dx: float, dy: float) -> None:
        self.calls.append(("scroll", dx, dy))

    def inject_finger_scroll_end(self) -> None:
        self.calls.append(("end",))


class OlderSeat:
    def inject_mouse_scroll(self, x: float, y: float) -> None:
        raise AssertionError("a wheel click is not a finger's scroll")


def make_handler(wayland: bool = True, seat: Any = None) -> WebRTCInput:
    h = WebRTCInput.__new__(WebRTCInput)
    h.is_wayland = wayland
    h.wayland_input = seat
    h.keyboard_queue = asyncio.Queue()
    h._keyboard_busy = False
    h.queued: List[tuple] = []
    h._keyboard_enqueue = h.queued.append
    return h


def display_configs(handler: Any) -> List[dict]:
    """The display config each transport sends, with only the state it reads."""
    ws = object.__new__(DataStreamingServer)
    ws.display_clients = {"primary": {}}
    ws.display_layouts = {}
    wr = object.__new__(WebRTCService)
    wr.display_clients = {}
    wr.display_layouts = {}
    wr._client_scales = {}
    wr._client_stream_boxes = {}
    ws.input_handler = wr.input_handler = handler
    return [ws._display_config_payload(), wr._display_config_payload()]


async def main() -> None:
    seat = Seat()
    h = make_handler(seat=seat)
    check("a Wayland session with the injection takes a finger's scroll", h.finger_scroll_available())
    await h._dispatch_message("sf,1.5,-2.25", "primary", "a")
    await h._dispatch_message("sfe", "primary", "a")
    check("the travel and the lift reach the seat", seat.calls == [("scroll", 1.5, -2.25), ("end",)], seat.calls)
    seat.calls.clear()
    await h._dispatch_message("sf,1e9,-1e9", "primary", "a")
    check("a travel past the bound is held to it",
          seat.calls == [("scroll", input_handler.FINGER_SCROLL_MAX_PX, -input_handler.FINGER_SCROLL_MAX_PX)],
          seat.calls)
    seat.calls.clear()
    for bad in ("sf,nan,1", "sf,inf,1", "sf,x,1", "sf,1"):
        await h._dispatch_message(bad, "primary", "a")
    check("a travel that is not two finite numbers is dropped", seat.calls == [], seat.calls)

    h._keyboard_busy = True
    await h._dispatch_message("sf,4,0", "primary", "a")
    await h._dispatch_message("sfe", "primary", "a")
    check("behind a key the keyboard worker has yet to type, both wait their turn there",
          h.queued == [("finger_scroll", (4.0, 0.0)), ("finger_scroll", None)] and seat.calls == [],
          (h.queued, seat.calls))
    h._keyboard_busy = False
    for _, delta in h.queued:
        h._inject_finger_scroll(delta)
    check("and the worker gives them to the seat in order", seat.calls == [("scroll", 4.0, 0.0), ("end",)],
          seat.calls)

    x11 = make_handler(wayland=False, seat=Seat())
    await x11._dispatch_message("sf,1,1", "primary", "a")
    await x11._dispatch_message("sfe", "primary", "a")
    check("X11 takes no finger's scroll", not x11.finger_scroll_available() and x11.wayland_input.calls == [],
          x11.wayland_input.calls)
    older = make_handler(seat=OlderSeat())
    await older._dispatch_message("sf,1,1", "primary", "a")
    check("nor does a pixelflux without the injection", not older.finger_scroll_available())

    check("both transports' display config tells the page it may send one",
          all(c["finger_scroll"] is True for c in display_configs(h)))
    check("and that it may not on X11, with an older pixelflux, or before the input starts",
          not any(c["finger_scroll"] for handler in (x11, older, None) for c in display_configs(handler)))


asyncio.run(main())

check("a collaborator may send a finger's scroll",
      "sf,1,1".startswith(input_handler.VIEWER_COLLAB_EXTRA_PREFIXES)
      and "sfe".startswith(input_handler.VIEWER_COLLAB_EXTRA_PREFIXES))
check("a read-only viewer may not", not "sf,1,1".startswith(input_handler.VIEWER_ALLOWED_PREFIXES))

print(f"[finger-scroll] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
