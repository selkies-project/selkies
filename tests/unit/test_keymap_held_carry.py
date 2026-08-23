#!/usr/bin/env python3
"""Held keys survive a Wayland base-layout change.

The keymap owner resolves keysyms against the seat's base keymap, so a new
base (client layout hint, deployment layout restore) means a rebuilt owner.
What the old owner left down are physical keycodes the compositor still has
pressed, valid under any keymap: the rebuilt owner must carry them, so the
later release — a client 'ku' or a reset — lifts the keycode that went down,
not the keycode the keysym resolves to now (nor nothing at all). X11 keeps
held keys across invalidate_mapping; this is the Wayland half of that parity.
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


def check(label: str, ok, detail="") -> None:
    results.append((label, bool(ok)))
    print(f"{'PASS' if ok else 'FAIL'}  [keymap-held-carry] {label}  {detail}", flush=True)


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
    """Compositor keymap + key channel: serves whichever base text is current
    and records every injected (keycode, state)."""

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


def make_handler(seat: FakeSeat) -> WebRTCInput:
    h = WebRTCInput.__new__(WebRTCInput)
    h.is_wayland = True
    h.wayland_input = seat
    h._wl_keymap_owner = None
    h._wl_keymap_stale = False
    h._wl_keymap_retry_at = 0.0
    h._wl_keymap_owner_lock = asyncio.Lock()
    h.active_modifiers = set()
    h.active_shortcut_modifiers = set()
    h.atomically_typed_keys = set()
    h.translated_keys = set()
    h.pressed_keys = {}
    h.reaped_atomic_keys = set()
    h.MODIFIER_KEYSYMS = {0xFFE1, 0xFFE2, 0xFFE3, 0xFFE4, 0xFFE9, 0xFFEA, 0xFE03}
    h.ACTION_MODIFIER_KEYSYMS = {0xFFE3, 0xFFE4, 0xFFE9, 0xFFEA}
    h.LEVEL_MODIFIER_KEYSYMS = frozenset({0xFFE1, 0xFFE2, 0xFE03})
    h.keyboard_worker_task = None
    return h


async def main() -> None:
    de, us = keymap_text("de"), keymap_text("us")
    if ih.libxkb is None or not de or not us:
        print("SKIP libxkbcommon or the xkb data files are unavailable", flush=True)
        sys.exit(SKIP_EXIT)

    Z = ord("z")
    seat = FakeSeat(de)
    h = make_handler(seat)
    owner = await h._ensure_wayland_keymap_owner()
    check("owner built from the de base", owner is not None)
    kc_de = owner._map[Z][0]
    await h.send_x11_keypress(Z, down=True)
    h.pressed_keys[Z] = 0.0
    check("z pressed at its de keycode", seat.injected[-1] == (kc_de, 1), str(seat.injected))

    # The seat's base changes (client hint / deployment restore) while z is held.
    seat.text = us
    h._invalidate_wayland_keymap_owner()
    rebuilt = await h._ensure_wayland_keymap_owner()
    kc_us = rebuilt._map[Z][0]
    check("owner rebuilt from the us base", rebuilt is not owner and rebuilt is not None)
    check("the two layouts place z on different keycodes (the test means something)",
          kc_us != kc_de, f"de={kc_de} us={kc_us}")
    check("rebuilt owner carries the held key", Z in rebuilt._pressed
          and rebuilt._pressed[Z][0] == kc_de and kc_de in rebuilt._down)

    # A client 'ku' lifts the keycode that went down, not the one z resolves to now.
    await h.send_x11_keypress(Z, down=False)
    check("release lifts the keycode that was pressed",
          seat.injected[-1] == (kc_de, 0), str(seat.injected[-2:]))
    check("nothing injected at the new keycode",
          all(kc != kc_us for kc, _ in seat.injected), str(seat.injected))
    check("the release untracks the key", Z not in rebuilt._pressed and kc_de not in rebuilt._down)

    # A shifted press synthesizes Shift; the refcount rides along so the release
    # after the rebuild lifts Shift too.
    seat.text = de
    h._invalidate_wayland_keymap_owner()
    owner = await h._ensure_wayland_keymap_owner()
    seat.injected.clear()
    await h.send_x11_keypress(ord("Z"), down=True)
    synth = [kc for kc, st in seat.injected if st and kc != owner._map[ord("Z")][0]]
    check("shifted press synthesizes a modifier", len(synth) == 1, str(seat.injected))
    seat.text = us
    h._invalidate_wayland_keymap_owner()
    rebuilt = await h._ensure_wayland_keymap_owner()
    seat.injected.clear()
    await h.send_x11_keypress(ord("Z"), down=False)
    check("release after the rebuild lifts the synthesized modifier as well",
          (synth[0], 0) in seat.injected, str(seat.injected))

    # A reset after the change releases the held key through the rebuilt owner.
    seat.text = de
    h._invalidate_wayland_keymap_owner()
    owner = await h._ensure_wayland_keymap_owner()
    await h.send_x11_keypress(Z, down=True)
    h.pressed_keys[Z] = 0.0
    seat.text = us
    h._invalidate_wayland_keymap_owner()
    seat.injected.clear()
    await h.reset_keyboard()
    check("reset after a layout change lifts the held keycode",
          (kc_de, 0) in seat.injected and not h.pressed_keys, str(seat.injected))
    check("the seat has nothing left pressed", seat.get_keyboard_state()[0] == [],
          str(seat.get_keyboard_state()))

    # Overlay binds are not carried: the compositor rebuilt its keymap without them.
    seat.text = de
    h._invalidate_wayland_keymap_owner()
    owner = await h._ensure_wayland_keymap_owner()
    owner._overlay_bind(0x01000000 | 0x1F600)
    seat.text = us
    h._invalidate_wayland_keymap_owner()
    rebuilt = await h._ensure_wayland_keymap_owner()
    check("overlay binds start empty on the rebuilt owner", rebuilt._overlay == {})


asyncio.run(main())
failed = [r for r in results if not r[1]]
print(f"\n{len(results) - len(failed)}/{len(results)} passed")
sys.exit(1 if failed else 0)
