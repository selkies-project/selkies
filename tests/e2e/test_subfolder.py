#!/usr/bin/env python3
"""A deployment reverse-proxied under a subfolder prefix.

Every server route moves under the prefix, so the client has to follow it: it
derives the prefix from its own document path rather than assuming the root,
and a client that gets that wrong asks a server that only answers
``/desk/api/...`` for ``/api/...`` and comes up dead with no visible cause.
Both streaming cores and both dashboards compute it, so each payload is served
under the prefix here and watched for where its requests, its websocket and
its static assets actually go.
"""
import os
import sys
import urllib.error
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C

from playwright.sync_api import sync_playwright

PREFIX = "desk"
res = H.Results("subfolder")


def status(path: str) -> int:
    """HTTP status of a GET against the server under test, 0 when unreachable."""
    try:
        with urllib.request.urlopen(f"{H.BASE_URL}{path}", timeout=5) as r:
            return r.status
    except urllib.error.HTTPError as e:
        return e.code
    except Exception:
        return 0


def drive(label: str, web_root: str, mode: str) -> None:
    """Serve one payload under the prefix and record where the client goes.

    Args:
        label: Name the checks are reported under.
        web_root: Directory served as the web client.
        mode: Transport the server runs, "websockets" or "webrtc".
    """
    H.server_start(mode=mode, wayland=False, web_root=web_root,
                   extra_env={"SELKIES_SUBFOLDER": f"/{PREFIX}/"})
    try:
        res.check(f"{label}: prefixed status answers",
                  status(f"/{PREFIX}/api/status") == 200, "")
        res.check(f"{label}: the unprefixed route is gone",
                  status("/api/status") == 404, status("/api/status"))

        with sync_playwright() as pw:
            browser = C.chromium_launch(pw)
            ctx = browser.new_context(viewport={"width": 1280, "height": 720})
            if mode == "webrtc":
                ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'webrtc';")
            page = ctx.new_page()
            requests, sockets, console_errors = [], [], []
            page.on("request", lambda r: requests.append(r.url))
            page.on("websocket", lambda ws: sockets.append(ws.url))
            page.on("console",
                    lambda m: console_errors.append(m.text) if m.type == "error" else None)
            page.on("pageerror", lambda e: console_errors.append(str(e)))

            page.goto(f"{H.BASE_URL}/{PREFIX}/", wait_until="load")
            info = (C.wait_wr_video(page, timeout=60) if mode == "webrtc"
                    else C.wait_ws_video(page, timeout=30))
            res.check(f"{label}: video flows from the prefixed deployment",
                      info is not None, info)

            own = [u for u in requests if u.startswith(H.BASE_URL)]
            stray = [u for u in own if not u.startswith(f"{H.BASE_URL}/{PREFIX}/")]
            res.check(f"{label}: every request stays under the prefix",
                      own and not stray, stray[:3])
            res.check(f"{label}: the websocket opens under the prefix",
                      sockets and all(f"/{PREFIX}/" in u for u in sockets),
                      sockets[:2])

            real_errors, _ = C.benign_console(console_errors, [])
            res.check(f"{label}: console clean", not real_errors, str(real_errors[:2]))
            browser.close()
    finally:
        H.server_stop()


drive("core-ws", H.CORE_DIST, "websockets")
drive("core-wr", H.CORE_DIST, "webrtc")
drive("classic", H.CLASSIC_DIST, "websockets")
drive("wish", H.WISH_DIST, "websockets")

sys.exit(0 if res.summary() else 1)
