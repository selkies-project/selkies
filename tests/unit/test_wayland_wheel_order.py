#!/usr/bin/env python3
"""A wheel click sent right behind a key lands behind it on Wayland.

Keys reach the Wayland seat through the keyboard worker's queue, pointer events
straight from the message that carries them, so a key and a wheel click
arriving back to back -- the Control a client wraps around a pinch it sends as
Ctrl+wheel -- would put the click on the seat before the key it follows, and
the pinch would scroll instead of zooming. The checks deliver the wire messages
back to back, the way a socket that has several buffered hands them over, to a
handler whose seat records every key and axis event in one sequence.
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
CONTROL_L = 0xFFE3
results = []


def check(label: str, ok, detail="") -> None:
    results.append((label, bool(ok)))
    print(f"{'PASS' if ok else 'FAIL'}  [wayland-wheel-order] {label}  {detail}", flush=True)


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
    """Compositor keymap and seat, recording key and axis events in one sequence."""

    def __init__(self, text: str) -> None:
        self.text = text
        self.events: list = []
        self.down: set = set()

    def get_xkb_keymap_string(self) -> str:
        return self.text

    def set_keymap_string(self, text: str) -> None:
        pass

    def set_keymap_overlay(self, binds) -> None:
        pass

    def inject_key(self, kc: int, state: int) -> None:
        self.events.append(("key", kc, state))
        (self.down.add if state else self.down.discard)(kc)

    def inject_keys(self, events) -> None:
        for kc, state in events:
            self.inject_key(kc, state)

    def get_keyboard_state(self):
        return sorted(self.down), 0

    def inject_mouse_scroll(self, dx: float, dy: float) -> None:
        self.events.append(("axis", dx, dy))

    def inject_mouse_button(self, button: int, state: int) -> None:
        self.events.append(("button", button, state))

    def inject_mouse_move(self, x: float, y: float) -> None:
        pass

    def inject_relative_mouse_move(self, dx: float, dy: float) -> None:
        pass


def make_handler(seat: FakeSeat) -> WebRTCInput:
    h = WebRTCInput.__new__(WebRTCInput)
    h.is_wayland = True
    h.wayland_input = seat
    h._wl_keymap_owner = None
    h._wl_keymap_stale = False
    h._wl_keymap_retry_at = 0.0
    h._wl_keymap_owner_lock = asyncio.Lock()
    h._wl_text_routed = {}
    h._has_separate_app_compositor = lambda: False
    h.keyboard_queue = asyncio.Queue()
    h._keyboard_busy = False
    h.active_modifiers = set()
    h.active_shortcut_modifiers = set()
    h.atomically_typed_keys = set()
    h.translated_keys = set()
    h.pressed_keys = {}
    h.max_pressed_keys = 1024
    h.reaped_atomic_keys = set()
    h.key_repeat_enabled = False
    h.key_repeat_state = {}
    h.MODIFIER_KEYSYMS = {0xFFE1, 0xFFE2, CONTROL_L, 0xFFE4, 0xFFE9, 0xFFEA, 0xFE03}
    h.ACTION_MODIFIER_KEYSYMS = {CONTROL_L, 0xFFE4, 0xFFE9, 0xFFEA}
    h.LEVEL_MODIFIER_KEYSYMS = frozenset({0xFFE1, 0xFFE2, 0xFE03})
    h.SHORTCUT_MODIFIER_XKEY_NAMES = {"Control_L", "Control_R", "Alt_L", "Alt_R", "Super_L", "Super_R"}
    h.keyboard_worker_task = None
    h._pointer_seq = {}
    h._pointer_echoes = {}
    h.button_mask = 0
    h.last_x = 0
    h.last_y = 0
    h.tracked_position_stale = False
    h.data_server_instance = None
    return h


async def main() -> None:
    us = keymap_text("us")
    if ih.libxkb is None or not us:
        print("SKIP libxkbcommon or the xkb data files are unavailable", flush=True)
        sys.exit(SKIP_EXIT)
    seat = FakeSeat(us)
    h = make_handler(seat)
    owner = await h._ensure_wayland_keymap_owner()
    ctrl = owner._map[CONTROL_L][0]
    worker = asyncio.create_task(h._keyboard_worker())
    try:
        # A pinch as a client sends it: Control, one wheel notch up (a rising
        # edge of bit 4 between two baselines), Control up; back to back.
        for msg in (f"kd,{CONTROL_L}", "m2,0,0,0,1,1", "m2,0,0,16,1,2", "m2,0,0,0,1,3",
                    f"ku,{CONTROL_L}"):
            await h._dispatch_message(msg, "primary", "conn")
        await h.keyboard_queue.join()
        ev = seat.events
        down = ev.index(("key", ctrl, 1)) if ("key", ctrl, 1) in ev else None
        up = ev.index(("key", ctrl, 0)) if ("key", ctrl, 0) in ev else None
        axis = next((i for i, e in enumerate(ev) if e[0] == "axis"), None)
        check("the wheel click reaches the seat", axis is not None, str(ev))
        check("inside the Control it was sent inside",
              None not in (down, up, axis) and down < axis < up, str(ev))

        # With nothing queued the click goes straight to the seat.
        seat.events.clear()
        await h._dispatch_message("m2,0,0,0,1,4", "primary", "conn")
        await h._dispatch_message("m2,0,0,8,1,5", "primary", "conn")
        check("a wheel click with no key waiting is injected as it arrives",
              seat.events == [("axis", 0.0, 10.0)], str(seat.events))
        await h._dispatch_message("m2,0,0,0,1,6", "primary", "conn")
    finally:
        worker.cancel()
        try:
            await worker
        except asyncio.CancelledError:
            pass


asyncio.run(main())
failed = [r for r in results if not r[1]]
print(f"\n{len(results) - len(failed)}/{len(results)} passed")
sys.exit(1 if failed else 0)
