#!/usr/bin/env python3
"""Two displays on screens of different pixel density share one physical UI size.

The desktop renders its UI at one DPI, the primary page's. A secondary page
therefore streams at the primary's density rather than its own: it asks for
its CSS size times the primary's stream-pixels-per-CSS-pixel, and the browser
resamples the stream by the ratio to its own screen. Checked both ways round
(a denser primary, then a denser secondary), on both transports: the monitor
the server realizes for the secondary, the secondary's own buffer against its
CSS box, the scale it publishes with the layout, and where a pointer on it
lands.

Uses `E2E_DISPLAY` when set; otherwise starts a throwaway Xvfb wide enough for
the two-display union.
Usage: python3 tests/e2e/test_mixed_dpi.py
"""
import json
import os
import subprocess
import sys
import time
from typing import Any, Dict, Optional, Tuple

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright

PRIMARY_CSS = (1512, 806)
SECONDARY_CSS = (1920, 936)

# A window this page can report a desktop position for, since the real one
# cannot be moved: the pages sit side by side with no chrome above them.
DESKTOP_INIT = """(() => {
  window.__desk = [%d, 0];
  window.__inset = [0, 0];
  Object.defineProperty(window, 'screenX', {get: () => window.__desk[0]});
  Object.defineProperty(window, 'screenY', {get: () => 0});
})()"""
LAYOUT_JS = "window.webrtcInput && window.webrtcInput._layout"
SINK_JS = """() => {
  const canvas = document.getElementById('videoCanvas');
  const video = document.getElementById('stream');
  const buffer = (canvas && canvas.width > 0) ? [canvas.width, canvas.height]
                 : (video && video.videoWidth > 0) ? [video.videoWidth, video.videoHeight] : null;
  if (!buffer) return null;
  // Whichever sink is drawn: the canvas, the worker's canvas, or the video.
  let css = [0, 0];
  for (const id of ['videoCanvas', 'videoWorkerCanvas', 'videoStream', 'stream']) {
    const el = document.getElementById(id);
    const r = el && el.getBoundingClientRect ? el.getBoundingClientRect() : null;
    if (r && r.width > 0) { css = [Math.round(r.width), Math.round(r.height)]; break; }
  }
  return {w: buffer[0], h: buffer[1], cssW: css[0], cssH: css[1],
          density: window.webrtcInput ? window.webrtcInput._cursorDensity() : null};
}"""


def wait_for(pred, timeout: float = 15) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if pred():
            return True
        time.sleep(0.25)
    return False


def monitors(display: str) -> Dict[str, Tuple[int, int, int, int]]:
    """The RandR monitors the server publishes: name -> (w, h, x, y)."""
    out = subprocess.run(["xrandr", "--display", display, "--listmonitors"],
                         capture_output=True, text=True).stdout
    found = {}
    for line in out.splitlines():
        parts = line.split()
        if len(parts) < 3 or not parts[0].rstrip(":").isdigit():
            continue
        name = parts[1].lstrip("+*")
        geom = parts[2]  # WIDTH/mmxHEIGHT/mm+X+Y
        size, x, y = geom.split("+")
        w = int(size.split("x")[0].split("/")[0])
        h = int(size.split("x")[1].split("/")[0])
        found[name] = (w, h, int(x), int(y))
    return found


def new_display_context(browser: Any, mode: str, css: Tuple[int, int], dpr: int,
                        desk_x: int, url_hash: str = "") -> Any:
    ctx = browser.new_context(viewport={"width": css[0], "height": css[1]},
                              device_scale_factor=dpr)
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    ctx.add_init_script(C.WIRE_TAP_JS)
    ctx.add_init_script(DESKTOP_INIT % desk_x)
    page = ctx.new_page()
    page.console_errors = []
    page.console_trace = []
    def on_console(m):
        if m.type == "error":
            page.console_errors.append(m.text)
        if "settings update" in m.text or "Sending resolution" in m.text or "density" in m.text:
            page.console_trace.append(m.text[:110])
    page.on("console", on_console)
    page.goto(H.BASE_URL + "/" + url_hash, wait_until="load")
    return page


def wait_video(page: Any, mode: str, timeout: float = 45) -> Optional[dict]:
    return (C.wait_wr_video(page, timeout=timeout) if mode == "webrtc"
            else C.wait_ws_video(page, timeout=timeout))


def hover(page: Any, cx: int, cy: int) -> Tuple[int, int]:
    """Hovers the stream at a client point and reads the X pointer back."""
    page.evaluate("""([cx, cy]) => {
      const ev = new MouseEvent('mousemove', {buttons: 0, clientX: cx, clientY: cy,
        screenX: window.__desk[0] + cx, screenY: cy, bubbles: true});
      document.getElementById('overlayInput').dispatchEvent(ev);
    }""", [cx, cy])
    time.sleep(0.3)
    return C.x11_mouse_pos()


def secondary_rect(page: Any) -> dict:
    layout = page.evaluate(LAYOUT_JS) or {}
    return next((r for r in layout.get("rects", []) if r["x"] > 0), {})


def density_case(res: "H.Results", mode: str, browser: Any,
                 primary_dpr: int, secondary_dpr: int) -> None:
    """One arrangement: the secondary must end up at the primary's density."""
    tag = f"[{mode}] primary x{primary_dpr}, secondary x{secondary_dpr}:"
    page = new_display_context(browser, mode, PRIMARY_CSS, primary_dpr, 0)
    res.check(f"{tag} primary video flows", bool(wait_video(page, mode)), "")
    primary_w = PRIMARY_CSS[0] * primary_dpr
    dpage = new_display_context(browser, mode, SECONDARY_CSS, secondary_dpr,
                                PRIMARY_CSS[0], "#display2-right")
    res.check(f"{tag} secondary video flows", bool(wait_video(dpage, mode)), "")
    want = (SECONDARY_CSS[0] * primary_dpr, SECONDARY_CSS[1] * primary_dpr)
    try:
        # The server realizes the secondary's monitor at the primary's density;
        # a secondary whose own density differs re-requests once the layout
        # carries the primary's scale.
        realized = wait_for(
            lambda: monitors(H.TEST_DISPLAY).get("selkies-display2", (0, 0))[:2] == want, 30)
        got = monitors(H.TEST_DISPLAY)
        res.check(f"{tag} the secondary's monitor is its CSS size at the primary's density",
                  realized, f"want {want} monitors={got}")
        res.check(f"{tag} the primary's monitor keeps its own density",
                  got.get("selkies-primary", (0,))[0] == primary_w, got)
        res.check(f"{tag} the secondary sits at the primary's right edge",
                  got.get("selkies-display2", (0, 0, -1))[2] == primary_w, got)

        # The page draws that many stream pixels into its CSS box, and maps
        # the cursor and pointer at the same density.
        sink_ok = wait_for(lambda: (dpage.evaluate(SINK_JS) or {}).get("w") == want[0], 15)
        sink = dpage.evaluate(SINK_JS) or {}
        res.check(f"{tag} the secondary's buffer is at the primary's density in its own CSS box",
                  sink_ok and abs(sink.get("cssW", 0) - SECONDARY_CSS[0]) <= 2
                  and abs(sink.get("density", 0) - primary_dpr) < 0.01, sink)
        laid = wait_for(lambda: secondary_rect(page).get("scale") == primary_dpr, 15)
        settings = dpage.evaluate("(window.__wireSent || []).filter(d => typeof d === 'string' && d.startsWith('SETTINGS,'))")
        reported = [json.loads(m[len('SETTINGS,'):]).get("displayScale") for m in settings]
        res.check(f"{tag} the secondary publishes the primary's scale with the layout",
                  laid, f"reported displayScale={reported} trace={dpage.console_trace[-6:]}")

        # A point on the secondary lands where the density puts it.
        pos = hover(dpage, 100, 100)
        expect = (primary_w + 100 * primary_dpr, 100 * primary_dpr)
        res.check(f"{tag} a pointer on the secondary maps at the primary's density",
                  abs(pos[0] - expect[0]) <= 2 and abs(pos[1] - expect[1]) <= 2,
                  f"got {pos} expected {expect}")
        errors = [m for m in page.console_errors + dpage.console_errors
                  if "favicon" not in m and "ERR_" not in m and "404" not in m]
        res.check(f"{tag} no console errors", not errors, errors[:3])
    finally:
        dpage.context.close()
        page.context.close()
        # The secondary's monitor is gone with its page before the next case.
        wait_for(lambda: "selkies-display2" not in monitors(H.TEST_DISPLAY), 15)


def drive(res: "H.Results", mode: str) -> None:
    H.server_start(mode=mode, wayland=False)
    with sync_playwright() as p:
        kwargs = {"headless": True, "args": C.BROWSER_ARGS}
        if C.CHROME_PATH:
            kwargs["executable_path"] = C.CHROME_PATH
        browser = p.chromium.launch(**kwargs)
        try:
            density_case(res, mode, browser, 2, 1)
            density_case(res, mode, browser, 1, 2)
        finally:
            browser.close()


def main() -> "H.Results":
    res = H.Results("mixed-dpi")
    xproc = None
    if not H.TEST_DISPLAY:
        xproc, xdisp = H.private_x_server(width=8192, height=4096)
        H.TEST_DISPLAY = xdisp
    try:
        for mode in ("websockets", "webrtc"):
            drive(res, mode)
    finally:
        H.server_stop()
        if xproc is not None:
            H.stop_x_server(xproc, H.TEST_DISPLAY)
    res.summary()
    return res


if __name__ == "__main__":
    r = main()
    sys.exit(0 if not r.failed() else 1)
