#!/usr/bin/env python3
"""A rootful Xwayland desktop is adopted only when the X server really is one.

On the Wayland backend a live X server on $DISPLAY is taken as a rootful
Xwayland hosting an X11 desktop — its selection is watched, its apps launch on
it, and no second display is offered, since that desktop is one X screen of a
fixed size. A leftover Xvfb/Xorg that merely holds the display number (a
devcontainer's own :20, say) must not be: the server has to be an Xwayland
process of this user whose args name the display. _x11_session_display, the
rootful branch of app_session, and the second-screen capability must agree on
that test.
"""
import os
import asyncio
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))

from selkies import input_handler as ih  # noqa: E402
from selkies.input_handler import WebRTCInput  # noqa: E402

results = []


def check(label: str, ok, detail="") -> None:
    results.append(bool(ok))
    print(f"{'PASS' if ok else 'FAIL'}  [session-xwayland] {label}  {detail}", flush=True)


def make_handler(separate: bool = False) -> WebRTCInput:
    h = WebRTCInput.__new__(WebRTCInput)
    h.is_wayland = True
    h._app_wayland_display = lambda: "wayland-1" if separate else "wayland-0"
    h._wayland_display_name = lambda: "wayland-0"
    h._has_separate_app_compositor = lambda: separate
    return h


def main():
    saved_live = ih.x_display_live
    saved_xwl = ih.x_display_is_xwayland
    saved_display = os.environ.get("DISPLAY")
    disp = ":83"
    try:
        os.environ["DISPLAY"] = disp
        ih.x_display_live = lambda name: name == disp

        ih.x_display_is_xwayland = lambda name: False
        h = make_handler()
        check("non-Xwayland server is not the session's X display",
              h._x11_session_display() is None)
        s = h.app_session()
        check("app_session ignores a non-Xwayland server, apps stay Wayland",
              s == {"x11_display": None, "wayland_display": "wayland-0", "type": "wayland"}, str(s))
        cap = h.session_screen_capability()
        check("Wayland clients on the capture compositor are offered a second display",
              cap == (True, ""), str(cap))

        ih.x_display_is_xwayland = lambda name: name == disp
        h = make_handler()
        check("rootful Xwayland is the session's X display",
              h._x11_session_display() == disp)
        s = h.app_session()
        check("app_session adopts the rootful Xwayland as an X session",
              s == {"x11_display": disp, "wayland_display": None, "type": "x11"}, str(s))
        cap = h.session_screen_capability()
        check("a rootful Xwayland desktop is offered no second display, and told why",
              cap[0] is False and "rootful Xwayland" in cap[1], str(cap))
        announced = []

        async def screens_changed():
            announced.append(h.session_screen_capability()[0])

        async def monitor_comes_up():
            h._x11_monitor_build_lock = asyncio.Lock()
            await h._ensure_x11_clipboard_monitor_async()
            await asyncio.sleep(0)

        h.on_session_screens_changed = screens_changed
        h._x11_clipboard_monitor = None
        h._x11_monitor_retry_at = 0.0
        h._bg_tasks = set()
        h._ensure_x11_clipboard_monitor = lambda name: object()
        asyncio.run(monitor_comes_up())
        check("its monitor coming up tells the transport, which then offers no second display",
              announced == [False], str(announced))
        h._session_ipc_ok = True
        check("a session compositor with a screen control still is",
              h.session_screen_capability() == (True, ""), str(h.session_screen_capability()))

        ih.x_display_live = lambda name: False
        h = make_handler()
        check("no live server: no X session display", h._x11_session_display() is None)

        # Under a nested session compositor its XWM bridges its Xwayland, so the
        # unbridged-X monitor is not built even for a real Xwayland.
        ih.x_display_live = lambda name: name == disp
        ih.x_display_is_xwayland = lambda name: name == disp
        h = make_handler(separate=True)
        check("nested session: no unbridged X monitor", h._x11_session_display() is None)

        check("this process is not an Xwayland", ih._proc_is_xwayland(os.getpid(), disp) is False)
        check("empty display name is rejected", ih.x_display_is_xwayland("") is False)
        check("a dead pid is not an Xwayland", ih._proc_is_xwayland(2 ** 31 - 1, disp) is False)
    finally:
        ih.x_display_live = saved_live
        ih.x_display_is_xwayland = saved_xwl
        if saved_display is None:
            os.environ.pop("DISPLAY", None)
        else:
            os.environ["DISPLAY"] = saved_display

    print(f"\n{sum(results)}/{len(results)} passed")
    return all(results)


sys.exit(0 if main() else 1)
