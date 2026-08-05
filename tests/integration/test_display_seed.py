#!/usr/bin/env python3
"""Rigorous D4 probe: after SETTINGS sets framerate=33 on the primary, the
app-level global must carry the new value so a later-registered display state
seeds from 33fps (not the CLI default 60). Uses a fresh WS client per display."""
import asyncio
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import websockets


def _settings_payload(display_id, framerate, bitrate):
    return {
        "displayId": display_id, "initialClientWidth": 1280, "initialClientHeight": 720,
        "is_manual_resolution_mode": False, "framerate": framerate, "encoder": "h264enc",
        "video_crf": 25, "video_bitrate": bitrate, "audio_bitrate": 128000,
        "scaling_dpi": 96, "displayPosition": "right",
    }


def loglen():
    return len(H.server_log())


def wait_log_from(mark, substr, timeout=8):
    deadline = time.time() + timeout
    while time.time() < deadline:
        txt = H.server_log()
        if txt.find(substr, mark) >= 0:
            return True
        time.sleep(0.4)
    return False


async def connect_and_set(ws, display_id, framerate, bitrate):
    await ws.send("SETTINGS," + json.dumps(_settings_payload(display_id, framerate, bitrate)))


async def read_nonstop(ws, seconds):
    end = time.time() + seconds
    while time.time() < end:
        try:
            await asyncio.wait_for(ws.recv(), timeout=0.5)
        except asyncio.TimeoutError:
            continue
        except Exception:
            return


def main():
    H.server_start(mode="websockets", wayland=False)
    res = H.Results("D4-probe")

    async def drive():
        uri = f"ws://localhost:{H.PORT}/api/websockets"
        async with websockets.connect(uri, max_size=None) as p_ws:
            await asyncio.wait_for(p_ws.recv(), timeout=10)
            await connect_and_set(p_ws, "primary", 33, 2200)
            await asyncio.sleep(3.0)
            await read_nonstop(p_ws, 1.0)
            # A second display registers now; its state must inherit the primary's
            # live framerate (33), not the server default (60).
            st = loglen()
            async with websockets.connect(uri, max_size=None) as d2_ws:
                await asyncio.wait_for(d2_ws.recv(), timeout=10)
                await connect_and_set(d2_ws, "display2", 33, 2200)
                await asyncio.sleep(3.0)
                txt = H.server_log()[st:]
                res.check("D4: display2 seeded at client framerate",
                          "FPS: 33" in txt, txt[-300:])
            # cleanup

    asyncio.get_event_loop().run_until_complete(drive())
    res.summary()
    return res


if __name__ == "__main__":
    r = main()
    sys.exit(0 if not r.failed() else 1)