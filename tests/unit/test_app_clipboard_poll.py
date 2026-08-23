#!/usr/bin/env python3
"""Outbound clipboard under a nested app compositor: the watch rung and its
poll fallback.

The data-control watch only reports a failed handshake on its own thread, so
arming it must make the same handshake first and say — once — when the app
compositor offers no selection watch; the monitor loop then polls that
compositor's selection on a bounded cadence, retries the arm each tick, and
hands back to the watch the moment one holds. A persistent read failure is
reported once, not per poll or per 'cr'.
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
    print(f"{'PASS' if ok else 'FAIL'}  [app-clip-poll] {label}  {detail}", flush=True)


class Records(logging.Handler):
    def __init__(self) -> None:
        super().__init__()
        self.lines: list = []

    def emit(self, record: logging.LogRecord) -> None:
        self.lines.append((record.levelno, record.getMessage()))


records = Records()
ih.logger_webrtc_input.addHandler(records)
ih.logger_webrtc_input.setLevel(logging.DEBUG)

NO_DC = ("app compositor advertises neither ext_data_control_manager_v1 "
         "nor zwlr_data_control_manager_v1")


class FakeWaylandInput:
    """The pixelflux app-compositor data-control ABI: a selection that
    serves text, a handshake that can be made to fail like a compositor
    without data-control, and a watch that can be made to fail to arm."""

    def __init__(self) -> None:
        self.payload = b"one"
        self.dc_error = None
        self.watch_error = None
        self.watch_attempts: list = []
        self.watches: list = []

    def clipboard_types_app(self, display: str) -> list:
        if self.dc_error:
            raise RuntimeError(self.dc_error)
        return ["text/plain;charset=utf-8", "text/plain"]

    def clipboard_read_app(self, display: str, mime: str):
        if self.dc_error:
            raise RuntimeError(self.dc_error)
        return self.payload

    def clipboard_watch_app(self, display: str, callback) -> None:
        self.watch_attempts.append(display)
        if self.watch_error:
            raise RuntimeError(self.watch_error)
        self.watches.append((display, callback))

    def clipboard_unwatch_app(self, display: str) -> None:
        pass


def make_handler() -> WebRTCInput:
    """A WebRTCInput with the outbound-clipboard state, every client-facing
    side recorded, and the app compositor reported as a nested session."""
    h = WebRTCInput.__new__(WebRTCInput)
    h.is_wayland = True
    h.wayland_input = FakeWaylandInput()
    h.enable_clipboard = "true"
    h.enable_binary_clipboard = "false"
    h._clipboard_monitor_active = False
    h.clipboard_running = False
    h._clipboard_last_bytes = None
    h._app_watch_failure = None
    h._app_clip_read_failure = None
    h._x11_clipboard_monitor = None
    h._app_wl_display_cached = "wayland-9"
    h._has_separate_app_compositor = lambda: True
    h._app_wayland_display = lambda: "wayland-9"
    h._invalidate_app_wl_display_if_dead = lambda display: None
    h._clipboard_has_consumers = lambda: True
    h.sent: list = []

    async def no_monitor():
        return None
    h._ensure_x11_clipboard_monitor_async = no_monitor

    async def on_read(data, mime):
        h.sent.append((data, mime))
    h.on_clipboard_read = on_read
    return h


def lines_with(text: str) -> list:
    return [(lvl, line) for lvl, line in records.lines if text in line]


async def main() -> None:
    # --- a compositor without data-control: no watch, said once ---
    h = make_handler()
    h.wayland_input.dc_error = NO_DC
    q1 = await h._arm_app_compositor_watch()
    q2 = await h._arm_app_compositor_watch()
    check("no data-control: no watch armed", q1 is None and q2 is None
          and h.wayland_input.watches == [] and h.wayland_input.watch_attempts == [])
    warned = lines_with("no selection watch on app compositor")
    check("the failure is reported once, at arm time, naming the fallback",
          len(warned) == 1 and warned[0][0] == logging.WARNING and "polling" in warned[0][1],
          str(warned))

    # --- the read path reports the same failure once, not per call ---
    r1 = await h._app_clipboard_read(False)
    r2 = await h._app_clipboard_read(False)
    read_fail = lines_with("data-control clipboard read failed")
    check("reads return empty", r1 == (None, None) and r2 == (None, None))
    check("read failure warns once, then debug",
          [lvl for lvl, _ in read_fail] == [logging.WARNING, logging.DEBUG], str(read_fail))
    h.wayland_input.dc_error = None
    await h._app_clipboard_read(False)
    h.wayland_input.dc_error = NO_DC
    await h._app_clipboard_read(False)
    read_fail = lines_with("data-control clipboard read failed")
    check("a failure returning after a success warns again",
          [lvl for lvl, _ in read_fail][-1] == logging.WARNING, str(read_fail))

    # --- data-control present: the watch arms and delivers ---
    h = make_handler()
    q = await h._arm_app_compositor_watch()
    check("data-control present: watch armed", q is not None and len(h.wayland_input.watches) == 1)
    _, cb = h.wayland_input.watches[0]
    threading.Thread(target=cb, args=(["text/plain"],)).start()
    try:
        got = await asyncio.wait_for(q.get(), 1.0)
    except asyncio.TimeoutError:
        got = None
    check("watch callback from a foreign thread reaches the loop queue", got == ["text/plain"])

    # --- arming itself failing: no watch, said once ---
    h = make_handler()
    h.wayland_input.watch_error = "spawn: resource temporarily unavailable"
    q = await h._arm_app_compositor_watch()
    await h._arm_app_compositor_watch()
    check("a failed arm returns no watch", q is None)
    check("and is reported once", len(lines_with("resource temporarily unavailable")) == 1)

    # --- the monitor loop polls while no watch holds, then hands over ---
    h = make_handler()
    h.wayland_input.watch_error = "spawn: resource temporarily unavailable"
    task = asyncio.create_task(h.start_clipboard())
    await asyncio.sleep(0.3)
    check("first pass publishes the current selection", h.sent == [("one", "text/plain")], str(h.sent))
    h.wayland_input.payload = b"two"
    await asyncio.sleep(2.4)
    check("poll rung picks up a change within a tick", ("two", "text/plain") in h.sent, str(h.sent))
    check("the arm is retried per tick", len(h.wayland_input.watch_attempts) >= 2,
          str(len(h.wayland_input.watch_attempts)))
    sent_before = len(h.sent)
    h.wayland_input.watch_error = None
    await asyncio.sleep(2.4)
    check("the watch takes over once it arms", len(h.wayland_input.watches) == 1,
          str(h.wayland_input.watches))
    check("unchanged content is not re-sent while polling", len(h.sent) == sent_before, str(h.sent))
    h.clipboard_running = False
    try:
        await asyncio.wait_for(task, 5.0)
    except asyncio.TimeoutError:
        task.cancel()
    check("monitor loop stops", h._clipboard_monitor_active is False)


asyncio.run(main())
failed = [r for r in results if not r[1]]
print(f"\n{len(results) - len(failed)}/{len(results)} passed")
sys.exit(1 if failed else 0)
