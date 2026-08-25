#!/usr/bin/env python3
"""A keyboard-layout switch, seen from the application: us -> de -> ru.

The client sends every key as the keysym the browser resolved on the user's
own layout, so a German 'ä' or a Russian 'ф' has to arrive as that character
whatever the server's layout is, and it has to arrive as the layout's OWN key
(its keysym on its physical keycode, no overlay bind) once the server's layout
matches the client's. Two switches drive that here, on both transports:

X11:     the keymap is deployment-owned, so the switch is a server-side
         `setxkbmap` under a live handler (which learns of it only through
         XkbNewKeyboardNotify); the client's `keyboardLayout` hint is noted
         in the log and not applied. What arrives is read back through
         libX11's own XLookupString on a focused window, on a private Xvfb so
         the layout changes touch nothing shared.
Wayland: the client's `keyboardLayout` hint (the layout-map probe for de, the
         language tag for ru) moves the seat's base layout, and the observer
         resolves the keycodes it is delivered against the keymap the seat
         handed it, as a Wayland application would.

The keys are pressed through CDP with the key/code a real de or ru keyboard
produces, since Playwright's own keyboard knows only the US layout.

    python3 tests/e2e/test_keyboard_layout.py ws-x11|wr-x11|ws-wl|wr-wl
"""
import ctypes
import os
import shutil
import subprocess
import sys
import time
from typing import Any, Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "integration"))
import helpers as H
import core_lib as C
from playwright.sync_api import sync_playwright

WL_SOCKET = "wayland-1"
XKB_KEYMAP_FORMAT_TEXT_V1 = 1
# Evdev codes of the keys the probes sit on; an X keycode is eight higher.
KEY_A, KEY_APOSTROPHE = 30, 40

# (layout, the character a user of it types, the DOM code of the key it sits
# on, the keysym the layout binds there). Every probe is typed under every
# layout: it arrives as its character throughout, and as the native key under
# its own layout, which only a handler that saw the switch can deliver.
PROBES = (
    ("us", "a", "KeyA", 0x61, KEY_A),
    ("de", "ä", "Quote", 0xE4, KEY_APOSTROPHE),
    ("ru", "ф", "KeyA", 0x6C6, KEY_A),
)

# How a page announces its layout: Chromium's layout-map probe identifies the
# QWERTZ family, and a Russian client resolves through its language tag.
LAYOUT_MAPS = {
    "us": {"KeyY": "y", "KeyZ": "z", "KeyQ": "q", "KeyA": "a", "Semicolon": ";"},
    "de": {"KeyY": "z", "KeyZ": "y", "Minus": "ß"},
    "ru": {},
}
LOCALES = {"us": "en-US", "de": "de-DE", "ru": "ru-RU"}


def layout_js(layout: str) -> str:
    entries = ", ".join(f"['{k}', '{v}']" for k, v in LAYOUT_MAPS[layout].items())
    return ("(() => { const map = new Map([%s]); Object.defineProperty(navigator, 'keyboard', "
            "{ value: { getLayoutMap: async () => map }, configurable: true }); })();" % entries)


class Xkb:
    """libxkbcommon: keysym characters, and a state machine over a delivered
    keymap for the Wayland observer."""

    def __init__(self) -> None:
        lib = ctypes.CDLL("libxkbcommon.so.0")
        lib.xkb_context_new.restype = ctypes.c_void_p
        lib.xkb_context_new.argtypes = [ctypes.c_int]
        lib.xkb_keymap_new_from_string.restype = ctypes.c_void_p
        lib.xkb_keymap_new_from_string.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int, ctypes.c_int]
        lib.xkb_state_new.restype = ctypes.c_void_p
        lib.xkb_state_new.argtypes = [ctypes.c_void_p]
        lib.xkb_state_update_mask.restype = ctypes.c_int
        lib.xkb_state_update_mask.argtypes = [ctypes.c_void_p] + [ctypes.c_uint32] * 6
        lib.xkb_state_key_get_one_sym.restype = ctypes.c_uint32
        lib.xkb_state_key_get_one_sym.argtypes = [ctypes.c_void_p, ctypes.c_uint32]
        lib.xkb_keysym_to_utf32.restype = ctypes.c_uint32
        lib.xkb_keysym_to_utf32.argtypes = [ctypes.c_uint32]
        self.lib = lib
        self.ctx = lib.xkb_context_new(0)
        self.state = None

    def load(self, path: str) -> bool:
        with open(path, "rb") as f:
            text = f.read()
        keymap = self.lib.xkb_keymap_new_from_string(self.ctx, text, XKB_KEYMAP_FORMAT_TEXT_V1, 0)
        self.state = self.lib.xkb_state_new(keymap) if keymap else None
        return self.state is not None

    def modifiers(self, depressed: int, latched: int, locked: int, group: int) -> None:
        if self.state:
            self.lib.xkb_state_update_mask(self.state, depressed, latched, locked, 0, 0, group)

    def keysym(self, evdev_key: int) -> int:
        return self.lib.xkb_state_key_get_one_sym(self.state, evdev_key + 8) if self.state else 0

    def char(self, keysym: int) -> str:
        cp = self.lib.xkb_keysym_to_utf32(keysym)
        return chr(cp) if cp else ""


class WlKeys:
    """Resolve the observer's key events like an application: every keymap
    and modifier event is replayed in order into the xkb state, so a key is
    read against the keymap the seat had delivered ahead of it."""

    def __init__(self, obs: "H.WlObs", xkb: Xkb) -> None:
        self.obs, self.xkb, self.cursor = obs, xkb, 0

    def since(self, start: int) -> list:
        """`(pressed, evdev key, keysym)` for the key events from `start` on."""
        out = []
        while self.cursor < len(self.obs.lines):
            index, line = self.cursor, self.obs.lines[self.cursor]
            self.cursor += 1
            kind = line.get("kind")
            if kind == "keymap":
                self.xkb.load(line["path"])
                os.unlink(line["path"])
            elif kind == "kbd_mods":
                self.xkb.modifiers(line["dep"], line["lat"], line["lock"], line["group"])
            elif kind == "kbd_key" and index >= start:
                out.append((line["state"] == 1, line["key"], self.xkb.keysym(line["key"])))
        return out


def tap(cdp: Any, char: str, code: str) -> None:
    """Press and release one key the way a keyboard with that layout does."""
    for kind in ("keyDown", "keyUp"):
        cdp.send("Input.dispatchKeyEvent", {"type": kind, "key": char, "code": code,
                                            "text": char, "unmodifiedText": char})


def x11_tapper(cdp: Any, obs: Any) -> Any:
    """A tap that returns what the X observer received: `(pressed, keycode, keysym)`."""
    def events_after(char: str, code: str) -> list:
        obs.drain(0.02)
        tap(cdp, char, code)
        return [(pressed, kc, ks) for pressed, kc, _group, ks in obs.drain(0.6)]
    return events_after


def wl_tapper(cdp: Any, keys: "WlKeys") -> Any:
    """A tap that returns what the Wayland observer resolved it to."""
    def events_after(char: str, code: str) -> list:
        start = len(keys.obs.lines)
        tap(cdp, char, code)
        time.sleep(0.6)
        return keys.since(start)
    return events_after


def open_page(browser: Any, mode: str, layout: str) -> Any:
    ctx = browser.new_context(viewport={"width": 1280, "height": 720}, locale=LOCALES[layout])
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    ctx.add_init_script(layout_js(layout))
    page = ctx.new_page()
    page.goto(H.BASE_URL + "/", wait_until="load")
    return page


def wait_video(page: Any, mode: str) -> Optional[dict]:
    return C.wait_ws_video(page, timeout=30) if mode == "websockets" else C.wait_wr_video(page)


def check_probes(res: "H.Results", layout: str, xkb: Xkb, events_after: Any, keycode_base: int) -> None:
    """Type every probe under `layout` and judge what arrived.

    Args:
        events_after: Callable running one tap and returning the key events it
            produced as `(pressed, keycode, keysym)`.
        keycode_base: What the observer adds to an evdev code (8 on X11).
    """
    for owner, char, code, native, evdev in PROBES:
        events = events_after(char, code)
        got = [(kc, ks) for pressed, kc, ks in events if pressed and xkb.char(ks) == char]
        res.check(f"{layout}: '{char}' arrives as '{char}'", bool(got),
                  [(kc, hex(ks)) for _, kc, ks in events])
        if owner == layout:
            res.check(f"{layout}: '{char}' is the layout's own key",
                      bool(got) and got[0] == (evdev + keycode_base, native), got)


def run_x11(mode: str, res: "H.Results") -> None:
    from test_x11_multigroup import Observer

    xkb = Xkb()
    xvfb, display = H.private_x_server(1280, 720, extra_args=("-s", "0", "-dpms"))
    H.TEST_DISPLAY = display
    try:
        subprocess.run(["setxkbmap", "-display", display, "us"], check=True, timeout=20)
        H.server_start(mode=mode, wayland=False)
        obs = Observer(display)
        try:
            with sync_playwright() as p:
                browser = C.chromium_launch(p)
                try:
                    for layout, _, _, _, _ in PROBES:
                        subprocess.run(["setxkbmap", "-display", display, layout], check=True, timeout=20)
                        time.sleep(0.5)
                        page = open_page(browser, mode, layout)
                        res.check(f"{layout}: video flowing", bool(wait_video(page, mode)))
                        res.check(f"{layout}: hint noted, X11 keymap left to the deployment",
                                  C.wait_log(f"keyboard layout hint '{layout}' noted (X11 keymap", timeout=15), "")
                        page.mouse.click(640, 360)
                        time.sleep(0.5)
                        check_probes(res, layout, xkb, x11_tapper(page.context.new_cdp_session(page), obs), 8)
                        page.context.close()
                finally:
                    browser.close()
        finally:
            obs.close()
            H.server_stop()
    finally:
        H.stop_x_server(xvfb, display)


def run_wayland(mode: str, res: "H.Results") -> None:
    xkb = Xkb()
    H.server_start(mode=mode, wayland=True)
    obs = H.WlObs(WL_SOCKET)
    keys = WlKeys(obs, xkb)
    try:
        res.check("wl observer mapped", obs.ready(20))
        with sync_playwright() as p:
            browser = C.chromium_launch(p)
            try:
                for layout, _, _, _, _ in PROBES:
                    page = open_page(browser, mode, layout)
                    res.check(f"{layout}: video flowing", bool(wait_video(page, mode)))
                    res.check(f"{layout}: seat base layout follows the client hint",
                              C.wait_log(f"Wayland base layout set to '{layout}'", timeout=15), "")
                    page.mouse.click(640, 360)
                    time.sleep(0.5)
                    check_probes(res, layout, xkb, wl_tapper(page.context.new_cdp_session(page), keys), 0)
                    page.context.close()
            finally:
                browser.close()
    finally:
        obs.stop()
        H.server_stop()


SELECTORS = ("ws-x11", "wr-x11", "ws-wl", "wr-wl")


def main() -> bool:
    which = sys.argv[1] if len(sys.argv) > 1 else "ws-x11"
    if which not in SELECTORS:
        raise SystemExit(f"unknown selector {which!r}; one of {SELECTORS}")
    if not shutil.which("setxkbmap"):
        H.skip_suite("setxkbmap is not installed")
    try:
        ctypes.CDLL("libxkbcommon.so.0")
    except OSError:
        H.skip_suite("libxkbcommon is not installed")
    transport, backend = which.split("-")
    mode = "websockets" if transport == "ws" else "webrtc"
    res = H.Results(f"keyboard-layout-{which}")
    if backend == "x11":
        run_x11(mode, res)
    else:
        run_wayland(mode, res)
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
