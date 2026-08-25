#!/usr/bin/env python3
"""What the server tracks while a client sends pointer deltas.

Relative motion is injected as the delta itself, so the position the handler
keeps alongside it exists only to answer the next absolute message. It has to
stay inside the laid-out screen, because that is where the X server and the
compositor keep the pointer it is describing, and it has to be treated as an
estimate anyway: the framebuffer can be wider than the layout, and an
application can move the pointer without telling anyone. A backend with no
relative injection of its own reads that position directly, which is what
makes an unbounded one visible as a pointer stuck against an edge.
"""
import asyncio
import os
import sys
from typing import Any, List, Optional, Tuple

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))

from selkies.display_utils import layout_extent  # noqa: E402
from selkies.input_handler import WebRTCInput, MOUSE_MOVE, MOUSE_POSITION  # noqa: E402

results: List[Tuple[str, bool]] = []


def check(label: str, ok: Any, detail: str = "") -> None:
    results.append((label, bool(ok)))
    print(f"{'PASS' if ok else 'FAIL'}  [pointer-tracking] {label}  {detail}", flush=True)


class Layouts:
    """A data server that only knows how the displays are laid out."""

    def __init__(self, layouts: Optional[dict]) -> None:
        self.display_layouts = layouts if layouts is not None else {}


class WaylandStub:
    """A capture backend, with or without its own relative injection."""

    def __init__(self, relative: bool) -> None:
        self.moves: List[tuple] = []
        self.buttons: List[tuple] = []
        if relative:
            self.inject_relative_mouse_move = self._relative

    def _relative(self, dx: float, dy: float) -> None:
        self.moves.append(("relative", dx, dy))

    def inject_mouse_move(self, x: float, y: float) -> None:
        self.moves.append(("absolute", x, y))

    def inject_mouse_button(self, button: int, state: int) -> None:
        self.buttons.append((button, state))

    def inject_mouse_scroll(self, dx: float, dy: float) -> None:
        pass


def make_handler(layouts: Optional[dict] = None,
                 wayland: Optional[WaylandStub] = None) -> WebRTCInput:
    """A handler with only the pointer state `send_x11_mouse` reads."""
    h = WebRTCInput.__new__(WebRTCInput)
    h.button_mask = 0
    h.last_x = -1
    h.last_y = -1
    h.tracked_position_stale = False
    h.data_server_instance = Layouts(layouts)
    h.wayland_input = wayland
    h.uinput_mouse_socket_path = None
    h.xdisplay = None
    h.mouse = None
    h.sent: List[tuple] = []
    h.send_mouse = lambda action, data: h.sent.append((action, data))
    return h


def send(h: WebRTCInput, x: int, y: int, mask: int = 0, relative: bool = False) -> None:
    asyncio.run(h.send_x11_mouse(x, y, mask, 0, relative))


SINGLE = {"primary": {"x": 0, "y": 0, "w": 1920, "h": 1080}}
DUAL = {"primary": {"x": 0, "y": 0, "w": 1920, "h": 1080},
        "second": {"x": 1920, "y": 0, "w": 1280, "h": 720}}


def main() -> None:
    check("one display spans its own size", layout_extent(SINGLE) == (1920, 1080),
          f"{layout_extent(SINGLE)}")
    check("two displays span their union", layout_extent(DUAL) == (3200, 1080),
          f"{layout_extent(DUAL)}")
    check("no layout has no extent", layout_extent({}) == (0, 0) and layout_extent(None) == (0, 0))
    check("a null field costs only its own axis",
          layout_extent({"primary": {"x": 0, "y": 0, "w": 1920, "h": None}}) == (1920, 0),
          f"{layout_extent({'primary': {'x': 0, 'y': 0, 'w': 1920, 'h': None}})}")

    # A backend with no relative injection reads the tracked position.
    wl = WaylandStub(relative=False)
    h = make_handler(SINGLE, wl)
    send(h, 900, 500)
    send(h, 4000, 0, relative=True)
    send(h, -100, 0, relative=True)
    check("a delta past the edge leaves the pointer one step back from it",
          wl.moves[-1] == ("absolute", 1819.0, 500.0), f"{wl.moves[-1]}")
    check("the fallback injected positions, not deltas",
          all(m[0] == "absolute" for m in wl.moves), f"{wl.moves}")

    # The same drag with relative injection available.
    wl = WaylandStub(relative=True)
    h = make_handler(SINGLE, wl)
    send(h, 900, 500)
    send(h, 4000, 0, relative=True)
    check("a backend with relative injection gets the delta verbatim",
          wl.moves[-1] == ("relative", 4000.0, 0.0), f"{wl.moves[-1]}")
    check("the tracked position still stops at the edge",
          h.last_x == 1919, f"{h.last_x}")

    # A button change carries no motion.
    wl = WaylandStub(relative=True)
    h = make_handler(SINGLE, wl)
    send(h, 900, 500)
    before = len(wl.moves)
    send(h, 0, 0, mask=1, relative=True)
    check("wayland: a zero delta injects no motion", len(wl.moves) == before,
          f"{wl.moves[before:]}")
    check("wayland: the button still goes down", wl.buttons == [(272, 1)], f"{wl.buttons}")

    h = make_handler(SINGLE)
    send(h, 900, 500)
    before = len(h.sent)
    send(h, 0, 0, mask=1, relative=True)
    check("x11: a zero delta injects no motion",
          all(action != MOUSE_MOVE for action, _ in h.sent[before:]),
          f"{h.sent[before:]}")

    # The first absolute after a delta warps.
    h = make_handler(SINGLE)
    send(h, 900, 500)
    send(h, 300, 0, relative=True)
    send(h, -300, 0, relative=True)
    before = len(h.sent)
    send(h, 900, 500)
    warps = [data for action, data in h.sent[before:] if action == MOUSE_POSITION]
    check("an absolute position after deltas warps even where it was tracked",
          warps == [(900, 500)], f"{h.sent[before:]}")
    before = len(h.sent)
    send(h, 900, 500)
    check("a repeat of it does not warp again",
          not [1 for action, _ in h.sent[before:] if action == MOUSE_POSITION],
          f"{h.sent[before:]}")

    # The wire's own limits.
    h = make_handler(SINGLE)
    send(h, 900, 500)
    before = len(h.sent)
    send(h, 99999, 0, relative=True)
    moves = [data for action, data in h.sent[before:] if action == MOUSE_MOVE]
    check("a delta too large for the request is saturated, not dropped",
          moves == [(32767, 0)], f"{moves}")

    h = make_handler({})
    send(h, 900, 500)
    send(h, 4000, 0, relative=True)
    check("no layout means no bound, and no failure", h.last_x == 4900, f"{h.last_x}")

    h = make_handler(DUAL)
    send(h, 900, 500)
    send(h, 4000, 0, relative=True)
    check("the bound follows the whole extended desktop", h.last_x == 3199, f"{h.last_x}")

    failed = [label for label, ok in results if not ok]
    print(f"[pointer-tracking] {len(results) - len(failed)}/{len(results)} passed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
