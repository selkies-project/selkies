#!/usr/bin/env python3
"""Window management is restarted only where the deployment owns the session.

XFCE and Plasma tile a maximized window across the whole framebuffer rather than
against the per-display regions an extended layout defines, and handing window
management to Openbox is the workaround. That is right for a session assembled
around Selkies and wrong for a desktop somebody is using: restarting the window
manager there takes their session apart. So it is a setting, off unless asked
for, and the image that owns its session is what asks.
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


def swap_attempt(asked: bool, is_wayland: bool = False, displays: int = 2,
                 running_wm: str = "xfwm4") -> list:
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
             DU.shutil.which, settings.multi_monitor_wm_swap)
    DU.asyncio.create_subprocess_exec = fake_exec
    DU.current_wm_name = fake_wm_name
    DU.wait_for_wm = fake_wait
    DU.shutil.which = lambda name: "/usr/bin/" + name if name == "xfce4-session" else None
    settings.multi_monitor_wm_swap = (asked, False)
    try:
        asyncio.run(DU.MultiMonitorWindowManager().ensure_for(displays, is_wayland))
    finally:
        (DU.asyncio.create_subprocess_exec, DU.current_wm_name, DU.wait_for_wm,
         DU.shutil.which, settings.multi_monitor_wm_swap) = saved
    return started


res.check("a session nobody asked about keeps its window manager",
          swap_attempt(asked=False) == [], swap_attempt(asked=False))
res.check("a deployment that asked gets the swap",
          swap_attempt(asked=True) == ["openbox --replace"], swap_attempt(asked=True))
res.check("a session already on Openbox is left alone",
          swap_attempt(asked=True, running_wm="Openbox") == [])
res.check("one display never swaps", swap_attempt(asked=True, displays=1) == [])
res.check("Wayland manages its own windows", swap_attempt(asked=True, is_wayland=True) == [])
res.check("the setting is off unless something asks",
          settings.multi_monitor_wm_swap[0] is False, settings.multi_monitor_wm_swap)

# The image assembles the session it runs, so it is the one that asks.
entrypoint = open(ENTRYPOINT, encoding="utf-8").read()
res.check("the container image asks for it",
          'SELKIES_MULTI_MONITOR_WM_SWAP="${SELKIES_MULTI_MONITOR_WM_SWAP:-true}"' in entrypoint)

sys.exit(0 if res.summary() else 1)
