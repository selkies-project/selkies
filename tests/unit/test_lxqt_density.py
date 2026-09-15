#!/usr/bin/env python3
"""An LXQt session on Qt 6 has its desktop rebuilt when the density changes.

Qt 6 derives a device pixel ratio from Xft.dpi and rescales every window with
it, so the font pixel size the DPI ladder resolves for Qt 5 would scale twice
there, and pcmanfm-qt keeps painting the wallpaper it built at the old ratio.
The ladder tells the two generations apart by the Qt core library the session
has mapped, leaves the font alone on Qt 6 and has pcmanfm-qt drop the desktop
and start it over with its own command line and environment.

The readers run against real processes; the decision and the rebuild are
driven against stand-ins that record what would be executed.
"""

import asyncio
import os
import re
import subprocess
import sys
import time

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, TESTS)
sys.path.insert(0, os.path.join(os.path.dirname(TESTS), "src"))

import helpers as H  # noqa: E402
from selkies import display_utils as DU  # noqa: E402

res = H.Results("lxqt-density")

# --- readers, against real processes ------------------------------------------

sleeper = subprocess.Popen(["sleep", "30"], env={"PATH": os.environ.get("PATH", ""),
                                                  "SELKIES_PROBE": "density"})
try:
    time.sleep(0.2)
    env = DU._process_environ(sleeper.pid)
    res.check("a process's environment is read back from /proc",
              env.get("SELKIES_PROBE") == "density", str(env))
    res.check("a process mapping no Qt has no generation",
              DU._qt_major(sleeper.pid) is None, str(DU._qt_major(sleeper.pid)))
    pids = asyncio.run(DU._pids_of("sleep"))
    res.check("the processes running a binary are found by name",
              sleeper.pid in pids, str(pids))
finally:
    sleeper.kill()
    sleeper.wait()

res.check("a process that is gone has an empty environment and no generation",
          DU._process_environ(sleeper.pid) == {} and DU._qt_major(sleeper.pid) is None, "")

qt_child = None
for module in ("PyQt6.QtCore", "PyQt5.QtCore", "PySide6.QtCore"):
    if subprocess.run(["/usr/bin/python3", "-c", f"import {module}"], capture_output=True).returncode == 0:
        qt_child = subprocess.Popen(["/usr/bin/python3", "-c", f"import {module}, time; time.sleep(30)"])
        break
if qt_child is None:
    res.skip("the Qt generation is read off the mapped core library",
             "no Python Qt binding installed on the system interpreter")
else:
    try:
        deadline = time.time() + 10
        major = None
        while time.time() < deadline and major is None:
            time.sleep(0.2)
            major = DU._qt_major(qt_child.pid)
        want = int(re.search(r"\d", module).group(0))
        res.check("the Qt generation is read off the mapped core library",
                  major == want, f"{module}: read {major}")
    finally:
        qt_child.kill()
        qt_child.wait()

# --- the decision and the rebuild, against stand-ins ---------------------------

saved = (DU._pids_of, DU._qt_major, DU._process_environ, DU.wm_command,
         DU.subprocess.create_subprocess_exec, DU.os.path.exists)


class _Proc:
    async def communicate(self):
        return b"", b""


def drive(pids: dict, majors: dict, commands: dict, gone=True) -> list:
    """Run one rebuild against stand-ins; report the commands it would run."""
    started = []

    async def fake_pids_of(binary):
        return pids.get(binary, [])

    async def fake_exec(*command, **kwargs):
        started.append((list(command), kwargs.get("env"), kwargs.get("start_new_session", False)))
        return _Proc()

    DU._pids_of = fake_pids_of
    DU._qt_major = lambda pid: majors.get(pid)
    DU._process_environ = lambda pid: {"DISPLAY": ":9", "DBUS_SESSION_BUS_ADDRESS": "unix:path=/x"}
    DU.wm_command = lambda pid: commands.get(pid, [])
    DU.subprocess.create_subprocess_exec = fake_exec
    DU.os.path.exists = lambda path: not gone
    try:
        by_ratio = asyncio.run(DU._lxqt_scales_by_ratio())
        if by_ratio:
            asyncio.run(DU._rebuild_lxqt_desktop(DU.logger_app_resize))
        return [by_ratio, started]
    finally:
        (DU._pids_of, DU._qt_major, DU._process_environ, DU.wm_command,
         DU.subprocess.create_subprocess_exec, DU.os.path.exists) = saved


DESKTOP = ["pcmanfm-qt", "--desktop", "--profile=lxqt"]

by_ratio, started = drive({"lxqt-session": [10], "pcmanfm-qt": [20, 21]}, {10: 6},
                          {20: ["pcmanfm-qt", "/home"], 21: DESKTOP})
res.check("a session on Qt 6 scales by ratio", by_ratio is True, "")
res.check("the desktop is dropped and started over, on the session's own bus",
          started == [(["pcmanfm-qt", "--desktop-off"], {"DISPLAY": ":9", "DBUS_SESSION_BUS_ADDRESS": "unix:path=/x"}, False),
                      (DESKTOP, {"DISPLAY": ":9", "DBUS_SESSION_BUS_ADDRESS": "unix:path=/x"}, True)],
          str(started))

by_ratio, started = drive({"lxqt-session": [10]}, {10: 5}, {})
res.check("a session on Qt 5 keeps its ratio, so the font path stays", by_ratio is False, "")

by_ratio, started = drive({}, {}, {})
res.check("no LXQt session, no ratio", by_ratio is False, "")

by_ratio, started = drive({"lxqt-session": [10], "pcmanfm-qt": [20]}, {10: 6},
                          {20: ["pcmanfm-qt", "/home"]})
res.check("a file manager window alone is not a desktop to rebuild",
          by_ratio is True and started == [], str(started))

by_ratio, started = drive({"lxqt-session": [10], "pcmanfm-qt": [21]}, {10: 6},
                          {21: DESKTOP}, gone=False)
res.check("an instance that stays up is started over in place",
          [c for c, _, _ in started] == [["pcmanfm-qt", "--desktop-off"], DESKTOP], str(started))

# --- the seed a transport gives a fresh page ------------------------------------

saved_dpi = DU._APPLIED_DPI
try:
    DU._APPLIED_DPI = None
    res.check("no density applied yet reads as None", DU.applied_dpi() is None, "")
    DU._APPLIED_DPI = 192
    res.check("the desktop's density is what the last ladder gave it", DU.applied_dpi() == 192, "")
finally:
    DU._APPLIED_DPI = saved_dpi

res.summary()
sys.exit(0 if not res.failed() else 1)
