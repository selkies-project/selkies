#!/usr/bin/env python3
"""The pointer echo a trackpad page draws its cursor from (`_pointer_echo`): a
subscriber is sent the position at once, mapped onto the display it is on. The
page draws the pointer at the echo moved on by its own deltas, so later echoes
go out only where the pointer is not where the page draws it: a move the page
predicts sends nothing, one it cannot (a warp, another connection's move, a gain
the page does not apply, another display) is echoed at most once per
POINTER_ECHO_MIN_S, with a trailing echo for the end of the motion. Each echo
names the last of the page's pointer messages its position includes: on Wayland
the compositor's move numbers say which moves it has applied, on X11 every move
sent is. X11 compares the position it tracks while the pointer moves and asks
the server once it rests, and the tracking continues from that answer. A
resting pointer is read again, so an application's warp reaches the page; a
session that cannot say where its pointer is answers `pointer,none`, and an echo
stops with its connection.
"""
import asyncio
import os
import sys
import threading
from typing import Any, List, Optional, Tuple

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
from selkies import input_handler  # noqa: E402
from selkies.input_handler import WebRTCInput  # noqa: E402

passed = failed = 0


def check(label: str, ok: Any, detail="") -> None:
    global passed, failed
    if ok:
        passed += 1
    else:
        failed += 1
    print(f"{'PASS' if ok else 'FAIL'}  [pointer-echo] {label}  {detail}", flush=True)


class Transport:
    mode = "websockets"

    def __init__(self) -> None:
        self.sent: List[Tuple[Any, str]] = []

    def send_system_action(self, action: str, conn_id: Any = None) -> None:
        self.sent.append((conn_id, action))

    def to(self, conn_id: Any) -> List[str]:
        return [a for c, a in self.sent if c == conn_id]


class Layouts:
    display_layouts = {"primary": {"x": 0, "y": 0, "w": 1920, "h": 1080},
                       "display2": {"x": 1920, "y": 0, "w": 1280, "h": 720}}


class Compositor:
    """Stands in for pixelflux: moves are numbered as they are injected and
    applied only when `apply` says, as the compositor thread would, a relative
    one by `gain` (a compositor's own acceleration) times the output's scale,
    held inside the 1920x1080 primary."""

    def __init__(self, scale: float = 1.0, gain: float = 1.0) -> None:
        self.x, self.y, self.scale, self.gain = 100.0, 100.0, scale, gain
        self.numbered = 0
        self.applied = 0
        self.queue: List[Tuple[int, str, float, float]] = []

    def inject_mouse_move(self, x: float, y: float) -> int:
        self.numbered += 1
        self.queue.append((self.numbered, "abs", x, y))
        return self.numbered

    def inject_relative_mouse_move(self, dx: float, dy: float) -> int:
        self.numbered += 1
        self.queue.append((self.numbered, "rel", dx, dy))
        return self.numbered

    def apply(self) -> None:
        for number, kind, a, b in self.queue:
            if kind == "abs":
                self.x, self.y = a, b
            else:
                k = self.gain * self.scale
                self.x = min(max(self.x + a * k, 0.0), 1919.0)
                self.y = min(max(self.y + b * k, 0.0), 1079.0)
            self.applied = number
        self.queue.clear()

    def pointer_location(self) -> Optional[Tuple[float, float, float, int]]:
        return (self.x, self.y, self.scale, self.applied)


class XServer:
    """Stands in for the X server's pointer, counts how often it was asked, and
    whether ever from the event loop's thread."""

    def __init__(self) -> None:
        self.at = (300, 200)
        self.queries = 0
        self.on_loop = False


XSERVER = XServer()


class Reader:
    """`_XPointerReader` on the stand-in server, asked from a worker thread as the real one is."""

    def read(self) -> tuple:
        XSERVER.queries += 1
        XSERVER.on_loop |= threading.current_thread() is threading.main_thread()
        return XSERVER.at

    def close(self) -> None:
        pass


input_handler._XPointerReader = Reader


def make_handler(wayland: Optional[Compositor] = None) -> WebRTCInput:
    h = WebRTCInput.__new__(WebRTCInput)
    h.loop = asyncio.get_running_loop()
    h.rtc_app = Transport()
    h.data_server_instance = Layouts()
    h._pointer_seq = {}
    h._pointer_echoes = {}
    h._pointer_echo_at = 0.0
    h._pointer_moved_at = 0.0
    h._pointer_echo_trailing = None
    h._pointer_echo_poll = None
    h._wl_motion = 0
    h.button_mask = 0
    h.last_x, h.last_y = -1, -1
    h.tracked_position_stale = False
    h.is_wayland = wayland is not None
    h.wayland_input = wayland
    h.mouse = None if wayland is not None else object()
    h._x_pointer_reader = None
    h.xdisplay = None
    h.uinput_mouse_socket_path = None
    h.client_gamepad_associations = {}
    if wayland is None:
        h.send_mouse = lambda action, data: None
    return h


REST = input_handler.POINTER_ECHO_POLL_S * 3


async def wayland_echo() -> None:
    comp = Compositor()
    h = make_handler(comp)
    await h._dispatch_message("_pointer_echo,1", "primary", "a")
    check("a subscriber is sent the position at once", h.rtc_app.to("a") == ["pointer,primary,100.0,100.0,1,0"],
          h.rtc_app.sent)
    check("and the pointer is read again while it is watched", h._pointer_echo_poll is not None)

    for seq in range(1, 4):
        await h._dispatch_message(f"m2,10,5,0,0,{seq}", "primary", "a")
        await asyncio.sleep(input_handler.POINTER_ECHO_MIN_S * 2)
        comp.apply()
    await asyncio.sleep(REST)
    check("moves the page draws ahead of the echo, applied as it draws them, send nothing",
          h.rtc_app.to("a") == ["pointer,primary,100.0,100.0,1,0"] and (comp.x, comp.y) == (130.0, 115.0),
          h.rtc_app.to("a"))

    comp.x, comp.y = 2000.0, 50.0
    await asyncio.sleep(input_handler.POINTER_ECHO_POLL_S * 3)
    check("an application's warp reaches the page with no motion sent, with the messages the position includes",
          h.rtc_app.to("a")[-1:] == ["pointer,display2,80.0,50.0,1,3"], h.rtc_app.to("a"))
    count = len(h.rtc_app.to("a"))
    await asyncio.sleep(REST)
    check("an unchanged pointer is not echoed again", len(h.rtc_app.to("a")) == count, h.rtc_app.to("a"))

    await h._dispatch_message("m,50,60,0,0,4", "primary", "b")
    await asyncio.sleep(input_handler.POINTER_ECHO_MIN_S * 2)
    comp.apply()
    await asyncio.sleep(REST)
    check("another connection's move is echoed, and the subscriber keeps its own count",
          h.rtc_app.to("a")[-1] == "pointer,primary,50.0,60.0,1,3", h.rtc_app.to("a"))
    check("and a connection that did not ask is sent nothing", h.rtc_app.to("b") == [], h.rtc_app.sent)

    await h.release_gamepads_for_conn("a")
    check("an echo stops with its connection", not h._pointer_echoes and h._pointer_echo_trailing is None)
    await asyncio.sleep(input_handler.POINTER_ECHO_POLL_S * 3)
    check("and so does the reading", h._pointer_echo_poll is None)


async def surprises() -> None:
    comp = Compositor(gain=1.5)
    h = make_handler(comp)
    await h._dispatch_message("_pointer_echo,1", "primary", "a")
    await h._dispatch_message("m2,10,0,0,0,1", "primary", "a")
    comp.apply()
    await h._dispatch_message("m2,10,0,0,0,2", "primary", "a")
    check("a move inside the interval is not echoed at once", len(h.rtc_app.to("a")) == 1, h.rtc_app.to("a"))
    await asyncio.sleep(input_handler.POINTER_ECHO_MIN_S * 3)
    check("but when it ends, where the compositor's gain left the pointer elsewhere",
          h.rtc_app.to("a")[-1:] == ["pointer,primary,115.0,100.0,1,1"], h.rtc_app.to("a"))
    comp.apply()
    await asyncio.sleep(REST)
    check("naming the last message the applied moves include",
          h.rtc_app.to("a")[-1:] == ["pointer,primary,130.0,100.0,1,2"], h.rtc_app.to("a"))

    edge = Compositor()
    h = make_handler(edge)
    edge.x = 1915.0
    await h._dispatch_message("_pointer_echo,1", "primary", "a")
    await h._dispatch_message("m2,10,0,0,0,1", "primary", "a")
    edge.apply()
    await asyncio.sleep(REST)
    check("a move the display's edge stops, which the page stops too, sends nothing",
          h.rtc_app.to("a") == ["pointer,primary,1915.0,100.0,1,0"] and edge.x == 1919.0, h.rtc_app.to("a"))
    await h._dispatch_message("m2,-6,0,0,0,2", "primary", "a")
    edge.apply()
    await asyncio.sleep(REST)
    check("nor does one back from it, which the page draws from the edge as well",
          h.rtc_app.to("a") == ["pointer,primary,1915.0,100.0,1,0"] and edge.x == 1913.0, h.rtc_app.to("a"))


async def scaled_output() -> None:
    comp = Compositor(scale=2.0)
    h = make_handler(comp)
    await h._dispatch_message("_pointer_echo,1", "primary", "a")
    check("the echo names how far a relative pixel moves the pointer",
          h.rtc_app.to("a") == ["pointer,primary,100.0,100.0,2,0"], h.rtc_app.to("a"))
    await h._dispatch_message("m2,10,0,0,0,1", "primary", "a")
    comp.apply()
    await asyncio.sleep(REST)
    check("and a move scaled by it is the page's to draw", len(h.rtc_app.to("a")) == 1 and comp.x == 120.0,
          h.rtc_app.to("a"))
    await h._dispatch_message("_pointer_echo,0", "primary", "a")
    check("a page that stops asking is dropped", "a" not in h._pointer_echoes)


async def x11_echo() -> None:
    XSERVER.__init__()
    h = make_handler()
    await h._dispatch_message("_pointer_echo,1", "primary", "a")
    check("X11 asks the server where the pointer is on subscribing",
          h.rtc_app.to("a") == ["pointer,primary,300.0,200.0,1,0"] and XSERVER.queries == 1,
          (h.rtc_app.sent, XSERVER.queries))
    check("and tracks from its answer", (h.last_x, h.last_y) == (300, 200))
    for seq in range(1, 6):
        await h._dispatch_message(f"m2,4,2,0,0,{seq}", "primary", "a")
        await asyncio.sleep(input_handler.POINTER_ECHO_MIN_S * 1.5)
    check("a moving pointer is compared from the tracking, with no round trip, and the page drew it",
          XSERVER.queries == 1 and len(h.rtc_app.to("a")) == 1, (XSERVER.queries, h.rtc_app.to("a")))
    XSERVER.at = (330, 214)
    await asyncio.sleep(REST)
    check("a resting pointer is read from the server, and echoed where it is not where the page drew it",
          XSERVER.queries >= 2 and h.rtc_app.to("a")[-1] == "pointer,primary,330.0,214.0,1,5",
          (XSERVER.queries, h.rtc_app.to("a")))
    await h._dispatch_message("m2,1,1,0,0,6", "primary", "a")
    check("and the tracking continues from that answer", (h.last_x, h.last_y) == (331, 215), (h.last_x, h.last_y))
    XSERVER.at = (3500, 900)
    await asyncio.sleep(REST)
    check("the server is only ever asked from a worker thread, never the event loop",
          XSERVER.queries >= 3 and not XSERVER.on_loop, (XSERVER.queries, XSERVER.on_loop))
    check("a point outside every display is placed on the nearest, clamped into it",
          h.rtc_app.to("a")[-1].split(",")[1:4] == ["display2", "1279.0", "719.0"], h.rtc_app.to("a"))
    await h.release_gamepads_for_conn("a")


async def unknown_pointer() -> None:
    comp = Compositor()
    comp.pointer_location = lambda: None
    h = make_handler(comp)
    await h._dispatch_message("_pointer_echo,1", "primary", "a")
    check("a session that cannot say where its pointer is answers none",
          h.rtc_app.to("a") == ["pointer,none"] and not h._pointer_echoes, h.rtc_app.sent)


async def main() -> None:
    await wayland_echo()
    await surprises()
    await scaled_output()
    await x11_echo()
    await unknown_pointer()


asyncio.run(main())

viewer_ok = "_pointer_echo,1".startswith(input_handler.VIEWER_COLLAB_EXTRA_PREFIXES)
check("a collaborator may ask for the echo", viewer_ok)
check("a read-only viewer's request is dropped without a warning",
      not "_pointer_echo,1".startswith(input_handler.VIEWER_ALLOWED_PREFIXES)
      and "_pointer_echo,1".startswith(input_handler.VIEWER_SILENT_DROP_PREFIXES))

print(f"[pointer-echo] {passed}/{passed + failed} passed")
sys.exit(1 if failed else 0)
