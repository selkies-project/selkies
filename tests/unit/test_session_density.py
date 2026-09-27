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
import signal
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

res.check("a display's number is read from every local form of its name",
          [DU._display_number(n) for n in (":20", ":20.0", "unix:20", "localhost:20", "", None)]
          == ["20", "20", "20", None, None, None])
elsewhere, here = (subprocess.Popen(["sleep", "30"], env={"PATH": os.environ.get("PATH", ""),
                                                          "SELKIES_PROBE": "density", "DISPLAY": d})
                   for d in (":97", ":98.0"))
suite_display = os.environ.get("DISPLAY")
try:
    time.sleep(0.2)
    env = DU._process_environ(here.pid)
    res.check("a process's environment is read back from /proc",
              env.get("SELKIES_PROBE") == "density", str(env))
    os.environ["DISPLAY"] = "unix:98"
    pids = asyncio.run(DU._pids_on_display("sleep"))
    res.check("the processes running a binary are found on this display alone",
              here.pid in pids and elsewhere.pid not in pids, str(pids))
    res.check("a binary nothing is running has no pids",
              asyncio.run(DU._pids_on_display("selkies-no-such-binary")) == [])
finally:
    if suite_display is None:
        os.environ.pop("DISPLAY", None)
    else:
        os.environ["DISPLAY"] = suite_display
    for sleeper in (elsewhere, here):
        sleeper.kill()
        sleeper.wait()

res.check("a process that is gone has no environment to read",
          DU._process_environ(here.pid) == {})

res.check("no density is reported before one is applied", DU.applied_dpi() is None)
DU._APPLIED_DPI = 192
res.check("the density last applied is what a joining page is told", DU.applied_dpi() == 192)
DU._APPLIED_DPI = None

# The density a restart finds in the home, and what startup makes of it.
home = tempfile.mkdtemp(prefix="density-home-")
saved_env = {k: os.environ.get(k) for k in ("HOME", "DISPLAY", "XDG_CONFIG_HOME")}
os.environ["HOME"] = home
os.environ["XDG_CONFIG_HOME"] = os.path.join(home, ".config")
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

        # Stand-ins named xsettingsd, one serving this display and one another
        # display from this same home, which a SIGHUP ends.
        standin = [sys.executable, "-c", "import ctypes, time; "
                   "ctypes.CDLL(None).prctl(15, b'xsettingsd', 0, 0, 0); time.sleep(30)"]
        other = f":{int(display.lstrip(':')) + 1}"
        daemons = {d: subprocess.Popen(standin, env={"PATH": os.environ.get("PATH", ""), "DISPLAY": d})
                   for d in (display, other)}
        try:
            deadline = time.time() + 10
            while time.time() < deadline and not all(
                    open(f"/proc/{p.pid}/comm").read().strip() == "xsettingsd" for p in daemons.values()):
                time.sleep(0.05)
            merged = asyncio.run(DU._run_xrdb(144, DU.logger_app_resize))
            time.sleep(0.3)
            with open(os.path.join(home, ".xsettingsd")) as f:
                served = f.read()
            res.check("a density reaches the resource database and the xsettingsd file",
                      merged and DU.desktop_dpi() == 144 and "Xft/DPI 147456" in served, served)
            res.check("only the xsettingsd serving this display is told to reload",
                      daemons[display].poll() == -signal.SIGHUP and daemons[other].poll() is None,
                      {d: p.poll() for d, p in daemons.items()})
        finally:
            for daemon in daemons.values():
                daemon.kill()
                daemon.wait()
            subprocess.run(["xrdb", "-merge", "-"], input="Xft.dpi: 120\n", text=True,
                           env=dict(os.environ), check=True)
        lxqt = os.path.join(home, "lxqt.conf")
        with open(lxqt, "w") as f:
            f.write('[General]\nicon_theme=x\n[Qt]\nfont="Sans,-1,20,5,50,0,0,0,0,0"\n')
        res.check("a pixel-only session font resolves at the density the desktop has",
                  DU._rewrite_lxqt_font(lxqt, 96) == (12.0, 16),
                  open(lxqt).read())

        # An XFCE session keeps its density in xfconf, the one store set_dpi
        # writes there, so a restart reads it before the database and the files.
        channel = os.path.join(home, ".config", "xfce4", "xfconf", "xfce-perchannel-xml")
        os.makedirs(channel)
        real_declared = DU._declared_desktop
        DU._declared_desktop = lambda: "XFCE"
        try:
            res.check("an XFCE session that persisted no density reads the desktop as any other",
                      DU.desktop_dpi() == 120, DU.desktop_dpi())
            for value, want, label in ((-1, 120, "xfconf's unset density falls through"),
                                       (192, 192, "the density xfconf persists is read over the database and the files")):
                with open(os.path.join(channel, "xsettings.xml"), "w") as f:
                    f.write('<channel name="xsettings" version="1.0">\n  <property name="Xft" type="empty">\n'
                            f'    <property name="DPI" type="int" value="{value}"/>\n  </property>\n</channel>\n')
                res.check(label, DU.desktop_dpi() == want, DU.desktop_dpi())
            DU._declared_desktop = lambda: "Openbox"
            res.check("another session never reads xfconf's channel", DU.desktop_dpi() == 120, DU.desktop_dpi())
        finally:
            DU._declared_desktop = real_declared

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
