#!/usr/bin/env python3
"""The CPU and memory figures a page shows are the session's own cgroup's on
both transports: what the server publishes as `system_stats` is what the
sampler reads on this host, the memory total being the cgroup's limit where
one is set, and it keeps arriving.
Usage: python3 tests/e2e/test_system_usage.py [websockets|webrtc|all]
"""
import os
import sys
import time
from typing import Any, Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))), "src"))
SELECTOR = sys.argv[1] if len(sys.argv) > 1 else "all"
# The settings parser reads argv when the package is imported.
sys.argv = ["selkies"]

import helpers as H  # noqa: E402
import core_lib as C  # noqa: E402
from playwright.sync_api import sync_playwright  # noqa: E402
from selkies.system_usage import SystemUsage  # noqa: E402

STATS_JS = "() => window.system_stats ? [window.system_stats.cpu_percent, window.system_stats.mem_total, window.system_stats.mem_used] : null"


def wait_stats(page: Any, timeout: float = 20) -> Optional[list]:
    deadline = time.time() + timeout
    while time.time() < deadline:
        got = page.evaluate(STATS_JS)
        if got and got[1]:
            return got
        time.sleep(0.5)
    return page.evaluate(STATS_JS)


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
            first = wait_stats(page)
            _, total, used = own.sample()
            res.check("the page holds the session's CPU and memory figures",
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
