#!/usr/bin/env python3
"""A second display opens where the client asked for it, on the Wayland backend.

"Left", "up" and "down" have to place the second screen there, not beside the
first: each display is a screen of the capture compositor's own, the secondary
created at the rectangle the layout says and the primary's screen moved off the
origin for the arrangements that put something left of or above it, so the
asked-for position is the one the compositor captures. (The session compositor
nested inside arranges its own screens by its own rule -- side by side, in the
order they were opened -- until Selkies mirrors this arrangement into it, which
is the nested session's business and not checked here.)

Both halves are checked for each position: the layout the server computes, and
where the compositor actually put the screens. Driven with raw websockets
clients against the in-process pixelflux compositor, no browser.

Usage: python3 tests/e2e/test_display_positions.py
"""
import asyncio
import importlib.util
import json
import os
import sys
import time
from typing import Dict, Tuple

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import websockets

PRIMARY = (1920, 1080)
SECONDARY = (1280, 720)


def settings_for(display_id: str, size: Tuple[int, int], position: str) -> dict:
    return {
        "displayId": display_id, "initialClientWidth": size[0],
        "initialClientHeight": size[1], "manual_resolution": False,
        "framerate": 30, "encoder": "jpeg", "video_crf": 25,
        "video_bitrate": 6000, "audio_bitrate": 128000,
        "scaling_dpi": 96, "displayPosition": position,
    }


async def wait_log_from(mark: int, substr: str, timeout: float = 45) -> bool:
    """Whether `substr` shows up in the server log at or after byte `mark`.

    Awaited rather than slept through: the sockets opened here are read by
    tasks on this loop, and blocking it for the timeout stops them answering
    the keepalive their peer expects, which drops a connection mid-run.
    """
    deadline = time.time() + timeout
    while time.time() < deadline:
        if H.server_log().find(substr, mark) >= 0:
            return True
        await asyncio.sleep(0.4)
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


def reconfigure_trail(mark: int) -> str:
    """What the server did about reconfiguring since `mark`, for a run that
    produced no layout: whether it started a pass at all, what it aborted on,
    and the layout line itself when one was logged but would not parse."""
    marks = ("Starting display reconfiguration", "No display clients connected",
             "Calculating new extended desktop layout", "total display size is zero",
             "Removing and triggering full display reconfiguration",
             "dropped on Wayland", "Layout calculated")
    seen = [line.rsplit(" - ", 1)[-1] for line in H.server_log()[mark:].splitlines()
            if any(m in line for m in marks)]
    return " | ".join(seen)[-400:] or "no reconfiguration logged"


def beside(position: str, first: Dict[str, int], second: Dict[str, int]) -> bool:
    """Whether `second` really sits at `position` relative to `first`."""
    if position == "right":
        return second["x"] >= first["x"] + first["w"]
    if position == "left":
        return second["x"] + second["w"] <= first["x"]
    if position == "down":
        return second["y"] >= first["y"] + first["h"]
    return second["y"] + second["h"] <= first["y"]


def outputs_from(log: str) -> Dict[int, Tuple[int, int]]:
    """Where the compositor last put each display's screen, by output id.

    Read from the compositor's own log lines, since it runs inside the server
    rather than in this process. The primary's screen (output 0) boots at the
    origin and logs only its moves; a secondary is recreated wherever it goes.
    """
    import re

    where: Dict[int, Tuple[int, int]] = {0: (0, 0)}
    for oid, x, y in re.findall(
            r"Output (\d+) (?:created: \d+x\d+ @ |repositioned to )\((-?\d+), (-?\d+)\)",
            log):
        where[int(oid)] = (int(x), int(y))
    return where


async def drain(ws) -> None:
    """Keep reading a client's socket, so the server never drops it for not
    consuming the stream it is being sent."""
    try:
        while True:
            await ws.recv()
    except Exception:
        return


async def drive(res: "H.Results") -> None:
    """Move the second display to each position and see where it lands.

    One pair of connections for the whole run, re-sent with a new position each
    time: the server debounces reconnects, and a live position change is what a
    client does anyway.
    """
    from selkies.display_utils import WAYLAND_SCREEN_OUTPUT_ID, wayland_output_id

    uri = f"ws://localhost:{H.PORT}/api/websockets"
    async with websockets.connect(uri, max_size=None) as primary:
        await asyncio.wait_for(primary.recv(), timeout=10)
        mark = len(H.server_log())
        pump = asyncio.create_task(drain(primary))
        await primary.send("SETTINGS," + json.dumps(
            settings_for("primary", PRIMARY, "right")))
        if not await wait_log_from(mark, "SUCCESS: Capture started for 'primary'"):
            res.check("the primary streams", False, "no capture")
            return
        # The server debounces reconnects from one address; the second client is
        # a fresh connection from the same one.
        await asyncio.sleep(1.0)
        async with websockets.connect(uri, max_size=None) as secondary:
            await asyncio.wait_for(secondary.recv(), timeout=10)
            pump2 = asyncio.create_task(drain(secondary))
            for position in ("right", "left", "up", "down"):
                mark = len(H.server_log())
                await secondary.send("SETTINGS," + json.dumps(
                    settings_for("display2", SECONDARY, position)))
                await wait_log_from(mark, "SUCCESS: Capture started for 'display2'", 20)
                await asyncio.sleep(4.0)
                layouts = layouts_from(mark)
                got = layouts[-1] if layouts else {}
                placed = (got.get("primary") and got.get("display2")
                          and beside(position, got["primary"], got["display2"]))
                res.check(f"the layout puts a '{position}' display there", placed,
                          got or reconfigure_trail(mark))

                want = {(WAYLAND_SCREEN_OUTPUT_ID if did == "primary"
                         else wayland_output_id(did)): (rect["x"], rect["y"])
                        for did, rect in got.items()} if got else {}
                where = outputs_from(H.server_log())
                res.check(f"the compositor's screens follow it for '{position}'",
                          bool(want) and all(where.get(oid) == pos
                                             for oid, pos in want.items()),
                          f"screens={where} layout={want}")
            pump2.cancel()
        pump.cancel()


def main() -> "H.Results":
    res = H.Results("display-positions")
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
    r = main()
    sys.exit(0 if not r.failed() else 1)
