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

A macOS client then types the same way through its Option key, which the layout
uses as a level-3 shift while Blink reports it as a plain Alt: the chord has to
arrive as the character it produced, and an Option over a key that produced none
has to stay the Alt shortcut it is. The page announces itself as macOS, so the
client takes the platform's own path.

Its Command chords go through Chromium, Firefox, and WebKit, with keyups arriving
as Blink and WebKit deliver them on macOS: none for a key let go while Command is
down, and none for Command itself when Spotlight takes it. A second chord under
the same Command has to keep the Control it stands for, a key pressed twice under
it has to arrive twice, and the application has to hold nothing Command withheld
once an event shows Command up. A key held ten seconds with no Command has to
stay held all the while.

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

# A macOS client, and the Option chords a US and a German macOS layout produce.
# CDP's Alt bit is all Blink gives Option: it never reports AltGraph for it.
CDP_ALT = 1
MAC_INIT = ("Object.defineProperty(navigator, 'platform', "
            "{ get: () => 'MacIntel', configurable: true });")
XK_ALT_L = 0xFFE9
XK_TAB = 0xFF09
# What Blink and WebKit on macOS keep from the page: the keyup of a key let go
# while Command is down, and Command's own once Spotlight has taken it.
MAC_COMMAND_KEYUPS = """
window.__loseCommandUp = false;
window.addEventListener('keyup', (e) => {
  const command = e.code === 'MetaLeft' || e.code === 'MetaRight';
  if (command ? window.__loseCommandUp : e.metaKey) {
    if (command) window.__loseCommandUp = false;
    e.stopImmediatePropagation();
  }
}, true);
"""
COMMAND_ENGINES = ("chromium", "firefox", "webkit")
XK_CONTROL_L, XK_SPACE, XK_BACKSPACE, XK_RETURN = 0xFFE3, 0x20, 0xFF08, 0xFF0D
XK_A, XK_C, XK_W = 0x61, 0x63, 0x77
LONG_HOLD_S = 10.0


def opt(kind: str, key: str, code: str, text: Optional[str] = None,
        location: int = 0, modifiers: int = CDP_ALT) -> dict:
    """One CDP key event of an Option chord."""
    event = {"type": kind, "key": key, "code": code, "modifiers": modifiers}
    if location:
        event["location"] = location
    if text is not None:
        event.update({"text": text, "unmodifiedText": text})
    return event


def option_chord(code: str, char: Optional[str]) -> list:
    """Option held down over one key, then both released."""
    key = char if char is not None else code
    return [opt("keyDown", "Alt", "AltLeft", location=1),
            opt("keyDown", key, code, text=char),
            opt("keyUp", key, code, text=char),
            opt("keyUp", "Alt", "AltLeft", location=1, modifiers=0)]


# (name, the chord, the character it has to type, the keysym it has to press).
OPTION_CHORDS = (
    ("Option+Z", option_chord("KeyZ", "\u03a9"), "\u03a9", None),
    ("Option+L on a German layout", option_chord("KeyL", "@"), "@", None),
    ("Option+Tab", option_chord("Tab", None), None, XK_TAB),
)


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


def open_page(browser: Any, mode: str, layout: str, mac: bool = False) -> Any:
    ctx = browser.new_context(viewport={"width": 1280, "height": 720}, locale=LOCALES[layout])
    ctx.add_init_script(f"window.__SELKIES_STREAMING_MODE__ = '{mode}';")
    ctx.add_init_script(layout_js(layout))
    if mac:
        ctx.add_init_script(MAC_INIT)
    page = ctx.new_page()
    page.goto(H.BASE_URL + "/", wait_until="load")
    return page


def x11_chorder(cdp: Any, obs: Any) -> Any:
    """A chord runner returning what the X observer received."""
    def events_after(chord: list) -> list:
        obs.drain(0.02)
        for event in chord:
            cdp.send("Input.dispatchKeyEvent", event)
        return [(pressed, kc, ks) for pressed, kc, _group, ks in obs.drain(0.8)]
    return events_after


def wl_chorder(cdp: Any, keys: "WlKeys") -> Any:
    """A chord runner returning what the Wayland observer resolved it to, read
    once its key events stop for 0.3 s: a loaded host can deliver a chord past
    any fixed window, and the 5 s bound still fails one that never comes."""
    def events_after(chord: list) -> list:
        start = len(keys.obs.lines)
        for event in chord:
            cdp.send("Input.dispatchKeyEvent", event)
        deadline, seen, settled_at = time.monotonic() + 5.0, 0, 0.0
        while time.monotonic() < deadline:
            count = sum(1 for line in keys.obs.lines[start:] if line.get("kind") == "kbd_key")
            if count != seen:
                seen, settled_at = count, time.monotonic()
            elif seen and time.monotonic() - settled_at >= 0.3:
                break
            time.sleep(0.02)
        return keys.since(start)
    return events_after


def check_option_chords(res: "H.Results", label: str, xkb: Xkb, events_after: Any) -> None:
    """Type every Option chord and judge what the application received."""
    for name, chord, char, keysym in OPTION_CHORDS:
        pressed = [ks for down, _kc, ks in events_after(chord) if down]
        seen = [hex(ks) for ks in pressed]
        if char is not None:
            res.check(f"{label}: {name} types the character it produced",
                      any(xkb.char(ks) == char for ks in pressed), seen)
            res.check(f"{label}: {name} wraps no Alt around it",
                      XK_ALT_L not in pressed, seen)
        else:
            res.check(f"{label}: {name} stays the Alt shortcut",
                      XK_ALT_L in pressed and keysym in pressed, seen)


class Held:
    """The keys an application holds, followed through the press and release
    events it is delivered: `read` returns `(pressed, keysym)` for those that
    arrived since it last ran."""

    def __init__(self, read: Any) -> None:
        self.read, self.down = read, set()

    def presses(self) -> list:
        """Keysyms pressed since the last call, repeats included."""
        out = []
        for pressed, keysym in self.read():
            if pressed:
                out.append(keysym)
                self.down.add(keysym)
            else:
                self.down.discard(keysym)
        return out


def command_page(p: Any, chromium: Any, engine: str, mode: str) -> tuple:
    """A macOS page on `engine` whose keyups arrive as macOS Blink and WebKit
    deliver them; `(page, closer)`, the closer ending what was opened for it."""
    if engine == "firefox":
        ctx = closer = C.firefox_persistent_context(p, viewport={"width": 1280, "height": 720})
    else:
        browser = chromium if engine == "chromium" else C.launch_browser(p, engine)
        ctx = browser.new_context(viewport={"width": 1280, "height": 720})
        closer = ctx if engine == "chromium" else browser
    for script in (f"window.__SELKIES_STREAMING_MODE__ = '{mode}';", MAC_INIT, MAC_COMMAND_KEYUPS):
        ctx.add_init_script(script)
    page = ctx.pages[0] if ctx.pages else ctx.new_page()
    page.goto(H.BASE_URL + "/", wait_until="load")
    return page, closer


def check_command_chords(res: "H.Results", label: str, page: Any, held: Held,
                         repeats: bool) -> None:
    """Command chords with the macOS keyups missing, then a long plain hold.

    Args:
        repeats: Whether the server repeats a held key itself (X11), which
            the long hold then has to show.
    """
    kb = page.keyboard
    kb.down("Meta")
    kb.press("a")
    kb.press("c")
    time.sleep(0.3)
    pressed = held.presses()
    res.check(f"{label}: Cmd+A then Cmd+C holds Control for the C",
              XK_C in pressed and XK_CONTROL_L in held.down,
              f"pressed {[hex(k) for k in pressed]}, held {sorted(hex(k) for k in held.down)}")
    kb.up("Meta")
    time.sleep(0.3)
    held.presses()
    res.check(f"{label}: Command's keyup lets go of the chord",
              not held.down & {XK_A, XK_C, XK_CONTROL_L}, [hex(k) for k in held.down])

    def rolled() -> None:
        kb.down(" ")
        kb.down("Meta")
        kb.up(" ")

    def chorded() -> None:
        kb.down("Meta")
        kb.press("Enter")

    spotlight(res, f"{label}: a space rolled into Cmd+Space goes at the next key", page, held,
              XK_SPACE, rolled, lambda: kb.press("Escape"))
    spotlight(res, f"{label}: a Cmd+Return's Return goes at a click", page, held,
              XK_RETURN, chorded, lambda: page.mouse.click(640, 360))
    kb.down("Meta")
    kb.press("Backspace")
    kb.press("Backspace")
    time.sleep(0.3)
    pressed = held.presses()
    res.check(f"{label}: Cmd+Backspace pressed twice deletes twice",
              pressed.count(XK_BACKSPACE) == 2 and XK_CONTROL_L in held.down,
              f"pressed {[hex(k) for k in pressed]}, held {sorted(hex(k) for k in held.down)}")
    kb.up("Meta")
    time.sleep(0.3)
    held.presses()
    long_hold(res, label, kb, held, repeats)


def spotlight(res: "H.Results", name: str, page: Any, held: Held, keysym: int,
              before: Any, after: Any) -> None:
    """`before` leaves `keysym` down under Command, Spotlight takes Command's
    keyup, and `after` is the first the page hears of Command being up."""
    before()
    page.evaluate("window.__loseCommandUp = true")
    page.keyboard.up("Meta")
    time.sleep(0.3)
    held.presses()
    stuck = keysym in held.down
    after()
    time.sleep(0.3)
    held.presses()
    res.check(f"{name} once Spotlight took Command's keyup", stuck and keysym not in held.down,
              f"held before {stuck}, after {keysym in held.down}")


def long_hold(res: "H.Results", label: str, kb: Any, held: Held, repeats: bool) -> None:
    """A key held for LONG_HOLD_S stays held, and repeats where the server repeats."""
    kb.down("w")
    time.sleep(LONG_HOLD_S)
    pressed = held.presses()
    res.check(f"{label}: a key held {LONG_HOLD_S:.0f} s stays held",
              XK_W in held.down and (not repeats or pressed.count(XK_W) > 1),
              f"{pressed.count(XK_W)} presses")
    kb.up("w")
    time.sleep(0.3)
    held.presses()
    res.check(f"{label}: and goes at its keyup", XK_W not in held.down, "")


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
                    subprocess.run(["setxkbmap", "-display", display, "us"], check=True, timeout=20)
                    time.sleep(0.5)
                    page = open_page(browser, mode, "us", mac=True)
                    res.check("macOS: video flowing", bool(wait_video(page, mode)))
                    page.mouse.click(640, 360)
                    time.sleep(0.5)
                    check_option_chords(res, "x11", xkb,
                                        x11_chorder(page.context.new_cdp_session(page), obs))
                    page.context.close()
                    for engine in COMMAND_ENGINES:
                        page, closer = command_page(p, browser, engine, mode)
                        res.check(f"macOS {engine}: video flowing", bool(wait_video(page, mode)))
                        page.mouse.click(640, 360)
                        time.sleep(0.5)
                        obs.drain(0.1)
                        held = Held(lambda: [(down, ks) for down, _kc, _g, ks in obs.drain(0.05)])
                        check_command_chords(res, f"x11 {engine}", page, held, repeats=True)
                        closer.close()
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
    obs = H.WlObs(WL_SOCKET, WLOBS_DURATION="600")
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
                page = open_page(browser, mode, "us", mac=True)
                res.check("macOS: video flowing", bool(wait_video(page, mode)))
                page.mouse.click(640, 360)
                time.sleep(0.5)
                check_option_chords(res, "wayland", xkb,
                                    wl_chorder(page.context.new_cdp_session(page), keys))
                page.context.close()
                for engine in COMMAND_ENGINES:
                    page, closer = command_page(p, browser, engine, mode)
                    res.check(f"macOS {engine}: video flowing", bool(wait_video(page, mode)))
                    page.mouse.click(640, 360)
                    time.sleep(0.5)
                    keys.since(0)
                    held = Held(lambda: [(down, ks) for down, _key, ks in keys.since(0)])
                    check_command_chords(res, f"wayland {engine}", page, held, repeats=False)
                    closer.close()
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
