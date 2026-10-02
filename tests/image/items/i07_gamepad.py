"""7. Test the gamepad (first, the touch gamepad, and your real gamepad if you happen to have it), both on Firefox and Chrome (they each use different joypad interfaces) with https://hardwaretester.com/gamepad, or any other native client. You need to press all buttons and test four diagonal directions for each thumbstick.

The real pad is a virtual one in the client page (the Gamepad API the client
polls, tests/tools/pad_init.js), and the touch gamepad is the dashboard's own,
tapped and dragged with touches (CDP touch in Chrome, synthetic touches in
WebKit; Firefox's persistent profile cannot emulate touch). Every button and
four diagonals of each stick must reach a tester page in the session's own
Chrome and Firefox, which read the pads through the session's joypad devices.
"""
import time
from typing import Any

from image_lib import H, session_browser, stop_session_browser

ITEM = 7
TITLE = "gamepad: touch pad and virtual pad"
DIAGONALS = ((1, 1), (1, -1), (-1, 1), (-1, -1))
TOUCH_JS = """([type, x, y]) => {
  const el = document.elementFromPoint(x, y);
  if (!el) return false;
  const t = new Touch({identifier: 7, target: el, clientX: x, clientY: y, pageX: x, pageY: y});
  const list = type === 'touchend' ? [] : [t];
  el.dispatchEvent(new TouchEvent(type, {touches: list, targetTouches: list, changedTouches: [t],
                                          bubbles: true, cancelable: true}));
  return true;
}"""


def union(report: Any) -> tuple:
    """Every button index pressed and every stick diagonal seen, over all the session's pads."""
    pressed, diag = set(), set()
    for pad in (report or {}).values():
        pressed |= set(pad.get("pressed", []))
        diag |= set(pad.get("diagonals", []))
    return pressed, diag


def drive_virtual(page: Any) -> None:
    for i in range(17):
        page.evaluate(f"window.__padPress({i}, 1)")
        time.sleep(0.15)
        page.evaluate(f"window.__padPress({i}, 0)")
        time.sleep(0.1)
    for stick in (0, 1):
        for x, y in DIAGONALS:
            page.evaluate(f"window.__padAxis({2 * stick}, {0.95 * x}); window.__padAxis({2 * stick + 1}, {0.95 * y})")
            time.sleep(0.35)
            page.evaluate(f"window.__padAxis({2 * stick}, 0); window.__padAxis({2 * stick + 1}, 0)")
            time.sleep(0.2)


def touch(page: Any, cdp: Any, kind: str, x: float, y: float) -> None:
    if cdp is not None:
        cdp_type = {"touchstart": "touchStart", "touchmove": "touchMove", "touchend": "touchEnd"}[kind]
        points = [] if kind == "touchend" else [{"x": x, "y": y, "id": 7}]
        cdp.send("Input.dispatchTouchEvent", {"type": cdp_type, "touchPoints": points})
    else:
        page.evaluate(TOUCH_JS, [kind, x, y])


def drive_touch(page: Any, cdp: Any, moves: bool) -> tuple:
    """Tap every button of the touch gamepad, tap each stick (its click) and, where
    touches can move, push it into its four diagonals; `(controls tapped, sticks)`."""
    rects = page.evaluate("""() => [...document.querySelectorAll(
        '#universal-touch-gamepad-controls-overlay .touch-button, #universal-touch-gamepad-controls-overlay .touch-analog-trigger')]
        .map(e => { const r = e.getBoundingClientRect(); return [r.x + r.width / 2, r.y + r.height / 2]; })""")
    sticks = page.evaluate("""() => [...document.querySelectorAll(
        '#universal-touch-gamepad-controls-overlay .touch-joystick-base')]
        .map(e => { const r = e.getBoundingClientRect(); return [r.x + r.width / 2, r.y + r.height / 2, r.width / 2]; })""")
    for x, y in rects + [(x, y) for x, y, _ in sticks]:
        if cdp is None:
            page.touchscreen.tap(x, y)
        else:
            touch(page, cdp, "touchstart", x, y)
            time.sleep(0.12)
            touch(page, cdp, "touchend", x, y)
        time.sleep(0.2)
    for x, y, r in sticks if moves else ():
        for dx, dy in DIAGONALS:
            touch(page, cdp, "touchstart", x, y)
            time.sleep(0.25)
            for step in (0.4, 0.8, 1.0):
                touch(page, cdp, "touchmove", x + dx * r * step, y + dy * r * step)
                time.sleep(0.08)
            time.sleep(0.3)
            touch(page, cdp, "touchend", x + dx * r, y + dy * r)
            time.sleep(0.2)
    return len(rects), len(sticks)


def run(cell: Any) -> None:
    R = cell.res
    for which in ("chrome", "firefox"):
        cell.clear_report("gamepad")
        if not session_browser(cell.target, which, "/gamepad.html", kiosk=False, profile="pad"):
            R.skip(f"a gamepad tester in the session's {which}", "not installed in the image")
            continue
        time.sleep(4)
        # The virtual pad.
        page = cell.open(init=(H.pad_init_js(),))
        time.sleep(2)
        drive_virtual(page)
        time.sleep(1.5)
        pressed, diag = union(cell.report("gamepad", 10))
        R.check(f"every button of a virtual pad reaches the session's {which}", len(pressed) >= 17,
                f"{len(pressed)} of 17: {sorted(pressed)}")
        R.check(f"four diagonals of both sticks reach the session's {which}",
                {f"{s}:{a}{b}" for s in (0, 1) for a in "+-" for b in "+-"} <= diag, sorted(diag))
        page.close()
        # The touch gamepad.
        if cell.engine == "firefox":
            R.skip(f"the touch gamepad into the session's {which}", "Firefox's persistent profile emulates no touch")
        else:
            # A fresh tester: what the virtual pad pressed must not count for the touch pad.
            cell.go("/gamepad.html", ch="pad")
            time.sleep(3)
            cell.clear_report("gamepad")
            tctx = None
            if cell.engine == "webkit":
                tctx = cell.ctx.browser.new_context(viewport={"width": 1280, "height": 720}, has_touch=True,
                                                    ignore_https_errors=True)
            page = cell.open(ctx=tctx)
            cdp = None
            if cell.engine == "chromium":
                cdp = cell.ctx.new_cdp_session(page)
                cdp.send("Emulation.setTouchEmulationEnabled", {"enabled": True, "maxTouchPoints": 5})
            page.evaluate("""() => window.postMessage({type: 'TOUCH_GAMEPAD_SETUP',
                payload: {targetDivId: 'touch-gamepad-host', visible: true}}, window.location.origin)""")
            time.sleep(1.5)
            moves = cdp is not None
            tapped, sticks = drive_touch(page, cdp, moves)
            time.sleep(1.5)
            pressed, diag = union(cell.report("gamepad", 10))
            detail = f"{len(pressed)} indices for {tapped} buttons and {sticks} sticks: {sorted(pressed)}"
            if moves:
                R.check(f"every touch gamepad button and stick click reaches the session's {which}",
                        tapped and len(pressed) >= tapped + sticks, detail)
            else:
                R.skip(f"every touch gamepad button into the session's {which}",
                       f"Playwright's WebKit taps last no time at all, shorter than the client's pad poll ({detail})")
            if moves:
                R.check(f"four diagonals of both touch sticks reach the session's {which}",
                        {f"{s}:{a}{b}" for s in (0, 1) for a in "+-" for b in "+-"} <= diag, sorted(diag))
            else:
                R.skip(f"touch stick diagonals into the session's {which}",
                       "WebKit has no Touch constructor and Playwright's touchscreen only taps")
            page.close()
            if tctx:
                tctx.close()
        stop_session_browser(cell.target, which, "pad")
