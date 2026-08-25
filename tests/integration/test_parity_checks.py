#!/usr/bin/env python3
"""Protocol-level checks for WS/D3 (s, scaling opcode) and D4 (app.framerate/video_bitrate globals)."""
import asyncio
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import websockets


def _settings_payload(**over) -> dict:
    """Build a primary-display SETTINGS payload with overrides applied on top."""
    base = {
        "displayId": "primary", "initialClientWidth": 1280, "initialClientHeight": 720,
        "is_manual_resolution_mode": False, "framerate": 60, "encoder": "h264enc",
        "video_crf": 25, "video_bitrate": 6000, "audio_bitrate": 128000,
        "scaling_dpi": 96, "displayPosition": "right",
    }
    base.update(over)
    return base


def loglen() -> int:
    return len(H.server_log())


def wait_log_from(mark: int, substr: str, timeout: float = 8) -> bool:
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
        txt = H.server_log()
        i = txt.find(substr, mark)
        if i >= 0:
            return True
        time.sleep(0.4)
    return False


def main() -> "H.Results":
    """Exercise the scaling opcode and framerate-seeding over one WS client."""
    H.server_start(mode="websockets", wayland=False)
    res = H.Results("parity-check")

    async def drive():
        uri = f"ws://localhost:{H.PORT}/api/websockets"
        async with websockets.connect(uri, max_size=None) as ws:
            await asyncio.wait_for(ws.recv(), timeout=10)
            await ws.send("SETTINGS," + json.dumps(_settings_payload(framerate=60, video_bitrate=6000)))
            await asyncio.sleep(3.0)

            # The 's,120' scaling opcode must be handled, not warned away.
            st = loglen()
            await ws.send("s,120")
            await asyncio.sleep(1.5)
            txt = H.server_log()[st:]
            res.check("D3: 's,120' not dropped",
                      "unhandled on_scaling_ratio" not in txt or "DPI" in txt, txt.strip()[:150])

            await ws.send("STOP_VIDEO")
            await asyncio.sleep(1.0)
            await ws.send("START_VIDEO")
            await asyncio.sleep(1.0)

            # SETTINGS framerate=33 seeds app.framerate, so a display registering
            # afterwards starts at 33fps rather than the CLI default 60.
            st = loglen()
            await ws.send("SETTINGS," + json.dumps(_settings_payload(framerate=33, video_bitrate=2200)))
            await asyncio.sleep(2.0)
            st = loglen()
            await ws.send("SETTINGS," + json.dumps(_settings_payload(framerate=33, video_bitrate=2200, displayId="display2")))
            await asyncio.sleep(2.0)
            await ws.send("START_VIDEO")
            await asyncio.sleep(2.5)
            txt = H.server_log()[st:]
            res.check("D4: framerate 33 seeded for new display",
                      "FPS: 33.0" in txt or "33.00 FPS" in txt, txt[:160])

    asyncio.run(drive())
    res.summary()
    return res


if __name__ == "__main__":
    r = main()
    sys.exit(0 if not r.failed() else 1)