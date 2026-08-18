#!/usr/bin/env python3
"""The scaling DPI the desktop ends up at must match the browser's display density.

Drives devicePixelRatio values through a real browser and reads back Xft.dpi, what
the session's toolkits scale from. A density the stops do not name (a 3.5x phone)
has to land on the nearest stop.
"""
import os
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import core_lib as C  # noqa: E402
import helpers as H  # noqa: E402
from playwright.sync_api import sync_playwright  # noqa: E402

# The scaling_dpi stops the clients offer, in 25% steps.
STOPS = [96, 120, 144, 168, 192, 216, 240, 264, 288]
DPRS = [1.0, 1.25, 1.5, 2.0, 2.5, 3.0, 3.5, 4.0]


def expected(dpr: float) -> int:
    """Return the scaling stop nearest to a devicePixelRatio's implied DPI.

    Args:
        dpr: Browser devicePixelRatio.

    Returns:
        The closest entry in ``STOPS`` after quantizing the density to
        quarter steps (96 DPI per 1.0x, 24 DPI per quarter).
    """
    return min(STOPS, key=lambda stop: abs(stop - round(dpr * 4) * 24))


def xft_dpi() -> int:
    """Read Xft.dpi from the running resource database.

    Returns:
        The Xft.dpi value, or 96 when unset: X's own default, which is what
        an application reads when nothing overrides it.
    """
    env = {**os.environ, "DISPLAY": H.TEST_DISPLAY}
    out = subprocess.run(["xrdb", "-query"], capture_output=True, text=True, env=env).stdout
    for line in out.splitlines():
        if line.startswith("Xft.dpi"):
            return int(line.split(":")[1].strip())
    return 96


def run() -> int:
    """Drive each DPR through a fresh browser context and compare Xft.dpi."""
    res = H.Results("dpi-accuracy")
    H.server_start(mode="websockets", web_root=H.CLASSIC_DIST)
    subprocess.run(["xrdb", "-remove"], capture_output=True,
                   env={**os.environ, "DISPLAY": H.TEST_DISPLAY})
    with sync_playwright() as p:
        browser = C.chromium_launch(p)
        try:
            for dpr in DPRS:
                ctx = browser.new_context(viewport={"width": 1000, "height": 700},
                                          device_scale_factor=dpr)
                page = ctx.new_page()
                page.goto(f"{H.BASE_URL}/", wait_until="load", timeout=60000)
                page.wait_for_timeout(9000)
                got, want = xft_dpi(), expected(dpr)
                res.check(f"dpr {dpr} applies Xft.dpi {want}", got == want,
                          f"got {got}")
                ctx.close()
        finally:
            browser.close()
    H.server_stop()
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(run())
