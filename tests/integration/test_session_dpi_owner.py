#!/usr/bin/env python3
"""One desktop, one DPI: the primary display's page owns it.

Every display page derives its `scaling_dpi` from the density of the screen it
is shown on, and the server applies it to the session -- Xft resources on X11,
the session compositor's output scale on Wayland -- not to one display's
region. Two pages on screens of different densities therefore each ask for a
different desktop, and the desktop rescales for whichever spoke last, which is
whenever a window is restored or dragged between monitors. A secondary's DPI is
refused instead, and it says so rather than moving the whole session.

Driven with raw websockets clients: what is under test is the server's rule,
not a browser's derivation.

Usage: python3 tests/integration/test_session_dpi_owner.py
"""
import asyncio
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import websockets

APPLIED = "Applying system-level change."
REFUSED = "the desktop DPI follows the primary display"


def settings(display_id: str, dpi: int, size: tuple) -> str:
    return "SETTINGS," + json.dumps({
        "displayId": display_id, "initialClientWidth": size[0], "initialClientHeight": size[1],
        "manual_resolution": False, "framerate": 30, "encoder": "jpeg", "video_crf": 25,
        "video_bitrate": 6000, "audio_bitrate": 128000, "scaling_dpi": dpi,
        "displayPosition": "right",
    })


async def saw(mark: int, substr: str, timeout: float = 8) -> bool:
    """Whether `substr` reaches the server log at or after byte `mark`."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if H.server_log().find(substr, mark) >= 0:
            return True
        await asyncio.sleep(0.3)
    return False


async def drain(ws) -> None:
    try:
        while True:
            await ws.recv()
    except Exception:
        return


async def drive(res: "H.Results") -> None:
    uri = f"ws://localhost:{H.PORT}/api/websockets"
    async with websockets.connect(uri, max_size=None) as primary:
        await asyncio.wait_for(primary.recv(), timeout=10)
        pump = asyncio.create_task(drain(primary))
        mark = len(H.server_log())
        await primary.send(settings("primary", 192, (1280, 720)))
        res.check("the primary sets the desktop DPI", await saw(mark, APPLIED), "")

        # The server debounces reconnects from one address.
        await asyncio.sleep(1.0)
        async with websockets.connect(uri, max_size=None) as secondary:
            await asyncio.wait_for(secondary.recv(), timeout=10)
            pump2 = asyncio.create_task(drain(secondary))
            await secondary.send(settings("display2", 96, (1280, 720)))
            await asyncio.sleep(1.0)
            # The window is restored on a screen of another density, which is
            # what makes its page derive a new DPI and say so.
            mark = len(H.server_log())
            await secondary.send(settings("display2", 240, (1280, 720)))
            res.check("a secondary that rederives its density is refused",
                      await saw(mark, REFUSED), "")
            res.check("and the desktop is not rescaled for it",
                      not await saw(mark, APPLIED, timeout=2), "")

            mark = len(H.server_log())
            await secondary.send("s,96")
            res.check("the DPI sync verb is refused from a secondary too",
                      await saw(mark, REFUSED), "")

            mark = len(H.server_log())
            await primary.send(settings("primary", 144, (1280, 720)))
            res.check("the primary still moves it", await saw(mark, APPLIED), "")
            pump2.cancel()
        pump.cancel()


def main() -> "H.Results":
    res = H.Results("session-dpi-owner")
    H.server_start(mode="websockets", wayland=False)
    try:
        asyncio.run(drive(res))
    finally:
        H.server_stop()
    res.summary()
    return res


if __name__ == "__main__":
    r = main()
    sys.exit(0 if not r.failed() else 1)
