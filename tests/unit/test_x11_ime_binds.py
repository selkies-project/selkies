#!/usr/bin/env python3
"""IME text binds on X11 cost the event loop nothing but the binds.

A character the layout lacks is bound to a spare keycode, and every client
refetches the keymap after each bind. The handler must not add to that: the
settle a bind owes clients is awaited before the press, never slept out on the
event loop, for the printable letters a composition types as for any other
keysym; and telling its own MappingNotify from a foreign change reads the
keymap cache the notify refreshed, for the bindings in the notify's range only,
since a binding outside it is one whose own notify is still queued.
"""
import asyncio
import functools
import os
import sys
import time
from types import SimpleNamespace

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(TESTS), "src"))

from selkies import input_handler as ih  # noqa: E402

results = []
HANGUL = [0x100D55C, 0x100AD6D, 0x100C5B4]  # 한 국 어


def check(label: str, ok, detail="") -> None:
    results.append((label, bool(ok)))
    print(f"{'PASS' if ok else 'FAIL'}  [x11-ime-binds] {label}  {detail}", flush=True)


class FakeXDisplay:
    """A core keymap from 8 to 255, Latin letters on 38-63, counting the keymap requests made."""

    def __init__(self) -> None:
        self.map = {kc: ([0x61 + kc - 38] if 38 <= kc < 64 else []) for kc in range(8, 256)}
        self.display = SimpleNamespace(info=SimpleNamespace(min_keycode=8, max_keycode=255))
        self.requests = 0

    def get_keyboard_mapping(self, first: int, count: int) -> list:
        self.requests += 1
        return [(self.map[kc] + [0, 0, 0, 0])[:4] for kc in range(first, first + count)]

    def get_modifier_mapping(self) -> list:
        return [[50, 62], [66, 0], [37, 105], [64, 108], [77, 0], [0, 0], [133, 134], [92, 0]]

    def keysym_to_keycode(self, keysym: int) -> int:
        return next((kc for kc, syms in self.map.items() if keysym in syms), 0)

    def keycode_to_keysym(self, keycode: int, level: int) -> int:
        return (self.map.get(keycode, []) + [0, 0, 0, 0])[level]

    def change_keyboard_mapping(self, first: int, rows: list) -> None:
        for i, row in enumerate(rows):
            self.map[first + i] = list(row)

    def sync(self) -> None:
        pass

    def flush(self) -> None:
        pass


class FakeXtest:
    def __init__(self, display: FakeXDisplay) -> None:
        self.display = display
        self.typed = []

    def fake_input(self, display, kind: int, keycode: int) -> None:
        if kind == ih.X.KeyPress:
            self.typed.append(self.display.map[keycode][0])


def keyboard() -> "ih._XTestKeyboard":
    open_xkb, ih.open_xkb_link = ih.open_xkb_link, None
    try:
        return ih._XTestKeyboard(FakeXDisplay())
    finally:
        ih.open_xkb_link = open_xkb


def notify_checks() -> None:
    kb = keyboard()
    first = kb._overlay_keycode(HANGUL[0])
    second = kb._overlay_keycode(HANGUL[1])
    d = kb._d
    d.requests = 0
    check("its own notify reads as intact", kb.bindings_intact(first, 1), first)
    check("with no keymap request", d.requests == 0, d.requests)
    stale = dict(d.map)
    d.map[second] = []
    check("a binding outside the notify's range is left to its own notify", kb.bindings_intact(first, 1))
    d.map = stale
    d.map[first] = [0x61]
    check("a foreign change in range reads as one", not kb.bindings_intact(8, 248))


async def press_keeps_loop_free() -> None:
    kb = keyboard()
    fake, saved = FakeXtest(kb._d), ih.xtest
    ih.xtest = fake
    handler = SimpleNamespace(
        active_modifiers=set(), translated_keys=set(), active_shortcut_modifiers=set(),
        ACTION_MODIFIER_KEYSYMS={0xFFE3, 0xFFE4, 0xFFE9, 0xFFEA, 0xFFE7, 0xFFE8, 0xFFEB, 0xFFEC, 0xFFED, 0xFFEE},
        MODIFIER_KEYSYMS={0xFFE1, 0xFFE2, 0xFFE3, 0xFFE4, 0xFFE9, 0xFFEA, 0xFE03},
        LEVEL_MODIFIER_KEYSYMS=frozenset({0xFFE1, 0xFFE2, 0xFE03, 0xFF7E}),
        is_wayland=False, wayland_input=None, keyboard=kb, xdisplay=kb._d, _virtual_key=lambda keysym, down: None,
        _is_x_conn_closed=lambda e: False, _reconnect_xdisplay=lambda: None)

    async def fallback(keysym, down):
        raise AssertionError("fell back")
    handler._type_keysym_fallback = fallback
    handler._xtest_press = functools.partial(ih.WebRTCInput._xtest_press, handler)
    ticks = []

    async def ticker():
        while True:
            ticks.append(time.monotonic())
            await asyncio.sleep(0.002)
    task = asyncio.create_task(ticker())
    await asyncio.sleep(0.01)
    try:
        start = time.monotonic()
        for ks in HANGUL:
            await ih.WebRTCInput.send_x11_keypress(handler, ks, True)
            await ih.WebRTCInput.send_x11_keypress(handler, ks, False)
        took = time.monotonic() - start
    finally:
        task.cancel()
        ih.xtest = saved
    points = [start, *[t for t in ticks if t >= start], start + took]
    gap = max(b - a for a, b in zip(points, points[1:])) * 1000
    check("three unbound letters are typed whole", fake.typed == HANGUL, [hex(k) for k in fake.typed])
    check("each waits out its bind's settle", took >= 3 * ih._XTestKeyboard._BIND_SETTLE_S * 0.9, f"{took * 1000:.0f} ms")
    check("with the event loop free throughout", gap < 15, f"longest loop gap {gap:.1f} ms")


notify_checks()
asyncio.run(press_keeps_loop_free())
passed = sum(ok for _, ok in results)
print(f"[x11-ime-binds] {passed}/{len(results)} passed")
sys.exit(0 if passed == len(results) else 1)
