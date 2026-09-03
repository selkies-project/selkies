#!/usr/bin/env python3
"""Window management is restarted where the running manager needs it, once.

A window manager that reads the monitor set only as it starts tiles a
maximized window across the whole framebuffer rather than against the
per-display regions an extended layout defines. Which managers do is measured,
so a session extending onto a second display restarts one of those, with the
command line it was started with, and leaves every other alone: one that
follows monitor changes live needs nothing, and kwin_x11 builds its screens
from CRTCs, which a restart does not change.

The decision is driven against stand-ins; the readers are then checked against
a real Openbox on the test display, restarted for real.
"""
import asyncio
import os
import subprocess
import sys
import time

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
DESKTOP_IMAGE = os.path.join(REPO, "addons", "desktop", "Dockerfile")
sys.path.insert(0, TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))
import helpers as H  # noqa: E402

from selkies import display_utils as DU  # noqa: E402

res = H.Results("wm-swap")


def restart_attempt(running_wm: str = "Openbox", cmdline=("openbox", "--config-file", "/tmp/rc.xml"),
                    is_wayland: bool = False, displays: int = 2, manager=None) -> list:
    """Run one `ensure_for` against stand-ins; report the commands it ran."""
    started: list = []

    async def fake_exec(*command, **kwargs):
        started.append(" ".join(command))

        class _Proc:
            pass
        return _Proc()

    async def fake_wm_name() -> str:
        return running_wm

    async def fake_wm_pid() -> int:
        return 4242 if cmdline else 0

    def fake_wm_command(pid: int) -> list:
        return list(cmdline) if pid == 4242 else []

    async def fake_wait(_name: str) -> bool:
        return True

    saved = (DU.asyncio.create_subprocess_exec, DU.current_wm_name, DU.current_wm_pid,
             DU.wm_command, DU.wait_for_wm)
    DU.asyncio.create_subprocess_exec = fake_exec
    DU.current_wm_name = fake_wm_name
    DU.current_wm_pid = fake_wm_pid
    DU.wm_command = fake_wm_command
    DU.wait_for_wm = fake_wait
    try:
        asyncio.run((manager or DU.MultiMonitorWindowManager()).ensure_for(displays, is_wayland))
    finally:
        (DU.asyncio.create_subprocess_exec, DU.current_wm_name, DU.current_wm_pid,
         DU.wm_command, DU.wait_for_wm) = saved
    return started


res.check("Openbox is restarted with the command line it was started with, plus --replace",
          restart_attempt() == ["openbox --config-file /tmp/rc.xml --replace"], restart_attempt())
res.check("a --replace already on its command line is not doubled",
          restart_attempt(cmdline=("openbox", "--replace")) == ["openbox --replace"],
          restart_attempt(cmdline=("openbox", "--replace")))
res.check("xfwm4 is restarted as well",
          restart_attempt(running_wm="Xfwm4", cmdline=("xfwm4",)) == ["xfwm4 --replace"],
          restart_attempt(running_wm="Xfwm4", cmdline=("xfwm4",)))
res.check("a manager whose command line cannot be read is restarted by name",
          restart_attempt(cmdline=()) == ["openbox --replace"], restart_attempt(cmdline=()))
res.check("kwin_x11 is left alone: a restart does not change its CRTC screens",
          restart_attempt(running_wm="KWin", cmdline=("kwin_x11",)) == [])
res.check("a manager that follows monitor changes live is left alone",
          restart_attempt(running_wm="Mutter", cmdline=("mutter",)) == [])
res.check("no window manager, nothing to restart",
          restart_attempt(running_wm="", cmdline=()) == [])
res.check("one display never restarts", restart_attempt(displays=1) == [])
res.check("Wayland manages its own windows", restart_attempt(is_wayland=True) == [])
manager = DU.MultiMonitorWindowManager()
restart_attempt(manager=manager)
res.check("once per session: a second extend restarts nothing",
          restart_attempt(manager=manager) == [], restart_attempt(manager=manager))
res.check("the desktop image names no window manager",
          "MULTI_MONITOR_WM" not in open(DESKTOP_IMAGE, encoding="utf-8").read())


def live_openbox_check() -> None:
    """The readers against a real Openbox, and a real restart of it."""
    if H.shutil.which("openbox") is None:
        res.skip("a real Openbox is read and restarted", "no openbox on PATH")
        return
    display = H.require_display()
    env = {**os.environ, "DISPLAY": display}
    # The readers open the display the environment names; never the desktop's.
    os.environ["DISPLAY"] = display
    DU._drop_module_display()
    stale = DU._sync_wm_pid()
    if stale:
        os.kill(stale, 15)
        deadline = time.time() + 10
        while time.time() < deadline and DU._sync_wm_pid() == stale:
            time.sleep(0.2)
    proc = H.spawn(["openbox", "--replace"], env=env,
                   stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)
    try:
        deadline = time.time() + 15
        while time.time() < deadline and DU._sync_wm_pid() != proc.pid:
            time.sleep(0.2)
        name = DU._sync_wm_name()
        pid = DU._sync_wm_pid()
        res.check("the running Openbox advertises its name and process id",
                  DU.wm_name_matches("openbox", name) and pid == proc.pid, (name, pid, proc.pid))
        res.check("and its command line is read back from /proc",
                  DU.wm_command(pid) == ["openbox", "--replace"], DU.wm_command(pid))
        asyncio.run(DU.MultiMonitorWindowManager().ensure_for(2, False))
        deadline = time.time() + 10
        while time.time() < deadline and (proc.poll() is None
                                          or DU._sync_wm_pid() in (0, proc.pid)):
            time.sleep(0.2)
        new_pid = DU._sync_wm_pid()
        res.check("the first extend restarts it: the old process exits and a new one manages",
                  proc.poll() is not None and new_pid not in (0, proc.pid)
                  and DU.wm_name_matches("openbox", DU._sync_wm_name()),
                  (proc.poll(), new_pid, proc.pid))
        if new_pid not in (0, proc.pid):
            os.kill(new_pid, 15)
    finally:
        if proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=5)


live_openbox_check()
sys.exit(0 if res.summary() else 1)
