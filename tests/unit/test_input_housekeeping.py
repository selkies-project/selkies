#!/usr/bin/env python3
"""Small input-handler invariants shared by both backends.

Fire-and-forget session tasks keep a reference (asyncio only weakly holds a
running task); the xdotool type fallback ends its options before the text so
a payload starting with '-' is typed, not parsed; a client's REQUEST_CLIPBOARD
waits for the XFixes change without consuming the edge the monitor loop
broadcasts on; the per-connect cursor fetch reuses the PNG the monitor
already encoded for that cursor serial; and an X11 pointer message is one
packed XTEST request, byte for byte the generic one, and one flush.
"""
import asyncio
import os
import sys
import threading

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))

from selkies import input_handler as ih  # noqa: E402
from selkies.Xlib import X  # noqa: E402
from selkies.Xlib.ext import xtest  # noqa: E402
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


class FakeProtocolDisplay:
    """The request queue of python-xlib's protocol display, recording each request's bytes."""

    def __init__(self) -> None:
        self.sent = []

    def get_extension_major(self, name: str) -> int:
        return 132

    def send_request(self, request, wait_for_response: bool) -> None:
        self.sent.append(request._binary)


class FakeXDisplay:
    def __init__(self) -> None:
        self.display = FakeProtocolDisplay()
        self.flushes = 0

    def flush(self) -> None:
        self.flushes += 1


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

    async def realize(dpi, display_id=None, size=None):
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
    async def _no_xtest_type(text, neutralize=False):
        return False
    hx._type_text_xtest = _no_xtest_type
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


    # An X11 pointer message is one packed XTEST request and one flush.
    xd = FakeXDisplay()
    for event_type, detail, x, y in ((X.MotionNotify, 0, 640, 360), (X.MotionNotify, 1, -5, 7),
                                     (X.ButtonPress, 3, 0, 0), (X.KeyRelease, 38, 0, 0)):
        xtest.fake_input(xd, event_type, detail=detail, root=X.NONE, x=x, y=y)
        xtest.FakeInput(display=xd.display, opcode=132, event_type=event_type, detail=detail,
                        time=X.CurrentTime, root=X.NONE, x=x, y=y)
    sent = xd.display.sent
    check("the packed FakeInput is the generic request, byte for byte",
          len(sent) == 8 and sent[0::2] == sent[1::2], str(sent[:2]))
    hm = WebRTCInput.__new__(WebRTCInput)
    hm.xdisplay = FakeXDisplay()
    hm.mouse = ih._XTestMouse(hm.xdisplay)
    hm.wayland_input = None
    hm.data_server_instance = None
    hm.uinput_mouse_socket_path = None
    hm.tracked_position_stale = False
    hm.last_x = hm.last_y = 0
    hm.button_mask = 0
    await hm.send_x11_mouse(100, 200, 0, 0)
    check("an absolute move queues one warp and flushes once",
          len(hm.xdisplay.display.sent) == 1 and hm.xdisplay.flushes == 1,
          f"{len(hm.xdisplay.display.sent)} requests, {hm.xdisplay.flushes} flushes")


asyncio.run(main())
failed = [r for r in results if not r[1]]
print(f"\n{len(results) - len(failed)}/{len(results)} passed")
sys.exit(1 if failed else 0)
