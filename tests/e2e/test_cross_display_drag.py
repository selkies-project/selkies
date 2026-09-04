#!/usr/bin/env python3
"""One remote pointer under a held drag, across display pages and pen contact.

Every pointermove of a button-held drag keeps streaming to the page the press
landed on, with client coordinates far past its viewport. The client maps such
positions onto the neighboring display instead of clamping at its own edge,
which pins the remote pointer at the seam for as long as the button is held.
Toward an edge with no neighbor the drag still clamps, and the union of display
rectangles bounds it everywhere, the way a multihead X server bounds its
pointer.

Where the pointer has physically reached the neighbor's window, both axes come
from the box that page published for itself on DISPLAY_CONFIG_UPDATE: the two
browser windows sit at their own desktop origins, and nothing about the
grabbed page's own coordinates says where the pointer is on the other one. The
pages here are placed on a modeled desktop -- browser windows with their own
origins and title bars, one of which moves mid-run -- and every event carries
both frames, the way a browser delivers them. Short of that box the overshoot
converts at the neighbor's CSS-to-remote scale instead.

The primary page runs DPR 2 with a manual resolution (the configuration that
maps input through the presented stream box rather than the viewport) and the
secondary runs DPR 1, so the crossing also proves the per-display scale
conversion. Checked over websockets by watching the X server's own pointer,
then over WebRTC for transport parity; the vertical and left-neighbor
arrangements are driven through the same mapping on synthetic layouts.

The held button is the second half: a page runs an `Input` of its own against
the one remote pointer, so a drag that crosses reaches a page that never saw
the press and leaves one that never sees the release, and a stylus ends contact
with a cancel rather than a release. Every one of them carries what the event
says is held.

Both backends: on X11 the crossing is read back from the X pointer, and on the
Wayland backend, which has none to read, from the coordinates and mask the
pages put on the wire -- the client's half of it, which is where the mapping
and the held button live.

Uses `E2E_DISPLAY` when set; otherwise starts a throwaway Xvfb wide enough for
the two-display union.

Usage: python3 tests/e2e/test_cross_display_drag.py wl
"""
import os
import shutil
import subprocess
import sys
import time
from typing import Any, Optional, Tuple

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright

PRIMARY_CSS = (1512, 806)     # DPR 2 -> remote 3024x1612
# DPR 1, streamed at the primary's density of 2 so the desktop's one DPI shows
# at the same physical size on both screens -> remote 3840x1872 at x=3024.
SECONDARY_CSS = (1920, 936)

# The primary's browser window on the modeled desktop: origin, then the chrome
# above its viewport. The secondary's window carries a title bar the primary's
# does not, and is placed so its stream box starts where the primary's ends,
# the way Add Screen puts one against the next monitor; a letterboxed box puts
# that short of the viewport's own edge.
PRIMARY_WINDOW = (0, 0, 0, 0)
SECONDARY_CHROME = (0, 123)
# Where the run drags it afterwards: the monitor above and to the right, past a
# gap wide enough that the crossing spends some of its travel on neither.
MOVED_OFFSET = (88, -768)

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

# A window this page can be moved on, since the real one cannot be.
DESKTOP_INIT = """(() => {
  window.__desk = [%d, %d];
  window.__inset = [%d, %d];
  Object.defineProperty(window, 'screenX', {get: () => window.__desk[0]});
  Object.defineProperty(window, 'screenY', {get: () => window.__desk[1]});
})()"""

LAYOUT_JS = "window.webrtcInput && window.webrtcInput._layout"


def wait_for(pred, timeout: float = 15) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        if pred():
            return True
        time.sleep(0.25)
    return False


def new_display_context(browser: Any, mode: str, css: Tuple[int, int],
                        dpr: int, window: Tuple[int, int, int, int],
                        url_hash: str = "") -> Any:
    """A display page in its own context, so each can hold its own DPR."""
    ctx = browser.new_context(viewport={"width": css[0], "height": css[1]},
                              device_scale_factor=dpr)
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    ctx.add_init_script(C.WIRE_TAP_JS)
    ctx.add_init_script(MANUAL_INIT)
    ctx.add_init_script(DESKTOP_INIT % window)
    page = ctx.new_page()
    page.goto(H.BASE_URL + "/" + url_hash, wait_until="load")
    return page


def mouse(page: Any, kind: str, cx: int, cy: int, held: bool,
          on_stream: bool = False) -> None:
    """Dispatches a synthetic mouse event the way a captured drag delivers it:
    the press on the stream overlay, everything after on the window, client
    coordinates free to leave the viewport, and the desktop position of the
    same point alongside them as every browser event carries.

    Args:
        on_stream: Deliver to the overlay instead, which is what the page the
            pointer arrives on gets: the browser targets whatever sits under
            the pointer, and on that page the stream does.
    """
    page.evaluate("""([kind, cx, cy, held, onStream]) => {
      const ev = new MouseEvent(kind, {button: 0, buttons: held ? 1 : 0,
        clientX: cx, clientY: cy,
        screenX: window.__desk[0] + window.__inset[0] + cx,
        screenY: window.__desk[1] + window.__inset[1] + cy, bubbles: true});
      if (onStream || kind === 'mousedown') {
        document.getElementById('overlayInput').dispatchEvent(ev);
      } else {
        window.dispatchEvent(ev);
      }
    }""", [kind, cx, cy, held, on_stream])


def wire_pos(page: Any) -> Tuple[int, int]:
    """The point the page's last motion asked for, in the layout's own space:
    its display origin plus the coordinate it sent.

    What the Wayland backend is read by, having no X pointer to query. It is
    the client's half of the crossing -- the mapping under test -- and not the
    compositor's answer to it.
    """
    sent = page.evaluate("() => { const s = window.__wireSent.filter("
                         "d => d.startsWith && d.startsWith('m,'));"
                         " return s.length ? s[s.length - 1] : null; }")
    layout = page.evaluate(LAYOUT_JS) or {}
    parts = (sent or "m,0,0,0,0").split(",")
    return (layout.get("ownX", 0) + int(parts[1]), layout.get("ownY", 0) + int(parts[2]))


def wire_mask(page: Any) -> int:
    """The button mask the page's last pointer message carried."""
    sent = page.evaluate("() => { const s = window.__wireSent.filter("
                         "d => d.startsWith && d.startsWith('m,'));"
                         " return s.length ? s[s.length - 1] : null; }")
    return int((sent or "m,0,0,0,0").split(",")[3])


def wire_buttons(page: Any) -> tuple:
    """The buttons the page's last pointer message carried, lowest first. The
    eraser is not among them: it rides a bit of its own that the server folds
    onto the primary button, which the wire never shows."""
    mask = wire_mask(page)
    return tuple(b for b in range(1, 6) if mask & (1 << (b - 1)))


def moved_to(page: Any, cx: int, cy: int, wayland: bool = False) -> Tuple[int, int]:
    """One held move, then the pointer read back from the X server, or from the
    wire where there is no X pointer to read."""
    mouse(page, "mousemove", cx, cy, True)
    time.sleep(0.3)
    return wire_pos(page) if wayland else C.x11_mouse_pos()


def hovered_to(page: Any, cx: int, cy: int, wayland: bool = False) -> Tuple[int, int]:
    """The same, hovering the stream this page shows: what the pointer reaches
    through the page that owns the position, with no crossing in it."""
    mouse(page, "mousemove", cx, cy, False, on_stream=True)
    time.sleep(0.3)
    return wire_pos(page) if wayland else C.x11_mouse_pos()


def desktop(window: Tuple[int, int, int, int], cx: int, cy: int) -> Tuple[int, int]:
    """Desktop position of a client point on `window`'s page."""
    return (window[0] + window[2] + cx, window[1] + window[3] + cy)


def client_on(window: Tuple[int, int, int, int], gx: int, gy: int) -> Tuple[int, int]:
    """Client point on `window`'s page for a desktop position."""
    return (gx - window[0] - window[2], gy - window[1] - window[3])


def place_window(page: Any, x: int, y: int) -> None:
    """Moves the browser window this page runs in, chrome and all. Nothing on
    the page hears about it: the box it publishes is what carries the move."""
    page.evaluate("([x, y]) => { window.__desk = [x, y]; }", [x, y])


def neighbor_rect(page: Any, seam: int) -> dict:
    """The layout rectangle `page` holds for the display across the seam."""
    layout = page.evaluate(LAYOUT_JS) or {}
    return next((r for r in layout.get("rects", []) if r["x"] == seam), {})


def wait_video(page: Any, mode: str, timeout: float = 45) -> Optional[dict]:
    return (C.wait_wr_video(page, timeout=timeout) if mode == "webrtc"
            else C.wait_ws_video(page, timeout=timeout))


def pen(page: Any, kind: str, button: int, buttons: int, cx: int, cy: int) -> None:
    """Dispatches one stylus pointer event on the stream overlay."""
    page.evaluate("""([kind, button, buttons, cx, cy]) => {
      document.getElementById('overlayInput').dispatchEvent(new PointerEvent(kind, {
        pointerType: 'pen', pointerId: 3, isPrimary: true, button, buttons,
        clientX: cx, clientY: cy,
        screenX: window.__desk[0] + window.__inset[0] + cx,
        screenY: window.__desk[1] + window.__inset[1] + cy,
        bubbles: true, cancelable: true}));
    }""", [kind, button, buttons, cx, cy])
    time.sleep(0.3)


def buttons_held(page: Any, wayland: bool) -> tuple:
    """Buttons the remote pointer holds: from the X server, or from the last
    message `page` sent where there is no X pointer to ask."""
    return wire_buttons(page) if wayland else C.x11_buttons_held()


def pen_contact(res: "H.Results", mode: str, page: Any, wayland: bool) -> None:
    """A stylus drives the same one pointer, and its contact can end unsaid.

    Contact reaches the mouse path, but the browser can end it with a cancel
    carrying button -1 and nothing held, or simply stop reporting the tip on
    the next move. Either way what the event says is held is what is left, and
    the eraser presses as the tip does, since neither X nor wl_pointer has a
    button for it.
    """
    pen(page, "pointerdown", 0, 1, 700, 400)
    res.check(f"[{mode}] pen contact presses", buttons_held(page, wayland) == (1,),
              buttons_held(page, wayland))
    pen(page, "pointermove", -1, 0, 720, 400)
    res.check(f"[{mode}] contact the page never saw end is not still held",
              buttons_held(page, wayland) == (), buttons_held(page, wayland))

    pen(page, "pointerdown", 0, 1, 700, 400)
    pen(page, "pointercancel", -1, 0, 700, 400)
    res.check(f"[{mode}] a cancel releases with no pointerup to follow",
              buttons_held(page, wayland) == (), buttons_held(page, wayland))

    pen(page, "pointerdown", 5, 32, 700, 400)
    if wayland:
        # Only the fold onto the primary button makes the eraser press, and it
        # happens where the pointer is injected; the wire carries the bit.
        res.check(f"[{mode}] the eraser reaches the server on its own bit",
                  wire_mask(page) == 32, wire_mask(page))
    else:
        res.check(f"[{mode}] the eraser presses like the tip",
                  buttons_held(page, wayland) == (1,), buttons_held(page, wayland))
    pen(page, "pointerup", 5, 0, 700, 400)
    res.check(f"[{mode}] the eraser lifts",
              wire_mask(page) == 0 if wayland else buttons_held(page, wayland) == (),
              wire_mask(page) if wayland else buttons_held(page, wayland))


def handoff(res: "H.Results", mode: str, page: Any, dpage: Any, seam: int,
            wayland: bool) -> None:
    """The press lands on one page and everything after it on the neighbor.

    What the browser does once the pointer crosses into the other window: it
    delivers there instead, so the page that owns the grab stops hearing about
    the drag and the neighbor hears about it having missed the press. The
    neighbor holds an `Input` of its own whose mask starts empty, and a move
    reporting that mask would release the button under the window being
    dragged. It carries what the event says is held instead, and the page left
    behind drops its own stale mask rather than pressing again on the next
    hover.
    """
    mouse(page, "mousedown", 700, 400, True)
    time.sleep(0.3)
    res.check(f"[{mode}] the press lands", buttons_held(page, wayland) == (1,),
              buttons_held(page, wayland))
    dpage.evaluate("window.__wireSent.length = 0")
    for cx in (300, 700):
        mouse(dpage, "mousemove", cx, 500, True, on_stream=True)
        time.sleep(0.3)
        pos = wire_pos(dpage) if wayland else C.x11_mouse_pos()
        res.check(f"[{mode}] the drag stays held on the page it crossed to",
                  buttons_held(dpage, wayland) == (1,) and pos[0] > seam,
                  f"{buttons_held(dpage, wayland)} {pos} seam={seam}")
    carried = [m for m in dpage.evaluate(
        "window.__wireSent.filter(d => d.startsWith && d.startsWith('m,'))")
        if m.split(",")[3] == "1"]
    res.check(f"[{mode}] the neighbor's own wire carries the held button",
              len(carried) >= 2, carried[:2])

    mouse(dpage, "mouseup", 700, 500, False, on_stream=True)
    time.sleep(0.3)
    res.check(f"[{mode}] the only release, on the page that ends the drag",
              buttons_held(dpage, wayland) == (), buttons_held(dpage, wayland))

    mouse(page, "mousemove", 700, 400, False, on_stream=True)
    time.sleep(0.3)
    res.check(f"[{mode}] the page left behind does not press again",
              buttons_held(page, wayland) == (), buttons_held(page, wayland))


def managed_window_drag(res: "H.Results", mode: str, page: Any, seam: int,
                        to_client) -> None:
    """A window the session's window manager owns follows a held drag across
    the seam.

    The crossing checked above is the client's half of the gesture. What a user
    sees is the window under the grab, which the window manager moves itself and
    which has to travel exactly as far as the pointer -- across the seam, where
    the overshoot converts at the neighbor's scale rather than the grabbed
    page's, so equal client steps are not equal remote ones.

    The window manager started here already has both displays, so a monitor set
    it never re-read is not what this exercises; that lives in
    `integration/test_monitor_change_announced`, at the layer the re-read
    happens on.

    X11 only: on the Wayland backend the session compositor owns the window and
    the seam is its own (`test_wayland_seam`).

    Args:
        page: The page holding the grab.
        seam: Remote x where the primary ends and its neighbor begins.
        to_client: Maps a remote `(x, y)` to the client coordinates that reach
            it on this page.
    """
    wm = next((w for w in ("openbox", "xfwm4") if shutil.which(w)), None)
    if wm is None or shutil.which("xdotool") is None:
        res.skip(f"[{mode}] a managed window follows the drag across the seam",
                 "no window manager or xdotool on PATH")
        return
    from selkies.Xlib import X, display as x11_display

    display_name = H.require_display()
    env = {**os.environ, "DISPLAY": display_name}
    proc = H.spawn([wm, "--replace"], env=env,
                   stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)
    d = x11_display.Display(display_name)
    win = None
    try:
        screen = d.screen()
        win = screen.root.create_window(
            0, 0, 400, 260, 0, screen.root_depth,
            background_pixel=screen.white_pixel, event_mask=X.StructureNotifyMask)
        win.set_wm_name("selkies-drag-probe")
        win.map()
        d.sync()
        # The reparent is what says a window manager took it; without one there
        # is no frame to grab and nothing this check could mean.
        frame, deadline = None, time.time() + 15
        while time.time() < deadline:
            parent = win.query_tree().parent
            if parent.id != screen.root.id:
                frame = parent
                break
            time.sleep(0.25)
        if frame is None:
            res.skip(f"[{mode}] a managed window follows the drag across the seam",
                     f"{wm} did not take the probe window")
            return
        # Placed so the grab point starts inside the primary and the travel
        # below carries it well past the seam.
        start_x, start_y = seam - 500, 200
        subprocess.run(["xdotool", "windowmove", str(win.id), str(start_x), str(start_y)],
                       env=env, capture_output=True)
        time.sleep(0.6)
        before = frame.get_geometry()
        # A point on the frame's own titlebar, above the client area.
        grab = (before.x + 60, before.y + max(2, (before.height - 260) // 2))
        # What the window has to match is the pointer, not the client-side
        # request: past the seam the overshoot converts at the neighbor's own
        # scale, so equal client steps are not equal remote ones.
        travel, last = 800, None
        mouse(page, "mousedown", *to_client(*grab), True)
        time.sleep(0.3)
        pointer_from = C.x11_mouse_pos()
        for step in range(100, travel + 1, 100):
            last = to_client(grab[0] + step, grab[1])
            mouse(page, "mousemove", *last, True)
            time.sleep(0.12)
        time.sleep(0.4)
        pointer_to = C.x11_mouse_pos()
        mouse(page, "mouseup", *last, False)
        time.sleep(0.6)
        after = frame.get_geometry()
        moved, carried = after.x - before.x, pointer_to[0] - pointer_from[0]
        res.check(f"[{mode}] a managed window follows the drag across the seam",
                  abs(moved - carried) <= 12 and pointer_to[0] > seam > pointer_from[0],
                  f"{wm}: window moved {moved}, pointer {carried} "
                  f"({pointer_from[0]} -> {pointer_to[0]}), seam={seam}")
    finally:
        try:
            if win is not None:
                win.destroy()
            d.sync()
            d.close()
        except Exception:
            pass
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except Exception:
            proc.kill()


def secondary_density(res: "H.Results", mode: str, dpage: Any) -> None:
    """A secondary restored on a screen of another density is refused the
    desktop's DPI.

    The desktop has one DPI and the primary's page owns it; a secondary shown
    at another device pixel ratio re-derives its own on every restore, and the
    WebRTC core sends that re-derivation as the bare sync verb, off the SETTINGS
    path the rule was first put on. Driven by changing the page's device pixel
    ratio under it, which is what moving its window between such screens does.
    """
    mark = len(H.server_log())
    cdp = dpage.context.new_cdp_session(dpage)
    cdp.send("Emulation.setDeviceMetricsOverride", {
        "width": SECONDARY_CSS[0], "height": SECONDARY_CSS[1],
        "deviceScaleFactor": 2, "mobile": False})
    refused = wait_for(
        lambda: H.server_log().find("Ignoring DPI 192 from 'display2'", mark) >= 0, 15)
    res.check(f"[{mode}] a secondary rederiving its density is refused",
              refused, H.server_log(tail=3))
    time.sleep(2.0)
    res.check(f"[{mode}] and the desktop is not rescaled for it",
              H.server_log().find("set DPI to 192", mark) < 0, "")
    cdp.detach()


def drive(res: "H.Results", mode: str, wayland: bool) -> None:
    """One transport's crossing checks; the full set runs on websockets."""
    full = mode == "websockets"
    H.server_start(mode=mode, wayland=wayland)
    with sync_playwright() as p:
        kwargs = {"headless": True, "args": C.BROWSER_ARGS}
        if C.CHROME_PATH:
            kwargs["executable_path"] = C.CHROME_PATH
        browser = p.chromium.launch(**kwargs)
        try:
            page = new_display_context(browser, mode, PRIMARY_CSS, 2, PRIMARY_WINDOW)
            res.check(f"[{mode}] primary video flows", bool(wait_video(page, mode)), "")

            if full:
                # Alone, the page has no multi-display layout and the wire is
                # what it always was: unclamped window math or the sink clamp.
                res.check("single display keeps no layout",
                          page.evaluate(LAYOUT_JS) is None, "")

            dpage = new_display_context(
                browser, mode, SECONDARY_CSS, 1,
                (PRIMARY_CSS[0], -SECONDARY_CHROME[1]) + SECONDARY_CHROME,
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
            res.check(f"[{mode}] secondary laid out at the seam at the primary's density",
                      bool(d2) and d2.get("scale") == 2 and d2.get("w") == SECONDARY_CSS[0] * 2,
                      layout)

            # Each page anchors itself on two events that agree about where
            # its viewport sits, then publishes the box it draws the stream in;
            # the grabbed page maps a crossing through the neighbor's.
            for pg in (page, dpage):
                hovered_to(pg, 700, 400, wayland)
                hovered_to(pg, 760, 430, wayland)
            got_box = wait_for(lambda: neighbor_rect(page, seam).get("scaleX") == 2, 15)
            own_box = (page.evaluate(LAYOUT_JS) or {}).get("own") or {}
            res.check(f"[{mode}] the neighbor publishes the box it draws in",
                      got_box and own_box.get("scaleX", 0) > 0,
                      f"own={own_box} neighbor={neighbor_rect(page, seam)}")
            if not got_box or not own_box.get("scaleX"):
                return

            # Windows edge to edge: the secondary's box starts where the
            # primary's ends, its own title bar accounted for.
            box_left = int(round(own_box["originX"] + own_box["w"] / own_box["scaleX"]))
            box_top = int(round(own_box["originY"]))
            second = (box_left - SECONDARY_CHROME[0],
                      box_top - SECONDARY_CHROME[1]) + SECONDARY_CHROME
            place_window(dpage, second[0], second[1])
            placed = wait_for(
                lambda: neighbor_rect(page, seam).get("originX") == box_left and
                neighbor_rect(page, seam).get("originY") == box_top, 10)
            res.check(f"[{mode}] a window the user moves republishes its box",
                      placed, f"box at {box_left},{box_top}: {neighbor_rect(page, seam)}")

            # The held drag: press well inside, then cross the right edge. The
            # page's own live mapping (measured from two in-bounds samples)
            # locates the edge in client coordinates, so the letterboxed WebRTC
            # video box maps as exactly as the websockets canvas.
            mouse(page, "mousedown", 700, 400, True)
            p1 = moved_to(page, 700, 400, wayland)
            p2 = moved_to(page, 1100, 400, wayland)
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
            d2_scale = neighbor_rect(page, seam).get("scaleX")
            over = moved_to(page, edge + 300, 400, wayland)
            far = moved_to(page, edge + 1088, 400, wayland)
            res.check(f"[{mode}] held drag crosses the seam",
                      over[0] > seam, f"{over} seam={seam}")
            res.check(f"[{mode}] overshoot travels at the neighbor's scale",
                      abs(over[0] - (seam + 300 * d2_scale)) <= 4
                      and abs(far[0] - (seam + 1088 * d2_scale)) <= 4,
                      f"{over} {far} seam={seam} scale={d2_scale}")

            if full:
                clamped = moved_to(page, edge + 3000, 400, wayland)
                res.check("far overshoot clamps at the union's edge",
                          abs(clamped[0] - (union_r - 1)) <= 1, f"{clamped} union={union_r}")
                low = moved_to(page, 2000, 1000, wayland)
                res.check("the corner below the neighbor's bottom is out of reach",
                          low[1] <= d2["y"] + d2["h"], f"{low} d2={d2}")
                left = moved_to(page, -300, 400, wayland)
                res.check("an edge with no neighbor still clamps",
                          left[0] == 0, left)

            end = moved_to(page, edge + 1088, 400, wayland)
            mouse(page, "mouseup", edge + 1088, 400, False)
            time.sleep(0.3)
            released = wire_pos(page) if wayland else C.x11_mouse_pos()
            res.check(f"[{mode}] release lands across the seam",
                      released[0] > seam and abs(released[0] - end[0]) <= 2,
                      f"end={end} released={released}")

            # The pointer is physically over the neighbor's window by now, so
            # the crossing has to reach the pixel that page shows there. Read
            # through its own mapping, which has no crossing in it.
            spot = desktop(PRIMARY_WINDOW, edge + 1088, 400)
            neighbor = hovered_to(dpage, *client_on(second, *spot), wayland)
            res.check(f"[{mode}] a crossing lands where the neighbor puts the same spot",
                      abs(neighbor[0] - end[0]) <= 1 and abs(neighbor[1] - end[1]) <= 1,
                      f"drag={end} neighbor={neighbor} desktop={spot}")
            sent = page.evaluate("window.__wireSent.filter(d => d.startsWith && d.startsWith('m,'))")
            crossing = [m for m in sent if int(m.split(",")[1]) > seam]
            res.check(f"[{mode}] the wire itself carries the crossing",
                      len(crossing) > 0 and crossing[-1].split(",")[3] == "0",
                      crossing[-1:] or sent[-3:])

            handoff(res, mode, page, dpage, seam, wayland)
            pen_contact(res, mode, page, wayland)

            if full:
                # The window dragged onto a monitor of its own, above and right
                # of where it was. The pages share no edge and no top now, and
                # the grabbed page's own coordinates say nothing about where the
                # pointer is on the other one: only the published box does.
                moved = (second[0] + MOVED_OFFSET[0],
                         second[1] + MOVED_OFFSET[1]) + SECONDARY_CHROME
                place_window(dpage, moved[0], moved[1])
                moved_ok = wait_for(
                    lambda: neighbor_rect(page, seam).get("originY") ==
                    box_top + MOVED_OFFSET[1], 10)
                res.check("a window moved with no pointer over it republishes its box",
                          moved_ok, f"{moved} {neighbor_rect(page, seam)}")
                spot = desktop(moved, 400, 845)
                mouse(page, "mousedown", 700, 400, True)
                crossed = moved_to(page, *client_on(PRIMARY_WINDOW, *spot), wayland)
                mouse(page, "mouseup", *client_on(PRIMARY_WINDOW, *spot), False)
                time.sleep(0.3)
                neighbor = hovered_to(dpage, *client_on(moved, *spot), wayland)
                res.check("a crossing onto a window at its own desktop origin lands there",
                          abs(crossed[0] - neighbor[0]) <= 1 and
                          abs(crossed[1] - neighbor[1]) <= 1,
                          f"drag={crossed} neighbor={neighbor} desktop={spot}")

                res.check("vertical and left arrangements map the same way",
                          page.evaluate(SYNTH_JS), "see SYNTH_JS")
                res.check("a client frame that scales against the desktop publishes nothing",
                          page.evaluate(ANCHOR_JS), "see ANCHOR_JS")

            if not wayland:
                managed_window_drag(
                    res, mode, page, seam,
                    lambda sx, sy: (int(round(700 + (sx - p1[0]) / own_scale)),
                                    int(round(400 + (sy - p1[1]) / own_scale))))
            secondary_density(res, mode, dpage)
        finally:
            browser.close()


# The offset between a page's client coordinates and the desktop's describes
# the page only while nothing scales one frame and not the other. Page zoom
# does, and so do screen coordinates reported in device pixels; the anchor has
# to notice rather than publish a box wrong by the pointer's own position.
ANCHOR_JS = """(() => {
  const proto = Object.getPrototypeOf(window.webrtcInput);
  const feed = (k) => {
    const sent = [];
    const o = Object.assign(Object.create(proto), {
      send: (m) => sent.push(m),
      _layout: {own: {originX: 0, originY: 0, scaleX: 0, scaleY: 0}},
      _anchorX: null, _anchorY: null, _chromeInsetX: null, _chromeInsetY: null,
      _sampleClientX: -1e9, _sampleClientY: -1e9,
      _sampleAnchorX: NaN, _sampleAnchorY: NaN,
      _streamBox: () => ({left: 0, top: 0, scaleX: 1, scaleY: 1}),
    });
    for (let cx = 0; cx < 400; cx += 17) {
      o._noteScreenAnchor({clientX: cx, clientY: 0, screenX: 100 + k * cx, screenY: 0});
    }
    return sent.length;
  };
  return feed(1) > 0 && feed(1.5) === 0 && feed(2) === 0 && feed(0.5) === 0;
})()"""

# Drives the mapping helper itself over arrangements the session above does not
# realize: a lower neighbor at double scale, a left neighbor, two windows whose
# desktop origins share neither an edge nor a top, and two whose boxes overlap.
# Runs on a bare object, which publishes nothing, so the live page keeps its
# own layout.
SYNTH_JS = """(() => {
  const proto = Object.getPrototypeOf(window.webrtcInput);
  const map = (layouts, own, rx, ry, sx, sy, gx, gy) => {
    const o = Object.assign(Object.create(proto),
                            {_layout: null, x: 0, y: 0, _watchScreenGeometry() {}});
    o.setDisplayLayouts(layouts, own);
    return o._mapToLayout(rx, ry, sx, sy, gx, gy) ? [o.x, o.y] : null;
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
  // Windows 200 apart across and 400 up: the desktop position decides both
  // axes, including one the grabbed page still reads as inside its own height.
  const apart = {primary: {x: 0, y: 0, w: 1000, h: 500, scale: 1,
                           originX: 0, originY: 100, scaleX: 1, scaleY: 1},
                 display2: {x: 1000, y: 0, w: 800, h: 400, scale: 1,
                            originX: 1200, originY: -300, scaleX: 1, scaleY: 1}};
  const d = map(apart, 'primary', 1300, -150, 1, 1, 1300, -50);
  if (!d || d[0] !== 1100 || d[1] !== 250) return false;
  // Past that box -- the desktop beside the window -- the overshoot converts
  // and clamps as it does with no box published at all.
  const e = map(apart, 'primary', 2100, -150, 1, 1, 2100, -50);
  if (!e || e[0] !== 1800 || e[1] !== 0) return false;
  // Boxes that overlap on the desktop -- one window over the other, or an
  // engine reporting screen coordinates relative to its own window -- are
  // not crossed through: the overshoot converts as with no box at all.
  const over = {primary: {x: 0, y: 0, w: 1000, h: 500, scale: 1,
                          originX: 0, originY: 100, scaleX: 1, scaleY: 1},
                display2: {x: 1000, y: 0, w: 800, h: 400, scale: 1,
                           originX: 600, originY: 0, scaleX: 1, scaleY: 1}};
  const f = map(over, 'primary', 1300, 50, 1, 1, 1300, 150);
  if (!f || f[0] !== 1300 || f[1] !== 50) return false;
  // Inside its own rectangle the mapping stands aside.
  return map(down, 'primary', 500, 250, 1, 1) === null;
})()"""


def main() -> "H.Results":
    """One backend per run: `x11` reads the crossing back from the X pointer,
    `wl` from the wire, having none to read."""
    backend = sys.argv[1] if len(sys.argv) > 1 else "x11"
    wayland = backend == "wl"
    res = H.Results(f"cross-display-drag-{backend}")
    xproc = None
    if not H.TEST_DISPLAY:
        xproc, xdisp = H.private_x_server(width=8192, height=4096)
        H.TEST_DISPLAY = xdisp
    try:
        for mode in ("websockets", "webrtc"):
            drive(res, mode, wayland)
    finally:
        H.server_stop()
        if xproc is not None:
            H.stop_x_server(xproc, H.TEST_DISPLAY)
    res.summary()
    return res


if __name__ == "__main__":
    r = main()
    sys.exit(0 if not r.failed() else 1)
