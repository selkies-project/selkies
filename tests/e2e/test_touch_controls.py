#!/usr/bin/env python3
"""Touch clients' own controls in both dashboards: the special-key palette and
the trackpad's speed, end to end.

The palette, beside the soft modifier keys a touch client gets, holds the keys
an on-screen keyboard lacks, a few chords, and chords the user adds and keeps.
Each key or chord has to reach the session as those keys, pressed in order and
released in reverse, a modifier held on a soft key has to stay held across a
chord, and a chord the user added has to be there after a reload. On phone
viewports, portrait and landscape, in direct touch and in trackpad mode, the
palette's toggle has to stay in the viewport and take a click with the palette
open or closed, and its keys and field have to take one too. The keys are read
back from the X server, and on Wayland from the seat.

Trackpad travel is accelerated by the finger's speed: a slow drag moves the
pointer as far as the finger went, a fast one further, and the speed picked in
the dashboard scales it and is kept. Every block runs in Chromium, Firefox,
and WebKit on both transports: Chromium driven through CDP touch, Firefox and
WebKit through synthetic touches.

    python3 tests/e2e/test_touch_controls.py x11|wl [keyboard]
"""
import os
import sys
import time
from typing import Any

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright
from test_touch import FIREFOX_TOUCH_PREFS, Fingers, XWatcher  # noqa: E402

XK = {"F5": 0xFFC2, "Tab": 0xFF09, "Alt_L": 0xFFE9, "Control_L": 0xFFE3, "Shift_L": 0xFFE1,
      "t": 0x74, "Delete": 0xFFFF}


def open_client(pw: Any, engine: str, mode: str) -> tuple:
    viewport = {"width": 1280, "height": 720}
    if engine == "firefox":
        ctx = C.firefox_persistent_context(pw, viewport=viewport, has_touch=True, prefs=FIREFOX_TOUCH_PREFS)
        browser = ctx
    else:
        browser = C.launch_browser(pw, engine)
        ctx = browser.new_context(viewport=viewport, device_scale_factor=1, has_touch=True)
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    if mode == "webrtc":
        # So a <video> that never plays can be told from a stream that never came.
        ctx.add_init_script(C.PC_TAP_JS)
    page = ctx.new_page()
    page.goto(H.BASE_URL + "/", wait_until="load")
    if engine == "firefox":
        # Firefox runs on one persistent profile, so the touch mode, the
        # trackpad speed, and the chords an earlier block left would carry into
        # this one: each block starts from none of them.
        page.evaluate("""() => {
          for (const k of Object.keys(localStorage)) {
            if (/_(trackpadMode|trackpad_speed|user_chords)$/.test(k)) localStorage.removeItem(k);
          }
        }""")
        page.goto(H.BASE_URL + "/", wait_until="load")
    return browser, page


def touch_once(page: Any, engine: str) -> None:
    """One finger down and up on the stream, which is how a dashboard learns the
    screen takes touch and offers its touch controls."""
    f = Fingers(page, engine)
    f.down((1, 900, 600))
    time.sleep(0.05)
    f.up(1)
    time.sleep(0.8)


def open_palette(page: Any, dashboard: str) -> bool:
    if dashboard == "classic" and not page.evaluate("!!document.querySelector('.sidebar.is-open')"):
        page.evaluate("window.postMessage({type: 'toggleDashboard'}, window.location.origin)")
        time.sleep(0.8)
    toggle = page.locator(".key-palette-toggle").first
    if not toggle.count():
        return False
    if toggle.get_attribute("aria-expanded") != "true":
        toggle.click()
        time.sleep(0.4)
    return page.locator("[data-code='F5']").count() > 0


def keys_since(watcher: XWatcher, t: float, codes: dict) -> list:
    """The key events since `t`, as (press, keysym name) for the keycodes of interest."""
    by_code = {code: name for name, code in codes.items()}
    out = []
    for _, kind, detail, _, _ in watcher.since(t):
        if kind in ("KeyPress", "KeyRelease") and detail in by_code:
            out.append((kind == "KeyPress", by_code[detail]))
    return out


def palette_block(res: "H.Results", dashboard: str, dist: str, engine: str, mode: str) -> None:
    tag = f"{dashboard} {engine} {mode}"
    H.server_start(mode=mode, web_root=dist)
    watcher = None
    try:
        with sync_playwright() as pw:
            browser, page = open_client(pw, engine, mode)
            try:
                video = C.wait_ws_video(page, 40) if mode == "websockets" else C.wait_wr_video(page, 60)
                res.check(f"{tag}: video up", video is not None, video)
                if not video:
                    return
                touch_once(page, engine)
                opened = open_palette(page, dashboard)
                res.check(f"{tag}: a touch client is offered the key palette", opened)
                if not opened:
                    return
                watcher = XWatcher()
                codes = {name: watcher.d.keysym_to_keycode(sym) for name, sym in XK.items()}
                time.sleep(0.3)

                t0 = time.monotonic()
                page.locator("[data-code='F5']").first.click()
                time.sleep(0.6)
                got = keys_since(watcher, t0, codes)
                res.check(f"{tag}: the palette's F5 reaches the session", got == [(True, "F5"), (False, "F5")], got)

                t0 = time.monotonic()
                page.locator("[data-chord='Alt+Tab']").first.click()
                time.sleep(0.6)
                got = keys_since(watcher, t0, codes)
                res.check(f"{tag}: Alt+Tab reaches it pressed in order and released in reverse",
                          got == [(True, "Alt_L"), (True, "Tab"), (False, "Tab"), (False, "Alt_L")], got)

                field = page.locator(".key-palette-input").first
                t_typed = time.monotonic()
                field.click()
                field.press_sequentially("ctrl+shift+t", delay=20)
                field.press("Enter")
                time.sleep(0.5)
                leaked = [e for e in watcher.since(t_typed) if e[1] in ("KeyPress", "KeyRelease")]
                res.check(f"{tag}: typing a chord into the palette sends nothing to the session", not leaked,
                          leaked[:4])
                added = page.locator("[data-chord='Ctrl+Shift+T']")
                res.check(f"{tag}: a chord the user writes is added, written back one way", added.count() == 1,
                          added.count())
                t0 = time.monotonic()
                if added.count():
                    added.first.click()
                time.sleep(0.6)
                got = keys_since(watcher, t0, codes)
                res.check(f"{tag}: and plays as its keys", got == [
                    (True, "Control_L"), (True, "Shift_L"), (True, "t"), (False, "t"), (False, "Shift_L"),
                    (False, "Control_L")], got)

                # CTL held on its soft key, then the palette's Del: Ctrl+Del, and CTL still held.
                ctl = page.locator("text=/^CTR?L$/").first
                t0 = time.monotonic()
                ctl.click()
                time.sleep(0.3)
                page.locator("[data-code='Delete']").first.click()
                time.sleep(0.6)
                mid = keys_since(watcher, t0, codes)
                ctl.click()
                time.sleep(0.4)
                got = keys_since(watcher, t0, codes)
                res.check(f"{tag}: a palette key under a held soft modifier goes out with it",
                          mid == [(True, "Control_L"), (True, "Delete"), (False, "Delete")]
                          and got[-1] == (False, "Control_L"), got)

                page.reload(wait_until="load")
                (C.wait_ws_video(page, 40) if mode == "websockets" else C.wait_wr_video(page, 60))
                touch_once(page, engine)
                open_palette(page, dashboard)
                res.check(f"{tag}: the user's chord is still there after a reload",
                          page.locator("[data-chord='Ctrl+Shift+T']").count() == 1)
                for width, height in PHONES:
                    page.set_viewport_size({"width": width, "height": height})
                    time.sleep(1.5)
                    for trackpad in (False, True):
                        how = "trackpad mode" if trackpad else "direct touch"
                        ok, detail = in_reach(page, dashboard, trackpad)
                        res.check(f"{tag}: at {width}x{height} in {how}, the palette's toggle and keys are in reach",
                                  ok, detail)
            finally:
                C.close_browser(browser)
    finally:
        if watcher is not None:
            watcher.close()
        H.server_stop()


# Phone viewports, portrait and landscape: the palette has to stay in reach on them.
PHONES = ((390, 844), (844, 390))

RECT_JS = """(sel) => {
  const el = document.querySelector(sel);
  if (!el) return null;
  const r = el.getBoundingClientRect();
  const hit = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
  return {x: Math.round(r.left), y: Math.round(r.top), w: Math.round(r.width), h: Math.round(r.height),
          inside: r.left >= 0 && r.top >= 0 && r.right <= innerWidth && r.bottom <= innerHeight,
          on_top: !!hit && (hit === el || el.contains(hit))};
}"""


def keyboard_button_block(res: "H.Results", dashboard: str, dist: str, engine: str) -> None:
    """A tablet's attached keyboard keeps the system's on-screen one down, so
    the button that pops it goes once a key only a keyboard has is pressed,
    not on a letter, which an on-screen keyboard types too; each dashboard's
    own keyboard control brings it back (the classic tile, Wish's menu button)."""
    tag = f"{dashboard} {engine}"
    H.server_start(mode="websockets", web_root=dist)
    try:
        with sync_playwright() as pw:
            browser, page = open_client(pw, engine, "websockets")
            try:
                video = C.wait_ws_video(page, 40)
                res.check(f"{tag}: video up", video is not None, video)
                if not video:
                    return
                touch_once(page, engine)
                button = page.locator(".virtual-keyboard-button" if dashboard == "classic"
                                      else "[data-virtual-keyboard-button]")
                shown = button.count() > 0 and button.first.is_visible()
                if dashboard == "wish" and not shown:
                    # The Wish dashboard offers it by `(pointer: coarse)`, which a
                    # touch-emulating desktop engine may not report.
                    print(f"[{tag}] no keyboard button offered on this engine's emulation", flush=True)
                    return
                res.check(f"{tag}: a touch client is offered the keyboard button", shown)
                if not shown:
                    return
                page.keyboard.press("a")
                time.sleep(0.4)
                res.check(f"{tag}: a letter leaves it, as an on-screen keyboard's would",
                          button.count() > 0 and button.first.is_visible())
                page.keyboard.press("Escape")
                time.sleep(0.4)
                res.check(f"{tag}: a key only a keyboard has takes it away",
                          button.count() == 0 or not button.first.is_visible())
                if dashboard == "classic":
                    open_sidebar(page)
                    page.locator(".keyboard-toggle-button").first.click()
                    time.sleep(0.4)
                    close_sidebar(page)
                else:
                    page.locator("button:has(svg.lucide-keyboard):not([data-virtual-keyboard-button])"
                                 ).first.click()
                    time.sleep(0.4)
                res.check(f"{tag}: the dashboard's own keyboard control brings it back",
                          button.count() > 0 and button.first.is_visible())
            finally:
                C.close_browser(browser)
    finally:
        H.server_stop()


def set_trackpad(page: Any, dashboard: str, on: bool) -> None:
    """Puts the page in trackpad mode or direct touch with the dashboard's own button."""
    active = page.evaluate("() => !!(window.webrtcInput && window.webrtcInput._trackpadMode)")
    if active != on:
        trackpad_button(page, dashboard)


def in_reach(page: Any, dashboard: str, trackpad: bool) -> tuple:
    """Whether the palette's toggle opens it, its keys and its field take a click,
    and the toggle, in the viewport and uncovered, closes it again."""
    set_trackpad(page, dashboard, trackpad)
    if dashboard == "classic":
        open_sidebar(page)
    toggle = page.locator(".key-palette-toggle").first
    steps = []
    try:
        if toggle.get_attribute("aria-expanded", timeout=5000) == "true":
            toggle.click(timeout=5000)
            time.sleep(0.3)
        toggle.scroll_into_view_if_needed(timeout=5000)
        closed = page.evaluate(RECT_JS, ".key-palette-toggle")
        toggle.click(timeout=5000)
        time.sleep(0.4)
        opened = page.evaluate(RECT_JS, ".key-palette-toggle")
        for sel in ("[data-code='F12']", "[data-chord='Ctrl+Shift+Esc']", ".key-palette-input"):
            page.locator(sel).first.click(timeout=5000)
            steps.append(sel)
        toggle.click(timeout=5000)
        time.sleep(0.3)
        shut = toggle.get_attribute("aria-expanded") == "false"
    except Exception as e:
        return False, f"after {steps}: {str(e).splitlines()[0][:160]}"
    finally:
        if dashboard == "classic":
            close_sidebar(page)
    ok = bool(closed and opened and closed["inside"] and closed["on_top"] and opened["inside"]
              and opened["on_top"] and shut)
    return ok, f"closed {closed} open {opened} shut {shut}"


def pointer_x() -> int:
    d = H.x_display()
    try:
        return d.screen().root.query_pointer().root_x
    finally:
        d.close()


def drag(f: Fingers, px: int, steps: int, pause: float) -> None:
    f.down((1, 300, 360))
    for i in range(1, steps + 1):
        if pause:
            time.sleep(pause)
        f.move((1, 300 + px * i / steps, 360))
    time.sleep(0.05)
    f.up(1)
    time.sleep(0.5)


def travel(f: Fingers, px: int, steps: int, pause: float) -> int:
    warp = H.x_display()
    warp.screen().root.warp_pointer(200, 360)
    warp.sync()
    warp.close()
    time.sleep(0.2)
    before = pointer_x()
    drag(f, px, steps, pause)
    return pointer_x() - before


def open_sidebar(page: Any) -> None:
    if not page.evaluate("!!document.querySelector('.sidebar.is-open')"):
        page.evaluate("window.postMessage({type: 'toggleDashboard'}, window.location.origin)")
        time.sleep(0.8)


def close_sidebar(page: Any) -> None:
    if page.evaluate("!!document.querySelector('.sidebar.is-open')"):
        page.evaluate("window.postMessage({type: 'toggleDashboard'}, window.location.origin)")
        time.sleep(0.8)


def trackpad_button(page: Any, dashboard: str, engine: str = "") -> bool:
    """Turns trackpad mode on with the dashboard's own button, as a user does:
    a dashboard learns of the mode from its button, not from the core. The
    button shows once the dashboard has seen a touch; with `engine`, one more
    touch is made if it has not."""
    if dashboard == "classic":
        open_sidebar(page)
        button = page.locator(".trackpad-mode-button").first
    else:
        button = page.locator("button[title='Trackpad Mode']").first
    for attempt in range(3):
        try:
            button.wait_for(state="attached", timeout=3000)
            break
        except Exception:
            if not engine or attempt == 2:
                return False
            touch_once(page, engine)
            if dashboard == "classic":
                open_sidebar(page)
    button.click()
    time.sleep(0.6)
    if dashboard == "classic":
        close_sidebar(page)
    return True


def speed_block(res: "H.Results", dashboard: str, dist: str, mode: str, engine: str = "chromium") -> None:
    tag = f"{dashboard} {engine} {mode}"
    H.server_start(mode=mode, web_root=dist)
    try:
        with sync_playwright() as pw:
            browser, page = open_client(pw, engine, mode)
            try:
                video = C.wait_ws_video(page, 40) if mode == "websockets" else C.wait_wr_video(page, 60)
                res.check(f"{tag}: video up", video is not None, video)
                if not video:
                    return
                touch_once(page, engine)
                if not trackpad_button(page, dashboard, engine):
                    res.check(f"{tag}: the dashboard offers trackpad mode", False)
                    return
                f = Fingers(page, engine)
                slow = travel(f, 100, 50, 0.02)
                # A flick: a few long steps sent back to back.
                fast = travel(f, 100, 4, 0)
                res.check(f"{tag}: a slow trackpad drag moves the pointer as far as the finger went",
                          90 <= slow <= 110, f"{slow} px for 100")
                res.check(f"{tag}: a fast one moves it further", fast >= 1.8 * slow, f"{fast} px for 100")
                if dashboard == "classic":
                    open_sidebar(page)
                picker = page.locator("#trackpadSpeedSelect, .key-palette ~ label select, label:has-text('Trackpad') select").first
                offered = picker.count() > 0
                res.check(f"{tag}: the dashboard offers the trackpad speed in trackpad mode", offered)
                if not offered:
                    return
                picker.select_option("2")
                time.sleep(0.5)
                if dashboard == "classic":
                    close_sidebar(page)
                doubled = travel(f, 100, 50, 0.02)
                res.check(f"{tag}: twice the speed moves a slow drag twice as far", 180 <= doubled <= 220,
                          f"{doubled} px for 100")
                page.reload(wait_until="load")
                (C.wait_ws_video(page, 40) if mode == "websockets" else C.wait_wr_video(page, 60))
                kept = page.evaluate("window.webrtcInput ? window.webrtcInput.trackpadSpeed : null")
                res.check(f"{tag}: the pick is kept over a reload", kept == 2, kept)
            finally:
                C.close_browser(browser)
    finally:
        H.server_stop()


def wayland_block(res: "H.Results", mode: str, engine: str = "chromium") -> None:
    tag = f"classic wayland {engine} {mode}"
    H.server_start(mode=mode, wayland=True, web_root=H.CLASSIC_DIST)
    obs = None
    try:
        with sync_playwright() as pw:
            browser, page = open_client(pw, engine, mode)
            try:
                video = C.wait_ws_video(page, 40) if mode == "websockets" else C.wait_wr_video(page, 60)
                res.check(f"{tag}: video up", video is not None, video)
                if not video:
                    return
                obs = H.WlObs("wayland-1")
                res.check(f"{tag}: observer mapped", obs.ready())
                # A tap on the observer gives it the keyboard focus.
                f = Fingers(page, engine)
                f.down((1, 640, 360))
                time.sleep(0.06)
                f.up(1)
                time.sleep(0.8)
                if not open_palette(page, "classic"):
                    res.check(f"{tag}: a touch client is offered the key palette", False)
                    return
                mark = len(obs.lines)
                page.locator("[data-chord='Alt+Tab']").first.click()
                time.sleep(0.8)
                keys = [(l.get("state"), l.get("key")) for l in obs.lines[mark:] if l.get("kind") == "kbd_key"]
                # evdev codes: KEY_LEFTALT 56, KEY_TAB 15.
                res.check(f"{tag}: Alt+Tab reaches the seat pressed in order and released in reverse",
                          keys == [(1, 56), (1, 15), (0, 15), (0, 56)], keys)
            finally:
                C.close_browser(browser)
    finally:
        if obs is not None:
            obs.stop()
        H.server_stop()


def main() -> None:
    which = sys.argv[1] if len(sys.argv) > 1 else "x11"
    only = sys.argv[2] if len(sys.argv) > 2 else ""
    res = H.Results(f"touch-controls-{which}")
    if which == "x11":
        for dashboard, dist in (("classic", H.CLASSIC_DIST), ("wish", H.WISH_DIST)):
            for mode in ("websockets", "webrtc") if only != "keyboard" else ():
                for engine in ("chromium", "firefox", "webkit"):
                    palette_block(res, dashboard, dist, engine, mode)
                    speed_block(res, dashboard, dist, mode, engine)
        for dashboard, dist in (("classic", H.CLASSIC_DIST), ("wish", H.WISH_DIST)):
            for engine in ("chromium", "firefox", "webkit"):
                keyboard_button_block(res, dashboard, dist, engine)
    elif which == "wl":
        for mode in ("websockets", "webrtc"):
            for engine in ("chromium", "firefox", "webkit"):
                wayland_block(res, mode, engine)
    else:
        raise SystemExit(f"unknown block {which}")
    sys.exit(0 if res.summary() else 1)


if __name__ == "__main__":
    main()
