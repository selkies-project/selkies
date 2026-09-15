#!/usr/bin/env python3
"""The LXQt desktop is rebuilt when the session density changes.

pcmanfm-qt sizes its desktop window and wallpaper from the density it started
at and follows no later change, so the desktop is dropped and started over
with the command line and environment it was running with, on its own session
bus. An instance showing file manager windows as well is left alone.

The readers run against real processes; the rebuild is driven against
stand-ins that record what would be executed.
"""

import asyncio
import os
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
    pids = asyncio.run(DU._pids_of("sleep"))
    res.check("the processes running a binary are found by name",
              sleeper.pid in pids, str(pids))
finally:
    sleeper.kill()
    sleeper.wait()

res.check("a process that is gone has an empty environment",
          DU._process_environ(sleeper.pid) == {}, "")

# --- the rebuild, against stand-ins --------------------------------------------

saved = (DU._pids_of, DU._process_environ, DU.wm_command,
         DU.subprocess.create_subprocess_exec, DU.os.path.exists)


class _Proc:
    async def communicate(self):
        return b"", b""


def drive(pids: dict, commands: dict, gone=True) -> list:
    """Run one rebuild against stand-ins; report the commands it would run."""
    started = []

    async def fake_pids_of(binary):
        return pids.get(binary, [])

    async def fake_exec(*command, **kwargs):
        started.append((list(command), kwargs.get("env"), kwargs.get("start_new_session", False)))
        return _Proc()

    DU._pids_of = fake_pids_of
    DU._process_environ = lambda pid: {"DISPLAY": ":9", "DBUS_SESSION_BUS_ADDRESS": "unix:path=/x"}
    DU.wm_command = lambda pid: commands.get(pid, [])
    DU.subprocess.create_subprocess_exec = fake_exec
    DU.os.path.exists = lambda path: not gone
    try:
        asyncio.run(DU._rebuild_lxqt_desktop(DU.logger_app_resize))
        return started
    finally:
        (DU._pids_of, DU._process_environ, DU.wm_command,
         DU.subprocess.create_subprocess_exec, DU.os.path.exists) = saved


DESKTOP = ["pcmanfm-qt", "--desktop", "--profile=lxqt"]
SESSION_ENV = {"DISPLAY": ":9", "DBUS_SESSION_BUS_ADDRESS": "unix:path=/x"}

started = drive({"pcmanfm-qt": [20, 21]}, {20: ["pcmanfm-qt", "/home"], 21: DESKTOP})
res.check("the desktop is dropped and started over, on the session's own bus",
          started == [(["pcmanfm-qt", "--desktop-off"], SESSION_ENV, False),
                      (DESKTOP, SESSION_ENV, True)],
          str(started))

res.check("no desktop running, nothing to rebuild", drive({}, {}) == [], "")

res.check("a file manager window alone is not a desktop to rebuild",
          drive({"pcmanfm-qt": [20]}, {20: ["pcmanfm-qt", "/home"]}) == [], "")

started = drive({"pcmanfm-qt": [21]}, {21: DESKTOP}, gone=False)
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
