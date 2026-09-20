#!/usr/bin/env python3
"""The session's automatic audio start must not hold the input behind it.

A primary page's first SETTINGS brings the audio capture up when the session
starts with audio on, and that start asks the sound server for the capture
sink. A sound server that accepts the connection and then never answers is
read to the control layer's own bounds, tens of seconds, so the start runs off
the websockets receive loop: a pad announced while it is outstanding is
associated at once, as it is over the WebRTC data channel, whose audio start
was never on its message path.

Usage: python3 tests/integration/test_audio_start_stall.py
"""
import asyncio
import base64
import json
import os
import socket
import sys
import threading
import time
from typing import Callable, Optional

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import helpers as H  # noqa: E402
import websockets  # noqa: E402

SETTINGS_PROCESSED = "settings applied for 'primary'"
ASSOCIATED = "associated with persistent virtual gamepad slot 0"
AUDIO_ATTEMPTED = "Initial setup: Primary client connected, audio not active, attempting start."


def settings_payload() -> str:
    """The initial SETTINGS a primary page sends."""
    return "SETTINGS," + json.dumps({
        "displayId": "primary",
        "initialClientWidth": 1280, "initialClientHeight": 720,
        "manual_resolution": False, "framerate": 60, "encoder": "h264enc",
        "scaling_dpi": 96,
    })


def silent_sound_server(path: str) -> Callable[[], None]:
    """Listen on `path` as a sound server that accepts every connection and
    answers nothing, the shape of one that is up and wedged.

    Returns:
        A callable that closes the listener and every held connection.
    """
    if os.path.exists(path):
        os.unlink(path)
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(path)
    listener.listen(8)
    listener.settimeout(0.5)
    held: list = []
    stop = threading.Event()

    def accept() -> None:
        while not stop.is_set():
            try:
                held.append(listener.accept()[0])
            except socket.timeout:
                continue
            except OSError:
                break

    threading.Thread(target=accept, daemon=True).start()

    def close() -> None:
        stop.set()
        listener.close()
        for conn in held:
            conn.close()
        try:
            os.unlink(path)
        except OSError:
            pass

    return close


async def wait_log(needle: str, timeout: float) -> Optional[float]:
    """Poll the server log for `needle`; the time it appeared, or None."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if needle in H.server_log():
            return time.monotonic()
        await asyncio.sleep(0.2)
    return None


def run() -> "H.Results":
    res = H.Results("audio-start-stall")
    os.makedirs(H.RUNTIME_DIR, exist_ok=True)
    close = silent_sound_server(os.path.join(H.RUNTIME_DIR, "pulse-silent.sock"))
    H.server_start(mode="websockets", wayland=False,
                   extra_env={"PULSE_SERVER": f"unix:{H.RUNTIME_DIR}/pulse-silent.sock",
                              "SELKIES_DEBUG": "true"})

    async def announce_behind_the_audio_start() -> None:
        async with websockets.connect(f"ws://localhost:{H.PORT}/api/websockets", max_size=None) as ws:
            await asyncio.wait_for(ws.recv(), timeout=10)
            await ws.send(settings_payload())
            name = base64.b64encode(b"Stall Test Pad").decode()
            await ws.send(f"js,c,0,{name},4,17")
            processed = await wait_log(SETTINGS_PROCESSED, 60)
            res.check("the first SETTINGS is processed", processed is not None)
            associated = await wait_log(ASSOCIATED, 20)
            res.check("a pad announced during the audio start is associated within 5 s",
                      processed is not None and associated is not None
                      and associated - processed < 5.0,
                      "never" if associated is None or processed is None
                      else f"{associated - processed:.1f}s after the settings")
            res.check("the audio start was still attempted",
                      await wait_log(AUDIO_ATTEMPTED, 20) is not None)

    try:
        asyncio.run(announce_behind_the_audio_start())
    finally:
        H.server_stop()
        close()
    res.summary()
    return res


if __name__ == "__main__":
    sys.exit(0 if not run().failed() else 1)
