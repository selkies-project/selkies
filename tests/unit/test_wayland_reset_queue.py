#!/usr/bin/env python3
"""Wayland keyboard reset discipline.

Every Wayland key goes through one serialized worker, so a server-side reset
(client departure, connect) must queue behind the key work in flight exactly
like a client 'kr' does: a press still queued would otherwise land after the
reset, untracked and held for good. The reset runs in place only when no
worker drains the queue (or the worker went away while the reset waited), a
flood that evicts the queued reset must not strand its waiter, and the caps
readback — a compositor round-trip — must not block the event loop.
"""
import asyncio
import os
import sys
import threading

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))

from selkies.input_handler import WebRTCInput  # noqa: E402

results = []


def check(label: str, ok, detail="") -> None:
    results.append((label, bool(ok)))
    print(f"{'PASS' if ok else 'FAIL'}  [wl-reset-queue] {label}  {detail}", flush=True)


class FakeWaylandInput:
    """A seat with keymap control (so keys take the direct seat path) that
    records which thread answers the keyboard-state round-trip."""

    def __init__(self) -> None:
        self.state_threads: list = []

    def set_keymap_string(self, text: str) -> None:
        pass

    def get_keyboard_state(self):
        self.state_threads.append(threading.get_ident())
        return [], 0


def make_handler(queue_size: int = 4096) -> WebRTCInput:
    """A WebRTCInput with the keyboard worker state and recording injectors."""
    h = WebRTCInput.__new__(WebRTCInput)
    h.is_wayland = True
    h.wayland_input = FakeWaylandInput()
    h.active_modifiers = set()
    h.active_shortcut_modifiers = set()
    h.MODIFIER_KEYSYMS = {0xFFE1, 0xFFE2, 0xFFE3, 0xFFE4, 0xFFE9, 0xFFEA}
    h.ACTION_MODIFIER_KEYSYMS = {0xFFE3, 0xFFE4, 0xFFE9, 0xFFEA}
    h.LEVEL_MODIFIER_KEYSYMS = frozenset({0xFFE1, 0xFFE2})
    h.atomically_typed_keys = set()
    h.translated_keys = set()
    h.pressed_keys = {}
    h.reaped_atomic_keys = set()
    h._wl_text_routed = {}
    h.max_pressed_keys = 1024
    h.keyboard_queue = asyncio.Queue(maxsize=queue_size)
    h.keyboard_worker_task = None
    h._wl_keymap_owner = None
    h._has_separate_app_compositor = lambda: False
    h.events: list = []

    async def send(keysym, down=True):
        # A slow injection, so a reset issued meanwhile has something to wait on.
        await asyncio.sleep(0.05)
        h.events.append(("key", keysym, down))
    h.send_x11_keypress = send

    async def no_owner():
        return None
    h._ensure_wayland_keymap_owner = no_owner

    real_reset = h._reset_keyboard_wayland

    async def reset():
        h.events.append(("reset", asyncio.current_task() is h.keyboard_worker_task))
        await real_reset()
    h._reset_keyboard_wayland = reset
    return h


def start_worker(h: WebRTCInput) -> None:
    h.keyboard_worker_task = asyncio.create_task(h._keyboard_worker())


async def stop_worker(h: WebRTCInput) -> None:
    task = h.keyboard_worker_task
    if task is None:
        return
    task.cancel()
    try:
        await task
    except asyncio.CancelledError:
        pass


async def main() -> None:
    loop_thread = threading.get_ident()

    # --- a server-side reset queues behind the key work in flight ---
    h = make_handler()
    start_worker(h)
    h.pressed_keys[0x77] = 0.0
    for _ in range(3):
        h._keyboard_enqueue(("kd", 0x77))
    await h.reset_keyboard()
    kinds = [e[0] for e in h.events]
    check("reset runs after every queued press",
          kinds[:3] == ["key"] * 3 and "reset" in kinds and kinds.index("reset") == 3,
          str(kinds))
    check("reset runs on the worker task", ("reset", True) in h.events)
    check("reset releases the tracked held key",
          ("key", 0x77, False) in h.events and not h.pressed_keys, str(h.events[-3:]))
    check("caps readback answered off the event loop",
          h.wayland_input.state_threads
          and all(t != loop_thread for t in h.wayland_input.state_threads))

    # --- a client 'kr' still carries no future ---
    h.events.clear()
    h._keyboard_enqueue(("kr", None))
    await asyncio.sleep(0.15)
    check("client kr resets on the worker", ("reset", True) in h.events, str(h.events))
    await stop_worker(h)

    # --- no worker: the reset runs in place ---
    h = make_handler()
    h.events.clear()
    await h.reset_keyboard()
    check("without a worker the reset runs in place",
          h.events == [("reset", False)], str(h.events))

    # --- a worker cancelled mid-wait does not strand the caller ---
    h = make_handler()
    start_worker(h)
    for _ in range(40):
        h._keyboard_enqueue(("kd", 0x61))
    waiter = asyncio.create_task(h.reset_keyboard())
    await asyncio.sleep(0.12)
    await stop_worker(h)
    h.keyboard_worker_task = None
    try:
        await asyncio.wait_for(waiter, 2.0)
        finished = True
    except asyncio.TimeoutError:
        finished = False
    check("reset completes once the worker is gone", finished)
    check("and then runs in place", ("reset", False) in h.events, str([e for e in h.events if e[0] == "reset"]))

    # --- a flood that evicts the queued reset settles its waiter ---
    h = make_handler(queue_size=2)
    evicted = asyncio.get_running_loop().create_future()
    h._keyboard_enqueue(("kr", evicted))
    h._keyboard_enqueue(("kd", 0x61))
    h._keyboard_enqueue(("kd", 0x62))
    check("evicted reset future is settled", evicted.done())

    # --- a reset issued from the worker task itself runs in place ---
    # (wrapped in its own task so current_task() is a real task identity — not
    # via wait_for, which on some versions wraps the coroutine in a fresh task).
    h = make_handler()

    async def reset_as_worker():
        h.keyboard_worker_task = asyncio.current_task()
        await h.reset_keyboard()
    await asyncio.wait_for(asyncio.create_task(reset_as_worker()), 2.0)
    check("a reset issued on the worker task runs in place",
          h.events == [("reset", True)], str(h.events))

asyncio.run(main())
failed = [r for r in results if not r[1]]
print(f"\n{len(results) - len(failed)}/{len(results)} passed")
sys.exit(1 if failed else 0)
