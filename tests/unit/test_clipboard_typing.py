#!/usr/bin/env python3
"""Clipboard-paste text injection: the typing route for Wayland compositors
with no zwp_virtual_keyboard (KWin).

Drives the real keyboard worker and _inject_text_via_clipboard against fakes:
buffered unicode keysyms must flush through the virtual keyboard when it works
and fall back to save clipboard -> write text -> Shift+Insert -> restore when
it does not, the text offered as a secret and the restore left until the
focused app has fetched it (as the app compositor's selection reports its
sends, else after a margin), lifting held modifiers around the chord, restoring binary
clipboards under their mime, clearing when there was nothing to restore, and
dropping nothing silently unless both engines fail.
"""
import asyncio
import os
import sys
import threading
import time

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))

from selkies.input_handler import (  # noqa: E402
    CLIPBOARD_PASTE_FETCH_MAX_S, CLIPBOARD_PASTE_FETCH_S, PixelfluxVkUnavailable, SecretText,
    WebRTCInput)

SHIFT_L = 0xFFE1
INSERT = 0xFF63
CTRL_L = 0xFFE3
BACKSPACE = 0xFF08

results = []


def check(label: str, ok, detail="") -> None:
    results.append((label, bool(ok)))
    print(f"{'PASS' if ok else 'FAIL'}  {label}  {detail}", flush=True)


class FakeWaylandInput:
    """No set_keymap_string, so the worker treats the seat as non-native and
    buffers unicode keysyms the way it does for a nested app compositor."""

    def __init__(self) -> None:
        self.cleared: list = []
        self.set_calls: list = []

    def clipboard_clear_app(self, display: str) -> None:
        self.cleared.append(display)

    def set_clipboard(self, entries) -> None:
        self.set_calls.append(entries)


class FetchedWaylandInput(FakeWaylandInput):
    """An app compositor's selection that reports its sends as pixelflux does
    (`clipboard_served_app`): `fetches` are the (seconds after the paste
    chord, mime) reads the focused app and a clipboard history make, `early`
    the mimes read before the chord."""

    def __init__(self, fetches: list, early: tuple = ()) -> None:
        super().__init__()
        self.fetches = fetches
        self.served = list(early)
        self.grew = threading.Condition()

    def chord(self) -> None:
        for delay, mime in self.fetches:
            timer = threading.Timer(delay, self._serve, (mime,))
            timer.daemon = True
            timer.start()

    def _serve(self, mime: str) -> None:
        with self.grew:
            self.served.append(mime)
            self.grew.notify_all()

    def clipboard_served_app(self, after: int, timeout_s: float) -> list:
        with self.grew:
            self.grew.wait_for(lambda: len(self.served) > after, timeout=timeout_s)
            return list(self.served)


def make_handler(vk_error: bool = True, separate: bool = True,
                 clipboard=None, write_ok: bool = True, wl=None) -> WebRTCInput:
    """A WebRTCInput whose injection dependencies are recording fakes.

    Args:
        vk_error: Whether the virtual keyboard raises PixelfluxVkUnavailable,
            forcing the clipboard-paste fallback.
        separate: Whether a nested app compositor is reported.
        clipboard: The `(data, mime)` the pre-paste save reads, or None.
        write_ok: Whether clipboard writes succeed.
        wl: The wayland_input fake, FakeWaylandInput by default.
    """
    h = WebRTCInput.__new__(WebRTCInput)
    # The key path's sets, as __init__ (not run here) makes them.
    h.MODIFIER_KEYSYMS = {65505, 65506, 65507, 65508, 65513, 65514, 65027, 65511, 65512,
                          65515, 65516, 65517, 65518}
    h.ACTION_MODIFIER_KEYSYMS = {65507, 65508, 65513, 65514, 65511, 65512, 65515, 65516, 65517, 65518}
    h.is_wayland = True
    h.wayland_input = wl or FakeWaylandInput()
    h.active_modifiers = set()
    h.atomically_typed_keys = set()
    h._wl_text_routed = {}
    h.max_pressed_keys = 1024
    h.keyboard_queue = asyncio.Queue()
    h._clipboard_inject_lock = asyncio.Lock()
    h._clipboard_inject_active = False
    h.keys = []
    h.key_times = []
    h.typed = []
    h.writes = []
    h.write_times = []
    h.clipboard = clipboard

    async def send(keysym, down=True, neutralize=None):
        h.keys.append((keysym, down))
        h.key_times.append(time.monotonic())
        if keysym == INSERT and down and isinstance(h.wayland_input, FetchedWaylandInput):
            h.wayland_input.chord()
    h.send_x11_keypress = send

    async def wl_type(text):
        if vk_error:
            raise PixelfluxVkUnavailable(
                "app compositor does not advertise zwp_virtual_keyboard_manager_v1")
        h.typed.append(text)
    h._wl_type_text = wl_type

    async def read_clipboard(use_binary=False):
        return h.clipboard if h.clipboard else (None, None)
    h.read_clipboard = read_clipboard

    async def write_clipboard(data, mime_type="text/plain"):
        if not write_ok:
            return False
        h.writes.append((data, mime_type))
        h.write_times.append(time.monotonic())
        return True
    h.write_clipboard = write_clipboard

    h._has_separate_app_compositor = lambda: separate
    h._app_wayland_display = lambda: "wayland-app"

    async def no_owner():
        return None
    h._ensure_wayland_keymap_owner = no_owner
    return h


async def run_worker(h: WebRTCInput, items: list, settle: float = 0.4) -> None:
    """Feed `items` through the keyboard worker, then cancel it after `settle`."""
    task = asyncio.create_task(h._keyboard_worker())
    for item in items:
        h.keyboard_queue.put_nowait(item)
    await asyncio.sleep(settle)
    task.cancel()
    try:
        await task
    except asyncio.CancelledError:
        pass


def chord_of(keys: list) -> list:
    """Just the Shift/Insert events, for comparing against PASTE_CHORD."""
    return [k for k in keys if k[0] in (SHIFT_L, INSERT)]


PASTE_CHORD = [(SHIFT_L, True), (INSERT, True), (INSERT, False), (SHIFT_L, False)]


async def main() -> None:
    h = make_handler(clipboard=("before", "text/plain"))
    ks = [0x01000000 | ord(c) for c in "한국어"]
    await run_worker(h, [("kd", k) for k in ks] + [("ku", k) for k in ks])
    check("buffered keysyms paste as one clipboard write",
          [w[0] for w in h.writes][:1] == ["한국어"], h.writes)
    check("paste chord is Shift+Insert in order", chord_of(h.keys) == PASTE_CHORD, h.keys)
    check("previous clipboard is restored after the paste",
          h.writes[-1] == ("before", "text/plain"), h.writes)
    check("the pasted text is offered as a secret, kept out of clipboard histories",
          isinstance(h.writes[0][0], SecretText) and not isinstance(h.writes[-1][0], SecretText), h.writes)
    fetch = h.write_times[-1] - h.key_times[-1] if h.key_times and len(h.write_times) > 1 else 0
    check("the clipboard is restored only once the focused app has had time to fetch the paste",
          fetch >= CLIPBOARD_PASTE_FETCH_S - 0.005, f"{fetch * 1000:.0f} ms")
    check("unicode keyups are swallowed",
          all(k[0] in (SHIFT_L, INSERT) for k in h.keys), h.keys)

    h = make_handler(clipboard=("before", "text/plain"))
    await run_worker(h, [("kd", 0x01000000 | ord("가")), ("kd", 0x01000000 | ord("나")),
                         ("kd", BACKSPACE)])
    check("backspace pops the buffer before it flushes",
          [w[0] for w in h.writes][:1] == ["가"], h.writes)

    # Blink's order for an IME commit: the last update streams it as a text
    # commit, then textInput erases and retypes it.
    h = make_handler(clipboard=("before", "text/plain"))
    await run_worker(h, [("co_end", "你好"), ("kd", BACKSPACE), ("ku", BACKSPACE),
                         ("kd", BACKSPACE), ("ku", BACKSPACE), ("co_end", "你好")])
    check("a backspace takes one character off a commit waiting to be typed",
          [w[0] for w in h.writes][:1] == ["你好"] and (BACKSPACE, True) not in h.keys,
          (h.writes, h.keys))

    h = make_handler(clipboard=None)
    await run_worker(h, [("co_end", "組み合わせ")])
    check("composition text takes the same clipboard route",
          [w[0] for w in h.writes] == ["組み合わせ"], h.writes)
    check("an empty clipboard is cleared, not repopulated",
          h.wayland_input.cleared == ["wayland-app"] and len(h.writes) == 1,
          (h.wayland_input.cleared, h.writes))

    h = make_handler(clipboard=("before", "text/plain"))
    h.active_modifiers = {CTRL_L}
    await run_worker(h, [("kd", 0x01000000 | ord("ㅎ"))])
    lifted = h.keys and h.keys[0] == (CTRL_L, False)
    restored = h.keys and h.keys[-1] == (CTRL_L, True)
    check("held modifier is lifted before the chord and restored after",
          lifted and restored and chord_of(h.keys) == PASTE_CHORD, h.keys)

    h = make_handler(clipboard=(b"\x89PNG", "image/png"))
    await run_worker(h, [("co_end", "text")])
    check("binary clipboard is restored under its own mime",
          h.writes[-1] == (b"\x89PNG", "image/png"), h.writes)

    h = make_handler(write_ok=False)
    await run_worker(h, [("co_end", "text")])
    check("failed clipboard write sends no chord", chord_of(h.keys) == [], h.keys)

    h = make_handler(vk_error=False, clipboard=("before", "text/plain"))
    await run_worker(h, [("co_end", "text")])
    check("working virtual keyboard bypasses the clipboard entirely",
          h.typed == ["text"] and h.writes == [], (h.typed, h.writes))

    h = make_handler(clipboard=("before", "text/plain"))
    h._clipboard_inject_active = True
    ok = await h._inject_text_via_clipboard("text")
    check("reentry is refused while an injection is running",
          ok is False and h.writes == [], h.writes)

    h = make_handler(clipboard=("before", "text/plain"))
    await h._type_keysym_fallback(0x20AC, down=True)
    check("single-keysym fallback pastes through the clipboard too",
          [w[0] for w in h.writes][:1] == ["€"] and chord_of(h.keys) == PASTE_CHORD,
          (h.writes, h.keys))

    h = make_handler(clipboard=("before", "text/plain"), separate=False)
    ok = await h._inject_text_via_clipboard("direct")
    check("non-nested topology injects and restores the same way",
          ok and [w[0] for w in h.writes] == ["direct", "before"], h.writes)

    async def restored_after(fetches: list, early: tuple = ()) -> float:
        """Seconds from the paste chord to the restore, against a selection
        reporting `fetches` (and `early` reads before the chord)."""
        h = make_handler(clipboard=("before", "text/plain"), wl=FetchedWaylandInput(fetches, early))
        ok = await h._inject_text_via_clipboard("느린")
        chord = h.key_times[h.keys.index((INSERT, True))]
        assert ok and h.writes[-1] == ("before", "text/plain"), h.writes
        return h.write_times[-1] - chord

    text = "text/plain;charset=utf-8"
    took = await restored_after([(0.3, text)])
    check("a fetch slower than the fixed margin is waited for",
          took >= 0.3 and took > CLIPBOARD_PASTE_FETCH_S, f"{took * 1000:.0f} ms")
    took = await restored_after([(0.03, text)])
    check("a quick fetch is followed at once, sooner than the fixed margin",
          0.03 <= took < CLIPBOARD_PASTE_FETCH_S - 0.05, f"{took * 1000:.0f} ms")
    took = await restored_after([(0.02, "application/x-kde-passwordManagerHint"), (0.25, text)])
    check("a clipboard history's read of the secret hint is not the paste",
          took >= 0.25, f"{took * 1000:.0f} ms")
    took = await restored_after([(0.2, text)], early=(text,))
    check("a read before the chord is not the paste",
          took >= 0.2, f"{took * 1000:.0f} ms")
    took = await restored_after([])
    check("an app that never fetches holds the clipboard no longer than the most it waits",
          CLIPBOARD_PASTE_FETCH_MAX_S - 0.01 <= took < CLIPBOARD_PASTE_FETCH_MAX_S + 0.15,
          f"{took * 1000:.0f} ms")


asyncio.run(main())
failed = [r for r in results if not r[1]]
print(f"\n{len(results) - len(failed)}/{len(results)} passed")
sys.exit(1 if failed else 0)
