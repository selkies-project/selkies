#!/usr/bin/env python3
"""Gaming mode in a real browser: the pointer and the keyboard held, and a game fed.

Gaming mode is where a game gets its input: fullscreen with the pointer locked,
the keyboard locked so Escape reaches the session, and locked motion relayed as
deltas. A headless engine takes synthetic input and holds no keyboard, so this
drives the installed Chrome, windowed under openbox on a private X server of
its own, with XTEST keys, clicks and relative motion the way a mouse and a
keyboard deliver them, while the server streams the test display or its
Wayland compositor and an SDL2 window in relative mode over that desktop
(tests/tools/sdl_relative_probe.py) stands in for the game.

What has to hold: the gaming mode chord fullscreens the page and locks the
pointer; every relative move on the browser's display goes out as exactly that
delta and reaches the game as exactly that delta, one event per message; a key
reaches the game; a tap of Escape reaches it as Escape and leaves the mode
standing, since the keyboard lock holds the key; a held Escape ends the mode.
A second page withholds the Keyboard Lock API the way Brave's Shields do, and
there the client has to say so once and a single Escape has to end the mode,
which is what a user of that browser gets.

    python3 tests/e2e/test_gaming_mode.py x11|wl
"""
import os
import shutil
import subprocess
import sys
import time
from typing import Any

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import helpers as H
import core_lib as C
import test_pointer_lock as PL
from playwright.sync_api import sync_playwright

WINDOW = (1600, 900)
CENTER = (800, 450)
# Deltas a hand makes: a flick each way, a one-axis nudge, a diagonal.
MOVES = ((12, 7), (-5, 9), (80, -30), (3, 0), (0, -4))
ESCAPE_HOLD = 3.0

NOTICE_TAP_JS = """
window.__notices = [];
window.addEventListener('message', (e) => {
  const d = e.data;
  if (d && d.type === 'fileUpload' && d.payload && typeof d.payload.code === 'string') {
    window.__notices.push(d.payload.code);
  }
});
"""
# What Brave's Shields leave a page: no Keyboard API at all, and Brave's own
# navigator object to tell the browser by.
SHIELDS_JS = """
Object.defineProperty(navigator, 'keyboard', { value: undefined, configurable: true });
Object.defineProperty(navigator, 'brave', { value: {}, configurable: true });
"""
MODE_JS = ("({ fullscreen: document.fullscreenElement !== null, "
           "locked: document.pointerLockElement !== null, "
           "gaming: !!(window.webrtcInput && window.webrtcInput.gamingMode) })")


class Desk:
    """The browser's own desktop: a private X server managed by openbox, and the
    XTEST connection that plays the user's hands on it."""

    def __init__(self) -> None:
        self.xvfb, self.display = H.private_x_server(*WINDOW)
        self.wm = H.spawn(["openbox", "--replace"], env={**os.environ, "DISPLAY": self.display},
                          stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)
        deadline = time.time() + 15
        while time.time() < deadline and not PL.Desktop._x11_managed(self.display):
            time.sleep(0.25)
        from selkies.Xlib.display import Display
        self.d = Display(self.display)
        self.root = self.d.screen().root

    def key(self, name: str, down: bool) -> None:
        from selkies.Xlib import X, XK
        from selkies.Xlib.ext import xtest
        keycode = self.d.keysym_to_keycode(XK.string_to_keysym(name))
        xtest.fake_input(self.d, X.KeyPress if down else X.KeyRelease, keycode)
        self.d.flush()

    def tap(self, name: str, hold: float = 0.06) -> None:
        self.key(name, True)
        time.sleep(hold)
        self.key(name, False)

    def chord(self, *names: str) -> None:
        for name in names:
            self.key(name, True)
            time.sleep(0.03)
        for name in reversed(names):
            self.key(name, False)
            time.sleep(0.03)

    def click(self, x: int, y: int) -> None:
        from selkies.Xlib import X
        from selkies.Xlib.ext import xtest
        self.root.warp_pointer(x, y)
        self.d.sync()
        time.sleep(0.1)
        xtest.fake_input(self.d, X.ButtonPress, 1)
        self.d.flush()
        time.sleep(0.05)
        xtest.fake_input(self.d, X.ButtonRelease, 1)
        self.d.flush()

    def move(self, dx: int, dy: int) -> None:
        from selkies.Xlib import X
        from selkies.Xlib.ext import xtest
        xtest.fake_input(self.d, X.MotionNotify, detail=True, root=X.NONE, x=dx, y=dy)
        self.d.flush()

    def stop(self) -> None:
        try:
            self.d.close()
        except Exception:
            pass
        for proc in (self.wm, self.xvfb):
            try:
                proc.terminate()
                proc.wait(5)
            except Exception:
                pass


def wait_mode(page: Any, timeout: float, **want: bool) -> dict:
    """The page's mode flags, polled until the wanted ones hold or time runs out."""
    deadline = time.time() + timeout
    mode = page.evaluate(MODE_JS)
    while time.time() < deadline:
        mode = page.evaluate(MODE_JS)
        if all(mode.get(k) == v for k, v in want.items()):
            break
        time.sleep(0.1)
    return mode


def open_page(browser: Any, shields: bool) -> Any:
    ctx = browser.new_context(no_viewport=True)
    ctx.add_init_script("window.__SELKIES_STREAMING_MODE__ = 'websockets';")
    ctx.add_init_script(C.WIRE_TAP_JS)
    ctx.add_init_script(NOTICE_TAP_JS)
    if shields:
        ctx.add_init_script(SHIELDS_JS)
    page = ctx.new_page()
    page.goto(H.BASE_URL + "/", wait_until="load")
    return page


def game_window(res: "H.Results", desk: Desk, wayland: bool, capture: str, tag: str) -> Any:
    """The game window over the desktop, with the pointer already on it, as a player
    has it before locking: SDL counts relative motion only once something has
    positioned its pointer, so the first delta of a lock taken cold is dropped."""
    probe = PL.GameProbe(wayland, capture or None)
    mode = probe.wait("relative_mode", 20)
    if mode is None:
        res.skip(f"{tag}: a game window over the desktop takes relative mouse mode", probe.unavailable())
        probe.stop()
        return None
    res.check(f"{tag}: a game window over the desktop takes relative mouse mode", mode.get("on"), mode)
    desk.click(*CENTER)
    entered = probe.wait("window", 8, event="enter")
    res.check(f"{tag}: the pointer reaches the game window", entered is not None, probe.lines[-3:])
    return probe


def played(res: "H.Results", desk: Desk, page: Any, probe: Any, tag: str) -> None:
    """Gaming mode with a keyboard the browser locks: the game is fed, Escape is held."""
    try:
        time.sleep(0.3)
        wire_mark = len(page.evaluate(PL.MOVES_JS))
        probe_mark = len(probe.lines)
        for dx, dy in MOVES:
            desk.move(dx, dy)
            time.sleep(0.1)
        wire = PL.wire_deltas(PL.moves_since(page, wire_mark))
        seen = [(m["dx"], m["dy"]) for m in probe.events("motion", probe_mark, len(MOVES), 6)]
        # The engine rounds a locked movement its own way (a pixel can shift
        # between two events), so the count and the distance are what the
        # browser owes; the game must then see exactly what the wire carried.
        travel = (sum(d[0] for d in wire), sum(d[1] for d in wire))
        moved = (sum(d[0] for d in MOVES), sum(d[1] for d in MOVES))
        res.check(f"{tag}: the moves of the mouse go out as relative motion adding up to the distance moved",
                  len(wire) == len(MOVES) and travel == moved, f"{wire} travel {travel} moved {moved}")
        res.check(f"{tag}: every move reaches the game as the delta the wire carried, one event each",
                  seen == wire, f"game {seen} wire {wire}")

        key_mark = len(probe.lines)
        desk.tap("w")
        keys = [(k["down"], k["sym"]) for k in probe.events("key", key_mark, 2, 6)]
        res.check(f"{tag}: a key reaches the game", keys == [(True, ord("w")), (False, ord("w"))], keys)

        key_mark = len(probe.lines)
        desk.tap("Escape")
        keys = [(k["down"], k["sym"]) for k in probe.events("key", key_mark, 2, 6)]
        res.check(f"{tag}: a tap of Escape reaches the game as Escape", keys == [(True, 27), (False, 27)], keys)
        time.sleep(1.0)
        mode = page.evaluate(MODE_JS)
        res.check(f"{tag}: a tap of Escape leaves gaming mode standing",
                  mode["fullscreen"] and mode["locked"] and mode["gaming"], mode)
    finally:
        probe.stop()

    desk.key("Escape", True)
    time.sleep(ESCAPE_HOLD)
    desk.key("Escape", False)
    mode = wait_mode(page, 10, fullscreen=False)
    res.check(f"{tag}: a held Escape ends gaming mode",
              not mode["fullscreen"] and not mode["locked"] and not mode["gaming"], mode)
    notices = page.evaluate("window.__notices")
    res.check(f"{tag}: a browser that locks the keyboard hears no notice about it", notices == [], notices)


def shielded(res: "H.Results", desk: Desk, page: Any, tag: str) -> None:
    """Gaming mode without the Keyboard Lock API: said once, and one Escape ends it."""
    notices = page.evaluate("window.__notices")
    res.check(f"{tag}: entering without a keyboard lock says so once, naming the Shields",
              notices == ["keyboardLockBlockedByShields"], notices)
    desk.tap("Escape")
    mode = wait_mode(page, 10, fullscreen=False)
    res.check(f"{tag}: a single Escape ends gaming mode there",
              not mode["fullscreen"] and not mode["locked"] and not mode["gaming"], mode)
    res.check(f"{tag}: the notice is not repeated", page.evaluate("window.__notices") == ["keyboardLockBlockedByShields"],
              page.evaluate("window.__notices"))


def enter_gaming_mode(res: "H.Results", desk: Desk, page: Any, tag: str) -> bool:
    """Focus the stream and press the chord; True once the page is fullscreen and locked."""
    desk.click(*CENTER)
    time.sleep(0.5)
    desk.chord("Control_L", "Shift_L", "x")
    mode = wait_mode(page, 15, fullscreen=True, locked=True)
    ok = mode["fullscreen"] and mode["locked"] and mode["gaming"]
    res.check(f"{tag}: the gaming mode chord fullscreens the page and locks the pointer", ok, mode)
    if ok:
        time.sleep(0.5)
    return ok


def run(wayland: bool, res: "H.Results") -> None:
    if not shutil.which("openbox"):
        H.skip_suite("openbox is not installed, and a fullscreen browser window needs a window manager")
    chrome = C.CHROME_PATH or shutil.which("google-chrome")
    if not chrome:
        H.skip_suite("no installed Chrome (E2E_CHROME or google-chrome on PATH) to hold a real keyboard lock")
    H.server_start(mode="websockets", wayland=wayland)
    desk = None
    capture = ""
    try:
        if wayland:
            capture = H.capture_socket()
            res.check("the capture compositor announced its socket", bool(capture), capture)
        desk = Desk()
        with sync_playwright() as p:
            browser = p.chromium.launch(
                executable_path=chrome, headless=False,
                args=C.BROWSER_ARGS + ["--window-position=0,0", f"--window-size={WINDOW[0]},{WINDOW[1]}",
                                       "--disable-features=KeyboardAndPointerLockPrompt"],
                env={**os.environ, "DISPLAY": desk.display})
            try:
                page = open_page(browser, shields=False)
                video = C.wait_ws_video(page, timeout=30)
                res.check("chrome: video flowing", bool(video), video)
                probe = game_window(res, desk, wayland, capture, "chrome")
                if probe is not None and enter_gaming_mode(res, desk, page, "chrome"):
                    played(res, desk, page, probe, "chrome")
                if probe is not None:
                    probe.stop()
                page.context.close()

                page = open_page(browser, shields=True)
                video = C.wait_ws_video(page, timeout=30)
                res.check("shields: video flowing", bool(video), video)
                if enter_gaming_mode(res, desk, page, "shields"):
                    shielded(res, desk, page, "shields")
                page.context.close()
            finally:
                browser.close()
    finally:
        if desk is not None:
            desk.stop()
        H.server_stop()


SELECTORS = ("x11", "wl")


def main() -> bool:
    which = sys.argv[1] if len(sys.argv) > 1 else "x11"
    if which not in SELECTORS:
        raise SystemExit(f"unknown selector {which!r}; one of {SELECTORS}")
    res = H.Results(f"gaming-mode-{which}")
    run(which == "wl", res)
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
