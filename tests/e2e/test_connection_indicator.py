#!/usr/bin/env python3
"""The poor-connection mark, end to end.

A page whose link carries its stream is told nothing. A page behind a metered
link slower than its stream is told its connection is poor and shows the mark,
in each dashboard, and is told it is good again, the mark gone, once its stream
fits the link: a low rate and a scene without the noise band, which a software
encoder would not bring down to that rate (WebKit's page streams VP8). A still
screen behind that link tells it nothing either way.
Congestion control is off, or it would fit the stream to the link before the
link could hold any of it back. Over WebRTC the headless client of the pacer
suite crosses a userspace relay narrower than the stream, which the pacer meets
by abandoning frames, and that client is told its connection is poor over its
data channel; with the relay unshaped it is told nothing.

Usage: python3 tests/e2e/test_connection_indicator.py [classic|wish|engines|webrtc|all]
(`engines` runs the default dashboard in Firefox and WebKit, whose session
socket can run in a worker.)
"""
import asyncio
import json
import os
import sys
import time
from typing import Any, List, Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "perf"))
import helpers as H  # noqa: E402
import core_lib as C  # noqa: E402

WIDTH, HEIGHT = 1280, 720
PAINTER = os.path.join(H.TOOLS, "motion_scene.py")
BWRELAY = os.path.join(H.TOOLS, "bwrelay.py")
RELAY_PORT = int(os.environ.get("E2E_CONNECTION_PORT", "18213"))
# Under the 8 Mbit/s a moving scene streams at, over the 500 kbit/s it is
# turned down to.
LINK_KBIT = 2000
ENV = {"SELKIES_CONGESTION_CONTROL": "false"}
MARK = "[role=status]"


def connection(page: Any) -> Optional[str]:
    return page.evaluate("window.stream_client ? window.stream_client.connection : null")


def mark_shown(page: Any) -> bool:
    return page.locator(MARK, has_text="Poor connection").count() > 0


def watch(page: Any, seconds: float) -> List[str]:
    """The verdicts the page held, sampled twice a second for `seconds`."""
    seen = []
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        seen.append(connection(page))
        time.sleep(0.5)
    return seen


def wait_for(page: Any, verdict: str, seconds: float) -> Optional[float]:
    """Seconds until the page holds `verdict`, or None."""
    start = time.monotonic()
    while time.monotonic() - start < seconds:
        if connection(page) == verdict:
            return time.monotonic() - start
        time.sleep(0.25)
    return None


def ws_block(dashboard: str, dist: str, engine: str = "chromium") -> "H.Results":
    from playwright.sync_api import sync_playwright

    res = H.Results(f"connection-{dashboard}" + ("" if engine == "chromium" else f"-{engine}"))
    painter = relay = None
    H.server_start(mode="websockets", wayland=False, web_root=dist, extra_env=ENV)
    try:
        painter = H.spawn([sys.executable, PAINTER, H.TEST_DISPLAY, str(WIDTH), str(HEIGHT), "60"])
        relay = H.spawn([sys.executable, BWRELAY, str(RELAY_PORT), str(H.PORT), str(LINK_KBIT)])
        with sync_playwright() as p:
            browser = C.launch_browser(p, engine)
            ctx = browser.new_context(viewport={"width": WIDTH, "height": HEIGHT})
            ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'websockets';")
            page = ctx.new_page()
            page.goto(H.BASE_URL, wait_until="load")
            res.check("a page streams the moving scene", C.wait_ws_video(page, timeout=45) is not None)
            fast = watch(page, 20)
            res.check("a page whose link carries the stream is never told its connection is poor",
                      fast and set(fast) == {"ok"} and not mark_shown(page), sorted(set(map(str, fast))))
            page.close()

            page = ctx.new_page()
            page.goto(f"http://127.0.0.1:{RELAY_PORT}", wait_until="load", timeout=90000)
            res.check("a page streams over the metered link", C.wait_ws_video(page, timeout=90) is not None)
            took = wait_for(page, "poor", 90)
            res.check("a page behind a link slower than its stream is told its connection is poor",
                      took is not None, f"after {took:.1f} s" if took is not None else "never")
            res.check("and the dashboard shows the mark", took is not None and mark_shown(page))
            painter.kill()
            painter = H.spawn([sys.executable, PAINTER, H.TEST_DISPLAY, str(WIDTH), str(HEIGHT), "60", "0"])
            page.evaluate("window.postMessage({type: 'settings', settings: {video_bitrate: 500}}, location.origin)")
            took = wait_for(page, "ok", 40)
            res.check("once its stream fits the link it is told the connection is good again",
                      took is not None, f"after {took:.1f} s" if took is not None else "never")
            res.check("and the mark goes", took is not None and not mark_shown(page))
            painter.kill()
            painter = None
            page.evaluate("window.postMessage({type: 'settings', settings: {video_bitrate: 8000}}, location.origin)")
            still = watch(page, 30)
            res.check("a still screen behind the link is not judged", set(still) == {"ok"},
                      sorted(set(map(str, still))))
            C.close_browser(browser)
    finally:
        for proc in (painter, relay):
            if proc is not None:
                proc.kill()
        H.server_stop()
    res.summary()
    return res


async def webrtc_session(rig: Any, shaped: bool) -> List[bool]:
    """The verdicts a headless WebRTC client was sent over a narrow link, or an
    unshaped one, while the scene moves."""
    told = []

    def on_message(msg: Any) -> None:
        if isinstance(msg, str) and '"connection"' in msg:
            data = json.loads(msg)
            if data.get("type") == "connection":
                told.append(bool(data["data"]["poor"]))

    rig.CURRENT_SHAPER["up"] = rig.Shaper()
    rig.CURRENT_SHAPER["down"] = rig.Shaper(rate_bps=2.5e6, delay_s=0.03) if shaped else rig.Shaper()
    await rig.run_client(f"ws://127.0.0.1:{H.PORT}/api/ws", measure_s=25.0, warmup_s=10.0,
                         on_message=on_message)
    return told


def webrtc_block() -> "H.Results":
    import test_pacer as rig

    res = H.Results("connection-webrtc")
    xproc, H.TEST_DISPLAY = H.private_x_server(WIDTH, HEIGHT)
    painter = H.spawn([sys.executable, PAINTER, H.TEST_DISPLAY, str(WIDTH), str(HEIGHT), "60"])
    try:
        for shaped in (False, True):
            H.server_start("webrtc", extra_env={
                "SELKIES_WEBRTC_PACER": "true", "SELKIES_CONGESTION_CONTROL": "false", "SELKIES_STUN_HOST": "",
                "SELKIES_TURN_REST_URI": "", "SELKIES_VIDEO_BITRATE": "4000"})
            told = asyncio.run(webrtc_session(rig, shaped))
            H.server_stop()
            if shaped:
                res.check("a client behind a link narrower than its stream is told its connection is poor",
                          told[:1] == [True], told)
            else:
                res.check("a client whose link carries the stream is told nothing", told == [], told)
    finally:
        painter.kill()
        xproc.kill()
    res.summary()
    return res


def main() -> None:
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    blocks = []
    if which in ("all", "classic"):
        blocks.append(ws_block("classic", H.CLASSIC_DIST))
    if which in ("all", "wish"):
        blocks.append(ws_block("wish", H.WISH_DIST))
    if which in ("all", "engines"):
        for engine in ("firefox", "webkit"):
            blocks.append(ws_block("classic", H.CLASSIC_DIST, engine))
    if which in ("all", "webrtc"):
        blocks.append(webrtc_block())
    failed = sum(len(b.failed()) for b in blocks)
    total = sum(len(b.items) for b in blocks)
    print(f"\n=== CONNECTION: {total - failed}/{total} passed ===")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
