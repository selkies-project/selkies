"""3. Make sure the second display works properly by moving a window to the right. Then, toggle force aligned resolution, native cursor style, and antialiasing. Try changing resolutions (both preset and manual, UI scaling, and HiDPI (it's supposed to be disabled automatically when a resolution is set), and the reset button as well. See if the desktops or windows respond.

A second page opens `#display2`; a window of the session's Chrome is moved to
the right of the primary with xdotool and has to show up in the second
display's stream. The classic dashboard's screen controls are then driven one
by one, each checked on the session (its xrandr geometry, its Xft.dpi, and the
tester page's own size, which is what a maximized window does) and on the page
(its stream, its canvas style).
"""
import re
import time
from typing import Any, Optional

from image_lib import open_sidebar, reveal, session_browser, stop_session_browser

ITEM = 3
TITLE = "second display, screen settings"
SOLID = (255, 0, 255)


def geometry(cell: Any) -> dict:
    """The session's monitors as xrandr reports them: name -> (w, h, x, y)."""
    out = cell.target.out("xrandr --listactivemonitors 2>/dev/null")
    mons = {}
    for m in re.finditer(r"\d+: \+?\*?(\S+) (\d+)/\d+x(\d+)/\d+\+(\d+)\+(\d+)", out):
        mons[m.group(1)] = tuple(int(m.group(i)) for i in range(2, 6))
    return mons


def screen(cell: Any) -> Optional[tuple]:
    m = re.search(r"current (\d+) x (\d+)", cell.target.out("xrandr --current 2>/dev/null | head -1"))
    return (int(m.group(1)), int(m.group(2))) if m else None


def xft(cell: Any) -> str:
    return cell.target.out("xrdb -query 2>/dev/null | awk '/Xft.dpi/ {print $2}'") or "96"


def wait(fn: Any, want: Any, timeout: float = 15) -> Any:
    deadline, got = time.time() + timeout, fn()
    while time.time() < deadline and not want(got):
        time.sleep(0.7)
        got = fn()
    return got


def style(page: Any, prop: str) -> str:
    """A computed style of the stream's sink, or for the cursor of the input overlay over it."""
    return page.evaluate("""(prop) => {
      if (prop === 'cursor') {
        const el = document.getElementById('overlayInput');
        return el ? getComputedStyle(el).cursor.slice(0, 40) : '';
      }
      const v = document.querySelector('video'), c = [...document.querySelectorAll('canvas')].find(c => c.width >= 320);
      const el = (v && v.videoWidth > 0 && v.style.display !== 'none') ? v : c;
      return el ? getComputedStyle(el)[prop] : '';
    }""", prop)


def click_toggle(page: Any, sel: str) -> Optional[bool]:
    el = page.locator(sel)
    if not reveal(page, sel):
        return None
    el.first.click(timeout=5000)
    time.sleep(1.5)
    return (el.first.get_attribute("aria-pressed") or el.first.get_attribute("aria-checked")) == "true"


def run(cell: Any) -> None:
    R = cell.res
    page = cell.open()
    if cell.backend == "wayland" and "xrandr" not in cell.facts.get("tools", []):
        R.skip("session geometry", "no xrandr in the Wayland session")
    # The second display, and a window moved onto it.
    second = cell.open("#display2")
    mons = wait(lambda: geometry(cell), lambda m: len(m) >= 2, 20)
    R.check("#display2 adds a second monitor to the session", len(mons) >= 2, mons)
    primary = max(mons.values(), key=lambda g: -g[2]) if mons else (1280, 720, 0, 0)
    if len(mons) >= 2 and "xdotool" in cell.facts.get("tools", []):
        session_browser(cell.target, "chrome", "/solid.html", kiosk=False, profile="solid")
        wid = wait(lambda: cell.target.out("xdotool search --name '^it-solid' 2>/dev/null | head -1"), bool, 20)
        if wid:
            x = primary[0] + 100
            cell.target.sh(f"xdotool windowsize {wid} 400 300; xdotool windowmove {wid} {x} 100")
            time.sleep(3)
            on2 = wait(lambda: cell.sample(second, [(0.15, 0.3)]), lambda s: bool(s) and _near(s["points"][0], SOLID), 15)
            R.check("a window moved to the right of the primary shows on the second display",
                    bool(on2) and _near(on2["points"][0], SOLID), {"window": wid, "x": x, "sample": on2 and on2["points"]})
        else:
            R.skip("a window moved onto the second display",
                   f"no X window to move on {cell.backend}: the session's Chrome is a native Wayland client there")
        stop_session_browser(cell.target, "chrome", "solid")
    else:
        R.skip("a window moved onto the second display", f"needs a second monitor and xdotool ({mons})")
    second.close()

    open_sidebar(page)
    base = screen(cell)
    # Force aligned resolution: the desktop snaps to a multiple of 16.
    page.set_viewport_size({"width": 1277, "height": 715})
    time.sleep(3)
    odd = screen(cell)
    on = click_toggle(page, "#forceAlignedResolutionToggle")
    page.set_viewport_size({"width": 1275, "height": 713})
    aligned = wait(lambda: screen(cell), lambda g: bool(g) and g[0] % 16 == 0 and g[1] % 16 == 0, 15)
    R.check("force aligned resolution snaps the desktop to multiples of 16", on and aligned and
            aligned[0] % 16 == 0 and aligned[1] % 16 == 0, f"{odd} -> {aligned}")
    click_toggle(page, "#forceAlignedResolutionToggle")
    page.set_viewport_size({"width": 1280, "height": 720})
    time.sleep(2)

    # Native cursor style and antialiasing: the stream element's own CSS follows each.
    before = style(page, "cursor")
    on = click_toggle(page, "#useBrowserCursorsToggle")
    page.mouse.move(640, 360)
    time.sleep(1.5)
    after = style(page, "cursor")
    R.check("native cursor style changes the cursor the page shows", on is not None and before != after,
            f"{before!r} -> {after!r}")
    click_toggle(page, "#useBrowserCursorsToggle")

    # A preset resolution turns HiDPI off and sizes the desktop; the window follows.
    # Left as the dashboard derives it: a user's own pick of HiDPI outranks the derivation.
    hidpi = _on(page, "#hidpiToggle")
    presets = page.eval_on_selector_all("#resolutionPresetSelect option", "els => els.map(e => e.value)") \
        if reveal(page, "#resolutionPresetSelect") else []
    pick = next((p for p in presets if p.startswith("1600x900")), next((p for p in presets if "x" in p), None))
    if pick:
        page.select_option("#resolutionPresetSelect", pick, timeout=5000)
        want = tuple(int(v) for v in pick.split("x")[:2])
        got = wait(lambda: screen(cell), lambda g: g == want, 15)
        rep = cell.report("pattern", 10, where=lambda r: (r.get("w"), r.get("h")) == want)
        R.check(f"the preset {pick} sizes the desktop and the maximized window follows", got == want and rep,
                {"screen": got, "window": rep and (rep["w"], rep["h"])})
        R.check("HiDPI turns itself off once a resolution is set", hidpi is None or not _on(page, "#hidpiToggle"),
                f"hidpi before {hidpi}, after {_on(page, '#hidpiToggle')}")
    else:
        R.skip("preset resolution", f"no presets offered ({presets[:5]})")

    # Manual resolution.
    if reveal(page, "#manualWidthInput"):
        # Typed as a user types (a scripted fill is not an edit React sees in every engine).
        for sel, value in (("#manualWidthInput", "1440"), ("#manualHeightInput", "810")):
            page.click(sel, timeout=5000)
            page.keyboard.press("Control+a")
            page.keyboard.type(value)
        page.locator('button:has-text("Set Manual Resolution")').first.click(timeout=5000)
        got = wait(lambda: screen(cell), lambda g: g == (1440, 810), 15)
        rep = cell.report("pattern", 10, where=lambda r: (r.get("w"), r.get("h")) == (1440, 810))
        R.check("a manual 1440x810 sizes the desktop and the window follows", got == (1440, 810) and rep,
                {"screen": got, "window": rep and (rep["w"], rep["h"])})
        # Scaled to the page, the picture is smoothed only while antialiasing is on.
        before = style(page, "imageRendering")
        on = click_toggle(page, "#antiAliasingToggle")
        after = style(page, "imageRendering")
        R.check("antialiasing changes how the scaled stream is drawn", on is not None and before != after,
                f"{before!r} -> {after!r}")
        click_toggle(page, "#antiAliasingToggle")
    else:
        R.skip("manual resolution", "no manual width field")

    # UI scaling sets the desktop's DPI.
    if reveal(page, "#uiScalingSelect"):
        values = page.eval_on_selector_all("#uiScalingSelect option", "els => els.map(e => e.value)")
        pick = "144" if "144" in values else (values[-1] if values else None)
        if pick:
            page.select_option("#uiScalingSelect", pick, timeout=5000)
            # X11 desktops read Xft.dpi; a Wayland one scales its output, which its apps see as their pixel ratio.
            want_dpr = int(pick) / 96
            got = wait(lambda: (xft(cell), (cell.report("pattern", 1) or {}).get("dpr")),
                       lambda v: v[0] == pick or abs((v[1] or 0) - want_dpr) < 0.01, 15)
            R.check(f"UI scaling {pick} dpi reaches the desktop", got[0] == pick or abs((got[1] or 0) - want_dpr) < 0.01,
                    f"Xft.dpi {got[0]}, the window's pixel ratio {got[1]}")
            page.select_option("#uiScalingSelect", "96" if "96" in values else values[0], timeout=5000)
    else:
        R.skip("UI scaling", "no scaling menu")

    # Reset: back to the window's own size.
    if reveal(page, "button.reset-button"):
        page.locator("button.reset-button").first.click(timeout=5000)
        got = wait(lambda: screen(cell), lambda g: g == (1280, 720), 15)
        R.check("reset gives the desktop the window's size again", got == (1280, 720), f"{got} (was {base})")
    else:
        R.skip("reset", "no reset button")
    cell.go("/pattern.html")


def _near(rgb: Any, want: tuple) -> bool:
    return isinstance(rgb, list) and all(abs(a - b) <= 30 for a, b in zip(rgb, want))


def _on(page: Any, sel: str) -> Optional[bool]:
    el = page.locator(sel)
    if not el.count():
        return None
    return (el.first.get_attribute("aria-pressed") or el.first.get_attribute("aria-checked")) == "true"
