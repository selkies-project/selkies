#!/usr/bin/env python3
"""The X server's fake vblank follows the stream on both transports.

While a page streams the X11 display, the root window carries the capture's
frame rate as `_FAKE_SCREEN_MILLIHZ` (millihertz) and `_FAKE_SCREEN_FPS` (whole
frames), a live frame-rate change moves them and the output's mode with it, and
they are gone once the page leaves. The Xvfb the images build paces the clients
that wait on Present by them (never below the rate it started at unless at most
0.1% slower), so a vsynced application presents as fast as the session streams;
pixelflux publishes the properties on any server, which is what is read here.
"""
import os
import re
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright



RATE_ATOMS = ("_FAKE_SCREEN_FPS", "_FAKE_SCREEN_MILLIHZ")


def fake_rate() -> tuple:
    """The root window's `_FAKE_SCREEN_FPS` and `_FAKE_SCREEN_MILLIHZ`, None where absent."""
    from selkies.Xlib import Xatom
    d = H.x_display()
    try:
        props = [d.screen().root.get_full_property(d.intern_atom(name), Xatom.CARDINAL) for name in RATE_ATOMS]
        return tuple(int(p.value[0]) if p and len(p.value) else None for p in props)
    finally:
        d.close()


def clear_leftover() -> None:
    """Delete a rate a capture left behind: a server killed mid-stream, as an earlier suite's
    SIGKILL does, never takes its rate back, and the check is of this server."""
    d = H.x_display()
    try:
        for name in RATE_ATOMS:
            d.screen().root.delete_property(d.intern_atom(name))
        d.sync()
    finally:
        d.close()


NONE = (None, None)


def settles(want: tuple, timeout: float = 15) -> tuple:
    deadline = time.time() + timeout
    got = fake_rate()
    while got != want and time.time() < deadline:
        time.sleep(0.25)
        got = fake_rate()
    return got


def output_refresh() -> float:
    """The refresh of the mode the test X server's output shows, 0 if none."""
    out = subprocess.run(["xrandr"], env={**os.environ, "DISPLAY": H.TEST_DISPLAY},
                         capture_output=True, text=True, timeout=10).stdout
    rate = re.search(r"(\d+\.\d+)\*", out)
    return float(rate.group(1)) if rate else 0.0


def run(mode: str) -> bool:
    res = H.Results(f"vblank-{mode}")
    clear_leftover()
    H.server_start(mode=mode, wayland=False, extra_env={"SELKIES_FRAMERATE": "144"})
    try:
        res.check("nothing is published before a page streams", settles(NONE, 3) == NONE, fake_rate())
        with sync_playwright() as p:
            browser, page, _, _ = C.launch_chrome(p, mode=mode)
            try:
                info = (C.wait_wr_video(page, timeout=45) if mode == "webrtc"
                        else C.wait_ws_video(page, timeout=20))
                res.check("the page streams", info is not None, info)
                got = settles((144, 144000))
                res.check("the stream's rate is published", got == (144, 144000), got)
                C.settings_change(page, {"framerate": 90})
                got = settles((90, 90000))
                res.check("a live frame-rate change follows", got == (90, 90000), got)
                rate = output_refresh()
                res.check("so does the display's refresh", abs(rate - 90) <= 0.9, rate)
                C.settings_change(page, {"framerate": 59.94})
                got = settles((60, 59940))
                res.check("a fractional rate is published to the millihertz", got == (60, 59940), got)
            finally:
                browser.close()
        got = settles(NONE, 30)
        res.check("the rate is taken back once the page leaves", got == NONE, got)
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
