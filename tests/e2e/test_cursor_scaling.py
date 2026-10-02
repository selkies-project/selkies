#!/usr/bin/env python3
"""Cursor geometry in Chromium, without a desktop or streaming server.

Serves the real Input module and a decoded video/canvas fixture. CSS and canvas
cursors follow window, buffer, fullscreen, and reconnect changes while clicks
retain their stream coordinates. This measures DOM/canvas geometry and emitted
input, not the physical operating-system cursor or server-side injection.
"""
import functools
import http.server
import math
import os
import sys
import threading

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import core_lib as C
import helpers as H
from playwright.sync_api import Page, sync_playwright


class Quiet(http.server.SimpleHTTPRequestHandler):
    """Serve the repository without logging every module request."""

    def log_message(self, *args) -> None:
        pass


def check_cursor(page: Page, res: H.Results, label: str, browser: bool,
                 density: float, size: int = 48) -> None:
    """Wait for geometry delivery, then check both the bitmap and the hotspot."""
    page.wait_for_timeout(150)
    got = page.evaluate("readCursor()")
    if browser:
        hot = f"{math.floor(12 / density + 0.5)} {math.floor(6 / density + 0.5)}, default"
        # CSS serialization may round the density, but the hotspot is integral.
        correct = hot in got["css"] and (density == 1 or f"{density:g}dppx" in got["css"]
                                       or f"{density:g}x" in got["css"])
    else:
        correct = (abs(got["width"] - size / density) < 0.01
                   and abs(got["height"] - size / density) < 0.01
                   and abs(got["hotspot"]["x"] - 12 / density) < 0.01
                   and abs(got["hotspot"]["y"] - 6 / density) < 0.01)
    res.check(label, correct, {k: v for k, v in got.items() if k != "css"})


def run() -> bool:
    """Exercise measured scale changes and observer lifetimes on decoded sinks."""
    res = H.Results("cursor-scaling-browser")
    handler = functools.partial(Quiet, directory=H.REPO)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with sync_playwright() as pw:
            browser = C.chromium_launch(pw)
            try:
                for dpr in (1, 1.25, 2):
                    context = browser.new_context(viewport={"width": 1920, "height": 900},
                                                  device_scale_factor=dpr)
                    try:
                        for canvas in (False, True):
                            page = context.new_page()
                            page.goto(f"http://127.0.0.1:{server.server_port}/tests/tools/"
                                      f"cursor_scaling_page.html{'?canvas' if canvas else ''}")
                            page.wait_for_function("window.ready")
                            if not canvas:
                                res.check(f"video DPR {dpr}: metadata fallback",
                                          page.evaluate("input._cursorDensity()") == dpr)
                            page.evaluate("async () => { await decode(3840,2160); geometry(1600,900); }")
                            for css in (True, False):
                                label = f"{'canvas' if canvas else 'video'} DPR {dpr}, {'CSS' if css else 'canvas'} cursor"
                                page.evaluate("([css]) => cursor(css)", [css])
                                check_cursor(page, res, label + ": 4K fit", css, 2.4)
                                page.mouse.click(660, 300)
                                messages = page.evaluate("sent.filter(s => s.startsWith('m,'))")
                                res.check(label + ": click maps through the letterbox",
                                          any(s.split(',')[1:4] == ['1200', '720', '1'] for s in messages),
                                          messages[-2:])
                                if not css:
                                    got = page.evaluate("readCursor()")
                                    res.check(label + ": drawn hotspot at the click",
                                              got['transform'] == 'translate(655px, 297.5px)', got['transform'])
                                page.evaluate("geometry(1280,720)")
                                check_cursor(page, res, label + ": stationary resize", css, 3)
                                page.evaluate("geometry(3840/devicePixelRatio,2160/devicePixelRatio)")
                                check_cursor(page, res, label + ": 1:1", css, dpr)
                                page.evaluate("geometry(1600,900)")
                                page.evaluate("document.documentElement.requestFullscreen()")
                                check_cursor(page, res, label + ": fullscreen", css, 2.4)
                                page.evaluate("document.exitFullscreen()")
                                page.evaluate("async () => { await decode(1920,1080); }")
                                check_cursor(page, res, label + ": decoded resize at same CSS size", css, 1.2)
                                page.evaluate("css => cursor(css,96)", css)
                                check_cursor(page, res, label + ": larger application cursor", css, 1.2, 96)
                                page.evaluate("async () => { await decode(3840,2160); }")
                                page.evaluate("([css]) => { window.replaced = input; makeInput(); return cursor(css); }", [css])
                                check_cursor(page, res, label + ": replacement input", css, 2.4)
                                page.evaluate("window.retired = input; input.detach(); geometry(1280,720)")
                                page.wait_for_timeout(150)
                                res.check(label + ": detached cursor stays hidden",
                                          page.evaluate("retired.cursorDiv.style.display === 'none' && "
                                                        "!document.getElementById('overlay').style.cursor.includes('image-set') && "
                                                        "![retired, replaced].some(i => observers.has(i._cursorResizeObserver) || "
                                                        "observers.has(i._cursorMutationObserver))"))
                                page.evaluate("([css]) => { geometry(1600,900); makeInput(); return cursor(css); }", [css])
                            page.evaluate("async () => { await cursor(false,24); await cursor(true,96); "
                                          "await input.setUseBrowserCursors(false); }")
                            check_cursor(page, res, f"DPR {dpr}: mode switch retains current shape", False, 2.4, 96)
                            page.close()
                    finally:
                        context.close()
            finally:
                browser.close()
    finally:
        server.shutdown()
        server.server_close()
        thread.join()
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if run() else 1)
