#!/usr/bin/env python3
"""Outbound clipboard on the capture compositor: the callback is armed once.

pixelflux keeps the clipboard callback across capture stops and starts, and
every set_clipboard_callback stages a fresh read of the whole current
selection. The monitor loop must therefore register it once per monitor
start — not on every idle timeout, which re-read and re-delivered a large
copied image every two seconds — and that staged read must not be broadcast as
a copy. Every other delivery is one: the compositor hands the selection over
when an application offers it, so identical bytes are the user copying the
same thing again, which a client that failed to apply the first one is waiting
for. A backend that is not up yet is retried per tick with the failure said
once.
"""
import asyncio
import logging
import os
import sys
import threading

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))

from selkies import input_handler as ih  # noqa: E402
from selkies.input_handler import WebRTCInput  # noqa: E402

results = []


def check(label: str, ok, detail="") -> None:
    results.append((label, bool(ok)))
    print(f"{'PASS' if ok else 'FAIL'}  [clip-rearm] {label}  {detail}", flush=True)


class Records(logging.Handler):
    def __init__(self) -> None:
        super().__init__()
        self.lines: list = []

    def emit(self, record: logging.LogRecord) -> None:
        self.lines.append((record.levelno, record.getMessage()))


records = Records()
ih.logger_webrtc_input.addHandler(records)
ih.logger_webrtc_input.setLevel(logging.DEBUG)


def lines_with(text: str) -> list:
    return [(lvl, line) for lvl, line in records.lines if text in line]


class FakeCompositor:
    """pixelflux's compositor clipboard ABI: set_clipboard_callback keeps the
    callback and, like the real one, stages a re-read of the current
    selection on every registration; deliveries arrive from a foreign thread."""

    def __init__(self) -> None:
        self.arm_error = None
        self.arms: list = []
        self.callback = None
        self.selection = None
        self.reads = 0

    def set_clipboard_callback(self, callback) -> None:
        self.arms.append(callback)
        if self.arm_error:
            raise RuntimeError(self.arm_error)
        self.callback = callback
        if self.selection is not None:
            self._deliver_current()

    def copy(self, mime: str, data: bytes) -> None:
        self.selection = (mime, data)
        if self.callback is not None:
            self._deliver_current()

    def _deliver_current(self) -> None:
        self.reads += 1
        mime, data = self.selection
        t = threading.Thread(target=self.callback, args=(mime, data))
        t.start()
        t.join(2.0)


def make_handler() -> WebRTCInput:
    """A WebRTCInput with the outbound-clipboard state of a plain pixelflux
    session (no X server, no nested app compositor), every send recorded."""
    h = WebRTCInput.__new__(WebRTCInput)
    h.is_wayland = True
    h.wayland_input = FakeCompositor()
    h.enable_clipboard = "true"
    h.enable_binary_clipboard = "true"
    h._clipboard_monitor_active = False
    h.clipboard_running = False
    h._clipboard_last_bytes = None
    h._x11_clipboard_monitor = None
    h._app_wl_display_cached = None
    h._has_separate_app_compositor = lambda: False
    h._app_wayland_display = lambda: "wayland-1"
    h._clipboard_has_consumers = lambda: True
    h.sent: list = []

    async def no_monitor():
        return None
    h._ensure_x11_clipboard_monitor_async = no_monitor

    async def on_read(data, mime):
        h.sent.append((data, mime))
    h.on_clipboard_read = on_read
    return h


async def stop_monitor(h: WebRTCInput, task: "asyncio.Task") -> None:
    h.clipboard_running = False
    try:
        await asyncio.wait_for(task, 5.0)
    except asyncio.TimeoutError:
        task.cancel()


async def main() -> None:
    # Armed once; idle ticks do not re-register.
    h = make_handler()
    comp = h.wayland_input
    task = asyncio.create_task(h.start_clipboard())
    await asyncio.sleep(0.4)
    check("the compositor callback is armed at monitor start",
          len(comp.arms) == 1 and comp.callback is not None, str(len(comp.arms)))
    check("arming is said once", len(lines_with("native compositor callback active")) == 1)
    comp.copy("text/plain", b"one")
    await asyncio.sleep(0.5)
    check("a copy reaches the clients", h.sent == [("one", "text/plain")], str(h.sent))
    # Two full idle timeouts of the loop.
    await asyncio.sleep(4.6)
    check("idle ticks do not re-register the callback", len(comp.arms) == 1, str(len(comp.arms)))
    check("idle ticks do not re-read the selection", comp.reads == 1, str(comp.reads))
    check("idle ticks do not re-send", h.sent == [("one", "text/plain")], str(h.sent))

    # Handed over again outside an arm: the same thing copied again.
    comp._deliver_current()
    await asyncio.sleep(0.5)
    check("the same selection handed over again reaches the clients",
          h.sent == [("one", "text/plain"), ("one", "text/plain")], str(h.sent))
    image = bytes(range(256)) * (3 * 1024 * 4)
    comp.copy("image/png", image)
    await asyncio.sleep(0.8)
    check("a large image is sent once", h.sent[2:] == [(image, "image/png")], str(len(h.sent)))
    comp._deliver_current()
    await asyncio.sleep(2.6)
    check("a re-copied image reaches the clients without a re-arm",
          len(h.sent) == 4 and len(comp.arms) == 1, f"sent={len(h.sent)} arms={len(comp.arms)}")
    reads_before = comp.reads
    data, mime = await h.read_clipboard(use_binary=True)
    check("an on-demand read is served from the delivered cache, without a compositor read",
          data == image and mime == "image/png" and comp.reads == reads_before,
          f"reads={comp.reads}")

    # A monitor restart registers again, and the re-staged read is not re-sent.
    await stop_monitor(h, task)
    check("monitor loop stops", h._clipboard_monitor_active is False)
    task = asyncio.create_task(h.start_clipboard())
    await asyncio.sleep(0.6)
    check("a restarted monitor registers once more", len(comp.arms) == 2, str(len(comp.arms)))
    check("the re-staged read of the unchanged selection is not re-sent",
          len(h.sent) == 4, str(len(h.sent)))
    await stop_monitor(h, task)

    # The backend not up yet: retried per tick, said once.
    h = make_handler()
    comp = h.wayland_input
    comp.arm_error = "wayland backend not running"
    task = asyncio.create_task(h.start_clipboard())
    await asyncio.sleep(1.7)
    warned = lines_with("native callback failed to arm")
    check("a failed arm is retried per tick", len(comp.arms) >= 2, str(len(comp.arms)))
    check("and reported once", len(warned) == 1 and warned[0][0] == logging.WARNING, str(warned))
    comp.arm_error = None
    await asyncio.sleep(1.0)
    armed = comp.callback is not None
    arms_after = len(comp.arms)
    await asyncio.sleep(2.6)
    check("the arm holds once the backend answers, with no further registration",
          armed and len(comp.arms) == arms_after, f"{armed} {arms_after}->{len(comp.arms)}")
    comp.copy("text/plain", b"two")
    await asyncio.sleep(0.5)
    check("copies flow after the late arm", h.sent == [("two", "text/plain")], str(h.sent))
    await stop_monitor(h, task)


asyncio.run(main())
failed = [r for r in results if not r[1]]
print(f"\n{len(results) - len(failed)}/{len(results)} passed")
sys.exit(1 if failed else 0)
