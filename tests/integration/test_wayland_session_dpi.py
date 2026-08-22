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
        self.calls: list[tuple] = []

    def _record(self, call):
        self.calls.append(call)
        if isinstance(self.answer, Exception):
            raise self.answer
        return self.answer

    def set_app_output_scale(self, display: str, index: int, scale: float):
        return self._record((display, index, scale))

    def set_app_screen_geometry(self, display: str, index: int,
                                width: int, height: int, scale: float):
        return self._record((display, index, width, height, scale))


class OlderSession(FakeSession):
    """A pixelflux from before the combined call, which takes the scale alone."""

    set_app_screen_geometry = None


def make_handler(separate: bool, answer=True, session=FakeSession) -> WebRTCInput:
    """Build a bare WebRTCInput wired to a fake session.

    Args:
        separate: Whether the handler believes a nested app compositor exists.
        answer: What the fake session returns, or an Exception instance to raise
            instead.
        session: The fake session class, so the fallback for a pixelflux without
            the combined call is exercised too.

    Returns:
        A WebRTCInput constructed without __init__, carrying only the
        attributes the DPI realization policy reads.
    """
    h = WebRTCInput.__new__(WebRTCInput)
    h._app_wl_is_separate = separate
    h._app_wayland_display = lambda: "wayland-9"
    h._has_separate_app_compositor = lambda: separate
    h.wayland_input = session(answer)
    return h


def realize(handler: WebRTCInput, dpi: float, index: int = 0, size=None) -> float:
    """Run the async DPI realization policy and return the capture scale."""
    return asyncio.run(handler.realize_wayland_dpi(dpi, index, size))


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

# A scale on its own leaves the screen at the mode it had before the client, so
# the session lays out at a fraction of the size it ends at; a caller that knows
# the size sends both in one configuration and the session lays out once.
sized = make_handler(True)
check("a known size lands with the scale in one configuration",
      realize(sized, 192, 0, (2864, 1656)) == 1.0
      and sized.wayland_input.calls == [("wayland-9", 0, 2864, 1656, 2.0)],
      sized.wayland_input.calls)
for label, size in (("no size", None), ("an unrealized size", (0, 0))):
    unsized = make_handler(True)
    realize(unsized, 192, 0, size)
    check(f"{label} takes the scale alone", unsized.wayland_input.calls
          == [("wayland-9", 0, 2.0)], unsized.wayland_input.calls)
older = make_handler(True, session=OlderSession)
check("a pixelflux without the combined call still takes the scale",
      realize(older, 192, 0, (2864, 1656)) == 1.0
      and older.wayland_input.calls == [("wayland-9", 0, 2.0)],
      older.wayland_input.calls)

if not shutil.which("Xvfb") or not shutil.which("xrdb"):
    print("SKIP Xvfb/xrdb not installed; resource-merge checks need an X server",
          flush=True)
    failed = [r for r in results if not r[1]]
    print(f"\n{len(results) - len(failed)}/{len(results)} passed", flush=True)
    sys.exit(1 if failed else 77 if not failed else 0)

xvfb, DISP = H.private_x_server(640, 480)
try:
    def query() -> dict[str, str]:
        """Read the private X server's resource database as a dict."""
        out = subprocess.run(["xrdb", "-query", "-display", DISP],
                             capture_output=True, text=True).stdout
        return dict(line.split(":\t") for line in out.splitlines() if ":\t" in line)

    lxqt_conf = os.path.join(home, ".config", "lxqt", "lxqt.conf")
    os.makedirs(os.path.dirname(lxqt_conf), exist_ok=True)

    def seed_session_font() -> None:
        """An LXQt configuration carrying the packaged font, in points."""
        with open(lxqt_conf, "w") as f:
            f.write('[General]\nicon_theme=breeze\n\n[Qt]\n'
                    'font="Sans,11,-1,5,50,0,0,0,0,0"\nstyle=Fusion\n')

    def session_font() -> str:
        """The font line the platform theme would read back."""
        return next((line.strip() for line in open(lxqt_conf)
                     if line.strip().startswith("font=")), "")

    os.environ["DISPLAY"] = DISP
    display_utils._is_wayland = lambda: True
    seed_session_font()
    check("wayland set_dpi merges nothing: the scale ladder owns the DPI",
          asyncio.run(display_utils.set_dpi(192)) is False
          and "Xft.dpi" not in query(), query().get("Xft.dpi"))
    # The compositor scales the session's own screen, so a font pinned to
    # pixels here would be scaled a second time.
    check("wayland leaves the session font in points",
          session_font() == 'font="Sans,11,-1,5,50,0,0,0,0,0"', session_font())

    display_utils._is_wayland = lambda: False
    ok = asyncio.run(display_utils.set_dpi(144))
    check("x11 backend path still merges through the DE ladder",
          ok and query().get("Xft.dpi") == "144", query().get("Xft.dpi"))
    check("xsettingsd config follows the merge",
          "Xft/DPI 147456" in open(os.path.join(home, ".xsettingsd")).read())
    # Qt keeps a widget's font from the moment it is built, so Xft resources
    # reach nothing already on screen. Resolving the point size to pixels is
    # what the platform theme can tell apart, and so what repolishes them.
    check("x11 resolves the session font to pixels for the density",
          session_font() == 'font="Sans,11,22,5,50,0,0,0,0,0"', session_font())
    check("the rest of the session configuration survives the rewrite",
          "icon_theme=breeze" in open(lxqt_conf).read()
          and "style=Fusion" in open(lxqt_conf).read())
    asyncio.run(display_utils.set_dpi(288))
    check("a later density rescales from the same point size",
          session_font() == 'font="Sans,11,44,5,50,0,0,0,0,0"', session_font())
finally:
    H.stop_x_server(xvfb, DISP)
    shutil.rmtree(home, ignore_errors=True)

failed = [r for r in results if not r[1]]
print(f"\n{len(results) - len(failed)}/{len(results)} passed", flush=True)
sys.exit(1 if failed else 0)
