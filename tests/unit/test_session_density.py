#!/usr/bin/env python3
"""What the density ladder reads about the session, and what it remembers.

A desktop that keeps its settings on a session bus is reached through the
environment its own process runs with, which is read out of /proc rather than
inherited, and the density the ladder last applied is what a page joining an
existing session is told the desktop already has. The readers run against real
processes.
"""

import asyncio
import os
import shutil
import subprocess
import sys
import tempfile
import time

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, TESTS)
sys.path.insert(0, os.path.join(os.path.dirname(TESTS), "src"))

import helpers as H  # noqa: E402
from selkies import display_utils as DU  # noqa: E402

res = H.Results("session-density")

sleeper = subprocess.Popen(["sleep", "30"], env={"PATH": os.environ.get("PATH", ""),
                                                 "SELKIES_PROBE": "density"})
try:
    time.sleep(0.2)
    env = DU._process_environ(sleeper.pid)
    res.check("a process's environment is read back from /proc",
              env.get("SELKIES_PROBE") == "density", str(env))
    pids = asyncio.run(DU._pids_of("sleep"))
    res.check("the processes running a binary are found by name",
              sleeper.pid in pids, str(pids))
finally:
    sleeper.kill()
    sleeper.wait()

res.check("a process that is gone has no environment to read",
          DU._process_environ(sleeper.pid) == {})
res.check("a binary nothing is running has no pids",
          asyncio.run(DU._pids_of("selkies-no-such-binary")) == [])

res.check("no density is reported before one is applied", DU.applied_dpi() is None)
DU._APPLIED_DPI = 192
res.check("the density last applied is what a joining page is told", DU.applied_dpi() == 192)
DU._APPLIED_DPI = None

# The density a restart finds in the home, and what startup makes of it.
home = tempfile.mkdtemp(prefix="density-home-")
saved_env = {k: os.environ.get(k) for k in ("HOME", "DISPLAY")}
os.environ["HOME"] = home
os.environ.pop("DISPLAY", None)
xserver = display = None
try:
    res.check("a fresh home names no density", DU.desktop_dpi() is None, DU.desktop_dpi())
    with open(os.path.join(home, ".xsettingsd"), "w") as f:
        f.write("Xft/Antialias 1\nXft/DPI 147456\n")
    res.check("persisted xsettingsd resources name the density they serve",
              DU.desktop_dpi() == 144, DU.desktop_dpi())
    with open(os.path.join(home, ".Xresources"), "w") as f:
        f.write("*background: black\nXft.dpi:   192\n")
    res.check("persisted Xresources are read over xsettingsd",
              DU.desktop_dpi() == 192, DU.desktop_dpi())
    try:
        xserver, display = H.private_x_server(320, 240)
    except RuntimeError as e:
        res.skip("the server's resource database is read over the files", e)
    else:
        os.environ["DISPLAY"] = display
        res.check("a resource database without Xft.dpi falls back to the files",
                  DU.desktop_dpi() == 192, DU.desktop_dpi())
        subprocess.run(["xrdb", "-merge", "-"], input="Xft.dpi: 120\n", text=True,
                       env=dict(os.environ), check=True)
        res.check("the server's resource database is read over the files",
                  DU.desktop_dpi() == 120, DU.desktop_dpi())
        lxqt = os.path.join(home, "lxqt.conf")
        with open(lxqt, "w") as f:
            f.write('[General]\nicon_theme=x\n[Qt]\nfont="Sans,-1,20,5,50,0,0,0,0,0"\n')
        res.check("a pixel-only session font resolves at the density the desktop has",
                  DU._rewrite_lxqt_font(lxqt, 96) == (12.0, 16),
                  open(lxqt).read())

    real_set_dpi, real_desktop_dpi = DU.set_dpi, DU.desktop_dpi
    applied = []

    async def record(value):
        applied.append(int(value))
        return True

    DU.set_dpi = record
    try:
        for current, configured, locked, want in (
                (None, 96, False, []), (96, 96, False, []), (192, 96, False, [192]),
                (192, 96, True, [96]), (None, 96, True, []), (None, 192, True, [192]),
                (192, 192, True, [192]), (144, 192, True, [192])):
            DU.desktop_dpi = lambda c=current: c
            applied.clear()
            got = asyncio.run(DU.restore_dpi(configured, locked))
            settle = configured if locked else (current or configured)
            res.check(f"startup finds {current or 'no'} density, {configured} "
                      f"{'operator-set' if locked else 'configured'}: applies "
                      f"{want or 'nothing'}, settles at {settle}",
                      applied == want and got == settle, (applied, got))
    finally:
        DU.set_dpi, DU.desktop_dpi = real_set_dpi, real_desktop_dpi
finally:
    for k, v in saved_env.items():
        if v is None:
            os.environ.pop(k, None)
        else:
            os.environ[k] = v
    if xserver:
        H.stop_x_server(xserver, display)
    shutil.rmtree(home, ignore_errors=True)

sys.exit(0 if res.summary() else 1)
