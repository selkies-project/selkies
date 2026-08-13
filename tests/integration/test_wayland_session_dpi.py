#!/usr/bin/env python3
"""Wayland-backend DPI: the capture-scale policy and the session Xft merge.

Under a nested app compositor, DPI must not become compositor output scale
(that would halve the nested desktop's logical size and upscale its buffers);
it reaches applications as Xft resources merged into the session's XWayland
display instead — the same realization the X11 backend uses. These checks run
set_dpi against a private X server and read the resource database back, and
pin the wayland_capture_scale policy on both topologies.
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

results = []


def check(label, ok, detail=""):
    results.append((label, bool(ok)))
    print(f"{'PASS' if ok else 'FAIL'}  [session-dpi] {label}  {detail}", flush=True)


home = tempfile.mkdtemp(prefix="dpi-home-")
os.environ["HOME"] = home

from selkies import display_utils  # noqa: E402
from selkies.input_handler import WebRTCInput  # noqa: E402


def make_handler(separate):
    h = WebRTCInput.__new__(WebRTCInput)
    h._app_wl_is_separate = separate
    h._app_wayland_display = lambda: "wayland-9"
    return h


check("nested session pins capture scale to 1.0",
      make_handler(True).wayland_capture_scale(192) == 1.0)
check("direct session keeps DPI/96 as capture scale",
      make_handler(False).wayland_capture_scale(192) == 2.0)
check("scale floor guards degenerate DPI",
      make_handler(False).wayland_capture_scale(0) == 0.1)
check("policy survives a detection failure", (lambda h: (
    setattr(h, "_app_wayland_display",
            lambda: (_ for _ in ()).throw(RuntimeError("down"))) or
    h.wayland_capture_scale(144) == 1.5))(make_handler(False)))

os.environ["SELKIES_APP_X_DISPLAY"] = "7"
check("display override wins discovery",
      display_utils._wayland_session_display() == ":7")
os.environ["SELKIES_APP_X_DISPLAY"] = ":12"
check("display override keeps an explicit colon form",
      display_utils._wayland_session_display() == ":12")
del os.environ["SELKIES_APP_X_DISPLAY"]

if not shutil.which("Xvfb") or not shutil.which("xrdb"):
    print("SKIP Xvfb/xrdb not installed; resource-merge checks need an X server",
          flush=True)
    failed = [r for r in results if not r[1]]
    print(f"\n{len(results) - len(failed)}/{len(results)} passed", flush=True)
    sys.exit(1 if failed else 77 if not failed else 0)

DISP = ":98"
xvfb = subprocess.Popen(
    ["Xvfb", DISP, "-screen", "0", "640x480x24", "-extension", "GLX",
     "-nolisten", "tcp", "-ac", "-noreset"],
    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
try:
    for _ in range(50):
        if subprocess.run(["xrdb", "-query", "-display", DISP],
                          capture_output=True).returncode == 0:
            break
        time.sleep(0.2)

    def query():
        out = subprocess.run(["xrdb", "-query", "-display", DISP],
                             capture_output=True, text=True).stdout
        return dict(line.split(":\t") for line in out.splitlines() if ":\t" in line)

    display_utils._is_wayland = lambda: True
    ok = asyncio.run(display_utils.set_dpi(192, x_display=DISP))
    check("wayland set_dpi merges Xft.dpi into the session display",
          ok and query().get("Xft.dpi") == "192", query().get("Xft.dpi"))
    check("xsettingsd config follows the merge",
          "Xft/DPI 196608" in open(os.path.join(home, ".xsettingsd")).read())

    ok = asyncio.run(display_utils.set_dpi(96, x_display=DISP))
    check("wayland set_dpi retargets on change",
          ok and query().get("Xft.dpi") == "96", query().get("Xft.dpi"))

    display_utils._wayland_session_display = lambda: None
    check("no session display reports failure, not success",
          asyncio.run(display_utils.set_dpi(120)) is False)

    display_utils._is_wayland = lambda: False
    os.environ["DISPLAY"] = DISP
    ok = asyncio.run(display_utils.set_dpi(144))
    check("x11 backend path still merges through the DE ladder",
          ok and query().get("Xft.dpi") == "144", query().get("Xft.dpi"))
finally:
    xvfb.terminate()
    xvfb.wait()
    shutil.rmtree(home, ignore_errors=True)

failed = [r for r in results if not r[1]]
print(f"\n{len(results) - len(failed)}/{len(results)} passed", flush=True)
sys.exit(1 if failed else 0)
