#!/usr/bin/env python3
"""Small input-handler invariants shared by both backends.

Fire-and-forget session tasks keep a reference (asyncio only weakly holds a
running task); the xdotool type fallback ends its options before the text so
a payload starting with '-' is typed, not parsed; a client's REQUEST_CLIPBOARD
waits for the XFixes change without consuming the edge the monitor loop
broadcasts on; and the per-connect cursor fetch reuses the PNG the monitor
already encoded for that cursor serial.
"""
import asyncio
import os
import sys
import threading

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))

from selkies import input_handler as ih  # noqa: E402
from selkies.input_handler import WebRTCInput, _X11ClipboardMonitor  # noqa: E402

results = []


def check(label: str, ok, detail="") -> None:
    results.append((label, bool(ok)))
    print(f"{'PASS' if ok else 'FAIL'}  [input-housekeeping] {label}  {detail}", flush=True)


class FakeProcess:
    async def communicate(self, input=None):
        return b"", b""


class Cursor:
    def __init__(self, serial: int) -> None:
        self.cursor_serial = serial
        self.width = self.height = 1


def make_handler() -> WebRTCInput:
    h = WebRTCInput.__new__(WebRTCInput)
    h._bg_tasks = set()
    h.is_wayland = True
    h.wayland_input = object()
    h.system_dpi = 120.0
    return h


async def main() -> None:
    # Session tasks are spawned with a keep-alive reference.
    h = make_handler()
    started = []

    async def hold():
        started.append("hold")
        await asyncio.sleep(0.05)

    async def realize(dpi, display_index=0, size=None):
        started.append(("dpi", dpi))
        await asyncio.sleep(0.05)
        return 1.0
    h._hold_spare_screens = hold
    h.realize_wayland_dpi = realize
    h._schedule_spare_screen_hold()
    h._schedule_session_scale()
    check("spare-screen hold and session scale are referenced while running",
          len(h._bg_tasks) == 2, str(h._bg_tasks))
    await asyncio.sleep(0.15)
    check("both ran and dropped their reference", started == ["hold", ("dpi", 120)]
          and not h._bg_tasks, str(started))

    # The xdotool type fallback ends its options before the text.
    hx = WebRTCInput.__new__(WebRTCInput)
    hx.is_wayland = False
    hx.active_modifiers = set()
    hx.ACTION_MODIFIER_KEYSYMS = set()
    hx._type_text_xtest = lambda text, neutralize=False: False
    argv = []

    async def fake_exec(*cmd, **kwargs):
        argv.append(list(cmd))
        return FakeProcess()

    async def no_kill(proc, timeout, description, input=None):
        return await proc.communicate()
    hx._communicate_or_kill = no_kill
    saved_exec = ih.subprocess.create_subprocess_exec
    ih.subprocess.create_subprocess_exec = fake_exec
    try:
        await hx._dispatch_message("co,end,--delay 5")
    finally:
        ih.subprocess.create_subprocess_exec = saved_exec
    check("xdotool type gets '--' before the text",
          argv == [["xdotool", "type", "--", "--delay 5"]], str(argv))

    # REQUEST_CLIPBOARD's wait leaves the XFixes change edge to the monitor.
    m = _X11ClipboardMonitor.__new__(_X11ClipboardMonitor)
    m._changed = threading.Event()
    m._changed.set()
    peeked = await m.peek_change(0.2)
    check("peek_change sees the change", peeked is True)
    check("and leaves it set for the monitor loop", m._changed.is_set())
    consumed = await m.wait_change(0.2)
    check("wait_change consumes it", consumed is True and not m._changed.is_set())
    check("peek_change times out quietly with no change", await m.peek_change(0.05) is False)

    # The per-connect cursor fetch reuses the monitor's encode.
    hc = WebRTCInput.__new__(WebRTCInput)
    hc.cursor_size_cap = 64
    hc._cursor_msg_cache = None
    encodes = []

    def encode(cursor):
        encodes.append(cursor.cursor_serial)
        return {"serial": cursor.cursor_serial, "cap": hc.cursor_size_cap}
    hc.cursor_to_msg = encode
    first = hc._encode_cursor(Cursor(7))
    again = hc._encode_cursor(Cursor(7))
    check("same cursor serial encodes once", encodes == [7] and first is again)
    hc._encode_cursor(Cursor(8))
    check("a new serial encodes", encodes == [7, 8])
    hc.cursor_size_cap = 32
    hc._encode_cursor(Cursor(8))
    check("a changed size cap re-encodes", encodes == [7, 8, 8])


asyncio.run(main())
failed = [r for r in results if not r[1]]
print(f"\n{len(results) - len(failed)}/{len(results)} passed")
sys.exit(1 if failed else 0)
