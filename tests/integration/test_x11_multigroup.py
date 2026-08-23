#!/usr/bin/env python3
"""Keysyms of a later layout group reach X11 applications as typed.

On a multi-group X layout (`setxkbmap us,ru`) a Cyrillic keysym shares its
keycode with a Latin glyph and the server translates an injected keycode under
whichever group it has locked, so the XTEST keyboard has to lock the keysym's
group around the injection and put the previous lock back afterwards. A
single-group layout must keep its bare fast path, a German layout its AltGr
levels, and a layout switch made while the handler is live must be seen — the
XKB-aware input connection learns of it only through XkbNewKeyboardNotify.

Runs against its own X server and reads what arrives back through libX11's
own XLookupString, the translation real X clients perform, so the check is
against the server's word rather than this package's own keymap reading.
"""
import asyncio
import ctypes
import os
import shutil
import subprocess
import sys
import time
from typing import Any

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))) + "/src")
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402

XKB_USE_CORE_KBD = 0x0100
KEY_PRESS, KEY_RELEASE = 2, 3
KEY_PRESS_MASK, KEY_RELEASE_MASK = 1 << 0, 1 << 1
REVERT_TO_PARENT = 2
# The group bits XKB folds into a key event's state field.
GROUP_SHIFT = 13

A, A_UPPER, Q, AT, AE, ADIAERESIS, ADIAERESIS_UPPER = 0x61, 0x41, 0x71, 0x40, 0xE6, 0xE4, 0xC4
COMMA, PERIOD = 0x2C, 0x2E
CYRILLIC_EF, CYRILLIC_EF_UPPER = 0x6C6, 0x6E6
CYRILLIC_BE, CYRILLIC_BE_UPPER = 0x6C2, 0x6E2
CYRILLIC_YERU, NUMEROSIGN = 0x6D9, 0x6B0


class XKeyEvent(ctypes.Structure):
    _fields_ = [("type", ctypes.c_int), ("serial", ctypes.c_ulong), ("send_event", ctypes.c_int),
                ("display", ctypes.c_void_p), ("window", ctypes.c_ulong), ("root", ctypes.c_ulong),
                ("subwindow", ctypes.c_ulong), ("time", ctypes.c_ulong), ("x", ctypes.c_int),
                ("y", ctypes.c_int), ("x_root", ctypes.c_int), ("y_root", ctypes.c_int),
                ("state", ctypes.c_uint), ("keycode", ctypes.c_uint), ("same_screen", ctypes.c_int)]


class XEvent(ctypes.Union):
    _fields_ = [("type", ctypes.c_int), ("xkey", XKeyEvent), ("pad", ctypes.c_long * 24)]


class XkbStateRec(ctypes.Structure):
    _fields_ = [("group", ctypes.c_ubyte), ("locked_group", ctypes.c_ubyte),
                ("base_group", ctypes.c_ushort), ("latched_group", ctypes.c_ushort),
                ("mods", ctypes.c_ubyte), ("base_mods", ctypes.c_ubyte), ("latched_mods", ctypes.c_ubyte),
                ("locked_mods", ctypes.c_ubyte), ("compat_state", ctypes.c_ubyte),
                ("grab_mods", ctypes.c_ubyte), ("compat_grab_mods", ctypes.c_ubyte),
                ("lookup_mods", ctypes.c_ubyte), ("compat_lookup_mods", ctypes.c_ubyte),
                ("ptr_buttons", ctypes.c_ushort)]


def _libx11() -> ctypes.CDLL:
    lib = ctypes.CDLL("libX11.so.6")
    lib.XOpenDisplay.restype = ctypes.c_void_p
    lib.XOpenDisplay.argtypes = [ctypes.c_char_p]
    lib.XDefaultRootWindow.restype = ctypes.c_ulong
    lib.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
    lib.XCreateSimpleWindow.restype = ctypes.c_ulong
    lib.XCreateSimpleWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_int,
                                        ctypes.c_uint, ctypes.c_uint, ctypes.c_uint, ctypes.c_ulong,
                                        ctypes.c_ulong]
    lib.XSelectInput.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_long]
    lib.XMapWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
    lib.XSetInputFocus.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_ulong]
    lib.XSync.argtypes = [ctypes.c_void_p, ctypes.c_int]
    lib.XPending.argtypes = [ctypes.c_void_p]
    lib.XNextEvent.argtypes = [ctypes.c_void_p, ctypes.POINTER(XEvent)]
    lib.XLookupString.argtypes = [ctypes.POINTER(XKeyEvent), ctypes.c_char_p, ctypes.c_int,
                                  ctypes.POINTER(ctypes.c_ulong), ctypes.c_void_p]
    lib.XkbGetState.argtypes = [ctypes.c_void_p, ctypes.c_uint, ctypes.POINTER(XkbStateRec)]
    lib.XkbLockGroup.argtypes = [ctypes.c_void_p, ctypes.c_uint, ctypes.c_uint]
    lib.XDestroyWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
    lib.XCloseDisplay.argtypes = [ctypes.c_void_p]
    return lib


class Observer:
    """A focused libX11 window that decodes the key events it receives the way
    an application does, on a connection of its own."""

    def __init__(self, display_name: str) -> None:
        self.lib = _libx11()
        self.dpy = self.lib.XOpenDisplay(display_name.encode())
        if not self.dpy:
            raise RuntimeError("XOpenDisplay failed for the observer")
        root = self.lib.XDefaultRootWindow(self.dpy)
        self.win = self.lib.XCreateSimpleWindow(self.dpy, root, 0, 0, 100, 100, 0, 0, 0)
        self.lib.XSelectInput(self.dpy, self.win, KEY_PRESS_MASK | KEY_RELEASE_MASK)
        self.lib.XMapWindow(self.dpy, self.win)
        self.lib.XSync(self.dpy, 0)
        self.lib.XSetInputFocus(self.dpy, self.win, REVERT_TO_PARENT, 0)
        self.lib.XSync(self.dpy, 0)

    def locked_group(self) -> int:
        st = XkbStateRec()
        self.lib.XkbGetState(self.dpy, XKB_USE_CORE_KBD, ctypes.byref(st))
        return st.locked_group

    def lock_group(self, group: int) -> None:
        self.lib.XkbLockGroup(self.dpy, XKB_USE_CORE_KBD, group)
        self.lib.XSync(self.dpy, 0)

    def drain(self, settle: float = 0.25) -> list:
        """Key events received since the last drain: `(press, keycode, group, keysym)`."""
        time.sleep(settle)
        self.lib.XSync(self.dpy, 0)
        out = []
        ev = XEvent()
        while self.lib.XPending(self.dpy):
            self.lib.XNextEvent(self.dpy, ctypes.byref(ev))
            if ev.type not in (KEY_PRESS, KEY_RELEASE):
                continue
            ks = ctypes.c_ulong(0)
            buf = ctypes.create_string_buffer(16)
            self.lib.XLookupString(ctypes.byref(ev.xkey), buf, 16, ctypes.byref(ks), None)
            out.append((ev.type == KEY_PRESS, ev.xkey.keycode,
                        (ev.xkey.state >> GROUP_SHIFT) & 3, ks.value))
        return out

    def close(self) -> None:
        self.lib.XDestroyWindow(self.dpy, self.win)
        self.lib.XCloseDisplay(self.dpy)


def make_handler(xdisplay: Any) -> Any:
    """A bare X11 WebRTCInput: just what send_x11_keypress and the keymap
    watch consult."""
    from selkies.input_handler import WebRTCInput, _XTestKeyboard

    h = WebRTCInput.__new__(WebRTCInput)
    h.is_wayland = False
    h.wayland_input = None
    h.xdisplay = xdisplay
    h.keyboard = _XTestKeyboard(xdisplay)
    h.active_modifiers = set()
    h.active_shortcut_modifiers = set()
    h.atomically_typed_keys = set()
    h.translated_keys = set()
    h.pressed_keys = {}
    h.reaped_atomic_keys = set()
    h.key_repeat_state = {}
    h.SHORTCUT_MODIFIER_XKEY_NAMES = {'Control_L', 'Control_R', 'Alt_L', 'Alt_R',
                                      'Super_L', 'Super_R', 'Meta_L', 'Meta_R'}
    h.ACTION_MODIFIER_KEYSYMS = {0xFFE3, 0xFFE4, 0xFFE9, 0xFFEA, 0xFFE7, 0xFFE8,
                                 0xFFEB, 0xFFEC, 0xFFED, 0xFFEE}
    h.LEVEL_MODIFIER_KEYSYMS = frozenset({0xFFE1, 0xFFE2, 0xFE03, 0xFF7E})
    h.MODIFIER_KEYSYMS = {0xFFE1, 0xFFE2, 0xFFE3, 0xFFE4, 0xFFE9, 0xFFEA, 0xFE03,
                          0xFFE7, 0xFFE8, 0xFFEB, 0xFFEC, 0xFFED, 0xFFEE}
    h.loop = asyncio.get_running_loop()
    return h


def unicode_alias(keysym: int) -> int:
    from selkies.input_handler import overlay_bind_keysym
    return overlay_bind_keysym(keysym)


async def tap(handler, observer: Observer, keysym: int) -> list:
    """Press and release one keysym through the handler; the events received."""
    observer.drain(0.02)
    await handler.send_x11_keypress(keysym, down=True)
    await handler.send_x11_keypress(keysym, down=False)
    handler.xdisplay.sync()
    return observer.drain(0.15)


def pressed(events: list, keysym: int) -> bool:
    return any(p and ks == keysym for p, _kc, _g, ks in events)


def released(events: list, keysym: int) -> bool:
    return any(not p and ks == keysym for p, _kc, _g, ks in events)


def set_layout(display_name: str, layout: str) -> None:
    subprocess.run(["setxkbmap", "-display", display_name, layout], check=True, timeout=20)
    time.sleep(0.2)


def pump_keymap_events(handler) -> int:
    """Feed queued X events to the handler's keymap dispatch, as its watch
    loop would; the number of keymap events handled."""
    handler.xdisplay.sync()
    handled = 0
    while handler.xdisplay.pending_events():
        if handler._dispatch_keymap_event(handler.xdisplay.next_event()):
            handled += 1
    return handled


async def run(res: "H.Results", display_name: str) -> None:
    from selkies.Xlib import display as xdisplay

    # --- single-group control: the bare fast path and the overlay stay ---
    set_layout(display_name, "us")
    obs = Observer(display_name)
    d = xdisplay.Display(display_name)
    h = make_handler(d)
    res.check("XKB link opens on the input connection", h.keyboard._xkb is not None)
    ev = await tap(h, obs, A)
    res.check("us: a arrives as a on its keycode", pressed(ev, A) and ev and ev[0][1] == 38, ev)
    ev = await tap(h, obs, A_UPPER)
    res.check("us: A arrives shifted", pressed(ev, A_UPPER), ev)
    ev = await tap(h, obs, CYRILLIC_EF)
    res.check("us: Cyrillic_ef overlay-binds to its Unicode keysym",
              pressed(ev, unicode_alias(CYRILLIC_EF)), ev)
    res.check("us: no group lock taken", obs.locked_group() == 0 and h.keyboard._group_hold is None)
    d.close()
    obs.close()

    # --- German control: AltGr levels, not groups ---
    set_layout(display_name, "de")
    obs = Observer(display_name)
    d = xdisplay.Display(display_name)
    h = make_handler(d)
    for ks, label in ((A, "a"), (ADIAERESIS, "adiaeresis"), (ADIAERESIS_UPPER, "Adiaeresis"),
                      (AT, "at (AltGr+Q)"), (AE, "ae (AltGr+A)")):
        ev = await tap(h, obs, ks)
        res.check(f"de: {label} arrives", pressed(ev, ks), ev)
    res.check("de: no group lock taken", obs.locked_group() == 0 and h.keyboard._group_hold is None)
    d.close()
    obs.close()

    # --- us,ru: Cyrillic keysyms via a group lock on their physical keycode ---
    set_layout(display_name, "us,ru")
    obs = Observer(display_name)
    d = xdisplay.Display(display_name)
    h = make_handler(d)
    res.check("us,ru: two groups in the XKB map", h.keyboard._xkb.locate(CYRILLIC_EF) == (38, 1, 0),
              h.keyboard._xkb.locate(CYRILLIC_EF))
    ev = await tap(h, obs, A)
    res.check("us,ru: a still bare on keycode 38", pressed(ev, A) and ev and ev[0][1] == 38 and ev[0][2] == 0, ev)
    ev = await tap(h, obs, CYRILLIC_EF)
    res.check("us,ru: Cyrillic_ef arrives on keycode 38 under group 2",
              pressed(ev, CYRILLIC_EF) and ev[0][1] == 38 and ev[0][2] == 1, ev)
    res.check("us,ru: its release carries the same keysym", released(ev, CYRILLIC_EF), ev)
    for ks, label in ((CYRILLIC_EF_UPPER, "Cyrillic_EF (shifted)"), (CYRILLIC_BE, "Cyrillic_be"),
                      (CYRILLIC_BE_UPPER, "Cyrillic_BE"), (NUMEROSIGN, "numerosign (Shift+3)")):
        ev = await tap(h, obs, ks)
        res.check(f"us,ru: {label} arrives", pressed(ev, ks), ev)
    ev = await tap(h, obs, COMMA)
    res.check("us,ru: comma typed during the lingering lock switches back to group 1",
              pressed(ev, COMMA) and ev[0][2] == 0, ev)
    ev = await tap(h, obs, PERIOD)
    res.check("us,ru: period arrives", pressed(ev, PERIOD), ev)
    await asyncio.sleep(h.keyboard._GROUP_LINGER_S + 0.3)
    res.check("us,ru: the lock is restored once the linger elapses",
              obs.locked_group() == 0 and h.keyboard._group_hold is None, obs.locked_group())

    # Rollover: a second Cyrillic key under the same hold, then a Latin key on
    # another keycode while the first is still down; each keeps its glyph.
    obs.drain(0.02)
    await h.send_x11_keypress(CYRILLIC_EF, down=True)
    await h.send_x11_keypress(CYRILLIC_YERU, down=True)
    await h.send_x11_keypress(CYRILLIC_YERU, down=False)
    await h.send_x11_keypress(Q, down=True)
    await h.send_x11_keypress(Q, down=False)
    await h.send_x11_keypress(CYRILLIC_EF, down=False)
    d.sync()
    ev = obs.drain(0.15)
    res.check("us,ru: Cyrillic_yeru rolled over under the held Cyrillic_ef arrives",
              pressed(ev, CYRILLIC_EF) and pressed(ev, CYRILLIC_YERU), ev)
    res.check("us,ru: q pressed while Cyrillic_ef is held arrives as q",
              pressed(ev, Q) and released(ev, Q), ev)
    res.check("us,ru: the held Cyrillic_ef is released as itself", released(ev, CYRILLIC_EF), ev)
    await asyncio.sleep(h.keyboard._GROUP_LINGER_S + 0.3)
    res.check("us,ru: lock restored after the chord", obs.locked_group() == 0, obs.locked_group())

    # A typed run (clipboard re-type, soft input) switches once for the whole
    # word, not once per character.
    locks = []
    real_lock = h.keyboard._xkb.lock_group
    h.keyboard._xkb.lock_group = lambda g: (locks.append(g), real_lock(g))
    obs.drain(0.02)
    ok = h._type_text_xtest("привет")
    d.sync()
    ev = obs.drain(0.15)
    h.keyboard._xkb.lock_group = real_lock
    typed = [ks for p, _kc, _g, ks in ev if p]
    res.check("us,ru: a typed Cyrillic word arrives in full",
              ok and typed == [0x6D0, 0x6D2, 0x6C9, 0x6D7, 0x6C5, 0x6D4], [hex(k) for k in typed])
    res.check("us,ru: the word costs a single group switch", locks == [1], locks)
    await asyncio.sleep(h.keyboard._GROUP_LINGER_S + 0.3)
    res.check("us,ru: lock restored after the word", obs.locked_group() == 0, obs.locked_group())

    # The user's own lock on the Cyrillic group is honoured and left alone.
    obs.lock_group(1)
    ev = await tap(h, obs, CYRILLIC_EF)
    res.check("us,ru: Cyrillic_ef under the user's own group-2 lock", pressed(ev, CYRILLIC_EF), ev)
    res.check("us,ru: no hold taken when the lock already matches", h.keyboard._group_hold is None)
    await asyncio.sleep(h.keyboard._GROUP_LINGER_S + 0.3)
    res.check("us,ru: the user's lock survives", obs.locked_group() == 1, obs.locked_group())
    obs.lock_group(0)

    # A keyboard reset (client blur, departure) releases the held Cyrillic key
    # and puts the lock back at once, without waiting out the linger.
    obs.drain(0.02)
    await h.send_x11_keypress(CYRILLIC_EF, down=True)
    h.pressed_keys[CYRILLIC_EF] = time.monotonic()
    await h.reset_keyboard()
    d.sync()
    ev = obs.drain(0.15)
    res.check("us,ru: reset releases the held Cyrillic_ef as itself",
              pressed(ev, CYRILLIC_EF) and released(ev, CYRILLIC_EF), ev)
    res.check("us,ru: reset restores the lock at once",
              obs.locked_group() == 0 and h.keyboard._group_hold is None, obs.locked_group())

    # --- a layout switch under a live handler is seen through XKB ---
    pump_keymap_events(h)
    set_layout(display_name, "us")
    handled = pump_keymap_events(h)
    res.check("keyboard replacement reaches the handler as a keymap change", handled >= 1, handled)
    ev = await tap(h, obs, CYRILLIC_EF)
    res.check("after switching to us, Cyrillic_ef overlay-binds instead of typing a",
              pressed(ev, unicode_alias(CYRILLIC_EF)) and not pressed(ev, A), ev)
    set_layout(display_name, "us,ru")
    pump_keymap_events(h)
    ev = await tap(h, obs, CYRILLIC_EF)
    res.check("after switching back to us,ru, Cyrillic_ef is its keycode-38 group-2 key again",
              pressed(ev, CYRILLIC_EF) and ev[0][1] == 38, ev)
    await asyncio.sleep(h.keyboard._GROUP_LINGER_S + 0.3)
    res.check("lock restored at the end", obs.locked_group() == 0, obs.locked_group())
    d.close()
    obs.close()


def main() -> bool:
    if not shutil.which("setxkbmap"):
        H.skip_suite("setxkbmap is not installed")
    try:
        ctypes.CDLL("libX11.so.6")
    except OSError:
        H.skip_suite("libX11 is not installed")
    res = H.Results("x11-multigroup")
    server, display_name = H.private_x_server(1280, 720)
    try:
        asyncio.run(run(res, display_name))
    finally:
        H.stop_x_server(server, display_name)
    return res.summary()


if __name__ == "__main__":
    sys.exit(0 if main() else 1)
