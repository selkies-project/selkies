#!/usr/bin/env python3
"""A primary that shrinks beside a secondary keeps the second screen (Wayland).

Both displays are views of one screen, so the secondary only has to move into
the room the primary gives up -- no output is destroyed and none can collide
with another, and the screen the two are cut from is resized around them. What
this pins is that the move happens and that the second display survives it,
since dropping it on every primary shrink is what a screen-per-display arrangement would do.
Driven with raw websockets clients on the websockets transport against the
in-process pixelflux compositor, no browser.
"""
import asyncio
import importlib.util
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import websockets



def settings_for(display_id: str, width: int, height: int, position: str = "right") -> dict:
    return {
        "displayId": display_id, "initialClientWidth": width, "initialClientHeight": height,
        "manual_resolution": False, "framerate": 30, "encoder": "jpeg",
        "video_crf": 25, "video_bitrate": 6000, "audio_bitrate": 128000,
        "scaling_dpi": 96, "displayPosition": position,
    }


def loglen() -> int:
    return len(H.server_log())


def wait_log_from(mark: int, substr: str, timeout: float = 20) -> bool:
    """Whether `substr` shows up in the server log at or after byte `mark`."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if H.server_log().find(substr, mark) >= 0:
            return True
        time.sleep(0.4)
    return False


def layouts_from(mark: int) -> list:
    """Every layout the server calculated at or after byte `mark`."""
    import ast
    out = []
    for line in H.server_log()[mark:].splitlines():
        if "Layout calculated" in line and "Layouts: " in line:
            try:
                out.append(ast.literal_eval(line.split("Layouts: ", 1)[1]))
            except (ValueError, SyntaxError):
                continue
    return out


async def drain(ws, seconds: float) -> list:
    """Text messages a socket receives within `seconds`."""
    got = []
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            msg = await asyncio.wait_for(ws.recv(), timeout=0.5)
        except asyncio.TimeoutError:
            continue
        if isinstance(msg, str):
            got.append(msg)
    return got


async def drive(res: "H.Results") -> None:
    uri = f"ws://localhost:{H.PORT}/api/websockets"
    async with websockets.connect(uri, max_size=None) as primary:
        await asyncio.wait_for(primary.recv(), timeout=10)
        mark = loglen()
        await primary.send("SETTINGS," + json.dumps(settings_for("primary", 1920, 1080)))
        res.check("primary capture starts", wait_log_from(mark, "SUCCESS: Capture started for 'primary'", 45), "")
        await drain(primary, 1.0)

        async with websockets.connect(uri, max_size=None) as secondary:
            await asyncio.wait_for(secondary.recv(), timeout=10)
            mark = loglen()
            await secondary.send("SETTINGS," + json.dumps(settings_for("display2", 1280, 720)))
            res.check("secondary capture starts beside the primary",
                      wait_log_from(mark, "SUCCESS: Capture started for 'display2'", 45), "")
            layouts = layouts_from(mark)
            before = layouts[-1] if layouts else {}
            res.check("secondary laid out at the primary's right edge",
                      before.get("display2", {}).get("x") == 1920, before)
            await drain(primary, 1.0)
            await drain(secondary, 1.0)

            mark = loglen()
            await primary.send("r,1280x720,primary")
            res.check("the shrink moves the secondary's view to its new offset",
                      wait_log_from(mark, "View 2 moved to (1280, 0).", 30), "")
            secondary_msgs = await drain(secondary, 3.0)
            tail = H.server_log()[mark:]
            # Moving a view does not disturb what it captures, so the second
            # display streams across the shrink rather than being torn down and
            # rebuilt as a screen-per-display arrangement would leave it.
            res.check("the secondary streams across the shrink without a restart",
                      "Stopping all streams for display 'display2'" not in tail, "")
            res.check("no view placement was refused",
                      "rejected" not in tail and "cannot place a view" not in tail, "")
            res.check("the secondary was not dropped",
                      "dropped on Wayland" not in tail
                      and not any(m.startswith("KILL") for m in secondary_msgs), secondary_msgs[:3])
            layouts = layouts_from(mark)
            after = layouts[-1] if layouts else {}
            res.check("the new layout keeps both displays, secondary at the shrunken edge",
                      after.get("primary", {}).get("w") == 1280 and after.get("display2", {}).get("x") == 1280,
                      after)
            await primary.send("STOP_VIDEO")
            await secondary.send("STOP_VIDEO")
            await asyncio.sleep(0.5)


def main() -> "H.Results":
    res = H.Results("wl-primary-shrink")
    if importlib.util.find_spec("pixelflux") is None:
        H.skip_suite("pixelflux is not installed")
    H.server_start(mode="websockets", wayland=True)
    try:
        asyncio.run(drive(res))
    finally:
        H.server_stop()
    res.summary()
    return res


if __name__ == "__main__":
    sys.exit(0 if not main().failed() else 1)
