#!/usr/bin/env python3
"""A held drag crosses from one display page into its neighbor's region.

Every pointermove of a button-held drag keeps streaming to the page the press
landed on, with client coordinates far past its viewport. The client maps such
positions into the neighboring display's rectangle -- converting the overshoot
at the neighbor's own CSS-to-remote scale, the layout and scales arriving on
DISPLAY_CONFIG_UPDATE -- instead of clamping at its own edge, which pins the
remote pointer at the seam for as long as the button is held. Toward an edge
with no neighbor the drag still clamps, and the union of display rectangles
bounds it everywhere, the way a multihead X server bounds its pointer.

The primary page runs DPR 2 with a manual resolution (the configuration that
maps input through the presented stream box rather than the viewport) and the
secondary runs DPR 1, so the crossing also proves the per-display scale
conversion. Checked over websockets by watching the X server's own pointer,
then over WebRTC for transport parity; the vertical and left-neighbor
arrangements are driven through the same mapping on synthetic layouts.

Uses `E2E_DISPLAY` when set; otherwise starts a throwaway Xvfb wide enough for
the two-display union.

Usage: python3 tests/e2e/test_cross_display_drag.py
"""
import os
import sys
import time
from typing import Any, Optional, Tuple

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright

PRIMARY_CSS = (1512, 806)     # DPR 2 -> remote 3024x1612
SECONDARY_CSS = (1920, 936)   # DPR 1 -> remote 1920x936 at x=3024

# Persists the primary's manual resolution before load, as a stored user pick.
MANUAL_INIT = """(() => {
  if (window.location.hash.startsWith('#display2')) return;
  const prefix = (window.location.origin + window.location.pathname)
    .replace(/[^a-zA-Z0-9._-]/g, '_');
  localStorage.setItem(prefix + '_manual_resolution', 'true');
  localStorage.setItem(prefix + '_manual_width', '%d');
  localStorage.setItem(prefix + '_manual_height', '%d');
  localStorage.setItem(prefix + '_scaleLocallyManual', 'true');
})()""" % (PRIMARY_CSS[0] * 2, PRIMARY_CSS[1] * 2)

LAYOUT_JS = "window.webrtcInput && window.webrtcInput._layout"


def wait_for(pred, timeout: float = 15) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if pred():
            return True
        time.sleep(0.25)
    return False


def new_display_context(browser: Any, mode: str, css: Tuple[int, int],
                        dpr: int, url_hash: str = "") -> Any:
    """A display page in its own context, so each can hold its own DPR."""
    ctx = browser.new_context(viewport={"width": css[0], "height": css[1]},
                              device_scale_factor=dpr)
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    ctx.add_init_script(C.WIRE_TAP_JS)
    ctx.add_init_script(MANUAL_INIT)
    page = ctx.new_page()
    page.goto(H.BASE_URL + "/" + url_hash, wait_until="load")
    return page


def mouse(page: Any, kind: str, cx: int, cy: int, held: bool) -> None:
    """Dispatches a synthetic mouse event the way a captured drag delivers it:
    the press on the stream overlay, everything after on the window, client
    coordinates free to leave the viewport."""
    page.evaluate("""([kind, cx, cy, held]) => {
      const ev = new MouseEvent(kind, {button: 0, buttons: held ? 1 : 0,
        clientX: cx, clientY: cy, bubbles: true});
      if (kind === 'mousedown') {
        document.getElementById('overlayInput').dispatchEvent(ev);
      } else {
        window.dispatchEvent(ev);
      }
    }""", [kind, cx, cy, held])


def moved_to(page: Any, cx: int, cy: int) -> Tuple[int, int]:
    """One held move, then the pointer read back from the X server."""
    mouse(page, "mousemove", cx, cy, True)
    time.sleep(0.3)
    return C.x11_mouse_pos()


def wait_video(page: Any, mode: str, timeout: float = 45) -> Optional[dict]:
    return (C.wait_wr_video(page, timeout=timeout) if mode == "webrtc"
            else C.wait_ws_video(page, timeout=timeout))


def drive(res: "H.Results", mode: str) -> None:
    """One transport's crossing checks; the full set runs on websockets."""
    full = mode == "websockets"
    H.server_start(mode=mode, wayland=False)
    with sync_playwright() as p:
        kwargs = {"headless": True, "args": C.BROWSER_ARGS}
        if C.CHROME_PATH:
            kwargs["executable_path"] = C.CHROME_PATH
        browser = p.chromium.launch(**kwargs)
        try:
            page = new_display_context(browser, mode, PRIMARY_CSS, 2)
            res.check(f"[{mode}] primary video flows", bool(wait_video(page, mode)), "")

            if full:
                # Alone, the page has no multi-display layout and the wire is
                # what it always was: unclamped window math or the sink clamp.
                res.check("single display keeps no layout",
                          page.evaluate(LAYOUT_JS) is None, "")

            dpage = new_display_context(browser, mode, SECONDARY_CSS, 1,
                                        "#display2-right")
            res.check(f"[{mode}] secondary video flows", bool(wait_video(dpage, mode)), "")
            got_layout = wait_for(lambda: (page.evaluate(LAYOUT_JS) or {}).get("rects")
                                  and len(page.evaluate(LAYOUT_JS)["rects"]) == 2, 30)
            layout = page.evaluate(LAYOUT_JS) or {}
            res.check(f"[{mode}] both rectangles reach the grabbed page", got_layout, layout)
            if not got_layout:
                return
            seam = layout["ownW"]
            union_r = max(r["x"] + r["w"] for r in layout["rects"])
            d2 = next((r for r in layout["rects"] if r["x"] == seam), None)
            res.check(f"[{mode}] secondary laid out at the seam with its scale",
                      bool(d2) and d2.get("scale") == 1, layout)

            # The held drag: press well inside, then cross the right edge. The
            # page's own live mapping (measured from two in-bounds samples)
            # locates the edge in client coordinates, so the letterboxed WebRTC
            # video box maps as exactly as the websockets canvas.
            mouse(page, "mousedown", 700, 400, True)
            p1 = moved_to(page, 700, 400)
            p2 = moved_to(page, 1100, 400)
            own_scale = (p2[0] - p1[0]) / 400.0
            res.check(f"[{mode}] in-bounds motion maps linearly",
                      own_scale > 0, f"{p1} {p2}")
            if own_scale <= 0:
                return
            edge = int(round((700 - p1[0] / own_scale) + seam / own_scale))
            if full:
                res.check("manual DPR-2 mapping is exact",
                          abs(p1[0] - 1400) <= 2 and abs(p1[1] - 800) <= 2 and edge == PRIMARY_CSS[0],
                          f"{p1} edge={edge}")
            over = moved_to(page, edge + 300, 400)
            far = moved_to(page, edge + 1088, 400)
            res.check(f"[{mode}] held drag crosses the seam",
                      over[0] > seam, f"{over} seam={seam}")
            res.check(f"[{mode}] overshoot travels at the neighbor's scale",
                      abs(over[0] - (seam + 300)) <= 4 and abs(far[0] - (seam + 1088)) <= 4,
                      f"{over} {far} seam={seam}")

            if full:
                clamped = moved_to(page, edge + 3000, 400)
                res.check("far overshoot clamps at the union's edge",
                          abs(clamped[0] - (union_r - 1)) <= 1, f"{clamped} union={union_r}")
                low = moved_to(page, 2000, 700)
                res.check("the dead corner past a shorter neighbor is out of reach",
                          low[1] <= d2["y"] + d2["h"], f"{low} d2={d2}")
                left = moved_to(page, -300, 400)
                res.check("an edge with no neighbor still clamps",
                          left[0] == 0, left)

            end = moved_to(page, edge + 1088, 400)
            mouse(page, "mouseup", edge + 1088, 400, False)
            time.sleep(0.3)
            released = C.x11_mouse_pos()
            res.check(f"[{mode}] release lands across the seam",
                      released[0] > seam and abs(released[0] - end[0]) <= 2,
                      f"end={end} released={released}")
            sent = page.evaluate("window.__wireSent.filter(d => d.startsWith && d.startsWith('m,'))")
            crossing = [m for m in sent if int(m.split(",")[1]) > seam]
            res.check(f"[{mode}] the wire itself carries the crossing",
                      len(crossing) > 0 and crossing[-1].split(",")[3] == "0",
                      crossing[-1:] or sent[-3:])

            if full:
                # The neighbor page maps the same physical spot to the same
                # remote pixel, so the hover handoff after release cannot jump.
                dpage.evaluate("""([cx, cy]) => {
                  document.getElementById('overlayInput').dispatchEvent(
                    new MouseEvent('mousemove', {buttons: 0, clientX: cx, clientY: cy, bubbles: true}));
                }""", [1088, 800])
                time.sleep(0.3)
                hover = C.x11_mouse_pos()
                res.check("hover handoff to the neighbor page does not jump",
                          abs(hover[0] - released[0]) <= 1 and abs(hover[1] - released[1]) <= 1,
                          f"released={released} hover={hover}")

                res.check("vertical and left arrangements map the same way",
                          page.evaluate(SYNTH_JS), "see SYNTH_JS")
        finally:
            browser.close()


# Drives the mapping helper itself over arrangements the session above does not
# realize: a lower neighbor at double scale, and a left neighbor. Runs on a bare
# object so the live page keeps its own layout.
SYNTH_JS = """(() => {
  const proto = Object.getPrototypeOf(window.webrtcInput);
  const map = (layouts, own, rx, ry, sx, sy) => {
    const o = {_layout: null, x: 0, y: 0};
    proto.setDisplayLayouts.call(o, layouts, own);
    return proto._mapToLayout.call(o, rx, ry, sx, sy) ? [o.x, o.y] : null;
  };
  const down = {primary: {x: 0, y: 0, w: 1000, h: 500, scale: 1},
                display2: {x: 0, y: 500, w: 800, h: 400, scale: 2}};
  // 40 CSS px below the seam at scale 2 -> 80 remote px into the neighbor.
  const a = map(down, 'primary', 300, 540, 1, 1);
  if (!a || a[0] !== 300 || a[1] !== 580) return false;
  // Sideways past the narrower neighbor clamps to the nearest union point,
  // here the primary's own bottom edge.
  const b = map(down, 'primary', 950, 540, 1, 1);
  if (!b || b[0] !== 950 || b[1] !== 500) return false;
  const left = {primary: {x: 600, y: 0, w: 1000, h: 500, scale: 2},
                display2: {x: 0, y: 0, w: 600, h: 500, scale: 1}};
  // 100 own-remote px past the left edge at own scale 2 -> 50 remote px.
  const c = map(left, 'primary', -100, 200, 2, 2);
  if (!c || c[0] !== -50 || c[1] !== 200) return false;
  // Inside its own rectangle the mapping stands aside.
  return map(down, 'primary', 500, 250, 1, 1) === null;
})()"""


def main() -> "H.Results":
    res = H.Results("cross-display-drag")
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
