#!/usr/bin/env python3
"""Cross-transition test: start in webrtc, stream in browser, switch to
websockets while a webrtc peer is connected, verify old peer cleanup + new
transport works. Then the reverse. Ensures no capture-instance leak or port
conflict across mode flips with an active client."""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright


def verify(vp, mode: str, res: "H.Results", tag: str):
    """Wait for video on the page via the given transport and record a check.

    Args:
        vp: Playwright page showing the stream.
        mode: Transport mode, ``websockets`` or ``webrtc``.
        res: Results accumulator to record the check into.
        tag: Label prefix for the recorded check.

    Returns:
        The video info reported by the wait helper, or None on timeout.
    """
    if mode == "webrtc":
        info = C.wait_wr_video(vp, timeout=45)
        res.check(f"{tag}: video via webrtc", info is not None, info)
    else:
        info = C.wait_ws_video(vp, timeout=20)
        res.check(f"{tag}: video via websockets", info is not None, info)
    return info


def main() -> "H.Results":
    """Flip transports both ways with a live client attached each time."""
    H.server_start(mode="webrtc", wayland=False)
    res = H.Results("cross")
    with sync_playwright() as p:
        browser = C.chromium_launch(p)
        ctx = browser.new_context(viewport={"width": 1280, "height": 720})
        ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'webrtc';")
        page = ctx.new_page()
        page.goto(H.BASE_URL, wait_until="load")
        try:
            verify(page, "webrtc", res, "initial webrtc")
            # Flip to websockets while the webrtc peer is still alive.
            s, body = H.curl("/api/switch", method="POST", data={"mode": "websockets"})
            res.check("switch to websockets returns 200", s == 200, body[:40])
            time.sleep(5.0)
            # Fresh page pinned to websockets so it exercises the new transport.
            ctx2 = browser.new_context(viewport={"width": 1280, "height": 720})
            ctx2.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'websockets';")
            p2 = ctx2.new_page()
            p2.goto(H.BASE_URL, wait_until="load")
            verify(p2, "websockets", res, "after flip to websockets")
            # Reverse direction: back to webrtc with the websockets client alive.
            s, body = H.curl("/api/switch", method="POST", data={"mode": "webrtc"})
            res.check("switch back to webrtc returns 200", s == 200, body[:40])
            time.sleep(5.0)
            ctx3 = browser.new_context(viewport={"width": 1280, "height": 720})
            ctx3.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'webrtc';")
            p3 = ctx3.new_page()
            p3.goto(H.BASE_URL, wait_until="load")
            verify(p3, "webrtc", res, "after flip back to webrtc")
            # A client whose stored transport is the one the server is not
            # running gets 409 from its websocket probe and moves itself onto
            # the live transport, so a stale tab heals instead of retrying
            # forever. The probe URL is built from the document's own path.
            ctx4 = browser.new_context(viewport={"width": 1280, "height": 720})
            ctx4.add_init_script(
                "try { const app = (location.origin + location.pathname)"
                ".replace(/[^a-zA-Z0-9._-]/g, '_');"
                " if (!localStorage.getItem(app + '_stream_mode'))"
                " localStorage.setItem(app + '_stream_mode', 'websockets'); }"
                " catch (e) {}")
            p4 = ctx4.new_page()
            p4.goto(H.BASE_URL, wait_until="load")
            # The flip reloads the page, which destroys the evaluation context
            # the wait is polling, so the wait itself is retried across it.
            info, deadline = None, time.time() + 90
            while info is None and time.time() < deadline:
                try:
                    info = C.wait_wr_video(p4, timeout=15)
                except Exception:
                    p4.wait_for_timeout(1000)
            res.check("a client stored on the inactive transport flips itself over",
                      info is not None, info)
            p4.close(); ctx4.close()

            time.sleep(2)
            # Multiple flips must not accumulate signaling/capture instances.
            txt = H.server_log()
            res.check("no duplicate 'Signaling server started'", txt.count("started signaling server") <= 2, txt.count("started signaling server"))
            p3.close(); p2.close(); ctx2.close(); ctx3.close()
        finally:
            browser.close()
    res.summary()
    return res


if __name__ == "__main__":
    r = main()
    sys.exit(0 if not r.failed() else 1)