#!/usr/bin/env python3
"""Touch clients' own controls in both dashboards, end to end: the trackpad's
speed.

Trackpad travel is accelerated by the finger's speed: a slow drag moves the
pointer as far as the finger went, a fast one further, and the speed picked in
the dashboard scales it and is kept. Chromium is driven through CDP touch.

    python3 tests/e2e/test_touch_controls.py x11
"""
import os
import sys
import time
from typing import Any

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright
from test_touch import FIREFOX_TOUCH_PREFS, Fingers  # noqa: E402

def open_client(pw: Any, engine: str, mode: str) -> tuple:
    viewport = {"width": 1280, "height": 720}
    if engine == "firefox":
        ctx = C.firefox_persistent_context(pw, viewport=viewport, has_touch=True, prefs=FIREFOX_TOUCH_PREFS)
        browser = ctx
    else:
        browser = C.launch_browser(pw, engine)
        ctx = browser.new_context(viewport=viewport, device_scale_factor=1, has_touch=True)
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    page = ctx.new_page()
    page.goto(H.BASE_URL + "/", wait_until="load")
    return browser, page


def touch_once(page: Any, engine: str) -> None:
    """One finger down and up on the stream, which is how a dashboard learns the
    screen takes touch and offers its touch controls."""
    f = Fingers(page, engine)
    f.down((1, 900, 600))
    time.sleep(0.05)
    f.up(1)
    time.sleep(0.8)


def pointer_x() -> int:
    d = H.x_display()
    try:
        return d.screen().root.query_pointer().root_x
    finally:
        d.close()


def drag(f: Fingers, px: int, steps: int, pause: float) -> None:
    f.down((1, 300, 360))
    for i in range(1, steps + 1):
        if pause:
            time.sleep(pause)
        f.move((1, 300 + px * i / steps, 360))
    time.sleep(0.05)
    f.up(1)
    time.sleep(0.5)


def travel(f: Fingers, px: int, steps: int, pause: float) -> int:
    warp = H.x_display()
    warp.screen().root.warp_pointer(200, 360)
    warp.sync()
    warp.close()
    time.sleep(0.2)
    before = pointer_x()
    drag(f, px, steps, pause)
    return pointer_x() - before


def open_sidebar(page: Any) -> None:
    if not page.evaluate("!!document.querySelector('.sidebar.is-open')"):
        page.evaluate("window.postMessage({type: 'toggleDashboard'}, window.location.origin)")
        time.sleep(0.8)


def close_sidebar(page: Any) -> None:
    if page.evaluate("!!document.querySelector('.sidebar.is-open')"):
        page.evaluate("window.postMessage({type: 'toggleDashboard'}, window.location.origin)")
        time.sleep(0.8)


def trackpad_button(page: Any, dashboard: str) -> bool:
    """Turns trackpad mode on with the dashboard's own button, as a user does:
    a dashboard learns of the mode from its button, not from the core."""
    if dashboard == "classic":
        open_sidebar(page)
        button = page.locator(".trackpad-mode-button").first
    else:
        button = page.locator("button[title='Trackpad Mode']").first
    if not button.count():
        return False
    button.click()
    time.sleep(0.6)
    if dashboard == "classic":
        close_sidebar(page)
    return True


def speed_block(res: "H.Results", dashboard: str, dist: str, mode: str) -> None:
    tag = f"{dashboard} chromium {mode}"
    H.server_start(mode=mode, web_root=dist)
    try:
        with sync_playwright() as pw:
            browser, page = open_client(pw, "chromium", mode)
            try:
                video = C.wait_ws_video(page, 40) if mode == "websockets" else C.wait_wr_video(page, 60)
                res.check(f"{tag}: video up", video is not None, video)
                if not video:
                    return
                touch_once(page, "chromium")
                if not trackpad_button(page, dashboard):
                    res.check(f"{tag}: the dashboard offers trackpad mode", False)
                    return
                f = Fingers(page, "chromium")
                slow = travel(f, 100, 50, 0.02)
                # A flick: a few long steps sent back to back.
                fast = travel(f, 100, 4, 0)
                res.check(f"{tag}: a slow trackpad drag moves the pointer as far as the finger went",
                          90 <= slow <= 110, f"{slow} px for 100")
                res.check(f"{tag}: a fast one moves it further", fast >= 1.8 * slow, f"{fast} px for 100")
                if dashboard == "classic":
                    open_sidebar(page)
                picker = page.locator("#trackpadSpeedSelect, .key-palette ~ label select, label:has-text('Trackpad') select").first
                offered = picker.count() > 0
                res.check(f"{tag}: the dashboard offers the trackpad speed in trackpad mode", offered)
                if not offered:
                    return
                picker.select_option("2")
                time.sleep(0.5)
                if dashboard == "classic":
                    close_sidebar(page)
                doubled = travel(f, 100, 50, 0.02)
                res.check(f"{tag}: twice the speed moves a slow drag twice as far", 180 <= doubled <= 220,
                          f"{doubled} px for 100")
                page.reload(wait_until="load")
                (C.wait_ws_video(page, 40) if mode == "websockets" else C.wait_wr_video(page, 60))
                kept = page.evaluate("window.webrtcInput ? window.webrtcInput.trackpadSpeed : null")
                res.check(f"{tag}: the pick is kept over a reload", kept == 2, kept)
            finally:
                C.close_browser(browser)
    finally:
        H.server_stop()


def main() -> None:
    which = sys.argv[1] if len(sys.argv) > 1 else "x11"
    res = H.Results(f"touch-controls-{which}")
    if which == "x11":
        for dashboard, dist in (("classic", H.CLASSIC_DIST), ("wish", H.WISH_DIST)):
            speed_block(res, dashboard, dist, "websockets")
    else:
        raise SystemExit(f"unknown block {which}")
    sys.exit(0 if res.summary() else 1)


if __name__ == "__main__":
    main()
