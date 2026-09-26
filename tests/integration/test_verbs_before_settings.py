#!/usr/bin/env python3
"""What a page sends before its initial SETTINGS must not hold that SETTINGS.

A page sends its first SETTINGS once its decoder probes answer, and a resize, a
DPI sync, or the cursor-rendering choice can reach the server ahead of it. The
websockets receive loop reads a connection's messages in order, so none of them
may wait there for the layout that the SETTINGS read behind them makes. The
resize and the DPI travel in that SETTINGS as well; the cursor-rendering choice
does not, and is still taken.

Usage: python3 tests/integration/test_verbs_before_settings.py [x11|wl]
"""
import asyncio
import json
import os
import sys
import time
from typing import Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402
import websockets  # noqa: E402

#: Logged once the receive loop has read the SETTINGS, before it applies them.
READ = "Parsed client settings"
APPLIED = "settings applied for 'primary'"
CURSOR_TAKEN = "Received SET_NATIVE_CURSOR_RENDERING: True"
#: Verbs a page can send ahead of its SETTINGS, and what shows each was taken where it must be.
EARLY = (
    ("a resize", "r,1600x900,primary", None),
    ("a DPI sync", "s,120", None),
    ("the cursor-rendering choice", "SET_NATIVE_CURSOR_RENDERING,1", CURSOR_TAKEN),
)


def settings_payload() -> str:
    """The initial SETTINGS a primary page sends."""
    return "SETTINGS," + json.dumps({
        "displayId": "primary",
        "initialClientWidth": 1280, "initialClientHeight": 720,
        "manual_resolution": False, "framerate": 60, "encoder": "h264enc",
        "scaling_dpi": 96,
    })


async def wait_log(needle: str, timeout: float) -> Optional[float]:
    """Poll the server log for `needle`; the time it appeared, or None."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if needle in H.server_log():
            return time.monotonic()
        await asyncio.sleep(0.2)
    return None


async def ahead_of_the_settings(res: "H.Results", label: str, verb: str, taken: Optional[str]) -> None:
    """Send `verb`, then the initial SETTINGS, and check the loop reads the SETTINGS at once."""
    async with websockets.connect(f"ws://localhost:{H.PORT}/api/websockets", max_size=None) as ws:
        await asyncio.wait_for(ws.recv(), timeout=10)
        await ws.send(verb)
        sent = time.monotonic()
        await ws.send(settings_payload())
        read = await wait_log(READ, 40)
        res.check(f"{label} ahead of the initial SETTINGS does not hold it",
                  read is not None and read - sent < 5.0,
                  "never read" if read is None else f"read {read - sent:.1f}s after it was sent")
        res.check(f"the SETTINGS behind {label} are applied", await wait_log(APPLIED, 40) is not None, "")
        if taken:
            res.check(f"{label} ahead of the initial SETTINGS is still taken",
                      await wait_log(taken, 10) is not None, "")


def run(wayland: bool) -> "H.Results":
    res = H.Results(f"verbs-before-settings-{'wl' if wayland else 'x11'}")
    for label, verb, taken in EARLY:
        # A fresh server each: only a session's first SETTINGS is ever waited for.
        H.server_start(mode="websockets", wayland=wayland, extra_env={"SELKIES_DEBUG": "true"})
        try:
            asyncio.run(ahead_of_the_settings(res, label, verb, taken))
        finally:
            H.server_stop()
    res.summary()
    return res


if __name__ == "__main__":
    backends = sys.argv[1:] or ["x11", "wl"]
    results = [run(backend == "wl") for backend in backends]
    sys.exit(0 if not any(r.failed() for r in results) else 1)
