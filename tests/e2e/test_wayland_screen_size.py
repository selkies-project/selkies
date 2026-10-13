#!/usr/bin/env python3
"""The session's screen is the size its page streams (Wayland).

The primary's capture is a view over the session's screen (output 0), and a
capture start sizes the view alone, so the screen has to be sized to it as
well. A page whose size differs from the server's is resized into it; a page at
the server's own size asks for no resize, and its capture still has to find
the screen sized, or applications lay out for the compositor's first size
while the stream shows the rest of the view empty. A fullscreen Wayland client
(tests/tools/wlobs.py) reads the screen's size. Chromium, both transports.

Usage: python3 tests/e2e/test_wayland_screen_size.py [websockets|webrtc|all]
"""
import importlib.util
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright

MODES = ("websockets", "webrtc")
# The server's own size first, then one the page has to resize it to.
SIZES = ((1920, 1080), (1280, 720))


def stream_size(page, mode: str):
    video = (C.wait_wr_video(page, timeout=45) if mode == "webrtc"
             else C.wait_ws_video(page, timeout=45))
    return (video["w"], video["h"]) if video else None


def run(res: H.Results, browser, mode: str, size: tuple) -> None:
    w, h = size
    H.server_start(mode, wayland=True)
    obs = None
    try:
        ctx = browser.new_context(viewport={"width": w, "height": h}, device_scale_factor=1)
        ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
        page = ctx.new_page()
        page.goto(H.BASE_URL, wait_until="load")
        deadline = time.time() + 30
        shown = stream_size(page, mode)
        while shown != size and time.time() < deadline:
            time.sleep(0.5)
            shown = stream_size(page, mode)
        res.check(f"{mode} {w}x{h}: the page streams at its size", shown == size, str(shown))
        time.sleep(2.0)
        obs = H.WlObs(H.capture_socket())
        mapped = obs.wait_for("mapped", timeout=15)
        screen = (mapped["w"], mapped["h"]) if mapped and mapped.get("configured") else None
        res.check(f"{mode} {w}x{h}: a fullscreen client fills the screen the page streams",
                  screen == size, str(screen))
        ctx.close()
    finally:
        if obs is not None:
            obs.stop()
        H.server_stop()


def main() -> int:
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    res = H.Results(f"wl-screen-size-{which}")
    if importlib.util.find_spec("pixelflux") is None:
        H.skip_suite("pixelflux is not installed")
    with sync_playwright() as p:
        browser = C.chromium_launch(p)
        try:
            for mode in MODES:
                if which in ("all", mode):
                    for size in SIZES:
                        run(res, browser, mode, size)
        finally:
            C.close_browser(browser)
    res.summary()
    return 0 if not res.failed() else 1


if __name__ == "__main__":
    sys.exit(main())
