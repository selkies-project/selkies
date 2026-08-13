#!/usr/bin/env python3
"""Clipboard ladder discipline around a dead or toolless X display.

The XFixes monitor is the top X11 rung: a failed build must back off instead
of hammering the display with a connection attempt per poll, must keep
re-probing after the cooldown so a late X server upgrades polling back to
events, and must never be built at all on the Wayland backend. A missing
xclip warns once — not a traceback per poll.
"""
import asyncio
import logging
import os
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))

from selkies import input_handler as ih  # noqa: E402
from selkies.input_handler import WebRTCInput  # noqa: E402

results = []


def check(label, ok, detail=""):
    results.append((label, bool(ok)))
    print(f"{'PASS' if ok else 'FAIL'}  [clip-ladder] {label}  {detail}", flush=True)


class Records(logging.Handler):
    def __init__(self):
        super().__init__()
        self.lines = []

    def emit(self, record):
        self.lines.append((record.levelno, record.getMessage()))


records = Records()
ih.logger_webrtc_input.addHandler(records)
ih.logger_webrtc_input.setLevel(logging.DEBUG)

builds = []


class FailingMonitor:
    def __init__(self):
        builds.append(1)
        raise RuntimeError("no display")


def make_handler(is_wayland=False):
    h = WebRTCInput.__new__(WebRTCInput)
    h.is_wayland = is_wayland
    h._x11_clipboard_monitor = None
    h._x11_monitor_retry_at = 0.0
    h._x11_monitor_unavail_logged = False
    h._xclip_missing_warned = False
    h._x11_monitor_build_lock = asyncio.Lock()
    return h


async def main():
    saved_monitor = ih._X11ClipboardMonitor
    saved_available = ih.X11_LIBS_AVAILABLE
    ih._X11ClipboardMonitor = FailingMonitor
    ih.X11_LIBS_AVAILABLE = True

    h = make_handler()
    m1 = await h._ensure_x11_clipboard_monitor_async()
    m2 = await h._ensure_x11_clipboard_monitor_async()
    check("failed build backs off instead of retrying per call",
          m1 is None and m2 is None and len(builds) == 1, f"builds={len(builds)}")

    infos = [lvl for lvl, line in records.lines if "XFixes monitor unavailable" in line]
    check("first failure logs at info", infos[:1] == [logging.INFO], infos)

    h._x11_monitor_retry_at = 0.0
    await h._ensure_x11_clipboard_monitor_async()
    check("cooldown expiry re-probes the top rung", len(builds) == 2, f"builds={len(builds)}")
    later = [lvl for lvl, line in records.lines if "XFixes monitor unavailable" in line]
    check("repeat failures drop to debug", later[-1] == logging.DEBUG, later)

    builds.clear()
    hw = make_handler(is_wayland=True)
    await hw._ensure_x11_clipboard_monitor_async()
    check("wayland backend never builds the X11 monitor", len(builds) == 0)

    def raise_missing(*args, **kwargs):
        raise FileNotFoundError(2, "No such file or directory")
    saved_exec = ih.subprocess.create_subprocess_exec
    ih.subprocess.create_subprocess_exec = raise_missing
    try:
        h = make_handler()
        h._x11_monitor_retry_at = sys.maxsize
        r1 = await h.read_clipboard()
        r2 = await h.read_clipboard()
        warnings = [line for lvl, line in records.lines if "xclip is not installed" in line]
        check("missing xclip reads return empty", r1 == (None, None) and r2 == (None, None))
        check("missing xclip warns once across polls", len(warnings) == 1, warnings)
        h._clipboard_last_bytes = None
        ok = await h.write_clipboard("text")
        warnings = [line for lvl, line in records.lines if "xclip is not installed" in line]
        check("missing xclip write fails quietly under the same warning",
              ok is False and len(warnings) == 1, warnings)
        tracebacks = [line for lvl, line in records.lines
                      if "Error reading clipboard with xclip" in line]
        check("no traceback spam for the missing binary", tracebacks == [], tracebacks)
    finally:
        ih.subprocess.create_subprocess_exec = saved_exec
        ih._X11ClipboardMonitor = saved_monitor
        ih.X11_LIBS_AVAILABLE = saved_available


asyncio.run(main())
failed = [r for r in results if not r[1]]
print(f"\n{len(results) - len(failed)}/{len(results)} passed")
sys.exit(1 if failed else 0)
