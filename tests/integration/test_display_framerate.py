#!/usr/bin/env python3
"""D4 empirical: connect a primary, set framerate via SETTINGS (streaming
customizes it), then open a display2 that does NOT carry framerate (browser
without stored per-display param). Its capture must inherit 33fps, not the
server-default 60fps. Reads the server log's "FPS:" line for display2."""
import asyncio
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import websockets


def _settings(display_id: str, **over) -> dict:
    """Build a SETTINGS payload for a display, with overrides applied on top.

    The baseline deliberately omits ``framerate`` so callers control whether a
    display declares one.
    """
    base = {
        "displayId": display_id, "initialClientWidth": 1280, "initialClientHeight": 720,
        "manual_resolution": False, "encoder": "h264enc",
        "video_crf": 25, "video_bitrate": 6000, "audio_bitrate": 128000,
        "scaling_dpi": 96, "displayPosition": "right",
    }
    base.update(over)
    return base


def loglen() -> int:
    return len(H.server_log())


def status_lines(text: str) -> list[str]:
    """The capture module's per-display "Stream settings active" lines, which
    carry the settings a capture actually started with."""
    return [ln for ln in text.splitlines() if "Stream settings active" in ln]


def wait_contains(mark: int, substr: str, timeout: float = 12) -> bool:
    """Poll the server log for a substring appearing at or after an offset.

    Args:
        mark: Byte offset into the log where the search starts.
        substr: Substring to wait for.
        timeout: Seconds to keep polling before giving up.

    Returns:
        True when the substring appeared, False on timeout.
    """
    deadline = time.time() + timeout
    while time.time() < deadline:
        if substr in H.server_log()[mark:]:
            return True
        time.sleep(0.4)
    return False


async def read_ws(ws, seconds: float) -> None:
    """Drain incoming websocket messages for up to the given duration."""
    end = time.time() + seconds
    while time.time() < end:
        try:
            await asyncio.wait_for(ws.recv(), timeout=0.5)
        except Exception:
            return


def main() -> "H.Results":
    """Declare 33fps on the primary, then check a bare display2 inherits it."""
    H.server_start(mode="websockets", wayland=False)
    res = H.Results("d4")

    async def drive():
        uri = f"ws://localhost:{H.PORT}/api/websockets"
        async with websockets.connect(uri, max_size=None) as p1:
            await asyncio.wait_for(p1.recv(), timeout=10)
            await p1.send("SETTINGS," + json.dumps(_settings("primary", framerate=33, video_bitrate=2200)))
            await asyncio.sleep(3.0)
            await read_ws(p1, 1.5)
            # display2: SETTINGS without framerate/video_bitrate keys (fresh
            # browser, no per-display stored param) — its capture must inherit
            # the primary's live framerate (33) and bitrate (2200).
            mark2 = loglen()
            async with websockets.connect(uri, max_size=None) as p2:
                await asyncio.wait_for(p2.recv(), timeout=10)
                d2 = _settings("display2")
                d2.pop("video_bitrate", None)
                d2.pop("framerate", None)
                await p2.send("SETTINGS," + json.dumps(d2))
                await asyncio.sleep(3.0)
                await read_ws(p2, 1.5)
                d2_status = status_lines(H.server_log()[mark2:])
                res.check("display2 capture carries client framerate",
                          any("FPS: 33" in ln for ln in d2_status), d2_status[:1])
                # The primary set its bitrate explicitly, so the primary's own
                # status line says whether this capture module reports one at
                # all; where it does not, inheritance is unobservable here.
                if any("2200" in ln for ln in status_lines(H.server_log()[:mark2])):
                    res.check("display2 capture carries client bitrate",
                              any("2200" in ln for ln in d2_status), d2_status[:1])
                else:
                    res.skip("display2 capture carries client bitrate",
                             "the capture module omits bitrate from its status line")
                await asyncio.sleep(0.5)
            await read_ws(p1, 1)

    asyncio.new_event_loop().run_until_complete(drive())
    res.summary()
    return res


if __name__ == "__main__":
    r = main()
    sys.exit(0 if not r.failed() else 1)