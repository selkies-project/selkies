#!/usr/bin/env python3
"""Wayland-backend DPI: where a scale lands, and what X11 still merges.

A nested session scales its own screen — scaling the capture instead would
halve the logical size that session is handed and upscale the whole desktop —
and the capture output carries the scale only for a session that manages no
outputs of its own (KWin) or no session at all. Applications on XWayland get
the DPI as Xft resources either way. These checks run set_dpi against a private
X server and read the resource database back, and pin the realization policy on
every topology.
"""
import asyncio
import os
import shutil
import subprocess
import sys
import tempfile
import time

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))
sys.path.insert(0, TESTS)
import helpers as H  # noqa: E402

results = []


def check(label: str, ok, detail: str = "") -> None:
    """Record and print one pass/fail result."""
    results.append((label, bool(ok)))
    print(f"{'PASS' if ok else 'FAIL'}  [session-dpi] {label}  {detail}", flush=True)


home = tempfile.mkdtemp(prefix="dpi-home-")
os.environ["HOME"] = home

from selkies import display_utils  # noqa: E402
from selkies.input_handler import WebRTCInput  # noqa: E402


class FakeSession:
    """The pixelflux ABI the policy calls, with a scriptable answer."""

    def __init__(self, answer) -> None:
        self.answer = answer
        self.calls: list[tuple[str, int, float]] = []

    def set_app_output_scale(self, display: str, index: int, scale: float):
        self.calls.append((display, index, scale))
        if isinstance(self.answer, Exception):
            raise self.answer
        return self.answer


def make_handler(separate: bool, answer=True) -> WebRTCInput:
    """Build a bare WebRTCInput wired to a FakeSession.

    Args:
        separate: Whether the handler believes a nested app compositor exists.
        answer: What the fake session returns from set_app_output_scale, or an
            Exception instance to raise instead.

    Returns:
        A WebRTCInput constructed without __init__, carrying only the
        attributes the DPI realization policy reads.
    """
    h = WebRTCInput.__new__(WebRTCInput)
    h._app_wl_is_separate = separate
    h._app_wayland_display = lambda: "wayland-9"
    h._has_separate_app_compositor = lambda: separate
    h.wayland_input = FakeSession(answer)
    return h


def realize(handler: WebRTCInput, dpi: float, index: int = 0) -> float:
    """Run the async DPI realization policy and return the capture scale."""
    return asyncio.run(handler.realize_wayland_dpi(dpi, index))


nested = make_handler(True)
check("a nested session takes the scale on its own screen",
      realize(nested, 192) == 1.0
      and nested.wayland_input.calls == [("wayland-9", 0, 2.0)])
second = make_handler(True)
realize(second, 144, 1)
check("a second display scales the session's second screen",
      second.wayland_input.calls == [("wayland-9", 1, 1.5)])
check("a session that manages no outputs keeps the capture scale",
      realize(make_handler(True, answer=False), 192) == 2.0)
check("a refused configuration keeps the capture scale",
      realize(make_handler(True, answer=RuntimeError("refused")), 192) == 2.0)
check("no nested session leaves the scale on the capture output",
      realize(make_handler(False), 192) == 2.0)
check("scale floor guards degenerate DPI",
      realize(make_handler(False), 0) == 0.1)

if not shutil.which("Xvfb") or not shutil.which("xrdb"):
    print("SKIP Xvfb/xrdb not installed; resource-merge checks need an X server",
          flush=True)
    failed = [r for r in results if not r[1]]
    print(f"\n{len(results) - len(failed)}/{len(results)} passed", flush=True)
    sys.exit(1 if failed else 77 if not failed else 0)

DISP = ":98"
xvfb = H.spawn(
    ["Xvfb", DISP, "-screen", "0", "640x480x24", "-extension", "GLX",
     "-nolisten", "tcp", "-ac", "-noreset"],
    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
try:
    for _ in range(50):
        if subprocess.run(["xrdb", "-query", "-display", DISP],
                          capture_output=True).returncode == 0:
            break
        time.sleep(0.2)

    def query() -> dict[str, str]:
        """Read the private X server's resource database as a dict."""
        out = subprocess.run(["xrdb", "-query", "-display", DISP],
                             capture_output=True, text=True).stdout
        return dict(line.split(":\t") for line in out.splitlines() if ":\t" in line)

    os.environ["DISPLAY"] = DISP
    display_utils._is_wayland = lambda: True
    check("wayland set_dpi merges nothing: the scale ladder owns the DPI",
          asyncio.run(display_utils.set_dpi(192)) is False
          and "Xft.dpi" not in query(), query().get("Xft.dpi"))

    display_utils._is_wayland = lambda: False
    ok = asyncio.run(display_utils.set_dpi(144))
    check("x11 backend path still merges through the DE ladder",
          ok and query().get("Xft.dpi") == "144", query().get("Xft.dpi"))
    check("xsettingsd config follows the merge",
          "Xft/DPI 147456" in open(os.path.join(home, ".xsettingsd")).read())
finally:
    xvfb.terminate()
    xvfb.wait()
    shutil.rmtree(home, ignore_errors=True)

failed = [r for r in results if not r[1]]
print(f"\n{len(results) - len(failed)}/{len(results)} passed", flush=True)
sys.exit(1 if failed else 0)
