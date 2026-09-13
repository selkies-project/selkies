#!/usr/bin/env python3
"""What a page says about encoders its browser cannot play.

Either dashboard's encoder menu lists every encoder the server allows and
disables the ones this browser cannot play on the transport, each marked as
unsupported by the browser, so a missing option reads as the browser's limit
and never as a server feature left out. A stream the browser cannot play at
all, from an encoder the server holds, is said on the page for the owner and
a viewer alike: the WebSocket core learns it from the stream, the WebRTC core
is told by the server once the browser's answer left the codec out. Chromium
on Linux plays no H.265, which is the case driven here.
"""
import os
import sys
import time
from typing import Any, Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import helpers as H
import core_lib as C
from test_dashboard_matrix import open_settings, encoder_menu_button
from playwright.sync_api import sync_playwright

UNSUPPORTED = "not supported by this browser"
NOTICE = "H.265 video, which this browser cannot decode"
H265 = "H.265"
H264 = "H.264"


def wait_video(page: Any, mode: str) -> Optional[dict]:
    return C.wait_wr_video(page, timeout=45) if mode == "webrtc" else C.wait_ws_video(page, timeout=30)


def classic_menu(page: Any) -> Optional[list]:
    """The classic dashboard's encoder options as `(text, disabled)`, with its
    Video section opened; None when the select never renders."""
    if not page.evaluate("!!document.querySelector('.sidebar.is-open')"):
        page.evaluate("window.postMessage({type: 'toggleDashboard'}, window.location.origin)")
        page.wait_for_timeout(500)
    if not page.locator("#encoderSelect").count():
        header = page.locator('.sidebar-section-header:has-text("Video")').first
        if header.count():
            header.click(force=True, timeout=3000)
    deadline = time.time() + 10
    while time.time() < deadline:
        options = page.evaluate(
            "() => Array.from(document.querySelectorAll('#encoderSelect option'))"
            ".map((o) => [o.textContent, o.disabled])")
        if options:
            return options
        time.sleep(0.3)
    return None


def wish_menu(page: Any) -> Optional[list]:
    """The wish dashboard's encoder menu items as `(text, disabled)`, opened
    from its Settings panel; None when the menu never renders."""
    if not open_settings(page):
        return None
    button = encoder_menu_button(page)
    if button is None:
        return None
    button.click(force=True, timeout=3000)
    deadline = time.time() + 10
    while time.time() < deadline:
        items = page.evaluate(
            "() => Array.from(document.querySelectorAll('[role=\"menu\"][data-state=\"open\"] [role=\"menuitem\"]'))"
            ".map((i) => [i.textContent, i.getAttribute('aria-disabled') === 'true' || i.hasAttribute('data-disabled')])")
        if items:
            return items
        time.sleep(0.3)
    return None


def menu_block(dashboard: str, mode: str) -> "H.Results":
    res = H.Results(f"encoder-menu-{dashboard}-{mode}")
    H.server_start(mode=mode, wayland=False, web_root=H.WISH_DIST if dashboard == "wish" else H.CLASSIC_DIST)
    with sync_playwright() as pw:
        browser = C.chromium_launch(pw)
        try:
            ctx = browser.new_context(viewport={"width": 1440, "height": 900}, device_scale_factor=1)
            ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
            page = ctx.new_page()
            page.goto(H.BASE_URL, wait_until="load")
            res.check("video streams", wait_video(page, mode) is not None)
            entries = classic_menu(page) if dashboard == "classic" else wish_menu(page)
            by_label = {text.replace(UNSUPPORTED, "").strip(" []"): (text, disabled) for text, disabled in (entries or [])}
            res.check("every encoder the server allows is listed",
                      entries is not None and len(entries) == (5 if mode == "webrtc" else 7), entries)
            res.check("H.265, which this browser does not play, is listed disabled and marked as unsupported here",
                      H265 in by_label and by_label[H265][1] and UNSUPPORTED in by_label[H265][0], by_label.get(H265))
            res.check("H.264 is listed, enabled and unmarked",
                      H264 in by_label and not by_label[H264][1] and UNSUPPORTED not in by_label[H264][0], by_label.get(H264))
            if dashboard == "wish":
                layers = page.evaluate(
                    "() => { const m = document.querySelector('[role=\"menu\"][data-state=\"open\"]');"
                    " const z = (e) => parseInt(getComputedStyle(e).zIndex, 10);"
                    " return m ? [z(m.parentElement), z(document.getElementById('dashboard-root'))] : null; }")
                res.check("the open menu stacks above the dashboard root that hosts the panel",
                          layers is not None and layers[0] > layers[1], layers)
        finally:
            browser.close()
    H.server_stop()
    res.summary()
    return res


def wait_notice(page: Any, timeout: float = 40) -> Optional[str]:
    """The status bar's text once it shows the undecodable-stream notice, else
    whatever it last showed."""
    deadline = time.time() + timeout
    shown = None
    while time.time() < deadline:
        shown = page.evaluate(
            "() => { const e = document.getElementById('status-display');"
            " return e && !e.classList.contains('hidden') ? e.textContent : null; }")
        if shown and NOTICE in shown:
            return shown
        time.sleep(0.5)
    return shown


def pinned_block(mode: str) -> "H.Results":
    """The server holding H.265 for browsers that play none: the owner's page
    and a viewer's both say so instead of staying black and quiet."""
    res = H.Results(f"encoder-pinned-{mode}")
    H.server_start(mode=mode, wayland=False, extra_env={"SELKIES_ENCODER": "h265enc"})
    with sync_playwright() as pw:
        browser = C.chromium_launch(pw)
        try:
            for role, url_hash in (("owner", ""), ("viewer", "#shared")):
                ctx = browser.new_context(viewport={"width": 1280, "height": 720}, device_scale_factor=1)
                ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
                page = ctx.new_page()
                said: list = []
                page.on("console", lambda m, said=said: said.append(m.text))
                page.goto(H.BASE_URL + "/" + url_hash, wait_until="load")
                shown = wait_notice(page)
                res.check(f"{role}: the page says the session streams H.265 video it cannot decode",
                          shown is not None and NOTICE in shown, shown or said[-3:])
                ctx.close()
        finally:
            browser.close()
    H.server_stop()
    res.summary()
    return res


def main() -> None:
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    blocks = []
    try:
        if which in ("all", "menu"):
            for dashboard in ("classic", "wish"):
                for mode in ("websockets", "webrtc"):
                    blocks.append(menu_block(dashboard, mode))
        if which in ("all", "pinned"):
            for mode in ("websockets", "webrtc"):
                blocks.append(pinned_block(mode))
    finally:
        H.server_stop()
    failed = sum(len(b.failed()) for b in blocks)
    total = sum(len(b.items) for b in blocks)
    print(f"=== ENCODER SUPPORT: {total - failed}/{total} passed ===", flush=True)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
