#!/usr/bin/env python3
"""The figures a page shows reach it only while its stats are open, on both
transports: nothing periodic arrives before a dashboard says so, what the server
then publishes as `stream_stats` is what the sampler reads on this host, the
memory total being the cgroup's limit where one is set, it keeps arriving, and
it stops when the stats shut. Where the installed pixelflux reports its capture
(`ScreenCapture.stream_info`), the page is also told what the stream runs on
without asking.
Usage: python3 tests/e2e/test_system_usage.py [websockets|webrtc|all]
"""
import os
import sys
import time
from typing import Any

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
SELECTOR = sys.argv[1] if len(sys.argv) > 1 else "all"
# The settings parser reads argv when the package is imported.
sys.argv = ["selkies"]

import helpers as H  # noqa: E402
import core_lib as C  # noqa: E402
from playwright.sync_api import sync_playwright  # noqa: E402
from selkies.resource_stats import SystemUsage  # noqa: E402

STATS_JS = """() => {
    const latest = window.stream_stats && window.stream_stats.latest;
    return latest && latest.mem_total ? [latest.cpu_percent, latest.mem_total, latest.mem_used, latest.t] : null;
}"""
HISTORY_JS = "() => window.stream_stats ? window.stream_stats.history.length : -1"
OPEN_JS = "(open) => window.postMessage({ type: 'statsOpen', open }, window.location.origin)"
INFO_JS = "() => window.stream_info"


def reports_capture() -> bool:
    """Whether the pixelflux the server runs describes its capture."""
    try:
        import pixelflux
    except ImportError:
        return False
    return hasattr(pixelflux.ScreenCapture, "stream_info")


def wait_for(page: Any, script: str, timeout: float = 20) -> Any:
    deadline = time.time() + timeout
    while time.time() < deadline:
        got = page.evaluate(script)
        if got:
            return got
        time.sleep(0.5)
    return page.evaluate(script)


def block(mode: str) -> "H.Results":
    res = H.Results(f"system-usage-{mode}")
    own = SystemUsage()
    own.sample()
    H.server_start(mode=mode, wayland=False)
    with sync_playwright() as pw:
        browser = C.chromium_launch(pw)
        try:
            ctx = browser.new_context(viewport={"width": 1280, "height": 720}, device_scale_factor=1)
            ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
            page = ctx.new_page()
            page.goto(H.BASE_URL, wait_until="load")
            res.check("video streams", (C.wait_wr_video(page, timeout=45) if mode == "webrtc"
                                        else C.wait_ws_video(page, timeout=30)) is not None)
            time.sleep(4)
            res.check("nothing periodic arrives while the stats are shut",
                      page.evaluate(STATS_JS) is None and page.evaluate(HISTORY_JS) == 0,
                      page.evaluate(HISTORY_JS))
            if reports_capture():
                info = wait_for(page, INFO_JS)
                res.check("the page is told what the stream runs on without asking",
                          bool(info) and info.get("backend") == "x11" and bool(info.get("encoder"))
                          and bool(info.get("capture")), info)
            page.evaluate(OPEN_JS, True)
            first = wait_for(page, STATS_JS)
            _, total, used = own.sample()
            res.check("opened, the page holds the session's CPU and memory figures",
                      first is not None and 0 <= first[0] <= 100 and 0 < first[2] <= first[1], first)
            res.check("the memory total is what the session's own cgroup allows",
                      first is not None and first[1] == total, (first and first[1], total))
            res.check("the use it shows is the cgroup's, near what the sampler reads here",
                      first is not None and abs(first[2] - used) < max(total * 0.05, 256 << 20), (first and first[2], used))
            deadline = time.time() + 12
            later = page.evaluate(STATS_JS)
            while time.time() < deadline and later == first:
                time.sleep(0.5)
                later = page.evaluate(STATS_JS)
            res.check("the figures keep arriving", later is not None and later != first, (first, later))
            res.check("and the history grows with them", page.evaluate(HISTORY_JS) >= 2, page.evaluate(HISTORY_JS))
            page.evaluate(OPEN_JS, False)
            time.sleep(3)
            res.check("shut, the history is empty and stays empty",
                      page.evaluate(STATS_JS) is None and page.evaluate(HISTORY_JS) == 0,
                      page.evaluate(HISTORY_JS))
        finally:
            browser.close()
    H.server_stop()
    res.summary()
    return res


def main() -> None:
    blocks = []
    try:
        for mode in ("websockets", "webrtc"):
            if SELECTOR in ("all", mode):
                blocks.append(block(mode))
    finally:
        H.server_stop()
    failed = sum(len(b.failed()) for b in blocks)
    total = sum(len(b.items) for b in blocks)
    print(f"=== SYSTEM USAGE: {total - failed}/{total} passed ===", flush=True)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
