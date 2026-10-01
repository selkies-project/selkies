#!/usr/bin/env python3
"""Touch input end to end: a browser client on a touch screen drives the server,
and what the session receives is read back from the X server or the Wayland seat.

Trackpad mode has to click as a tap lifts rather than when the double-tap window
closes, keep a quick double tap two clicks, scroll by the distance two fingers
travel whatever rate the digitizer reports at, hand a two-finger gesture down to
the finger left on the glass, middle-click on three fingers, and send a pinch as
Ctrl+wheel; direct touch scrolls and pinches the same way. Chromium is driven
through CDP multi-touch, which enters the engine's own touch pipeline. Firefox
and WebKit take synthetic TouchEvents dispatched on the stream overlay, which
exercise the client's handlers in those engines but not their touch pipelines.

On Wayland the pinch is also a check of the server: keys reach the seat through
the keyboard worker and wheel clicks straight from the message, so the Control a
pinch is wrapped in has to be down before its click arrives.

Trackpad mode draws the cursor on the page, where the server echoes the pointer,
with nothing composited into the video: the drawn cursor has to sit where the
session's pointer is after the finger moves it, and follow a warp no page sent.

    python3 tests/e2e/test_touch.py x11|wl
"""
import os
import sys
import threading
import time
from typing import Any, List, Optional, Tuple

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright

# Synthetic touches, for the engines without a CDP touch channel. Each call
# dispatches one TouchEvent on the stream overlay. WebKit refuses to construct
# a Touch and makes one through its legacy document.createTouch instead, and
# takes its TouchEvent's lists as TouchLists rather than arrays.
SYNTHETIC_TOUCH_JS = """(a) => {
  const el = document.getElementById('overlayInput');
  const legacy = (() => { try { new Touch({identifier: 0, target: el}); return false; } catch (e) { return true; } })();
  const mk = (p) => legacy
    ? document.createTouch(window, el, p[0], p[1], p[2], p[1], p[2])
    : new Touch({identifier: p[0], target: el, clientX: p[1], clientY: p[2], pageX: p[1], pageY: p[2],
                 screenX: p[1], screenY: p[2], radiusX: 1, radiusY: 1, force: 1});
  const list = (ps) => legacy ? document.createTouchList(...ps.map(mk)) : ps.map(mk);
  el.dispatchEvent(new TouchEvent(a.type, {touches: list(a.touches), targetTouches: list(a.touches),
      changedTouches: list(a.changed), bubbles: true, cancelable: true}));
}"""

# Where the page draws trackpad mode's cursor, in the stream's pixels, and
# whether it had the cursor composited instead; null while none is drawn.
PAGE_CURSOR_JS = r"""() => {
  const input = window.webrtcInput;
  if (!input || input.cursorDiv.style.display !== 'block') return null;
  const m = /translate\(([-\d.]+)px, ([-\d.]+)px\)/.exec(input.cursorDiv.style.transform || '');
  const box = input._streamBox();
  if (!m || !box) return null;
  const x = Number(m[1]) + input.cursorHotspot.x, y = Number(m[2]) + input.cursorHotspot.y;
  return [(x - box.left) * box.scaleX, (y - box.top) * box.scaleY, !!input._echoComposited];
}"""

# Firefox exposes touch events on a desktop without a touch screen only when
# asked, `ontouchstart` included, which the client looks for.
FIREFOX_TOUCH_PREFS = {"dom.w3c_touch_events.enabled": 1,
                       "dom.w3c_touch_events.legacy_apis.enabled": True}


class Fingers:
    """Fingers on the page: CDP multi-touch on Chromium, synthetic TouchEvents elsewhere.

    `down` and `up` land or lift fingers, `move` moves them; each takes
    `(id, x, y)` points in viewport coordinates. CDP presses and moves the
    points a touchStart or touchMove lists, and releases the ones a touchEnd
    lists: a touchMove that leaves a point out does not lift it.
    """

    def __init__(self, page: Any, engine: str) -> None:
        self.page = page
        self.cdp = page.context.new_cdp_session(page) if engine == "chromium" else None
        self.live: dict = {}

    def _cdp(self, kind: str, points: Optional[list] = None) -> None:
        if points is None:
            points = [(i, x, y) for i, (x, y) in self.live.items()]
        self.cdp.send("Input.dispatchTouchEvent", {
            "type": kind, "touchPoints": [{"x": x, "y": y, "id": i} for i, x, y in points]})

    def _synthetic(self, kind: str, changed: List[Tuple[int, float, float]]) -> None:
        touches = [[i, x, y] for i, (x, y) in self.live.items()]
        self.page.evaluate(SYNTHETIC_TOUCH_JS, {"type": kind, "touches": touches,
                                                "changed": [list(p) for p in changed]})

    def down(self, *points: Tuple[int, float, float]) -> None:
        for i, x, y in points:
            self.live[i] = (x, y)
            if self.cdp:
                self._cdp("touchStart")
        if not self.cdp:
            self._synthetic("touchstart", list(points))

    def move(self, *points: Tuple[int, float, float]) -> None:
        for i, x, y in points:
            self.live[i] = (x, y)
        if self.cdp:
            self._cdp("touchMove")
        else:
            self._synthetic("touchmove", list(points))

    def up(self, *ids: int) -> None:
        changed = [(i, *self.live[i]) for i in ids]
        for i in ids:
            del self.live[i]
        if self.cdp:
            self._cdp("touchEnd", changed)
        else:
            self._synthetic("touchend", changed)


class XWatcher:
    """Every pointer and key event the session gets, stamped as it arrives.

    A window covering the screen, mapped without a window manager, receives the
    buttons and motion under the pointer, and holding the input focus, the keys.
    """

    def __init__(self) -> None:
        from selkies.Xlib import display as xdisp, X
        self.X = X
        self.d = xdisp.Display(H.require_display())
        scr = self.d.screen()
        self.win = scr.root.create_window(
            0, 0, 8192, 4096, 0, scr.root_depth, window_class=X.InputOutput,
            override_redirect=True,
            event_mask=(X.KeyPressMask | X.KeyReleaseMask | X.ButtonPressMask |
                        X.ButtonReleaseMask | X.PointerMotionMask))
        self.win.map()
        self.d.set_input_focus(self.win, X.RevertToPointerRoot, X.CurrentTime)
        self.d.sync()
        self.control = self.d.keysym_to_keycode(0xFFE3)
        self.events: list = []
        self._stop = False
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()

    def _run(self) -> None:
        while not self._stop:
            if self.d.pending_events():
                e = self.d.next_event()
                n = type(e).__name__
                if n in ("KeyPress", "KeyRelease", "ButtonPress", "ButtonRelease"):
                    self.events.append((time.monotonic(), n, e.detail, e.root_x, e.root_y))
                elif n == "MotionNotify":
                    self.events.append((time.monotonic(), n, 0, e.root_x, e.root_y))
            else:
                time.sleep(0.001)

    def since(self, t: float, name: Optional[str] = None, detail: Optional[int] = None) -> list:
        return [e for e in self.events if e[0] >= t and (name is None or e[1] == name)
                and (detail is None or e[2] == detail)]

    def close(self) -> None:
        self._stop = True
        self._thread.join(timeout=2)
        try:
            self.win.destroy()
            self.d.close()
        except Exception:
            pass


def near(drawn: Optional[list], at: Tuple[float, float], slack: float = 2) -> bool:
    return drawn is not None and abs(drawn[0] - at[0]) <= slack and abs(drawn[1] - at[1]) <= slack


def trackpad_stroke(f: Fingers, x: float = 300, y: float = 300) -> None:
    """One finger drawn slowly down and to the right across the trackpad."""
    f.down((1, x, y))
    for i in range(1, 11):
        time.sleep(0.016)
        f.move((1, x + 8 * i, y + 4 * i))
    f.up(1)


def set_mode(page: Any, trackpad: bool) -> None:
    kind = "touchinput:trackpad" if trackpad else "touchinput:touch"
    page.evaluate(f"window.postMessage({{type: '{kind}'}}, window.location.origin)")
    time.sleep(0.4)


def two_finger_swipe(f: Fingers, dy: float, steps: int, x: float = 600, y: float = 500,
                     gap: float = 0.008) -> None:
    f.down((1, x, y))
    f.down((2, x + 100, y))
    for i in range(1, steps + 1):
        time.sleep(gap)
        f.move((1, x, y + dy * i / steps), (2, x + 100, y + dy * i / steps))
    time.sleep(gap)
    f.up(1, 2)


def pinch(f: Fingers, start: float, end: float, steps: int = 20) -> None:
    cx, cy = 640, 400
    f.down((1, cx - start / 2, cy))
    f.down((2, cx + start / 2, cy))
    for i in range(1, steps + 1):
        time.sleep(0.016)
        d = start + (end - start) * i / steps
        f.move((1, cx - d / 2, cy), (2, cx + d / 2, cy))
    time.sleep(0.016)
    f.up(1, 2)


def x11_block(res: "H.Results", engine: str, mode: str) -> None:
    tag = f"{engine} {mode}"
    H.server_start(mode=mode)
    watcher = None
    with sync_playwright() as pw:
        viewport = {"width": 1280, "height": 720}
        if engine == "firefox":
            ctx = C.firefox_persistent_context(pw, viewport=viewport, has_touch=True,
                                               prefs=FIREFOX_TOUCH_PREFS)
            browser = ctx
        else:
            browser = C.launch_browser(pw, engine)
            ctx = browser.new_context(viewport=viewport, device_scale_factor=1, has_touch=True)
        try:
            ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
            page = ctx.new_page()
            page.goto(H.BASE_URL + "/", wait_until="load")
            video = C.wait_ws_video(page, 40) if mode == "websockets" else C.wait_wr_video(page, 60)
            res.check(f"{tag}: video up", video is not None, video)
            if not video:
                return
            has_touch = page.evaluate("'ontouchstart' in window && typeof TouchEvent === 'function'")
            if not has_touch:
                res.check(f"{tag}: the engine has touch events", False, "no ontouchstart/TouchEvent")
                return
            time.sleep(1.0)
            watcher = XWatcher()
            # Trackpad motion is relative: from wherever a resize left the pointer
            # (a corner, after the screen shrank to the page), a move toward that
            # edge goes nowhere.
            warp = H.x_display()
            warp.screen().root.warp_pointer(640, 360)
            warp.sync()
            warp.close()
            f = Fingers(page, engine)

            # --- trackpad mode ---------------------------------------------
            set_mode(page, True)
            f.down((1, 500, 400))
            time.sleep(0.08)
            t_lift = time.monotonic()
            f.up(1)
            time.sleep(0.6)
            press = watcher.since(t_lift, "ButtonPress", 1)
            release = watcher.since(t_lift, "ButtonRelease", 1)
            lag = (press[0][0] - t_lift) * 1000 if press else None
            res.check(f"{tag}: a trackpad tap clicks as the finger lifts",
                      len(press) == 1 and lag is not None and lag < 100, f"press {lag and round(lag, 1)} ms")
            res.check(f"{tag}: and releases the click",
                      len(release) == 1 and release[0][0] > press[0][0] if press else False,
                      f"{len(release)} releases")

            t0 = time.monotonic()
            for _ in range(2):
                f.down((1, 500, 400))
                time.sleep(0.06)
                f.up(1)
                time.sleep(0.09)
            time.sleep(0.6)
            clicks = watcher.since(t0, "ButtonPress", 1)
            res.check(f"{tag}: a quick double tap is two clicks", len(clicks) == 2, f"{len(clicks)} presses")

            counts = []
            for steps in (10, 50):
                t0 = time.monotonic()
                two_finger_swipe(f, -200, steps)
                time.sleep(0.6)
                counts.append(len(watcher.since(t0, "ButtonPress", 5)))
            res.check(f"{tag}: a 200 px two-finger scroll scrolls as far in 10 events as in 50",
                      min(counts) >= 3 and max(counts) - min(counts) <= 1, f"notches {counts}")

            f.down((1, 400, 500))
            f.down((2, 500, 500))
            for i in range(1, 11):
                time.sleep(0.016)
                f.move((1, 400, 500 - 10 * i), (2, 500, 500 - 10 * i))
            f.up(2)
            time.sleep(0.3)
            t0 = time.monotonic()
            for i in range(1, 11):
                time.sleep(0.016)
                f.move((1, 400 + 10 * i, 400))
            time.sleep(0.4)
            motion = watcher.since(t0, "MotionNotify")
            moved = motion[-1][3] - motion[0][3] if len(motion) >= 2 else 0
            f.up(1)
            res.check(f"{tag}: the finger left after a two-finger scroll moves the pointer",
                      len(motion) >= 5 and moved >= 60, f"{len(motion)} motions, {moved} px")

            t0 = time.monotonic()
            f.down((1, 500, 400))
            f.down((2, 560, 400))
            f.down((3, 620, 400))
            time.sleep(0.06)
            f.up(1, 2, 3)
            time.sleep(0.5)
            middle = watcher.since(t0, "ButtonPress", 2)
            res.check(f"{tag}: a three-finger tap is a middle click", len(middle) == 1, f"{len(middle)} presses")

            t0 = time.monotonic()
            pinch(f, 100, 200)
            time.sleep(0.6)
            ev = [e for e in watcher.since(t0) if e[1] != "MotionNotify"]
            zoom = [e for e in ev if e[1] == "ButtonPress" and e[2] == 4]
            inside = all(any(k[1] == "KeyPress" and k[2] == watcher.control and k[0] <= z[0] for k in ev)
                         for z in zoom)
            res.check(f"{tag}: a trackpad pinch out is Ctrl+wheel up",
                      4 <= len(zoom) <= 6 and inside, f"{len(zoom)} notches, control held {inside}")

            trackpad_stroke(f)
            time.sleep(0.4)
            drawn = page.evaluate(PAGE_CURSOR_JS)
            xd = H.x_display()
            p = xd.screen().root.query_pointer()
            res.check(f"{tag}: trackpad mode draws the cursor on the page, where the pointer is",
                      near(drawn, (p.root_x, p.root_y)), f"page {drawn}, server {(p.root_x, p.root_y)}")
            res.check(f"{tag}: with nothing composited into the video", drawn is not None and not drawn[2], drawn)
            xd.screen().root.warp_pointer(320, 200)
            xd.sync()
            xd.close()
            t0 = time.monotonic()
            warped = None
            while time.monotonic() - t0 < 1.5 and not near(warped, (320, 200), 1):
                time.sleep(0.01)
                warped = page.evaluate(PAGE_CURSOR_JS)
            res.check(f"{tag}: a warp no page sent moves it too", near(warped, (320, 200), 1),
                      f"page {warped} after {round((time.monotonic() - t0) * 1000)} ms")

            # --- direct touch ----------------------------------------------
            set_mode(page, False)
            counts = []
            for steps in (10, 50):
                t0 = time.monotonic()
                two_finger_swipe(f, -200, steps)
                time.sleep(0.6)
                counts.append(len(watcher.since(t0, "ButtonPress", 5)))
            res.check(f"{tag}: direct touch scrolls a 200 px swipe as far in 10 events as in 50",
                      min(counts) >= 3 and max(counts) - min(counts) <= 1, f"notches {counts}")
            t0 = time.monotonic()
            pinch(f, 300, 150)
            time.sleep(0.6)
            ev = [e for e in watcher.since(t0) if e[1] != "MotionNotify"]
            zoom = [e for e in ev if e[1] == "ButtonPress" and e[2] == 5]
            controls = [e for e in ev if e[1] == "KeyPress" and e[2] == watcher.control]
            res.check(f"{tag}: a direct-touch pinch in is Ctrl+wheel down",
                      4 <= len(zoom) <= 6 and controls and controls[0][0] <= zoom[0][0],
                      f"{len(zoom)} notches, {len(controls)} Control presses")
        finally:
            if watcher:
                watcher.close()
            C.close_browser(browser)
            H.server_stop()


def wayland_block(res: "H.Results", mode: str) -> None:
    """The pinch's Control is down on the Wayland seat before its wheel click."""
    tag = f"wayland {mode}"
    H.server_start(mode=mode, wayland=True)
    obs = None
    with sync_playwright() as pw:
        browser = C.launch_browser(pw, "chromium")
        try:
            ctx = browser.new_context(viewport={"width": 1280, "height": 720}, device_scale_factor=1,
                                      has_touch=True)
            ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
            page = ctx.new_page()
            page.goto(H.BASE_URL + "/", wait_until="load")
            video = C.wait_ws_video(page, 40) if mode == "websockets" else C.wait_wr_video(page, 60)
            res.check(f"{tag}: video up", video is not None, video)
            if not video:
                return
            obs = H.WlObs("wayland-1")
            res.check(f"{tag}: observer mapped", obs.ready())
            f = Fingers(page, "chromium")
            set_mode(page, True)
            # A move and a tap, to give the observer the pointer and keyboard focus.
            f.down((1, 600, 360))
            for i in range(1, 6):
                time.sleep(0.016)
                f.move((1, 600 + 10 * i, 360))
            f.up(1)
            time.sleep(0.4)
            f.down((1, 640, 360))
            time.sleep(0.06)
            f.up(1)
            time.sleep(1.0)
            mark = len(obs.lines)
            pinch(f, 100, 200)
            time.sleep(1.0)
            ev = [l for l in obs.lines[mark:] if l.get("kind") in ("kbd_key", "ptr_axis")]
            first_axis = next((i for i, l in enumerate(ev) if l["kind"] == "ptr_axis"), None)
            ctrl_down = next((i for i, l in enumerate(ev) if l["kind"] == "kbd_key" and l.get("state") == 1), None)
            res.check(f"{tag}: a pinch reaches the seat as wheel clicks", first_axis is not None, ev[:6])
            res.check(f"{tag}: with Control down before the first of them",
                      ctrl_down is not None and first_axis is not None and ctrl_down < first_axis, ev[:6])
            f.down((1, 500, 400))
            time.sleep(0.06)
            mark = len(obs.lines)
            t_lift = time.monotonic()
            f.up(1)
            lag = None
            while time.monotonic() - t_lift < 3:
                if any(l.get("kind") == "ptr_button" and l.get("state") == 1 for l in obs.lines[mark:]):
                    lag = (time.monotonic() - t_lift) * 1000
                    break
                time.sleep(0.002)
            res.check(f"{tag}: a trackpad tap clicks on the seat as the finger lifts",
                      lag is not None and lag < 100, f"press seen after {lag and round(lag)} ms")
            trackpad_stroke(f)
            time.sleep(0.4)
            drawn = page.evaluate(PAGE_CURSOR_JS)
            seat = [l for l in obs.lines if l.get("kind") == "ptr_motion"]
            at = (seat[-1]["x"], seat[-1]["y"]) if seat else None
            res.check(f"{tag}: trackpad mode draws the cursor on the page, where the seat's pointer is",
                      at is not None and near(drawn, at), f"page {drawn}, seat {at}")
            res.check(f"{tag}: with nothing composited into the video", drawn is not None and not drawn[2], drawn)
        finally:
            if obs is not None:
                obs.stop()
            C.close_browser(browser)
            H.server_stop()


def main() -> None:
    which = sys.argv[1] if len(sys.argv) > 1 else "x11"
    res = H.Results(f"touch-{which}")
    if which == "x11":
        for mode in ("websockets", "webrtc"):
            for engine in ("chromium", "firefox", "webkit"):
                x11_block(res, engine, mode)
    elif which == "wl":
        for mode in ("websockets", "webrtc"):
            wayland_block(res, mode)
    else:
        raise SystemExit(f"unknown block {which}")
    sys.exit(0 if res.summary() else 1)


if __name__ == "__main__":
    main()
