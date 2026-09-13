#!/usr/bin/env python3
"""A held Shift stays down under a navigation key on both backends.

The injectors lift a held Shift/AltGr around a keysym whose keymap level does
not want it, so a client layout's Shift pairing cannot move a glyph onto a
different one. Left, Home, End and the rest of the function block sit at
level 0 too, and lifting Shift around them turns Shift+Home into a bare Home:
the selection the user was extending never happens, while Ctrl+Shift+Home, a
chord the injectors never neutralize, still works. The checks send the wire
messages a browser sends through the real injectors -- the Wayland keymap
owner over a fake seat and the X11 XTEST shim over a fake display -- and
require the Shift keycode to stay down across every navigation press, while a
glyph under a held Shift is still neutralized and a chord still passes.
"""
import asyncio
import ctypes
import os
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))

from selkies import input_handler as ih  # noqa: E402
from selkies.input_handler import WebRTCInput  # noqa: E402

SKIP_EXIT = 77
results = []

SHIFT_L, SHIFT_R, CONTROL_L, ALTGR = 0xFFE1, 0xFFE2, 0xFFE3, 0xFE03
NAVIGATION = {
    "Left": 0xFF51, "Right": 0xFF53, "Up": 0xFF52, "Down": 0xFF54,
    "Home": 0xFF50, "End": 0xFF57, "Page_Up": 0xFF55, "Page_Down": 0xFF56,
    "Tab": 0xFF09, "Insert": 0xFF63, "Delete": 0xFFFF, "F3": 0xFFC0,
}
LETTER_A = ord("a")


def check(label: str, ok, detail="") -> None:
    results.append((label, bool(ok)))
    print(f"{'PASS' if ok else 'FAIL'}  [shift-navigation] {label}  {detail}", flush=True)


class RuleNames(ctypes.Structure):
    _fields_ = [("rules", ctypes.c_char_p), ("model", ctypes.c_char_p),
                ("layout", ctypes.c_char_p), ("variant", ctypes.c_char_p),
                ("options", ctypes.c_char_p)]


def keymap_text(layout: str) -> str:
    """The XKB_KEYMAP_FORMAT_TEXT_V1 text of `layout` from libxkbcommon, or ''."""
    try:
        lib = ctypes.CDLL("libxkbcommon.so.0")
    except OSError:
        return ""
    lib.xkb_context_new.restype = ctypes.c_void_p
    lib.xkb_context_new.argtypes = [ctypes.c_int]
    lib.xkb_keymap_new_from_names.restype = ctypes.c_void_p
    lib.xkb_keymap_new_from_names.argtypes = [ctypes.c_void_p, ctypes.POINTER(RuleNames), ctypes.c_int]
    lib.xkb_keymap_get_as_string.restype = ctypes.c_void_p
    lib.xkb_keymap_get_as_string.argtypes = [ctypes.c_void_p, ctypes.c_int]
    lib.xkb_keymap_unref.argtypes = [ctypes.c_void_p]
    lib.xkb_context_unref.argtypes = [ctypes.c_void_p]
    ctx = lib.xkb_context_new(0)
    names = RuleNames(b"evdev", b"pc105", layout.encode(), b"", b"")
    km = lib.xkb_keymap_new_from_names(ctx, ctypes.byref(names), 0)
    if not km:
        lib.xkb_context_unref(ctx)
        return ""
    ptr = lib.xkb_keymap_get_as_string(km, 1)
    text = ctypes.string_at(ptr).decode() if ptr else ""
    if ptr:
        ctypes.CDLL(None).free(ctypes.c_void_p(ptr))
    lib.xkb_keymap_unref(km)
    lib.xkb_context_unref(ctx)
    return text


class FakeSeat:
    """Compositor keymap plus key channel, recording every injected (keycode, state)."""

    def __init__(self, text: str) -> None:
        self.text = text
        self.injected: list = []
        self.down: set = set()

    def get_xkb_keymap_string(self) -> str:
        return self.text

    def set_keymap_string(self, text: str) -> None:
        pass

    def set_keymap_overlay(self, binds) -> None:
        pass

    def inject_key(self, kc: int, state: int) -> None:
        self.injected.append((kc, state))
        (self.down.add if state else self.down.discard)(kc)

    def inject_keys(self, events) -> None:
        for kc, state in events:
            self.inject_key(kc, state)

    def get_keyboard_state(self):
        return sorted(self.down), 0


# A pc105 us core keymap slice: keycode to its keysyms per level.
X_KEYMAP = {
    37: [CONTROL_L], 50: [SHIFT_L], 62: [SHIFT_R], 108: [ALTGR],
    38: [LETTER_A, ord("A")],
    23: [NAVIGATION["Tab"]], 110: [NAVIGATION["Home"]], 111: [NAVIGATION["Up"]],
    112: [NAVIGATION["Page_Up"]], 113: [NAVIGATION["Left"]], 114: [NAVIGATION["Right"]],
    115: [NAVIGATION["End"]], 116: [NAVIGATION["Down"]], 117: [NAVIGATION["Page_Down"]],
    118: [NAVIGATION["Insert"]], 119: [NAVIGATION["Delete"]], 69: [NAVIGATION["F3"]],
}


class FakeXDisplay:
    """The keymap queries the XTEST shim makes, answered from X_KEYMAP."""

    def keysym_to_keycode(self, keysym: int) -> int:
        return next((kc for kc, syms in X_KEYMAP.items() if keysym in syms), 0)

    def keycode_to_keysym(self, kc: int, level: int) -> int:
        syms = X_KEYMAP.get(kc, [])
        return syms[level] if level < len(syms) else 0

    def get_modifier_mapping(self) -> list:
        return [[0, 0], [50, 62], [37, 0], [0, 0], [0, 0], [0, 0], [0, 0], [108, 0]]

    def flush(self) -> None:
        pass


class FakeXtest:
    """Records every fake_input as (keycode, down)."""

    def __init__(self) -> None:
        self.injected: list = []

    def fake_input(self, display, kind: int, keycode: int) -> None:
        self.injected.append((keycode, 1 if kind == ih.X.KeyPress else 0))


def make_handler(is_wayland: bool, seat=None) -> WebRTCInput:
    h = WebRTCInput.__new__(WebRTCInput)
    h.is_wayland = is_wayland
    h.wayland_input = seat
    h._wl_keymap_owner = None
    h._wl_keymap_stale = False
    h._wl_keymap_retry_at = 0.0
    h._wl_keymap_owner_lock = asyncio.Lock()
    h._wl_text_routed = {}
    h._has_separate_app_compositor = lambda: False
    h.keyboard_queue = asyncio.Queue()
    h.active_modifiers = set()
    h.active_shortcut_modifiers = set()
    h.atomically_typed_keys = set()
    h.translated_keys = set()
    h.pressed_keys = {}
    h.max_pressed_keys = 1024
    h.reaped_atomic_keys = set()
    h.key_repeat_enabled = False
    h.key_repeat_state = {}
    h.MODIFIER_KEYSYMS = {SHIFT_L, SHIFT_R, CONTROL_L, 0xFFE4, 0xFFE9, 0xFFEA, ALTGR}
    h.ACTION_MODIFIER_KEYSYMS = {CONTROL_L, 0xFFE4, 0xFFE9, 0xFFEA}
    h.LEVEL_MODIFIER_KEYSYMS = frozenset({SHIFT_L, SHIFT_R, ALTGR})
    h.SHORTCUT_MODIFIER_XKEY_NAMES = {"Control_L", "Control_R", "Alt_L", "Alt_R", "Super_L", "Super_R"}
    h.keyboard_worker_task = None
    return h


async def send(h: WebRTCInput, *messages: str) -> None:
    """Deliver wire messages the way the transport does, and let a Wayland worker drain them."""
    for msg in messages:
        await h._dispatch_message(msg)
    if h.is_wayland:
        await h.keyboard_queue.join()


def shift_lifted(events: list, shift_kc: int, key_kc: int) -> bool:
    """Whether Shift went up at any point before the key's own press."""
    for kc, state in events:
        if (kc, state) == (key_kc, 1):
            return False
        if kc == shift_kc and state == 0:
            return True
    return False


async def run_backend(name: str, h: WebRTCInput, injected: list, shift_kc: int,
                      ctrl_kc: int, keycode_of) -> None:
    """The three checks on one backend: navigation keeps Shift, a glyph does not, a chord passes."""
    await send(h, f"kd,{SHIFT_L}")
    check(f"{name}: Shift goes down", (shift_kc, 1) in injected, str(injected))
    for label, keysym in NAVIGATION.items():
        injected.clear()
        await send(h, f"kd,{keysym}", f"ku,{keysym}")
        kc = keycode_of(keysym)
        check(f"{name}: Shift+{label} keeps Shift down",
              (kc, 1) in injected and not shift_lifted(injected, shift_kc, kc), str(injected))
    injected.clear()
    await send(h, f"kd,{LETTER_A}", f"ku,{LETTER_A}")
    check(f"{name}: a glyph under a held Shift is still neutralized",
          shift_lifted(injected, shift_kc, keycode_of(LETTER_A)), str(injected))
    await send(h, f"kd,{CONTROL_L}")
    injected.clear()
    left = NAVIGATION["Left"]
    await send(h, f"kd,{left}", f"ku,{left}")
    check(f"{name}: Ctrl+Shift+Left passes through untouched",
          (keycode_of(left), 1) in injected and (shift_kc, 0) not in injected
          and (ctrl_kc, 0) not in injected, str(injected))
    await send(h, f"ku,{CONTROL_L}", f"ku,{SHIFT_L}")
    check(f"{name}: releasing the chord lifts Shift once",
          injected.count((shift_kc, 0)) == 1, str(injected))


async def wayland() -> None:
    us = keymap_text("us")
    if ih.libxkb is None or not us:
        print("SKIP libxkbcommon or the xkb data files are unavailable", flush=True)
        sys.exit(SKIP_EXIT)
    seat = FakeSeat(us)
    h = make_handler(True, seat)
    owner = await h._ensure_wayland_keymap_owner()
    check("wayland: owner built from the us base", owner is not None)
    worker = asyncio.create_task(h._keyboard_worker())
    try:
        await run_backend("wayland", h, seat.injected, owner._map[SHIFT_L][0],
                          owner._map[CONTROL_L][0], lambda ks: owner._map[ks][0])
    finally:
        worker.cancel()
        try:
            await worker
        except asyncio.CancelledError:
            pass


async def x11() -> None:
    fake_xtest = FakeXtest()
    saved = ih.xtest, ih.open_xkb_link
    ih.xtest, ih.open_xkb_link = fake_xtest, None
    try:
        h = make_handler(False)
        h.xdisplay = FakeXDisplay()
        h.keyboard = ih._XTestKeyboard(h.xdisplay)
        await run_backend("x11", h, fake_xtest.injected, 50, 37, h.xdisplay.keysym_to_keycode)
    finally:
        ih.xtest, ih.open_xkb_link = saved


async def main() -> None:
    await wayland()
    await x11()


asyncio.run(main())
failed = [r for r in results if not r[1]]
print(f"\n{len(results) - len(failed)}/{len(results)} passed")
sys.exit(1 if failed else 0)
