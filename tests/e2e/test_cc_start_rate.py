#!/usr/bin/env python3
"""A page keeps the rate congestion control held its display at, and a
restarted server starts the page's next stream there, on both transports.

With congestion control on, a display's steered rate that held for 10 s is told
to its controller (`CC_RATE` over WebSockets, a `cc_rate` data-channel message
over WebRTC), which keeps it in localStorage per origin and display. A page
with a rate kept sends it when it connects (`ccStartKbps` in its WebSockets
SETTINGS, `cc_start_kbps` in its WebRTC HELLO), and a restarted server starts
the display there rather than at the configured rate. A rate a day old starts
nothing, and one the user overrides by setting a bitrate is dropped.
"""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import core_lib as C  # noqa: E402
import helpers as H  # noqa: E402
from playwright.sync_api import sync_playwright  # noqa: E402

ENV = {"SELKIES_CONGESTION_CONTROL": "true"}
KEY_JS = "(location.origin + location.pathname).replace(/[^a-zA-Z0-9._-]/g, '_') + '_cc_start'"
SEEDED = "starting at 2000 kbps, from the rate its page last held"


def kept(page) -> dict:
    """The start rate the page keeps for its primary display, or {}."""
    return page.evaluate(f"JSON.parse(localStorage.getItem({KEY_JS}) || '{{}}')")


def keep(page, kbps: int, age_s: float = 0.0) -> None:
    """Put a start rate `age_s` old in the page's storage."""
    page.evaluate(f"localStorage.setItem({KEY_JS}, JSON.stringify({{kbps: {kbps}, at: Date.now() - {age_s * 1000}}}))")


def video(page, mode: str) -> bool:
    return bool(C.wait_ws_video(page, timeout=30) if mode == "websockets" else C.wait_wr_video(page, timeout=30))


def restart(page, mode: str) -> bool:
    """Restart the server and reload the page against it; whether video came back."""
    H.server_stop()
    H.server_start(mode=mode, extra_env=ENV)
    page.reload(wait_until="load")
    return video(page, mode)


def run(res: "H.Results", mode: str) -> None:
    H.server_start(mode=mode, extra_env=ENV)
    try:
        with sync_playwright() as pw:
            browser, page, _, _ = C.launch_chrome(pw, mode=mode)
            try:
                res.check(f"{mode}: video flowing", video(page, mode))
                deadline, got = time.time() + 25, {}
                while time.time() < deadline and not got:
                    got = kept(page)
                    time.sleep(0.5)
                res.check(f"{mode}: the page keeps the rate its display held",
                          100 <= got.get("kbps", 0) <= 100000, got)

                keep(page, 2000)
                res.check(f"{mode}: a restarted server streams again", restart(page, mode))
                res.check(f"{mode}: and starts the display at the rate the page kept",
                          C.wait_log(SEEDED, timeout=10), H.tail(H.LOG))

                keep(page, 2000, age_s=25 * 3600)
                res.check(f"{mode}: after a restart with a rate a day old", restart(page, mode))
                res.check(f"{mode}: the display starts at the configured rate", C.wait_log_absent(SEEDED, timeout=5))

                keep(page, 2000)
                page.evaluate("window.postMessage({type: 'settings', settings: {video_bitrate: 6000}}, "
                              "window.location.origin)")
                deadline = time.time() + 5
                while time.time() < deadline and kept(page):
                    time.sleep(0.2)
                res.check(f"{mode}: a bitrate the user sets drops the kept rate", not kept(page), kept(page))
            finally:
                browser.close()
    finally:
        H.server_stop()


def main() -> int:
    res = H.Results("cc-start-rate")
    for mode in ("websockets", "webrtc"):
        run(res, mode)
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(main())
