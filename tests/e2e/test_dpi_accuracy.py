#!/usr/bin/env python3
"""The scaling DPI the desktop ends up at must match what asked for it.

Drives devicePixelRatio values through a real browser and reads back Xft.dpi, what
the session's toolkits scale from. A density the stops do not name (a 3.5x phone)
has to land on the nearest stop.

A resolution the operator sets answers the question instead of the display:
that framebuffer decides how large the desktop draws its UI, and the screen
showing it says nothing about it. So the last blocks drive manual resolutions
at dpr 1, where the density would ask for 96 whatever the size, and pin that a
stored pick outranks both.
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
# Manual resolutions and the pick each derives: the shorter side against the
# 1080 rows 96 DPI is for, snapped to a stop.
MANUAL_DPIS = [((3840, 2160), 192), ((2560, 1440), 120), ((1280, 720), 96)]
# A pick no derivation returns, so a desktop sitting at it can only have taken
# the stored one.
PINNED_DPI = 168

# A stored pick, written before the core reads its settings.
PINNED_INIT = """(() => {
  const prefix = (window.location.origin + window.location.pathname)
    .replace(/[^a-zA-Z0-9._-]/g, '_');
  localStorage.setItem(prefix + '_scaling_dpi', '%d');
})()""" % PINNED_DPI


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
    env = {**os.environ, "DISPLAY": H.require_display()}
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
                   env={**os.environ, "DISPLAY": H.require_display()})
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

            # A live density change (the window dragged to another monitor, an
            # OS scaling change) must follow without a reload: the DPR watcher
            # re-derives the automatic scaling_dpi and pushes it.
            ctx = browser.new_context(viewport={"width": 1000, "height": 700},
                                      device_scale_factor=1.0)
            page = ctx.new_page()
            page.goto(f"{H.BASE_URL}/", wait_until="load", timeout=60000)
            page.wait_for_timeout(9000)
            res.check("live dpr: starts at Xft.dpi 96", xft_dpi() == 96,
                      f"got {xft_dpi()}")
            cdp = ctx.new_cdp_session(page)
            cdp.send("Emulation.setDeviceMetricsOverride", {
                "width": 1000, "height": 700, "deviceScaleFactor": 2.0,
                "mobile": False})
            page.wait_for_timeout(9000)
            res.check("live dpr: 2.0 applies Xft.dpi 192 without reload",
                      xft_dpi() == 192, f"got {xft_dpi()}")
            ctx.close()

            # A manual resolution on the automatic default: the framebuffer
            # asked for is what the desktop's UI is sized from, so the pick
            # follows it and not the dpr-1 screen showing it.
            ctx = browser.new_context(viewport={"width": 1000, "height": 700},
                                      device_scale_factor=1.0)
            page = ctx.new_page()
            page.goto(f"{H.BASE_URL}/", wait_until="load", timeout=60000)
            page.wait_for_timeout(9000)
            for (width, height), want in MANUAL_DPIS:
                page.evaluate(
                    "([w, h]) => window.postMessage("
                    "{type: 'setManualResolution', width: w, height: h},"
                    " window.location.origin)", [width, height])
                page.wait_for_timeout(9000)
                res.check(f"a manual {width}x{height} applies Xft.dpi {want}",
                          xft_dpi() == want, f"got {xft_dpi()}")
            page.evaluate("window.postMessage({type: 'resetResolutionToWindow'},"
                          " window.location.origin)")
            page.wait_for_timeout(9000)
            res.check("back on the window size the dpr answers again",
                      xft_dpi() == 96, f"got {xft_dpi()}")
            ctx.close()

            # The derived value is a default: a stored pick governs whatever
            # the resolution is, which is what makes it configurable.
            ctx = browser.new_context(viewport={"width": 1000, "height": 700},
                                      device_scale_factor=1.0)
            ctx.add_init_script(PINNED_INIT)
            page = ctx.new_page()
            page.goto(f"{H.BASE_URL}/", wait_until="load", timeout=60000)
            page.wait_for_timeout(9000)
            res.check(f"a stored pick applies Xft.dpi {PINNED_DPI}",
                      xft_dpi() == PINNED_DPI, f"got {xft_dpi()}")
            page.evaluate(
                "window.postMessage({type: 'setManualResolution',"
                " width: 3840, height: 2160}, window.location.origin)")
            page.wait_for_timeout(9000)
            res.check("and a manual resolution does not re-derive over it",
                      xft_dpi() == PINNED_DPI, f"got {xft_dpi()}")
            ctx.close()
        finally:
            browser.close()
    H.server_stop()
    return 0 if res.summary() else 1


if __name__ == "__main__":
    sys.exit(run())
