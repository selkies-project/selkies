#!/usr/bin/env python3
"""Windows follow their display when an X11 layout moves it.

RandR moves a CRTC or a logical monitor and leaves every window where it was in
root coordinates, so a primary that moves aside for a display added on its left
or above, or back to the origin when that display leaves, would leave its
windows on the other display or beyond the screen. `display_utils` asks the
window manager to move them along (`window_moves`), and this suite checks the
result against a real X server and window manager: a window placed on the
primary is on the primary after every arrangement, at the same place within it,
and a window on a display that leaves lands on the primary.

Both layout paths are covered by the server the suite is pointed at: the
XLibre Xvfb the images build offers pluggable outputs, a stock Xvfb takes the
logical-monitor path.

Usage: python3 tests/integration/test_display_window_follow.py [wm command ...]
Environment: E2E_XVFB names the X server binary (default Xvfb), E2E_XVFB_ARGS
adds arguments to it (a manager that composites through GLX needs
``+extension GLX``).
"""
import asyncio
import os
import shutil
import subprocess
import sys
import time
from typing import Optional, Tuple

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H

from selkies import display_utils as DU
from selkies.Xlib import X, Xutil, display as xdisplay
from selkies.Xlib.protocol import event as xevent

PRIMARY = (1280, 720)
SECOND = (1024, 768)
#: How far a manager may put a window from where it was asked to, for the
#: frame it draws around it.
SLACK = 4


def wm_ready(d: xdisplay.Display, timeout: float = 15.0) -> bool:
    """Wait for a manager to publish its check window on the root."""
    root = d.screen().root
    atom = d.intern_atom("_NET_SUPPORTING_WM_CHECK")
    deadline = time.time() + timeout
    while time.time() < deadline:
        prop = root.get_full_property(atom, X.AnyPropertyType)
        if prop is not None and prop.value:
            return True
        time.sleep(0.2)
    return False


def make_window(d: xdisplay.Display, name: str, x: int, y: int, w: int, h: int):
    """Map a top-level window and put it where asked: as a user position, and
    then, for a manager that places new windows by its own rule (KWin), as a
    pager's move once the window is managed."""
    screen = d.screen()
    win = screen.root.create_window(
        x, y, w, h, 0, screen.root_depth, X.InputOutput, X.CopyFromParent,
        background_pixel=screen.white_pixel, event_mask=X.StructureNotifyMask)
    win.set_wm_name(name)
    win.set_wm_normal_hints(flags=Xutil.USPosition | Xutil.USSize, x=x, y=y, width=w, height=h)
    win.map()
    d.sync()
    if not near(settle(d, win, (x, y), timeout=2.0), (x, y)):
        flags = X.StaticGravity | (1 << 8) | (1 << 9) | (2 << 12)
        screen.root.send_event(
            xevent.ClientMessage(window=win, client_type=d.intern_atom("_NET_MOVERESIZE_WINDOW"),
                                 data=(32, [flags, x, y, 0, 0])),
            event_mask=X.SubstructureRedirectMask | X.SubstructureNotifyMask)
        d.sync()
        settle(d, win, (x, y), timeout=2.0)
    return win


def maximize(d: xdisplay.Display, win) -> None:
    """Ask the manager to maximize `win` both ways."""
    root = d.screen().root
    state = d.intern_atom("_NET_WM_STATE")
    horz = d.intern_atom("_NET_WM_STATE_MAXIMIZED_HORZ")
    vert = d.intern_atom("_NET_WM_STATE_MAXIMIZED_VERT")
    root.send_event(xevent.ClientMessage(window=win, client_type=state, data=(32, [1, horz, vert, 1, 0])),
                    event_mask=X.SubstructureRedirectMask | X.SubstructureNotifyMask)
    d.sync()


def place(d: xdisplay.Display, win) -> Tuple[int, int, int, int]:
    """`(x, y, w, h)` of the client window in root coordinates."""
    d.sync()
    root = d.screen().root
    geom = win.get_geometry()
    at = root.translate_coords(win, 0, 0)
    return int(at.x), int(at.y), int(geom.width), int(geom.height)


def settle(d: xdisplay.Display, win, want: Tuple[int, int], timeout: float = 5.0) -> Tuple[int, int, int, int]:
    """The window's place once it has come to rest near `want`, or wherever it
    is at the deadline."""
    deadline = time.time() + timeout
    while True:
        got = place(d, win)
        if abs(got[0] - want[0]) <= SLACK and abs(got[1] - want[1]) <= SLACK:
            return got
        if time.time() > deadline:
            return got
        time.sleep(0.1)


def near(got: Tuple[int, int, int, int], want: Tuple[int, int]) -> bool:
    return abs(got[0] - want[0]) <= SLACK and abs(got[1] - want[1]) <= SLACK


def inside(rect: Tuple[int, int, int, int], area: Tuple[int, int, int, int]) -> bool:
    """Whether the rectangle's center lies within `area`."""
    cx, cy = rect[0] + rect[2] // 2, rect[1] + rect[3] // 2
    return area[0] <= cx < area[0] + area[2] and area[1] <= cy < area[1] + area[3]


def extend(position: str) -> Tuple[dict, dict]:
    """Apply a two-display layout with the secondary at `position`; returns
    `(layouts, primary rect)`."""
    layouts, tw, th = DU.compute_dual_layout(PRIMARY, SECOND, position)
    layouts["display2"] = layouts.pop("secondary")
    ok = asyncio.run(DU.apply_extended_layout(layouts, tw, th))
    if not ok:
        raise RuntimeError(f"layout {position} refused")
    p = layouts["primary"]
    return layouts, (p["x"], p["y"], p["w"], p["h"])


def collapse() -> None:
    """Back to the primary alone, the way a page leaving does over websockets."""
    layouts = {"primary": {"x": 0, "y": 0, "w": PRIMARY[0], "h": PRIMARY[1]}}
    if not asyncio.run(DU.apply_extended_layout(layouts, PRIMARY[0], PRIMARY[1])):
        raise RuntimeError("single layout refused")


def run(wm: list, xvfb: str) -> bool:
    tag = "window-follow-" + os.path.basename(wm[0])
    res = H.Results(tag)
    proc, display = H.private_x_server(width=4096, height=2048, xvfb=xvfb,
                                       extra_args=os.environ.get("E2E_XVFB_ARGS", "").split())
    wm_proc: Optional[subprocess.Popen] = None
    try:
        os.environ["DISPLAY"] = display
        DU._drop_module_display()
        asyncio.run(DU.resize_display(f"{PRIMARY[0]}x{PRIMARY[1]}"))
        env = {**os.environ, "DISPLAY": display}
        wm_proc = H.spawn(wm, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT,
                          start_new_session=True)
        d = xdisplay.Display(display)
        res.check("the window manager comes up", wm_ready(d), wm)
        outputs = asyncio.run(DU.has_pluggable_outputs())
        print(f"[{tag}] {display} pluggable outputs: {outputs}", flush=True)

        plain = make_window(d, "follow-plain", 200, 150, 400, 300)
        big = make_window(d, "follow-maximized", 50, 50, 300, 200)
        maximize(d, big)
        time.sleep(1.0)
        at0 = place(d, plain)
        res.check("the manager placed the window on the primary", inside(at0, (0, 0) + PRIMARY), at0)
        big0 = place(d, big)
        res.check("and maximized the other over it", inside(big0, (0, 0) + PRIMARY) and big0[2] >= PRIMARY[0] - 2 * SLACK, big0)

        for position, shift in (("left", (SECOND[0], 0)), ("up", (0, SECOND[1]))):
            layouts, prect = extend(position)
            want = (at0[0] + shift[0], at0[1] + shift[1])
            got = settle(d, plain, want)
            res.check(f"a display {position} of the primary moves the primary's window with it",
                      near(got, want), f"want {want} got {got} primary {prect}")
            time.sleep(0.5)
            gotbig = place(d, big)
            res.check("and the maximized window fills the primary there", inside(gotbig, prect), f"{gotbig} primary {prect}")
            collapse()
            got = settle(d, plain, at0[:2])
            res.check(f"the {position} display leaving brings the window back",
                      near(got, at0[:2]), f"want {at0[:2]} got {got}")
            time.sleep(0.5)
            gotbig = place(d, big)
            res.check("and the maximized window back over the primary", inside(gotbig, (0, 0) + PRIMARY), gotbig)

        layouts, prect = extend("left")
        time.sleep(0.5)
        asyncio.run(DU.retire_displays())
        asyncio.run(DU.resize_display(f"{PRIMARY[0]}x{PRIMARY[1]}"))
        got = settle(d, plain, at0[:2])
        res.check("retiring the displays outright brings the window back too",
                  near(got, at0[:2]), f"want {at0[:2]} got {got}")

        layouts, prect = extend("right")
        s = layouts["display2"]
        guest = make_window(d, "follow-guest", s["x"] + 100, s["y"] + 80, 300, 200)
        time.sleep(1.0)
        g0 = place(d, guest)
        if inside(g0, (s["x"], s["y"], s["w"], s["h"])):
            collapse()
            want = (g0[0] - s["x"], g0[1] - s["y"])
            got = settle(d, guest, want)
            res.check("the right display leaving lands its window on the primary where it sat",
                      near(got, want) and inside(got, (0, 0) + PRIMARY), f"want {want} got {got}")
        else:
            # A manager placing new windows by its own rule (KWin) keeps this one on the
            # primary, so there is no window on the departing display to follow.
            res.skip("the right display leaving lands its window on the primary where it sat",
                     f"the manager placed the new window at {g0[:2]}, off the right display")
            collapse()
        got = place(d, plain)
        res.check("and leaves the primary's own window alone", near(got, at0[:2]), f"want {at0[:2]} got {got}")
    finally:
        DU._drop_module_display()
        if wm_proc is not None:
            wm_proc.terminate()
            try:
                wm_proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                wm_proc.kill()
        H.stop_x_server(proc, display)
    return res.summary()


if __name__ == "__main__":
    wm = sys.argv[1:] or ["openbox"]
    xvfb = os.environ.get("E2E_XVFB", "Xvfb")
    if shutil.which(wm[0]) is None:
        print(f"{wm[0]} is not installed", file=sys.stderr)
        sys.exit(2)
    sys.exit(0 if run(wm, xvfb) else 1)
