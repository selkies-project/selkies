#!/usr/bin/env python3
"""Window management is restarted only where the deployment owns the session.

Some desktops tile a maximized window across the whole framebuffer rather than
against the per-display regions an extended layout defines, and handing the
session to a window manager that does not is the way out. That is right for a
session assembled around Selkies and wrong for a desktop somebody is using:
restarting the window manager there takes their session apart. So the deployment
names the one it wants, and an image that assembles its own session is what
names it.
"""
import asyncio
import os
import sys

TESTS = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REPO = os.path.dirname(TESTS)
ENTRYPOINT = os.path.join(REPO, "addons", "base", "container-entrypoint.sh")
sys.path.insert(0, TESTS)
sys.path.insert(0, os.path.join(REPO, "src"))
import helpers as H  # noqa: E402

from selkies import display_utils as DU  # noqa: E402
from selkies.settings import settings  # noqa: E402

res = H.Results("wm-swap")


def swap_attempt(named: str = "openbox --replace", is_wayland: bool = False,
                 displays: int = 2, running_wm: str = "xfwm4") -> list:
    """Run one `ensure_for` against stand-ins; report the commands it ran."""
    started: list = []

    async def fake_exec(*command, **kwargs):
        started.append(" ".join(command))

        class _Proc:
            pass
        return _Proc()

    async def fake_wm_name() -> str:
        return running_wm

    async def fake_wait(_name: str) -> bool:
        return True

    saved = (DU.asyncio.create_subprocess_exec, DU.current_wm_name, DU.wait_for_wm,
             settings.multi_monitor_wm)
    DU.asyncio.create_subprocess_exec = fake_exec
    DU.current_wm_name = fake_wm_name
    DU.wait_for_wm = fake_wait
    settings.multi_monitor_wm = named
    try:
        asyncio.run(DU.MultiMonitorWindowManager().ensure_for(displays, is_wayland))
    finally:
        (DU.asyncio.create_subprocess_exec, DU.current_wm_name, DU.wait_for_wm,
         settings.multi_monitor_wm) = saved
    return started


res.check("a session that named no window manager keeps its own",
          swap_attempt(named="") == [], swap_attempt(named=""))
res.check("the one the deployment named is what starts",
          swap_attempt() == ["openbox --replace"], swap_attempt())
res.check("any window manager can be named, not one this code knows",
          swap_attempt(named="i3 --replace") == ["i3 --replace"],
          swap_attempt(named="i3 --replace"))
res.check("a session already running it is left alone",
          swap_attempt(running_wm="Openbox") == [])
res.check("one display never swaps", swap_attempt(displays=1) == [])
res.check("Wayland manages its own windows", swap_attempt(is_wayland=True) == [])
res.check("no window manager is named unless something names one",
          settings.multi_monitor_wm == "", repr(settings.multi_monitor_wm))

# The image assembles the session it runs, so it is the one that asks.
entrypoint = open(ENTRYPOINT, encoding="utf-8").read()
res.check("the container image names the one it ships",
          'SELKIES_MULTI_MONITOR_WM="${SELKIES_MULTI_MONITOR_WM:-openbox --replace}"' in entrypoint)

sys.exit(0 if res.summary() else 1)
