#!/usr/bin/env python3
"""Rumble end to end: an application's force feedback, played on the pad of
the browser that drives the slot.

An application under the Input Interposer uploads a rumble effect on the
virtual pad's evdev node and plays it; the server mixes it and relays it to the
one client whose pad drives that slot, and the page plays it on its pads. On
the transport named on argv:

- pads: the dual-rumble effect of a `vibrationActuator` (Chromium, WebKit) or
  the pulse of Gecko's `hapticActuators`, on pads stubbed with actuators that
  record what they are asked to play, so every engine is driven; a `#player2`
  page on the same server drives slot 1 and must hear nothing of slot 0's.
- takeover: a client that takes the slot while an effect without an end plays
  is handed it at once rather than at the next renewal, and hears its stop.
- touch: the on-screen touch gamepad's pad vibrates the device through
  `navigator.vibrate`, stubbed, since a desktop engine has no motor.
- dashboards: each dashboard's Rumble toggle turns playing off in the core,
  stopping what plays, and the core keeps the pick over a reload.

    python3 tests/e2e/test_gamepad_rumble.py [websockets|webrtc]
"""
import json
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                                "integration"))
from test_gamepad_rumble import build_interposer  # noqa: E402

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from test_dashboards import wish_open_menu_item  # noqa: E402

# A standard pad whose motors record what they are asked to play, in the
# shape the engine gives its pads: `vibrationActuator` for Chromium and WebKit,
# Gecko's `hapticActuators` otherwise. A hidden pad is reported only once
# `__connectPad` announces it, so a test chooses when the page takes its slot.
PAD_INIT = """
window.__rumble = [];
const gecko = %s;
const hidden = %s;
const actuator = {
  type: 'dual-rumble', effects: ['dual-rumble'],
  playEffect(type, p) { window.__rumble.push([performance.now(), 'play', type, p.strongMagnitude, p.weakMagnitude, p.duration]); return Promise.resolve('complete'); },
  reset() { window.__rumble.push([performance.now(), 'reset']); return Promise.resolve('complete'); },
};
window.__pad = {
  index: 0, id: "Selkies Test Pad (STANDARD GAMEPAD Vendor: 045e Product: 028e)",
  mapping: "standard", connected: true, timestamp: 1,
  buttons: Array.from({length: 17}, () => ({pressed: false, touched: false, value: 0})),
  axes: [0, 0, 0, 0],
};
if (gecko) window.__pad.hapticActuators = [{ pulse(v, ms) { window.__rumble.push([performance.now(), 'pulse', v, ms]); return Promise.resolve(true); } }];
else window.__pad.vibrationActuator = actuator;
window.__padShown = !hidden;
navigator.getGamepads = () => [window.__padShown ? window.__pad : null, null, null, null];
window.__padPress = (i, v) => {
  window.__pad.buttons[i] = {pressed: v > 0, touched: v > 0, value: v};
  window.__pad.timestamp = performance.now();
};
window.__connectPad = () => {
  window.__padShown = true;
  const e = new Event('gamepadconnected');
  Object.defineProperty(e, 'gamepad', {value: window.__pad});
  window.__padConnectedAt = performance.now();
  window.dispatchEvent(e);
};
"""

# The device's vibrator, for the touch gamepad's pad.
VIBRATE_INIT = """
window.__vibes = [];
Object.defineProperty(navigator, 'vibrate', {configurable: true, value: (p) => {
  window.__vibes.push([performance.now(), p]); return true; }});
"""

# One touch on the touch gamepad's A button, for engines without CDP touch.
SYNTH_TAP = """([x, y]) => {
  const el = document.elementFromPoint(x, y);
  const legacy = (() => { try { new Touch({identifier: 0, target: el}); return false; } catch (e) { return true; } })();
  const t = legacy ? document.createTouch(window, el, 1, x, y, x, y) : new Touch({identifier: 1, target: el, clientX: x, clientY: y});
  const list = (ts) => legacy ? document.createTouchList(...ts) : ts;
  el.dispatchEvent(new TouchEvent('touchstart', {touches: list([t]), targetTouches: list([t]), changedTouches: list([t]), bubbles: true, cancelable: true}));
  el.dispatchEvent(new TouchEvent('touchend', {touches: list([]), targetTouches: list([]), changedTouches: list([t]), bubbles: true, cancelable: true}));
}"""

FIREFOX_TOUCH_PREFS = {"dom.w3c_touch_events.enabled": 1, "dom.w3c_touch_events.legacy_apis.enabled": True}

# The application: one rumble effect, strong at half and weak at a quarter, for
# 400 ms, played once; the time of its write on CLOCK_MONOTONIC.
APP = r'''
import fcntl, json, os, struct, time
def ioc(direction, nr, size):
    return (direction << 30) | (size << 16) | (ord("E") << 8) | nr
FF_SIZE = 16 + (32 if struct.calcsize("P") == 8 else 28)
fd = os.open("/dev/input/event1000", os.O_RDWR | os.O_NONBLOCK)
buf = bytearray(FF_SIZE)
struct.pack_into("=HhHHHHH", buf, 0, 0x50, -1, 0, 0, 0, 400, 0)
struct.pack_into("=HH", buf, 16, 0x8000, 0x4000)
fcntl.ioctl(fd, ioc(1, 0x80, FF_SIZE), buf, True)
eid = struct.unpack_from("=h", buf, 2)[0]
t = time.monotonic()
os.write(fd, struct.pack("=qqHHi" if struct.calcsize("P") == 8 else "=llHHi", 0, 0, 0x15, eid, 1))
time.sleep(0.8)
os.close(fd)
print(json.dumps({"played": t}), flush=True)
'''

# An effect without an end, full on the strong motor and half on the weak,
# played until a line on stdin asks for its stop.
ENDLESS_APP = r'''
import fcntl, json, os, struct, sys, time
def ioc(direction, nr, size):
    return (direction << 30) | (size << 16) | (ord("E") << 8) | nr
FF_SIZE = 16 + (32 if struct.calcsize("P") == 8 else 28)
EVENT = "=qqHHi" if struct.calcsize("P") == 8 else "=llHHi"
fd = os.open("/dev/input/event1000", os.O_RDWR | os.O_NONBLOCK)
buf = bytearray(FF_SIZE)
struct.pack_into("=HhHHHHH", buf, 0, 0x50, -1, 0, 0, 0, 0, 0)
struct.pack_into("=HH", buf, 16, 0xffff, 0x8000)
fcntl.ioctl(fd, ioc(1, 0x80, FF_SIZE), buf, True)
eid = struct.unpack_from("=h", buf, 2)[0]
os.write(fd, struct.pack(EVENT, 0, 0, 0x15, eid, 1))
print(json.dumps({"played": time.monotonic()}), flush=True)
sys.stdin.readline()
os.write(fd, struct.pack(EVENT, 0, 0, 0x15, eid, 0))
print(json.dumps({"stopped": time.monotonic()}), flush=True)
time.sleep(0.3)
os.close(fd)
'''


def page_offset(page) -> float:
    """CLOCK_MONOTONIC seconds minus the page's performance.now() seconds,
    from the tightest of a few round trips."""
    best = None
    for _ in range(15):
        t0 = time.monotonic()
        pn = page.evaluate("performance.now()")
        t1 = time.monotonic()
        if best is None or t1 - t0 < best[0]:
            best = (t1 - t0, (t0 + t1) / 2 - pn / 1000.0)
    return best[1]


def app_env(preload: str) -> dict:
    env = {k: v for k, v in os.environ.items() if k != "LD_PRELOAD"}
    env.update({"LD_PRELOAD": preload, "SELKIES_JS_SOCKET_PATH": H.RUNTIME_DIR})
    return env


def run_app(preload: str) -> float:
    """Run APP; the CLOCK_MONOTONIC time of its play, or None."""
    out = subprocess.run([sys.executable, "-c", APP], env=app_env(preload), capture_output=True,
                         text=True, timeout=60)
    lines = [ln for ln in out.stdout.splitlines() if ln.startswith("{")]
    return json.loads(lines[-1])["played"] if lines else None


def open_page(pw, engine: str, mode: str, fragment: str = "", init: str = "", touch: bool = False):
    if engine == "firefox":
        ctx = C.firefox_persistent_context(pw, viewport={"width": 1280, "height": 720},
                                           prefs=FIREFOX_TOUCH_PREFS if touch else None, has_touch=touch)
        browser = ctx
    else:
        browser = C.launch_browser(pw, engine)
        ctx = browser.new_context(viewport={"width": 1280, "height": 720}, has_touch=touch)
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    if init:
        ctx.add_init_script(init)
    page = ctx.new_page()
    page.goto(H.BASE_URL + "/" + fragment, wait_until="load")
    return browser, ctx, page


def wait_video(page, mode: str):
    return C.wait_ws_video(page, 40) if mode == "websockets" else C.wait_wr_video(page, 60)


def press(page) -> None:
    page.evaluate("window.__padPress(0, 1)")
    time.sleep(0.3)
    page.evaluate("window.__padPress(0, 0)")
    time.sleep(0.5)


def plays_and_stops(calls: list) -> tuple:
    plays = [c for c in calls if c[1] == "play" or (c[1] == "pulse" and c[2] > 0)]
    stops = [c for c in calls if c[1] == "reset" or (c[1] == "pulse" and c[2] == 0)]
    return plays, stops


def pads(pw, mode: str, preload: str, res: "H.Results") -> None:
    for engine in ("chromium", "firefox", "webkit"):
        tag = f"{engine} {mode}"
        gecko = "true" if engine == "firefox" else "false"
        browser, _, page = open_page(pw, engine, mode, init=PAD_INIT % (gecko, "false"))
        closers = [browser]
        try:
            res.check(f"{tag}: video up", wait_video(page, mode) is not None)
            if engine == "chromium":
                other_browser, _, other = open_page(pw, engine, mode, "#player2", PAD_INIT % (gecko, "false"))
                closers.append(other_browser)
                wait_video(other, mode)
                press(other)
            press(page)
            offset = page_offset(page)
            played = run_app(preload)
            if played is None:
                res.check(f"{tag}: the application played an effect", False)
                continue
            time.sleep(0.3)
            calls = page.evaluate("window.__rumble")
            plays, stops = plays_and_stops(calls)
            first = plays[0] if plays else None
            if engine == "firefox":
                want = first is not None and first[2] == 0.5 and 380 <= first[3] <= 400
            else:
                want = (first is not None and first[2] == "dual-rumble" and first[3] == 0.5
                        and first[4] == 0.25 and 380 <= first[5] <= 400)
            lag = (first[0] / 1000.0 + offset - played) * 1000 if first else None
            res.check(f"{tag}: the page plays the application's rumble on its pad",
                      want, f"{calls[:3]}, {lag and round(lag, 1)} ms after the write")
            stop = stops[0] if stops else None
            held = (stop[0] - first[0]) if (stop and first) else None
            res.check(f"{tag}: and stops it when the effect has run",
                      held is not None and 350 <= held <= 480, f"after {held and round(held)} ms")
            if engine == "chromium":
                res.check(f"{tag}: a page driving another slot hears none of it",
                          other.evaluate("window.__rumble.length") == 0, other.evaluate("window.__rumble"))
        finally:
            for closer in closers:
                C.close_browser(closer)


def takeover(pw, mode: str, preload: str, res: "H.Results") -> None:
    tag = f"chromium {mode}"
    first_browser, _, first = open_page(pw, "chromium", mode, init=PAD_INIT % ("false", "false"))
    app = None
    try:
        wait_video(first, mode)
        press(first)
        app = subprocess.Popen([sys.executable, "-c", ENDLESS_APP], env=app_env(preload), text=True,
                               stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        played = json.loads(app.stdout.readline())["played"]
        time.sleep(0.4)
        plays, _ = plays_and_stops(first.evaluate("window.__rumble"))
        res.check(f"{tag}: an effect without an end plays for a lease at a time",
                  plays and plays[0][3:6] == [1, 0.5, 2000], plays[:1])
        C.close_browser(first_browser)
        first_browser = None
        time.sleep(1.0)
        browser, _, page = open_page(pw, "chromium", mode, init=PAD_INIT % ("false", "true"))
        try:
            wait_video(page, mode)
            time.sleep(1.0)
            # Take the slot 0.3 s past a renewal, so the next one is 0.7 s off.
            phase = (time.monotonic() - played) % 1.0
            time.sleep((1.3 - phase) % 1.0)
            page.evaluate("window.__connectPad()")
            time.sleep(0.9)
            calls = page.evaluate("window.__rumble")
            taken = page.evaluate("window.__padConnectedAt")
            plays, _ = plays_and_stops(calls)
            lag = plays[0][0] - taken if plays else None
            res.check(f"{tag}: a client taking the slot mid-effect is handed it at once, not at the next renewal",
                      lag is not None and lag < 250 and plays[0][3:6] == [1, 0.5, 2000],
                      f"{lag and round(lag)} ms after taking the slot: {plays[:1]}")
            app.stdin.write("stop\n")
            app.stdin.flush()
            stopped = json.loads(app.stdout.readline())["stopped"]
            time.sleep(0.4)
            offset = page_offset(page)
            _, stops = plays_and_stops(page.evaluate("window.__rumble"))
            lag = (stops[-1][0] / 1000.0 + offset - stopped) * 1000 if stops else None
            res.check(f"{tag}: and hears its stop", lag is not None and lag < 100,
                      f"{lag and round(lag, 1)} ms after the write")
        finally:
            C.close_browser(browser)
    finally:
        if first_browser is not None:
            C.close_browser(first_browser)
        if app is not None:
            try:
                app.communicate(input="stop\n", timeout=10)
            except Exception:
                app.kill()


def touch(pw, mode: str, preload: str, res: "H.Results", dashboard: str, engines: tuple) -> None:
    for engine in engines:
        tag = f"{engine} {mode} {dashboard}"
        browser, _, page = open_page(pw, engine, mode, init=VIBRATE_INIT, touch=True)
        try:
            wait_video(page, mode)
            time.sleep(1.0)
            page.evaluate("window.postMessage({type: 'toggleTouchGamepad'}, window.location.origin)")
            # The touch gamepad draws its buttons on its own time.
            box = None
            deadline = time.time() + 8
            while box is None and time.time() < deadline:
                time.sleep(0.5)
                box = page.evaluate("""(() => {
                  const els = [...document.querySelectorAll('#universal-touch-gamepad-controls-overlay .touch-button')];
                  const a = els.find((e) => e.textContent.trim() === 'A');
                  if (!a) return null;
                  const r = a.getBoundingClientRect();
                  return [r.left + r.width / 2, r.top + r.height / 2];
                })()""")
            if not box:
                res.check(f"{tag}: the touch gamepad shows its A button", False)
                continue
            # The first touch connects the touch gamepad's pad, which takes the slot.
            if engine == "chromium":
                cdp = page.context.new_cdp_session(page)
                cdp.send("Input.dispatchTouchEvent", {"type": "touchStart",
                                                      "touchPoints": [{"x": box[0], "y": box[1], "id": 1}]})
                time.sleep(0.06)
                cdp.send("Input.dispatchTouchEvent", {"type": "touchEnd", "touchPoints": []})
            else:
                page.evaluate(SYNTH_TAP, box)
            time.sleep(1.0)
            offset = page_offset(page)
            played = run_app(preload)
            time.sleep(0.3)
            vibes = page.evaluate("window.__vibes")
            on = next((v for v in vibes if v[1] > 0), None)
            off = next((v for v in vibes if on and v[0] > on[0] and v[1] == 0), None)
            lag = (on[0] / 1000.0 + offset - played) * 1000 if on and played else None
            res.check(f"{tag}: the touch gamepad vibrates the device for the effect's length",
                      on is not None and 380 <= on[1] <= 400,
                      f"{vibes[:3]}, {lag and round(lag, 1)} ms after the write")
            res.check(f"{tag}: and stops it when the effect has run",
                      off is not None and 350 <= off[0] - on[0] <= 480,
                      f"after {off and round(off[0] - on[0])} ms")
        finally:
            C.close_browser(browser)


def click_rumble_toggle(page, dashboard: str) -> bool:
    if dashboard == "classic":
        try:
            page.locator('.toggle-handle').first.click()
            time.sleep(0.8)
        except Exception:
            pass
        toggle = page.locator('#gamepadRumbleToggle')
        if not toggle.count():
            header = page.locator('.sidebar-section-header:has-text("Gamepads")').first
            if not header.count():
                return False
            header.scroll_into_view_if_needed()
            header.click()
            time.sleep(0.8)
        if not toggle.count():
            return False
        toggle.first.scroll_into_view_if_needed()
        toggle.first.click()
        time.sleep(0.5)
        return True
    ok = wish_open_menu_item(page, "Rumble")
    page.keyboard.press("Escape")
    return ok


def dashboards(pw, mode: str, preload: str, res: "H.Results") -> None:
    stored = """(() => {
      for (let i = 0; i < localStorage.length; i++) {
        const k = localStorage.key(i);
        if (k.endsWith('_gamepad_rumble')) return localStorage.getItem(k);
      }
      return null;
    })()"""
    state = "() => window.webrtcInput ? window.webrtcInput.gamepadRumble : null"
    for dashboard, dist in (("classic", H.CLASSIC_DIST), ("wish", H.WISH_DIST)):
        tag = f"{dashboard} {mode}"
        H.server_stop()
        H.server_start(mode=mode, web_root=dist)
        browser, _, page = open_page(pw, "chromium", mode, init=PAD_INIT % ("false", "false"))
        try:
            wait_video(page, mode)
            time.sleep(1.0)
            press(page)
            res.check(f"{tag}: rumble starts on", page.evaluate(state) is True, page.evaluate(state))
            app = subprocess.Popen([sys.executable, "-c", ENDLESS_APP], env=app_env(preload), text=True,
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            try:
                app.stdout.readline()
                time.sleep(0.4)
                clicked = click_rumble_toggle(page, dashboard)
                res.check(f"{tag}: the dashboard offers a Rumble toggle", clicked, clicked)
                time.sleep(0.6)
                calls = page.evaluate("window.__rumble")
                plays, stops = plays_and_stops(calls)
                res.check(f"{tag}: turning it off stops what plays",
                          plays and stops and stops[-1][0] > plays[-1][0] and page.evaluate(state) is False,
                          calls[-2:])
                n = len(plays)
                time.sleep(1.3)
                plays, _ = plays_and_stops(page.evaluate("window.__rumble"))
                res.check(f"{tag}: and nothing plays while it is off, renewals included", len(plays) == n,
                          plays[n:])
                app.stdin.write("stop\n")
                app.stdin.flush()
                app.stdout.readline()
            finally:
                try:
                    app.communicate(input="stop\n", timeout=10)
                except Exception:
                    app.kill()
            res.check(f"{tag}: the core keeps the pick", page.evaluate(stored) == "false", page.evaluate(stored))
            page.reload(wait_until="load")
            wait_video(page, mode)
            time.sleep(1.0)
            res.check(f"{tag}: a reload comes back with rumble off", page.evaluate(state) is False,
                      page.evaluate(state))
        finally:
            C.close_browser(browser)


def run(mode: str, res: "H.Results") -> None:
    work = os.path.join(H.WORKDIR, f"rumble-{mode}")
    os.makedirs(work, exist_ok=True)
    preload = build_interposer(work)
    try:
        with sync_playwright() as pw:
            H.server_start(mode=mode)
            pads(pw, mode, preload, res)
            takeover(pw, mode, preload, res)
            # The touch gamepad lives in the dashboards.
            H.server_stop()
            H.server_start(mode=mode, web_root=H.CLASSIC_DIST)
            touch(pw, mode, preload, res, "classic", ("chromium", "firefox", "webkit"))
            H.server_stop()
            H.server_start(mode=mode, web_root=H.WISH_DIST)
            touch(pw, mode, preload, res, "wish", ("chromium",))
            dashboards(pw, mode, preload, res)
    finally:
        H.server_stop()


def main() -> None:
    which = sys.argv[1] if len(sys.argv) > 1 else "all"
    res = H.Results("gamepad-rumble-e2e")
    for mode in ("websockets", "webrtc"):
        if which in ("all", mode):
            run(mode, res)
    sys.exit(0 if res.summary() else 1)


if __name__ == "__main__":
    main()
