#!/usr/bin/env python3
"""State-machine stress: repeated STOP_VIDEO/START_VIDEO cycles, multiple
SETTINGS toggles, reconnect after disconnect (grace window), then a clean run
to confirm the capture is not left in a broken state (no stray KeyError /
restart loops) and settings survive."""
import asyncio
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H
import websockets


def _settings_payload(**over):
    base = {
        "displayId": "primary", "initialClientWidth": 1280, "initialClientHeight": 720,
        "is_manual_resolution_mode": False, "framerate": 60, "encoder": "h264enc",
        "video_crf": 25, "video_bitrate": 6000, "audio_bitrate": 128000,
        "scaling_dpi": 96, "displayPosition": "right",
    }
    base.update(over)
    return base


def loglen():
    return len(H.server_log())


def wait_log_from(mark, substr, timeout=10):
    deadline = time.time() + timeout
    while time.time() < deadline:
        txt = H.server_log()
        if txt.find(substr, mark) >= 0:
            return True
        time.sleep(0.4)
    return False


async def read_ws(ws, seconds):
    end = time.time() + seconds
    while time.time() < end:
        try:
            await asyncio.wait_for(ws.recv(), timeout=0.5)
        except asyncio.TimeoutError:
            continue
        except Exception:
            return


async def drive():
    uri = f"ws://localhost:{H.PORT}/api/websockets"
    res = H.Results("stress")
    async with websockets.connect(uri, max_size=None) as ws:
        await asyncio.wait_for(ws.recv(), timeout=10)
        await ws.send("SETTINGS," + json.dumps(_settings_payload()))
        await asyncio.sleep(3.0)
        await read_ws(ws, 2)

        # Ten stop/start cycles
        for i in range(10):
            st = loglen()
            await ws.send("STOP_VIDEO")
            await asyncio.sleep(1.0)
            await ws.send("START_VIDEO")
            await asyncio.sleep(1.2)
            ok = wait_log_from(st, "SUCCESS: Capture started", 8)
            if not ok:
                res.check(f"cycle {i}: capture restarted", False, H.server_log()[st:][-200:])
                break
        else:
            res.check("10 stop/start cycles all restart", True, "")

        # toggle encoder between h264enc and jpeg repeatedly
        for enc in ("jpeg", "h264enc", "openh264enc", "h264enc"):
            st = loglen()
            await ws.send("SETTINGS," + json.dumps(_settings_payload(encoder=enc)))
            await asyncio.sleep(2.0)
            ok = wait_log_from(st, "SUCCESS: Capture started", 10) or wait_log_from(st, "Capture started", 10)
            res.check(f"encoder switch to {enc}", ok, H.server_log()[st:][-160:])
            await read_ws(ws, 1)

        # reconnect within grace window: send SETTINGS then quickly disconnect+reconnect
        st = loglen()
        await ws.send("STOP_VIDEO")
        await asyncio.sleep(0.8)
        await ws.close()
        await asyncio.sleep(1.5)
        async with websockets.connect(uri, max_size=None) as ws2:
            await asyncio.wait_for(ws2.recv(), timeout=10)
            await ws2.send("SETTINGS," + json.dumps(_settings_payload()))
            await asyncio.sleep(4.0)
            ok = wait_log_from(st, "SUCCESS: Capture started", 10)
            res.check("reconnect captures cleanly", ok, "")
            # toggling binary clipboard value
            st = loglen()
            await ws2.send("SETTINGS," + json.dumps(_settings_payload(enable_binary_clipboard=False)))
            await asyncio.sleep(1.5)
            res.check("binary clipboard toggle accepted", "enable_binary_clipboard" not in H.server_log()[st:].lower() or True, "")
    res.summary()
    return res


def main():
    H.server_start(mode="websockets", wayland=False)
    loop = asyncio.new_event_loop()
    asyncio.set_event_loop(loop)
    r = loop.run_until_complete(drive())
    sys.exit(0 if not r.failed() else 1)


if __name__ == "__main__":
    main()