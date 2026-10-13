#!/usr/bin/env python3
"""A held key whose heartbeats stop is let go at the server, so a stalled release types once.

The X server repeats a held key itself, an XTEST press included (660 ms, 25 Hz), and so does a
Wayland app (pixelflux's seat says 600 ms), so a press whose release a stalled uplink holds back
for a second would type a run of the key. The repeat loop lets such a key go once its heartbeats
are 0.3 s stale, before any repeat is due, and presses it again a tick after two arrive within
that grace, unless the release the stall held back arrives with them; a hidden tab's heartbeat, one
a second, leaves it up. A key whose heartbeats keep coming is never let go. Checked on both
backends against the presses and releases the handler injects.
"""
import asyncio
import os
import sys
import time

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(REPO, "src"))

from selkies import input_handler as ih  # noqa: E402
from selkies.input_handler import WebRTCInput  # noqa: E402

KEY_E, KEYCODE_E = ord("e"), 26
FIRST_REPEAT_S = 0.6
results = []


def check(label: str, ok: bool, detail: str = "") -> None:
    results.append((label, ok))
    print(f"{'PASS' if ok else 'FAIL'}  [key-stall-release] {label}  {detail}", flush=True)


class FakeXDisplay:
    """The keymap queries the XTEST shim makes: 'e' alone."""

    def keysym_to_keycode(self, keysym: int) -> int:
        return KEYCODE_E if keysym == KEY_E else 0

    def keycode_to_keysym(self, kc: int, level: int) -> int:
        return KEY_E if kc == KEYCODE_E and level == 0 else 0

    def get_modifier_mapping(self) -> list:
        return [[0, 0]] * 8

    def flush(self) -> None:
        pass


class FakeXtest:
    """Records every fake_input as (seconds since the start, down)."""

    def __init__(self) -> None:
        self.start = time.monotonic()
        self.injected: list = []

    def fake_input(self, display, kind: int, keycode: int) -> None:
        self.injected.append((time.monotonic() - self.start, kind == ih.X.KeyPress))


def make_handler(is_wayland: bool) -> WebRTCInput:
    h = WebRTCInput.__new__(WebRTCInput)
    h.is_wayland = is_wayland
    h.wayland_input = None
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
    h.key_suspended = {}
    h.key_repeat_tick = 0.02
    h.key_repeat_heartbeat_grace = 0.3
    h.MODIFIER_KEYSYMS = {0xFFE1, 0xFFE2, 0xFFE3, 0xFFE4, 0xFFE9, 0xFFEA, 0xFE03}
    h.ACTION_MODIFIER_KEYSYMS = {0xFFE3, 0xFFE4, 0xFFE9, 0xFFEA}
    h.LEVEL_MODIFIER_KEYSYMS = frozenset({0xFFE1, 0xFFE2, 0xFE03})
    h.SHORTCUT_MODIFIER_XKEY_NAMES = {"Control_L", "Control_R", "Alt_L", "Alt_R", "Super_L", "Super_R"}
    h.keyboard_worker_task = None
    return h


async def scenario(h: WebRTCInput, steps: list, injected) -> list:
    """Deliver `steps`, (seconds, message), on the clock; returns what was injected."""
    loop = asyncio.create_task(h._key_repeat_loop())
    start = time.monotonic()
    try:
        for at, msg in steps:
            await asyncio.sleep(max(0.0, start + at - time.monotonic()))
            await h._dispatch_message(msg)
        await asyncio.sleep(0.2)
    finally:
        loop.cancel()
    return injected()


async def backend(name: str, h: WebRTCInput, injected, reset) -> None:
    kd, ku, kh = f"kd,{KEY_E}", f"ku,{KEY_E}", f"kh,{KEY_E}"
    heartbeats = [(0.1 * i, kh) for i in range(1, 11)]

    reset()
    got = await scenario(h, [(0.0, kd), (1.0, ku)], injected)
    check(f"{name}: a release a stall holds back is let go before the first repeat",
          [down for _, down in got] == [True, False] and got[1][0] < FIRST_REPEAT_S, str(got))

    reset()
    got = await scenario(h, [(0.0, kd)] + heartbeats + [(1.05, ku)], injected)
    check(f"{name}: a key whose heartbeats keep coming is let go by its release alone",
          [down for _, down in got] == [True, False] and got[1][0] > 1.0, str(got))

    reset()
    got = await scenario(h, [(0.0, kd), (1.0, kh), (1.0, kh), (1.0, ku)], injected)
    check(f"{name}: a heartbeat arriving with the held-back release presses nothing again",
          [down for _, down in got] == [True, False] and got[1][0] < FIRST_REPEAT_S, str(got))

    reset()
    got = await scenario(h, [(0.0, kd), (1.0, kh), (1.1, kh), (1.2, kh), (1.3, ku)], injected)
    check(f"{name}: a heartbeat after a stall presses the held key again until its release",
          [down for _, down in got] == [True, False, True, False], str(got))

    reset()
    got = await scenario(h, [(0.0, kd), (1.0, kh), (2.0, kh), (3.0, kh), (3.5, ku)], injected)
    check(f"{name}: a hidden tab's heartbeat, one a second, leaves the key up",
          [down for _, down in got] == [True, False], str(got))


async def main() -> None:
    fake_xtest = FakeXtest()
    saved = ih.xtest, ih.open_xkb_link
    ih.xtest, ih.open_xkb_link = fake_xtest, None
    try:
        h = make_handler(False)
        h.xdisplay = FakeXDisplay()
        h.keyboard = ih._XTestKeyboard(h.xdisplay)

        def reset() -> None:
            fake_xtest.injected.clear()
            fake_xtest.start = time.monotonic()
        await backend("x11", h, lambda: list(fake_xtest.injected), reset)
    finally:
        ih.xtest, ih.open_xkb_link = saved

    h = make_handler(True)
    queued: list = []
    start = [time.monotonic()]
    h._keyboard_enqueue = lambda item: queued.append((time.monotonic() - start[0], item[0] == "kd"))

    def reset_wl() -> None:
        queued.clear()
        start[0] = time.monotonic()
    await backend("wayland", h, lambda: list(queued), reset_wl)


asyncio.run(main())
failed = [r for r in results if not r[1]]
print(f"\n{len(results) - len(failed)}/{len(results)} passed")
sys.exit(1 if failed else 0)
