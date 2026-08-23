#!/usr/bin/env python3
"""A rootful Xwayland desktop is adopted only when the X server really is one.

On the Wayland backend a live X server on $DISPLAY is taken as a rootful
Xwayland hosting an X11 desktop — its selection is watched and its apps launch
on it. A leftover Xvfb/Xorg that merely holds the display number (a
devcontainer's own :20, say) must not be: the server has to be an Xwayland
process of this user whose args name the display. _x11_session_display and the
rootful branch of app_session must agree on that test.
"""
import os
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
    h._app_wayland_display = lambda: "wayland-1"
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

        # A live server that is NOT an Xwayland (leftover Xvfb): not adopted.
        ih.x_display_is_xwayland = lambda name: False
        h = make_handler()
        check("non-Xwayland server is not the session's X display",
              h._x11_session_display() is None)
        s = h.app_session()
        check("app_session ignores a non-Xwayland server, apps stay Wayland",
              s == {"x11_display": None, "wayland_display": "wayland-1", "type": "wayland"}, str(s))

        # A real rootful Xwayland on that display: adopted, both agree.
        ih.x_display_is_xwayland = lambda name: name == disp
        h = make_handler()
        check("rootful Xwayland is the session's X display",
              h._x11_session_display() == disp)
        s = h.app_session()
        check("app_session adopts the rootful Xwayland as an X session",
              s == {"x11_display": disp, "wayland_display": None, "type": "x11"}, str(s))

        # No live server: never consulted, never adopted.
        ih.x_display_live = lambda name: False
        h = make_handler()
        check("no live server: no X session display", h._x11_session_display() is None)

        # Under a nested session compositor its XWM bridges its Xwayland, so the
        # unbridged-X monitor is not built even for a real Xwayland.
        ih.x_display_live = lambda name: name == disp
        ih.x_display_is_xwayland = lambda name: name == disp
        h = make_handler(separate=True)
        check("nested session: no unbridged X monitor", h._x11_session_display() is None)

        # The real /proc helpers: this process is not an Xwayland, and an empty
        # display name is rejected without scanning.
        check("this process is not an Xwayland", ih._proc_is_xwayland(os.getpid(), disp) is False)
        check("empty display name is rejected", ih.x_display_is_xwayland("") is False)
        # A pid that does not exist is not an Xwayland.
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
