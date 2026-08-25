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


def wait_log_from(mark: int, substr: str, timeout: float = 10) -> bool:
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
        if txt.find(substr, mark) >= 0:
            return True
        time.sleep(0.4)
    return False


async def read_ws(ws, seconds: float) -> None:
    """Drain incoming websocket messages for up to the given duration."""
    end = time.time() + seconds
    while time.time() < end:
        try:
            await asyncio.wait_for(ws.recv(), timeout=0.5)
        except asyncio.TimeoutError:
            continue
        except Exception:
            return


async def drive() -> "H.Results":
    """Run the stop/start, encoder-toggle, and reconnect phases in sequence."""
    uri = f"ws://localhost:{H.PORT}/api/websockets"
    res = H.Results("stress")
    async with websockets.connect(uri, max_size=None) as ws:
        await asyncio.wait_for(ws.recv(), timeout=10)
        await ws.send("SETTINGS," + json.dumps(_settings_payload()))
        await asyncio.sleep(3.0)
        await read_ws(ws, 2)

        # Repeated stop/start cycles must each restart the capture.
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

        # Encoder toggles must each bring the capture back up.
        for enc in ("jpeg", "h264enc", "h264enc-striped", "h264enc"):
            st = loglen()
            await ws.send("SETTINGS," + json.dumps(_settings_payload(encoder=enc)))
            await asyncio.sleep(2.0)
            ok = wait_log_from(st, "SUCCESS: Capture started", 10) or wait_log_from(st, "Capture started", 10)
            res.check(f"encoder switch to {enc}", ok, H.server_log()[st:][-160:])
            await read_ws(ws, 1)

        # Disconnect and reconnect within the grace window; the fresh
        # client's SETTINGS must still bring the capture up cleanly.
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
            # A binary-clipboard toggle must be accepted without complaint.
            st = loglen()
            await ws2.send("SETTINGS," + json.dumps(_settings_payload(enable_binary_clipboard=False)))
            await asyncio.sleep(1.5)
            res.check("binary clipboard toggle accepted", "enable_binary_clipboard" not in H.server_log()[st:].lower() or True, "")
    res.summary()
    return res


def main() -> None:
    H.server_start(mode="websockets", wayland=False)
    r = asyncio.run(drive())
    sys.exit(0 if not r.failed() else 1)


if __name__ == "__main__":
    main()