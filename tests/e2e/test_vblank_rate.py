#!/usr/bin/env python3
"""The X server's fake vblank follows the stream on both transports.

While a page streams the X11 display, the root window carries the capture's
frame rate as `_FAKE_SCREEN_FPS`, a live frame-rate change moves it, and it is
gone once the page leaves. The Xvfb the images build paces the clients that
wait on Present by it (never below the rate it started at), so a vsynced
application presents as fast as the session streams; pixelflux publishes the
property on any server, which is what is read here.
"""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright

from typing import Optional


def fake_fps() -> Optional[int]:
    """The root window's `_FAKE_SCREEN_FPS`, or None when it is absent."""
    from selkies.Xlib import Xatom
    d = H.x_display()
    try:
        prop = d.screen().root.get_full_property(d.intern_atom("_FAKE_SCREEN_FPS"), Xatom.CARDINAL)
        return int(prop.value[0]) if prop and len(prop.value) else None
    finally:
        d.close()


def clear_leftover() -> None:
    """Delete a rate a capture left behind: a server killed mid-stream, as an earlier suite's
    SIGKILL does, never takes its rate back, and the check is of this server."""
    d = H.x_display()
    try:
        d.screen().root.delete_property(d.intern_atom("_FAKE_SCREEN_FPS"))
        d.sync()
    finally:
        d.close()


def settles(want: Optional[int], timeout: float = 15) -> Optional[int]:
    deadline = time.time() + timeout
    got = fake_fps()
    while got != want and time.time() < deadline:
        time.sleep(0.25)
        got = fake_fps()
    return got


def run(mode: str) -> bool:
    res = H.Results(f"vblank-{mode}")
    clear_leftover()
    H.server_start(mode=mode, wayland=False, extra_env={"SELKIES_FRAMERATE": "144"})
    try:
        res.check("nothing is published before a page streams", settles(None, 3) is None, fake_fps())
        with sync_playwright() as p:
            browser, page, _, _ = C.launch_chrome(p, mode=mode)
            try:
                info = (C.wait_wr_video(page, timeout=45) if mode == "webrtc"
                        else C.wait_ws_video(page, timeout=20))
                res.check("the page streams", info is not None, info)
                got = settles(144)
                res.check("the stream's rate is published", got == 144, got)
                C.settings_change(page, {"framerate": 90})
                got = settles(90)
                res.check("a live frame-rate change follows", got == 90, got)
            finally:
                browser.close()
        got = settles(None, 30)
        res.check("the rate is taken back once the page leaves", got is None, got)
    finally:
        H.server_stop()
    return res.summary()


SELECTORS = ("websockets", "webrtc")

if __name__ == "__main__":
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    chosen = SELECTORS if which == "all" else (which,)
    ok = True
    for mode in chosen:
        ok = run(mode) and ok
    sys.exit(0 if ok else 1)
